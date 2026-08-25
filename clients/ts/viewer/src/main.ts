import {Deck} from '@deck.gl/core';
import {
  TesseraClient,
  createStore,
  worldBbox,
  type FilterDraft,
  type Store as DataStore
} from '@tesseradb/client';
import {loadDatasets, readConfig, type Dataset} from './config.js';
import {esc} from './html.js';
import {renderErrors} from './panels/errors.js';
import {renderItem, renderItemError} from './panels/item.js';
import {renderSource} from './panels/source.js';
import {renderFilters} from './panels/filters.js';
import {renderLegend} from './panels/legend.js';
import {renderStats, toggleStatsDrawer} from './panels/stats.js';
import {renderCounts, renderDepth} from './panels/view.js';
import {renderArtifactDetail, renderArtifacts, renderLayerControl} from './panels/layers.js';
import {loadArtifactPlaces} from './artifacts.js';
import {materialise} from './assemble.js';
import {dateToMicros, isDateColumn, type TextMode} from './filters.js';
import {MarkSlab} from './slab.js';
import {installTrace, installTraceBar, trace} from './trace.js';
import {coalesce, createStore as createAppState, type Store} from './state.js';
import {
  INITIAL_VIEW_STATE,
  VIEW,
  buildViewportLayers,
  encodingSignature,
  type ViewState
} from './viewportLayer.js';

/**
 * The demo, rebuilt on the store.
 *
 * **What moved.** The whole data path — the replica, the driver and the presented frame, the
 * artifact channel, the encoding accumulators, item and artifact detail, filter composition and
 * category resolution — lives in `@tesseradb/client`'s `createStore` now (design client-components
 * §4). What remains here is the vis and the instruments: the deck binding and its slab, the panels,
 * the trace bar, and the dataset/principal switching. The panels read a mirror of the store's
 * projections held in the demo's own {@link AppState}, plus the measurement numbers the §4 surface
 * deliberately omits, which the store forwards on its demo-only `instruments` channel.
 */

const config = readConfig();
/**
 * The persistent mark buffer, owned here because it outlives every frame and every response.
 *
 * One per document: it is GPU-facing storage, not view state, and putting it in the store would
 * make every subscriber think a redraw had changed something when the whole point is that it
 * usually has not.
 */
const markSlab = new MarkSlab();

/**
 * The session client and the store, both of which belong to **one dataset and one principal**.
 *
 * The client carries the session credential and is what `authorise` runs through (design §5.4: the
 * store never holds a session credential); the store is handed a token supplier over it. A
 * principal switch disposes the store and opens a new one — a different mask is a different
 * partition, and nothing held under the old one may be drawn under the new.
 */
let client: TesseraClient | null = null;
let dataStore: DataStore | null = null;
let unsubscribe: (() => void) | null = null;
let datasets: Dataset[] = [];
/** The presets of the dataset currently active — per bundle, since a term id is per dictionary. */
let presets: Dataset['presets'] = [];

/** The slider's own maximum — see `panels/view.ts`. */
const DEFAULT_BUDGET = 500_000;
/** A declared category column, so the palette, the legend and `/v1/categories` are all live. */
const DEFAULT_COLOUR_BY = 'archive';
/**
 * How long a filter control must be quiet before its change is sent.
 *
 * A filtered selection turns over completely on every keystroke, so an un-debounced text box would
 * issue a full re-selection per character. Long enough to swallow typing, short enough that a
 * finished word lands before you look up.
 */
const FILTER_DEBOUNCE_MS = 350;

const store = createAppState({
  meta: null,
  session: null,
  view: '',
  datasetId: '',
  switching: false,
  termsLabel: '',
  terms: [],
  filters: {},
  filterValues: {},
  filterValueErrors: {},
  k: undefined,
  underlayOffset: 0,
  assembled: null,
  sessionWarm: false,
  status: 'idle',
  lastError: null,
  depthChoice: null,
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
  lastPick: null,
  colourBy: DEFAULT_COLOUR_BY,
  categories: {},
  categoryErrors: {},
  ranks: {},
  domains: {},
  artifactLayer: null,
  artifacts: [],
  artifactVersion: 0,
  artifactPlaces: new Map(),
  artifactStatus: 'idle',
  artifactError: null,
  selectedArtifact: null,
  artifactDetailError: null
});

