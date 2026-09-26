#!/usr/bin/env node
// Checks, against a running deployment, which label description each principal is served and
// that a label does not outlive the cluster it labels.
//
//   TESSERA_SESSION_CRED=… TESSERA_OPERATOR_CRED=… node clients/ts/scripts/check-labels.mjs \
//     --presets ../../tessera-demo/presets/stage3.json --clusters centroids/kmeans-2026-08 \
//     --labels topics/ctfidf-2026-08 --term 46
//
// Publish the two layers with `publish-clusters.mjs --labels … --label-term …` first. The script
// exits non-zero if a claim fails:
//
//  1. A viewer is served the first variation whose generating set they can see entirely, whatever
//     fraction of the corpus they see. Principals holding the label term are served the per-term
//     description; a principal who sees more of the corpus through other terms is served no label.
//  2. Every served label carries its text; a viewer who contains no variation gets no artifact.
//  3. Suppressing a cluster stops its label being served, on the identifier route too.
import {readFile} from 'node:fs/promises';
import {tableFromIPC} from 'apache-arrow';
// Loading a `.ts` module needs Node 22.18 or later, which strips its types.
import {Control} from '../core/src/control.ts';
import {accepted} from './operator.ts';

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
const clusterLayer = args.clusters ?? 'centroids/kmeans-2026-08';
const labelLayer = args.labels ?? 'topics/ctfidf-2026-08';
const labelTerm = args.term ?? '46';
const presets = JSON.parse(await readFile(args.presets ?? '../../tessera-demo/presets/stage3.json', 'utf8'));

/** This deployment's `/v1/meta`. */
async function metaOf(token) {
  const r = await fetch(`${viewer}/v1/meta`, {headers: {authorization: `Bearer ${token}`}});
  if (!r.ok) throw new Error(`meta: ${r.status} ${await r.text()}`);
  return r.json();
}
const viewOf = async (token) => (await metaOf(token)).views[0].id;

async function authorise(terms) {
  const r = await fetch(`${session}/session/authorise`, {
    method: 'POST',
    headers: {authorization: `Bearer ${sessionCred}`, 'content-type': 'application/json'},
    body: JSON.stringify({auth_data: Buffer.from(JSON.stringify({terms})).toString('base64')})
  });
  if (!r.ok) throw new Error(`authorise: ${r.status} ${await r.text()}`);
  return (await r.json()).token;
}

function frames(buf) {
  const frame = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
  const out = [];
  let at = 0;
  while (at < buf.byteLength) {
    const kind = frame.getUint8(at);
    const length = frame.getUint32(at + 1, true);
    out.push({kind, payload: buf.subarray(at + 5, at + 5 + length)});
    at += 5 + length;
  }
  return out;
}

/** Every artifact this principal is served for these layers, as `(layer, key) → row`. */
async function artifacts(token, layers) {
  const meta = await metaOf(token);
  const q = meta.views[0].quantisation;
  const r = await fetch(`${viewer}/v1/viewport`, {
    method: 'POST',
    headers: {authorization: `Bearer ${token}`, 'content-type': 'application/json'},
    // `k = 0` asks for no points, so nothing depends on which documents were sampled.
    body: JSON.stringify({view: meta.views[0].id, zoom: 0, bbox: [q.x_min, q.y_min, q.x_max, q.y_max], k: 0, layers})
  });
  if (!r.ok) throw new Error(`viewport: ${r.status} ${await r.text()}`);
  const frame = frames(new Uint8Array(Buffer.from(await r.arrayBuffer()))).find((f) => f.kind === 5);
  const out = new Map();
  if (!frame) return out;
  const table = tableFromIPC(frame.payload);
  const layerCol = table.getChild('layer');
  const keys = table.getChild('key');
  const ids = table.getChild('tessera_id').toArray();
  const masked = table.getChild('masked_count').toArray();
  const content = table.getChild('content');
  for (let i = 0; i < ids.length; i++) {
    const values = content ? [...(content.get(i) ?? [])].map(String) : [];
    out.set(`${layerCol.get(i)}::${keys.get(i)}`, {
      layer: String(layerCol.get(i)),
      key: String(keys.get(i)),
      id: ids[i],
      masked: Number(masked[i]),
      text: values[0] ?? null
    });
  }
  return out;
}

/** Whether the identifier route still answers for this artifact. */
async function byIdentifier(token, id) {
  const r = await fetch(`${viewer}/v1/artifacts/${id}`, {
    method: 'POST',
    headers: {authorization: `Bearer ${token}`, 'content-type': 'application/json'},
    body: JSON.stringify({view: await viewOf(token)})
  });
  if (r.status === 404) return null;
  if (!r.ok) throw new Error(`drill-down: ${r.status} ${await r.text()}`);
  return r.json();
}

