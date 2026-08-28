#!/usr/bin/env node
// The hover over a cluster under real pointer input (design client-components §5.10), and the
// before/after shots of the contour line.
//
// The owner, on the built map: the hover "is very unstable when moving the mouse, flipping between
// categories and subcategories within it". That is not something a synthetic event finds. A
// response carries a frontier **and its ancestors**; every one of them used to sit in the outline
// layer at zero alpha so that it still answered a pick, so the pointer crossed a parent's
// invisible ring on its way across a child's and deck answered with whichever the picking pass
// found. The claims below drive the mouse through Playwright's input pipeline at a human pace —
// small steps, hover events between — and read the map's own `hoveredArtifact` after each one.
//
//   node clients/ts/harness/hover.mjs [--url http://localhost:5173/?dataset=notebook-2m4]
//                                     [--shots DIR] [--tag before|after] [--executable PATH]
//                                     [--headless]
//
// Headed by default, for the same reason `modes.mjs` is: the pick pass runs on the GPU and the
// display is where it is measured rather than emulated. `--shots DIR` writes the contour shots the
// delivery record carries — one of a wide single-ring cluster, one of a multi-ring one — zoomed to
// the artifact's own extent so the line is legible.
import {mkdirSync} from 'node:fs';
import {chromium} from 'playwright';

const args = Object.fromEntries(process.argv.slice(2).reduce((acc, a, i, all) => (a.startsWith('--') ? [...acc, [a.slice(2), all[i + 1]]] : acc), []));
const url = args.url ?? 'http://localhost:5173/?prefetch=0&dataset=notebook-2m4';
const headless = 'headless' in args;
const executablePath = args.executable;
const shots = args.shots ?? null;
const tag = args.tag ?? 'now';
if (shots) mkdirSync(shots, {recursive: true});

// A page served from an origin the demo server does not enumerate in `serve.dev_cors_origins`
// cannot reach it, and the owner's own viewer holds :5173. The browser's origin check is switched
// off rather than the server's: this is a measuring browser, and the server is not touched.
const sameOrigin = new URL(url).port === '5173';
const flags = ['--disable-gpu-sandbox', ...(sameOrigin ? [] : ['--disable-web-security'])];
const browser = await chromium.launch(
  headless
    ? {args: ['--use-gl=swiftshader', '--enable-unsafe-swiftshader', ...flags], ...(executablePath ? {executablePath} : {})}
    : {headless: false, args: flags, ...(executablePath ? {executablePath} : {})}
);
const page = await browser.newPage({viewport: {width: 1440, height: 900}});
const consoleErrors = [];
page.on('pageerror', (e) => consoleErrors.push(`pageerror: ${e.message}`));

const failures = [];
const passes = [];
const check = (claim, ok, evidence) => {
  (ok ? passes : failures).push(`${claim} — ${evidence}`);
  console.log(`  ${ok ? 'ok  ' : 'FAIL'} ${claim} — ${evidence}`);
};

await page.goto(url, {waitUntil: 'load'});
const map = page.locator('tessera-map').first();

/** The served artifacts, as the page holds them: identity, lineage, shape and extent. */
const servedArtifacts = () =>
  map.evaluate((el) => {
    const m = /** @type {any} */ (el);
    const a = m.activeStore?.get('artifacts');
    if (!a) return null;
    const G = 2 ** 32 / 512;
    return a.served.map((x) => ({
      id: String(x.tesseraId),
      parent: x.parentId === null ? null : String(x.parentId),
      layer: x.layer,
      count: Number(x.maskedCount),
      rings: (x.hull ?? []).filter((r) => r.length >= 3).map((r) => r.map(([px, py]) => [px / G, py / G])),
      vertices: (x.hull ?? []).reduce((n, r) => n + r.length, 0)
    }));
  });

/** The map's own answer to "what is under the pointer". */
const hovered = () =>
  map.evaluate((el) => {
    const m = /** @type {any} */ (el);
    return m.hoveredArtifact === null || m.hoveredArtifact === undefined ? null : String(m.hoveredArtifact);
  });

/** World points to canvas pixels, through the live viewport. */
const project = (points) => map.evaluate((el, pts) => pts.map((p) => /** @type {any} */ (el).deck.getViewports()[0].project(p)), points);

