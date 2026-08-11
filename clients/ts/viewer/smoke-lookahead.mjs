#!/usr/bin/env node
// Does the anticipatory ring remove the wait when panning at high zoom?
//
//   node clients/ts/viewer/smoke-lookahead.mjs [--no-prefetch]
//
// Measured on the 2.4M demo corpus, six consecutive same-direction pans at depth 10:
//   look-ahead off: 9 requests, 0 of 6 pans free, 14,382 of 16,524 tiles from cache
//   look-ahead on : 4 requests, 3 of 6 pans free, 16,524 of 16,524 tiles from cache
//
// Requires a running `tessera serve` and `vite dev`.
//
// The measurement is the fraction of a pan that is answered entirely from held bands. A pan that
// issues no request at all is one the user never waited for. Pauses between pans are deliberate:
// the ring only runs when the view is still, which is the whole point — it spends an idle moment
// so the next movement does not have to.
import {chromium} from 'playwright';

const browser = await chromium.launch({
  args: ['--use-gl=swiftshader', '--enable-unsafe-swiftshader', '--disable-gpu-sandbox']
});
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

const url = process.argv.includes('--no-prefetch')
  ? 'http://localhost:5173/?prefetch=0'
  : 'http://localhost:5173';
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

// Settle the depth budget first: bands are keyed by depth, so a moving m_target lands every view
// where nothing is held, and that is not what this measures.
for (let i = 0; i < 4; i++) {
  await drag(-200, 2500);
  await drag(200, 2500);
}

// A run of pans in ONE direction — the case the ring is biased for, and the case a user panning
// across a map actually performs.
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
  const text = document.getElementById('panels')?.innerText ?? '';
  const g = (l) => text.match(new RegExp(`${l}\\s*\\n\\s*([\\d,.]+( of [\\d,]+)?)`))?.[1] ?? '?';
  return {cache: g('tiles from cache'), prefetched: g('prefetched ahead'), held: g('replica held')};
});

const free = perPan.filter((n) => n === 0).length;
console.log(`per-pan viewport requests: [${perPan.join(', ')}]`);
console.log(`${free} of ${perPan.length} pans needed no request at all (${total} requests total)`);
console.log(`server CPU over those pans: ${((serverUs - beforeUs) / 1000).toFixed(1)} ms; wire ${(((bytes - beforeBytes)) / 1e6).toFixed(2)} MB`);
console.log(`tiles from cache ${stats.cache}, prefetched ahead ${stats.prefetched}, replica ${stats.held}MB`);
console.log(`console errors: ${errors.length ? errors.join(' | ') : 'none'}`);

await browser.close();
