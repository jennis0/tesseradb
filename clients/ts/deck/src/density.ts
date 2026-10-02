import {WORLD_SIZE, type AggregateTable} from '@tesseradb/client';
import {RAMPS, rampAt, type Rgb} from './colour.js';

/**
 * Density: counts by cell drawn as a smooth wash, as hexagons, as a grid or as contour lines.
 *
 * Every mode reads counts, not marks. The counts are the aggregate route's cell grouping
 * (`POST /v1/aggregate` with `cells`), exact and taken inside the viewer's visible set under the
 * map's filters; the marks are a per-tile-capped sample and their density on screen says nothing
 * about the corpus. The hexagons, grid and contours aggregate one point per cell at its centre,
 * weighted by its count. {@link DensityCounter} keeps the counts for a camera.
 *
 * The smooth wash is one texture under the marks, with one bin per cell.
 *
 * Density is coloured by count alone: a bin coloured by its majority cluster would take its colour
 * from the sample. Intensity is histogram-equalised over the bins drawn (datashader's `eq_hist`),
 * since the counts span several orders of magnitude; the hexagons and grid use a quantile scale,
 * which is the same idea per bin.
 *
 * {@link filterDensity} smooths the binned image so the cells do not show as hard-edged squares.
 */

/**
 * How density is drawn: `none`; `smooth`, a soft wash; `hex`, hexagonal bins; `grid`, square bins
 * one cell wide; or `contours`, lines of equal density.
 *
 * @category Colour
 */
export type DensityMode = 'none' | 'smooth' | 'hex' | 'grid' | 'contours';

/**
 * The colours density is drawn in: `warm-grey`, one neutral hue whose strength follows the count,
 * or a ramp (`viridis`, `cividis`, `magma`, `greys`) whose dense end contrasts with the ground, so
 * dense ground is dark on a light map and bright on a dark one.
 *
 * @category Colour
 */
export type DensityColours = 'warm-grey' | 'viridis' | 'cividis' | 'magma' | 'greys';

/** The titles of the density colours, as a menu shows them. */
export const DENSITY_COLOUR_TITLES: Readonly<Record<DensityColours, string>> = {
  'warm-grey': 'Warm grey',
  viridis: RAMPS.viridis.title,
  cividis: RAMPS.cividis.title,
  magma: RAMPS.magma.title,
  greys: RAMPS.greys.title
};

/** Warm grey from nearly the ground to the wash's hue, per ground. */
const WARM_GREY: Record<'light' | 'dark', readonly Rgb[]> = {
  light: [
    [233, 231, 227],
    [110, 104, 96]
  ],
  dark: [
    [42, 44, 48],
    [200, 202, 206]
  ]
};

/**
 * The stops of `colours` from the sparse end to the dense end on `scheme`'s ground. A ramp whose
 * light end is its high end ({@link RAMPS}) runs backwards on a light ground.
 */
export function densityStops(colours: DensityColours, scheme: 'light' | 'dark'): readonly Rgb[] {
  if (colours === 'warm-grey') return WARM_GREY[scheme];
  const stops = RAMPS[colours].stops;
  const lightAtHigh = luminance(stops[stops.length - 1]!) > luminance(stops[0]!);
  // Dense is dark on a light ground and light on a dark one.
  const denseLight = scheme === 'dark';
  return lightAtHigh === denseLight ? stops : [...stops].reverse();
}

function luminance(c: Rgb): number {
  return 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
}

/**
 * How the smooth wash is painted: one hue whose alpha is the intensity (warm grey), or a ramp the
 * intensity picks a colour from, fading to nothing at the sparse end.
 */
export type DensityPaint = {kind: 'hue'; rgb: Rgb} | {kind: 'ramp'; stops: readonly Rgb[]};

/** The paint for `colours` on `scheme`'s ground. */
export function densityPaint(colours: DensityColours, scheme: 'light' | 'dark'): DensityPaint {
  return colours === 'warm-grey' ? {kind: 'hue', rgb: WASH_HUE[scheme]} : {kind: 'ramp', stops: densityStops(colours, scheme)};
}

/**
 * One cell of {@link DensityCounts}.
 *
 * @category Colour
 */
export type DensityCell = {
  /** The cell's column at its depth, from 0 at the world's left edge. */
  x: number;
  /** The cell's row at its depth, from 0 at the world's top edge. */
  y: number;
  /** The cell's centre in world units. */
  position: [number, number];
  /** The items counted in the cell. */
  count: number;
};

/**
 * The counts density is drawn from: one {@link DensityCell} per non-empty cell at `depth`, where a
 * cell at depth `d` is one of `2^d` by `2^d` over the view, as a tile is at depths up to 16.
 *
 * @category Colour
 */
