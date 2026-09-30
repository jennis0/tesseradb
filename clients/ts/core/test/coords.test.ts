import {describe, expect, it} from 'vitest';
import {
  CELL_GRID,
  WORLD_SIZE,
  dataToWorldXY,
  tileToCellBox,
  tileToRequestBbox
} from '../src/coords.js';

const square = {xMin: 0, xMax: 65536, yMin: 0, yMax: 65536};
const skewed = {xMin: -10, xMax: 10, yMin: 0, yMax: 1000};

describe('coords', () => {
  it('sends data-space corners to world-space corners', () => {
    expect(dataToWorldXY(square.xMin, square.yMin, square)).toEqual([0, 0]);
    expect(dataToWorldXY(square.xMax, square.yMax, square)).toEqual([WORLD_SIZE, WORLD_SIZE]);
    // The skewed extent lands on the same square world, so a tile is square on screen.
    expect(dataToWorldXY(skewed.xMax, skewed.yMax, skewed)).toEqual([WORLD_SIZE, WORLD_SIZE]);
  });

  it('requests a bbox naming exactly one tile, at every depth', () => {
    // The server quantises both corners to cells and includes both, so a bbox closed on the tile
    // boundary selects the neighbours too. This reproduces that arithmetic.
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
    // Over `square`, data coordinates are cell coordinates.
    const cells = tileToCellBox({x: 2, y: 3, z: 4});
    const exact = [cells.cx0, cells.cy0, cells.cx1, cells.cy1] as const;
    const request = tileToRequestBbox({x: 2, y: 3, z: 4}, square);
    expect(request[0]).toBeGreaterThan(exact[0]);
    expect(request[1]).toBeGreaterThan(exact[1]);
    expect(request[2]).toBeLessThan(exact[2]);
    expect(request[3]).toBeLessThan(exact[3]);
  });
});
