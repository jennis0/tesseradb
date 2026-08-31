export type Session = {token: string; tokenId: number; expiresAt: number};

export type Quantisation = {xMin: number; xMax: number; yMin: number; yMax: number};

/**
 * Every Arrow type a declared scalar may have (contracts §2.2). `timestamp_us` is stored as an
 * `i64` of microseconds and named separately so the unit is a fact rather than a convention.
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
  | 'utf8';

/**
 * What makes a column a category rather than a plain integer.
 *
 * The hot path ships a category as a bare integer of the declared width, so **this block's absence
 * is the only thing that distinguishes the two**. A client that ignores it will render category
 * codes as numbers on a continuous ramp, which is wrong rather than merely ugly: the codes are
 * drawn at random from the width (contracts §2.2), so their numeric order means nothing.
 */
export type CategoryDescriptor = {
  /**
   * The value set this column's codes index. Two columns may share one — keys, codes and titles
   * are shared with it, so a resolved palette may be reused across them.
   *
   * **Visibility may not be reused across them.** Member sets are per column, so a value visible
   * under one column may be invisible under another that shares this vocabulary.
   */
  vocabulary: string;
  /** Whether the value set is closed at build (`declared`) or grows from the corpus. */
  kind: 'declared' | 'discovered';
  /**
   * Whether the *existence* of a value is sensitive. `derived` means `/v1/categories` filters the
   * set per principal — the value is offered only where the viewer can already see a point
   * carrying it.
   */
  visibility: 'derived' | 'public';
};

/** One declared per-item column. `category` is present only for a category column. */
export type DeclaredScalar = {
  /**
   * The column's name, which is also its **identifier**: it addresses the column in
   * `/v1/categories/{column}`. Unique bundle-wide, and not view-qualified.
   */
  name: string;
  arrowType: ArrowType;
  category: CategoryDescriptor | null;
  /**
   * Whether the column occupies a slot in every row of the hot column — and therefore whether it
   * **arrives in a viewport response at all**.
   *
   * This is the flag that decides what may be drawn. A column with `render: false` lives in entity
   * space or in the record blob; it can be filtered on and returned at drill-down, but no viewport
   * response carries a value for it, so nothing can be coloured by it. A client that offered every
   * declared column to its colour control would be offering columns whose values never arrive.
   */
  render: boolean;
  /** Whether the column carries an entity-space filter index, as against being answered by a scan. */
  index: boolean;
};

/**
 * What a client may filter a column by, as `/v1/meta` publishes it (contracts §3.2).
 *
 * **`family` is what decides which control to draw**, and it is published rather than inferred
 * because two of the four cannot be derived from `arrowType`: a `text` column's type is a string
 * type and its operand is not a string predicate, and a `keyword`'s values are held in a dictionary
 * the server never serves. Concretely — a `category` has a value set, so `/v1/categories` fills a
 * dropdown; a `string` or `keyword` has none, because its values are row data rather than a
 * vocabulary, so the control is a free-text box and no endpoint will ever enumerate it.
 */
export type FilterOperandSet = {
  column: string;
  family: 'category' | 'keyword' | 'string' | 'text' | 'numeric';
  /** The operator names this column accepts — `eq`, `in`, `prefix`, `contains`, `match`, `phrase`, `range`. */
  operands: string[];
  /**
   * Present only on a **group-scoped** attribute: the view group whose views this column's values
   * are per (`views.md` §5). Absent is entity scope — one value per entity, the same under every
   * view — which is the ordinary case and says itself.
   *
   * Under a view of that group, or of a group sharing its views, the leaf is sent bare and the
   * request's own view decides which column it reads. Under any other view the leaf must **pin**
   * one: `column@key`, the key being a view's only address.
   * An unpinned leaf there is a 422 naming the group, and a pin naming no view of it is the same
   * 404 an unknown view gets.
   */
  scope?: {group: string};
};

/**
 * A filter expression, exactly as `/v1/viewport` takes it.
 *
 * A node carries **one** key: a column name, or one of the three combinators. Two keys would need
 * an implicit operator between them, and the server refuses rather than choosing one.
 *
 * **An unresolvable value is an empty operand, never a refusal** (contracts §3.2): a filter naming
 * a value this principal cannot see and one naming a value that does not exist are indistinguishable
 * in status, body and every count. A client must not present "no matches" as "no such value".
 */
export type FilterExpr =
  | {all_of: FilterExpr[]}
  | {any_of: FilterExpr[]}
  | {none_of: FilterExpr[]}
  | {region: RegionOperand}
  | {[column: string]: FilterOperator};

