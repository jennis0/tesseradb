#!/usr/bin/env node
// How does look-ahead affect the SERVER as clients are added?
//
//   node probes/2026-08-08-lookahead-contention/load.mjs [--clients N] [--depth D] [--steps S]
//
// Needs a running `tessera serve` (run_demo.sh --no-viewer) and TESSERA_SESSION_CRED.
//
// **No browser, deliberately.** Driving N headless pages measures software rasterisation of N
// deck.gl canvases long before it measures the engine: at three clients the box saturated and the
// run produced nothing in twenty minutes. This replays exactly what the real client puts on the
// wire — a foreground tile list per pan, plus a ring when look-ahead is on, both filtered against
// a held set that emulates the replica — and nothing else.
//
// The emulation is faithful in the one way that matters: the ring is mostly already held, so what
// it costs the server is only its novel remainder, which is the whole question.
const arg = (n, d) => {
  const i = process.argv.indexOf(`--${n}`);
  return i >= 0 ? Number(process.argv[i + 1]) : d;
};
const CLIENTS = arg('clients', 1);
const DEPTH = arg('depth', 10);
const STEPS = arg('steps', 12);
// Probability that a pan reverses direction. Zero is a straight traverse, where the ring's guess
// is always right; higher values are a user who changes their mind, where it is sometimes wrong.
// This is the parameter the whole trade turns on: look-ahead moves work earlier when it guesses
// right, and adds work when it guesses wrong.
const TURN = arg('turn', 0) / 100;
const VIEWER = process.env.TESSERA_VIEWER_URL ?? 'http://127.0.0.1:37585';
const SESSION = process.env.TESSERA_SESSION_URL ?? 'http://127.0.0.1:49303';
const CRED = process.env.TESSERA_SESSION_CRED ?? 'dev-session-credential';

// The viewer's own constants, so the shapes match what a real client sends.
const MARGIN = 1.3;
const RING_MARGIN = 2.2;
// A depth-10 viewport on the demo corpus spans ~130x127 tiles; the ring ~2.9x that. Measured from
// the browser client, so the request sizes here are the ones the engine actually sees.
const VIEW_W = 130;
const VIEW_H = 127;
const PAN_TILES = 44; // ~a third of the viewport, which is what a drag moves

const morton = (x, y, depth) => {
  let p = 0n;
  for (let b = 0; b < depth; b++) {
    p |= BigInt((x >> b) & 1) << BigInt(2 * b);
    p |= BigInt((y >> b) & 1) << BigInt(2 * b + 1);
  }
  return p;
};

function block(cx, cy, w, h, depth) {
  const max = 2 ** depth - 1;
  const out = [];
  const x0 = Math.max(0, Math.round(cx - w / 2));
  const x1 = Math.min(max, Math.round(cx + w / 2));
  const y0 = Math.max(0, Math.round(cy - h / 2));
  const y1 = Math.min(max, Math.round(cy + h / 2));
  for (let y = y0; y <= y1; y++) for (let x = x0; x <= x1; x++) out.push(morton(x, y, depth));
  return out;
}

async function authorise(terms) {
  const r = await fetch(`${SESSION}/session/authorise`, {
    method: 'POST',
    headers: {authorization: `Bearer ${CRED}`, 'content-type': 'application/json'},
    body: JSON.stringify({auth_data: Buffer.from(JSON.stringify({terms})).toString('base64')})
  });
  if (!r.ok) throw new Error(`authorise ${r.status}`);
  return (await r.json()).token;
}

async function viewport(token, tiles, k = 500) {
  const t0 = performance.now();
  const r = await fetch(`${VIEWER}/v1/viewport`, {
    method: 'POST',
    headers: {authorization: `Bearer ${token}`, 'content-type': 'application/json'},
    body: JSON.stringify({view: 's0', zoom: DEPTH, tiles: tiles.map(Number), k})
  });
  const body = await r.arrayBuffer();
  return {
    wallMs: performance.now() - t0,
    serverUs: Number(r.headers.get('x-tessera-server-us') ?? 0),
    status: r.status,
    bytes: body.byteLength
  };
}

