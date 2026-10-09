// The session and map sections of `interactive_bench.py`: the TypeScript core's own store, driver
// and replica, driven by a fixed camera script, with every request timed on the wire.
//
//   MOSAICA_BENCH_SESSION_CRED=… node test_corpora/common/interactive_bench.mjs <plan.json> <out.json>
//
// The plan names the core's dist, the server, the screen, the principals and the script; the
// Python side writes it. The core may be any build of `clients/ts/core`, so one script measures a
// server and the client of its own commit: an older core asks for artifacts on `/v1/viewport`, a
// newer one on `/v1/artifacts/viewport`, and both are recorded under the same kinds. Requests go
// through a fetch that runs in a worker thread, so the times it records (counts frame, first
// points frame, last byte) are when the bytes arrived, not when this thread, busy decoding, got
// round to reading them.
import {readFileSync, writeFileSync} from 'node:fs';
import {Worker} from 'node:worker_threads';

const [planPath, outPath] = process.argv.slice(2);
const plan = JSON.parse(readFileSync(planPath, 'utf8'));
const {createStore, MosaicaClient, inlineDecoder} = await import(plan.core);
const {tableFromIPC} = await import(plan.arrow);
const cred = process.env.MOSAICA_BENCH_SESSION_CRED;
if (!cred) throw new Error('set MOSAICA_BENCH_SESSION_CRED to the deployment operator credential');

const now = () => performance.timeOrigin + performance.now();

// ---------------------------------------------------------------------------------------------
// The wire: fetch in a worker thread, timing each response's frames as they arrive
// ---------------------------------------------------------------------------------------------

const wireSource = `
const {parentPort} = require('node:worker_threads');
const now = () => performance.timeOrigin + performance.now();
const controllers = new Map();
parentPort.on('message', async (m) => {
  if (m.abort !== undefined) { controllers.get(m.abort)?.abort(); return; }
  const {id, url, init} = m;
  const controller = new AbortController();
  controllers.set(id, controller);
  const t = {start: now()};
  try {
    const r = await fetch(url, {...init, signal: controller.signal});
    t.headers = now();
    parentPort.postMessage({id, head: {status: r.status, headers: [...r.headers]}, t: t.headers});
    // Frames are read as they stream past: a 5-byte header (kind, then a u32 LE length) and a
    // payload, kept only for the trailer. Nothing is copied but the header and the trailer, so a
    // frame of any size costs the time its bytes take to arrive.
    const header = new Uint8Array(5);
    let headerFill = 0;
    let kind = 0;
    let remaining = -1;
    let trailer = null;
    let trailerFill = 0;
    let bytes = 0;
    const reader = r.body.getReader();
    for (;;) {
      const {done, value} = await reader.read();
      const at = now();
      if (done) break;
      t.firstByte ??= at;
      bytes += value.byteLength;
      let i = 0;
      for (;;) {
        if (remaining < 0) {
          const n = Math.min(5 - headerFill, value.length - i);
          header.set(value.subarray(i, i + n), headerFill);
          headerFill += n;
          i += n;
          if (headerFill < 5) break;
          headerFill = 0;
          kind = header[0];
          remaining = new DataView(header.buffer).getUint32(1, true);
          trailer = kind === 4 ? new Uint8Array(remaining) : null;
          trailerFill = 0;
        }
        const n = Math.min(remaining, value.length - i);
        if (trailer) trailer.set(value.subarray(i, i + n), trailerFill);
        trailerFill += n;
        i += n;
        remaining -= n;
        if (remaining > 0) break;
        if (kind === 1) t.counts ??= at;
        if (kind === 3) t.points ??= at;
        if (kind === 4) t.trailer = JSON.parse(new TextDecoder().decode(trailer));
        remaining = -1;
        if (i === value.length) break;
      }
      parentPort.postMessage({id, chunk: value}, [value.buffer]);
    }
    t.lastByte = now();
    parentPort.postMessage({id, done: true, t, bytes});
  } catch (e) {
    parentPort.postMessage({id, error: String(e && e.name === 'AbortError' ? 'aborted' : e), t});
  } finally {
    controllers.delete(id);
  }
});
`;
const wire = new Worker(wireSource, {eval: true});
wire.unref();
const pending = new Map();
let nextId = 1;
let inFlight = 0;
let lastActivity = now();
/** Every request, in issue order, with what was sent and when its parts arrived. */
const log = [];
let step = null;

