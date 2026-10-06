#!/usr/bin/env node
// Drive the annotation layer in a headless browser: the same clustering under several principals.
//
//   node clients/ts/viewer/smoke-artifacts.mjs [--url http://localhost:5173] [--shots DIR]
//     [--headed] [--executable /path/to/chrome]
//
// One clustering, several viewers: the count beside each cluster is that viewer's own, a cluster
// is absent for a viewer who is served none of it, and the served geometry and labels reach the
// map under two principals.
//
// It reports, per principal, how many clusters were served, what the same cluster counts for each,
// and how many outlines and labels the map drew. It fails if the counts do not change with the
// mask, if a cluster is served to everyone alike, or if no geometry or label arrived.
//
// Everything is read through the components' parts, the explorer's store and the probe. Requires a running
// `tessera serve` with a published layer (`scripts/publish-clusters.mjs`) and a running `vite dev`.
import {mkdir} from 'node:fs/promises';
import {join} from 'node:path';
import {flags, isSupersededAbort, launchBrowser, withParams} from './smoke-browser.mjs';

const args = flags();
const url = args.url ?? 'http://localhost:5173';
const shots = args.shots ?? '/tmp/tessera-artifacts';
await mkdir(shots, {recursive: true});

// A layer × principal grid is one settle per cell, so a headed browser helps most here.
const browser = await launchBrowser(args);
const page = await browser.newPage({viewport: {width: 1280, height: 800}});

/** The layer picker sits in the explorer's Layers popover, which a press on the map closes; open it. */
async function openLayers() {
  const toggle = page.locator('[part="layers-toggle"]').first();
  await toggle.waitFor({timeout: 60_000});
  if ((await toggle.getAttribute('aria-expanded')) !== 'true') await toggle.click();
}
const consoleErrors = [];
page.on('console', (m) => {
  if (m.type() === 'error') consoleErrors.push(m.text());
});
page.on('pageerror', (e) => consoleErrors.push(`pageerror: ${e.message}`));

// Look-ahead off: it sends requests while the view is still, when this script measures.
await page.goto(withParams(url, {prefetch: 0}), {waitUntil: 'load'});

/** Wait until the mark count stops changing. */
const settled = async (limitMs = 45_000) => {
  const started = Date.now();
  let last = -1;
  let stable = 0;
  while (Date.now() - started < limitMs) {
    await page.waitForTimeout(500);
    const marks = await page.evaluate(() => window.__tesseraProbe?.marks ?? -1);
    if (marks === last) {
      if (++stable >= 4) return;
    } else {
      stable = 0;
      last = marks;
    }
  }
};

/**
 * The artifacts served, from the explorer's store: the channel's state, how many were served, and
 * each one's count and name, keyed by `tessera_id`, which is the same for every principal.
 */
const artifactList = async () =>
  page.evaluate(() => {
    const explorer = /** @type {{activeStore: {get(name: 'artifacts'): {status: string; served: {tesseraId: bigint; maskedCount: bigint; content: string[]}[]}} | null} | null} */ (/** @type {unknown} */ (document.querySelector('tessera-explorer')));
    const artifacts = explorer?.activeStore?.get('artifacts');
    const counts = {};
    const names = {};
    for (const a of artifacts?.served ?? []) {
      counts[String(a.tesseraId)] = Number(a.maskedCount);
      names[String(a.tesseraId)] = a.content[0] ?? '';
    }
    const served = artifacts?.served.length ?? null;
    return {state: artifacts?.status ?? null, served, empty: served === 0, counts, names};
  });

/**
 * What the map draws of the artifacts, from the probe (outline parts, their artifacts, placed
 * labels, layers on), and from the explorer's store how many served artifacts carry text and how
 * many carry geometry. The outline layer draws only the hovered and opened artifacts, so whether
 * geometry arrived is read from the store.
 */
const drawn = async () =>
  page.evaluate(() => {
    const p = window.__tesseraProbe;
    const explorer = /** @type {{store: {get(name: 'artifacts'): {served: {content: string[]; box: unknown; shape: unknown}[]}} | null} | null} */ (/** @type {unknown} */ (document.querySelector('tessera-explorer')));
    const served = explorer?.store?.get('artifacts').served ?? [];
    const named = served.filter((a) => (a.content[0] ?? '').length > 0).length;
    const withGeometry = served.filter((a) => a.box !== null || a.shape !== null).length;
    return p ? {outlines: p.timings.outlines, outlinesDrawn: p.timings.outlinesDrawn, labels: p.timings.labels, layersOn: p.cluster.layersOn, served: p.cluster.servedIds.length, named, withGeometry} : null;
  });

