#!/usr/bin/env node
// Shoot what a client does with `/v1/meta`'s projection fields: a basemap under the points where a
// scheme addresses the frame, and **no basemap where none does** — the evidence for design
// projections §9.
//
//   node clients/ts/viewer/smoke-basemap.mjs [--url http://localhost:5173] [--dataset ID]
//     [--shots DIR] [--name PREFIX] [--headed] [--executable /path/to/chrome]
//
// **A map whose numbers are right and whose basemap is offset has failed, and only a picture shows
// it.** So the pictures are the point, and the arithmetic around them is read off the running
// client rather than typed in: the four fields the server published, and a handful of known places
// put through the frame and inverted back — a check a picture cannot make, and the one that catches
// a frame mirrored north-south, which round-trips perfectly on the equator and puts London in the
// southern ocean.
//
// The two outcomes are both assertions, not a branch between a test and a screenshot:
//
// * `tile_scheme: "xyz"` — the map must be carrying a basemap layer, and every known place must
//   round-trip to within a cell of where it belongs.
// * `tile_scheme: null` — the map must be carrying **none**, whatever its frame looks like. This is
//   the equirectangular case, whose frame is as aligned as the Mercator one's and which a boolean
//   would get wrong.
//
// Requires a running `tessera serve` and a running `vite dev`.
import {mkdir, writeFile} from 'node:fs/promises';
import {join} from 'node:path';
import {flags, isSupersededAbort, launchBrowser, withParams} from './smoke-browser.mjs';

const args = flags();
const url = args.url ?? 'http://localhost:5173';
const shots = args.shots ?? '/tmp/tessera-basemap';
const name = args.name ?? 'basemap';
await mkdir(shots, {recursive: true});

const browser = await launchBrowser(args);
const page = await browser.newPage({viewport: {width: 1600, height: 1000}});
const consoleErrors = [];
page.on('console', (m) => {
  if (m.type() === 'error') consoleErrors.push(m.text());
});
page.on('pageerror', (e) => consoleErrors.push(`pageerror: ${e.message}`));

/**
 * Cities whose coordinates are published facts, on both sides of the equator and the meridian.
 * @type {[string, number, number][]}
 */
const PLACES = [
  ['London', -0.1276, 51.5072],
  ['Nairobi', 36.8219, -1.2921],
  ['Sydney', 151.2093, -33.8688],
  ['Buenos Aires', -58.3816, -34.6037],
  ['Anchorage', -149.9003, 61.2181],
  ['Singapore', 103.8198, 1.3521]
];

/** One cell of a whole-world frame is 360/65536 = 0.0055° of longitude. */
const CELL_GRID = 65536;
const A_CELL_OR_SO = 0.01;

await page.goto(withParams(url, {dataset: args.dataset}), {waitUntil: 'networkidle'});
await page.waitForFunction(() => window.__tesseraProbe !== undefined, null, {timeout: 120_000});

const publishedScheme = await page.evaluate(
  () => /** @type {any} */ (document.querySelector('tessera-explorer')).store.get('meta').views[0].tileScheme
);
// The basemap is composed from a tile server, so where one is expected it settles after the first
// frame; where none is, waiting proves nothing and the settle below is the whole wait.
if (publishedScheme !== null) {
  await page.waitForFunction(
    () => /** @type {any} */ (document.querySelector('tessera-explorer'))?.map?.basemap != null,
    null,
    {timeout: 120_000}
  );
}
await page.waitForTimeout(5_000);

const reading = await page.evaluate(() => {
  const explorer = /** @type {any} */ (document.querySelector('tessera-explorer'));
  const meta = explorer.store.get('meta');
  const drawn = explorer.store.get('view');
  return {
    view: meta.views[0],
    quantisation: meta.quantisation,
    basemap: explorer.map?.basemap?.id ?? null,
    depth: drawn.depth,
    served: drawn.served.shown,
    visible: String(drawn.visible.value)
  };
});

