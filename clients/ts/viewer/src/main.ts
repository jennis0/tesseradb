import '@mosaica/components';
import type {MosaicaExplorer, MapProbe} from '@mosaica/components';
import {DEFAULT_BUDGET, MosaicaClient, createStore, dataToWorldXY, type Store as DataStore} from '@mosaica/client';
import {refusalOf} from '@mosaica/client/internal';
import type {ViewInfo} from '@mosaica/client';
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
 * The viewer: `<mosaica-explorer layout="overlay">` and a column of instruments.
 *
 * The viewer hands the explorer a store it opened per dataset and principal through its own
 * session client, which holds the session URL and credential; the store only gets a token
 * supplier.
 *
 * The instruments measure: the dataset and principal pickers, the mark budget, the depth chosen,
 * the last request's timings, the replica drawer and the refusals. They read a mirror of the
 * store's projections and the store's `instruments` channel. The probe on `window` carries the
 * decode, absorb and region timings for the harness.
 */

const config = readConfig();

let client: MosaicaClient | null = null;
let dataStore: DataStore | null = null;
let unsubscribe: (() => void) | null = null;
let datasets: Dataset[] = [];
/** The active dataset's presets; term ids are per bundle. */
let presets: Dataset['presets'] = [];

type Session = Awaited<ReturnType<MosaicaClient['authorise']>>;
type Meta = Awaited<ReturnType<MosaicaClient['meta']>>;

/**
 * The principal `dataStore` was opened on and the session its supplier last returned, or null
 * while no store is open. A store that has been replaced no longer writes here.
 */
let opened: {preset: Dataset['presets'][number]; session: Session | null} | null = null;

/** The column preferred for colour where a dataset has no artifact layer. */
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
  artifactsPerTile: config.artifactsPerTile,
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

const explorer = document.getElementById('explorer') as MosaicaExplorer;
const instrumentsEl = document.getElementById('instruments')!;

declare global {
  interface Window {
    __mosaicaProbe?: MapProbe & {lanes: Lanes};
  }
}

/** How long the camera must be still before the basemap is recomposed, so a wheel gesture composes once. */
const BASEMAP_SETTLE_MS = 180;

/**
 * Draw a basemap under the points where the view declares a `tile_scheme`, and none where it does
 * not. A basemap that fails to load leaves the map as it was.
 */
let basemapGeneration = 0;

async function installBasemap(view: ViewInfo, camera?: Camera): Promise<void> {
  // A basemap is installed only for the switch that asked for it; a faster later switch
  // supersedes it through the generation.
  const mine = ++basemapGeneration;
  await explorer.updateComplete;
  const map = explorer.map;
  if (!map || mine !== basemapGeneration) return;
  try {
    const basemap = await basemapLayer(view, camera);
    if (mine !== basemapGeneration) return;
    map.basemap = basemap;
    // OpenStreetMap's style is pale under a dark page, so the map's ground is light while it shows.
    map.ground = basemap ? 'light' : '';
  } catch (error) {
    store.update((s) => {
      s.failures = [...s.failures.slice(-19), {code: 'basemap', detail: String(error), at: Date.now()}];
    });
  }
}

/**
 * Keep the basemap at the camera's resolution. It is recomposed when the camera settles outside
 * the tiles last composed; a pan within them or a zoom at the same depth needs nothing. One
 * listener serves the page, reading the view to follow from `followedView`, which a switch
 * replaces.
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
  explorer.addEventListener('mosaica-viewchange', (event) => {
    const followed = followedView;
    // A view with no tiling (every embedding) has no basemap to follow.
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
 * Take the basemap down and cancel every fetch in flight. Called at the switch, since a view with
 * no `tile_scheme` has no basemap to replace the old one.
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
 * Write a setting to the URL, as `?view=<id>` or `?per-tile=<n>`, so a link opens the same.
 * `replaceState`, so stepping through a roster does not fill the history.
 */
function writeToUrl(name: 'view' | 'per-tile', value: string): void {
  const url = new URL(location.href);
  if (url.searchParams.get(name) === value) return;
  url.searchParams.set(name, value);
  history.replaceState(null, '', url);
}

