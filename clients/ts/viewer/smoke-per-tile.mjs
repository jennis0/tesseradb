#!/usr/bin/env node
// Drive the Source panel's `artifacts per tile` control in a headless browser.
//
//   node clients/ts/viewer/smoke-per-tile.mjs [--url http://localhost:5173] [--shots DIR]
//     [--headed] [--executable /path/to/chrome]
//
// The number of artifacts per tile the viewer asks for is the one the control shows and the one
// the address names: opened without `?per-tile=`, opened with it, and after the control is moved.
// An address naming no whole number is reported in the failures panel and not used.
//
// A move asks for the drawn layers again at the new number, keeps the layers drawn and the
// colour, and asks under the current principal's token. Before the move the script switches
// principal twice with the first one's authorisation held back until the second has answered,
// then draws the last layer the Layers popover offers and colours by a column. The principal
// check needs three principals in the picker and the layer check two layers; with fewer, each is
// skipped and says so. The colour is set through the explorer's store.
//
// The number in use is read off the `per_tile` of each `POST /v1/artifacts/viewport` the page
// sends, and the token off its `authorization`. Requires a running `mosaica serve` whose bundle
// declares a layer, and a running `vite dev`.
import {mkdir} from 'node:fs/promises';
import {join} from 'node:path';
import {flags, isSupersededAbort, launchBrowser, withParams} from './smoke-browser.mjs';

const args = flags();
const url = args.url ?? 'http://localhost:5173';
const shots = args.shots ?? '/tmp/mosaica-per-tile';
await mkdir(shots, {recursive: true});

const browser = await launchBrowser(args);
const page = await browser.newPage({viewport: {width: 1280, height: 800}});

/** The step the script is at, which each console error is reported with. */
let step = 'opening';
const consoleErrors = [];
page.on('console', (m) => {
  if (m.type() === 'error') consoleErrors.push(`${step}: ${m.text()}`);
});
page.on('pageerror', (e) => consoleErrors.push(`${step}: pageerror: ${e.message}`));

/** The `per_tile` and bearer token of every artifact request, in the order the page sent them. */
const asked = [];
page.on('request', (request) => {
  if (request.method() !== 'POST' || !request.url().endsWith('/v1/artifacts/viewport')) return;
  asked.push({perTile: Number(request.postDataJSON()?.per_tile), token: (request.headers()['authorization'] ?? '').replace(/^Bearer /, '')});
});
const perTiles = (from) => asked.slice(from).map((a) => a.perTile);

/** Every session minted, by principal, in the order the answers arrived. */
const minted = [];
page.on('response', async (response) => {
  if (!response.url().endsWith('/session/authorise') || !response.ok()) return;
  try {
    minted.push({principal: response.request().postDataJSON()?.principal, token: (await response.json()).token});
  } catch {
    // A body the page stopped reading.
  }
});

/** The next authorisation is held here until `release` is called. */
let holdNext = false;
/** @type {{principal: string; release: () => void} | null} */
let held = null;
await page.route('**/session/authorise', async (route) => {
  if (holdNext) {
    holdNext = false;
    await new Promise((release) => {
      held = {principal: route.request().postDataJSON()?.principal, release: () => release(undefined)};
    });
  }
  await route.continue();
});

/** Wait until `ready()` holds; false on timeout. */
async function until(ready, limitMs = 60_000) {
  const started = Date.now();
  while (Date.now() - started < limitMs) {
    if (ready()) return true;
    await page.waitForTimeout(250);
  }
  return false;
}

/**
 * Wait until the mark count stops changing, so no response is still decoding when the next step
 * replaces the store.
 */
async function settled(limitMs = 45_000) {
  const started = Date.now();
  let last = -1;
  let stable = 0;
  while (Date.now() - started < limitMs) {
    await page.waitForTimeout(500);
    const marks = await page.evaluate(() => window.__mosaicaProbe?.marks ?? -1);
    if (marks === last) {
      if (++stable >= 4) return;
    } else {
      stable = 0;
      last = marks;
    }
  }
}

/** Wait until an artifact request at index `from` or later asks for `perTile`; false on timeout. */
const askedFor = (perTile, from) => until(() => perTiles(from).includes(perTile));

