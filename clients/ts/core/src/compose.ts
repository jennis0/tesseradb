import {BandCache, type Band} from './bands.js';
import {WORLD_SIZE} from './coords.js';
import type {ReplicaFrame} from './replica.js';
import type {TileRect} from './rects.js';

/**
 * Frame composition: which bands contribute to a drawn frame, at what prefix length, on what
 * authority — the invariant-bearing half of what the viewer's `assemble.ts` used to decide,
 * moved to `tessera-client` (client-architecture §5) so the rules a dependent client must obey
 * are enforced where every client gets them, and testable without a browser.
 *
 * The rules, each load-bearing:
 *
 * - **Any drawn subset of a band is an id-order prefix** (`delta-serving.md` §7) — anything else
 *   makes the cache the second sampler I7 forbids. Enforced by construction: contributions are
 *   `limit` prefixes and index lists that only ever shorten, over ids asserted ascending at
 *   `bandSplitter`.
 * - **Ground an exact band answers admits no stand-in, whatever coverage says.** The replica
 *   clips stand-ins by coverage rects; exact bands can precede their rect. The exact set itself
 *   is the shared authority, so derive-time and fold-time cannot disagree — the granularity
 *   drift that oscillated thousands of tiles per frame when they could.
 * - **Descendant stand-ins are density-matched per drawn tile, not per band**, one floor per
 *   tile, largest-remainder across bands — a per-band floor handed a coarse tile a mark per
 *   tiny deep band, up to ~200x its own density.
 * - **Evaluated per contributing band projected onto the integer grid, never per viewport
 *   tile** — a per-tile pass would re-buy the O(tiles) walk measured at 181 ms for a 262k-tile
 *   region. Ancestors stay one piece with an index list; only exact and descendant claims (one
 *   tile each) touch per-tile state.
 * - **No counts on non-exact tiles**: provenance rides with every contribution so the consumer
 *   can suppress the number channel where the marks are a superset.
 */

/** One tile's contribution to the draw, and the authority it rests on. */
export type ComposedTile = {
  prefix: bigint;
  /** The depth `prefix` is addressed at — the band's own, not necessarily the frame's. */
  depth: number;
  exact: boolean;
  /** Marks this entry puts on screen — for an exact tile, exactly `served`. */
  drawn: number;
  /** The server's own counts, present only for an exact tile. */
  counts: {visible: bigint; matched: bigint; highlighted: bigint; served: number} | null;
};

/** A stand-in contribution: a band, drawn as `indices` (ancestors) or a `limit` prefix. */
export type StandInPiece = {band: Band; indices: number[] | null; limit: number};

export type Composition = {
  depth: number;
  want: TileRect;
  version: number;
  /** Exact bands, by reference — the slab writes them once; nothing here copies them. */
  exact: Band[];
  /** Stand-in pieces in coarsest-first order, already density-matched and supersession-filtered. */
  standIn: StandInPiece[];
  tiles: ComposedTile[];
  exactDrawn: number;
  exactServed: number;
  visibleInView: number;
  /** Σ stand-in marks — what the pieces will materialise to. */
  provisional: number;
  /**
   * True where the stand-ins rode along from an older composition (a fold) rather than being
   * derived for this one — what the settle exists to repair.
   */
  standInStale: boolean;
};

function exactTileSet(exact: readonly Band[], dim: number): Set<number> {
  const tiles = new Set<number>();
  for (const band of exact) {
    if (band.ids.length > 0) tiles.add(band.x * dim + band.y);
  }
  return tiles;
}

