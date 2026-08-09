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
import {foldBandColumn} from './assemble.js';
import {countCodesCached, extendRanks, widenDomain} from './colour.js';
import {MarkSlab} from './slab.js';
import {installTrace, installTraceBar, trace} from './trace.js';
import {coalesce, createStore, type Store} from './state.js';
import {
  INITIAL_VIEW_STATE,
  VIEW,
  ViewportController,
  buildViewportLayers,
  encodingSignature,
  type ViewState
} from './viewportLayer.js';

const config = readConfig();
/**
 * The persistent mark buffer, owned here because it outlives every frame and every response.
 *
 * One per document: it is GPU-facing storage, not view state, and putting it in the store would
 * make every subscriber think a redraw had changed something when the whole point is that it
 * usually has not.
 */
const markSlab = new MarkSlab();
// Counted here rather than in the controller: a driver needs to attribute a paint to whether it
// cost a request, and the two are updated from different places.
let requestCount = 0;
const client = new TesseraClient({
  viewerUrl: config.viewerUrl,
  sessionUrl: config.sessionUrl,
  sessionCredential: config.sessionCredential
});

const presets = presetsJson as Preset[];
/**
 * The demo opens on its **hardest** case, not its gentlest: the broadest principal, the largest
 * mark budget, and a category encoding.
 *
 * The narrow default it replaces saturates at almost any depth, so the budget never binds, the
 * cache has nothing to do and the render path is never loaded — which made every session start by
 * changing three controls before anything under measurement was running. A demo whose defaults
 * exercise none of the machinery it exists to show is a demo of the controls.
 */
const first = presets.find((p) => p.label.startsWith('everything')) ?? presets.at(-1) ?? presets[0]!;
/** The slider's own maximum — see `panels/view.ts`. */
const DEFAULT_BUDGET = 500_000;
/** A declared category column, so the palette, the legend and `/v1/categories` are all live. */
const DEFAULT_COLOUR_BY = 'archive';

