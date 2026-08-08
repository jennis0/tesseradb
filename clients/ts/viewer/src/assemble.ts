import {BandCache, CELLS_PER_WORLD_UNIT, type Band, type ReplicaFrame} from '@tessera/client';
import type {ScalarColumn} from '@tessera/client';

/**
 * Turning a frame of per-tile bands into the buffers one `ScatterplotLayer` draws.
 *
 * **One layer, not one per tile.** deck.gl's `TileLayer` would give the tile lifecycle for free but
 * produces a sublayer per tile, and the request sizing here is `budget / m_target` tiles per view —
 * hundreds to thousands. It also derives depth from zoom, which is exactly the coupling the depth
 * budget exists to break. So deck.gl stays a renderer and the tile lifecycle lives in the replica.
 *
 * **A plain concat, not a slab allocator.** Measured: assembling one packed buffer costs 0.4–2.5 ms
 * at the 5 × 10^4-mark budget in force, which is inside a frame — but 17.8 ms at `caching.md`'s
 * 1–2 × 10^6-mark target, which is not. The slab (stable per-band slot ranges, dirty-span uploads)
 * is what that target needs, and it is not needed until the budget rises.
 */

/** One tile's contribution to the draw, and the authority it rests on. */
export type AssembledTile = {
  prefix: bigint;
  /** `[from, to)` into the assembled buffers. */
  from: number;
  to: number;
  /**
   * False where the marks came from an ancestor or from descendants, in which case they are a
   * superset of what the definition serves for this tile. Presentation only: no count may be shown
   * against such a tile, and it is excluded from the drawn-count assertion.
   */
  exact: boolean;
  /** The server's own counts, present only for an exact tile. */
  counts: {visible: bigint; matched: bigint; served: number} | null;
};

export type Assembled = {
  ids: BigUint64Array;
  /** Interleaved x,y in deck.gl world units, narrowed to the f32 a binary attribute takes. */
  positions: Float32Array;
  scalars: Record<string, ScalarColumn>;
  tiles: AssembledTile[];
  /** Marks drawn from exact bands, and what the server said it served for those same tiles. */
  exactDrawn: number;
  exactServed: number;
  /** Marks drawn from an ancestor or descendants — every one of them stale-marked. */
  provisional: number;
  /** Σ visible over exact tiles: the number channel, and `calibrate`'s saturation term. */
  visibleInView: number;
};

type Piece = {band: Band; indices: number[] | null};

/**
 * Which points of which bands a tile contributes.
 *
 * An ancestor band is restricted by Morton prefix — sound because the parent's band is a prefix of
 * its own visible set in identity order, so its restriction to a child is a prefix of the child's
 * visible set. Descendant bands contribute whole.
 */
function piecesOf(frame: ReplicaFrame, prefix: bigint, resolved: ReplicaFrame['tiles'][number]['resolved']): Piece[] {
  if (resolved.provenance === 'exact') return [{band: resolved.bands[0]!, indices: null}];
  if (resolved.provenance === 'ancestor') {
    const parent = resolved.bands[0]!;
    return [{band: parent, indices: BandCache.restrict(parent, frame.depth, prefix)}];
  }
  return resolved.bands.map((band) => ({band, indices: null}));
}

function pieceLength(piece: Piece): number {
  return piece.indices ? piece.indices.length : piece.band.ids.length;
}

/** Concatenate one column across pieces, preserving its declared Arrow type. */
function assembleScalar(name: string, pieces: Piece[], total: number): ScalarColumn | null {
  const first = pieces.find((p) => p.band.scalars[name])?.band.scalars[name];
  if (!first) return null;

  if (first.arrowType === 'bool' || first.arrowType === 'utf8') {
    const values: unknown[] = [];
    for (const piece of pieces) {
      const column = piece.band.scalars[name];
      if (!column) {
        for (let i = 0; i < pieceLength(piece); i++) values.push(null);
        continue;
      }
      const source = column.values as unknown[];
      if (piece.indices) for (const i of piece.indices) values.push(source[i]);
      else values.push(...source);
    }
    return {arrowType: first.arrowType, values} as ScalarColumn;
  }

  const Ctor = (first.values as unknown as {constructor: new (n: number) => ArrayLike<unknown>})
    .constructor;
  const out = new Ctor(total) as unknown as {[i: number]: unknown; length: number};
  let o = 0;
  for (const piece of pieces) {
    const column = piece.band.scalars[name];
    const len = pieceLength(piece);
    if (!column) {
      o += len;
      continue;
    }
    const source = column.values as unknown as {[i: number]: unknown};
    if (piece.indices) for (const i of piece.indices) out[o++] = source[i];
    else for (let i = 0; i < len; i++) out[o++] = source[i];
  }
  return {arrowType: first.arrowType, values: out} as unknown as ScalarColumn;
}

