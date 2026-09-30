import {MAX_DEPTH, WORLD_SIZE, tileContains, tileXY} from './coords.js';
import {NO_ORDINAL, type ArtifactRef, type SessionArtifactTable} from './artifactTable.js';
import type {ScalarColumn, ViewportResult} from './types.js';
import {
  coverageAdd,
  coverageAt,
  rectArea,
  rectContainsTile,
  rectIntersection,
  rectSubtractAll,
  type Coverage,
  type TileRect
} from './rects.js';

/**
 * The replica: the points a client holds, as one band per tile.
 *
 * The server sends a prefix of each tile's visible set in `tessera_id` order, and the prefixes
 * nest across depth: a point served at depth 3 is served again by every deeper fetch that covers
 * it. A band is such a prefix, so one bound describes everything held for a tile: every member of
 * its visible set with `tessera_id` below `heldBelow`. The bound is an identity rather than a count
 * because it means the same at every depth, so one declaration against a parent answers for its
 * four children.
 *
 * Nothing here fetches. Eviction keeps a prefix of each band, and a change of principal drops
 * every band.
 */

/** A tile's address: its Morton prefix at a depth. Depth is not recoverable from the prefix. @internal */
export type TileAddress = {depth: number; prefix: bigint};

/** `${depth}:${prefix}`, the map key. @internal */
export type BandKey = string;

/** @internal */
export function bandKey(depth: number, prefix: bigint): BandKey {
  return `${depth}:${prefix}`;
}

/**
 * One tile's held points, ascending by `tessera_id` as the server sent them. A {@link Composition}
 * and the store's `marks` projection hold bands by reference. Every array is the band's own copy.
 *
 * @category Projections
 */
export type Band = {
  // Every array is a copy. A view onto the response buffer would keep the whole response alive,
  // so evicting the band would free nothing and `bytes` would be wrong.
  /** The tile's depth, 0 to 16. */
  depth: number;
  /** The tile's Morton prefix at `depth`. */
  prefix: bigint;
  // `x` and `y` are kept because recovering them from `prefix` is a `BigInt` loop per bit, and
  // region queries run over every held band on every redraw.
  /** The tile's column index at `depth`. */
  x: number;
  /** The tile's row index at `depth`. */
  y: number;
  /** Each point's `tessera_id`, ascending. */
  ids: BigUint64Array;
  /**
   * Each point's position in deck.gl world space: `x` and `y` interleaved, two `f32` entries a
   * point.
   */
  positions: Float32Array;
  /** The rendered columns the response carried, by name, one value per point. */
  scalars: Record<string, ScalarColumn>;
  /**
   * For each layer the request named, each point's artifact ordinal on the session's
   * {@link SessionArtifactTable}, `0` for a point under no served artifact of that layer. A layer
   * missing here was not named when the band was fetched, so the band has no colour for it yet. A
   * layer turned off keeps its column until the band is evicted.
   */
  membership: Record<string, BandMembership>;
  /**
   * One byte a point: `1` where the point satisfies the request's `highlight`, `0` where it does
   * not. `null` where the request carried no highlight, and then nothing is dulled. A change of
   * highlight fetches the band again, so the bits answer the highlight being drawn.
   */
  highlightBits: Uint8Array | null;
  /**
   * The tile's `served` count: how many points the server serves for this tile at this depth.
   * Equal to `ids.length` unless eviction has cut the band short.
   */
  served: number;
  /** The `k` the band was fetched with, which decides whether a request at a larger `k` could serve more. */
  capUsed: number;
  /** How many items this principal may see in the tile. */
  visible: bigint;
  /** How many of `visible` match the request's filter. */
  matched: bigint;
  /** How many of `matched` also match the request's highlight; equal to `matched` where none is set. */
  highlighted: bigint;
  /** One past the highest `tessera_id` held, so every id in `ids` is below it. `0n` for an empty band. */
  heldBelow: bigint;
  /** The identity key of the response the band came from. */
  identityKey: string;
  /** The content key of the response the band came from. */
  contentKey: string;
  /**
   * Bytes the band's arrays occupy, the figure the replica's byte budget counts. Text and boolean
   * columns are estimated high.
   */
  bytes: number;
  /**
   * When the request that fetched the band started, in milliseconds on the store's clock
   * (`performance.now()` unless the store was given a `clock`). Eviction takes the least recently
   * touched first within a depth.
   */
  touchedAt: number;
};

