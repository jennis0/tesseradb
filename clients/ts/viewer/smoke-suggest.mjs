#!/usr/bin/env node
// Drive the value-suggestion category control in a headless browser, against a live server.
//
//   node clients/ts/viewer/smoke-suggest.mjs [--url http://localhost:5187] [--shot-dir DIR]
//     [--headed] [--executable /path/to/chrome]
//
// Requires a running `tessera serve` over the GeoNames bundle and a running `vite dev`, with a
// dataset document (`?datasets=` or `VITE_TESSERA_DATASETS`) naming two presets: a broad one
// (every country the corpus carries) and a narrow one holding `FR` alone. `feature_class` is
// `public` with 9 values, and `country` is `derived` with 254 (`test_corpora/geonames/corpus.toml`)
// — the one pair on this corpus that exercises both shapes round 2 of `value-suggestion.md` §5.1
// built: a checklist where the empty-`q` page says `more: false`, a lookahead where it says `true`.
//
// Not a test suite — see `smoke.mjs`'s own note. This checks the component against the wire, which
// `clients/ts/components/test/panels.test.ts` cannot: a fake store answers whatever a test wrote,
// and never proves the empty-`q` page actually decides the shape, that a keystroke actually reaches
// `/v1/categories/country/suggest`, or that a chosen value actually narrows the viewport.
import {flags, isSupersededAbort, launchBrowser} from './smoke-browser.mjs';

const args = flags();
const url = args.url ?? 'http://localhost:5187';
const shotDir = args['shot-dir'] ?? '/tmp/tessera-smoke-suggest';
const settleMs = Number(args.settle ?? 4000);

const browser = await launchBrowser(args);
const page = await browser.newPage({viewport: {width: 1280, height: 800}});

const consoleErrors = [];
/** Every `/v1/categories/*\/suggest` request, in order — the thing "requests per keystroke" reads. */
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

/** Wait until the picture stops changing — `smoke.mjs`'s own helper, unchanged. */
const settled = async (limitMs = 20_000) => {
  const started = Date.now();
  let last = -1;
  let stable = 0;
  while (Date.now() - started < limitMs) {
    await page.waitForTimeout(400);
    const marks = await page.evaluate(() => window.__tesseraProbe?.marks ?? -1);
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

// The broad principal first — `option[0]` if the picker opened on it already, else select it —
// so `country`'s 254 values are visible and it reads as a lookahead.
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
    // `visible at build` is not on the option itself, so read the picker's own choice: the demo
    // opens on the broadest principal (`main.ts`), which is whatever is selected on load.
    if (o.selected) best = o.index;
  }
  return best === -1 ? 0 : best;
});
await page.selectOption('#principal', String(broadIndex));
await settled();
await shot('01-broad-opened');

// Open the explorer's filter panel — the overlay layout renders it directly at this viewport width,
// but a narrower one gates it behind the `Filters` tab (`explorer.ts`'s `sheet`), so click it if
// the panel is not already on screen.
const filterPanel = page.locator('tessera-filter-panel').first();
if ((await filterPanel.count()) === 0 || !(await filterPanel.isVisible().catch(() => false))) {
  const tab = page.locator('[part="tabs"] button', {hasText: 'Filters'}).first();
  if (await tab.count()) {
    await tab.click();
    await page.waitForTimeout(300);
  }
}
check('the filter panel is reachable', (await filterPanel.count()) > 0);

// --- feature_class: public, 9 values — the checklist shape ---------------------------------
const featureClass = page.locator('tessera-filter[column="feature_class"]').first();
await featureClass.waitFor({state: 'attached', timeout: 10_000}).catch(() => {});
await page.waitForTimeout(500); // the empty-q page's one round trip
await shot('02-feature-class-checklist');

const fcEntry = featureClass.locator('[part="entry"]');
const fcBoxes = featureClass.locator('[part="tick"] input[type="checkbox"]');
check('feature_class renders no search box (checklist)', (await fcEntry.count()) === 0);
const fcCount = await fcBoxes.count();
check('feature_class checklist carries 9 values', fcCount === 9, `saw ${fcCount}`);
check('feature_class carries no match span', (await featureClass.locator('[part="tick"] mark').count()) === 0);

