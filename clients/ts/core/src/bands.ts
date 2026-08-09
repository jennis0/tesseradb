import {WORLD_SIZE, tileContains, tileXY} from './coords.js';
import type {ScalarColumn, ViewportResult} from './types.js';
import {
  coverageAdd,
  coverageAt,
  rectArea,
  rectContainsTile,
  rectSubtractAll,
  rectsIntersect,
  type Coverage,
  type TileRect
} from './rects.js';

/**
 * The replica: what a client holds, as per-tile bands of points.
 *
 * **The unit is the (tile, cut) band, not the point** (`caching.md` §6). Design §7.2 serves a
 * prefix of each tile's visible set in `tessera_id` order, and those prefixes nest across depth, so
 * a point served at depth 3 is served again by every deeper fetch covering it. Storing points
 * independently would carry per-point metadata over 10^7 points and fight the nesting the sampler
 * already paid for; storing prefixes means one bound per tile describes everything held.
 *
 * That bound is what a client declares (`delta-serving.md` §3): *I hold every member of this tile's
 * visible set with `tessera_id` below X*. It is an identity bound rather than a count because the
 * same value is meaningful at every depth — one declaration against a parent answers for all four
 * children — and because a band assembled from several responses still describes itself exactly.
 *
 * Nothing here fetches. This is the replica store `client-interaction.md` §10 places above the
 * stateless client, and it is in `core` because its rules are invariant-bearing: the eviction
 * order, the prefix-only truncation, and the identity-key partition are each load-bearing and each
 * testable without a browser.
 */

/** A tile's address: its Morton prefix at a depth. Depth is not recoverable from the prefix. */
export type TileAddress = {depth: number; prefix: bigint};

/** `${depth}:${prefix}` — the map key. Depth leads so a scan can stop at a depth boundary. */
export type BandKey = string;

export function bandKey(depth: number, prefix: bigint): BandKey {
  return `${depth}:${prefix}`;
}

/**
 * One tile's held points, ascending by `tessera_id` — the wire's own order (contracts §3.2), kept
 * because every operation here is a prefix operation over it.
 *
 * **The arrays are copies, never views onto a response buffer.** A `subarray` keeps the whole
 * response alive, so evicting a band would free nothing and `bytes` would be fiction — and the
 * byte ledger is the only thing standing between a 30-minute session and 2–6 GB of accumulation
 * (`caching.md` §6).
 */
export type Band = {
  depth: number;
  prefix: bigint;
  /**
   * The tile's `(x, y)` index, de-interleaved once when the band is built.
   *
   * **Every region query needs it and none of them should pay for it.** Recovering it from the
   * prefix is a per-bit `BigInt` loop, and `bandsForRegion` runs over every held band on every
   * redraw — so at 2.4 × 10^4 bands that was ~2.4 × 10^5 `BigInt` allocations per frame, measured
   * at 14–20 ms of redraw for a view drawing as few as 5,700 marks. It scaled with what the cache
   * *held* rather than with what was drawn, which is why a small mark budget did not help.
   */
  x: number;
  y: number;
  ids: BigUint64Array;
  /**
   * Interleaved x,y in **deck.gl world space**, `f32` — two entries per point.
   *
   * Converted once when the band is built rather than on every assembly. The wire carries `f64`
   * cell space and the renderer wants `f32` world space; doing that per point per redraw is the
   * single most expensive thing in the render path, and it produces the same numbers every time.
   * Storing the converted form also halves the bytes: 8 per point rather than 16.
   */
  positions: Float32Array;
  scalars: Record<string, ScalarColumn>;
  /** `m(T)` as the server reported it: how many points the definition serves for this tile. */
  served: number;
  /** `min(k, k_max_marks)` in force when this band was fetched — see {@link isComplete}. */
  capUsed: number;
  visible: bigint;
  matched: bigint;
  /** The declaration: every held identity is strictly below this. `0n` for an empty band. */
  heldBelow: bigint;
  identityKey: string;
  contentKey: string;
  bytes: number;
  touchedAt: number;
};

