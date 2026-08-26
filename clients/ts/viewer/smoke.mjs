#!/usr/bin/env node
// Drive the viewer in a headless browser and report what it actually did.
//
// Not a test suite — the instrument's assertion is the owner looking at it. This exists so that
// "it builds" can be upgraded to "it authorised, fetched tiles, and drew marks" without a human
// in the loop, and so a screenshot lands somewhere reviewable.
//
//   node clients/ts/viewer/smoke.mjs [--url http://localhost:5173] [--shot /tmp/viewer.png]
//
// Requires a running `tessera serve` and a running `vite dev`.
import {chromium} from 'playwright';

const args = Object.fromEntries(
  process.argv
    .slice(2)
    .reduce((acc, a, i, all) => (a.startsWith('--') ? [...acc, [a.slice(2), all[i + 1]]] : acc), [])
);
const url = args.url ?? 'http://localhost:5173';
const shot = args.shot ?? '/tmp/tessera-viewer.png';
const settleMs = Number(args.settle ?? 6000);

const browser = await chromium.launch({
  args: ['--use-gl=swiftshader', '--enable-unsafe-swiftshader', '--disable-gpu-sandbox']
});
const page = await browser.newPage({viewport: {width: 1280, height: 800}});

const consoleErrors = [];
const requests = [];
page.on('console', (m) => {
  if (m.type() === 'error') consoleErrors.push(m.text());
});
page.on('pageerror', (e) => consoleErrors.push(`pageerror: ${e.message}`));
page.on('response', (r) => {
  const u = new URL(r.url());
  if (!u.pathname.startsWith('/v1/') && !u.pathname.startsWith('/session/')) return;
  // **Which channel asked**, because the viewer has two that both post to `/v1/viewport`: the
  // point path, and the annotation channel that asks for artifacts alone (`k = 0`, a named layer).
  // Counting them together made a layer's own request read as a colour change refetching marks.
  let artifacts = false;
  try {
    const body = JSON.parse(r.request().postData() ?? '{}');
    // The channel's ask: counts only, a named layer. The point path names the layers too (§5.10)
    // but always asks for points.
    artifacts = body.k === 0 && Array.isArray(body.layers) && body.layers.length > 0;
  } catch {
    // A GET, or a body that is not JSON. Neither is the artifact channel.
  }
  requests.push({path: u.pathname, status: r.status(), artifacts});
});

await page.goto(url, {waitUntil: 'load'});
await page.waitForTimeout(settleMs);

/**
 * Wait until the picture stops changing, rather than for a fixed interval.
 *
 * **Every figure this script compares is only meaningful once the load has settled.** A broad
 * principal on a large bundle streams bands for tens of seconds, so a fixed wait samples a mark
 * count that is still climbing — and the colour check then reads a load in progress as "the
 * encoding changed the selection", which is a false report of the one property it exists to
 * protect. `__tesseraProbe.marks` is published for exactly this.
 */
const settled = async (limitMs = 45_000) => {
  const started = Date.now();
  let last = -1;
  let stable = 0;
  while (Date.now() - started < limitMs) {
    await page.waitForTimeout(500);
    const marks = await page.evaluate(() => window.__tesseraProbe?.marks ?? -1);
    if (marks === last) {
      if (++stable >= 4) return true;
    } else {
      stable = 0;
      last = marks;
    }
  }
  return false;
};
await settled();

/** Pixels on the canvas that are not the page background — "did it draw anything". */
// The canvas is inside the map's shadow root: found by the locator, which pierces, and read
// through its handle — `document.querySelector` would report no canvas at all.
const litPixels = () =>
  page.locator('canvas').first().evaluate((canvas) => {
    if (!(canvas instanceof HTMLCanvasElement)) return 0;
    const off = document.createElement('canvas');
    off.width = canvas.width;
    off.height = canvas.height;
    const ctx = off.getContext('2d');
    ctx.drawImage(canvas, 0, 0);
    const {data} = ctx.getImageData(0, 0, off.width, off.height);
    let lit = 0;
    for (let i = 0; i < data.length; i += 4) {
      if (data[i] > 24 || data[i + 1] > 26 || data[i + 2] > 30) lit += 1;
    }
    return lit;
  });

/**
 * The three counts, read through the status strip's parts — shadow-piercing locators, never an
 * id in a panel's markup (design §9). `shown` renders its figure with the total on `data-total`;
 * the other two are one figure each. A count that rendered nothing (stale, inexact, not shown) reads as null.
 */
const counts = async () => {
  const text = async (part) => {
    const el = page.locator(`tessera-status [part="${part}"] [part="count"]`).first();
    if ((await el.count()) === 0) return '';
    return (await el.textContent()) ?? '';
  };
  // The shown cell renders its figure with the total on `data-total` (the visible cell's number).
  const shownEl = page.locator('tessera-status [part="count-shown"] [part="count"]').first();
  const served = (await text('count-shown')).trim();
  const visible = (await shownEl.count()) > 0 ? await shownEl.getAttribute('data-total') : null;
  const matched = (await text('count-matched')).trim();
  return served && visible ? {served, visible, matched} : null;
};

