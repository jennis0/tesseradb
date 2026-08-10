import {BandCache, WORLD_SIZE, type Band, type ReplicaFrame, type TileRect} from '@tessera/client';
import type {ScalarColumn} from '@tessera/client';

/**
 * Turning a frame of per-tile bands into the buffers one `ScatterplotLayer` draws.
 *
 * **One layer, not one per tile.** deck.gl's `TileLayer` would give the tile lifecycle for free but
 * produces a sublayer per tile, and the request sizing here is `budget / m_target` tiles per view —
 * hundreds to thousands. It also derives depth from zoom, which is exactly the coupling the depth
 * budget exists to break. So deck.gl stays a renderer and the tile lifecycle lives in the replica.
 *
 * **Exact bands are not copied here.** They go to `slab.ts`, which writes each band once into a slot
 * it keeps, so a frame that adds nothing costs nothing. What is left for this file is the part that
 * genuinely cannot be retained: the **stand-in** bands, whose extent is clipped to whatever ground is
 * not yet held at the requested depth and therefore changes every time a response lands. Those are
 * concatenated per frame, as everything used to be.
 *
 * So this file's output is two things that were previously one — the accounting over exact bands
 * (counts, calibration, the fidelity check), and the buffers for the stand-in layer.
 */

/** One tile's contribution to the draw, and the authority it rests on. */
export type AssembledTile = {
  prefix: bigint;
  /** The depth `prefix` is addressed at — the band's own, which for a stand-in is not the frame's. */
  depth: number;
  /**
   * False where the marks came from an ancestor or from descendants, in which case they are a
   * superset of what the definition serves for this tile. Presentation only: no count may be shown
   * against such a tile, and it is excluded from the drawn-count assertion.
   */
  exact: boolean;
  /** Marks this entry puts on screen — for an exact tile, exactly `served`. */
  drawn: number;
  /** The server's own counts, present only for an exact tile. */
  counts: {visible: bigint; matched: bigint; served: number} | null;
};

export type Assembled = {
  /** The depth every exact band sits at — the slab's partition, with the identity key. */
  depth: number;
  /**
   * The region this was derived for, and the store's change counter at the time.
   *
   * Together they say when it stops being the answer: a view wanting ground inside `want`, at the
   * same depth, against an unchanged store, is answered by exactly these marks — so the redraw can
   * be skipped rather than repeated. See `viewportLayer.ts`.
   */
  want: TileRect;
  version: number;
  /**
   * The exact bands this frame draws — handed to the slab, never copied.
   *
   * Held as a list rather than as concatenated columns because everything downstream of it either
   * folds over bands (the colour domain, the category ranks) or wants the slab's own buffers (the
   * layer, picking). Concatenating was the cost this file existed to pay and no longer does.
   */
  bands: Band[];
  /** Marks from an ancestor or descendants, concatenated — every one of them stale-marked. */
  standIn: {
    ids: BigUint64Array;
    /** Interleaved x,y in deck.gl world units, narrowed to the f32 a binary attribute takes. */
    positions: Float32Array;
    scalars: Record<string, ScalarColumn>;
  };
  tiles: AssembledTile[];
  /** Marks the exact bands carry, and what the server said it served for those same tiles. */
  exactDrawn: number;
  exactServed: number;
  /** How many stand-in marks — `standIn.ids.length`, named for the panels that report it. */
  provisional: number;
  /** Σ visible over exact tiles: the number channel, and `calibrate`'s saturation term. */
  visibleInView: number;
  /**
   * True where the stand-ins rode along from an older frame (`refreshExact`) rather than being
   * derived for this one. What the settle pass exists to repair — and, when false on a frame whose
   * version still matches the store, proof there is nothing left for a settle to do.
   */
  standInStale: boolean;
};

/** Everything drawn for this frame, before the slab's retained marks are added. */
export function assembledMarks(assembled: Assembled): number {
  return assembled.exactDrawn + assembled.provisional;
}

