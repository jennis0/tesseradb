#!/usr/bin/env node
// Drive the annotation layer in a headless browser: the same clustering under several principals.
//
//   node clients/ts/viewer/smoke-artifacts.mjs [--url http://localhost:5173] [--shots DIR]
//     [--headed] [--executable /path/to/chrome]
//
// **This is the instrument for the artifacts claim**, which is not "clusters draw" but: one
// clustering, several viewers, and a count beside each cluster that is *that viewer's own* — with
// clusters simply absent for a viewer who is served none of them, and nothing anywhere saying why;
// and, at step 3, that the geometry drawn is the wire's: a hull and a label render under two
// principals, read off the map's probe rather than eyeballed.
//
// It reports, per principal: how many clusters were served, what the same cluster is worth to
// each of them, and how many outlines and labels the map drew. It fails if the counts do not move
// with the mask, if a cluster is served to everyone alike, or if no hull or label rendered.
//
// Everything is read through the components' parts — `tessera-artifact-list [part="item"]` — and
// the probe; never through an id the shadow DOM hides. Requires a running `tessera serve` with a
// published layer (`scripts/publish-clusters.mjs`) and a running `vite dev`.
import {mkdir} from 'node:fs/promises';
import {join} from 'node:path';
import {flags, isSupersededAbort, launchBrowser, withParams} from './smoke-browser.mjs';

const args = flags();
const url = args.url ?? 'http://localhost:5173';
const shots = args.shots ?? '/tmp/tessera-artifacts';
await mkdir(shots, {recursive: true});

// A layer × principal grid is one settle per cell — twenty-five of them on a corpus with five
// layers — so this is the script the headed browser matters most to (`smoke-browser.mjs`).
const browser = await launchBrowser(args);
const page = await browser.newPage({viewport: {width: 1280, height: 800}});
const consoleErrors = [];
page.on('console', (m) => {
  if (m.type() === 'error') consoleErrors.push(m.text());
});
page.on('pageerror', (e) => consoleErrors.push(`pageerror: ${e.message}`));

// Look-ahead off: it issues requests while the view is still, which is every moment this script
// measures in, and none of them are this instrument's business.
await page.goto(withParams(url, {prefetch: 0}), {waitUntil: 'load'});

/** Wait until the mark count stops moving — every figure below is only meaningful once it has. */
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
 * The artifact list, parsed through its parts: how many were served and what each is worth here.
 *
 * Read off the rendered element rather than from the wire, deliberately — what is on screen is
 * what this is checking, and a reader that went to the service directly would pass while the
 * viewer showed something else entirely.
 */
const artifactList = async () =>
  page.locator('tessera-artifact-list').first().evaluate((root) => {
    const scope = root.shadowRoot ?? root;
    const state = scope.querySelector('[part="state"]')?.getAttribute('data-state') ?? null;
    const counts = {};
    for (const item of scope.querySelectorAll('[part="item"]')) {
      const name = item.querySelector('[part="name"]')?.textContent?.trim() ?? '';
      const countEl = item.querySelector('tessera-count');
      const text = (countEl?.shadowRoot ?? countEl)?.querySelector('[part="count"]')?.textContent ?? '';
      const n = Number(text.replaceAll(',', ''));
      if (name && Number.isFinite(n)) counts[name] = n;
    }
    const served = /([\d,]+) clusters?/.exec(scope.textContent ?? '');
    return {state, served: served ? Number(served[1].replaceAll(',', '')) : null, empty: /Nothing in this view/.test(scope.textContent ?? ''), counts};
  });

/**
 * What the map drew of the artifacts, from the probe: outlines, placed labels, the layers on —
 * and how many served artifacts carry a text, read off the explorer's store, since an artifact
 * with no text draws no label (its key is an id, never a name).
 */
const drawn = async () =>
  page.evaluate(() => {
    const p = window.__tesseraProbe;
    const explorer = /** @type {{store: {get(name: 'artifacts'): {served: {content: string[]}[]}} | null} | null} */ (/** @type {unknown} */ (document.querySelector('tessera-explorer')));
    const named = explorer?.store?.get('artifacts').served.filter((a) => (a.content[0] ?? '').length > 0).length ?? 0;
    return p ? {outlines: p.timings.outlines, labels: p.timings.labels, layersOn: p.cluster.layersOn, served: p.cluster.servedIds.length, named} : null;
  });

// The picker is rendered from `/v1/meta`, so nothing can be counted until the first response has
// landed — and an entry exists only where this principal reaches a layer at all.
await page.locator('tessera-layer-picker [part="entry"]').first().waitFor({timeout: 60_000});
await settled();

const layers = await page.locator('tessera-layer-picker [part="entry"]').count();
const principals = await page.locator('#principal option').count();
const results = [];

