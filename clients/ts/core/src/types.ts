import type {Table} from 'apache-arrow';

/**
 * A viewer session, as {@link TesseraClient.authorise} returns it.
 *
 * @category Requests and responses
 */
export type Session = {
  /** The bearer token every viewer route takes. It is a credential. */
  token: string;
  /** The session's handle for {@link TesseraClient.revoke}. It is not a credential. */
  tokenId: number;
  /** When the token expires, in seconds since the Unix epoch: the time of the request plus the server's `token_max_lifetime_secs`. */
  expiresAt: number;
};

/**
 * The extent of a view's data coordinates. Each axis maps onto 32-bit grid units: `0` at the
 * minimum, the top of the grid at the maximum, which is inclusive.
 *
 * @category Meta
 */
export type Quantisation = {
  /** The smallest x, in data coordinates. */
  xMin: number;
  /** The largest x, in data coordinates. */
  xMax: number;
  /** The smallest y, in data coordinates. */
  yMin: number;
  /** The largest y, in data coordinates. */
  yMax: number;
};

/**
 * The storage type `/v1/meta` publishes for a declared column. `timestamp_us` is an `i64` of
 * microseconds since the Unix epoch. `keyword` is a short string matched exactly, and `text` is
 * prose searched by analysed words. Neither string type can be `render`, so neither arrives in a
 * viewport response; both are read with {@link TesseraClient.item}. `utf8` cannot be declared.
 *
 * @category Meta
 */
export type ArrowType =
  | 'bool'
  | 'u8'
  | 'u16'
  | 'u32'
  | 'u64'
  | 'i8'
  | 'i16'
  | 'i32'
  | 'i64'
  | 'f32'
  | 'f64'
  | 'timestamp_us'
  | 'utf8'
  | 'keyword'
  | 'text';

/**
 * What makes an integer column a category. The points frame carries a category as a bare integer
 * code of the declared width, so this block is the only sign that a column is one. The codes are
 * drawn at random from the width and their numeric order means nothing, so a continuous colour
 * ramp over them is meaningless. {@link TesseraClient.categories} resolves a code to its value.
 *
 * @category Meta
 */
export type CategoryDescriptor = {
  /**
   * The name of the value set the codes index. Columns sharing a vocabulary share its keys, codes
   * and titles, so one palette serves them all. Which values are visible is per column, so a value
   * visible under one column may be hidden under another that shares the vocabulary.
   */
  vocabulary: string;
  /**
   * `declared`: the value set is closed and a write naming an unknown key is refused.
   * `discovered`: a new key gets a code when it is first written.
   */
  kind: 'declared' | 'discovered';
  /**
   * `public`: every session is offered the whole value set. `derived`: `/v1/categories` offers a
   * value only where this principal can see at least one item carrying it.
   */
  visibility: 'derived' | 'public';
};

/**
 * One declared per-item column.
 *
 * @category Meta
 */
export type DeclaredScalar = {
  /**
   * The column's name, unique in the bundle. Filters, `/v1/categories/{column}` and
   * `ViewportResult.scalars` all address the column by it.
   */
  name: string;
  /** The column's storage type. */
  arrowType: ArrowType;
  /** Present for a category column; `null` for any other. */
  category: CategoryDescriptor | null;
  /**
   * Whether the column arrives with each point in a viewport response. Only a `render` column
   * can colour or size points. A point with no value arrives as null, except in a category, where
   * it arrives as code `0`. A column with `render: false` may still be filterable (see
   * {@link Meta.filterOperands}) and is returned by {@link TesseraClient.item}.
   */
  render: boolean;
  /**
   * Whether the column has a filter index. A column with neither `render`, `index` nor `unique`
   * can be read on an item and cannot be filtered on.
   */
  index: boolean;
  /**
   * Whether no two items hold one value. `eq` and `in` on the column answer from its unique index,
   * so a filter `{column: {in: values}}` finds the items holding the values; a unique column with
   * neither `render` nor `index` takes those two operators alone.
   */
  unique: boolean;
  /**
   * For a `text` column, the `<name>/<version>` of the analyser that split its values into terms;
   * `null` for every other type.
   */
  analyser: string | null;
  /**
   * Where `POST /v1/items` reads the value from: `rendered`, the view's point columns;
   * `value_column`, a per-item column; `record`, the record store. A column whose only home is
   * `record` reads fastest in `stored` order.
   */
  homes: ('rendered' | 'value_column' | 'record')[];
};

/**
 * One group-scoped column family: one column per view of the group `scope` names, each with a
 * value per item. Absent from `/v1/meta` when this principal cannot reach the group.
 *
 * @category Meta
 */
export type ScopedScalar = {
  /** The family's name, unique in the bundle. A filter names it, pinned to a view where needed (see {@link FilterOperandSet.scope}). */
  name: string;
  /** The storage type, as on a {@link DeclaredScalar}. */
  arrowType: ArrowType;
  /** The view group whose views each have a column of this family. */
  scope: {group: string};
  /** Present for a category family; `null` for any other. */
  category: CategoryDescriptor | null;
  /** For a `text` family, the `<name>/<version>` of its analyser; `null` for every other type. */
  analyser: string | null;
  /**
   * Whether the column arrives with each point in a viewport response under one of {@link views}.
   * Under any other view it does not arrive.
   */
  render: boolean;
  /** Whether a filter can be answered from the family's columns. */
  index: boolean;
  /**
   * The ids of the views whose points carry this column and that this principal may reach: the
   * group's own views, and the same keys under each group that shares them through `members`. A
   * view created while the service runs is not listed until a flush has written its values.
   */
  views: string[];
};

/**
 * One column a request may filter on, and how. `family` says which control fits: a `category` has
 * a value set {@link TesseraClient.categories} lists, a `keyword` has none and takes typed text, a
 * `text` column is searched by words, and a `numeric` one by range.
 *
 * @category Meta
 */
export type FilterOperandSet = {
  /** The column or family name a filter leaf uses. */
  column: string;
  /** The column's filter family, which decides its operators. */
  family: 'category' | 'keyword' | 'text' | 'numeric';
  /**
   * The operators this column accepts: `eq` and `in` for a category; `eq`, `in`, `prefix` and
   * `contains` for a keyword; `eq`, `in` and `range` for a numeric column; `match` and `phrase`
   * for text; `eq` and `in` alone for a unique column with neither `render` nor `index`.
   */
  operands: string[];
  /**
   * Present only on a group-scoped column: the view group whose views each hold their own value.
   * Absent for a column with one value per item under every view.
   *
   * Under a view of that group, or of a group sharing its views, the leaf names the column bare
   * and the request's own view decides which value it reads. Under any other view the leaf pins a
   * view as `column@key`. An unpinned leaf there is a `422`, and a pin naming no view of the group
   * is the `404` an unknown view gets.
   */
  scope?: {group: string};
};

/**
 * A filter expression, as `/v1/viewport` and `/v1/artifacts/browse` take it.
 *
 * Each node has exactly one key: a column name mapped to a {@link FilterOperator}, `region`,
 * `member_of`, or one of the combinators `all_of`, `any_of` and `none_of`. `all_of: []` matches
 * every item and `any_of: []` matches none. The server refuses with a `422` a node with two keys,
 * an unknown column, an operator outside the column's family, `none_of: []`, and a `none_of` whose
 * branches name a `text` column or two different columns. Nesting deeper than the deployment
 * allows is refused.
 *
 * A value that does not exist and a value this principal cannot see both match nothing, with the
 * same status, body and counts. Present both as "no matches".
 *
 * @category Filters
 */
export type FilterExpr =
  | {all_of: FilterExpr[]}
  | {any_of: FilterExpr[]}
  | {none_of: FilterExpr[]}
  | {region: RegionOperand}
  | {member_of: MemberOfOperand}
  | {[column: string]: FilterOperator};

