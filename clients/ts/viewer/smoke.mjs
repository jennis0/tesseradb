#!/usr/bin/env node
// Drive the viewer in a headless browser and report what it did.
//
// It checks that the viewer authorised, fetched tiles and drew marks, and writes a screenshot. It
// is not part of the test suites.
//
//   node clients/ts/viewer/smoke.mjs [--url http://localhost:5173] [--shot /tmp/viewer.png]
//     [--headed] [--executable /path/to/chrome]
//
// Requires a running `tessera serve` and a running `vite dev`. `--headed` is what a corpus of a
// few million points needs: see `smoke-browser.mjs`.
import {flags, isSupersededAbort, launchBrowser, withParams} from './smoke-browser.mjs';

const args = flags();
const url = args.url ?? 'http://localhost:5173';
const shot = args.shot ?? '/tmp/tessera-viewer.png';
const settleMs = Number(args.settle ?? 6000);

const browser = await launchBrowser(args);
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
  // The artifact channel asks on its own route, so the point path's requests are `/v1/viewport`'s.
  const artifacts = u.pathname === '/v1/artifacts/viewport';
  requests.push({path: u.pathname, status: r.status(), artifacts});
});

await page.goto(url, {waitUntil: 'load'});
await page.waitForTimeout(settleMs);

/**
 * Wait until the probe's mark count stops changing. A broad principal on a large bundle streams
 * bands for tens of seconds, and a count read mid-load would make the colour check misreport.
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

/** Pixels on the canvas that are not the page background. The canvas is in a shadow root, so the locator finds it. */
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
 * The three counts, read through the status strip's parts. `shown` renders its figure with the
 * total on `data-total`; the other two are one figure each. A count that rendered nothing reads
 * as null.
 */
const counts = async () => {
  const text = async (part) => {
    const el = page.locator(`tessera-status [part="${part}"] [part="count"]`).first();
    if ((await el.count()) === 0) return '';
    return (await el.textContent()) ?? '';
  };
  const shownEl = page.locator('tessera-status [part="count-shown"] [part="count"]').first();
  const served = (await text('count-shown')).trim();
  const visible = (await shownEl.count()) > 0 ? await shownEl.getAttribute('data-total') : null;
  const matched = (await text('count-matched')).trim();
  return served && visible ? {served, visible, matched} : null;
};

// A different principal must produce a different picture and different masked counts.
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

// Zooming in must add marks and not remove them.
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

// Switching the encoding must repaint without changing the mark count or issuing a
// `/v1/viewport` request, since every declared column is already held. Look-ahead is off for this
// section only: it sends requests while the view is still, which is when the switch is measured.
await page.goto(withParams(url, {prefetch: 0}), {waitUntil: 'load'});
await page.waitForTimeout(settleMs);
await settled();

const colourSeries = [];
const colourOptions = await page.locator('#colour-by option').count().catch(() => 0);
for (let i = 0; i < colourOptions; i++) {
  const value = await page.locator('#colour-by option').nth(i).getAttribute('value');
  const viewportsBefore = requests.filter((r) => r.path === '/v1/viewport').length;
  await page.selectOption('#colour-by', value);
  await settled();
  colourSeries.push({
    column: value === '' ? '(uniform)' : value,
    counts: await counts(),
    lit: await litPixels(),
    viewportRequests:
      requests.filter((r) => r.path === '/v1/viewport').length - viewportsBefore,
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

// The text of the instruments and the explorer's panels, read through shadow roots by the locator.
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

// The strip's final state, through its part.
const stripState = await page.locator('tessera-status [part="state"]').first().getAttribute('data-state').catch((e) => `error: ${e.message.slice(0, 120)}`);

// A minute: a screenshot waits for a frame, and a software rasteriser draws about 10^6 marks slowly.
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
// `/v1/categories` answers 200 for `derived` and `public` columns alike, so a 500 is a fault and
// every console error counts. The 500s are listed separately, to say which kind of failure it was.
const categoryFaults = requests.filter(
  (r) => r.path.startsWith('/v1/categories/') && r.status === 500
);
const unexplained = consoleErrors.filter((e) => !isSupersededAbort(e));
console.log('--- category route faults (none expected) ---');
console.log(
  categoryFaults.length
    ? categoryFaults.map((r) => `  ${r.status} ${r.path}`).join('\n')
    : '  none'
);
console.log('--- console errors (a superseded request’s abort excepted) ---');
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
// If either of these changes, the encoding changed what is drawn.
const servedAcrossEncodings = new Set(colourSeries.map((c) => c.counts?.served));
if (servedAcrossEncodings.size > 1) failures.push('changing the colour column changed the mark count');
if (colourSeries.some((c) => c.viewportRequests > 0)) failures.push('a colour change refetched');

if (failures.length) {
  console.error(`SMOKE FAILED: ${failures.join('; ')}`);
  process.exit(1);
}
console.log('SMOKE OK');
