import type {SubCell} from '@mosaica/client';

/**
 * Rasterise a tile's masked sub-cell counts into an RGBA image for a `BitmapLayer`. The counts are
 * exact masked aggregates; marks are a sample and cannot show density.
 *
 * Colour is histogram-equalised (datashader's `eq_hist`), since counts span several orders of
 * magnitude, computed only from counts this principal was served.
 *
 * A sub-cell's code is `(parent_prefix << 2·offset) + i`, as in `mosaica-spatial::interleave_bits`:
 * its low `2·offset` bits are the local Morton index, x in the even bits and y in the odd.
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
  // BitmapLayer draws nothing for a bare `{data, width, height}`; it needs an `ImageData`.
  return new ImageData(data, side, side);
}
