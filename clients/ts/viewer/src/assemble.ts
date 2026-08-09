import {BandCache, type Band, type ReplicaFrame} from '@tessera/client';
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

function pieceLength(piece: Piece): number {
  return piece.indices ? piece.indices.length : piece.band.ids.length;
}

/**
 * Concatenate one column across pieces, preserving its declared Arrow type.
 *
 * **Only the columns a caller asks for.** Every declared column arrives in the response and is held
 * per band, but a redraw needs exactly the one being coloured by — assembling all of them was five
 * times the work for four columns nothing reads.
 */
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
    if (piece.indices) {
      const source = column.values as unknown as {[i: number]: unknown};
      for (const i of piece.indices) out[o++] = source[i];
    } else {
      (out as unknown as {set(v: ArrayLike<number>, o: number): void}).set(
        column.values as unknown as ArrayLike<number>,
        o
      );
      o += len;
    }
  }
  return {arrowType: first.arrowType, values: out} as unknown as ScalarColumn;
}

/**
 * Assemble a frame.
 *
 * Positions narrow from `f64` cell space to `f32` world here, which is the single place precision
 * is spent — the same conversion `positionsToWorld` performs for a whole response.
 */
export function assemble(frame: ReplicaFrame, columns?: Iterable<string>): Assembled {
  const tiles: AssembledTile[] = [];
  const pieces: Piece[] = [];
  let total = 0;
  let exactDrawn = 0;
  let exactServed = 0;
  let provisional = 0;
  let visibleInView = 0;

  // **Walks the bands the replica holds, not the tiles the viewport spans.** A settled view spans
  // ~16.5k tiles of which ~450 carry anything, so iterating what is held is two orders of magnitude
  // less work than iterating what was asked about — and it is the same list either way.
  for (const band of frame.exact) {
    const length = band.ids.length;
    if (length === 0) continue;
    const from = total;
    pieces.push({band, indices: null});
    total += length;
    exactDrawn += length;
    exactServed += band.served;
    visibleInView += Number(band.visible);
    tiles.push({
      prefix: band.prefix,
      from,
      to: total,
      exact: true,
      counts: {visible: band.visible, matched: band.matched, served: band.served}
    });
  }

  // Bands from another depth, admitted by the replica only over ground not held at this one. An
  // ancestor is restricted to the wanted region by Morton prefix — sound because a parent's band is
  // a prefix of its own visible set in identity order, so its restriction is a prefix of the
  // child's. Descendants contribute whole.
  for (const band of frame.fallback) {
    const indices =
      band.depth < frame.depth ? BandCache.restrictToRect(band, frame.depth, frame.want) : null;
    const length = indices ? indices.length : band.ids.length;
    if (length === 0) continue;
    const from = total;
    pieces.push({band, indices});
    total += length;
    provisional += length;
    tiles.push({prefix: band.prefix, from, to: total, exact: false, counts: null});
  }

  const ids = new BigUint64Array(total);
  const positions = new Float32Array(total * 2);
  let o = 0;
  for (const piece of pieces) {
    if (piece.indices) {
      // The restricted path — an ancestor band clipped to the region — is inherently per point.
      for (const i of piece.indices) {
        ids[o] = piece.band.ids[i]!;
        positions[o * 2] = piece.band.positions[i * 2]!;
        positions[o * 2 + 1] = piece.band.positions[i * 2 + 1]!;
        o++;
      }
    } else {
      // **A whole band is a memcpy.** `TypedArray.set` copies in native code; the per-element loop
      // this replaces was the bulk of a 136 ms assembly at 10^6 marks, and it was copying values
      // that had already been converted to their final form when the band was built.
      ids.set(piece.band.ids, o);
      positions.set(piece.band.positions, o * 2);
      o += piece.band.ids.length;
    }
  }

  const names = new Set<string>(columns ?? []);
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
