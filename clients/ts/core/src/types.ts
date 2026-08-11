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
   * `/v1/categories/{column}`. Unique bundle-wide, and not slice-qualified.
   */
  name: string;
  arrowType: ArrowType;
  category: CategoryDescriptor | null;
};

export type Meta = {
  apiVersion: number;
  idset: number;
  slices: {id: string; displayName: string}[];
  quantisation: Quantisation;
  /** The column schema in full — see {@link DeclaredScalar}. Order is the declaration order. */
  declaredScalars: DeclaredScalar[];
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
  slice: string;
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
   * Over the idset, the credential, the mask fragment's identity and the slice. A cache keyed more
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

export type ItemDetail = {scalars: unknown[]; externalId: string | null};
