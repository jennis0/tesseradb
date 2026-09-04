/**
 * The encoding accumulators: which values the marks on screen carry, in what order they were
 * first seen, and how wide a numeric column has ranged.
 *
 * In the store rather than the vis layer (design client-components §4, amending
 * client-architecture's review finding F8): they are computed from held data — *what was
 * counted* — and a host on a plain canvas needs them without deck.gl. What stays vis-side is
 * rank-to-colour: the palette applied.
 *
 * **The boundary** (client-interaction §9): if it renders as a number, it is served exact; if it
 * renders as a mapping — colour, size, transfer function — the client may compute it. Nothing
 * here is a masked quantity and nothing here decides what is drawn.
 *
 * **The domain a numeric ramp spans is derived from the marks actually served**, never published
 * by the server. A corpus-wide min/max would be an aggregate over items the viewer cannot see —
 * the one trap the design names by name — so the ramp rescales as the viewport moves, and the
 * legend says so rather than hiding it. It is **sticky**: widened as new marks arrive, never
 * narrowed, so a pan does not recolour the whole map under the user. It resets on identity-key
 * change, a different mask being a different domain.
 */
import type {StandInPiece} from './compose.js';
import type {CategoryValue, ScalarColumn} from './types.js';

/** The sticky domain for one numeric column. */
export type Domain = {min: number; max: number};

/** Palette rank per code, as a plain object so it lives comfortably in a projection. */
export type Ranks = Record<number, number>;

/**
 * A column's values as plain numbers, or `null` if it has no ramp.
 *
 * `i64` and `timestamp_us` arrive as `BigInt64Array`. Narrowing to a double loses precision past
 * 2⁵³ — but a colour ramp has ~256 distinguishable steps, so the loss is invisible *here* and
 * would be unacceptable anywhere the value is displayed. That is why this is local to encoding and
 * the decoder still hands back the `BigInt64Array`.
 */
export function numericValues(column: ScalarColumn): ArrayLike<number> | null {
  switch (column.arrowType) {
    case 'bool':
    case 'utf8':
      return null;
    case 'u64':
    case 'i64':
    case 'timestamp_us': {
      const out = new Float64Array(column.values.length);
      for (let i = 0; i < column.values.length; i++) out[i] = Number(column.values[i]!);
      return out;
    }
    default:
      return column.values;
  }
}

/**
 * Widen `held` to cover `column`, or establish it. Never narrows: a pan that happens to land on a
 * narrow view of the data must not recolour everything that is still on screen.
 *
 * Returns `null` for a column with no numeric reading (`bool`, `utf8`) or no marks.
 */
export function widenDomain(held: Domain | null, column: ScalarColumn): Domain | null {
  return widenDomainOver(held, column, null, Infinity);
}

/**
 * {@link widenDomain} over a prefix or an index list of the column — a stand-in piece's drawn
 * subset, which is never materialised in the store.
 */
export function widenDomainOver(
  held: Domain | null,
  column: ScalarColumn,
  indices: readonly number[] | null,
  limit: number
): Domain | null {
  const values = numericValues(column);
  if (!values || values.length === 0) return held;
  let min = Infinity;
  let max = -Infinity;
  const take = (v: number | undefined) => {
    if (v === undefined || !Number.isFinite(v)) return;
    if (v < min) min = v;
    if (v > max) max = v;
  };
  if (indices) {
    const n = Math.min(indices.length, limit);
    for (let j = 0; j < n; j++) take(values[indices[j]!]);
  } else {
    const n = Math.min(values.length, limit);
    for (let i = 0; i < n; i++) take(values[i]);
  }
  if (min === Infinity) return held;
  if (!held) return {min, max};
  return {min: Math.min(held.min, min), max: Math.max(held.max, max)};
}

/**
 * Count each code's occurrences among the marks on screen.
 *
 * Over the *served* marks, which is a sample of the visible set rather than the set — fine here,
 * because the result decides only which values get a colour. It never becomes a displayed count:
 * a per-value total would be C8's `and_cardinality` against `M_auth`, which is a different
 * question and is deliberately not asked.
 */
export function countCodes(column: ScalarColumn): Map<number, number> {
  const counts = new Map<number, number>();
  const values = column.values as ArrayLike<number | bigint>;
  for (let i = 0; i < values.length; i++) {
    const code = Number(values[i]);
    if (code === 0) continue; // *absent* is not a value
    counts.set(code, (counts.get(code) ?? 0) + 1);
  }
  return counts;
}

/**
 * {@link countCodes}, memoised on the column object.
 *
 * Bands are immutable, so a band's code counts never change — but the legend re-counts the whole
 * frame whenever new codes could have arrived, and scanning every mark of every held band measured
 * 54 ms per count at 10^5 bands. Memoised per column, a recount scans only the bands it has never
 * seen and merges small held maps for the rest: the work becomes proportional to what arrived.
 */
const heldCounts = new WeakMap<object, Map<number, number>>();

export function countCodesCached(column: ScalarColumn): Map<number, number> {
  let held = heldCounts.get(column);
  if (!held) {
    held = countCodes(column);
    heldCounts.set(column, held);
  }
  return held;
}

/** {@link countCodes} over a stand-in piece's drawn subset, into `into`. Never memoised: pieces are rebuilt per derive. */
export function countCodesInPiece(into: Map<number, number>, piece: StandInPiece, column: string): void {
  const values = piece.band.scalars[column]?.values as ArrayLike<number | bigint> | undefined;
  if (!values) return;
  const add = (raw: number | bigint | undefined) => {
    const code = Number(raw);
    if (code === 0 || Number.isNaN(code)) return;
    into.set(code, (into.get(code) ?? 0) + 1);
  };
  if (piece.indices) {
    const n = Math.min(piece.indices.length, piece.limit);
    for (let j = 0; j < n; j++) add(values[piece.indices[j]!]);
  } else {
    const n = Math.min(values.length, piece.limit);
    for (let i = 0; i < n; i++) add(values[i]);
  }
}

/**
 * Extend `held` with ranks for any code it does not carry, **most frequent first**.
 *
 * **Frequency, not key order.** A 171-value vocabulary against a 12-colour palette has to choose
 * which values get a colour, and alphabetical order chooses arbitrarily: on the arXiv fixture it
 * spends the whole palette on `acc-phys`…`atom-ph` and greys out every category anyone is looking
 * at. Ranking by what is actually on screen puts the colours where the marks are.
 *
 * **Sticky, for the same reason the numeric domain is.** Ranks already assigned are never
 * reordered, so a pan that changes which value is commonest does not recolour the map underneath
 * the reader. The first viewport establishes the palette; later ones only append. Cleared on
 * identity-key change, a different mask being a different set of values.
 */
export function extendRanks(held: Ranks, counts: Map<number, number>): Ranks {
  const unranked = [...counts.entries()].filter(([code]) => held[code] === undefined);
  if (unranked.length === 0) return held;
  unranked.sort((a, b) => b[1] - a[1] || a[0] - b[0]);
  const next = {...held};
  let rank = Object.keys(held).length;
  for (const [code] of unranked) next[code] = rank++;
  return next;
}

/** The resolved values that hold one of `paletteSize` colours, in rank order — the legend's list. */
export function rankedValues(
  values: readonly CategoryValue[],
  ranks: Ranks,
  paletteSize: number
): {value: CategoryValue; rank: number}[] {
  return values
    .map((value) => ({value, rank: ranks[value.code] ?? Number.MAX_SAFE_INTEGER}))
    .filter((v) => v.rank < paletteSize)
    .sort((a, b) => a.rank - b.rank);
}