/**
 * Fold fresh exact bands into a frame already on screen, keeping its stand-ins.
 *
 * **The stand-in walk is the expensive half of deriving a frame, and an arrival does not need it.**
 * New bands only ever add exact ground; the held stand-ins become one arrival stale — some of their
 * marks now sit over ground that has exact bands, which draws a few marks twice until the settle
 * pass rebuilds them properly. That is a superset of the same served points for a bounded moment
 * (§7.2's nesting: a coarser band's marks restricted to a tile contain the tile's own served set),
 * traded against re-deriving 10^4 stand-in bands inside the arrival's own frame.
 *
 * The exact half is fully recomputed — bands, counts, tiles — so the fidelity check and the number
 * channel stay exact. Only the presentation layer is stale, and the count channel never reads from
 * it.
 */
/**
 * **Stand-in marks over ground that is now exact are dropped here, not left to the settle.** The
 * held stand-ins ride along, but wherever an exact band now answers, keeping their marks draws the
 * same ground twice — measured with the density audit as 10^4 tiles at once flashing to ~2x on
 * every arrival and collapsing at the settle, which at a 10^9 corpus is a continuous shimmer under
 * any pan. The filter is one pass over the stand-in marks against the exact tiles' grid; dropping
 * a stand-in mark is presentation, always safe, and these are marks whose ground has just been
 * answered properly.
 */
export function refreshExact(held: Assembled, bands: Band[], version: number): Assembled {
  const tiles: AssembledTile[] = [];
  let exactDrawn = 0;
  let exactServed = 0;
  let visibleInView = 0;
  const dim = 2 ** held.depth;
  const covered = new Set<number>();
  for (const band of bands) {
    if (band.ids.length === 0) continue;
    exactDrawn += band.ids.length;
    exactServed += band.served;
    visibleInView += Number(band.visible);
    covered.add(band.x * dim + band.y);
    tiles.push({
      prefix: band.prefix,
      depth: band.depth,
      exact: true,
      drawn: band.ids.length,
      counts: {visible: band.visible, matched: band.matched, served: band.served}
    });
  }
  const standIn = filterStandIn(held.standIn, held.depth, covered);
  for (const tile of held.tiles) {
    if (!tile.exact) tiles.push(tile);
  }
  return {
    depth: held.depth,
    want: held.want,
    version,
    standInStale: true,
    bands: bands.filter((band) => band.ids.length > 0),
    standIn,
    tiles,
    exactDrawn,
    exactServed,
    provisional: standIn.ids.length,
    visibleInView
  };
}

/**
 * The stand-in buffers with every mark on exact ground removed — the identical object when nothing
 * is removed, which is most folds and what keeps the colour memo and deck's upload skip intact.
 */
function filterStandIn(
  standIn: Assembled['standIn'],
  depth: number,
  covered: Set<number>
): Assembled['standIn'] {
  if (covered.size === 0 || standIn.ids.length === 0) return standIn;
  const dim = 2 ** depth;
  const span = WORLD_SIZE / dim;
  const {ids, positions} = standIn;
  const survivors: number[] = [];
  for (let i = 0; i < ids.length; i++) {
    const tx = Math.floor(positions[i * 2]! / span);
    const ty = Math.floor(positions[i * 2 + 1]! / span);
    if (!covered.has(tx * dim + ty)) survivors.push(i);
  }
  if (survivors.length === ids.length) return standIn;

  const keptIds = new BigUint64Array(survivors.length);
  const keptPositions = new Float32Array(survivors.length * 2);
  for (let o = 0; o < survivors.length; o++) {
    const i = survivors[o]!;
    keptIds[o] = ids[i]!;
    keptPositions[o * 2] = positions[i * 2]!;
    keptPositions[o * 2 + 1] = positions[i * 2 + 1]!;
  }
  const scalars: Record<string, ScalarColumn> = {};
  for (const [name, column] of Object.entries(standIn.scalars)) {
    if (column.arrowType === 'bool' || column.arrowType === 'utf8') {
      const source = column.values as unknown[];
      scalars[name] = {
        arrowType: column.arrowType,
        values: survivors.map((i) => source[i])
      } as ScalarColumn;
    } else {
      const source = column.values as unknown as {[i: number]: unknown};
      const Ctor = (column.values as unknown as {constructor: new (n: number) => unknown})
        .constructor;
      const kept = new Ctor(survivors.length) as unknown as {[i: number]: unknown};
      for (let o = 0; o < survivors.length; o++) kept[o] = source[survivors[o]!];
      scalars[name] = {arrowType: column.arrowType, values: kept} as unknown as ScalarColumn;
    }
  }
  return {ids: keptIds, positions: keptPositions, scalars};
}

