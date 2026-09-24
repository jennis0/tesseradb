#!/usr/bin/env node
// The time from the user stopping a pan to the points being on screen, with and without look-ahead.
//
//   node clients/ts/viewer/smoke-latency.mjs [--clients N] [--url http://localhost:5173]
//     [--headed] [--executable /path/to/chrome]
//
// Requires a running `tessera serve` and `vite dev`.
//
// It measures:
//
//  1. Pan-to-paint latency, timed in the page from `window.__tesseraProbe`. Request counts cannot
//     show a pan answered from held bands, which makes no request.
//  2. How it varies with pan speed: look-ahead covers a fixed distance, which a fast pan crosses
//     sooner, so the script sweeps speeds.
//  3. Contention: look-ahead is speculative server work paid per client. `--clients N` runs N
//     pages against one server.
import {flags, launchBrowser, withParams} from './smoke-browser.mjs';

const args = flags();
const CLIENTS = Number(args.clients ?? 1);
const BASE_URL = args.url ?? 'http://localhost:5173';
const PAN_PX = 420;
// Pixels per second. A slow drag is a careful read; 3000 px/s is a flick across the screen.
const SPEEDS = [300, 1000, 2500];

// Pan-to-paint is timed to the paint, and a software-rasterised frame over a few million marks
// takes seconds, so use `--headed`.
const browser = await launchBrowser(args);

/** Drive one page: settle it, then sweep pan speeds, reporting latency per pan. */
async function runClient(index, prefetch) {
  const page = await browser.newPage({viewport: {width: 1280, height: 800}});
  const serverUs = {total: 0};
  page.on('response', (r) => {
    if (new URL(r.url()).pathname === '/v1/viewport') {
      serverUs.total += Number(r.headers()['x-tessera-server-us'] ?? 0);
    }
  });

  const url = prefetch ? BASE_URL : withParams(BASE_URL, {prefetch: 0});
  await page.goto(url, {waitUntil: 'load'});
  await page.waitForTimeout(6000);
  const options = await page.locator('#principal option').count();
  await page.selectOption('#principal', String(options - 1));
  await page.waitForTimeout(6000);

  // Zoom in: at zoom 0 the world bbox clamps and a pan changes nothing.
  for (let i = 0; i < 5; i++) {
    await page.mouse.move(450, 400);
    await page.mouse.wheel(0, -240);
    await page.waitForTimeout(800);
  }
  await page.waitForTimeout(3000);

  // Settle the depth budget: bands are keyed by depth, and a moving `m_target` lands every view
  // where nothing is held.
  for (let i = 0; i < 5; i++) {
    await pan(page, -200, 800);
    await pan(page, 200, 800);
    await page.waitForTimeout(1200);
  }

  const results = [];
  for (const speed of SPEEDS) {
    // A run of same-direction pans, which is what crossing a map looks like.
    const samples = [];
    for (let i = 0; i < 6; i++) {
      await page.waitForTimeout(1200); // let anticipation happen, if it is enabled
      samples.push(await timedPan(page, -PAN_PX, speed));
    }
    // Come back, so the next speed starts from comparable ground rather than off the corpus.
    for (let i = 0; i < 6; i++) await pan(page, PAN_PX, 2000);
    await page.waitForTimeout(1500);
    const w = samples.filter((x) => x.requests > 0).map((x) => x.ms);
    console.log(
      `    [c${index} ${prefetch ? 'on ' : 'off'}] ${String(speed).padStart(4)} px/s: ` +
        `${samples.length - w.length}/${samples.length} free, p50 ${w.length ? p(w, 0.5) : '-'} ms`
    );
    results.push({speed, samples});
  }

  const out = {index, prefetch, serverUs: serverUs.total, results};
  await page.close();
  return out;
}

async function pan(page, dx, pxPerSecond) {
  const steps = 12;
  const stepMs = Math.max(1, (Math.abs(dx) / pxPerSecond) * 1000 / steps);
  await page.mouse.move(450, 400);
  await page.mouse.down();
  for (let i = 1; i <= steps; i++) {
    await page.mouse.move(450 + (dx * i) / steps, 400);
    await page.waitForTimeout(stepMs);
  }
  await page.mouse.up();
}

/**
 * One pan, timed from mouse-up to the paint that answers it. A pan needing nothing repaints within
 * a frame or two with no request; one that waits costs the debounce and a round trip.
 */
async function timedPan(page, dx, speed) {
  await pan(page, dx, speed);
  // Counters are read after mouse-up: a fast drag outlives the debounce, so a repaint mid-pan
  // answers an earlier position.
  const before = await page.evaluate(() => ({
    paints: window.__tesseraProbe?.paints ?? 0,
    requests: window.__tesseraProbe?.requests ?? 0
  }));
  const t0 = Date.now();
  const settled = await page
    .waitForFunction(
      (b) => {
        const p = window.__tesseraProbe;
        return p && p.paints > b.paints ? {requests: p.requests, marks: p.marks} : null;
      },
      before,
      // Short, since a pan that needs nothing never repaints and pays this timeout.
      {timeout: 2000, polling: 16}
    )
    .then((h) => h.jsonValue())
    .catch(() => null);
  const ms = Date.now() - t0;
  if (!settled) return {ms: null, requests: 0}; // nothing repainted: the view was already correct
  return {ms, requests: settled.requests - before.requests};
}

const p = (xs, q) => {
  if (xs.length === 0) return null;
  const s = [...xs].sort((a, b) => a - b);
  return s[Math.min(s.length - 1, Math.floor(q * s.length))];
};

for (const prefetch of [false, true]) {
  const runs = await Promise.all(
    Array.from({length: CLIENTS}, (_, i) => runClient(i, prefetch))
  );
  const totalServerMs = runs.reduce((a, r) => a + r.serverUs, 0) / 1000;
  console.log(`\n=== look-ahead ${prefetch ? 'ON ' : 'OFF'} · ${CLIENTS} client(s) ===`);
  console.log('  speed px/s   pans free   p50 wait   p95 wait   requests');
  for (let s = 0; s < SPEEDS.length; s++) {
    const all = runs.flatMap((r) => r.results[s].samples);
    const waited = all.filter((x) => x.requests > 0).map((x) => x.ms);
    const free = all.length - waited.length;
    const reqs = all.reduce((a, x) => a + x.requests, 0);
    console.log(
      `  ${String(SPEEDS[s]).padStart(9)}   ${String(free).padStart(2)}/${String(all.length).padEnd(6)}` +
        `  ${String(p(waited, 0.5) ?? '—').padStart(8)}   ${String(p(waited, 0.95) ?? '—').padStart(8)}` +
        `   ${String(reqs).padStart(8)}`
    );
  }
  console.log(`  server CPU across all clients: ${totalServerMs.toFixed(0)} ms`);
}

await browser.close();
