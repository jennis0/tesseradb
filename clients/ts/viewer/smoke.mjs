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
  if (u.pathname.startsWith('/v1/') || u.pathname.startsWith('/session/')) {
    requests.push({path: u.pathname, status: r.status()});
  }
});

await page.goto(url, {waitUntil: 'load'});
await page.waitForTimeout(settleMs);

/** Pixels on the canvas that are not the page background — "did it draw anything". */
const litPixels = () =>
  page.evaluate(() => {
    const canvas = document.querySelector('canvas');
    if (!canvas) return 0;
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

const counts = () =>
  page.evaluate(() => {
    const text = document.querySelector('#panels')?.textContent ?? '';
    const m = /([\d,]+) of ([\d,]+) shown/.exec(text);
    return m ? {served: m[1], visible: m[2]} : null;
  });

// Success criterion 3, checked rather than asserted by eye: a different principal must produce a
// different picture and different masked counts.
const principals = [];
const options = await page.locator('#principal option').count().catch(() => 0);
for (let i = 0; i < options; i++) {
  await page.selectOption('#principal', String(i));
  await page.waitForTimeout(2500);
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
  await page.waitForTimeout(2500);
  for (const step of [0, 1, 2, 3]) {
    if (step > 0) {
      await page.mouse.move(640, 400);
      await page.mouse.wheel(0, -400);
      await page.waitForTimeout(2500);
    }
    zoomSeries.push({step, counts: await counts(), lit: await litPixels()});
  }
}

// **Colour is presentation, not selection.** Switching the encoding must repaint the canvas and
// must NOT change the mark count — and must issue no `/v1/viewport` at all, since every declared
// column is already in the held response. The count is the I7 property, observable from outside;
// the request count is what proves the switch is a layer rebuild rather than a refetch.
const colourSeries = [];
const colourOptions = await page.locator('#colour-by option').count().catch(() => 0);
for (let i = 0; i < colourOptions; i++) {
  const value = await page.locator('#colour-by option').nth(i).getAttribute('value');
  const viewportsBefore = requests.filter((r) => r.path === '/v1/viewport').length;
  await page.selectOption('#colour-by', value);
  await page.waitForTimeout(2000);
  colourSeries.push({
    column: value === '' ? '(uniform)' : value,
    counts: await counts(),
    lit: await litPixels(),
    viewportRequests: requests.filter((r) => r.path === '/v1/viewport').length - viewportsBefore,
    legend: (await page.locator('#panels').innerText())
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
  await page.waitForTimeout(2500);
}

const panelText = await page.locator('#panels').innerText().catch(() => '(no panels)');
const canvasPixels = await page.evaluate(() => {
  const canvas = document.querySelector('canvas');
  if (!canvas) return {found: false};
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

await page.screenshot({path: shot});
await browser.close();

const byPath = requests.reduce((acc, r) => {
  const key = `${r.path} ${r.status}`;
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
console.log('--- canvas ---');
console.log(' ', JSON.stringify(canvasPixels));
// A refusal the contract *requires* still makes the browser log "Failed to load resource", so the
// two are separated rather than one hiding the other. `/v1/categories` answers 500 for a
// `per_viewer` column by design (contracts §3.2: the visibility predicate is ⊘ unbuilt, and
// serving the set empty would be indistinguishable from a computed empty answer), so the viewer
// exercising that column is the instrument working, not breaking. Only the *unexplained* ones fail
// the run — and the explained ones are still printed, or this becomes a place to hide a real 500.
const expectedRefusals = requests.filter(
  (r) => r.path.startsWith('/v1/categories/') && r.status === 500
);
const unexplained = consoleErrors.filter(
  (e) => !(/Failed to load resource/.test(e) && expectedRefusals.length > 0)
);
console.log('--- expected refusals (contract, not breakage) ---');
console.log(
  expectedRefusals.length
    ? expectedRefusals.map((r) => `  ${r.status} ${r.path}`).join('\n')
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
if (!maskingVisible) failures.push('every principal reported the same visible count');
if (unexplained.length) failures.push(`${unexplained.length} unexplained console error(s)`);
// Colour is presentation. If either of these moves, the encoding has become a selection rule.
const servedAcrossEncodings = new Set(colourSeries.map((c) => c.counts?.served));
if (servedAcrossEncodings.size > 1) failures.push('changing the colour column changed the mark count');
if (colourSeries.some((c) => c.viewportRequests > 0)) failures.push('a colour change refetched');

if (failures.length) {
  console.error(`SMOKE FAILED: ${failures.join('; ')}`);
  process.exit(1);
}
console.log('SMOKE OK');