/**
 * One layer's membership column on a {@link Band}.
 *
 * @category Projections
 */
export type BandMembership = {
  /** Each point's artifact ordinal on the session's {@link SessionArtifactTable}, `0` for none. */
  ordinals: Uint32Array;
  /**
   * The distinct non-zero ordinals in `ordinals`, ascending. The band holds one reference on the
   * table for each, released when the band is evicted or cut short.
   */
  distinct: Uint32Array;
};

/** The distinct non-zero ordinals of a slice, ascending: a band's references on the table. @internal */
export function distinctOrdinals(ordinals: Uint32Array): Uint32Array {
  const seen = new Set<number>();
  for (let i = 0; i < ordinals.length; i++) {
    const o = ordinals[i]!;
    if (o !== NO_ORDINAL) seen.add(o);
  }
  return Uint32Array.from(seen).sort();
}

/** How many bytes a band's buffers occupy, the figure eviction counts. */
function bandBytes(
  ids: BigUint64Array,
  positions: Float32Array,
  scalars: Record<string, ScalarColumn>,
  membership: Record<string, BandMembership>,
  highlightBits: Uint8Array | null
): number {
  let bytes = ids.byteLength + positions.byteLength;
  for (const column of Object.values(scalars)) {
    bytes += scalarBytes(column);
  }
  for (const m of Object.values(membership)) bytes += m.ordinals.byteLength + m.distinct.byteLength;
  bytes += highlightBits?.byteLength ?? 0;
  return bytes;
}

function scalarBytes(column: ScalarColumn): number {
  // `bool` and `utf8` decode to arrays of heap values. The estimates err high, so the ledger does
  // not report less than is held.
  if (column.arrowType === 'bool') return column.values.length * 4 + (column.present?.byteLength ?? 0);
  if (column.arrowType === 'utf8') {
    let bytes = 0;
    for (const value of column.values) bytes += 40 + value.length * 2;
    return bytes + (column.present?.byteLength ?? 0);
  }
  return column.values.byteLength + (column.present?.byteLength ?? 0);
}

function sliceScalars(
  scalars: Record<string, ScalarColumn>,
  from: number,
  to: number
): Record<string, ScalarColumn> {
  const out: Record<string, ScalarColumn> = {};
  for (const [name, column] of Object.entries(scalars)) {
    const sliced = {arrowType: column.arrowType, values: column.values.slice(from, to)} as ScalarColumn;
    if (column.present) sliced.present = column.present.slice(from, to);
    out[name] = sliced;
  }
  return out;
}

/** A resumable split of a response into bands; see {@link bandSplitter}. */
export type BandSplitter = {
  done(): boolean;
  /** Builds bands until `performance.now()` passes `deadline`. Returns at least one band while any remain. */
  step(deadline: number): Band[];
};

/**
 * One layer's membership column, mapped to session ordinals. The decoder's distinct ids map
 * through the table once per response, and each band's points are remapped from response-local
 * index to ordinal as the band is built.
 *
 * The response holds one reference per distinct ordinal until the split completes, so no ordinal
 * it names is recycled between two slices.
 */
type ResponseNaming = {
  layer: string;
  index: Uint16Array | Uint32Array;
  /** Response-local index to session ordinal; `map[0] = 0`. */
  map: Uint32Array;
  /** Scratch over local indices, for collecting a band's distinct ordinals without a `Set`. */
  mark: Uint8Array;
};