/**
 * The `region` leaf: the items whose stored position lies inside a shape. Give exactly one of:
 *
 * - `polygon`: at least three `[x, y]` vertices, closed implicitly. More than
 *   `selection.maxRegionVertices` is a `422`.
 * - `bbox`: `[x0, y0, x1, y1]`, closed on every side.
 * - `circle`: `[cx, cy, r]`.
 * - `ellipse`: `[cx, cy, a, b, angle_degrees]`.
 * - `artifact`: a published artifact's `tessera_id` as a decimal string, matching its members.
 *   An id naming nothing, an artifact this principal would not be served, one on another view and
 *   one whose layer draws an authored shape all match nothing.
 *
 * Coordinates are in the view's data coordinates (`space: 'view'`, the default) and are
 * quantised to the view's grid before any point is tested. A shape with a long perimeter may be
 * answered for a cover of it; the response's `region` says so ({@link RegionVerdict}). `region`
 * is a reserved column name.
 *
 * @category Filters
 */
export type RegionOperand =
  | {polygon: [number, number][]; space?: 'view'}
  | {bbox: [number, number, number, number]; space?: 'view'}
  | {circle: [number, number, number]; space?: 'view'}
  | {ellipse: [number, number, number, number, number]; space?: 'view'}
  | {artifact: string};

/**
 * The `member_of` leaf: the members of one artifact of one layer that this principal may see.
 * It works the same in `filters` and in `highlight`, so one clause can narrow the map to a cluster
 * or light it. {@link memberOf} builds one. `member_of` is a reserved column name.
 *
 * A layer this principal's `/v1/meta` does not list is a `422`. An artifact is never refused: an
 * id naming nothing, an artifact of another layer, a suppressed one and one this principal is not
 * served all match nothing, so the leaf does not reveal whether an artifact exists.
 *
 * @category Filters
 */
export type MemberOfOperand = {
  /** The layer's name, as `/v1/meta` lists it. */
  layer: string;
  /**
   * The artifact's `tessera_id` as a decimal string. A JSON number cannot hold every 64-bit id,
   * so a number here would lose the top of the range.
   */
  artifact: string;
};

/**
 * The `x-tessera-region` verdict on a response whose filter had a `region` leaf. `exact: true`
 * means each region's answer is exact for the shape against every point's stored position.
 * `exact: false` means a shape's boundary needed more than `selection.maxRegionCells` cells, so the
 * answer is exact for a cover of the shape (a superset) taken at depth `depth`. The verdict
 * depends on the shape and the grid alone, never on the items. With several region leaves it is
 * the coarsest.
 *
 * @category Filters
 */
export type RegionVerdict = {exact: true; depth: null} | {exact: false; depth: number};

/**
 * One column's predicate, with exactly one key; the server refuses a leaf with two. A category
 * takes its vocabulary key (a string) or its code (a number). A keyword takes exact strings. A
 * numeric column takes numbers, and `range` takes at least one bound and at most one per side.
 * An integer or timestamp column also takes a `bigint`, sent as its decimal digits, which is exact
 * past 2^53 where a `number` is not. A `text` column takes `match` or `phrase`.
 *
 * @category Filters
 */
export type FilterOperator =
  | {eq: string | number | boolean | bigint}
  | {in: (string | number | bigint)[]}
  | {prefix: string}
  | {contains: string}
  /** Every analysed term must appear, unless `minimum_should_match` says how many must. */
  | {match: string | {query: string; minimum_should_match?: number}}
  /** The analysed terms must appear next to each other, in order. */
  | {phrase: string}
  | {range: {gte?: number | bigint; gt?: number | bigint; lte?: number | bigint; lt?: number | bigint}};

/**
 * One annotation layer, as `/v1/meta` publishes it to this principal.
 *
 * A layer this principal cannot reach is absent, as a layer nobody registered is. The entry does
 * not say how many artifacts the layer holds, since that would count artifacts this principal may
 * not see, and it does not carry the layer's access label.
 *
 * @category Meta
 */
export type Layer = {
  /** The layer's name, which a viewport request's `layers` names. */
  name: string;
  /** The layer's display title; `null` where the declaration gave none. */
  title: string | null;
  /** The ids of the views the layer appears in that this principal may reach. The layer is served in no other view. */
  views: string[];
  /**
   * Where the layer's artifacts get their members: `enumerated`, a stored set per artifact;
   * `spatial`, the items inside each artifact's shape; or, for an attribute layer, the items
   * holding a value of one column. An attribute layer arrives as `{attribute: <column>}`, which
   * this type does not declare.
   */
  membership: 'enumerated' | 'spatial' | 'attribute';
  /**
   * `kind` is where the layer's lineage lives: `flat` has none; `nested` is a tree held in parent
   * links, with no levels; `dag` is `nested` where a child may have several parents; `stacked` is
   * independent analyses, one per level, with no links between them; `tiered` is levels whose
   * artifacts sit inside those of the level above, linked. `pruneChildren` is the default cut:
   * `true` serves only the deepest artifact per branch where a parent and its child both qualify.
   * A request's `artifactBudget` may ask for more detail.
   */
  hierarchy: {kind: 'flat' | 'nested' | 'dag' | 'stacked' | 'tiered'; pruneChildren: boolean};
  /**
   * The levels the layer declares, each with its number, its title (`null` where none) and `zoom`:
   * the `[min, max]` tile depths at which a viewport request naming no `levels` serves it, or
   * `null` for every depth. Empty for a `nested` or `dag` layer, which declares none.
   */
  levels: {level: number; title: string | null; zoom: [number, number] | null}[];
  /**
   * The computed properties the layer declares: any of `centroid`, `box` and `hull`. Each is
   * computed per request from the members this principal can see. A viewport request asks for a
   * hull as `shape` ({@link ComputedProperty}).
   */
  computedContent: string[];
  /**
   * The kind of the layer's one drawn outline, or `null` where it draws none. A `derived` shape is
   * the hull of the members this principal can see and differs between principals, so it must not
   * be kept against a `tesseraId` across a change of principal. A `predicate` shape (a spatial
   * layer's membership shape) and an `authored` one (supplied by the publisher) are the same for
   * every principal served the artifact and may be kept.
   */
  shape: ShapeKind | null;
  /** The types of publisher-supplied content each artifact carries, in the order of {@link Artifact.content}. */
  suppliedContent: string[];
  /** The layers whose artifacts this layer's artifacts attach to, as a clustering's labels attach to its clusters. Empty for most layers. */
  depsOn: string[];
  /** A number that changes when the layer's access rule is edited, so a client can notice the edit. */
  version: number;
};

/**
 * The map projection that placed a view's positions. See {@link ViewInfo.projection}.
 *
 * @category Meta
 */
export type MapProjection = 'web_mercator' | 'equirectangular' | 'gall_isographic' | 'none';

/**
 * The tile schemes a view's frame may be addressed in. There is one: `xyz`, the slippy-map
 * `z/x/y` scheme basemap servers publish. See {@link ViewInfo.tileScheme}.
 *
 * @category Meta
 */
export type TileScheme = 'xyz';

/**
 * One view this principal may reach, as `/v1/meta` publishes it. The frame and projection fields
 * are the same for every principal, and a host decides from them whether to draw a basemap
 * ({@link ViewInfo.tileScheme}) and how to turn a position into longitude and latitude
 * ({@link lonLatOfCell}).
 *
 * @category Meta
 */
export type ViewInfo = {
  /** The view's id, which every viewer request names: a plain view's name, or `<group>:<key>` for a view in a group. */
  id: string;
  /** The view's name for display. For a view in a group it is the id. */
  displayName: string;
  /**
   * The data extent this view's positions are quantised over. Each view has its own, so a
   * position is decoded against the extent of the view it came from.
   */
  quantisation: Quantisation;
  /**
   * What placed this view's positions: `web_mercator`, `equirectangular`, `gall_isographic`, or
   * `none` for a view that projects nothing.
   */
  projection: MapProjection;
  /**
   * The width-to-height ratio to draw the world at: `1` for `web_mercator`, `2cos φ₁` for
   * `equirectangular` and `gall_isographic` (φ₁ being the standard parallel), and `null` for
   * `none`, which has no world to draw.
   */
  worldAspect: number | null;
  /**
   * `xyz` where this view's frame lines up with the slippy-map tiling, so a tile basemap may be
   * drawn under the points. `null` means draw the points and no basemap. Only an aligned
   * `web_mercator` view has a scheme.
   */
  tileScheme: TileScheme | null;
  /** The tile of {@link tileScheme} this view's frame covers; `null` exactly when `tileScheme` is. */
  tile: {z: number; x: number; y: number} | null;
  /** The view's place in its group, or `null` for a view in no group. */
  roster: ViewRoster | null;
};