// Success criterion 3, checked rather than asserted by eye: a different principal must produce a
// different picture and different masked counts.
const principals = [];
const options = await page.locator('#principal option').count().catch(() => 0);
for (let i = 0; i < options; i++) {
  await page.selectOption('#principal', String(i));
  await settled();
  principals.push({
    label: (await page.locator('#principal option').nth(i).innerText()).trim(),
    counts: await counts(),
    lit: await litPixels()
  });
}

// Success criterion 1, partially: zooming in must add marks, never remove them (§7.2 nesting).
const zoomSeries = [];
if (options > 0) {
  await page.selectOption('#principal', String(options - 1));
  await settled();
  for (const step of [0, 1, 2, 3]) {
    if (step > 0) {
      await page.mouse.move(640, 400);
      await page.mouse.wheel(0, -400);
      await settled();
    }
    zoomSeries.push({step, counts: await counts(), lit: await litPixels()});
  }
}

// **Colour is presentation, not selection.** Switching the encoding must repaint the canvas and
// must NOT change the mark count — and must issue no `/v1/viewport` at all, since every declared
// column is already in the held response. The count is the I7 property, observable from outside;
// the request count is what proves the switch is a layer rebuild rather than a refetch.
// **Look-ahead off for this section, and only this one.** The ring issues its requests while the
// view is *still*, which is exactly when a colour switch is measured — so with it on, a request
// nobody made lands inside the window and reads as the encoding refetching. The knob is the same
// A/B the cache measurements use; the principal and zoom sections above ran with it on.
await page.goto(`${url}${url.includes('?') ? '&' : '?'}prefetch=0`, {waitUntil: 'load'});
await page.waitForTimeout(settleMs);
await settled();

const colourSeries = [];
const colourOptions = await page.locator('#colour-by option').count().catch(() => 0);
for (let i = 0; i < colourOptions; i++) {
  const value = await page.locator('#colour-by option').nth(i).getAttribute('value');
  const viewportsBefore = requests.filter((r) => r.path === '/v1/viewport' && !r.artifacts).length;
  await page.selectOption('#colour-by', value);
  await settled();
  colourSeries.push({
    column: value === '' ? '(uniform)' : value,
    counts: await counts(),
    lit: await litPixels(),
    viewportRequests:
      requests.filter((r) => r.path === '/v1/viewport' && !r.artifacts).length - viewportsBefore,
    legend: (await page.locator('#instruments').innerText())
      .split('\n')
      .filter((l) => l.trim())
      .slice(-3)
      .join(' | ')
      .slice(0, 110)
  });
}
// Leave a category encoding on for the screenshot: a uniform map proves nothing about colour.
const categoryOption = colourSeries.find((c) => c.column === 'primary_category');
if (categoryOption) {
  await page.selectOption('#colour-by', 'primary_category');
  await settled();
}

// Both columns: what you change is on the left, what came back is on the right, and a report
// that read only one of them would omit half of what the run did.
// The instruments and the explorer's panels alike: what you change and what came back, read
// through shadow roots by the locator rather than by an id the shadow DOM hides.
const panelText = await Promise.all([
  page.locator('#instruments').innerText(),
  page.locator('tessera-status').first().innerText(),
  page.locator('tessera-filter-panel').first().innerText()
])
  .then((parts) => parts.filter(Boolean).join('\n'))
  .catch(() => '(no panels)');
const canvasPixels = await page.locator('canvas').first().evaluate((canvas) => {
  if (!(canvas instanceof HTMLCanvasElement)) return {found: false};
  // Read back the framebuffer via a 2D copy: how many pixels are not the page background?
  const off = document.createElement('canvas');
  off.width = canvas.width;
  off.height = canvas.height;
  const ctx = off.getContext('2d');
  ctx.drawImage(canvas, 0, 0);
  const {data} = ctx.getImageData(0, 0, off.width, off.height);
  let lit = 0;
  for (let i = 0; i < data.length; i += 4) {
    if (data[i] > 24 || data[i + 1] > 26 || data[i + 2] > 30) lit += 1;
  }
  return {found: true, width: off.width, height: off.height, lit, total: data.length / 4};
});

// The strip's state, read before the browser goes: the smoke's last word on whether the page
// ended `shown`, through the part, which is the only way the shadow DOM offers.
const stripState = await page.locator('tessera-status [part="state"]').first().getAttribute('data-state').catch((e) => `error: ${e.message.slice(0, 120)}`);

// A minute, not the default thirty seconds: a screenshot waits for a frame, and this page draws
// ~10^6 marks under a software rasteriser in CI-like conditions.
await page.screenshot({path: shot, timeout: 60_000});
await browser.close();