/**
 * Follow a switch: decide the basemap from the new view's `tile_scheme` and write the view to the
 * URL. Within a group the camera does not move, so the basemap is composed for the last camera at
 * once; across frames the map refits and the next settle composes.
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
  writeToUrl('view', id);
  dropBasemap();
  followCameraWithBasemap(view);
  const camera = keepsFrame && view.tileScheme !== null && view.tile !== null ? lastCamera : null;
  if (camera) composed = coverFor(view, camera);
  void installBasemap(view, camera ?? undefined);
}

/** Publish the first map's probe on `window` for the harness. */
async function publishProbe(): Promise<void> {
  await explorer.updateComplete;
  const map = explorer.map;
  if (!map) return;
  // The frame gaps and the cluster sample the harness reads are filled only while measuring.
  map.measure = true;
  const probe = map.probe as MapProbe & {lanes: Lanes};
  probe.lanes ??= {decode: [], absorb: {split: [], store: [], remap: [], remapPoints: [], sliceMaxMs: 0}, region: null, coverage: null, longTasks: []};
  window.__mosaicaProbe = probe;
  observeLongTasks(probe.lanes);
}

/**
 * The main thread's ten longest tasks (Chromium's `longtask` entries), to set against a latency
 * the lanes do not explain.
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
    // Not every runtime has the entry type.
  }
}

/** Decode, absorb and region timings, kept on the probe for the harness. */
type Lanes = {
  decode: {ms: number; workerMs: number | null; points: number; bytes: number; at: number}[];
  absorb: {split: number[]; store: number[]; remap: number[]; remapPoints: number[]; sliceMaxMs: number};
  region: Record<string, number | string> | null;
  coverage: Record<string, number | string> | null;
  /** The ten longest main-thread tasks: duration in milliseconds, and start on `performance.now()`'s clock. */
  longTasks: {ms: number; at: number}[];
};

/** The controls: what changes the request. */
function renderControls(): string {
  return renderSource(store.state, datasets, presets);
}

/** The readouts: what came back. */
function renderReadouts(): string {
  const slab = explorer.map?.slab;
  return renderStats(store.state, {drawn: slab?.drawn ?? 0, departed: slab?.departed ?? 0}) + renderDepth(store.state) + renderErrors(store.state);
}

/**
 * Everything the controls' markup depends on, as one string. The controls are rebuilt only when
 * it changes, so an element is not replaced under the cursor between mouse-down and click.
 */
function controlsSignature(): string {
  const s = store.state;
  return [s.datasetId, s.switching ? '1' : '0', s.termsLabel, s.terms.length, s.budget, s.artifactsPerTile, s.meta?.selection.maxArtifactsPerTile ?? 0].join('|');
}

// Under 1000 px the instruments open over the explorer from a button; see `style.css`.
const instrumentsToggle = document.getElementById('instruments-toggle');
instrumentsToggle?.addEventListener('click', () => {
  const open = document.body.classList.toggle('instruments-open');
  instrumentsToggle.setAttribute('aria-expanded', open ? 'true' : 'false');
});

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

/** Rebuild the instruments: readouts on every change, controls when their signature changes. */
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
    // A different principal is a different visible set: open a new store, so nothing held under
    // the old one survives.
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

  const perTileInput = document.getElementById('per-tile') as HTMLInputElement | null;
  perTileInput?.addEventListener('change', () => {
    // A held arrow key fires a change per step; the store is opened again once the value rests.
    if (perTileTimer !== null) clearTimeout(perTileTimer);
    perTileTimer = setTimeout(() => {
      perTileTimer = null;
      setArtifactsPerTile(Number(perTileInput.value));
    }, PER_TILE_SETTLE_MS);
  });
}

/** How long the per-tile control must rest before the store is opened again at its value. */
const PER_TILE_SETTLE_MS = 200;
let perTileTimer: ReturnType<typeof setTimeout> | null = null;

/**
 * Ask for `perTile` artifacts per tile from now on. The store takes the number only when it opens,
 * so the current one is opened again on the same principal and session, keeping the drawn layers
 * and the colour. Filters and the selection start empty, as they do for a new principal. With no
 * store open, the next one opens at it.
 */
