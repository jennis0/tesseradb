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
console.log('--- panels ---');
console.log(panelText.split('\n').map((l) => `  ${l}`).join('\n'));
console.log('--- canvas ---');
console.log(' ', JSON.stringify(canvasPixels));
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
if (consoleErrors.length) failures.push(`${consoleErrors.length} console error(s)`);

if (failures.length) {
  console.error(`SMOKE FAILED: ${failures.join('; ')}`);
  process.exit(1);
}
console.log('SMOKE OK');
