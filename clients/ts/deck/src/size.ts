/**
 * Mark size from a declared number column: where each value falls between the smallest and the
 * largest size, under a linear, log or rank scale.
 *
 * The range and the sample a size is placed against come from the marks drawn, which are drawn
 * from this viewer's visible set. Size decides nothing about what is drawn: every served mark is
 * drawn, and one with no value draws at the smallest size, hollow.
 */
import type {LegendProjection, Meta, ScalarColumn} from '@tesseradb/client';
import {numericValues} from '@tesseradb/client/internal';
import {fractionOf} from './colour.js';

/**
 * How a number is placed between the smallest and largest size: `linear` in the value; `log`, which
 * places `log(1 + (v − min))`, so a long tail of counts does not leave most points at the smallest
 * size; or `rank`, the value's rank among a sample of the values drawn, which spreads skewed values
 * evenly. A rank is an estimate from the points drawn, which are themselves a sample.
 *
 * @category Size
 */
export type SizeScale = 'linear' | 'log' | 'rank';

/**
 * The size choices for a number column: the radius in pixels of the smallest and the largest value,
 * and the scale between them.
 *
 * @category Size
 */
export type Sizing = {
  /** The radius in pixels of the smallest value, and of a point with no value. */
  min: number;
  /** The radius in pixels of the largest value. */
  max: number;
  /** How a value is placed between the two. */
  scale: SizeScale;
};

/**
 * The sizing with nothing chosen: from 2 to 9 pixels, on a linear scale.
 *
 * @category Size
 */
export const DEFAULT_SIZING: Sizing = {min: 2, max: 9, scale: 'linear'};

/**
 * How a column is being sized. `none` is one size for every mark. `pending` names a column with no
 * value drawn yet, and draws every mark at the smallest size.
 */
export type SizeEncoding =
  | {kind: 'none'}
  | {kind: 'pending'; column: string}
  | {kind: 'linear' | 'log'; column: string; domain: {min: number; max: number}}
  | {kind: 'rank'; column: string; sample: readonly number[]};

export const NO_SIZE: SizeEncoding = {kind: 'none'};

/** Whether a declared column can size points: a rendered number that is not a category or a time. */
export function sizesPoints(column: Meta['declaredScalars'][number]): boolean {
  return column.render && column.category === null && !['bool', 'utf8', 'timestamp_us'].includes(column.arrowType);
}

/** The size encoding for the legend's `sizeBy`, from the range and sample it holds and the scale chosen. */
export function sizeEncodingOf(meta: Meta | null, legend: LegendProjection | null, scale: SizeScale): SizeEncoding {
  const name = legend?.sizeBy ?? null;
  const column = name === null ? undefined : meta?.declaredScalars.find((c) => c.name === name);
  if (!legend || !column || !sizesPoints(column)) return NO_SIZE;
  if (scale === 'rank') {
    const sample = legend.samples[column.name];
    return sample && sample.values.length > 0 ? {kind: 'rank', column: column.name, sample: sample.values} : {kind: 'pending', column: column.name};
  }
  const domain = legend.domains[column.name];
  return domain ? {kind: scale, column: column.name, domain} : {kind: 'pending', column: column.name};
}

/** A key equal for two encodings that size alike. A domain only widens and a sample is replaced whole, so their bounds and length identify them. */
export function sizeSignature(encoding: SizeEncoding): string {
  switch (encoding.kind) {
    case 'none':
      return 'none';
    case 'pending':
      return `pending|${encoding.column}`;
    case 'rank': {
      const s = encoding.sample;
      return `rank|${encoding.column}|${s.length}|${s[0]}|${s[s.length >> 1]}|${s[s.length - 1]}`;
    }
    default:
      return `${encoding.kind}|${encoding.column}|${encoding.domain.min}|${encoding.domain.max}`;
  }
}

/**
 * Where `v` falls between the smallest size (0) and the largest (1). Under `rank` it is the value's
 * position among the sample, ties sharing the middle of their run, so the sample's smallest value
 * is 0 and its largest 1.
 */
export function sizeFraction(v: number, encoding: SizeEncoding): number {
  switch (encoding.kind) {
    case 'none':
    case 'pending':
      return 0;
    case 'rank': {
      const s = encoding.sample;
      if (s.length === 1) return v < s[0]! ? 0 : v > s[0]! ? 1 : 0.5;
      const below = lowerBound(s, v);
      const through = upperBound(s, v);
      return clamp01((below + through - 1) / 2 / (s.length - 1));
    }
    default: {
      const {min, max} = encoding.domain;
      return max === min ? 0.5 : clamp01(fractionOf(v, encoding.domain, encoding.kind, false));
    }
  }
}

/** The value at fraction `t` of the way from the smallest size to the largest: the inverse of {@link sizeFraction}. */
export function valueAtSize(t: number, encoding: SizeEncoding): number | null {
  const c = clamp01(t);
  switch (encoding.kind) {
    case 'none':
    case 'pending':
      return null;
    case 'rank': {
      const s = encoding.sample;
      return s[Math.round(c * (s.length - 1))]!;
    }
    default: {
      const {min, max} = encoding.domain;
      return encoding.kind === 'log' ? min + Math.expm1(c * Math.log1p(max - min)) : min + c * (max - min);
    }
  }
}

/** The radius in pixels at fraction `t`, between the sizing's smallest and largest. */
export function radiusAt(t: number, sizing: Sizing): number {
  return sizing.min + (sizing.max - sizing.min) * clamp01(t);
}

/**
 * Write the size fraction of `count` marks into `out` from mark `offset`, reading `column` from its
 * index 0: from 0 to 1 for a mark with a value, and -1 for one with none, which draws at the
 * smallest size, hollow. Every mark gets an entry. Under `none` every entry is 0.
 */
export function writeSizes(out: Float32Array, offset: number, count: number, column: ScalarColumn | undefined, encoding: SizeEncoding): void {
  const span = out.subarray(offset, offset + count);
  if (encoding.kind === 'none') {
    span.fill(0);
    return;
  }
  const values = column ? numericValues(column) : null;
  if (!values) {
    span.fill(-1);
    return;
  }
  for (let i = 0; i < count; i++) {
    const v = values[i];
    span[i] = v === undefined || !Number.isFinite(v) ? -1 : sizeFraction(v, encoding);
  }
}

/** {@link writeSizes} into a new buffer, for the stand-in layer rebuilt per frame. */
export function buildSizeAttribute(pointCount: number, scalars: Record<string, ScalarColumn>, encoding: SizeEncoding): Float32Array {
  const out = new Float32Array(pointCount);
  writeSizes(out, 0, pointCount, encoding.kind === 'none' ? undefined : scalars[encoding.column], encoding);
  return out;
}

const clamp01 = (t: number) => (Number.isFinite(t) ? Math.min(1, Math.max(0, t)) : 0);

/** The first index in the sorted `s` whose value is not below `v`. */
function lowerBound(s: readonly number[], v: number): number {
  let lo = 0;
  let hi = s.length;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (s[mid]! < v) lo = mid + 1;
    else hi = mid;
  }
  return lo;
}

/** The first index in the sorted `s` whose value is above `v`. */
function upperBound(s: readonly number[], v: number): number {
  let lo = 0;
  let hi = s.length;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (s[mid]! <= v) lo = mid + 1;
    else hi = mid;
  }
  return lo;
}