/**
 * The `region` leaf (`selection-operand.md` §2; `polygon-membership.md` §8): exactly one of a
 * shape in the view's own data space — `polygon` (at least three vertices, implicitly closed),
 * `bbox` (`[x0, y0, x1, y1]`), `circle` (`[cx, cy, r]`), `ellipse` (`[cx, cy, a, b,
 * angle_degrees]`) — with `space` (`view`, the only value a view honours today), or `artifact`, a
 * published shape's `tessera_id` as a decimal string. `region` is a reserved column name.
 */
export type RegionOperand =
  | {polygon: [number, number][]; space?: 'view'}
  | {bbox: [number, number, number, number]; space?: 'view'}
  | {circle: [number, number, number]; space?: 'view'}
  | {ellipse: [number, number, number, number, number]; space?: 'view'}
  | {artifact: string};

/**
 * `x-tessera-region` (`selection-operand.md` §6): whether every region leaf's answer is exact for
 * the shape against each point's stored position, or exact for a **cover** of it — a superset —
 * taken at `depth` because the shape's perimeter exceeded `max_region_cells`. A function of the
 * shape and the grid alone, never of the rows.
 */
export type RegionVerdict = {exact: true; depth: null} | {exact: false; depth: number};

/** One column's predicate. Exactly one key — the server refuses a leaf carrying two. */
export type FilterOperator =
  | {eq: string | number | boolean}
  | {in: (string | number)[]}
  | {prefix: string}
  | {contains: string}
  /** Every analysed token must appear, unless `minimum_should_match` names how many must. */
  | {match: string | {query: string; minimum_should_match?: number}}
  /** Adjacent, in order. There is no `minimum_should_match` for a phrase — adjacency is not a count. */
  | {phrase: string}
  | {range: {gte?: number; gt?: number; lte?: number; lt?: number}};

/**
 * One annotation layer, as `/v1/meta` publishes it to **this** principal.
 *
 * The list is gate-filtered: a layer this principal cannot reach is absent, by exactly the route a
 * never-registered one takes. So a client draws its layer controls from this and nothing else —
 * there is no other document naming a layer, and no request that reveals one.
 *
 * **It never says how many artifacts a layer holds.** That is a corpus-wide count over objects the
 * principal may not individually see, and its absence here is deliberate rather than an oversight:
 * how much of a layer a principal reaches is only ever answered artifact by artifact, by the
 * viewport. Nor is the gate label published — a caller who reaches the layer has already satisfied
 * it, and one who has not never sees the entry.
 */
export type Layer = {
  /** The layer's identity, and what a viewport request names to select it. */
  name: string;
  title: string;
  /** Which views the layer appears in. A layer is not answerable in a view it does not name. */
  views: string[];
  membership: 'enumerated' | 'spatial' | 'attribute';
  /**
   * Where the layer's lineage lives, and the **default** cut depth — not its only setting, since a
   * request may ask for more detail (`artifactBudget`).
   */
  /**
   * The four kinds the wire declares (`tessera-types`' `HierarchyKind`): `flat` (no lineage),
   * `nested` (a tree in the edges, no levels), and the two levelled shapes `stacked` and `tiered`.
   * `tiered` was missing here until 2026-08-28 — the union is what the fetch model classifies
   * layers by, so an absent member is a layer silently treated as something it is not.
   */
  hierarchy: {kind: 'flat' | 'nested' | 'stacked' | 'tiered'; pruneChildren: boolean};
  /**
   * The resolutions the layer declares. **Empty for a treed layer**, which declares none: its
   * lineage is in its edges, and a level number would say nothing about position in it.
   */
  levels: {level: number; title: string; zoom: [number, number] | null}[];
  /**
   * Which computed properties the layer **declares** — `centroid`, `box`, `hull` — as `/v1/meta`
   * publishes them in `computed_content` (contracts §3.2 r42). It is the declaration and not a
   * property of any one artifact: a layer that declares `hull` serves one for every artifact that
   * exists for this principal, though the viewport asks for centroids and boxes and a client
   * fetches a shape by identifier when it needs one.
   */
  computedContent: string[];
  /**
   * **Which kind the layer's one drawn geometry is** (`polygon-membership.md` §7.1), or `null`
   * where it draws none: `derived` — the hull over the members this principal can see, which
   * moves with the principal; `predicate` — the membership shape of a spatial layer, the same for
   * every principal served the artifact; `authored` — a supplied drawing, likewise. A client
   * draws from this — a box drawn for an artifact whose layer declares a shape is a placeholder
   * for one on its way, and the map draws nothing rather than a rectangle that becomes the shape
   * a moment later — and it decides what may be held across principals: a derived shape is never
   * kept against a `tesseraId` across a change of principal, and the other two may be.
   */
  shape: ShapeKind | null;
  /** The kinds of supplied content its artifacts carry. */
  suppliedContent: string[];
  depsOn: string[];
  /** Echoed to notice a gate edit, in the same shape as every other version coordinate. */
  version: number;
};

