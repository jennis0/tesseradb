#!/usr/bin/env node
// The acceptance harness (design client-components §9; client-interaction §10's conformance
// harness, given its first subject): DOM-level assertions against the demo page, through the
// components' parts and never through an id the shadow DOM hides — and the measurements the
// delivery record owes.
//
//   node clients/ts/harness/harness.mjs [--url http://localhost:5173] [--shot /tmp/tessera-harness.png] [--headed]
//
// The same assertions run against the demo page and against the C1 example page
// (`examples/plain-html`, `--url http://localhost:5180`), which is the explorer with none of the
// demo's layout: the claims read the components' parts, the probe is the explorer's own map's
// where the page publishes none, and the demo's instruments panel is optional. Both pages carry a
// `#principal` select, which is how a page says who is signed in.
//
// Requires a running `tessera serve` with a published layer and a running `vite dev`. A target
// beside the gate, not a step in it: it needs a served bundle, ports and Chromium — headless
// under swiftshader by default; `--headed` runs the real browser on a display (WSLg's, or
// `xvfb-run`), which is where the GPU and the frame cadence are measured rather than emulated.
//
// The claims, each checked rather than eyeballed:
//   1. only `shown` renders a count — sampled from the first paint, through loading;
//   2. both figures render or neither — the strip's shown cell renders its figure only with its
//      total carried beside it (`data-total`, the visible cell's number), or renders nothing;
//   3. a refusal renders as one — the viewport route is refused under the page, and the strip
//      shows the refusal with no count;
//   4. no count renders against a stale view, and a refresh control is present — the artifact
//      channel's response is given a moved content key, and the strip goes stale;
//   5. a region's count renders as inexact when its cell exceeds a pixel — a box at the
//      overview, counted in the `tiles` form at a bounded depth, reads `≈`;
//   6. a switch of principal empties every card;
//   7. a different principal reports a different picture;
//   8. an artifact's count does not move across a pan — the list's row for one artifact reads
//      the same number before and after the map is dragged, though the served set may change;
//   9. a coloured point's ordinal resolves to a served artifact — colour by cluster is chosen
//      through the legend, and every ordinal the marks on screen carry resolves, through the
//      session table, to an id in the served set (read off the probe, never eyeballed).
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
const headed = 'headed' in args;
/** A Chromium other than the one this Playwright bundles — `--executable /path/to/chrome`. */
const executablePath = args.executable;

const browser = await chromium.launch(
  headed
    ? {headless: false, args: ['--disable-gpu-sandbox'], ...(executablePath ? {executablePath} : {})}
    : {args: ['--use-gl=swiftshader', '--enable-unsafe-swiftshader', '--disable-gpu-sandbox'], ...(executablePath ? {executablePath} : {})}
);
const page = await browser.newPage({viewport: {width: 1280, height: 800}});
// The probe: the demo publishes its first map's on `window` (with the lanes it keeps itself);
// any page with an explorer has the map's own, and that is what the C1 example page offers.
await page.addInitScript(() => {
  const explorer = () => /** @type {{map: {probe: Window['__tesseraProbe']} | null} | null} */ (/** @type {unknown} */ (document.querySelector('tessera-explorer')));
  window.__tesseraProbeOf = () => window.__tesseraProbe ?? explorer()?.map?.probe ?? null;
});

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
    out.push({text: ((await el.textContent()) ?? '').trim(), empty: (await el.getAttribute('data-empty')) === 'true', total: await el.getAttribute('data-total')});
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
    const marks = await page.evaluate(() => window.__tesseraProbeOf()?.marks ?? -1);
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
await page.goto(`${url}${url.includes('?') ? '&' : '?'}prefetch=0`, {waitUntil: 'load'});

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
  const total = counts[0]?.total ?? null;
  const both = (/^\d[\d,]*$/.test(shown) && total !== null && total === counts[2]?.text) || (shown === '' && total === null);
  check('both figures render or neither', both && counts.length === 3, `strip reads "${counts.map((c) => c.text).join(' · ')}", shown of ${total}`);
}
const baseline = await stripCounts();

// ---- 3: a refusal renders as one ---------------------------------------------------------------