/**
 * Does this band hold the whole of `served(T)`, and will it still at `k`?
 *
 * θ is viewport-invariant — design §7.2's threshold depends on the mask, the generation and the
 * slice, never on the bounding box or the zoom — so at a fixed content key and fixed depth `m(T)`
 * does not move, and the `served` the server already reported is still current. Two clauses:
 *
 * 1. the band holds every point the definition serves; and
 * 2. `served` is below the cap that was in force, so the **cap was not the binding clause** and a
 *    larger `k` cannot grow `m(T)`. Where `served` equals that cap the cap *was* binding, and a
 *    larger `k` yields more — so the tile must be requested again.
 *
 * A tile passing both need not appear in a request at all, which is the only mechanism that makes
 * server work scale with novelty rather than with viewport area (`delta-serving.md` §1).
 */
export function isComplete(band: Band, contentKey: string, k: number): boolean {
  if (band.contentKey !== contentKey) return false;
  if (band.ids.length !== band.served) return false;
  return band.served < band.capUsed || band.capUsed >= k;
}

/** How many bytes a band's buffers occupy, for the ledger eviction runs against. */
function bandBytes(
  ids: BigUint64Array,
  positions: Float32Array,
  scalars: Record<string, ScalarColumn>
): number {
  let bytes = ids.byteLength + positions.byteLength;
  for (const column of Object.values(scalars)) {
    bytes += scalarBytes(column);
  }
  return bytes;
}

function scalarBytes(column: ScalarColumn): number {
  // `bool` and `utf8` decode to boxed arrays rather than typed ones. Their true cost is a heap
  // object per value; the estimates here are deliberately generous rather than accurate, because
  // undercounting them is what would let the ledger drift above the bound it exists to hold.
  if (column.arrowType === 'bool') return column.values.length * 4;
  if (column.arrowType === 'utf8') {
    let bytes = 0;
    for (const value of column.values) bytes += 40 + value.length * 2;
    return bytes;
  }
  return column.values.byteLength;
}

function sliceScalars(
  scalars: Record<string, ScalarColumn>,
  from: number,
  to: number
): Record<string, ScalarColumn> {
  const out: Record<string, ScalarColumn> = {};
  for (const [name, column] of Object.entries(scalars)) {
    out[name] = {arrowType: column.arrowType, values: column.values.slice(from, to)} as ScalarColumn;
  }
  return out;
}

/**
 * Split a response into bands, one per tile it reports.
 *
 * The wire orders points by tile in the order the tile stream lists them, and `served` gives each
 * tile's length — so this is a prefix-sum walk, and `served` is the *only* way to recover the
 * grouping without recomputing the selection (contracts §3.2).
 *
 * Tiles reporting no served points yield no band: an empty band would declare a bound of zero,
 * which is what a client with nothing declares anyway.
 */
export function bandsOfResult(
  result: ViewportResult,
  depth: number,
  meta: {identityKey: string; contentKey: string; capUsed: number; now: number}
): Band[] {
  const bands: Band[] = [];
  let offset = 0;
  for (const tile of result.tiles) {
    const served = Number(tile.served);
    if (served === 0) continue;
    const end = offset + served;
    const ids = result.ids.slice(offset, end);
    // Already in world space — the decoder produced it, which in a browser means a worker did.
    const positions = result.world.slice(offset * 2, end * 2);
    const scalars = sliceScalars(result.scalars, offset, end);
    const {x, y} = tileXY(tile.tile, depth);
    bands.push({
      depth,
      prefix: tile.tile,
      x,
      y,
      ids,
      positions,
      scalars,
      served,
      capUsed: meta.capUsed,
      visible: tile.visible,
      matched: tile.matched,
      heldBelow: ids.length === 0 ? 0n : ids[ids.length - 1]! + 1n,
      identityKey: meta.identityKey,
      contentKey: meta.contentKey,
      bytes: bandBytes(ids, positions, scalars),
      touchedAt: meta.now
    });
    offset = end;
  }
  return bands;
}

/** What a band contributes to a render, and on what authority. */
export type Provenance = 'exact' | 'ancestor' | 'descendants';

export type Resolved = {
  provenance: Provenance;
  bands: Band[];
  /**
   * False where the points came from anywhere but this tile at this depth, in which case the drawn
   * marks are a superset of `served(T)`. Presentation only: no number-channel value may be shown
   * against such a tile, and the drawn-count assertion does not range over it
   * (`delta-serving.md` §7).
   */
  exact: boolean;
};

