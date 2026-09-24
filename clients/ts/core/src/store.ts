import {ArtifactChannel, servedLineage, type ArtifactChannelState, type ServedLineage} from './artifactChannel.js';
import {SessionArtifactTable} from './artifactTable.js';
import type {Composition} from './compose.js';
import {NO_COUNT, NO_MASKED, type Count, type Masked} from './counts.js';
import {dataToWorldXY, gridToWorld, MAX_DEPTH, WORLD_SIZE} from './coords.js';
import {tileRectOfBbox} from './budget.js';
import {worldBbox} from './prefetch.js';
import type {Clock, DriverOptions, ViewState as DriverViewState} from './driver.js';
import {countCodesCached, countCodesInPiece, extendRanks, widenDomain, widenDomainOver, type Domain, type Ranks} from './encoding.js';
import {composeFilters, emptyDraft, type ClauseVerb, type FilterDraft} from './filters.js';
import {withMember, withMembers, withoutMember, type MemberClause} from './members.js';
import {Presenter, defaultFrameScheduler, refusalOf, type FrameScheduler, type PresentedStatus, type Refusal} from './presented.js';
import {regionOperand, withRegion} from './region.js';
import {colourLayers, isFilterLayer, layerClosure} from './layers.js';
import {requestLevels} from './artifactChannel.js';
import {artifactBudgetFor} from './artifactBudget.js';
import type {PaletteKind, PaletteScheme, Rgba} from './palette.js';
import {BandBudget} from './bands.js';
import {DEFAULT_CACHE_BYTES, Replica, type ReplicaOptions} from './replica.js';
import {TesseraClient, type TesseraClientOptions} from './client.js';
import {HeldRecords, HeldShapes} from './held.js';
import {SelectedRegion, type RegionProjection, type SelectionShape} from './selectedRegion.js';
import {Suggestions} from './suggestions.js';
import {CLUSTER_PREFIX, Legend, type LegendProjection} from './legend.js';
import {HeldViews, type ViewMachinery} from './heldViews.js';
import {ArtifactColours, ColourCoverage} from './colours.js';
import {TokenSupply, type TokenSupplier} from './token.js';
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
  Shape,
  ShapeKind,
  SuggestValue,
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
 * **The membership column** (D12, §5.10): the point path names the layers drawn with their
 * closure, and the layer the points are coloured by, so each band arrives with its ordinals named
 * through the session table; the `artifacts` projection carries the table, the served set's
 * ordinals and each one's colour, and the colour coverage over the bands in view — a band whose
 * ordinals no longer resolve to anything served, or that lacks a column for a layer now asked
 * for, is colour-stale and is refetched after novel ground by the replica's own path.
 */

export type {Count, Masked} from './counts.js';
export {formatCount, formatMasked} from './counts.js';

/** A data-coordinates bbox and the pixel size it is drawn at — what `setView` takes (§4). */
export type ViewInput = {bbox: [number, number, number, number]; width: number; height: number};

