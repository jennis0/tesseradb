#!/usr/bin/env node
// Register an annotation layer and publish a synthetic clustering into it, so the viewer has
// something to draw.
//
//   TESSERA_SESSION_CRED=… TESSERA_OPERATOR_CRED=… node clients/ts/scripts/publish-clusters.mjs \
//     --presets clients/ts/.dev/presets/2m4.json --clusters 24 [--min-visible 400]
//
// The clustering is k-means over a sample of the corpus's own points, and it is deliberately
// unremarkable: **what this exists to demonstrate is the masking, not the clustering.** Two
// principals get different counts for the same cluster, and under `--min-visible` a cluster the
// broad one sees is simply absent for the narrow one.
//
// ## Why it publishes by `tessera_id` and not by external id
//
// Both are accepted (`addressing`, per request rather than per member). External ids would need
// the source corpus; identifiers come back from the service itself, in the same response that
// carries the positions this clusters on — so the script needs nothing but a running server. The
// idset goes with them, because a `tessera_id` is only meaningful under the identity lineage that
// minted it.
//
// ## The sidecar, and what it must never carry
//
// **There is no artifact geometry on the wire** — deliberately, since a bounding box over full
// membership would disclose a cluster's true extent by panning — so the viewer cannot place a
// cluster on the map by itself. This writes the centroids it computed to `viewer/public/clusters.json`
// and the viewer joins them by stable key. That is publisher-side scaffolding for a development
// demo, not a pattern: real artifact geometry arrives as derived content at Stage 3, gated by the
// containment test.
//
// It carries positions and nothing else. **The declared membership size stays out of it**: that is
// a corpus-wide count over items a viewer may not see, and the whole point of the masked count
// beside a cluster is that it is *not* that number. Sizes are printed here, for the operator
// choosing a criterion, and go no further.
import {readFile, writeFile, mkdir} from 'node:fs/promises';
import {dirname, join} from 'node:path';
import {fileURLToPath} from 'node:url';
import {tableFromIPC} from 'apache-arrow';

const args = Object.fromEntries(
  process.argv
    .slice(2)
    .reduce((acc, a, i, all) => (a.startsWith('--') ? [...acc, [a.slice(2), all[i + 1]]] : acc), [])
);
const viewer = args.viewer ?? 'http://127.0.0.1:37585';
const session = args.session ?? 'http://127.0.0.1:49303';
const control = args.control ?? 'http://127.0.0.1:45721';
const sessionCred = process.env.TESSERA_SESSION_CRED;
const operatorCred = process.env.TESSERA_OPERATOR_CRED;
if (!sessionCred) throw new Error('set TESSERA_SESSION_CRED');
if (!operatorCred) throw new Error('set TESSERA_OPERATOR_CRED');

const here = dirname(fileURLToPath(import.meta.url));
const CLUSTERS = Number(args.clusters ?? 24);
const SAMPLE_DEPTH = Number(args['sample-depth'] ?? 7);
const SAMPLE_K = Number(args['sample-k'] ?? 64);
const ITERATIONS = Number(args.iterations ?? 12);
/** Members per publish request. The batch is the commit unit, and each one is an fsync. */
const BATCH = Number(args.batch ?? 50_000);
const OUT = args.out ?? join(here, '..', 'viewer', 'public', 'clusters.json');

/**
 * The layer name, which is an identity rather than a label: publication is append-only, a repeated
 * stable key is refused, and a dropped name is refused for ever. So a re-run publishes a **new
 * version** rather than editing the old one — which is also the shape a real pipeline has, an edit
 * being a delete plus a re-publish.
 */
const layerName = args.layer ?? `clusters/kmeans-${Date.now().toString(36)}`;
const minVisible = args['min-visible'] === undefined ? null : Number(args['min-visible']);

const SIDECAR_NOTE =
  "Development scaffolding: these positions are the publisher's, not the service's. There is no " +
  'artifact geometry on the wire, and the viewer draws a marker only for an artifact the server ' +
  'actually served. No membership and no declared size: the count beside a cluster is the ' +
  "viewer's own, and it is never the cluster's size.";

// --------------------------------------------------------------------------------- the plumbing

async function authorise(terms) {
  const r = await fetch(`${session}/session/authorise`, {
    method: 'POST',
    headers: {authorization: `Bearer ${sessionCred}`, 'content-type': 'application/json'},
    body: JSON.stringify({auth_data: Buffer.from(JSON.stringify({terms})).toString('base64')})
  });
  if (!r.ok) throw new Error(`authorise: ${r.status} ${await r.text()}`);
  return (await r.json()).token;
}

