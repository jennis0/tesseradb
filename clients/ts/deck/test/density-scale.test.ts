import {describe, expect, it} from 'vitest';
import {WORLD_SIZE} from '@mosaica/client';
import {DEFAULT_DENSITY_SCALE, densityCountAt, densityPosition} from '../src/index.js';
import {binDensity, contourThresholds, densityPaint, densitySteps, densityStops, drawnCells, filterDensity, gridImage, maxCount, type DensityCell, type DensityCounts} from '../src/density.js';
import {rampAt} from '../src/colour.js';

/**
 * The one scale every density mode colours by: a count's position between no items and the largest
 * count drawn, linear or log.
 */

const cell = (x: number, count: number, depth = 4): DensityCell => {
  const span = WORLD_SIZE / 2 ** depth;
  return {x, y: 0, position: [(x + 0.5) * span, 0.5 * span], count};
};
const counts = (...tallies: number[]): DensityCounts => ({depth: 4, cells: tallies.map((n, x) => cell(x, n))});

describe('densityPosition', () => {
  it('places a count in proportion to the largest under linear', () => {
    expect(densityPosition(0, 100, 'linear')).toBe(0);
    expect(densityPosition(25, 100, 'linear')).toBe(0.25);
    expect(densityPosition(100, 100, 'linear')).toBe(1);
  });

  it('places a count by log1p under log', () => {
    expect(densityPosition(0, 99, 'log')).toBe(0);
    expect(densityPosition(9, 99, 'log')).toBeCloseTo(0.5, 12);
    expect(densityPosition(99, 99, 'log')).toBe(1);
  });

  it.each(['linear', 'log'] as const)('under %s, puts a lone cell or a largest of 1 at the top, ties together, and nothing at 0', (scale) => {
    expect(densityPosition(7, 7, scale)).toBe(1);
    expect(densityPosition(1, 1, scale)).toBe(1);
    expect(densityPosition(0, 1, scale)).toBe(0);
    expect(densityPosition(40, 300, scale)).toBe(densityPosition(40, 300, scale));
    // No count drawn: every position is 0.
    expect(densityPosition(0, 0, scale)).toBe(0);
    expect(densityPosition(5, 0, scale)).toBe(0);
  });

  it.each(['linear', 'log'] as const)('is inverted by densityCountAt under %s', (scale) => {
    for (const count of [0, 1, 17, 300, 80_000]) expect(densityCountAt(densityPosition(count, 80_000, scale), 80_000, scale)).toBeCloseTo(count, 6);
  });

  it('places small counts higher under log than under linear', () => {
    expect(densityPosition(300, 80_000, 'log')).toBeGreaterThan(0.5);
    expect(densityPosition(300, 80_000, 'linear')).toBeLessThan(0.01);
  });

  it('defaults to log', () => {
    expect(DEFAULT_DENSITY_SCALE).toBe('log');
  });
});

