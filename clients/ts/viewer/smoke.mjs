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
for (const [key, count] of Object.entries(byPath)) console.log(`  ${count.toString().padStart(4)}x ${key}`);
console.log('--- panels ---');
console.log(panelText.split('\n').map((l) => `  ${l}`).join('\n'));
console.log('--- canvas ---');
console.log(' ', JSON.stringify(canvasPixels));
console.log('--- console errors ---');
console.log(consoleErrors.length ? consoleErrors.map((e) => `  ${e}`).join('\n') : '  none');
console.log(`--- screenshot: ${shot}`);

const viewportOk = requests.some((r) => r.path === '/v1/viewport' && r.status === 200);
const drewSomething = canvasPixels.found && canvasPixels.lit > 0;
if (!viewportOk || !drewSomething || consoleErrors.length) {
  console.error('SMOKE FAILED');
  process.exit(1);
}
console.log('SMOKE OK');
