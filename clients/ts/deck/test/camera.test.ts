import {describe, expect, it} from 'vitest';
import {WORLD_SIZE, type Quantisation} from '@tesseradb/client';
import {viewInputOf} from '../src/camera.js';

/** A store's frame and its world-to-data conversion, over the given extent or none. */
function store(q: Quantisation | null) {
  return {
    frame: () => q,
    dataXY: (x: number, y: number): [number, number] => [q!.xMin + (x / WORLD_SIZE) * (q!.xMax - q!.xMin), q!.yMin + (y / WORLD_SIZE) * (q!.yMax - q!.yMin)]
  };
}

const EXTENT: Quantisation = {xMin: -180, xMax: 180, yMin: -90, yMax: 90};

describe('viewInputOf', () => {
  it('asks for nothing before the store has a frame', () => {
    expect(viewInputOf(store(null), {target: [256, 256], zoom: 0}, 512, 512)).toBeNull();
  });

  it('asks for the whole extent when the world fills the canvas', () => {
    expect(viewInputOf(store(EXTENT), {target: [256, 256, 0], zoom: 0}, 512, 512)).toEqual({bbox: [-180, -90, 180, 90], zoom: 0, width: 512, height: 512});
  });

  it('asks for the quarter the camera shows at zoom 1', () => {
    const input = viewInputOf(store(EXTENT), {target: [128, 128], zoom: 1}, 512, 512);
    expect(input).toEqual({bbox: [-180, -90, 0, 0], zoom: 1, width: 512, height: 512});
  });

  it('clamps a camera that shows past the world to the extent', () => {
    const input = viewInputOf(store(EXTENT), {target: [256, 256], zoom: -1}, 512, 256);
    expect(input?.bbox).toEqual([-180, -90, 180, 90]);
    // The camera's own zoom, not the zoom at which the clamped box fills the canvas.
    expect(input?.zoom).toBe(-1);
  });
});
