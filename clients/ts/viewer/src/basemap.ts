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
 * The tiles are composed into one image rather than drawn as a layer each, because `basemap` is one
 * layer and because a single texture is one upload instead of `4^depth`.
 */

/** A slippy-map tile's pixel size — the size every `xyz` server publishes. */
const TILE_PX = 256;

/**
 * OpenStreetMap's own tile server. Fine for a screenshot and **not for a deployment**: its usage
 * policy forbids production load, and the client-components design's answer for that is a
 * self-hosted PMTiles basemap (client-interaction §12).
 */
const osm = (z: number, x: number, y: number) => `https://tile.openstreetmap.org/${z}/${x}/${y}.png`;

/**
 * A basemap for `view`, or `null` where no scheme addresses its frame.
 *
 * `depth` is levels **below the frame's own tile**: the frame is `view.tile`, and the image is the
 * `2^depth × 2^depth` tiles that subdivide it, which is what makes this work for a whole-world
 * frame and for an aligned sub-square alike. It covers the frame exactly, so its bounds are the
 * deck.gl world the points are drawn in and no second alignment is computed here.
 */
export async function basemapLayer(view: ViewInfo, depth = 4): Promise<Layer | null> {
  const scheme = basemapScheme(view);
  if (scheme === null || view.tile === null || typeof document === 'undefined') return null;

  const n = 1 << depth;
  const canvas = document.createElement('canvas');
  canvas.width = n * TILE_PX;
  canvas.height = n * TILE_PX;
  const ctx = canvas.getContext('2d');
  if (!ctx) return null;

  const {z, x, y} = view.tile;
  await Promise.all(
    Array.from({length: n * n}, async (_, i) => {
      const [dx, dy] = [i % n, Math.floor(i / n)];
      const response = await fetch(osm(z + depth, x * n + dx, y * n + dy));
      if (!response.ok) throw new Error(`basemap tile ${z + depth}/${x * n + dx}/${y * n + dy}: ${response.status}`);
      ctx.drawImage(await createImageBitmap(await response.blob()), dx * TILE_PX, dy * TILE_PX);
    })
  );

  return new BitmapLayer({
    id: 'basemap',
    image: canvas,
    // `[left, bottom, right, top]`. The map's orthographic view is `flipY`, so the world's y runs
    // south exactly as the tile grid's row index does — which is the frame's own convention
    // (projections §4) and why the image needs no flip of its own.
    bounds: [0, WORLD_SIZE, WORLD_SIZE, 0],
    opacity: 0.85
  }) as unknown as Layer;
}
