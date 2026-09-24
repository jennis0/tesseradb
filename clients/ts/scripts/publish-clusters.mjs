#!/usr/bin/env node
// Register an annotation layer and publish a synthetic clustering into it, so the viewer has
// something to draw.
//
//   TESSERA_SESSION_CRED=… TESSERA_OPERATOR_CRED=… node clients/ts/scripts/publish-clusters.mjs \
//     --presets tessera-demo/presets/2m4.json --clusters 24 [--min-visible 400]
//
// The clustering is k-means over a sample of the corpus's points, to show masking: two principals
// get different counts for the same cluster, and under `--min-visible` a cluster the broad one sees
// is absent for the narrow one.
//
// Members are published by `tessera_id` with the view's idset, since the ids come back from the
// server in the same response as the positions clustered on, and external ids would need the
// source corpus.
//
// The viewer draws a cluster from the geometry the server derives per principal (`centroid`,
// `box`, `hull`) and from the per-point membership column. The centroids computed here, over the
// publisher's view, are used only for the clustering and are not published. The declared
// membership sizes are printed for the operator choosing a criterion and published nowhere, since
// they count items a viewer may not see.
import {readFile} from 'node:fs/promises';
import {tableFromIPC} from 'apache-arrow';
// Loading a `.ts` module needs Node 22.18 or later, which strips its types.
import {Control} from '../core/src/control.ts';
import {accepted, clusterLayerDeclaration, labelLayerDeclaration} from './operator.ts';

const args = Object.fromEntries(
  process.argv
    .slice(2)
    .reduce((acc, a, i, all) => (a.startsWith('--') ? [...acc, [a.slice(2), all[i + 1]]] : acc), [])
);
const viewer = args.viewer ?? 'http://127.0.0.1:37585';
const session = args.session ?? 'http://127.0.0.1:49303';
const sessionCred = process.env.TESSERA_SESSION_CRED;
const operatorCred = process.env.TESSERA_OPERATOR_CRED;
if (!sessionCred) throw new Error('set TESSERA_SESSION_CRED');
if (!operatorCred) throw new Error('set TESSERA_OPERATOR_CRED');
const control = new Control({controlUrl: args.control ?? 'http://127.0.0.1:45721', operatorCredential: operatorCred});

const CLUSTERS = Number(args.clusters ?? 24);
const SAMPLE_DEPTH = Number(args['sample-depth'] ?? 7);
const SAMPLE_K = Number(args['sample-k'] ?? 64);
const ITERATIONS = Number(args.iterations ?? 12);
/** Members per publish request. Each request is one commit and one fsync. */
const BATCH = Number(args.batch ?? 50_000);

/**
 * The layer name. Publication is append-only, a repeated key is refused and a dropped name cannot
 * be reused, so each run publishes under a new name.
 */
const layerName = args.layer ?? `clusters/kmeans-${Date.now().toString(36)}`;
const minVisible = args['min-visible'] === undefined ? null : Number(args['min-visible']);

/**
 * With `--labels <name> --label-term <t>`, a second layer of labels attached to the clusters.
 *
 * Each label carries two ranked variations of one description: one generated from the cluster's
 * whole membership, one from the part a `--label-term` principal can see. A viewer is served the
 * first whose generating set they can see entirely, and no label if neither.
 *
 * Attachment is a visibility condition: suppressing a cluster stops its labels being served on
 * every route, the identifier route included.
 */
const labelLayer = args.labels ?? null;
const labelTerm = args['label-term'] ?? null;
if (labelLayer && !labelTerm) {
  throw new Error('--labels needs --label-term: the per-term variation is generated from what that principal can see');
}

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
 * Split a viewport body into its frames, `u8 kind, u32 LE length, payload`, repeated. An unknown
 * kind throws, as in `core/src/frame.ts`, so a new frame's data is not dropped unnoticed.
 */
