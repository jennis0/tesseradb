import type {SubCell} from '@tesseradb/client';

/**
 * Rasterise a tile's masked sub-cell counts into an RGBA image for a `BitmapLayer`.
 *
 * The counts are **exact masked aggregates** from the server — the density signal that mark count
 * cannot be, because marks are a sample. Client-interaction §9 makes the underlay the expected
 * default for that reason rather than an optional garnish.
 *
 * Colour is **histogram equalisation** (datashader's `eq_hist`), which §9 recommends over a fixed
 * log transfer: it is the field's hard-won answer to counts spanning several decades. It is safely
 * client-side because it is computed only from counts this principal was served — a mapping, not a
 * masked quantity (§9's decision rule).
 *
 * **Cell addressing, checked against `tessera-spatial::interleave_bits`**: a sub-cell's code is
 * `(parent_prefix << 2·offset) + i`, so its low `2·offset` bits are the local Morton index, with x
 * in the even bit positions and y in the odd ones.
 */
export function subCellsToImage(subCells: SubCell[], offset: number): ImageData {
  const side = 2 ** offset;
  const data = new Uint8ClampedArray(side * side * 4);
  if (subCells.length === 0) return new ImageData(data, side, side);

  // Rank the distinct counts: a cell's colour is its rank among the served counts, not its value.
  const distinct = [...new Set(subCells.map((c) => c.count))].sort((a, b) =>
    a < b ? -1 : a > b ? 1 : 0
  );
  const rank = new Map<bigint, number>();
  distinct.forEach((count, i) => rank.set(count, distinct.length === 1 ? 1 : i / (distinct.length - 1)));

  const mask = (1n << BigInt(2 * offset)) - 1n;
  for (const {cell, count} of subCells) {
    const local = cell & mask;
    let x = 0;
    let y = 0;
    for (let bit = 0; bit < offset; bit++) {
      x |= Number((local >> BigInt(2 * bit)) & 1n) << bit;
      y |= Number((local >> BigInt(2 * bit + 1)) & 1n) << bit;
    }
    const t = rank.get(count) ?? 0;
    const index = (y * side + x) * 4;
    data[index] = 30 + t * 210;
    data[index + 1] = 45 + t * 120;
    data[index + 2] = 95 + t * 70;
    data[index + 3] = 205;
  }
  // An `ImageData`, not a bare `{data, width, height}`: BitmapLayer silently draws nothing for the
  // latter, which is the worst possible failure for a density layer — it reads as "no density
  // here" rather than "this layer did not render".
  return new ImageData(data, side, side);
}
