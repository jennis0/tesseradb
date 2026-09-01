import {BitmapLayer} from '@deck.gl/layers';
import type {Layer} from '@deck.gl/core';
import {WORLD_SIZE, basemapScheme, type ViewInfo} from '@tesseradb/client';

/**
 * The demo's basemap, built from what `/v1/meta` says the view is a picture of
 * (design projections §9).
 *
 * **This is the host's job and not the map component's.** `<tessera-map>` takes a `basemap` layer
 * and draws it under the points; which tiles those are, and from whose server, is a decision the
 * page around it makes — so the whole of it lives here, in the demo.
 *
 * **The condition is `tile_scheme`, not the extent.** A tile basemap lines up with the points only
 * when the view's frame is a square of that scheme's own tiling, and a frame being aligned does not
 * say so on its own: an equirectangular frame is a square of a square tiling no server serves, so a
 * host that read alignment as availability would draw a Mercator basemap under a corpus that cannot
 * line up with one. A null scheme means draw the points and no basemap, which is every corpus in
 * the demo's own picker.
 *
 * **It follows the camera.** The first version composed one fixed depth over the whole frame and
 * never changed it, so the ground under a corpus of 73 million places stayed at the resolution of a
 * world map however far in you went — street-level marks over a picture of continents. What is
 * composed now is the visible box at the depth the camera is at, which is one texture again, and a
 * different one per view.
 *
 * The tiles are still composed into one image rather than drawn as a layer each, because `basemap`
 * is one layer and because a single texture is one upload instead of one per tile.
 */

/** A slippy-map tile's pixel size — the size every `xyz` server publishes. */
const TILE_PX = 256;

/**
 * The most tiles one composition will fetch. It bounds the request burst on a view change, and it
 * is what a depth is given up for: a box needing more than this is drawn one level coarser rather
 * than fetching a screenful of tiles the next pan will discard.
 */
const MAX_TILES = 64;

/** The deepest XYZ level to ask for. Beyond 19 the standard style has nothing more to say. */
const MAX_LEVEL = 19;

/**
 * OpenStreetMap's own tile server. Fine for a screenshot and **not for a deployment**: its usage
 * policy forbids production load, and the client-components design's answer for that is a
 * self-hosted PMTiles basemap (client-interaction §12).
 */
const osm = (z: number, x: number, y: number) => `https://tile.openstreetmap.org/${z}/${x}/${y}.png`;

/**
 * Tiles already decoded, by `z/x/y`.
 *
 * Panning back over ground already seen is the common gesture, and without this every return trip
 * re-decodes what the browser cache is holding anyway. Bounded, and evicted oldest-first: a
 * `Map` iterates in insertion order, so the first key is the coldest.
 */
const held = new Map<string, ImageBitmap>();
const HELD_MAX = 512;

async function tile(z: number, x: number, y: number): Promise<ImageBitmap | null> {
  const key = `${z}/${x}/${y}`;
  const bitmap = held.get(key);
  if (bitmap) return bitmap;
  const response = await fetch(osm(z, x, y));
  // **One tile that will not load is a hole, not a failure.** The whole composition used to be
  // thrown away for a single bad response, which turned an edge tile the server declined into no
  // basemap at all.
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
  /** The viewport's zoom, which is 1:1 with tile depth (`coords.ts`, measured). */
  zoom: number;
};

/** Which tiles a composition covers — what a caller compares to decide it need not recompose. */
export type BasemapCover = {level: number; x0: number; y0: number; x1: number; y1: number};

/**
 * The tiles to compose for a camera: the depth below the frame's own tile, and the range of the
 * frame's subdivision the box touches.
 *
 * **The depth is the viewport's zoom plus one.** A deck tile `z` is the depth at which a tile is
 * `TILE_SIZE` (512) screen pixels, and an `xyz` tile's image is 256 — so composing at the zoom
 * itself stretches every tile over twice its pixels, which is the blur one level of detail costs.
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
 * change has been reported — the picture the map opens on.
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
    // `[left, bottom, right, top]`. The map's orthographic view is `flipY`, so the world's y runs
    // south exactly as the tile grid's row index does — which is the frame's own convention
    // (projections §4) and why the image needs no flip of its own: `bottom` is the larger y.
    bounds: [cover.x0 * span, (cover.y1 + 1) * span, (cover.x1 + 1) * span, cover.y0 * span],
    opacity: 0.85
  }) as unknown as Layer;
}
