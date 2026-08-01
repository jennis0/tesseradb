import {Deck} from '@deck.gl/core';
import {TesseraClient} from '@tessera/client';
import presetsJson from '../presets.json';
import {readConfig} from './config.js';
import {esc} from './html.js';
import {renderErrors} from './panels/errors.js';
import {renderItem, renderItemError} from './panels/item.js';
import {renderPrincipal, type Preset} from './panels/principal.js';
import {renderStats} from './panels/stats.js';
import {renderBudget, renderCounts} from './panels/view.js';
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
  result: null,
  worldPositions: null,
  status: 'idle',
  lastError: null,
  view: null,
  budget: 50_000,
  mTarget: 16,
  lastVisibleInView: null,
  lastTimings: null,
  latency: null,
  lastBytes: 0,
  inFlight: 0,
  failures: [],
  selected: null,
  selectedWorldXY: null,
  itemError: null
});

const controller = new ViewportController(store, client);
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
    controller.schedule(currentView, mapEl.clientWidth, mapEl.clientHeight);
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

function render() {
  if (panels.contains(document.activeElement)) return;

  panels.innerHTML =
    renderPrincipal(store.state, presets) +
    renderCounts(store.state) +
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
    controller.cancel();
    client
      .authorise(preset.terms)
      .then((session) => {
        store.update((s) => {
          s.session = session;
          s.terms = preset.terms;
          s.termsLabel = preset.label;
          s.result = null;
          s.worldPositions = null;
          s.status = 'idle';
          s.lastVisibleInView = null;
          s.mTarget = s.meta?.selection.thetaTargetMarks ?? 16;
          s.selected = null;
          s.selectedWorldXY = null;
          s.itemError = null;
        });
        controller.schedule(currentView, mapEl.clientWidth, mapEl.clientHeight);
      })
      .catch((error) => recordFailure(store, `authorise ${preset.label}`, error));
  });

  const budgetInput = document.getElementById('budget') as HTMLInputElement | null;
  budgetInput?.addEventListener('change', () => {
    store.update((s) => {
      s.budget = Number(budgetInput.value);
    });
    controller.schedule(currentView, mapEl.clientWidth, mapEl.clientHeight);
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

async function start() {
  const session = await client.authorise(store.state.terms);
  const meta = await client.meta(session.token);
  store.update((s) => {
    s.session = session;
    s.meta = meta;
    s.slice = meta.slices[0]!.id;
    s.mTarget = meta.selection.thetaTargetMarks;
  });
  controller.schedule(currentView, mapEl.clientWidth, mapEl.clientHeight);
}

start().catch((error) => {
  panels.innerHTML = `<section class="panel"><h2>Startup failed</h2>
    <div class="bad">${esc(error)}</div>
    <div class="muted">Is <code>tessera serve</code> running, and is this origin listed in
    <code>serve.dev_cors_origins</code>?</div></section>`;
});
