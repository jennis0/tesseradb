import {Aggregates, joinedAggregate, type AggregateSpec, type AggregatesProjection} from './aggregates.js';
import {ArtifactChannel, requestLevels, servedLineage, type ArtifactChannelState, type ServedLineage} from './artifactChannel.js';
import {SessionArtifactTable, type ArtifactTable} from './artifactTable.js';
import {BandBudget, bandKey, type Band, type BandKey} from './bands.js';
import {tileRectOfBbox, type DepthChoice} from './budget.js';
import {TesseraClient, TesseraError, type TesseraClientOptions} from './client.js';
import {ArtifactColours} from './colours.js';
import type {Composition} from './compose.js';
import {dataToWorldXY, gridToWorld, MAX_DEPTH, rectToRequestBbox, WORLD_SIZE} from './coords.js';
import {NO_COUNT, NO_MASKED, type Count, type Masked} from './counts.js';
import {RETRY_DEFAULTS, type Clock, type DriverOptions, type ViewState as DriverViewState} from './driver.js';
import {composeFilters, emptyDraft, withoutClause, type ClauseVerb, type FilterDraft} from './filters.js';
import {HeldRecords, HeldShapes} from './held.js';
import {HeldViews, type ViewMachinery} from './heldViews.js';
import {colourLayers, isFilterLayer, layerClosure} from './layers.js';
import {CLUSTER_PREFIX, EMPTY_LEGEND, Legend, type LegendProjection} from './legend.js';
import {withMembers, type MemberClause} from './members.js';
import {attachedTextOf} from './names.js';
import type {PaletteKind, PaletteScheme, Rgba} from './palette.js';
import {worldBbox} from './prefetch.js';
import {rectContainsTile} from './rects.js';
import {Presenter, defaultFrameScheduler, refusalOf, type FrameScheduler, type Presented, type PresentedStatus, type Refusal} from './presented.js';
import {insideBox, regionOperand, withRegion} from './region.js';
import type {RegionOperand} from './types.js';
import {DEFAULT_CACHE_BYTES, Replica, type ReplicaOptions} from './replica.js';
import {SelectedRegion, type RegionProjection, type SelectionShape} from './selectedRegion.js';
import {Suggestions, type SuggestState} from './suggestions.js';
import {TokenSupply, type TokenSupplier} from './token.js';
import type {
  Artifact,
  ArtifactDetail,
  BrowsePage,
  BrowseRequest,
  FilterExpr,
  ItemDetail,
  Meta,
  Quantisation,
  Shape,
  ShapeKind,
  Timings,
  ViewportResult
} from './types.js';

/**
 * The headless store: told where the viewer is looking, it hands back what to draw, from its
 * cache, on its own schedule. It holds no camera; the host calls `setView`. It hands the vis side
 * a `Composition` by reference and typed counts, never pixels.
 *
 * The frame scheduler and the clock are injected, defaulting to `requestAnimationFrame` and
 * `setTimeout`, so the store runs in node against a fake client.
 *
 * The store composes parts that each own their state: the token ({@link TokenSupply}), each view's
 * replica, presenter and artifact channel ({@link HeldViews}), the typeahead ({@link Suggestions}),
 * shapes and hovered records ({@link HeldShapes}, {@link HeldRecords}), the selected region
 * ({@link SelectedRegion}), artifact colours ({@link ArtifactColours}) and
 * the legend ({@link Legend}). What remains here is what they share: `meta`, the current view id,
 * the camera, the layers, the filters and the projections.
 */

export type {Count, Masked} from './counts.js';
export {formatCount, formatMasked} from './counts.js';
export type {TokenSupplier} from './token.js';
export {CLUSTER_PREFIX, type LegendProjection} from './legend.js';
export {REGION_HELD_LIMIT, type RegionProjection, type SelectionShape} from './selectedRegion.js';
export type {AggregateEntry, AggregateSpec, AggregatesProjection} from './aggregates.js';

/**
 * Where the host's camera looks: a box in the current view's data coordinates, the camera's zoom,
 * and the pixel size it is drawn at.
 *
 * @category Store
 */
export type ViewInput = {
  /**
   * `[x0, y0, x1, y1]` in the current view's data coordinates: what the canvas shows, which may be
   * clamped to the view's extent, as `viewInputOf` in `@tesseradb/deck` clamps it.
   */
  bbox: [number, number, number, number];
  /**
   * The camera's zoom over the 512-unit world, as deck.gl's `OrthographicView` counts it: the canvas
   * shows `2 ** zoom` pixels per world unit, so at 0 the world is 512 pixels wide. The levels a
   * layer shows and the depth its artifacts are asked at follow this zoom. It is not read from
   * `bbox`, which, clamped where the world is smaller than the canvas, would give the zoom at which
   * the world fills the canvas.
   */
  zoom: number;
  /** The canvas width in pixels. */
  width: number;
  /** The canvas height in pixels. */
  height: number;
};

/**
 * Options for {@link createStore}. One of `token`, `authorise` and `client` must be given. Requests
 * need a token: a store given only a `client` has every request refused as `bad-credential`.
 *
 * @category Store
 */
export type StoreOptions = {
  /** The viewer server's base URL. Unused where `client` is given. */
  viewerUrl: string;
  /**
   * A viewer token used for every request and never renewed. Where `token` and `authorise` are both
   * given, `token` is used and `authorise` is never called.
   */
  token?: string;
  /**
   * A function the store calls for a viewer token, and again to renew it before it expires (see
   * {@link TokenSupplier}). A store serves one viewer: to show another, call {@link Store.clear} or
   * create a new store. {@link Store} says how long a previous viewer's data stays without that.
   */
  authorise?: TokenSupplier;
  /**
   * The id of the view to open with. Defaults to the first view `meta.views` lists. A
   * `setCurrentView` call made before `/v1/meta` arrives takes its place.
   */
  view?: string;
  /** How many marks the store aims to draw on screen. Defaults to `500000`; `setBudget` changes it. */
  budget?: number;
  /** How artifacts are coloured (see {@link PaletteKind}). Defaults to `positional`; `setPalette` changes it. */
  palette?: PaletteKind;
  /**
   * Whether the store fetches ahead while the camera is still: the tiles around the view and one
   * zoom level deeper. Defaults to `true`.
   */
  prefetch?: boolean;
  /** Injected for tests; browser defaults otherwise. @internal */
  scheduler?: FrameScheduler;
  /** @internal */
  clock?: Clock;
  /** @internal */
  driver?: DriverOptions;
  /**
   * The store's cache of fetched tiles, which every view shares. Each of its four fields is
   * optional. `cacheBytes` is the byte budget for tiles held across every view, the tiles drawn
   * least recently being evicted first; it defaults to 512 MiB (`536870912`). `cache: false` holds
   * nothing, so every camera move becomes a request, for measuring what the cache saves; it defaults
   * to `true`. `revalidateAfterMs` is how long, in milliseconds since the last response, the store
   * may answer the camera wholly from held tiles before it sends a counts-only request (`k = 0`)
   * that refreshes the counts and shows whether the data has changed; it defaults to `60000`.
   * `onPhase(kind, ms, n)` is called with how long a named step inside the cache took, in
   * milliseconds, and over how many items, where `kind` is one of `plan`, `walk`, `revalidate`,
   * `remap`, `piece`, `split`, `store` and `slice`; it is for instrumentation and changes no
   * behaviour.
   */
  replica?: Pick<ReplicaOptions, 'cacheBytes' | 'cache' | 'revalidateAfterMs' | 'onPhase'>;
  /**
   * How the drawn layers' artifacts are asked for. `perTile` is the most artifacts one level shows
   * in one tile, largest first, at most `meta.selection.maxArtifactsPerTile`; the store has no
   * number of its own, so without it a drawn layer shows nothing, colouring by a layer
   * (`setColourBy('cluster:<layer>')`) has no colours either, and the `artifacts` projection is
   * `refused`, saying what to set. `budget` is the most artifacts a `nested` or `dag` layer is cut
   * to, met by drawing ancestors in place of their descendants, and the most it tags the points
   * with; omitted, the cut is unbounded. `heldTiles` is how many tiles of artifacts the store keeps
   * per view, the least recently drawn leaving first; it defaults to `4096`.
   *
   * Artifacts are asked for by tile at a fixed depth, map zoom + 2, the zoom being the camera's as
   * {@link ViewInput} gives it, rounded down; a tile is 128 to 256 pixels. One request names at most 558 tiles, the most a 3840 by 2160
   * screen touches at that depth, or the deployment's `max_tiles_per_request` where that is fewer; a
   * larger screen is asked for at zoom + 1, then coarser, until its tiles fit. So the tiles in view
   * times `perTile` bounds the artifacts one level draws. The depth does not follow the points'.
   */
  artifacts?: {perTile?: number; budget?: number; heldTiles?: number};
  /**
   * A `/v1/meta` response the host has already read, so the store does not fetch it again. It must
   * have been read with this viewer's token: another viewer's meta lists layers and views this one
   * may not be served. {@link Store.clear} reads meta again under the next token.
   */
  meta?: Meta;
  /**
   * A {@link TesseraClient} to send requests through, such as a test's fake. Defaults to one built
   * from `viewerUrl` and `clientOptions`. {@link Store.dispose} closes it.
   */
  client?: TesseraClient;
  /**
   * Options for the client the store builds, such as `headers`, `fetch` or `decoder`. Ignored where
   * `client` is given. The store calls no session route, so `sessionUrl` may be `''`.
   */
  clientOptions?: Omit<TesseraClientOptions, 'viewerUrl'>;
  /** Measurements the projections do not carry, for a demo's instrument panels. @internal */
  instruments?: {
    onFrame?(info: {plan: {choice: DepthChoice}; timings: Timings | null; bytes: number; held: number; fetched: number; calibration: {mTarget: number; visibleInView: number | undefined}; replica: {bytes: number; points: number; bands: number}}): void;
    onTrace?(kind: string, fields: Record<string, number | string>): void;
  };
};

/**
 * Everything the store publishes, by name. Each projection is immutable and replaced whole when it
 * changes, so a reader can compare by identity. {@link Store.subscribe} reports each replacement.
 *
 * @category Store
 */
