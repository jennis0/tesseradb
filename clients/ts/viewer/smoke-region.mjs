#!/usr/bin/env node
// Screenshot the region leaf under two principals: a lasso over the places, exact for the shape,
// and a filter to one division.
//
//   node clients/ts/viewer/smoke-region.mjs [--url http://localhost:5173] [--shots DIR]
//     [--headed] [--executable /path/to/chrome] [--principals i,j]
//
// A lasso is a filter leaf on the ordinary viewport request, with no counting request of its own,
// and its count is exact for the shape: the server says so on `x-mosaica-region` and the panel
// renders the count exact. Under a second principal the same lasso counts that principal's items,
// with the same verdict. Filtering to a division narrows the map and every count to its members.
//
// It reports, per principal: the lasso's count and verdict, the request that carried the leaf,
// and for the division the card's count against the region's. It fails if a verdict is not exact,
// if a request carrying a region leaf was a counts-only request of its own, if the panel renders
// the count inexact, or if filtering to the division does not narrow the map.
//
// Everything is read through the components' parts and the store. Requires a running
// `mosaica serve` over a bundle with a `spatial` layer (such as the Overture one-part ladder) and
// a running `vite dev`.
import {mkdir, writeFile} from 'node:fs/promises';
import {join} from 'node:path';
import {flags, isSupersededAbort, launchBrowser, withParams} from './smoke-browser.mjs';

const args = flags();
const url = args.url ?? 'http://localhost:5173';
const shots = args.shots ?? '/tmp/mosaica-region';
await mkdir(shots, {recursive: true});

const browser = await launchBrowser(args);
const page = await browser.newPage({viewport: {width: 1920, height: 1080}});

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

/** Every viewport request: whether it carried a region leaf, and whether it was counts-only. */
const requests = [];
/** The `region` leaf anywhere in a filter expression, or null. */
const regionOf = (expr) => {
  if (!expr || typeof expr !== 'object') return null;
  if (expr.region) return Object.keys(expr.region).find((k) => k !== 'space') ?? null;
  for (const key of ['all_of', 'any_of', 'none_of']) {
    for (const kid of expr[key] ?? []) {
      const found = regionOf(kid);
      if (found) return found;
    }
  }
  return null;
};
page.on('request', (r) => {
  if (!r.url().includes('/v1/viewport')) return;
  try {
    const body = JSON.parse(r.postData() ?? '{}');
    requests.push({leaf: regionOf(body.filters), k: body.k, tiles: Array.isArray(body.tiles)});
  } catch {
    // Not JSON.
  }
});

await page.goto(withParams(url, {prefetch: 0}), {waitUntil: 'load'});

/** Wait until the mark count stops changing. */
const settled = async (limitMs = 60_000) => {
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
};

/** The region as the store and the probe hold it, and the panel's rendered count. */
const region = async () =>
  page.evaluate(() => {
    const p = window.__mosaicaProbe;
    const explorer = /** @type {{store: {get(name: 'region'): {status: string; matched: {value: number; exact: boolean}; visible: {value: number; exact: boolean} | null; verdict: {exact: boolean; depth: number | null} | null; shape: {kind: string}} | null} | null} | null} */ (/** @type {unknown} */ (document.querySelector('mosaica-explorer')));
    const r = explorer?.store?.get('region') ?? null;
    // The panel lives in the explorer's shadow tree, and its count in the panel's.
    const panel = document.querySelector('mosaica-explorer')?.shadowRoot?.querySelector('mosaica-selection') ?? null;
    const count = panel?.shadowRoot?.querySelector('[part="count-matched"]')?.shadowRoot?.querySelector('[part="count"]') ?? null;
    return {
      status: r?.status ?? null,
      kind: r?.shape.kind ?? null,
      matched: r?.matched.value ?? null,
      exact: r?.matched.exact ?? null,
      verdict: r?.verdict ?? null,
      probe: p?.region ?? null,
      view: p?.view ?? null,
      panel: count ? {text: count.textContent?.trim() ?? '', exact: count.getAttribute('data-exact')} : null
    };
  });

const roster = async () =>
  page.evaluate(() => {
    const explorer = /** @type {{store: {get(name: 'meta'): {layers: {name: string; shape: string | null}[]} | null} | null} | null} */ (/** @type {unknown} */ (document.querySelector('mosaica-explorer')));
    return (explorer?.store?.get('meta')?.layers ?? []).map((l) => ({name: l.name, shape: l.shape}));
  });

