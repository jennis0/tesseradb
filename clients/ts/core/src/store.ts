import {ArtifactChannel, servedLineage, type ArtifactChannelState, type ServedLineage} from './artifactChannel.js';
import {SessionArtifactTable} from './artifactTable.js';
import type {Composition} from './compose.js';
import {NO_COUNT, NO_MASKED, type Count, type Masked} from './counts.js';
import {dataToWorldXY, gridToWorld, MAX_DEPTH, WORLD_SIZE} from './coords.js';
import {tileRectOfBbox} from './budget.js';
import {rectContainsTile} from './rects.js';
import {worldBbox} from './prefetch.js';
import type {Clock, DriverOptions, ViewState as DriverViewState} from './driver.js';
import {countCodesCached, countCodesInPiece, extendRanks, widenDomain, widenDomainOver, type Domain, type Ranks} from './encoding.js';
import {composeFilters, emptyDraft, type FilterDraft} from './filters.js';
import {Presenter, defaultFrameScheduler, type FrameScheduler, type PresentedStatus, type Refusal} from './presented.js';
import {cellExceedsPixel, insideBox, insidePolygon, rasteriseBox, rasterisePolygon, type WorldPolygon} from './region.js';
import {layerClosure} from './layers.js';
import {artifactBudgetFor} from './artifactBudget.js';
import {artifactColours, type PaletteKind, type PaletteScheme, type Rgba} from './palette.js';
import type {Band, BandKey} from './bands.js';
import {bandKey} from './bands.js';
import {Replica, type ReplicaOptions} from './replica.js';
import {TesseraClient, TesseraError, type TesseraClientOptions} from './client.js';
import type {DepthChoice} from './budget.js';
import type {Presented} from './presented.js';
import type {
  Artifact,
  ArtifactDetail,
  CategoryValue,
  FilterExpr,
  ItemDetail,
  Meta,
  ViewportResult
} from './types.js';

/**
 * The headless store (design client-components §4): *tell it where I am looking, and it hands you
 * what to draw, from its cache, with its scheduling.* It is `@tesseradb/client`'s main export.
 *
 * Behind it: the session client, the replica, the driver and the presented frame (all in this
 * package), the artifact channel, the encoding accumulators, item and artifact detail, filter
 * composition, and the session artifact table. What it does **not** own is a camera — C2's engine
 * does, and `setView` is how it is told (§4). Rendering is the vis side's: the store hands over a
 * `Composition` by reference and the numbers typed by what they are, never pixels.
 *
 * **Headless by construction.** The frame scheduler and the driver clock are injected, defaulting
 * to `requestAnimationFrame`/`setTimeout`, so the whole store is testable in node against a fake
 * `fetch` (or a fake `TesseraClient`) with a fake scheduler.
 *
 * **The membership column** (D12, §5.10): the point path names the layers that are on with their
 * closure, so each band arrives with its ordinals named through the session table; the
 * `artifacts` projection carries the table, the served set's ordinals and each one's colour, and
 * the colour coverage over the bands in view — a band whose ordinals no longer resolve to
 * anything served, or that lacks a column for a layer now on, is colour-stale and is refetched
 * after novel ground by the replica's own path.
 */

export type {Count, Masked} from './counts.js';
export {formatCount, formatMasked} from './counts.js';

/** A data-coordinates bbox and the pixel size it is drawn at — what `setView` takes (§4). */
export type ViewInput = {bbox: [number, number, number, number]; width: number; height: number};

/**
 * A selection shape, in **data coordinates** — the space `setView` takes and `dataXY` returns.
 * A box and a lasso are both counted (§5.11): the lasso is rasterised to the tiles it meets at
 * the same bounded depth, and typed exact by the same cell-versus-pixel rule.
 */
export type SelectionShape =
  | {kind: 'box'; bbox: [number, number, number, number]}
  | {kind: 'lasso'; points: [number, number][]};

/** How to get a viewer token: a fixed string, or a supplier the store renews before expiry. */
export type TokenSupplier = () => Promise<{token: string; expiresAt: number}>;

export type StoreOptions = {
  viewerUrl: string;
  /** A fixed token, or {@link authorise} for one the store renews. Exactly one is required. */
  token?: string;
  authorise?: TokenSupplier;
  /** Which view to answer for; defaults to `meta.views[0]`. */
  view?: string;
  /** Marks-on-screen budget — the input's default, not a ceiling (design, owner 2026-08-25). */
  budget?: number;
  /** How served artifacts are coloured (§5.10): positional by default, or spread over the served set. */
  palette?: PaletteKind;
  prefetch?: boolean;
  /** Injected for tests; browser defaults otherwise. */
  scheduler?: FrameScheduler;
  clock?: Clock;
  driver?: DriverOptions;
  replica?: Pick<ReplicaOptions, 'cacheBytes' | 'cache' | 'revalidateAfterMs' | 'onPhase'>;
  /** A client already built (a test's fake, or a host that owns `authorise`); else one is made. */
  client?: TesseraClient;
  clientOptions?: Omit<TesseraClientOptions, 'viewerUrl'>;
  /**
   * A demo-only side channel for measurement instruments — **not part of §4**. A conformant
   * client reads projections; the demo's depth/stage/calibration panels read numbers the §4
   * surface deliberately omits (predicted marks, stage timings, the plan's held-vs-fetched split,
   * m_target), so they are forwarded here rather than widening the projection table for them.
   */
  instruments?: {
    onFrame?(info: {plan: {choice: DepthChoice}; timings: import('./types.js').Timings | null; bytes: number; held: number; fetched: number; calibration: {mTarget: number; visibleInView: number | undefined}; replica: {bytes: number; points: number; bands: number}}): void;
    onTrace?(kind: string, fields: Record<string, number>): void;
  };
};

/** The projections table of §4 — each an immutable object, replaced on change. */
export type Projections = {
  meta: Meta | null;
  status: StatusProjection;
  view: ViewProjection;
  marks: MarksProjection;
  tiles: TilesProjection;
  artifacts: ArtifactsProjection;
  selection: SelectionProjection;
  region: RegionProjection | null;
  filters: FiltersProjection;
  legend: LegendProjection;
  replica: ReplicaProjection;
};