export type Projections = {
  /**
   * The `/v1/meta` the store read or was given, or `null` before it has arrived, and after
   * {@link Store.clear} or an answer under another identity key until it is read again.
   */
  meta: Meta | null;
  /** Whether the current view's map is loading, shown, empty or refused. */
  status: StatusProjection;
  /** The current view's id, the frame on screen and the frame's counts. */
  view: ViewProjection;
  /** The marks on screen, for a renderer. */
  marks: MarksProjection;
  /** The tiles of the frame on screen. */
  tiles: TilesProjection;
  /** The artifacts served for the drawn layers, with their colours and shapes. */
  artifacts: ArtifactsProjection;
  /** The picked item and the opened artifact. */
  selection: SelectionProjection;
  /** The selected region and its counts, or `null` while nothing is selected. */
  region: RegionProjection | null;
  /** The filter controls, the expressions composed from them, the `member_of` clauses and the typeahead. */
  filters: FiltersProjection;
  /** The legend of the colour and size columns. */
  legend: LegendProjection;
  /** How much the store's tile cache holds. */
  replica: ReplicaProjection;
  /** Each aggregate registered with {@link Store.setAggregate}, by its id. */
  aggregates: AggregatesProjection;
};

/**
 * The name of one projection, a key of {@link Projections}. The names of map projections are a
 * different type, `ViewInfo['projection']`.
 *
 * @category Store
 */
export type ProjectionName = keyof Projections;

/**
 * The state of the current view's map, for a status line or an overlay.
 *
 * @category Projections
 */
export type StatusProjection = {
  /**
   * `idle` before the first request and after {@link Store.clear}. `loading` while a request is out,
   * or from a view switch until the incoming view draws. `retrying` while a request the server shed
   * (`429`) or answered as still starting (`503`) waits to be sent again. `shown` when a frame
   * answered. `empty` when the answer holds no item this viewer can see. `refused` when the request,
   * or the store's read of `/v1/meta`, was refused. Show counts only while it is `shown`, and do
   * not show `empty` and `refused` alike: `refused` says nothing about what exists.
   */
  status: PresentedStatus;
  /** Whether a frame has been shown or has drawn marks since the store was made or last cleared. */
  sessionWarm: boolean;
  /** The refusal while `status` is `refused`, else `null`. */
  refusal: Refusal | null;
  /**
   * Whether a response since the frame on screen was fetched reported that the data has changed,
   * so the counts on screen may be out of date. It clears when a frame fetched after the change is
   * drawn. Pass it to {@link formatCount} and {@link formatMasked} as `stale`.
   */
  stale: boolean;
  /**
   * Whether the refusal means the session ended: an `expired-token`, or a `bad-credential` for a
   * token this store has already used. A new token is needed.
   */
  expired: boolean;
  /** Whether `status` is `retrying`. */
  retrying: boolean;
};

/**
 * The current view and the frame on screen, with the frame's counts. Each count is summed over the
 * frame's exact tiles and taken from within this viewer's visible set.
 *
 * @category Projections
 */
export type ViewProjection = {
  /**
   * The view the store answers from: `''` before `/v1/meta` arrives, then an id `meta.views` lists.
   * A component compares it to learn that a switch happened.
   */
  id: string;
  /** The composition on screen, by reference; `null` before the view's first frame. */
  composition: Composition | null;
  /** The tile depth the frame was composed at; `0` before the first frame. */
  depth: number;
  /** How many items this viewer can see in the frame's exact tiles. */
  visible: Masked;
  /**
   * How many of `visible` the request's `filters` admit (see {@link Store.requestFilters}). Equal to
   * `visible` where no filter is set.
   */
  matched: Masked;
  /**
   * How many of `matched` the request's `highlight` also admits. Equal to `matched` where no
   * highlight is set, and also where a highlight admits everything the filter does, so
   * `highlighting` says whether one is set.
   */
  highlighted: Masked;
  /** Whether a `highlight` was set when this frame was presented. */
  highlighting: boolean;
  /** The marks the server sent for the exact tiles (`shown`) against `visible` (`total`). */
  served: Count;
  /**
   * How many stand-in marks are on screen, drawn from another depth while the frame's own tiles
   * arrive. A plain mark count, not a masked count.
   */
  provisional: number;
  /** The counts over the camera's box or the selected region; `null` before the view's first. */
  inView: InViewCounts | null;
};

/**
 * Counts over the camera's box, or over the selected region while one is selected, from `POST
 * /v1/aggregate`. They are asked for once the camera has rested for 250 ms, and again when the
 * filters, the highlight or the selection change. The frame's own counts cover whole tiles, which
 * reach past the box by an amount that changes with the depth drawn; these count the area itself,
 * so `visible` does not move when only a filter changes.
 *
 * @category Projections
 */
export type InViewCounts = {
  /**
   * `loading` while a count for the current camera, filters and selection is out, the previous
   * figures standing until it lands; `shown` once it has; `refused` where it was refused.
   */
  status: 'loading' | 'shown' | 'refused';
  /** The items this viewer can see in the area. */
  visible: Masked;
  /** How many of `visible` the filters admit. */
  matched: Masked;
  /** How many of `matched` the highlight also admits; equal to `matched` where none is set. */
  highlighted: Masked;
  /** The held marks of the frame's exact tiles that lie in the area. */
  shown: number;
};

/**
 * The marks on screen, for a renderer.
 *
 * @category Projections
 */
export type MarksProjection = {
  /** The frame's exact bands: the points served for the frame's own tiles, by reference. */
  bands: Composition['exact'];
  /**
   * Stand-in pieces, coarsest first: points from bands of another depth, drawn where the frame's own
   * tiles have not arrived. Each draws its band's `indices`, or its first `limit` points.
   */
  standIn: Composition['standIn'];
  /** The marks drawn from exact bands (`shown`) against the frame's `visible` count (`total`). */
  count: Count;
};

/**
 * The tiles of the frame on screen.
 *
 * @category Projections
 */
export type TilesProjection = {
  /**
   * One entry per contributing tile: its address, whether it is exact, how many marks it draws and,
   * for an exact tile or one whose counts have landed before its points, the server's counts.
   */
  tiles: Composition['tiles'];
};

/**
 * The artifacts served for the current view's drawn layers, with what a renderer needs to colour
 * and outline them. An artifact this viewer may not see is absent, as one that does not exist is.
 *
 * @category Projections
 */
export type ArtifactsProjection = {
  /** The first layer drawn, or `null` for none. */
  layer: string | null;
  /**
   * Every layer drawn, with its closure, and without filter layers. The colour layer is here only
   * when it is drawn too.
   */
  layers: string[];
  /** The served artifacts of the drawn layers, for the current camera. */
  served: Artifact[];
  /**
   * The served artifacts of the layer `setColourBy('cluster:<layer>')` names, whether it is drawn
   * or not; else empty.
   */
  colourServed: Artifact[];
  /** The forest `served` forms through its parent links. */
  lineage: ServedLineage;
  /**
   * The text of each attached artifact the channel serves (a clustering's topic labels), keyed by
   * the artifact it is attached to, over the drawn layers and the colour layer. The names
   * {@link artifactName} gives the artifacts in `served` and `colourServed`.
   */
  attached: ReadonlyMap<bigint, string>;
  /**
   * `idle` before the first answer and while no layer is asked for; `loading` while a request is
   * out; `shown` when `served` answers the current camera; `refused` when the request was refused,
   * `served` then being empty.
   */
  status: ArtifactChannelState['status'];
  /** The refusal while `status` is `refused`, else `null`. */
  refusal: {code: string; detail: string} | null;
  /** A number that increases each time `served` is replaced. */
  version: number;
  /** How many artifact payloads the session holds: the served set and those kept from earlier answers. */
  held: number;
  /**
   * The session artifact table, shared by every view, which resolves the ordinals in a band's
   * membership columns to artifacts.
   */
  table: ArtifactTable;
  /** The ordinals of `served` in `table`. */
  servedOrdinals: ReadonlySet<number>;
  /**
   * Shapes by `tesseraId`: those {@link Store.needShape} fetched, simplified for the view's zoom,
   * and those {@link Store.openArtifact} fetched, at full detail. An artifact with no entry has not
   * been asked for, has not answered or was refused, and is drawn as its `box`. A view switch,
   * {@link Store.clear} and an answer under another identity key empty it.
   */
  shapes: ReadonlyMap<bigint, Shape>;
  /**
   * A colour for every ordinal `table` holds. A band fetched under a coarser cut names artifacts
   * outside `servedOrdinals`, and its points take those artifacts' colours. An ordinal not here
   * has no colour of its own.
   */
  colours: ReadonlyMap<number, Rgba>;
  /** The palette `colours` was built under. */
  palette: PaletteKind;
  /**
   * How many bands in view have a colour for every point under each layer asked for (`current`),
   * and how many do not and are being fetched again (`stale`). Both are `0` unless `status` is
   * `shown` and a layer is asked for.
   */
  coverage: {current: number; stale: number};
};

/**
 * The picked item and the opened artifact, for a card. An id this viewer cannot see is refused as
 * an id that does not exist is.
 *
 * @category Projections
 */
export type SelectionProjection = {
  /**
   * The item {@link Store.pick} last fetched: its `tessera_id` and its record. `null` before a
   * pick, after a refused one, and once {@link Store.openArtifact} succeeds.
   */
  item: {id: bigint; detail: ItemDetail} | null;
  /** The refusal of the last pick, else `null`. */
  itemRefusal: Refusal | null;
  /**
   * The artifact {@link Store.openArtifact} last fetched: its `tessera_id` and its detail. `null`
   * before one is opened and after a refusal.
   */
  artifact: {id: bigint; detail: ArtifactDetail} | null;
  /** The refusal of the last `openArtifact`, else `null`. */
  artifactRefusal: Refusal | null;
};

/**
 * The filter controls, the expressions composed from them, the `member_of` clauses and the
 * category typeahead.
 *
 * @category Projections
 */