/** The names a view's `projection` may take — the closed set of `projections.md` §5. */
export type ProjectionName = 'web_mercator' | 'equirectangular' | 'gall_isographic' | 'none';

/**
 * The tile schemes a view's frame may be addressed in. One: the slippy-map `z/x/y` every basemap
 * server publishes. It is a scheme rather than a flag because a frame can be aligned to a tiling
 * nobody serves — see {@link ViewInfo.tileScheme}.
 */
export type TileScheme = 'xyz';

/**
 * One declared view, and what it is a picture of (`projections.md` §9).
 *
 * The frame and the four projection fields are **deployment constants, identical for every
 * principal**, and they are what a host decides a basemap by. `projection.ts` holds the two
 * things a client does with them.
 */
export type ViewInfo = {
  id: string;
  displayName: string;
  /**
   * The extent this view's positions are quantised against — what every wire coordinate is a
   * fraction of, and what a client turns a grid unit back into a data coordinate with.
   *
   * **Per view and not per bundle** (decision 0040): two views of one bundle may quantise
   * differently — an embedding and a map cannot share a frame without one of them wasting most
   * of the grid — so a client holding one extent for the deployment would decode every position
   * of the second view against the first's ground.
   */
  quantisation: Quantisation;
  /** What placed this view's positions; `'none'` for a view that projects nothing. */
  projection: ProjectionName;
  /**
   * The ratio the world should be drawn at, width ÷ height — 1 for `web_mercator`, `2cos φ₁` for
   * an equirectangular alias, and `null` for `none`, which has no world to draw.
   */
  worldAspect: number | null;
  /**
   * The tile scheme this view's frame addresses, and **the field that decides whether a basemap
   * may be drawn**. `null` — the answer for every projection but an aligned `web_mercator` one —
   * means draw the points and draw no basemap. It is a scheme's name rather than a flag because
   * an equirectangular frame is aligned to a square tiling no server publishes, so alignment
   * alone would have a host draw a Mercator basemap under a corpus that cannot line up with one.
   */
  tileScheme: TileScheme | null;
  /** The tile this view's frame is under {@link tileScheme}. Present exactly when it is. */
  tile: {z: number; x: number; y: number} | null;
  /**
   * Where this view sits in its group's roster (`views.md` §3.2), or `null` for a plain view —
   * which has no group and no key, so the three fields are null together.
   */
  roster: ViewRoster | null;
};

/**
 * One view's roster record: its group, the caller's key, and the typed per-view metadata its
 * group declared.
 *
 * **The key is the view's only address** — `<group>:<key>` wherever a view id goes — and the
 * order of a group's views is the order {@link Meta.groups} lists them in, which is creation
 * order. A client offering previous-and-next walks that list rather than comparing keys: a key is
 * the caller's own string and means nothing to a client.
 */
export type ViewRoster = {
  group: string;
  key: string;
  /**
   * One entry per metadata name the group declared, typed. Empty on a `members` group's views,
   * whose metadata belongs to the group that owns the keys (`views.md` §3.3).
   */
  metadata: Record<string, ViewMetadataValue>;
};

/**
 * One roster metadata value, typed as the declaration typed it. `timestamp_us` is microseconds
 * since the Unix epoch — the one unit that type may hold, so a client need not guess whether a
 * large integer is a count or an instant.
 *
 * **Not an attribute**: one value per view rather than one per (entity, view), and it filters
 * nothing.
 */
export type ViewMetadataValue =
  | {type: 'bool'; value: boolean}
  | {type: 'int'; value: number}
  | {type: 'float'; value: number}
  | {type: 'text'; value: string}
  | {type: 'timestamp_us'; value: number};

/**
 * One view group: its name and its view ids in creation order (`views.md` §3.2).
 *
 * **A group is not a view** — it cannot be named on a viewer verb — and it carries no frame,
 * projection or gate of its own here: every one of those is already on each of its views, and a
 * second copy would be a second thing to disagree with the first.
 */
export type ViewGroup = {
  name: string;
  /**
   * The group whose keys these are, where this group is a second layout over
   * another's views (`views.md` §3.3); `null` where it owns them.
   */
  membersOf: string | null;
  /** This group's view ids, in creation order — the `group:key` form a request names. */
  views: string[];
};