export type ProjectionName = keyof Projections;

export type StatusProjection = {
  status: PresentedStatus;
  sessionWarm: boolean;
  refusal: Refusal | null;
  /** True when the content key the latest response observed differs from the presented frame's. */
  stale: boolean;
  /** Set with a refusal that means the session ended (design §5.4's expired row). */
  expired: boolean;
  retrying: boolean;
};

export type ViewProjection = {
  /** The composition on screen and the depth it is drawn at, by reference — the deck side's input. */
  composition: Composition | null;
  depth: number;
  visible: Masked;
  matched: Masked;
  served: Count;
  /** Provisional marks — a screen fact, a plain mark count, never a masked quantity. */
  provisional: number;
};

export type MarksProjection = {
  /** The exact bands, by reference — the slab writes them once; nothing here copies them. */
  bands: Composition['exact'];
  standIn: Composition['standIn'];
  count: Count;
};

export type TilesProjection = {tiles: Composition['tiles']};

export type ArtifactsProjection = {
  /** The first layer on. */
  layer: string | null;
  /** Every layer on — the closure the request names (decision 0096). */
  layers: string[];
  served: Artifact[];
  lineage: ServedLineage;
  status: ArtifactChannelState['status'];
  refusal: {code: string; detail: string} | null;
  version: number;
  /**
   * How many artifact payloads the session holds — the served set plus everything served earlier
   * under the same identity and content keys, which the channel keeps rather than refetching
   * (`artifact-cache-handover.md`). Instrumentation: what is drawn is `served`.
   */
  held: number;
  /** The session artifact table, for a consumer resolving ordinals (§5.10). */
  table: SessionArtifactTable;
  /** The served set's ordinals — what an opened artifact resolves through. */
  servedOrdinals: ReadonlySet<number>;
  /**
   * A colour for **every ordinal the session table holds**, not only the served set's (§5.10).
   * A band held under a coarser cut, or one fetched a moment before the channel caught up with
   * a finer one, names artifacts that are not in `servedOrdinals`; its points still wear the
   * colour of an artifact the wire said they belong to, which is exact. An ordinal not here —
   * one whose whole parent chain was never seen — resolves to neutral.
   */
  colours: ReadonlyMap<number, Rgba>;
  palette: PaletteKind;
  /**
   * Colour coverage over the bands in view (§5.10): `current` resolve wholly to the served set;
   * `stale` do not, or lack a column for a layer on, and are being refetched. The status strip's
   * hover reads *colours exact* when `stale` is zero.
   */
  coverage: {current: number; stale: number};
};

export type SelectionProjection = {
  item: {id: bigint; detail: ItemDetail} | null;
  itemRefusal: Refusal | null;
  artifact: {id: bigint; detail: ArtifactDetail} | null;
  artifactRefusal: Refusal | null;
};

/**
 * The selected region and what it holds (§5.11). `visible` and `matched` are the sum over the
 * counted cells, exact only where every cell is at most a screen pixel; `served` is the held marks
 * inside the shape against `matched` — a sample, so both figures always. `status` is the counting
 * request's own: the numbers are `NO_MASKED` until it answers, and a refusal is not a zero.
 */
export type RegionProjection = {
  shape: SelectionShape;
  status: 'loading' | 'shown' | 'refused';
  refusal: Refusal | null;
  visible: Masked;
  matched: Masked;
  served: Count;
  /** The depth the region was counted at, and how many tiles that was. */
  depth: number;
  tiles: number;
  /** The held marks inside the shape — ids and world positions, the first {@link REGION_HELD_LIMIT}. */
  held: {ids: BigUint64Array; positions: Float32Array; count: number};
};

export type FiltersProjection = {
  draft: FilterDraft;
  expr: FilterExpr | null;
  values: Record<string, CategoryValue[]>;
  valueErrors: Record<string, Refusal>;
};

export type LegendProjection = {
  /** Palette rank per code, per column — assigned by observed frequency, never reordered. */
  ranks: Record<string, Ranks>;
  /** Sticky numeric domains per column — widened as marks arrive, never narrowed. */
  domains: Record<string, Domain>;
  /** Resolved category values per column — the codes drawn, named. */
  categories: Record<string, CategoryValue[]>;
  categoryErrors: Record<string, Refusal>;
  colourBy: string | null;
};

export type ReplicaProjection = {
  bytes: number;
  points: number;
  bands: number;
  lastPlan: {held: number; fetched: number} | null;
};

type Listener = () => void;

export interface Store {
  readonly projections: Projections;
  get<K extends ProjectionName>(name: K): Projections[K];
  subscribe(fn: Listener): () => void;
  subscribe<K extends ProjectionName>(name: K, fn: (value: Projections[K]) => void): () => void;

  setView(input: ViewInput): void;
  setFilters(draft: FilterDraft): void;
  /** Page a filterable category's value set into `filters.values` — for its picker. */
  loadFilterValues(column: string): Promise<void>;
  /** Turn layers on — each with its closure (decision 0096); `[]` turns every layer off. */
  setLayers(names: string[]): void;
  /**
   * Colour by a declared column, by `cluster:<layer>` for a layer that is on (a lookup-texture
   * switch on the vis side, never a per-point pass), or `null` for uniform.
   */
  setColourBy(column: string | null): void;
  setPalette(kind: PaletteKind): void;
  setBudget(budget: number): void;
  pick(id: bigint): Promise<void>;
  openArtifact(id: bigint): Promise<void>;
  /** Drop the picked point and the opened artifact — a card's close. */
  clearSelection(): void;
  /** The colour scheme the map draws on, so the positional palette reads on its ground (§5.10). */
  setScheme(scheme: PaletteScheme): void;
  select(shape: SelectionShape | null): void;
  /** A data-coordinates bbox for an artifact — what a map's `fitTo` uses. */
  extentOf(artifactId: bigint): [number, number, number, number] | null;
  /** Data coordinates from marks or artifact geometry, derived from world positions. */
  dataXY(worldX: number, worldY: number): [number, number];
  clear(): void;
  refresh(): void;
  dispose(): void;
}

