import {describe, expect, it} from 'vitest';
import {
  CELL_GRID,
  WORLD_SIZE,
  dataToWorldXY,
  positionsToWorld,
  tileToCellBox,
  tileToDataBbox,
  tileToRequestBbox
} from '../src/coords.js';

const square = {xMin: 0, xMax: 65536, yMin: 0, yMax: 65536};
const skewed = {xMin: -10, xMax: 10, yMin: 0, yMax: 1000};

describe('coords', () => {
  it('maps the z=0 tile onto the whole data extent', () => {
    expect(tileToDataBbox({x: 0, y: 0, z: 0}, skewed)).toEqual([-10, 0, 10, 1000]);
  });

  it('partitions the extent across a depth’s tiles with no gap or overlap', () => {
    const z = 3;
    const n = 2 ** z;
    for (let x = 0; x < n - 1; x++) {
      const left = tileToDataBbox({x, y: 0, z}, skewed);
      const right = tileToDataBbox({x: x + 1, y: 0, z}, skewed);
      expect(left[2]).toBeCloseTo(right[0], 9);
    }
  });

  it('handles a non-square extent per axis', () => {
    const box = tileToDataBbox({x: 0, y: 0, z: 1}, skewed);
    expect(box[2] - box[0]).toBeCloseTo(10, 9); // half of 20
    expect(box[3] - box[1]).toBeCloseTo(500, 9); // half of 1000
  });

  it('sends data-space corners to world-space corners', () => {
    expect(dataToWorldXY(square.xMin, square.yMin, square)).toEqual([0, 0]);
    expect(dataToWorldXY(square.xMax, square.yMax, square)).toEqual([WORLD_SIZE, WORLD_SIZE]);
    // The skewed extent lands on the same square world, which is what makes a tile square on
    // screen despite a rectangular data extent.
    expect(dataToWorldXY(skewed.xMax, skewed.yMax, skewed)).toEqual([WORLD_SIZE, WORLD_SIZE]);
  });

  it('keeps the cell box and the data bbox describing the same block', () => {
    const cells = tileToCellBox({x: 5, y: 2, z: 4});
    const data = tileToDataBbox({x: 5, y: 2, z: 4}, square);
    expect(data[0]).toBeCloseTo((cells.cx0 / CELL_GRID) * 65536, 6);
    expect(data[1]).toBeCloseTo((cells.cy0 / CELL_GRID) * 65536, 6);
  });

  it('requests a bbox naming exactly one tile, at every depth', () => {
    // `tessera-spatial`'s tile_corners quantises both corners to cells and iterates lo..=hi
    // INCLUSIVELY, so a bbox closed on the tile boundary selects the neighbours too. This
    // reproduces that arithmetic and asserts the request bbox does not trip it.
    const cellOf = (v: number, lo: number, hi: number) =>
      Math.min(CELL_GRID - 1, Math.max(0, Math.floor(((v - lo) / (hi - lo)) * CELL_GRID)));

    for (const q of [square, skewed]) {
      for (const z of [0, 1, 4, 8, 16]) {
        const index = {x: z === 0 ? 0 : 1, y: z === 0 ? 0 : 1, z};
        const [x0, y0, x1, y1] = tileToRequestBbox(index, q);
        const shift = 16 - z;
        const tx0 = cellOf(x0, q.xMin, q.xMax) >> shift;
        const tx1 = cellOf(x1, q.xMin, q.xMax) >> shift;
        const ty0 = cellOf(y0, q.yMin, q.yMax) >> shift;
        const ty1 = cellOf(y1, q.yMin, q.yMax) >> shift;
        const tileCount = (tx1 - tx0 + 1) * (ty1 - ty0 + 1);
        expect(tileCount, `depth ${z} must name one tile, named ${tileCount}`).toBe(1);
        expect(tx0).toBe(index.x);
        expect(ty0).toBe(index.y);
      }
    }
  });

  it('keeps the request bbox strictly inside the tile’s exact bbox', () => {
    const exact = tileToDataBbox({x: 2, y: 3, z: 4}, square);
    const request = tileToRequestBbox({x: 2, y: 3, z: 4}, square);
    expect(request[0]).toBeGreaterThan(exact[0]);
    expect(request[1]).toBeGreaterThan(exact[1]);
    expect(request[2]).toBeLessThan(exact[2]);
    expect(request[3]).toBeLessThan(exact[3]);
  });

  it('converts an interleaved buffer in place, pair by pair', () => {
    const positions = new Float32Array([0, 0, 65536, 65536, 32768, 16384]);
    const out = positionsToWorld(positions, square);
    expect(out).toBe(positions); // in place: no allocation per tile
    expect([...out]).toEqual([0, 0, WORLD_SIZE, WORLD_SIZE, WORLD_SIZE / 2, WORLD_SIZE / 4]);
  });
});