export type DensityCounts = {
  /** The depth the cells are at. */
  depth: number;
  /** The cells with a count, in any order. */
  cells: readonly DensityCell[];
};

/**
 * The cells of an aggregate table counted by `cells` at `depth`: one per row with a count, at the
 * cell's centre in world units. The table's `cell` column is the first `2·depth` bits of the Morton
 * position, which is a tile's prefix at depths up to 16.
 */
export function densityCellsOf(table: Pick<AggregateTable, 'rows'>, depth: number): DensityCell[] {
  const rows = table.rows;
  const cell = rows.getChild('cell');
  const count = rows.getChild('count');
  if (!cell || !count) return [];
  const span = WORLD_SIZE / 2 ** depth;
  const cells: DensityCell[] = [];
  for (let i = 0; i < rows.numRows; i++) {
    const n = Number(count.get(i) as bigint | number);
    if (!(n > 0)) continue;
    const {x, y} = cellXY(BigInt(cell.get(i) as bigint | number));
    cells.push({x, y, position: [(x + 0.5) * span, (y + 0.5) * span], count: n});
  }
  return cells;
}

/** The even bits of a 32-bit number, packed into its low 16. */
function evenBits(v: number): number {
  v &= 0x55555555;
  v = (v | (v >>> 1)) & 0x33333333;
  v = (v | (v >>> 2)) & 0x0f0f0f0f;
  v = (v | (v >>> 4)) & 0x00ff00ff;
  return (v | (v >>> 8)) & 0x0000ffff;
}

/**
 * A cell's column and row from its Morton prefix: x on the even bits and y on the odd, as
 * `mortonOfTile` writes them. Split into two 32-bit halves, since a row of a table holds thousands
 * of cells and bigint arithmetic per bit would cost more than the rest of the read.
 */
function cellXY(cell: bigint): {x: number; y: number} {
  const lo = Number(cell & 0xffffffffn);
  const hi = Number(cell >> 32n);
  return {x: evenBits(lo) + evenBits(hi) * 65536, y: evenBits(lo >>> 1) + evenBits(hi >>> 1) * 65536};
}

/**
 * The counts contour lines are drawn at: up to four distinct counts at the 25th, 50th, 75th and
 * 90th percentiles of the cells' counts, ascending. Empty where no cell has a count.
 */
export function contourThresholds(cells: readonly DensityCell[]): number[] {
  const counts = cells.map((c) => c.count).sort((a, b) => a - b);
  if (counts.length === 0) return [];
  const at = [0.25, 0.5, 0.75, 0.9].map((q) => counts[Math.min(counts.length - 1, Math.floor(q * counts.length))]!);
  return [...new Set(at)];
}

export type DensityImage = {
  width: number;
  height: number;
  /** RGBA, row-major from the lowest cell row. */
  data: Uint8ClampedArray<ArrayBuffer>;
  /** World-space `[x0, y0, x1, y1]` the image covers, half-open on the far edges. */
  bounds: [number, number, number, number];
  /** How many bins carry a count. */
  filled: number;
};

/**
 * The wash's hue per ground, RGB: a warm grey on a light ground and a light grey on a dark one,
 * neutral so it does not read as a data colour.
 */
export const WASH_HUE: Record<'light' | 'dark', Rgb> = {light: [110, 104, 96], dark: [200, 202, 206]};

/** The most alpha {@link binDensity} writes, so an intensity is `alpha / BIN_ALPHA_MAX`. */
const BIN_ALPHA_MAX = 178;

/**
 * Bin the cells into one texel each, over the rectangle they span. A texel's alpha is its intensity
 * and its colour is unset; {@link filterDensity} paints it. `null` where no cell has a count.
 */
export function binDensity(counts: DensityCounts): DensityImage | null {
  const {depth} = counts;
  const cells = counts.cells.filter((c) => c.count > 0);
  if (cells.length === 0) return null;
  let x0 = Infinity;
  let y0 = Infinity;
  let x1 = -Infinity;
  let y1 = -Infinity;
  for (const {x, y} of cells) {
    if (x < x0) x0 = x;
    if (y < y0) y0 = y;
    if (x > x1) x1 = x;
    if (y > y1) y1 = y;
  }

  const width = x1 - x0 + 1;
  const height = y1 - y0 + 1;
  const data = new Uint8ClampedArray(width * height * 4);

  // Rank the distinct counts: a bin's intensity is its rank among the counts drawn.
  const distinct = [...new Set(cells.map((c) => c.count))].sort((a, b) => a - b);
  const rank = new Map<number, number>();
  distinct.forEach((count, i) => rank.set(count, distinct.length === 1 ? 1 : i / (distinct.length - 1)));

  for (const cell of cells) {
    const t = rank.get(cell.count) ?? 0;
    const i = ((cell.y - y0) * width + (cell.x - x0)) * 4;
    // Faint at the low end and never opaque, so the points stay legible over the densest bin.
    data[i + 3] = Math.round(28 + t * (BIN_ALPHA_MAX - 28));
  }

  const span = WORLD_SIZE / 2 ** depth;
  return {
    width,
    height,
    data,
    bounds: [x0 * span, y0 * span, (x1 + 1) * span, (y1 + 1) * span],
    filled: cells.length
  };
}

