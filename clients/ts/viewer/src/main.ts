import '@tesseradb/components';
import type {TesseraExplorer, MapProbe} from '@tesseradb/components';
import {TesseraClient, createStore, dataToWorldXY, type Store as DataStore} from '@tesseradb/client';
import type {ViewInfo} from '@tesseradb/client';
import {basemapLayer, coverFor, type BasemapCover, type Camera} from './basemap.js';
import {loadDatasets, readConfig, type Dataset} from './config.js';
import {esc} from './html.js';
import {renderErrors} from './panels/errors.js';
import {renderSource} from './panels/source.js';
import {renderStats, toggleStatsDrawer} from './panels/stats.js';
import {renderDepth} from './panels/view.js';
import {installTrace, installTraceBar, trace} from './trace.js';
import {coalesce, createStore as createAppState, type Store} from './state.js';

/**
 * The demo: `<tessera-explorer layout="overlay">` plus its instruments (design client-components
 * §7).
 *
 * **What the explorer is.** The map, the status strip, the filters, the selection panel and the
 * item card are `@tesseradb/components`, reading one store by context. The demo hands the explorer
 * the store it built — the `.store` property, first in the precedence — because the store is
 * opened per dataset and principal through the demo's own session client, which is where
 * `session-url` and the session credential stay (§5.3: never on a C1 surface).
 *
 * **What the instruments are.** The things that measure rather than show: the dataset and
 * principal pickers, the mark budget, the depth the budget chose, the last request's timings and
 * the replica drawer, and the refusals observed. The layer picker, the legend, the artifact list
 * and the artifact card are the explorer's own (§5.3). The instruments read a mirror of the
 * store's projections plus the numbers the §4 surface deliberately omits, which the store
 * forwards on its demo-only `instruments` channel; the probe on `window` carries the three lanes'
 * timings — decode in the worker, absorb on this thread, the region's counting request — for the
 * harness.
 */

const config = readConfig();

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

const store = createAppState({
  meta: null,
  session: null,
  view: '',
  datasetId: '',
  switching: false,
  termsLabel: '',
  terms: [],
  frame: null,
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
  colourBy: DEFAULT_COLOUR_BY,
  artifactLayer: null
});

const explorer = document.getElementById('explorer') as TesseraExplorer;
const instrumentsEl = document.getElementById('instruments')!;

declare global {
  interface Window {
    __tesseraProbe?: MapProbe & {lanes: Lanes};
  }
}

/**
 * How long the camera must sit still before the ground under it is recomposed. Long enough that a
 * wheel gesture composes its destination and not every notch on the way.
 */
const BASEMAP_SETTLE_MS = 180;

/**
 * Draw a basemap under the points where `/v1/meta` says one lines up, and none where it does not.
 *
 * The decision is `tile_scheme`'s and the demo does not second-guess it: no dataset entry says
 * whether its corpus is geographic, and nothing here reads the extent. A tile that will not load —
 * no network, a refused request — leaves the map exactly as it was, a basemap being an underlay
 * and not the picture.
 */
let basemapGeneration = 0;

async function installBasemap(view: ViewInfo, camera?: Camera): Promise<void> {
  // **A basemap is only ever installed for the switch that asked for it.** Both awaits below run
  // for as long as a tile fetch takes, and a viewer stepping geographic → embedding →
  // geographic-2 faster than that would otherwise have view A's tiles land under view C's points.
  // The generation is the same guard `activate` uses for its meta.
  const mine = ++basemapGeneration;
  await explorer.updateComplete;
  const map = explorer.map;
  if (!map || mine !== basemapGeneration) return;
  try {
    const basemap = await basemapLayer(view, camera);
    if (mine !== basemapGeneration) return;
    map.basemap = basemap;
    // **The ground under the labels is the basemap's, not the page's.** OpenStreetMap's standard
    // style is a pale one, and the demo's chrome is dark — so with the ground left to
    // `color-scheme` the names came out white in a black halo over a light street map, which is
    // the one combination that reads as a rendering fault rather than a choice. The panels stay
    // dark; only the canvas answers to what is behind it.
    map.ground = basemap ? 'light' : '';
  } catch (error) {
    store.update((s) => {
      s.failures = [...s.failures.slice(-19), {code: 'basemap', detail: String(error), at: Date.now()}];
    });
  }
}