function setArtifactsPerTile(perTile: number): void {
  trace.event('per-tile', {n: perTile});
  store.update((s) => {
    s.artifactsPerTile = perTile;
  });
  writeToUrl('per-tile', String(perTile));
  const previous = dataStore;
  const current = opened;
  if (!previous || !current) return;
  const meta = previous.get('meta');
  openSession(current.preset, current.session && meta ? {session: current.session, meta} : undefined, {
    layers: previous.get('artifacts').layers,
    colourBy: previous.get('legend').colourBy
  });
}

installTrace(explorer);
installTraceBar();

const rerender = coalesce(() => trace.phase('panels', render));
store.subscribe(rerender);

/** Copy the store's projections into the viewer's state, so the instrument panels see one consistent picture per change. */
function mirror(): void {
  const ds = dataStore;
  if (!ds) return;
  const meta = ds.get('meta');
  const status = ds.get('status');
  const view = ds.get('view');
  const replica = ds.get('replica');

  // The basemap and the URL follow the `view` projection, whichever control switched it.
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

/**
 * Open a store on one principal of the active dataset; the only writer of `dataStore`.
 *
 * `held` is a session and meta already read for this principal, passed on so it is not authorised
 * again: each authorisation materialises the principal's visible set on the server. The store gets
 * a supplier that returns this token once and then mints new ones. `carried` is the layers and
 * colour to open on; without it the store opens on the dataset's first layer and colour.
 */
function openSession(
  preset: Dataset['presets'][number],
  held?: {session: Session; meta: Meta},
  carried?: {layers: string[]; colourBy: string | null}
): void {
  if (!client) return;
  const active = client;
  unsubscribe?.();
  dataStore?.dispose();
  /** Returned once, then dropped, so a renewal mints a new token. */
  let heldSession = held?.session;
  const mine = {preset, session: held?.session ?? null};
  opened = mine;

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
    // The store gets a supplier that authorises through the viewer's client, not the key.
    authorise: async () => {
      const session = heldSession ?? (await active.authorise({principal: preset.principal}));
      heldSession = undefined;
      // A replaced store's late answer is not the current principal's session.
      if (opened === mine) {
        mine.session = session;
        store.update((s) => {
          s.session = session;
        });
      }
      return {token: session.token, expiresAt: session.expiresAt};
    },
    // The meta `activate` read under this token.
    ...(held ? {meta: held.meta} : {}),
    budget: store.state.budget,
    view: store.state.view,
    prefetch,
    driver: {prefetchLayers: config.prefetchLayers},
    artifacts: {perTile: store.state.artifactsPerTile},
    replica: {
      // The absorb lane: the split and store phases per response, and the longest single slice.
      onPhase: (kind, ms, n) => {
        const lanes = window.__mosaicaProbe?.lanes;
        if (!lanes) return;
        if (kind === 'split' || kind === 'store' || kind === 'remap') lanes.absorb[kind].push(ms);
        if (kind === 'slice') lanes.absorb.sliceMaxMs = Math.max(lanes.absorb.sliceMaxMs, ms);
        for (const key of ['split', 'store', 'remap'] as const) if (lanes.absorb[key].length > 50) lanes.absorb[key].shift();
        if (kind === 'remap') lanes.absorb.remapPoints.push(n);
      }
    },
    instruments: {
      onFrame: (info) => {
        const probe = window.__mosaicaProbe;
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
        const lanes = window.__mosaicaProbe?.lanes;
        if (!lanes) return;
        // The region's counting request, in its three lanes, and the coverage check per settle.
        if (kind === 'region') lanes.region = {...fields};
        if (kind === 'coverage') lanes.coverage = {...fields};
      }
    }
  });
  dataStore.setColourBy(carried ? carried.colourBy : store.state.colourBy);
  // The viewer opens with the first layer on.
  dataStore.setLayers(carried ? carried.layers : store.state.artifactLayer ? [store.state.artifactLayer] : []);
  unsubscribe = dataStore.subscribe(() => mirror());
  // The explorer takes the store by property; its map pushes the first view once meta lands.
  explorer.store = dataStore;
  void publishProbe();
}

/** Which `activate` is current; an earlier one still awaiting its meta stands down. */
let activation = 0;

/**
 * Point the viewer at a dataset with a new client and store. Ids, term ids and quantised geometry
 * are all per bundle, so nothing is reused.
 */