export type Meta = {
  apiVersion: number;
  idset: number;
  /**
   * The declared views in serving order (`views.md` §3.2) — the plain views first, then each
   * group's views in creation order — each carrying its own frame, see
   * {@link ViewInfo.quantisation}.
   */
  views: ViewInfo[];
  /**
   * The view groups and their orderings — see {@link ViewGroup}. Empty where the deployment
   * declares plain views alone, which is the ordinary case.
   */
  groups: ViewGroup[];
  /** The column schema in full — see {@link DeclaredScalar}. Order is the declaration order. */
  declaredScalars: DeclaredScalar[];
  /**
   * The annotation layers this principal reaches — see {@link Layer}. Empty when it reaches none,
   * which is also what a deployment with no layers at all looks like.
   */
  layers: Layer[];
  selection: {
    kMin: number;
    kMaxMarks: number;
    maxK: number;
    thetaTargetMarks: number;
    maxUnderlayOffset: number;
    /**
     * `/v1/categories`' page ceiling and default. A client that pages needs it to tell a short
     * page that means "the set ended" from one that means "the deployment truncated".
     */
    maxCategoryValues: number;
    /** The most vertices a `region` leaf's polygon may carry; over it the request is a `422`. */
    maxRegionVertices: number;
    /**
     * The most boundary cells a `region` leaf's descent may hold at one depth. Not a refusal:
     * over it the answer is a cover, said on `x-tessera-region` ({@link RegionVerdict}).
     */
    maxRegionCells: number;
  };
  /** `serve.max_tiles_per_request` — the client's own bound when it chooses a request depth. */
  maxTilesPerRequest: number;
  /**
   * Which columns this bundle may be filtered on, and by which operators. Empty when the schema
   * declares nothing filterable.
   *
   * A client draws its filter controls from **this** rather than from {@link Meta.declaredScalars}:
   * the two lists differ, and the difference is load-bearing in both directions. A blob-resident
   * text column is filterable and never appears in a viewport response; a column with no index and
   * no render placement appears in neither list's useful half.
   */
  filterOperands: FilterOperandSet[];
};

/** One category value: what a code stands for, and how to show it. */
export type CategoryValue = {
  code: number;
  /** The stable key the code is bound to. The display fallback when there is no title. */
  key: string;
  /** Presentation, amendable without a build. Absent for every value a discovered vocabulary mints. */
  title: string | null;
};

export type ViewportRequest = {
  view: string;
  zoom: number;
  /**
   * The region to answer for. Send exactly one of this and {@link ViewportRequest.tiles} — the
   * server refuses both, and refuses neither.
   */
  bbox?: [number, number, number, number];
  /**
   * The exact depth-`zoom` Morton prefixes to answer for.
   *
   * How a client with a replica elides: a tile it can prove it already holds is simply left out,
   * and an omitted tile costs the server nothing — no range derivation, counting, selection or
   * gather. A client with no replica sends a `bbox` and needs none of this.
   */
  tiles?: bigint[];
  /**
   * Omitted unless set. Contracts §3.2 defaults `k` to the deployment's own ceiling, so a caller
   * who never mentions it can never decrease it — which is what keeps P6's non-decreasing
   * obligation off the naive path.
   */
  k?: number;
  underlayOffset?: number;
  /**
   * The {@link ViewportResponse.pin} of the response currently being displayed, echoed back so the
   * server can report whether geometry has moved since. Advisory in both directions: omitting it
   * simply means every response comes back `stale: false`.
   */
  stamp?: string | null;
  /**
   * The filter expression, or null for the unfiltered request.
   *
   * **A filter narrows what is served without changing the response's identity key**, which is the
   * one thing a caller holding a replica has to know: the key partitions by principal, credential,
   * mask and view, so bands held under one filter are *renderable* under another and will be
   * served as if they belonged. A client that changes this must drop what it holds itself — the
   * server cannot tell it to.
   */
  filters?: FilterExpr | null;
  /**
   * Which annotation layers to answer for. **Omitted or `[]` means none**; the string `'all'`
   * means every layer this principal reaches; an array is those layers ∩ the reachable set.
   *
   * **It narrows and never widens.** Naming a layer this principal cannot reach is not a way to
   * learn it exists — the response is what it would have been without the name. A client that wants
   * artifacts names the layers that are on (or `'all'`); a point-fetching client that wants none
   * sends `[]` or omits this, so a deployment with layers does not pay for them on every tile
   * request. The old convention — omitted meant *all* — was replaced server-side; do not rely on
   * omission meaning anything but *none*.
   */
  layers?: string[] | 'all';
  /**
   * Which of each named layer's declared levels to answer for.
   *
   * **Omitted follows the layer's own declaration** — the levels whose declared zoom range covers
   * this request's `zoom`, which is what `/v1/meta`'s zoom→level map has always described and which
   * nothing on the wire could previously ask for. `'all'` answers for every level; an array answers
   * for exactly those.
   *
   * **The default is the useful one and the expensive answer is the one you ask for by name** —
   * the opposite arrangement from `layers`, and deliberately: naming a layer has already opted into
   * the artifact pass, so what is left is which of its rungs to pay for. A five-level administrative
   * hierarchy answered whole at the overview is a response two orders of magnitude larger than the
   * one a client draws.
   *
   * **Inert on a layer that declares no zoom ranges**, which is every treed layer (they declare no
   * levels at all) and any levelled layer whose author declared none — those serve every level in
   * all three cases.
   *
   * A level a layer does not hold is absent from the answer rather than a refusal, the same way an
   * unreachable layer name is.
   */
  levels?: number[] | 'all';
  /**
   * How many artifacts the client wants back at most, in the same shape as `k` beside it.
   *
   * **Inert on a flat layer, and never a sample.** Artifacts are not dropped to meet a budget: the
   * only reduction the representation allows is structural — serving an ancestor in place of its
   * descendants — so a flat layer, having no ancestors to cut to, ignores this. Half the clusters
   * would be a wrong map rather than half a map.
   */
  artifactBudget?: number;
  /**
   * Which of each layer's **declared** computed properties — `'centroid'`, `'box'`, `'shape'` — the
   * response should carry.
   *
   * **Omitted is the layer's own declaration**, so a client that never thinks about geometry is
   * answered exactly as it was before this field existed. An array answers for those, intersected
   * with what each layer declared; the empty array is counts and no geometry.
   *
   * **It narrows and never widens.** Naming `'shape'` on a layer that draws none serves none.
   *
   * The reason to narrow is cost, and it is large: a hull is derived per artifact per request from
   * the members this principal can see, and a viewport carrying 197 clusters derives 197 of them
   * while the map draws one. Measured on a 2.42M-member corpus, that was 2.03 s against 0.17 s for
   * the same request asking for `['centroid', 'box']`. Ask the drill-down route
   * ({@link TesseraClient.artifact}) for the one hull that is drawn.
   */
  computed?: ComputedProperty[];
  /**
   * Which columns each served artifact row answers with (contracts §3.2 r44). Omitted or `'full'`
   * is every column; `'identity'` is the same rows in the fixed four-column schema
   * ({@link ArtifactIdentity}). **The row set, the `matched` bits and the `rung` values are
   * identical under either value; only the columns change** — which is what it is for: a filter
   * change over rows the caller already holds, the bit being the one field a filter moves.
   */
  artifactRows?: 'full' | 'identity';
};