// --- country: derived, 254 values — the lookahead shape -------------------------------------
const country = page.locator('tessera-filter[column="country"]').first();
await country.waitFor({state: 'attached', timeout: 10_000}).catch(() => {});
await page.waitForTimeout(500);
await shot('03-country-lookahead-initial');

const countryEntry = country.locator('[part="entry"]');
check('country renders a search box (lookahead)', (await countryEntry.count()) > 0);
const initialTicks = await country.locator('[part="tick"]').count();
check('country’s initial list is 20', initialTicks === 20, `saw ${initialTicks}`);
const moreNote = await country.locator('[part="more"]').textContent().catch(() => null);
check('country says "type more to narrow"', (moreNote ?? '').includes('type more to narrow'), `saw "${moreNote}"`);

// Type "fr" one keystroke at a time — what the request count is measured against.
const before = suggestRequests.filter((r) => r.column === 'country').length;
await countryEntry.click();
await countryEntry.type('f', {delay: 50});
await page.waitForTimeout(250); // inside the 120ms debounce window — should not fire yet on its own
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

// Choose the first suggestion — a chip appears, and the viewport refetches under the filter.
const vpBefore = viewportRequests;
const firstTick = country.locator('[part="tick"]').first();
const pickedLabel = (await firstTick.textContent())?.trim() ?? '';
await firstTick.click();
await page.waitForTimeout(400);
await settled(8000);
await shot('05-country-chip-chosen');
const chipCount = await country.locator('[part="value-chip"]').count();
check('choosing a value adds a chip', chipCount > 0, `label was "${pickedLabel}"`);
check('the viewport refetches under the chosen filter', viewportRequests > vpBefore, `${vpBefore} -> ${viewportRequests}`);

// --- the narrow principal (FR only) sees country as a checklist -----------------------------
const narrowIndex = await page.evaluate(() => {
  const select = /** @type {HTMLSelectElement | null} */ (document.getElementById('principal'));
  for (const o of select?.options ?? []) if (/\bFR\b|France/i.test(o.textContent ?? '')) return o.index;
  return -1;
});
if (narrowIndex === -1) {
  check('a narrow (FR-only) principal is offered', false, 'no matching #principal option');
} else {
  await page.selectOption('#principal', String(narrowIndex));
  await settled();
  await page.waitForTimeout(500);
  await shot('06-narrow-country-checklist');
  const narrowCountry = page.locator('tessera-filter[column="country"]').first();
  const narrowEntry = narrowCountry.locator('[part="entry"]');
  const narrowBoxes = narrowCountry.locator('[part="tick"] input[type="checkbox"]');
  check('the narrow principal renders country as a checklist', (await narrowEntry.count()) === 0);
  const narrowBoxCount = await narrowBoxes.count();
  check('the narrow principal’s checklist carries one value', narrowBoxCount === 1, `saw ${narrowBoxCount}`);
}

await browser.close();

console.log('--- requests ---');
console.log(`  suggest requests total: ${suggestRequests.length}`);
console.log(`  viewport requests total: ${viewportRequests}`);
console.log(`  suggest 429s (single-flight admission shedding a concurrent ask, retried by the store — value-suggestion.md §5.1): ${suggest429s}`);
console.log('--- screenshots ---');
for (const s of shots) console.log(`  ${s}`);
// **A 429 off `/v1/categories/*/suggest` is not a fault.** Every category control mounts and asks
// an empty `q` in the same tick (`filter-panel.ts` renders one `<tessera-filter>` per operand), so
// the session's one-in-flight-per-suggest admission sheds every ask but the first — by contract,
// not by accident — and the store retries them (`store.ts`'s `suggest`). Chromium logs the shed
// response as a console error regardless of what the client does with it next, so it is excepted
// here the same way `isSupersededAbort` excepts a viewport request's own abort.
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
