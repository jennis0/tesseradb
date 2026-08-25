#!/usr/bin/env node
// The acceptance harness (design client-components §9; client-interaction §10's conformance
// harness, given its first subject): DOM-level assertions against the demo page, through the
// components' parts and never through an id the shadow DOM hides — and the measurements the
// delivery record owes.
//
//   node clients/ts/harness/harness.mjs [--url http://localhost:5173] [--shot /tmp/tessera-harness.png]
//
// Requires a running `tessera serve` with a published layer and a running `vite dev`. A target
// beside the gate, not a step in it: it needs a served bundle, ports and headless Chromium.
//
// The claims, each checked rather than eyeballed:
//   1. only `shown` renders a count — sampled from the first paint, through loading;
//   2. both figures render or neither — the sample's text is `a of b` or empty;
//   3. a refusal renders as one — the viewport route is refused under the page, and the strip
//      shows the refusal with no count;
//   4. no count renders against a stale view, and a refresh control is present — the artifact
//      channel's response is given a moved content key, and the strip goes stale;
//   5. a region's count renders as inexact when its cell exceeds a pixel — a box at the
//      overview, counted in the `tiles` form at a bounded depth, reads `≈`;
//   6. a switch of principal empties every card.
//
// Hover and click on marks are not driven here: deck.gl's `onClick` does not fire under headless
// chromium (the same reason the smoke scripts never drove the drill-down), so the pick path is
// covered by the component and deck unit tests and `core/test/client.live.test.ts`. The item
// card is filled here through the selection panel's list, which is DOM.
import {chromium} from 'playwright';

const args = Object.fromEntries(
  process.argv
    .slice(2)
    .reduce((acc, a, i, all) => (a.startsWith('--') ? [...acc, [a.slice(2), all[i + 1]]] : acc), [])
);
const url = args.url ?? 'http://localhost:5173';
const shot = args.shot ?? '/tmp/tessera-harness.png';

const browser = await chromium.launch({
  args: ['--use-gl=swiftshader', '--enable-unsafe-swiftshader', '--disable-gpu-sandbox']
});
const page = await browser.newPage({viewport: {width: 1280, height: 800}});

const consoleErrors = [];
page.on('console', (m) => {
  if (m.type() === 'error') consoleErrors.push(m.text());
});
page.on('pageerror', (e) => consoleErrors.push(`pageerror: ${e.message}`));

/** Every viewport request's body, so a claim about the request's shape is checked on the wire. */
const viewportRequests = [];
page.on('request', (r) => {
  if (!r.url().includes('/v1/viewport')) return;
  try {
    viewportRequests.push(JSON.parse(r.postData() ?? '{}'));
  } catch {
    // Not JSON; not a viewport request this harness understands.
  }
});

