import {describe, expect, it} from 'vitest';
import {tableFromArrays} from 'apache-arrow';
import {WORLD_SIZE} from '@tesseradb/client';
import {mortonOfTile} from '@tesseradb/client/internal';
import {binDensity, densityCellsOf, type DensityCell, type DensityCounts} from '../src/density.js';
import {resolvePick} from '../src/pick.js';

/** One cell at `depth` with `count`, as {@link densityCellsOf} reads it from an answer. */
const cell = (x: number, y: number, depth: number, count: number): DensityCell => {
  const span = WORLD_SIZE / 2 ** depth;
  return {x, y, position: [(x + 0.5) * span, (y + 0.5) * span], count};
};
const counts = (depth: number, cells: DensityCell[]): DensityCounts => ({depth, cells});

describe('densityCellsOf', () => {
  it('places each row at its cell’s centre, at the viewport’s depths and past them', () => {
    for (const depth of [3, 16, 20]) {
      const span = WORLD_SIZE / 2 ** depth;
      const table = tableFromArrays({cell: BigUint64Array.from([mortonOfTile(1, 2, depth), mortonOfTile(5, 3, depth)]), count: BigUint64Array.from([7n, 40n])});
      expect(densityCellsOf({rows: table}, depth)).toEqual([
        {x: 1, y: 2, position: [1.5 * span, 2.5 * span], count: 7},
        {x: 5, y: 3, position: [5.5 * span, 3.5 * span], count: 40}
      ]);
    }
  });

  it('reads nothing from a table without cells', () => {
    expect(densityCellsOf({rows: tableFromArrays({count: BigUint64Array.from([3n])})}, 4)).toEqual([]);
  });
});

describe('binDensity', () => {
  it('lands each cell’s count in its own bin, over the rectangle the cells span', () => {
    const image = binDensity(counts(3, [cell(4, 6, 3, 100), cell(6, 7, 3, 5), cell(5, 6, 3, 0)]))!;
    expect(image.width).toBe(3);
    expect(image.height).toBe(2);
    // Depth 3: 64 world units a cell; x 4..6, y 6..7.
    expect(image.bounds).toEqual([256, 384, 448, 512]);
    const alphaAt = (x: number, y: number) => image.data[((y - 6) * 3 + (x - 4)) * 4 + 3]!;
    expect(alphaAt(4, 6)).toBeGreaterThan(alphaAt(6, 7)); // the larger count is the stronger bin
    expect(alphaAt(6, 7)).toBeGreaterThan(0);
    expect(alphaAt(5, 6)).toBe(0); // a zero count is transparent
    expect(alphaAt(5, 7)).toBe(0); // a cell with no row is transparent
    expect(image.filled).toBe(2);
  });

  it('gives cells of equal count equal intensity', () => {
    const image = binDensity(counts(1, [cell(0, 0, 1, 100), cell(1, 0, 1, 100)]))!;
    expect(image.data[3]).toBe(image.data[7]);
  });

  it('washes nothing where no cell has a count', () => {
    expect(binDensity(counts(3, [cell(2, 1, 3, 0)]))).toBeNull();
    expect(binDensity(counts(3, []))).toBeNull();
  });
});

describe('resolvePick — a miss is not a broken pick', () => {
  const ids = BigUint64Array.from([11n, 12n]);
  it('resolves a mark by its sublayer’s identity array', () => {
    expect(resolvePick({index: 1, sourceLayer: {id: 'm', props: {tesseraIds: ids}}, coordinate: [3, 4]})).toEqual({
      kind: 'mark',
      id: 12n,
      worldXY: [3, 4]
    });
  });
  it('answers the mark’s own position, not the pointer’s', () => {
    const positions = Float32Array.from([1, 2, 30, 40]);
    expect(resolvePick({index: 1, sourceLayer: {id: 'm', props: {tesseraIds: ids, tesseraPositions: positions}}, coordinate: [31, 41]})).toEqual({
      kind: 'mark',
      id: 12n,
      worldXY: [30, 40]
    });
  });
  it('resolves an artifact marker on its own route', () => {
    expect(resolvePick({index: 0, sourceLayer: {id: 'a', props: {artifactIds: [99n]}}})).toEqual({kind: 'artifact', id: 99n});
  });
  it('reports a miss for nothing under the cursor', () => {
    expect(resolvePick({index: -1})).toEqual({kind: 'miss'});
  });
  it('reports a hit it cannot resolve as broken, naming the layer', () => {
    expect(resolvePick({index: 5, sourceLayer: {id: 'm', props: {tesseraIds: ids}}})).toEqual({
      kind: 'broken',
      index: 5,
      layer: 'm',
      hasIds: true,
      idCount: 2
    });
    expect(resolvePick({index: 0, sourceLayer: {id: 'x', props: {}}}).kind).toBe('broken');
  });
});

import {DENSITY_SUPERSAMPLE, filterDensity} from '../src/density.js';

describe('filterDensity: the cell grid is not shown', () => {
  it('turns a single non-zero bin into a halo with no one-texel step from nothing to full', () => {
    const binned = binDensity(counts(3, [cell(4, 6, 3, 100)]))!;
    const soft = filterDensity(binned, 3);
    const S = DENSITY_SUPERSAMPLE;
    // One padding cell each side, S texels a cell.
    expect(soft.width).toBe(3 * S);
    expect(soft.height).toBe(3 * S);
    expect(soft.bounds).toEqual([256 - 64, 384 - 64, 320 + 64, 448 + 64]);
    const alpha = (x: number, y: number) => soft.data[(y * soft.width + x) * 4 + 3]!;
    let peak = 0;
    for (let y = 0; y < soft.height; y++) for (let x = 0; x < soft.width; x++) peak = Math.max(peak, alpha(x, y));
    expect(peak).toBeGreaterThan(0);
    // Every neighbouring pair of texels differs by well under the peak: the edge is a ramp.
    let worst = 0;
    for (let y = 0; y < soft.height; y++) {
      for (let x = 1; x < soft.width; x++) worst = Math.max(worst, Math.abs(alpha(x, y) - alpha(x - 1, y)));
    }
    for (let x = 0; x < soft.width; x++) {
      for (let y = 1; y < soft.height; y++) worst = Math.max(worst, Math.abs(alpha(x, y) - alpha(x, y - 1)));
    }
    expect(worst).toBeLessThanOrEqual(peak * 0.35);
    // The halo extends beyond the cell and fades to nothing at the padding's far edge.
    expect(alpha(S + S / 2, S + S / 2)).toBe(peak);
    expect(alpha(0, S + S / 2)).toBe(0);
    expect(alpha(S / 2, S + S / 2)).toBeGreaterThan(0);
    expect(alpha(S / 2, S + S / 2)).toBeLessThan(peak);
  });

  it('keeps an empty bin between two filled ones dimmer than either', () => {
    const binned = binDensity(counts(3, [cell(0, 0, 3, 100), cell(2, 0, 3, 100)]))!;
    const soft = filterDensity(binned, 3);
    const S = DENSITY_SUPERSAMPLE;
    const mid = S + S / 2;
    const alpha = (x: number) => soft.data[(mid * soft.width + x) * 4 + 3]!;
    expect(alpha(2 * S + S / 2)).toBeLessThan(alpha(S + S / 2));
    expect(alpha(2 * S + S / 2)).toBeLessThan(alpha(3 * S + S / 2));
  });
});