function nameResponse(result: ViewportResult, table: SessionArtifactTable): {naming: ResponseNaming[]; generation: number; release: () => void} {
  // Parent links and centroids come from this response's artifacts frame, so a band can be coloured
  // from the response that carried it.
  const frameOf = new Map<string, {parentIds: readonly bigint[]; centroid: readonly [number, number] | null; rung: number}>();
  for (const a of result.artifacts) frameOf.set(`${a.layer} ${a.tesseraId}`, {parentIds: a.parentIds, centroid: a.centroid, rung: a.rung});
  const naming: ResponseNaming[] = [];
  const held: Uint32Array[] = [];
  const generation = table.generation;
  for (const [layer, column] of Object.entries(result.membership)) {
    const refs: ArtifactRef[] = [];
    for (let d = 0; d < column.ids.length; d++) {
      const id = column.ids[d]!;
      const known = frameOf.get(`${layer} ${id}`);
      // `rung` is the wire's where the artifacts frame carries the artifact, and absent otherwise; it
      // is not counted from parent links.
      refs.push({tesseraId: id, layer, parentIds: known?.parentIds ?? [], centroid: known?.centroid ?? null, rung: known?.rung});
    }
    const ordinals = table.take(refs);
    held.push(ordinals);
    const map = new Uint32Array(column.ids.length + 1);
    map.set(ordinals, 1);
    naming.push({layer, index: column.index, map, mark: new Uint8Array(column.ids.length + 1)});
  }
  return {
    naming,
    generation,
    release: () => {
      for (const ordinals of held) table.release(ordinals);
    }
  };
}

/** One band's ordinals for one layer, and the distinct list it retains on the table. */
function remapBand(n: ResponseNaming, from: number, to: number, table: SessionArtifactTable): BandMembership {
  const ordinals = new Uint32Array(to - from);
  const {index, map, mark} = n;
  let distinctCount = 0;
  for (let i = from; i < to; i++) {
    const local = index[i]!;
    ordinals[i - from] = map[local]!;
    if (local !== 0 && mark[local] === 0) {
      mark[local] = 1;
      distinctCount++;
    }
  }
  const distinct = new Uint32Array(distinctCount);
  let d = 0;
  for (let i = from; i < to; i++) {
    const local = index[i]!;
    if (mark[local] === 1) {
      mark[local] = 0;
      distinct[d++] = map[local]!;
    }
  }
  distinct.sort();
  table.retain(distinct);
  return {ordinals, distinct};
}

/** Thrown where a response is split or stored after the table it was named in was cleared. */
function tableCleared(): Error {
  return new DOMException('the artifact table was cleared under this response; it is asked for again', 'AbortError');
}

/**
 * Splits a response into one band per tile, in slices, so a large response does not block the
 * thread that draws: each `step` builds bands until its deadline and the caller yields between
 * steps. Bands must be copies, and ten thousand of them do not transfer from a worker cheaply, so
 * the split runs on the main thread.
 *
 * The wire orders points by tile in the order the tile list gives, and a tile's `served` is its
 * length, so the split is a running sum. A tile with nothing served yields no band.
 */