/**
 * The three geometries a request may ask for. `shape` is the layer's one drawn geometry of
 * whichever kind {@link Meta} publishes for it — a hull, a membership shape or an authored one —
 * so a request asks for the drawing without knowing its derivation; `hull` is the declaration's
 * word (`computedContent`) and not an ask word.
 */
export type ComputedProperty = 'centroid' | 'box' | 'shape';

/** The three kinds of a layer's one drawn geometry (`polygon-membership.md` §7.1). */
export type ShapeKind = 'derived' | 'predicate' | 'authored';

/**
 * A served shape: **parts, then rings, then vertices**, in grid units. A part's first ring is
 * its outer and the rest are holes — the nesting deck's `PolygonLayer` takes — and two parts are
 * two shapes, never a shape with a gap. A derived hull is one part per α-group, with no holes.
 *
 * **A served shape is a drawing and never the predicate.** It is generalised to the pixel at the
 * zoom it was asked at and may differ from the membership by up to a cell, so a client must never
 * test a point against these rings to decide whether the point is a member: the wire's
 * `membership:<layer>` column is that answer, and the only one (`polygon-membership.md` §7.1).
 */
export type Shape = [number, number][][][];

/**
 * One tile's exact masked counts.
 *
 * These come from the server and are never recomputed, interpolated or scaled client-side.
 * `served` is what was drawn; `visible` is what exists inside the mask. Any surface showing one
 * without the other lets a sample read as a set.
 */
export type TileCounts = {tile: bigint; visible: bigint; matched: bigint; served: bigint};

export type SubCell = {cell: bigint; count: bigint};

/**
 * One annotation artifact — a cluster, a boundary, a topic — as a viewport serves it.
 *
 * **`maskedCount` is how many members *this principal* can see, and never how many the artifact
 * has.** Two principals looking at the same `tesseraId` legitimately get different numbers, and
 * neither is the artifact's size; a surface that presented it as "the cluster's size" would be
 * asserting the bug the whole mechanism exists to prevent.
 *
 * **It does not change as the map moves.** The count is over the whole membership, not over the
 * viewport — a per-viewport count would let two boxes be differenced for the members between them.
 * A panel showing it will hold steady during a pan, which is correct.
 *
 * There is deliberately no ordinal, no membership and no unmasked size on this wire, and an
 * artifact withheld from this principal is simply absent with no reason given — indistinguishable
 * from one that was never published. Do not model a "hidden" state; there is nothing to fill it
 * from.
 */
