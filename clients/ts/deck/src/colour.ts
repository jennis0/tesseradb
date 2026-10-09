/**
 * Mark colour from a declared column: the palette and the numeric ramp applied to ranks and
 * domains the store computes.
 *
 * A number shown to the viewer is served exact; a colour is a mapping the client may compute.
 * Colour decides nothing about what is drawn: every served mark gets a colour, and a value that
 * does not resolve gets grey.
 */
import type {CategoryValue, Ranks, ScalarColumn} from '@mosaica/client';
import {hasValue, numericValues, rankedValues} from '@mosaica/client/internal';

/** RGBA, 0–255, the form deck.gl's binary `getFillColor` attribute wants. */
type Rgba = readonly [number, number, number, number];

/** An RGB colour, each channel 0–255. */
export type Rgb = readonly [number, number, number];

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
 * The name of a qualitative palette for a category column: `tableau10` (Tableau 10, ten colours),
 * `okabe-ito` (Okabe and Ito's eight, distinguishable under the common colour-vision
 * deficiencies), `set2` and `dark2` (ColorBrewer's, eight each).
 *
 * @category Colour
 */
export type CategoryPaletteName = 'tableau10' | 'okabe-ito' | 'set2' | 'dark2';

/**
 * A qualitative palette: its title, whether it stays distinguishable under the common colour-vision
 * deficiencies, and its colours in rank order.
 *
 * @category Colour
 */
export type CategoryPalette = {
  /** The palette's name as a menu shows it. */
  title: string;
  /** Whether the colours stay distinguishable under protanopia, deuteranopia and tritanopia. */
  colourBlindSafe: boolean;
  /** The colours, given to category values in order of rank. */
  colours: readonly Rgb[];
};

const hex = (h: string): Rgb => [parseInt(h.slice(1, 3), 16), parseInt(h.slice(3, 5), 16), parseInt(h.slice(5, 7), 16)];

/**
 * The qualitative palettes, by name. A value's colour is the palette's entry at its rank among the
 * values drawn, ordered by frequency; a value ranked past the palette's end is drawn in
 * {@link UNMAPPED} and listed as "Other".
 *
 * @category Colour
 */
export const CATEGORY_PALETTES: Readonly<Record<CategoryPaletteName, CategoryPalette>> = {
  tableau10: {
    title: 'Tableau 10',
    colourBlindSafe: false,
    colours: ['#4e79a7', '#f28e2b', '#e15759', '#76b7b2', '#59a14f', '#edc948', '#b07aa1', '#ff9da7', '#9c755f', '#bab0ac'].map(hex)
  },
  'okabe-ito': {
    title: 'Okabe-Ito',
    colourBlindSafe: true,
    colours: ['#e69f00', '#56b4e9', '#009e73', '#f0e442', '#0072b2', '#d55e00', '#cc79a7', '#000000'].map(hex)
  },
  set2: {title: 'Set 2', colourBlindSafe: false, colours: ['#66c2a5', '#fc8d62', '#8da0cb', '#e78ac3', '#a6d854', '#ffd92f', '#e5c494', '#b3b3b3'].map(hex)},
  dark2: {title: 'Dark 2', colourBlindSafe: false, colours: ['#1b9e77', '#d95f02', '#7570b3', '#e7298a', '#66a61e', '#e6ab02', '#a6761d', '#666666'].map(hex)}
};

/**
 * The name of a colour ramp for a number column: `viridis`, `cividis` and `magma` (perceptually
 * uniform, dark to light), `greys` (light to dark) and `red-blue` (diverging, red below the
 * midpoint and blue above it).
 *
 * @category Colour
 */
export type RampName = 'viridis' | 'cividis' | 'magma' | 'greys' | 'red-blue';

/**
 * A colour ramp: its title, whether it diverges from a midpoint, and its stops from the low end to
 * the high end, interpolated linearly in RGB.
 *
 * @category Colour
 */
export type Ramp = {
  /** The ramp's name as a menu shows it. */
  title: string;
  /**
   * Whether the ramp diverges from a midpoint: zero where the range spans zero, else the middle of
   * the range. The midpoint takes the ramp's middle colour.
   */
  diverging: boolean;
  /** The stops from the low end to the high end. */
  stops: readonly Rgb[];
};

/**
 * The colour ramps, by name.
 *
 * @category Colour
 */
export const RAMPS: Readonly<Record<RampName, Ramp>> = {
  viridis: {
    title: 'Viridis',
    diverging: false,
    stops: ['#440154', '#482475', '#414487', '#355f8d', '#2a788e', '#21918c', '#22a884', '#44bf70', '#7ad151', '#bddf26', '#fde725'].map(hex)
  },
  cividis: {
    title: 'Cividis',
    diverging: false,
    stops: ['#00224e', '#123570', '#3b496c', '#575d6d', '#707173', '#8a8779', '#a69d75', '#c4b56c', '#e4cf5b', '#fee838'].map(hex)
  },
  magma: {
    title: 'Magma',
    diverging: false,
    stops: ['#000004', '#140e36', '#3b0f70', '#641a80', '#8c2981', '#b73779', '#de4968', '#f7705c', '#fe9f6d', '#fecf92', '#fcfdbf'].map(hex)
  },
  greys: {title: 'Greys', diverging: false, stops: ['#f2f2ef', '#1b1d21'].map(hex)},
  'red-blue': {title: 'Red–Blue, diverging', diverging: true, stops: ['#b2182b', '#ef8a62', '#f7f7f7', '#67a9cf', '#2166ac'].map(hex)}
};

/**
 * How a number is placed on a ramp: `linear` in the value, or `log`, which places
 * `log(1 + (v − min))` so that a long tail does not push most values into one colour. On a
 * diverging ramp, `log` applies either side of the midpoint.
 *
 * @category Colour
 */
export type RampScale = 'linear' | 'log';

/**
 * The colour choices for a column: the palette for a category, the ramp for a number, and colours
 * chosen for single category values. The same choices colour the marks and the legend.
 *
 * @category Colour
 */
export type Colouring = {
  /** The palette a category column's values take in order of rank. */
  palette: CategoryPaletteName;
  /** The ramp a number column's values are placed on. */
  ramp: RampName;
  /** How a number is placed on the ramp. */
  scale: RampScale;
  /** Whether the ramp runs from its high end to its low end. */
  reverse: boolean;
  /**
   * Colours chosen for single category values, per column, per category key, as `#rrggbb`. A value
   * named here takes this colour in place of its palette colour. A key that is not a drawn value of
   * the column colours nothing.
   */
  values: Readonly<Record<string, Readonly<Record<string, string>>>>;
};

/**
 * The colouring with nothing chosen: Tableau 10, Viridis on a linear scale, and no value's colour
 * chosen.
 *
 * @category Colour
 */
export const DEFAULT_COLOURING: Colouring = {palette: 'tableau10', ramp: 'viridis', scale: 'linear', reverse: false, values: {}};

/** How a column is being coloured. `uniform` is the default and the fallback for anything unmapped. */
export type Encoding =
  | {kind: 'uniform'}
  | {kind: 'unmapped'}
  | {
      kind: 'category';
      column: string;
      rankOfCode: Ranks;
      palette: CategoryPaletteName;
      /** Colours chosen for single values, by code. */
      chosen: ReadonlyMap<number, Rgb>;
    }
  | {kind: 'numeric'; column: string; domain: {min: number; max: number}; ramp: RampName; scale: RampScale; reverse: boolean};

/**
 * A key equal for two encodings that colour alike. Ranks and domains only grow, so their size
 * identifies them.
 */
export function encodingSignature(encoding: Encoding): string {
  switch (encoding.kind) {
    case 'uniform':
    case 'unmapped':
      return encoding.kind;
    case 'category': {
      const chosen = [...encoding.chosen].map(([code, c]) => `${code}=${c.join(',')}`).join(';');
      return `category|${encoding.column}|${Object.keys(encoding.rankOfCode).length}|${encoding.palette}|${chosen}`;
    }
    case 'numeric':
      return `numeric|${encoding.column}|${encoding.domain.min}|${encoding.domain.max}|${encoding.ramp}|${encoding.scale}|${encoding.reverse}`;
  }
}

/** The resolved values that hold a palette colour, in rank order: the legend's list. */
export function paletteValues(
  values: readonly CategoryValue[],
  ranks: Ranks,
  palette: CategoryPaletteName = 'tableau10'
): {value: CategoryValue; rank: number}[] {
  return rankedValues(values, ranks, CATEGORY_PALETTES[palette].colours.length);
}

/** The colour for palette rank `rank`, or {@link UNMAPPED} past the palette's end. */
export function colourOfRank(rank: number | undefined, palette: CategoryPaletteName = 'tableau10'): Rgba {
  const colours = CATEGORY_PALETTES[palette].colours;
  if (rank === undefined || rank >= colours.length) return UNMAPPED;
  const c = colours[rank]!;
  return [c[0], c[1], c[2], ALPHA];
}

/** The colour at `t` in `[0, 1]` along `ramp`, from its low end, or from its high end when `reverse`. */
export function colourOfFraction(t: number, ramp: RampName = 'viridis', reverse = false): Rgba {
  const [r, g, b] = rampAt(RAMPS[ramp].stops, reverse ? 1 - t : t);
  return [r, g, b, ALPHA];
}

/** The colour at `t` in `[0, 1]` along `stops`, interpolated linearly in RGB. */
export function rampAt(stops: readonly Rgb[], t: number): Rgb {
  const c = Math.min(1, Math.max(0, Number.isFinite(t) ? t : 0));
  const at = c * (stops.length - 1);
  const i = Math.min(stops.length - 2, Math.floor(at));
  const k = at - i;
  const a = stops[i]!;
  const z = stops[i + 1]!;
  return [Math.round(a[0] + (z[0] - a[0]) * k), Math.round(a[1] + (z[1] - a[1]) * k), Math.round(a[2] + (z[2] - a[2]) * k)];
}

/**
 * Where `v` falls on a ramp over `domain`, from 0 to 1, under `scale`. On a diverging ramp the
 * midpoint ({@link Ramp.diverging}) is 0.5 and the side further from it reaches the end.
 */
export function fractionOf(v: number, domain: {min: number; max: number}, scale: RampScale, diverging: boolean): number {
  const {min, max} = domain;
  if (diverging) {
    const mid = min < 0 && max > 0 ? 0 : (min + max) / 2;
    const half = Math.max(max - mid, mid - min);
    if (half === 0) return 0.5;
    const d = v - mid;
    const s = scale === 'log' ? (Math.sign(d) * Math.log1p(Math.abs(d))) / Math.log1p(half) : d / half;
    return 0.5 + s / 2;
  }
  const span = max - min;
  if (span === 0) return 0.5;
  return scale === 'log' ? Math.log1p(Math.max(0, v - min)) / Math.log1p(span) : (v - min) / span;
}

/** The value at `t` on a ramp over `domain` under `scale`: the inverse of {@link fractionOf}. */
export function valueAtFraction(t: number, domain: {min: number; max: number}, scale: RampScale, diverging: boolean): number {
  const {min, max} = domain;
  const c = Math.min(1, Math.max(0, t));
  if (diverging) {
    const mid = min < 0 && max > 0 ? 0 : (min + max) / 2;
    const half = Math.max(max - mid, mid - min);
    const s = (c - 0.5) * 2;
    return mid + (scale === 'log' ? Math.sign(s) * Math.expm1(Math.abs(s) * Math.log1p(half)) : s * half);
  }
  const span = max - min;
  return min + (scale === 'log' ? Math.expm1(c * Math.log1p(span)) : c * span);
}

/** `#rrggbb` as RGB, or null where it is not six hex digits after a `#`. */
export function rgbOfHex(text: string): Rgb | null {
  return /^#[0-9a-f]{6}$/i.test(text) ? hex(text) : null;
}

/** RGB as `#rrggbb`, lower case. */
export function hexOf(c: Rgb | Rgba): string {
  return `#${[c[0], c[1], c[2]].map((v) => v.toString(16).padStart(2, '0')).join('')}`;
}

/** `c` moved `amount` of the way to white. */
export function lighter(c: Rgb, amount: number): Rgb {
  return [Math.round(c[0] + (255 - c[0]) * amount), Math.round(c[1] + (255 - c[1]) * amount), Math.round(c[2] + (255 - c[2]) * amount)];
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
        const chosen = encoding.chosen.get(code);
        p = packRgba(chosen ? [chosen[0], chosen[1], chosen[2], ALPHA] : colourOfRank(encoding.rankOfCode[code], encoding.palette));
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
  const {ramp: name, scale, reverse, domain} = encoding;
  const diverging = RAMPS[name].diverging;
  const ramp = new Uint32Array(256);
  for (let i = 0; i < 256; i++) ramp[i] = packRgba(colourOfFraction(i / 255, name, reverse));
  const unmapped = packRgba(UNMAPPED);
  for (let i = 0; i < count; i++) {
    const v = values[i];
    if (v === undefined || !Number.isFinite(v)) {
      out32[i] = unmapped;
      continue;
    }
    const t = fractionOf(v, domain, scale, diverging);
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