export function bandSplitter(
  result: ViewportResult,
  depth: number,
  meta: {identityKey: string; contentKey: string; capUsed: number; now: number; table?: SessionArtifactTable; onRemap?: (ms: number) => void}
): BandSplitter {
  let offset = 0;
  let i = 0;
  const table = meta.table;
  const named = table && Object.keys(result.membership).length > 0 ? nameResponse(result, table) : null;
  let remapMs = 0;
  return {
    done: () => i >= result.tiles.length,
    step(deadline: number): Band[] {
      // The response's ordinals were named under a generation of the table a clear has ended.
      if (named && table!.generation !== named.generation) throw tableCleared();
      const bands: Band[] = [];
      // The clock is read every 64 tiles; once per band would be a noticeable share of the work.
      while (i < result.tiles.length) {
        if ((i & 63) === 0 && bands.length > 0 && performance.now() >= deadline) break;
        const tile = result.tiles[i++]!;
        const served = Number(tile.served);
        if (served === 0) continue;
        const end = offset + served;
        const ids = result.ids.slice(offset, end);
        // Truncation and the declared bound rely on ascending ids, so a response out of order is
        // refused here.
        for (let p = 1; p < ids.length; p++) {
          if (ids[p]! <= ids[p - 1]!) {
            throw new Error(
              `band ${tile.tile}: ids out of ascending order at ${p}; a response lists each tile's ` +
                `ids in ascending \`tessera_id\` order.`
            );
          }
        }
        const positions = result.world.slice(offset * 2, end * 2);
        const scalars = sliceScalars(result.scalars, offset, end);

        const highlightBits = result.highlighted ? result.highlighted.slice(offset, end) : null;
        const membership: Record<string, BandMembership> = {};
        if (named) {
          const started = performance.now();
          for (const n of named.naming) membership[n.layer] = remapBand(n, offset, end, table!);
          remapMs += performance.now() - started;
        }
        const {x, y} = tileXY(tile.tile, depth);
        bands.push({
          depth,
          prefix: tile.tile,
          x,
          y,
          ids,
          positions,
          scalars,
          membership,
          highlightBits,
          served,
          capUsed: meta.capUsed,
          visible: tile.visible,
          matched: tile.matched,
          highlighted: tile.highlighted,
          heldBelow: ids.length === 0 ? 0n : ids[ids.length - 1]! + 1n,
          identityKey: meta.identityKey,
          contentKey: meta.contentKey,
          bytes: bandBytes(ids, positions, scalars, membership, highlightBits),
          touchedAt: meta.now
        });
        offset = end;
      }
      if (i >= result.tiles.length && named) {
        named.release();
        meta.onRemap?.(remapMs);
      }
      return bands;
    }
  };
}

/** {@link bandSplitter}, drained in one call. @internal */
export function bandsOfResult(
  result: ViewportResult,
  depth: number,
  meta: {identityKey: string; contentKey: string; capUsed: number; now: number}
): Band[] {
  return bandSplitter(result, depth, meta).step(Infinity);
}

/** What a band contributes to a render, and on what authority. @internal */
export type Provenance = 'exact' | 'ancestor' | 'descendants';

/** @internal */
export type Resolved = {
  provenance: Provenance;
  bands: Band[];
  /**
   * False where the points came from another depth, so the marks drawn are a superset of
   * `served(T)`. No count may be shown against such a tile.
   */
  exact: boolean;
};

/** @internal */
export type PlannedRequest = {
  /** The regions to fetch, in tile-index space at the planned depth. Empty means nothing to fetch. */
  fetch: TileRect[];
  /** Tiles the wanted region spans, and how many of them the request covers. For reporting only. */
  wanted: number;
  novel: number;
};

/** @internal */
export type EvictionFocus = {
  depth: number;
  prefix: bigint;
  /**
   * The bands on screen at this depth, which eviction does not truncate. A streamed response lands
   * in parts, so its first rows are the least recently touched; without this they would be halved
   * while the rest of the same view kept every point.
   */
  protect?: {depth: number; rect: TileRect};
};

/**
 * One byte budget over every view's band cache.
 *
 * Each view has its own {@link BandCache}, because positions from different views are in
 * different coordinate systems and cannot share an index, but one number bounds their total.
 * Eviction runs on the fetch that overflowed the budget, which is always the current view's, so
 * the evicting cache's protected rectangle is the one that applies.
 */
export class BandBudget {
  private readonly caches = new Set<BandCache>();

  constructor(readonly budgetBytes: number) {}

  /** Called by {@link BandCache}'s constructor; a cache belongs to one budget. */
  register(cache: BandCache): void {
    this.caches.add(cache);
  }

  /** Bytes held across every registered cache, the figure {@link budgetBytes} bounds. */
  get bytes(): number {
    let held = 0;
    for (const cache of this.caches) held += cache.bytes;
    return held;
  }

  /** How many caches hold any band. */
  get views(): number {
    let n = 0;
    for (const cache of this.caches) if (cache.bandCount > 0) n++;
    return n;
  }