/** The control's value, the address's `per-tile`, and the layers and colour the explorer's store draws. */
const reading = async () => ({
  control: Number(await page.locator('#per-tile').inputValue()),
  address: Number(new URL(page.url()).searchParams.get('per-tile')),
  ...(await page.evaluate(() => {
    const explorer = /** @type {{activeStore: {get(name: 'artifacts'): {layers: string[]}; get(name: 'legend'): {colourBy: string | null}} | null} | null} */ (/** @type {unknown} */ (document.querySelector('mosaica-explorer')));
    const store = explorer?.activeStore;
    return {layers: store?.get('artifacts').layers ?? [], colourBy: store?.get('legend').colourBy ?? null};
  }))
});

/**
 * Draw only the last layer the explorer's Layers popover offers, so it is not the one the viewer
 * opens on. Returns how many layers it offers.
 */
async function drawLastLayer() {
  const toggle = page.locator('[part="layers-toggle"]').first();
  if ((await toggle.getAttribute('aria-expanded')) !== 'true') await toggle.click();
  const entries = page.locator('mosaica-layer-picker [part="entry"]');
  await entries.first().waitFor({timeout: 60_000});
  const count = await entries.count();
  for (let i = 0; i < count; i++) {
    if ((await toggle.getAttribute('aria-expanded')) !== 'true') await toggle.click();
    const box = entries.nth(i).locator('input');
    if ((await box.isChecked()) !== (i === count - 1)) await box.click();
  }
  await toggle.click();
  return count;
}

/** Colour by a rendered column other than the current colour, through the explorer's store; returns it, or null. */
const colourByAColumn = () =>
  page.evaluate(() => {
    const explorer = /** @type {{activeStore: {get(name: 'meta'): {declaredScalars: {name: string; render: boolean}[]} | null; get(name: 'legend'): {colourBy: string | null}; setColourBy(column: string | null): void} | null} | null} */ (/** @type {unknown} */ (document.querySelector('mosaica-explorer')));
    const store = explorer?.activeStore;
    const current = store?.get('legend').colourBy;
    const column = store?.get('meta')?.declaredScalars.find((c) => c.render && c.name !== current)?.name ?? null;
    if (column !== null) store.setColourBy(column);
    return column;
  });

const failures = [];
/** Check that the control, the address and the requests from `from` on all name `perTile`. */
function expectInUse(stage, perTile, r, from) {
  const sent = [...new Set(perTiles(from))];
  console.log(`  ${stage.padEnd(34)} control=${r.control} address=${r.address} requests=${sent.join(',') || 'none'} layers=${r.layers.join(',') || 'none'} colour=${r.colourBy}`);
  if (r.control !== perTile) failures.push(`${stage}: the control shows ${r.control}, not ${perTile}`);
  if (r.address !== perTile) failures.push(`${stage}: the address names ${r.address}, not ${perTile}`);
  if (sent.length !== 1 || sent[0] !== perTile) failures.push(`${stage}: artifact requests asked for ${sent.join(',') || 'nothing'}, not ${perTile}`);
}

console.log('--- artifacts per tile: control, address, requests ---');

// Opened without `?per-tile=`. Look-ahead off, so requests go out only when the view moves.
await page.goto(withParams(url, {prefetch: 0}), {waitUntil: 'load'});
await page.locator('#per-tile').waitFor({timeout: 60_000});
const opened = (await askedFor(50, 0)) ? await reading() : null;
if (!opened) {
  console.error(`PER-TILE SMOKE FAILED: no artifact request asked for 50 within a minute (asked: ${perTiles(0).join(',') || 'nothing'}); does the bundle declare a layer?`);
  await browser.close();
  process.exit(1);
}
expectInUse('opened without ?per-tile=', 50, opened, 0);
await settled();

// Switch principal to A and then B, A's authorisation answering only after B's.
const options = await page.locator('#principal option').count();
const selected = await page.locator('#principal').inputValue();
const others = [...Array(options).keys()].map(String).filter((v) => v !== selected);
/** @type {{late: string; current: string} | null} */
let principals = null;
if (others.length >= 2) {
  step = 'switching principal';
  const armed = minted.length;
  holdNext = true;
  await page.selectOption('#principal', others[0]);
  if (!(await until(() => held !== null))) failures.push('switching principal sent no authorisation to hold back');
  await page.selectOption('#principal', others[1]);
  const late = held?.principal;
  if (!(await until(() => minted.slice(armed).some((m) => m.principal !== late)))) failures.push('the second principal was never authorised');
  const current = minted.slice(armed).find((m) => m.principal !== late)?.principal;
  held?.release();
  if (!(await until(() => minted.slice(armed).some((m) => m.principal === late)))) failures.push('the held authorisation never answered');
  principals = {late, current};
  console.log(`  principal ${late} answered after ${current}`);
} else {
  console.log(`  principal check skipped: the picker offers ${options} principal(s), and it needs 3`);
}