const served = async () =>
  page.evaluate(() => {
    const explorer = /** @type {{store: {get(name: 'artifacts'): {served: {mosaicaId: bigint; layer: string; box: number[] | null; parentIds: bigint[]; maskedCount: bigint}[]}} | null} | null} */ (/** @type {unknown} */ (document.querySelector('mosaica-explorer')));
    return (explorer?.store?.get('artifacts')?.served ?? []).map((x) => ({id: String(x.mosaicaId), layer: x.layer, box: x.box, parents: x.parentIds.map((p) => String(p)), count: Number(x.maskedCount)}));
  });

/** Tick exactly one layer in the picker, or none. */
const only = async (layerName) => {
  await openLayers();
  const entries = page.locator('mosaica-layer-picker [part="entry"]');
  const n = await entries.count();
  for (let o = 0; o < n; o++) {
    const entry = entries.nth(o);
    const box = entry.locator('input');
    const want = (await entry.getAttribute('data-layer')) === layerName;
    if ((await box.isChecked()) !== want) await box.click();
  }
};

const principal = async (index) => {
  await page.selectOption('#principal', String(index));
  await openLayers();
  await page.locator('mosaica-layer-picker [part="entry"]').first().waitFor({timeout: 60_000});
};

const fitBbox = async (bbox) => {
  await page.evaluate((bbox) => {
    const explorer = /** @type {{map: {fitBbox(extent: [number, number, number, number]): boolean} | null} | null} */ (/** @type {unknown} */ (document.querySelector('mosaica-explorer')));
    explorer?.map?.fitBbox(/** @type {[number, number, number, number]} */ (bbox));
  }, bbox);
  await settled();
};

const select = async (shape) => {
  await page.evaluate((shape) => {
    const explorer = /** @type {{map: {select(shape: unknown): void} | null} | null} */ (/** @type {unknown} */ (document.querySelector('mosaica-explorer')));
    explorer?.map?.select(shape);
  }, shape);
};

/** Wait for the region to be answered: the frame carrying the leaf presented. */
const answered = async () => {
  for (let i = 0; i < 60; i++) {
    const r = await region();
    if (r.status === 'shown' || r.status === 'refused') return r;
    await page.waitForTimeout(500);
  }
  return region();
};

await openLayers();
await page.locator('mosaica-layer-picker [part="entry"]').first().waitFor({timeout: 60_000});
await settled();

const layers = await roster();
const predicateLayer = layers.find((l) => l.shape === 'predicate')?.name ?? null;
const principals = await page.locator('#principal option').count();
const labels = [];
for (let i = 0; i < principals; i++) labels.push((await page.locator('#principal option').nth(i).innerText()).trim());
console.log(`--- principals: ${labels.join(' | ')} ---`);
const pair = args.principals ? args.principals.split(',').map(Number) : [principals - 1, Math.max(0, principals - 2)];
const failures = [];
const results = [];

/**
 * The lasso, in the corpus's data coordinates (the unit-square Web Mercator frame): a jagged
 * shape over central Mexico, drawn once and asked under each principal, so the two counts are
 * of one question.
 */
const VIEW = [0.2, 0.42, 0.26, 0.47];
const LASSO = [
  [0.212, 0.432],
  [0.238, 0.428],
  [0.252, 0.443],
  [0.246, 0.462],
  [0.226, 0.466],
  [0.208, 0.455],
  [0.220, 0.445]
];