async function activate(dataset: Dataset, requestedView: string | null = null): Promise<void> {
  const mine = ++activation;
  unsubscribe?.();
  unsubscribe = null;
  dataStore?.dispose();
  dataStore = null;
  opened = null;
  // The stores opened on this client leave it open; it is closed here, once none is left.
  client?.close();
  presets = dataset.presets;

  client = new MosaicaClient({
    viewerUrl: dataset.viewerUrl,
    sessionUrl: dataset.sessionUrl,
    sessionCredential: dataset.apiKey,
    // Per-response decode time as seen from this thread and in the worker; the difference is
    // the queue.
    onDecode: (ms, bytes, points, workerMs) => {
      const probe = window.__mosaicaProbe;
      if (!probe) return;
      probe.timings.decodeMs.push(ms);
      if (probe.timings.decodeMs.length > 50) probe.timings.decodeMs.shift();
      probe.lanes.decode.push({ms, workerMs, points, bytes, at: performance.now()});
      if (probe.lanes.decode.length > 50) probe.lanes.decode.shift();
    }
  });

  /** The viewer opens on the broadest principal, the hardest case. */
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

  // Choose the colour and layer from meta before opening the store, so it opens on them.
  if (first) {
    let session: Session;
    let meta: Meta;
    try {
      session = await client.authorise({principal: first.principal});
      meta = await client.meta(session.token);
    } catch (error) {
      // The picker's `change` handler cannot await this, so the refusal is reported here, where
      // every other refusal is, and `switching` is cleared.
      if (mine !== activation) return;
      store.update((s) => {
        s.switching = false;
        s.status = 'refused';
        s.lastError = refusalOf(error);
        s.failures = [...s.failures.slice(-19), {...s.lastError, at: Date.now()}];
      });
      return;
    }
    if (mine !== activation) return;
    const rendered = meta.declaredScalars.filter((c) => c.render);
    // A view id the bundle does not declare falls back to the first view and is reported in the
    // failures panel.
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
      // Colour by cluster where a layer exists, else by a column.
      s.colourBy = s.artifactLayer
        ? `cluster:${s.artifactLayer}`
        : rendered.some((c) => c.name === DEFAULT_COLOUR_BY)
          ? DEFAULT_COLOUR_BY
          : (rendered.find((c) => c.category)?.name ?? rendered[0]?.name ?? null);
      s.switching = false;
    });
    explorer.titleField = dataset.titleField ?? '';
    // The heading names the dataset as the dataset picker does.
    explorer.datasetTitle = dataset.label;
    writeToUrl('view', opening.id);
    followCameraWithBasemap(opening);
    void installBasemap(opening);
    trace.event('session', {
      dataset: dataset.id,
      kMaxMarks: meta.selection.kMaxMarks,
      budget: store.state.budget,
      maxTiles: meta.maxTilesPerRequest
    });
    openSession(first, {session, meta});
  } else {
    // No principals means no dataset document was given; say so rather than show an empty map.
    store.update((s) => {
      s.switching = false;
      s.status = 'refused';
      s.lastError = {code: 'no-principals', detail: `dataset '${dataset.id}' names no principal to authorise as. Open the viewer with ?datasets=<document> (run_demo.sh prints the address), or set VITE_MOSAICA_DATASETS`};
      s.failures = [...s.failures.slice(-19), {...s.lastError, at: Date.now()}];
    });
  }
}

async function start() {
  datasets = await loadDatasets();
  const params = new URLSearchParams(location.search);
  const requested = params.get('dataset');
  const chosen = datasets.find((d) => d.id === requested) ?? datasets[0]!;
  writeToUrl('per-tile', String(store.state.artifactsPerTile));
  // `?view=` applies to the first activation only, since a view id belongs to a bundle.
  await activate(chosen, params.get('view'));
  // After the activation, which empties the failures panel.
  if (config.refused.length > 0) {
    store.update((s) => {
      s.failures = [...s.failures, ...config.refused.map((f) => ({...f, at: Date.now()}))].slice(-20);
    });
  }
}

start().catch((error) => {
  readoutsEl.innerHTML = `<section class="panel"><h2>Startup failed</h2>
    <div class="bad">${esc(error)}</div>
    <div class="muted">Is <code>mosaica serve</code> running, and is this origin listed in
    <code>serve.dev_cors_origins</code>?</div></section>`;
});
