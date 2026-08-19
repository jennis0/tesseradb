import {Deck} from '@deck.gl/core';
import {Replica, TesseraClient} from '@tessera/client';
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
import {ArtifactChannel, loadArtifactPlaces} from './artifacts.js';
import {foldBandColumn} from './assemble.js';
import {countCodes, countCodesCached, extendRanks, widenDomain} from './colour.js';
import {
  composeFilters,
  dateToMicros,
  emptyDraft,
  isDateColumn,
  type TextMode
} from './filters.js';
import {MarkSlab} from './slab.js';
import {installTrace, installTraceBar, trace} from './trace.js';
import {coalesce, createStore, type Store} from './state.js';
import {DriverBinding} from './binding.js';
import {
  INITIAL_VIEW_STATE,
  VIEW,
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

/**
 * The session client, the replica and the driver, all of which belong to **one dataset**.
 *
 * Rebuilt together on a dataset switch and never partially: a client pointed at one server with a
 * replica holding another's bands would draw one bundle's geometry under the other's identity space.
 * `activate` is the only writer.
 */
let client: TesseraClient | null = null;
let replica: Replica | null = null;
let controller: DriverBinding | null = null;
/**
 * The annotation channel, which issues its own requests rather than reading the point path's.
 *
 * Beside the replica rather than inside it, for the reason `artifacts.ts` opens with: the replica
 * elides tiles it holds, and an elided tile carries no artifacts — so clusters would thin out as
 * the cache warmed.
 */
let artifactChannel: ArtifactChannel | null = null;
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
 * A filtered selection turns over completely on every keystroke — nothing of the previous answer is
 * reusable — so an un-debounced text box would issue a full re-selection per character and show the
 * answers to prefixes nobody asked about. Long enough to swallow typing, short enough that a
 * finished word lands before you look up.
 */
const FILTER_DEBOUNCE_MS = 350;

const store = createStore({
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

const deck = new Deck({
  parent: mapEl,
  // The slab uploads its own dirty spans from here on; without a device it stays on the
  // typed-array path, which is also what `?gpu=0` forces. See `slab.ts`.
  onDeviceInitialized: (device) => {
    if (config.gpuBuffers) markSlab.attach(device);
  },
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
    artifactChannel?.schedule(currentView, mapEl.clientWidth, mapEl.clientHeight);
    return viewState;
  },
  onClick: (info) => {
    /**
     * The layer that answered the pick — `sourceLayer` **or** `layer`, and it has to be both.
     *
     * `sourceLayer` names the sublayer a *composite* layer generated; a layer handed to deck directly
     * has none, and every mark layer here is handed over directly. So reading only `sourceLayer` found
     * nothing on every click, the identity array was never reached, and drill-down could not resolve a
     * mark at all — measured as a hit on index 112,633 with no identities behind it.
     */
    const layer = info.sourceLayer ?? info.layer;
    /**
     * A cluster ring answered the pick, so this is a **different question** — a grouping and one
     * number, not a document and its record — and it goes to its own route. Routing both through
     * one endpoint would let a caller learn which kind an identifier names from the shape of the
     * answer, which is why the server keeps them apart; the client has no business joining them
     * back together.
     */
    const artifactIds = (layer?.props as {artifactIds?: bigint[]} | undefined)?.artifactIds;
    if (artifactIds && info.index >= 0 && info.index < artifactIds.length) {
      openArtifact(artifactIds[info.index]!);
      return;
    }
    const ids = (layer?.props as {tesseraIds?: BigUint64Array} | undefined)?.tesseraIds;
    const id = ids && info.index >= 0 && info.index < ids.length ? ids[info.index] : undefined;
    const session = store.state.session;
    /**
     * What the pick actually returned, recorded whether or not it resolved.
     *
     * **A click that hits nothing and a click whose mark carried no identity are different failures,
     * and the panel showed neither.** Both left it reading "click a mark", so a broken pick was
     * indistinguishable from a miss — which is the same empty-versus-failed conflation the counts
     * panel goes to some length to avoid, reached through the one surface nobody had instrumented.
     * `index` is deck's own hit index and `layer` is which layer answered.
     */
    const pick = {
      index: info.index,
      layer: layer?.id ?? null,
      hasIds: ids !== undefined,
      idCount: ids?.length ?? 0
    };
    if (id === undefined || !session || !client) {
      store.update((s) => {
        s.selected = null;
        s.selectedWorldXY = null;
        s.itemError = null;
        s.lastPick = pick;
      });
      return;
    }
    const worldXY = info.coordinate
      ? ([info.coordinate[0]!, info.coordinate[1]!] as [number, number])
      : null;
    client
      .item(session.token, id)
      .then((detail) => {
        store.update((s) => {
          s.selected = {id, fields: detail.fields, externalId: detail.externalId};
          s.selectedWorldXY = worldXY;
          s.itemError = null;
          s.lastPick = pick;
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
          s.lastPick = pick;
        });
      });
  }
});

/**
 * Open one cluster by identifier — `POST /v1/artifacts/{tessera_id}`.
 *
 * The count it returns is the one the map is already showing, from the same predicate on the
 * server: an artifact openable but not drawable, or the reverse, would be that rule transcribed
 * twice. So this is a round trip that confirms rather than reveals, which is the point — the
 * detail panel has nothing behind the count to show, and there is deliberately nothing to fetch.
 */
function openArtifact(id: bigint) {
  const {session, view} = store.state;
  if (!session || !client) return;
  client
    .artifact(session.token, id, {view})
    .then((detail) => {
      store.update((s) => {
        s.selectedArtifact = {...detail, id};
        s.artifactDetailError = null;
        // A cluster and a document are different selections, and showing both at once would invite
        // reading the one as the other's context.
        s.selected = null;
        s.selectedWorldXY = null;
        s.itemError = null;
      });
    })
    .catch((error) => {
      const e = error as {code?: string; detail?: string; message?: string};
      store.update((s) => {
        s.selectedArtifact = null;
        s.artifactDetailError = {
          code: e.code ?? 'fetch-failed',
          detail: e.detail ?? e.message ?? String(error)
        };
      });
    });
}

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
  if (!assembled || !meta || !session || !client) return;
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
  /**
   * Whether nothing has been ranked for this column yet — the interval in which the map is grey.
   *
   * The exact-bands-only rule below is right for every *recount* and wrong for the first count, and
   * the difference is what a viewer sees as pop-in. During a load the marks on screen are largely
   * **stand-ins** borrowed from another zoom level, so a count that reads only exact bands finds
   * nothing to rank and the map stays uniform until this depth's own bands have streamed in —
   * measured on the 2.4M bundle at 7.7 s after the first marks were already drawn, and 4.1 s on the
   * 25M one.
   */
  const bootstrap = Object.keys(store.state.ranks[column] ?? {}).length === 0;

  const counts = trace.phase('legend', () => {
    const held = new Map<number, number>();
    for (const band of assembled.bands) {
      const values = band.scalars[column];
      if (!values) continue;
      for (const [code, n] of countCodesCached(values)) held.set(code, (held.get(code) ?? 0) + n);
    }
    // The stand-in column joins the count **only to bootstrap the palette**, never afterwards. It is
    // rebuilt on every derive, so it can never be memoised — which is why folding it unconditionally
    // was measured at 51 ms per count on the largest column and taken back out. Under this guard it
    // is paid at most once per column per principal: the moment anything is ranked the map is
    // coloured, `bootstrap` goes false, and every later count is exact-bands-only as before.
    if (bootstrap && held.size === 0) {
      const values = assembled.standIn.scalars[column];
      if (values) for (const [code, n] of countCodes(values)) held.set(code, (held.get(code) ?? 0) + n);
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

/**
 * Enumerate a filterable category's value set, for its picker.
 *
 * **This is a different question from the legend's**, asked of the same endpoint through its other
 * form. The legend resolves the codes it *drew*; this pages the values the server is willing to
 * list, which `visibility` gates before answering — `public` publishes taxonomy whose existence
 * discloses nothing, `derived` is refused. Keeping the two results in separate maps is what stops
 * a listed-but-undrawn value ever reaching a swatch.
 */
async function loadFilterValues(column: string) {
  const {session, meta} = store.state;
  if (!session || !meta || !client) return;
  if (store.state.filterValues[column] || store.state.filterValueErrors[column]) return;
  try {
    const values = await client.categories(session.token, column);
    // Sorted by key, because a picker is scanned rather than read in rank order — the legend's
    // frequency ordering is right there and wrong here.
    values.sort((a, b) => a.key.localeCompare(b.key));
    store.update((s) => {
      s.filterValues[column] = values;
    });
  } catch (error) {
    const e = error as {code?: string; detail?: string; message?: string};
    store.update((s) => {
      s.filterValueErrors[column] = {
        code: e.code ?? 'fetch-failed',
        detail: e.detail ?? e.message ?? String(error)
      };
    });
  }
}

/**
 * Apply the filter draft: drop everything held, and ask again.
 *
 * **The replica must be reset by hand here, and this is the one thing about filtering a client gets
 * wrong.** A response's identity key partitions by principal, credential, mask and view — *not* by
 * filter — so bands fetched under one filter remain renderable under the next and would be served
 * from cache as though they belonged to it. Nothing on the wire says otherwise; the client that
 * changed the question is the only party that knows the held answers are to a different one.
 */
function applyFilters() {
  controller?.cancel();
  replica?.reset();
  store.update((s) => {
    s.assembled = null;
    s.status = 'loading';
    s.lastVisibleInView = null;
    s.selected = null;
    s.selectedWorldXY = null;
    s.itemError = null;
    s.lastPick = null;
  });
  trace.event('filters', {n: Object.keys(store.state.filters).length});
  controller?.schedule(currentView, mapEl.clientWidth, mapEl.clientHeight);
}

let filterTimer: ReturnType<typeof setTimeout> | null = null;
/** Debounced for typing; a tick or a date lands immediately, having no intermediate states. */
function scheduleFilters(immediate = false) {
  if (filterTimer) clearTimeout(filterTimer);
  if (immediate) {
    applyFilters();
    return;
  }
  filterTimer = setTimeout(() => {
    filterTimer = null;
    applyFilters();
  }, FILTER_DEBOUNCE_MS);
}

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
 * Everything the control column's markup depends on, as one string.
 *
 * **A control column is only rebuilt when its own content would change**, and it needs this rather
 * than a focus check because the two failures are different. A focus guard protects a control the
 * user is *already* in; it cannot protect one they are reaching for — the store ticks several times
 * a second while marks stream in, and `innerHTML` replaces the element under the cursor between the
 * mouse going down and the click landing. Measured as exactly that: a filter box that could not be
 * clicked into while a viewport was still arriving.
 *
 * So the readouts that move every frame were moved to the other column, and what is left changes
 * only when a user changes it. The focus guard stays as well, for the case this cannot cover: a
 * legend rank extending while a box is focused.
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

/**
 * Rebuild the control column, carrying the user's place in it across the rebuild.
 *
 * **Focus is restored, not protected.** The obvious guard — skip the rebuild while focus is inside —
 * deadlocks: a `<select>` keeps focus after it is used, so the column that must now describe a
 * *different bundle* is the one thing that cannot refresh, and it sits there showing the old one
 * until the user happens to click elsewhere. Restoring instead means a rebuild is always allowed and
 * always harmless.
 *
 * Three things have to survive it, all of them found by being wrong: which element had focus, the
 * caret and selection inside a text box (or every rebuild sends the caret to the end mid-word), and
 * the scroll offset of a category list (which is 171 rows on this corpus, so losing it loses the
 * user's place entirely).
 */
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

/**
 * Rebuild the panels.
 *
 * **The two columns are refreshed under different rules, and the split is what makes that possible.**
 * The readouts refresh on every store change; the controls refresh only when
 * {@link controlsSignature} moves, because rebuilding markup that has not changed is what replaces
 * the element under a user's cursor between mouse-down and click. With one column those rules were
 * in conflict — the numbers froze for as long as a control was focused. Now they keep updating while
 * a filter box is being typed into, which is exactly when watching them is most interesting.
 */
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
    if (!preset || !client) return;
    trace.event('principal', {label: preset.label, n: preset.terms.length});
    // A different principal is a different mask: abort anything in flight for the old token, and
    // drop the calibration, which was measured against a different visible set.
    controller?.cancel();
    // The same argument, and a sharper one: the same cluster has a different count under a
    // different mask, and some clusters cease to exist entirely. Held artifacts belong to the old
    // token and must not be drawn for a moment longer.
    artifactChannel?.reset();
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
          // The filter *drafts* stay — a user switching principal is asking the same question of a
          // different viewer. The offered value sets do not: `visibility` is evaluated per principal,
          // so a picker built under the old token may list values this one may not see.
          s.filterValues = {};
          s.filterValueErrors = {};
        });
        // The artifact channel is *not* scheduled here: there is no drawn frame at this moment, so
        // it would have no depth to ask at. It asks on the first frame the new token produces —
        // see the subscriber below `activate`.
        controller?.schedule(currentView, mapEl.clientWidth, mapEl.clientHeight);
      })
      .catch((error) => recordFailure(store, `authorise ${preset.label}`, error));
  });

  const artifactLayer = document.getElementById('artifact-layer') as HTMLSelectElement | null;
  artifactLayer?.addEventListener('change', () => {
    const chosen = artifactLayer.value === '' ? null : artifactLayer.value;
    trace.event('layer', {name: chosen ?? 'none'});
    store.update((s) => {
      s.artifactLayer = chosen;
      // A different layer is a different set of artifacts and a different criterion, so the opened
      // one goes with it: keeping it would leave a count from one layer under another's name.
      s.selectedArtifact = null;
      s.artifactDetailError = null;
    });
    // Immediately rather than on the settle timer: the view has not changed, so nothing else will
    // ask, and the user is waiting on this one.
    artifactChannel?.refresh(currentView, mapEl.clientWidth, mapEl.clientHeight);
  });

  bindFilterControls();

  const colourBy = document.getElementById('colour-by') as HTMLSelectElement | null;
  colourBy?.addEventListener('change', () => {
    const chosen = colourBy.value === '' ? null : colourBy.value;
    store.update((s) => {
      s.colourBy = chosen;
    });
    // **No refetch.** Every rendered column is already in the held response, so this is a layer
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
    // The driver holds its own copy of the budget — the store's is only seed and display — so the
    // change must be handed over before the reschedule or the plan replays the old depth.
    controller?.setBudget(Number(budgetInput.value));
    controller?.schedule(currentView, mapEl.clientWidth, mapEl.clientHeight);
  });
}