let chosenDivision = null;
for (const p of pair) {
  await principal(p);
  const label = labels[p];
  const tag = label.split(' ')[0].toLowerCase().replace(/[^a-z0-9]+/g, '-');
  await only(null);
  await fitBbox(VIEW);

  // 1. The lasso: a filter leaf on the next request, its count exact for the shape.
  const before = requests.length;
  await select({kind: 'lasso', points: LASSO});
  await answered();
  // The first frame carrying the leaf answers the region; later frames fill in the shape's extent,
  // so the count is read once nothing is arriving.
  await settled();
  await page.waitForTimeout(1000);
  const r = await region();
  await page.screenshot({path: join(shots, `lasso-${tag}.png`), timeout: 60_000});
  const carried = requests.slice(before);
  const countsOnlyWithLeaf = carried.filter((q) => q.leaf && q.k === 0 && q.tiles).length;
  results.push({principal: label, what: 'lasso', status: r.status, matched: r.matched, exact: r.exact, verdict: r.verdict, panel: r.panel, requests: carried.length, withLeaf: carried.filter((q) => q.leaf === 'polygon').length, file: `lasso-${tag}.png`});
  if (r.status !== 'shown') failures.push(`${label}: the lasso was ${r.status}`);
  if (!r.verdict?.exact) failures.push(`${label}: the lasso's verdict is ${JSON.stringify(r.verdict)}, not exact`);
  if (r.exact !== true) failures.push(`${label}: the lasso's count is not typed exact (${JSON.stringify(r)})`);
  if (r.panel && r.panel.exact !== 'true') failures.push(`${label}: the panel renders the count inexact (${JSON.stringify(r.panel)})`);
  if (!carried.some((q) => q.leaf === 'polygon')) failures.push(`${label}: no request carried the polygon leaf`);
  if (countsOnlyWithLeaf > 0) failures.push(`${label}: ${countsOnlyWithLeaf} counts-only tiles-form request(s) carried the leaf — the round trip that was to go`);
  await select(null);
  await settled();

  // 2. Filter to this, on a division: the map narrows to its members.
  if (predicateLayer) {
    await only(predicateLayer);
    await fitBbox(VIEW);
    const all = await served();
    const parents = new Set(all.flatMap((a) => a.parents));
    const divisions = all.filter((a) => a.layer === predicateLayer && a.box && !parents.has(a.id));
    const pick = chosenDivision && divisions.some((a) => a.id === chosenDivision) ? chosenDivision : [...divisions].sort((a, b) => b.count - a.count)[0]?.id ?? null;
    if (!pick) {
      console.log(`  ${label}: no division is served in the view`);
    } else {
      chosenDivision = pick;
      const card = all.find((a) => a.id === pick);
      const unfiltered = (await region()).view?.matched ?? -1;
      await page.evaluate((id) => {
        const explorer = /** @type {{store: {needShape(id: bigint): void; openArtifact(id: bigint): Promise<void>} | null; map: {fitTo(id: bigint): boolean} | null} | null} */ (/** @type {unknown} */ (document.querySelector('mosaica-explorer')));
        explorer?.map?.fitTo(BigInt(id));
        explorer?.store?.needShape(BigInt(id));
        void explorer?.store?.openArtifact(BigInt(id));
      }, pick);
      await settled();
      const wide = (await region()).view?.matched ?? -1;
      await page.locator('mosaica-artifact-card [part="filter"]').first().click({timeout: 30_000});
      await answered();
      await settled();
      await page.waitForTimeout(1000);
      const f = await region();
      await page.screenshot({path: join(shots, `filter-${tag}.png`), timeout: 60_000});
      const narrowed = (await region()).view?.matched ?? -1;
      results.push({principal: label, what: 'filter to this', division: pick, cardCount: card?.count ?? null, status: f.status, matched: f.matched, exact: f.exact, verdict: f.verdict, viewBefore: unfiltered, viewFitted: wide, viewAfter: narrowed, file: `filter-${tag}.png`});
      if (f.status !== 'shown' || f.kind !== 'artifact') failures.push(`${label}: filter to this left the region ${f.status} (${f.kind})`);
      if (!(narrowed <= wide)) failures.push(`${label}: filtering to the division did not narrow the map (${wide} → ${narrowed})`);
      if (card && f.matched !== null && f.matched > card.count) failures.push(`${label}: the region's count ${f.matched} exceeds the card's ${card.count}`);
      await select(null);
      await page.evaluate(() => {
        const explorer = /** @type {{store: {clearSelection(): void} | null} | null} */ (/** @type {unknown} */ (document.querySelector('mosaica-explorer')));
        explorer?.store?.clearSelection();
      });
      await settled();
    }
  } else {
    console.log('  no predicate layer is served: the filter-to-this half is skipped');
  }
}

await browser.close();

console.log('--- the region, per principal ---');
for (const r of results) console.log(`  ${JSON.stringify(r)}`);
await writeFile(join(shots, 'region.json'), JSON.stringify({layers, results, requests}, null, 2));

console.log('--- console errors (a superseded request’s abort excepted) ---');
console.log(consoleErrors.length ? consoleErrors.map((e) => `  ${e}`).join('\n') : '  none');
const unexplained = consoleErrors.filter((e) => !isSupersededAbort(e) && !e.includes('ERR_INCOMPLETE_CHUNKED_ENCODING') && !e.includes('decoder closed'));
if (unexplained.length) failures.push(`${unexplained.length} console error(s)`);

if (failures.length) {
  console.log('--- FAILED ---');
  for (const f of failures) console.log(`  ${f}`);
  process.exit(1);
}
console.log('--- ok ---');
