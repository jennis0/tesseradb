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
 */
export function binDensity(
  tiles: readonly ComposedTile[],
  depth: number,
  channel: 'visible' | 'matched' = 'matched',
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