const cursor = {x: 20, y: 20};
/** A human-paced move: `n` small steps with the hover events a hand produces, sampling as it goes. */
const glide = async (x, y, n = 24, ms = 12) => {
  const seen = [];
  for (let i = 1; i <= n; i++) {
    await page.mouse.move(cursor.x + ((x - cursor.x) * i) / n, cursor.y + ((y - cursor.y) * i) / n);
    await page.waitForTimeout(ms);
    seen.push(await hovered());
  }
  cursor.x = x;
  cursor.y = y;
  return seen;
};

/** The observed sequence as runs of one answer — `[[id, length], …]`. */
const runs = (seen) => {
  const out = [];
  for (const v of seen) {
    if (out.length > 0 && out[out.length - 1][0] === v) out[out.length - 1][1] += 1;
    else out.push([v, 1]);
  }
  return out;
};
/** A **flip**: an answer that comes back after the hover had left it. The owner's complaint, counted. */
const flips = (seen) => runs(seen).length - new Set(seen).size;
const show = (seen) =>
  runs(seen)
    .map(([v, n]) => `${v === null ? '—' : v.slice(0, 6)}×${n}`)
    .join(' → ');

// Let the first marks land: every gesture below is against a drawn map.
{
  const started = Date.now();
  while (Date.now() - started < 90_000) {
    if ((await page.evaluate(() => window.__tesseraProbeOf?.()?.marks ?? window.__tesseraProbe?.marks ?? 0)) > 0) break;
    await page.waitForTimeout(200);
  }
  await page.waitForTimeout(2500);
}

const served = await servedArtifacts();
if (!served) {
  console.error('HOVER FAILED — the page serves no artifacts; is the cluster layer on?');
  await browser.close();
  process.exit(1);
}
/** The frontier oracle: a served artifact with no served child. Computed here, not read off the map. */
const parents = new Set(served.map((x) => x.parent).filter(Boolean));
const frontier = new Set(served.filter((x) => !parents.has(x.id)).map((x) => x.id));
const shaped = served.filter((x) => x.rings.length > 0);
console.log(`--- ${served.length} served, ${frontier.size} on the frontier, ${shaped.length} carrying a hull ---`);

/** The subject the shots are taken of: the widest single-ring frontier cluster, and the multi-ring one. */
const onFrontier = shaped.filter((x) => frontier.has(x.id));
const wide = [...onFrontier].sort((a, b) => b.vertices - a.vertices || b.count - a.count)[0];
const many = [...onFrontier].sort((a, b) => b.rings.length - a.rings.length || b.vertices - a.vertices)[0];
/** A frontier cluster with a served ancestor — the nested case the hover used to flip across. */
const nested = onFrontier.filter((x) => x.parent && served.some((y) => y.id === x.parent)).sort((a, b) => b.vertices - a.vertices)[0];

/** Put the camera on one artifact's own extent, so the contour is drawn large. */
const focus = async (subject) => {
  await map.evaluate((el, id) => /** @type {any} */ (el).fitTo(BigInt(id)), subject.id);
  await page.waitForTimeout(2500);
};

/** A canvas point inside the subject's shape: its rings' vertices, pulled toward their own centre. */
const interior = async (subject) => {
  const ring = [...subject.rings].sort((a, b) => b.length - a.length)[0];
  const centre = ring.reduce((acc, p) => [acc[0] + p[0] / ring.length, acc[1] + p[1] / ring.length], [0, 0]);
  const inward = ring.map((p) => [p[0] + (centre[0] - p[0]) * 0.45, p[1] + (centre[1] - p[1]) * 0.45]);
  const screen = await project([centre, ...inward]);
  for (const [x, y] of screen) {
    if (x < 8 || y < 8 || x > 1432 || y > 892) continue;
    await page.mouse.move(x, y);
    cursor.x = x;
    cursor.y = y;
    await page.waitForTimeout(90);
    if ((await hovered()) === subject.id) return [x, y];
  }
  return null;
};

console.log('--- the shots ---');
for (const [name, subject] of [['wide', wide], ['multi-ring', many]]) {
  if (!subject) continue;
  await focus(subject);
  const at = await interior(subject);
  console.log(`  ${name}: ${subject.id.slice(0, 8)} — ${subject.rings.length} ring(s), ${subject.vertices} vertices, ${subject.count.toLocaleString('en-GB')} members; hovered at ${at ? at.map(Math.round).join(',') : 'nowhere'}`);
  if (shots && at) await page.screenshot({path: `${shots}/contour-${tag}-${name}.png`});
}

