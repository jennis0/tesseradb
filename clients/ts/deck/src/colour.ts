/**
 * Turning a declared column into mark colour — the vis half of encoding.
 *
 * **The boundary this file sits on** (client-interaction §9): *if it renders as a number, it is
 * served exact; if it renders as a mapping — colour, size, transfer function — the client may
 * compute it.* Nothing here is a masked quantity and nothing here decides what is drawn. Every
 * served mark gets a colour, always; an unresolvable value gets grey rather than being dropped,
 * because dropping it would make this file a second selection rule and break I7.
 *
 * **The accumulators moved to the store** (`@tesseradb/client`, design client-components §4): which
 * values the marks carry, in what order they were first seen, and how wide a numeric column has
 * ranged is *what was counted*, and a host on a plain canvas needs it without deck.gl. What stays
 * here is rank-to-colour: the palette applied. `Domain`, `Ranks`, `widenDomain`, `countCodes`,
 * `extendRanks` and `numericValues` are re-exported from core so the viewer has one source.
 */
import type {CategoryValue, ScalarColumn} from '@tesseradb/client';
import {numericValues, rankedValues, type Ranks} from '@tesseradb/client';

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

/** The uniform colour, and what the map looked like before any of this existed. */
export const UNIFORM: Rgba = [120, 190, 255, ALPHA];

/**
 * Everything unmappable: the *absent* sentinel (code 0), a code no key explains, and any value
 * past the palette's end.
 *
 * **Deliberately legible rather than invisible.** These marks are served and must be drawn (I7),
 * so the colour has to read as "no value" without reading as "no mark" — a low-saturation grey
 * that recedes behind the palette without disappearing against the background.
 */
export const UNMAPPED: Rgba = [110, 118, 132, ALPHA];

/**
 * A qualitative palette, indexed by a value's **rank among the resolved values ordered by
 * frequency** — never by its code (codes are drawn at random from the declared width, so their
 * numeric order means nothing). Twelve hues, ordered so neighbours differ in both hue and
 * lightness — adjacent legend entries are the pairs a reader most needs to tell apart.
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

/** A numeric column's colour ramp: min → max, low to high. */
const RAMP_LOW: Rgba = [40, 60, 140, ALPHA];
const RAMP_HIGH: Rgba = [255, 214, 120, ALPHA];

/** How a column is being coloured. `uniform` is the default and the fallback for anything unmapped. */
export type Encoding =
  | {kind: 'uniform'}
  | {kind: 'unmapped'}
  | {kind: 'category'; column: string; rankOfCode: Ranks}
  | {kind: 'numeric'; column: string; domain: {min: number; max: number}};

/** The resolved values that hold a palette colour, in rank order — the legend's list. */
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
 * Colour `count` marks into `out` starting at mark `offset`, reading `column` from its own index 0.
 *
 * **Written per band rather than per frame**, which is what lets the slab colour an arriving band
 * without touching the marks already resident. **Exactly one entry per mark, unconditionally** —
 * an unresolvable value contributes a grey mark, never an untouched one. Colour is presentation and
 * must never decide what is drawn, and an unwritten span is transparent black.
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

/** An RGBA tuple as the packed `u32` a `Uint32Array` view stores — endian-correct via the scratch. */
const packScratch = new Uint8Array(4);
const packScratch32 = new Uint32Array(packScratch.buffer);

function packRgba(c: Rgba): number {
  packScratch[0] = c[0];
  packScratch[1] = c[1];
  packScratch[2] = c[2];
  packScratch[3] = c[3];
  return packScratch32[0]!;
}

/** {@link writeColours} over a whole buffer of its own — the provisional layer, rebuilt per frame. */
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

/**
 * A column's value at one point, as text — for the item panel, where a category must read as its
 * key rather than as the integer the wire carried.
 */
export function formatScalar(
  column: ScalarColumn,
  index: number,
  keyOfCode: Map<number, string> | null
): string {
  const raw = column.values[index];
  if (raw === undefined) return '—';
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