function frames(buf) {
  const frame = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
  const out = [];
  let at = 0;
  while (at < buf.byteLength) {
    const kind = frame.getUint8(at);
    const length = frame.getUint32(at + 1, true);
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

/** Gather the even bits of a u32 into the low 16, the inverse of the Morton spread. */
function compact(v) {
  let x = v & 0x55555555;
  x = (x | (x >>> 1)) & 0x33333333;
  x = (x | (x >>> 2)) & 0x0f0f0f0f;
  x = (x | (x >>> 4)) & 0x00ff00ff;
  x = (x | (x >>> 8)) & 0x0000ffff;
  return x >>> 0;
}

const presets = args.presets ? JSON.parse(await readFile(args.presets, 'utf8')) : null;
/**
 * Cluster over the view of a principal broader than any the viewer offers (pass `--ranks`). A
 * narrow publisher would make most members invisible to others, and publishing as the broadest
 * preset would let that preset's masked count equal the declared size. Above the top preset, every
 * principal the viewer offers sees fewer members than the cluster holds.
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
const view = meta.views[0].id;
const q = meta.views[0].quantisation;

console.log(`sampling points at depth ${SAMPLE_DEPTH}, k=${SAMPLE_K}, as the publishing principal`);
const sampled = await viewport(token, {
  view,
  zoom: SAMPLE_DEPTH,
  bbox: [q.x_min, q.y_min, q.x_max, q.y_max],
  k: SAMPLE_K,
  // `[]` asks for no layers; omitting the field asks for every layer the principal reaches.
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

// Lloyd's algorithm from a deterministic stride-spread seeding, so a re-run against the same
// sample publishes the same clusters.
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
  // An empty cluster is not published: its masked count is zero for everyone.
  if (members[c].length === 0) continue;
  clusters.push({
    key: `c-${String(c).padStart(4, '0')}`,
    // The centroid in cell space, `[0, 65536)` per axis. Not published.
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

// The layer's own access label (`--label`), or public, which each artifact inherits.
const declaration = clusterLayerDeclaration({
  name: layerName,
  title: args.title ?? `k-means over ${ids.length.toLocaleString()} sampled points`,
  view,
  visibility: args.label ?? null,
  minVisible,
  computed: ['centroid', 'box', 'hull']
});

const layer = accepted('register', await control.declareLayer(declaration));
console.log(`registered ${layer.name} (tessera_id ${layer.tessera_id}), the address to suppress it by`);

let batch = [];
let batchMembers = 0;
let published = 0;
const publish = async () => {
  if (batch.length === 0) return;
  const answer = await control.publish(layerName, {
    level: 0,
    // Addressing is per request; a per-member tag would be most of the body.
    addressing: 'tessera',
    idset: meta.idset,
    artifacts: batch.map((c) => ({
      key: c.key,
      members: c.members.map((id) => id.toString())
    }))
  });
  published += accepted('publish', answer).artifacts.length;
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

// One label per cluster. The per-term variation is generated from the `--label-term` principal's
// own sample intersected with the cluster. Visibility is a disjunction over terms, so every viewer
// holding that term sees the set entirely, and a viewer holding only other terms generally does not.

if (labelLayer) {
  console.log(`sampling as the term-${labelTerm} principal, for the per-term variation`);
  const termToken = await authorise([labelTerm]);
  const termSample = await viewport(termToken, {
    view,
    zoom: SAMPLE_DEPTH,
    bbox: [q.x_min, q.y_min, q.x_max, q.y_max],
    k: SAMPLE_K,
    layers: []
  });
  const termVisible = new Set();
  for (const frame of termSample.filter((f) => f.kind === 3)) {
    for (const id of tableFromIPC(frame.payload).getChild('tessera_id').toArray()) termVisible.add(id);
  }
  console.log(`term ${labelTerm} sees ${termVisible.size.toLocaleString()} of the sampled points`);

  const labelDeclaration = labelLayerDeclaration({
    name: labelLayer,
    title: args['labels-title'] ?? `toponymy over ${layerName}`,
    view,
    clusters: layerName
  });
  const labelsRegistered = accepted('register labels', await control.declareLayer(labelDeclaration));
  console.log(`registered ${labelLayer} (tessera_id ${labelsRegistered.tessera_id})`);

  const labels = [];
  for (const cluster of clusters) {
    const seen = cluster.members.filter((id) => termVisible.has(id));
    // A cluster with no `--label-term` document gets no label: the server refuses an empty
    // generating set for corpus-derived content.
    if (seen.length === 0) continue;
    labels.push({
      key: `l-${cluster.key}`,
      cluster: cluster.key,
      members: cluster.members,
      variations: [
        {values: [`${cluster.key} · whole cluster`], generated_from: cluster.members},
        {values: [`${cluster.key} · term ${labelTerm}`], generated_from: seen}
      ]
    });
  }
  console.log(`${labels.length} labels of ${clusters.length} clusters carry a per-term variation`);

  let pending = [];
  let pendingIds = 0;
  const publishLabels = async () => {
    if (pending.length === 0) return;
    const answer = await control.publish(labelLayer, {
      level: 0,
      addressing: 'tessera',
      idset: meta.idset,
      artifacts: pending.map((l) => ({
        key: l.key,
        members: l.members.map((id) => id.toString()),
        content: l.variations.map((v) => ({
          values: v.values,
          generated_from: v.generated_from.map((id) => id.toString())
        })),
        // The target is named by its key.
        attached_to: {layer: layerName, level: 0, key: l.cluster}
      }))
    });
    accepted('publish labels', answer);
    pending = [];
    pendingIds = 0;
  };
  for (const label of labels) {
    const size = label.members.length + label.variations.reduce((n, v) => n + v.generated_from.length, 0);
    if (pendingIds + size > BATCH) await publishLabels();
    pending.push(label);
    pendingIds += size;
  }
  await publishLabels();
  console.log(`published ${labels.length} labels into ${labelLayer}`);
}

/**
 * What each principal is served for this layer: the same cluster and identifier, with a masked
 * count that differs per principal. Under a criterion, some clusters are absent for narrower
 * principals, and the response does not say why.
 */
if (presets) {
  const declaredSize = new Map(clusters.map((c) => [c.key, c.members.length]));
  /** One cluster followed across every principal: the largest, which a criterion removes last. */
  const WITNESS = clusters.reduce((a, b) => (a.members.length >= b.members.length ? a : b)).key;
  const rows = [];
  for (const preset of presets) {
    const t = await authorise(preset.terms);
    const served = await viewport(t, {
      view,
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
      const keys = table.getChild('key');
      for (let i = 0; i < masked.length; i++) counts.set(String(keys.get(i)), Number(masked[i]));
    }
    const shown = [...counts.values()].sort((a, b) => a - b);
    // The witness cluster's masked count for this principal, against the declared size.
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