const store = createStore({
  meta: null,
  session: null,
  slice: '',
  termsLabel: first.label,
  terms: first.terms,
  k: undefined,
  underlayOffset: 0,
  assembled: null,
  sessionWarm: false,
  status: 'idle',
  lastError: null,
  view: null,
  budget: DEFAULT_BUDGET,
  mTarget: 16,
  lastVisibleInView: null,
  lastTimings: null,
  latency: null,
  lastBytes: 0,
  replicaBytes: 0,
  replicaPoints: 0,
  replicaBands: 0,
  prefetched: 0,
  lastPlan: null,
  inFlight: 0,
  failures: [],
  selected: null,
  selectedWorldXY: null,
  itemError: null,
  colourBy: DEFAULT_COLOUR_BY,
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
  /**
   * deck.gl's own accounting, once a second, and the only view of what happens after `setProps`.
   *
   * `painted` — the gap from handing deck the layers to the next frame — has held at ~50% of every
   * recorded session and ~80 ms mean while every client cost around it was halved. It bounds the
   * work but cannot attribute it: GPU draw, deck's attribute upload and the wait for vsync are all
   * inside it. `gpuTime` and `updateAttributesTime` split the first two apart, and
   * `updateAttributesCount` says directly whether the binary-descriptor skip is firing — which is
   * currently argued from deck's source rather than measured here.
   */
  _onMetrics: trace.enabled
    ? (m) =>
        trace.event('deck', {
          fps: Math.round(m.fps),
          gpu: Math.round(m.gpuTime),
          cpu: Math.round(m.cpuTime),
          attrs: Math.round(m.updateAttributesTime),
          attrCount: m.updateAttributesCount,
          redrawn: m.framesRedrawn,
          setProps: Math.round(m.setPropsTime),
          gpuMemory: m.gpuMemory
        })
    : null,
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

  // Counted before anything is fetched, because frequency is what decides which values get one of
  // the palette's colours — and it must be measured over the marks on screen, not over whatever
  // order the server happened to return values in. Folded band by band: a count is a sum, so it
  // never needed the concatenated column the exact bands no longer build.
  // Exact bands only, each memoised. The stand-in column is rebuilt per derive, so folding it
  // defeated the memo on precisely the largest column — measured at 51 ms per count *after* the
  // per-band cache landed, which is what gave it away. A code visible only through stand-ins
  // renders grey until its own bands arrive, and they are already on their way.
  const counts = trace.phase('legend', () => {
    const held = new Map<number, number>();
    for (const band of assembled.bands) {
      const values = band.scalars[column];
      if (!values) continue;
      for (const [code, n] of countCodesCached(values)) held.set(code, (held.get(code) ?? 0) + n);
    }
    return held;
  });
  if (counts.size === 0 && assembled.bands.length === 0) return;
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
    renderStats(store.state, {drawn: markSlab.drawn, departed: markSlab.departed}) +
    renderErrors(store.state);

  const select = document.getElementById('principal') as HTMLSelectElement | null;
  select?.addEventListener('change', () => {
    const preset = presets[Number(select.value)]!;
    trace.event('principal', {label: preset.label, n: preset.terms.length});
    // A different principal is a different mask: abort anything in flight for the old token, and
    // drop the calibration, which was measured against a different visible set.
    controller?.cancel();
    client
      .authorise(preset.terms)
      .then((session) => {
        store.update((s) => {
          // A different principal is a different render partition. The replica also drops on the
          // identity coordinate the next response carries; this is the earlier of the two, so no
          // band from the old mask is ever held while the new token's first request is in flight.
          replica?.reset();
          s.session = session;
          s.terms = preset.terms;
          s.termsLabel = preset.label;
          s.assembled = null;
          s.sessionWarm = false;
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
        const assembled = store.state.assembled;
        const widened = assembled
          ? foldBandColumn(assembled, chosen, store.state.domains[chosen] ?? null, widenDomain)
          : null;
        store.update((s) => {
          if (widened) s.domains[chosen] = widened;
        });
      }
    }
  });

  const budgetInput = document.getElementById('budget') as HTMLInputElement | null;
  budgetInput?.addEventListener('change', () => {
    trace.event('budget', {n: Number(budgetInput.value)});
    store.update((s) => {
      s.budget = Number(budgetInput.value);
    });
    controller?.schedule(currentView, mapEl.clientWidth, mapEl.clientHeight);
  });
}

installTrace(mapEl);
installTraceBar();

/**
 * The panels are rebuilt by writing `innerHTML`, and that happens inside the window `painted`
 * measures — so until it is timed it is indistinguishable from GPU cost in every trace so far.
 */
const rerender = coalesce(() => trace.phase('panels', render));
/**
 * The measurement surface: when marks last reached the screen, and what they cost to get there.
 *
 * A driver cannot time pan-to-paint from outside — the interesting case is a pan answered entirely
 * from held bands, which produces no network activity at all and so is invisible to anything
 * watching requests. Exposed rather than inferred, and read by `smoke-latency.mjs`.
 */
declare global {
  interface Window {
    __tesseraProbe?: {paints: number; at: number; marks: number; requests: number};
  }
}
let paints = 0;
/**
 * What the last paint actually drew.
 *
 * **The store changes far more often than the picture does.** Latency, timings, held bytes, the
 * in-flight count — every panel row is a store update, and each one was rebuilding the layers and
 * handing deck.gl a fresh `data` object, which it can only read as "everything changed". Measured
 * in a browser: 54 of 144 paints handed deck an identical drawn set, costing 2.3 s of a 22.8 s
 * session. The panels still re-render; only the marks are spared.
 */
let painted = '';
store.subscribe(
  coalesce(() => {
    const view = store.state;
    const drawing = `${view.status}|${view.assembled?.version ?? -1}|${
      view.assembled?.depth ?? -1
    }|${markSlab.drawn}|${view.assembled?.provisional ?? 0}|${encodingSignature(store)}|${
      view.selectedWorldXY?.join(',') ?? ''
    }`;
    if (drawing === painted) return;
    painted = drawing;

    const built = trace.phase('layers', () => buildViewportLayers(store, markSlab));
    const handed = performance.now();
    deck.setProps({layers: built});
    // **The gap from handing deck the layers to the next frame is the upload.** It is the one cost
    // the browser-free harness cannot reach, and the reason the slab's remaining gap — a re-upload
    // of the live range whenever a band arrives — is still an open question rather than a settled
    // one. Measured here by the frame that follows, not by `setProps`, which returns before any of
    // it has happened.
    if (trace.enabled) {
      requestAnimationFrame(() =>
        trace.event('painted', {ms: performance.now() - handed, n: markSlab.drawn})
      );
    }
    paints += 1;
    window.__tesseraProbe = {
      paints,
      at: performance.now(),
      marks: markSlab.drawn + (store.state.assembled?.provisional ?? 0),
      requests: requestCount
    };
  })
);
store.subscribe(rerender);
panels.addEventListener('focusout', () => setTimeout(rerender, 0));

// A pan brings marks carrying codes the legend has not seen. Resolving on every store change
// rather than only on a response is deliberate: the coloured column can also change without one,
// and `resolveCategoryCodes` is a no-op once every drawn code is held, so the common case costs a
// set walk and no request.
//
// **Throttled, because the counting is a fold over every held band.** Keying the memo on the
// assembled object re-counted on every arrival and every fold — measured in a Firefox profile at
// 2.16 s of a 12.9 s recording, 24% of the main thread's entire CPU, none of it visible to the
// trace because this subscription had no phase. A new code can only appear when new bands arrive,
// and a legend that gains a colour half a second late is imperceptible; a frame spent counting is
// not.
// **And deferred to a quiet moment, not merely rate-limited.** The memo only helps re-counts;
// during arrival-heavy zooming most bands are being counted for the first time, so a throttled
// count still lands a 25-70 ms scan inside a gesture frame. The count feeds nothing but the
// palette, so it runs when the store has been quiet for a beat — the legend gains its colours as
// the gesture ends, which is also when anyone looks at it.
const LEGEND_RECOUNT_MS = 500;
let lastResolved: {column: string; version: number} | null = null;
let legendTimer: ReturnType<typeof setTimeout> | null = null;
store.subscribe(() => {
  const {colourBy, assembled} = store.state;
  if (!colourBy || !assembled) return;
  if (lastResolved && lastResolved.column === colourBy && lastResolved.version === assembled.version)
    return;
  // Re-armed on every change, so it fires once things settle rather than mid-stream.
  if (legendTimer) clearTimeout(legendTimer);
  legendTimer = setTimeout(() => {
    legendTimer = null;
    const now = store.state;
    if (!now.colourBy || !now.assembled) return;
    lastResolved = {column: now.colourBy, version: now.assembled.version};
    void resolveCategoryCodes(now.colourBy);
  }, LEGEND_RECOUNT_MS);
});

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
    (req, signal) => {
      requestCount += 1;
      return client.viewport(store.state.session!.token, {...req, slice: store.state.slice}, signal);
    },
    meta.quantisation,
    {
      slice: meta.slices[0]!.id,
      onPhase: trace.enabled ? (kind, ms, n) => trace.event(kind, {ms, n}) : undefined
    }
  );
  // `?prefetch=0` turns look-ahead off without touching the replica — the A/B the measurement
  // wants, and the switch an operator watching aggregate select CPU would reach for.
  const prefetch = new URLSearchParams(location.search).get('prefetch') !== '0';
  controller = new ViewportController(store, replica, prefetch);
  trace.event('session', {
    prefetch: prefetch ? 1 : 0,
    kMaxMarks: meta.selection.kMaxMarks,
    budget: store.state.budget,
    maxTiles: meta.maxTilesPerRequest
  });
  controller.schedule(currentView, mapEl.clientWidth, mapEl.clientHeight);
}

start().catch((error) => {
  panels.innerHTML = `<section class="panel"><h2>Startup failed</h2>
    <div class="bad">${esc(error)}</div>
    <div class="muted">Is <code>tessera serve</code> running, and is this origin listed in
    <code>serve.dev_cors_origins</code>?</div></section>`;
});
