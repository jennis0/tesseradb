/**
 * The encoding accumulators: which values the marks on screen carry, in the order first seen, and
 * how wide a numeric column has ranged. They live in the store so a host without deck.gl has them;
 * turning a rank into a colour is the renderer's.
 *
 * A value shown as a number is served exact; a mapping (colour, size) the client may compute. None
 * of this is a masked quantity or decides what is drawn.
 *
 * A numeric ramp's domain comes from the marks served, not from the server: a corpus-wide min and
 * max would be an aggregate over items the viewer cannot see. The domain widens as marks arrive and
 * does not narrow, so a pan does not recolour the map, and it resets when the identity key changes.
 */
import type {StandInPiece} from './compose.js';
import type {ArrowType, CategoryValue, DeclaredScalar, ScalarColumn} from './types.js';

/** The storage types whose values are numbers: every integer and float width. */
const NUMBER_TYPES: ReadonlySet<ArrowType> = new Set(['u8', 'u16', 'u32', 'u64', 'i8', 'i16', 'i32', 'i64', 'f32', 'f64']);

/**
 * Whether a declared column can size points: it arrives with each point (`render`), is a number
 * type, and is not a category, whose codes are labels.
 *
 * @internal
 */
export function sizesPoints(column: DeclaredScalar): boolean {
  return column.render && column.category === null && NUMBER_TYPES.has(column.arrowType);
}

/**
 * The range of one numeric column's values among the marks drawn, for a colour ramp. The store's
 * `legend` projection holds one per column in `domains`. It widens as marks arrive and does not
 * narrow until the store is cleared, so a pan does not recolour the map. It is taken from the marks
 * alone, since a range over the whole corpus would be an aggregate over items the viewer may not
 * see.
 *
 * @category Projections
 */
export type Domain = {
  /** The smallest finite value drawn. */
  min: number;
  /** The largest finite value drawn. */
  max: number;
};

/**
 * A palette rank per category code. The store's `legend` projection holds one per column in
 * `ranks`. Codes are ranked by how often they appear among the marks drawn when first seen, and a
 * code keeps its rank until the store is cleared, so a pan does not recolour the map.
 *
 * @category Projections
 */
export type Ranks = Record<number, number>;

/**
 * Whether point `i` of `column` has a value: `false` where the server sent a null. A category
 * column has no nulls, so this is `true` for all its points; there code `0` means no value.
 *
 * @category Projections
 */
export function hasValue(column: ScalarColumn, i: number): boolean {
  return !column.present || column.present[i] === 1;
}

/**
 * A column's values as plain numbers, or `null` if it has no ramp. A point with no value is `NaN`,
 * so it takes no part in a domain and colours as unmapped, as a category's code 0 does.
 *
 * `u64`, `i64` and `timestamp_us` lose precision past 2^53 as doubles. A colour ramp has about 256
 * steps, so the loss does not show here; the decoder keeps the 64-bit arrays for display.
 *
 * @internal
 */
export function numericValues(column: ScalarColumn): ArrayLike<number> | null {
  if (!NUMBER_TYPES.has(column.arrowType) && column.arrowType !== 'timestamp_us') return null;
  const values = column.values as ArrayLike<number | bigint>;
  const wide = column.arrowType === 'u64' || column.arrowType === 'i64' || column.arrowType === 'timestamp_us';
  if (!wide && !column.present) return column.values as ArrayLike<number>;
  const out = new Float64Array(values.length);
  for (let i = 0; i < values.length; i++) out[i] = hasValue(column, i) ? Number(values[i]!) : NaN;
  return out;
}

/**
 * Widens `held` to cover `column`, or establishes it. It does not narrow, so a pan onto a narrow
 * part of the data does not recolour what is on screen. `null` for a column with no numeric reading
 * or no marks.
 *
 * @internal
 */
export function widenDomain(held: Domain | null, column: ScalarColumn): Domain | null {
  return widenDomainOver(held, column, null, Infinity);
}

/** {@link widenDomain} over a stand-in piece's drawn subset: a prefix or an index list. @internal */
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

/** {@link lacksValues} memoised per column, since bands do not change. */
const heldLacks = new WeakMap<object, boolean>();

/**
 * Whether any point of `column` has no finite value: no value at all, or NaN or an infinity, which
 * a size cannot place and draws as a point with no value.
 *
 * @internal
 */
