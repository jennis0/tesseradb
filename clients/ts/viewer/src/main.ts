import {Deck} from '@deck.gl/core';
import {Replica, TesseraClient} from '@tessera/client';
import presetsJson from '../presets.json';
import {readConfig} from './config.js';
import {esc} from './html.js';
import {renderErrors} from './panels/errors.js';
import {renderItem, renderItemError} from './panels/item.js';
import {renderPrincipal, type Preset} from './panels/principal.js';
import {renderLegend} from './panels/legend.js';
import {renderStats} from './panels/stats.js';
import {renderBudget, renderCounts} from './panels/view.js';
import {countCodes, extendRanks, widenDomain} from './colour.js';
import {coalesce, createStore, type Store} from './state.js';
import {
  INITIAL_VIEW_STATE,
  VIEW,
  ViewportController,
  buildViewportLayers,
  type ViewState
} from './viewportLayer.js';

const config = readConfig();
const client = new TesseraClient({
  viewerUrl: config.viewerUrl,
  sessionUrl: config.sessionUrl,
  sessionCredential: config.sessionCredential
});

const presets = presetsJson as Preset[];
const first = presets[0]!;

const store = createStore({
  meta: null,
  session: null,
  slice: '',
  termsLabel: first.label,
  terms: first.terms,
  k: undefined,
  underlayOffset: 0,
  assembled: null,
  status: 'idle',
  lastError: null,
  view: null,
  budget: 50_000,
  mTarget: 16,
  lastVisibleInView: null,
  lastTimings: null,
  latency: null,
  lastBytes: 0,
  replicaBytes: 0,
  lastPlan: null,
  inFlight: 0,
  failures: [],
  selected: null,
  selectedWorldXY: null,
  itemError: null,
  colourBy: null,
  categories: {},
  categoryErrors: {},
  ranks: {},
  domains: {}
});

/**
 * The replica sits between the stateless client and the scheduler: it owns the held bands and
 * decides which of the scheduler's wanted tiles actually need asking for. Created after `meta`,
 * because it needs the quantisation extent to turn tiles into a request box.
 */
let replica: Replica | null = null;
let controller: ViewportController | null = null;
const mapEl = document.getElementById('map') as HTMLDivElement;
const panels = document.getElementById('panels')!;

let currentView: ViewState = {target: INITIAL_VIEW_STATE.target, zoom: INITIAL_VIEW_STATE.zoom};

const deck = new Deck({
  parent: mapEl,
  views: VIEW,
  initialViewState: INITIAL_VIEW_STATE,
  controller: true,
  pickingRadius: 8,
  layers: [],
  onViewStateChange: ({viewState}) => {
    const v = viewState as {target: number[]; zoom: number};
    currentView = {target: [v.target[0]!, v.target[1]!, 0], zoom: v.zoom};
    controller?.schedule(currentView, mapEl.clientWidth, mapEl.clientHeight);
    return viewState;
  },
  onClick: (info) => {
    const ids = (info.sourceLayer?.props as {tesseraIds?: BigUint64Array} | undefined)?.tesseraIds;
    const id = ids && info.index >= 0 ? ids[info.index] : undefined;
    if (id === undefined || !store.state.session) {
      store.update((s) => {
        s.selected = null;
        s.selectedWorldXY = null;
        s.itemError = null;
      });
      return;
    }
    const worldXY = info.coordinate
      ? ([info.coordinate[0]!, info.coordinate[1]!] as [number, number])
      : null;
    client
      .item(store.state.session.token, id)
      .then((detail) => {
        store.update((s) => {
          s.selected = {id, scalars: detail.scalars, externalId: detail.externalId};
          s.selectedWorldXY = worldXY;
          s.itemError = null;
        });
      })
      .catch((error) => {
        const e = error as {code?: string; detail?: string; message?: string};
        store.update((s) => {
          s.selected = null;
          s.selectedWorldXY = worldXY;
          s.itemError = {
            code: e.code ?? 'fetch-failed',
            detail: e.detail ?? e.message ?? String(error)
          };
        });
      });
  }
});

