import {describe, expect, it} from 'vitest';
import {MAX_DEPTH, WORLD_SIZE, tileXY} from '../src/coords.js';
import {REGION_TILE_BOUND, cellExceedsPixel, insideBox, rasteriseBox} from '../src/region.js';

/**
 * A box becomes one `tiles`-form request under a bound, and the count under it is typed exact
 * only where the cell is no wider than a pixel (design §5.11, decision 0097).
 */

describe('rasteriseBox — Morton prefixes at the deepest depth under the bound', () => {
  it('names every tile the box intersects and nothing else', () => {
    // A quarter of the world: at depth 6 that is 32 × 32 = 1,024 tiles; depth 7 would be 4,096
    // (exactly the bound, allowed); depth 8 overflows. So the request is at depth 7.
    const r = rasteriseBox([0, 0, WORLD_SIZE / 2 - 1e-6, WORLD_SIZE / 2 - 1e-6]);
    expect(r.depth).toBe(7);
    expect(r.tiles.length).toBe(4096);
    const xs = new Set<number>();
    for (const t of r.tiles) {
      const {x, y} = tileXY(t, 7);
      expect(x).toBeLessThan(64);
      expect(y).toBeLessThan(64);
      xs.add(x);
    }
    expect(xs.size).toBe(64);
    expect(new Set(r.tiles.map(String)).size).toBe(r.tiles.length);
  });

  it('never exceeds the bound, whatever the box', () => {
    for (const box of [
      [0, 0, WORLD_SIZE, WORLD_SIZE],
      [10, 10, 11, 11],
      [100, 200, 300, 220],
      [0, 0, 0.001, 0.001]
    ] as [number, number, number, number][]) {
      const r = rasteriseBox(box);
      expect(r.tiles.length).toBeLessThanOrEqual(REGION_TILE_BOUND);
      expect(r.tiles.length).toBeGreaterThan(0);
    }
  });

  it('goes as deep as the grid for a box smaller than a cell', () => {
    // A sub-cell box still names the cell it falls in, at the grid's own depth.
    const r = rasteriseBox([10, 10, 10.001, 10.001]);
    expect(r.depth).toBe(MAX_DEPTH);
    expect(r.tiles.length).toBe(1);
  });

  it('takes a box whose corners arrive in either order, and clamps to the world', () => {
    const a = rasteriseBox([300, 220, 100, 200]);
    const b = rasteriseBox([100, 200, 300, 220]);
    expect(a).toEqual(b);
    const c = rasteriseBox([-50, -50, WORLD_SIZE + 50, WORLD_SIZE + 50]);
    expect(c.depth).toBe(6);
    expect(c.tiles.length).toBe(4096);
  });

  it('honours a tighter bound', () => {
    const r = rasteriseBox([0, 0, WORLD_SIZE, WORLD_SIZE], 16);
    expect(r.depth).toBe(2);
    expect(r.tiles.length).toBe(16);
  });
});

describe('cellExceedsPixel — the exactness rule', () => {
  it('is the ordinary case at the overview', () => {
    // At zoom 0 the world is 512 px, so a depth-6 cell is 8 px.
    expect(cellExceedsPixel(6, 0)).toBe(true);
  });
  it('is the exception once zoomed in', () => {
    // A depth-d cell is 512 · 2^(z − d) px: one pixel exactly at z = d − 9.
    expect(cellExceedsPixel(16, 7)).toBe(false);
    expect(cellExceedsPixel(16, 6)).toBe(false);
    expect(cellExceedsPixel(16, 7.5)).toBe(true);
    expect(cellExceedsPixel(12, 3)).toBe(false);
    expect(cellExceedsPixel(12, 4)).toBe(true);
  });
});

describe('insideBox', () => {
  it('is closed on every side', () => {
    expect(insideBox(1, 1, [1, 1, 2, 2])).toBe(true);
    expect(insideBox(2, 2, [1, 1, 2, 2])).toBe(true);
    expect(insideBox(2.1, 2, [1, 1, 2, 2])).toBe(false);
  });
});