export type FiltersProjection = {
  /**
   * The controls as `setFilters` last set them. Until it is called, the store seeds one empty
   * control per filterable column ({@link emptyDraft}) when `/v1/meta` arrives.
   */
  draft: FilterDraft;
  /**
   * The controls in the `filter` position, composed; `null` for none. The `member_of` clauses and
   * the region are not here; {@link Store.requestFilters} gives the whole expression.
   */
  expr: FilterExpr | null;
  /**
   * The controls in the `highlight` position, composed; `null` for none. The `member_of` clauses in
   * that position are not here, though a request's `highlight` carries them.
   */
  highlight: FilterExpr | null;
  /** The `member_of` clauses, in either position, as `setMembers` last set them. */
  members: MemberClause[];
} & SuggestState;

/**
 * How much the store's tile cache holds.
 *
 * @category Projections
 */
export type ReplicaProjection = {
  /** Bytes held across every view's bands, the figure `replica.cacheBytes` bounds. */
  bytes: number;
  /** Points held for the current view. */
  points: number;
  /** Bands held for the current view. A band is one tile's points. */
  bands: number;
  /** How many views hold any band. */
  views: number;
  /**
   * For the last frame composed in full, rather than refreshed from the frame before: how many of
   * the tiles it wanted were already held (`held`) and how many were not (`fetched`). `null` before
   * the first such frame and after `clear`.
   */
  lastPlan: {held: number; fetched: number} | null;
};

type Listener = () => void;

/**
 * The headless store {@link createStore} returns. The host tells it where the camera looks, what to
 * filter and which layers to draw. The store fetches, caches and composes frames on its own
 * schedule and publishes what to draw as {@link Projections}. It holds no camera and draws nothing.
 * Every count and artifact it publishes is computed over this viewer's visible set.
 *
 * A store serves one viewer. To show the map to another viewer, call {@link Store.clear} or create
 * a new store.
 *
 * Behind that rule the store checks each viewport answer's identity key, as far as answers allow.
 * The server derives the key from the terms the session's principal held when it was minted, the
 * identity of the viewer's visible-set fragment, and the view. An ingest or a suppression does not
 * change it. A compaction or a rebuilt bundle gives the fragment a new identity, and so a new key,
 * for every viewer. A renewal whose principal now holds different terms also gives a new key, so
 * the store treats it as another viewer even for the same person.
 *
 * The store forgets what the server answered where an answer's key differs from the one held for
 * its view; where, after a renewal, an answer arrives on a view that holds no key before any answer
 * under the new token has matched a held key; where the server refuses the renewed token the
 * current view, other than by shedding load or for an expired token; and at a renewal while it
 * holds `meta` and no key. What it drops: points, frames, counts, artifacts, shapes, item records,
 * legend values, typeahead pages, the picked item and opened artifact, the `member_of` clauses, a
 * region selected from an artifact, the region's counts, and `meta`. What it keeps: a drawn
 * region's shape, the filter controls, the layers, the colouring, the current view and the camera.
 * It then reads `meta` again under the token it holds, drops a kept input that names a view, layer
 * or column the new `meta` does not list, and asks again. A token renewal whose key matches keeps
 * what is drawn.
 *
 * The check runs on answers. The store calls `authorise` only when the token it holds is due for
 * renewal, and then asks for the current view's key at once. So without {@link Store.clear}, the
 * previous viewer's data stays published until that renewal and the answer or refusal to that ask;
 * where the ask is shed, fails or finds the token expired, until the next answer under the new
 * token.
 *
 * @category Store
 */
export interface Store {
  /**
   * The current projections. The object is the store's own and its fields are replaced as
   * projections change, so read a field when it is needed.
   */
  readonly projections: Projections;
  /** The current value of projection `name`. */
  get<K extends ProjectionName>(name: K): Projections[K];
  /**
   * Call `fn` after any projection is replaced. A subscriber that throws is reported to the console
   * and the others are still called.
   *
   * @returns A function that unsubscribes `fn`.
   */
  subscribe(fn: Listener): () => void;
  /**
   * Call `fn` with the new value each time projection `name` is replaced. `fn` is not called with the
   * current value; read that with `get`. A subscriber that throws is reported to the console and the
   * others are still called.
   *
   * @returns A function that unsubscribes `fn`.
   */
  subscribe<K extends ProjectionName>(name: K, fn: (value: Projections[K]) => void): () => void;

