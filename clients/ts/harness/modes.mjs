#!/usr/bin/env node
// The mode toolbar under real pointer input (design client-components §5.3, §5.11): pan → box →
// drag → pan → lasso → draw → pan, with the mouse driven through Playwright's input pipeline at a
// human pace — a glide of small moves between every press, the hover events a hand produces.
// The synthetic path (a handful of moves, no hover between gestures) passed while the toolbar was
// broken: deck's input layer saw a selection's `pointerdown` and never its `pointerup`, its
// session stayed pressed, and the next glide — button up — panned the camera, after which a real
// pan drag did nothing. So every transition here asserts three things: the `mode` attribute, the
// highlight (the map's live drag state while the button is down, and the store's region after
// it), and the counting request on the wire; and between gestures, that a glide with the button
// up moves nothing.
//
//   node clients/ts/harness/modes.mjs [--url http://localhost:5173/?dataset=2m4] [--executable /path/to/chrome] [--headless]
//
// Headed by default: the bug is in the input layer, and the display is where it shows.
import {chromium} from 'playwright';

const args = Object.fromEntries(process.argv.slice(2).reduce((acc, a, i, all) => (a.startsWith('--') ? [...acc, [a.slice(2), all[i + 1]]] : acc), []));
const url = args.url ?? 'http://localhost:5173/?prefetch=0&dataset=2m4';
const headless = 'headless' in args;
const executablePath = args.executable;

// **A page served from another port.** The demo enumerates one browser origin in
// `serve.dev_cors_origins` — `http://localhost:5173` — so a viewer run on another port cannot
// reach the server at all, and every claim below reads as a detached store rather than as a
// configuration. Where the URL is not that origin the *browser's* origin check is switched off
// rather than the server's: this is a measuring browser, and a running demo is not touched.
const sameOrigin = new URL(url).port === '5173';
const originFlags = sameOrigin ? [] : ['--disable-web-security'];
const browser = await chromium.launch(
  headless
    ? {args: ['--use-gl=swiftshader', '--enable-unsafe-swiftshader', '--disable-gpu-sandbox', ...originFlags], ...(executablePath ? {executablePath} : {})}
    : {headless: false, args: ['--disable-gpu-sandbox', ...originFlags], ...(executablePath ? {executablePath} : {})}
);
const page = await browser.newPage({viewport: {width: 1440, height: 900}});
const consoleErrors = [];
page.on('pageerror', (e) => consoleErrors.push(`pageerror: ${e.message}`));

/**
 * Every viewport request carrying a `region` leaf — a box or a lasso settled is a *filter*
 * (`selection-operand.md` §8: the shape rides the request the client was sending anyway), so
 * what is counted here is the kind of shape each request carried, not a counting request of its
 * own.
 */
const regionRequests = [];
/** The `region` leaf anywhere in a filter expression, or null. */
const regionOf = (expr) => {
  if (!expr || typeof expr !== 'object') return null;
  if (expr.region) return Object.keys(expr.region).find((k) => k !== 'space') ?? null;
  for (const key of ['all_of', 'any_of', 'none_of']) {
    for (const kid of expr[key] ?? []) {
      const found = regionOf(kid);
      if (found) return found;
    }
  }
  return null;
};
page.on('request', (r) => {
  if (!r.url().includes('/v1/viewport')) return;
  try {
    const body = JSON.parse(r.postData() ?? '{}');
    const kind = regionOf(body.filters);
    if (kind && body.k !== 0) regionRequests.push(kind);
  } catch {
    // Not a request this test reads.
  }
});

const failures = [];
const passes = [];
const check = (claim, ok, evidence) => {
  (ok ? passes : failures).push(`${claim} — ${evidence}`);
  console.log(`  ${ok ? 'ok  ' : 'FAIL'} ${claim} — ${evidence}`);
};

await page.goto(url, {waitUntil: 'load'});
const map = page.locator('tessera-map').first();
/** The map's live state: the mode, the highlight while dragging, the region after, the camera. */
const state = () =>
  map.evaluate((el) => {
    const m = /** @type {any} */ (el);
    return {
      mode: el.getAttribute('mode'),
      drag: m.drag ? m.drag.map((v) => Math.round(v)) : null,
      polygon: m.dragPolygon ? m.dragPolygon.length : null,
      region: m.activeStore?.get('region')?.shape.kind ?? null,
      target: m.viewState.target.slice(0, 2).map((v) => Math.round(v * 10) / 10)
    };
  });
