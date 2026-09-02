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
import {composeFilters, emptyDraft, type ClauseVerb, type FilterDraft} from './filters.js';
import {withMember, withMembers, withoutMember, type MemberClause} from './members.js';
import {Presenter, defaultFrameScheduler, type FrameScheduler, type PresentedStatus, type Refusal} from './presented.js';
import {insideBox, insidePolygon, regionOperand, withRegion, type WorldPolygon} from './region.js';
import {isFilterLayer, layerClosure} from './layers.js';
import {requestLevels} from './artifactChannel.js';
import {artifactBudgetFor} from './artifactBudget.js';
import {artifactColours, positionalEntry, type PaletteKind, type PaletteScheme, type Rgba} from './palette.js';
import type {Band, BandKey} from './bands.js';
import {BandBudget, bandKey} from './bands.js';
import {DEFAULT_CACHE_BYTES, Replica, type ReplicaOptions} from './replica.js';
import {TesseraClient, TesseraError, type TesseraClientOptions} from './client.js';
import type {DepthChoice} from './budget.js';
import type {Presented} from './presented.js';
import type {
  Artifact,
  ArtifactDetail,
  BrowsePage,
  BrowseRequest,
  CategoryValue,
  FilterExpr,
  ItemDetail,
  Meta,
  Quantisation,
  RegionVerdict,
  Shape,
  ShapeKind,
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
 * A selection, in **data coordinates** — the space `setView` takes and `dataXY` returns — or a
 * published shape named by its `tessera_id` (*filter to this* on an artifact card).
 *
 * **A selection is a filter** (`selection-operand.md`; §5.11): it rides every viewport request as
 * the `region` leaf composed with the other filters, so the marks, the counts and the artifacts'
 * `matched` bits narrow to it, and the region's own count is read off the same frame as
 * everything else — no counting request of its own. `outside` negates it: `none_of` over the
 * leaf, the complement within what this principal can see (`polygon-membership.md` §8).
 */
export type SelectionShape = (
  | {kind: 'box'; bbox: [number, number, number, number]}
  | {kind: 'lasso'; points: [number, number][]}
  | {kind: 'artifact'; id: bigint}
) & {outside?: boolean};

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
    onTrace?(kind: string, fields: Record<string, number | string>): void;
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
  /**
   * The view the store is answering from — `''` before `meta`, then a view id `meta.views` lists
   * (`view-switching.md` §3). A component that draws or lists compares this, not `frame()`, to
   * learn that a switch happened; the frame is what it draws under once it has.
   */
  id: string;
  /** The composition on screen and the depth it is drawn at, by reference — the deck side's input. */
  composition: Composition | null;
  depth: number;
  visible: Masked;
  matched: Masked;
  /**
   * The sum of the frame's per-tile `highlighted` — *the highlight matched N points*
   * (`highlight-and-hierarchy.md` §2, §5.2).
   *
   * **Equal to `matched` where no highlight is set**, because the wire's column is: a client
   * reading it never has to ask whether the question was put. Which is why the strip's third line
   * is drawn from {@link ViewProjection.highlighting} and not from this being different — the two
   * are legitimately equal when a highlight matches everything the filter did.
   */
  highlighted: Masked;
  /** Whether the request behind this frame carried a `highlight` at all. */
  highlighting: boolean;
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
   * The shapes fetched by identifier, by `tesseraId` — what the map draws.
   *
   * **Not part of the viewport's answer.** The channel asks for centroids and boxes; a consumer
   * that wants a shape calls {@link TesseraStore.needShape} and reads it here when it lands. An
   * artifact with no entry has not been asked for or has not answered yet, and a consumer draws
   * its `box` meanwhile. A **derived** shape here was fetched by this principal and is dropped
   * with the principal; a **predicate** or an **authored** one is the same for every principal
   * and survives a switch (`polygon-membership.md` §7.1, the layer's `shape` kind in the meta).
   */
  shapes: ReadonlyMap<bigint, Shape>;
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
 * The selected region and what it holds (§5.11), read off the presented frame: the region is a
 * leaf of the request, so `matched` is the sum of the frame's `matched` counts — the items inside
 * the shape that the other filters admit — exact for the shape when the server said so
 * (`verdict`) and the frame's exact tiles cover the shape, and a cover otherwise. `visible` is
 * the region alone: the same number where no other filter is on, and `null` where one is — the
 * frame answered a narrower question, and a figure for the wider one would be a second request.
 * `served` is the held marks inside against `matched` — a sample, so both figures always.
 * `status` is the frame's own: `loading` until a derive lands after the selection, and a refusal
 * of the request carrying the leaf is the region's refusal, never a zero.
 */
export type RegionProjection = {
  shape: SelectionShape;
  status: 'loading' | 'shown' | 'refused';
  refusal: Refusal | null;
  visible: Masked | null;
  matched: Masked;
  served: Count;
  /** `x-tessera-region`: exact for the shape, or a cover at a depth; `null` until it has answered. */
  verdict: RegionVerdict | null;
  /** The held marks inside the shape — ids and world positions, the first {@link REGION_HELD_LIMIT}. */
  held: {ids: BigUint64Array; positions: Float32Array; count: number};
};

export type FiltersProjection = {
  draft: FilterDraft;
  /**
   * The controls in the `filter` position, composed — what rides the request's `filters` beside
   * the drawn region's leaf. Null for the unfiltered request.
   */
  expr: FilterExpr | null;
  /**
   * The controls and `member_of` clauses in the `highlight` position, composed — what rides the
   * request's `highlight` (`highlight-and-hierarchy.md` §5.2). Null where nothing is highlighted.
   *
   * **A filter never moves the mask and a highlight never moves the draw**: these are two fields
   * of one request and the store keeps them apart from composition onwards, so a clause's
   * position is the only thing that decides which of the two it reaches.
   */
  highlight: FilterExpr | null;
  /** The `member_of` clauses held, in either position (§3, §5.5). */
  members: MemberClause[];
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
  /** Bytes held across **every** view's bands — the figure the one budget bounds (`view-switching.md` §3). */
  bytes: number;
  points: number;
  bands: number;
  /** How many views hold any band. */
  views: number;
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
  /**
   * Replace the `member_of` clauses (`highlight-and-hierarchy.md` §3, §5.5): the card's *filter to
   * this* and *outside this*, and a hierarchy panel's nodes.
   *
   * Like `setFilters`, this is the whole set rather than an edit, so a caller composes with
   * `withMember`/`withoutMember` and the store never has to reconcile two half-states. It requeries
   * for the same reason `setFilters` does — the question changed — and a clause in the `highlight`
   * position moves no mask, which is asserted where it is composed.
   */
  setMembers(clauses: readonly MemberClause[]): void;
  /**
   * One page of a layer's hierarchy by lineage (`highlight-and-hierarchy.md` §4) — roots, an
   * artifact's children and parents, or a name search.
   *
   * **Not a projection.** The panel walks a tree, opening and paging nodes at its own pace, and
   * what is expanded is the panel's state rather than the store's; a projection would have to hold
   * the walk and would be rebuilt on every store tick. What the store owns is the token and the
   * question: `filters` is supplied from the store's own composition where the caller does not
   * name one, so a filtered map and a filtered tree read the same numbers.
   */
  browse(req: Omit<BrowseRequest, 'filters' | 'view'> & {filters?: FilterExpr | null; view?: string}): Promise<BrowsePage>;
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
  /**
   * Make `id` the view the store answers from (`view-switching.md` §3): a pointer change, never a
   * rebuild. Queued before `meta` arrives; an id `meta.views` does not list is ignored and traced.
   */
  setCurrentView(id: string): void;
  /**
   * The extent this store's view is quantised against, or `null` before `meta` has arrived.
   *
   * **A view's, not the bundle's** (decision 0040) — a host converting between data coordinates
   * and the world space the camera works in needs the frame of the view it is looking at, and a
   * second view of the same bundle may declare another.
   */
  frame(): Quantisation | null;
  pick(id: bigint): Promise<void>;
  /**
   * One item's record for a *hint*, held once asked — what a hover wants, where {@link pick} is
   * what a click wants.
   *
   * **It writes no projection.** `pick` puts the record in `selection`, which opens a card; a
   * pointer crossing a map must not open anything, so this answers the caller and nothing else.
   *
   * **The reason it exists at all is that a name cannot be drawn from the marks.** A text column
   * lives in the record blob and is refused `render` (records-and-search §3), so no viewport
   * response can carry one — the id is genuinely all a mark has, and the alternative to a request
   * here is a tooltip that names an opaque number.
   *
   * At most one request per id: the answer is held, and so is a refusal, so a hover that cannot
   * be answered is not asked again on every pointer move. Held per principal — {@link clear}
   * drops it, since an item this session cannot see is not a fact about the item.
   */
  describe(id: bigint): Promise<Record<string, unknown> | null>;
  openArtifact(id: bigint): Promise<void>;
  /**
   * Ask for one artifact's shape, if it is not already held.
   *
   * **The shape is fetched where it is drawn.** The viewport asks for centroids and boxes
   * (`artifactChannel.ts`), because a derived shape costs a per-request derivation over every
   * member this principal can see and a settled view carries a couple of hundred artifacts while
   * the map draws one. This is the one that draws: call it for the hovered and the opened
   * artifact, and the shape arrives in `artifacts.shapes` a moment later, served at the depth the
   * view is at.
   *
   * Idempotent and cheap to call on every pointer move: a shape already held, or already in
   * flight, is not asked for twice. A derived shape is dropped whenever the mask could have moved,
   * because holding one across that would draw another viewer's shape; a predicate or an authored
   * shape is the same for every principal and is kept.
   */
  needShape(id: bigint): void;
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

/** How many held marks a region lists — the panel's list, not the count, which is always whole. */
export const REGION_HELD_LIMIT = 500;

/**
 * How long a view waits, after becoming current, before it asks for the camera it inherited.
 *
 * The driver's own debounce, applied a level up: a slider held down steps through a group's views
 * faster than this, and each step's request is cancelled by the next, so a passed-over view costs
 * nothing and the one the hand stops on issues the single request (`view-switching.md` §4). It is
 * here rather than in the driver because the driver's leading edge fires for a view arriving from
 * stillness, which every stepped-to view is.
 */
const VIEW_SETTLE_MS = 140;

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

  /**
   * One view's geometry machinery (`view-switching.md` §3). A replica's bands are quantised under
   * one frame, a presenter's composition is one view's drawn frame and its counts, and a channel's
   * served set is per row space, so all three are per view by construction. Everything else in the
   * store — the token, the mask, `meta`, the filters, the colour, the layer choice, the budget,
   * the item details and the session artifact table — is shared and untouched by a switch.
   */
  type ViewMachinery = {id: string; replica: Replica; presenter: Presenter; channel: ArtifactChannel};

  /** Built on first visit, kept on leaving: a switch is a pointer change, never a rebuild (§3). */
  const perView = new Map<string, ViewMachinery>();

  /** One byte budget across every view's bands, evicted least-recently-drawn across them (§3). */
  const bandBudget = new BandBudget(options.replica?.cacheBytes ?? DEFAULT_CACHE_BYTES);

  // The current view's machinery, rebound at a switch so every reader below — and every consumer
  // reading through an accessor — follows the current view without knowing a switch happened.
  // Built after `meta`, which carries the views and their frames.
  let replica: Replica | null = null;
  let presenter: Presenter | null = null;
  let channel: ArtifactChannel | null = null;
  /** The layers `setLayers` last named — held here so a call before meta survives to the channel. */
  let layersOn: string[] = [];
  let lastView: {input: ViewInput} | null = null;
  let queuedView: ViewInput | null = null; // a setView before meta arrives
  /** A `setCurrentView` before meta arrives — applied at warm-up in place of `options.view` (§3). */
  let queuedCurrentView: string | null = null;
  /** The pending {@link VIEW_SETTLE_MS} wait, cancelled by the next switch. */
  let switchTimer: unknown = null;
  /**
   * A switch published an empty frame and `loading`, and the incoming view's own bands are what
   * answer it: the driver transitions on requests, and a frame derived from the cache is not one.
   * Cleared by {@link setView}, so only a frame presented before anything was asked for may end
   * the wait — a cold switch stays at `loading` until the request it triggered says otherwise.
   */
  let awaitingSwitchFrame = false;

  const projections: Projections = {
    meta: null,
    status: NO_STATUS,
    view: {id: '', composition: null, depth: 0, visible: NO_MASKED, matched: NO_MASKED, highlighted: NO_MASKED, highlighting: false, served: NO_COUNT, provisional: 0},
    marks: {bands: [], standIn: [], count: NO_COUNT},
    tiles: {tiles: []},
    artifacts: {layer: null, layers: [], served: [], lineage: servedLineage([]), status: 'idle', refusal: null, version: 0, held: 0, table, servedOrdinals: new Set(), shapes: new Map(), colours: new Map(), palette, coverage: {current: 0, stale: 0}},
    selection: {item: null, itemRefusal: null, artifact: null, artifactRefusal: null},
    region: null,
    filters: {draft: {}, expr: null, highlight: null, members: [], values: {}, valueErrors: {}},
    legend: {ranks: {}, domains: {}, categories: {}, categoryErrors: {}, colourBy: null},
    replica: {bytes: 0, points: 0, bands: 0, views: 0, lastPlan: null}
  };

  const all: Set<Listener> = new Set();
  const perName = new Map<ProjectionName, Set<() => void>>();

  /**
   * Publish one projection to every subscriber — **each one, whatever the others do.** A
   * subscriber that throws is reported — to the console with its stack, and to `onTrace` — and
   * the fan-out continues past it: a `forEach` that let the throw escape stopped at the first bad
   * listener, and every element subscribed after it drew the previous publish for as long as the
   * fault lasted — a status strip at zero beside a million marks, or a blank map beside a live
   * one, depending on nothing but connection order.
   */
  function replaceProjection<K extends ProjectionName>(name: K, value: Projections[K]): void {
    projections[name] = value;
    perName.get(name)?.forEach(deliver);
    all.forEach(deliver);
  }

  function deliver(fn: () => void): void {
    try {
      fn();
    } catch (error) {
      console.error('a store subscriber threw; the other subscribers were still told', error);
      options.instruments?.onTrace?.('subscriber-fault', {message: error instanceof Error ? error.message : String(error)});
    }
  }

  // ---- the token supplier -------------------------------------------------------------------

  async function ensureToken(): Promise<string> {
    if (token && Date.now() < expiresAt - 5_000) return token;
    if (!options.authorise) {
      if (!token) throw new TesseraError(401, 'bad-credential', 'no token');
      return token;
    }
    const got = await options.authorise();
    // A derived shape is a function of the principal's own visible members, so one held across a
    // change of token would draw the previous principal's shape against the new one's identifiers.
    if (token !== got.token) forgetShapes('derived');
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

  /**
   * The frame the store's current view is quantised against, or `null` before `meta` has arrived
   * — **the view's, not the bundle's** (decision 0040): every conversion between wire grid units
   * and data coordinates is a fraction of *this* view's extent, and a second view of the same
   * bundle may declare another.
   */
  function frameOrNull(): Quantisation | null {
    return quantisationOf(viewId);
  }

  /** One named view's frame, or `null` where the bundle declares no such view. */
  function quantisationOf(id: string): Quantisation | null {
    return meta?.views.find((v) => v.id === id)?.quantisation ?? null;
  }

  /**
   * Whether two views share a frame — the question §4 turns on. Every view of a group quantises
   * against one extent (`views.md` §3.1), so the Morton addresses, the depth and the camera mean
   * the same thing in both and a switch keeps them; two frames that differ share nothing, and a
   * camera carried across would put the marks somewhere the user did not point at.
   *
   * By value, not by identity: `meta` hands out a fresh object per view.
   */
  function sameFrame(a: Quantisation | null, b: Quantisation | null): boolean {
    return (
      a !== null && b !== null && a.xMin === b.xMin && a.xMax === b.xMax && a.yMin === b.yMin && a.yMax === b.yMax
    );
  }

  /**
   * {@link frameOrNull} where the caller has already established that `meta` is in hand.
   *
   * It throws rather than returning a default because there is no default to return: a guessed
   * extent draws every point in the wrong place, and nothing downstream would notice.
   */
  function frame(): Quantisation {
    const q = frameOrNull();
    if (!q) throw new Error(`the bundle declares no view '${viewId}'`);
    return q;
  }

  // ---- session bring-up ---------------------------------------------------------------------

  /**
   * Build one view's machinery (`view-switching.md` §3), on its first visit and never again.
   *
   * Each part is bound to **this** view's id and frame, not to whichever view is current: the
   * replica names it on every request, the channel asks under it, and the events they emit are
   * dropped where the view is no longer the one being drawn — a held view emits nothing anybody
   * reads, and cannot write the current view's projections. The session artifact table is the one
   * thing handed in from outside: an ordinal indexes a colour rather than a position, so an
   * artifact identity served in two views takes one ordinal and one colour (§3).
   */
  function buildView(id: string): ViewMachinery {
    const m = meta;
    if (!m) throw new Error('a view is built after meta');
    const q = quantisationOf(id);
    if (!q) throw new Error(`the bundle declares no view '${id}'`);
    /** This view's own presenter, read by the fetch closure below; assigned a few lines on. */
    let ownPresenter: Presenter | null = null;
    const current = () => id === viewId;

    const built = new Replica(
      async (req, signal, background, onPart) => {
        const tok = await ensureToken();
        tokenEverUsed = true;
        // The point path names the layers that are on, with their closure, and pays their pass
        // (§5.10): that is what puts the membership column on each band. `[]` until a layer is on
        // — and `[]` on the replica's counts-only revalidation, which absorbs no points and would
        // pay the artifact pass for a frame nobody reads.
        // The point path carries the same budget as the channel, so a point's membership column
        // names the deepest artifact of the *same* cut the panels show.
        const zoom = ownPresenter?.view?.view.zoom ?? 0;
        return client.viewport(
          tok,
          {
            ...req,
            view: id,
            filters: requestFilters(),
            // Beside it and never instead of it: the draw is unchanged by a highlight, so this
            // costs the response three columns and nothing else (`highlight-and-hierarchy.md` §2).
            highlight: requestHighlight(),
            layers: req.k === 0 ? [] : layersOn,
            ...(req.k === 0 || layersOn.length === 0 ? {} : {artifactBudget: artifactBudgetFor(zoom)}),
            // The levels from the camera zoom, so the membership column names the same cut the
            // channel asks for — and not the deepest level alone, which is what the server's
            // depth-keyed default answers a budget-deepened request with (`requestLevels`).
            ...(req.k === 0 || layersOn.length === 0 || requestLevels(m.layers, layersOn, zoom) === undefined ? {} : {levels: requestLevels(m.layers, layersOn, zoom)})
          },
          signal,
          background,
          onPart
        );
      },
      q,
      {
        view: id,
        table,
        budget: bandBudget,
        cache: options.replica?.cache,
        revalidateAfterMs: options.replica?.revalidateAfterMs,
        onPhase: (kind, ms, n) => {
          options.replica?.onPhase?.(kind, ms, n);
          // A stored slice is drawable now: the driver derives at most once per its gap while
          // the response streams in, so the first marks arrive with the first slice.
          if (kind === 'piece' || kind === 'store') ownPresenter?.absorbed();
        },
        now: () => clock.now()
      }
    );

    ownPresenter = new Presenter(
      built,
      {
        kMaxMarks: m.selection.kMaxMarks,
        maxTilesPerRequest: m.maxTilesPerRequest,
        thetaTargetMarks: m.selection.thetaTargetMarks
      },
      clock,
      scheduler,
      {
        onPresented: (p) => {
          if (current()) onPresented(p);
        },
        onStatus: (status, refusal) => {
          if (current()) onStatus(status, refusal);
        },
        onTrace: (kind, fields) => {
          if (current()) onTrace(kind, fields);
        }
      },
      {budget, ...options.driver},
      options.prefetch ?? true
    );

    const viewChannel = new ArtifactChannel(client, {
      clock,
      view: id,
      quantisation: q,
      token: () => token,
      // The drawn depth, from the projection the frame handler has just replaced — the presenter's
      // own handle is assigned after it hands the frame over, so it is one frame behind here. A
      // view that is not current has no drawn depth in the projection and reads its own.
      depth: () => (current() ? projections.view.depth : undefined) ?? ownPresenter?.frame?.depth,
      maxTiles: m.maxTilesPerRequest,
      table,
      // The same composition the point path sends — the region leaf included, so an artifact's
      // `matched` bit is *has a member inside the selection the filters admit*: a filtered view
      // asks the server (the bit is per request, decision 0104), an unfiltered one over scopes
      // held whole is served locally.
      filters: () => requestFilters(),
      // What classifies each layer for the fetch model — levelled and flat scopes may be held
      // whole; treed ones ask per view always.
      declarations: m.layers,
      onChange: (state) => {
        if (current()) onArtifacts(state);
      }
    });
    // A `setLayers` that arrived before meta is honoured now: the channel is what asks, and it
    // did not exist to be told. (Found by the artifacts smoke: the demo chooses its layer before
    // opening the session's store, and the choice was lost on every principal switch.)
    viewChannel.setLayers(layersOn);

    const machinery: ViewMachinery = {id, replica: built, presenter: ownPresenter, channel: viewChannel};
    perView.set(id, machinery);
    return machinery;
  }

  /** This view's machinery, built on first visit (§3). */
  function machineryFor(id: string): ViewMachinery {
    return perView.get(id) ?? buildView(id);
  }

  /** Point the store's own handles at one view's machinery — the whole of what a switch changes. */
  function bind(machinery: ViewMachinery): void {
    replica = machinery.replica;
    presenter = machinery.presenter;
    channel = machinery.channel;
  }

  async function warm(): Promise<void> {
    const t = await ensureToken();
    tokenEverUsed = true;
    meta = await client.meta(t);
    // A `setCurrentView` before meta names the view to open with, in place of `options.view` (§3);
    // an id the bundle does not declare is refused here exactly as it is afterwards.
    if (queuedCurrentView !== null) {
      const wanted = queuedCurrentView;
      queuedCurrentView = null;
      if (meta.views.some((v) => v.id === wanted)) viewId = wanted;
      else onTrace('view-switch', {refused: 1, id: wanted});
    }
    if (!viewId) viewId = meta.views[0]?.id ?? '';
    replaceProjection('meta', meta);
    replaceProjection('view', {...projections.view, id: viewId});
    // Seed the filter draft from what this bundle publishes as filterable — one control per
    // operand set, all empty. A bundle without an `abstract` simply has no abstract control.
    if (Object.keys(projections.filters.draft).length === 0) {
      const draft = emptyDraft(meta.filterOperands);
      replaceProjection('filters', {...projections.filters, draft, expr: composeFilters(draft, 'filter'), highlight: composeFilters(draft, 'highlight')});
    }

    layersOn = drawnOnly(layerClosure(meta.layers, layersOn));
    bind(machineryFor(viewId));

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
    // The request carrying the region leaf was refused — a polygon over `max_region_vertices`,
    // a coordinate that is not one — so the region's numbers are a refusal and never a zero.
    const region = projections.region;
    if (status === 'refused' && refusal && region && region.status !== 'refused') {
      replaceProjection('region', {...region, status: 'refused', refusal, visible: null, matched: NO_MASKED, served: {shown: region.held.count, total: 0, exact: false}});
    }
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

  function onTrace(kind: string, fields: Record<string, number | string>): void {
    // A revalidation observed a (possibly new) content key without redrawing the marks.
    if (kind === 'revalidate') {
      recomputeStale();
      observeArtifactRotation();
    }
    options.instruments?.onTrace?.(kind, fields);
  }

  /**
   * Hand the point path's content-key observation to the artifact channel. While its held scopes
   * answer views locally the channel issues no request of its own, so this is the only route by
   * which a rotation can reach its rule-7 drop (`artifact-cache-handover.md` §4a.3: the point path
   * carries the key on every response, so the client learns without asking).
   */
  function observeArtifactRotation(): void {
    const observed = replica?.currentContentKey;
    if (observed) channel?.observeContentKey(observed);
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
    let highlighted = 0n;
    let served = 0;
    for (const tile of frame.tiles) {
      if (!tile.counts) continue;
      visible += tile.counts.visible;
      matched += tile.counts.matched;
      // Exact, and the whole point of the third line: it counts the members the cap clause did
      // not draw as well as the ones it did.
      highlighted += tile.counts.highlighted;
      served += tile.counts.served;
    }

    // A frame on screen ends *Starting session…*: the session answered, whatever the driver's
    // status says about the request still streaming.
    if (!projections.status.sessionWarm && frame.exactDrawn + frame.provisional > 0) replaceProjection('status', {...projections.status, sessionWarm: true});
    // A switch published `loading` over an empty frame; this one came from the view's own bands
    // and no request will arrive to transition the status (`view-switching.md` §3).
    if (awaitingSwitchFrame) {
      awaitingSwitchFrame = false;
      if (projections.status.status === 'loading') replaceProjection('status', {...projections.status, status: 'shown'});
    }
    replaceProjection('view', {
      id: viewId,
      composition: frame,
      depth: frame.depth,
      visible: {value: Number(visible), exact: true},
      matched: {value: Number(matched), exact: true},
      highlighted: {value: Number(highlighted), exact: true},
      // Read off the composition rather than off the counts: `highlighted` equals `matched`
      // legitimately, and *there is no highlight* is a different state from *the highlight
      // matched everything the filter did*.
      highlighting: requestHighlight() !== null,
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
    projectRegion(frame, replica?.lastRegionVerdict ?? null, p.fetched !== null, Number(matched));
    accumulateEncoding(frame);
    refreshColours();
    checkColourCoverage();

    if (replica) {
      const fetched = p.fetched;
      publishReplica(
        fetched
          ? {held: fetched.plan.wanted - fetched.plan.novel, fetched: fetched.plan.novel}
          : projections.replica.lastPlan
      );
    }
    if (stale !== projections.status.stale) {
      replaceProjection('status', {...projections.status, stale, sessionWarm: true});
    }
    // After the frame's projections are settled: a rotation the points observed reaches the
    // artifact channel's rule-7 drop, which matters exactly when held scopes answer views locally.
    if (p.fetched) observeArtifactRotation();
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

  /**
   * The `replica` projection (`view-switching.md` §3): **`bytes` is the whole cache** — every
   * view's bands, the figure the one budget bounds — and `views` how many views hold any band,
   * which is what a default budget is measured against. `points` and `bands` are the current
   * view's: they describe what is drawable now.
   */
  function publishReplica(lastPlan: ReplicaProjection['lastPlan']): void {
    replaceProjection('replica', {
      bytes: replica?.bytes ?? 0,
      points: replica?.points ?? 0,
      bands: replica?.bandCount ?? 0,
      views: replica?.heldViews ?? 0,
      lastPlan
    });
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
      shapes: heldShapes,
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
  /** The palette and ground the held map was built under; either moving is a whole rebuild. */
  let colouredUnder: {palette: PaletteKind; scheme: PaletteScheme} | null = null;
  /**
   * The map the `artifacts` projection publishes. It is a `ReadonlyMap` to every reader and this
   * is the one writer: an extension for newly named ordinals is applied here, in place, so the
   * map's identity is what tells a reader an extension from a recolour.
   */
  let colourMap = new Map<number, Rgba>();

  /**
   * A colour per live ordinal (§5.10), **extended for what the table gained rather than rebuilt
   * over what it holds**, and only when the table has moved at all — a response naming artifacts
   * already known computes nothing.
   *
   * The distinction the incremental route rests on is the palette's, not an optimisation's.
   * `positional` is a pure function of one artifact's centroid, so an ordinal's colour is
   * unaffected by every other ordinal and the table's change list is exactly the work to do; the
   * map is then **mutated in place and keeps its identity**, which is what lets the lookup
   * texture tell an extension from a recolour (`lut.ts`). `spread` assigns hues by rank around
   * the whole set's angle circle, so one arrival moves every colour: it rebuilds whole, at the
   * table's live count, on any settle that names an artifact.
   */
  function colourTable(): Map<number, Rgba> {
    const changes = palette === 'positional' && colouredUnder?.palette === 'positional' && colouredUnder.scheme === scheme ? table.changesSince(colouredAt) : null;
    colouredAt = table.version;
    colouredUnder = {palette, scheme};
    if (changes) {
      for (const {ordinal, kind} of changes) {
        if (kind === 'freed') colourMap.delete(ordinal);
        else colourMap.set(ordinal, positionalEntry(table.entry(ordinal)?.centroid ?? null, scheme));
      }
      return colourMap;
    }
    colourMap = artifactColours(
      table.liveEntries().map(({ordinal, entry}) => ({ordinal, centroid: entry.centroid})),
      palette,
      scheme
    );
    return colourMap;
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
    // **Nothing is asked for while a switch is settling.** `reschedule` re-enters the driver,
    // which fires its leading edge for a view arriving from stillness — so a view the slider is
    // passing through would ask here, outside the settle that exists to stop exactly that
    // (`view-switching.md` §4). Not merely deferred but *not decided*: recording these bands as
    // asked and then not asking would leave them stale until the served set moved. The frame the
    // settled request draws runs this check again, with nothing pending.
    const toAsk = switchTimer === null ? stale.filter((b) => colourAsked.get(bandKey(b.depth, b.prefix)) !== a.version) : [];
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
      const resolved = await client.categories(token, column, {codes: wanted, view: viewId});
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
    const q = frame();
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

  /**
   * Make `id` the view the store answers from (`view-switching.md` §3–§4).
   *
   * **A pointer change, never a rebuild.** The machinery of the view being left is cancelled — a
   * view that is not current has nothing in flight and no timer that could put something there
   * (§8) — the machinery of the view being entered is built on its first visit, brought up to date
   * from the shared fields, and bound; nothing re-subscribes and no component is told to.
   *
   * **The camera follows the frame, not the view.** Within a group every view quantises against
   * one extent, so the camera, the depth rule and the selection carry over and the same tiles are
   * asked for in the view arrived at. Across frames there is no camera to carry: the incoming view
   * is published with none and the selection goes, because a selection is a shape in one frame's
   * data coordinates. The map owns the camera, sees the frame change under `view.id`, refits and
   * issues the `setView` that asks for what the refitted camera covers.
   *
   * An id `meta.views` does not list is **ignored and reported**, never a throw and never a guess
   * at a neighbour: the pickers are built from `meta.views` and cannot produce one, a host's URL
   * state can, and a wrong view id discloses nothing.
   */
  function setCurrentView(id: string): void {
    if (!meta) {
      // Queued as `setView` is, and applied at warm-up in place of `options.view`.
      queuedCurrentView = id;
      return;
    }
    if (id === viewId) return;
    if (!meta.views.some((v) => v.id === id)) {
      onTrace('view-switch', {refused: 1, id});
      return;
    }

    const from = viewId;
    const kept = sameFrame(quantisationOf(from), quantisationOf(id));

    // Nothing of a view that is not current may be in flight, and nothing of it may be scheduled:
    // a debounce left armed would put a request on the wire for a view nobody is looking at.
    const outgoing = perView.get(from);
    outgoing?.presenter.cancel();
    outgoing?.channel.cancel();
    if (switchTimer !== null) {
      clock.cancel(switchTimer);
      switchTimer = null;
    }

    // **The incoming machinery is built before `viewId` moves, and bound in the same breath.**
    // Everything below this line publishes, and every publish reaches the map synchronously — and
    // the map decides its refit from `frame()`, which follows `viewId`. With `viewId` moved and
    // `presenter` still the outgoing view's, that refit's `setView` schedules the view being
    // *left*: the request goes out for a view nobody is looking at, the incoming view is never
    // asked for at all, and the switch sits at `loading` for ever. Found by the V2 smoke against
    // the multiview fixture, on the first switch across frames.
    //
    // Building first is safe because a view's machinery reports through `current()`, which is
    // `id === viewId` read at the time of the event: while it is being built it is not current and
    // publishes nothing.
    const incoming = machineryFor(id);
    viewId = id;
    bind(incoming);
    // Set here rather than below for the same reason: a `setView` arriving from a subscriber
    // during this call clears it, and from then on the request on the wire answers for the status.
    awaitingSwitchFrame = true;
    // Shared state reaches a view when it becomes current, rather than at every change: a change
    // pushed to a held view would have it *ask* (§3).
    incoming.channel.setLayers(layersOn);
    incoming.presenter.setBudget(budget);
    // The content key was the frame of another view's replica, and the colour refetch's record is
    // per band key with no view axis — both are about what was drawn, and what was drawn is gone.
    contentKeyAtFrame = '';
    colourAsked.clear();
    // A shape is generalised against the frame it was fetched under and names an artifact of one
    // view's row space (§8: the client does not match artifact identity across views).
    forgetShapes('all');

    if (!kept) {
      // No camera to carry, so no question to re-ask: `lastView` is the outgoing frame's bbox and
      // means nothing here. The map's refit is what supplies the next one.
      lastView = null;
      selection = null;
      regionVerdict = null;
      replaceProjection('region', null);
    }

    // The `view` projection, immediately, with the new id and no marks: view A's marks must never
    // be drawn under view B's frame, and the presenter of the view being entered was cancelled
    // when it was left, so it holds nothing to publish yet.
    replaceProjection('view', {id, composition: null, depth: 0, visible: NO_MASKED, matched: NO_MASKED, highlighted: NO_MASKED, highlighting: false, served: NO_COUNT, provisional: 0});
    replaceProjection('marks', {...projections.marks, bands: [], standIn: [], count: NO_COUNT});
    replaceProjection('tiles', {tiles: []});
    // The artifacts of the view being entered — its own channel's state, which is empty for a
    // cold view and its held served set for a warm one. Left alone, the projection would show the
    // outgoing view's artifacts under the incoming view's id until that channel next answered,
    // which on a cross-frame switch is not until the map has refitted: the list, the coverage
    // score and every `tesseraId` lookup would be against another view's row space (§8).
    onArtifacts(incoming.channel.current);
    replaceProjection('status', {...projections.status, status: 'loading', refusal: null, stale: false});
    publishReplica(projections.replica.lastPlan);

    if (kept && lastView) {
      const v = toDriverView(lastView.input);
      // What this view already holds for the camera, drawn without asking for anything — the
      // return to a warm view is the cache's whole point. It reaches the projections on the next
      // scheduler tick rather than inside this call, because deriving a frame is the presenter's
      // work and the presenter coalesces to one paint per tick; nothing waits on the network. A
      // cold view draws nothing, and the empty frame published above stands until the request
      // below answers.
      incoming.presenter.redraw({target: v.target, zoom: v.zoom}, v.width, v.height);
      // And the request, after the settle: a slider stepping through five views issues one
      // request, for the fifth (§4).
      switchTimer = clock.after(VIEW_SETTLE_MS, () => {
        switchTimer = null;
        if (lastView) setView(lastView.input);
      });
    }

    onTrace('view-switch', {from, to: id, sameFrame: kept ? 1 : 0});
  }

  function setView(input: ViewInput): void {
    lastView = {input};
    // A request answers for the status from here on: the driver transitions on its own, and the
    // partial frames it presents on the way are not the switch's cache-derived frame.
    awaitingSwitchFrame = false;
    if (!meta || !presenter) {
      // A setView before meta has arrived is queued (§4).
      queuedView = input;
      return;
    }
    const v = toDriverView(input);
    presenter.schedule({target: v.target, zoom: v.zoom}, v.width, v.height);
    channel?.schedule({target: v.target, zoom: v.zoom}, v.width, v.height);
  }

  /**
   * The filter every request carries: the draft's expression composed with the selection's
   * `region` leaf (`selection-operand.md` §5). One composition site, so the point path, the
   * artifact channel and the region's own count are answers to one question.
   */
  /**
   * The layers a viewport request may name: every one that is not a **filter layer**
   * (`highlight-and-hierarchy.md` §5.4, owner ruling 2026-09-02).
   *
   * A layer declaring `computed = []` is listed in the client's roster and never presented for
   * viewing, so naming it here would pay an artifact pass for rows nothing draws — and would put
   * its artifacts in the *In view* list and its names on the map, which is the whole of what the
   * ruling forbids. It is reached through the hierarchy panel and applied as a `member_of` clause.
   * Filtered here rather than in the picker, so a host driving `setLayers` directly cannot get it
   * wrong either.
   */
  function drawnOnly(names: readonly string[]): string[] {
    if (!meta) return [...names];
    const filterLayers = new Set(meta.layers.filter(isFilterLayer).map((l) => l.name));
    return names.filter((n) => !filterLayers.has(n));
  }

  function requestFilters(): FilterExpr | null {
    const expr = withMembers(composeFilters(projections.filters.draft, 'filter'), projections.filters.members, 'filter');
    return withRegion(expr, selection ? regionOperand(selection) : null, selection?.outside ?? false);
  }

  /**
   * The highlight every request carries: the controls and the `member_of` clauses in the
   * `highlight` position (`highlight-and-hierarchy.md` §5.2), composed the same way and at the
   * same one site.
   *
   * **The drawn region is never here.** A drawn region is a shape the viewer put on the map and
   * the region panel's own numbers are read off the filtered frame's `matched`; moving it would
   * make those numbers answer a different question with nothing on screen saying so. Every other
   * clause carries both verbs.
   */
  function requestHighlight(): FilterExpr | null {
    return withMembers(composeFilters(projections.filters.draft, 'highlight'), projections.filters.members, 'highlight');
  }

  /**
   * The question changed — a filter or the selection — so what is held answers a different one.
   * A filter narrows what is served without changing the identity key, so bands held under one
   * filter are renderable under another; the client that changed the question is the only party
   * that knows the held answers are to a different one (§4).
   */
  function requery(): void {
    presenter?.cancel();
    replica?.reset();
    // A filter and a selection are shared, so a held view's bands answer the question that has
    // just changed as surely as this view's do (`view-switching.md` §3). They are dropped now
    // rather than at the switch, so no view is ever drawn under a filter it was not fetched
    // under; a held view asks for nothing here, and refills when it is next current.
    for (const held of perView.values()) {
      if (held.id === viewId) continue;
      held.presenter.cancel();
      held.replica.reset();
    }
    contentKeyAtFrame = '';
    if (lastView) setView(lastView.input);
  }

  async function browse(req: Omit<BrowseRequest, 'filters' | 'view'> & {filters?: FilterExpr | null; view?: string}): Promise<BrowsePage> {
    const t = await ensureToken();
    // The map's own filter unless the caller named one — including `null`, which asks for the
    // unfiltered counts explicitly. The view is the store's own current one: a masked count is an
    // intersection in row space and row space is per view, so a panel that named none would be
    // asking about whichever view the server chose.
    const filters = 'filters' in req ? (req.filters ?? null) : requestFilters();
    return client.browse(t, {view: viewId, ...req, filters});
  }

  function setMembers(clauses: readonly MemberClause[]): void {
    const members = [...clauses];
    replaceProjection('filters', {...projections.filters, members});
    if (selection) projectRegionLoading(selection);
    requery();
  }

  function setFilters(draft: FilterDraft): void {
    const expr = composeFilters(draft, 'filter');
    replaceProjection('filters', {...projections.filters, draft, expr, highlight: composeFilters(draft, 'highlight')});
    // A region's `matched` is under the filters, so its numbers are to a question just changed.
    if (selection) projectRegionLoading(selection);
    requery();
  }

  /**
   * Columns whose values are being enumerated. **One enumeration per column, however many times
   * it is asked for**: a control asks on every store change until the values land, and a large
   * `derived` vocabulary lands only after every page has been walked — on GeoNames' 231,645-value
   * `admin4`, 232 pages. Without this guard each store change in that window started another
   * walk, every completed page was a store change, and the demo issued 21,500 category requests
   * in its first minute (2026-08-28) — the load never settled.
   */
  const enumerating = new Set<string>();

  async function loadFilterValues(column: string): Promise<void> {
    if (!token || disposed) return;
    if (projections.filters.values[column] || projections.filters.valueErrors[column]) return;
    if (enumerating.has(column)) return;
    enumerating.add(column);
    try {
      const values = await client.categories(token, column, {view: viewId});
      if (disposed) return;
      values.sort((a, b) => a.key.localeCompare(b.key));
      replaceProjection('filters', {...projections.filters, values: {...projections.filters.values, [column]: values}});
    } catch (error) {
      if (disposed) return;
      const e = error as {code?: string; detail?: string; message?: string};
      replaceProjection('filters', {
        ...projections.filters,
        valueErrors: {...projections.filters.valueErrors, [column]: {code: e.code ?? 'fetch-failed', detail: e.detail ?? e.message ?? String(error)}}
      });
    } finally {
      enumerating.delete(column);
    }
  }

  function setLayers(names: string[]): void {
    // Usually one, with its closure (decision 0096) — the request names every layer in it.
    layersOn = meta ? drawnOnly(layerClosure(meta.layers, names)) : names;
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

  /**
   * {@link Store.describe} — the hover's record, asked for once per id and held.
   *
   * A refusal is held as `null` for the same reason the shape cache holds one: the pointer will
   * cross this mark again within the second, and an id that answered 404 once will answer 404
   * every time until the principal changes.
   */
  const described = new Map<string, Record<string, unknown> | null>();
  const describing = new Map<string, Promise<Record<string, unknown> | null>>();

  async function describe(id: bigint): Promise<Record<string, unknown> | null> {
    const key = id.toString();
    const held = described.get(key);
    if (held !== undefined) return held;
    const inFlight = describing.get(key);
    if (inFlight) return inFlight;
    if (!token) return null;
    const request = client
      .item(token, id)
      .then((detail) => detail.fields)
      .catch(() => null)
      .then((fields) => {
        describing.delete(key);
        if (!disposed) described.set(key, fields);
        return fields;
      });
    describing.set(key, request);
    return request;
  }

  // ---- the drawn shape, fetched by identifier (`artifact-shapes.md` §9) -----------------------

  /**
   * The shapes held, with the kind each was served as, and the identifiers already asked for.
   *
   * **Two maps, because an absence has two meanings.** A shape not in `heldShapes` is either not
   * asked for or asked for and not answered, and the second must not be asked again on every
   * pointer move; `askedShapes` is what separates them. A refusal stays in `askedShapes` and out
   * of `heldShapes`, so a `404` is asked once and drawn as a box — which is what the map does for
   * an artifact whose layer draws no shape, and the two are indistinguishable on purpose.
   *
   * **The kind decides what survives a change of principal** (`polygon-membership.md` §7.1): a
   * `derived` shape is this principal's and goes with them; a `predicate` or an `authored` one is
   * identical for every principal served the artifact and is kept. An artifact the new
   * principal is not served is simply never looked up.
   */
  let heldShapes = new Map<bigint, Shape>();
  let heldKinds = new Map<bigint, ShapeKind>();
  let askedShapes = new Set<bigint>();

  /** Drop what the principal's change invalidates — every derived shape, or everything. */
  function forgetShapes(which: 'derived' | 'all'): void {
    if (heldShapes.size === 0 && askedShapes.size === 0) return;
    if (which === 'all') {
      heldShapes = new Map();
      heldKinds = new Map();
      askedShapes = new Set();
    } else {
      const keep = new Map<bigint, Shape>();
      const kinds = new Map<bigint, ShapeKind>();
      for (const [id, shape] of heldShapes) {
        const kind = heldKinds.get(id);
        if (kind !== undefined && kind !== 'derived') {
          keep.set(id, shape);
          kinds.set(id, kind);
        }
      }
      heldShapes = keep;
      heldKinds = kinds;
      askedShapes = new Set(keep.keys());
    }
    replaceProjection('artifacts', {...projections.artifacts, shapes: heldShapes});
  }

  /** The kind a served artifact's layer draws, from the meta; null where it draws none. */
  function shapeKindOf(id: bigint): ShapeKind | null {
    const layer = projections.artifacts.served.find((a) => a.tesseraId === id)?.layer;
    if (layer === undefined) return null;
    return projections.meta?.layers.find((l) => l.name === layer)?.shape ?? null;
  }

  function needShape(id: bigint): void {
    if (askedShapes.has(id)) return;
    askedShapes.add(id);
    void (async () => {
      const t = token;
      if (!t) return;
      try {
        // The view's own zoom, so the shape is generalised to this screen's pixel.
        const zoom = presenter?.view?.view.zoom;
        const detail = await client.artifact(t, id, {view: viewId, ...(zoom === undefined ? {} : {zoom})});
        // The principal may have changed under the request — a renewal, a cleared store — in which
        // case this answer describes a mask that is no longer the one being drawn.
        if (!askedShapes.has(id)) return;
        if (!detail.shape) return;
        heldShapes = new Map(heldShapes).set(id, detail.shape);
        heldKinds = new Map(heldKinds).set(id, shapeKindOf(id) ?? 'derived');
        replaceProjection('artifacts', {...projections.artifacts, shapes: heldShapes});
      } catch {
        // A refused shape is a shape not drawn, and the `box` already in hand answers instead.
        // There is nothing here to report: `404` covers an unknown identifier, one this principal
        // may not reach and one below its layer's criterion identically, so a message would be
        // inventing a distinction the wire does not carry.
      }
    })();
  }

  /**
   * Take the shape an answer already carried into the held shapes, so the map draws it.
   *
   * The drill-down route answers `centroid`, `box` **and** the shape in one body, and
   * {@link openArtifact} was reading the first two and discarding the third: the shape landed in
   * `selection.artifact.detail` — where the card reads it and draws nothing with it — and never
   * in `artifacts.shapes`, which is what `focusOutlines` looks in. So an artifact opened from
   * `<tessera-artifact-list>` drew no outline at all on a layer that declares a hull, because the
   * box the served row carries is refused for a layer whose declared shape is a hull; a layer
   * declaring none drew its box and hid the fault. The map's own click and hover call
   * {@link needShape} beside this, which is why a pick drew and a list row did not.
   *
   * Recorded as asked, so `needShape` never issues a second request for a shape already in hand.
   */
  function holdShape(id: bigint, shape: Shape | null): void {
    if (!shape) return;
    askedShapes.add(id);
    heldShapes = new Map(heldShapes).set(id, shape);
    heldKinds = new Map(heldKinds).set(id, shapeKindOf(id) ?? 'derived');
    replaceProjection('artifacts', {...projections.artifacts, shapes: heldShapes});
  }

  async function openArtifact(id: bigint): Promise<void> {
    if (!token) return;
    const asked = token;
    try {
      const detail = await client.artifact(token, id, {view: viewId});
      // The principal may have changed under the request, in which case this shape describes a
      // mask that is no longer the one being drawn — the same guard `needShape` takes.
      if (token === asked) holdShape(id, detail.shape);
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

  /** A shape in world space: the box, the lasso's polygon, or nothing to highlight by (an artifact). */
  type WorldShape = {kind: 'box'; box: [number, number, number, number]} | {kind: 'lasso'; polygon: WorldPolygon} | {kind: 'artifact'};

  /** When the selection was made, so the answer can be timed (the instruments' `region` trace). */
  let selectedAt = 0;
  /** The last verdict the wire gave for this selection — read off the replica, which observes every response. */
  let regionVerdict: RegionVerdict | null = null;

  /**
   * The held marks whose world positions fall inside the shape — the region's sample (P1), by
   * the server's own predicate over the quantised grid (`region.ts`). An artifact selection has
   * no client-side predicate: the wire's `membership:<layer>` column is the membership, and the
   * sample is every held mark, which the request already narrowed to the artifact's members.
   */
  function heldInside(world: WorldShape, outside: boolean): RegionProjection['held'] {
    const ids: bigint[] = [];
    const xy: number[] = [];
    let count = 0;
    const inside =
      world.kind === 'box' ? (x: number, y: number) => insideBox(x, y, world.box) : world.kind === 'lasso' ? (x: number, y: number) => insidePolygon(x, y, world.polygon) : () => true;
    const take = (band: {ids: BigUint64Array; positions: Float32Array}, i: number) => {
      const x = band.positions[i * 2]!;
      const y = band.positions[i * 2 + 1]!;
      if (world.kind !== 'artifact' && inside(x, y) === outside) return;
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
    const q = frame();
    if (shape.kind === 'artifact') return {kind: 'artifact'};
    if (shape.kind === 'box') {
      const [x0, y0] = dataToWorldXY(shape.bbox[0], shape.bbox[1], q);
      const [x1, y1] = dataToWorldXY(shape.bbox[2], shape.bbox[3], q);
      return {kind: 'box', box: [Math.min(x0, x1), Math.min(y0, y1), Math.max(x0, x1), Math.max(y0, y1)]};
    }
    if (shape.points.length < 3) return null;
    return {kind: 'lasso', polygon: shape.points.map(([x, y]) => dataToWorldXY(x, y, q))};
  }

  /**
   * The world box the selection's count is over, or `null` where the client cannot know it — an
   * artifact whose extent the store does not hold. A frame whose exact tiles cover it has counted
   * the whole shape; one that does not has counted the part in view.
   */
  function worldExtentOf(shape: SelectionShape): [number, number, number, number] | null {
    const world = worldOfShape(shape);
    if (!world) return null;
    if (world.kind === 'box') return world.box;
    if (world.kind === 'lasso') {
      let x0 = Infinity;
      let y0 = Infinity;
      let x1 = -Infinity;
      let y1 = -Infinity;
      for (const [x, y] of world.polygon) {
        if (x < x0) x0 = x;
        if (y < y0) y0 = y;
        if (x > x1) x1 = x;
        if (y > y1) y1 = y;
      }
      return [x0, y0, x1, y1];
    }
    const extent = shape.kind === 'artifact' ? extentOf(shape.id) : null;
    if (!extent || !meta) return null;
    const q = frame();
    const [x0, y0] = dataToWorldXY(extent[0], extent[1], q);
    const [x1, y1] = dataToWorldXY(extent[2], extent[3], q);
    return [Math.min(x0, x1), Math.min(y0, y1), Math.max(x0, x1), Math.max(y0, y1)];
  }

  /** The region as it stands before a frame has answered for it: its held sample, no numbers. */
  function projectRegionLoading(shape: SelectionShape): void {
    const world = worldOfShape(shape);
    if (!world) {
      replaceProjection('region', null);
      return;
    }
    const held = heldInside(world, shape.outside ?? false);
    replaceProjection('region', {
      shape,
      status: 'loading',
      refusal: null,
      visible: null,
      matched: NO_MASKED,
      served: {shown: held.count, total: 0, exact: false},
      verdict: regionVerdict,
      held
    });
  }

  /**
   * The region's numbers, read off a presented frame. `matched` is the frame's own sum — the
   * request carried the leaf, so a tile's `matched` is the items inside the shape the filters
   * admit. Exact for the shape when the wire said so *and* the frame's exact tiles cover the
   * shape's extent; an outside selection is never covered by one frame, since its complement is
   * the whole map.
   */
  function projectRegion(frame: Composition, verdict: RegionVerdict | null, derived: boolean, matched: number): void {
    const shape = selection;
    const current = projections.region;
    if (!shape || !current || current.shape !== shape) return;
    if (verdict) regionVerdict = verdict;
    // A frame folded from the replica, before any derive answered for this selection, is to the
    // previous question; the numbers wait for the derive.
    if (current.status === 'loading' && !derived) return;
    const world = worldOfShape(shape);
    if (!world) return;
    const outside = shape.outside ?? false;
    const extent = outside ? null : worldExtentOf(shape);
    // Covered means the replica holds every tile of the shape's extent at the frame's depth —
    // asked of the replica rather than read off the frame's tile list, which names only the
    // tiles that drew a point; a tile the region emptied is held, counted and listed nowhere.
    const covered = extent !== null && replica !== null && meta !== null && replica.novelIn(tileRectOfBbox(extent, frame.depth), frame.depth, meta.selection.kMaxMarks) === 0;
    const exact = (regionVerdict?.exact ?? false) && covered;
    const held = heldInside(world, outside);
    const draftEmpty = composeFilters(projections.filters.draft) === null;
    const wasLoading = current.status === 'loading';
    replaceProjection('region', {
      ...current,
      status: 'shown',
      refusal: null,
      matched: {value: matched, exact},
      // The region alone is the same question only while no other filter narrows the frame.
      visible: draftEmpty ? {value: matched, exact} : null,
      served: {shown: held.count, total: matched, exact: true},
      verdict: regionVerdict,
      held
    });
    if (wasLoading) options.instruments?.onTrace?.('region', {answerMs: clock.now() - selectedAt, exact: exact ? 1 : 0, depth: regionVerdict?.depth ?? -1});
  }

  /**
   * Select a shape — and, with it, filter to it: the leaf joins every request from here on, and
   * the region's numbers are read off the next frame. `null` clears both.
   */
  function select(shape: SelectionShape | null): void {
    const changed = shape !== selection;
    selection = shape;
    regionVerdict = null;
    selectedAt = clock.now();
    if (!shape || (shape.kind === 'lasso' && shape.points.length < 3)) {
      selection = null;
      replaceProjection('region', null);
      if (changed) requery();
      return;
    }
    projectRegionLoading(shape);
    requery();
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
    const q = frame();
    return [
      q.xMin + (worldX / WORLD_SIZE) * (q.xMax - q.xMin),
      q.yMin + (worldY / WORLD_SIZE) * (q.yMax - q.yMin)
    ];
  }

  function clear(): void {
    // **Every held view, not the current one alone**: a mask change invalidates all of them
    // (`view-switching.md` §4), and a view left warm across it would draw the previous
    // principal's marks the moment it was returned to.
    for (const held of perView.values()) {
      held.presenter.cancel();
      held.channel.reset();
      held.replica.reset();
    }
    if (switchTimer !== null) {
      clock.cancel(switchTimer);
      switchTimer = null;
    }
    table.clear();
    forgetShapes('all');
    described.clear();
    contentKeyAtFrame = '';
    replaceProjection('view', {id: viewId, composition: null, depth: 0, visible: NO_MASKED, matched: NO_MASKED, highlighted: NO_MASKED, highlighting: false, served: NO_COUNT, provisional: 0});
    replaceProjection('marks', {...projections.marks, bands: [], count: NO_COUNT});
    replaceProjection('legend', {ranks: {}, domains: {}, categories: {}, categoryErrors: {}, colourBy});
    replaceProjection('status', {...NO_STATUS});
    selection = null;
    regionVerdict = null;
    replaceProjection('region', null);
    publishReplica(null);
  }

  function refresh(): void {
    // Redraw the marks against the refreshed content key — a held layer set goes with it (§4).
    channel?.reset();
    if (selection) projectRegionLoading(selection);
    if (lastView) setView(lastView.input);
  }

  /** Set by `dispose`: a fetch that lands afterwards writes nothing into a store nobody reads. */
  let disposed = false;

  function dispose(): void {
    disposed = true;
    for (const held of perView.values()) {
      held.presenter.cancel();
      held.channel.cancel();
    }
    if (switchTimer !== null) clock.cancel(switchTimer);
    switchTimer = null;
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
    browse,
    setFilters,
    setMembers,
    loadFilterValues,
    setLayers,
    setColourBy,
    setPalette,
    setBudget,
    setCurrentView,
    frame: frameOrNull,
    pick,
    describe,
    openArtifact,
    needShape,
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