export function lacksValues(column: ScalarColumn): boolean {
  let held = heldLacks.get(column);
  if (held === undefined) {
    const values = numericValues(column);
    held = false;
    if (values) {
      for (let i = 0; i < values.length; i++) {
        if (!Number.isFinite(values[i]!)) {
          held = true;
          break;
        }
      }
    }
    heldLacks.set(column, held);
  }
  return held;
}

/**
 * A uniform sample of one numeric column's values among the marks drawn, for sizing points by
 * rank. The store's `legend` projection holds one per column sized by, in `samples`. A value's rank
 * is where it falls among `values`; the marks are themselves a sample of the viewer's visible set,
 * so a rank is an estimate and is never shown as a statistic.
 *
 * @category Projections
 */
export type ValueSample = {
  /** Up to {@link SAMPLE_SIZE} finite values, drawn uniformly from the marks seen, in ascending order. */
  values: readonly number[];
  /**
   * How many marks with a finite value the sample was drawn from. It grows with each sample
   * published, so it tells one sample from the next.
   */
  seen: number;
};

/** The most values a {@link ValueSample} holds. @internal */
export const SAMPLE_SIZE = 1024;

/**
 * A reservoir of values drawn uniformly from every mark offered to it, each band once. It publishes
 * a sorted {@link ValueSample} when the marks seen have doubled since the last one, so sizes are
 * rewritten a few times as a view fills and then hold still while the viewer pans.
 *
 * @internal
 */
export class ValueReservoir {
  private held: number[] = [];
  private seen = 0;
  private published = 0;
  private offered = new WeakSet<object>();
  /** A fixed-seed generator, so one sequence of bands always gives one sample. */
  private state = 0x2545f491;

  /** Offer a column's values; a column offered before is skipped. */
  offer(column: ScalarColumn): void {
    if (this.offered.has(column)) return;
    this.offered.add(column);
    const values = numericValues(column);
    if (!values) return;
    for (let i = 0; i < values.length; i++) {
      const v = values[i]!;
      if (!Number.isFinite(v)) continue;
      this.seen += 1;
      if (this.held.length < SAMPLE_SIZE) this.held.push(v);
      else {
        const j = Math.floor(this.random() * this.seen);
        if (j < SAMPLE_SIZE) this.held[j] = v;
      }
    }
  }

  /** The sample to publish, or null where the marks seen have not doubled since the last one. */
  take(): ValueSample | null {
    if (this.seen === 0 || this.seen < this.published * 2) return null;
    this.published = this.seen;
    return {values: [...this.held].sort((a, b) => a - b), seen: this.seen};
  }

  private random(): number {
    this.state ^= this.state << 13;
    this.state ^= this.state >>> 17;
    this.state ^= this.state << 5;
    return (this.state >>> 0) / 4294967296;
  }
}

/**
 * Counts each code among the marks on screen. The served marks are a sample of the visible set,
 * which is enough to choose which values get a colour; the result is never shown as a count.
 *
 * @internal
 */
export function countCodes(column: ScalarColumn): Map<number, number> {
  const counts = new Map<number, number>();
  const values = column.values as ArrayLike<number | bigint>;
  for (let i = 0; i < values.length; i++) {
    const code = Number(values[i]);
    if (code === 0) continue; // absent
    counts.set(code, (counts.get(code) ?? 0) + 1);
  }
  return counts;
}

/**
 * {@link countCodes} memoised per column. Bands do not change, so a recount of the frame scans only
 * bands it has not seen.
 */
const heldCounts = new WeakMap<object, Map<number, number>>();

/** @internal */
export function countCodesCached(column: ScalarColumn): Map<number, number> {
  let held = heldCounts.get(column);
  if (!held) {
    held = countCodes(column);
    heldCounts.set(column, held);
  }
  return held;
}

/** {@link countCodes} over a stand-in piece's drawn subset, into `into`. Not memoised: pieces are rebuilt per derive. @internal */
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
 * Extends `held` with ranks for codes it lacks, most frequent first. With more values than palette
 * colours, key order would give the colours to arbitrary values; frequency gives them to what is on
 * screen. Assigned ranks are not reordered, so a pan does not recolour the map. Cleared when the
 * identity key changes.
 *
 * @internal
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

/** The resolved values that hold one of `paletteSize` colours, in rank order: the legend's list. @internal */
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
