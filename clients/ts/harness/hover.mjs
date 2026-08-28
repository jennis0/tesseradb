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
const clock = Date.now();
const elapsed = () => `${((Date.now() - clock) / 1000).toFixed(0)}s`;
const check = (claim, ok, evidence) => {
  (ok ? passes : failures).push(`${claim} — ${evidence}`);
  console.log(`  ${ok ? 'ok  ' : 'FAIL'} [${elapsed()}] ${claim} — ${evidence}`);
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
const glide = async (x, y, n = 24, ms = 8) => {
  const seen = [];
  const from = [cursor.x, cursor.y];
  for (let i = 1; i <= n; i++) {
    const at = [from[0] + ((x - from[0]) * i) / n, from[1] + ((y - from[1]) * i) / n];
    await page.mouse.move(at[0], at[1]);
    await page.waitForTimeout(ms);
    seen.push({at, id: await hovered()});
  }
  cursor.x = x;
  cursor.y = y;
  return seen;
};

/** The observed sequence as runs of one answer — `[[id, length], …]`. */
const runs = (samples) => {
  const out = [];
  for (const v of samples.map((s) => (typeof s === 'object' && s !== null && 'id' in s ? s.id : s))) {
    if (out.length > 0 && out[out.length - 1][0] === v) out[out.length - 1][1] += 1;
    else out.push([v, 1]);
  }
  return out;
};
const show = (samples) =>
  runs(samples)
    .map(([v, n]) => `${v === null ? '—' : v.slice(0, 6)}×${n}`)
    .join(' → ');

// Wait for a drawn map with a served set, not merely for a loaded page. `?dataset=` opens the
// demo's default corpus and then switches, so marks arriving is not the same event as the corpus
// under test being ready, and the absorb that follows the switch holds the main thread long enough
// that an evaluate against it times out.
// The wait goes through the locator, not `document.querySelector`: the map is inside the
// explorer's shadow root, where a page-level query does not reach it.
page.setDefaultTimeout(180_000);
{
  const started = Date.now();
  while (Date.now() - started < 180_000) {
    const served = await map.evaluate((el) => /** @type {any} */ (el).activeStore?.get('artifacts')?.served.length ?? 0);
    const marks = await page.evaluate(() => window.__tesseraProbeOf?.()?.marks ?? window.__tesseraProbe?.marks ?? 0);
    if (served > 0 && marks > 0) break;
    await page.waitForTimeout(400);
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
console.log(`--- [${elapsed()}] ${served.length} served, ${frontier.size} on the frontier, ${shaped.length} carrying a hull ---`);

/** The subjects the shots are taken of: the widest single-ring frontier cluster, and the multi-ring one. */
const onFrontier = shaped.filter((x) => frontier.has(x.id));
// A shape whose whole extent fits a viewport and has corners to look at: the widest single ring
// among the middling clusters. The corpus's largest cluster is half the map and its contour runs
// off every edge, which shows nothing about the line.
const single = onFrontier.filter((x) => x.rings.length === 1 && x.count >= 4_000 && x.count <= 80_000);
const wide = [...(single.length > 0 ? single : onFrontier)].sort((a, b) => b.vertices - a.vertices || b.count - a.count)[0];
const many = [...onFrontier].sort((a, b) => b.rings.length - a.rings.length || b.vertices - a.vertices)[0];
/** A frontier cluster with a served ancestor — the nested case the hover used to flip across. */
const nested = onFrontier.filter((x) => x.parent && served.some((y) => y.id === x.parent)).sort((a, b) => b.vertices - a.vertices)[0];

/**
 * Put the camera on one artifact's own extent, so the contour is drawn large.
 *
 * Through the whole extent first: `fitTo` reads the store's `extentOf`, which knows only what is
 * served for the view it is standing in, so fitting one cluster and then another fits the second
 * against a served set that no longer holds it and the camera stays where it was.
 */
const focus = async (subject) => {
  await map.evaluate((el) => /** @type {any} */ (el).fit());
  await page.waitForTimeout(1200);
  const ok = await map.evaluate((el, id) => /** @type {any} */ (el).fitTo(BigInt(id)), subject.id);
  await page.waitForTimeout(2000);
  if (!ok) console.log(`  note: the store holds no extent for ${subject.id.slice(0, 8)}; the camera did not move`);
  return ok;
};

/** Whether a point is inside a closed ring — an even-odd crossing count, as the client's is. */
const inRing = (p, ring) => {
  let odd = false;
  for (let i = 0, j = ring.length - 1; i < ring.length; j = i++) {
    const [xi, yi] = ring[i];
    const [xj, yj] = ring[j];
    if (yi > p[1] !== yj > p[1] && p[0] < ((xj - xi) * (p[1] - yi)) / (yj - yi) + xi) odd = !odd;
  }
  return odd;
};

/**
 * A world point genuinely inside one of a shape's rings, found by sampling its own bounding box.
 *
 * Not the ring's centroid: a cluster's arm is a crescent, and its centroid is outside it. Not a
 * vertex pulled inward either, for the same reason. The grid is the honest way to a point that is
 * in the shape, and it costs nothing — the containment is arithmetic here, not a pick pass.
 */
const insidePoint = (rings) => {
  for (const ring of [...rings].sort((a, b) => b.length - a.length)) {
    const xs = ring.map((p) => p[0]);
    const ys = ring.map((p) => p[1]);
    const [x0, x1, y0, y1] = [Math.min(...xs), Math.max(...xs), Math.min(...ys), Math.max(...ys)];
    let best = null;
    for (let i = 1; i < 24; i++) {
      for (let j = 1; j < 24; j++) {
        const p = [x0 + ((x1 - x0) * i) / 24, y0 + ((y1 - y0) * j) / 24];
        if (!inRing(p, ring)) continue;
        // The deepest point of the lot: furthest from the boundary, so a 3 px jitter stays in.
        const d = Math.min(...ring.map((q, k) => {
          const r = ring[(k + 1) % ring.length];
          const dx = r[0] - q[0];
          const dy = r[1] - q[1];
          const len2 = dx * dx + dy * dy;
          const t = len2 === 0 ? 0 : Math.max(0, Math.min(1, ((p[0] - q[0]) * dx + (p[1] - q[1]) * dy) / len2));
          return Math.hypot(q[0] + t * dx - p[0], q[1] + t * dy - p[1]);
        }));
        if (best === null || d > best.d) best = {p, d};
      }
    }
    if (best) return best.p;
  }
  return null;
};

/**
 * The canvas point the pointer is put at for a subject: a world point inside its **live** shape,
 * projected. The rings are re-read after the camera has settled — a response carries the shape of
 * what is served for the view it answered, so the overview's ring is not the one on screen now.
 */
const interior = async (subject) => {
  const live = (await servedArtifacts()).find((x) => x.id === subject.id);
  if (!live || live.rings.length === 0) return null;
  const world = insidePoint(live.rings);
  if (!world) return null;
  const [[x, y]] = await project([world]);
  const box = await map.boundingBox();
  // The projection is canvas-relative; the mouse is driven in page coordinates. The bounds are
  // the canvas's own, which is not the viewport's — the map sits beside the demo's panels.
  if (!(x > 8 && y > 8 && x < box.width - 8 && y < box.height - 8)) return null;
  const at = [box.x + x, box.y + y];
  await page.mouse.move(at[0], at[1]);
  cursor.x = at[0];
  cursor.y = at[1];
  await page.waitForTimeout(120);
  return at;
};

/**
 * Every drawn shape's rings in **canvas pixels**, projected in one pass, so a sampled pointer
 * position can be tested against them without another round trip.
 */
const drawnInScreen = async (shapes) => {
  const rings = shapes.flatMap((s) => s.rings.map((ring) => ({id: s.id, ring})));
  const projected = await map.evaluate(
    (el, sets) => sets.map((ring) => ring.map((p) => /** @type {any} */ (el).deck.getViewports()[0].project(p))),
    rings.map((r) => r.ring)
  );
  const box = await map.boundingBox();
  return rings.map((r, i) => ({id: r.id, ring: projected[i].map(([x, y]) => [x + box.x, y + box.y])}));
};

/** The set of drawn shapes a page point is inside — the answer the hover is allowed to give. */
const idsAt = (drawn, p) => new Set(drawn.filter((d) => inRing(p, d.ring)).map((d) => d.id));
const same = (a, b) => a.size === b.size && [...a].every((v) => b.has(v));

/** How far a page point is from the nearest drawn line, in pixels. */
const toLine = (drawn, p) => {
  let best = Number.POSITIVE_INFINITY;
  for (const d of drawn) {
    for (let i = 0, j = d.ring.length - 1; i < d.ring.length; j = i++) {
      const a = d.ring[j];
      const b = d.ring[i];
      const dx = b[0] - a[0];
      const dy = b[1] - a[1];
      const len2 = dx * dx + dy * dy;
      const t = len2 === 0 ? 0 : Math.max(0, Math.min(1, ((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / len2));
      best = Math.min(best, Math.hypot(a[0] + t * dx - p[0], a[1] + t * dy - p[1]));
    }
  }
  return best;
};

/**
 * **A hover that holds still**, measured: the answer may change only where the pointer crosses a
 * drawn boundary.
 *
 * Counting flips in the sequence alone is the wrong measure on a real map — a straight glide can
 * leave one cluster's arm and re-enter it, and the answer honestly changes twice. What must never
 * happen is the answer changing between two positions that lie inside exactly the same drawn
 * shapes, which is what the pointer crossing an *invisible* ancestor produced.
 */
const unprovoked = (samples, drawn, near = 6) => {
  const out = [];
  for (let i = 1; i < samples.length; i++) {
    if (samples[i].id === samples[i - 1].id) continue;
    if (!same(idsAt(drawn, samples[i].at), idsAt(drawn, samples[i - 1].at))) continue;
    // A step that straddles a line has the line between its ends whatever the two containment
    // sets say — a four-pixel step lands within a pixel or two of the edge it just crossed, and
    // the projection this harness does is not the client's to the pixel.
    if (Math.min(toLine(drawn, samples[i].at), toLine(drawn, samples[i - 1].at)) <= near) continue;
    out.push(`${String(samples[i - 1].id).slice(0, 6)}→${String(samples[i].id).slice(0, 6)} at ${samples[i].at.map(Math.round).join(',')}`);
  }
  return out;
};

/**
 * The served set as it stands **now**, with the frontier computed over it.
 *
 * The frontier moves with the camera: zoom into a cluster and its children are served, which makes
 * it an ancestor and takes it off the map. So every claim below reads the set it is standing in
 * rather than the overview's, and names the artifact it ran against.
 */
const live = async () => {
  const now = await servedArtifacts();
  const ancestors = new Set(now.map((x) => x.parent).filter(Boolean));
  const front = new Set(now.filter((x) => !ancestors.has(x.id)).map((x) => x.id));
  const shapes = now.filter((x) => x.rings.length > 0 && front.has(x.id));
  return {now, front, shapes};
};


console.log('--- the shots ---');
for (const [name, camera, rank] of [
  ['wide', wide, (a, b) => a.rings.length - b.rings.length],
  ['multi-ring', many, (a, b) => b.rings.length - a.rings.length]
]) {
  if (!camera) continue;
  await focus(camera);
  // The frontier moves with the camera, so the shape to hover is chosen from what is drawn now,
  // not from the overview's list — otherwise the shot is of a hover that did not take. Among
  // those, the largest on screen: the complaint is about the line, so the line has to be big.
  const {shapes} = await live();
  const onScreen = await drawnInScreen(shapes);
  const bounds = await map.boundingBox();
  const areaOf = new Map();
  const whole = new Map();
  for (const d of onScreen) {
    const xs = d.ring.map((p) => p[0]);
    const ys = d.ring.map((p) => p[1]);
    const [x0, x1, y0, y1] = [Math.min(...xs), Math.max(...xs), Math.min(...ys), Math.max(...ys)];
    areaOf.set(d.id, Math.max(areaOf.get(d.id) ?? 0, (x1 - x0) * (y1 - y0)));
    const inside = x0 > bounds.x + 4 && y0 > bounds.y + 4 && x1 < bounds.x + bounds.width - 4 && y1 < bounds.y + bounds.height - 4;
    whole.set(d.id, (whole.get(d.id) ?? true) && inside);
  }
  // Whole on screen first, then the biggest of those: a contour running off the edge shows less of
  // the line than a smaller one that is all there.
  const choose = (xs) => [...xs].sort((a, b) => rank(a, b) || Number(whole.get(b.id) ?? false) - Number(whole.get(a.id) ?? false) || (areaOf.get(b.id) ?? 0) - (areaOf.get(a.id) ?? 0));
  // In preference order, the first candidate that is actually on screen and actually answers: a
  // shape chosen off the list alone may be one the camera has left behind, and the shot would be
  // of a hover that did not take.
  let subject = null;
  let at = null;
  let answer = null;
  for (const candidate of choose(shapes)) {
    const point = await interior(candidate);
    if (!point) continue;
    subject = candidate;
    at = point;
    answer = await hovered();
    if (answer === candidate.id) break;
  }
  // Then in close, on the subject's own extent, because the complaint is about the line: a
  // contour eighty pixels across says nothing about whether its corners are square. Kept only if
  // the same artifact still answers there — zooming in serves its children and moves the frontier.
  let close = null;
  if (subject) {
    const moved = await map.evaluate((el, id) => /** @type {any} */ (el).fitTo(BigInt(id)), subject.id);
    if (moved) {
      await page.waitForTimeout(2500);
      const point = await interior(subject);
      if (point && (await hovered()) === subject.id) close = point;
    }
  }
  console.log(
    `  [${elapsed()}] ${name}: ${subject ? subject.id.slice(0, 8) : 'none'} — ${subject ? subject.rings.length : 0} ring(s), ${subject ? subject.vertices : 0} vertices, ${subject ? subject.count.toLocaleString('en-GB') : 0} members; hovered ${answer === null ? 'nothing' : answer.slice(0, 8)} at ${at ? at.map(Math.round).join(',') : 'nowhere'}${close ? `, and again on its own extent at ${close.map(Math.round).join(',')}` : ''}`
  );
  // The map element alone: the complaint is about the line, and the demo's panels are not it.
  if (shots && (close || at)) await map.screenshot({path: `${shots}/contour-${tag}-${name}.png`});
}

console.log('--- the hover ---');

// 1. Only what is drawn answers a hover. An ancestor is served alongside its children and nothing
//    is drawn for it, so pointing at one must not reach it.
await focus(nested ?? wide);
const wander = [];
{
  const {front} = await live();
  const box = await map.boundingBox();
  const path = [[0.3, 0.3], [0.7, 0.35], [0.65, 0.7], [0.35, 0.65], [0.5, 0.5]];
  for (const [fx, fy] of path) wander.push(...(await glide(box.x + box.width * fx, box.y + box.height * fy, 14)));
  const answers = wander.map((x) => x.id);
  const offFrontier = [...new Set(answers.filter((v) => v !== null && !front.has(v)))];
  check(
    'only the drawn frontier answers a hover',
    offFrontier.length === 0 && answers.some(Boolean),
    `${wander.length} pointer positions over five glides, ${new Set(answers.filter(Boolean)).size} distinct answers over a frontier of ${front.size}, ${offFrontier.length} of them not on it${offFrontier.length ? ` (${offFrontier.map((v) => v.slice(0, 8)).join(', ')})` : ''}`
  );
}

// 2. A boundary crossed at four pixels a step: the answer changes where the line is and nowhere
//    else. The flip the owner saw was the answer changing where nothing was crossed at all —
//    the pointer moving inside one cluster, over its parent's invisible ring.
{
  const {shapes} = await live();
  const subject = [...shapes].sort((a, b) => b.vertices - a.vertices)[0];
  const inside = subject ? await interior(subject) : null;
  const drawn = subject ? await drawnInScreen([subject]) : [];
  // A crossing of the subject's own edge: out to in, forty pixels either side of one vertex.
  const bounds = await map.boundingBox();
  const edge = drawn.flatMap((d) => d.ring).filter(([x, y]) => x > bounds.x + 60 && y > bounds.y + 60 && x < bounds.x + bounds.width - 60 && y < bounds.y + bounds.height - 60);
  const v = inside && edge.length > 0 ? edge[Math.floor(edge.length / 2)] : null;
  let crossing = [];
  if (inside && v) {
    const len = Math.hypot(v[0] - inside[0], v[1] - inside[1]) || 1;
    const away = [(v[0] - inside[0]) / len, (v[1] - inside[1]) / len];
    const from = [v[0] + away[0] * 40, v[1] + away[1] * 40];
    const to = [v[0] - away[0] * 40, v[1] - away[1] * 40];
    await page.mouse.move(from[0], from[1]);
    cursor.x = from[0];
    cursor.y = from[1];
    await page.waitForTimeout(200);
    crossing = await glide(to[0], to[1], 20, 30);
  }
  const all = await drawnInScreen(shapes);
  const loose = unprovoked(crossing, all);
  check(
    'a crossing changes the answer only where a drawn boundary is crossed',
    crossing.length > 0 && loose.length === 0 && runs(crossing).length <= 2 && crossing[crossing.length - 1].id === subject.id,
    subject
      ? `across ${subject.id.slice(0, 8)}'s edge, 20 steps of 4 px: ${show(crossing)}; ${loose.length} change(s) with no boundary between${loose.length ? ` (${loose.join('; ')})` : ''}`
      : 'no frontier shape in view'
  );

  // 3. A few pixels never change the answer. A hand does not hold still, and the hover must.
  const at = subject ? await interior(subject) : null;
  const jitter = [];
  for (let i = 0; i < 16 && at; i++) {
    await page.mouse.move(at[0] + Math.cos((i * Math.PI) / 4) * 3, at[1] + Math.sin((i * Math.PI) / 4) * 3);
    await page.waitForTimeout(40);
    jitter.push(await hovered());
  }
  if (at) {
    cursor.x = at[0];
    cursor.y = at[1];
  }
  check(
    'a few pixels of movement inside one cluster never change the answer',
    at !== null && new Set(jitter).size === 1 && jitter[0] === subject.id,
    at ? `16 moves within 3 px of one point inside ${subject.id.slice(0, 8)}: ${show(jitter)}` : 'no point inside the subject is on screen'
  );
}

// 4. The nested case: a frontier cluster with a served ancestor, entered and left over its own
//    edge. The ancestor must never answer, and the crossing must be one change each way.
{
  const {now, shapes} = await live();
  const under = shapes.filter((x) => x.parent && now.some((y) => y.id === x.parent)).sort((a, b) => b.vertices - a.vertices)[0];
  const inside = under ? await interior(under) : null;
  const drawn = under ? await drawnInScreen([under]) : [];
  const bounds = await map.boundingBox();
  const edge = drawn.flatMap((d) => d.ring).filter(([x, y]) => x > bounds.x + 60 && y > bounds.y + 60 && x < bounds.x + bounds.width - 60 && y < bounds.y + bounds.height - 60);
  const v = inside && edge.length > 0 ? edge[Math.floor(edge.length / 2)] : null;
  let there = [];
  let back = [];
  if (inside && v) {
    const len = Math.hypot(v[0] - inside[0], v[1] - inside[1]) || 1;
    const away = [(v[0] - inside[0]) / len, (v[1] - inside[1]) / len];
    const from = [v[0] + away[0] * 40, v[1] + away[1] * 40];
    const to = [v[0] - away[0] * 40, v[1] - away[1] * 40];
    await page.mouse.move(from[0], from[1]);
    cursor.x = from[0];
    cursor.y = from[1];
    await page.waitForTimeout(200);
    there = await glide(to[0], to[1], 20, 30);
    back = await glide(from[0], from[1], 20, 30);
  }
  const seen = [...there, ...back].map((x) => x.id);
  check(
    'a nested region: crossing in and out never answers with the ancestor',
    there.length > 0 && seen.includes(under.id) && !seen.includes(under.parent),
    under ? `child ${under.id.slice(0, 8)} under parent ${String(under.parent).slice(0, 8)}; in: ${show(there)}; out: ${show(back)}` : 'no frontier artifact with a served parent in view'
  );
  const all = await drawnInScreen(shapes);
  const loose = there.length > 0 ? [...unprovoked(there, all), ...unprovoked(back, all)] : ['not measured'];
  check(
    'a nested region: in and back out changes only at a boundary',
    there.length > 0 && loose.length === 0 && runs(there).length <= 2 && runs(back).length <= 2,
    there.length > 0
      ? `${runs(there).length} run(s) in, ${runs(back).length} run(s) out, ${loose.length} change(s) with no boundary between${loose.length ? ` (${loose.join('; ')})` : ''}`
      : 'not measured'
  );
}

await browser.close();
if (consoleErrors.length) failures.push(...consoleErrors);
if (failures.length) {
  console.error(`HOVER FAILED (${passes.length} ok, ${failures.length} failed):\n  ${failures.join('\n  ')}`);
  process.exit(1);
}
console.log(`HOVER OK — ${passes.length} claims hold`);