/**
 * Keep the ground at the resolution the camera is at.
 *
 * **Composed per view, and only when the view left the tiles it was composed from.** A pan inside
 * the covered box and a zoom that does not change the depth need no new texture, so a gesture that
 * stays put costs nothing; what a recomposition costs is the tiles it has not already decoded.
 *
 * Debounced on the view *settling* rather than driven per frame: the wheel emits a view change per
 * notch, and composing a texture per notch would fetch every level between the two ends of the
 * gesture to draw none of them.
 *
 * **Bound to the current view, and rebound at a switch** (`view-switching.md` §6.5): one listener
 * for the page's life, reading the view it follows from a variable a switch replaces, so a
 * settle after a switch composes against the new view's frame and never the one it left.
 */
let followedView: ViewInfo | null = null;
let composed: BasemapCover | null = null;
let lastCamera: Camera | null = null;
let followTimer: ReturnType<typeof setTimeout> | null = null;
let followListening = false;

function followCameraWithBasemap(view: ViewInfo): void {
  followedView = view;
  composed = null;
  if (followTimer !== null) {
    clearTimeout(followTimer);
    followTimer = null;
  }
  if (followListening) return;
  followListening = true;
  explorer.addEventListener('tessera-viewchange', (event) => {
    const followed = followedView;
    // A view whose frame is aligned to no tiling has no ground to follow (`tile` is null exactly
    // when `tileScheme` is): every embedding view is this case, and composing for it threw from
    // inside the store's own fan-out, which starved every subscriber behind the map.
    if (!followed || followed.tileScheme === null || followed.tile === null) return;
    const {bbox, zoom} = (event as CustomEvent<{bbox: [number, number, number, number]; zoom: number}>).detail;
    const [x0, y0] = dataToWorldXY(bbox[0], bbox[1], followed.quantisation);
    const [x1, y1] = dataToWorldXY(bbox[2], bbox[3], followed.quantisation);
    const camera: Camera = {
      worldBox: [Math.min(x0, x1), Math.min(y0, y1), Math.max(x0, x1), Math.max(y0, y1)],
      zoom
    };
    lastCamera = camera;
    const wanted = coverFor(followed, camera);
    if (
      composed &&
      composed.level === wanted.level &&
      composed.x0 === wanted.x0 &&
      composed.y0 === wanted.y0 &&
      composed.x1 === wanted.x1 &&
      composed.y1 === wanted.y1
    ) {
      return;
    }
    if (followTimer !== null) clearTimeout(followTimer);
    followTimer = setTimeout(() => {
      followTimer = null;
      composed = wanted;
      void installBasemap(followed, camera);
    }, BASEMAP_SETTLE_MS);
  });
}

/**
 * Take the basemap down, now, and stand every fetch in flight down with it.
 *
 * Called at the switch rather than when the next one arrives: a view with a `tile_scheme` of
 * `null` has no basemap to replace the old one with, so leaving the tiles up until an answer
 * comes would draw a map of the world under an embedding — and for as long as the fetch takes,
 * which is the whole of what a viewer sees of the switch.
 */
function dropBasemap(): void {
  basemapGeneration += 1;
  composed = null;
  if (followTimer !== null) {
    clearTimeout(followTimer);
    followTimer = null;
  }
  const map = explorer.map;
  if (map) {
    map.basemap = null;
    map.ground = '';
  }
}

/**
 * The current view is URL state (`view-switching.md` §6.5): `?view=<id>` beside `?dataset=`, so a
 * link means what it showed. Written with `replaceState` — a switch is not a page in the history,
 * and a slider run through a group's roster would otherwise leave one entry per step behind it.
 */
function writeViewToUrl(id: string): void {
  const url = new URL(location.href);
  if (url.searchParams.get('view') === id) return;
  url.searchParams.set('view', id);
  history.replaceState(null, '', url);
}

/**
 * Follow a switch: the basemap is decided **per switch** from the new view's `tile_scheme`, and
 * the URL says which view it is. A scheme of `null` means the basemap goes, which is what
 * `basemapLayer` answers with, so a switch from a geographic view to an embedding removes it
 * rather than leaving tiles under points that cannot line up with them.
 *
 * Within a group the camera does not move, so no settle will recompose the ground: the last
 * camera is composed for straight away. Across frames the map refits, and the settle that
 * follows composes for wherever it lands.
 */
function onViewChanged(id: string): void {
  const meta = store.state.meta;
  const view = meta?.views.find((v) => v.id === id);
  if (!meta || !view) return;
  const previous = meta.views.find((v) => v.id === store.state.view);
  const p = previous?.quantisation;
  const q = view.quantisation;
  const keepsFrame = p !== undefined && p.xMin === q.xMin && p.xMax === q.xMax && p.yMin === q.yMin && p.yMax === q.yMax;
  store.update((s) => {
    s.view = id;
  });
  writeViewToUrl(id);
  dropBasemap();
  followCameraWithBasemap(view);
  const camera = keepsFrame && view.tileScheme !== null && view.tile !== null ? lastCamera : null;
  if (camera) composed = coverFor(view, camera);
  void installBasemap(view, camera ?? undefined);
}

