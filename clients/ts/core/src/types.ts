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
   * The value set this column's codes index. Two columns may share one — keys, codes and labels
   * are shared with it, so a resolved palette may be reused across them.
   *
   * **Visibility may not be reused across them.** Member sets are per column, so a value visible
   * under one column may be invisible under another that shares this vocabulary.
   */
  vocabulary: string;
  /** Whether the value set is closed at build (`declared`) or grows from the corpus. */
  kind: 'declared' | 'discovered';
  /**
   * Whether the *existence* of a value is sensitive. `per_viewer` means `/v1/categories` filters
   * the set per principal — and today refuses it, the predicate being unbuilt.
   */
  listing: 'per_viewer' | 'public';
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
  | {[column: string]: FilterOperator};

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
  hierarchy: {kind: 'flat' | 'nested' | 'stacked'; pruneChildren: boolean};
  /**
   * The resolutions the layer declares. **Empty for a treed layer**, which declares none: its
   * lineage is in its edges, and a level number would say nothing about position in it.
   */
  levels: {level: number; title: string; zoom: [number, number] | null}[];
  /** The derived vocabulary a client must know to draw anything the layer's artifacts carry. */
  derivedContent: string[];
  /** The kinds of supplied content its artifacts carry. */
  suppliedContent: string[];
  depsOn: string[];
  /** Echoed to notice a gate edit, in the same shape as every other version coordinate. */
  version: number;
};

export type Meta = {
  apiVersion: number;
  idset: number;
  views: {id: string; displayName: string}[];
  quantisation: Quantisation;
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
  /** The stable key the code is bound to. The display fallback when there is no label. */
  key: string;
  /** Presentation, amendable without a build. Absent for every value a discovered vocabulary mints. */
  label: string | null;
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
   * Which annotation layers to answer for. Omitted means every layer this principal reaches; `[]`
   * means none, and costs the server nothing.
   *
   * **It narrows and never widens.** Naming a layer this principal cannot reach is not a way to
   * learn it exists — the response is what it would have been without the name. A client fetching
   * points it will not draw artifacts against should send `[]` rather than omitting this, so a
   * deployment with layers does not pay for them on every tile request.
   */
  layers?: string[];
  /**
   * How many artifacts the client wants back at most, in the same shape as `k` beside it.
   *
   * **Inert on a flat layer, and never a sample.** Artifacts are not dropped to meet a budget: the
   * only reduction the representation allows is structural — serving an ancestor in place of its
   * descendants — so a flat layer, having no ancestors to cut to, ignores this. Half the clusters
   * would be a wrong map rather than half a map.
   */
  artifactBudget?: number;
};

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
  stableKey: string | null;
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
  /** Convex hull vertices, counter-clockwise, grid units. */
  hull: [number, number][] | null;
  /**
   * The publisher's supplied content — label text, an authored name, a polygon — as **one
   * variation, entire**, positional to the layer's `suppliedContent` kinds from `/v1/meta`.
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
export type ArtifactDetail = {layer: string; stableKey: string | null; maskedCount: bigint};