export type Artifact = {
  layer: string;
  /** Wire identity, u64 — never narrowed to a number. Stable across principals and sessions. */
  tesseraId: bigint;
  /** The publisher's own key, when they supplied one. */
  key: string | null;
  maskedCount: bigint;
  /**
   * Derived geometry, recomputed **for this principal** from the members they can see — in the
   * same grid units as `codes`, so it draws with `positionOfCode`'s arithmetic and needs no
   * extent.
   *
   * `null` means the layer declares this property, or rather does not: content is never withheld
   * from a served artifact, so a null is a fact about the layer and never about the viewer. The
   * shape moves with the principal for the same reason the count does — a narrow viewer's centroid
   * sits over the members *they* can see, which is usually not where a broad viewer's sits. Do not
   * cache one principal's geometry against a `tesseraId` and reuse it for another.
   */
  centroid: [number, number] | null;
  /** `[minX, minY, maxX, maxY]`, grid units. */
  box: [number, number, number, number] | null;
  /**
   * The artifact's one drawn geometry ({@link Shape}), of the kind its layer's {@link Meta}
   * `shape` names, or `null` where the layer draws none or the request did not ask (contracts
   * §3.2 item 4). For a derived hull, each α-group of the visible members is its own part: a
   * membership that is two separated clouds is two parts, never one polygon over the gap between
   * them, and a group of one or two members is a ring of one or two vertices — rounding one up to
   * a triangle would claim an area no member occupies. Two parts of one artifact may overlap,
   * which costs nothing: both are this artifact.
   */
  shape: Shape | null;
  /**
   * The publisher's supplied content — label text, an authored name, a polygon — as **one
   * entry of the artifact's ranked contents, entire**, positional to the layer's `suppliedContent`
   * kinds from `/v1/meta`.
   *
   * Empty means the layer declares none. It never means *withheld*: a viewer who may not read an
   * artifact's content is not served the artifact, so there is no state to render as "label
   * hidden" and nothing to fill it from. Where an artifact carries several ranked descriptions,
   * this is the one this principal qualifies for — so two principals may legitimately see
   * different text against the same `tesseraId`.
   */
  content: string[];
  /**
   * This artifact's parent, **and only ever one that is in the same response**.
   *
   * The structure to nest what you draw, or to filter to one subtree while still drawing the rest
   * of the map — which is what a levelled layer's edges are for, since its resolution comes from
   * choosing a level rather than from coarsening along them.
   *
   * **`null` means "no parent in this response", not "no parent".** It covers a root and a parent
   * this principal was not served — below its own criterion for them, suppressed, or dropped by
   * the layer's frontier — and the two are one value deliberately: distinguishing them would
   * disclose that a coarser artifact exists which they may not see. Build the tree from what you
   * were given and treat unlinked artifacts as roots of it; do not model a "hidden parent" state,
   * because there is nothing to fill it from.
   */
  parentId: bigint | null;
  /**
   * **The resolution a client draws this artifact at**, computed the right way for its layer's
   * kind (contracts §3.2 r44): the declared level on a **levelled** layer — a fact about the
   * artifact, agreeing across principals, indexing the level set `/v1/meta` publishes — the
   * **response-local parent-chain depth** on a **treed** one, computed after the cut so the root
   * of a re-rooted subtree reads 0, and `0` on a flat one.
   *
   * **A client draws by this column and never derives it.** Which derivation a layer kind wants
   * — declared level or chain count — was a documented per-client trap, fallen into once; the
   * server now serves the right number for every kind, so counting `parentId` links here answers
   * no question this field does not.
   */
  rung: number;
  /**
   * **Whether this artifact holds a member the current filter admits** — one this principal may
   * see, inside the requested tiles.
   *
   * `null` where the request carried no filter: there was no question, and `false` would answer
   * one that was never asked. Draw on the distinction — a `false` is a cluster with nothing in it
   * for this search, a `null` is every cluster as it always looked.
   *
   * **The only field here a filter moves.** Existence, `maskedCount` and the geometry are what
   * this principal may see, filter or no filter, so a filter never makes an artifact appear or
   * vanish and never changes its count.
   *
   * **Do not derive this from the points you hold**: they are a *sample* of the matches, so a
   * cluster whose few matching members were not sampled looks empty — false for exactly the small
   * clusters a filter is used to find.
   *
   * **It answers about the members in view**, where `maskedCount` and the geometry answer about
   * the whole visible membership. An artifact whose only matches sit off screen reads `false`
   * until the view moves over them.
   */
  matched: boolean | null;
};