const failures = [];
const expect = (claim, held) => {
  if (!held) failures.push(claim);
};

// What each principal is served.

const rows = [];
const served = new Map();
/**
 * One label followed across every principal, so the table shows what viewers served the same
 * description are each told. The count is each viewer's masked count of the label's members.
 */
const WITNESS = args.witness ?? 'l-c-0001';
for (const preset of presets) {
  const token = await authorise(preset.terms);
  const all = await artifacts(token, [clusterLayer, labelLayer]);
  served.set(preset.label, {token, all});
  const clusters = [...all.values()].filter((a) => a.layer === clusterLayer);
  const labels = [...all.values()].filter((a) => a.layer === labelLayer);
  const descriptions = new Set(labels.map((l) => (l.text ?? '').split(' · ')[1] ?? '—'));
  rows.push({
    principal: preset.label,
    'visible items': preset.visible.toLocaleString(),
    'clusters served': clusters.length,
    'labels served': labels.length,
    'description served': labels.length === 0 ? 'none' : [...descriptions].join(', '),
    [`${WITNESS} masked count`]: all.has(`${labelLayer}::${WITNESS}`)
      ? all.get(`${labelLayer}::${WITNESS}`).masked.toLocaleString()
      : 'absent'
  });
  expect(
    `${preset.label}: every served label carries its text`,
    labels.every((l) => l.text)
  );
  // Holding the label term decides, not corpus coverage.
  if (preset.terms.includes(labelTerm)) {
    expect(`${preset.label} holds term ${labelTerm} and is served labels`, labels.length > 0);
  }
}
console.table(rows);

const holders = presets.filter((p) => p.terms.includes(labelTerm)).map((p) => p.label);
const nonHolders = presets.filter((p) => !p.terms.includes(labelTerm) && p.terms.length === 1);
// A principal holding only the label term is served the per-term description. Checked for the
// single-term holders, since any holder accepting either description would always pass.
expect(
  `a principal whose only term is ${labelTerm} is served the per-term description`,
  presets
    .filter((p) => p.terms.length === 1 && p.terms.includes(labelTerm))
    .every((p) =>
      [...served.get(p.label).all.values()]
        .filter((a) => a.layer === labelLayer)
        .every((l) => l.text.endsWith(`term ${labelTerm}`))
    )
);
// The whole-cluster description reaches someone, so the first variation is exercised.
expect(
  'some principal contains the whole cluster and is served the description generated from it',
  holders.some((label) =>
    [...served.get(label).all.values()].some(
      (a) => a.layer === labelLayer && a.text.endsWith('whole cluster')
    )
  )
);
expect(
  'a single-term principal without the label term is served no label, however much of the corpus it sees',
  nonHolders.every((p) => [...served.get(p.label).all.values()].every((a) => a.layer !== labelLayer))
);

// Whether a label outlives its cluster.

// Run as the principal that sees everything, so only the suppression can explain a change.
const witness = presets[presets.length - 1];
const {token: witnessToken, all} = served.get(witness.label);
const label = [...all.values()].find((a) => a.layer === labelLayer);
if (!label) throw new Error(`${witness.label} is served no label at all — nothing to check`);
const cluster = all.get(`${clusterLayer}::${label.key.replace(/^l-/, '')}`);
if (!cluster) throw new Error(`no cluster ${label.key.replace(/^l-/, '')} served to ${witness.label}`);

expect('the label answers on its identifier before the suppression', (await byIdentifier(witnessToken, label.id)) !== null);

const change = async (op) => {
  accepted(op, await control.changes([{tessera_id: cluster.id.toString(), op}]));
};

// The unsuppress runs in `finally`, so a throw does not leave the cluster hidden.
await change('suppress');
try {
  const afterSuppression = await artifacts(witnessToken, [clusterLayer, labelLayer]);
  expect('the suppressed cluster is gone from the viewport', !afterSuppression.has(`${clusterLayer}::${cluster.key}`));
  expect('its label is gone with it', !afterSuppression.has(`${labelLayer}::${label.key}`));
  expect(
    'and the identifier route — which traverses no edge — agrees',
    (await byIdentifier(witnessToken, label.id)) === null
  );
} finally {
  await change('unsuppress');
}
const restored = await artifacts(witnessToken, [clusterLayer, labelLayer]);
expect('lifting the suppression restores both, the label never having been touched itself', restored.has(`${labelLayer}::${label.key}`));

console.log(`\nsuppression checked on ${cluster.key} and its label ${label.key}, as ${witness.label}`);
if (failures.length > 0) {
  console.error(`\n${failures.length} claim(s) FAILED:`);
  for (const f of failures) console.error(`  - ${f}`);
  process.exit(1);
}
console.log(`${rows.length} principals checked; every claim holds`);
