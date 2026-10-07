import {BandCache, type Band, type CountedTile} from './bands.js';
import {WORLD_SIZE} from './coords.js';
import type {ReplicaFrame} from './replica.js';
import type {TileRect} from './rects.js';

/**
 * Frame composition: which bands contribute to a drawn frame, at what prefix length, and on what
 * authority. The rules every client must keep:
 *
 * - Every contribution is an id-order prefix of a band, or of a band's points inside a clip, so
 *   the client never samples differently from the server.
 * - Ground an exact band answers takes no stand-in, whatever the coverage says: exact bands can
 *   arrive before their coverage rectangle. `compose` and `fold` both test against the exact set,
 *   so they agree.
 * - Descendant stand-ins are density-matched per drawn tile, by largest remainder across bands. A
 *   floor per band would give a coarse tile one mark per small deep band.
 * - The work is per contributing band on the integer grid, never per viewport tile.
 * - A stand-in carries no counts, since its marks are a superset.
 * - A tile whose counts have landed and whose points have not is an entry that draws nothing and
 *   carries the server's counts. Its band carries the same counts, and replaces the entry.
 */

/**
 * One entry of a {@link Composition}'s `tiles`: one band's contribution to the frame, and whether
 * it is the server's exact answer for its tile.
 *
 * @category Projections
 */
export type ComposedTile = {
  /** The tile's Morton prefix at `depth`. */
  prefix: bigint;
  /** The depth `prefix` is addressed at: the band's own, which may differ from the frame's. */
  depth: number;
  /**
   * True where the marks are the server's served points for this tile at the frame's depth. False
   * for a stand-in, whose marks may be a superset of what the server would serve here, and for a
   * tile counted before its points, which draws nothing.
   */
  exact: boolean;
  /** Marks this entry puts on screen; the tile's `served` count for an exact tile. */
  drawn: number;
  /**
   * The server's counts for the tile, or `null` for a stand-in. A tile counted before its points
   * carries them with `drawn` 0. `visible` is how many items this principal may see in the tile,
   * `matched` how many of those match the filter, `highlighted` how many of those match the
   * highlight, and `served` how many points were sent.
   */
  counts: {visible: bigint; matched: bigint; highlighted: bigint; served: number} | null;
};

/**
 * One entry of a {@link Composition}'s `standIn`: a band drawn over ground that no exact band
 * answers yet. It is a band held from another depth, or one that eviction cut short.
 *
 * @category Projections
 */
export type StandInPiece = {
  /** The band, by reference. */
  band: Band;
  /** Indices into the band's points to draw, in order, or `null` to draw its first points. */
  indices: number[] | null;
  /** How many points to draw: the first `limit` of `indices`, or of the band where it is `null`. */
  limit: number;
};

/**
 * The frame on screen, as the store's `view` projection holds it: which bands are drawn and on
 * what authority. Exact bands are the server's answer for their tiles at the frame's depth. A
 * stand-in holds points from elsewhere, drawn until the exact answer arrives, and carries no count.
 * Bands are held by reference; building draw buffers from them is the renderer's work.
 *
 * @category Projections
 */
export type Composition = {
  /** The tile depth the frame is drawn at, 0 to 16. */
  depth: number;
  /** @internal */
  want: TileRect;
  /** The replica's version when the frame was composed or last refreshed. Compare for equality only. */
  version: number;
  /** The exact bands, by reference. */
  exact: Band[];
  /**
   * The stand-in pieces, coarsest first. Pieces from deeper bands are thinned to the density of
   * the frame's depth, and no piece covers ground an exact band answers.
   */
  standIn: StandInPiece[];
  /** One entry per exact band and per stand-in piece. */
  tiles: ComposedTile[];
  /** Marks drawn from exact bands. */
  exactDrawn: number;
  /** The sum of the exact bands' `served` counts. Equal to `exactDrawn` in every frame presented. */
  exactServed: number;
  /** The sum of the exact bands' `visible` counts: items this principal may see in those tiles. */
  visibleInView: number;
  /** Stand-in marks in total. It counts marks on screen and says nothing of how many items exist. */
  provisional: number;
  /**
   * True where the stand-ins were carried from an older frame when fresh exact bands were added.
   * The next full composition replaces them.
   */
  standInStale: boolean;
};

/** The tiles exact bands answer, a band of no points among them: nothing is drawn over them. */
function exactTileSet(exact: readonly Band[], dim: number): Set<number> {
  const tiles = new Set<number>();
  for (const band of exact) tiles.add(band.x * dim + band.y);
  return tiles;
}

/**
 * The entries for tiles counted before their points, at `depth`, less those an exact or truncated
 * band answers.
 */
function countedEntries(counted: readonly CountedTile[], answered: readonly Band[], depth: number): ComposedTile[] {
  if (counted.length === 0) return [];
  const drawn = new Set(answered.map((band) => band.prefix));
  return counted
    .filter((c) => !drawn.has(c.prefix))
    .map((c) => ({prefix: c.prefix, depth, exact: false, drawn: 0, counts: c.counts}));
}

/** Derives a full composition from a replica frame. The expensive path; the caller rate-limits it. @internal */
export function compose(frame: ReplicaFrame): Composition {
  const tiles: ComposedTile[] = [];
  const exact: Band[] = [];
  let exactDrawn = 0;
  let exactServed = 0;
  let visibleInView = 0;

  // A truncated band is not exact: it keeps its `served` figure, so counted as exact it would fail
  // the drawn-equals-served check on every paint until refetched. Its head is still a prefix, so it
  // is drawn as a stand-in over its own tile.
  const truncated: Band[] = [];
  for (const band of frame.exact) {
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

  tiles.push(...countedEntries(frame.counted ?? [], [...exact, ...truncated], frame.depth));

  const dim = 2 ** frame.depth;
  const exactTiles = exactTileSet(exact, dim);
  // A truncated head still answers its tile, so held descendants do not draw over it.
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
 * Folds fresh exact bands into a held composition. The exact half is recomputed; the stand-in
 * pieces are carried, with ground now exact removed, and returned by reference when nothing was
 * removed so a consumer can keep its buffers. `counted` is the tiles counted before their points;
 * left out, the held composition's are carried.
 *
 * @internal
 */
export function fold(held: Composition, exact: Band[], version: number, counted?: readonly CountedTile[]): Composition {
  const carried =
    counted ??
    held.tiles
      .filter((t) => !t.exact && t.drawn === 0 && t.counts !== null)
      .map((t) => ({prefix: t.prefix, counts: t.counts!}));
  const tiles: ComposedTile[] = [];
  const live: Band[] = [];
  const truncated: Band[] = [];
  let exactDrawn = 0;
  let exactServed = 0;
  let visibleInView = 0;
  const dim = 2 ** held.depth;
  const span = WORLD_SIZE / dim;
  for (const band of exact) {
    if (band.ids.length < band.served) {
      // Drawn as a stand-in, as in {@link compose}.
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
  tiles.push(...countedEntries(carried, [...live, ...truncated], held.depth));
  const exactTiles = exactTileSet(live, dim);
  // As in {@link compose}, a truncated head answers its tile.
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
        // A carried truncated head, dropped once its tile is exact.
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

  // Stand-in tile entries are rebuilt from the surviving pieces, so `drawn` matches what a refiltered
  // piece draws.
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

/** Drops an ancestor's marks over exact tiles, by the same integer-grid test in both paths. */
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