describe('the modes on one scale', () => {
  const alphaAt = (image: {data: Uint8ClampedArray}, x: number) => image.data[x * 4 + 3]!;

  it.each(['linear', 'log'] as const)('washes each cell at its %s position: the faint floor at 0, the full strength at the largest', (scale) => {
    const image = binDensity(counts(1, 10, 100, 100), scale)!;
    expect(alphaAt(image, 2)).toBe(178);
    expect(alphaAt(image, 3)).toBe(alphaAt(image, 2));
    expect(alphaAt(image, 1)).toBe(Math.round(28 + densityPosition(10, 100, scale) * 150));
    expect(alphaAt(image, 0)).toBe(Math.round(28 + densityPosition(1, 100, scale) * 150));
  });

  it('washes a single cell and cells of one count at full strength', () => {
    expect(alphaAt(binDensity(counts(3))!, 0)).toBe(178);
    const flat = binDensity(counts(1, 1, 1), 'linear')!;
    expect([0, 1, 2].map((x) => alphaAt(flat, x))).toEqual([178, 178, 178]);
  });

  it.each(['linear', 'log'] as const)('colours each grid cell from the step its %s position falls in', (scale) => {
    const stops = densityStops('viridis', 'dark');
    const steps = 8;
    const image = gridImage(counts(1, 10, 100), stops, steps, scale)!;
    const width = image.width;
    const per = width / 3;
    for (const [x, n] of [1, 10, 100].entries()) {
      const i = x * per * 4;
      const step = Math.min(steps - 1, Math.floor(densityPosition(n, 100, scale) * steps));
      expect([...image.data.slice(i, i + 3)]).toEqual([...rampAt(stops, (step + 0.5) / steps)]);
    }
  });

  it('takes each step’s colour at the middle of its share of the scale', () => {
    const stops = densityStops('greys', 'light');
    expect(densitySteps(stops, 4)).toEqual([0.125, 0.375, 0.625, 0.875].map((t) => rampAt(stops, t)));
  });

  it('colours the wash under a ramp by the position its intensity stands for, the alpha floor taken off', () => {
    const stops = densityStops('viridis', 'dark');
    // A broad field of one count is drawn at its own position in its middle, where the blur has nothing to average in.
    const flat = (count: number, max: number) => {
      const cells: DensityCell[] = [];
      for (let x = 0; x < 9; x++) for (let y = 0; y < 9; y++) cells.push({...cell(x, count), y});
      cells.push({...cell(20, max), y: 20});
      return filterDensity(binDensity({depth: 4, cells}, 'linear')!, 4, densityPaint('viridis', 'dark'));
    };
    const at = (image: {data: Uint8ClampedArray; width: number}, x: number, y: number) => [...image.data.slice((y * image.width + x) * 4, (y * image.width + x) * 4 + 3)];
    // Cell (4, 4) is at texels 4·(4 + 1) + 2 with the padding cell, inside the field.
    const centre = 4 * 5 + 2;
    for (const [count, max] of [[0.0001, 1], [50, 100], [100, 100]] as const) {
      const image = flat(count, max);
      const p = densityPosition(count, max, 'linear');
      const want = rampAt(stops, p);
      at(image, centre, centre).forEach((v, k) => expect(Math.abs(v - want[k]!)).toBeLessThanOrEqual(3));
    }
  });

  it.each(['linear', 'log'] as const)('draws four contours evenly spaced on the %s scale, ascending', (scale) => {
    const thresholds = contourThresholds(counts(1, 50, 1000).cells, scale);
    expect(thresholds).toHaveLength(4);
    expect(thresholds.map((t) => densityPosition(t, 1000, scale))).toEqual([0.2, 0.4, 0.6, 0.8].map((p) => expect.closeTo(p, 9)));
    expect([...thresholds].sort((a, b) => a - b)).toEqual(thresholds);
  });

  it('keeps one contour of the levels between the same two whole counts, and none under one item', () => {
    // A largest count of 1: every level of either scale lies under 1, and would ring each point.
    expect(contourThresholds(counts(1, 1).cells, 'linear')).toEqual([]);
    expect(contourThresholds(counts(1).cells, 'log')).toEqual([]);
    // A largest count of 3 under linear: 0.6 is under 1; 1.2, 1.8 and 2.4 fall between 1–2, 1–2 and 2–3.
    expect(contourThresholds(counts(3).cells, 'linear').map((t) => Math.ceil(t))).toEqual([2, 3]);
  });

  it('draws no contours where no cell has a count', () => {
    expect(contourThresholds([], 'log')).toEqual([]);
    expect(contourThresholds(counts(0, 0).cells, 'linear')).toEqual([]);
  });

  it('takes the top of the hexagons’ and contours’ scale from the merged cells they draw', () => {
    // Four neighbouring cells of 1 merge to one of 4 once the budget is a single cell.
    const many: DensityCounts = {depth: 14, cells: [0, 1].flatMap((x) => [0, 1].map((y) => ({x, y, position: [0, 0] as [number, number], count: 1})))};
    const fine = {...many, cells: many.cells.slice()};
    for (let i = 0; i < 10_001; i++) fine.cells.push({x: 2 * (i % 2000) + 4, y: 2 * Math.floor(i / 2000), position: [0, 0], count: 1});
    expect(maxCount(drawnCells(fine, 'grid').cells)).toBe(1);
    expect(maxCount(drawnCells(fine, 'hex').cells)).toBeGreaterThan(1);
    expect(drawnCells(fine, 'hex')).toBe(drawnCells(fine, 'hex'));
  });
});