/**
 * One row of the identity projection (`artifact_rows: "identity"`, contracts §3.2 r44): the same
 * row set a full answer to the identical request would carry, in a fixed four-column schema.
 *
 * The row set, the `matched` bits and the `rung` values are identical under either value of
 * `artifact_rows`; only the columns change. The payload columns are absent from the schema rather
 * than null, so a caller resolves each row against payloads it already holds by
 * `(layer, tesseraId)` — and one meeting an identifier its store cannot resolve knows it, and
 * re-asks with `"full"`: one round trip, never a wrong map.
 */
export type ArtifactIdentity = {
  layer: string;
  /** Wire identity, u64 — never narrowed to a number. */
  tesseraId: bigint;
  /** See {@link Artifact.rung} — identical to the full row's value. */
  rung: number;
  /** See {@link Artifact.matched} — identical to the full row's value, null with no filter. */
  matched: boolean | null;
};

export type ViewportResult = {
  tiles: TileCounts[];
  /** Wire identity, u64 — never narrowed to a number. */
  ids: BigUint64Array;
  /**
   * Each point's 64-bit Morton position code, exactly as the server sent it. Its high 32 bits are
   * the point's Morton cell and its low 32 the sub-cell residual, so `tileOfCode(code, z)` is the
   * depth-`z` tile containing it — hover bucketing and client-side clustering without a round trip.
   */
  codes: BigUint64Array;
  /**
   * Interleaved x,y in CELL space — `[0, 65536)` per axis, fractional below the cell — ready for
   * deck.gl once scaled to world space.
   *
   * `Float64Array`, not `Float32Array`: the code carries 32 bits per axis and an `f32` mantissa
   * holds 24, so narrowing here would discard precision the wire went to some trouble to deliver.
   * The narrowing happens once, at the GPU attribute (`positionsToWorld`), which is where fp64
   * emulation would later recover it.
   */
  positions: Float64Array;
  /**
   * The same points in the renderer's world space, `f32`.
   *
   * Carried beside {@link positions} rather than instead of it because the two answer different
   * questions: cell space is 32 bits per axis and round-trips to the server's `code`, which `f32`
   * cannot (see `decode.ts`); world space is what a band holds and a buffer uploads. Computing it
   * here means the per-point pass happens wherever decoding does — a worker, in a browser — rather
   * than on the thread that draws.
   */
  world: Float32Array;
  /**
   * The declared-scalar tail, one entry per column, keyed by column name.
   *
   * See {@link ScalarColumn} for why these are typed arrays rather than `unknown[]`.
   */
  scalars: Record<string, ScalarColumn>;
  /**
   * The per-point membership column per served layer (D12, contracts §3.2 r39, design §5.10),
   * already hashed to a **response-local index**: `index[i]` is `0` for a point under no served
   * artifact of that layer, else `1 + d` where `ids[d]` is the `tessera_id` of the deepest served
   * artifact holding it — always one in this response's {@link ViewportResult.artifacts}. Keyed by
   * layer name. Empty when the response served no artifacts, which is also what a request naming
   * no layers gets.
   *
   * The session ordinal a band carries is assigned on the main thread from the short `ids` list
   * (`bands.ts`); the decoder cannot name it, its lanes sharing no table.
   */
  membership: Record<string, MembershipColumn>;
  subCells: SubCell[] | null;
  /**
   * The artifacts this viewport served — empty when none did.
   *
   * **Empty and absent are one state here**, unlike {@link subCells}: the server omits the frame
   * whenever nothing qualifies, and *why* nothing qualified — no layer reachable, none intersecting
   * the view, none clearing its existence criterion — is deliberately not on the wire. Which layers
   * a principal reaches at all comes from `GET /v1/meta`.
   */
  artifacts: Artifact[];
  /**
   * The identity projection's rows, where the response answered `artifact_rows: "identity"` —
   * `null` where the frame was full or absent. Exactly one of this and a non-empty
   * {@link ViewportResult.artifacts} is populated: the projection is read off the frame's own
   * schema (four columns against the full frame's fourteen-or-more), never off the request.
   */
  artifactsIdentity: ArtifactIdentity[] | null;
};

/** One layer's membership column as the decoder hands it over — see {@link ViewportResult.membership}. */
export type MembershipColumn = {
  /** Per point, `0` for none, else one past the position in `ids`. `Uint16Array` where it fits. */
  index: Uint16Array | Uint32Array;
  /** The distinct `tessera_id`s the column named, in first-seen order. */
  ids: BigUint64Array;
};