type Piece = {band: Band; indices: number[] | null; limit?: number};

function pieceLength(piece: Piece): number {
  const whole = piece.indices ? piece.indices.length : piece.band.ids.length;
  return piece.limit === undefined ? whole : Math.min(whole, piece.limit);
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
      const len = pieceLength(piece);
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

/**
 * Assemble a frame.
 *
 * Positions narrow from `f64` cell space to `f32` world here, which is the single place precision
 * is spent — the same conversion `positionsToWorld` performs for a whole response.
 */
export function assemble(frame: ReplicaFrame, columns?: Iterable<string>): Assembled {
  const tiles: AssembledTile[] = [];
  const bands: Band[] = [];
  const pieces: Piece[] = [];
  let total = 0;
  let exactDrawn = 0;
  let exactServed = 0;
  let visibleInView = 0;

  // **Walks the bands the replica holds, not the tiles the viewport spans.** A settled view spans
  // ~16.5k tiles of which ~450 carry anything, so iterating what is held is two orders of magnitude
  // less work than iterating what was asked about — and it is the same list either way.
  for (const band of frame.exact) {
    if (band.ids.length === 0) continue;
    bands.push(band);
    exactDrawn += band.ids.length;
    exactServed += band.served;
    visibleInView += Number(band.visible);
    tiles.push({
      prefix: band.prefix,
      depth: band.depth,
      exact: true,
      drawn: band.ids.length,
      counts: {visible: band.visible, matched: band.matched, served: band.served}
    });
  }

  // Bands from another depth, admitted by the replica only over ground not held at this one. An
  // ancestor is restricted to the wanted region by Morton prefix — sound because a parent's band is
  // a prefix of its own visible set in identity order, so its restriction is a prefix of the
  // child's.
  //
  // **A descendant stand-in is density-matched by prefix, per DRAWN TILE, not per band.** Four
  // children carry ~4x the marks the drawn depth would serve for their parent, so drawing them
  // whole renders a patch visibly denser than its exactly-covered neighbours. Each band draws only
  // an id-order prefix of itself — the one subset a client may draw (`delta-serving.md` §7;
  // anything else makes the cache a second sampler) — sized so the tile's total approximates what
  // the drawn depth would serve. The sizing must aggregate over the tile: a per-band rule with a
  // one-mark floor hands a drawn tile holding a thousand five-mark depth-10 bands a thousand marks
  // where its own depth would serve a handful — patches of the map two hundred times too dense, in
  // clumps, persisting until the coarse fetch lands. One floor per tile keeps every tile visibly
  // covered; the largest-remainder split spends the rest where the data is. Ancestors have no
  // matching move: marks cannot be invented, so their patches stay sparse until data lands, which
  // reads as loading rather than as wrongness.
  const fallback: {band: Band; indices: number[] | null; limit: number}[] = [];
  const groups = new Map<bigint, number[]>();
  for (const {band, clip} of frame.fallback) {
    const indices = BandCache.restrictToRect(band, frame.depth, clip);
    const length = indices ? indices.length : band.ids.length;
    if (length === 0) continue;
    // Truncated in place rather than copied: `restrictToRect` built this array for this call, so
    // shortening it is free where a `slice` re-copies the kept prefix.
    if (indices && indices.length > length) indices.length = length;
    const at = fallback.length;
    fallback.push({band, indices, limit: length});
    if (band.depth > frame.depth) {
      const ancestor = band.prefix >> BigInt(2 * (band.depth - frame.depth));
      const held = groups.get(ancestor);
      if (held) held.push(at);
      else groups.set(ancestor, [at]);
    }
  }
  for (const members of groups.values()) {
    const shares = members.map(
      (i) => fallback[i]!.limit / 4 ** (fallback[i]!.band.depth - frame.depth)
    );
    const target = Math.max(1, Math.round(shares.reduce((a, s) => a + s, 0)));
    const floors = shares.map(Math.floor);
    let remaining = target - floors.reduce((a, n) => a + n, 0);
    const byFraction = shares
      .map((s, j) => [s - Math.floor(s), j] as const)
      .sort((a, b) => b[0] - a[0]);
    for (const [, j] of byFraction) {
      if (remaining <= 0) break;
      floors[j]! += 1;
      remaining -= 1;
    }
    members.forEach((i, j) => {
      fallback[i]!.limit = Math.min(fallback[i]!.limit, floors[j]!);
    });
  }
  for (const piece of fallback) {
    if (piece.limit === 0) continue;
    if (piece.indices && piece.indices.length > piece.limit) piece.indices.length = piece.limit;
    pieces.push(piece);
    total += piece.limit;
    tiles.push({
      prefix: piece.band.prefix,
      depth: piece.band.depth,
      exact: false,
      drawn: piece.limit,
      counts: null
    });
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
      // this replaces was the bulk of a 136 ms assembly at 10^6 marks. `subarray` honours the
      // density-matching limit for a descendant drawn whole — a view, no copy.
      const len = pieceLength(piece);
      ids.set(piece.band.ids.subarray(0, len), o);
      positions.set(piece.band.positions.subarray(0, len * 2), o * 2);
      o += len;
    }
  }

  const names = new Set<string>(columns ?? []);
  const scalars: Record<string, ScalarColumn> = {};
  for (const name of names) {
    const column = assembleScalar(name, pieces, total);
    if (column) scalars[name] = column;
  }

  return {
    depth: frame.depth,
    want: frame.want,
    version: frame.version,
    standInStale: false,
    bands,
    standIn: {ids, positions, scalars},
    tiles,
    exactDrawn,
    exactServed,
    provisional: total,
    visibleInView
  };
}

/**
 * Fold one column's values across the exact bands, for a reader that needs the whole frame.
 *
 * The colour domain and the category ranks are both **accumulators** — widened, never narrowed — so
 * they can be folded band by band and never need the concatenated column that used to be built for
 * them. That is the only thing the concat was still doing for exact marks.
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
 * **Dropping a mark fails in the safe direction.** Every mark the client holds is masked output the
 * server chose to serve, so a thinner picture discloses nothing; the invariants bind the server's
 * selection, and "which marks a client declines to draw is presentation, which marks it is served is
 * I7" (`client-interaction.md`; `caching.md` §11 the same). What a lost mark costs is quality, and
 * that is worth throwing over because it has been the signature of every assembly bug so far — a
 * band written outside its clip, a slot range that went stale — none of which announce themselves.
 *
 * **Exact tiles only.** An ancestor or descendant band draws a superset of what the definition
 * serves, so ranging the equality over them would either fail on correct output or, if loosened to
 * an inequality, stop catching the thing it exists to catch.
 *
 * The second clause is the one nearest to load-bearing: a superset read as density **overstates**,
 * so no non-exact tile may carry a count. That is `delta-serving.md`'s rule, and unlike the first it
 * is about what the viewer is told, not about how much of it is drawn.
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
