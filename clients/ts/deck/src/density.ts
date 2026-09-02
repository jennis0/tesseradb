import {WORLD_SIZE, tileXY, type ComposedTile} from '@tesseradb/client';

/**
 * The density wash: the number channel as one texture (design client-components §5.10).
 *
 * **It reads counts, never marks.** A tile's `visible`/`matched` are exact masked aggregates from
 * the server; the marks are a per-tile-capped sample whose on-screen density says nothing about
 * the corpus. So the wash is binned from the `tiles` projection — one bin per exact tile at the
 * drawn depth — and a tile that is not exact (drawn from an ancestor or from held descendants)
 * contributes **nothing**: a superset read as density overstates (`delta-serving.md` §7), and the
 * honest wash has a hole there rather than a guess.
 *
 * **Single hue.** Colouring a bin by its majority cluster would be a colour chosen from a sample,
 * which is the guess exact-only refuses (decision 0099). Intensity is histogram-equalised over the
 * bins on screen — datashader's `eq_hist`, the field's answer to counts spanning several decades —
 * which is a mapping computed from counts this principal was served, not a masked quantity.
 *
 * Rebuilt at the settle, O(tiles); a full viewport at 10⁹ scale is ~10⁵ tiles, a millisecond or
 * two, and it is drawn as one `BitmapLayer` under the points.
 *
 * **The tile grid is never shown** (decision 0097). A bin per tile drawn nearest-neighbour is the
 * storage grid drawn — at a coarse depth under a sparse principal it read as hard-edged squares.
 * So the binned image is {@link filterDensity}'d: supersampled with the intensity interpolated
 * between tile centres, softened at the scale of one drawn cell, its alpha fading with density
 * so an isolated cell reads as a halo and never as a block, and sampled linearly on the GPU. No
 * texel column steps from nothing to full across one texel.
 */

export type DensityImage = {
  width: number;
  height: number;
  /** RGBA, row-major from the lowest tile row — `bounds` says where it sits. */
  data: Uint8ClampedArray<ArrayBuffer>;
  /** World-space `[x0, y0, x1, y1]` the image covers, half-open on the far edges. */
  bounds: [number, number, number, number];
  /** How many bins carry a count — zero means nothing exact is on screen and nothing is drawn. */
  filled: number;
};

/** The wash's one hue, RGB — a cool neutral that sits under every palette colour. */
export const WASH_HUE: [number, number, number] = [96, 132, 190];

/**
 * Bin the exact tiles at `depth` into one texel each, over the rectangle those tiles span.
 *
 * `channel` chooses which count is washed; `matched` is the default because it is the filtered
 * answer — the wash narrows with a filter as the counts do, while `visible` would hold.
 *
 * **`highlighted` is the channel a highlight washes** (`highlight-and-hierarchy.md` §5.3), and it
 * is the only picture a spread artifact has: a highlight over 27 million articles draws 66,000 of
 * them, so the marks say almost nothing about where the rest are and the count says it exactly.
 * The caller labels the wash as the count it is; this function only bins what it is handed.
 */
export function binDensity(
  tiles: readonly ComposedTile[],
  depth: number,
  channel: 'visible' | 'matched' | 'highlighted' = 'matched',
  hue: [number, number, number] = WASH_HUE
): DensityImage | null {
  const cells: {x: number; y: number; count: number}[] = [];
  let x0 = Infinity;
  let y0 = Infinity;
  let x1 = -Infinity;
  let y1 = -Infinity;
  for (const tile of tiles) {
    if (!tile.exact || !tile.counts || tile.depth !== depth) continue;
    const {x, y} = tileXY(tile.prefix, depth);
    cells.push({x, y, count: Number(tile.counts[channel])});
    if (x < x0) x0 = x;
    if (y < y0) y0 = y;
    if (x > x1) x1 = x;
    if (y > y1) y1 = y;
  }
  if (cells.length === 0) return null;

  const width = x1 - x0 + 1;
  const height = y1 - y0 + 1;
  const data = new Uint8ClampedArray(width * height * 4);

  // Rank the distinct non-zero counts: a bin's intensity is its rank among the counts on screen.
  const distinct = [...new Set(cells.map((c) => c.count).filter((n) => n > 0))].sort((a, b) => a - b);
  const rank = new Map<number, number>();
  distinct.forEach((count, i) => rank.set(count, distinct.length === 1 ? 1 : i / (distinct.length - 1)));

  let filled = 0;
  for (const cell of cells) {
    if (cell.count === 0) continue;
    const t = rank.get(cell.count) ?? 0;
    const i = ((cell.y - y0) * width + (cell.x - x0)) * 4;
    data[i] = hue[0];
    data[i + 1] = hue[1];
    data[i + 2] = hue[2];
    // Faint at the low end so a sparse principal's ground still reads as ground, never opaque so
    // the points stay legible over the densest bin.
    data[i + 3] = Math.round(28 + t * 150);
    filled++;
  }

  const span = WORLD_SIZE / 2 ** depth;
  return {
    width,
    height,
    data,
    bounds: [x0 * span, y0 * span, (x1 + 1) * span, (y1 + 1) * span],
    filled
  };
}

/** Texels per tile in the filtered image — four gives a ramp of four steps across a cell edge. */
export const DENSITY_SUPERSAMPLE = 4;

/**
 * The binned image as a soft field: one padding cell around it so a halo can extend past an
 * edge tile, {@link DENSITY_SUPERSAMPLE} texels per cell, intensity bilinear between cell
 * centres and box-blurred by one texel, alpha proportional to intensity.
 */
export function filterDensity(image: DensityImage, depth: number, hue: [number, number, number] = WASH_HUE): DensityImage {
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
      data[i] = hue[0];
      data[i + 1] = hue[1];
      data[i + 2] = hue[2];
      data[i + 3] = a;
      if (a > 0) filled++;
    }
  }
  const span = WORLD_SIZE / 2 ** depth;
  const [bx0, by0, bx1, by1] = image.bounds;
  return {width, height, data, bounds: [bx0 - span, by0 - span, bx1 + span, by1 + span], filled};
}
