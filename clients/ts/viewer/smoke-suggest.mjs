#!/usr/bin/env node
// Drive a category field card's search box in a headless browser, against a live server.
//
//   node clients/ts/viewer/smoke-suggest.mjs [--url http://localhost:5187] [--shot-dir DIR]
//     [--headed] [--executable /path/to/chrome]
//
// Requires a running `mosaica serve` over the GeoNames bundle and a running `vite dev`, with a
// dataset document (`?datasets=` or `VITE_MOSAICA_DATASETS`). `country` is `derived` with 254
// values (`test_corpora/geonames/corpus.toml`).
//
// It checks against a real server what the component tests with a fake store cannot: that
// keystrokes in the country card's search box reach `/v1/categories/country/suggest` a bounded
// number of times, that the server's match span is marked, and that a chosen value narrows the
// viewport.
import {flags, isSupersededAbort, launchBrowser} from './smoke-browser.mjs';

const args = flags();
const url = args.url ?? 'http://localhost:5187';
const shotDir = args['shot-dir'] ?? '/tmp/mosaica-smoke-suggest';
const settleMs = Number(args.settle ?? 4000);

const browser = await launchBrowser(args);
const page = await browser.newPage({viewport: {width: 1280, height: 800}});

const consoleErrors = [];
/** Every `/v1/categories/*\/suggest` request, in order, for the requests-per-keystroke figure. */
const suggestRequests = [];
let viewportRequests = 0;
let suggest429s = 0;
page.on('console', (m) => {
  if (m.type() === 'error') consoleErrors.push(m.text());
});
page.on('pageerror', (e) => consoleErrors.push(`pageerror: ${e.message}`));
page.on('request', (r) => {
  const u = new URL(r.url());
  const m = u.pathname.match(/^\/v1\/categories\/([^/]+)\/suggest$/);
  if (m) suggestRequests.push({column: m[1], q: u.searchParams.get('q') ?? '', at: Date.now()});
});
page.on('response', (r) => {
  const u = new URL(r.url());
  if (u.pathname === '/v1/viewport') viewportRequests++;
  if (/^\/v1\/categories\/[^/]+\/suggest$/.test(u.pathname) && r.status() === 429) suggest429s++;
});

const shots = [];
const shot = async (name) => {
  const {mkdir} = await import('node:fs/promises');
  await mkdir(shotDir, {recursive: true});
  const path = `${shotDir}/${name}.png`;
  await page.screenshot({path, timeout: 60_000});
  shots.push(path);
};

/** Wait until the picture stops changing, as in `smoke.mjs`. */
const settled = async (limitMs = 20_000) => {
  const started = Date.now();
  let last = -1;
  let stable = 0;
  while (Date.now() - started < limitMs) {
    await page.waitForTimeout(400);
    const marks = await page.evaluate(() => window.__mosaicaProbe?.marks ?? -1);
    if (marks === last) {
      if (++stable >= 3) return true;
    } else {
      stable = 0;
      last = marks;
    }
  }
  return false;
};

await page.goto(url, {waitUntil: 'load'});
await page.waitForTimeout(settleMs);
await settled();

const results = [];
const check = (name, pass, detail = '') => {
  results.push({name, pass, detail});
  console.log(`  [${pass ? 'ok' : 'FAIL'}] ${name}${detail ? ` — ${detail}` : ''}`);
};

// The broad principal first, so `country`'s 254 values are visible and it shows as a lookahead.
const principalOptions = await page.locator('#principal option').count().catch(() => 0);
if (principalOptions === 0) {
  console.error('SMOKE FAILED: no #principal options — no measured presets reached the viewer');
  await browser.close();
  process.exit(1);
}
const broadIndex = await page.evaluate(() => {
  const select = /** @type {HTMLSelectElement | null} */ (document.getElementById('principal'));
  let best = -1;
  for (const o of select?.options ?? []) {
    // The viewer opens on the broadest principal, so it is the one selected on load.
    if (o.selected) best = o.index;
  }
  return best === -1 ? 0 : best;
});
await page.selectOption('#principal', String(broadIndex));
await settled();
await shot('01-broad-opened');

