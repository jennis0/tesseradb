import {describe, expect, it} from 'vitest';
import {
  CELL_GRID,
  WORLD_SIZE,
  dataToWorldXY,
  positionsToWorld,
  tileToCellBox,
  tileToDataBbox
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

  it('converts an interleaved buffer in place, pair by pair', () => {
    const positions = new Float32Array([0, 0, 65536, 65536, 32768, 16384]);
    const out = positionsToWorld(positions, square);
    expect(out).toBe(positions); // in place: no allocation per tile
    expect([...out]).toEqual([0, 0, WORLD_SIZE, WORLD_SIZE, WORLD_SIZE / 2, WORLD_SIZE / 4]);
  });
});