/** The first map's probe, published for the smoke scripts and the harness (§5.9). */
async function publishProbe(): Promise<void> {
  await explorer.updateComplete;
  const map = explorer.map;
  if (!map) return;
  const probe = map.probe as MapProbe & {lanes: Lanes};
  probe.lanes ??= {decode: [], absorb: {split: [], store: [], remap: [], remapPoints: [], sliceMaxMs: 0}, region: null, coverage: null, longTasks: []};
  window.__tesseraProbe = probe;
  observeLongTasks(probe.lanes);
}

/**
 * The main thread's long tasks, so a latency the lanes cannot explain — a response answered in
 * milliseconds and projected seconds later — can be laid against what blocked the thread and
 * when. Chromium's `longtask` entries, the ten longest kept.
 */
let longTasksObserved = false;
function observeLongTasks(lanes: Lanes): void {
  if (longTasksObserved || typeof PerformanceObserver === 'undefined') return;
  longTasksObserved = true;
  try {
    const observer = new PerformanceObserver((list) => {
      for (const entry of list.getEntries()) {
        lanes.longTasks.push({ms: entry.duration, at: entry.startTime});
        lanes.longTasks.sort((a, b) => b.ms - a.ms);
        if (lanes.longTasks.length > 10) lanes.longTasks.length = 10;
      }
    });
    observer.observe({type: 'longtask', buffered: true});
  } catch {
    // Not every runtime has the entry type; the lanes still say what they can.
  }
}

/** The three lanes' timings (design §5.10's measurement), kept on the probe for the harness. */
type Lanes = {
  decode: {ms: number; workerMs: number | null; points: number; bytes: number; at: number}[];
  absorb: {split: number[]; store: number[]; remap: number[]; remapPoints: number[]; sliceMaxMs: number};
  region: Record<string, number | string> | null;
  coverage: Record<string, number | string> | null;
  /** The ten longest main-thread tasks, ms and their start time on `performance.now()`'s clock. */
  longTasks: {ms: number; at: number}[];
};

// ---------------------------------------------------------------------------------- the panels

/** The controls: everything that changes what is asked for. */
function renderControls(): string {
  return renderSource(store.state, datasets, presets);
}

/** The readouts: everything that reports what came back. */
function renderReadouts(): string {
  const slab = explorer.map?.slab;
  return renderStats(store.state, {drawn: slab?.drawn ?? 0, departed: slab?.departed ?? 0}) + renderDepth(store.state) + renderErrors(store.state);
}

/**
 * Everything the controls' markup depends on, as one string — rebuilt only when it moves, so a
 * rebuild never replaces the element under a user's cursor between mouse-down and click.
 */
function controlsSignature(): string {
  const s = store.state;
  return [s.datasetId, s.switching ? '1' : '0', s.termsLabel, s.terms.length, s.budget].join('|');
}

let controlsPainted = '';
const controlsEl = document.createElement('div');
const readoutsEl = document.createElement('div');
instrumentsEl.append(controlsEl, readoutsEl);

function repaintControls() {
  const active = document.activeElement as HTMLElement | null;
  const focusId = active && controlsEl.contains(active) ? active.id : null;
  controlsEl.innerHTML = renderControls();
  bindControls();
  if (focusId) document.getElementById(focusId)?.focus({preventScroll: true});
}

