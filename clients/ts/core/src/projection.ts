import {CELL_GRID} from './coords.js';
import type {MapProjection, Quantisation, TileScheme, ViewInfo} from './types.js';

/**
 * What a view is a picture of, read from `/v1/meta`: whether a basemap may be drawn under it, and
 * how to invert a stored position to longitude and latitude.
 *
 * The host supplies the basemap; nothing here fetches a tile. The scheme is a name because a frame
 * that aligns with a square tiling may have no tile server: an equirectangular frame is a square
 * of a square tiling, and the published longitude and latitude schemes are 2:1 at their top level.
 * A `null` scheme means draw the points and no basemap.
 */

/**
 * The tile scheme `view`'s frame is addressed in (`view.tileScheme`), or `null`. A tile basemap
 * lines up with the points only where this is not `null`; with `null`, draw the points and no
 * basemap. The server decides it from the view's projection and frame.
 *
 * @category Coordinates and colour
 */
export function basemapScheme(view: ViewInfo): TileScheme | null {
  return view.tileScheme;
}

/**
 * A stored position as `[longitude, latitude]` in degrees, or `null` for a view whose projection is
 * `none`. A point clipped at the build (beyond Web Mercator's ±85.0511°) returns the frame's edge,
 * where it is stored and drawn.
 *
 * @param cx - The x position in cell space, `[0, 65536)` across the frame: a
 *   {@link ViewportResult}'s `positions`, or a {@link Band}'s world-space `positions` times
 *   `65536 / WORLD_SIZE`.
 * @param cy - The y position in cell space.
 * @param q - The view's `quantisation`.
 *
 * @category Coordinates and colour
 */
export function lonLatOfCell(
  cx: number,
  cy: number,
  view: ViewInfo,
  q: Quantisation
): [number, number] | null {
  if (view.projection === 'none') return null;
  const x = q.xMin + (cx / CELL_GRID) * (q.xMax - q.xMin);
  const y = q.yMin + (cy / CELL_GRID) * (q.yMax - q.yMin);
  return lonLatOfUnitSquare(x, y, view.projection);
}

/**
 * The projection's inverse over its unit square, x east and y south, as XYZ tile rows count. Every
 * equirectangular alias is one transform; the standard parallel affects only
 * {@link ViewInfo.worldAspect}.
 */
function lonLatOfUnitSquare(x: number, y: number, projection: MapProjection): [number, number] {
  const lon = x * 360 - 180;
  if (projection === 'web_mercator') {
    const mercY = (0.5 - y) * 2 * Math.PI;
    return [lon, ((2 * Math.atan(Math.exp(mercY)) - Math.PI / 2) * 180) / Math.PI];
  }
  return [lon, (0.5 - y) * 180];
}