/**
 * Assemble a frame.
 *
 * Positions narrow from `f64` cell space to `f32` world here, which is the single place precision
 * is spent — the same conversion `positionsToWorld` performs for a whole response.
 */
export function assemble(frame: ReplicaFrame): Assembled {
  const tiles: AssembledTile[] = [];
  const pieces: Piece[] = [];
  let total = 0;
  let exactDrawn = 0;
  let exactServed = 0;
  let provisional = 0;
  let visibleInView = 0;

  for (const {prefix, resolved} of frame.tiles) {
    const mine = piecesOf(frame, prefix, resolved);
    const length = mine.reduce((sum, p) => sum + pieceLength(p), 0);
    if (length === 0) continue;

    const from = total;
    pieces.push(...mine);
    total += length;

    if (resolved.exact) {
      const band = resolved.bands[0]!;
      exactDrawn += length;
      exactServed += band.served;
      visibleInView += Number(band.visible);
      tiles.push({
        prefix,
        from,
        to: total,
        exact: true,
        counts: {visible: band.visible, matched: band.matched, served: band.served}
      });
    } else {
      provisional += length;
      tiles.push({prefix, from, to: total, exact: false, counts: null});
    }
  }

  const ids = new BigUint64Array(total);
  const positions = new Float32Array(total * 2);
  let o = 0;
  for (const piece of pieces) {
    if (piece.indices) {
      for (const i of piece.indices) {
        ids[o] = piece.band.ids[i]!;
        positions[o * 2] = piece.band.positions[i * 2]! / CELLS_PER_WORLD_UNIT;
        positions[o * 2 + 1] = piece.band.positions[i * 2 + 1]! / CELLS_PER_WORLD_UNIT;
        o++;
      }
    } else {
      for (let i = 0; i < piece.band.ids.length; i++) {
        ids[o] = piece.band.ids[i]!;
        positions[o * 2] = piece.band.positions[i * 2]! / CELLS_PER_WORLD_UNIT;
        positions[o * 2 + 1] = piece.band.positions[i * 2 + 1]! / CELLS_PER_WORLD_UNIT;
        o++;
      }
    }
  }

  const names = new Set<string>();
  for (const piece of pieces) for (const name of Object.keys(piece.band.scalars)) names.add(name);
  const scalars: Record<string, ScalarColumn> = {};
  for (const name of names) {
    const column = assembleScalar(name, pieces, total);
    if (column) scalars[name] = column;
  }

  return {ids, positions, scalars, tiles, exactDrawn, exactServed, provisional, visibleInView};
}

/**
 * The I7 check, over the domain it is actually true on.
 *
 * **Exact tiles only.** An ancestor or descendant band draws a superset of what the definition
 * serves, so ranging the equality over them would either fail on correct output or, if loosened to
 * an inequality, stop catching the thing it exists to catch. The complement is asserted separately:
 * every non-exact tile must be marked provisional, which is what suppresses its number channel.
 *
 * This is the one place a client could violate I7 by omission, so it throws rather than warns.
 */
export function assertDrawsEveryServedMark(assembled: Assembled): void {
  if (assembled.exactDrawn !== assembled.exactServed) {
    throw new Error(
      `I7: drawing ${assembled.exactDrawn} marks across exact tiles but the server served ` +
        `${assembled.exactServed}. The client must draw every mark it is served.`
    );
  }
  for (const tile of assembled.tiles) {
    if (!tile.exact && tile.counts !== null) {
      throw new Error(
        `I7: tile ${tile.prefix} draws a superset of its served set but carries counts. ` +
          `A superset of marks must never be read as density.`
      );
    }
  }
}
