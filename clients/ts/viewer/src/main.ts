import '@tesseradb/components';
import type {TesseraExplorer, MapProbe} from '@tesseradb/components';
import {TesseraClient, createStore, type Store as DataStore} from '@tesseradb/client';
import {loadDatasets, readConfig, type Dataset} from './config.js';
import {esc} from './html.js';
import {renderErrors} from './panels/errors.js';
import {renderSource} from './panels/source.js';
import {renderLegend} from './panels/legend.js';
import {renderStats, toggleStatsDrawer} from './panels/stats.js';
import {renderDepth} from './panels/view.js';
import {renderArtifactDetail, renderArtifacts, renderLayerControl} from './panels/layers.js';
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
 * principal pickers, the mark budget, the layer and colour controls (componentised at step 3),
 * the clusters in view, the depth the budget chose, the last request's timings and the replica
 * drawer, and the refusals observed. They read a mirror of the store's projections plus the
 * numbers the §4 surface deliberately omits, which the store forwards on its demo-only
 * `instruments` channel.
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
  categories: {},
  categoryErrors: {},
  ranks: {},
  domains: {},
  artifactLayer: null,
  artifacts: [],
  artifactVersion: 0,
  artifactStatus: 'idle',
  artifactError: null,
  selectedArtifact: null,
  artifactDetailError: null
});

const explorer = document.getElementById('explorer') as TesseraExplorer;
const instrumentsEl = document.getElementById('instruments')!;

declare global {
  interface Window {
    __tesseraProbe?: MapProbe;
  }
}

/** The first map's probe, published for the smoke scripts and the harness (§5.9). */
async function publishProbe(): Promise<void> {
  await explorer.updateComplete;
  const map = explorer.map;
  if (!map) return;
  window.__tesseraProbe = map.probe;
}

// ---------------------------------------------------------------------------------- the panels

/** The controls: everything that changes what is asked for. */
function renderControls(): string {
  return renderSource(store.state, datasets, presets) + renderLayerControl(store.state) + renderLegend(store.state);
}

/** The readouts: everything that reports what came back. */
function renderReadouts(): string {
  const slab = explorer.map?.slab;
  return (
    renderArtifacts(store.state) +
    (store.state.selectedArtifact || store.state.artifactDetailError ? renderArtifactDetail(store.state) : '') +
    renderStats(store.state, {drawn: slab?.drawn ?? 0, departed: slab?.departed ?? 0}) +
    renderDepth(store.state) +
    renderErrors(store.state)
  );
}

/**
 * Everything the controls' markup depends on, as one string — rebuilt only when it moves, so a
 * rebuild never replaces the element under a user's cursor between mouse-down and click.
 */
function controlsSignature(): string {
  const s = store.state;
  const colour = s.colourBy ?? '';
  return [
    s.datasetId,
    s.switching ? '1' : '0',
    s.termsLabel,
    s.terms.length,
    colour,
    s.categories[colour]?.length ?? -1,
    Object.keys(s.ranks[colour] ?? {}).length,
    s.categoryErrors[colour]?.code ?? '',
    s.domains[colour] ? `${s.domains[colour]!.min}..${s.domains[colour]!.max}` : '',
    s.budget,
    s.artifactLayer ?? '',
    s.meta?.layers.length ?? -1,
    s.meta?.declaredScalars.length ?? -1
  ].join('|');
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

  const artifactLayer = document.getElementById('artifact-layer') as HTMLSelectElement | null;
  artifactLayer?.addEventListener('change', () => {
    const chosen = artifactLayer.value === '' ? null : artifactLayer.value;
    trace.event('layer', {name: chosen ?? 'none'});
    dataStore?.setLayers(chosen ? [chosen] : []);
  });

  const colourBy = document.getElementById('colour-by') as HTMLSelectElement | null;
  colourBy?.addEventListener('change', () => {
    const chosen = colourBy.value === '' ? null : colourBy.value;
    // No refetch: every rendered column is already in the held response, so this is an encoding
    // pass over what is drawn — the switch a viewer can run as a check on I7.
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
  const legend = ds.get('legend');
  const artifacts = ds.get('artifacts');
  const selection = ds.get('selection');
  const replica = ds.get('replica');

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
    s.colourBy = legend.colourBy;
    s.ranks = legend.ranks;
    s.domains = legend.domains;
    s.categories = legend.categories;
    s.categoryErrors = legend.categoryErrors;
    if (s.artifacts !== artifacts.served) s.artifactVersion += 1;
    s.artifacts = artifacts.served;
    s.artifactLayer = artifacts.layer;
    s.artifactStatus = artifacts.status;
    s.artifactError = artifacts.refusal;
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

  store.update((s) => {
    s.terms = preset.terms;
    s.termsLabel = preset.label;
    s.frame = null;
    s.sessionWarm = false;
    s.status = 'idle';
    s.selectedArtifact = null;
    s.artifactDetailError = null;
    s.categories = {};
    s.categoryErrors = {};
    s.ranks = {};
    s.domains = {};
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
        const probe = window.__tesseraProbe;
        if (probe) {
          probe.requests += 1;
          probe['instruments'] = {
            depth: info.plan.choice.depth,
            tiles: info.plan.choice.tiles,
            predictedMarks: info.plan.choice.predictedMarks,
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
      onTrace: (kind, fields) => trace.event(kind, fields)
    }
  });
  dataStore.setColourBy(store.state.colourBy);
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
async function activate(dataset: Dataset): Promise<void> {
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
    // Per-response decode time, for the harness's measurement (design §5.10).
    onDecode: (ms) => {
      const probe = window.__tesseraProbe;
      if (!probe) return;
      probe.timings.decodeMs.push(ms);
      if (probe.timings.decodeMs.length > 50) probe.timings.decodeMs.shift();
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
  readoutsEl.innerHTML = `<section class="panel"><h2>Startup failed</h2>
    <div class="bad">${esc(error)}</div>
    <div class="muted">Is <code>tessera serve</code> running, and is this origin listed in
    <code>serve.dev_cors_origins</code>?</div></section>`;
});