  /**
   * Point the store at a camera. The store fetches what the view needs after a short debounce and
   * publishes each frame as it is composed. Called before `/v1/meta` arrives, the last call is
   * applied when it does.
   */
  setView(input: ViewInput): void;
  /**
   * Replace the filter controls. Publishes `filters` with the draft and its composed expressions,
   * drops every view's held tiles, which answer the old filters, and asks again for the current
   * camera. A selected region shows `loading` until the new frame lands.
   */
  setFilters(draft: FilterDraft): void;
  /**
   * Replace the `member_of` clauses: a card's "filter to this" and "outside this", and a hierarchy
   * panel's nodes. It takes the whole list; build it with {@link withMember} and
   * {@link withoutMember}. Publishes `filters.members`, then drops held tiles and asks again as
   * `setFilters` does.
   */
  setMembers(clauses: readonly MemberClause[]): void;
  /**
   * One page of a layer's hierarchy (`POST /v1/artifacts/browse`): its roots, an artifact's children
   * and parents, or a name search, each row with its masked count. An artifact this viewer was
   * never served answers an empty page. Waits for `/v1/meta`.
   *
   * @param req - The page to ask for. `filters` defaults to {@link Store.requestFilters}, so the
   *   tree counts what the map counts; pass `null` for unfiltered counts. `view` defaults to the
   *   current view.
   * @throws {@link TesseraError} where the server refuses the request, such as `422` for an unknown
   *   layer, or where the store's read of `/v1/meta` was refused.
   */
  browse(req: Omit<BrowseRequest, 'filters' | 'view'> & {filters?: FilterExpr | null; view?: string}): Promise<BrowsePage>;
  /**
   * The `filters` expression every request carries: the filter-position controls, the `member_of`
   * clauses in that position and the selected region's leaf, joined by `all_of`. `null` where none
   * is set. The expression is JSON-safe: an artifact in a `member_of` or `region` leaf is a decimal
   * string.
   */
  requestFilters(): FilterExpr | null;
  /**
   * Keep an aggregate of the current view under `id` (`POST /v1/aggregate`), published in the
   * `aggregates` projection; `null` drops it. The request carries `spec`'s groupings and reference as
   * given, and {@link Store.requestFilters} as its `filters`, so the counts are over what the map
   * counts. With `spec.without` naming a column, its control in the filter position is left out of
   * `filters`, as {@link withoutClause} empties it, so a filter control's own counts keep showing
   * the values its clause would exclude while every other clause still narrows them. With
   * `spec.withoutMembersOf` naming a layer, that layer's `member_of` clauses in the filter position
   * are left out in the same way. A `429` or
   * `503` is sent again as the map's requests are, with the same backoff, and published as
   * `retrying` meanwhile. The store asks again, aborting the request it replaces, when `setAggregate` is called
   * for the id again, when the filters, the `member_of` clauses or the selected region change, at a
   * view switch, at {@link Store.refresh}, and once it has read `/v1/meta` again after forgetting
   * what the server answered. Waits for `/v1/meta`. Each call sends a new request, so call it when
   * the component's spec changes, not on every render.
   */
  setAggregate(id: string, spec: AggregateSpec | null): void;
  /**
   * Ask a category column's typeahead for `q`, from a control in position `verb`, 120 ms after the
   * last call for that column. The page lands in `filters.suggestions[column]`, each value with its
   * count in the current view among the items passing a filter, and the `total` those counts are
   * taken over. In `filter` the filter is {@link Store.requestFilters} without the column's own
   * filter clause, so each value counts what choosing it as well would add; in `highlight` it is the
   * whole of it, so a value the filter excludes counts 0. A refusal lands in
   * `filters.suggestErrors[column]`. A call repeating the `q` and `verb` last asked for does nothing;
   * any other call asks again and cancels the column's request before. A change of filter or region
   * asks again for each column whose request would carry another filter, keeping its page until the
   * new one lands. Requests are sent one at a time, since the server runs one suggestion per
   * session. An ask the server sheds (`429`) is retried after the wait it gives, at least 0.25 s, up
   * to five times; an ask from the box then reports `backpressure`, and one from a change of filter
   * keeps the page it had. Waits for `/v1/meta`.
   */
  suggest(column: string, q: string, verb: ClauseVerb): void;
  /**
   * Stop asking for `column`'s suggestions and drop its page and refusal, so a change of filter no
   * longer asks for it. A control calls this when its box is emptied and when it goes away.
   */
  forgetSuggestions(column: string): void;
  /**
   * Draw these layers, each with its closure ({@link layerClosure}); `[]` draws none. Filter layers
   * are dropped from the list. A layer newly named is fetched at once. Publishes `artifacts`. Called
   * before `/v1/meta` arrives, the names are published as given and resolved when it does.
   */
  setLayers(names: string[]): void;
  /**
   * Colour points by a declared column, by `cluster:<layer>` for a layer {@link colourLayers} lists,
   * or `null` for uniform. Publishes `legend`. Colouring by a layer fetches its artifacts into
   * `artifacts.colourServed`, and its labels into `artifacts.attached`, without drawing them. A
   * `cluster:` layer that `meta` does not list as one that can colour is named in no request, and
   * the points draw uniform. A `render` column is carried by the points from then on, and the held
   * bands without it are fetched again (see {@link Store.setPointColumns}).
   */
  setColourBy(column: string | null): void;
  /**
   * Size points by a declared number column, or `null` for one size. Publishes `legend`, whose
   * `domains` and `missing` then accumulate the column from the marks drawn, and with `rank` set
   * `samples` too, for sizing by rank. The column is carried by the points as the colour column is.
   * A column that is a category, not a number or not rendered sizes nothing.
   */
  setSizeBy(column: string | null, options?: {rank?: boolean}): void;
  /**
   * Have each point carry the render columns `columns` names, on behalf of `id`, beside the colour
   * and size columns; `[]` withdraws `id`'s ask. A point request names the union of every ask with
   * the colour and size columns, and the server reads no other render column, so a column read
   * from {@link Band.scalars} (a hover's fields, say) must be asked for here. A name that is not a
   * render column of the current view is left out of the request. Each held band without a column
   * newly asked for is fetched again, in view or not, and stays drawn until its replacement
   * arrives; dropping a column sends no request. The asks hold for every view.
   */
  setPointColumns(id: string, columns: readonly string[]): void;
  /**
   * Colour artifacts by `kind`. Publishes `artifacts.colours` and `artifacts.palette`; the kind in
   * use does nothing.
   */
  setPalette(kind: PaletteKind): void;
  /**
   * Set how many marks the store aims to draw on screen, for every view, and ask again for the
   * current camera. A value that is not a finite number above zero is ignored.
   */
  setBudget(budget: number): void;
  /** The marks the store aims to draw: `budget` as created, else 500000, until `setBudget` changes it. */
  readonly budget: number;
  /**
   * Make `id` the view the store answers from. An id `meta.views` does not list is ignored, as is
   * the current id. Called before `/v1/meta` arrives, it names the view to open with, in place of
   * `options.view`.
   *
   * Between views quantised against the same extent, the camera and the selected region carry
   * over: the incoming view draws what it holds at once and fetches 140 ms after the last switch.
   * Across extents both are dropped, and the host's next `setView` supplies the camera. A switch
   * publishes `view` with no frame and `status` as `loading`, empties `marks` and `tiles`, drops
   * the typeahead's pages and the fetched shapes, and publishes the incoming view's artifacts.
   */
  setCurrentView(id: string): void;
  /**
   * The extent the current view's positions are quantised against, in data coordinates, or `null`
   * before `/v1/meta` has arrived.
   */
  frame(): Quantisation | null;
  /**
   * Fetch one item's record (`POST /v1/items/{tessera_id}`) and publish it as `selection.item`,
   * keeping any opened artifact. A refusal is published as `selection.itemRefusal` and the promise
   * still resolves. An id this viewer cannot see is refused as `unknown`, as an id that does not
   * exist is. An answer that lands after `clear` or `dispose` is dropped.
   */
  pick(id: bigint): Promise<void>;
  /**
   * One item's fields for a hover, fetched once per id and held. Resolves to `null` where the
   * request was refused, and holds that too. It publishes nothing; {@link Store.pick} opens a card.
   * When the store forgets what the server answered (see {@link Store}) it drops what is held, and
   * an answer asked for before then resolves to `null`.
   */
  describe(id: bigint): Promise<Record<string, unknown> | null>;
  /**
   * Fetch one artifact's detail under the current view (`POST /v1/artifacts/{tessera_id}`) and
   * publish it as `selection.artifact`, clearing `selection.item`. Its shape is held in
   * `artifacts.shapes` at full detail. A refusal is published as `selection.artifactRefusal` and the
   * promise still resolves. An artifact this viewer cannot reach is refused as one that does not
   * exist is. Waits for `/v1/meta`; an answer that lands after `clear` or `dispose` is dropped.
   */
  openArtifact(id: bigint): Promise<void>;
  /**
   * Ask for one artifact's shape, which lands in `artifacts.shapes` simplified for the current
   * view's zoom. The viewport serves only centroids and boxes. A shape held, in flight or refused is
   * not asked for again, so this may be called on every pointer move. Waits for `/v1/meta`.
   */
  needShape(id: bigint): void;
  /** Clear the picked item, the opened artifact and their refusals. Publishes `selection`. */
  clearSelection(): void;
  /**
   * Set the ground the map is drawn on, which the palette chooses lightness against. The store
   * starts on `dark`. Publishes `artifacts.colours` when it changes.
   */
  setScheme(scheme: PaletteScheme): void;
  /**
   * Select a region, or clear the selection with `null` or a lasso of fewer than three points.
   * Publishes `region`. The selection goes on every request as a `region` filter leaf, so selecting
   * drops every view's held tiles and asks again for the current camera.
   */
  select(shape: SelectionShape | null): void;
  /**
   * A served artifact's box in the current view's data coordinates, `[x0, y0, x1, y1]`, or `null`
   * where the artifact is not in `artifacts.served` or has no box.
   */
  extentOf(artifactId: bigint): [number, number, number, number] | null;
  /**
   * The data coordinates of a world position in the current view. World space runs from `0` to
   * {@link WORLD_SIZE} on each axis across the view's extent.
   *
   * @throws `Error` before `/v1/meta` has arrived.
   */
  dataXY(worldX: number, worldY: number): [number, number];
  /**
   * Forget the viewer, to show the map to another one. {@link Store} says how long a previous
   * viewer's data stays without this call.
   * Drops the token, `meta`, every view's held tiles and artifacts, the session artifact table, the
   * shapes, item records, legend, typeahead pages, `member_of` clauses, region and selection, and
   * abandons every request in flight. Publishes the emptied projections, with `meta` as `null` and
   * `status` as `idle`, before it returns. It then asks `authorise` for a token, reads `/v1/meta`
   * with it and asks again for the camera. A store given a fixed `token` keeps it.
   *
   * The filters set with `setFilters`, the layers, the colouring, the columns asked for with
   * `setPointColumns`, the current view and the camera are kept. A filter draft the store seeded from `meta` is seeded again from the next. A filter
   * control, layer or colouring naming a column or layer the next `meta` does not list is dropped.
   * Where the next `meta` does not list the current view, the store opens on its first view and
   * drops the camera.
   */
  clear(): void;
  /**
   * Ask again for the current camera, as after a refusal. The current view's artifacts are dropped
   * and fetched afresh, and the marks are fetched where the held tiles do not answer the view. A
   * selected region shows `loading` until the new frame lands.
   */
  refresh(): void;
  /**
   * Stop every timer and request, and close the client, including one passed as `client`. An answer
   * that lands afterwards publishes nothing.
   */
  dispose(): void;
}

/** The `view` projection before a frame has been drawn in view `id`. */
function noFrame(id: string): ViewProjection {
  return {id, composition: null, depth: 0, visible: NO_MASKED, matched: NO_MASKED, highlighted: NO_MASKED, highlighting: false, served: NO_COUNT, provisional: 0, inView: null};
}

/**
 * The camera's centre on one axis, from the box it shows `[lo, hi]` in world units and half the
 * canvas at its zoom. Where the box is narrower than the canvas it was clamped to the world on the
 * side that meets an edge; where it meets both, the world's middle shows the same box.
 */
function cameraCentre(lo: number, hi: number, half: number): number {
  const clamped = hi - lo < 2 * half * (1 - 1e-9);
  if (clamped && lo <= 0 && hi < WORLD_SIZE) return hi - half;
  if (clamped && hi >= WORLD_SIZE && lo > 0) return lo + half;
  return (lo + hi) / 2;
}

/** How long the camera rests before the counts in view are asked for. */
const IN_VIEW_REST_MS = 250;

/** How many tiles of artifacts a view keeps where `artifacts.heldTiles` names no number. */
const DEFAULT_HELD_TILES = 4096;

const NO_STATUS: StatusProjection = {
  status: 'idle',
  sessionWarm: false,
  refusal: null,
  stale: false,
  expired: false,
  retrying: false
};