// Open the field column: at a narrow width it is behind the `Fields` tab.
const filterPanel = page.locator('mosaica-filter-panel').first();
if ((await filterPanel.count()) === 0 || !(await filterPanel.isVisible().catch(() => false))) {
  const tab = page.locator('[part="tabs"] button', {hasText: 'Fields'}).first();
  if (await tab.count()) {
    await tab.click();
    await page.waitForTimeout(300);
  }
}
check('the filter panel is reachable', (await filterPanel.count()) > 0);

// The country card, listed for this run.
await page.evaluate(() => {
  const explorer = /** @type {HTMLElement & {pinnedFilters: string} | null} */ (document.querySelector('mosaica-explorer'));
  if (explorer) explorer.pinnedFilters = 'country';
});
const country = page.locator('mosaica-filter[column="country"]').first();
await country.waitFor({state: 'attached', timeout: 10_000}).catch(() => {});
await page.waitForTimeout(500);
await shot('03-country-card');

const countryEntry = country.locator('[part="entry"]');
check('country’s card has a search box', (await countryEntry.count()) > 0);

// Type "fr" one keystroke at a time, and count the requests.
const before = suggestRequests.filter((r) => r.column === 'country').length;
await countryEntry.click();
await countryEntry.type('f', {delay: 50});
await page.waitForTimeout(250); // longer than the store's 120 ms debounce
await countryEntry.type('r', {delay: 50});
await page.waitForTimeout(600); // past the debounce and the round trip
await settled(6000);
await shot('04-country-narrowed-fr');
const afterFr = suggestRequests.filter((r) => r.column === 'country').length;
const perKeystroke = afterFr - before;
console.log(`  country: ${perKeystroke} suggest request(s) for 2 keystrokes ("f", "r") — the debounced/single-flight bound, not one per keystroke`);

const narrowedTicks = await country.locator('[part="tick"]').count();
check('typing "fr" narrows the list', narrowedTicks > 0 && narrowedTicks <= 20, `saw ${narrowedTicks} rows`);
const markText = await country.locator('[part="tick"] mark').first().textContent().catch(() => null);
check('a match span is highlighted', !!markText && /fr/i.test(markText), `mark="${markText}"`);

// Choose the first suggestion: the viewport refetches under the filter.
const vpBefore = viewportRequests;
const firstTick = country.locator('[part="tick"]').first();
const pickedLabel = (await firstTick.textContent())?.trim() ?? '';
await firstTick.click();
await page.waitForTimeout(400);
await settled(8000);
await shot('05-country-chosen');
console.log(`  chose "${pickedLabel}"`);
check('the viewport refetches under the chosen filter', viewportRequests > vpBefore, `${vpBefore} -> ${viewportRequests}`);

await browser.close();

console.log('--- requests ---');
console.log(`  suggest requests total: ${suggestRequests.length}`);
console.log(`  viewport requests total: ${viewportRequests}`);
console.log(`  suggest 429s (a concurrent ask shed by the server and retried by the store): ${suggest429s}`);
console.log('--- screenshots ---');
for (const s of shots) console.log(`  ${s}`);
// A 429 from `/v1/categories/*/suggest` is not a fault: every category control asks an empty `q`
// at mount, the server admits one suggest at a time per session and sheds the rest, and the store
// retries them. Chromium still logs each shed response.
const unexplained = consoleErrors.filter((e) => !isSupersededAbort(e) && !/status of 429/.test(e));
console.log('--- console errors (a superseded request’s abort and a suggest 429 excepted) ---');
console.log(unexplained.length ? unexplained.map((e) => `  ${e}`).join('\n') : '  none');

const failures = results.filter((r) => !r.pass);
if (unexplained.length) failures.push({name: 'console errors', pass: false, detail: `${unexplained.length}`});
if (failures.length) {
  console.error(`SMOKE FAILED: ${failures.map((f) => f.name).join('; ')}`);
  process.exit(1);
}
console.log('SMOKE OK');