const mapEl = document.getElementById('map') as HTMLDivElement;
/** The two panel columns: what you change on the left, what you read on the right. */
const controlsEl = document.getElementById('controls')!;
const statsEl = document.getElementById('stats')!;

let currentView: ViewState = {target: INITIAL_VIEW_STATE.target, zoom: INITIAL_VIEW_STATE.zoom};

/**
 * Tell the store where the camera is looking, in **data coordinates**.
 *
 * C2's engine owns the camera; the store is told (design §4). deck gives a world-space
 * `{target, zoom}`, so this converts it to the view's world bbox and then to data coordinates
 * through the store's own `dataXY`, which is the space `setView` takes. Before `meta` there is no
 * quantisation to convert through, so the call is skipped — the store replays the first view once
 * `meta` lands (its queued-before-meta rule).
 */
function pushView(): void {
  if (!dataStore || !dataStore.get('meta')) return;
  const width = mapEl.clientWidth;
  const height = mapEl.clientHeight;
  const wb = worldBbox({target: [currentView.target[0], currentView.target[1]], zoom: currentView.zoom, width, height}, 1);
  const [x0, y0] = dataStore.dataXY(wb[0], wb[1]);
  const [x1, y1] = dataStore.dataXY(wb[2], wb[3]);
  dataStore.setView({bbox: [x0, y0, x1, y1], width, height});
}

const deck = new Deck({
  parent: mapEl,
  onDeviceInitialized: (device) => {
    if (config.gpuBuffers) markSlab.attach(device);
  },
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
    pushView();
    return viewState;
  },
  onClick: (info) => {
    const layer = info.sourceLayer ?? info.layer;
    // A cluster ring answered the pick — a different question, on its own route.
    const artifactIds = (layer?.props as {artifactIds?: bigint[]} | undefined)?.artifactIds;
    if (artifactIds && info.index >= 0 && info.index < artifactIds.length) {
      void dataStore?.openArtifact(artifactIds[info.index]!);
      return;
    }
    const ids = (layer?.props as {tesseraIds?: BigUint64Array} | undefined)?.tesseraIds;
    const id = ids && info.index >= 0 && info.index < ids.length ? ids[info.index] : undefined;
    /**
     * What the pick returned, resolved or not — a miss and a broken pick are different failures,
     * and the item panel shows which. `index` is deck's hit index, `layer` which layer answered.
     */
    const pick = {index: info.index, layer: layer?.id ?? null, hasIds: ids !== undefined, idCount: ids?.length ?? 0};
    const worldXY = info.coordinate
      ? ([info.coordinate[0]!, info.coordinate[1]!] as [number, number])
      : null;
    store.update((s) => {
      s.lastPick = pick;
      s.selectedWorldXY = id === undefined ? null : worldXY;
    });
    if (id === undefined || !dataStore) return;
    void dataStore.pick(id);
  }
});

// ---------------------------------------------------------------------------------- the panels

/** The left column: everything that changes what is asked for. */
function renderControls(): string {
  return (
    renderSource(store.state, datasets, presets) +
    renderLayerControl(store.state) +
    renderFilters(store.state) +
    renderLegend(store.state)
  );
}

/** The right column: everything that reports what came back. */
function renderReadouts(): string {
  return (
    renderCounts(store.state) +
    renderArtifacts(store.state) +
    (store.state.selectedArtifact || store.state.artifactDetailError
      ? renderArtifactDetail(store.state)
      : '') +
    renderStats(store.state, {drawn: markSlab.drawn, departed: markSlab.departed}) +
    renderDepth(store.state) +
    (store.state.itemError
      ? renderItemError(store.state.itemError.code, store.state.itemError.detail)
      : renderItem(store.state)) +
    renderErrors(store.state)
  );
}