/** Rebuild the instruments — readouts every change, controls only when their signature moves. */
function render() {
  readoutsEl.innerHTML = renderReadouts();
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
    // with it, which is exactly what a mask change requires — and every card empties (§9).
    openSession(preset);
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

installTrace(explorer);
installTraceBar();

const rerender = coalesce(() => trace.phase('panels', render));
store.subscribe(rerender);

// ------------------------------------------------------------------- mirroring the store's projections

/**
 * Copy the store's projections into the demo's app state, so the instrument panels see one
 * consistent picture per tick. The data-path decisions are all the store's; this is the demo's
 * mirror of them.
 */
function mirror(): void {
  const ds = dataStore;
  if (!ds) return;
  const meta = ds.get('meta');
  const status = ds.get('status');
  const view = ds.get('view');
  const replica = ds.get('replica');

  // The store is the one place a switch is decided (§6.3): the pickers, the notebook and a host
  // calling `setCurrentView` all land here, so the basemap and the URL follow the projection
  // rather than any one control's event.
  if (view.id && view.id !== store.state.view) onViewChanged(view.id);

  store.update((s) => {
    s.meta = meta;
    s.status = status.status === 'idle' && s.switching ? 'idle' : status.status;
    s.sessionWarm = status.sessionWarm;
    if (status.refusal && status.refusal !== s.lastError) {
      s.failures = [...s.failures.slice(-19), {...status.refusal, at: Date.now()}];
    }
    s.lastError = status.refusal;
    s.inFlight = status.status === 'loading' ? 1 : 0;
    s.frame = view.composition;
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

  store.update((s) => {
    s.terms = preset.terms;
    s.termsLabel = preset.label;
    s.frame = null;
    s.sessionWarm = false;
    s.status = 'idle';
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
    view: store.state.view,
    prefetch,
    driver: {prefetchLayers: config.prefetchLayers},
    replica: {
      // The absorb lane: the split and store phases per response, and the longest single slice —
      // the figure that says whether the slice budget held the thread (§5.10's measurement).
      onPhase: (kind, ms, n) => {
        const lanes = window.__tesseraProbe?.lanes;
        if (!lanes) return;
        if (kind === 'split' || kind === 'store' || kind === 'remap') lanes.absorb[kind].push(ms);
        if (kind === 'slice') lanes.absorb.sliceMaxMs = Math.max(lanes.absorb.sliceMaxMs, ms);
        for (const key of ['split', 'store', 'remap'] as const) if (lanes.absorb[key].length > 50) lanes.absorb[key].shift();
        if (kind === 'remap') lanes.absorb.remapPoints.push(n);
      }
    },
    instruments: {
      onFrame: (info) => {
        const probe = window.__tesseraProbe;
        if (probe) {
          probe.requests += 1;
          probe['instruments'] = {
            depth: info.plan.choice.depth,
            tiles: info.plan.choice.tiles,
            predictedMarks: info.plan.choice.predictedMarks,
            source: info.plan.choice.source,
            limitedBy: info.plan.choice.limitedBy,
            bytes: info.bytes
          };
        }
        store.update((s) => {
          s.depthChoice = {...info.plan.choice, requestedAt: Date.now()};
          s.mTarget = info.calibration.mTarget;
          if (info.calibration.visibleInView !== undefined) s.lastVisibleInView = info.calibration.visibleInView;
          s.lastTimings = info.timings;
          s.lastBytes = info.bytes;
        });
      },
      onTrace: (kind, fields) => {
        trace.event(kind, fields);
        const lanes = window.__tesseraProbe?.lanes;
        if (!lanes) return;
        // The region's counting request, in its three lanes, and the coverage check per settle.
        if (kind === 'region') lanes.region = {...fields};
        if (kind === 'coverage') lanes.coverage = {...fields};
      }
    }
  });
  dataStore.setColourBy(store.state.colourBy);
  // The demo opens with the first layer on: the smoke and the harness read counts off it.
  dataStore.setLayers(store.state.artifactLayer ? [store.state.artifactLayer] : []);
  unsubscribe = dataStore.subscribe(() => mirror());
  // The explorer takes the store by property — first in the precedence — and its map pushes the
  // first view once meta lands.
  explorer.store = dataStore;
  void publishProbe();
}

/**
 * Point the viewer at a dataset: a fresh client and a fresh store.
 *
 * A `tessera_id` minted by one bundle means nothing to another, a term id names a different set in
 * each, and a held band carries geometry quantised under one bundle's extent — so a switch is a
 * clean rebuild, never a reuse.
 */
/** Which `activate` is current: an earlier one that is still awaiting its meta stands down. */
let activation = 0;

async function activate(dataset: Dataset, requestedView: string | null = null): Promise<void> {
  const mine = ++activation;
  unsubscribe?.();
  unsubscribe = null;
  dataStore?.dispose();
  dataStore = null;
  client?.close();
  presets = dataset.presets;

  client = new TesseraClient({
    viewerUrl: dataset.viewerUrl,
    sessionUrl: dataset.sessionUrl,
    sessionCredential: config.sessionCredential,
    // Per-response decode time, for the harness's measurement (design §5.10): as seen from this
    // thread, and the worker's own — the difference is the lane's queue.
    onDecode: (ms, bytes, points, workerMs) => {
      const probe = window.__tesseraProbe;
      if (!probe) return;
      probe.timings.decodeMs.push(ms);
      if (probe.timings.decodeMs.length > 50) probe.timings.decodeMs.shift();
      probe.lanes.decode.push({ms, workerMs, points, bytes, at: performance.now()});
      if (probe.lanes.decode.length > 50) probe.lanes.decode.shift();
    }
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
    // A view id belongs to a bundle; the next one's is not known until its meta arrives.
    s.view = '';
    s.session = null;
    s.terms = first?.terms ?? [];
    s.termsLabel = first?.label ?? '';
    s.frame = null;
    s.sessionWarm = false;
    s.status = 'idle';
    s.failures = [];
    s.lastPlan = null;
    s.latency = null;
    s.lastTimings = null;
    s.colourBy = DEFAULT_COLOUR_BY;
    s.artifactLayer = null;
  });

  // Choose the colour and layer defaults once meta is known, before opening the session's store, so
  // the store opens already pointed at them. A throwaway meta fetch through the client.
  if (first) {
    let session: Awaited<ReturnType<TesseraClient['authorise']>>;
    let meta: Awaited<ReturnType<TesseraClient['meta']>>;
    try {
      session = await client.authorise(first.terms);
      meta = await client.meta(session.token);
    } catch (error) {
      // The picker's `change` handler cannot await this, so a refusal here used to vanish and
      // leave `switching` set for good — the panel said "establishing a session…" over an empty
      // map with nothing to say why. It is reported where every other refusal is.
      if (mine !== activation) return;
      const e = error as {code?: string; detail?: string; message?: string};
      store.update((s) => {
        s.switching = false;
        s.status = 'refused';
        s.lastError = {code: e.code ?? 'switch-failed', detail: e.detail ?? e.message ?? String(error)};
        s.failures = [...s.failures.slice(-19), {...s.lastError, at: Date.now()}];
      });
      return;
    }
    if (mine !== activation) return;
    const rendered = meta.declaredScalars.filter((c) => c.render);
    // **An id the bundle does not declare falls back to the first view and is reported** (§6.5).
    // A wrong view discloses nothing and costs a rerun, so it is a line in the failures panel and
    // never a refusal to open the dataset.
    const asked = requestedView === null ? null : (meta.views.find((v) => v.id === requestedView) ?? null);
    const opening = asked ?? meta.views[0]!;
    store.update((s) => {
      s.session = session;
      s.meta = meta;
      s.view = opening.id;
      if (requestedView !== null && asked === null) {
        s.failures = [...s.failures.slice(-19), {code: 'unknown-view', detail: `this bundle declares no view '${requestedView}'`, at: Date.now()}];
      }
      s.mTarget = meta.selection.thetaTargetMarks;
      s.artifactLayer = meta.layers[0]?.name ?? null;
      // The demo opens coloured by cluster where a layer exists (the boards), else by a column.
      s.colourBy = s.artifactLayer
        ? `cluster:${s.artifactLayer}`
        : rendered.some((c) => c.name === DEFAULT_COLOUR_BY)
          ? DEFAULT_COLOUR_BY
          : (rendered.find((c) => c.category)?.name ?? rendered[0]?.name ?? null);
      s.switching = false;
    });
    writeViewToUrl(opening.id);
    followCameraWithBasemap(opening);
    void installBasemap(opening);
    trace.event('session', {
      dataset: dataset.id,
      kMaxMarks: meta.selection.kMaxMarks,
      budget: store.state.budget,
      maxTiles: meta.maxTilesPerRequest
    });
    openSession(first);
  } else {
    // A dataset with no principals cannot open a session, and a page that then says nothing reads
    // as a server with no data. It is the bare address without a dataset document — say so.
    store.update((s) => {
      s.switching = false;
      s.status = 'refused';
      s.lastError = {code: 'no-principals', detail: `dataset '${dataset.id}' names no principal to authorise as — open the viewer with ?datasets=<document> (run_demo.sh prints the address), or set VITE_TESSERA_DATASETS`};
      s.failures = [...s.failures.slice(-19), {...s.lastError, at: Date.now()}];
    });
  }
}

async function start() {
  datasets = await loadDatasets();
  const params = new URLSearchParams(location.search);
  const requested = params.get('dataset');
  const chosen = datasets.find((d) => d.id === requested) ?? datasets[0]!;
  // `?view=` is read once, at the first activation: a view id belongs to a bundle, so carrying one
  // across a dataset change from the picker would ask the next bundle for a view of the last.
  await activate(chosen, params.get('view'));
}

start().catch((error) => {
  readoutsEl.innerHTML = `<section class="panel"><h2>Startup failed</h2>
    <div class="bad">${esc(error)}</div>
    <div class="muted">Is <code>tessera serve</code> running, and is this origin listed in
    <code>serve.dev_cors_origins</code>?</div></section>`;
});
