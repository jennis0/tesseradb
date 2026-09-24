#!/usr/bin/env node
// Whether the replica makes a revisit free: zoom in, pan away, pan back, count requests.
//
//   node clients/ts/viewer/smoke-cache.mjs [--url http://localhost:5173]
//     [--headed] [--executable /path/to/chrome]
//
// Requires a running `tessera serve` and `vite dev`.
//
//  - It zooms in first: at zoom 0 the world bbox clamps, so a pan changes nothing.
//  - It lets the depth budget settle: bands are keyed by depth, so while `mTarget` moves every
//    view lands at a depth nothing is held at.
import {flags, isSupersededAbort, launchBrowser} from './smoke-browser.mjs';

const args = flags();
const url = args.url ?? 'http://localhost:5173';

const browser = await launchBrowser(args);
const page = await browser.newPage({viewport: {width: 1280, height: 800}});

let requests = 0;
const errors = [];
page.on('console', (m) => m.type() === 'error' && errors.push(m.text()));
page.on('pageerror', (e) => errors.push(`pageerror: ${e.message}`));
// Counts-only requests are counted apart: a client panning from its replica still revalidates
// on a timer with a `k = 0` request that fetches no marks, which is not a cache miss.
let revalidations = 0;
page.on('request', (r) => {
  if (new URL(r.url()).pathname !== '/v1/viewport') return;
  try {
    if (JSON.parse(r.postData() ?? '{}').k === 0) revalidations++;
    else requests++;
  } catch {
    requests++;
  }
});

const snap = async (tag) => {
  const t = await page.evaluate(() => {
    const text = document.getElementById('instruments')?.innerText ?? '';
    const g = (l) => text.match(new RegExp(`${l}\\s*\\n\\s*([\\d,.]+)`))?.[1] ?? '?';
    const depth = text.match(/depth\s*\n\s*(\d+)/)?.[1] ?? '?';
    const tiles = g('tiles in view');
    const lim = text.match(/limited by\s*\n\s*(\w+)/)?.[1] ?? '?';
    const mt = text.match(/m_target \(calibrated\)\s*\n\s*([\d.]+)/)?.[1] ?? '?';
    const cache = text.match(/tiles from cache\s*\n\s*([\d]+ of [\d]+)/)?.[1] ?? '?';
    return {drawn: g('marks drawn'), prov: g('of which provisional'), held: g('replica held'), depth, tiles, lim, mt, cache};
  });
  console.log(
    `  [${tag.padEnd(10)}] requests=${String(requests).padStart(2)} drawn=${t.drawn.padStart(9)} ` +
      `provisional=${t.prov.padStart(7)} replica=${t.held}MB tiles=${t.tiles} lim=${t.lim} mTarget=${t.mt} fromCache=${t.cache}`
  );
  return requests;
};

await page.goto(url, {waitUntil: 'load'});
await page.waitForTimeout(6000);

// The broadest principal, so the budget binds.
const options = await page.locator('#principal option').count();
await page.selectOption('#principal', String(options - 1));
await page.waitForTimeout(6000);

// Zoom in so the viewport is smaller than the world and a pan means something.
for (let i = 0; i < 5; i++) {
  await page.mouse.move(450, 400);
  await page.mouse.wheel(0, -240);
  await page.waitForTimeout(900);
}
await page.waitForTimeout(4000);
await snap('zoomed in');

const drag = async (dx) => {
  await page.mouse.move(450, 400);
  await page.mouse.down();
  for (let i = 1; i <= 10; i++) await page.mouse.move(450 + (dx * i) / 10, 400);
  await page.mouse.up();
  await page.waitForTimeout(4000);
};

// Let the depth budget converge first.
for (let i = 0; i < 6; i++) {
  await drag(-300);
  await drag(300);
}
const base = await snap('settled');

await drag(-600);
const away = await snap('panned out');

await drag(600);
const back = await snap('panned back');

console.log('');
// Only the revisit is asserted. What panning out costs depends on how far look-ahead reached,
// and panning beyond it changes the density and so the depth chosen.
console.log(`panning into new territory cost : ${away - base} request(s)  (0 means the ring had it)`);
console.log(`panning back to what we held cost: ${back - away} request(s)  (expect 0)`);
console.log(`counts-only revalidations over the run: ${revalidations}`);
// Superseded aborts are not failures, but their number is reported: many of them mean the view
// was moving during the measurement.
const unexplained = errors.filter((e) => !isSupersededAbort(e));
const aborts = errors.length - unexplained.length;
console.log(
  `console errors: ${unexplained.length ? unexplained.join(' | ') : 'none'}` +
    `${aborts ? ` (and ${aborts} superseded request(s) aborted)` : ''}`
);
console.log(back - away === 0 ? 'REVISIT FREE' : 'REVISIT COST A REQUEST');

await browser.close();