export type PlannedRequest = {
  /** The novel regions, in tile-index space at the planned depth. Empty means nothing to ask for. */
  fetch: TileRect[];
  /** Tiles the wanted region spans, and how many of them the request covers — for reporting only. */
  wanted: number;
  novel: number;
};

export type EvictionFocus = {depth: number; prefix: bigint};

/**
 * The held bands for one principal, under one byte budget.
 *
 * Partitioned by identity key: a change of principal drops the whole partition rather than
 * filtering it, so cross-principal reuse is impossible by construction rather than by discipline
 * (`client-interaction.md` §10). A cache keyed too loosely here serves one principal's authorised
 * data to another — a disclosure, not a staleness bug (decision 0029).
 */
export class BandCache {
  private bands = new Map<BandKey, Band>();
  /**
   * The regions this client has asked for and absorbed the answer to.
   *
   * **This is how emptiness is cached, and caching emptiness is what makes any of the rest work.**
   * A response omits a tile whose visible count is zero, so without a record of having asked, every
   * empty tile is re-requested on every view — and a viewport is overwhelmingly empty tiles:
   * measured on the 2.4M demo corpus, 454 of 16,524 tiles in a settled view carry any data at all.
   * The other 16,070 would put a request on the wire forever and no revisit would ever be free.
   *
   * **Rectangles rather than one entry per tile**, which an earlier version of this did. That
   * version was unbounded — one map entry per empty tile ever looked at, ~16k per viewport, never
   * evicted and never counted against `budgetBytes` — and it forced planning to enumerate every
   * tile in the viewport to consult it. A covered rectangle asserts the same thing over its whole
   * area in four integers: *we asked here and absorbed the answer, so anything we were not sent is
   * empty*.
   */
  private covered: Coverage[] = [];
  private identityKey: string | null = null;
  private held = 0;

  constructor(private readonly budgetBytes: number) {}

  get bytes(): number {
    return this.held;
  }

  /**
   * Points held, which is the figure to size a replica against — bytes hide how much of the budget
   * is per-band overhead rather than payload, and bands here are small (`m_target` is single
   * digits, so a band is ~10 points) so that overhead is not a rounding error.
   */
  get points(): number {
    let n = 0;
    for (const band of this.bands.values()) n += band.ids.length;
    return n;
  }

  get bandCount(): number {
    return this.bands.size;
  }

  get size(): number {
    return this.bands.size;
  }

  get(depth: number, prefix: bigint): Band | undefined {
    return this.bands.get(bandKey(depth, prefix));
  }

  /**
   * Admit a band, dropping every other principal's first.
   *
   * A band whose content key differs from the held one **replaces** it rather than merging into it.
   * Unioning would let an item suppressed since the held band was fetched survive into a band the
   * client now marks fresh — a client-side fail-open, and against the property that the server
   * names the complete served set, so an item it does not name is one the client drops
   * (`delta-serving.md` §7).
   */
  put(band: Band): void {
    if (this.identityKey !== band.identityKey) {
      this.dropIdentity();
      this.identityKey = band.identityKey;
    }
    const key = bandKey(band.depth, band.prefix);
    const previous = this.bands.get(key);
    if (previous) this.held -= previous.bytes;
    this.bands.set(key, band);
    this.held += band.bytes;
  }

  /**
   * Record that a region was asked for and its answer absorbed.
   *
   * Called only *after* the response's bands are in, never before: a region marked covered before
   * its points are held would let the next plan omit tiles whose data never arrived.
   */
  markCovered(rect: TileRect, depth: number, contentKey: string, capUsed: number): void {
    this.covered = coverageAdd(this.covered, {rect, depth, contentKey, capUsed});
  }

  /** Regions held at this depth, content key and cap — the holes a plan subtracts. */
  coverageFor(depth: number, contentKey: string, k: number): TileRect[] {
    return coverageAt(this.covered, depth, contentKey, k);
  }

  /** Drop everything. Called on a token change, where the whole partition becomes unrenderable. */
  dropIdentity(): void {
    this.bands.clear();
    this.covered = [];
    this.identityKey = null;
    this.held = 0;
  }

