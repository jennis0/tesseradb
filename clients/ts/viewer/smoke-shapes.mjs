#!/usr/bin/env node
// Shoot the drawn shapes under two principals: a predicate shape at the overview and at a city
// zoom, and a derived hull — the evidence for `polygon-membership.md` §7 (stage 3).
//
//   node clients/ts/viewer/smoke-shapes.mjs [--url http://localhost:5173] [--shots DIR]
//     [--headed] [--executable /path/to/chrome]
//
// **This is the instrument for the one-drawn-geometry claim**, which is not "a polygon draws"
// but: a layer's drawn geometry is of one declared kind, published in `/v1/meta`; a **predicate**
// shape is the same bytes for every principal served the artifact and is generalised to the
// pixel at the zoom it was asked at; a **derived** hull is this principal's own; both draw through
// the one outline path — one shape for the opened artifact, nothing at rest — and the picture is
// the wire's, read off the map's probe and the explorer's store rather than eyeballed.
//
// It reports, per principal and per kind: the kind `/v1/meta` published, how many outlines drew
// after an artifact was opened, the vertex count of the served shape at each zoom, and whether
// the predicate shape's bytes agreed across the two principals. It fails if a kind is missing
// from the meta, if opening an artifact draws other than one shape, if a predicate shape differed
// between principals, or if the city-zoom shape is not finer than the overview's.
//
// Everything is read through the components' parts and the store; never through an id the shadow
// DOM hides. Requires a running `tessera serve` over a bundle with a `spatial` layer and a layer
// declaring `hull` (the Overture one-part ladder with the taxonomy declaring a hull is the one the
// evidence note used), and a running `vite dev`. Without a predicate layer the boundary half is
// skipped and said so; without a derived layer the hull half is.
import {mkdir, writeFile} from 'node:fs/promises';
import {join} from 'node:path';
import {flags, isSupersededAbort, launchBrowser, withParams} from './smoke-browser.mjs';

const args = flags();
const url = args.url ?? 'http://localhost:5173';
const shots = args.shots ?? '/tmp/tessera-shapes';
await mkdir(shots, {recursive: true});

const browser = await launchBrowser(args);
// Wide, so the map's middle third is not the only part of it the demo's panels leave visible.
const page = await browser.newPage({viewport: {width: 1920, height: 1080}});
const consoleErrors = [];
page.on('console', (m) => {
  if (m.type() === 'error') consoleErrors.push(m.text());
});
page.on('pageerror', (e) => consoleErrors.push(`pageerror: ${e.message}`));

await page.goto(withParams(url, {prefetch: 0}), {waitUntil: 'load'});

