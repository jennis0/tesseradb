#!/usr/bin/env node
// Drive the Source panel's `artifacts per tile` control in a headless browser.
//
//   node clients/ts/viewer/smoke-per-tile.mjs [--url http://localhost:5173] [--shots DIR]
//     [--headed] [--executable /path/to/chrome]
//
// The number of artifacts per tile the viewer asks for is the one the control shows and the one
// the address names: opened without `?per-tile=`, opened with it, and after the control is moved.
// A move asks for the drawn layers again at the new number and keeps the layers drawn.
//
// The number in use is read off the `per_tile` of each `POST /v1/artifacts/viewport` the page
// sends. Requires a running `tessera serve` whose bundle declares a layer, and a running
// `vite dev`.
import {mkdir} from 'node:fs/promises';
import {join} from 'node:path';
import {flags, isSupersededAbort, launchBrowser, withParams} from './smoke-browser.mjs';

const args = flags();
const url = args.url ?? 'http://localhost:5173';
const shots = args.shots ?? '/tmp/tessera-per-tile';
await mkdir(shots, {recursive: true});

const browser = await launchBrowser(args);
const page = await browser.newPage({viewport: {width: 1280, height: 800}});

const consoleErrors = [];
page.on('console', (m) => {
  if (m.type() === 'error') consoleErrors.push(m.text());
});
page.on('pageerror', (e) => consoleErrors.push(`pageerror: ${e.message}`));

/** The `per_tile` of every artifact request, in the order the page sent them. */
const asked = [];
page.on('request', (request) => {
  if (request.method() !== 'POST' || !request.url().endsWith('/v1/artifacts/viewport')) return;
  asked.push(Number(request.postDataJSON()?.per_tile));
});

/** Wait until an artifact request at index `from` or later asks for `perTile`; false on timeout. */
async function askedFor(perTile, from, limitMs = 60_000) {
  const started = Date.now();
  while (Date.now() - started < limitMs) {
    if (asked.slice(from).includes(perTile)) return true;
    await page.waitForTimeout(250);
  }
  return false;
}

/** The control's value, the address's `per-tile` and the layers the explorer's store draws. */
const reading = async () => ({
  control: Number(await page.locator('#per-tile').inputValue()),
  address: Number(new URL(page.url()).searchParams.get('per-tile')),
  layers: await page.evaluate(() => {
    const explorer = /** @type {{activeStore: {get(name: 'artifacts'): {layers: string[]}} | null} | null} */ (/** @type {unknown} */ (document.querySelector('tessera-explorer')));
    return explorer?.activeStore?.get('artifacts').layers ?? [];
  })
});

const failures = [];
/** Check that the control, the address and the requests from `from` on all name `perTile`. */
function expectInUse(stage, perTile, r, from) {
  const sent = [...new Set(asked.slice(from))];
  console.log(`  ${stage.padEnd(34)} control=${r.control} address=${r.address} requests=${sent.join(',') || 'none'} layers=${r.layers.join(',') || 'none'}`);
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
  console.error(`PER-TILE SMOKE FAILED: no artifact request asked for 50 within a minute (asked: ${asked.join(',') || 'nothing'}); does the bundle declare a layer?`);
  await browser.close();
  process.exit(1);
}
expectInUse('opened without ?per-tile=', 50, opened, 0);

// Moving the control asks for the layers again at the new number, and keeps them drawn.
const before = asked.length;
await page.locator('#per-tile').fill('7');
if (!(await askedFor(7, before))) failures.push('moving the control to 7 sent no artifact request for 7');
await page.waitForTimeout(1500);
const firstMoved = asked.indexOf(7, before);
const moved = await reading();
expectInUse('after the control moved to 7', 7, moved, firstMoved < 0 ? asked.length : firstMoved);
if (moved.layers.join(',') !== opened.layers.join(',')) failures.push(`moving the control changed the layers drawn from ${opened.layers.join(',')} to ${moved.layers.join(',')}`);
const shot = join(shots, 'per-tile-7.png');
await page.screenshot({path: shot, timeout: 60_000});

// Opened with `?per-tile=`.
const reopened = asked.length;
await page.goto(withParams(url, {prefetch: 0, 'per-tile': 12}), {waitUntil: 'load'});
await page.locator('#per-tile').waitFor({timeout: 60_000});
if (!(await askedFor(12, reopened))) failures.push('opening with ?per-tile=12 sent no artifact request for 12');
expectInUse('opened with ?per-tile=12', 12, await reading(), reopened);

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