function recordFailure(store: Store, what: string, error: unknown) {
  const e = error as {code?: string; detail?: string; message?: string};
  store.update((s) => {
    s.failures.push({
      tileId: what,
      code: e.code ?? 'fetch-failed',
      detail: e.detail ?? e.message ?? String(error),
      at: Date.now()
    });
  });
}

/**
 * Resolve the codes the current response actually carries for the coloured column.
 *
 * **Only the codes drawn, and only the ones not already held.** That is what keeps a 60,000-value
 * vocabulary off the wire: a viewport carries at most a few hundred distinct codes, and a pan
 * usually carries none that are new. It is also why the legend can never name a value that exists
 * only in items this principal cannot see — the codes come from marks the mask already admitted.
 *
 * A refusal is recorded per column rather than thrown: the marks are drawn either way, and the
 * legend shows the refusal in place of a swatch list.
 */
async function resolveCategoryCodes(column: string) {
  const {assembled, meta, session} = store.state;
  if (!assembled || !meta || !session) return;
  if (store.state.categoryErrors[column]) return;
  const declared = meta.declaredScalars.find((c) => c.name === column);
  if (!declared?.category) return;

  const values = assembled.scalars[column];
  if (!values) return;

  // Counted before anything is fetched, because frequency is what decides which values get one of
  // the palette's colours — and it must be measured over the marks on screen, not over whatever
  // order the server happened to return values in.
  const counts = countCodes(values);
  store.update((s) => {
    s.ranks[column] = extendRanks(s.ranks[column] ?? {}, counts);
  });

  const held = new Set((store.state.categories[column] ?? []).map((v) => v.code));
  // 0 is *absent*, not a value: the server never returns it, so asking would be a wasted round
  // trip on the most common code in a sparsely-populated column.
  const wanted = [...counts.keys()].filter((code) => !held.has(code));
  if (wanted.length === 0) {
    if (!store.state.categories[column]) store.update((s) => (s.categories[column] ??= []));
    return;
  }

  try {
    const resolved = await client.categories(session.token, column, {codes: wanted});
    store.update((s) => {
      // Merged rather than replaced: earlier viewports' values stay in the legend so that a
      // colour does not change meaning as the user pans.
      const existing = s.categories[column] ?? [];
      const byCode = new Map(existing.map((v) => [v.code, v]));
      for (const v of resolved) byCode.set(v.code, v);
      s.categories[column] = [...byCode.values()];
    });
  } catch (error) {
    const e = error as {code?: string; detail?: string; message?: string};
    store.update((s) => {
      s.categoryErrors[column] = {
        code: e.code ?? 'fetch-failed',
        detail: e.detail ?? e.message ?? String(error)
      };
    });
  }
}