/**
 * Split a viewport body into its frames — `u8 kind, u32 LE length, payload`, repeated.
 *
 * The same grammar `core/src/frame.ts` carries, in the same posture: an unknown kind throws rather
 * than being skipped, because skipping is how a future frame's data goes silently missing.
 */
function frames(buf) {
  const view = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
  const out = [];
  let at = 0;
  while (at < buf.byteLength) {
    const kind = view.getUint8(at);
    const length = view.getUint32(at + 1, true);
    if (kind < 1 || kind > 5) throw new Error(`unknown frame kind ${kind} at byte ${at}`);
    out.push({kind, payload: buf.subarray(at + 5, at + 5 + length)});
    at += 5 + length;
  }
  return out;
}

async function viewport(token, body) {
  const r = await fetch(`${viewer}/v1/viewport`, {
    method: 'POST',
    headers: {authorization: `Bearer ${token}`, 'content-type': 'application/json'},
    body: JSON.stringify(body)
  });
  if (!r.ok) throw new Error(`viewport: ${r.status} ${await r.text()}`);
  return frames(new Uint8Array(Buffer.from(await r.arrayBuffer())));
}

/** Gather the even bits of a u32 into the low 16 — the inverse of the Morton spread. */
function compact(v) {
  let x = v & 0x55555555;
  x = (x | (x >>> 1)) & 0x33333333;
  x = (x | (x >>> 2)) & 0x0f0f0f0f;
  x = (x | (x >>> 4)) & 0x00ff00ff;
  x = (x | (x >>> 8)) & 0x0000ffff;
  return x >>> 0;
}

// ------------------------------------------------------------------------------- the corpus half

const presets = args.presets ? JSON.parse(await readFile(args.presets, 'utf8')) : null;
/**
 * Cluster over the view of a principal **broader than any the viewer offers**, because a
 * clustering is a claim about the corpus rather than about a viewer.
 *
 * Two things follow from the choice, and the second is why `--ranks` is worth passing. A narrow
 * publisher would produce clusters whose members are mostly invisible to everyone else, so every
 * masked count would be near zero — counts that differ are the demonstration, counts that vanish
 * are not. And a publisher that is exactly the broadest *preset* would let that preset see 100% of
 * every cluster it published, so its masked count would coincide with the declared size — true,
 * unalarming, and the one coincidence that makes "the count is never the size" hard to read off
 * the screen. Publishing above the top preset removes it: every principal the viewer can be sees
 * strictly fewer members than the cluster holds.
 */
const publisherTerms = args.terms
  ? args.terms.split(',')
  : args.ranks
    ? JSON.parse(await readFile(args.ranks, 'utf8'))
        .slice(0, Number(args['ranks-top'] ?? 16384))
        .map((r) => String(r.term))
    : presets
      ? presets.reduce((best, p) => (best && best.visible >= p.visible ? best : p)).terms
      : ['0'];

const token = await authorise(publisherTerms);
const metaResp = await fetch(`${viewer}/v1/meta`, {headers: {authorization: `Bearer ${token}`}});
if (!metaResp.ok) throw new Error(`meta: ${metaResp.status} ${await metaResp.text()}`);
const meta = await metaResp.json();
const slice = meta.slices[0].id;
const q = meta.quantisation;

console.log(`sampling points at depth ${SAMPLE_DEPTH}, k=${SAMPLE_K}, as the publishing principal`);
const sampled = await viewport(token, {
  slice,
  zoom: SAMPLE_DEPTH,
  bbox: [q.x_min, q.y_min, q.x_max, q.y_max],
  k: SAMPLE_K,
  // Nothing is being drawn against a layer here, so the artifact pass costs nothing: `[]` is not
  // the same request as omitting the field, which asks for every layer this principal reaches.
  layers: []
});

const ids = [];
const xs = [];
const ys = [];
for (const frame of sampled.filter((f) => f.kind === 3)) {
  const table = tableFromIPC(frame.payload);
  const id = table.getChild('tessera_id').toArray();
  const code = table.getChild('code').toArray();
  const halves = new Uint32Array(code.buffer, code.byteOffset, code.length * 2);
  for (let i = 0; i < id.length; i++) {
    ids.push(id[i]);
    const lo = halves[i * 2];
    const hi = halves[i * 2 + 1];
    xs.push((compact(lo) + compact(hi) * 65536) / 65536);
    ys.push((compact(lo >>> 1) + compact(hi >>> 1) * 65536) / 65536);
  }
}
if (ids.length < CLUSTERS) {
  throw new Error(`sampled only ${ids.length} points — too few for ${CLUSTERS} clusters`);
}
console.log(`sampled ${ids.length.toLocaleString()} points`);

