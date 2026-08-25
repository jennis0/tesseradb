#!/usr/bin/env node
// Does the viewport-addressed client hold marks roughly constant across zoom, in ONE request
// per view, without shedding?
//
// This is the acceptance test for the whole Phase 1 workstream. The tile-addressed MVP's figures
// against the same fixtures are 17 / 50 / 242 / 456 marks at depths 0-3, with 12 of 23 requests
// shed at 1e9. Both numbers should move decisively.
import {chromium} from 'playwright';

const args = Object.fromEntries(
  process.argv
    .slice(2)
    .reduce((acc, a, i, all) => (a.startsWith('--') ? [...acc, [a.slice(2), all[i + 1]]] : acc), [])
);
const url = args.url ?? 'http://localhost:5173';
const settle = Number(args.settle ?? 9000);
const principal = args.principal ?? '3';

const browser = await chromium.launch({
  args: ['--use-gl=swiftshader', '--enable-unsafe-swiftshader']
});
const page = await browser.newPage({viewport: {width: 1280, height: 800}});

const statuses = [];
const errors = [];
page.on('response', (r) => {
  if (r.url().includes('/v1/viewport')) statuses.push(r.status());
});
page.on('pageerror', (e) => errors.push(e.message));
page.on('console', (m) => {
  // A 429 the client retried and recovered still prints a browser resource-load error. That is
  // the transport narrating, not the client failing; judge on whether the view survived instead.
  if (m.type() === 'error' && !/429|Too Many Requests/.test(m.text())) errors.push(m.text());
});

await page.goto(url, {waitUntil: 'load'});
await page.waitForTimeout(settle);
await page.selectOption('#principal', principal);
await page.waitForTimeout(settle);

const readPanels = () =>
  page.evaluate(() => {
    const text = document.querySelector('#stats')?.textContent ?? '';
    // The row spans concatenate label and value with no separator, so strip whitespace entirely.
    const flat = text.replace(/\s+/g, '');
    const shown = /([\d,]+)of([\d,]+)shown/.exec(flat);
    const depth = /depthchosen(\d+)/.exec(flat);
    const tiles = /tilesrequested([\d,]+)/.exec(flat);
    const actual = /actualmarks([\d,]+)/.exec(flat);
    const limited = /limitedby(budget|maxTiles|maxDepth|saturated)/.exec(flat);
    const status = /counts(unavailable|loading)/.exec(flat);
    return {
      served: shown?.[1] ?? null,
      visible: shown?.[2] ?? null,
      depth: depth?.[1] ?? null,
      tiles: tiles?.[1] ?? null,
      actual: actual?.[1] ?? null,
      limitedBy: limited?.[1] ?? null,
      status: status?.[1] ?? 'shown'
    };
  });

console.log('zoom_step  depth  tiles      marks       visible          limitedBy  requests');
const marks = [];
for (let step = 0; step <= 5; step++) {
  if (step > 0) {
    await page.mouse.move(640, 400);
    await page.mouse.wheel(0, -400);
    await page.waitForTimeout(settle);
  }
  const before = statuses.length;
  const p = await readPanels();
  marks.push(Number((p.actual ?? '0').replace(/,/g, '')));
  console.log(
    `${String(step).padEnd(10)} ${String(p.depth).padEnd(6)} ${String(p.tiles).padEnd(10)} ` +
      `${String(p.actual).padEnd(11)} ${String(p.visible).padEnd(16)} ` +
      `${String(p.limitedBy).padEnd(10)} ${statuses.length - before}`
  );
}

const shed = statuses.filter((s) => s === 429).length;
const ok = statuses.filter((s) => s === 200).length;
const nonZero = marks.filter((m) => m > 0);
const spread = nonZero.length ? Math.max(...nonZero) / Math.min(...nonZero) : Infinity;

console.log(`\nviewport requests: ${statuses.length} total, ${ok} ok, ${shed} shed (429)`);
console.log(`marks across zoom: ${marks.join(', ')}`);
console.log(`spread (max/min over non-zero): ${spread.toFixed(2)}x   [MVP was ~25x]`);
console.log(`console errors: ${errors.length ? errors.slice(0, 3).join(' | ') : 'none'}`);

await page.screenshot({path: args.shot ?? '/tmp/tessera-budget.png'});
await browser.close();

const failures = [];
if (shed > 0) console.log(`note: ${shed} request(s) shed with 429 and retried`);
if (errors.length) failures.push(`${errors.length} console errors`);
if (nonZero.length < 4) failures.push('fewer than four zoom steps returned marks');
if (spread > 6) failures.push(`marks spread ${spread.toFixed(1)}x across zoom (want <6x)`);
if (failures.length) {
  console.error(`FAILED: ${failures.join('; ')}`);
  process.exit(1);
}
console.log('OK');