/**
 * One decoded attribute column: its declared type, and its values in point order.
 *
 * **Typed arrays, not boxed `unknown[]`.** Arrow's own child arrays are already typed, and
 * spreading them into a JS array allocates one heap object per value per column — at 5 × 10⁴ marks
 * and eighteen columns that is nearly a million objects per response, thrown away on the next pan.
 * Handing back the underlying buffer costs nothing and is the form a GPU attribute wants.
 *
 * **The type travels with the values** rather than being looked up in `Meta`. A consumer that had
 * to re-derive it would be joining two documents on every frame, and the one that matters —
 * whether a `u16` is a category or an integer — is a mistake that renders silently wrong.
 * `arrowType` here is the *storage* type; whether it is a category is `Meta`'s answer.
 */
export type ScalarColumn =
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

export type Timings = {
  serverUs: number;
  admissionUs: number;
  /**
   * `x-tessera-stage-ns`, a positional CSV. **Null is the normal case**: the header needs both the
   * `bench-timing` cargo feature and `[serve] stage_timing = true`, so every reader must treat its
   * absence as a configuration fact rather than an error.
   */
  stageNs: number[] | null;
};

/**
 * One points frame of a streamed response, with the counts its points satisfy.
 *
 * **A part is a set of whole bands, never a fragment of one.** The server flushes at whole tiles
 * (`streamed-serving.md` §2), so the tiles here are exactly the run this part's points cover, in
 * the response's own order, and a consumer stores them exactly as it stores a whole response.
 *
 * It carries its own identity and content coordinates rather than reading them from the response,
 * because the response has not resolved yet — a part landed under one principal must never be
 * stored under another, and a part from a request a pan has superseded must be recognisable as
 * belonging to the answer it came from.
 */
export type ViewportPart = {
  result: ViewportResult;
  /** See {@link ViewportResponse.identityKey}. */
  identityKey: string;
  /** See {@link ViewportResponse.contentKey}. */
  contentKey: string;
};

export type ViewportResponse = {
  result: ViewportResult;
  timings: Timings;
  /**
   * Whether a held band may be **rendered at all** — the replica's partition key.
   *
   * Over the idset, the credential, the mask fragment's identity and the view. A cache keyed more
   * loosely than this serves one principal's authorised data to another, which is a disclosure and
   * not a staleness bug (decision 0029), so a client drops everything held when it changes.
   */
  identityKey: string;
  /**
   * Whether a held band may be **declared** in a request — the response's entity tag, unquoted.
   *
   * Moves when the visible set could have gained rows, and deliberately not when a merge or a
   * compaction rearranges the ones already there. A band whose key has rotated is still renderable;
   * what it has lost is the right to be declared (`delta-serving.md` §2).
   */
  contentKey: string;
  /**
   * The geometry this response was answered from (`x-tessera-pin`), to echo back on the next
   * request as {@link ViewportRequest.stamp}.
   *
   * **A stamp, not a selector.** It does not pin anything: the server always answers from live
   * geometry, presenting a superseded one is never an error, and nothing is retained on its
   * behalf. Its only effect is {@link ViewportResponse.stale}. The header keeps the name
   * `x-tessera-pin` for compatibility; the meaning is contracts §3.1/§3.2's advisory stamp.
   */
  pin: string | null;
  /**
   * Whether the geometry moved since the stamp this request presented — `false` when none was
   * presented.
   *
   * Advisory. The client decides what to do: refetch now, refetch on the next idle, or ignore it.
   * Nothing expires and no response is withheld while it is `true`.
   */
  stale: boolean;
  /**
   * The region leaves' verdict — `null` when the request carried none. `exact: false` sets
   * `Masked.exact` false on every number the region produced.
   */
  region: RegionVerdict | null;
  bytes: number;
};

/**
 * One item's whole record, keyed by declared column name — see {@link TesseraClient.item}.
 *
 * A category arrives already resolved to its vocabulary key, and a column the item carries no value
 * for is absent from `fields` rather than present as null.
 */
export type ItemDetail = {fields: Record<string, unknown>; externalId: string | null};

/**
 * One artifact opened by identifier — see {@link TesseraClient.artifact}.
 *
 * The same three facts the viewport carried, from the same predicate: an artifact openable but not
 * drawable, or the reverse, would be that rule transcribed twice. Notably **no membership and no
 * declared size**: what a drill-down adds over the wire's own row is a name for the layer, not a
 * way behind the count.
 */
export type ArtifactDetail = {
  layer: string;
  key: string | null;
  maskedCount: bigint;
  /**
   * The artifact's declared geometry, computed for this principal — the same values and the same
   * grid units the viewport's artifacts frame carries, from the same predicate. `null` where the
   * layer declares none.
   *
   * **This route is where a shape is fetched from.** The viewport asks for centroids and boxes
   * and this asks for the one shape that draws, which is what the drawing has always needed and
   * what the viewport was paying 197× over to supply (see {@link ViewportRequest.computed}).
   */
  centroid: [number, number] | null;
  box: [number, number, number, number] | null;
  shape: Shape | null;
};