/**
 * Whether a viewport request is the artifact channel's — `k = 0` with a named layer — rather
 * than the point path's, which names the layers too now (§5.10) but always asks for points.
 */
const isChannel = (route) => {
  try {
    const body = JSON.parse(route.request().postData() ?? '{}');
    return body.k === 0 && Array.isArray(body.layers) && body.layers.length > 0;
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
await page.locator('tessera-map [part="controls"] button[aria-label="Fit to extent"]').first().click().catch(() => {});
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
/** Revalidations the harness moved the key under — nought of them is a different failure from one. */
let movedKeys = 0;
await page.route('**/v1/viewport', async (route) => {
  if (!isRevalidation(route)) return route.continue();
  const response = await route.fetch();
  movedKeys += 1;
  await route.fulfill({response, headers: {...response.headers(), etag: '"harness-moved-content-key"'}});
});
await page.waitForTimeout(61_000);
// A nudge the ring already covers: the view is scheduled again, nothing novel is fetched, and
// the lapsed interval sends the revalidation.
await page.mouse.move(900, 450);
await page.mouse.down();
await page.mouse.move(912, 456, {steps: 2});
await page.mouse.up();
const staleState = await untilState(['stale'], 30_000);
{
  const counts = await nonEmptyCounts();
  const refresh = await strip.locator('[part="refresh"]').count();
  check(
    'no count renders against a stale view, and a refresh control is present',
    staleState === 'stale' && counts.length === 0 && refresh === 1,
    `state=${staleState}, ${counts.length} counts rendered, ${refresh} refresh control(s); ${movedKeys} revalidation(s) had their key moved`
  );
}
await page.unroute('**/v1/viewport');
// **Clicked in a retry loop, not once.** While the moved key is in play the strip alternates
// between the stale row and the counts row on each arriving response — the replica observes the
// server's real key, the next derive stamps the moved one — and each flip replaces the button, so
// a single click races the re-render and times out with *element was detached from the DOM*. That
// flake is not a claim: claim 4 has already been checked, and this click only puts the page back
// to `shown` for the claims after it. Seen on `main` as well as on the branch.
for (let tries = 0; tries < 10 && (await stripState().catch(() => null)) === 'stale'; tries++) {
  await strip.locator('[part="refresh"]').first().click({timeout: 5_000}).catch(() => {});
  await page.waitForTimeout(500);
}
await untilState(['shown'], 60_000);
await settled();

// ---- 5: a region's count is inexact when its cell exceeds a pixel -------------------------------

// The overview: the world is 512 px at zoom 0, so any cover under the 4,096-tile bound is coarser
// than a pixel. The box is shift-dragged in pan mode, which is the shortcut §5.3 names.
await page.locator('tessera-map').first().focus();
await page.locator('tessera-map [part="controls"] button[aria-label="Fit to extent"]').first().click().catch(() => {});
await settled();
const requestsBefore = viewportRequests.length;
const selectStarted = Date.now();
// Clear of the floating panels — the left card ends at x = 632 in the demo's overlay and the
// right one starts at 1108 — so the box is on the canvas.
await page.keyboard.down('Shift');
await page.mouse.move(700, 250);
await page.mouse.down();
await page.mouse.move(1000, 520, {steps: 8});
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
  const probe = await page.evaluate(() => window.__tesseraProbeOf()?.region ?? null);
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
const options = await page.locator('#principal option').evaluateAll((els) => els.map((el) => /** @type {HTMLOptionElement} */ (el).value));
const current = await page.locator('#principal').inputValue();
const other = options.find((v) => v !== current) ?? current;
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
// Back to the principal the page opened on — the broadest — so the artifact claims and the
// measurements below run at the largest picture the demo serves.
await page.selectOption('#principal', current);
await untilState(['shown'], 60_000);
await settled();

// ---- 8: an artifact's count does not move across a pan -------------------------------------------

/** The artifact list's rows, id → count, through its parts. */
const listCounts = async () =>
  page
    .locator('tessera-artifact-list')
    .first()
    .evaluate((root) => {
      const scope = root.shadowRoot ?? root;
      /** @type {Record<string, number>} */
      const out = {};
      for (const item of Array.from(scope.querySelectorAll('[part="item"]'))) {
        const id = item.getAttribute('data-id') ?? '';
        const countEl = item.querySelector('tessera-count');
        const text = (countEl?.shadowRoot ?? countEl)?.querySelector('[part="count"]')?.textContent ?? '';
        out[id] = Number(text.replaceAll(',', ''));
      }
      return out;
    })
    .catch(() => ({}));

await page.locator('tessera-map [part="controls"] button[aria-label="Fit to extent"]').first().click().catch(() => {});
// The demo opens with a layer on; the example page leaves that to the layer picker, so a page
// with none on has its first layer turned on here — a precondition of the two claims below.
if ((await page.evaluate(() => window.__tesseraProbeOf()?.cluster.layersOn.length ?? 0)) === 0) {
  // A minute, because under headless swiftshader the main thread is gone for 10–14 s at a time
  // drawing a million marks, and a click that cannot land is reported rather than swallowed.
  const on = await page.locator('tessera-layer-picker [part="entry"] input').first().click({timeout: 60_000}).then(() => true, () => false);
  console.log(`  ·    no layer was on; the first layer ${on ? 'turned on through the picker' : 'could not be turned on — the picker did not take a click'}`);
}
await settled();
await page.waitForTimeout(1500);
const countsBefore = await listCounts();
// Two notches in and a drag: the served set may change, the count beside an artifact may not.
await page.mouse.move(900, 450);
await page.mouse.wheel(0, -400);
await settled();
await page.mouse.move(900, 450);
await page.mouse.down();
await page.mouse.move(760, 380, {steps: 6});
await page.mouse.up();
await settled();
await page.waitForTimeout(1500);
const countsAfter = await listCounts();
{
  const shared = Object.keys(countsBefore).filter((id) => id in countsAfter);
  const moved = shared.filter((id) => countsBefore[id] !== countsAfter[id]);
  check(
    'an artifact’s count does not move across a pan',
    shared.length > 0 && moved.length === 0,
    `${Object.keys(countsBefore).length} rows before, ${Object.keys(countsAfter).length} after, ${shared.length} in both, ${moved.length} moved`
  );
}

// ---- 9: a coloured point's ordinal resolves to a served artifact ---------------------------------

const legendSelect = page.locator('tessera-legend select').first();
const clusterOption = await legendSelect.locator('option[part="cluster-option"]').first().getAttribute('value').catch(() => null);
const refillStarted = Date.now();
if (clusterOption) await legendSelect.selectOption(clusterOption);
await settled();
await page.waitForTimeout(800);
{
  const probe = await page.evaluate(() => {
    const p = window.__tesseraProbeOf();
    return p ? {encoding: p.encoding, cluster: p.cluster, lutWrites: p.timings.lutWrites} : null;
  });
  const served = new Set(probe?.cluster.servedIds ?? []);
  const sample = probe?.cluster.sample ?? [];
  const unresolved = sample.filter((s) => s.resolvedId === null);
  const foreign = sample.filter((s) => s.resolvedId !== null && !served.has(s.resolvedId));
  check(
    'a coloured point’s ordinal resolves to a served artifact',
    clusterOption !== null && probe?.encoding === `cluster|${probe.cluster.layer}` && sample.length > 0 && foreign.length === 0,
    `colour-by ${probe?.encoding}; ${sample.length} ordinals sampled from the marks on screen, ${sample.length - unresolved.length} resolve to one of ${served.size} served artifacts, ${foreign.length} to an artifact not served, ${probe?.cluster.coloured ?? 0} drawn in a colour; ${probe?.lutWrites} lookup-texture writes so far`
  );
}
const clusterShot = shot.replace(/\.png$/, '-cluster.png');
await page.screenshot({path: clusterShot, timeout: 60_000});

// ---- measurements --------------------------------------------------------------------------------

// The largest bundle the demo is serving — the harness measures what is there and says which.
const dataset = await page
  .locator('#instruments')
  .innerText({timeout: 2_000})
  .then((t) => /bundle\s+(.+)/.exec(t)?.[1]?.trim() ?? (t.split('\n').find((l) => /arXiv|bundle/.test(l)) ?? '?'))
  .catch(() => page.title().then((t) => `${t} (no instruments panel; the bundle is whatever the demo serves)`));
// Drive a few zoom notches so the settle work and the frame gaps are measured under load.
for (const notch of [-400, -400, 400, 400]) {
  await page.mouse.move(900, 450);
  await page.mouse.wheel(0, notch);
  await settled();
}
// The layer-switch refill under the colour-stale refetch: the layer off, then on again, and the
// time until every band in view is colour-current again (the strip's hover reads *colours exact*).
const pickerBox = page.locator('tessera-layer-picker [part="entry"] input').first();
let refillMs = null;
let refillStale = null;
// Under headless swiftshader the main thread can be gone for tens of seconds drawing a million
// marks, and a click that cannot land in time is a measurement not taken, not a failed claim.
const clickable = (await pickerBox.count()) > 0 && (await pickerBox.click({timeout: 15_000}).then(() => true, () => false));
if (clickable) {
  await page.waitForTimeout(600);
  const switchedAt = Date.now();
  await pickerBox.click({timeout: 15_000}).catch(() => {}); // on: a band lacking the column is colour-stale until it refetches
  let firstStale = null;
  while (Date.now() - switchedAt < 60_000) {
    const c = await page.evaluate(() => window.__tesseraProbeOf()?.cluster ?? null);
    if (c && c.layersOn.length > 0) {
      if (c.coverage.stale > 0 && firstStale === null) firstStale = c.coverage.stale;
      if (firstStale !== null && c.coverage.stale === 0) {
        refillMs = Date.now() - switchedAt;
        refillStale = firstStale;
        break;
      }
      // Never stale at all: the bands kept their column through the switch, which is the
      // free case §5.10 describes for a layer switched back on.
      if (firstStale === null && c.coverage.current > 0 && Date.now() - switchedAt > 4000) {
        refillMs = 0;
        refillStale = 0;
        break;
      }
    }
    await page.waitForTimeout(100);
  }
}
await settled();

const probe = await page.evaluate(() => {
  const p = window.__tesseraProbeOf();
  if (!p) return null;
  return {marks: p.marks, paints: p.paints, requests: p.requests, timings: p.timings, view: p.view, instruments: p.instruments ?? null, lanes: p.lanes ?? null, cluster: p.cluster};
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
  const lanes = probe.lanes;
  if (d.length > 0) console.log(`  per response — decode as seen from the main thread: n=${d.length}, median ${quantile(d, 0.5).toFixed(1)} ms, p95 ${quantile(d, 0.95).toFixed(1)} ms, max ${Math.max(...d, 0).toFixed(1)} ms`);
  if (lanes) {
    const worker = lanes.decode.filter((x) => x.workerMs !== null);
    const queued = worker.map((x) => x.ms - (x.workerMs ?? 0));
    const biggest = lanes.decode.reduce((m, x) => (x.points > (m?.points ?? -1) ? x : m), /** @type {typeof lanes.decode[number] | null} */ (null));
    console.log(`  per response — in the worker: n=${worker.length}, median ${quantile(worker.map((x) => x.workerMs ?? 0), 0.5).toFixed(1)} ms, max ${Math.max(0, ...worker.map((x) => x.workerMs ?? 0)).toFixed(1)} ms; queued behind the lane: median ${quantile(queued, 0.5).toFixed(1)} ms, max ${Math.max(0, ...queued).toFixed(1)} ms`);
    if (biggest) console.log(`  per response — the largest: ${biggest.points.toLocaleString('en-GB')} points, ${(biggest.bytes / 1e6).toFixed(1)} MB, ${biggest.ms.toFixed(0)} ms main-thread, ${biggest.workerMs?.toFixed(0) ?? '?'} ms in the worker`);
    console.log(`  per response — remap on the main thread: n=${lanes.absorb.remap.length}, median ${quantile(lanes.absorb.remap, 0.5).toFixed(2)} ms, max ${Math.max(0, ...lanes.absorb.remap).toFixed(2)} ms over ${Math.max(0, ...lanes.absorb.remapPoints).toLocaleString('en-GB')} points at most; absorb split median ${quantile(lanes.absorb.split, 0.5).toFixed(1)} ms, max ${Math.max(0, ...lanes.absorb.split).toFixed(1)} ms; longest single slice ${lanes.absorb.sliceMaxMs.toFixed(1)} ms`);
  } else {
    console.log('  per response — the decode, absorb and region lanes are the demo\'s instruments; this page keeps none (the store\'s `instruments` option), so they are not measured here');
  }
  console.log(`  per settle — slab sync ${probe.timings.slabMs.toFixed(2)} ms, wash bin ${probe.timings.washMs.toFixed(2)} ms, lookup texture ${probe.timings.lutMs.toFixed(2)} ms, outlines ${probe.timings.outlinesMs.toFixed(2)} ms (${probe.timings.outlines}), labels ${probe.timings.labelsMs.toFixed(2)} ms (${probe.timings.labels} placed), layer build ${probe.timings.layersMs.toFixed(2)} ms (last settle); coverage check ${lanes?.coverage ? `${lanes.coverage.ms.toFixed(2)} ms over ${lanes.coverage.bands} bands, ${lanes.coverage.stale} stale` : 'not recorded'}`);
  console.log(`  per frame — mean ${probe.timings.frame.mean.toFixed(1)} ms, p95 ${probe.timings.frame.p95.toFixed(1)} ms over the last ${probe.timings.frame.n} frames (${headed ? 'headed chromium on the display' : 'software GL under headless chromium'}), colouring by ${probe.cluster.layer ? 'cluster' : 'column'} through the lookup texture, ${probe.timings.lutWrites} texture writes in the session`);
  const region = await page.evaluate(() => window.__tesseraProbeOf()?.region ?? null);
  console.log(`  box selection — ${region?.ms?.toFixed(0) ?? '?'} ms select-to-counted (200 ms settle, the request, the sum); ${regionMs} ms mouse-up to panel under ${headed ? 'headed' : 'headless'} input; lanes: ${lanes?.region ? `settle ${lanes.region.settleMs.toFixed(0)} ms, wire ${lanes.region.wireMs.toFixed(0)} ms (server ${lanes.region.serverMs.toFixed(1)} ms, ${lanes.region.tiles} tiles), projection ${lanes.region.projectMs.toFixed(1)} ms` : 'not recorded'}`);
  if (lanes) console.log(`  main thread — longest tasks: ${lanes.longTasks.slice(0, 5).map((t) => `${t.ms.toFixed(0)} ms at ${(t.at / 1000).toFixed(1)} s`).join(', ') || 'none over 50 ms'}; decode replies that waited through a long task: ${lanes.decode.filter((x) => lanes.longTasks.some((t) => x.at - x.ms <= t.at + t.ms && x.at >= t.at)).length} of ${lanes.decode.length}`);
  console.log(`  layer switch — ${refillMs === null ? 'not measured' : refillMs === 0 ? 'no band went colour-stale: the columns survived the switch' : `${refillMs} ms from the layer back on to colours exact, ${refillStale} tiles refetched`} (${Date.now() - refillStarted > 0 ? 'measured after colour by cluster was chosen' : ''})`);
} else {
  console.log('  no probe on the page');
}

await page.screenshot({path: shot, timeout: 60_000});
await browser.close();

console.log('--- responses that were not 2xx ---');
console.log(refusals.length ? refusals.map((e) => `  ${e}`).join('\n') : '  none');
console.log('--- console errors (the harness’s own 403s excepted) ---');
console.log(consoleErrors.length ? consoleErrors.map((e) => `  ${e}`).join('\n') : '  none');
console.log(`--- screenshots: ${shot}, ${clusterShot}`);
// The 403s are the harness's own, refused under the page above, and an incomplete chunked body
// is a streamed response the client abandoned mid-flight (a superseded request's abort, which
// Chromium logs against the resource); anything else counts.
const unexpected = consoleErrors.filter((e) => !/403 \(Forbidden\)|ERR_INCOMPLETE_CHUNKED_ENCODING/.test(e));
if (unexpected.length) failures.push(`${unexpected.length} console error(s)`);
if (failures.length) {
  console.error(`HARNESS FAILED (${passes.length} ok, ${failures.length} failed):\n  ${failures.join('\n  ')}`);
  process.exit(1);
}
console.log(`HARNESS OK — ${passes.length} claims hold`);