const failures = [];
const {quantisation: q, view} = reading;

// The transforms written out rather than imported, so this does not borrow the code it checks.
const forward = {
  web_mercator: (lon, lat) => [
    (lon + 180) / 360,
    0.5 - Math.log(Math.tan(Math.PI / 4 + (lat * Math.PI) / 360)) / (2 * Math.PI)
  ],
  equirectangular: (lon, lat) => [(lon + 180) / 360, 0.5 - lat / 180]
};
const inverse = {
  web_mercator: (x, y) => [
    x * 360 - 180,
    ((2 * Math.atan(Math.exp((0.5 - y) * 2 * Math.PI)) - Math.PI / 2) * 180) / Math.PI
  ],
  equirectangular: (x, y) => [x * 360 - 180, (0.5 - y) * 180]
};
const family = view.projection === 'web_mercator' ? 'web_mercator' : 'equirectangular';

const places = [];
if (view.projection !== 'none') {
  for (const [place, lon, lat] of PLACES) {
    const [fx, fy] = forward[family](lon, lat);
    // What the corpus stores: the cell this position falls in, against the published frame.
    const cx = Math.floor(((fx - q.xMin) / (q.xMax - q.xMin)) * CELL_GRID);
    const cy = Math.floor(((fy - q.yMin) / (q.yMax - q.yMin)) * CELL_GRID);
    const rx = q.xMin + (cx / CELL_GRID) * (q.xMax - q.xMin);
    const ry = q.yMin + (cy / CELL_GRID) * (q.yMax - q.yMin);
    const [rlon, rlat] = inverse[family](rx, ry);
    const driftDeg = Math.max(Math.abs(rlon - lon), Math.abs(rlat - lat));
    places.push({place, lon, lat, cell: [cx, cy], back: [rlon, rlat], driftDeg});
    if (driftDeg > A_CELL_OR_SO) failures.push(`${place} round-trips ${driftDeg.toFixed(4)}° away`);
  }
}

if (view.tileScheme === 'xyz' && reading.basemap === null) {
  failures.push('the view publishes tile_scheme "xyz" and the map is carrying no basemap');
}
if (view.tileScheme === null && reading.basemap !== null) {
  failures.push(
    `the view publishes no tile scheme and the map drew a basemap anyway (${reading.basemap}) — ` +
      'an aligned frame is not an addressed one'
  );
}

await page.screenshot({path: join(shots, `${name}-world.png`)});

// Western Europe: a coastline is where a one-cell offset between the basemap and the marks shows,
// and the whole-world shot cannot show it. In the unit square whatever the projection, so the same
// numbers frame the same ground under either — a lower box under equirectangular, the two
// projections placing 50°N differently, which is itself the thing being drawn.
await page.evaluate((box) => {
  /** @type {any} */ (document.querySelector('tessera-explorer')).map.fitBbox(box);
}, view.projection === 'web_mercator' ? [0.47, 0.31, 0.52, 0.35] : [0.47, 0.22, 0.52, 0.26]);
await page.waitForTimeout(6_000);
await page.screenshot({path: join(shots, `${name}-europe.png`)});

const report = {
  url,
  dataset: args.dataset ?? null,
  headed: 'headed' in args,
  view: reading.view,
  quantisation: reading.quantisation,
  basemapLayer: reading.basemap,
  depth: reading.depth,
  served: reading.served,
  visible: reading.visible,
  places,
  consoleErrors: consoleErrors.filter((t) => !isSupersededAbort(t)),
  failures
};
await writeFile(join(shots, `${name}.json`), JSON.stringify(report, null, 2) + '\n');
console.log(JSON.stringify(report, null, 2));
await browser.close();
if (failures.length > 0) {
  console.error(`\nFAILED:\n  ${failures.join('\n  ')}`);
  process.exit(1);
}
console.log('\nok');