/**
 * A view's place in its group: the group, the view's key, and the per-view values the group
 * declared. The view's id is `<group>:<key>`. The order of a group's views is the order
 * {@link ViewGroup.views} lists them in, which is creation order; step through that list rather
 * than comparing keys, which are the caller's own strings.
 *
 * @category Meta
 */
export type ViewRoster = {
  /** The group's name. */
  group: string;
  /** The view's key within its group. */
  key: string;
  /**
   * One entry per metadata name the group declared. Empty on the views of a group declared with
   * `members`, whose metadata belongs to the group that owns the keys.
   */
  metadata: Record<string, ViewMetadataValue>;
};

/**
 * One per-view metadata value, typed as the group's declaration typed it. `timestamp_us` is
 * microseconds since the Unix epoch. A metadata value is one value per view; it is not an item
 * attribute and cannot be filtered on.
 *
 * @category Meta
 */
export type ViewMetadataValue =
  | {type: 'bool'; value: boolean}
  | {type: 'int'; value: number}
  | {type: 'float'; value: number}
  | {type: 'text'; value: string}
  | {type: 'timestamp_us'; value: number};

/**
 * One view group this principal may reach. A group cannot be named where a view is expected. Its
 * frame and projection are on each of its views.
 *
 * @category Meta
 */
export type ViewGroup = {
  /** The group's name. */
  name: string;
  /** The group's display title, or `null` where the deployment declared none. */
  title: string | null;
  /**
   * The group whose keys this group's views use, where this group is a second layout over another
   * group's views; `null` where it owns its keys.
   */
  membersOf: string | null;
  /** The ids of this group's views that this principal may reach, as `<group>:<key>`, in creation order. */
  views: string[];
};

/**
 * `GET /v1/meta`, as {@link TesseraClient.meta} decodes it: the views, column schema, filterable
 * columns, annotation layers and request limits. The lists are filtered to what this principal
 * may reach, so one principal's `Meta` must not be shown to another.
 *
 * @category Meta
 */
export type Meta = {
  /** The viewer API's version, `1`. */
  apiVersion: number;
  /** The bundle format version the server opened. */
  bundleFormat: number;
  /**
   * The views this principal may reach: the plain views, then each group's in creation order.
   * A view this principal cannot reach is absent, and naming it gets the `404` an unknown view
   * gets.
   */
  views: ViewInfo[];
  /** The view groups this principal may reach. Empty where there are none. */
  groups: ViewGroup[];
  /** Every entity-scoped column, in declaration order. */
  declaredScalars: DeclaredScalar[];
  /** The group-scoped column families this principal may reach. */
  scopedScalars: ScopedScalar[];
  /** The annotation layers this principal may reach. Empty when it reaches none. */
  layers: Layer[];
  /**
   * The deployment's point-selection parameters and request limits. They are the same for every
   * principal.
   */
  selection: {
    /** The fewest points a tile serves when it has that many to serve, whatever the density threshold says. */
    kMin: number;
    /** The most points one tile serves, and the default for a request's `k`. */
    kMaxMarks: number;
    /** The machine ceiling on `k`. A request is served at most `min(k, maxK, kMaxMarks)` points per tile. */
    maxK: number;
    /**
     * The number of points the density threshold aims for in the average occupied tile, at any
     * depth, on an unfiltered request. A filtered request serves every match up to the per-tile
     * cap.
     */
    thetaTargetMarks: number;
    /** The largest `underlayOffset` a viewport request may ask for. */
    maxUnderlayOffset: number;
    /**
     * The page size, and the largest one, of {@link TesseraClient.categories}. A page shorter than
     * this means the value set ended.
     */
    maxCategoryValues: number;
    /** The most vertices a `region` leaf's polygon may have; over it the request is a `422`. */
    maxRegionVertices: number;
    /**
     * The most boundary cells a `region` leaf may use at one depth. Over it the request is not
     * refused: the answer is exact for a cover of the shape, and {@link RegionVerdict} says so.
     */
    maxRegionCells: number;
    /** The page size, and the largest one, of {@link TesseraClient.browse}. */
    maxBrowseRows: number;
    /** The most vertices a published shape may have. */
    maxShapeVertices: number;
    /** The default and the largest `limit` of {@link TesseraClient.suggest}. */
    maxSuggestions: number;
    /** The most values one suggestion request examines before it stops and answers `more: true`. */
    maxSuggestionWalk: number;
    /**
     * The visible-set size at or below which a suggestion is answered from the values this
     * session can see. Above it, `more` may mean that `maxSuggestionWalk` ran out.
     */
    maxSuggestSetEntities: number;
    /** The most rows a `POST /v1/items` or `POST /v1/artifacts` page holds. */
    maxPageRows: number;
    /**
     * The most Arrow bytes a `POST /v1/items` or `POST /v1/artifacts` page holds before
     * compression. A single larger row is sent alone.
     */
    maxPageBytes: number;
    /** The most groupings one {@link TesseraClient.aggregate} request may carry. */
    maxAggregateGroupings: number;
    /** The largest `top` of an aggregate grouping. */
    maxAggregateTop: number;
    /** The most `values` or `artifacts` an aggregate grouping may name. */
    maxAggregateNamed: number;
    /** The most cells an aggregate grouping's cell level may list: the cells at its `depth` in its `area`. */
    maxAggregateCells: number;
  };
  /** The most tiles one viewport request may span; over it the request is a `422`. */
  maxTilesPerRequest: number;
  /**
   * The columns a request may filter on, with their operators. Empty when nothing is filterable.
   * Filter controls come from this list. It differs from {@link Meta.declaredScalars}: a
   * filterable column need not arrive with the points, and a declared column need not be
   * filterable.
   */
  filterOperands: FilterOperandSet[];
};

/**
 * One category value: what a code stands for, and how to show it.
 *
 * @category Requests and responses
 */
export type CategoryValue = {
  /** The code the points carry. Never `0`, which marks an absent value. */
  code: number;
  /** The value's key, which filters use. Show it where there is no title. */
  key: string;
  /** The value's display title, or `null` where none was written, as for every value a discovered vocabulary mints. */
  title: string | null;
};

/**
 * Where a suggestion matched, in the string the server served (`key` or `title`), so a client
 * can mark the match without repeating the server's case folding. `start` and `len` count Unicode
 * code points, which differ from JavaScript string indices for characters outside the Basic
 * Multilingual Plane; index `Array.from(text)` to be exact.
 *
 * @category Requests and responses
 */
export type MatchSpan = {
  /** The field that matched. A match at the start of a word reports the field the word is in; on a value with no title it is `key`. */
  field: 'key' | 'title';
  /** The code point the match starts at. */
  start: number;
  /** The match's length in code points. */
  len: number;
};

/**
 * One suggested category value, with where it matched and, on request, a count.
 *
 * @category Requests and responses
 */
export type SuggestValue = {
  /** The value's code. Never `0`. */
  code: number;
  /** The value's key. */
  key: string;
  /** The value's display title, or `null` where none was written. */
  title: string | null;
  /** Where the query matched. */
  match: MatchSpan;
  /**
   * How many items carrying this value this principal can see, computed exactly per request, in
   * the request's view where it named one and among the items passing its `filters` where it
   * sent them. Present only when the request set `counts: true`.
   */
  count?: number;
};

/**
 * One page of `/v1/categories/{column}/suggest`.
 *
 * @category Requests and responses
 */
