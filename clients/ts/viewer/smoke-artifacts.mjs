#!/usr/bin/env node
// Drive the annotation layer in a headless browser: the same clustering under several principals.
//
//   node clients/ts/viewer/smoke-artifacts.mjs [--url http://localhost:5173] [--shots DIR]
//
// **This is the instrument for Stage 2's claim**, which is not "clusters draw" but: one clustering,
// several viewers, and a count beside each cluster that is *that viewer's own* — with clusters
// simply absent for a viewer who is served none of them, and nothing anywhere saying why.
//
// It reports, per layer and per principal: how many clusters were served, what the same cluster is
// worth to each of them, and whether the drill-down returns the number the map is showing. It
// fails if the counts do not move with the mask, if a cluster is served to everyone alike, or if
// the panel and the drill-down disagree.
//
// Requires a running `tessera serve` with a published layer (`scripts/publish-clusters.mjs`) and a
// running `vite dev`.
import {mkdir} from 'node:fs/promises';
import {join} from 'node:path';
import {chromium} from 'playwright';

const args = Object.fromEntries(
  process.argv
    .slice(2)
    .reduce((acc, a, i, all) => (a.startsWith('--') ? [...acc, [a.slice(2), all[i + 1]]] : acc), [])
);
const url = args.url ?? 'http://localhost:5173';
const shots = args.shots ?? '/tmp/tessera-artifacts';
await mkdir(shots, {recursive: true});

const browser = await chromium.launch({
  args: ['--use-gl=swiftshader', '--enable-unsafe-swiftshader', '--disable-gpu-sandbox']
});
const page = await browser.newPage({viewport: {width: 1280, height: 800}});
const consoleErrors = [];
page.on('console', (m) => {
  if (m.type() === 'error') consoleErrors.push(m.text());
});
page.on('pageerror', (e) => consoleErrors.push(`pageerror: ${e.message}`));

// Look-ahead off: it issues requests while the view is still, which is every moment this script
// measures in, and none of them are this instrument's business.
await page.goto(`${url}?prefetch=0`, {waitUntil: 'load'});

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
 * The clusters panel, parsed: how many were served and what each is worth here.
 *
 * Read off the rendered panel rather than from the wire, deliberately — what is on screen is what
 * this is checking, and a reader that went to the service directly would pass while the viewer
 * showed something else entirely.
 */
const clustersPanel = async () =>
  page.evaluate(() => {
    const text = document.getElementById('instruments')?.innerText ?? '';
    const section = text.split('CLUSTERS IN VIEW')[1]?.split(/\n[A-Z][A-Z ]+\n/)[0] ?? '';
    const lines = section.split('\n').map((l) => l.trim()).filter(Boolean);
    const served = /^(\d[\d,]*) served$/.exec(lines[0] ?? '');
    const counts = {};
    for (let i = 0; i < lines.length - 1; i++) {
      if (/^c-\d+$|^#\d+$/.test(lines[i]) && /^[\d,]+$/.test(lines[i + 1])) {
        counts[lines[i]] = Number(lines[i + 1].replaceAll(',', ''));
      }
    }
    return {
      served: served ? Number(served[1].replaceAll(',', '')) : null,
      empty: /nothing served here/.test(section),
      counts
    };
  });

/** The cluster detail panel, after a click. */
const detailPanel = async () =>
  page.evaluate(() => {
    const text = document.getElementById('instruments')?.innerText ?? '';
    if (!/\nCLUSTER\n/.test(`\n${text}`)) return null;
    const section = text.split(/\nCLUSTER\n/)[1] ?? '';
    const key = /\n(c-\d+)\n/.exec(`\n${section}`);
    const count = /([\d,]+) members you can see/.exec(section);
    return {
      key: key ? key[1] : null,
      maskedCount: count ? Number(count[1].replaceAll(',', '')) : null
    };
  });

// The controls are rendered from `/v1/meta`, so nothing can be counted until the first response
// has landed — and the layer select exists only where this principal reaches a layer at all.
await page.waitForSelector('#artifact-layer', {timeout: 60_000});
await settled();

const layers = await page.locator('#artifact-layer option').count();
const principals = await page.locator('#principal option').count();
const results = [];

for (let l = 0; l < layers; l++) {
  const layerValue = await page.locator('#artifact-layer option').nth(l).getAttribute('value');
  if (!layerValue) continue;
  await page.selectOption('#artifact-layer', layerValue);
  for (let p = 0; p < principals; p++) {
    const label = (await page.locator('#principal option').nth(p).innerText()).trim();
    await page.selectOption('#principal', String(p));
    await settled();
    // The artifact channel asks once the view settles; give it its own beat.
    await page.waitForTimeout(1500);
    const panel = await clustersPanel();
    results.push({layer: layerValue, principal: label, ...panel});
  }
}