  /**
   * Decide, for a tile set at one depth, what need not be asked for and what must be.
   *
   * Declarations are computed **from the cache**, never from a record of what the server named:
   * eviction is normal operation, and a client that declared a band it had since evicted would get
   * a silent hole (`caching.md` §7.1). Understating is safe in the other direction — it costs
   * bytes, never correctness.
   */
  planRegion(want: TileRect, depth: number, contentKey: string, k: number): PlannedRequest {
    const wanted = rectArea(want);

    // A counts-only request (`k = 0`) subtracts nothing. Its whole purpose is to refresh the number
    // channel and the content key over ground the client already holds — which is what keeps the
    // staleness bound reachable once look-ahead has emptied the request (`delta-serving.md` §8).
    if (k === 0) return {fetch: [want], wanted, novel: wanted};

    // **At most two pieces, because each piece is a request.** The per-request floor is ~170 µs and
    // a tile the client already holds costs the server essentially nothing to be asked for again —
    // so past two, one slightly-too-large request beats four exact ones. Measured the wrong way
    // round first: unbounded subtraction turned a single pan into six requests.
    const fetch = rectSubtractAll(want, this.coverageFor(depth, contentKey, k), 2);
    return {fetch, wanted, novel: fetch.reduce((n, r) => n + rectArea(r), 0)};
  }

  /**
   * The bands to draw for a region, and on what authority.
   *
   * **Iterates what is held, not what is wanted**, which is the whole reason this is affordable: a
   * settled viewport spans ~16.5k tiles of which ~450 carry any data, so walking the held bands is
   * two orders of magnitude cheaper than walking the viewport and asking about each tile.
   *
   * Exact bands come from the region the client has covered at this depth. Fallbacks — an ancestor
   * band restricted by prefix, or held descendants — are admitted **only over the part of the
   * region that is not covered**, which is what keeps them from double-drawing ground an exact band
   * already answers. Both are supersets of `served(T)` and are presentation, never selection
   * (`caching.md` §6, I7): the caller stale-marks them and shows no count against them.
   */
  bandsForRegion(
    want: TileRect,
    depth: number,
    contentKey: string,
    k: number
  ): {exact: Band[]; fallback: Band[]} {
    const uncovered = rectSubtractAll(want, this.coverageFor(depth, contentKey, k));
    const exact: Band[] = [];
    const fallback: Band[] = [];

    for (const band of this.bands.values()) {
      if (band.depth === depth) {
        if (rectContainsTile(want, band.x, band.y)) exact.push(band);
        continue;
      }
      if (uncovered.length === 0) continue; // the view is wholly held; nothing to fall back for

      // Project the band's tile onto this depth's grid and admit it only where the view is not
      // already answered. An ancestor covers a block; a descendant collapses to a single tile.
      const shift = Math.abs(band.depth - depth);
      const {x, y} = band;
      const box: TileRect =
        band.depth < depth
          ? {x0: x << shift, y0: y << shift, x1: ((x + 1) << shift) - 1, y1: ((y + 1) << shift) - 1}
          : {x0: x >> shift, y0: y >> shift, x1: x >> shift, y1: y >> shift};
      if (uncovered.some((r) => rectsIntersect(r, box))) fallback.push(band);
    }
    return {exact, fallback};
  }

  /**
   * The best available answer for a tile: its own band, else the nearest ancestor holding one, else
   * whatever descendants are held.
   *
   * Both fallbacks draw a superset of `served(T)` and are presentation, never selection
   * (`caching.md` §6, I7). The caller must stale-mark them and show no count against them.
   */
  resolve(depth: number, prefix: bigint): Resolved | null {
    const exact = this.get(depth, prefix);
    if (exact) return {provenance: 'exact', bands: [exact], exact: true};

    for (let d = depth - 1; d >= 0; d--) {
      const ancestor = this.get(d, prefix >> BigInt(2 * (depth - d)));
      if (ancestor) return {provenance: 'ancestor', bands: [ancestor], exact: false};
    }

    const descendants: Band[] = [];
    for (const band of this.bands.values()) {
      if (band.depth > depth && tileContains(prefix, depth, band.prefix, band.depth)) {
        descendants.push(band);
      }
    }
    if (descendants.length > 0) return {provenance: 'descendants', bands: descendants, exact: false};
    return null;
  }