// The picker renders from `/v1/meta`, with an entry only for layers this principal reaches.
await openLayers();
await page.locator('tessera-layer-picker [part="entry"]').first().waitFor({timeout: 60_000});
await settled();

await openLayers();
const layers = await page.locator('tessera-layer-picker [part="entry"]').count();
const principals = await page.locator('#principal option').count();
const results = [];

for (let l = 0; l < layers; l++) {
  await openLayers();
  const entry = page.locator('tessera-layer-picker [part="entry"]').nth(l);
  const layerName = await entry.getAttribute('data-layer');
  // One layer on at a time: tick this entry, untick the others.
  for (let o = 0; o < layers; o++) {
    await openLayers();
    const box = page.locator('tessera-layer-picker [part="entry"]').nth(o).locator('input');
    if ((await box.isChecked()) !== (o === l)) await box.click();
  }
  for (let p = 0; p < principals; p++) {
    const label = (await page.locator('#principal option').nth(p).innerText()).trim();
    await page.selectOption('#principal', String(p));
    // A new session's store: the layer choice is re-applied through the picker after meta.
    await openLayers();
    await page.locator('tessera-layer-picker [part="entry"]').first().waitFor({timeout: 60_000});
    for (let o = 0; o < layers; o++) {
      await openLayers();
      const box = page.locator('tessera-layer-picker [part="entry"]').nth(o).locator('input');
      if ((await box.isChecked()) !== (o === l)) await box.click();
    }
    await settled();
    // The artifact channel asks once the view settles; give it its own beat.
    await page.waitForTimeout(1500);
    results.push({layer: layerName, principal: label, ...(await artifactList()), drawn: await drawn()});
  }
}

// Screenshots to compare: the same clustering under two principals.
const shotsTaken = [];
for (const p of [Math.max(0, principals - 3), principals - 1]) {
  await page.selectOption('#principal', String(p));
  await openLayers();
  await page.locator('tessera-layer-picker [part="entry"]').first().waitFor({timeout: 60_000});
  await openLayers();
  const box = page.locator('tessera-layer-picker [part="entry"]').first().locator('input');
  if (!(await box.isChecked())) await box.click();
  await settled();
  await page.waitForTimeout(1500);
  const label = (await page.locator('#principal option').nth(p).innerText()).trim();
  const file = join(shots, `principal-${p}.png`);
  await page.screenshot({path: file, timeout: 60_000});
  shotsTaken.push({label, file, list: await artifactList(), drawn: await drawn()});
}

// Opening a cluster draws its outline and no other. Opened through the store, since deck's pick
// does not fire headless.
let openedDrawn = null;
{
  const opened = await page.evaluate(() => {
    const store = /** @type {{activeStore: {get(name: 'artifacts'): {served: {tesseraId: bigint}[]}; openArtifact(id: bigint): Promise<void>} | null} | null} */ (/** @type {unknown} */ (document.querySelector('tessera-explorer')))?.activeStore;
    const first = store?.get('artifacts').served[0];
    if (!store || !first) return false;
    void store.openArtifact(first.tesseraId);
    return true;
  });
  if (opened) {
    await page.waitForTimeout(2000);
    openedDrawn = (await drawn())?.outlinesDrawn ?? null;
  }
}

await browser.close();

console.log('--- clusters served, by layer and principal ---');
for (const r of results) {
  const sample = Object.entries(r.counts)
    .slice(0, 3)
    .map(([k, v]) => `#${k.slice(-6)}=${v.toLocaleString()}`)
    .join(' ');
  console.log(
    `  ${(r.layer ?? '?').padEnd(28)} ${r.principal.padEnd(26)} served=${String(r.served ?? (r.empty ? 0 : '?')).padStart(4)}  shapes=${String(r.drawn?.withGeometry ?? '?').padStart(3)} drawn=${String(r.drawn?.outlinesDrawn ?? '?').padStart(2)} labels=${String(r.drawn?.labels ?? '?').padStart(3)}  ${sample}`
  );
}
console.log('--- the same cluster, across principals ---');
const byLayer = new Map();
for (const r of results) {
  if (!byLayer.has(r.layer)) byLayer.set(r.layer, []);
  byLayer.get(r.layer).push(r);
}
const failures = [];
/**
 * The list's row limit. A cluster missing from the list is either not served to this principal
 * or past the limit, and only the first counts as absent.
 */
const LIST_ROWS = 40;
/** What a row with no text and no attached topic draws. */
const NO_NAME = '\u2014';
const readingOf = (row, key) => {
  const count = row.counts[key];
  if (count !== undefined) return String(count);
  return (row.served ?? 0) > LIST_ROWS ? 'not listed' : 'absent';
};