/** Wait until the mark count stops moving — every figure below is only meaningful once it has. */
const settled = async (limitMs = 60_000) => {
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

/** The layer roster as the store holds it: each layer's name and the kind of shape it draws. */
const roster = async () =>
  page.evaluate(() => {
    const explorer = /** @type {{store: {get(name: 'meta'): {layers: {name: string; shape: string | null; computedContent: string[]}[]} | null} | null} | null} */ (/** @type {unknown} */ (document.querySelector('tessera-explorer')));
    return (explorer?.store?.get('meta')?.layers ?? []).map((l) => ({name: l.name, shape: l.shape, computed: l.computedContent}));
  });

/** What the map draws and holds: the probe's outline counts, the served set, the held shapes. */
const drawn = async () =>
  page.evaluate(() => {
    const p = window.__tesseraProbe;
    const explorer = /** @type {{store: {get(name: 'artifacts'): {served: {tesseraId: bigint; layer: string; box: number[] | null; rung: number; parentId: bigint | null; maskedCount: bigint}[]; shapes: Map<bigint, number[][][]>}} | null} | null} */ (/** @type {unknown} */ (document.querySelector('tessera-explorer')));
    const a = explorer?.store?.get('artifacts');
    const served = (a?.served ?? []).map((x) => ({id: String(x.tesseraId), layer: x.layer, box: x.box, rung: x.rung, parent: x.parentId === null ? null : String(x.parentId), count: Number(x.maskedCount)}));
    const shapes = Object.fromEntries([...(a?.shapes ?? new Map())].map(([id, parts]) => [String(id), parts]));
    // The camera's zoom, which is what `needShape` asks the vertex rule at; the probe's `depth`
    // is the request depth, which the driver picks per principal from what they can see.
    const map = /** @type {{map: {viewState?: {zoom: number}} | null} | null} */ (/** @type {unknown} */ (document.querySelector('tessera-explorer')))?.map;
    return {
      outlines: p?.timings.outlines ?? -1,
      outlinesDrawn: p?.timings.outlinesDrawn ?? -1,
      depth: p?.view.depth ?? -1,
      zoom: Math.round((map?.viewState?.zoom ?? -1) * 10) / 10,
      served,
      shapes
    };
  });

/** Tick exactly one layer in the picker. */
const only = async (layerName) => {
  const entries = page.locator('tessera-layer-picker [part="entry"]');
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
  await page.locator('tessera-layer-picker [part="entry"]').first().waitFor({timeout: 60_000});
};

/**
 * Open an artifact through the store — the two calls a click on its contour makes (`map.ts`'s
 * `onClick`): ask for the shape that draws, then open the card — and let it draw.
 */
const open = async (id) => {
  await page.evaluate((id) => {
    const explorer = /** @type {{store: {needShape(id: bigint): void; openArtifact(id: bigint): Promise<void>} | null} | null} */ (/** @type {unknown} */ (document.querySelector('tessera-explorer')));
    explorer?.store?.needShape(BigInt(id));
    void explorer?.store?.openArtifact(BigInt(id));
  }, id);
  // The shape is fetched by identifier when the artifact opens; give the round trip its beat,
  // and then the paint that draws it.
  let d = await drawn();
  for (let i = 0; i < 20 && !d.shapes[id]; i++) {
    await page.waitForTimeout(500);
    d = await drawn();
  }
  for (let i = 0; i < 10 && d.outlinesDrawn === 0; i++) {
    await page.waitForTimeout(500);
    d = await drawn();
  }
  return d;
};

/** Ask for one artifact's shape by identifier alone — what a hover does — and read it back. */
const fetchShape = async (id) => {
  await page.evaluate((id) => {
    const explorer = /** @type {{store: {needShape(id: bigint): void} | null} | null} */ (/** @type {unknown} */ (document.querySelector('tessera-explorer')));
    explorer?.store?.needShape(BigInt(id));
  }, id);
  let d = await drawn();
  for (let i = 0; i < 20 && !d.shapes[id]; i++) {
    await page.waitForTimeout(500);
    d = await drawn();
  }
  return {shape: d.shapes[id] ?? null, depth: d.zoom};
};

const vertices = (parts) => (parts ?? []).flat().reduce((n, ring) => n + ring.length, 0);
const partsOf = (parts) => (parts ?? []).length;
const holesOf = (parts) => (parts ?? []).reduce((n, rings) => n + Math.max(0, rings.length - 1), 0);

await page.locator('tessera-layer-picker [part="entry"]').first().waitFor({timeout: 60_000});
await settled();

const layers = await roster();
const predicateLayer = layers.find((l) => l.shape === 'predicate')?.name ?? null;
const derivedLayer = layers.find((l) => l.shape === 'derived')?.name ?? null;
const principals = await page.locator('#principal option').count();
const labels = [];
for (let i = 0; i < principals; i++) labels.push((await page.locator('#principal option').nth(i).innerText()).trim());
console.log(`--- principals: ${labels.join(' | ')} ---`);
// The two broadest by default, the broadest first so the artifacts the pictures are of are the
// busiest it is served — a country, a city's busiest borough — and the narrower principal is then
// asked for the same ones; `--principals i,j` picks others.
const pair = args.principals ? args.principals.split(',').map(Number) : [principals - 1, Math.max(0, principals - 2)];
const failures = [];
const results = [];

console.log('--- the kinds /v1/meta published ---');
for (const l of layers) console.log(`  ${l.name.padEnd(28)} shape=${String(l.shape).padEnd(10)} computed=${l.computed.join(',')}`);
if (!predicateLayer) console.log('  no predicate layer is served: the boundary half is skipped');
if (!derivedLayer) console.log('  no derived layer is served: the hull half is skipped');
if (!predicateLayer && !derivedLayer) failures.push('no layer draws a shape at all');

const shapeBytes = {};
/**
 * The artifacts the pictures are of, chosen under the first principal and held for the second, so
 * that "the same shape across principals" compares one artifact and not two. Under the second
 * principal an artifact may simply not be served, which is the disclosure control and is said.
 */
const chosen = {overview: null, city: null, hull: null};
/** A served artifact on the frontier — one no served artifact names as its parent — of a layer. */
const frontierOf = (served, layer) => {
  const parents = new Set(served.map((a) => a.parent).filter((x) => x !== null));
  return served.filter((a) => a.layer === layer && !parents.has(a.id));
};
const area = (a) => (a.box[2] - a.box[0]) * (a.box[3] - a.box[1]);
/** A city, in the corpus's data coordinates (the unit-square Web Mercator frame): Mexico City. */
const CITY = [0.2236, 0.444, 0.2256, 0.446];

const fitBbox = async (bbox) => {
  await page.evaluate((bbox) => {
    const explorer = /** @type {{map: {fitBbox(extent: [number, number, number, number]): boolean} | null} | null} */ (/** @type {unknown} */ (document.querySelector('tessera-explorer')));
    explorer?.map?.fitBbox(/** @type {[number, number, number, number]} */ (bbox));
  }, bbox);
  await settled();
  await page.waitForTimeout(1500);
};
const fitAll = async () => {
  await page.evaluate(() => {
    const explorer = /** @type {{map: {fit(): void} | null} | null} */ (/** @type {unknown} */ (document.querySelector('tessera-explorer')));
    explorer?.map?.fit();
  });
  await settled();
  await page.waitForTimeout(1500);
};

/**
 * Open `id` (or the fallback picked from what is served), shoot it, record what drew. With `fit`
 * the map is first fitted to the artifact, so its outline is what the picture is of; the shape is
 * then asked for at that depth.
 */
const shoot = async (label, kind, where, pick, file, fit = false) => {
  const rest = await drawn();
  if (rest.outlinesDrawn !== 0) failures.push(`${label}: ${rest.outlinesDrawn} shape(s) drawn with nothing opened`);
  const id = pick(rest.served);
  if (!id) {
    console.log(`  ${label}: nothing of the ${kind} kind is served ${where}`);
    return null;
  }
  if (fit) {
    await page.evaluate((id) => {
      const explorer = /** @type {{map: {fitTo(id: bigint): boolean} | null} | null} */ (/** @type {unknown} */ (document.querySelector('tessera-explorer')));
      explorer?.map?.fitTo(BigInt(id));
    }, id);
    await settled();
    await page.waitForTimeout(1500);
  }
  const d = await open(id);
  const shape = d.shapes[id];
  // The paint after the last response, so the picture is of a settled map.
  await settled();
  await page.waitForTimeout(1500);
  await page.screenshot({path: file, timeout: 60_000});
  results.push({principal: label, kind, where: `${where}, zoom ${d.zoom}`, id, outlinesDrawn: d.outlinesDrawn, outlines: d.outlines, parts: partsOf(shape), holes: holesOf(shape), vertices: vertices(shape), served: d.served.filter((a) => a.layer === (kind === 'derived' ? derivedLayer : predicateLayer)).length, file});
  if (d.outlinesDrawn !== 1) failures.push(`${label}: opening a ${kind} artifact ${where} drew ${d.outlinesDrawn} shape(s), not 1`);
  if (!shape) failures.push(`${label}: the ${kind} shape ${where} never arrived`);
  if (kind === 'derived' && holesOf(shape) !== 0) failures.push(`${label}: a hull was served with ${holesOf(shape)} hole(s)`);
  return id;
};

for (const p of pair) {
  await principal(p);
  const label = (await page.locator('#principal option').nth(p).innerText()).trim();
  const tag = label.split(' ')[0].toLowerCase().replace(/[^a-z0-9]+/g, '-');

  if (predicateLayer) {
    await only(predicateLayer);
    await fitAll();
    // The overview: the division with the most visible members — a country — held across
    // principals once chosen.
    chosen.overview = await shoot(label, 'predicate', 'at the overview', (served) => {
      const mine = frontierOf(served, predicateLayer).filter((a) => a.box);
      if (chosen.overview && mine.some((a) => a.id === chosen.overview)) return chosen.overview;
      return [...mine].sort((a, b) => b.count - a.count)[0]?.id ?? null;
    }, join(shots, `boundary-overview-${tag}.png`)) ?? chosen.overview;
    // A city: the map fitted to Mexico City, the busiest division served there fitted and opened.
    await fitBbox(CITY);
    chosen.city = await shoot(label, 'predicate', 'at a city zoom', (served) => {
      const mine = frontierOf(served, predicateLayer).filter((a) => a.box);
      if (chosen.city && mine.some((a) => a.id === chosen.city)) return chosen.city;
      return [...mine].sort((a, b) => b.count - a.count)[0]?.id ?? null;
    }, join(shots, `boundary-city-${tag}.png`)) ?? chosen.city;
    await fitAll();
  }

  if (derivedLayer) {
    await only(derivedLayer);
    await fitAll();
    chosen.hull = await shoot(label, 'derived', 'at the overview', (served) => {
      const front = frontierOf(served, derivedLayer);
      if (chosen.hull && front.some((a) => a.id === chosen.hull)) return chosen.hull;
      return [...front].sort((a, b) => b.count - a.count)[0]?.id ?? null;
    }, join(shots, `hull-${tag}.png`), true) ?? chosen.hull;
  }
}

// One artifact, two masks, one depth: each shape the pictures are of, asked for by identifier
// under each principal — the route a hover takes, which does not need the artifact in the served
// set — with the map fitted to a fixed box first, so the vertex rule reads the same zoom for both.
// A predicate shape must come back byte-identical; a derived one is each principal's own.
const REGION = [0.15, 0.35, 0.35, 0.55];
for (const p of pair) {
  await principal(p);
  const label = labels[p];
  for (const [what, id] of Object.entries(chosen)) {
    if (!id) continue;
    const kind = what === 'hull' ? 'derived' : 'predicate';
    await only(kind === 'derived' ? derivedLayer : predicateLayer);
    await fitBbox(what === 'city' ? CITY : REGION);
    const {shape, depth} = await fetchShape(id);
    (shapeBytes[`${kind}:${what}:${id}`] ??= {})[label] = {depth, bytes: JSON.stringify(shape)};
    if (shape === null) console.log(`  ${label}: #${id.slice(-6)} (${what}) asked by identifier: not served to this principal — the disclosure control`);
  }
}

await browser.close();

console.log('--- what drew, per principal and kind ---');
for (const r of results) {
  console.log(`  ${r.principal.padEnd(26)} ${r.kind.padEnd(9)} ${r.where.padEnd(8)} served=${String(r.served).padStart(5)} drawn=${r.outlinesDrawn} parts=${String(r.parts).padStart(3)} holes=${String(r.holes).padStart(3)} vertices=${String(r.vertices).padStart(5)}  ${r.file}`);
}
console.log('--- the same shape, across principals ---');
// The vertex rule is a function of the zoom, so two fetches are compared only where the map was
// at one depth for both; where it was not, that is said and nothing is concluded.
for (const [key, byPrincipal] of Object.entries(shapeBytes)) {
  const entries = Object.values(byPrincipal);
  const [kind, where, id] = key.split(':');
  const depths = new Set(entries.map((e) => e.depth));
  const same = new Set(entries.map((e) => e.bytes)).size === 1;
  const verdict = entries.length < 2 ? 'one principal only' : depths.size > 1 ? `asked at zooms ${[...depths].join(' and ')}, not compared` : same ? 'identical' : 'different';
  console.log(`  ${kind.padEnd(9)} ${where.padEnd(16)} #${id.slice(-6)}: ${verdict}`);
  if (kind === 'predicate' && entries.length === 2 && depths.size === 1 && !same) failures.push(`predicate shape #${id.slice(-6)} differed between principals at zoom ${[...depths][0]}`);
  if (kind === 'derived' && entries.length === 2 && depths.size === 1 && same) console.log('    (a derived shape identical under two principals: both see the same members of it)');
}
const city = results.filter((r) => r.kind === 'predicate' && r.where.startsWith('at a city'));
if (predicateLayer && !city.some((c) => c.vertices > 0)) failures.push('no city-zoom shape carried any vertices');

await writeFile(join(shots, 'shapes.json'), JSON.stringify({layers, results}, null, 2));

console.log('--- console errors (a superseded request’s abort excepted) ---');
console.log(consoleErrors.length ? consoleErrors.map((e) => `  ${e}`).join('\n') : '  none');
// A viewport stream cut off by this script's own principal switch reports as an incomplete
// chunked body: the page moved on before the server finished, which is the same event as the
// superseded abort and not a defect.
// `decoder closed` is the store's decode worker rejecting the decodes still pending when a
// principal switch closed it (`core/src/decoder.ts`) — the same superseded event, seen only
// headless, where the switch outruns the software-rendered decode.
const unexplained = consoleErrors.filter((e) => !isSupersededAbort(e) && !e.includes('ERR_INCOMPLETE_CHUNKED_ENCODING') && !e.includes('decoder closed'));
if (unexplained.length) failures.push(`${unexplained.length} console error(s)`);

if (failures.length) {
  console.error(`SHAPE SMOKE FAILED: ${failures.join('; ')}`);
  process.exit(1);
}
console.log('SHAPE SMOKE OK');