/** Every response that was not a 2xx, with the request that earned it — a 422 is the client's bug. */
const refusals = [];
page.on('response', async (r) => {
  if (!/\/v1\/|\/session\//.test(r.url()) || r.ok()) return;
  const req = r.request();
  const body = req.postData() ?? '';
  refusals.push(`${r.status()} ${new URL(r.url()).pathname} ← ${body.slice(0, 160)}`);
});

const failures = [];
const passes = [];
/** @param {string} claim @param {boolean} ok @param {string} evidence */
const check = (claim, ok, evidence) => {
  (ok ? passes : failures).push(`${claim} — ${evidence}`);
  console.log(`  ${ok ? 'ok  ' : 'FAIL'} ${claim} — ${evidence}`);
};

// ---- locators, all shadow-piercing -----------------------------------------------------------

const strip = page.locator('tessera-status').first();
const stripState = () => strip.locator('[part="state"]').first().getAttribute('data-state');
const stripCounts = async () => {
  const parts = strip.locator('[part="count"]');
  const n = await parts.count();
  const out = [];
  for (let i = 0; i < n; i++) {
    const el = parts.nth(i);
    out.push({text: ((await el.textContent()) ?? '').trim(), empty: (await el.getAttribute('data-empty')) === 'true'});
  }
  return out;
};
const nonEmptyCounts = async () => (await stripCounts()).filter((c) => !c.empty);

/** Wait until the strip reports a state in `states`, up to `limitMs`. */
const untilState = async (states, limitMs = 60_000) => {
  const started = Date.now();
  while (Date.now() - started < limitMs) {
    const s = await stripState().catch(() => null);
    if (s && states.includes(s)) return s;
    await page.waitForTimeout(150);
  }
  return await stripState().catch(() => null);
};

/** Wait until the probe's mark count stops moving. */
const settled = async (limitMs = 45_000) => {
  const started = Date.now();
  let last = -1;
  let stable = 0;
  while (Date.now() - started < limitMs) {
    await page.waitForTimeout(400);
    const marks = await page.evaluate(() => window.__tesseraProbe?.marks ?? -1);
    if (marks === last) {
      if (++stable >= 4) return true;
    } else {
      stable = 0;
      last = marks;
    }
  }
  return false;
};

// ---- 1 and 2: only shown renders a count; both figures or neither -------------------------------

console.log('--- claims ---');
await page.goto(`${url}?prefetch=0`, {waitUntil: 'load'});

// Sample from the first paint: every (state, counts) pair observed while the session comes up.
const samples = [];
{
  const started = Date.now();
  while (Date.now() - started < 90_000) {
    // One evaluate for the pair: read in two round-trips, a transition landing between them
    // (`loading` read, then counts read after the frame arrived) reads as a count outside shown.
    const sample = await strip
      .evaluate((root) => {
        const scope = root.shadowRoot ?? root;
        const state = scope.querySelector('[part="state"]')?.getAttribute('data-state') ?? null;
        const counts = Array.from(scope.querySelectorAll('[part="count"]'))
          .filter((el) => el.getAttribute('data-empty') !== 'true')
          .map((el) => ({text: (el.textContent ?? '').trim(), empty: false}));
        return state ? {state, counts} : null;
      })
      .catch(() => null);
    if (sample) samples.push(sample);
    if (sample?.state === 'shown') break;
    await page.waitForTimeout(100);
  }
}
const leaked = samples.filter((s) => s.state !== 'shown' && s.state !== 'stale' && s.counts.length > 0);
const nonShown = samples.filter((s) => s.state !== 'shown');
check(
  'only `shown` renders a count',
  leaked.length === 0 && samples.some((s) => s.state === 'shown'),
  `${samples.length} samples, ${nonShown.length} in ${[...new Set(nonShown.map((s) => s.state))].join('/') || 'no other state'}, ${leaked.length} with a count outside shown`
);
await settled();
{
  const counts = await stripCounts();
  const shown = counts[0]?.text ?? '';
  const both = /^\d[\d,]* of \d[\d,]*$/.test(shown) || shown === '';
  check('both figures render or neither', both && counts.length === 3, `strip reads "${counts.map((c) => c.text).join(' · ')}"`);
}
const baseline = await stripCounts();

// ---- 3: a refusal renders as one ---------------------------------------------------------------

/** Whether a viewport request is the artifact channel's — a named layer — rather than the point path's. */
const isChannel = (route) => {
  try {
    const body = JSON.parse(route.request().postData() ?? '{}');
    return Array.isArray(body.layers) && body.layers.length > 0;
  } catch {
    return false;
  }
};
// The point path is refused; the channel is left alone, so the refusal the strip shows is the
// view's own and not the artifacts'. Zooming several notches into a corner reaches ground the
// replica does not hold, so a request goes out.
await page.route('**/v1/viewport', (route) =>
  isChannel(route)
    ? route.continue()
    : route.fulfill({status: 403, contentType: 'application/json', body: JSON.stringify({error: 'harness-refusal', detail: 'refused under the page by the harness'})})
);
await page.mouse.move(900, 250);
for (let i = 0; i < 4; i++) {
  await page.mouse.wheel(0, -500);
  await page.waitForTimeout(300);
}
const refusedState = await untilState(['refused', 'expired'], 30_000);
{
  const counts = await nonEmptyCounts();
  const refusal = await strip.locator('[part="refusal"]').first().textContent().catch(() => '');
  check(
    'a refusal renders as one',
    (refusedState === 'refused' || refusedState === 'expired') && counts.length === 0 && /harness-refusal/.test(refusal ?? ''),
    `state=${refusedState}, refusal="${(refusal ?? '').trim().slice(0, 60)}", ${counts.length} counts rendered`
  );
}
await page.unroute('**/v1/viewport');
await page.locator('tessera-map [part="controls"] button', {hasText: 'fit'}).first().click().catch(() => {});
await untilState(['shown'], 60_000);
await settled();

// ---- 4: no count against a stale view, and a refresh control ------------------------------------

// The replica's revalidation — a counts-only `k = 0` bbox request it issues for a still, covered
// view once its interval (60 s) has lapsed and the view is scheduled again — is given a content
// key the presented frame did not see. The store keys staleness on the key its replica observes
// (never on `x-tessera-stale`), and the strip goes stale without a redraw. A point request is
// left alone: it would redraw under the new key and never be stale.
const isRevalidation = (route) => {
  try {
    const body = JSON.parse(route.request().postData() ?? '{}');
    return body.k === 0 && Array.isArray(body.bbox) && !Array.isArray(body.tiles) && !(Array.isArray(body.layers) && body.layers.length > 0);
  } catch {
    return false;
  }
};
await page.route('**/v1/viewport', async (route) => {
  if (!isRevalidation(route)) return route.continue();
  const response = await route.fetch();
  await route.fulfill({response, headers: {...response.headers(), etag: '"harness-moved-content-key"'}});
});
await page.waitForTimeout(61_000);
// A nudge the ring already covers: the view is scheduled again, nothing novel is fetched, and
// the lapsed interval sends the revalidation.
await page.mouse.move(640, 400);
await page.mouse.down();
await page.mouse.move(652, 406, {steps: 2});
await page.mouse.up();
const staleState = await untilState(['stale'], 30_000);
{
  const counts = await nonEmptyCounts();
  const refresh = await strip.locator('[part="refresh"]').count();
  check(
    'no count renders against a stale view, and a refresh control is present',
    staleState === 'stale' && counts.length === 0 && refresh === 1,
    `state=${staleState}, ${counts.length} counts rendered, ${refresh} refresh control(s)`
  );
}
await page.unroute('**/v1/viewport');
if (staleState === 'stale') await strip.locator('[part="refresh"]').first().click();
await untilState(['shown'], 60_000);
await settled();

// ---- 5: a region's count is inexact when its cell exceeds a pixel -------------------------------

// The overview: the world is 512 px at zoom 0, so any cover under the 4,096-tile bound is coarser
// than a pixel. The box is shift-dragged in pan mode, which is the shortcut §5.3 names.
await page.locator('tessera-map').first().focus();
await page.locator('tessera-map [part="controls"] button', {hasText: 'fit'}).first().click().catch(() => {});
await settled();
const requestsBefore = viewportRequests.length;
const selectStarted = Date.now();
await page.keyboard.down('Shift');
await page.mouse.move(500, 300);
await page.mouse.down();
await page.mouse.move(760, 500, {steps: 8});
await page.mouse.up();
await page.keyboard.up('Shift');
const selection = page.locator('tessera-selection').first();
await selection.locator('[part="count-matched"] [part="count"]').first().waitFor({timeout: 30_000}).catch(() => {});
const regionMs = Date.now() - selectStarted;
{
  const matched = selection.locator('[part="count-matched"] [part="count"]').first();
  const text = ((await matched.textContent().catch(() => '')) ?? '').trim();
  const exact = await matched.getAttribute('data-exact').catch(() => null);
  const state = await selection.locator('[part="state"]').first().textContent().catch(() => '');
  const request = viewportRequests.slice(requestsBefore).find((r) => Array.isArray(r.tiles) && r.k === 0 && !(Array.isArray(r.layers) && r.layers.length > 0));
  const probe = await page.evaluate(() => window.__tesseraProbe?.region ?? null);
  check(
    'a region’s count renders as inexact when its cell exceeds a pixel',
    exact === 'false' && text.startsWith('≈') && request !== undefined && request.tiles.length <= 4096,
    `matched reads "${text}" (data-exact=${exact}); the request named ${request?.tiles?.length ?? 'no'} tiles at depth ${request?.zoom ?? '?'} with k=${request?.k ?? '?'}; ${(state ?? '').trim().slice(0, 90)}`
  );
  console.log(`  ·    region: ${JSON.stringify(probe)}; ${regionMs} ms from mouse-up to the panel under headless input, ${probe?.ms?.toFixed(0) ?? '?'} ms select-to-counted on the store's clock`);
}

// ---- 6: a switch of principal empties every card -----------------------------------------------

// Fill the item card through the selection's list — a click that is DOM, not a canvas pick.
const item = selection.locator('[part="item"]').first();
if ((await item.count()) > 0) await item.click();
const card = page.locator('tessera-item-card').first();
await card.locator('[part="field"]').first().waitFor({timeout: 20_000}).catch(() => {});
const fieldsBefore = await card.locator('[part="field"]').count();
const options = await page.locator('#principal option').count();
const current = await page.locator('#principal').inputValue();
const other = [...Array(options).keys()].map(String).find((v) => v !== current) ?? current;
await page.selectOption('#principal', other);
await page.waitForTimeout(300);
{
  const fieldsAfter = await card.locator('[part="field"]').count();
  const selectionAfter = await page.locator('tessera-selection').count();
  const stripAfter = await nonEmptyCounts();
  const state = await stripState();
  check(
    'a switch of principal empties every card',
    fieldsBefore > 0 && fieldsAfter === 0 && selectionAfter === 0 && (state === 'shown' || stripAfter.length === 0),
    `item card ${fieldsBefore} → ${fieldsAfter} fields, selection panel ${selectionAfter === 0 ? 'gone' : 'still shown'}, strip ${state} with ${stripAfter.length} counts`
  );
}
await untilState(['shown'], 60_000);
await settled();
{
  const after = await stripCounts();
  check(
    'a different principal reports a different picture',
    after[0]?.text !== baseline[0]?.text,
    `"${baseline.map((c) => c.text).join(' · ')}" → "${after.map((c) => c.text).join(' · ')}"`
  );
}

// ---- measurements --------------------------------------------------------------------------------

// The largest bundle the demo is serving — the harness measures what is there and says which.
const dataset = await page.locator('#instruments').innerText().then((t) => /bundle\s+(.+)/.exec(t)?.[1]?.trim() ?? (t.split('\n').find((l) => /arXiv|bundle/.test(l)) ?? '?'));
// Drive a few zoom notches so the settle work and the frame gaps are measured under load.
for (const notch of [-400, -400, 400, 400]) {
  await page.mouse.move(640, 400);
  await page.mouse.wheel(0, notch);
  await settled();
}
const probe = await page.evaluate(() => {
  const p = window.__tesseraProbe;
  if (!p) return null;
  return {marks: p.marks, paints: p.paints, requests: p.requests, timings: p.timings, view: p.view, instruments: p.instruments ?? null};
});
const quantile = (xs, q) => {
  const s = [...xs].sort((a, b) => a - b);
  return s.length ? s[Math.min(s.length - 1, Math.floor(s.length * q))] : NaN;
};
console.log('--- measurements ---');
if (probe) {
  const d = probe.timings.decodeMs;
  console.log(`  bundle: ${dataset}`);
  console.log(`  marks on screen: ${probe.marks.toLocaleString('en-GB')} at depth ${probe.view.depth}; ${probe.paints} paints, ${probe.requests} fetched frames`);
  console.log(`  per response — decode: n=${d.length}, median ${quantile(d, 0.5).toFixed(1)} ms, p95 ${quantile(d, 0.95).toFixed(1)} ms, max ${Math.max(...d, 0).toFixed(1)} ms (worker; remap: not measurable until D12's column is on this branch)`);
  console.log(`  per settle — slab sync ${probe.timings.slabMs.toFixed(2)} ms, wash bin ${probe.timings.washMs.toFixed(2)} ms, layer build ${probe.timings.layersMs.toFixed(2)} ms (last settle)`);
  console.log(`  per frame — mean ${probe.timings.frame.mean.toFixed(1)} ms, p95 ${probe.timings.frame.p95.toFixed(1)} ms over the last ${probe.timings.frame.n} frames (software GL under headless chromium)`);
  const region = await page.evaluate(() => window.__tesseraProbe?.region ?? null);
  console.log(`  box selection — ${region?.ms?.toFixed(0) ?? '?'} ms select-to-counted (200 ms settle, the request, the sum); ${regionMs} ms mouse-up to panel under headless input, which waits on deck's software-GL hover picks`);
} else {
  console.log('  no probe on the page');
}

await page.screenshot({path: shot, timeout: 60_000});
await browser.close();

console.log('--- responses that were not 2xx ---');
console.log(refusals.length ? refusals.map((e) => `  ${e}`).join('\n') : '  none');
console.log('--- console errors (the harness’s own 403s excepted) ---');
console.log(consoleErrors.length ? consoleErrors.map((e) => `  ${e}`).join('\n') : '  none');
console.log(`--- screenshot: ${shot}`);
// The 403s are the harness's own, refused under the page above; anything else counts.
const unexpected = consoleErrors.filter((e) => !/403 \(Forbidden\)/.test(e));
if (unexpected.length) failures.push(`${unexpected.length} console error(s)`);
if (failures.length) {
  console.error(`HARNESS FAILED (${passes.length} ok, ${failures.length} failed):\n  ${failures.join('\n  ')}`);
  process.exit(1);
}
console.log(`HARNESS OK — ${passes.length} claims hold`);