/**
 * Wire the filter controls.
 *
 * Every handler writes the *draft* and then schedules; nothing composes an expression here, because
 * the composition has one home (`filters.ts`) and a second would be a second set of rules about what
 * an empty box means.
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
          const value = date
            ? dateToMicros(box.value)
            : box.value === ''
              ? null
              : Number(box.value);
          store.update((s) => {
            const d = s.filters[column];
            if (d?.family === 'numeric') d[bound] = Number.isFinite(value) ? value : null;
          });
          scheduleFilters(true);
        });
      }
    }
  }

  // Delegated, because a category's ticks are rebuilt whenever its value list arrives and binding
  // each box individually would leave the earlier listeners attached to detached nodes.
  controlsEl.querySelectorAll<HTMLElement>('[data-checks]').forEach((box) => {
    box.addEventListener('change', (event) => {
      const input = event.target as HTMLInputElement;
      const column = input.dataset.cat;
      if (!column) return;
      store.update((s) => {
        const d = s.filters[column];
        if (d?.family !== 'category') return;
        d.keys = input.checked
          ? [...d.keys, input.value]
          : d.keys.filter((k) => k !== input.value);
      });
      scheduleFilters(true);
    });
  });

  const clear = document.getElementById('filters-clear') as HTMLButtonElement | null;
  clear?.addEventListener('click', () => {
    store.update((s) => {
      s.filters = emptyDraft(s.meta?.filterOperands ?? []);
    });
    scheduleFilters(true);
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
    __tesseraProbe?: {
      paints: number;
      at: number;
      marks: number;
      requests: number;
      /**
       * The colour half of the paint key — `uniform` until a palette has been assigned.
       *
       * Published because **when the palette lands was otherwise unmeasurable from outside**, and a
       * cost nobody can measure is a cost that drifts. It is what made the legend's deferral wrong
       * for as long as it was: the swatch list needs a round trip and the marks do not, so timing
       * the swatches timed the wrong thing and the grey interval went unnoticed.
       */
      encoding: string;
    };
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
    }|${view.artifactVersion}|${view.artifactLayer ?? ''}|${
      view.selectedArtifact?.id ?? ''
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
      requests: requestCount,
      encoding: encodingSignature(store)
    };
  })
);
store.subscribe(rerender);

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
const LEGEND_QUIET_MS = 500;
/**
 * The **cap** on how long the count may be deferred, however busy the store stays.
 *
 * Re-arming on every change alone was wrong, and the 25M bundle is where it showed: the store ticks
 * for as long as bands are arriving, so "fire once things settle" meant *not until the whole load
 * finished* — several seconds of grey marks, then the palette in one jump. Waiting for quiet is
 * still right, because most of the deferral's value is skipping recounts mid-gesture; what it needed
 * was a ceiling, so a load that never goes quiet still gets its colours on the way.
 */