export type SuggestPage = {
  /** The column, spelt as the caller sent it. */
  column: string;
  /** The query as received, before folding, so a caller can match a page to the request it answers. */
  q: string;
  /** The matching values this principal can see, ordered by the matched text. */
  values: SuggestValue[];
  /**
   * `true` when the server stopped before running out of matches: the page filled, or it examined
   * `selection.maxSuggestionWalk` values. There is no cursor; a longer query narrows the answer.
   */
  more: boolean;
  /**
   * The number of items the counts are taken over: those this principal can see, in the request's
   * view where it named one, passing its `filters` where it sent them. A value's share is `count /
   * total`. Present only when the request set `counts: true`.
   */
  total?: number;
  /** The verdict on the counts' `region` leaves (`x-tessera-region`), present only where the request's `filters` carried one and asked for counts. */
  region?: RegionVerdict;
};

/**
 * The outcome of {@link TesseraClient.suggest}. `status: 'ok'` carries the page.
 *
 * `status: 'shed'` is any `429` the server answered, returned rather than thrown: the
 * request was shed and may be sent again after `retryAfterS` seconds. `detail` is the server's
 * reason. Usually the session's previous suggestion is still running, since a session has one in
 * flight at a time. A request with `view` and `counts` can also be shed by compute admission, or
 * while another request builds the session's view of the map.
 *
 * @category Requests and responses
 */
export type SuggestResult =
  | ({status: 'ok'} & SuggestPage)
  | {status: 'shed'; retryAfterS: number; detail: string | null};

/**
 * The body of `POST /v1/viewport`, as {@link TesseraClient.viewport} takes it. Send exactly one of
 * `bbox` and `tiles`; the server refuses both and neither. A field left unset is not sent, so the
 * server's default applies.
 *
 * @category Requests and responses
 */
export type ViewportRequest = {
  /** A view id from {@link Meta.views}. An unknown view, or one this principal cannot reach, is a `404`. */
  view: string;
  /** The tile depth to answer at, `0` to `16`. */
  zoom: number;
  /**
   * The region to answer for, as `[x0, y0, x1, y1]` in the view's data coordinates, with
   * `x0 <= x1` and `y0 <= y1`. The response covers every depth-`zoom` tile the box touches, edges
   * included.
   */
  bbox?: [number, number, number, number];
  /**
   * The depth-`zoom` tiles to answer for, as Morton prefixes, in place of `bbox`. Answered in the
   * order given, with duplicates removed. A prefix with bits above `zoom` is refused. A tile left
   * out costs the server nothing, so a client that holds a tile leaves it out.
   */
  tiles?: bigint[];
  /**
   * The most points each tile serves. Defaults to `selection.kMaxMarks`, and is capped at the
   * smaller of `selection.maxK` and `selection.kMaxMarks`. `0` asks for counts and artifacts with
   * no points. Do not decrease it as the view zooms in, or points already drawn drop out.
   */
  k?: number;
  /**
   * Also serve exact visible counts per cell at depth `zoom + underlayOffset`, as
   * {@link ViewportResult.subCells}. Omitted or `0` serves none. Above
   * `selection.maxUnderlayOffset`, past depth 16, or over the deployment's cell budget, the
   * request is a `422`.
   */
  underlayOffset?: number;
  /**
   * The {@link ViewportResponse.pin} of the response on screen, sent back so the server can say
   * whether the data has moved since ({@link ViewportResponse.stale}). Omitted, every response
   * reports `stale: false`.
   */
  stamp?: string | null;
  /**
   * The filter, or `null` for none. It narrows which points are served and each tile's `matched`
   * count; `visible`, which artifacts are served and their `maskedCount` do not change. The
   * response's `identityKey` does not change with the filter either, so a client holding points
   * from one filter drops them itself when the filter changes.
   */
  filters?: FilterExpr | null;
  /**
   * A second expression in the grammar of `filters`, or `null` for none. It does not change which
   * points are served. It adds a `highlighted` count to each tile, a bit to each point and a bit to
   * each artifact, each answering `all_of[filters, highlight]`. Several highlights are one
   * expression under `any_of` or `all_of`.
   */
  highlight?: FilterExpr | null;
  /**
   * Which columns each served point carries. `'full'`, the default, is every column.
   * `'highlight'` is the `tessera_id` and the highlight bit only, for a client that changed only
   * its highlight and already holds the points. The points served, and each tile's `served`
   * count, are the same under either value. Once {@link ViewportResponse.stale} is `true`, ask
   * again with `'full'`. Without a `highlight` the request is answered as `'full'`.
   */
  pointRows?: 'full' | 'highlight';
  /**
   * Which annotation layers to serve artifacts for. Omitted or `[]` is none; `'all'` is every
   * layer this principal reaches; an array is the named layers this principal reaches. A name it
   * cannot reach is ignored, as an unknown name is. It also decides which membership columns the
   * points carry.
   */
  layers?: string[] | 'all';
  /**
   * Which declared levels of each named layer to serve. Omitted serves the levels whose `zoom`
   * range covers the request's `zoom`, or every level where no level declares a range. `'all'` is
   * every level, `[]` is none, and an array is those levels. A layer with no levels ignores it. A
   * level a layer does not hold is left out of the answer and is not refused.
   */
  levels?: number[] | 'all';
  /**
   * The most artifacts to serve. The server meets it by serving ancestors in place of their
   * descendants, and does not drop artifacts to meet it. It applies to the treed kinds (`nested`,
   * `dag`); a flat layer has no ancestors and ignores it. Unbounded when omitted.
   */
  artifactBudget?: number;
  /**
   * Which declared geometry each artifact carries: any of `'centroid'`, `'box'` and `'shape'`.
   * Omitted is each layer's declaration; an array is intersected with it, and `[]` is none. It
   * never adds a property a layer does not declare. A derived shape is computed per artifact per
   * request, so a client drawing one outline leaves `'shape'` out here and fetches that one with
   * {@link TesseraClient.artifact}.
   */
  computed?: ComputedProperty[];
  /**
   * Which columns each served artifact carries. `'full'`, the default, is every column.
   * `'identity'` serves the same artifacts as {@link ArtifactIdentity} rows in
   * {@link ViewportResult.artifactsIdentity}, for a client that already holds the rest. The
   * artifacts served, their `matched` and `highlighted` bits and their `rung` are the same under
   * either value.
   */
  artifactRows?: 'full' | 'identity';
};

/**
 * The geometries a viewport request may ask for. `shape` is the layer's one drawn outline, of
 * whichever kind {@link Layer.shape} names: a hull, a membership shape or an authored one.
 *
 * @category Meta
 */
export type ComputedProperty = 'centroid' | 'box' | 'shape';

/**
 * The kinds of a layer's drawn outline: `derived`, the hull of the members this principal can
 * see; `predicate`, a spatial layer's membership shape; `authored`, a shape the publisher
 * supplied. See {@link Layer.shape}.
 *
 * @category Meta
 */
export type ShapeKind = 'derived' | 'predicate' | 'authored';

/**
 * A served outline: parts, then rings, then `[x, y]` vertices, in 32-bit grid units. A part's
 * first ring is its outer boundary and the others are holes. Two parts are two separate shapes.
 * A derived hull has one part per separate group of visible members and no holes.
 *
 * The outline is simplified for the zoom it was asked at and may differ from the membership by up
 * to a cell. Do not test a point against it to decide membership; the points' membership column
 * ({@link ViewportResult.membership}) gives that answer.
 *
 * @category Requests and responses
 */
export type Shape = [number, number][][][];

/**
 * One tile's exact counts, computed over the items this principal may see. The `served` points
 * are a sample of `matched`. Show `served` beside `matched` or `visible`: alone, it reads as the
 * whole set.
 *
 * @category Requests and responses
 */
export type TileCounts = {
  /** The tile's Morton prefix at the request's `zoom`. */
  tile: bigint;
  /** How many items in the tile this principal may see. A filter does not change it. */
  visible: bigint;
  /** How many of `visible` the request's filter admits; equal to `visible` with no filter. */
  matched: bigint;
  /** How many points the response carries for this tile. */
  served: bigint;
  /**
   * How many of `matched` also satisfy the request's `highlight`; equal to `matched` when the
   * request carried none. It counts every matching item, drawn or not.
   */
  highlighted: bigint;
};

