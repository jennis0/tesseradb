import {describe, expect, it} from 'vitest';
import {fitWorld, pan, toScreen, worldBox, zoomAt} from '../src/camera.js';

describe('the hand-rolled camera', () => {
  it('fits the world square to the shorter side', () => {
    const cam = fitWorld(1024, 512);
    expect(worldBox(cam, 1024, 512)).toEqual([-256, 0, 768, 512]);
  });
  it('zooming about a point keeps the world under it', () => {
    const cam = fitWorld(800, 600);
    const [wx, wy] = [cam.cx + (100 - 400) / cam.scale, cam.cy + (50 - 300) / cam.scale];
    const z = zoomAt(cam, 800, 600, 100, 50, 2);
    const [sx, sy] = toScreen(z, 800, 600, wx, wy);
    expect(sx).toBeCloseTo(100);
    expect(sy).toBeCloseTo(50);
    expect(z.scale).toBeCloseTo(cam.scale * 2);
  });
  it('a pan moves the world with the pointer', () => {
    const cam = fitWorld(800, 600);
    const moved = pan(cam, 40, -20);
    expect(toScreen(moved, 800, 600, cam.cx, cam.cy)).toEqual([440, 280]);
  });
});