const LEGEND_MAX_DEFER_MS = 1_200;
let lastResolved: {column: string; version: number} | null = null;
let legendTimer: ReturnType<typeof setTimeout> | null = null;
/** When the currently-deferred count first became due — the cap is measured from here, not from the last change. */
let legendDueSince = 0;

function runLegendCount() {
  legendTimer = null;
  legendDueSince = 0;
  const now = store.state;
  if (!now.colourBy || !now.assembled) return;
  lastResolved = {column: now.colourBy, version: now.assembled.version};
  void resolveCategoryCodes(now.colourBy);
}

store.subscribe(() => {
  const {colourBy, assembled} = store.state;
  if (!colourBy || !assembled) return;
  if (lastResolved && lastResolved.column === colourBy && lastResolved.version === assembled.version)
    return;
  // **Nothing ranked yet means the map is grey, and a grey map is not worth deferring.** The
  // deferral exists to skip *recounts* mid-gesture; the first count is the one the user is waiting
  // on, so it runs on the next store change rather than after the load goes quiet.
  if (Object.keys(store.state.ranks[colourBy] ?? {}).length === 0) {
    if (legendTimer) clearTimeout(legendTimer);
    runLegendCount();
    return;
  }
  const elapsed = legendDueSince === 0 ? 0 : performance.now() - legendDueSince;
  if (legendDueSince === 0) legendDueSince = performance.now();
  if (elapsed >= LEGEND_MAX_DEFER_MS) {
    if (legendTimer) clearTimeout(legendTimer);
    runLegendCount();
    return;
  }
  // Re-armed on every change so it fires once things settle — but never past the cap above.
  if (legendTimer) clearTimeout(legendTimer);
  legendTimer = setTimeout(runLegendCount, Math.min(LEGEND_QUIET_MS, LEGEND_MAX_DEFER_MS - elapsed));
});