/**
 * One cell of the density underlay ({@link ViewportRequest.underlayOffset}).
 *
 * @category Requests and responses
 */
export type SubCell = {
  /** The cell's Morton prefix at depth `zoom + underlayOffset`. */
  cell: bigint;
  /** How many items in the cell this principal may see. Never `0`: empty cells are left out. */
  count: bigint;
};

/**
 * One annotation artifact (a cluster, a boundary, a topic) as a viewport serves it.
 *
 * `maskedCount` is how many of the artifact's members this principal can see. It is not the
 * artifact's size, and two principals can get different numbers for the same `tesseraId`. It
 * counts members out of view too, so it does not change as the map moves.
 *
 * An artifact withheld from this principal is absent, and nothing in the response shows that it
 * exists.
 *
 * @category Requests and responses
 */
export type Artifact = {
  /** The name of the layer the artifact belongs to. */
  layer: string;
  /** The artifact's `tessera_id`, the same for every principal and session. */
  tesseraId: bigint;
  /** The publisher's own key, or `null` where none was supplied. */
  key: string | null;
  /** How many of the artifact's members this principal can see. */
  maskedCount: bigint;
  /**
   * The centre of the members this principal can see, in 32-bit grid units
   * ({@link gridToWorldXY} converts to world space). `null` where the layer does not declare it or
   * the request's `computed` left it out; a served artifact's geometry is never withheld. It moves
   * with the principal, as the count does, so do not reuse one principal's value for another.
   */
  centroid: [number, number] | null;
  /** The bounding box of the members this principal can see, `[minX, minY, maxX, maxY]` in 32-bit grid units; `null` as for `centroid`. */
  box: [number, number, number, number] | null;
  /**
   * The artifact's drawn outline ({@link Shape}), of the kind {@link Layer.shape} names. `null`
   * where the layer draws none or the request's `computed` left it out. In a derived hull, each
   * separate group of visible members is its own part, and a group of one or two members is a ring
   * of one or two vertices. Two parts may overlap.
   */
  shape: Shape | null;
  /**
   * The publisher's supplied content (label text, a name, a polygon), one entry per type in the
   * layer's {@link Layer.suppliedContent}. Empty where the layer declares none. Where an artifact
   * has several ranked descriptions, this is the one this principal qualifies for, so two
   * principals may see different text for one `tesseraId`.
   */
  content: string[];
  /**
   * The artifact's parents that are also in this response, ascending by `tesseraId`. A tree gives
   * at most one; a `dag` layer may give several, and the first is the same one every time. Empty
   * for a root, for a flat artifact and for one whose parent this principal was not served; the
   * three are not told apart. Treat an artifact with no parent here as a root.
   */
  parentIds: bigint[];
  /**
   * The level to draw the artifact at: its declared level on a `stacked` or `tiered` layer, its
   * depth among this response's parent links on a `nested` or `dag` layer (a root reads `0`), and
   * `0` on a flat layer. Draw by this value; do not derive it from `parentIds`.
   */
  rung: number;
  /**
   * Whether a member this principal can see, inside the requested tiles, satisfies the request's
   * filter. `null` where the request carried no filter. A filter changes only this field: which
   * artifacts are served, their counts and their geometry stay the same. A member off screen does
   * not count until the view covers it. Do not derive it from the points held, which are a sample.
   */
  matched: boolean | null;
  /**
   * `matched` for `all_of[filters, highlight]`: whether a member this principal can see, inside the
   * requested tiles, satisfies both. `null` where the request carried no `highlight`. The cautions
   * on `matched` apply.
   */
  highlighted: boolean | null;
  /**
   * The `tesseraId` of the artifact in this response that this one attaches to, as a topic label
   * attaches to its cluster; `null` where it attaches to nothing. The target is always in the same
   * response: an artifact whose target is not served is not served either. An attached artifact's
   * `matched` and `highlighted` are its target's. Its `maskedCount` counts its own members, or its
   * target's where it declares none, and is not the number to show beside a label.
   */
  target: bigint | null;
};

/**
 * One artifact row in the identity projection (`artifactRows: 'identity'`). The rows, and their
 * `rung`, `matched` and `highlighted` values, are those the full answer to the same request would
 * carry; the other columns are left out. A client joins each row to what it holds by
 * `(layer, tesseraId)`, and asks again with `'full'` for any row it cannot resolve.
 *
 * @category Requests and responses
 */
export type ArtifactIdentity = {
  /** The name of the layer the artifact belongs to. */
  layer: string;
  /** The artifact's `tessera_id`. */
  tesseraId: bigint;
  /** The level to draw the artifact at, as on {@link Artifact}. */
  rung: number;
  /** As {@link Artifact.matched}; `null` with no filter. */
  matched: boolean | null;
  /** As {@link Artifact.highlighted}; `null` with no highlight. */
  highlighted: boolean | null;
};

/**
 * A decoded viewport response. The per-point arrays all follow the order of `ids`.
 *
 * @category Requests and responses
 */
export type ViewportResult = {
  /** One entry per tile with anything visible to this principal, in the response's tile order. */
  tiles: TileCounts[];
  /** Each served point's `tessera_id`. */
  ids: BigUint64Array;
  /**
   * Each point's 64-bit Morton position code, its two 32-bit grid coordinates interleaved. The
   * high 32 bits are the point's cell at depth 16; shifting right by `64 - 2z` gives the depth-`z`
   * tile that holds it.
   */
  codes: BigUint64Array;
  /**
   * Interleaved x, y per point in cell space: `[0, 65536)` per axis, with a fraction below the
   * cell. A `Float64Array`, since a position has 32 bits per axis and an `f32` holds 24.
   */
  positions: Float64Array;
  /**
   * The same points in world space, `[0, 512)` per axis ({@link WORLD_SIZE}), as `f32`: cell space
   * divided by 128. This is the space the renderer draws in.
   */
  world: Float32Array;
  /** The `render` columns, keyed by column name. */
  scalars: Record<string, ScalarColumn>;
  /**
   * One column per layer that served artifacts, keyed by layer name. For point `i`, `index[i]` is
   * `0` where the point is under no served artifact of the layer, and otherwise `1 + d`, where
   * `ids[d]` is the `tessera_id` of the deepest served artifact holding it. Every such id is an
   * artifact this response served. Empty when the response served no artifacts.
   */
  membership: Record<string, MembershipColumn>;
  /**
   * One byte per point: `1` where the point satisfies the request's `highlight`, `0` where it
   * does not. `null` when the request carried no `highlight`.
   */
  highlighted: Uint8Array | null;
  /**
   * Which columns the points came with. `'full'` for an ordinary response. `'highlight'` carries
   * `ids` and `highlighted` only: {@link codes}, {@link positions}, {@link world}, {@link scalars}
   * and {@link membership} are empty, and a caller joins the bits to the points it holds by
   * `tessera_id`.
   */
  pointsProjection: 'full' | 'highlight';
  /** The underlay's cells, where the request set `underlayOffset`; `null` otherwise. */
  subCells: SubCell[] | null;
  /**
   * The artifacts served; empty when none were, or when the response used the identity
   * projection. The response does not say why none were served: no layer asked for or reachable,
   * none in view, or none qualifying are one answer.
   */
  artifacts: Artifact[];
  /**
   * The artifact rows when the request set `artifactRows: 'identity'` and some artifact was
   * served; `null` otherwise. When this is set, `artifacts` is empty.
   */
  artifactsIdentity: ArtifactIdentity[] | null;
};

/**
 * One layer's membership column, as the decoder returns it. See {@link ViewportResult.membership}.
 *
 * @category Requests and responses
 */
export type MembershipColumn = {
  /**
   * Per point, `0` for no served artifact, otherwise one more than the position in `ids` of the
   * artifact holding the point. A `Uint16Array` when there are at most 65,535 points.
   */
  index: Uint16Array | Uint32Array;
  /** The distinct artifact `tessera_id`s the column names, in the order first seen. */
  ids: BigUint64Array;
};