/**
 * **The click-through is not exercised here, and the reason is the browser rather than the code.**
 * deck.gl's `onClick` does not fire under headless chromium — the same reason the item drill-down
 * has never been driven by `smoke.mjs` either — so a "no cluster opened" result would say nothing
 * about the viewer. What the click leads to is covered where it can be: `core/test/client.live.test.ts`
 * opens a served artifact by identifier against the running server and requires the count to be
 * the one the viewport already carried.
 */
const opened = await detailPanel();

// The pair a reader is meant to put side by side: the same clustering, two principals, on the
// layer whose criterion decides which clusters exist for each of them.
const criterionLayer = (await page.locator('#artifact-layer option').all()).at(-1);
if (criterionLayer) await page.selectOption('#artifact-layer', (await criterionLayer.getAttribute('value')) ?? '');
const shotsTaken = [];
for (const p of [Math.max(0, principals - 3), principals - 1]) {
  await page.selectOption('#principal', String(p));
  await settled();
  await page.waitForTimeout(1500);
  const label = (await page.locator('#principal option').nth(p).innerText()).trim();
  const file = join(shots, `principal-${p}.png`);
  await page.screenshot({path: file, timeout: 60_000});
  shotsTaken.push({label, file, panel: await clustersPanel()});
}

await browser.close();

console.log('--- clusters served, by layer and principal ---');
for (const r of results) {
  const sample = Object.entries(r.counts)
    .slice(0, 3)
    .map(([k, v]) => `${k}=${v.toLocaleString()}`)
    .join(' ');
  console.log(
    `  ${r.layer.padEnd(28)} ${r.principal.padEnd(26)} served=${String(r.served ?? (r.empty ? 0 : '?')).padStart(4)}  ${sample}`
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
 * What a missing row means, which is **two different things** and must not be conflated.
 *
 * The panel lists the largest dozen and says how many more there are, so a cluster can be missing
 * from it either because this principal was not served it — the disclosure control working — or
 * because it is merely the thirteenth. Reporting the second as "absent" would manufacture evidence
 * for the very claim this script exists to check.
 */
const PANEL_ROWS = 12;
const readingOf = (row, key) => {
  const count = row.counts[key];
  if (count !== undefined) return String(count);
  return (row.served ?? 0) > PANEL_ROWS ? 'not in top 12' : 'absent';
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
    if (new Set(values).size < 2) {
      failures.push(`${layer}: every principal saw the same count for ${anyKey}`);
    }
    // Genuinely absent — not merely below the panel's cut — for at least one principal, and served
    // to another: presence itself moving with the mask, which is the half of the claim that counts
    // alone cannot show.
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
console.log('--- drill-down ---');
console.log(
  opened
    ? `  a cluster is open: ${opened.key} = ${opened.maskedCount?.toLocaleString()}`
    : '  not exercised — deck.gl picking does not fire headless; see client.live.test.ts'
);
console.log('--- screenshots ---');
for (const s of shotsTaken) {
  console.log(`  ${s.label.padEnd(26)} ${String(s.panel.served ?? 0).padStart(3)} clusters  ${s.file}`);
}
// A run in which no principal was served a count from any layer proves nothing about masking — it
// is what a lost layer selection looks like (found 2026-08-25: the choice was dropped on every
// principal switch and this script still said OK). The panel must have read a number somewhere.
if (!results.some((r) => r.served !== null)) {
  console.error('ARTIFACT SMOKE FAILED: no principal was served a cluster count from any layer — the panel never showed one');
  process.exit(1);
}
console.log('--- console errors ---');
console.log(consoleErrors.length ? consoleErrors.map((e) => `  ${e}`).join('\n') : '  none');

// A layer with a criterion must hide clusters from someone, or the control is not being exercised.
const criterionRows = results.filter((r) => /min\d+/.test(r.layer));
if (criterionRows.length > 0) {
  const servedCounts = new Set(criterionRows.map((r) => r.served ?? 0));
  if (servedCounts.size < 2) {
    failures.push('the criterion layer served the same number of clusters to every principal');
  }
}
if (consoleErrors.length) failures.push(`${consoleErrors.length} console error(s)`);

if (failures.length) {
  console.error(`ARTIFACT SMOKE FAILED: ${failures.join('; ')}`);
  process.exit(1);
}
console.log('ARTIFACT SMOKE OK');
