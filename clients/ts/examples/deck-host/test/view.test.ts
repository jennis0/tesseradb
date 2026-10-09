import {describe, expect, it} from 'vitest';
import {WORLD_SIZE, type Quantisation} from '@mosaica/client';
import {viewInputOf} from '@mosaica/deck';
import {fitWorld} from '../src/view.js';

const EXTENT: Quantisation = {xMin: 0, xMax: 1000, yMin: -50, yMax: 50};
const store = {
  frame: () => EXTENT,
  dataXY: (x: number, y: number): [number, number] => [(x / WORLD_SIZE) * 1000, (y / WORLD_SIZE) * 100 - 50]
};

describe('the host camera', () => {
  it('asks the store for the whole extent when fitted to a square canvas', () => {
    expect(viewInputOf(store, fitWorld(800, 800), 800, 800)).toEqual({bbox: [0, -50, 1000, 50], zoom: fitWorld(800, 800).zoom, width: 800, height: 800});
  });

  it('fits the world square to the shorter side of a wide canvas', () => {
    const input = viewInputOf(store, fitWorld(1600, 800), 1600, 800);
    expect(input?.bbox).toEqual([0, -50, 1000, 50]);
    expect(fitWorld(1600, 800).zoom).toBeCloseTo(Math.log2(800 / WORLD_SIZE));
  });
});