for (const [layer, rows] of byLayer) {
  const keys = new Set(rows.flatMap((r) => Object.keys(r.counts)));
  for (const key of [...keys].slice(0, 3)) {
    const across = rows.map((r) => `${r.principal.split(' ')[0]}=${readingOf(r, key)}`);
    // A name, where the layer publishes one, is added for the reader; the placeholder is not.
    const named = rows.map((r) => r.names[key]).find((n) => n && n !== NO_NAME);
    console.log(`  ${layer} #${key.slice(-6)}${named ? ` (${named})` : ''}: ${across.join('  ')}`);
  }
  const anyKey = [...keys][0];
  if (anyKey) {
    const values = rows.map((r) => r.counts[anyKey]).filter((v) => v !== undefined);
    const anyName = `#${anyKey.slice(-6)}`;
    if (new Set(values).size < 2) failures.push(`${layer}: every principal saw the same count for ${anyName}`);
    const absentSomewhere = rows.some((r) => readingOf(r, anyKey) === 'absent');
    const presentSomewhere = rows.some((r) => r.counts[anyKey] !== undefined);
    console.log(
      `  => ${layer}: ${anyName} ${
        absentSomewhere && presentSomewhere
          ? 'is served to one principal and absent for another'
          : 'is served to every principal that lists it — presence differs only under a criterion'
      }`
    );
  }
}
console.log(`--- the opened cluster's hull: ${openedDrawn ?? 'not opened'} drawn (in artifacts, not rings) ---`);
console.log('--- geometry held and drawn (the wire’s, per principal) ---');
for (const s of shotsTaken) {
  console.log(`  ${s.label.padEnd(26)} ${String(s.list.served ?? 0).padStart(3)} clusters, ${s.drawn?.withGeometry ?? 0} with geometry, ${s.drawn?.outlines ?? 0} rings drawn over ${s.drawn?.outlinesDrawn ?? 0} artifacts, ${s.drawn?.labels ?? 0} labels (${s.drawn?.named ?? 0} with a text)  ${s.file}`);
}
// The served geometry must reach the client under two principals. None of it draws at rest: only
// the hovered and opened artifacts are outlined. A label renders where a served artifact carries
// text and nowhere else. `outlines` counts parts and `outlinesDrawn` artifacts; the opened-cluster
// check uses artifacts, since one cluster may have several parts.
const heldHull = shotsTaken.filter((s) => (s.drawn?.withGeometry ?? 0) > 0);
if (heldHull.length < 2) failures.push(`served geometry reached the client under ${heldHull.length} of 2 principals`);
for (const s of shotsTaken) {
  if ((s.drawn?.outlinesDrawn ?? 0) > 0) failures.push(`${s.label}: ${s.drawn.outlinesDrawn} shape(s) drawn with nothing hovered or opened`);
}
if (openedDrawn !== 1) failures.push(`opening a cluster drew ${openedDrawn} hull(s), not 1`);
for (const s of shotsTaken) {
  const named = s.drawn?.named ?? 0;
  const labels = s.drawn?.labels ?? 0;
  if (named > 0 && labels === 0) failures.push(`${s.label}: ${named} served artifacts carry a text and no label rendered`);
  if (named === 0 && labels > 0) failures.push(`${s.label}: no served artifact carries a text and ${labels} labels rendered — a key drawn as a name`);
}
// A run in which no principal was served a count from any layer proves nothing about masking,
// and is what a lost layer selection looks like.
if (!results.some((r) => r.served !== null)) {
  console.error('ARTIFACT SMOKE FAILED: no principal was served a cluster count from any layer — the list never showed one');
  process.exit(1);
}
console.log('--- console errors (a superseded request’s abort excepted) ---');
console.log(consoleErrors.length ? consoleErrors.map((e) => `  ${e}`).join('\n') : '  none');
const unexplained = consoleErrors.filter((e) => !isSupersededAbort(e));

// A layer with a criterion must hide clusters from someone, or the control is not being exercised.
const criterionRows = results.filter((r) => /min\d+/.test(r.layer ?? ''));
if (criterionRows.length > 0) {
  const servedCounts = new Set(criterionRows.map((r) => r.served ?? 0));
  if (servedCounts.size < 2) failures.push('the criterion layer served the same number of clusters to every principal');
}
if (unexplained.length) failures.push(`${unexplained.length} console error(s)`);

if (failures.length) {
  console.error(`ARTIFACT SMOKE FAILED: ${failures.join('; ')}`);
  process.exit(1);
}
console.log('ARTIFACT SMOKE OK');