/**
 * Everything the control column's markup depends on, as one string — rebuilt only when it moves,
 * so a rebuild never replaces the element under a user's cursor between mouse-down and click.
 */
function controlsSignature(): string {
  const s = store.state;
  const colour = s.colourBy ?? '';
  return [
    s.datasetId,
    s.switching ? '1' : '0',
    s.termsLabel,
    s.terms.length,
    s.meta ? s.meta.filterOperands.length : -1,
    JSON.stringify(s.filters),
    Object.entries(s.filterValues)
      .map(([k, v]) => `${k}:${v.length}`)
      .join(','),
    Object.keys(s.filterValueErrors).join(','),
    colour,
    s.categories[colour]?.length ?? -1,
    Object.keys(s.ranks[colour] ?? {}).length,
    s.categoryErrors[colour]?.code ?? '',
    s.domains[colour] ? `${s.domains[colour]!.min}..${s.domains[colour]!.max}` : '',
    s.budget,
    s.artifactLayer ?? '',
    s.meta?.layers.length ?? -1
  ].join('|');
}

let controlsPainted = '';

/** Rebuild the control column, carrying focus, caret and scroll across the rebuild. */
function repaintControls() {
  const active = document.activeElement as HTMLElement | null;
  const focusId = active && controlsEl.contains(active) ? active.id : null;
  const text =
    active instanceof HTMLInputElement && active.type === 'search'
      ? {start: active.selectionStart, end: active.selectionEnd}
      : null;
  const scrolled = Array.from(controlsEl.querySelectorAll<HTMLElement>('[data-checks]')).map((el) => [
    el.dataset.checks!,
    el.scrollTop
  ]) as [string, number][];

  controlsEl.innerHTML = renderControls();
  bindControls();

  for (const [column, top] of scrolled) {
    const el = controlsEl.querySelector<HTMLElement>(`[data-checks="${CSS.escape(column)}"]`);
    if (el) el.scrollTop = top;
  }
  if (!focusId) return;
  const restored = document.getElementById(focusId);
  if (!restored) return;
  restored.focus({preventScroll: true});
  if (text && restored instanceof HTMLInputElement && text.start !== null) {
    restored.setSelectionRange(text.start, text.end);
  }
}

/** Rebuild the panels — readouts every change, controls only when their signature moves. */
function render() {
  statsEl.innerHTML = renderReadouts();
  const drawer = document.getElementById('stats-drawer') as HTMLDetailsElement | null;
  drawer?.addEventListener('toggle', () => toggleStatsDrawer(drawer.open));

  const signature = controlsSignature();
  if (signature === controlsPainted) return;
  controlsPainted = signature;
  repaintControls();
}

function bindControls() {
  const dataset = document.getElementById('dataset') as HTMLSelectElement | null;
  dataset?.addEventListener('change', () => {
    const chosen = datasets.find((d) => d.id === dataset.value);
    if (chosen) void activate(chosen);
  });

  const select = document.getElementById('principal') as HTMLSelectElement | null;
  select?.addEventListener('change', () => {
    const preset = presets[Number(select.value)];
    if (!preset) return;
    trace.event('principal', {label: preset.label, n: preset.terms.length});
    // A different principal is a different mask, a different partition and a different visible set:
    // open a fresh store on it. The old one's replica, encoding accumulators and held artifacts go
    // with it, which is exactly what a mask change requires.
    openSession(preset);
  });

  const artifactLayer = document.getElementById('artifact-layer') as HTMLSelectElement | null;
  artifactLayer?.addEventListener('change', () => {
    const chosen = artifactLayer.value === '' ? null : artifactLayer.value;
    trace.event('layer', {name: chosen ?? 'none'});
    dataStore?.setLayers(chosen ? [chosen] : []);
  });

  bindFilterControls();

  const colourBy = document.getElementById('colour-by') as HTMLSelectElement | null;
  colourBy?.addEventListener('change', () => {
    const chosen = colourBy.value === '' ? null : colourBy.value;
    // **No refetch.** Every rendered column is already in the held response, so this is an encoding
    // pass over what is drawn — the switch a viewer can run as a check on I7, since the mark count
    // cannot move because no request is made.
    dataStore?.setColourBy(chosen);
  });

  const budgetInput = document.getElementById('budget') as HTMLInputElement | null;
  budgetInput?.addEventListener('change', () => {
    trace.event('budget', {n: Number(budgetInput.value)});
    store.update((s) => {
      s.budget = Number(budgetInput.value);
    });
    dataStore?.setBudget(Number(budgetInput.value));
  });
}