  /**
   * The same, to a *region* rather than a single tile — what a zoom-in fallback actually needs.
   *
   * **Tested in world space, not in identity space.** A tile rectangle at a depth is a world-space
   * box, and the band already holds each point's world position, so containment is four `f32`
   * comparisons. Deriving each point's tile instead — a `BigInt` shift and a per-bit `BigInt` loop
   * — is around twenty `BigInt` allocations per point, which at 10^6 points is the difference
   * between a redraw and a two-second freeze. This is the zoom path, so it runs on exactly the
   * interaction least able to afford it.
   */
  static restrictToRect(band: Band, depth: number, rect: TileRect): number[] {
    const span = WORLD_SIZE / 2 ** depth;
    const x0 = rect.x0 * span;
    const x1 = (rect.x1 + 1) * span;
    const y0 = rect.y0 * span;
    const y1 = (rect.y1 + 1) * span;
    const indices: number[] = [];
    const p = band.positions;
    for (let i = 0; i < band.ids.length; i++) {
      const x = p[i * 2]!;
      const y = p[i * 2 + 1]!;
      if (x >= x0 && x < x1 && y >= y0 && y < y1) indices.push(i);
    }
    return indices;
  }

  /**
   * Evict to the budget by **truncating band tails**, deepest / least-recently-touched /
   * farthest-from-focus first.
   *
   * **Never the head.** Coarse points are the low-identity head of every band, so keeping heads is
   * what keeps overview rendering from blanking — and it delivers "top-level points never get
   * bumped" without any per-point priority bookkeeping (`caching.md` §6). Truncating rather than
   * dropping is why: a band reduced to its head still answers for the zoomed-out view and still
   * declares a sound, lower bound.
   *
   * Runs to a low-water mark rather than to the budget exactly, so a steady stream of `put`s does
   * not re-sort the whole cache on each one.
   */
  evict(focus: EvictionFocus, lowWaterFraction = 0.9): void {
    if (this.held <= this.budgetBytes) return;
    const target = this.budgetBytes * lowWaterFraction;

    const order = [...this.bands.values()].sort((a, b) => {
      if (a.depth !== b.depth) return b.depth - a.depth;
      if (a.touchedAt !== b.touchedAt) return a.touchedAt - b.touchedAt;
      return Number(distance(b, focus) - distance(a, focus));
    });

    for (const band of order) {
      if (this.held <= target) return;
      const keep = Math.max(1, Math.floor(band.ids.length / 2));
      if (keep >= band.ids.length) continue;
      this.truncate(band, keep);
    }
  }

  /** Cut a band to its first `keep` points, lowering its bound to match exactly. */
  /**
   * Withdraw the coverage claim over a tile.
   *
   * **Eviction must retract coverage or it becomes a silent hole.** A covered rectangle asserts
   * *we asked here and hold the answer*; once a band inside it has been truncated that is no longer
   * true, and a plan would go on subtracting the region so the discarded points were never fetched
   * again. The client would draw short for the rest of the session and nothing would say so.
   *
   * The whole containing rectangle goes, not the tile's share of it — a rectangle minus a point is
   * not a rectangle, and the alternative is to start storing the holes. It is self-healing and it
   * costs a refetch of ground the cache had already decided to give up.
   */
  private retractCoverage(depth: number, x: number, y: number): void {
    this.covered = this.covered.filter(
      (c) => c.depth !== depth || !rectContainsTile(c.rect, x, y)
    );
  }

  private truncate(band: Band, keep: number): void {
    this.retractCoverage(band.depth, band.x, band.y);
    const ids = band.ids.slice(0, keep);
    const positions = band.positions.slice(0, keep * 2);
    const scalars = sliceScalars(band.scalars, 0, keep);
    const bytes = bandBytes(ids, positions, scalars);
    this.held += bytes - band.bytes;
    this.bands.set(bandKey(band.depth, band.prefix), {
      ...band,
      ids,
      positions,
      scalars,
      heldBelow: ids[keep - 1]! + 1n,
      bytes
    });
  }
}

/** Morton distance from a band to the focus tile, at the focus's depth. Ties are broken by depth. */
function distance(band: Band, focus: EvictionFocus): bigint {
  const at = band.depth >= focus.depth ? band.prefix >> BigInt(2 * (band.depth - focus.depth)) : band.prefix;
  const delta = at - focus.prefix;
  return delta < 0n ? -delta : delta;
}
