import {WORLD_SIZE} from '@tesseradb/client';

/**
 * A hand-rolled camera over the store's world space — the 512-unit square every band's
 * positions are in — and nothing of Tessera's. `scale` is pixels per world unit; `cx, cy` is
 * the world point at the canvas centre. What the store is told is the world bbox of the canvas,
 * converted to data coordinates by the host through `store.dataXY`.
 */
export type Camera = {cx: number; cy: number; scale: number};

export function fitWorld(width: number, height: number): Camera {
  return {cx: WORLD_SIZE / 2, cy: WORLD_SIZE / 2, scale: Math.min(width, height) / WORLD_SIZE};
}

/** The world bbox the canvas shows, `[x0, y0, x1, y1]`. */
export function worldBox(cam: Camera, width: number, height: number): [number, number, number, number] {
  const hw = width / 2 / cam.scale;
  const hh = height / 2 / cam.scale;
  return [cam.cx - hw, cam.cy - hh, cam.cx + hw, cam.cy + hh];
}

export function toScreen(cam: Camera, width: number, height: number, wx: number, wy: number): [number, number] {
  return [(wx - cam.cx) * cam.scale + width / 2, (wy - cam.cy) * cam.scale + height / 2];
}

/** Zoom by a factor about a screen point, so the world under the cursor stays put. */
export function zoomAt(cam: Camera, width: number, height: number, px: number, py: number, factor: number): Camera {
  const wx = cam.cx + (px - width / 2) / cam.scale;
  const wy = cam.cy + (py - height / 2) / cam.scale;
  const scale = Math.max(width / WORLD_SIZE / 4, Math.min(cam.scale * factor, 1 << 16));
  return {cx: wx - (px - width / 2) / scale, cy: wy - (py - height / 2) / scale, scale};
}

export function pan(cam: Camera, dxPx: number, dyPx: number): Camera {
  return {...cam, cx: cam.cx - dxPx / cam.scale, cy: cam.cy - dyPx / cam.scale};
}