let filterTimer: ReturnType<typeof setTimeout> | null = null;
/** Debounced for typing; a tick or a date lands immediately, having no intermediate states. */
function scheduleFilters(immediate = false) {
  if (filterTimer) clearTimeout(filterTimer);
  const apply = () => {
    filterTimer = null;
    trace.event('filters', {n: Object.keys(store.state.filters).length});
    // The store drops what it holds and re-asks — the identity key excludes filters, so held bands
    // would otherwise be served under the new question (design §4).
    dataStore?.setFilters(store.state.filters as FilterDraft);
  };
  if (immediate) return apply();
  filterTimer = setTimeout(apply, FILTER_DEBOUNCE_MS);
}

/**
 * Wire the filter controls. Every handler writes the *draft* in the demo's own app state and then
 * schedules; the store composes the expression — the composition has one home now (`@tesseradb/client`).
 */
function bindFilterControls() {
  for (const [column, draft] of Object.entries(store.state.filters)) {
    if (draft.family === 'text') {
      const box = document.getElementById(`flt-text-${column}`) as HTMLInputElement | null;
      box?.addEventListener('input', () => {
        store.update((s) => {
          const d = s.filters[column];
          if (d?.family === 'text') d.query = box.value;
        });
        scheduleFilters();
      });
      const mode = document.getElementById(`flt-mode-${column}`) as HTMLSelectElement | null;
      mode?.addEventListener('change', () => {
        store.update((s) => {
          const d = s.filters[column];
          if (d?.family === 'text') d.mode = mode.value as TextMode;
        });
        scheduleFilters(true);
      });
    }

    if (draft.family === 'string' || draft.family === 'keyword') {
      const box = document.getElementById(`flt-str-${column}`) as HTMLInputElement | null;
      box?.addEventListener('input', () => {
        store.update((s) => {
          const d = s.filters[column];
          if (d?.family === 'string' || d?.family === 'keyword') d.needle = box.value;
        });
        scheduleFilters();
      });
      const op = document.getElementById(`flt-op-${column}`) as HTMLSelectElement | null;
      op?.addEventListener('change', () => {
        store.update((s) => {
          const d = s.filters[column];
          if (d?.family === 'string' || d?.family === 'keyword') {
            d.op = op.value as 'eq' | 'prefix' | 'contains';
          }
        });
        scheduleFilters(true);
      });
    }

    if (draft.family === 'numeric') {
      const date = isDateColumn(store.state.meta, column);
      for (const bound of ['gte', 'lte'] as const) {
        const box = document.getElementById(`flt-${bound}-${column}`) as HTMLInputElement | null;
        box?.addEventListener('change', () => {
          const value = date ? dateToMicros(box.value) : box.value === '' ? null : Number(box.value);
          store.update((s) => {
            const d = s.filters[column];
            if (d?.family === 'numeric') d[bound] = Number.isFinite(value) ? value : null;
          });
          scheduleFilters(true);
        });
      }
    }
  }

  controlsEl.querySelectorAll<HTMLElement>('[data-checks]').forEach((box) => {
    box.addEventListener('change', (event) => {
      const input = event.target as HTMLInputElement;
      const column = input.dataset.cat;
      if (!column) return;
      store.update((s) => {
        const d = s.filters[column];
        if (d?.family !== 'category') return;
        d.keys = input.checked ? [...d.keys, input.value] : d.keys.filter((k) => k !== input.value);
      });
      scheduleFilters(true);
    });
  });

  const clear = document.getElementById('filters-clear') as HTMLButtonElement | null;
  clear?.addEventListener('click', () => {
    // Clear every draft to its empty shape, then apply. The store re-seeds `filters` on the next
    // projection tick, so the drafts stay in schema order.
    store.update((s) => {
      for (const d of Object.values(s.filters)) {
        if (d.family === 'text') d.query = '';
        else if (d.family === 'string' || d.family === 'keyword') d.needle = '';
        else if (d.family === 'category') d.keys = [];
        else if (d.family === 'numeric') {
          d.gte = null;
          d.lte = null;
        }
      }
    });
    scheduleFilters(true);
  });
}

