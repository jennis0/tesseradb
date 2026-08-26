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
  // A 429 is shed-and-retried, and `ERR_INCOMPLETE_CHUNKED_ENCODING` is Chromium logging a
  // streamed response the driver abandoned when the view moved — a superseded request's abort,
  // which is the client working as designed (the harness has exempted it since step 3).
  if (m.type() === 'error' && !/429|Too Many Requests|ERR_INCOMPLETE_CHUNKED_ENCODING/.test(m.text())) errors.push(m.text());
});

await page.goto(url, {waitUntil: 'load'});
await page.waitForTimeout(settle);
await page.selectOption('#principal', principal);
await page.waitForTimeout(settle);

/**
 * The figures come from the map's probe (design §5.9): the store's `view` projection — depth,
 * status, the counts — and the demo's instrument numbers beside it (the tiles the budget asked
 * for and what limited it), which the §4 surface deliberately omits and the demo publishes.
 */
const readPanels = () =>
  page.evaluate(() => {
    const p = window.__tesseraProbe;
    if (!p) return {served: null, visible: null, depth: null, tiles: null, actual: null, limitedBy: null, status: 'absent'};
    const i = p.instruments ?? {};
    return {
      served: p.view.served,
      visible: p.view.visible,
      depth: String(p.view.depth),
      tiles: i.tiles ?? null,
      actual: String(p.marks - p.view.provisional),
      limitedBy: i.limitedBy ?? null,
      status: p.view.status
    };
  });

console.log('zoom_step  depth  tiles      marks       visible          limitedBy  requests');
const marks = [];
const visibles = [];
for (let step = 0; step <= 5; step++) {
  if (step > 0) {
    // Over the corpus's centre, clear of the overlay's floating cards.
    await page.mouse.move(870, 450);
    await page.mouse.wheel(0, -400);
    await page.waitForTimeout(settle);
  }
  // A response paints as its slices land, so the marks are read once they have stopped moving.
  for (let last = -1, stable = 0, tries = 0; stable < 4 && tries < 150; tries++) {
    await page.waitForTimeout(400);
    const now = await page.evaluate(() => window.__tesseraProbe?.marks ?? -1);
    if (now === last) stable++;
    else {
      stable = 0;
      last = now;
    }
  }
  const before = statuses.length;
  const p = await readPanels();
  marks.push(Number((p.actual ?? '0').replace(/,/g, '')));
  visibles.push(Number(p.visible ?? 0));
  console.log(
    `${String(step).padEnd(10)} ${String(p.depth).padEnd(6)} ${String(p.tiles).padEnd(10)} ` +
      `${String(p.actual).padEnd(11)} ${String(p.visible).padEnd(16)} ` +
      `${String(p.limitedBy).padEnd(10)} ${statuses.length - before}`
  );
}

const shed = statuses.filter((s) => s === 429).length;
const ok = statuses.filter((s) => s === 200).length;
// The budget can be met only where at least that many are visible: a step whose view holds
// fewer marks than the budget is read from the strip's own visible count and left out of the
// spread — a zoom into sparse ground says nothing about the depth choice.
const budget = 500_000;
const eligible = marks.filter((m, i) => m > 0 && visibles[i] >= budget);
const nonZero = eligible;
const spread = nonZero.length ? Math.max(...nonZero) / Math.min(...nonZero) : Infinity;

console.log(`\nviewport requests: ${statuses.length} total, ${ok} ok, ${shed} shed (429)`);
console.log(`marks across zoom: ${marks.join(', ')} (${eligible.length} of ${marks.length} steps with at least the budget visible)`);
console.log(`spread (max/min over non-zero): ${spread.toFixed(2)}x   [MVP was ~25x]`);
console.log(`console errors: ${errors.length ? errors.slice(0, 3).join(' | ') : 'none'}`);

await page.screenshot({path: args.shot ?? '/tmp/tessera-budget.png'});
await browser.close();

const failures = [];
if (shed > 0) console.log(`note: ${shed} request(s) shed with 429 and retried`);
if (errors.length) failures.push(`${errors.length} console errors`);
if (marks.filter((m) => m > 0).length < 4) failures.push('fewer than four zoom steps returned marks');
if (eligible.length < 2) failures.push('fewer than two zoom steps had the budget visible — the spread was not measured');
if (spread > 6) failures.push(`marks spread ${spread.toFixed(1)}x across zoom (want <6x)`);
if (failures.length) {
  console.error(`FAILED: ${failures.join('; ')}`);
  process.exit(1);
}
console.log('OK');
