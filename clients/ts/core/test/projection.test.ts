import {describe, expect, it} from 'vitest';
import {CELL_GRID} from '../src/coords.js';
import {XYZ, basemapScheme, lonLatOfCell} from '../src/projection.js';
import type {Quantisation, ViewInfo} from '../src/types.js';

/** `/v1/meta`'s four projection fields, as the server publishes them for one view. */
const view = (
  projection: ViewInfo['projection'],
  tileScheme: ViewInfo['tileScheme'] = null,
  tile: ViewInfo['tile'] = null
): ViewInfo => ({
  id: 's0',
  displayName: 'S0',
  projection,
  worldAspect: projection === 'web_mercator' ? 1 : projection === 'none' ? null : 2,
  tileScheme,
  tile
});

/** The whole world: every projection's output is the unit square (`projections.md` §4). */
const world: Quantisation = {xMin: 0, xMax: 1, yMin: 0, yMax: 1};

/**
 * **The transforms written out rather than imported.** These are `projections.md` §5.1 and §5.2 as
 * a reader would type them, so a round trip below checks the client's inverse against the design
 * and not against itself. x east, **y south**.
 */
const forward = {
  web_mercator: (lon: number, lat: number): [number, number] => [
    (lon + 180) / 360,
    0.5 - Math.log(Math.tan(Math.PI / 4 + ((lat * Math.PI) / 180) / 2)) / (2 * Math.PI)
  ],
  equirectangular: (lon: number, lat: number): [number, number] => [(lon + 180) / 360, 0.5 - lat / 180]
};

/** A unit-square position as the wire stores and returns it: cell-grid units with a residual. */
const cellsOf = (x: number, y: number, q: Quantisation): [number, number] => [
  ((x - q.xMin) / (q.xMax - q.xMin)) * CELL_GRID,
  ((y - q.yMin) / (q.yMax - q.yMin)) * CELL_GRID
];

const places: [string, number, number][] = [
  ['Greenwich', 0, 51.4779],
  ['Quito', -78.4678, -0.1807],
  ['Tokyo', 139.6917, 35.6895],
  ['Sydney', 151.2093, -33.8688],
  ['Reykjavík', -21.8277, 64.1265],
  ['the antimeridian', 179.9999, -0.0001]
];

describe('what a client decides from the published projection fields', () => {
  it('round-trips a stored position to the longitude and latitude it came from', () => {
    for (const [projection, f] of [
      ['web_mercator', forward.web_mercator],
      ['equirectangular', forward.equirectangular],
      ['gall_isographic', forward.equirectangular]
    ] as const) {
      for (const [place, lon, lat] of places) {
        const [x, y] = f(lon, lat);
        const [cx, cy] = cellsOf(x, y, world);
        const got = lonLatOfCell(cx, cy, view(projection), world);
        expect(got, `${projection} at ${place}`).not.toBeNull();
        // 1e-9 degrees is about 0.1 mm on the ground — four orders below the 9.3 mm cell of the
        // finest frame the design permits, so nothing at this scale can move a point.
        expect(got![0], `${projection} lon at ${place}`).toBeCloseTo(lon, 9);
        expect(got![1], `${projection} lat at ${place}`).toBeCloseTo(lat, 9);
      }
    }
  });

  it('round-trips through a sub-square frame, where the extent is doing the work', () => {
    // The z3 tile (5, 2): x [0.625, 0.75], y [0.25, 0.375] — east of the meridian, north of the
    // equator. A client that ignored the extent and read the cells as the whole world would put
    // every one of these points somewhere off the west coast of Africa.
    const frame: Quantisation = {xMin: 0.625, xMax: 0.75, yMin: 0.25, yMax: 0.375};
    const inside = view('web_mercator', XYZ, {z: 3, x: 5, y: 2});
    for (const [lon, lat] of [
      [45, 45],
      [50.5, 60],
      [67, 47.1]
    ]) {
      const [x, y] = forward.web_mercator(lon!, lat!);
      expect(x).toBeGreaterThanOrEqual(frame.xMin);
      expect(x).toBeLessThanOrEqual(frame.xMax);
      const [cx, cy] = cellsOf(x, y, frame);
      const got = lonLatOfCell(cx, cy, inside, frame)!;
      expect(got[0]).toBeCloseTo(lon!, 9);
      expect(got[1]).toBeCloseTo(lat!, 9);
    }
  });

  it('offers no longitude for a view that projects nothing', () => {
    const embedding: Quantisation = {xMin: -10, xMax: 10, yMin: 0, yMax: 1000};
    expect(lonLatOfCell(0, 0, view('none'), embedding)).toBeNull();
    expect(lonLatOfCell(CELL_GRID / 2, CELL_GRID / 2, view('none'), embedding)).toBeNull();
  });

  it('draws a basemap only where a scheme addresses the frame', () => {
    // **The case a boolean gets wrong.** The equirectangular view's frame here is the same aligned
    // square as the Web Mercator one's, and it addresses no published scheme: the longitude/
    // latitude schemes are 2:1 at their top level, so a host reading alignment as availability
    // would put a Mercator basemap under a corpus that cannot line up with one.
    expect(basemapScheme(view('web_mercator', XYZ, {z: 3, x: 5, y: 2}))).toBe('xyz');
    expect(basemapScheme(view('equirectangular'))).toBeNull();
    expect(basemapScheme(view('gall_isographic'))).toBeNull();
    expect(basemapScheme(view('none'))).toBeNull();
  });
});
