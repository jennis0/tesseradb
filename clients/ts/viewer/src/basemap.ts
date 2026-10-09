import {BitmapLayer} from '@deck.gl/layers';
import type {Layer} from '@deck.gl/core';
import {WORLD_SIZE, type ViewInfo} from '@mosaicajs/client';
import {basemapScheme} from '@mosaicajs/client/internal';

/**
 * The viewer's basemap, built from what `/v1/meta` says the view is a picture of. `<mosaica-map>`
 * draws a `basemap` layer under the points; which tiles, from which server, is the host page's
 * choice.
 *
 * A basemap is drawn only where the view declares a `tile_scheme`. An aligned extent is not enough:
 * an equirectangular frame is square, but no tile server serves that tiling. With no scheme, no
 * basemap is drawn.
 *
 * The visible box is composed at the camera's depth into one image, so `basemap` stays one layer
 * and one texture upload.
 */

/** A slippy-map tile's size in pixels. */
const TILE_PX = 256;

/** The most tiles one composition fetches; a box needing more is drawn one level coarser. */
const MAX_TILES = 64;

/** The deepest XYZ level to ask for, the standard style's deepest. */
const MAX_LEVEL = 19;

/** OpenStreetMap's tile server, whose usage policy forbids production load. */
const osm = (z: number, x: number, y: number) => `https://tile.openstreetmap.org/${z}/${x}/${y}.png`;

/** Tiles already decoded, by `z/x/y`, evicted oldest first (a `Map` iterates in insertion order). */
const held = new Map<string, ImageBitmap>();
const HELD_MAX = 512;

async function tile(z: number, x: number, y: number): Promise<ImageBitmap | null> {
  const key = `${z}/${x}/${y}`;
  const bitmap = held.get(key);
  if (bitmap) return bitmap;
  const response = await fetch(osm(z, x, y));
  // A tile that will not load leaves a hole; the rest of the composition is kept.
  if (!response.ok) return null;
  const decoded = await createImageBitmap(await response.blob());
  if (held.size >= HELD_MAX) held.delete(held.keys().next().value as string);
  held.set(key, decoded);
  return decoded;
}

/** Where the camera is, in the deck world the points are drawn in. */
export type Camera = {
  /** `[x0, y0, x1, y1]` in world units, y running south as the frame does. */
  worldBox: [number, number, number, number];
  /** The viewport's zoom, equal to tile depth. */
  zoom: number;
};

/** Which tiles a composition covers, compared to decide whether to recompose. */
export type BasemapCover = {level: number; x0: number; y0: number; x1: number; y1: number};

/**
 * The tiles to compose for a camera: a depth below the frame's own tile, and the range of the
 * frame's subdivision the box touches. The depth is the zoom plus one, since a world tile at zoom
 * `z` is 512 screen pixels and an `xyz` image is 256.
 */
export function coverFor(view: ViewInfo, camera: Camera): BasemapCover {
  const frame = view.tile!;
  let level = Math.max(0, Math.min(MAX_LEVEL - frame.z, Math.round(camera.zoom) + 1));
  for (;;) {
    const span = WORLD_SIZE / 2 ** level;
    const last = 2 ** level - 1;
    const clamp = (v: number) => Math.max(0, Math.min(last, Math.floor(v / span)));
    const x0 = clamp(camera.worldBox[0]);
    const y0 = clamp(camera.worldBox[1]);
    const x1 = clamp(camera.worldBox[2]);
    const y1 = clamp(camera.worldBox[3]);
    if ((x1 - x0 + 1) * (y1 - y0 + 1) <= MAX_TILES || level === 0) return {level, x0, y0, x1, y1};
    level -= 1;
  }
}

/**
 * A basemap for `view` over `camera`, or `null` where no scheme addresses its frame.
 *
 * With no camera it covers the whole frame at a depth cheap enough to draw before the first view
 * change has been reported, for the map to open on.
 */
export async function basemapLayer(view: ViewInfo, camera?: Camera): Promise<Layer | null> {
  const scheme = basemapScheme(view);
  if (scheme === null || view.tile === null || typeof document === 'undefined') return null;

  const cover = camera
    ? coverFor(view, camera)
    : {level: 3, x0: 0, y0: 0, x1: 2 ** 3 - 1, y1: 2 ** 3 - 1};
  const nx = cover.x1 - cover.x0 + 1;
  const ny = cover.y1 - cover.y0 + 1;

  const canvas = document.createElement('canvas');
  canvas.width = nx * TILE_PX;
  canvas.height = ny * TILE_PX;
  const ctx = canvas.getContext('2d');
  if (!ctx) return null;

  const {z, x, y} = view.tile;
  const n = 2 ** cover.level;
  await Promise.all(
    Array.from({length: nx * ny}, async (_, i) => {
      const [dx, dy] = [cover.x0 + (i % nx), cover.y0 + Math.floor(i / nx)];
      const bitmap = await tile(z + cover.level, x * n + dx, y * n + dy);
      if (bitmap) ctx.drawImage(bitmap, (dx - cover.x0) * TILE_PX, (dy - cover.y0) * TILE_PX);
    })
  );

  const span = WORLD_SIZE / n;
  return new BitmapLayer({
    id: 'basemap',
    image: canvas,
    // `[left, bottom, right, top]`. The view is `flipY`, so world y runs south as tile rows do
    // and `bottom` is the larger y.
    bounds: [cover.x0 * span, (cover.y1 + 1) * span, (cover.x1 + 1) * span, cover.y0 * span],
    opacity: 0.85
  }) as unknown as Layer;
}