  /**
   * Evicts to `lowWaterFraction` of the budget. Other views' bands go before the evicting view's,
   * so a view left an hour ago gives up its bytes first. Within each group the order is
   * {@link evictionOrder}. See {@link BandCache.evict} for what one eviction does.
   */
  evict(focus: EvictionFocus, from: BandCache, lowWaterFraction = 0.9): void {
    if (this.bytes <= this.budgetBytes) return;
    const target = this.budgetBytes * lowWaterFraction;

    const held: {cache: BandCache; band: Band}[] = [];
    for (const cache of this.caches) {
      if (cache === from) continue;
      for (const band of cache.heldBands()) held.push({cache, band});
    }
    held.sort((a, b) => evictionOrder(a.band, b.band, focus));

    const evicting = from
      .heldBands()
      .map((band) => ({cache: from, band}))
      .sort((a, b) => evictionOrder(a.band, b.band, focus));
    const order = held.concat(evicting);

    const protect = focus.protect;
    for (const {cache, band} of order) {
      if (this.bytes <= target) return;
      // Another view's band at the same tile coordinates is not on screen.
      if (cache === from && protect && band.depth === protect.depth && rectContainsTile(protect.rect, band.x, band.y)) continue;
      cache.shed(band);
    }
  }
}

/**
 * Deepest first, then least recently touched, then farthest from the focus. Coarse points are
 * the head of every band and live in the shallow bands, so this order keeps the overview drawn.
 */
function evictionOrder(a: Band, b: Band, focus: EvictionFocus): number {
  if (a.depth !== b.depth) return b.depth - a.depth;
  if (a.touchedAt !== b.touchedAt) return a.touchedAt - b.touchedAt;
  return Number(distance(b, focus) - distance(a, focus));
}

/**
 * The held bands for one view and one principal, under a byte budget.
 *
 * A change of principal drops every band rather than filtering them. Serving one principal's
 * bands to another would disclose data the second may not see.
 *
 * `budget` is a {@link BandBudget} shared with other views' caches, or a number, which gives this
 * cache a budget of its own.
 *
 * @internal
 */
export class BandCache {
  private bands = new Map<BandKey, Band>();
  /**
   * The same bands grouped by depth. A frame needs one depth's bands, and a walk over the store
   * costs in proportion to what is held, which a mark budget does not bound.
   */
  private byDepth = new Map<number, Map<BandKey, Band>>();
  /**
   * Incremented by every change to what is held or covered. A frame derived from this cache stays
   * valid while this does not move, so a redraw can skip re-deriving it. A missed increment draws a
   * stand-in over ground that has since been covered.
   */
  private changes = 0;
  /**
   * The regions asked for whose answers have been absorbed. A response omits tiles with nothing
   * visible, and most tiles in a viewport are empty, so this is how empty ground is remembered:
   * anything inside a covered rectangle that was not sent is empty. Rectangles, rather than an
   * entry per tile, keep this small and let a plan subtract regions without listing tiles.
   */
  private covered: Coverage[] = [];
  private identityKey: string | null = null;
  private held = 0;
  private heldPoints = 0;

  /** The budget this cache counts against; its own where the caller passed a number. */
  private readonly budget: BandBudget;

  constructor(
    budget: number | BandBudget,
    /** The session table each band's membership holds references on; absent, nothing is named. */
    private readonly table: SessionArtifactTable | null = null
  ) {
    this.budget = typeof budget === 'number' ? new BandBudget(budget) : budget;
    this.budget.register(this);
  }

  /** The budget over this cache and every other view's; see {@link BandBudget}. */
  get budgetBytes(): number {
    return this.budget.budgetBytes;
  }

  /** Bytes held across every view sharing this cache's budget. */
  get sharedBytes(): number {
    return this.budget.bytes;
  }

  /** How many views hold any band. */
  get heldViews(): number {
    return this.budget.views;
  }

  /** Releases every table reference a band's membership holds. */
  private releaseMembership(band: Band): void {
    if (!this.table) return;
    for (const m of Object.values(band.membership)) this.table.release(m.distinct);
  }

  get bytes(): number {
    return this.held;
  }

  /**
   * Points held, the figure to size a replica against. A band is around ten points, so per-band
   * overhead is a large share of `bytes`. Kept as a counter because it is read on every store
   * update.
   */
  get points(): number {
    return this.heldPoints;
  }

