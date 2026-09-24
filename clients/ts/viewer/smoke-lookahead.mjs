#!/usr/bin/env node
// Whether look-ahead removes the wait when panning at high zoom.
//
//   node clients/ts/viewer/smoke-lookahead.mjs [--no-prefetch] [--url http://localhost:5173]
//     [--headed] [--executable /path/to/chrome]
//
// Measured on the 2.4M demo corpus, six consecutive same-direction pans at depth 10:
//   look-ahead off: 9 requests, 0 of 6 pans free, 14,382 of 16,524 tiles from cache
//   look-ahead on : 4 requests, 3 of 6 pans free, 16,524 of 16,524 tiles from cache
//
// Requires a running `tessera serve` and `vite dev`.
//
// The measurement is the fraction of pans answered entirely from held bands, with no request.
// The script pauses between pans, since look-ahead runs only while the view is still.
import {flags, isSupersededAbort, launchBrowser, withParams} from './smoke-browser.mjs';

const args = flags();

const browser = await launchBrowser(args);
const page = await browser.newPage({viewport: {width: 1280, height: 800}});

let requests = 0;
const errors = [];
page.on('console', (m) => m.type() === 'error' && errors.push(m.text()));
page.on('pageerror', (e) => errors.push(`pageerror: ${e.message}`));
let serverUs = 0;
let bytes = 0;
page.on('response', (r) => {
  if (new URL(r.url()).pathname === '/v1/viewport') {
    requests++;
    serverUs += Number(r.headers()['x-tessera-server-us'] ?? 0);
    bytes += Number(r.headers()['content-length'] ?? 0);
  }
});

const base = args.url ?? 'http://localhost:5173';
const url = 'no-prefetch' in args ? withParams(base, {prefetch: 0}) : base;
console.log(`driving ${url}`);
await page.goto(url, {waitUntil: 'load'});
await page.waitForTimeout(6000);

const options = await page.locator('#principal option').count();
await page.selectOption('#principal', String(options - 1));
await page.waitForTimeout(6000);

// Zoom in, so the viewport is smaller than the world and a pan means something.
for (let i = 0; i < 5; i++) {
  await page.mouse.move(450, 400);
  await page.mouse.wheel(0, -240);
  await page.waitForTimeout(900);
}
await page.waitForTimeout(4000);

const drag = async (dx, pause) => {
  await page.mouse.move(450, 400);
  await page.mouse.down();
  for (let i = 1; i <= 10; i++) await page.mouse.move(450 + (dx * i) / 10, 400);
  await page.mouse.up();
  await page.waitForTimeout(pause);
};

// Settle the depth budget first: bands are keyed by depth, so a moving `m_target` lands every
// view where nothing is held.
for (let i = 0; i < 4; i++) {
  await drag(-200, 2500);
  await drag(200, 2500);
}

// A run of pans in one direction, as when crossing a map.
const before = requests;
const beforeUs = serverUs;
const beforeBytes = bytes;
const perPan = [];
for (let i = 0; i < 6; i++) {
  const at = requests;
  await drag(-260, 2500);
  perPan.push(requests - at);
}
const total = requests - before;

const stats = await page.evaluate(() => {
  const text = document.getElementById('instruments')?.innerText ?? '';
  const g = (l) => text.match(new RegExp(`${l}\\s*\\n\\s*([\\d,.]+( of [\\d,]+)?)`))?.[1] ?? '?';
  return {cache: g('tiles from cache'), prefetched: g('prefetched ahead'), held: g('replica held')};
});

const free = perPan.filter((n) => n === 0).length;
console.log(`per-pan viewport requests: [${perPan.join(', ')}]`);
console.log(`${free} of ${perPan.length} pans needed no request at all (${total} requests total)`);
console.log(`server CPU over those pans: ${((serverUs - beforeUs) / 1000).toFixed(1)} ms; wire ${(((bytes - beforeBytes)) / 1e6).toFixed(2)} MB`);
console.log(`tiles from cache ${stats.cache}, prefetched ahead ${stats.prefetched}, replica ${stats.held}MB`);
// Superseded aborts are reported but not counted as failures, since the script moves the view.
const unexplained = errors.filter((e) => !isSupersededAbort(e));
const aborts = errors.length - unexplained.length;
console.log(
  `console errors: ${unexplained.length ? unexplained.join(' | ') : 'none'}` +
    `${aborts ? ` (and ${aborts} superseded request(s) aborted)` : ''}`
);

await browser.close();