const button = (label) => map.locator(`[part="controls"] button[aria-label="${label}"]`).first();
const cursor = {x: 100, y: 100};
/** A human-paced move: twenty-five small steps with hover events between, over `ms`. */
const glide = async (x, y, ms = 400) => {
  const n = 25;
  for (let i = 1; i <= n; i++) {
    await page.mouse.move(cursor.x + ((x - cursor.x) * i) / n, cursor.y + ((y - cursor.y) * i) / n);
    await page.waitForTimeout(ms / n);
  }
  cursor.x = x;
  cursor.y = y;
};
const press = async (label) => {
  const box = await button(label).boundingBox();
  await glide(box.x + box.width / 2, box.y + box.height / 2, 500);
  await page.mouse.down();
  await page.waitForTimeout(80);
  await page.mouse.up();
  await page.waitForTimeout(200);
};
const drag = async (path, ms = 400) => {
  await glide(path[0][0], path[0][1], 500);
  await page.mouse.down();
  await page.waitForTimeout(100);
  const mid = [];
  for (let i = 1; i < path.length; i++) {
    await glide(path[i][0], path[i][1], ms);
    mid.push(await state());
  }
  await page.waitForTimeout(150);
  await page.mouse.up();
  await page.waitForTimeout(900);
  return mid;
};
const same = (a, b) => a[0] === b[0] && a[1] === b[1];

// Let the first marks land: the gestures below are against a drawn map.
{
  const started = Date.now();
  while (Date.now() - started < 60_000) {
    if ((await page.evaluate(() => window.__tesseraProbeOf?.()?.marks ?? window.__tesseraProbe?.marks ?? 0)) > 0) break;
    await page.waitForTimeout(200);
  }
  await page.waitForTimeout(1500);
}

console.log('--- transitions ---');
let s = await state();
check('opens in pan', s.mode === 'pan' && s.drag === null, `mode=${s.mode}`);

// 1. pan: a drag moves the camera; a glide with the button up does not.
const before = s.target;
await drag([[700, 300], [600, 250]]);
s = await state();
check('pan: a drag moves the camera', !same(before, s.target), `target ${before} → ${s.target}`);
const still = s.target;
await glide(900, 500, 500);
s = await state();
check('pan: a glide with the button up moves nothing', same(still, s.target), `target ${still} → ${s.target}`);

// 2. box: the button, the attribute, the highlight while dragging, the region and the leaf on the request after.
await press('Box select');
s = await state();
check('box: the button sets the mode', s.mode === 'box', `mode=${s.mode}`);
let asked = regionRequests.length;
let mid = await drag([[700, 300], [760, 340], [900, 500]]);
s = await state();
check('box: the highlight follows the drag', mid[0].drag !== null && mid[1].drag !== null && mid[1].drag?.[2] > mid[0].drag?.[2], `drag ${JSON.stringify(mid[0].drag)} → ${JSON.stringify(mid[1].drag)}`);
check('box: release filters to the region', s.drag === null && s.region === 'box' && regionRequests.length > asked && regionRequests[regionRequests.length - 1] === 'bbox', `region=${s.region}, ${regionRequests.length - asked} request(s) carrying a ${regionRequests[regionRequests.length - 1] ?? '?'} leaf`);
check('box: the camera did not move', same(still, s.target), `target ${still} → ${s.target}`);

// 3. back to pan: the glide to the button moves nothing; then a drag moves the camera.
const beforePan = s.target;
await press('Pan');
s = await state();
check('pan again: the button sets the mode and the glide to it moved nothing', s.mode === 'pan' && same(beforePan, s.target), `mode=${s.mode}, target ${beforePan} → ${s.target}`);
await drag([[700, 300], [600, 250]]);
s = await state();
check('pan again: a drag moves the camera', !same(beforePan, s.target), `target ${beforePan} → ${s.target}`);
const afterPan = s.target;

// 4. lasso: the polygon grows while drawing; release counts it.
await press('Lasso select');
s = await state();
check('lasso: the button sets the mode', s.mode === 'lasso', `mode=${s.mode}`);
asked = regionRequests.length;
mid = await drag([[820, 420], [900, 440], [880, 520], [800, 500]], 300);
s = await state();
check('lasso: the polygon grows while drawing', (mid[0].polygon ?? 0) > 1 && (mid[2].polygon ?? 0) > (mid[0].polygon ?? 0), `${mid.map((m) => m.polygon).join(' → ')} vertices`);
check('lasso: release filters to the region', s.polygon === null && s.region === 'lasso' && regionRequests.length > asked && regionRequests[regionRequests.length - 1] === 'polygon', `region=${s.region}, ${regionRequests.length - asked} request(s) carrying a ${regionRequests[regionRequests.length - 1] ?? '?'} leaf`);
check('lasso: the camera did not move', same(afterPan, s.target), `target ${afterPan} → ${s.target}`);

// 5. pan once more.
await press('Pan');
s = await state();
check('pan a third time: the glide to the button moved nothing', s.mode === 'pan' && same(afterPan, s.target), `target ${afterPan} → ${s.target}`);
await drag([[700, 300], [600, 250]]);
s = await state();
check('pan a third time: a drag moves the camera', !same(afterPan, s.target), `target ${afterPan} → ${s.target}`);

await browser.close();
if (consoleErrors.length) failures.push(...consoleErrors);
if (failures.length) {
  console.error(`MODES FAILED (${passes.length} ok, ${failures.length} failed):\n  ${failures.join('\n  ')}`);
  process.exit(1);
}
console.log(`MODES OK — ${passes.length} claims hold`);