wire.on('message', (m) => {
  const p = pending.get(m.id);
  if (!p) return;
  if (m.head) {
    const body = new ReadableStream({
      start(c) {
        p.stream = c;
      },
      cancel() {
        wire.postMessage({abort: m.id});
      }
    });
    p.record.status = m.head.status;
    p.record.pin = Object.fromEntries(m.head.headers)['x-mosaica-pin'] ?? null;
    p.resolve(new Response(body, {status: m.head.status, headers: m.head.headers}));
  } else if (m.chunk) {
    p.stream?.enqueue(new Uint8Array(m.chunk));
  } else {
    if (m.done) p.stream?.close();
    else if (p.stream) p.stream.error(new Error(m.error));
    else p.reject(new Error(m.error));
    const t = m.t ?? {};
    const rel = (x) => (x === undefined ? null : Math.round((x - p.record.t0) * 10) / 10);
    Object.assign(p.record, {
      headers_ms: rel(t.headers),
      counts_ms: rel(t.counts),
      points_ms: rel(t.points),
      last_byte_ms: rel(t.lastByte ?? now()),
      bytes: m.bytes ?? 0,
      server_stream_ms: t.trailer?.stream_us === undefined ? null : t.trailer.stream_us / 1000,
      // A framed body ends with its trailer; one that ends without it was cut by the server.
      shed: m.done && p.record.framed ? !t.trailer : false,
      error: m.error ?? null
    });
    pending.delete(m.id);
    inFlight -= 1;
    lastActivity = now();
  }
});

/** The `fetch` the store's client uses. */
function wireFetch(url, init = {}) {
  const id = nextId++;
  const body = typeof init.body === 'string' ? init.body : init.body ? new TextDecoder().decode(init.body) : null;
  const t0 = now();
  const record = {id, step: step?.id ?? null, path: new URL(url).pathname, method: init.method ?? 'GET', body, t0};
  record.start_ms = step?.t0 === undefined ? null : Math.round((t0 - step.t0) * 10) / 10;
  record.kind = kindOf(record);
  record.whole_level = wholeLevel(record, step?.zoom ?? 0);
  record.extra = beyondReference(record);
  record.framed = FRAMED.has(record.path);
  log.push(record);
  inFlight += 1;
  lastActivity = now();
  return new Promise((resolve, reject) => {
    pending.set(id, {resolve, reject, record});
    init.signal?.addEventListener('abort', () => wire.postMessage({abort: id}));
    const headers = Object.fromEntries(new Headers(init.headers ?? {}));
    wire.postMessage({id, url, init: {method: init.method, headers, body}});
  });
}

const FRAMED = new Set(['/v1/viewport', '/v1/items', '/v1/artifacts/viewport']);

/**
 * What a request is for. `artifacts` is a request for artifacts, whichever route the core asks
 * on; `promotion` is an older core's idle fetch of one whole level, which `settled` leaves out.
 * `artifacts-by-id` reads the tags a newer core's held tiles do not name.
 */