for (let l = 0; l < layers; l++) {
  const entry = page.locator('tessera-layer-picker [part="entry"]').nth(l);
  const layerName = await entry.getAttribute('data-layer');
  // One layer on at a time: tick this entry, untick the others.
  for (let o = 0; o < layers; o++) {
    const box = page.locator('tessera-layer-picker [part="entry"]').nth(o).locator('input');
    if ((await box.isChecked()) !== (o === l)) await box.click();
  }
  for (let p = 0; p < principals; p++) {
    const label = (await page.locator('#principal option').nth(p).innerText()).trim();
    await page.selectOption('#principal', String(p));
    // A new session's store: the layer choice is re-applied through the picker after meta.
    await page.locator('tessera-layer-picker [part="entry"]').first().waitFor({timeout: 60_000});
    for (let o = 0; o < layers; o++) {
      const box = page.locator('tessera-layer-picker [part="entry"]').nth(o).locator('input');
      if ((await box.isChecked()) !== (o === l)) await box.click();
    }
    await settled();
    // The artifact channel asks once the view settles; give it its own beat.
    await page.waitForTimeout(1500);
    results.push({layer: layerName, principal: label, ...(await artifactList()), drawn: await drawn()});
  }
}

// The pair a reader is meant to put side by side: the same clustering, two principals.
const shotsTaken = [];
for (const p of [Math.max(0, principals - 3), principals - 1]) {
  await page.selectOption('#principal', String(p));
  await page.locator('tessera-layer-picker [part="entry"]').first().waitFor({timeout: 60_000});
  const box = page.locator('tessera-layer-picker [part="entry"]').first().locator('input');
  if (!(await box.isChecked())) await box.click();
  await settled();
  await page.waitForTimeout(1500);
  const label = (await page.locator('#principal option').nth(p).innerText()).trim();
  const file = join(shots, `principal-${p}.png`);
  await page.screenshot({path: file, timeout: 60_000});
  shotsTaken.push({label, file, list: await artifactList(), drawn: await drawn()});
}

await browser.close();

console.log('--- clusters served, by layer and principal ---');
for (const r of results) {
  const sample = Object.entries(r.counts)
    .slice(0, 3)
    .map(([k, v]) => `${k}=${v.toLocaleString()}`)
    .join(' ');
  console.log(
    `  ${(r.layer ?? '?').padEnd(28)} ${r.principal.padEnd(26)} served=${String(r.served ?? (r.empty ? 0 : '?')).padStart(4)}  outlines=${String(r.drawn?.outlines ?? '?').padStart(3)} labels=${String(r.drawn?.labels ?? '?').padStart(3)}  ${sample}`
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
 * What a missing row means, which is **two different things** and must not be conflated: a
 * cluster can be missing from the list because this principal was not served it — the disclosure
 * control working — or because the list stopped at its row limit. Reporting the second as
 * "absent" would manufacture evidence for the very claim this script exists to check.
 */
const LIST_ROWS = 40;
const readingOf = (row, key) => {
  const count = row.counts[key];
  if (count !== undefined) return String(count);
  return (row.served ?? 0) > LIST_ROWS ? 'not listed' : 'absent';
};

for (const [layer, rows] of byLayer) {
  const keys = new Set(rows.flatMap((r) => Object.keys(r.counts)));
  for (const key of [...keys].slice(0, 3)) {
    const across = rows.map((r) => `${r.principal.split(' ')[0]}=${readingOf(r, key)}`);
    console.log(`  ${layer} ${key}: ${across.join('  ')}`);
  }
  const anyKey = [...keys][0];
  if (anyKey) {
    const values = rows.map((r) => r.counts[anyKey]).filter((v) => v !== undefined);
    if (new Set(values).size < 2) failures.push(`${layer}: every principal saw the same count for ${anyKey}`);
    const absentSomewhere = rows.some((r) => readingOf(r, anyKey) === 'absent');
    const presentSomewhere = rows.some((r) => r.counts[anyKey] !== undefined);
    console.log(
      `  => ${layer}: ${anyKey} ${
        absentSomewhere && presentSomewhere
          ? 'is served to one principal and absent for another'
          : 'is served to every principal that lists it — presence differs only under a criterion'
      }`
    );
  }
}
console.log('--- geometry drawn (the wire’s, per principal) ---');
for (const s of shotsTaken) {
  console.log(`  ${s.label.padEnd(26)} ${String(s.list.served ?? 0).padStart(3)} clusters, ${s.drawn?.outlines ?? 0} outlines, ${s.drawn?.labels ?? 0} labels (${s.drawn?.named ?? 0} with a text)  ${s.file}`);
}
// A hull must render under two principals — the geometry is the wire's, derived per principal,
// and a map that drew none would pass every count check while showing a bare field. A label
// renders wherever a served artifact carries a text, and never where none does: a layer whose
// artifacts have keys alone draws no label, since a key is an id.
const drewHull = shotsTaken.filter((s) => (s.drawn?.outlines ?? 0) > 0);
if (drewHull.length < 2) failures.push(`a hull rendered under ${drewHull.length} of 2 principals`);
for (const s of shotsTaken) {
  const named = s.drawn?.named ?? 0;
  const labels = s.drawn?.labels ?? 0;
  if (named > 0 && labels === 0) failures.push(`${s.label}: ${named} served artifacts carry a text and no label rendered`);
  if (named === 0 && labels > 0) failures.push(`${s.label}: no served artifact carries a text and ${labels} labels rendered — a key drawn as a name`);
}
// A run in which no principal was served a count from any layer proves nothing about masking — it
// is what a lost layer selection looks like (found 2026-08-25: the choice was dropped on every
// principal switch and this script still said OK). The list must have read a number somewhere.
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