/** Texels per cell in the filtered image. */
export const DENSITY_SUPERSAMPLE = 4;

/**
 * The binned image as a soft field: one padding cell around it so a halo can extend past an
 * edge cell, {@link DENSITY_SUPERSAMPLE} texels per cell, intensity bilinear between cell
 * centres and box-blurred by one texel. Under a hue, alpha is proportional to intensity. Under a
 * ramp, the intensity picks the colour and the alpha rises from nothing to opaque over the lower
 * part of the range.
 */
export function filterDensity(image: DensityImage, depth: number, paint: DensityPaint = {kind: 'hue', rgb: WASH_HUE.light}): DensityImage {
  const S = DENSITY_SUPERSAMPLE;
  const W = image.width + 2;
  const H = image.height + 2;
  // The coarse intensity field, from the binned alpha, with a one-cell border of nothing.
  const field = new Float32Array(W * H);
  let peak = 0;
  for (let y = 0; y < image.height; y++) {
    for (let x = 0; x < image.width; x++) {
      const a = image.data[(y * image.width + x) * 4 + 3]!;
      field[(y + 1) * W + (x + 1)] = a;
      if (a > peak) peak = a;
    }
  }
  const width = W * S;
  const height = H * S;
  const fine = new Float32Array(width * height);
  for (let py = 0; py < height; py++) {
    const cy = (py + 0.5) / S - 0.5;
    const y0 = Math.max(0, Math.floor(cy));
    const y1 = Math.min(H - 1, y0 + 1);
    const ty = Math.min(1, Math.max(0, cy - y0));
    for (let px = 0; px < width; px++) {
      const cx = (px + 0.5) / S - 0.5;
      const x0 = Math.max(0, Math.floor(cx));
      const x1 = Math.min(W - 1, x0 + 1);
      const tx = Math.min(1, Math.max(0, cx - x0));
      const top = field[y0 * W + x0]! * (1 - tx) + field[y0 * W + x1]! * tx;
      const bottom = field[y1 * W + x0]! * (1 - tx) + field[y1 * W + x1]! * tx;
      fine[py * width + px] = top * (1 - ty) + bottom * ty;
    }
  }
  // One-texel box blur, separable: the bilinear creases go, the halo stays.
  const blurred = new Float32Array(width * height);
  for (let py = 0; py < height; py++) {
    for (let px = 0; px < width; px++) {
      let sum = 0;
      let n = 0;
      for (let dx = -1; dx <= 1; dx++) {
        const x = px + dx;
        if (x < 0 || x >= width) continue;
        sum += fine[py * width + x]!;
        n++;
      }
      blurred[py * width + px] = sum / n;
    }
  }
  const data = new Uint8ClampedArray(width * height * 4);
  let filled = 0;
  for (let py = 0; py < height; py++) {
    for (let px = 0; px < width; px++) {
      let sum = 0;
      let n = 0;
      for (let dy = -1; dy <= 1; dy++) {
        const y = py + dy;
        if (y < 0 || y >= height) continue;
        sum += blurred[y * width + px]!;
        n++;
      }
      const a = Math.round(sum / n);
      const i = (py * width + px) * 4;
      if (paint.kind === 'hue') {
        data[i] = paint.rgb[0];
        data[i + 1] = paint.rgb[1];
        data[i + 2] = paint.rgb[2];
        data[i + 3] = a;
      } else {
        const t = Math.min(1, a / BIN_ALPHA_MAX);
        const c = rampAt(paint.stops, t);
        data[i] = c[0];
        data[i + 1] = c[1];
        data[i + 2] = c[2];
        data[i + 3] = Math.round(255 * Math.min(1, t * 1.6));
      }
      if (a > 0) filled++;
    }
  }
  const span = WORLD_SIZE / 2 ** depth;
  const [bx0, by0, bx1, by1] = image.bounds;
  return {width, height, data, bounds: [bx0 - span, by0 - span, bx1 + span, by1 + span], filled};
}
