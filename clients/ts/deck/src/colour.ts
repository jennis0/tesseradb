/**
 * Mark colour from a declared column: the palette and the numeric ramp applied to ranks and
 * domains the store computes.
 *
 * A number shown to the viewer is served exact; a colour is a mapping the client may compute.
 * Colour decides nothing about what is drawn: every served mark gets a colour, and a value that
 * does not resolve gets grey.
 */
import type {CategoryValue, ScalarColumn} from '@tesseradb/client';
import {hasValue, numericValues, rankedValues, type Ranks} from '@tesseradb/client';

export {
  countCodes,
  countCodesCached,
  extendRanks,
  numericValues,
  widenDomain,
  type Domain,
  type Ranks
} from '@tesseradb/client';

/** RGBA, 0–255, the form deck.gl's binary `getFillColor` attribute wants. */
type Rgba = readonly [number, number, number, number];

const ALPHA = 200;

/** The colour of every mark when no column is encoded. */
export const UNIFORM: Rgba = [120, 190, 255, ALPHA];

/**
 * Everything unmappable: the absent sentinel (code 0), a number with no value, a code no key
 * explains, and any value past the palette's end. A low-saturation grey that reads as "no value"
 * and stays visible against the background, since these marks are served and drawn.
 */
export const UNMAPPED: Rgba = [110, 118, 132, ALPHA];

/**
 * A qualitative palette, indexed by a value's rank among the resolved values ordered by
 * frequency. Codes are random within the declared width, so their order means nothing. Adjacent
 * entries differ in both hue and lightness, since adjacent legend entries are the pairs a reader
 * compares.
 */
const PALETTE: readonly Rgba[] = [
  [102, 194, 255, ALPHA],
  [255, 158, 74, ALPHA],
  [122, 214, 148, ALPHA],
  [237, 118, 137, ALPHA],
  [190, 160, 255, ALPHA],
  [214, 178, 118, ALPHA],
  [246, 138, 214, ALPHA],
  [150, 210, 214, ALPHA],
  [222, 214, 110, ALPHA],
  [138, 166, 246, ALPHA],
  [246, 178, 158, ALPHA],
  [166, 206, 110, ALPHA]
];

export const PALETTE_SIZE = PALETTE.length;

/** A numeric column's colour ramp, from the domain's minimum to its maximum. */
const RAMP_LOW: Rgba = [40, 60, 140, ALPHA];
const RAMP_HIGH: Rgba = [255, 214, 120, ALPHA];

/** How a column is being coloured. `uniform` is the default and the fallback for anything unmapped. */
export type Encoding =
  | {kind: 'uniform'}
  | {kind: 'unmapped'}
  | {kind: 'category'; column: string; rankOfCode: Ranks}
  | {kind: 'numeric'; column: string; domain: {min: number; max: number}};

/** The resolved values that hold a palette colour, in rank order: the legend's list. */
export function paletteValues(
  values: readonly CategoryValue[],
  ranks: Ranks
): {value: CategoryValue; rank: number}[] {
  return rankedValues(values, ranks, PALETTE.length);
}

/** The colour for palette rank `i`, or {@link UNMAPPED} past the palette's end. */
export function colourOfRank(rank: number | undefined): Rgba {
  if (rank === undefined || rank >= PALETTE.length) return UNMAPPED;
  return PALETTE[rank]!;
}

/** The ramp colour for `t` in `[0, 1]`. */
export function colourOfFraction(t: number): Rgba {
  const c = Math.min(1, Math.max(0, t));
  return [
    Math.round(RAMP_LOW[0] + (RAMP_HIGH[0] - RAMP_LOW[0]) * c),
    Math.round(RAMP_LOW[1] + (RAMP_HIGH[1] - RAMP_LOW[1]) * c),
    Math.round(RAMP_LOW[2] + (RAMP_HIGH[2] - RAMP_LOW[2]) * c),
    ALPHA
  ];
}

/**
 * Colour `count` marks into `out` starting at mark `offset`, reading `column` from its index 0.
 * Per band, so the slab colours an arriving band without touching resident marks. Every mark gets
 * an entry, grey where the value does not resolve, because an unwritten span is transparent black.
 */
export function writeColours(
  out: Uint8Array,
  offset: number,
  count: number,
  column: ScalarColumn | undefined,
  encoding: Encoding
): void {
  const out32 = new Uint32Array(out.buffer, out.byteOffset + offset * 4, count);

  if (encoding.kind === 'uniform' || encoding.kind === 'unmapped') {
    out32.fill(packRgba(encoding.kind === 'uniform' ? UNIFORM : UNMAPPED));
    return;
  }
  if (!column) {
    out32.fill(packRgba(UNMAPPED));
    return;
  }
  if (encoding.kind === 'category') {
    const codes = column.values as ArrayLike<number | bigint>;
    const packed = new Map<number, number>();
    for (let i = 0; i < count; i++) {
      const code = Number(codes[i]);
      let p = packed.get(code);
      if (p === undefined) {
        p = packRgba(colourOfRank(encoding.rankOfCode[code]));
        packed.set(code, p);
      }
      out32[i] = p;
    }
    return;
  }
  const values = numericValues(column);
  if (!values) {
    out32.fill(packRgba(UNMAPPED));
    return;
  }
  const {min, max} = encoding.domain;
  const span = max - min;
  const ramp = new Uint32Array(256);
  for (let i = 0; i < 256; i++) ramp[i] = packRgba(colourOfFraction(i / 255));
  const unmapped = packRgba(UNMAPPED);
  for (let i = 0; i < count; i++) {
    const v = values[i];
    if (v === undefined || !Number.isFinite(v)) {
      out32[i] = unmapped;
      continue;
    }
    const t = span === 0 ? 0.5 : (v - min) / span;
    out32[i] = ramp[Math.max(0, Math.min(255, Math.round(t * 255)))]!;
  }
}

/** An RGBA tuple as the `u32` a `Uint32Array` view over the same bytes reads, in platform byte order. */
const packScratch = new Uint8Array(4);
const packScratch32 = new Uint32Array(packScratch.buffer);

function packRgba(c: Rgba): number {
  packScratch[0] = c[0];
  packScratch[1] = c[1];
  packScratch[2] = c[2];
  packScratch[3] = c[3];
  return packScratch32[0]!;
}

/** {@link writeColours} into a new buffer, for the stand-in layer rebuilt per frame. */
export function buildColourAttribute(
  pointCount: number,
  scalars: Record<string, ScalarColumn>,
  encoding: Encoding
): Uint8Array {
  const out = new Uint8Array(pointCount * 4);
  const column = encoding.kind === 'category' || encoding.kind === 'numeric' ? scalars[encoding.column] : undefined;
  writeColours(out, 0, pointCount, column, encoding);
  return out;
}

/** `rgb(...)` for a legend swatch. */
export function css(c: Rgba): string {
  return `rgb(${c[0]}, ${c[1]}, ${c[2]})`;
}

/** A column's value at one point as text, with a category shown as its key, not its code. */
export function formatScalar(
  column: ScalarColumn,
  index: number,
  keyOfCode: Map<number, string> | null
): string {
  const raw = column.values[index];
  if (raw === undefined) return '—';
  if (!hasValue(column, index)) return 'absent';
  if (keyOfCode) {
    const code = Number(raw);
    if (code === 0) return 'absent';
    return keyOfCode.get(code) ?? `code ${code} (unresolved)`;
  }
  if (column.arrowType === 'timestamp_us') {
    return new Date(Number(raw as bigint) / 1000).toISOString();
  }
  return String(raw);
}