async function runClient(id, prefetch, stats) {
  const token = await authorise(['4']);
  const held = new Set();
  const inflight = [];
  // Each client explores its own strip, so they are not trivially sharing a warm cache.
  // Kept inside the grid. At depth 10 the index tops out at 1023, and a strip that runs off the
  // edge clamps: the block stops changing, every pan is trivially "already held", and the arm
  // scores free pans it never earned. Clients are spread over the interior and reverse at the end.
  const max = 2 ** DEPTH - 1;
  const lo = Math.round(VIEW_W * RING_MARGIN) + 4;
  const hi = max - lo;
  let cx = lo + Math.round(((hi - lo) * id) / Math.max(1, CLIENTS));
  let dir = 1;
  const cy = Math.min(hi, Math.max(lo, 400 + id * 17));

  for (let step = 0; step < STEPS; step++) {
    const want = block(cx, cy, VIEW_W * MARGIN, VIEW_H * MARGIN, DEPTH);
    const fetchList = want.filter((t) => !held.has(t));
    if (fetchList.length > 0) {
      const r = await viewport(token, fetchList);
      if (r.status === 429) stats.shed += 1;
      else {
        stats.foreground.push(r.wallMs);
        stats.fgServer.push(r.serverUs / 1000);
        stats.serverUs += r.serverUs;
        stats.bytes += r.bytes;
        for (const t of want) held.add(t);
      }
    } else {
      stats.free += 1;
    }
    stats.pans += 1;

    if (prefetch) {
      const ring = block(cx, cy, VIEW_W * RING_MARGIN, VIEW_H * RING_MARGIN, DEPTH);
      const ringFetch = ring.filter((t) => !held.has(t));
      if (ringFetch.length > 0) {
        // **Not awaited, because the real client does not await it.** The ring is fired while the
        // view is still and the user carries on; blocking the next pan behind it would serialise
        // this driver in a way the client never is, and would charge look-ahead for a delay it
        // does not cause. Held is marked optimistically for the same reason: the real replica
        // records the tiles as it absorbs them, not before the next pan is allowed to start.
        for (const t of ring) held.add(t);
        inflight.push(
          viewport(token, ringFetch).then((r) => {
            if (r.status === 429) stats.shed += 1;
            else {
              stats.ringMs.push(r.wallMs);
              stats.serverUs += r.serverUs;
              stats.bytes += r.bytes;
            }
          })
        );
      }
    }
    if (TURN > 0 && Math.random() < TURN) dir = -dir;
    cx += PAN_TILES * dir;
    if (cx > hi || cx < lo) {
      dir = -dir;
      cx += 2 * PAN_TILES * dir;
    }
    await new Promise((r) => setTimeout(r, 250)); // think time between pans
  }
  await Promise.all(inflight);
}

const pct = (xs, q) => {
  if (!xs.length) return null;
  const s = [...xs].sort((a, b) => a - b);
  return Math.round(s[Math.min(s.length - 1, Math.floor(q * s.length))]);
};

console.log(
  `depth ${DEPTH}, ${STEPS} pans/client, viewport ${VIEW_W}x${VIEW_H} tiles, ` +
    `${(TURN * 100).toFixed(0)}% chance of reversing per pan`
);
for (const prefetch of [false, true]) {
  const stats = {foreground: [], fgServer: [], ringMs: [], serverUs: 0, bytes: 0, shed: 0, free: 0, pans: 0};
  const t0 = performance.now();
  await Promise.all(
    Array.from({length: CLIENTS}, (_, i) => runClient(i, prefetch, stats))
  );
  const wall = (performance.now() - t0) / 1000;
  console.log(
    `\nlook-ahead ${prefetch ? 'ON ' : 'OFF'} · ${CLIENTS} client(s) · ${wall.toFixed(1)}s wall`
  );
  console.log(`  pans                    ${stats.pans}  (${stats.free} needed no request)`);
  // **Wall and server reported apart, because only one of them is the server's.** This driver
  // multiplexes every simulated client through one process's connection pool, so fire-and-forget
  // ring requests queue ahead of a foreground one in a way separate browsers never would. Wall
  // time therefore charges look-ahead for the driver's own contention; the server's own figure
  // does not, and is what the engine actually spent.
  console.log(
    `  foreground  server p50 ${String(pct(stats.fgServer, 0.5)).padStart(5)} ms  p95 ${String(pct(stats.fgServer, 0.95)).padStart(5)} ms` +
      `   |  driver wall p50 ${String(pct(stats.foreground, 0.5)).padStart(5)} ms`
  );
  if (stats.ringMs.length) {
    console.log(`  ring request     p50 ${String(pct(stats.ringMs, 0.5)).padStart(5)} ms   p95 ${String(pct(stats.ringMs, 0.95)).padStart(5)} ms  (${stats.ringMs.length} of them)`);
  }
  console.log(
    `  server CPU            ${(stats.serverUs / 1000).toFixed(0)} ms total, ` +
      `${(stats.serverUs / 1000 / stats.pans).toFixed(2)} ms per pan`
  );
  console.log(`  wire                  ${(stats.bytes / 1e6).toFixed(1)} MB`);
  console.log(`  shed (429)            ${stats.shed}`);
}