installTrace(mapEl);
installTraceBar();

const rerender = coalesce(() => trace.phase('panels', render));

declare global {
  interface Window {
    __tesseraProbe?: {
      paints: number;
      at: number;
      marks: number;
      requests: number;
      encoding: string;
    };
  }
}
let paints = 0;
let requestCount = 0;
/** What the last paint actually drew — the store changes far more often than the picture does. */
let painted = '';
store.subscribe(
  coalesce(() => {
    const view = store.state;
    const drawing = `${view.status}|${view.assembled?.version ?? -1}|${
      view.assembled?.depth ?? -1
    }|${markSlab.drawn}|${view.assembled?.provisional ?? 0}|${encodingSignature(store)}|${
      view.selectedWorldXY?.join(',') ?? ''
    }|${view.artifactVersion}|${view.artifactLayer ?? ''}|${view.selectedArtifact?.id ?? ''}`;
    if (drawing === painted) return;
    painted = drawing;

    const built = trace.phase('layers', () => buildViewportLayers(store, markSlab));
    const handed = performance.now();
    deck.setProps({layers: built});
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
      requests: requestCount,
      encoding: encodingSignature(store)
    };
  })
);
store.subscribe(rerender);

// ------------------------------------------------------------------- mirroring the store's projections

/** The composition last materialised, so a mirror tick rebuilds the buffers only when it moved. */
let lastComposition: object | null = null;

/**
 * Copy the store's projections into the demo's app state, so the panels — which read app state —
 * see one consistent picture per tick. The data-path *decisions* are all the store's; this is only
 * the demo's mirror of them.
 */
function mirror(): void {
  const ds = dataStore;
  if (!ds) return;
  const meta = ds.get('meta');
  const status = ds.get('status');
  const view = ds.get('view');
  const legend = ds.get('legend');
  const filters = ds.get('filters');
  const artifacts = ds.get('artifacts');
  const selection = ds.get('selection');
  const replica = ds.get('replica');

  let assembled = store.state.assembled;
  if (view.composition !== lastComposition) {
    lastComposition = view.composition;
    assembled = view.composition
      ? materialise(view.composition, store.state.assembled, legend.colourBy ? [legend.colourBy] : [])
      : null;
  }

  store.update((s) => {
    s.meta = meta;
    s.status = status.status === 'idle' && s.switching ? 'idle' : status.status;
    s.sessionWarm = status.sessionWarm;
    s.lastError = status.refusal;
    s.inFlight = status.status === 'loading' ? 1 : 0;
    s.assembled = assembled;
    // The filter draft is the store's, re-seeded from `/v1/meta`; the demo edits a copy in place,
    // so adopt the store's shape only when the demo has none (first meta) to keep the user's typing.
    if (Object.keys(s.filters).length === 0) s.filters = filters.draft as typeof s.filters;
    s.filterValues = filters.values as typeof s.filterValues;
    s.filterValueErrors = filters.valueErrors as typeof s.filterValueErrors;
    s.colourBy = legend.colourBy;
    s.ranks = legend.ranks as typeof s.ranks;
    s.domains = legend.domains as typeof s.domains;
    s.categories = legend.categories as typeof s.categories;
    s.categoryErrors = legend.categoryErrors as typeof s.categoryErrors;
    if (s.artifacts !== artifacts.served) s.artifactVersion += 1;
    s.artifacts = artifacts.served;
    s.artifactLayer = artifacts.layer;
    s.artifactStatus = artifacts.status === 'shown' ? 'shown' : artifacts.status;
    s.artifactError = artifacts.refusal;
    s.selected = selection.item ? {id: selection.item.id, fields: selection.item.detail.fields, externalId: selection.item.detail.externalId} : null;
    s.itemError = selection.itemRefusal;
    s.selectedArtifact = selection.artifact ? {...selection.artifact.detail, id: selection.artifact.id} : null;
    s.artifactDetailError = selection.artifactRefusal;
    s.replicaBytes = replica.bytes;
    s.replicaPoints = replica.points;
    s.replicaBands = replica.bands;
    if (replica.lastPlan) s.lastPlan = {omitted: replica.lastPlan.held, fetched: replica.lastPlan.fetched};
  });
}