/**
 * Point the viewer at a dataset: a fresh client, session, replica and driver.
 *
 * **All four together, or none.** They are coupled through the identity space: a `tessera_id` minted
 * by one bundle means nothing to another, a term id names a different set in each, and a held band
 * carries geometry quantised under one bundle's extent. Reusing any of them across a switch would
 * draw one bundle's data under the other's labels — silently, since nothing on either wire says the
 * bundle changed.
 */
async function activate(dataset: Dataset) {
  controller?.cancel();
  artifactChannel?.cancel();
  client?.close();
  replica?.reset();
  markSlab.clear();
  controller = null;
  replica = null;
  artifactChannel = null;
  presets = dataset.presets;

  client = new TesseraClient({
    viewerUrl: dataset.viewerUrl,
    sessionUrl: dataset.sessionUrl,
    sessionCredential: config.sessionCredential
  });

  /**
   * The demo opens on its **hardest** case, not its gentlest: the broadest principal available.
   *
   * The narrow default it replaces saturates at almost any depth, so the budget never binds, the
   * cache has nothing to do and the render path is never loaded — which made every session start by
   * changing three controls before anything under measurement was running. A demo whose defaults
   * exercise none of the machinery it exists to show is a demo of the controls.
   */
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
    s.lastVisibleInView = null;
    s.selected = null;
    s.selectedWorldXY = null;
    s.itemError = null;
    s.failures = [];
    s.lastPlan = null;
    s.latency = null;
    s.lastTimings = null;
    s.categories = {};
    s.categoryErrors = {};
    s.ranks = {};
    s.domains = {};
    s.filters = {};
    s.filterValues = {};
    s.filterValueErrors = {};
    s.artifactLayer = null;
    s.artifacts = [];
    s.artifactVersion += 1;
    s.artifactStatus = 'idle';
    s.artifactError = null;
    s.selectedArtifact = null;
    s.artifactDetailError = null;
  });

  const session = await client.authorise(store.state.terms);
  const meta = await client.meta(session.token);
  store.update((s) => {
    s.session = session;
    s.meta = meta;
    s.view = meta.views[0]!.id;
    s.mTarget = meta.selection.thetaTargetMarks;
    // Seeded from what this bundle publishes as filterable, which is why the abstract box exists on
    // one dataset and not the other without a line of code knowing either name.
    s.filters = emptyDraft(meta.filterOperands);
    // Colour by the default when this bundle renders it, else by its first rendered column — a
    // bundle whose schema differs should still open on a live encoding rather than on grey.
    const rendered = meta.declaredScalars.filter((c) => c.render);
    s.colourBy = rendered.some((c) => c.name === DEFAULT_COLOUR_BY)
      ? DEFAULT_COLOUR_BY
      : (rendered.find((c) => c.category)?.name ?? rendered[0]?.name ?? null);
    // Opens on the first layer this principal reaches, and on nothing at all where it reaches
    // none — which is the ordinary case and costs a request nobody made.
    s.artifactLayer = meta.layers[0]?.name ?? null;
    s.switching = false;
  });

  /**
   * The replica sits between the stateless client and the scheduler: it owns the held bands and
   * decides which of the scheduler's wanted tiles actually need asking for. Created after `meta`,
   * because it needs the quantisation extent to turn tiles into a request box.
   */
  replica = new Replica(
    (req, signal, background) => {
      requestCount += 1;
      return client!.viewport(
        store.state.session!.token,
        // **The filter is composed per request, not cached.** It is read from the draft at the
        // moment of asking, so a request in flight when a control changes carries the filter it was
        // issued under, and `applyFilters` — not this closure — is what makes the held answers to
        // the old one go away.
        //
        // **`layers: []` and no selection are different requests**, and this is the one that means
        // "charge me nothing for annotations": these are the point path's requests, and the
        // artifacts they would carry could not be used anyway — a tile the replica already holds is
        // omitted, so the artifacts intersecting it would go missing exactly as the cache warmed.
        // The artifact channel asks for itself; see `artifacts.ts`.
        {
          ...req,
          view: store.state.view,
          filters: composeFilters(store.state.filters),
          layers: []
        },
        signal,
        background
      );
    },
    meta.quantisation,
    {
      view: meta.views[0]!.id,
      onPhase: (kind, ms, n) => {
        if (trace.enabled) trace.event(kind, {ms, n});
        // A piece of a split response has been absorbed: its bands are drawable NOW, not when the
        // whole fetch settles — so paint them. rAF-coalesced, and the fold path makes it cheap.
        if (kind === 'store') controller?.absorbed();
      }
    }
  );
  // `?prefetch=0` turns look-ahead off without touching the replica — the A/B the measurement
  // wants, and the switch an operator watching aggregate select CPU would reach for.
  const prefetch = new URLSearchParams(location.search).get('prefetch') !== '0';
  controller = new DriverBinding(store, replica, prefetch);
  artifactChannel = new ArtifactChannel(client, store, meta.quantisation);
  // Unawaited: the sidecar decides where a cluster is drawn, not whether it is served, so the map
  // and the counts panel do not wait on it.
  void loadArtifactPlaces().then((places) => {
    store.update((s) => {
      s.artifactPlaces = places;
      s.artifactVersion += 1;
    });
  });
  trace.event('session', {
    dataset: dataset.id,
    prefetch: prefetch ? 1 : 0,
    kMaxMarks: meta.selection.kMaxMarks,
    budget: store.state.budget,
    maxTiles: meta.maxTilesPerRequest
  });

  // Every filterable category's picker, fetched once per dataset and principal. Concurrent and
  // unawaited: the map must not wait on a control's value list.
  for (const operand of meta.filterOperands) {
    if (operand.family === 'category') void loadFilterValues(operand.column);
  }

  controller.schedule(currentView, mapEl.clientWidth, mapEl.clientHeight);
}

/**
 * The artifact channel needs a depth, and the depth is the driver's answer to the first view — so
 * it asks on the first frame the driver produces rather than at startup, where there is none.
 */
let askedFor: {channel: ArtifactChannel; token: string} | null = null;
store.subscribe(() => {
  const {assembled, session} = store.state;
  if (!assembled || !session || !artifactChannel) return;
  // **Keyed on the session as well as the channel**, which is what makes a principal switch work:
  // it clears the drawn frame, so at the moment the new token arrives there is no depth to ask at
  // and the switch's own call does nothing. The first frame under the new mask is the moment the
  // question can be asked, and it is a different question — same clusters, different counts, and
  // some of them gone.
  if (askedFor?.channel === artifactChannel && askedFor.token === session.token) return;
  askedFor = {channel: artifactChannel, token: session.token};
  artifactChannel.schedule(currentView, mapEl.clientWidth, mapEl.clientHeight);
});

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