  get bandCount(): number {
    return this.bands.size;
  }

  /** Keeps {@link byDepth} in step with a `bands` write. */
  private index(band: Band, key: BandKey): void {
    let atDepth = this.byDepth.get(band.depth);
    if (!atDepth) {
      atDepth = new Map();
      this.byDepth.set(band.depth, atDepth);
    }
    atDepth.set(key, band);
  }

  private atDepth(depth: number): Iterable<Band> {
    return this.byDepth.get(depth)?.values() ?? [];
  }

  /**
   * The exact bands inside a region: {@link bandsForRegion} without the stand-ins, for folding a
   * fresh arrival into a frame already drawn.
   */
  exactIn(want: TileRect, depth: number): Band[] {
    const exact: Band[] = [];
    for (const band of this.atDepth(depth)) {
      if (rectContainsTile(want, band.x, band.y)) exact.push(band);
    }
    return exact;
  }

  /** See {@link changes}. Compare for equality only. */
  get version(): number {
    return this.changes;
  }

  get size(): number {
    return this.bands.size;
  }

  get(depth: number, prefix: bigint): Band | undefined {
    return this.bands.get(bandKey(depth, prefix));
  }

  /**
   * Admits a band, first dropping every band of another principal.
   *
   * A band under a new content key replaces the held one. A merge would keep an item suppressed
   * since the held band was fetched: the server names the whole served set, so an item it does not
   * name is dropped.
   */
  put(band: Band): void {
    if (this.identityKey !== band.identityKey) {
      this.dropIdentity();
      this.identityKey = band.identityKey;
    }
    const key = bandKey(band.depth, band.prefix);
    const previous = this.bands.get(key);
    if (previous) {
      // A layer's column survives a refetch that did not name the layer, provided the served set is
      // the same, so turning a layer back on costs nothing. Under a new content key nothing carries.
      const sameSet = previous.contentKey === band.contentKey && previous.ids.length === band.ids.length;
      for (const [layer, held] of Object.entries(previous.membership)) {
        if (sameSet && !(layer in band.membership)) {
          band.membership[layer] = held;
          band.bytes += held.ordinals.byteLength + held.distinct.byteLength;
        } else if (this.table) {
          this.table.release(held.distinct);
        }
      }
      this.held -= previous.bytes;
      this.heldPoints -= previous.ids.length;
    }
    this.bands.set(key, band);
    this.index(band, key);
    this.held += band.bytes;
    this.heldPoints += band.ids.length;
    this.changes++;
  }

  /**
   * Records that a region was asked for and its answer absorbed. Call it only once the response's
   * bands are in, or the next plan skips tiles whose data never arrived.
   */
  markCovered(rect: TileRect, depth: number, contentKey: string, capUsed: number): void {
    this.covered = coverageAdd(this.covered, {rect, depth, contentKey, capUsed});
    this.changes++;
  }

  /** Regions held at this depth, content key and cap: the holes a plan subtracts. */
  coverageFor(depth: number, contentKey: string, k: number): TileRect[] {
    return coverageAt(this.covered, depth, contentKey, k);
  }

  /**
   * Withdraws coverage over each band's tile so the next plan fetches it again. Used for a band
   * that is colour-stale: its ordinals no longer resolve, or it lacks the column for a layer now
   * on. The band stays drawn until its replacement arrives.
   */
  retract(bands: readonly Band[]): void {
    for (const band of bands) {
      if (this.bands.get(bandKey(band.depth, band.prefix)) === band) this.retractCoverage(band.depth, band.x, band.y);
    }
  }

  /** Drops everything. Called when the token changes. */
  dropIdentity(): void {
    for (const band of this.bands.values()) this.releaseMembership(band);
    this.bands.clear();
    this.byDepth.clear();
    this.covered = [];
    this.changes++;
    this.identityKey = null;
    this.held = 0;
    this.heldPoints = 0;
  }

