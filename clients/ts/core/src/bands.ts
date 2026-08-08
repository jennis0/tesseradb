import {tileContains, tileOfCode} from './coords.js';
import type {ScalarColumn, ViewportResult} from './types.js';

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
  ids: BigUint64Array;
  codes: BigUint64Array;
  /** Interleaved x,y in cell space — two entries per point, so `positions.length === 2 * ids.length`. */
  positions: Float64Array;
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
  codes: BigUint64Array,
  positions: Float64Array,
  scalars: Record<string, ScalarColumn>
): number {
  let bytes = ids.byteLength + codes.byteLength + positions.byteLength;
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
    const codes = result.codes.slice(offset, end);
    const positions = result.positions.slice(offset * 2, end * 2);
    const scalars = sliceScalars(result.scalars, offset, end);
    bands.push({
      depth,
      prefix: tile.tile,
      ids,
      codes,
      positions,
      scalars,
      served,
      capUsed: meta.capUsed,
      visible: tile.visible,
      matched: tile.matched,
      heldBelow: ids.length === 0 ? 0n : ids[ids.length - 1]! + 1n,
      identityKey: meta.identityKey,
      contentKey: meta.contentKey,
      bytes: bandBytes(ids, codes, positions, scalars),
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
  /** Tiles proven complete client-side. Absent from the request entirely. */
  omit: bigint[];
  /** Tiles to request, with the identity bound already held for each. `0n` means nothing held. */
  fetch: {prefix: bigint; below: bigint; count: number}[];
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
   * Tiles known to hold nothing, against the content key that established it.
   *
   * **Emptiness must be cached, or nothing else can be.** A response omits empty tiles entirely
   * (the engine returns no row for a tile whose visible count is zero), so without this every empty
   * tile is re-requested on every view — and a viewport is mostly empty tiles: measured on the 2.4M
   * demo corpus, 454 of 16,524 tiles in a settled view carry any data at all. The other 16,070
   * would put a request on the wire forever, and no amount of held marks would ever make a revisit
   * free.
   *
   * Costs a map entry rather than a band, and is invalidated by exactly what a band is: a content
   * key rotation can add rows to a tile that had none.
   */
  private empties = new Map<BandKey, string>();
  private identityKey: string | null = null;
  private held = 0;

  constructor(private readonly budgetBytes: number) {}

  get bytes(): number {
    return this.held;
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

  /** Record that a tile holds nothing under this content key. */
  markEmpty(depth: number, prefix: bigint, contentKey: string): void {
    this.empties.set(bandKey(depth, prefix), contentKey);
  }

  isKnownEmpty(depth: number, prefix: bigint, contentKey: string): boolean {
    return this.empties.get(bandKey(depth, prefix)) === contentKey;
  }

  /** Drop everything. Called on a token change, where the whole partition becomes unrenderable. */
  dropIdentity(): void {
    this.bands.clear();
    this.empties.clear();
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
  plan(tiles: bigint[], depth: number, contentKey: string, k: number): PlannedRequest {
    const omit: bigint[] = [];
    const fetch: PlannedRequest['fetch'] = [];

    // A counts-only request (`k = 0`) omits nothing. Its whole purpose is to refresh the number
    // channel and the content key over tiles the client already holds — which is what keeps the
    // staleness bound reachable once look-ahead has emptied the request (`delta-serving.md` §8).
    // Left to the general rule below it would omit every tile, since at `k = 0` the definition
    // serves nothing and so any band trivially holds all of it.
    if (k === 0) {
      for (const prefix of tiles) fetch.push({prefix, below: 0n, count: 0});
      return {omit, fetch};
    }

    for (const prefix of tiles) {
      // A tile known to be empty is complete in the only sense that matters: there is nothing the
      // server could send for it. Checked first, because most tiles in a view are this.
      if (this.isKnownEmpty(depth, prefix, contentKey)) {
        omit.push(prefix);
        continue;
      }
      const band = this.get(depth, prefix);
      if (band && isComplete(band, contentKey, k)) {
        omit.push(prefix);
        continue;
      }
      const usable = band && band.contentKey === contentKey ? band : undefined;
      fetch.push({
        prefix,
        below: usable?.heldBelow ?? 0n,
        count: usable?.ids.length ?? 0
      });
    }
    return {omit, fetch};
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
   * The points of `band` that fall inside a deeper tile — the zoom-in fallback.
   *
   * Because the parent's band is a prefix of its own visible set in identity order, its restriction
   * to a child is a prefix of the *child's* visible set: the smallest identities of a subset are
   * the subset's own smallest. That is why an ancestor band may be drawn at all, and why the count
   * it yields is a sound declaration for the child.
   */
  static restrict(band: Band, depth: number, prefix: bigint): number[] {
    const indices: number[] = [];
    for (let i = 0; i < band.codes.length; i++) {
      if (tileOfCode(band.codes[i]!, depth) === prefix) indices.push(i);
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
  private truncate(band: Band, keep: number): void {
    const ids = band.ids.slice(0, keep);
    const codes = band.codes.slice(0, keep);
    const positions = band.positions.slice(0, keep * 2);
    const scalars = sliceScalars(band.scalars, 0, keep);
    const bytes = bandBytes(ids, codes, positions, scalars);
    this.held += bytes - band.bytes;
    this.bands.set(bandKey(band.depth, band.prefix), {
      ...band,
      ids,
      codes,
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
