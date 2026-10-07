import {
  type Band,
  type ComposedTile,
  type Composition,
  type ScalarColumn,
  type ScalarValues,
  type StandInPiece
} from '@tesseradb/client';
import {assertCompositionMatchesServed, compose, fold, type ReplicaFrame, type TileRect} from '@tesseradb/client/internal';

/**
 * Materialising a core composition into the buffers one `ScatterplotLayer` draws. Core's
 * `compose` decides which bands contribute; this module concatenates the stand-in pieces into
 * typed arrays, for the columns a renderer colours by. Exact bands are not copied: the slab
 * writes each once.
 *
 * `Assembled` carries its `composition` so core can fold new bands into it and every check of
 * what is drawn reads one structure.
 */

export type AssembledTile = ComposedTile;

export type Assembled = {
  depth: number;
  want: TileRect;
  version: number;
  standInStale: boolean;
  /** The exact bands this frame draws, handed to the slab uncopied. */
  bands: Band[];
  /** Stand-in marks, concatenated for the provisional layer. */
  standIn: {
    ids: BigUint64Array;
    positions: Float32Array;
    scalars: Record<string, ScalarColumn>;
    /**
     * The membership ordinal per stand-in mark for the layer materialised, `0` where the band
     * carried no column. A stand-in set oversamples its ground, but each mark is a real served
     * point with its served ordinal, so colouring it by artifact is exact.
     */
    ordinals: Float32Array;
    /** The highlight bit per stand-in mark, `1` where the band carried none, so stand-ins dull with the map. */
    highlights: Float32Array;
  };
  tiles: AssembledTile[];
  exactDrawn: number;
  exactServed: number;
  provisional: number;
  visibleInView: number;
  /** The core composition this frame materialises. */
  composition: Composition;
};

/** Everything drawn for this frame, before the slab's retained marks are added. */
export function assembledMarks(assembled: Assembled): number {
  return assembled.exactDrawn + assembled.provisional;
}

function pieceLength(piece: StandInPiece): number {
  const whole = piece.indices ? piece.indices.length : piece.band.ids.length;
  return Math.min(whole, piece.limit);
}

/** Concatenate one column across pieces, keeping its declared Arrow type. */
function assembleScalar(name: string, pieces: readonly StandInPiece[], total: number): ScalarColumn | null {
  const first = pieces.find((p) => p.band.scalars[name])?.band.scalars[name];
  if (!first) return null;
  const column: ScalarColumn = assembleValues(name, pieces, total, first);
  // A piece whose band lacks the column has no value there, as a null does.
  if (pieces.some((p) => p.band.scalars[name]?.present || !p.band.scalars[name])) {
    const present = new Uint8Array(total);
    let o = 0;
    for (const piece of pieces) {
      const source = piece.band.scalars[name];
      const len = pieceLength(piece);
      const at = (i: number) => (!source ? 0 : !source.present ? 1 : source.present[i]!);
      if (piece.indices) for (const i of piece.indices) present[o++] = at(i);
      else for (let i = 0; i < len; i++) present[o++] = at(i);
    }
    column.present = present;
  }
  return column;
}

function assembleValues(
  name: string,
  pieces: readonly StandInPiece[],
  total: number,
  first: ScalarColumn
): ScalarValues {

  if (first.arrowType === 'bool' || first.arrowType === 'utf8') {
    const values: unknown[] = [];
    for (const piece of pieces) {
      const column = piece.band.scalars[name];
      const len = pieceLength(piece);
      if (!column) {
        for (let i = 0; i < len; i++) values.push(null);
        continue;
      }
      const source = column.values as unknown[];
      if (piece.indices) for (const i of piece.indices) values.push(source[i]);
      else for (let i = 0; i < len; i++) values.push(source[i]);
    }
    return {arrowType: first.arrowType, values} as ScalarValues;
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
        (column.values as unknown as {slice(a: number, b: number): ArrayLike<number>}).slice(0, len),
        o
      );
      o += len;
    }
  }
  return {arrowType: first.arrowType, values: out} as unknown as ScalarValues;
}

/** The stand-in buffers a `ScatterplotLayer` draws, and how many marks they hold. */
export type StandInBuffers = Assembled['standIn'] & {count: number};

/**
 * The stand-in piece list as concatenated buffers, for the columns a renderer colours by.
 * `TesseraLayer` memoises this on the piece list's identity, which `fold` keeps when it filters
 * nothing.
 */