/**
 * One `render` column's values for a response's points, in point order.
 *
 * `arrowType` is the storage type the points came in; whether the column is a category is on
 * {@link DeclaredScalar.category}. The values are the typed array the decoder already holds, so
 * they can go into a GPU attribute without copying.
 *
 * `present`, where set, says which points have a value: `present[i]` is `0` where the server sent
 * null, and `values[i]` is then a zero that means nothing. Where `present` is missing or `null`,
 * every point has a value. A category has no nulls; code `0` is its absent value.
 *
 * @category Requests and responses
 */
export type ScalarColumn = ScalarValues & {
  /** Per point, `1` where it has a value and `0` where the server sent null. Missing or `null` when every point has a value. */
  present?: Uint8Array | null;
};

/**
 * A column's storage type and its values, one arm per type. 64-bit integers and `timestamp_us`
 * (microseconds since the Unix epoch) are `bigint` arrays.
 *
 * @category Requests and responses
 */
export type ScalarValues =
  | {arrowType: 'bool'; values: boolean[]}
  | {arrowType: 'utf8'; values: string[]}
  | {arrowType: 'u8'; values: Uint8Array}
  | {arrowType: 'u16'; values: Uint16Array}
  | {arrowType: 'u32'; values: Uint32Array}
  | {arrowType: 'u64'; values: BigUint64Array}
  | {arrowType: 'i8'; values: Int8Array}
  | {arrowType: 'i16'; values: Int16Array}
  | {arrowType: 'i32'; values: Int32Array}
  | {arrowType: 'i64'; values: BigInt64Array}
  | {arrowType: 'f32'; values: Float32Array}
  | {arrowType: 'f64'; values: Float64Array}
  | {arrowType: 'timestamp_us'; values: BigInt64Array};

/**
 * The server's timings for one viewport response, read from its headers and its trailer.
 *
 * @category Requests and responses
 */
export type Timings = {
  /**
   * Microseconds from admission to the server's first flush (`x-tessera-server-us`); `0` when the
   * header is absent. It does not cover the rest of the stream.
   */
  serverUs: number;
  /** Microseconds the request waited for admission (`x-tessera-admission-us`); `0` when the header is absent. */
  admissionUs: number;
  /**
   * Per-stage timings from the trailer's `stage_ns`, as numbers in its field order, or `null`
   * where the trailer has none. The server sends them only when it was built with the
   * `bench-timing` feature and `serve.stage_timing` is on.
   */
  stageNs: number[] | null;
};

/**
 * One points frame of a streamed viewport response, handed to a {@link PartSink} as it arrives.
 *
 * A part holds whole tiles: `result.tiles` is the run of tiles its points cover, in the
 * response's tile order. Its `subCells` is `null`, and its `artifacts` are the whole response's.
 * A part carries the response's keys because it arrives before the response resolves, so it can
 * be stored under the right principal and matched to the request it answers.
 *
 * @category Requests and responses
 */
export type ViewportPart = {
  /** This frame's points, with the tiles they cover and the response's artifacts. */
  result: ViewportResult;
  /** As {@link ViewportResponse.identityKey}. */
  identityKey: string;
  /** As {@link ViewportResponse.contentKey}. */
  contentKey: string;
};

/**
 * A viewport response, as {@link TesseraClient.viewport} returns it.
 *
 * @category Requests and responses
 */
export type ViewportResponse = {
  /**
   * The decoded response. Where the call was given a part sink, the points went to the sink and
   * this result's per-point arrays are empty.
   */
  result: ViewportResult;
  /** The server's timings. */
  timings: Timings;
  /**
   * The key held data is partitioned by (`x-tessera-identity-key`). It hashes the exact credential
   * bytes presented at authorisation, the identity of the principal's visible-set fragment, which a
   * compaction or a rebuilt bundle changes, and the view. Data held under one key must not be shown
   * under another, so a client drops what it holds when this changes.
   */
  identityKey: string;
  /**
   * The response's entity tag (`etag`), without its quotes. It changes when the visible set could
   * have gained rows. A merge or compaction that only rearranges rows leaves it unchanged. A tile
   * held under an older content key may still be drawn; list it in {@link ViewportRequest.tiles}
   * to fetch it again.
   */
  contentKey: string;
  /**
   * The generation this response was answered from (`x-tessera-pin`, a JSON string). Send it back
   * as {@link ViewportRequest.stamp}; its only effect is {@link ViewportResponse.stale}. Until the
   * refresh after a flush reaches this session, it names the previous generation, and the flush's
   * new rows are not yet visible. Deletions and suppressions apply at once whatever it names.
   * `null` when the header is absent.
   */
  pin: string | null;
  /**
   * Whether the request's `stamp` differs from this response's {@link pin}; `false` when no stamp
   * was sent. It changes at every flush, merge and compaction, whether or not anything this
   * principal can see changed. It is advisory: nothing expires and nothing is withheld while it is
   * `true`.
   */
  stale: boolean;
  /** The verdict on the request's `region` leaves (`x-tessera-region`); `null` when the request carried none. */
  region: RegionVerdict | null;
  /** The size of the response body, in bytes. */
  bytes: number;
};

/**
 * One item's record, as {@link TesseraClient.item} returns it.
 *
 * @category Requests and responses
 */
export type ItemDetail = {
  /**
   * The record, keyed by declared column name. Numbers of every width are JavaScript numbers, so
   * a 64-bit integer past 2^53 loses precision. A category is its vocabulary key. A column the
   * item has no value for is absent.
   */
  fields: Record<string, unknown>;
  /**
   * The views this item is in that this principal may reach, sorted by id, each with the item's
   * position in it. A view this principal cannot reach is absent, as an undeclared one is, so an
   * empty list means none of the item's views is reachable. An item in no view at all is a `404`.
   */
  views: ItemViewPosition[];
  /**
   * The item's group-scoped values, by family name and then by the view's key within the group:
   * `{mood: {'2026-Q1': 'calm'}}`. Two views sharing a key through a `members` group share one
   * entry. Which group a family belongs to is on {@link Meta.scopedScalars}. Keys this principal
   * cannot reach are absent, and so is a family with no value this principal may see.
   */
  scoped: Record<string, Record<string, unknown>>;
  /**
   * The item's access labels that this session satisfies, sorted. It never lists the item's other
   * labels, so it says which of the viewer's grants admit the item. It does not say how the item
   * is labelled. An empty list is an answer.
   */
  labels: string[];
};

/**
 * Where one view places an item. See {@link ItemDetail.views}.
 *
 * @category Requests and responses
 */
export type ItemViewPosition = {
  /** The view's id: a plain view's name, or `<group>:<key>`. */
  id: string;
  /**
   * The item's x in that view's 32-bit grid units, over the view's own
   * {@link ViewInfo.quantisation}. Two views quantise differently, so the same item has a different
   * position in each.
   */
  x: number;
  /** The item's y, in the same units as `x`. */
  y: number;
};

/**
 * One artifact opened by `tessera_id`, as {@link TesseraClient.artifact} returns it. Its count and
 * geometry are those the viewport serves for the same artifact in the same view. It carries no
 * member list and no unmasked size.
 *
 * @category Requests and responses
 */
export type ArtifactDetail = {
  /** The name of the layer the artifact belongs to. */
  layer: string;
  /** The publisher's own key, or `null` where none was supplied. */
  key: string | null;
  /** How many of the artifact's members this principal can see, in the requested view. */
  maskedCount: bigint;
  /**
   * The centre of the members this principal can see, in 32-bit grid units; `null` where the
   * layer does not declare it.
   */
  centroid: [number, number] | null;
  /** The bounding box of those members, `[minX, minY, maxX, maxY]` in 32-bit grid units; `null` where the layer does not declare it. */
  box: [number, number, number, number] | null;
  /**
   * The artifact's drawn outline, `null` where its layer draws none. A predicate or authored shape
   * is simplified for the `zoom` the call gave, or served whole without one. This is the route to
   * fetch an outline from; see {@link ViewportRequest.computed}.
   */
  shape: Shape | null;
};