export type {TokenSupplier} from './token.js';
export {CLUSTER_PREFIX, type LegendProjection} from './legend.js';
export {REGION_HELD_LIMIT, type RegionProjection, type SelectionShape} from './selectedRegion.js';

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
  /**
   * A `/v1/meta` already in hand, fetched under the token {@link authorise} will return.
   *
   * The host that has to read meta *before* opening a store — to choose the layer and the colour
   * the store opens pointed at, which is what the demo does — otherwise pays for the document
   * twice, and pays for the session twice with it. Handing the one it read is the whole saving:
   * the store's bring-up is unchanged in every other respect.
   *
   * **The caller's obligation is that it is this principal's meta.** The roster a principal
   * reaches is theirs, so a document fetched under another token would open the store on layers
   * and views this one may not have been served. Absent, the store fetches its own.
   */
  meta?: Meta;
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
  /** The first layer drawn. */
  layer: string | null;
  /** Every layer drawn, with its closure (decision 0096). The colour layer is here only when it is drawn too. */
  layers: string[];
  /** The served artifacts of the drawn layers — what the map, the lists and the labels draw. */
  served: Artifact[];
  /**
   * The served artifacts of the layer the points are coloured by (`colourBy = "cluster:<layer>"`),
   * drawn or not; empty under a column colouring. The legend's swatches and levels read these.
   */
  colourServed: Artifact[];
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
  /** The drawn served set's ordinals — what an opened or hovered artifact resolves through. */
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
  /**
   * The typeahead's last landed page per column (`value-suggestion.md` §5.1) — `q` is the query
   * it answers, so a control can tell a page that answers what is in the box from one that
   * answers what used to be. `suggest`'s only source: the enumeration's page-the-whole-set path
   * (`loadFilterValues`) is gone — the legend resolves codes it drew through `resolveCategoryCodes`
   * instead, and a category's picker is this typeahead everywhere else.
   */
  suggestions: Record<string, {q: string; values: SuggestValue[]; more: boolean}>;
  suggestErrors: Record<string, Refusal>;
  /**
   * Bumped every time {@link resetSuggestions} invalidates every column's held page (a view
   * switch, a re-authorise) — a control's own bookkeeping (`filter.ts`'s `lastEpoch`) compares
   * against this to notice an invalidation even where it never receives one through `suggestions`
   * or `suggestErrors` directly (a column stuck on a refusal, or still in flight when the reset
   * lands). Not meaningful on its own; only the fact that it moved matters.
   */
  suggestEpoch: number;
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
  /**
   * The expression every request carries as `filters`, composed: the draft's filter-position
   * leaves, the `member_of` clauses in that position, and the drawn region's leaf. Null for the
   * unfiltered request.
   *
   * **Read it rather than recomposing it.** A caller that needs to know *what question is being
   * asked* — a panel deciding whether its counts are stale, a host labelling a number — has three
   * projections to look in and no way to be told about a fourth. `filters.expr` is one of the
   * three, and a reader that took it for the whole would miss a drawn region entirely.
   *
   * It is JSON-safe: a `member_of` leaf carries its artifact as a decimal string and a region
   * leaf carries numbers, so this composes, hashes and logs without a replacer.
   */
  requestFilters(): FilterExpr | null;
  /**
   * Ask a category's typeahead for `q`, debounced per column (`value-suggestion.md` §5.1) — lands
   * in `filters.suggestions[column]`, or `filters.suggestErrors[column]` on a refusal. Fire and
   * forget: a control calls it on every keystroke it actually changes and reads the projection.
   */
  suggest(column: string, q: string): void;
  /** Draw layers — each with its closure (decision 0096); `[]` draws none. */
  setLayers(names: string[]): void;
  /**
   * Colour by a declared column, by `cluster:<layer>` for any layer {@link colourLayers} lists
   * (a lookup-texture switch on the vis side, never a per-point pass), or `null` for uniform.
   * Colouring by a layer does not draw it. A layer `meta` does not list as one that can colour
   * is named in no request and traced as `colour-by`; the points then draw uniform.
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
  /**
   * Forget everything answered under the current principal, for a re-authorise: the held views,
   * marks, shapes, legend, suggestions, region and the selected item and artifact.
   */
  clear(): void;
  refresh(): void;
  dispose(): void;
}

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

  const colours = new ArtifactColours(table, options.palette ?? 'positional', (map, palette) =>
    replaceProjection('artifacts', {...projections.artifacts, colours: map, palette})
  );
  const coverage = new ColourCoverage();

  const legend = new Legend(
    async (column, codes) => client.categories(await tokens.get(), column, {codes, view: viewId}),
    (value) => replaceProjection('legend', value)
  );

  const suggestions = new Suggestions(
    clock,
    async (column, q) => {
      const asked = await viewed();
      return client.suggest(asked.token, column, q, {view: asked.view});
    },
    (state) => replaceProjection('filters', {...projections.filters, ...state})
  );

  const shapes = new HeldShapes(
    async (id) => {
      const asked = await viewed();
      // Generalised to the pixel of the view's own zoom.
      const zoom = views.current?.presenter.view?.view.zoom;
      const detail = await client.artifact(asked.token, id, {view: asked.view, ...(zoom === undefined ? {} : {zoom})});
      return detail.shape;
    },
    shapeKindOf,
    (held) => replaceProjection('artifacts', {...projections.artifacts, shapes: held})
  );

  const region = new SelectedRegion({
    clock,
    frame: frameOrNull,
    extentOf,
    covered: (box, depth) => {
      const replica = views.current?.replica;
      return replica !== undefined && meta !== null && replica.novelIn(tileRectOfBbox(box, depth), depth, meta.selection.kMaxMarks) === 0;
    },
    publish: (value) => replaceProjection('region', value),
    trace: (kind, fields) => options.instruments?.onTrace?.(kind, fields)
  });

  const records = new HeldRecords((id) => tokens.get().then((t) => client.item(t, id)).then((detail) => detail.fields));

  const tokens = new TokenSupply(options.authorise, options.token, clock, (changed) => {
    // A derived shape and a hovered record answer one principal; a new token may be another.
    if (changed) {
      shapes.forget('derived');
      records.forget();
    }
    // A warm-up that failed for want of a token runs again now there is one.
    if (meta === null) void ready().catch(() => {});
  });

  let meta: Meta | null = null;
  let viewId = options.view ?? '';
  let budget = options.budget ?? 500_000;
  let contentKeyAtFrame = '';

  /** One byte budget across every view's bands, evicted least recently drawn across them. */
  const bandBudget = new BandBudget(options.replica?.cacheBytes ?? DEFAULT_CACHE_BYTES);
  const views = new HeldViews(clock, buildView);

  /** The layers drawn, as `setLayers` last named them — held here so a call before meta survives to the channel. */
  let layersOn: string[] = [];
  let lastView: {input: ViewInput} | null = null;
  let queuedView: ViewInput | null = null; // a setView before meta arrives
  /** A `setCurrentView` before meta arrives — applied at warm-up in place of `options.view` (§3). */
  let queuedCurrentView: string | null = null;
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
    artifacts: {layer: null, layers: [], served: [], colourServed: [], lineage: servedLineage([]), status: 'idle', refusal: null, version: 0, held: 0, table, servedOrdinals: new Set(), shapes: new Map(), colours: new Map(), palette: colours.palette, coverage: {current: 0, stale: 0}},
    selection: {item: null, itemRefusal: null, artifact: null, artifactRefusal: null},
    region: null,
    filters: {draft: {}, expr: null, highlight: null, members: [], suggestions: {}, suggestErrors: {}, suggestEpoch: 0},
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

  /** The token and the view a verb naming a view asks under, once the store has read its meta. */
  async function viewed(): Promise<{token: string; view: string}> {
    await ready();
    return {token: await tokens.get(), view: viewId};
  }

  /**
   * The store's first `/v1/meta`, read once and shared. A warm-up that failed is forgotten, so the
   * next call runs it again.
   */
  let warming: Promise<void> | null = null;

  function ready(): Promise<void> {
    warming ??= warm().catch((error: unknown) => {
      warming = null;
      if (!disposed) {
        const refusal = refusalOf(error);
        replaceProjection('status', {...projections.status, status: 'refused', refusal, expired: tokens.isExpiry(refusal)});
      }
      throw error;
    });
    return warming;
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
        const tok = await tokens.use();
        // The point path names the layers asked for and pays their pass (§5.10): that is what puts
        // the membership column on each band. `[]` until a layer is drawn or coloured by — and
        // `[]` on the replica's counts-only revalidation, which absorbs no points and would pay the
        // artifact pass for a frame nobody reads.
        // The point path carries the same budget as the channel, so a point's membership column
        // names the deepest artifact of the *same* cut the panels show.
        const zoom = ownPresenter?.view?.view.zoom ?? 0;
        const layers = req.k === 0 ? [] : layersAsked();
        return client.viewport(
          tok,
          {
            ...req,
            view: id,
            filters: requestFilters(),
            // Beside it and never instead of it: the draw is unchanged by a highlight, so this
            // costs the response three columns and nothing else (`highlight-and-hierarchy.md` §2).
            highlight: requestHighlight(),
            layers,
            ...(layers.length === 0 ? {} : {artifactBudget: artifactBudgetFor(zoom)}),
            // The levels from the camera zoom, so the membership column names the same cut the
            // channel asks for — and not the deepest level alone, which is what the server's
            // depth-keyed default answers a budget-deepened request with (`requestLevels`).
            ...(layers.length === 0 || requestLevels(m.layers, layers, zoom) === undefined ? {} : {levels: requestLevels(m.layers, layers, zoom)})
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
      token: () => tokens.get(),
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
    viewChannel.setLayers(layersAsked());

    return {id, replica: built, presenter: ownPresenter, channel: viewChannel};
  }

  async function warm(): Promise<void> {
    const t = await tokens.use();
    // The host may have read this document already (see {@link StoreOptions.meta}); a second
    // fetch of it is a round trip for something in hand.
    const read = options.meta ?? (await client.meta(t));
    if (disposed) return;
    meta = read;
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
    traceUnknownColourLayer();
    views.enter(viewId);

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
    const expired = tokens.isExpiry(refusal);
    replaceProjection('status', {
      status,
      sessionWarm: projections.status.sessionWarm || status === 'shown',
      refusal,
      stale: projections.status.stale,
      expired,
      retrying: status === 'retrying'
    });
    if (status === 'refused' && refusal) region.refuse(refusal);
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
    const observed = views.current?.replica.currentContentKey ?? '';
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
    const current = views.current;
    const observed = current?.replica.currentContentKey;
    if (observed) current?.channel.observeContentKey(observed);
  }

  function onPresented(p: Presented): void {
    const current = views.current;
    const replica = current?.replica;
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
    if (current && !current.channel.hasView && lastView && current.presenter.view) {
      const v = current.presenter.view;
      current.channel.schedule({target: v.view.target, zoom: v.view.zoom}, v.width, v.height);
    }
    replaceProjection('tiles', {tiles: frame.tiles});
    region.answer({
      marks: {bands: frame.exact, standIn: frame.standIn},
      depth: frame.depth,
      verdict: replica?.lastRegionVerdict ?? null,
      fetched: p.fetched !== null,
      matched: Number(matched),
      narrowed: filtersBesideRegion() !== null
    });
    legend.accumulate(frame, meta?.declaredScalars ?? []);
    colours.refresh();
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
    const replica = views.current?.replica;
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
    // The channel is asked for the colour layer as well as the drawn ones. Its rows are in the
    // table, so the colours cover them, and they reach `colourServed`; only the drawn layers'
    // rows reach `served`, which is what every drawn surface reads.
    const drawn = new Set(layersOn);
    const served = state.artifacts.every((a) => drawn.has(a.layer)) ? state.artifacts : state.artifacts.filter((a) => drawn.has(a.layer));
    const coloured = colourLayer();
    const servedOrdinals = new Set<number>();
    for (const a of served) {
      const ordinal = table.ordinalOf(a.layer, a.tesseraId);
      if (ordinal !== 0) servedOrdinals.add(ordinal);
    }
    replaceProjection('artifacts', {
      ...projections.artifacts,
      layer: layersOn[0] ?? null,
      layers: layersOn,
      served,
      colourServed: coloured === null ? [] : state.artifacts.filter((a) => a.layer === coloured),
      lineage: servedLineage(served),
      status: state.status,
      refusal: state.refusal,
      version: state.version,
      held: state.held,
      table,
      servedOrdinals,
      shapes: shapes.shapes,
      colours: colours.current(),
      palette: colours.palette
    });
    checkColourCoverage();
  }

  /**
   * Publish the colour coverage of the bands in view, and fetch the colour-stale ones again. In
   * view is the visible box: the point path also fetches a margin, whose bands name artifacts the
   * channel did not serve for this view.
   */
  function checkColourCoverage(): void {
    const a = projections.artifacts;
    const layers = layersAsked();
    const machinery = views.current;
    if (!machinery || a.status !== 'shown' || layers.length === 0) {
      if (a.coverage.stale !== 0 || a.coverage.current !== 0) replaceProjection('artifacts', {...a, coverage: {current: 0, stale: 0}});
      return;
    }
    const started = clock.now();
    const v = machinery.presenter.view;
    const depth = projections.view.depth;
    const visible = v ? tileRectOfBbox(worldBbox({target: [v.view.target[0], v.view.target[1]], zoom: v.view.zoom, width: v.width, height: v.height}, 1), depth) : null;
    const bands = projections.marks.bands;
    // While a switch settles nothing is asked for: asking reschedules the driver, which would
    // request a view the slider is passing through.
    const {current, stale, toAsk} = coverage.check({bands, layers, table, colours: a.colours, version: a.version, visible, depth, mayAsk: !views.settling});
    if (toAsk.length > 0) {
      machinery.replica.retract(toAsk);
      machinery.presenter.reschedule();
    }
    options.instruments?.onTrace?.('coverage', {ms: clock.now() - started, bands: bands.length, stale, asked: toAsk.length});
    if (a.coverage.current !== current || a.coverage.stale !== stale) {
      replaceProjection('artifacts', {...projections.artifacts, coverage: {current, stale}});
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
   * Make `id` the view the store answers from. Between views that share a frame the camera and the
   * selection carry over: the incoming view draws what it holds, and asks after the settle. Across
   * frames both are dropped, and the map's refit supplies the next `setView`. An id `meta.views`
   * does not list is ignored and traced.
   */
  function setCurrentView(id: string): void {
    if (!meta) {
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
    // A suggestion page answers one view.
    suggestions.reset();

    // Bound before anything below publishes, so a subscriber's `setView` reaches the incoming view.
    const incoming = views.enter(id);
    viewId = id;
    // A `setView` from a subscriber clears this, and its request then answers for the status.
    awaitingSwitchFrame = true;
    // The shared settings reach a view as it becomes current. Set on a held view, they would make it ask.
    incoming.channel.setLayers(layersAsked());
    incoming.presenter.setBudget(budget);
    // The content key, the bands asked for again and the shapes were the outgoing view's.
    contentKeyAtFrame = '';
    coverage.forget();
    shapes.forget('all');

    if (!kept) {
      // The camera and the selection are in the outgoing frame's data coordinates.
      lastView = null;
      region.drop();
    }

    // No marks until the incoming view presents, and the incoming channel's artifacts.
    replaceProjection('view', {id, composition: null, depth: 0, visible: NO_MASKED, matched: NO_MASKED, highlighted: NO_MASKED, highlighting: false, served: NO_COUNT, provisional: 0});
    replaceProjection('marks', {...projections.marks, bands: [], standIn: [], count: NO_COUNT});
    replaceProjection('tiles', {tiles: []});
    onArtifacts(incoming.channel.current);
    replaceProjection('status', {...projections.status, status: 'loading', refusal: null, stale: false});
    publishReplica(projections.replica.lastPlan);

    if (kept && lastView) {
      const v = toDriverView(lastView.input);
      // Draws what the view holds on the next scheduler tick, without a request.
      incoming.presenter.redraw({target: v.target, zoom: v.zoom}, v.width, v.height);
      views.settle(() => {
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
    const current = views.current;
    if (!meta || !current) {
      queuedView = input;
      return;
    }
    const v = toDriverView(input);
    current.presenter.schedule({target: v.target, zoom: v.zoom}, v.width, v.height);
    current.channel.schedule({target: v.target, zoom: v.zoom}, v.width, v.height);
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

  /** The layer `colourBy` colours by, where `meta` lists it as one that can colour; else null. */
  function colourLayer(): string | null {
    const colourBy = legend.colourBy;
    if (!meta || !colourBy?.startsWith(CLUSTER_PREFIX)) return null;
    const name = colourBy.slice(CLUSTER_PREFIX.length);
    return colourLayers(meta.layers).some((l) => l.name === name) ? name : null;
  }

  function traceUnknownColourLayer(): void {
    const colourBy = legend.colourBy;
    if (meta && colourBy?.startsWith(CLUSTER_PREFIX) && colourLayer() === null) onTrace('colour-by', {refused: 1, layer: colourBy.slice(CLUSTER_PREFIX.length)});
  }

  /**
   * The layers a viewport request names: the drawn layers with their closure, and the colour
   * layer alone. The colour layer's dependents are not asked for, since nothing of it is drawn.
   */
  function layersAsked(): string[] {
    const coloured = colourLayer();
    return coloured === null || layersOn.includes(coloured) ? layersOn : [...layersOn, coloured];
  }

  /**
   * Point the channel at the layers asked for, and publish the artifacts under the drawn layers and
   * the colour layer as they now stand. A layer newly asked for is fetched at once: the channel
   * asks for its artifacts, and every band in view that lacks its membership column is
   * colour-stale and refetched centre-first as the coverage check finds it (§5.10). A layer no
   * longer asked for costs nothing: held bands keep its column and the rows are filtered out here.
   */
  function askLayers(): void {
    const current = views.current;
    if (!current) {
      // Before meta: record the intent where a reader sees it; the channel adopts it at meta.
      replaceProjection('artifacts', {...projections.artifacts, layer: layersOn[0] ?? null, layers: layersOn});
      return;
    }
    const {channel, presenter} = current;
    const before = channel.current.layers;
    const asked = layersAsked();
    // The channel publishes when its layers move; when they do not, the drawn set may still have.
    if (asked.length === before.length && asked.every((l, i) => l === before[i])) onArtifacts(channel.current);
    else channel.setLayers(asked);
    if (asked.some((l) => !before.includes(l)) && presenter.view) {
      const v = presenter.view;
      channel.refresh(v.view, v.width, v.height);
    }
  }

  /** The filter-position controls and `member_of` clauses, without the region leaf. */
  function filtersBesideRegion(): FilterExpr | null {
    return withMembers(composeFilters(projections.filters.draft, 'filter'), projections.filters.members, 'filter');
  }

  function requestFilters(): FilterExpr | null {
    const selected = region.selected;
    return withRegion(filtersBesideRegion(), selected ? regionOperand(selected) : null, selected?.outside ?? false);
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
   * The filters or the selection changed, so every view's held bands answer another question. A
   * held view is reset without asking, and refills when it is next current.
   */
  function requery(): void {
    for (const held of views.all()) {
      held.presenter.cancel();
      held.replica.reset();
    }
    contentKeyAtFrame = '';
    if (lastView) setView(lastView.input);
  }

  async function browse(req: Omit<BrowseRequest, 'filters' | 'view'> & {filters?: FilterExpr | null; view?: string}): Promise<BrowsePage> {
    const asked = await viewed();
    // The map's own filter unless the caller named one — including `null`, which asks for the
    // unfiltered counts explicitly. The view is the store's own current one: a masked count is an
    // intersection in row space and row space is per view, so a panel that named none would be
    // asking about whichever view the server chose.
    const filters = 'filters' in req ? (req.filters ?? null) : requestFilters();
    // `req.view ?? viewId`, never a spread: an explicit `view: undefined` in the caller's object
    // would clobber the store's own with a spread and the request would go out without one.
    return client.browse(asked.token, {...req, view: req.view ?? asked.view, filters});
  }

  function setMembers(clauses: readonly MemberClause[]): void {
    const members = [...clauses];
    replaceProjection('filters', {...projections.filters, members});
    region.loading(projections.marks);
    requery();
  }

  function setFilters(draft: FilterDraft): void {
    const expr = composeFilters(draft, 'filter');
    replaceProjection('filters', {...projections.filters, draft, expr, highlight: composeFilters(draft, 'highlight')});
    // A region's `matched` is under the filters, so its numbers are to a question just changed.
    region.loading(projections.marks);
    requery();
  }

  function setLayers(names: string[]): void {
    // Usually one, with its closure (decision 0096) — the request names every layer in it.
    layersOn = meta ? drawnOnly(layerClosure(meta.layers, names)) : names;
    askLayers();
  }

  function clearSelection(): void {
    replaceProjection('selection', {item: null, itemRefusal: null, artifact: null, artifactRefusal: null});
  }

  function setColourBy(column: string | null): void {
    const before = colourLayer();
    legend.setColourBy(column);
    traceUnknownColourLayer();
    // `cluster:<layer>` is a uniform switch on the vis side and accumulates nothing. Its layer is
    // fetched only where it is not already asked for (`askLayers`).
    if (colourLayer() !== before) askLayers();
    // No refetch for a column: every declared column is already in the held response, so this is
    // an accumulator pass over what is drawn (§4). The mark count cannot move.
    if (column && projections.view.composition) legend.accumulate(projections.view.composition, meta?.declaredScalars ?? []);
  }

  function setBudget(next: number): void {
    if (!Number.isFinite(next) || next <= 0) return;
    budget = next;
    views.current?.presenter.setBudget(next);
    views.current?.presenter.reschedule();
  }

  async function pick(id: bigint): Promise<void> {
    const epoch = clears;
    try {
      const detail = await client.item(await tokens.get(), id);
      if (disposed || epoch !== clears) return;
      replaceProjection('selection', {...projections.selection, item: {id, detail}, itemRefusal: null});
    } catch (error) {
      if (disposed || epoch !== clears) return;
      replaceProjection('selection', {
        ...projections.selection,
        item: null,
        itemRefusal: refusalOf(error)
      });
    }
  }

  /** The kind a served artifact's layer draws, from the meta; null where it draws none. */
  function shapeKindOf(id: bigint): ShapeKind | null {
    const layer = projections.artifacts.served.find((a) => a.tesseraId === id)?.layer;
    if (layer === undefined) return null;
    return projections.meta?.layers.find((l) => l.name === layer)?.shape ?? null;
  }

  async function openArtifact(id: bigint): Promise<void> {
    const epoch = clears;
    try {
      const asked = await viewed();
      const detail = await client.artifact(asked.token, id, {view: asked.view});
      if (disposed || epoch !== clears) return;
      // A renewal may have changed the principal under the request, and a shape can be theirs.
      if (tokens.current === asked.token) shapes.hold(id, detail.shape);
      replaceProjection('selection', {...projections.selection, artifact: {id, detail}, artifactRefusal: null, item: null, itemRefusal: null});
    } catch (error) {
      if (disposed || epoch !== clears) return;
      replaceProjection('selection', {
        ...projections.selection,
        artifact: null,
        artifactRefusal: refusalOf(error)
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
    const q = frame();
    return [
      q.xMin + (worldX / WORLD_SIZE) * (q.xMax - q.xMin),
      q.yMin + (worldY / WORLD_SIZE) * (q.yMax - q.yMin)
    ];
  }

  /** How many times the store has been cleared: an item or artifact asked for before a clear is not shown. */
  let clears = 0;

  function clear(): void {
    clears += 1;
    suggestions.reset();
    // Every held view, so none draws the previous principal's marks when it is returned to.
    for (const held of views.all()) {
      held.presenter.cancel();
      held.channel.reset();
      held.replica.reset();
    }
    views.cancelSettle();
    table.clear();
    shapes.forget('all');
    records.forget();
    clearSelection();
    contentKeyAtFrame = '';
    replaceProjection('view', {id: viewId, composition: null, depth: 0, visible: NO_MASKED, matched: NO_MASKED, highlighted: NO_MASKED, highlighting: false, served: NO_COUNT, provisional: 0});
    replaceProjection('marks', {...projections.marks, bands: [], standIn: [], count: NO_COUNT});
    replaceProjection('tiles', {tiles: []});
    legend.clear();
    replaceProjection('status', {...NO_STATUS});
    region.drop();
    publishReplica(null);
  }

  function refresh(): void {
    // Redraw against the current content key, refetching the artifacts with it.
    views.current?.channel.reset();
    region.loading(projections.marks);
    if (lastView) setView(lastView.input);
  }

  /** Set by `dispose`: a fetch that lands afterwards writes nothing into a store nobody reads. */
  let disposed = false;

  function dispose(): void {
    disposed = true;
    tokens.dispose();
    suggestions.dispose();
    shapes.dispose();
    records.dispose();
    legend.dispose();
    for (const held of views.all()) {
      held.presenter.cancel();
      held.channel.cancel();
    }
    views.cancelSettle();
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
    requestFilters,
    setFilters,
    setMembers,
    suggest: (column, q) => suggestions.suggest(column, q),
    setLayers,
    setColourBy,
    setPalette: (kind) => colours.setPalette(kind),
    setBudget,
    setCurrentView,
    frame: frameOrNull,
    pick,
    describe: (id) => records.describe(id),
    openArtifact,
    needShape: (id) => shapes.need(id),
    clearSelection,
    setScheme: (scheme) => colours.setScheme(scheme),
    select(shape) {
      if (region.select(shape, projections.marks)) requery();
    },
    extentOf,
    dataXY,
    clear,
    refresh,
    dispose
  };

  void ready().catch(() => {});

  return store;
}

/** Re-export for a consumer building a `ViewportResult` in a test. */
export type {ViewportResult};