// ------------------------------------------------------------------------------- the clustering

// Lloyd's algorithm from a deterministic stride-spread seeding. Deterministic on purpose: a re-run
// against the same sample publishes the same clusters, so the numbers in a report are comparable
// across runs, and nothing here needs a seeded RNG to be reproducible.
const cx = new Float64Array(CLUSTERS);
const cy = new Float64Array(CLUSTERS);
for (let c = 0; c < CLUSTERS; c++) {
  const at = Math.floor((c * ids.length) / CLUSTERS);
  cx[c] = xs[at];
  cy[c] = ys[at];
}
const assign = new Int32Array(ids.length);
for (let iter = 0; iter < ITERATIONS; iter++) {
  let moved = 0;
  for (let i = 0; i < ids.length; i++) {
    let best = 0;
    let bestD = Infinity;
    for (let c = 0; c < CLUSTERS; c++) {
      const dx = xs[i] - cx[c];
      const dy = ys[i] - cy[c];
      const d = dx * dx + dy * dy;
      if (d < bestD) {
        bestD = d;
        best = c;
      }
    }
    if (assign[i] !== best) moved++;
    assign[i] = best;
  }
  const sx = new Float64Array(CLUSTERS);
  const sy = new Float64Array(CLUSTERS);
  const n = new Int32Array(CLUSTERS);
  for (let i = 0; i < ids.length; i++) {
    sx[assign[i]] += xs[i];
    sy[assign[i]] += ys[i];
    n[assign[i]]++;
  }
  for (let c = 0; c < CLUSTERS; c++) {
    if (n[c] === 0) continue;
    cx[c] = sx[c] / n[c];
    cy[c] = sy[c] / n[c];
  }
  if (moved === 0) break;
}

const members = Array.from({length: CLUSTERS}, () => []);
for (let i = 0; i < ids.length; i++) members[assign[i]].push(ids[i]);
const clusters = [];
for (let c = 0; c < CLUSTERS; c++) {
  // An empty cluster is dropped rather than published: an artifact with no members has a masked
  // count of zero for everyone, which is a row on every wire that can never be anything but noise.
  if (members[c].length === 0) continue;
  clusters.push({
    stableKey: `c-${String(c).padStart(4, '0')}`,
    // Cell space, `[0, 65536)` per axis — the same units the points frame decodes into, so the
    // viewer places a cluster with the transform it already applies to every mark.
    x: cx[c],
    y: cy[c],
    members: members[c]
  });
}
console.log(
  `${clusters.length} non-empty clusters, sizes ${clusters
    .map((c) => c.members.length)
    .sort((a, b) => a - b)
    .filter((_, i, all) => i === 0 || i === all.length - 1)
    .join('…')}`
);

// -------------------------------------------------------------------------------- the publishing

const declaration = {
  name: layerName,
  title: args.title ?? `k-means over ${ids.length.toLocaleString()} sampled points`,
  slices: [slice],
  membership: 'enumerated',
  // `artifacts_carry_own: true` would serve nothing at this stage — the per-artifact label arrives
  // with content at Stage 3, so a layer declaring it has nothing to satisfy and every artifact is
  // withheld, fail-closed.
  access: {label: args.label ?? null, artifacts_carry_own: false},
  // `null` is a declaration in its own right — *this layer needs no existence criterion* — rather
  // than a field nobody filled in.
  visible_when: minVisible === null ? null : {min_visible: minVisible},
  hierarchy: {kind: 'flat', prune_children: false}
};

const registered = await fetch(`${control}/control/layers`, {
  method: 'PUT',
  headers: {authorization: `Bearer ${operatorCred}`, 'content-type': 'application/json'},
  body: JSON.stringify(declaration)
});
if (!registered.ok) throw new Error(`register: ${registered.status} ${await registered.text()}`);
const layer = await registered.json();
console.log(`registered ${layer.name} (tessera_id ${layer.tessera_id}) — the address to suppress it by`);