/**
 * The body of `POST /v1/artifacts/browse`, as {@link TesseraClient.browse} takes it: a layer's
 * artifacts by lineage, independent of the map's viewport. It has three forms:
 *
 * - Roots, with neither `parent` nor `q`: the layer's artifacts with no served parent.
 * - Children, with `parent`: the artifacts naming it among their parents, and the parent's own
 *   parents in {@link BrowsePage.parents}. On a `dag` layer a child appears under each served
 *   parent.
 * - Search, with `q`: the artifacts whose key, or whose first supplied text, contains `q`,
 *   ignoring case.
 *
 * Sending both `parent` and `q` is a `422`. Pages are ordered by `matchedCount` where `filters` is
 * set and by `maskedCount` otherwise, then by `tesseraId` ascending, so a cursor neither repeats
 * nor skips a row.
 *
 * @category Requests and responses
 */
export type BrowseRequest = {
  /** The view whose items the counts are taken over. Required, since a masked count is per view. */
  view: string;
  /**
   * The layer to browse. A layer `/v1/meta` does not list for this principal is a `422`, and so is
   * a layer whose artifacts attach to another layer's.
   */
  layer: string;
  /**
   * The level the form addresses, on a `stacked` or `tiered` layer. A `422` on a `flat`, `nested`
   * or `dag` layer, and on a level the layer does not hold.
   */
  level?: number;
  /**
   * The children form's parent, by `tessera_id`. An id naming nothing, an artifact of another
   * layer, a suppressed one and one this principal is not served all answer an empty page.
   */
  parent?: bigint;
  /** The search form: the artifacts whose key or {@link BrowseRow.name} contains this, ignoring case. */
  q?: string;
  /**
   * The viewport's filter. Each row then carries `matchedCount` and the page is ordered by it.
   * Which rows are served and their `maskedCount` do not change, and a row whose `matchedCount` is
   * zero is still served.
   */
  filters?: FilterExpr | null;
  /** The page size. Defaults to `selection.maxBrowseRows` and is capped at it; `0` is a `422`. */
  limit?: number;
  /** A previous page's {@link BrowsePage.next}, unchanged. */
  cursor?: string;
};

/**
 * One row of a browse page.
 *
 * @category Requests and responses
 */
export type BrowseRow = {
  /** The artifact's `tessera_id`. */
  tesseraId: bigint;
  /** The publisher's own key, or `null` where none was supplied. */
  key: string | null;
  /**
   * The artifact's first supplied text; where it has none, the first text of an artifact attached
   * to it that this principal is served, such as a cluster's topic label, taking label layers in
   * `meta`'s order. `null` where neither gives text.
   */
  name: string | null;
  /** How many of the artifact's members this principal can see. A filter does not change it. */
  maskedCount: bigint;
  /** How many of those members the request's filter admits; `null` where the request carried no `filters`. */
  matchedCount: bigint | null;
  /**
   * The artifact's level: its declared level on a `stacked` or `tiered` layer, its depth among the
   * artifacts this principal is served on a `nested` or `dag` layer, and `0` on a flat layer.
   */
  rung: number;
  /** The artifact's parents that this principal is also served, ascending. A parent it is not served is absent. */
  parentIds: bigint[];
  /**
   * How many artifacts this principal is served name this one among their parents: the rows the
   * children form with this artifact as `parent` lists across its pages. `0` for a leaf, and on a
   * `flat` or `stacked` layer.
   */
  childCount: number;
};

/**
 * One page of `POST /v1/artifacts/browse`.
 *
 * @category Requests and responses
 */
export type BrowsePage = {
  /** The page's rows. */
  artifacts: BrowseRow[];
  /** In the children form, the requested parent's own served parents; `[]` in the other forms. */
  parents: BrowseRow[];
  /** The cursor for the next page, or `null` where this was the last. */
  next: string | null;
};

/**
 * The body of `POST /v1/items`, which {@link TesseraClient.items} sends: every item this
 * principal may see in `view` that matches `filters`. A field left unset is not sent, and the
 * server's default applies.
 *
 * The columns of each page are `tessera_id` (`uint64`), the named fields in the order named, the
 * system fields in the order named, then `tessera:matched` (`bool`) under `keepUnmatched`. Every
 * named field is present whether or not an item carries a value, and an absent value is null. A
 * category field is a dictionary column of its keys, each page's dictionary holding only the keys
 * its rows carry.
 *
 * @category Requests and responses
 */
export type ItemsRequest = {
  /** The view to read, from `/v1/meta`. An item with no position in it is not returned. */
  view: string;
  /**
   * Declared fields, by name, each once, in the order their columns come back. A group-scoped
   * field outside its group is pinned as `<field>@<key>`. An empty list returns `tessera_id`
   * alone. An undeclared or repeated field is a `422`.
   */
  fields: string[];
  /**
   * System columns, after the fields, in this order. `position` is `tessera:x` and `tessera:y`
   * (`float64`) in the view's coordinates, so degrees on a geographic view. `labels` is
   * `tessera:labels` (`list<utf8>`), the item's labels this principal holds, sorted.
   */
  systemFields?: ('position' | 'labels')[];
  /** The viewport's filter. Only the items matching it are returned, unless `keepUnmatched` is set. */
  filters?: FilterExpr;
  /**
   * Every visible item, with a `tessera:matched` column, in place of the matching ones only.
   * Without `filters` every row is marked matched.
   */
  keepUnmatched?: boolean;
  /**
   * Put {@link ItemsHead.visible} and {@link ItemsHead.matched} in the head. A `422` together with
   * `cursor`. {@link TesseraClient.items} sends it on the read's first request only.
   */
  count?: boolean;
  /**
   * `map` returns rows by map cell in `view`, then by `tessera_id`. `stored` returns them in the
   * order the record store holds items, which is faster for a field held only there. Unset takes
   * the cursor's order, or with no cursor the server's choice. A value different from the
   * cursor's is a `422`.
   */
  order?: 'map' | 'stored';
  /**
   * Rows per page, capped by `meta.selection.maxPageRows`; the head reports the size used. Unset
   * is the cap, and `0` is a `422`.
   */
  pageRows?: number;
  /** The most pages one response may carry. Unset is as many as the response's budgets allow, and `0` is a `422`. */
  pages?: number;
  /** A previous response's cursor, unchanged: where the read resumes. Unset starts from the beginning. */
  cursor?: string;
  /** Compress each page's Arrow buffers with zstd. The client decodes either form. */
  compression?: 'zstd';
};

/**
 * The body of `POST /v1/artifacts`, which {@link TesseraClient.artifacts} sends: every artifact of
 * `layer` this principal is served, ordered by level and then by publication order within the
 * level. A field left unset is not sent, and the server's default applies.
 *
 * The columns of each page are `tessera_id` (`uint64`), the named properties in the order named,
 * then `matched_count` (`uint64`) where the request carries `filters`.
 *
 * @category Requests and responses
 */
export type ArtifactsRequest = {
  /** The view the counts, centroids, boxes and shapes are taken in. */
  view: string;
  /** A layer `/v1/meta` publishes to this principal. Any other is a `422`. */
  layer: string;
  /**
   * Properties, each once, in the order their columns come back. `centroid` is the columns
   * `centroid_x` and `centroid_y`, and `box` the columns `box_x_min`, `box_y_min`, `box_x_max` and
   * `box_y_max`, in the view's coordinates. `shape` is a WKB `MultiPolygon`. A repeated property is
   * a `422`.
   */
  fields: ('key' | 'level' | 'parents' | 'target' | 'masked_count' | 'content' | 'centroid' | 'box' | 'shape')[];
  /** Only the artifacts at this level. A `422` on a layer with one level, and past the levels the layer holds. */
  level?: number;
  /** Only the artifacts naming this one among their parents. A `422` together with `q`. */
  parent?: bigint;
  /** Only the artifacts whose key, or first served text content, contains this, ignoring case. A `422` together with `parent`. */
  q?: string;
  /** Only artifacts with a visible member matching it, and a `matched_count` column. */
  filters?: FilterExpr;
  /** With `filters`, every served artifact, one with no matching member having a `matched_count` of zero. */
  keepUnmatched?: boolean;
  /**
   * Put {@link ArtifactsHead.served} and {@link ArtifactsHead.matched} in the head. A `422`
   * together with `cursor`. {@link TesseraClient.artifacts} sends it on the read's first request
   * only.
   */
  count?: boolean;
  /** As {@link ItemsRequest.pageRows}. */
  pageRows?: number;
  /** As {@link ItemsRequest.pages}. */
  pages?: number;
  /** As {@link ItemsRequest.cursor}. */
  cursor?: string;
  /** As {@link ItemsRequest.compression}. */
  compression?: 'zstd';
};