/** Derive a full composition from a replica frame — the expensive tier; rate-limit the caller. */
export function compose(frame: ReplicaFrame): Composition {
  const tiles: ComposedTile[] = [];
  const exact: Band[] = [];
  let exactDrawn = 0;
  let exactServed = 0;
  let visibleInView = 0;

  // **A truncated band is not exact.** Eviction keeps a band's head and its `served` figure;
  // counting it exact would fail the drawn-equals-served fidelity check on every paint until the
  // refetch heals it — a crash loop under exactly the memory pressure eviction exists for. Its
  // head is still an id-order prefix, so it is demoted to a stand-in over its own tile: drawn,
  // stale-marked, counts suppressed, refetched when its ground is next planned.
  const truncated: Band[] = [];
  for (const band of frame.exact) {
    if (band.ids.length === 0) continue;
    if (band.ids.length < band.served) {
      truncated.push(band);
      continue;
    }
    exact.push(band);
    exactDrawn += band.ids.length;
    exactServed += band.served;
    visibleInView += Number(band.visible);
    tiles.push({
      prefix: band.prefix,
      depth: band.depth,
      exact: true,
      drawn: band.ids.length,
      counts: {visible: band.visible, matched: band.matched, highlighted: band.highlighted, served: band.served}
    });
  }

  const dim = 2 ** frame.depth;
  const exactTiles = exactTileSet(exact, dim);
  // A truncated head still answers its tile for supersession: without this, held descendants
  // would draw over the same ground and the patch reads dense-then-thin instead of loading.
  for (const band of truncated) exactTiles.add(band.x * dim + band.y);
  const span = WORLD_SIZE / dim;

  const pieces: StandInPiece[] = [];
  const groups = new Map<bigint, number[]>();
  for (const band of truncated) {
    pieces.push({band, indices: null, limit: band.ids.length});
  }
  for (const {band, clip} of frame.fallback) {
    if (band.depth > frame.depth) {
      const shift = band.depth - frame.depth;
      if (exactTiles.has((band.x >> shift) * dim + (band.y >> shift))) continue;
    }
    let indices = BandCache.restrictToRect(band, frame.depth, clip);
    if (band.depth < frame.depth && exactTiles.size > 0) {
      indices = filterAncestor(band, indices, exactTiles, dim, span);
    }
    const length = indices ? indices.length : band.ids.length;
    if (length === 0) continue;
    const at = pieces.length;
    pieces.push({band, indices, limit: length});
    if (band.depth > frame.depth) {
      const ancestor = band.prefix >> BigInt(2 * (band.depth - frame.depth));
      const held = groups.get(ancestor);
      if (held) held.push(at);
      else groups.set(ancestor, [at]);
    }
  }

  for (const members of groups.values()) {
    const shares = members.map((i) => pieces[i]!.limit / 4 ** (pieces[i]!.band.depth - frame.depth));
    const target = Math.max(1, Math.round(shares.reduce((a, s) => a + s, 0)));
    const floors = shares.map(Math.floor);
    let remaining = target - floors.reduce((a, n) => a + n, 0);
    const byFraction = shares.map((s, j) => [s - Math.floor(s), j] as const).sort((a, b) => b[0] - a[0]);
    for (const [, j] of byFraction) {
      if (remaining <= 0) break;
      floors[j]! += 1;
      remaining -= 1;
    }
    members.forEach((i, j) => {
      pieces[i]!.limit = Math.min(pieces[i]!.limit, floors[j]!);
    });
  }

  const standIn: StandInPiece[] = [];
  let provisional = 0;
  for (const piece of pieces) {
    if (piece.limit === 0) continue;
    if (piece.indices && piece.indices.length > piece.limit) piece.indices.length = piece.limit;
    standIn.push(piece);
    provisional += piece.limit;
    tiles.push({
      prefix: piece.band.prefix,
      depth: piece.band.depth,
      exact: false,
      drawn: piece.limit,
      counts: null
    });
  }

  return {
    depth: frame.depth,
    want: frame.want,
    version: frame.version,
    exact,
    standIn,
    tiles,
    exactDrawn,
    exactServed,
    visibleInView,
    provisional,
    standInStale: false
  };
}

/**
 * Fold fresh exact bands into a held composition — the cheap tier.
 *
 * The exact half is fully recomputed; the stand-in pieces ride along **filtered against the new
 * exact set** (ground just answered admits no stand-in) and are returned by reference when
 * nothing was filtered, which is what lets a consumer skip re-materialising its buffers.
 */