// The layer name is path-shaped, so its slash is percent-encoded into the one path segment the
// route captures.
const artifactsUrl = `${control}/control/layers/${encodeURIComponent(layerName)}/artifacts`;
let batch = [];
let batchMembers = 0;
let published = 0;
const publish = async () => {
  if (batch.length === 0) return;
  const r = await fetch(artifactsUrl, {
    method: 'PUT',
    headers: {authorization: `Bearer ${operatorCred}`, 'content-type': 'application/json'},
    body: JSON.stringify({
      level: 0,
      // Per request, not per member: a clustering names its whole corpus, and a per-member tag
      // would be most of the body.
      addressing: 'tessera',
      idset: meta.idset,
      artifacts: batch.map((c) => ({
        stable_key: c.stableKey,
        members: c.members.map((id) => id.toString())
      }))
    })
  });
  if (!r.ok) throw new Error(`publish: ${r.status} ${await r.text()}`);
  published += (await r.json()).artifacts.length;
  batch = [];
  batchMembers = 0;
};
for (const cluster of clusters) {
  if (batchMembers + cluster.members.length > BATCH) await publish();
  batch.push(cluster);
  batchMembers += cluster.members.length;
}
await publish();
console.log(`published ${published} artifacts`);

// Merged rather than overwritten, and keyed by layer: publication is append-only and a name is
// never reused, so a second run is a second *layer* — and the demo's point is comparing two of
// them (one with an existence criterion, one without) over the same clusters.
let sidecar = {note: SIDECAR_NOTE, layers: {}};
try {
  const held = JSON.parse(await readFile(OUT, 'utf8'));
  if (held.layers) sidecar = {note: SIDECAR_NOTE, layers: held.layers};
} catch {
  // No file yet, which is the first run.
}
sidecar.layers[layerName] = clusters.map((c) => ({stableKey: c.stableKey, x: c.x, y: c.y}));
await mkdir(dirname(OUT), {recursive: true});
await writeFile(OUT, `${JSON.stringify(sidecar, null, 2)}\n`);
console.log(`wrote ${OUT}`);

// ----------------------------------------------------------------------------------- the report

/**
 * What each principal is served for this layer, from the service.
 *
 * This is the stage's own check, run from outside the viewer: the same cluster, the same
 * identifier, and a count that differs per principal — none of them equal to the size printed
 * above. Under a criterion, some clusters are simply **absent** for the narrower principals, with
 * nothing in the response saying why.
 */
if (presets) {
  const declaredSize = new Map(clusters.map((c) => [c.stableKey, c.members.length]));
  /** One cluster followed across every principal — the largest, so it survives a criterion longest. */
  const WITNESS = clusters.reduce((a, b) => (a.members.length >= b.members.length ? a : b)).stableKey;
  const rows = [];
  for (const preset of presets) {
    const t = await authorise(preset.terms);
    const served = await viewport(t, {
      slice,
      zoom: 3,
      bbox: [q.x_min, q.y_min, q.x_max, q.y_max],
      k: 1,
      layers: [layerName]
    });
    const frame = served.find((f) => f.kind === 5);
    const counts = new Map();
    if (frame) {
      const table = tableFromIPC(frame.payload);
      const masked = table.getChild('masked_count').toArray();
      const keys = table.getChild('stable_key');
      for (let i = 0; i < masked.length; i++) counts.set(String(keys.get(i)), Number(masked[i]));
    }
    const shown = [...counts.values()].sort((a, b) => a - b);
    // The comparison the stage is judged on, per principal: what the same cluster is worth to each
    // of them, against a size none of them is told.
    const witness = counts.get(WITNESS) ?? null;
    rows.push({
      principal: preset.label,
      'visible items': preset.visible.toLocaleString(),
      'clusters served': `${shown.length} of ${clusters.length}`,
      [`${WITNESS} masked count`]:
        witness === null ? 'absent' : `${witness.toLocaleString()} of ${declaredSize.get(WITNESS).toLocaleString()}`,
      'smallest served': shown.length > 0 ? shown[0].toLocaleString() : '—',
      'largest served': shown.length > 0 ? shown[shown.length - 1].toLocaleString() : '—'
    });
  }
  console.table(rows);
  console.log(
    `declared sizes (publisher-side, never served): ${[...declaredSize.values()]
      .reduce((a, b) => a + b, 0)
      .toLocaleString()} members over ${clusters.length} clusters, ` +
      `smallest ${Math.min(...declaredSize.values()).toLocaleString()}, ` +
      `largest ${Math.max(...declaredSize.values()).toLocaleString()}`
  );
}