/**
 * The head of a `POST /v1/items` response, as {@link RecordsRead.head} carries it.
 *
 * @category Requests and responses
 */
export type ItemsHead = {
  /** The order this response's rows are in. */
  order: 'map' | 'stored';
  /** The page size used, after the cap. */
  pageRows: number;
  /** Visible items in the view at the start of the response; `null` unless the request asked for `count`. */
  visible: number | null;
  /** Of those, the ones matching the filter; `null` unless the request asked for `count`. */
  matched: number | null;
};

/**
 * The head of a `POST /v1/artifacts` response, as {@link RecordsRead.head} carries it.
 *
 * @category Requests and responses
 */
export type ArtifactsHead = {
  /** The page size used, after the cap. */
  pageRows: number;
  /** Artifacts the request selects that this principal is served; `null` unless the request asked for `count`. */
  served: number | null;
  /** Of those, the ones with a visible member matching the filter; `null` unless the request asked for `count`. */
  matched: number | null;
};

/**
 * The frame after each page of a bulk read.
 *
 * @category Requests and responses
 */
export type PageEnd = {
  /** The cursor to resume after this page; `null` where no row remains. */
  next: string | null;
  /**
   * `rows`: the page holds `pageRows` rows. `bytes`: the next row would have taken it past
   * `meta.selection.maxPageBytes`. `time`: the response's time ran out, or the stream deadline
   * cancelled it, and the response ends after this page. `end`: no row remains.
   */
  endedBy: 'rows' | 'bytes' | 'time' | 'end';
};

/**
 * The last frame of a bulk read's response.
 *
 * @category Requests and responses
 */
export type RecordsTrailer = {
  /** The pages this response carried. A page of no rows, which a response that found no row carries, counts. */
  pages: number;
  /** The rows this response carried. */
  rows: number;
  /** The cursor to resume from, which can be past the last page end; `null` where no row remains. */
  next: string | null;
  /**
   * `end`: no row remains. `pages`: the response carried the request's `pages` pages.
   * `budget_bytes` and `budget_time`: the response's byte or time budget ran out. `deadline`: the
   * stream deadline cancelled it.
   */
  endedBy: 'end' | 'pages' | 'budget_bytes' | 'budget_time' | 'deadline';
  /** The whole response's wall time on the server, in microseconds, including waits on the client. */
  streamUs: number;
};

/**
 * The body of `POST /v1/aggregate`, which {@link TesseraClient.aggregate} sends: how the items this
 * principal may see in `view` are distributed, as one table of exact counts per grouping. A field
 * left unset is not sent, and the server's default applies.
 *
 * @category Requests and responses
 */
export type AggregateRequest = {
  /** The view the counts are taken in, from `/v1/meta`. An item with no position in it is not counted. */
  view: string;
  /** The set every count is taken over, in the viewport's grammar. Unset is every item this principal may see in `view`. */
  filters?: FilterExpr;
  /**
   * The set each count is compared with, in the same grammar and view, drawn from the same visible
   * set. `{}` is the whole visible set. Unset adds no comparison columns.
   */
  reference?: FilterExpr;
  /** One table each, in this order. At most `meta.selection.maxAggregateGroupings`. */
  groupings: Grouping[];
  /** As {@link ItemsRequest.pageRows}. */
  pageRows?: number;
  /** As {@link ItemsRequest.pages}. */
  pages?: number;
  /** A previous result's {@link AggregateResult.next}, unchanged, under the same request. */
  cursor?: string;
  /** As {@link ItemsRequest.compression}. */
  compression?: 'zstd';
};

/**
 * One table of an aggregate. `{}` is the size of the set. `by` picks groups; `cells` divides the
 * set, or each group, into cells of the view.
 *
 * @category Requests and responses
 */
export type Grouping = {
  /** The outer level: the groups. Unset counts the set as one group. */
  by?: AggregateBy;
  /** The inner level: the cells each group is divided into. Unset counts each group whole. */
  cells?: AggregateCells;
};

/**
 * A grouping's outer level: the values of a category field or the artifacts of one level of a
 * layer, as the `top` groups by count or as the groups named.
 *
 * A `field` is a category declared with `index` or `render`, or one with a `derived` vocabulary; a
 * group-scoped field resolves under the request's view as a filter leaf on it does, or is pinned as
 * `<field>@<key>`. A `layer` is one `/v1/meta` publishes to this principal, and `level` is required
 * on a layer with several levels and refused on one with a single level. A named value or artifact
 * this principal would not be listed gets no row.
 *
 * @category Requests and responses
 */
export type AggregateBy =
  | {field: string; top: number}
  | {field: string; values: string[]}
  | {layer: string; level?: number; top: number}
  | {layer: string; level?: number; artifacts: bigint[]};

/**
 * A grouping's inner level: the cells of the view at `depth`, from 0 to 32. Depths 0 to 16 are the
 * map's tiles at that zoom; 17 to 32 divide them down to the stored position.
 *
 * @category Requests and responses
 */
export type AggregateCells = {
  /** From 0 to 32. */
  depth: number;
  /**
   * `[x0, y0, x1, y1]` as the viewport's `bbox` takes it. Only the cells at `depth` that intersect
   * it are listed, each with all of its items. Unset is the view's whole extent. The cells may
   * number at most `meta.selection.maxAggregateCells`.
   */
  area?: [number, number, number, number];
};

/**
 * One grouping's table, as {@link AggregateResult.tables} carries it: the figures of its head and
 * its rows.
 *
 * The columns are, in this order and each only where stated: `group` (`listed`, `rest` or `none`,
 * with `by`); `key` (a vocabulary key, or an artifact's `tessera_id` as a `bigint`, with `by`; null on
 * `rest` and `none`); `title` (the value's title, with `by` on a field); `cell` (a `bigint`, the
 * first `2·depth` bits of the Morton position, with `cells`); `count` (a `bigint`); and with a
 * reference, `reference_count` (a `bigint`) and `lift` (a number, null where either count it
 * divides by is 0).
 *
 * @category Requests and responses
 */
export type AggregateTable = {
  /** The table's index in the request's `groupings`. */
  grouping: number;
  /** Items in the set. */
  total: number;
  /** Items in the reference set; `null` where the request carried no `reference`. */
  referenceTotal: number | null;
  /**
   * The groups with an item in the set before the cut to `top` or the named list; `null` where the
   * grouping has no `by`.
   */
  groups: number | null;
  /** The rows of every page read, joined in order. */
  rows: Table;
};

/**
 * What {@link TesseraClient.aggregate} returns.
 *
 * @category Requests and responses
 */
export type AggregateResult = {
  /**
   * One table per grouping read, in the order of `groupings`. The figures are those of the table's
   * first head read; a table carried over several responses keeps the groups its first page
   * listed.
   */
  tables: AggregateTable[];
  /** The `x-tessera-region` verdict, present where `filters` or `reference` carried a `region` leaf. */
  region: RegionVerdict | null;
  /**
   * Whether a page counted a different state of the corpus from the page before it, in any
   * response read. The tables then mix counts taken before and after a change.
   */
  recomposed: boolean;
  /** The last response's identity key for this principal and view; empty where it carried none. */
  identityKey: string;
  /** Where the read continues: `null` once every table is whole. */
  next: string | null;
};