export function fold(held: Composition, exact: Band[], version: number): Composition {
  const tiles: ComposedTile[] = [];
  const live: Band[] = [];
  const truncated: Band[] = [];
  let exactDrawn = 0;
  let exactServed = 0;
  let visibleInView = 0;
  const dim = 2 ** held.depth;
  const span = WORLD_SIZE / dim;
  for (const band of exact) {
    if (band.ids.length === 0) continue;
    if (band.ids.length < band.served) {
      // Same demotion as {@link compose}: eviction's head is a stand-in now, never exact.
      truncated.push(band);
      continue;
    }
    live.push(band);
    exactDrawn += band.ids.length;
    exactServed += band.served;
    visibleInView += Number(band.visible);
    tiles.push({
      prefix: band.prefix,
      depth: band.depth,
      exact: true,
      drawn: band.ids.length,
      counts: {visible: band.visible, matched: band.matched, highlighted: band.highlighted, served: band.served}
    });
  }
  const exactTiles = exactTileSet(live, dim);
  // As in {@link compose}: a truncated head answers its tile for supersession purposes.
  for (const band of truncated) exactTiles.add(band.x * dim + band.y);

  let standIn = held.standIn;
  if (exactTiles.size > 0 || truncated.length > 0) {
    let changed = truncated.length > 0;
    const kept: StandInPiece[] = truncated.map((band) => ({
      band,
      indices: null,
      limit: band.ids.length
    }));
    for (const piece of held.standIn) {
      const band = piece.band;
      if (band.depth > held.depth) {
        const shift = band.depth - held.depth;
        if (exactTiles.has((band.x >> shift) * dim + (band.y >> shift))) {
          changed = true;
          continue;
        }
        kept.push(piece);
        continue;
      }
      if (band.depth === held.depth) {
        // A carried truncated head; superseded if its tile has become exact.
        if (exactTiles.has(band.x * dim + band.y)) {
          changed = true;
          continue;
        }
        kept.push(piece);
        continue;
      }
      const filtered = filterAncestor(band, piece.indices, exactTiles, dim, span);
      const length = Math.min(filtered ? filtered.length : band.ids.length, piece.limit);
      if (length === 0) {
        changed = true;
        continue;
      }
      if (filtered !== piece.indices) changed = true;
      kept.push(filtered === piece.indices ? piece : {band, indices: filtered, limit: length});
    }
    if (changed) standIn = kept;
  }

  // Non-exact tile entries are rebuilt from the pieces that actually survived — a carried entry
  // keeps a `drawn` its refiltered piece no longer has, and every per-tile reader (the density
  // audit first among them) would sum marks that are not drawn.
  let provisional = 0;
  for (const piece of standIn) {
    const drawn = piece.indices ? Math.min(piece.indices.length, piece.limit) : piece.limit;
    provisional += drawn;
    tiles.push({prefix: piece.band.prefix, depth: piece.band.depth, exact: false, drawn, counts: null});
  }

  return {
    depth: held.depth,
    want: held.want,
    version,
    exact: live,
    standIn,
    tiles,
    exactDrawn,
    exactServed,
    visibleInView,
    provisional,
    standInStale: true
  };
}

/** Ancestor marks over exact tiles are dropped — the same integer-grid test at both tiers. */
function filterAncestor(
  band: Band,
  indices: number[] | null,
  exactTiles: Set<number>,
  dim: number,
  span: number
): number[] | null {
  const p = band.positions;
  const test = (i: number) =>
    !exactTiles.has(Math.floor(p[i * 2]! / span) * dim + Math.floor(p[i * 2 + 1]! / span));
  if (indices) {
    const kept: number[] = [];
    for (const i of indices) if (test(i)) kept.push(i);
    return kept.length === indices.length ? indices : kept;
  }
  let dropped = false;
  const kept: number[] = [];
  for (let i = 0; i < band.ids.length; i++) {
    if (test(i)) kept.push(i);
    else dropped = true;
  }
  return dropped ? kept : null;
}
