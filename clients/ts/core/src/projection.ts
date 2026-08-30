import {CELL_GRID} from './coords.js';
import type {ProjectionName, Quantisation, TileScheme, ViewInfo} from './types.js';

/**
 * What a view is a picture of, read off `/v1/meta` (`projections.md` §9).
 *
 * A client is told four things about each view — the projection that placed its positions, the
 * ratio its world should be drawn at, the tile scheme its frame addresses, and the tile the frame
 * is under that scheme — and decides two things from them: **whether a basemap may be drawn**, and
 * **how to invert a stored position back to longitude and latitude**. Both used to be facts a host
 * was told out of band, and a host that was told wrong drew a map that looked right.
 *
 * **A client does not choose a basemap — it is handed one** ({@link ViewInfo.tileScheme} is what a
 * host chooses by). So nothing here fetches a tile or names a server; it answers which scheme, if
 * any, this view's frame is addressed in.
 *
 * **Alignment is not availability, which is why the scheme is a name and not a flag.** An
 * equirectangular frame is a square of a square tiling exactly as a Web Mercator frame is, and no
 * tile server serves that tiling — the published longitude/latitude schemes are 2:1 at their top
 * level. A `null` scheme means: draw the points, draw no basemap.
 */

/** The only tile scheme a Tessera view can address: the slippy-map `z/x/y`. */
export const XYZ: TileScheme = 'xyz';

/**
 * The tile scheme `view`'s frame is addressed in, or `null` where none is — **the one condition
 * under which a tile basemap lines up with the points**.
 *
 * A host reads this rather than inspecting the frame: a frame's alignment to a square tiling is
 * necessary and not sufficient, and the server has already applied the projection half of the
 * question.
 */
export function basemapScheme(view: ViewInfo): TileScheme | null {
  return view.tileScheme;
}

/**
 * A stored position back to longitude and latitude in degrees, or `null` for a view that projects
 * nothing and therefore has no longitude at all.
 *
 * `cx`/`cy` are cell-grid units — what {@link decodeViewport} returns, `0…65536` per axis with the
 * residual as the fraction — which the view's own quantisation extent scales into the unit square
 * the projection produced, and the projection's inverse then reads as a place on the Earth.
 *
 * The inverse is exact for any position the forward transform produced from an in-domain
 * coordinate. It is **not** exact for a clipped one: a point beyond Web Mercator's ±85.0511° was
 * moved onto the frame's edge at the build, losing the difference, and no inverse recovers it
 * (`projections.md` §7). What comes back for such a point is the edge, which is where it is
 * stored and where it is drawn.
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
 * The projection's own inverse, over the unit square its forward transform produces — x east and
 * **y south**, which is the direction an XYZ tile row counts in and the opposite of EPSG:3857's
 * northing.
 *
 * Every equirectangular alias is one transform (`projections.md` §5.2): the standard parallel
 * never reaches a stored coordinate and survives only as {@link ViewInfo.worldAspect}, so
 * `gall_isographic` inverts identically to `equirectangular`.
 */
function lonLatOfUnitSquare(x: number, y: number, projection: ProjectionName): [number, number] {
  const lon = x * 360 - 180;
  if (projection === 'web_mercator') {
    const mercY = (0.5 - y) * 2 * Math.PI;
    return [lon, ((2 * Math.atan(Math.exp(mercY)) - Math.PI / 2) * 180) / Math.PI];
  }
  return [lon, (0.5 - y) * 180];
}