/**
 * Create a store and start reading `/v1/meta`, unless `options.meta` is given. A refused read
 * publishes `status` as `refused`, and is tried again by the next call that waits for `/v1/meta`
 * and whenever `authorise` supplies a token.
 *
 * @throws `Error` where none of `options.token`, `options.authorise` and `options.client` is given.
 *
 * @category Store
 */
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

  let meta: Meta | null = null;
  let budget = options.budget ?? 500_000;
  let contentKeyAtFrame = '';

  /** One byte budget across every view's bands, evicted least recently drawn across them. */
  let bandBudget = new BandBudget(options.replica?.cacheBytes ?? DEFAULT_CACHE_BYTES);
  const views = new HeldViews(clock, buildView, options.view ?? '');

  /** The layers drawn, with their closure, as `setLayers` last named them. */
  let layersOn: string[] = [];
  let lastView: {input: ViewInput} | null = null;
  /** A `setView` before meta. */
  let queuedView: ViewInput | null = null;
  /** A `setCurrentView` before meta, applied at warm-up in place of `options.view`. */
  let queuedCurrentView: string | null = null;
  /**
   * A switch published `loading`, and a frame drawn from the incoming view's held bands ends it,
   * since no request's status will. Cleared by {@link setView}, after which a cold switch stays at
   * `loading` until its request answers.
   */
  let awaitingSwitchFrame = false;

  const tokens = new TokenSupply(options.authorise, options.token, clock, (changed) => {
    // A renewal keeps what is held. Until an answer under the new token confirms a key held, `admit`
    // does not trust an answer on a view that holds no key, and one ask goes out at once so the key
    // is seen without waiting for the camera. A store holding `meta` and no key yet has nothing to
    // compare the next answer with, so it reads `meta` again.
    if (changed && identities.size > 0) {
      unconfirmed = tokens.current;
      void askIdentity();
    } else if (changed && meta !== null) {
      forgetAnswers();
    }
    // A warm-up that failed for want of a token runs again now there is one.
    if (meta === null) void ready().catch(() => {});
  });

  /** The identity key of the answers held, by view; see {@link Store} for what it derives from. */
  const identities = new Map<string, string>();
  /** A token a renewal brought, until an answer under it matches a key in {@link identities}. */
  let unconfirmed: string | null = null;

  const colours = new ArtifactColours(table, options.palette ?? 'positional', (map, palette) =>
    replaceProjection('artifacts', {...projections.artifacts, colours: map, palette})
  );
  /** The served-set version each colour-stale band was last fetched again under. */
  const colourAsked = new Map<BandKey, number>();
  /** The render columns each caller of {@link setPointColumns} asked for, by its id. */
  const pointColumnAsks = new Map<string, readonly string[]>();
  /** What the last {@link checkPointColumns} looked at, so a frame that changed none of it costs nothing. */
  let pointColumnsChecked: {replica: Replica; version: number; columns: string} | null = null;

  const legend = new Legend(
    async (column, codes) => client.categories(await tokens.get(), column, {codes, view: views.id}),
    (value) => replaceProjection('legend', value)
  );

  const suggestions = new Suggestions(
    clock,
    (column, verb) => (verb === 'highlight' ? requestFilters() : filtersWithout(column)),
    async (column, q, filters, signal) => {
      const asked = await viewed();
      return client.suggest(asked.token, column, q, {view: asked.view, counts: true, ...(filters === null ? {} : {filters}), signal});
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

  /** `client.aggregate`, with the requests asked for in one task over the same set joined. */
  const aggregate = joinedAggregate(
    (token, req, signal) => client.aggregate(token, req, signal),
    () => meta?.selection.maxAggregateGroupings ?? 1
  );

  const aggregates = new Aggregates(
    async (spec, signal) => {
      const asked = await viewed();
      const filters = aggregateFilters(spec);
      const reference = spec.reference === 'visible' ? {} : spec.reference;
      const result = await aggregate(
        asked.token,
        {view: asked.view, groupings: spec.groupings, ...(filters === null ? {} : {filters}), ...(reference === undefined ? {} : {reference})},
        signal
      );
      // A response cancelled before its first page carries no key.
      if (result.identityKey !== '' && !admit(asked.view, result.identityKey, asked.token)) throw identityChanged();
      return {result, view: asked.view};
    },
    (entries) => replaceProjection('aggregates', entries),
    clock,
    {...RETRY_DEFAULTS, ...options.driver}
  );

  /**
   * The counts in view: `counts` is the set under the filters against the area as its reference,
   * `highlighted` the set under the highlight as well, registered only while one is set.
   */
  const inView = new Aggregates(
    async (spec, signal) => {
      const asked = await viewed();
      const area = countedArea()!;
      const box = area.selected ? null : lastView!.input.bbox;
      const filters = withArea(aggregateFilters(spec), area);
      const result = await aggregate(asked.token, {view: asked.view, groupings: spec.groupings, filters: filters!, reference: withArea(null, area)!}, signal);
      if (box) boxOf.set(result, box);
      if (result.identityKey !== '' && !admit(asked.view, result.identityKey, asked.token)) throw identityChanged();
      return {result, view: asked.view};
    },
    (entries) => publishInView(entries),
    clock,
    {...RETRY_DEFAULTS, ...options.driver}
  );
  /** The pending ask after the camera moved. */
  let inViewTimer: unknown = null;
  /** The camera's box each count in view was taken over; a selected region's count has none. */
  const boxOf = new WeakMap<object, [number, number, number, number]>();
  /** The box the counts in view on show were taken over, which the shown count is taken over too. */
  let countedBox: [number, number, number, number] | null = null;
  /** Whether `inView` holds its two specs, and the highlighted one. */
  let inViewRegistered = false;
  let inViewHighlighted = false;

  const records = new HeldRecords((id) => tokens.get().then((t) => client.item(t, id)).then((detail) => detail.fields));

  const projections: Projections = {
    meta: null,
    status: NO_STATUS,
    view: noFrame(''),
    marks: {bands: [], standIn: [], count: NO_COUNT},
    tiles: {tiles: []},
    artifacts: {layer: null, layers: [], served: [], colourServed: [], lineage: servedLineage([]), attached: new Map(), status: 'idle', refusal: null, version: 0, held: 0, table, servedOrdinals: new Set(), shapes: new Map(), colours: new Map(), palette: colours.palette, coverage: {current: 0, stale: 0}},
    selection: {item: null, itemRefusal: null, artifact: null, artifactRefusal: null},
    region: null,
    filters: {draft: emptyDraft([]), expr: null, highlight: null, members: [], suggestions: {}, suggestErrors: {}, suggestEpoch: 0},
    legend: EMPTY_LEGEND,
    replica: {bytes: 0, points: 0, bands: 0, views: 0, lastPlan: null},
    aggregates: new Map()
  };

  const all: Set<Listener> = new Set();
  const perName = new Map<ProjectionName, Set<() => void>>();

  /**
   * Publish one projection to every subscriber. A subscriber that throws is reported to the console
   * and to `onTrace`, and the subscribers after it are still told.
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
    return {token: await tokens.get(), view: views.id};
  }

  /** The store's first `/v1/meta`, read once and shared. A failed warm-up is forgotten, so the next call runs it again. */
  let warming: Promise<void> | null = null;

  function ready(): Promise<void> {
    if (warming) return warming;
    const attempt: Promise<void> = warm().catch((error: unknown) => {
      // A `clear` may have started another warm-up since.
      if (warming === attempt) {
        warming = null;
        if (!disposed) {
          const refusal = refusalOf(error);
          replaceProjection('status', {...projections.status, status: 'refused', refusal, expired: tokens.isExpiry(refusal)});
        }
      }
      throw error;
    });
    warming = attempt;
    return attempt;
  }

  /** The current view's frame, or `null` before `meta`. Each view of a bundle may declare its own. */
  function frameOrNull(): Quantisation | null {
    return quantisationOf(views.id);
  }

  /** One named view's frame, or `null` where the bundle declares no such view. */
  function quantisationOf(id: string): Quantisation | null {
    return meta?.views.find((v) => v.id === id)?.quantisation ?? null;
  }

  /** Whether two views quantise against one extent, compared by value. The camera carries between them. */
  function sameFrame(a: Quantisation | null, b: Quantisation | null): boolean {
    return (
      a !== null && b !== null && a.xMin === b.xMin && a.xMax === b.xMax && a.yMin === b.yMin && a.yMax === b.yMax
    );
  }

  /** {@link frameOrNull} for a caller that holds `meta`. There is no default extent to fall back to. */
  function frame(): Quantisation {
    const q = frameOrNull();
    if (!q) throw new Error(`the bundle declares no view '${views.id}'`);
    return q;
  }

  /**
   * Build one view's machinery, bound to that view's id and frame. Its events reach the projections
   * only while it is the current view. The session artifact table is shared, so an artifact served
   * in two views has one ordinal and one colour.
   */
  function buildView(id: string): ViewMachinery {
    const m = meta;
    if (!m) throw new Error('a view is built after meta');
    const q = quantisationOf(id);
    if (!q) throw new Error(`the bundle declares no view '${id}'`);
    /** Assigned below; the fetch reads it. */
    let ownPresenter: Presenter | null = null;
    /** Assigned below. Machinery {@link forgetAnswers} dropped is never current again. */
    let own: ViewMachinery | null = null;
    const current = () => own !== null && views.current === own;
    /**
     * Whether an answer asked for under `token` may be held: while the store still holds this
     * machinery, and {@link admit} keeps it.
     */
    const admitted = (identityKey: string, token: string): boolean => own !== null && views.holds(own) && admit(id, identityKey, token);

    const built = new Replica(
      async (req, signal, background, onPart, onCounts) => {
        const tok = await tokens.use();
        // The layers named put a membership column on each band, and the columns named are the
        // render columns it carries. A counts-only revalidation (`k = 0`) absorbs no points, so it
        // names neither. The budget and levels are the channel's, so a point's membership names an
        // artifact of the cut the panels show.
        const zoom = ownPresenter?.view?.view.zoom ?? 0;
        const layers = req.k === 0 ? [] : pointLayers();
        const columns = req.k === 0 ? undefined : pointColumns(id);
        const budget = options.artifacts?.budget;
        const response = await client.viewport(
          tok,
          {
            ...req,
            view: id,
            filters: requestFilters(),
            highlight: requestHighlight(),
            layers,
            ...(columns === undefined ? {} : {pointRows: columns}),
            ...(layers.length === 0 || budget === undefined ? {} : {artifactBudget: budget}),
            ...(layers.length === 0 || requestLevels(m.layers, layers, zoom) === undefined ? {} : {levels: requestLevels(m.layers, layers, zoom)})
          },
          {
            signal,
            background,
            onPart:
              onPart &&
              ((part) => {
                if (!admitted(part.identityKey, tok)) throw identityChanged();
                return onPart(part);
              }),
            onCounts:
              onCounts &&
              ((counts) => {
                if (!admitted(counts.identityKey, tok)) throw identityChanged();
                onCounts(counts);
              })
          }
        );
        if (!admitted(response.identityKey, tok)) throw identityChanged();
        return response;
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
          // A stored slice is drawable, so the first marks arrive with the first slice.
          if (kind === 'piece' || kind === 'store') ownPresenter?.absorbed();
        },
        // Counts are drawable as density and figures before the points that follow them.
        onCounts: () => ownPresenter?.absorbed(),
        now: () => clock.now()
      }
    );

    ownPresenter = new Presenter(
      built,
      {
        kMaxMarks: m.selection.kMaxMarks,
        maxTilesPerRequest: m.maxTilesPerRequest,
        thetaTargetMarks: m.selection.thetaTargetMarks,
        kMin: m.selection.kMin,
        filtered: () => requestFilters() !== null
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
      projection: m.views.find((v) => v.id === id)?.projection ?? 'none',
      perTile: options.artifacts?.perTile ?? null,
      ...(options.artifacts?.budget === undefined ? {} : {budget: options.artifacts.budget}),
      heldTiles: options.artifacts?.heldTiles ?? DEFAULT_HELD_TILES,
      prefetch: options.prefetch ?? true,
      token: () => tokens.get(),
      maxTiles: m.maxTilesPerRequest,
      table,
      // The point path's filters, region included, so an artifact's `matched` bit counts members
      // inside the selection.
      filters: () => requestFilters(),
      declarations: m.layers,
      onChange: (state) => {
        if (current()) onArtifacts(state);
      },
      admit: admitted
    });
    // Layers set before this view existed, including before meta.
    viewChannel.setLayers(layersAsked());

    own = {replica: built, presenter: ownPresenter, channel: viewChannel};
    return own;
  }

  /**
   * Every viewport answer, on the point path and the artifact channel, passes through here before
   * anything in it is held, with the token it was asked under. The store runs
   * {@link forgetAnswers} where the answer's identity key differs from the one held for its view,
   * and where a renewal brought a token no answer has yet matched to a held key and the answer's
   * view holds none, since that key could be another viewer's.
   *
   * @returns Whether the answer may be held. False drops it: the machinery that asked is gone.
   */
  function admit(view: string, identityKey: string, token: string): boolean {
    const held = identities.get(view);
    const unchecked = held === undefined && unconfirmed !== null && identities.size > 0;
    if ((held !== undefined && held !== identityKey) || unchecked) {
      forgetAnswers();
      return false;
    }
    if (held !== undefined && token === unconfirmed) unconfirmed = null;
    identities.set(view, identityKey);
    return true;
  }

  /**
   * Asks for the current view's identity key under a token a renewal brought: counts only, one tile
   * at depth 0, with no filter, highlight or layer, so nothing set for the previous viewer can have
   * it refused. The server refusing the view the previous token reached shows another viewer, and
   * the store forgets as {@link admit} does. A shed request, an expired token or a failed fetch
   * leaves the next answer to {@link admit}.
   */
  async function askIdentity(): Promise<void> {
    const current = views.current;
    const q = frameOrNull();
    if (!current || !q) return;
    const view = views.id;
    let tok: string | null = null;
    try {
      tok = await tokens.get();
      const response = await client.viewport(tok, {view, zoom: 0, bbox: rectToRequestBbox({x0: 0, y0: 0, x1: 0, y1: 0}, 0, q), k: 0, layers: []});
      if (!disposed && views.current === current) admit(view, response.identityKey, tok);
    } catch (error) {
      if (disposed || tok === null || unconfirmed !== tok || !(error instanceof TesseraError)) return;
      if (error.status === 429 || error.status === 503 || tokens.isExpiry(refusalOf(error))) return;
      forgetAnswers();
    }
  }

  async function warm(): Promise<void> {
    // A warm-up overtaken by a `clear` hands over to the one the clear started.
    const epoch = clears;
    let read: Meta;
    try {
      const t = await tokens.use();
      // `options.meta` was read for the viewer the store was made for.
      read = (epoch === 0 ? options.meta : undefined) ?? (await client.meta(t));
    } catch (error) {
      if (!disposed && epoch !== clears) return ready();
      throw error;
    }
    if (disposed) return;
    if (epoch !== clears) return ready();
    meta = read;
    // A `setCurrentView` before meta names the view to open with, in place of `options.view`.
    if (queuedCurrentView !== null) {
      const wanted = queuedCurrentView;
      queuedCurrentView = null;
      if (meta.views.some((v) => v.id === wanted)) views.name(wanted);
      else onTrace('view-switch', {refused: 1, id: wanted});
    }
    if (!meta.views.some((v) => v.id === views.id)) {
      if (views.id) {
        // A view this viewer is not served, named by `options.view` or kept through a `clear`. The
        // camera was in its data coordinates.
        onTrace('view-switch', {refused: 1, id: views.id});
        lastView = null;
        queuedView = null;
        region.drop();
      }
      views.name(meta.views[0]?.id ?? '');
    }
    if (epoch > 0) dropUnoffered(meta);
    replaceProjection('meta', meta);
    replaceProjection('view', {...projections.view, id: views.id});
    // One empty control per operand set the bundle publishes.
    const held = projections.filters.draft;
    if (Object.keys(held.filter).length === 0 && Object.keys(held.highlight).length === 0) {
      const draft = emptyDraft(meta.filterOperands);
      replaceProjection('filters', {...projections.filters, draft, expr: composeFilters(draft, 'filter'), highlight: composeFilters(draft, 'highlight')});
    }

    layersOn = drawnOnly(layerClosure(meta.layers, layersOn));
    traceUnknownColourLayer();
    // The layers as `meta` resolved them, published with the view's artifacts.
    onArtifacts(views.enter(views.id).channel.current);

    if (queuedView) {
      const q = queuedView;
      queuedView = null;
      setView(q);
    } else if (lastView) {
      setView(lastView.input);
    }
  }

  /**
   * After `meta` is read again, drop the host's inputs that name what it no longer offers: filter
   * controls on columns it does not list, layers it does not list, a colouring by a column or
   * layer it does not list, and a sizing by a column it does not list.
   */
  function dropUnoffered(m: Meta): void {
    const columns = new Set(m.filterOperands.map((f) => f.column));
    const layerNames = new Set(m.layers.map((l) => l.name));
    const {draft} = projections.filters;
    const kept = (controls: FilterDraft['filter']) => Object.fromEntries(Object.entries(controls).filter(([column]) => columns.has(column)));
    const keptDraft: FilterDraft = {filter: kept(draft.filter), highlight: kept(draft.highlight)};
    const count = (d: FilterDraft) => Object.keys(d.filter).length + Object.keys(d.highlight).length;
    if (count(keptDraft) !== count(draft)) {
      replaceProjection('filters', {
        ...projections.filters,
        draft: keptDraft,
        expr: composeFilters(keptDraft, 'filter'),
        highlight: composeFilters(keptDraft, 'highlight')
      });
    }
    layersOn = layersOn.filter((name) => layerNames.has(name));
    const colourBy = legend.colourBy;
    const offered =
      colourBy === null ||
      (colourBy.startsWith(CLUSTER_PREFIX)
        ? colourLayers(m.layers).some((l) => CLUSTER_PREFIX + l.name === colourBy)
        : m.declaredScalars.some((c) => c.name === colourBy));
    if (!offered) legend.setColourBy(null);
    const sizeBy = legend.sizeBy;
    if (sizeBy !== null && !m.declaredScalars.some((c) => c.name === sizeBy)) legend.setSizeBy(null);
  }

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
   * `status.stale`: the content key the replica last observed differs from the one the presented
   * marks were derived under. A revalidation or an artifact response can observe a new key without
   * a redraw. Not built yet: the counts from a revalidation are not published, so the numbers stay
   * as drawn until the next derive.
   */
  function recomputeStale(): void {
    const observed = views.current?.replica.currentContentKey ?? '';
    const stale = contentKeyAtFrame !== '' && observed !== '' && observed !== contentKeyAtFrame;
    if (stale !== projections.status.stale) {
      replaceProjection('status', {...projections.status, stale, sessionWarm: true});
    }
  }

  function onTrace(kind: string, fields: Record<string, number | string>): void {
    // A revalidation observed a content key without redrawing the marks.
    if (kind === 'revalidate') {
      recomputeStale();
      observeArtifactRotation();
    }
    options.instruments?.onTrace?.(kind, fields);
  }

  /**
   * Tell the artifact channel the content key the point path observed. A channel answering from
   * scopes it holds whole makes no request, so this is how it learns that the key moved.
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
    // A derive (`p.fetched` set) redrew the marks under the current content key; a fold did not.
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
      highlighted += tile.counts.highlighted;
      // Marks on screen: a tile counted before its points draws none.
      served += tile.drawn;
    }

    // A frame with marks means the session answered, while the request may still be streaming.
    if (!projections.status.sessionWarm && frame.exactDrawn + frame.provisional > 0) replaceProjection('status', {...projections.status, sessionWarm: true});
    // A switch published `loading`; a frame drawn from the view's own bands, with no request
    // behind it, ends that.
    if (awaitingSwitchFrame) {
      awaitingSwitchFrame = false;
      if (projections.status.status === 'loading') replaceProjection('status', {...projections.status, status: 'shown'});
    }
    replaceProjection('view', {
      id: views.id,
      composition: frame,
      depth: frame.depth,
      visible: {value: Number(visible), exact: true},
      matched: {value: Number(matched), exact: true},
      highlighted: {value: Number(highlighted), exact: true},
      highlighting: requestHighlight() !== null,
      served: {shown: served, total: Number(visible), exact: true},
      provisional: frame.provisional,
      inView: projections.view.inView && {...projections.view.inView, shown: shownInArea(frame.exact)}
    });
    replaceProjection('marks', {
      bands: frame.exact,
      standIn: frame.standIn,
      count: {shown: frame.exactDrawn, total: Number(visible), exact: true}
    });
    replaceProjection('tiles', {tiles: frame.tiles});
    region.answer({
      marks: {bands: frame.exact, standIn: frame.standIn},
      depth: frame.depth,
      verdict: replica?.lastRegionVerdict ?? null,
      fetched: p.fetched !== null,
      matched: Number(matched),
      narrowed: filtersBesideRegion(projections.filters.draft) !== null
    });
    const counted = projections.view.inView;
    if (counted && region.selected && counted.shown !== shownInArea(frame.exact)) {
      replaceProjection('view', {...projections.view, inView: {...counted, shown: shownInArea(frame.exact)}});
    }
    legend.accumulate(frame, meta?.declaredScalars ?? []);
    colours.refresh();
    checkPointColumns();
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

  /** `bytes` and `views` count every view's bands; `points` and `bands` the current view's. */
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
    // The channel is asked for the colour layer as well as the drawn ones. Only the drawn layers'
    // rows reach `served`; the colour layer's reach `colourServed`.
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
      attached: attachedTextOf(state.artifacts, meta?.layers ?? []),
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
   * Publish the colour coverage of the bands in view, and fetch the colour-stale ones again, once
   * per served-set version. A band is colour-stale when it has no membership column for a layer a
   * point request names ({@link pointLayers}), or names an ordinal that resolves to no colour. It
   * resolves against the colours, which cover every artifact the table holds, so a band fetched
   * under a coarser cut is still coloured. In view is the visible box: the point path also fetches a margin, whose bands name
   * artifacts the channel did not serve for this view.
   */
  function checkColourCoverage(): void {
    const a = projections.artifacts;
    const layers = pointLayers();
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
    const staleBands: Band[] = [];
    let current = 0;
    // The tags in view, so those no held tile carries are read by identifier.
    const tags = new Set<number>();
    for (const band of bands) {
      if (visible && (band.depth !== depth || !rectContainsTile(visible, band.x, band.y))) continue;
      for (const layer of layers) for (const ordinal of band.membership[layer]?.distinct ?? []) tags.add(ordinal);
      if (layers.every((layer) => resolves(band, layer, a.colours))) current++;
      else staleBands.push(band);
    }
    if (tags.size > 0) void machinery.channel.lookUp(tags);
    const stale = staleBands.length;
    // While a switch settles nothing is asked for or recorded: asking reschedules the driver, which
    // would request a view the slider is passing through.
    const toAsk = views.settling ? [] : staleBands.filter((b) => colourAsked.get(bandKey(b.depth, b.prefix)) !== a.version);
    for (const b of toAsk) colourAsked.set(bandKey(b.depth, b.prefix), a.version);
    if (toAsk.length > 0) {
      machinery.replica.retract(toAsk);
      machinery.presenter.reschedule();
    }
    options.instruments?.onTrace?.('coverage', {ms: clock.now() - started, bands: bands.length, stale, asked: toAsk.length});
    if (a.coverage.current !== current || a.coverage.stale !== stale) {
      replaceProjection('artifacts', {...projections.artifacts, coverage: {current, stale}});
    }
  }

  /**
   * The render columns of `view` a point request names, in the order `meta` lists them: the colour
   * column, the size column and those {@link setPointColumns} asked for. A name that is not a
   * render column of the view is left out.
   */
  function pointColumns(view: string): string[] {
    const wanted = new Set<string>();
    const colourBy = legend.colourBy;
    if (colourBy !== null && !colourBy.startsWith(CLUSTER_PREFIX)) wanted.add(colourBy);
    if (legend.sizeBy !== null) wanted.add(legend.sizeBy);
    for (const names of pointColumnAsks.values()) for (const name of names) wanted.add(name);
    if (!meta || wanted.size === 0) return [];
    const rendered = [
      ...meta.declaredScalars.filter((c) => c.render).map((c) => c.name),
      ...meta.scopedScalars.filter((c) => c.render && c.views.includes(view)).map((c) => c.name)
    ];
    return rendered.filter((name) => wanted.has(name));
  }

  /**
   * Fetch again every band the current view holds, in view or not, that lacks a column
   * {@link pointColumns} names and its request did not, so panning does not reveal a band without
   * it. A band asked for every column, or for this one, is not fetched again for lacking it: the
   * server served what there is. A band stays drawn until its replacement arrives, and is withdrawn
   * again if the piece it came in marks it covered after. A band with no points has nothing to
   * carry.
   */
  function checkPointColumns(): void {
    const machinery = views.current;
    // As in the colour coverage, asking while a switch settles would request a passing view.
    if (!machinery || views.settling) return;
    const replica = machinery.replica;
    const columns = pointColumns(views.id);
    const checked = {replica, version: replica.version, columns: `${views.id}\0${columns.join('\0')}`};
    const last = pointColumnsChecked;
    if (last && last.replica === checked.replica && last.version === checked.version && last.columns === checked.columns) return;
    pointColumnsChecked = checked;
    const unasked = (b: Band, c: string) => !(c in b.scalars) && b.columnsAsked != null && !b.columnsAsked.includes(c);
    const lacking = replica
      .heldBands()
      .filter((b) => b.ids.length > 0 && columns.some((c) => unasked(b, c)));
    if (replica.retract(lacking) > 0) machinery.presenter.reschedule();
  }

  function resolves(band: Band, layer: string, colourMap: ReadonlyMap<number, Rgba>): boolean {
    const m = band.membership[layer];
    // A layer the band's request named and the band carries no column for tagged none of its
    // points: absent is all null, not unasked.
    if (!m) return band.layersAsked === 'all' || (band.layersAsked?.includes(layer) ?? false);
    for (let i = 0; i < m.distinct.length; i++) {
      if (table.resolve(m.distinct[i]!, colourMap) === 0) return false;
    }
    return true;
  }

  /**
   * The camera as the driver and the channel take it: the host's zoom, and the centre whose canvas,
   * clamped to the world, is `input.bbox`. A box narrower than the canvas at that zoom was clamped
   * on the side it meets the world's edge, so the centre is half a canvas in from its other side.
   */
  function toDriverView(input: ViewInput): DriverViewState & {width: number; height: number} {
    const q = frame();
    const [wx0, wy0] = dataToWorldXY(input.bbox[0], input.bbox[1], q);
    const [wx1, wy1] = dataToWorldXY(input.bbox[2], input.bbox[3], q);
    const zoom = Math.min(MAX_DEPTH, input.zoom);
    const scale = 2 ** zoom;
    return {
      target: [cameraCentre(Math.min(wx0, wx1), Math.max(wx0, wx1), input.width / 2 / scale), cameraCentre(Math.min(wy0, wy1), Math.max(wy0, wy1), input.height / 2 / scale), 0],
      zoom,
      width: input.width,
      height: input.height
    };
  }

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
    if (id === views.id) return;
    if (!meta.views.some((v) => v.id === id)) {
      onTrace('view-switch', {refused: 1, id});
      return;
    }

    const from = views.id;
    const kept = sameFrame(quantisationOf(from), quantisationOf(id));
    // A suggestion page answers one view.
    suggestions.reset();

    // Current before anything below publishes, so a subscriber's `setView` reaches the incoming view.
    const incoming = views.enter(id);
    // A `setView` from a subscriber clears this, and its request then answers for the status.
    awaitingSwitchFrame = true;
    // The shared settings reach a view as it becomes current. Set on a held view, they would make it ask.
    incoming.channel.setLayers(layersAsked());
    incoming.presenter.setBudget(budget);
    // The content key, the bands asked for again and the shapes were the outgoing view's.
    contentKeyAtFrame = '';
    colourAsked.clear();
    shapes.forget('all');

    if (!kept) {
      // The camera and the selection are in the outgoing frame's data coordinates.
      lastView = null;
      region.drop();
    }

    // No marks until the incoming view presents, and the incoming channel's artifacts.
    replaceProjection('view', noFrame(id));
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

    aggregates.refresh(true);
    askInView(false, true);
    onTrace('view-switch', {from, to: id, sameFrame: kept ? 1 : 0});
  }

  function setView(input: ViewInput): void {
    lastView = {input};
    // From here the request's own status transitions answer for the status.
    awaitingSwitchFrame = false;
    const current = views.current;
    if (!meta || !current) {
      queuedView = input;
      return;
    }
    const v = toDriverView(input);
    current.presenter.schedule({target: v.target, zoom: v.zoom}, v.width, v.height);
    current.channel.schedule({target: v.target, zoom: v.zoom}, v.width, v.height);
    // A selected region is counted as it is wherever the camera goes.
    if (!region.selected) askInView(true);
  }

  /**
   * The layers a viewport request may name: all but the filter layers, which declare
   * `computed = []` and are applied only as `member_of` clauses. Filtered here so that a host
   * calling `setLayers` directly cannot draw one.
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
   * The layers the artifact channel asks for: the drawn layers and the colour layer, each with its
   * closure, so the colour layer's labels name the legend's rows.
   */
  function layersAsked(): string[] {
    const coloured = colourLayer();
    if (coloured === null || layersOn.includes(coloured) || !meta) return layersOn;
    return [...layersOn, ...drawnOnly(layerClosure(meta.layers, [coloured])).filter((l) => !layersOn.includes(l))];
  }

  /**
   * The layers a point request names, each putting a membership column on every band: those
   * {@link layersAsked} names, less the label layers (a layer that depends on another and declares
   * no geometry, as a clustering's topic labels), whose columns nothing reads. The first layer
   * drawn and the colour layer are always kept. A label layer's rows come from the channel.
   */
  function pointLayers(): string[] {
    const kept = new Set([layersOn[0], colourLayer()]);
    const labels = new Set(meta?.layers.filter((l) => l.depsOn.length > 0 && l.computedContent.length === 0 && l.shape === null && !kept.has(l.name)).map((l) => l.name) ?? []);
    return layersAsked().filter((l) => !labels.has(l));
  }

  /**
   * Point the channel at the layers asked for and publish the artifacts as they now stand. A layer
   * newly asked for is fetched at once, and the bands in view lacking its membership column are
   * refetched by the coverage check. A layer dropped costs nothing: its rows are filtered out.
   */
  function askLayers(): void {
    const current = views.current;
    if (!current) {
      // Before meta: published now, and asked for when the view is built.
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

  /** The filter-position controls of `draft` and the `member_of` clauses, without the region leaf. */
  function filtersBesideRegion(draft: FilterDraft): FilterExpr | null {
    return withMembers(composeFilters(draft, 'filter'), projections.filters.members, 'filter');
  }

  /** One composition for the point path, the artifact channel and the region's count. */
  function requestFilters(): FilterExpr | null {
    return withSelected(filtersBesideRegion(projections.filters.draft));
  }

  /** {@link requestFilters} without `column`'s filter-position control. */
  function filtersWithout(column: string): FilterExpr | null {
    return withSelected(filtersBesideRegion(withoutClause(projections.filters.draft, column, 'filter')));
  }

  /**
   * {@link requestFilters} less the filter-position control `spec.without` names and the
   * filter-position `member_of` clauses on the layer `spec.withoutMembersOf` names, and with
   * `spec.highlighted` the highlight joined to it.
   */
  function aggregateFilters(spec: AggregateSpec): FilterExpr | null {
    const {draft, members} = projections.filters;
    const kept = spec.without === undefined ? draft : withoutClause(draft, spec.without, 'filter');
    const layer = spec.withoutMembersOf;
    const clauses = layer === undefined ? members : members.filter((m) => m.verb !== 'filter' || m.layer !== layer);
    const filters = withSelected(withMembers(composeFilters(kept, 'filter'), clauses, 'filter'));
    const highlight = spec.highlighted === true ? requestHighlight() : null;
    if (highlight === null) return filters;
    return filters === null ? highlight : {all_of: [filters, highlight]};
  }

  /**
   * The area the counts in view are taken over: the selected region, else the camera's box. `null`
   * before the camera has been set. The selected region is already in the filters; the box is not.
   */
  function countedArea(): {operand: RegionOperand; outside: boolean; selected: boolean} | null {
    const selected = region.selected;
    if (selected) return {operand: regionOperand(selected), outside: selected.outside ?? false, selected: true};
    if (!lastView) return null;
    return {operand: regionOperand({kind: 'box', bbox: lastView.input.bbox}), outside: false, selected: false};
  }

  /** `expr` joined with the area's leaf, which the selected region's filters already carry. */
  function withArea(expr: FilterExpr | null, area: {operand: RegionOperand; outside: boolean; selected: boolean}): FilterExpr | null {
    if (area.selected && expr !== null) return expr;
    return withRegion(expr, area.operand, area.outside);
  }

  /** Ask for the counts in view again, now or once the camera has rested. */
  function askInView(rest: boolean, drop = false): void {
    if (inViewTimer !== null) clock.cancel(inViewTimer);
    inViewTimer = null;
    if (countedArea() === null) return;
    if (!rest) {
      const highlighting = requestHighlight() !== null;
      if (highlighting !== inViewHighlighted) {
        inViewHighlighted = highlighting;
        inView.set('highlighted', highlighting ? {groupings: [{}], highlighted: true} : null);
      }
      if (!inViewRegistered) {
        inViewRegistered = true;
        inView.set('counts', {groupings: [{}]});
        return;
      }
      inView.refresh(drop);
      return;
    }
    inViewTimer = clock.after(IN_VIEW_REST_MS, () => {
      inViewTimer = null;
      askInView(false);
    });
  }

  function publishInView(entries: AggregatesProjection): void {
    const counts = entries.get('counts');
    const lit = entries.get('highlighted');
    const total = counts?.result?.tables[0];
    if (counts?.status === 'refused') {
      // The last counts stand, marked refused; with none, the frame's own figures.
      const v = projections.view;
      const last = v.inView ?? {visible: v.visible, matched: v.matched, highlighted: v.highlighted, shown: v.served.shown};
      replaceProjection('view', {...v, inView: {...last, status: 'refused'}});
      return;
    }
    if (!counts || !total || counts.view !== views.id) {
      if (projections.view.inView !== null && (!counts || counts.result === null)) replaceProjection('view', {...projections.view, inView: null});
      return;
    }
    countedBox = counts.result ? (boxOf.get(counts.result) ?? null) : null;
    const exact = (counts.result?.region?.exact ?? true) && !(region.selected?.outside ?? false);
    const masked = (value: number) => ({value, exact});
    const visible = masked(total.referenceTotal ?? total.total);
    const matched = masked(total.total);
    const litTotal = lit?.result?.tables[0]?.total;
    const highlighted = litTotal === undefined ? matched : masked(litTotal);
    const loading = counts.status === 'loading' || counts.status === 'retrying' || lit?.status === 'loading' || lit?.status === 'retrying';
    const status = lit?.status === 'refused' ? 'refused' : loading ? 'loading' : 'shown';
    replaceProjection('view', {...projections.view, inView: {status, visible, matched, highlighted, shown: shownInArea(projections.marks.bands)}});
    if (status === 'shown' && region.selected) region.counted({visible, matched, verdict: counts.result?.region ?? null});
  }

  /** The held marks of `bands` inside the area, by the server's predicate; the region's own count while one is selected. */
  function shownInArea(bands: Composition['exact']): number {
    const q = frameOrNull();
    if (!q) return 0;
    if (region.selected) return projections.region?.held.count ?? 0;
    const counted = countedBox ?? lastView?.input.bbox;
    if (!counted) return 0;
    const [bx0, by0, bx1, by1] = counted;
    const [wx0, wy0] = dataToWorldXY(bx0, by0, q);
    const [wx1, wy1] = dataToWorldXY(bx1, by1, q);
    const box: [number, number, number, number] = [Math.min(wx0, wx1), Math.min(wy0, wy1), Math.max(wx0, wx1), Math.max(wy0, wy1)];
    let shown = 0;
    for (const band of bands) {
      const span = WORLD_SIZE / 2 ** band.depth;
      const x0 = band.x * span;
      const y0 = band.y * span;
      if (x0 >= box[0] && y0 >= box[1] && x0 + span <= box[2] && y0 + span <= box[3]) {
        shown += band.ids.length;
        continue;
      }
      if (x0 > box[2] || y0 > box[3] || x0 + span < box[0] || y0 + span < box[1]) continue;
      for (let i = 0; i < band.ids.length; i++) if (insideBox(band.positions[i * 2]!, band.positions[i * 2 + 1]!, box)) shown += 1;
    }
    return shown;
  }

  /** `expr` with the selected region's leaf joined. */
  function withSelected(expr: FilterExpr | null): FilterExpr | null {
    const selected = region.selected;
    return withRegion(expr, selected ? regionOperand(selected) : null, selected?.outside ?? false);
  }

  /**
   * The `highlight` every request carries: the controls and `member_of` clauses in that position.
   * The region is never here, because its numbers are read off the frame's `matched`.
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
    // Each suggestion count is taken under the filter.
    suggestions.refresh();
    aggregates.refresh(false);
    askInView(false);
  }

  async function browse(req: Omit<BrowseRequest, 'filters' | 'view'> & {filters?: FilterExpr | null; view?: string}): Promise<BrowsePage> {
    const asked = await viewed();
    // A caller's `filters: null` asks for the unfiltered counts.
    const filters = 'filters' in req ? (req.filters ?? null) : requestFilters();
    // Not a spread alone: a caller's `view: undefined` would replace the store's view.
    return client.browse(asked.token, {...req, view: req.view ?? asked.view, filters});
  }

  function setMembers(clauses: readonly MemberClause[]): void {
    const members = [...clauses];
    replaceProjection('filters', {...projections.filters, members});
    region.loading(projections.marks);
    requery();
  }

  function setFilters(draft: FilterDraft): void {
    filtersSet = true;
    const expr = composeFilters(draft, 'filter');
    replaceProjection('filters', {...projections.filters, draft, expr, highlight: composeFilters(draft, 'highlight')});
    region.loading(projections.marks);
    requery();
  }

  function setLayers(names: string[]): void {
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
    if (colourLayer() !== before) askLayers();
    checkPointColumns();
    if (column && projections.view.composition) legend.accumulate(projections.view.composition, meta?.declaredScalars ?? []);
  }

  function setSizeBy(column: string | null, options: {rank?: boolean} = {}): void {
    legend.setSizeBy(column, options.rank ?? false);
    checkPointColumns();
    if (column && projections.view.composition) legend.accumulate(projections.view.composition, meta?.declaredScalars ?? []);
  }

  function setPointColumns(id: string, columns: readonly string[]): void {
    if (columns.length === 0) pointColumnAsks.delete(id);
    else pointColumnAsks.set(id, [...columns]);
    checkPointColumns();
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
    // The box is in the wire's 32-bit grid units.
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

  /**
   * How many times the store has been cleared: an item or artifact asked for before a clear is not
   * shown, and a warm-up begun before one reads nothing.
   */
  let clears = 0;
  /** Whether the host has called `setFilters`. Until it has, the draft is seeded from `meta`. */
  let filtersSet = false;

  function clear(): void {
    tokens.forget();
    region.drop();
    forgetAnswers();
  }

  /**
   * Drop what the server answered: points, frames, counts, artifacts, shapes, records, legend
   * values, typeahead pages, the picked item and opened artifact, the `member_of` clauses, a region
   * drawn from an artifact, the region's held marks and counts, and `meta`. The store reads `meta`
   * again under the token it holds and asks again for the camera, the region's counts with it. What
   * is kept is listed on {@link Store}.
   */
  function forgetAnswers(): void {
    clears += 1;
    unconfirmed = null;
    suggestions.reset();
    for (const held of views.all()) {
      held.presenter.cancel();
      held.channel.reset();
      held.replica.reset();
    }
    // Each view is built again from the meta read next.
    views.forget();
    bandBudget = new BandBudget(bandBudget.budgetBytes);
    identities.clear();
    table.clear();
    colours.refresh();
    shapes.forget('all');
    records.forget();
    clearSelection();
    contentKeyAtFrame = '';
    colourAsked.clear();
    awaitingSwitchFrame = false;
    replaceProjection('view', noFrame(views.id));
    replaceProjection('marks', {...projections.marks, bands: [], standIn: [], count: NO_COUNT});
    replaceProjection('tiles', {tiles: []});
    // An artifact's id may name nothing under the next key. A drawn shape is placed against the
    // view's frame, so it is published before `meta` goes.
    if (region.selected?.kind === 'artifact') region.drop();
    else region.loading(projections.marks);
    // A clause names an artifact by an id the next key may not reach, and carries its served label.
    // A seeded draft names the previous viewer's scoped columns.
    replaceProjection('filters', {
      ...projections.filters,
      members: [],
      ...(filtersSet ? {} : {draft: emptyDraft([]), expr: null, highlight: null})
    });
    legend.clear();
    meta = null;
    warming = null;
    replaceProjection('meta', null);
    replaceProjection('status', {...NO_STATUS});
    publishReplica(null);
    void ready().catch(() => {});
    // Each asks under the meta and token the read above brings.
    aggregates.refresh(true);
    askInView(false, true);
  }

  function refresh(): void {
    // Redraw against the current content key, refetching the artifacts with it.
    views.current?.channel.reset();
    region.loading(projections.marks);
    if (lastView) setView(lastView.input);
    aggregates.refresh(false);
    askInView(false);
  }

  /** Thrown into an answer {@link admit} refused, so nothing in it is held. */
  function identityChanged(): Error {
    return new DOMException('the identity key changed; the store asks again', 'AbortError');
  }

  /** Set by `dispose`: an answer that lands afterwards writes nothing. */
  let disposed = false;

  function dispose(): void {
    disposed = true;
    tokens.dispose();
    suggestions.dispose();
    shapes.dispose();
    records.dispose();
    legend.dispose();
    aggregates.dispose();
    inView.dispose();
    if (inViewTimer !== null) clock.cancel(inViewTimer);
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
    setAggregate: (id, spec) => aggregates.set(id, spec),
    setFilters,
    setMembers,
    suggest: (column, q, verb) => suggestions.suggest(column, q, verb),
    forgetSuggestions: (column) => suggestions.forget(column),
    setLayers,
    setColourBy,
    setSizeBy,
    setPointColumns,
    setPalette: (kind) => colours.setPalette(kind),
    setBudget,
    get budget() {
      return budget;
    },
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