// ------------------------------------------------------------------------------- dataset and principal

/** Open a store on one principal of the active dataset — the only writer of `dataStore`. */
function openSession(preset: Dataset['presets'][number]): void {
  if (!client) return;
  const active = client;
  unsubscribe?.();
  dataStore?.dispose();
  markSlab.clear();
  lastComposition = null;

  store.update((s) => {
    s.terms = preset.terms;
    s.termsLabel = preset.label;
    s.assembled = null;
    s.sessionWarm = false;
    s.status = 'idle';
    s.selected = null;
    s.selectedWorldXY = null;
    s.itemError = null;
    s.selectedArtifact = null;
    s.artifactDetailError = null;
    // A different mask is a different set of values: the demo's mirror of them is cleared and the
    // store's fresh projections re-seed it.
    s.categories = {};
    s.categoryErrors = {};
    s.ranks = {};
    s.domains = {};
    s.filters = {};
    s.filterValues = {};
    s.filterValueErrors = {};
    s.artifacts = [];
    s.artifactVersion += 1;
    s.artifactStatus = 'idle';
    s.artifactError = null;
  });

  const prefetch = new URLSearchParams(location.search).get('prefetch') !== '0';
  dataStore = createStore({
    viewerUrl: '',
    client: active,
    // The store never holds the session credential: it is handed a supplier that re-authorises
    // through the demo's own client (design §5.3, §5.4). Renewal runs before the first refusal.
    authorise: async () => {
      const session = await active.authorise(preset.terms);
      store.update((s) => {
        s.session = session;
      });
      return {token: session.token, expiresAt: session.expiresAt};
    },
    budget: store.state.budget,
    prefetch,
    driver: {prefetchLayers: config.prefetchLayers},
    instruments: {
      onFrame: (info) => {
        store.update((s) => {
          s.depthChoice = {...info.plan.choice, requestedAt: Date.now()};
          s.mTarget = info.calibration.mTarget;
          if (info.calibration.visibleInView !== undefined) s.lastVisibleInView = info.calibration.visibleInView;
          s.lastTimings = info.timings;
          s.lastBytes = info.bytes;
        });
      },
      onTrace: (kind, fields) => trace.event(kind, fields)
    }
  });
  dataStore.setColourBy(store.state.colourBy);
  dataStore.setLayers(store.state.artifactLayer ? [store.state.artifactLayer] : []);
  unsubscribe = dataStore.subscribe(() => mirror());
  // Seed the layer's value pickers and the first view once meta has landed.
  const armOnMeta = dataStore.subscribe('meta', (meta) => {
    if (!meta) return;
    for (const operand of meta.filterOperands) {
      if (operand.family === 'category') void dataStore?.loadFilterValues(operand.column);
    }
    pushView();
  });
  // The meta subscriber is one-shot in spirit; keep it in the unsubscribe chain.
  const priorUnsub = unsubscribe;
  unsubscribe = () => {
    priorUnsub();
    armOnMeta();
  };
  pushView();
}

