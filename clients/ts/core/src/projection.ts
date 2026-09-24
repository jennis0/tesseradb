import {CELL_GRID} from './coords.js';
import type {ProjectionName, Quantisation, TileScheme, ViewInfo} from './types.js';

/**
 * What a view is a picture of, read from `/v1/meta`: whether a basemap may be drawn under it, and
 * how to invert a stored position to longitude and latitude.
 *
 * The host supplies the basemap; nothing here fetches a tile. The scheme is a name because a frame
 * that aligns with a square tiling may have no tile server: an equirectangular frame is a square
 * of a square tiling, and the published longitude and latitude schemes are 2:1 at their top level.
 * A `null` scheme means draw the points and no basemap.
 */

/** The only tile scheme a Tessera view can address: the slippy-map `z/x/y`. */
export const XYZ: TileScheme = 'xyz';

/**
 * The tile scheme `view`'s frame is addressed in, or `null`: the condition under which a tile
 * basemap lines up with the points. The server has already taken the projection into account, and
 * a frame aligned to a square tiling is not enough on its own.
 */
export function basemapScheme(view: ViewInfo): TileScheme | null {
  return view.tileScheme;
}

/**
 * A stored position as longitude and latitude in degrees, or `null` for a view with no projection.
 * `cx`/`cy` are cell-grid units, as {@link decodeViewport} returns. A point clipped at the build
 * (beyond Web Mercator's ±85.0511°) returns the frame's edge, where it is stored and drawn.
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
function lonLatOfUnitSquare(x: number, y: number, projection: ProjectionName): [number, number] {
  const lon = x * 360 - 180;
  if (projection === 'web_mercator') {
    const mercY = (0.5 - y) * 2 * Math.PI;
    return [lon, ((2 * Math.atan(Math.exp(mercY)) - Math.PI / 2) * 180) / Math.PI];
  }
  return [lon, (0.5 - y) * 180];
}