  /**
   * The parts of `want` to fetch at one depth: `want` less what is covered.
   *
   * Coverage is read from the cache, not from a record of what the server sent, because eviction
   * withdraws coverage. Understating what is held costs bytes and nothing else.
   */
  planRegion(want: TileRect, depth: number, contentKey: string, k: number): PlannedRequest {
    const wanted = rectArea(want);

    // A count-only request (`k = 0`) subtracts nothing: it refreshes counts and the content key
    // over ground already held.
    if (k === 0) return {fetch: [want], wanted, novel: wanted};

    // At most two pieces, because each piece is a request, and asking again for a held tile costs
    // the server almost nothing.
    const fetch = rectSubtractAll(want, this.coverageFor(depth, contentKey, k), 2);
    return {fetch, wanted, novel: fetch.reduce((n, r) => n + rectArea(r), 0)};
  }

  /**
   * The bands to draw for a region. Exact bands come from this depth. Stand-ins, an ancestor
   * clipped to a rectangle or a held descendant, are admitted only over the part of the region not
   * covered, so they do not draw over ground an exact band answers. A stand-in draws a superset of
   * `served(T)`: the caller marks it stale and shows no count against it.
   *
   * Walks the held bands rather than the tiles wanted: a settled viewport spans about 16,500 tiles,
   * of which about 450 hold data.
   */
  bandsForRegion(
    want: TileRect,
    depth: number,
    contentKey: string,
    k: number
  ): {exact: Band[]; fallback: {band: Band; clip: TileRect}[]} {
    const uncovered = rectSubtractAll(want, this.coverageFor(depth, contentKey, k));
    const exact = this.exactIn(want, depth);
    /** Stand-ins bucketed by distance from the drawn depth, coarsest first. */
    const byRank: {band: Band; clip: TileRect}[][] = [];

    if (uncovered.length === 0) return {exact, fallback: []};

    for (const band of this.bands.values()) {
      if (band.depth === depth) continue;
      // An ancestor's tile projects to a block of this depth's grid, a descendant's to one tile.
      const shift = Math.abs(band.depth - depth);
      const {x, y} = band;
      const box: TileRect =
        band.depth < depth
          ? {x0: x << shift, y0: y << shift, x1: ((x + 1) << shift) - 1, y1: ((y + 1) << shift) - 1}
          : {x0: x >> shift, y0: y >> shift, x1: x >> shift, y1: y >> shift};
      // Clipped to the uncovered part, since a stand-in drawn over covered ground overlays coarse
      // marks on fine ones. Bucketed rather than sorted because a broad view offers up to 1.5 x 10^5
      // candidates. The total is not capped: a cap drops whole bands and leaves bare ground.
      const rank = band.depth < depth ? depth - band.depth : MAX_DEPTH + (band.depth - depth);
      for (const r of uncovered) {
        const clip = rectIntersection(r, box);
        if (clip) (byRank[rank] ??= []).push({band, clip});
      }
    }

    // `push(...bucket)` with 10^5 entries throws a `RangeError`.
    const fallback: {band: Band; clip: TileRect}[] = [];
    for (const bucket of byRank) {
      if (!bucket) continue;
      for (const entry of bucket) fallback.push(entry);
    }
    return {exact, fallback};
  }

  /**
   * The best answer held for one tile: its own band, else the nearest ancestor's, else any held
   * descendants. A fallback draws a superset of `served(T)`; the caller marks it stale and shows no
   * count against it.
   */
  resolve(depth: number, prefix: bigint): Resolved | null {
    const exact = this.get(depth, prefix);
    if (exact) return {provenance: 'exact', bands: [exact], exact: true};

    for (let d = depth - 1; d >= 0; d--) {
      const ancestor = this.get(d, prefix >> BigInt(2 * (depth - d)));
      if (ancestor) return {provenance: 'ancestor', bands: [ancestor], exact: false};
    }

    const descendants: Band[] = [];
    for (const [held, atDepth] of this.byDepth) {
      if (held <= depth) continue;
      for (const band of atDepth.values()) {
        if (tileContains(prefix, depth, band.prefix, band.depth)) descendants.push(band);
      }
    }
    if (descendants.length > 0) return {provenance: 'descendants', bands: descendants, exact: false};
    return null;
  }