/**
 * Point the viewer at a dataset: a fresh client and a fresh store.
 *
 * A `tessera_id` minted by one bundle means nothing to another, a term id names a different set in
 * each, and a held band carries geometry quantised under one bundle's extent — so a switch is a
 * clean rebuild, never a reuse.
 */
async function activate(dataset: Dataset): Promise<void> {
  unsubscribe?.();
  unsubscribe = null;
  dataStore?.dispose();
  dataStore = null;
  client?.close();
  markSlab.clear();
  lastComposition = null;
  presets = dataset.presets;

  client = new TesseraClient({
    viewerUrl: dataset.viewerUrl,
    sessionUrl: dataset.sessionUrl,
    sessionCredential: config.sessionCredential
  });

  /** The demo opens on its **hardest** case: the broadest principal available. */
  const first = presets.reduce<Dataset['presets'][number] | undefined>(
    (best, p) => (best && best.visible >= p.visible ? best : p),
    undefined
  );

  store.update((s) => {
    s.datasetId = dataset.id;
    s.switching = true;
    s.meta = null;
    s.session = null;
    s.terms = first?.terms ?? [];
    s.termsLabel = first?.label ?? '';
    s.assembled = null;
    s.sessionWarm = false;
    s.status = 'idle';
    s.failures = [];
    s.lastPlan = null;
    s.latency = null;
    s.lastTimings = null;
    s.filters = {};
    s.filterValues = {};
    s.filterValueErrors = {};
    // Colour by the default if this bundle renders it, else its first rendered column — deferred to
    // the first meta tick, which knows the schema; seed the intent here.
    s.colourBy = DEFAULT_COLOUR_BY;
    s.artifactLayer = null;
    s.artifacts = [];
    s.artifactVersion += 1;
    s.artifactStatus = 'idle';
    s.artifactError = null;
    s.selectedArtifact = null;
    s.artifactDetailError = null;
  });

  // Choose the colour and layer defaults once meta is known, before opening the session's store, so
  // the store opens already pointed at them. A throwaway meta fetch through the client.
  if (first) {
    const session = await client.authorise(first.terms);
    const meta = await client.meta(session.token);
    const rendered = meta.declaredScalars.filter((c) => c.render);
    store.update((s) => {
      s.session = session;
      s.meta = meta;
      s.view = meta.views[0]!.id;
      s.mTarget = meta.selection.thetaTargetMarks;
      s.colourBy = rendered.some((c) => c.name === DEFAULT_COLOUR_BY)
        ? DEFAULT_COLOUR_BY
        : (rendered.find((c) => c.category)?.name ?? rendered[0]?.name ?? null);
      s.artifactLayer = meta.layers[0]?.name ?? null;
      s.switching = false;
    });
    trace.event('session', {
      dataset: dataset.id,
      kMaxMarks: meta.selection.kMaxMarks,
      budget: store.state.budget,
      maxTiles: meta.maxTilesPerRequest
    });
    void loadArtifactPlaces().then((places) => {
      store.update((s) => {
        s.artifactPlaces = places;
        s.artifactVersion += 1;
      });
    });
    openSession(first);
  } else {
    store.update((s) => {
      s.switching = false;
    });
  }
}

async function start() {
  datasets = await loadDatasets();
  const requested = new URLSearchParams(location.search).get('dataset');
  const chosen = datasets.find((d) => d.id === requested) ?? datasets[0]!;
  await activate(chosen);
}

start().catch((error) => {
  statsEl.innerHTML = `<section class="panel"><h2>Startup failed</h2>
    <div class="bad">${esc(error)}</div>
    <div class="muted">Is <code>tessera serve</code> running, and is this origin listed in
    <code>serve.dev_cors_origins</code>?</div></section>`;
});