export function materialiseStandIn(pieces: readonly StandInPiece[], columns: Iterable<string>, layer = ''): StandInBuffers {
  const total = pieces.reduce((n, piece) => n + pieceLength(piece), 0);
  return {...concatenatePieces(pieces, total, columns, layer), count: total};
}

function concatenatePieces(
  pieces: readonly StandInPiece[],
  total: number,
  columns: Iterable<string>,
  layer = ''
): Assembled['standIn'] {
  const ids = new BigUint64Array(total);
  const positions = new Float32Array(total * 2);
  // Ordinal 0 is no artifact, which the texture draws neutral.
  const ordinals = new Float32Array(total);
  // 1 is matched, which every mark is when no highlight is set.
  const highlights = new Float32Array(total).fill(1);
  let o = 0;
  for (const piece of pieces) {
    const membership = layer ? piece.band.membership[layer] : undefined;
    const bits = piece.band.highlightBits;
    if (piece.indices) {
      for (const i of piece.indices) {
        ids[o] = piece.band.ids[i]!;
        positions[o * 2] = piece.band.positions[i * 2]!;
        positions[o * 2 + 1] = piece.band.positions[i * 2 + 1]!;
        if (membership) ordinals[o] = membership.ordinals[i]!;
        if (bits) highlights[o] = bits[i]!;
        o++;
      }
    } else {
      // A whole band is a block copy up to the piece's density-matching limit.
      const len = pieceLength(piece);
      ids.set(piece.band.ids.subarray(0, len), o);
      positions.set(piece.band.positions.subarray(0, len * 2), o * 2);
      if (membership) ordinals.set(membership.ordinals.subarray(0, len), o);
      if (bits) highlights.set(bits.subarray(0, len), o);
      o += len;
    }
  }
  const scalars: Record<string, ScalarColumn> = {};
  for (const name of new Set(columns)) {
    const column = assembleScalar(name, pieces, total);
    if (column) scalars[name] = column;
  }
  return {ids, positions, scalars, ordinals, highlights};
}

function fromComposition(c: Composition, standIn: Assembled['standIn']): Assembled {
  return {
    depth: c.depth,
    want: c.want,
    version: c.version,
    standInStale: c.standInStale,
    // A tile that serves no point is counted in `tiles` and has no slot to draw.
    bands: c.exact.filter((band) => band.ids.length > 0),
    standIn,
    tiles: c.tiles,
    exactDrawn: c.exactDrawn,
    exactServed: c.exactServed,
    provisional: c.provisional,
    visibleInView: c.visibleInView,
    composition: c
  };
}

/**
 * Materialise a composition into buffers, reusing a held frame's stand-in buffers when core's
 * `fold` returned the same piece list. That keeps the colour memo and deck's upload skip valid.
 * Otherwise the named columns are copied.
 */
export function materialise(
  c: Composition,
  held: Assembled | null,
  columns: Iterable<string>,
  layer = ''
): Assembled {
  if (held && held.composition.standIn === c.standIn) return fromComposition(c, held.standIn);
  return fromComposition(c, concatenatePieces(c.standIn, c.provisional, columns, layer));
}

/** Compose and materialise a replica frame in one step. */
export function assemble(frame: ReplicaFrame, columns?: Iterable<string>, layer = ''): Assembled {
  return materialise(compose(frame), null, columns ?? [], layer);
}

/** Fold fresh exact bands into a frame already on screen. */
export function refreshExact(held: Assembled, bands: Band[], version: number): Assembled {
  return materialise(fold(held.composition, bands, version), held, Object.keys(held.standIn.scalars));
}

/** Fold one column's values across the exact bands and the stand-in marks. */
export function foldBandColumn<T>(
  assembled: Assembled,
  column: string | null,
  seed: T,
  step: (held: T, values: ScalarColumn) => T
): T {
  if (!column) return seed;
  let held = seed;
  for (const band of assembled.bands) {
    const values = band.scalars[column];
    if (values) held = step(held, values);
  }
  const standIn = assembled.standIn.scalars[column];
  if (standIn) held = step(held, standIn);
  return held;
}

/**
 * Throws when the drawn marks on exact tiles differ from what was served, or a non-exact tile
 * carries a count. A dropped mark discloses nothing, so this is a fidelity check, and a lost mark
 * is the usual sign of an assembly bug.
 */
export function assertAssemblyMatchesServed(assembled: Assembled): void {
  assertCompositionMatchesServed(assembled.composition);
}