const byPath = requests.reduce((acc, r) => {
  const key = `${r.path}${r.artifacts ? ' (artifacts)' : ''} ${r.status}`;
  acc[key] = (acc[key] ?? 0) + 1;
  return acc;
}, {});

console.log('--- requests ---');
for (const [key, count] of Object.entries(byPath))
  console.log(`  ${count.toString().padStart(4)}x ${key}`);
console.log('--- principals (does the mask change the picture?) ---');
for (const p of principals) {
  console.log(
    `  ${p.label.padEnd(26)} served=${p.counts?.served ?? '?'} visible=${
      p.counts?.visible ?? '?'
    } litPixels=${p.lit}`
  );
}
console.log('--- zoom series (do marks accumulate?) ---');
for (const z of zoomSeries) {
  console.log(
    `  step ${z.step}: served=${z.counts?.served ?? '?'} visible=${
      z.counts?.visible ?? '?'
    } litPixels=${z.lit}`
  );
}
console.log('--- colour encodings (presentation, never selection) ---');
for (const c of colourSeries) {
  console.log(
    `  ${c.column.padEnd(20)} served=${c.counts?.served ?? '?'} lit=${String(c.lit).padStart(6)} ` +
      `viewportReqs=${c.viewportRequests}  ${c.legend}`
  );
}
{
  const served = new Set(colourSeries.map((c) => c.counts?.served));
  const refetched = colourSeries.filter((c) => c.viewportRequests > 0).map((c) => c.column);
  console.log(
    `  => served counts across encodings: ${[...served].join(', ')} ` +
      `${served.size === 1 ? '(unchanged — I7 holds)' : '*** CHANGED: colour altered selection ***'}`
  );
  console.log(
    `  => encodings that refetched: ${refetched.length ? refetched.join(', ') : 'none (layer rebuild only)'}`
  );
}
console.log('--- panels ---');
console.log(panelText.split('\n').map((l) => `  ${l}`).join('\n'));
console.log('--- the status strip’s state ---');
console.log(`  data-state=${stripState}`);
console.log('--- canvas ---');
console.log(' ', JSON.stringify(canvasPixels));
// **`/v1/categories` no longer refuses a `derived` column, so a 500 here is breakage.** This block
// used to tolerate them, and to excuse the browser's "Failed to load resource" beside them, on the
// grounds that the visibility predicate was ⊘ unbuilt and serving the set empty would be
// indistinguishable from a computed empty answer. The predicate is built: a `derived` vocabulary's
// values are derived from inside `M_auth` per request (`Engine::categories`), and the route answers
// 200 for a `derived` column and a `public` one alike. What a 500 means now is that the column's
// membership could not be read at all — a real fault with nothing contractual behind it.
//
// So there is nothing left to excuse, and every console error counts. The 500s are still listed
// separately, because a run that fails wants to say *which* of the two it was.
const categoryFaults = requests.filter(
  (r) => r.path.startsWith('/v1/categories/') && r.status === 500
);
const unexplained = consoleErrors;
console.log('--- category route faults (none expected) ---');
console.log(
  categoryFaults.length
    ? categoryFaults.map((r) => `  ${r.status} ${r.path}`).join('\n')
    : '  none'
);
console.log('--- console errors ---');
console.log(consoleErrors.length ? consoleErrors.map((e) => `  ${e}`).join('\n') : '  none');
console.log(`--- screenshot: ${shot}`);

const viewportOk = requests.some((r) => r.path === '/v1/viewport' && r.status === 200);
const drewSomething = canvasPixels.found && canvasPixels.lit > 0;
// Distinct principals must report distinct visible sets, or the instrument cannot show masking.
const distinctVisible = new Set(principals.map((p) => p.counts?.visible ?? '?'));
const maskingVisible = principals.length < 2 || distinctVisible.size > 1;

const failures = [];
if (!viewportOk) failures.push('no successful /v1/viewport');
if (!drewSomething) failures.push('nothing drawn on the canvas');
if (stripState !== 'shown') failures.push(`the status strip ended in ${stripState}, not shown`);
if (!maskingVisible) failures.push('every principal reported the same visible count');
if (categoryFaults.length)
  failures.push(`${categoryFaults.length} 500(s) from /v1/categories — the derived predicate is built, so this is a fault`);
if (unexplained.length) failures.push(`${unexplained.length} console error(s)`);
// Colour is presentation. If either of these moves, the encoding has become a selection rule.
const servedAcrossEncodings = new Set(colourSeries.map((c) => c.counts?.served));
if (servedAcrossEncodings.size > 1) failures.push('changing the colour column changed the mark count');
if (colourSeries.some((c) => c.viewportRequests > 0)) failures.push('a colour change refetched');

if (failures.length) {
  console.error(`SMOKE FAILED: ${failures.join('; ')}`);
  process.exit(1);
}
console.log('SMOKE OK');