function render() {
  if (panels.contains(document.activeElement)) return;

  panels.innerHTML =
    renderPrincipal(store.state, presets) +
    renderCounts(store.state) +
    renderLegend(store.state) +
    renderBudget(store.state) +
    (store.state.itemError
      ? renderItemError(store.state.itemError.code, store.state.itemError.detail)
      : renderItem(store.state)) +
    renderStats(store.state) +
    renderErrors(store.state);

  const select = document.getElementById('principal') as HTMLSelectElement | null;
  select?.addEventListener('change', () => {
    const preset = presets[Number(select.value)]!;
    // A different principal is a different mask: abort anything in flight for the old token, and
    // drop the calibration, which was measured against a different visible set.
    controller?.cancel();
    client
      .authorise(preset.terms)
      .then((session) => {
        store.update((s) => {
          replica?.setSession(session.tokenId);
          s.session = session;
          s.terms = preset.terms;
          s.termsLabel = preset.label;
          s.assembled = null;
          s.status = 'idle';
          s.lastVisibleInView = null;
          s.mTarget = s.meta?.selection.thetaTargetMarks ?? 16;
          s.selected = null;
          s.selectedWorldXY = null;
          s.itemError = null;
          // A different principal is a different gate and a different visible set, so both the
          // resolved values and the sticky domains are dropped. Keeping either would show this
          // principal a legend entry derived from the last one's marks — the cross-principal
          // persistence the session's own cache lifetime rule exists to prevent.
          s.categories = {};
          s.categoryErrors = {};
          s.ranks = {};
          s.domains = {};
        });
        controller?.schedule(currentView, mapEl.clientWidth, mapEl.clientHeight);
      })
      .catch((error) => recordFailure(store, `authorise ${preset.label}`, error));
  });

  const colourBy = document.getElementById('colour-by') as HTMLSelectElement | null;
  colourBy?.addEventListener('change', () => {
    const chosen = colourBy.value === '' ? null : colourBy.value;
    store.update((s) => {
      s.colourBy = chosen;
    });
    // **No refetch.** Every declared column is already in the held response, so this is a layer
    // rebuild — which is also what makes the switch a check on I7 that anyone can run: the mark
    // count cannot move, because no request is made.
    if (chosen) {
      const column = store.state.meta?.declaredScalars.find((c) => c.name === chosen);
      if (column?.category) {
        void resolveCategoryCodes(chosen);
      } else {
        const values = store.state.assembled?.scalars[chosen];
        const widened = values ? widenDomain(store.state.domains[chosen] ?? null, values) : null;
        store.update((s) => {
          if (widened) s.domains[chosen] = widened;
        });
      }
    }
  });

  const budgetInput = document.getElementById('budget') as HTMLInputElement | null;
  budgetInput?.addEventListener('change', () => {
    store.update((s) => {
      s.budget = Number(budgetInput.value);
    });
    controller?.schedule(currentView, mapEl.clientWidth, mapEl.clientHeight);
  });
}

const rerender = coalesce(render);
store.subscribe(
  coalesce(() => {
    deck.setProps({layers: buildViewportLayers(store)});
  })
);
store.subscribe(rerender);
panels.addEventListener('focusout', () => setTimeout(rerender, 0));

// A pan brings marks carrying codes the legend has not seen. Resolving on every store change
// rather than only on a response is deliberate: the coloured column can also change without one,
// and `resolveCategoryCodes` is a no-op once every drawn code is held, so the common case costs a
// set walk and no request.
let lastResolved: {column: string; result: unknown} | null = null;
store.subscribe(
  coalesce(() => {
    const {colourBy, assembled} = store.state;
    if (!colourBy || !assembled) return;
    if (lastResolved?.column === colourBy && lastResolved.result === assembled) return;
    lastResolved = {column: colourBy, result: assembled};
    void resolveCategoryCodes(colourBy);
  })
);

async function start() {
  const session = await client.authorise(store.state.terms);
  const meta = await client.meta(session.token);
  store.update((s) => {
    s.session = session;
    s.meta = meta;
    s.slice = meta.slices[0]!.id;
    s.mTarget = meta.selection.thetaTargetMarks;
  });
  replica = new Replica(
    (req, signal) => client.viewport(store.state.session!.token, {...req, slice: store.state.slice}, signal),
    meta.quantisation,
    {slice: meta.slices[0]!.id}
  );
  replica.setSession(session.tokenId);
  controller = new ViewportController(store, replica);
  controller.schedule(currentView, mapEl.clientWidth, mapEl.clientHeight);
}

start().catch((error) => {
  panels.innerHTML = `<section class="panel"><h2>Startup failed</h2>
    <div class="bad">${esc(error)}</div>
    <div class="muted">Is <code>tessera serve</code> running, and is this origin listed in
    <code>serve.dev_cors_origins</code>?</div></section>`;
});
