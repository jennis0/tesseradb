export type Session = {token: string; tokenId: number; expiresAt: number};

export type Quantisation = {xMin: number; xMax: number; yMin: number; yMax: number};

export type Meta = {
  apiVersion: number;
  idset: number;
  slices: {id: string; displayName: string}[];
  quantisation: Quantisation;
  declaredScalars: {name: string; arrowType: string}[];
  selection: {
    kMin: number;
    kMaxMarks: number;
    maxK: number;
    thetaTargetMarks: number;
    maxUnderlayOffset: number;
  };
  /** `serve.max_tiles_per_request` — the client's own bound when it chooses a request depth. */
  maxTilesPerRequest: number;
};

export type ViewportRequest = {
  slice: string;
  zoom: number;
  bbox: [number, number, number, number];
  /**
   * Omitted unless set. Contracts §3.2 defaults `k` to the deployment's own ceiling, so a caller
   * who never mentions it can never decrease it — which is what keeps P6's non-decreasing
   * obligation off the naive path.
   */
  k?: number;
  underlayOffset?: number;
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
   * the point's cell, so `code >> BigInt(32 - 2 * z)` is the depth-`z` tile containing it — hover
   * bucketing and client-side clustering without a round trip.
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
  scalars: Record<string, unknown[]>;
  subCells: SubCell[] | null;
};

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
  pin: string | null;
  bytes: number;
};

export type ItemDetail = {scalars: unknown[]; externalId: string | null};
