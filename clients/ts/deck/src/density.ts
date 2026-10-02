import {WORLD_SIZE, type AggregateTable, type TilesProjection} from '@tesseradb/client';
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
 * from the sample. Every mode places a count on one {@link DensityScale} from 0 to the largest
 * count among the cells drawn ({@link densityPosition}), and picks its colour, alpha or contour
 * level from that position. The cells drawn are those of the area counted, which runs past the
 * viewport ({@link DensityCounter}), so the top of the scale holds while the camera pans within it.
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

/**
 * How a count is placed between no items and the largest count drawn: `linear`, in proportion to
 * the count, or `log`, in proportion to the logarithm of one more than the count, which spreads
 * counts that span several orders of magnitude.
 *
 * @category Colour
 */
export type DensityScale = 'linear' | 'log';

/** The density scale used where none is set. */
export const DEFAULT_DENSITY_SCALE: DensityScale = 'log';

/**
 * Where `count` sits on `scale`, from 0 at no items to 1 at `max`: `count / max` under `linear` and
 * `log1p(count) / log1p(max)` under `log`. A count is clamped to `[0, max]`, and every count is at
 * 0 where `max` is 0 or less.
 */
export function densityPosition(count: number, max: number, scale: DensityScale): number {
  if (!(max > 0)) return 0;
  const c = Math.min(max, Math.max(0, count));
  return scale === 'log' ? Math.log1p(c) / Math.log1p(max) : c / max;
}

/** The count at `position` on `scale` up to `max`: the inverse of {@link densityPosition}. */
export function densityCountAt(position: number, max: number, scale: DensityScale): number {
  if (!(max > 0)) return 0;
  const p = Math.min(1, Math.max(0, position));
  return scale === 'log' ? Math.expm1(p * Math.log1p(max)) : p * max;
}

/** The largest count among `cells`, 0 where there are none. */
export function maxCount(cells: readonly DensityCell[]): number {
  let max = 0;
  for (const c of cells) if (c.count > max) max = c.count;
  return max;
}

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
  const codes = cell.toArray() as ArrayLike<bigint>;
  const tallies = count.toArray() as ArrayLike<bigint | number>;
  // A 64-bit column as 32-bit halves, low half first, so no cell needs bigint arithmetic.
  const halves = codes instanceof BigUint64Array || codes instanceof BigInt64Array ? new Uint32Array(codes.buffer, codes.byteOffset, codes.length * 2) : null;
  const cells: DensityCell[] = [];
  for (let i = 0; i < rows.numRows; i++) {
    const n = Number(tallies[i]);
    if (!(n > 0)) continue;
    let lo: number;
    let hi: number;
    if (halves) {
      lo = halves[2 * i]!;
      hi = halves[2 * i + 1]!;
    } else {
      const code = BigInt(codes[i]!);
      lo = Number(code & 0xffffffffn);
      hi = Number(code >> 32n);
    }
    cells.push(cellAt(lo, hi, span, n));
  }
  return cells;
}

/**
 * The cells of a frame's tiles at `depth`: one per tile carrying the server's counts, its count the
 * highlighted items, which are the matched items where no highlight is set, as the aggregate route
 * counts them. A tile's prefix is the Morton cell the aggregate route names at the same depth.
 */
export function densityOfTiles(tiles: TilesProjection['tiles'], depth: number): DensityCounts {
  const span = WORLD_SIZE / 2 ** depth;
  const cells: DensityCell[] = [];
  for (const tile of tiles) {
    if (tile.depth !== depth || tile.counts === null) continue;
    const n = Number(tile.counts.highlighted);
    if (!(n > 0)) continue;
    cells.push(cellAt(Number(tile.prefix & 0xffffffffn), Number(tile.prefix >> 32n), span, n));
  }
  return {depth, cells};
}