// Moving the control asks for the layers again at the new number, and keeps them drawn.
await settled();
step = 'choosing a layer and a colour';
const layerCount = await drawLastLayer();
const colourBy = await colourByAColumn();
await page.waitForTimeout(1500);
await settled();
const chosen = await reading();
const before = asked.length;
step = 'moving the control';
await page.locator('#per-tile').fill('7');
if (!(await askedFor(7, before))) failures.push('moving the control to 7 sent no artifact request for 7');
await page.waitForTimeout(1500);
const firstMoved = perTiles(0).indexOf(7, before);
const moved = await reading();
expectInUse('after the control moved to 7', 7, moved, firstMoved < 0 ? asked.length : firstMoved);
if (layerCount < 2) console.log(`  layer check skipped: the Layers popover offers ${layerCount} layer(s), and it needs 2`);
else if (chosen.layers.join(',') === opened.layers.join(',')) failures.push(`choosing the last layer left the layers drawn at ${opened.layers.join(',')}`);
if (moved.layers.join(',') !== chosen.layers.join(',')) failures.push(`moving the control changed the layers drawn from ${chosen.layers.join(',')} to ${moved.layers.join(',')}`);
if (colourBy === null || chosen.colourBy !== colourBy) failures.push(`colouring by a column left the colour at ${chosen.colourBy}`);
if (moved.colourBy !== chosen.colourBy) failures.push(`moving the control changed the colour from ${chosen.colourBy} to ${moved.colourBy}`);
if (principals) {
  const tokensOf = (principal) => new Set(minted.filter((m) => m.principal === principal).map((m) => m.token));
  const current = tokensOf(principals.current);
  const late = tokensOf(principals.late);
  const after = asked.slice(firstMoved < 0 ? asked.length : firstMoved);
  if (after.some((a) => late.has(a.token))) failures.push(`after the move, artifacts were asked for under ${principals.late}'s token, which answered late, while ${principals.current} is chosen`);
  else if (!after.every((a) => current.has(a.token))) failures.push(`after the move, artifacts were asked for under a token not minted for ${principals.current}`);
}
const shot = join(shots, 'per-tile-7.png');
await page.screenshot({path: shot, timeout: 60_000});

// Opened with `?per-tile=`.
const reopened = asked.length;
step = 'opening with ?per-tile=12';
await page.goto(withParams(url, {prefetch: 0, 'per-tile': 12}), {waitUntil: 'load'});
await page.locator('#per-tile').waitFor({timeout: 60_000});
if (!(await askedFor(12, reopened))) failures.push('opening with ?per-tile=12 sent no artifact request for 12');
expectInUse('opened with ?per-tile=12', 12, await reading(), reopened);

// An address naming no whole number is reported and not used.
const refused = asked.length;
step = 'opening with ?per-tile=12.5';
await page.goto(withParams(url, {prefetch: 0, 'per-tile': '12.5'}), {waitUntil: 'load'});
await page.locator('#per-tile').waitFor({timeout: 60_000});
if (!(await askedFor(50, refused))) failures.push('opening with ?per-tile=12.5 sent no artifact request for 50');
expectInUse('opened with ?per-tile=12.5', 50, await reading(), refused);
const reported = await page.locator('#instruments .bad').allInnerTexts();
if (!reported.some((t) => t.includes('per-tile:'))) failures.push('?per-tile=12.5 was not reported in the failures panel');

await browser.close();

console.log(`--- screenshot after the move: ${shot} ---`);
console.log('--- console errors (a superseded request’s abort excepted) ---');
console.log(consoleErrors.length ? consoleErrors.map((e) => `  ${e}`).join('\n') : '  none');
const unexplained = consoleErrors.filter((e) => !isSupersededAbort(e));
if (unexplained.length) failures.push(`${unexplained.length} console error(s)`);

if (failures.length) {
  console.error(`PER-TILE SMOKE FAILED: ${failures.join('; ')}`);
  process.exit(1);
}
console.log('PER-TILE SMOKE OK');
