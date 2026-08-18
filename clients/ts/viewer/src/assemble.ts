import {
  compose,
  fold,
  type Band,
  type ComposedTile,
  type Composition,
  type ReplicaFrame,
  type ScalarColumn,
  type StandInPiece,
  type TileRect
} from '@tessera/client';

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
        (column.values as unknown as {view(a: number, b: number): ArrayLike<number>}).slice(0, len),
        o
      );
      o += len;
    }
  }
  return {arrowType: first.arrowType, values: out} as unknown as ScalarColumn;
}

/** The stand-in piece list as concatenated buffers — the one copy this file still pays. */
function materialiseStandIn(
  pieces: readonly StandInPiece[],
  total: number,
  columns: Iterable<string>
): Assembled['standIn'] {
  const ids = new BigUint64Array(total);
  const positions = new Float32Array(total * 2);
  let o = 0;
  for (const piece of pieces) {
    if (piece.indices) {
      for (const i of piece.indices) {
        ids[o] = piece.band.ids[i]!;
        positions[o * 2] = piece.band.positions[i * 2]!;
        positions[o * 2 + 1] = piece.band.positions[i * 2 + 1]!;
        o++;
      }
    } else {
      // A whole band is a memcpy; `subarray` honours the density-matching limit — a view, no copy.
      const len = pieceLength(piece);
      ids.set(piece.band.ids.subarray(0, len), o);
      positions.set(piece.band.positions.subarray(0, len * 2), o * 2);
      o += len;
    }
  }
  const scalars: Record<string, ScalarColumn> = {};
  for (const name of new Set(columns)) {
    const column = assembleScalar(name, pieces, total);
    if (column) scalars[name] = column;
  }
  return {ids, positions, scalars};
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

/** Assemble a frame — core decides what contributes; this materialises it. */
export function assemble(frame: ReplicaFrame, columns?: Iterable<string>): Assembled {
  const c = compose(frame);
  return fromComposition(c, materialiseStandIn(c.standIn, c.provisional, columns ?? []));
}

/**
 * Fold fresh exact bands into a frame already on screen.
 *
 * Core's `fold` recomputes the exact half and filters the stand-in pieces against the new exact
 * ground; when nothing was filtered the piece list comes back by reference and the held buffers
 * — and with them the colour memo and deck's upload skip — survive untouched.
 */
export function refreshExact(held: Assembled, bands: Band[], version: number): Assembled {
  const folded = fold(held.composition, bands, version);
  const standIn =
    folded.standIn === held.composition.standIn
      ? held.standIn
      : materialiseStandIn(folded.standIn, folded.provisional, Object.keys(held.standIn.scalars));
  return fromComposition(folded, standIn);
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
  if (assembled.exactDrawn !== assembled.exactServed) {
    throw new Error(
      `assembly: drawing ${assembled.exactDrawn} marks across exact tiles but the server served ` +
        `${assembled.exactServed}. A mark was lost between the replica and the buffers.`
    );
  }
  for (const tile of assembled.tiles) {
    if (!tile.exact && tile.counts !== null) {
      throw new Error(
        `tile ${tile.prefix} draws a superset of its served set but carries counts. ` +
          `A superset of marks must never be read as density.`
      );
    }
  }
}