/** The cell whose Morton code has these 32-bit halves, low half first, holding `count` items. */
function cellAt(lo: number, hi: number, span: number, count: number): DensityCell {
  const x = evenBits(lo) + evenBits(hi) * 65536;
  const y = evenBits(lo >>> 1) + evenBits(hi >>> 1) * 65536;
  return {x, y, position: [(x + 0.5) * span, (y + 0.5) * span], count};
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
 * `counts` merged to coarser cells, a depth at a time, until at most `budget` cells remain: each
 * coarser cell counts the items of the finer cells it holds. The hexagons and contours aggregate
 * on the CPU in time proportional to the cells, so their input is held to a budget.
 */
export function coarsened(counts: DensityCounts, budget: number): DensityCounts {
  let {depth, cells} = counts;
  while (cells.length > budget && depth > 0) {
    depth -= 1;
    const span = WORLD_SIZE / 2 ** depth;
    const merged = new Map<number, DensityCell>();
    for (const c of cells) {
      const x = Math.floor(c.x / 2);
      const y = Math.floor(c.y / 2);
      const key = x * 2 ** 32 + y;
      const held = merged.get(key);
      if (held) held.count += c.count;
      else merged.set(key, {x, y, position: [(x + 0.5) * span, (y + 0.5) * span], count: c.count});
    }
    cells = [...merged.values()];
  }
  return cells === counts.cells ? counts : {depth, cells};
}

/**
 * The most cells the hexagons and the contours aggregate. deck aggregates them in the render, so
 * finer counts are merged to a coarser depth first ({@link coarsened}). The figures hold one answer
 * under 50 ms of main thread at 1920 × 1080 in headless Chromium's software GL; the contours'
 * cost per cell is the higher.
 */
export const AGGREGATED_CELLS = {hex: 10_000, contours: 2_000} as const;

/** The merged cells each counts object has been drawn from, so a repaint hands deck the same data. */
const merged = new WeakMap<DensityCounts, Partial<Record<'hex' | 'contours', DensityCounts>>>();

/**
 * The cells `mode` draws from `counts`: merged to {@link AGGREGATED_CELLS} for the hexagons and
 * contours, and as given for the other modes. Their largest count is the top of the scale.
 */
export function drawnCells(counts: DensityCounts, mode: DensityMode): DensityCounts {
  if (mode !== 'hex' && mode !== 'contours') return counts;
  let held = merged.get(counts);
  if (!held) merged.set(counts, (held = {}));
  return (held[mode] ??= coarsened(counts, AGGREGATED_CELLS[mode]));
}

/** The positions on the scale contour lines are drawn at. */
const CONTOUR_POSITIONS = [0.2, 0.4, 0.6, 0.8];

/**
 * The counts contour lines are drawn at, ascending: evenly spaced positions on `scale` between no
 * items and the largest count among `cells`, up to four. Of the levels between the same two whole
 * counts only the lowest is kept, since the cells above each are the same. A level under 1 is left
 * out: it would ring every cell holding one item, which at a deep zoom is a ring round each point.
 * Empty where no cell has more than one item.
 */
export function contourThresholds(cells: readonly DensityCell[], scale: DensityScale): number[] {
  const max = maxCount(cells);
  if (max <= 1) return [];
  const levels: number[] = [];
  for (const p of CONTOUR_POSITIONS) {
    const level = densityCountAt(p, max, scale);
    if (level < 1) continue;
    const last = levels[levels.length - 1];
    if (last === undefined || Math.ceil(level) !== Math.ceil(last)) levels.push(level);
  }
  return levels;
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
/** The alpha {@link binDensity} writes for a count at the bottom of the scale. */
const BIN_ALPHA_MIN = 28;

/** The columns and rows `cells` span, inclusive. */
function extentOf(cells: readonly DensityCell[]): {x0: number; y0: number; x1: number; y1: number} {
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
  return {x0, y0, x1, y1};
}

/**
 * Bin the cells into one texel each, over the rectangle they span. A texel's alpha is its intensity,
 * its count's position on `scale`, and its colour is unset; {@link filterDensity} paints it. `null`
 * where no cell has a count.
 */
export function binDensity(counts: DensityCounts, scale: DensityScale = DEFAULT_DENSITY_SCALE): DensityImage | null {
  const {depth} = counts;
  const cells = counts.cells.filter((c) => c.count > 0);
  if (cells.length === 0) return null;
  const {x0, y0, x1, y1} = extentOf(cells);

  const width = x1 - x0 + 1;
  const height = y1 - y0 + 1;
  const data = new Uint8ClampedArray(width * height * 4);

  const max = maxCount(cells);
  for (const cell of cells) {
    const t = densityPosition(cell.count, max, scale);
    const i = ((cell.y - y0) * width + (cell.x - x0)) * 4;
    // Faint at the low end and never opaque, so the points stay legible over the densest bin.
    data[i + 3] = Math.round(BIN_ALPHA_MIN + t * (BIN_ALPHA_MAX - BIN_ALPHA_MIN));
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

/** Texels per cell in the filtered image, where the image fits {@link WASH_TEXELS} at that many. */
export const DENSITY_SUPERSAMPLE = 4;

/**
 * The most texels the filtered image holds. Each filtering pass visits every texel, so a viewport
 * of fine cells takes fewer texels per cell, down to one, which the texture's linear filtering
 * still smooths.
 */
const WASH_TEXELS = 1 << 20;

/** Texels per cell for an image of `cells` cells: {@link DENSITY_SUPERSAMPLE} where it fits {@link WASH_TEXELS}. */
function texelsPerCell(cells: number, most: number): number {
  return Math.max(1, Math.min(most, Math.floor(Math.sqrt(WASH_TEXELS / cells))));
}

/**
 * The binned image as a soft field: one padding cell around it so a halo can extend past an
 * edge cell, {@link DENSITY_SUPERSAMPLE} texels per cell, intensity bilinear between cell
 * centres and box-blurred by one texel. Under a hue, alpha is proportional to intensity. Under a
 * ramp, the alpha rises from nothing to opaque over the lower part of the intensity, and the
 * colour is the position on the scale the intensity stands for, the binned alpha's floor taken
 * off. The blur averages a cell with its neighbours, so a lone dense cell among sparse ones is
 * drawn below its own position.
 */
export function filterDensity(image: DensityImage, depth: number, paint: DensityPaint = {kind: 'hue', rgb: WASH_HUE.light}): DensityImage {
  const W = image.width + 2;
  const H = image.height + 2;
  const S = texelsPerCell(W * H, DENSITY_SUPERSAMPLE);
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
        const c = rampAt(paint.stops, Math.min(1, Math.max(0, (a - BIN_ALPHA_MIN) / (BIN_ALPHA_MAX - BIN_ALPHA_MIN))));
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

/**
 * `steps` colours of `stops`, sparse to dense, each taken at the middle of its equal share of the
 * scale: a position in `[i / steps, (i + 1) / steps)` draws in colour `i`.
 */
export function densitySteps(stops: readonly Rgb[], steps: number): Rgb[] {
  return Array.from({length: steps}, (_, i) => rampAt(stops, (i + 0.5) / steps));
}

/** The most texels per cell the grid's image takes, enough to leave a hairline between cells. */
const GRID_SUPERSAMPLE = 8;

/**
 * The grid as one image: each cell a square in the colour of its count's position on `scale`, in
 * `steps` steps of `stops`, drawn with nearest filtering so the squares keep their
 * edges. Where the image has room for four or more texels a cell, the last row and column of each
 * cell's texels are left clear, so neighbouring cells show a hairline gap. `null` where no cell has a
 * count.
 */
export function gridImage(counts: DensityCounts, stops: readonly Rgb[], steps: number, scale: DensityScale = DEFAULT_DENSITY_SCALE): DensityImage | null {
  const cells = counts.cells.filter((c) => c.count > 0);
  if (cells.length === 0) return null;
  const {x0, y0, x1, y1} = extentOf(cells);
  const columns = x1 - x0 + 1;
  const rows = y1 - y0 + 1;
  const max = maxCount(cells);
  const colours = densitySteps(stops, steps);
  const S = texelsPerCell(columns * rows, GRID_SUPERSAMPLE);
  const fill = S >= 4 ? S - 1 : S;
  const width = columns * S;
  const data = new Uint8ClampedArray(width * rows * S * 4);
  for (const c of cells) {
    const colour = colours[Math.min(steps - 1, Math.floor(densityPosition(c.count, max, scale) * steps))]!;
    for (let dy = 0; dy < fill; dy++) {
      let i = (((c.y - y0) * S + dy) * width + (c.x - x0) * S) * 4;
      for (let dx = 0; dx < fill; dx++, i += 4) {
        data[i] = colour[0];
        data[i + 1] = colour[1];
        data[i + 2] = colour[2];
        data[i + 3] = 255;
      }
    }
  }
  const span = WORLD_SIZE / 2 ** counts.depth;
  return {width, height: rows * S, data, bounds: [x0 * span, y0 * span, (x1 + 1) * span, (y1 + 1) * span], filled: cells.length};
}
