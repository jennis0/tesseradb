import {
  assertCompositionMatchesServed,
  compose,
  fold,
  type Band,
  type ComposedTile,
  type Composition,
  type ReplicaFrame,
  type ScalarColumn,
  type StandInPiece,
  type TileRect
} from '@tesseradb/client';

/**
 * Materialising a composition into the buffers one `ScatterplotLayer` draws.
 *
 * **The rules live in `tessera-client` now** (`compose.ts`; client-architecture §5): which bands
 * contribute, at what prefix length, on what authority — density matching, exact-supersession,
 * provenance. What remains here is the genuinely presentational half: turning the stand-in piece
 * list into concatenated typed arrays, and only for the columns a renderer is actually colouring
 * by. Exact bands are never copied here — they go to `slab.ts`, which writes each band once.
 *
 * `Assembled` carries its `composition` so a fold can run in core against it, and so every
 * account of what is drawn — the audit, the fidelity checks — reads one structure.
 */

export type AssembledTile = ComposedTile;

export type Assembled = {
  depth: number;
  want: TileRect;
  version: number;
  standInStale: boolean;
  /** The exact bands this frame draws — handed to the slab, never copied. */
  bands: Band[];
  /** Stand-in marks, concatenated for the provisional layer — every one of them stale-marked. */
  standIn: {
    ids: BigUint64Array;
    positions: Float32Array;
    scalars: Record<string, ScalarColumn>;
    /**
     * The membership ordinal per stand-in mark for the layer it was materialised for, `0` where
     * the band carried no column — the same attribute the slab writes for an exact band, so a
     * stand-in draws through the lookup texture rather than neutral (§5.10).
     *
     * A stand-in is a *set* that oversamples the ground it covers, but each mark in it is a real
     * point of a real band, carrying the ordinal the response that served it named. Colouring it
     * by the artifact it is a member of is therefore exact — the density is the superset, not the
     * membership — and drawing it neutral said *not known here yet* about a point whose cluster
     * the client was holding.
     */
    ordinals: Float32Array;
  };
  tiles: AssembledTile[];
  exactDrawn: number;
  exactServed: number;
  provisional: number;
  visibleInView: number;
  /** The core composition this frame materialises — the authority every reader shares. */
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

/**
 * Concatenate one column across pieces, preserving its declared Arrow type. Only the columns a
 * caller asks for: a redraw needs exactly the one being coloured by.
 */
function assembleScalar(name: string, pieces: readonly StandInPiece[], total: number): ScalarColumn | null {
  const first = pieces.find((p) => p.band.scalars[name])?.band.scalars[name];
  if (!first) return null;

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
        (column.values as unknown as {slice(a: number, b: number): ArrayLike<number>}).slice(0, len),
        o
      );
      o += len;
    }
  }
  return {arrowType: first.arrowType, values: out} as unknown as ScalarColumn;
}

/** The stand-in buffers a `ScatterplotLayer` draws, and how many marks they hold. */
export type StandInBuffers = Assembled['standIn'] & {count: number};

/**
 * The stand-in piece list as concatenated buffers — the one copy this file still pays — for the
 * columns a renderer is colouring by. `TesseraLayer` memoises this on the piece list's identity,
 * which `fold` preserves whenever nothing was filtered.
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
  // Zeros where the layer names none: ordinal 0 is *no artifact*, which the texture draws neutral.
  const ordinals = new Float32Array(total);
  let o = 0;
  for (const piece of pieces) {
    const membership = layer ? piece.band.membership[layer] : undefined;
    if (piece.indices) {
      for (const i of piece.indices) {
        ids[o] = piece.band.ids[i]!;
        positions[o * 2] = piece.band.positions[i * 2]!;
        positions[o * 2 + 1] = piece.band.positions[i * 2 + 1]!;
        if (membership) ordinals[o] = membership.ordinals[i]!;
        o++;
      }
    } else {
      // A whole band is a memcpy; `subarray` honours the density-matching limit — a view, no copy.
      const len = pieceLength(piece);
      ids.set(piece.band.ids.subarray(0, len), o);
      positions.set(piece.band.positions.subarray(0, len * 2), o * 2);
      if (membership) ordinals.set(membership.ordinals.subarray(0, len), o);
      o += len;
    }
  }
  const scalars: Record<string, ScalarColumn> = {};
  for (const name of new Set(columns)) {
    const column = assembleScalar(name, pieces, total);
    if (column) scalars[name] = column;
  }
  return {ids, positions, scalars, ordinals};
}

function fromComposition(c: Composition, standIn: Assembled['standIn']): Assembled {
  return {
    depth: c.depth,
    want: c.want,
    version: c.version,
    standInStale: c.standInStale,
    bands: c.exact,
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
 * Materialise a presented composition into buffers, reusing what a held frame already built.
 *
 * The stand-in piece list comes back from core's `fold` **by reference** when nothing was
 * filtered, and that identity is the signal here: the held buffers — and with them the colour memo
 * and deck's upload skip — survive untouched. A derive, or a fold that filtered a piece, pays the
 * copy for the columns named (the one being coloured by, in practice).
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

/** Compose and materialise a replica frame in one step — the shape the tests drive. */
export function assemble(frame: ReplicaFrame, columns?: Iterable<string>, layer = ''): Assembled {
  return materialise(compose(frame), null, columns ?? [], layer);
}

/** Fold fresh exact bands into a frame already on screen — see {@link materialise}. */
export function refreshExact(held: Assembled, bands: Band[], version: number): Assembled {
  return materialise(fold(held.composition, bands, version), held, Object.keys(held.standIn.scalars));
}

/**
 * Fold one column's values across the exact bands, for a reader that needs the whole frame.
 */
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
 * That the picture matches what was served — a fidelity check, **not an invariant check**.
 *
 * Dropping a mark fails in the safe direction (`client-interaction.md`; `caching.md` §11): a
 * thinner picture discloses nothing. It is thrown over because a lost mark has been the signature
 * of every assembly bug so far. Exact tiles only; and no non-exact tile may carry a count —
 * a superset read as density overstates (`delta-serving.md` §7).
 */
export function assertAssemblyMatchesServed(assembled: Assembled): void {
  assertCompositionMatchesServed(assembled.composition);
}