/** The `colourBy` prefix that names a layer's cluster colour rather than a column. */
export const CLUSTER_PREFIX = 'cluster:';

/** How long a selection must be still before its counting request goes out — the channel's settle. */
const REGION_SETTLE_MS = 200;
/** How many held marks a region lists — the panel's list, not the count, which is always whole. */
export const REGION_HELD_LIMIT = 500;

const NO_STATUS: StatusProjection = {
  status: 'idle',
  sessionWarm: false,
  refusal: null,
  stale: false,
  expired: false,
  retrying: false
};

export function createStore(options: StoreOptions): Store {
  if (!options.token && !options.authorise && !options.client) {
    throw new Error('createStore needs a token, an authorise supplier, or a client');
  }

  const scheduler = options.scheduler ?? defaultFrameScheduler();
  const clock: Clock =
    options.clock ??
    ({
      now: () => (typeof performance !== 'undefined' ? performance.now() : Date.now()),
      after: (ms, fire) => setTimeout(fire, ms),
      cancel: (handle) => clearTimeout(handle as ReturnType<typeof setTimeout>)
    } as Clock);

  const client =
    options.client ??
    new TesseraClient({viewerUrl: options.viewerUrl, sessionUrl: '', ...options.clientOptions});

  const table = new SessionArtifactTable();

  // The token the store holds, renewed before the first refusal, and whether this store has ever
  // used one — a 401 on a token it *did* use is a swept session (design §5.4), not a bad option.
  let token: string | null = options.token ?? null;
  let expiresAt = Infinity;
  let renewTimer: unknown = null;
  let tokenEverUsed = false;

  let meta: Meta | null = null;
  let viewId = options.view ?? '';
  let budget = options.budget ?? 500_000;
  let colourBy: string | null = null;
  let palette: PaletteKind = options.palette ?? 'positional';
  let scheme: PaletteScheme = 'dark';
  let contentKeyAtFrame = '';
  let selection: SelectionShape | null = null;

  // Replica and the machinery on top of it are built after `meta`, which carries the quantisation.
  let replica: Replica | null = null;
  let presenter: Presenter | null = null;
  let channel: ArtifactChannel | null = null;
  /** The layers `setLayers` last named — held here so a call before meta survives to the channel. */
  let layersOn: string[] = [];
  let lastView: {input: ViewInput} | null = null;
  let queuedView: ViewInput | null = null; // a setView before meta arrives

  const projections: Projections = {
    meta: null,
    status: NO_STATUS,
    view: {composition: null, depth: 0, visible: NO_MASKED, matched: NO_MASKED, served: NO_COUNT, provisional: 0},
    marks: {bands: [], standIn: [], count: NO_COUNT},
    tiles: {tiles: []},
    artifacts: {layer: null, layers: [], served: [], lineage: servedLineage([]), status: 'idle', refusal: null, version: 0, held: 0, table, servedOrdinals: new Set(), colours: new Map(), palette, coverage: {current: 0, stale: 0}},
    selection: {item: null, itemRefusal: null, artifact: null, artifactRefusal: null},
    region: null,
    filters: {draft: {}, expr: null, values: {}, valueErrors: {}},
    legend: {ranks: {}, domains: {}, categories: {}, categoryErrors: {}, colourBy: null},
    replica: {bytes: 0, points: 0, bands: 0, lastPlan: null}
  };

  const all: Set<Listener> = new Set();
  const perName = new Map<ProjectionName, Set<() => void>>();

  function replaceProjection<K extends ProjectionName>(name: K, value: Projections[K]): void {
    projections[name] = value;
    perName.get(name)?.forEach((fn) => fn());
    all.forEach((fn) => fn());
  }

  // ---- the token supplier -------------------------------------------------------------------

  async function ensureToken(): Promise<string> {
    if (token && Date.now() < expiresAt - 5_000) return token;
    if (!options.authorise) {
      if (!token) throw new TesseraError(401, 'bad-credential', 'no token');
      return token;
    }
    const got = await options.authorise();
    token = got.token;
    expiresAt = got.expiresAt * (got.expiresAt < 1e12 ? 1000 : 1); // seconds or ms, tolerant
    armRenewal();
    return token;
  }

  function armRenewal(): void {
    if (renewTimer) clock.cancel(renewTimer);
    if (!options.authorise || !Number.isFinite(expiresAt)) return;
    // Renew a beat before expiry, so a warm client never presents a token the server will refuse.
    const wait = Math.max(0, expiresAt - Date.now() - 30_000);
    renewTimer = clock.after(wait, () => {
      void ensureToken().catch(() => {});
    });
  }

  /** Whether a refusal means the session ended (design §5.4's expired row). */
  function isExpiry(refusal: Refusal | null): boolean {
    if (!refusal) return false;
    if (refusal.code === 'expired-token') return true;
    // A 401 `bad-credential` on a token this store has used is a swept session, indistinguishable
    // from one that never existed — expired either way. Before the store has used a token, it is a
    // bad option, not an expiry.
    return refusal.code === 'bad-credential' && tokenEverUsed;
  }

  // ---- session bring-up ---------------------------------------------------------------------

  async function warm(): Promise<void> {
    const t = await ensureToken();
    tokenEverUsed = true;
    meta = await client.meta(t);
    if (!viewId) viewId = meta.views[0]?.id ?? '';
    replaceProjection('meta', meta);
    // Seed the filter draft from what this bundle publishes as filterable — one control per
    // operand set, all empty. A bundle without an `abstract` simply has no abstract control.
    if (Object.keys(projections.filters.draft).length === 0) {
      const draft = emptyDraft(meta.filterOperands);
      replaceProjection('filters', {...projections.filters, draft, expr: composeFilters(draft)});
    }

    layersOn = layerClosure(meta.layers, layersOn);
    replica = new Replica(
      async (req, signal, background) => {
        const tok = await ensureToken();
        tokenEverUsed = true;
        // The point path names the layers that are on, with their closure, and pays their pass
        // (§5.10): that is what puts the membership column on each band. `[]` until a layer is on
        // — and `[]` on the replica's counts-only revalidation, which absorbs no points and would
        // pay the artifact pass for a frame nobody reads.
        // The point path carries the same budget as the channel, so a point's membership column
        // names the deepest artifact of the *same* cut the panels show.
        const zoom = presenter?.view?.view.zoom ?? 0;
        return client.viewport(
          tok,
          {...req, view: viewId, filters: composeFilters(projections.filters.draft), layers: req.k === 0 ? [] : layersOn, ...(req.k === 0 || layersOn.length === 0 ? {} : {artifactBudget: artifactBudgetFor(zoom)})},
          signal,
          background
        );
      },
      meta.quantisation,
      {
        view: viewId,
        table,
        cacheBytes: options.replica?.cacheBytes,
        cache: options.replica?.cache,
        revalidateAfterMs: options.replica?.revalidateAfterMs,
        onPhase: (kind, ms, n) => {
          options.replica?.onPhase?.(kind, ms, n);
          // A stored slice is drawable now: the driver derives at most once per its gap while
          // the response streams in, so the first marks arrive with the first slice.
          if (kind === 'piece' || kind === 'store') presenter?.absorbed();
        },
        now: () => clock.now()
      }
    );

    presenter = new Presenter(
      replica,
      {
        kMaxMarks: meta.selection.kMaxMarks,
        maxTilesPerRequest: meta.maxTilesPerRequest,
        thetaTargetMarks: meta.selection.thetaTargetMarks
      },
      clock,
      scheduler,
      {onPresented: (p) => onPresented(p), onStatus, onTrace},
      {budget, ...options.driver},
      options.prefetch ?? true
    );

    channel = new ArtifactChannel(client, {
      clock,
      view: viewId,
      quantisation: meta.quantisation,
      token: () => token,
      // The drawn depth, from the projection the frame handler has just replaced — the presenter's
      // own handle is assigned after it hands the frame over, so it is one frame behind here.
      depth: () => projections.view.depth ?? presenter?.frame?.depth,
      maxTiles: meta.maxTilesPerRequest,
      table,
      onChange: onArtifacts
    });
    // A `setLayers` that arrived before meta is honoured now: the channel is what asks, and it
    // did not exist to be told. (Found by the artifacts smoke: the demo chooses its layer before
    // opening the session's store, and the choice was lost on every principal switch.)
    channel.setLayers(layersOn);

    if (queuedView) {
      const q = queuedView;
      queuedView = null;
      setView(q);
    } else if (lastView) {
      setView(lastView.input);
    }
  }

  // ---- projection updates from the machinery ------------------------------------------------

  function onStatus(status: PresentedStatus, refusal: Refusal | null): void {
    const expired = isExpiry(refusal);
    replaceProjection('status', {
      status,
      sessionWarm: projections.status.sessionWarm || status === 'shown',
      refusal,
      stale: projections.status.stale,
      expired,
      retrying: status === 'retrying'
    });
    if (status === 'refused' && !refusal) return;
  }

  /**
   * Recompute staleness against the content key. `status.stale` is true when the content key the
   * latest response observed differs from the one the presented marks were derived under — the
   * built replica's own `currentContentKey`, deliberately not `x-tessera-stale` (design §4). A
   * revalidation or an artifact response can observe a bump without a redraw, which is exactly the
   * case a client wired to the geometry stamp would miss; `onTrace('revalidate')` is the signal.
   *
   * ⊘ Eager number refresh is not built here: the design has the numbers refresh from the
   * revalidation response's counts while the marks stay stale-marked until `refresh()`; the
   * replica's counts-only response is not surfaced to the store, so the numbers hold until the
   * next derive. Marking stale — the load-bearing half — is built; the eager refresh is not.
   */
  function recomputeStale(): void {
    const observed = replica?.currentContentKey ?? '';
    const stale = contentKeyAtFrame !== '' && observed !== '' && observed !== contentKeyAtFrame;
    if (stale !== projections.status.stale) {
      replaceProjection('status', {...projections.status, stale, sessionWarm: true});
    }
  }

  function onTrace(kind: string, fields: Record<string, number>): void {
    // A revalidation observed a (possibly new) content key without redrawing the marks.
    if (kind === 'revalidate') recomputeStale();
    options.instruments?.onTrace?.(kind, fields);
  }

  function onPresented(p: Presented): void {
    const frame = p.frame;
    // A derive redrew the marks under the current content key; a fold did not, so it may reveal a
    // bump observed since. `p.fetched` is non-null exactly for a derive.
    const observed = replica?.currentContentKey ?? '';
    if (p.fetched) contentKeyAtFrame = observed;
    const stale = contentKeyAtFrame !== '' && observed !== '' && observed !== contentKeyAtFrame;

    let visible = 0n;
    let matched = 0n;
    let served = 0;
    for (const tile of frame.tiles) {
      if (!tile.counts) continue;
      visible += tile.counts.visible;
      matched += tile.counts.matched;
      served += tile.counts.served;
    }

    // A frame on screen ends *Starting session…*: the session answered, whatever the driver's
    // status says about the request still streaming.
    if (!projections.status.sessionWarm && frame.exactDrawn + frame.provisional > 0) replaceProjection('status', {...projections.status, sessionWarm: true});
    replaceProjection('view', {
      composition: frame,
      depth: frame.depth,
      visible: {value: Number(visible), exact: true},
      matched: {value: Number(matched), exact: true},
      served: {shown: served, total: Number(visible), exact: true},
      provisional: frame.provisional
    });
    replaceProjection('marks', {
      bands: frame.exact,
      standIn: frame.standIn,
      count: {shown: frame.exactDrawn, total: Number(visible), exact: true}
    });
    // The channel asks at a depth the drawn frame supplies, so a view noted before the first frame
    // was refused (no depth) and nothing else re-asks until the camera moves. The first derive is
    // that moment: ask now, or the session's opening view shows its points and no artifacts.
    if (channel && !channel.hasView && lastView && presenter?.view) {
      const v = presenter.view;
      channel.schedule({target: v.view.target, zoom: v.view.zoom}, v.width, v.height);
    }
    replaceProjection('tiles', {tiles: frame.tiles});
    accumulateEncoding(frame);
    refreshColours();
    checkColourCoverage();

    if (replica) {
      const fetched = p.fetched;
      replaceProjection('replica', {
        bytes: replica.bytes,
        points: replica.points,
        bands: replica.bandCount,
        lastPlan: fetched
          ? {held: fetched.plan.wanted - fetched.plan.novel, fetched: fetched.plan.novel}
          : projections.replica.lastPlan
      });
    }
    if (stale !== projections.status.stale) {
      replaceProjection('status', {...projections.status, stale, sessionWarm: true});
    }
    if (options.instruments?.onFrame && replica && p.fetched) {
      options.instruments.onFrame({
        plan: {choice: p.plan.choice},
        timings: p.fetched.response?.timings ?? null,
        bytes: p.fetched.plan.bytes,
        held: p.fetched.plan.wanted - p.fetched.plan.novel,
        fetched: p.fetched.plan.novel,
        calibration: p.calibration,
        replica: {bytes: replica.bytes, points: replica.points, bands: replica.bandCount}
      });
    }
  }

  function onArtifacts(state: ArtifactChannelState): void {
    recomputeStale();
    // The served set's ordinals: the channel took its reference before it emitted, so every
    // served artifact is named. They are what an opened artifact resolves through; the colours
    // are built over the whole table, which is a superset of them.
    const servedOrdinals = new Set<number>();
    for (const a of state.artifacts) {
      const ordinal = table.ordinalOf(a.layer, a.tesseraId);
      if (ordinal !== 0) servedOrdinals.add(ordinal);
    }
    replaceProjection('artifacts', {
      ...projections.artifacts,
      layer: state.layer,
      layers: state.layers,
      served: state.artifacts,
      lineage: servedLineage(state.artifacts),
      status: state.status,
      refusal: state.refusal,
      version: state.version,
      held: state.held,
      table,
      servedOrdinals,
      // **Rebuilt only when the table moved.** A response that names artifacts already held names
      // no new ordinal, so the colours and the lookup texture built from them are the same ones —
      // which is what the channel's payload store buys, and it buys nothing if this rebuilds a map
      // of the same size every settle (`artifact-cache-handover.md` §6 step 2).
      colours: table.version === colouredAt ? projections.artifacts.colours : colourTable(),
      palette
    });
    checkColourCoverage();
  }

  /** The table's version the held `colours` map was built at — a rebuild only when it moved. */
  let colouredAt = -1;

  /**
   * A colour per live ordinal (§5.10). O(live), which the ordinals' refcounts bound by resident
   * marks, and only when the table has actually gained or lost an entry — a response that names
   * artifacts already known rebuilds nothing.
   */
  function colourTable(): Map<number, Rgba> {
    colouredAt = table.version;
    return artifactColours(
      table.liveEntries().map(({ordinal, entry}) => ({ordinal, centroid: entry.centroid})),
      palette,
      scheme
    );
  }

  /**
   * Rebuild the colours if a response has named artifacts the table had not seen — the point
   * path's own artifacts frames, which arrive ahead of the debounced channel's.
   */
  function refreshColours(): void {
    if (table.version === colouredAt) return;
    replaceProjection('artifacts', {...projections.artifacts, colours: colourTable(), palette});
  }

  /** Bands already asked for again under this served-set version — a refetch is asked once. */
  const colourAsked = new Map<BandKey, number>();

  /**
   * Colour coverage (§5.10): per band in view, over its distinct list — never its points — does
   * every ordinal resolve to something colourable, for every layer on? A band that has one that
   * does not, or that lacks the column for a layer on (fetched before the layer was), is
   * colour-stale: it keeps drawing what resolves, and its tile is asked for again after novel
   * ground, once per served set, through the replica's coverage retraction and the driver's
   * ordinary plan.
   *
   * **Against the colours, not against the channel's latest served set** — a deviation from
   * §5.10's wording, reported with the change. Resolving against the served set alone made every
   * band in view stale the moment a zoom moved the cut finer, because a walk cannot go down: on
   * the 2.4M corpus one notch retracted 2,267 of 15,006 bands, refetching tiles that had just
   * arrived, and drew them neutral meanwhile. A band whose ordinals resolve to an artifact the
   * table holds is coloured, exactly, by an artifact the wire said its points belong to; it needs
   * no refetch to be correct. What remains stale is what colour-staleness is for: a band with no
   * column for a layer just switched on, and one whose parent chain was never seen.
   */
  function checkColourCoverage(): void {
    const a = projections.artifacts;
    if (!replica || !presenter || a.status !== 'shown' || a.layers.length === 0) {
      if (a.coverage.stale !== 0 || a.coverage.current !== 0) replaceProjection('artifacts', {...a, coverage: {current: 0, stale: 0}});
      return;
    }
    const started = clock.now();
    const stale: Band[] = [];
    let current = 0;
    // **In view means the visible box, not the render rect.** The channel answers for what the
    // viewer is looking at; the point path fetches a wider ring, and a band in the margin names
    // artifacts the channel never served for this view. Those resolve to neutral, correctly, and
    // are not a reason to refetch — they colour when a pan brings their artifacts into the box.
    const v = presenter.view;
    const depth = projections.view.depth;
    const visible = v ? tileRectOfBbox(worldBbox({target: [v.view.target[0], v.view.target[1]], zoom: v.view.zoom, width: v.width, height: v.height}, 1), depth) : null;
    for (const band of projections.marks.bands) {
      if (visible && (band.depth !== depth || !rectContainsTile(visible, band.x, band.y))) continue;
      let ok = true;
      for (const layer of a.layers) {
        const m = band.membership[layer];
        if (!m) {
          ok = false;
          break;
        }
        for (let i = 0; i < m.distinct.length; i++) {
          if (table.resolve(m.distinct[i]!, a.colours) === 0) {
            ok = false;
            break;
          }
        }
        if (!ok) break;
      }
      if (ok) current++;
      else stale.push(band);
    }
    const toAsk = stale.filter((b) => colourAsked.get(bandKey(b.depth, b.prefix)) !== a.version);
    for (const b of toAsk) colourAsked.set(bandKey(b.depth, b.prefix), a.version);
    if (toAsk.length > 0) {
      replica.retract(toAsk);
      presenter.reschedule();
    }
    options.instruments?.onTrace?.('coverage', {ms: clock.now() - started, bands: projections.marks.bands.length, stale: stale.length, asked: toAsk.length});
    if (a.coverage.current !== current || a.coverage.stale !== stale.length) {
      replaceProjection('artifacts', {...projections.artifacts, coverage: {current, stale: stale.length}});
    }
  }

  // ---- the encoding accumulators (in the store, §4) -----------------------------------------

  function accumulateEncoding(frame: Composition): void {
    // Cluster colour is the lookup texture's, resolved on the vis side from the table; nothing
    // accumulates for it here.
    if (!colourBy || !meta || colourBy.startsWith(CLUSTER_PREFIX)) return;
    const column = meta.declaredScalars.find((c) => c.name === colourBy);
    if (!column) return;
    if (column.category) {
      const counts = new Map<number, number>();
      for (const band of frame.exact) {
        const values = band.scalars[colourBy];
        if (values) for (const [code, n] of countCodesCached(values)) counts.set(code, (counts.get(code) ?? 0) + n);
      }
      // The stand-in pieces bootstrap the palette while this depth's own bands stream in.
      if (counts.size === 0) for (const piece of frame.standIn) countCodesInPiece(counts, piece, colourBy);
      if (counts.size === 0) return;
      const ranks = extendRanks(projections.legend.ranks[colourBy] ?? {}, counts);
      if (ranks !== projections.legend.ranks[colourBy]) {
        replaceProjection('legend', {...projections.legend, ranks: {...projections.legend.ranks, [colourBy]: ranks}});
        void resolveCategoryCodes(colourBy, counts);
      }
    } else {
      let domain: Domain | null = projections.legend.domains[colourBy] ?? null;
      for (const band of frame.exact) {
        const values = band.scalars[colourBy];
        if (values) domain = widenDomain(domain, values);
      }
      for (const piece of frame.standIn) {
        const values = piece.band.scalars[colourBy];
        if (values) domain = widenDomainOver(domain, values, piece.indices, piece.limit);
      }
      if (domain && domain !== projections.legend.domains[colourBy]) {
        replaceProjection('legend', {...projections.legend, domains: {...projections.legend.domains, [colourBy]: domain}});
      }
    }
  }

  async function resolveCategoryCodes(column: string, counts: Map<number, number>): Promise<void> {
    if (!token || !meta) return;
    if (projections.legend.categoryErrors[column]) return;
    const held = new Set((projections.legend.categories[column] ?? []).map((v) => v.code));
    const wanted = [...counts.keys()].filter((code) => !held.has(code));
    if (wanted.length === 0) return;
    try {
      const resolved = await client.categories(token, column, {codes: wanted});
      const byCode = new Map((projections.legend.categories[column] ?? []).map((v) => [v.code, v]));
      for (const v of resolved) byCode.set(v.code, v);
      replaceProjection('legend', {
        ...projections.legend,
        categories: {...projections.legend.categories, [column]: [...byCode.values()]}
      });
    } catch (error) {
      const e = error as {code?: string; detail?: string; message?: string};
      replaceProjection('legend', {
        ...projections.legend,
        categoryErrors: {...projections.legend.categoryErrors, [column]: {code: e.code ?? 'fetch-failed', detail: e.detail ?? e.message ?? String(error)}}
      });
    }
  }

  // ---- setView's conversion (§4) ------------------------------------------------------------

  function toDriverView(input: ViewInput): DriverViewState & {width: number; height: number} {
    const q = meta!.quantisation;
    const [dx0, dy0, dx1, dy1] = input.bbox;
    const [wx0, wy0] = dataToWorldXY(dx0, dy0, q);
    const [wx1, wy1] = dataToWorldXY(dx1, dy1, q);
    const bw = Math.abs(wx1 - wx0) || 1;
    const bh = Math.abs(wy1 - wy0) || 1;
    // Zoom from the tighter axis, so a camera whose aspect differs over-covers the other axis,
    // which is safe (§4).
    const zoom = Math.min(MAX_DEPTH, Math.log2(Math.min(input.width / bw, input.height / bh)));
    return {
      target: [(wx0 + wx1) / 2, (wy0 + wy1) / 2, 0],
      zoom,
      width: input.width,
      height: input.height
    };
  }

  // ---- verbs --------------------------------------------------------------------------------

  function setView(input: ViewInput): void {
    lastView = {input};
    if (!meta || !presenter) {
      // A setView before meta has arrived is queued (§4).
      queuedView = input;
      return;
    }
    const v = toDriverView(input);
    presenter.schedule({target: v.target, zoom: v.zoom}, v.width, v.height);
    channel?.schedule({target: v.target, zoom: v.zoom}, v.width, v.height);
  }

  function setFilters(draft: FilterDraft): void {
    const expr = composeFilters(draft);
    replaceProjection('filters', {...projections.filters, draft, expr});
    // A filter narrows what is served without changing the identity key, so bands held under one
    // filter are renderable under another — the client that changed the question is the only party
    // that knows the held answers are to a different one (§4).
    presenter?.cancel();
    replica?.reset();
    contentKeyAtFrame = '';
    if (lastView) setView(lastView.input);
    // A region's `matched` is under the filters, so the question changed with them.
    if (selection) select(selection);
  }

  async function loadFilterValues(column: string): Promise<void> {
    if (!token) return;
    if (projections.filters.values[column] || projections.filters.valueErrors[column]) return;
    try {
      const values = await client.categories(token, column);
      values.sort((a, b) => a.key.localeCompare(b.key));
      replaceProjection('filters', {...projections.filters, values: {...projections.filters.values, [column]: values}});
    } catch (error) {
      const e = error as {code?: string; detail?: string; message?: string};
      replaceProjection('filters', {
        ...projections.filters,
        valueErrors: {...projections.filters.valueErrors, [column]: {code: e.code ?? 'fetch-failed', detail: e.detail ?? e.message ?? String(error)}}
      });
    }
  }

  function setLayers(names: string[]): void {
    // Usually one, with its closure (decision 0096) — the request names every layer in it.
    layersOn = meta ? layerClosure(meta.layers, names) : names;
    if (!channel) {
      // Before meta: record the intent where a reader sees it; the channel adopts it at meta.
      replaceProjection('artifacts', {...projections.artifacts, layer: layersOn[0] ?? null, layers: layersOn});
      return;
    }
    channel.setLayers(layersOn);
    if (lastView && presenter?.view) {
      const v = presenter.view;
      channel?.refresh(v.view, v.width, v.height);
    }
    // A layer switched on: no held band carries its column, so every band in view is colour-stale
    // at once and refetches centre-first as the coverage check finds them (§5.10). The hulls,
    // names and counts come at once from the channel; the points take colour as bands land.
    checkColourCoverage();
  }

  function recolour(): void {
    // O(live): the colours move, the ordinals do not, and the vis side rewrites its texture.
    replaceProjection('artifacts', {...projections.artifacts, colours: colourTable(), palette});
  }

  function setPalette(kind: PaletteKind): void {
    if (kind === palette) return;
    palette = kind;
    recolour();
  }

  function setScheme(next: PaletteScheme): void {
    if (next === scheme) return;
    scheme = next;
    recolour();
  }

  function clearSelection(): void {
    replaceProjection('selection', {item: null, itemRefusal: null, artifact: null, artifactRefusal: null});
  }

  function setColourBy(column: string | null): void {
    colourBy = column;
    replaceProjection('legend', {...projections.legend, colourBy: column});
    // No refetch: every declared column is already in the held response, so this is an accumulator
    // pass over what is drawn (§4). The vis side rebuilds its layers; the mark count cannot move.
    // `cluster:<layer>` is a uniform switch on the vis side and accumulates nothing.
    if (column && projections.view.composition) accumulateEncoding(projections.view.composition);
  }

  function setBudget(next: number): void {
    if (!Number.isFinite(next) || next <= 0) return;
    budget = next;
    presenter?.setBudget(next);
    presenter?.reschedule();
  }

  async function pick(id: bigint): Promise<void> {
    if (!token) return;
    try {
      const detail = await client.item(token, id);
      replaceProjection('selection', {...projections.selection, item: {id, detail}, itemRefusal: null});
    } catch (error) {
      const e = error as {code?: string; detail?: string; message?: string};
      replaceProjection('selection', {
        ...projections.selection,
        item: null,
        itemRefusal: {code: e.code ?? 'fetch-failed', detail: e.detail ?? e.message ?? String(error)}
      });
    }
  }

  async function openArtifact(id: bigint): Promise<void> {
    if (!token) return;
    try {
      const detail = await client.artifact(token, id, {view: viewId});
      replaceProjection('selection', {...projections.selection, artifact: {id, detail}, artifactRefusal: null, item: null, itemRefusal: null});
    } catch (error) {
      const e = error as {code?: string; detail?: string; message?: string};
      replaceProjection('selection', {
        ...projections.selection,
        artifact: null,
        artifactRefusal: {code: e.code ?? 'fetch-failed', detail: e.detail ?? e.message ?? String(error)}
      });
    }
  }

  // ---- the selected region (§5.11) ------------------------------------------------------------

  let regionTimer: unknown = null;
  let regionInFlight: AbortController | null = null;
  /** The counting request's three timestamps — selected, request sent, response in — for the instruments. */
  let regionClock = {selected: 0, requested: 0, answered: 0};

  /** A shape in world space: the box, or the lasso's polygon. */
  type WorldShape = {kind: 'box'; box: [number, number, number, number]} | {kind: 'lasso'; polygon: WorldPolygon};

  /** The held marks whose world positions fall inside the shape — the region's sample (P1). */
  function heldInside(world: WorldShape): RegionProjection['held'] {
    const ids: bigint[] = [];
    const xy: number[] = [];
    let count = 0;
    const inside = world.kind === 'box' ? (x: number, y: number) => insideBox(x, y, world.box) : (x: number, y: number) => insidePolygon(x, y, world.polygon);
    const take = (band: {ids: BigUint64Array; positions: Float32Array}, i: number) => {
      const x = band.positions[i * 2]!;
      const y = band.positions[i * 2 + 1]!;
      if (!inside(x, y)) return;
      count++;
      if (ids.length < REGION_HELD_LIMIT) {
        ids.push(band.ids[i]!);
        xy.push(x, y);
      }
    };
    for (const band of projections.marks.bands) {
      for (let i = 0; i < band.ids.length; i++) take(band, i);
    }
    for (const piece of projections.marks.standIn) {
      if (piece.indices) for (const i of piece.indices.slice(0, piece.limit)) take(piece.band, i);
      else for (let i = 0; i < Math.min(piece.limit, piece.band.ids.length); i++) take(piece.band, i);
    }
    return {ids: BigUint64Array.from(ids), positions: Float32Array.from(xy), count};
  }

  function worldOfShape(shape: SelectionShape): WorldShape | null {
    if (!meta) return null;
    const q = meta.quantisation;
    if (shape.kind === 'box') {
      const [x0, y0] = dataToWorldXY(shape.bbox[0], shape.bbox[1], q);
      const [x1, y1] = dataToWorldXY(shape.bbox[2], shape.bbox[3], q);
      return {kind: 'box', box: [Math.min(x0, x1), Math.min(y0, y1), Math.max(x0, x1), Math.max(y0, y1)]};
    }
    if (shape.points.length < 3) return null;
    return {kind: 'lasso', polygon: shape.points.map(([x, y]) => dataToWorldXY(x, y, q))};
  }

  /**
   * Count the selected region: one `k = 0` request in the `tiles` form at the bounded depth,
   * debounced like the artifact channel so a redrawn shape costs one request per settled gesture.
   * The held marks inside are computed at once — the client's own facts (P1) — and the numbers
   * land when the server answers.
   */
  function select(shape: SelectionShape | null): void {
    selection = shape;
    if (regionTimer) clock.cancel(regionTimer);
    regionTimer = null;
    regionInFlight?.abort();
    regionInFlight = null;
    const world = shape ? worldOfShape(shape) : null;
    if (!shape || !world) {
      replaceProjection('region', null);
      return;
    }
    const {depth, tiles} = world.kind === 'box' ? rasteriseBox(world.box) : rasterisePolygon(world.polygon);
    const zoom = presenter?.view?.view.zoom ?? 0;
    const held = heldInside(world);
    regionClock = {selected: clock.now(), requested: 0, answered: 0};
    replaceProjection('region', {
      shape,
      status: 'loading',
      refusal: null,
      visible: NO_MASKED,
      matched: NO_MASKED,
      served: {shown: held.count, total: 0, exact: false},
      depth,
      tiles: tiles.length,
      held
    });
    regionTimer = clock.after(REGION_SETTLE_MS, () => {
      regionTimer = null;
      void countRegion(shape, tiles, depth, cellExceedsPixel(depth, zoom));
    });
  }

  async function countRegion(shape: SelectionShape, tiles: bigint[], depth: number, coarse: boolean): Promise<void> {
    const signal = new AbortController();
    regionInFlight = signal;
    try {
      const tok = await ensureToken();
      tokenEverUsed = true;
      regionClock.requested = clock.now();
      const response = await client.viewport(
        tok,
        {view: viewId, zoom: depth, tiles, k: 0, filters: composeFilters(projections.filters.draft), layers: []},
        signal.signal
      );
      regionClock.answered = clock.now();
      if (regionInFlight !== signal || selection !== shape) return;
      regionInFlight = null;
      let visible = 0n;
      let matched = 0n;
      for (const t of response.result.tiles) {
        visible += t.visible;
        matched += t.matched;
      }
      const current = projections.region;
      if (!current || current.shape !== shape) return;
      replaceProjection('region', {
        ...current,
        status: 'shown',
        refusal: null,
        // Exact for the cells asked; exact for the shape only when no cell exceeds a pixel.
        visible: {value: Number(visible), exact: !coarse},
        matched: {value: Number(matched), exact: !coarse},
        // The sample is the held marks inside, against the region's matched — both figures always.
        served: {shown: current.held.count, total: Number(matched), exact: true}
      });
      // The three lanes of a counting request, for the delivery record: the settle, the wire (with
      // its main-thread decode), and the projection.
      options.instruments?.onTrace?.('region', {
        settleMs: regionClock.requested - regionClock.selected,
        wireMs: regionClock.answered - regionClock.requested,
        projectMs: clock.now() - regionClock.answered,
        serverMs: response.timings.serverUs / 1000,
        tiles: tiles.length
      });
    } catch (error) {
      if (signal.signal.aborted || regionInFlight !== signal) return;
      regionInFlight = null;
      const current = projections.region;
      if (!current || current.shape !== shape) return;
      const e = error as {code?: string; detail?: string; message?: string};
      replaceProjection('region', {
        ...current,
        status: 'refused',
        refusal: {code: e.code ?? 'fetch-failed', detail: e.detail ?? e.message ?? String(error)},
        visible: NO_MASKED,
        matched: NO_MASKED,
        served: {shown: current.held.count, total: 0, exact: false}
      });
    }
  }

  function extentOf(artifactId: bigint): [number, number, number, number] | null {
    const artifact = projections.artifacts.served.find((a) => a.tesseraId === artifactId);
    const box = artifact?.box;
    if (!box || !meta) return null;
    // The box is in the wire's 32-bit grid units (contracts §3.2), the same units the outlines
    // draw from: through world space, by the one conversion every reader of wire geometry uses.
    const [x0, y0] = dataXY(gridToWorld(box[0]), gridToWorld(box[1]));
    const [x1, y1] = dataXY(gridToWorld(box[2]), gridToWorld(box[3]));
    return [x0, y0, x1, y1];
  }

  function dataXY(worldX: number, worldY: number): [number, number] {
    const q = meta!.quantisation;
    return [
      q.xMin + (worldX / WORLD_SIZE) * (q.xMax - q.xMin),
      q.yMin + (worldY / WORLD_SIZE) * (q.yMax - q.yMin)
    ];
  }

  function clear(): void {
    presenter?.cancel();
    channel?.reset();
    replica?.reset();
    table.clear();
    contentKeyAtFrame = '';
    replaceProjection('view', {composition: null, depth: 0, visible: NO_MASKED, matched: NO_MASKED, served: NO_COUNT, provisional: 0});
    replaceProjection('marks', {...projections.marks, bands: [], count: NO_COUNT});
    replaceProjection('legend', {ranks: {}, domains: {}, categories: {}, categoryErrors: {}, colourBy});
    replaceProjection('status', {...NO_STATUS});
    selection = null;
    replaceProjection('region', null);
  }

  function refresh(): void {
    // Redraw the marks against the refreshed content key — a held layer set goes with it (§4).
    channel?.reset();
    if (lastView) setView(lastView.input);
    if (selection) select(selection);
  }

  function dispose(): void {
    presenter?.cancel();
    channel?.cancel();
    if (regionTimer) clock.cancel(regionTimer);
    regionInFlight?.abort();
    if (renewTimer) clock.cancel(renewTimer);
    client.close();
  }

  const store: Store = {
    get projections() {
      return projections;
    },
    get(name) {
      return projections[name];
    },
    subscribe(nameOrFn: ProjectionName | Listener, fn?: (value: never) => void): () => void {
      if (typeof nameOrFn === 'function') {
        all.add(nameOrFn);
        return () => all.delete(nameOrFn);
      }
      const name = nameOrFn;
      const set = perName.get(name) ?? new Set();
      const wrapped = () => (fn as (v: Projections[typeof name]) => void)(projections[name]);
      set.add(wrapped);
      perName.set(name, set);
      return () => set.delete(wrapped);
    },
    setView,
    setFilters,
    loadFilterValues,
    setLayers,
    setColourBy,
    setPalette,
    setBudget,
    pick,
    openArtifact,
    clearSelection,
    setScheme,
    select,
    extentOf,
    dataXY,
    clear,
    refresh,
    dispose
  };

  void warm().catch((error) => {
    const e = error as {code?: string; detail?: string; message?: string; status?: number};
    const refusal = {code: e.code ?? 'fetch-failed', detail: e.detail ?? e.message ?? String(error)};
    replaceProjection('status', {...projections.status, status: 'refused', refusal, expired: isExpiry(refusal)});
  });

  return store;
}

/** Re-export for a consumer building a `ViewportResult` in a test. */
export type {ViewportResult};