function kindOf(record) {
  if (record.path === '/v1/artifacts/viewport') return 'artifacts';
  if (record.path === '/v1/artifacts' && record.method === 'POST') return 'artifacts-by-id';
  if (record.path !== '/v1/viewport') return record.path.replace(/^\/v1\//, '');
  const b = JSON.parse(record.body);
  if (b.k !== 0) return 'marks';
  if (!b.computed) return 'counts';
  // An older core's idle fetch of one whole scope carries no artifact budget.
  return b.artifact_budget === undefined ? 'promotion' : 'artifacts';
}

/**
 * Whether an artifact request asks for a whole level: an older core's promotion, or every tile of a
 * depth more than two below the map zoom, which no view at that zoom needs. A view of the world
 * asks for every tile of its own depth and is not counted.
 */
function wholeLevel(record, mapZoom) {
  if (record.kind === 'promotion') return true;
  if (record.kind !== 'artifacts' || record.path !== '/v1/artifacts/viewport') return false;
  const b = JSON.parse(record.body);
  return b.zoom > Math.floor(mapZoom) + 2 && (b.tiles?.length ?? 0) >= 4 ** b.zoom;
}

/**
 * The requests a reference run (the same script with the store's prefetch off) sent, counted by
 * what they sent, as `interactive_bench.py`'s `sent` keys them. A request beyond those is the
 * prefetch's work, and is kept out of a step's points and layers.
 */
const reference = new Map(plan.reference ?? []);

function beyondReference(record) {
  if (!plan.reference) return false;
  const body = record.path === '/session/authorise' && record.body ? '<body>' : record.body;
  const key = JSON.stringify([record.step, record.method, record.path, body]);
  const left = reference.get(key) ?? 0;
  if (left === 0) return true;
  reference.set(key, left - 1);
  return false;
}

// ---------------------------------------------------------------------------------------------
// The camera
// ---------------------------------------------------------------------------------------------

const [W, H] = plan.screen;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** The data-space box a `W` by `H` screen shows at map zoom `z` around `centre`; zoom 0 fits the extent's height. */
function boxAt(q, centre, z) {
  const sy = (q.yMax - q.yMin) / 2 ** z;
  const sx = ((q.xMax - q.xMin) / 2 ** z) * (W / H);
  return [centre[0] - sx / 2, centre[1] - sy / 2, centre[0] + sx / 2, centre[1] + sy / 2];
}

/** Wait until nothing is in flight and nothing has started or ended for `plan.quiet_ms`. */
async function settled(since) {
  const deadline = since + plan.step_timeout_ms;
  for (;;) {
    await sleep(25);
    const t = now();
    if (inFlight === 0 && t - lastActivity >= plan.quiet_ms && t - since >= plan.quiet_ms) return true;
    if (t > deadline) return false;
  }
}

/** Show one camera and wait for the store to settle; the step's requests are those it issued. */
async function show(store, q, s) {
  const t0 = now();
  step = {id: s.id, ...s, t0};
  // Map zoom 0 fits the extent's height, 512 world units, in H pixels.
  store.setView({bbox: boxAt(q, s.centre, s.zoom), zoom: s.zoom + Math.log2(H / 512), width: W, height: H});
  const ok = await settled(t0);
  const requests = log.filter((r) => r.step === s.id);
  const rel = (r, field) => (r[field] === null || r[field] === undefined ? null : r.t0 - t0 + r[field]);
  // The view's own requests: without a prefetch's work beyond the reference run.
  const viewed = requests.filter((r) => !r.extra);
  const firstOf = (field) => {
    const v = viewed.filter((r) => r.kind === 'marks' || r.kind === 'counts').map((r) => rel(r, field)).filter((x) => x !== null);
    return v.length ? Math.min(...v) : null;
  };
  const lastByte = viewed.filter((r) => r.kind === 'marks').map((r) => rel(r, 'last_byte_ms')).filter((x) => x !== null);
  // Everything the store asked for but an older core's promotion, the prefetch's work included.
  const settledAt = requests.filter((r) => r.kind !== 'promotion').map((r) => rel(r, 'last_byte_ms')).filter((x) => x !== null);
  const extra = requests.filter((r) => r.extra);
  const extraAt = extra.map((r) => rel(r, 'last_byte_ms')).filter((x) => x !== null);
  const layers = viewed.filter((r) => r.kind === 'artifacts');
  const layersAt = layers.map((r) => rel(r, 'last_byte_ms')).filter((x) => x !== null);
  // The points requests in flight at some moment an artifacts request of the view was.
  const span = (r) => [r.t0, r.t0 + (r.last_byte_ms ?? 0)];
  const beside = viewed.filter((r) => r.kind === 'marks' && layers.some((a) => span(a)[0] < span(r)[1] && span(r)[0] < span(a)[1]));
  const besideAt = beside.map((r) => rel(r, 'last_byte_ms')).filter((x) => x !== null);
  const result = {
    ...s,
    t0,
    timed_out: !ok,
    requests: requests.length,
    counts_ms: firstOf('counts_ms'),
    first_points_ms: firstOf('points_ms'),
    last_byte_ms: lastByte.length ? Math.max(...lastByte) : null,
    settled_ms: settledAt.length ? Math.max(...settledAt) : null,
    layers_ms: layersAt.length ? Math.max(...layersAt) : null,
    points_beside_layers_ms: besideAt.length ? Math.max(...besideAt) : null,
    extra_ms: extraAt.length ? Math.max(...extraAt) : null,
    extra_requests: extra.length,
    whole_level: requests.filter((r) => r.whole_level).length,
    bytes: requests.reduce((n, r) => n + (r.bytes ?? 0), 0),
    kinds: Object.fromEntries([...new Set(requests.map((r) => r.kind))].map((k) => [k, requests.filter((r) => r.kind === k).length])),
    shed: requests.filter((r) => r.shed).length,
    errors: requests.filter((r) => r.error || (r.status && r.status >= 400)).length
  };
  step = null;
  return result;
}

// ---------------------------------------------------------------------------------------------
// A principal
// ---------------------------------------------------------------------------------------------

const client = new MosaicaClient({viewerUrl: plan.viewer, sessionUrl: plan.session, sessionCredential: cred, decoder: inlineDecoder(), fetch: wireFetch});

/** Authorise, read meta, build the viewer's store and show the whole extent. */
async function open(principal, label, keepIds) {
  step = {id: `${principal.label}/${label}`};
  const t0 = now();
  const session = await client.authorise({terms: principal.terms});
  const tToken = now();
  const meta = await client.meta(session.token);
  const view = meta.views[0];
  const q = view.quantisation;
  const ids = new Set();
  const storeClient = new MosaicaClient({viewerUrl: plan.viewer, sessionUrl: '', decoder: inlineDecoder(), fetch: wireFetch});
  if (keepIds) {
    const viewport = storeClient.viewport.bind(storeClient);
    storeClient.viewport = (token, req, opts = {}) =>
      viewport(token, req, {
        ...opts,
        ...(opts.onPart ? {onPart: (part) => {
          for (const id of part.result.ids) ids.add(id);
          return opts.onPart(part);
        }} : {})
      });
  }
  const store = createStore({
    viewerUrl: plan.viewer,
    token: session.token,
    client: storeClient,
    meta,
    budget: plan.budget,
    view: view.id,
    prefetch: plan.prefetch,
    // A newer core draws a layer's artifacts only with a number per tile; an older one ignores it.
    artifacts: {perTile: plan.per_tile},
    replica: {revalidateAfterMs: 1e12}
  });
  // As the demo viewer: colour by the first layer's clusters with that layer on, else by a column.
  const layer = meta.layers[0]?.name ?? null;
  const rendered = meta.declaredScalars.filter((c) => c.render);
  store.setColourBy(layer ? `cluster:${layer}` : (rendered.find((c) => c.name === 'archive') ?? rendered.find((c) => c.category) ?? rendered[0])?.name ?? null);
  store.setLayers(layer ? [layer] : []);
  if (plan.title_field) store.setPointColumns('map#1', [plan.title_field]);
  const centre = [(q.xMin + q.xMax) / 2, (q.yMin + q.yMax) / 2];
  const opened = await show(store, q, {id: `${principal.label}/${label}`, kind: 'open', region: 'world', zoom: 0, centre});
  const rel = (x) => (x === null ? null : Math.round((x + (opened.t0 - tToken)) * 10) / 10);
  return {
    store,
    q,
    view,
    ids,
    token: session.token,
    result: {
      authorise_ms: Math.round((tToken - t0) * 10) / 10,
      counts_ms: rel(opened.counts_ms),
      first_points_ms: rel(opened.first_points_ms),
      last_byte_ms: rel(opened.last_byte_ms),
      settled_ms: rel(opened.settled_ms),
      layers_ms: rel(opened.layers_ms),
      points_beside_layers_ms: rel(opened.points_beside_layers_ms),
      extra_ms: rel(opened.extra_ms),
      extra_requests: opened.extra_requests,
      whole_level: opened.whole_level,
      bytes: opened.bytes,
      kinds: opened.kinds,
      requests: opened.requests,
      shed: opened.shed,
      errors: opened.errors,
      timed_out: opened.timed_out
    }
  };
}

/** This principal's densest cells at depth 8, in distinct depth-3 ancestors, by a counts request
 * the bench makes on its own account, outside every measured step. */
async function densest(token, view, q, n) {
  const depth = 8;
  const r = await fetch(`${plan.viewer}/v1/viewport`, {
    method: 'POST',
    headers: {authorization: `Bearer ${token}`, 'content-type': 'application/json'},
    body: JSON.stringify({view: view.id, zoom: depth, bbox: [q.xMin, q.yMin, q.xMax, q.yMax], k: 0, layers: []})
  });
  const buf = new Uint8Array(await r.arrayBuffer());
  if (buf[0] !== 1) throw new Error(`expected the tiles frame first, got kind ${buf[0]}`);
  const len = new DataView(buf.buffer, 1, 4).getUint32(0, true);
  const tiles = tableFromIPC(buf.subarray(5, 5 + len));
  const prefixes = tiles.getChild('tile').toArray();
  const visible = tiles.getChild('visible').toArray();
  const cells = [];
  let total = 0;
  for (let i = 0; i < prefixes.length; i++) {
    const p = BigInt(prefixes[i]);
    let x = 0;
    let y = 0;
    for (let b = 0; b < depth; b++) {
      x |= Number((p >> BigInt(2 * b)) & 1n) << b;
      y |= Number((p >> BigInt(2 * b + 1)) & 1n) << b;
    }
    cells.push({x, y, count: Number(visible[i])});
    total += Number(visible[i]);
  }
  cells.sort((a, b) => b.count - a.count || a.x - b.x || a.y - b.y);
  const seen = new Set();
  const out = [];
  for (const c of cells) {
    const key = `${c.x >> (depth - 3)},${c.y >> (depth - 3)}`;
    if (seen.has(key)) continue;
    seen.add(key);
    const span = 2 ** depth;
    out.push({
      name: `dense-${out.length + 1}`,
      centre: [q.xMin + ((c.x + 0.5) / span) * (q.xMax - q.xMin), q.yMin + ((c.y + 0.5) / span) * (q.yMax - q.yMin)],
      cell: [c.x, c.y],
      count: c.count
    });
    if (out.length === n) break;
  }
  return {regions: out, visible: total};
}

/** The camera script for a principal's regions. */
function script(principal, regions, q) {
  const steps = [];
  const world = [(q.xMin + q.xMax) / 2, (q.yMin + q.yMax) / 2];
  for (const region of regions) {
    for (const z of plan.zooms) {
      steps.push({kind: 'zoom-in', region: region.name, zoom: z, centre: region.centre});
      let at = region.centre;
      for (const direction of plan.pans[String(z)] ?? []) {
        const [x0, y0, x1, y1] = boxAt(q, at, z);
        at = direction === 'east' ? [at[0] + (x1 - x0) / 2, at[1]] : [at[0], at[1] - (y1 - y0) / 2];
        steps.push({kind: 'pan', region: region.name, zoom: z, centre: at});
      }
    }
    steps.push({kind: 'zoom-out', region: 'world', zoom: 0, centre: world});
  }
  return steps.map((s, i) => ({...s, id: `${principal.label}/map/${i}`}));
}

const out = {principals: []};
// After a restart over the same cache: each principal opens once more, as the first viewer of a
// server that has kept what the last one computed.
if (plan.phase === 'reopen') {
  for (const principal of plan.principals) {
    console.error(`${principal.label}: reopening`);
    const reopen = await open(principal, 'reopen', false);
    reopen.store.dispose();
    out.principals.push({label: principal.label, reopen: reopen.result});
  }
  out.requests = log.map(({t0, ...r}) => r);
  writeFileSync(outPath, JSON.stringify(out));
  process.exit(0);
}
for (const principal of plan.principals) {
  console.error(`${principal.label}: opening`);
  const first = await open(principal, 'first', false);
  first.store.dispose();
  const again = await open(principal, 'again', principal.ids);
  const found = await densest(again.token, again.view, again.q, plan.regions);
  const entry = {label: principal.label, terms: principal.terms.length, visible: found.visible, first: first.result, again: again.result, regions: found.regions};
  if (principal.map) {
    console.error(`${principal.label}: map`);
    entry.map = [];
    for (const s of script(principal, found.regions, again.q)) entry.map.push(await show(again.store, again.q, s));
  }
  if (principal.ids) {
    // A fixed draw from the identifiers served, so two runs over one bundle ask for the same items.
    const sorted = [...again.ids].sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
    let seed = 2026;
    const pick = [];
    const taken = new Set();
    while (pick.length < Math.min(plan.lookups, sorted.length)) {
      seed = (seed * 1103515245 + 12345) % 2 ** 31;
      const i = seed % sorted.length;
      if (taken.has(i)) continue;
      taken.add(i);
      pick.push(sorted[i].toString());
    }
    entry.ids_served = sorted.length;
    entry.lookup_ids = pick;
    entry.token = again.token;
  }
  again.store.dispose();
  out.principals.push(entry);
}
out.requests = log.map(({t0, ...r}) => r);
writeFileSync(outPath, JSON.stringify(out));
process.exit(0);