console.log('--- the hover ---');

// 1. Only what is drawn answers a hover. An ancestor is served alongside its children and nothing
//    is drawn for it, so pointing at one must not reach it.
await focus(nested ?? wide);
const wander = [];
{
  const box = await map.boundingBox();
  const path = [[0.3, 0.3], [0.7, 0.35], [0.65, 0.7], [0.35, 0.65], [0.5, 0.5]];
  for (const [fx, fy] of path) wander.push(...(await glide(box.x + box.width * fx, box.y + box.height * fy, 18)));
}
const offFrontier = [...new Set(wander.filter((v) => v !== null && !frontier.has(v)))];
check(
  'only the drawn frontier answers a hover',
  offFrontier.length === 0,
  `${wander.length} pointer positions over four glides, ${new Set(wander.filter(Boolean)).size} distinct answers, ${offFrontier.length} of them not on the frontier${offFrontier.length ? ` (${offFrontier.map((v) => v.slice(0, 8)).join(', ')})` : ''}`
);

// 2. Crossing a boundary changes the answer once, and does not change back. This is the flip the
//    owner saw: in and out of a child while inside its parent's invisible ring.
await focus(wide);
{
  const inside = await interior(wide);
  const box = await map.boundingBox();
  const outside = [box.x + 12, box.y + 12];
  await page.mouse.move(outside[0], outside[1]);
  cursor.x = outside[0];
  cursor.y = outside[1];
  await page.waitForTimeout(150);
  const crossing = inside ? await glide(inside[0], inside[1], 40) : [];
  check(
    'a crossing changes the answer at most once, and never changes back',
    inside !== null && flips(crossing) === 0 && crossing[crossing.length - 1] === wide.id,
    `${crossing.length} positions: ${show(crossing)}; ${flips(crossing)} flip(s)`
  );
}

// 3. A few pixels never change the answer. A hand does not hold still, and the hover must.
{
  const at = await interior(wide);
  const jitter = [];
  for (let i = 0; i < 16 && at; i++) {
    const dx = Math.cos((i * Math.PI) / 4) * 3;
    const dy = Math.sin((i * Math.PI) / 4) * 3;
    await page.mouse.move(at[0] + dx, at[1] + dy);
    await page.waitForTimeout(40);
    jitter.push(await hovered());
  }
  cursor.x = at ? at[0] : cursor.x;
  cursor.y = at ? at[1] : cursor.y;
  check(
    'a few pixels of movement inside one cluster never change the answer',
    at !== null && new Set(jitter).size === 1 && jitter[0] === wide.id,
    at ? `16 moves within 3 px: ${show(jitter)}` : 'no interior point found'
  );
}

// 4. The nested case: a child inside a served parent's ring, crossed twice. The parent must never
//    answer, and the child must not be given up and taken back.
if (nested) {
  await focus(nested);
  const at = await interior(nested);
  const box = await map.boundingBox();
  const out = [box.x + box.width * 0.5, box.y + 10];
  await page.mouse.move(out[0], out[1]);
  cursor.x = out[0];
  cursor.y = out[1];
  await page.waitForTimeout(150);
  const there = at ? await glide(at[0], at[1], 30) : [];
  const back = at ? await glide(out[0], out[1], 30) : [];
  const seen = [...there, ...back];
  check(
    'a nested region: crossing in and out never answers with the ancestor',
    at !== null && !seen.includes(nested.parent),
    `child ${nested.id.slice(0, 8)} under parent ${String(nested.parent).slice(0, 8)}; in: ${show(there)}; out: ${show(back)}`
  );
  check(
    'a nested region: in and back out is two changes, not a flicker',
    at !== null && runs(there).length <= 2 && runs(back).length <= 2,
    `${runs(there).length} run(s) in, ${runs(back).length} run(s) out`
  );
} else {
  check('a nested region: crossing in and out never answers with the ancestor', false, 'no frontier artifact with a served parent on this corpus');
}

await browser.close();
if (consoleErrors.length) failures.push(...consoleErrors);
if (failures.length) {
  console.error(`HOVER FAILED (${passes.length} ok, ${failures.length} failed):\n  ${failures.join('\n  ')}`);
  process.exit(1);
}
console.log(`HOVER OK — ${passes.length} claims hold`);