  /**
   * Indices of the band's points inside a tile rectangle at `depth`, or `null` where the whole band
   * is inside. Tests world positions, four `f32` comparisons a point, because deriving each point's
   * tile costs about twenty `BigInt` allocations.
   */
  static restrictToRect(band: Band, depth: number, rect: TileRect): number[] | null {
    const span = WORLD_SIZE / 2 ** depth;
    const x0 = rect.x0 * span;
    const x1 = (rect.x1 + 1) * span;
    const y0 = rect.y0 * span;
    const y1 = (rect.y1 + 1) * span;

    // A band wholly inside needs no per-point loop. This is the common case: a descendant's tile is
    // far smaller than the region, and an ancestor is clipped to ground nothing finer covers.
    const own = WORLD_SIZE / 2 ** band.depth;
    if (
      band.x * own >= x0 &&
      (band.x + 1) * own <= x1 &&
      band.y * own >= y0 &&
      (band.y + 1) * own <= y1
    ) {
      return null;
    }

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
   * Evicts to the budget by truncating band tails. The low-identity head of each band holds its
   * coarse points, so a truncated band still draws the overview and still declares a sound, lower
   * bound. Runs to a low-water mark so a stream of `put`s does not sort the cache on each one. The
   * pass belongs to the budget, which may span several views; see {@link BandBudget.evict}.
   */
  evict(focus: EvictionFocus, lowWaterFraction = 0.9): void {
    this.budget.evict(focus, this, lowWaterFraction);
  }

  /** Every held band, for the budget's ordering. */
  heldBands(): Band[] {
    return [...this.bands.values()];
  }

  /** Halves one band, keeping its head. False where the band is one point. */
  shed(band: Band): boolean {
    const keep = Math.max(1, Math.floor(band.ids.length / 2));
    if (keep >= band.ids.length) return false;
    this.truncate(band, keep);
    return true;
  }

  /**
   * Withdraws coverage over a tile. Truncation calls this, or the plan would go on subtracting the
   * region and the discarded points would not be fetched again. The whole containing rectangle
   * goes, since a rectangle less one tile is not a rectangle; the cost is a refetch.
   */
  private retractCoverage(depth: number, x: number, y: number): void {
    this.covered = this.covered.filter(
      (c) => c.depth !== depth || !rectContainsTile(c.rect, x, y)
    );
    this.changes++;
  }

  /** Cuts a band to its first `keep` points and lowers its bound to match. */
  private truncate(band: Band, keep: number): void {
    this.retractCoverage(band.depth, band.x, band.y);
    const ids = band.ids.slice(0, keep);
    const positions = band.positions.slice(0, keep * 2);
    const scalars = sliceScalars(band.scalars, 0, keep);
    // The distinct list may shrink with the tail.
    const membership: Record<string, BandMembership> = {};
    for (const [layer, held] of Object.entries(band.membership)) {
      const ordinals = held.ordinals.slice(0, keep);
      const distinct = distinctOrdinals(ordinals);
      this.table?.retain(distinct);
      this.table?.release(held.distinct);
      membership[layer] = {ordinals, distinct};
    }
    const highlightBits = band.highlightBits ? band.highlightBits.slice(0, keep) : null;
    const bytes = bandBytes(ids, positions, scalars, membership, highlightBits);
    this.held += bytes - band.bytes;
    this.heldPoints += keep - band.ids.length;
    this.changes++;
    const truncated = bandKey(band.depth, band.prefix);
    const kept: Band = {...band, ids, positions, scalars, membership, highlightBits, heldBelow: ids[keep - 1]! + 1n, bytes};
    this.bands.set(truncated, kept);
    this.index(kept, truncated);
  }
}

/** Morton distance from a band to the focus tile, at the focus's depth. */
function distance(band: Band, focus: EvictionFocus): bigint {
  const at = band.depth >= focus.depth ? band.prefix >> BigInt(2 * (band.depth - focus.depth)) : band.prefix;
  const delta = at - focus.prefix;
  return delta < 0n ? -delta : delta;
}
