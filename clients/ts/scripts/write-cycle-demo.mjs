#!/usr/bin/env node
// A write cycle on a running deployment: what happens to a label when a document it was written
// from is deleted.
//
//   TESSERA_SESSION_CRED=… TESSERA_OPERATOR_CRED=… node clients/ts/scripts/write-cycle-demo.mjs
//
// A client cannot see a label's generating set, so the script publishes its own small cluster and
// one label over documents it chose, then deletes one of them. The steps:
//
// 1. The label is served to a principal who can see every document it was generated from.
// 2. One of those documents is deleted with one `POST /control/changes`. The label is gone at the
//    acknowledgement, since containment is evaluated per request against masks the delete has
//    already changed. The cluster stays, one member fewer.
// 3. A fold runs. A viewer should see no change.
// 4. The label is still gone after the fold, and the cluster's count is one less.
//
// The script registers two layers (their names are minted per run and cannot be reused), deletes
// one document irreversibly, and runs a fold, which writes a new prefix and reclaims the old one,
// so it needs free disc of about the bundle's size. `--dry-run` stops before the first write.
import {Buffer} from 'node:buffer';
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

const dryRun = 'dry-run' in args;

// The same request and decoding shapes as `check-labels.mjs`.

async function authorise(terms) {
  const r = await fetch(`${session}/session/authorise`, {
    method: 'POST',
    headers: {authorization: `Bearer ${sessionCred}`, 'content-type': 'application/json'},
    body: JSON.stringify({auth_data: Buffer.from(JSON.stringify({terms})).toString('base64')})
  });
  if (!r.ok) throw new Error(`authorise: ${r.status} ${await r.text()}`);
  return (await r.json()).token;
}

async function metaOf(token) {
  const r = await fetch(`${viewer}/v1/meta`, {headers: {authorization: `Bearer ${token}`}});
  if (!r.ok) throw new Error(`meta: ${r.status} ${await r.text()}`);
  return r.json();
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

async function viewport(token, body) {
  const meta = await metaOf(token);
  const q = meta.views[0].quantisation;
  const r = await fetch(`${viewer}/v1/viewport`, {
    method: 'POST',
    headers: {authorization: `Bearer ${token}`, 'content-type': 'application/json'},
    body: JSON.stringify({
      view: meta.views[0].id,
      zoom: 0,
      bbox: [q.x_min, q.y_min, q.x_max, q.y_max],
      ...body
    })
  });
  if (!r.ok) throw new Error(`viewport: ${r.status} ${await r.text()}`);
  return frames(new Uint8Array(Buffer.from(await r.arrayBuffer())));
}

/** Every artifact this principal is served for these layers, as `layer::key → row`. */
async function artifacts(token, layers) {
  // `k = 0` asks for no points, so nothing depends on which documents were sampled.
  const frame = (await viewport(token, {k: 0, layers})).find((f) => f.kind === 5);
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

/** `k` served point identifiers, which the operator API accepts. */
async function points(token, k) {
  // Kind 3 is the points frame and kind 5 the artifacts frame, each a complete Arrow stream.
  const frame = (await viewport(token, {k})).find((f) => f.kind === 3);
  if (!frame) throw new Error('the viewport served no points frame');
  return [...tableFromIPC(frame.payload).getChild('tessera_id').toArray()];
}

async function register(declaration) {
  return accepted(`register ${declaration.name}`, await control.declareLayer(declaration));
}

async function publish(layer, artifactsToPublish) {
  const idset = (await metaOf(await authorise(['0']))).idset;
  return accepted(`publish into ${layer}`, await control.publish(layer, {level: 0, addressing: 'tessera', idset, artifacts: artifactsToPublish}));
}

async function status() {
  return accepted('status', await control.status());
}

const line = (label, value) => console.log(`  ${String(label).padEnd(34)} ${value}`);
const describe = (row) => (row ? `${row.masked}${row.text ? ` — "${row.text}"` : ''}` : 'absent');

// The run.

const stamp = Date.now().toString(36);
const clusterLayer = args.layer ?? `clusters/write-cycle-${stamp}`;
const labelLayer = args.labels ?? `topics/write-cycle-${stamp}`;

const token = await authorise(['0']);
const meta = await metaOf(token);
const keysOf = (rows, layer) =>
  new Set([...rows.values()].filter((r) => r.layer === layer).map((r) => r.key));

// Publish a cluster and one label over documents chosen here.
const sample = await points(token, 12);
if (sample.length < 6) throw new Error(`the viewport served ${sample.length} points; this needs six`);
const members = sample.slice(0, 6);
const sources = members.slice(0, 3);

console.log('\n0. publish a cluster and a label over documents of our own choosing');
line('cluster layer', clusterLayer);
line('label layer', labelLayer);
line('members', members.length);
line('the label’s generating set', `${sources.length} of them`);
if (dryRun) {
  console.log('\n(dry run — nothing was sent)');
  process.exit(0);
}

// Once a document the label was generated from is deleted, the fold withdraws the label's
// content; step 4 checks it stays withdrawn.
await register(
  clusterLayerDeclaration({
    name: clusterLayer,
    title: 'write-cycle demo clusters',
    view: meta.views[0].id,
    visibility: null,
    minVisible: null,
    computed: ['centroid']
  })
);
await register(labelLayerDeclaration({name: labelLayer, title: 'write-cycle demo labels', view: meta.views[0].id, clusters: clusterLayer}));

await publish(clusterLayer, [{key: 'c0', members: members.map(String)}]);
await publish(labelLayer, [
  {
    key: 'l-c0',
    members: members.map(String),
    content: [{values: ['written from three documents'], generated_from: sources.map(String)}],
    attached_to: {layer: clusterLayer, level: 0, key: 'c0'}
  }
]);

console.log('\n1. what a viewer is served');
const before = await artifacts(token, [clusterLayer, labelLayer]);
line('cluster', describe(before.get(`${clusterLayer}::c0`)));
line('label', describe(before.get(`${labelLayer}::l-c0`)));
if (!before.has(`${labelLayer}::l-c0`)) {
  throw new Error('the label was not served before the deletion, so nothing below means anything');
}

console.log('\n2. delete one of the three documents the label was written from');
line('tessera_id', sources[0]);
accepted('delete', await control.changes([{tessera_id: sources[0].toString(), idset: meta.idset, op: 'delete'}]));

const afterDelete = await artifacts(token, [clusterLayer, labelLayer]);
line('cluster', describe(afterDelete.get(`${clusterLayer}::c0`)));
line('label', describe(afterDelete.get(`${labelLayer}::l-c0`)));

console.log('\n3. run a fold — which a node holding artifacts used to refuse outright');
const statusBefore = await status();
accepted('compact', await control.compact());
const folds = (st) => st.compaction?.folds ?? st.folds ?? 0;
const failures = (st) => st.compaction?.fold_failures ?? st.fold_failures ?? 0;
const deadline = Date.now() + 30 * 60_000;
for (;;) {
  const now = await status();
  if (folds(now) > folds(statusBefore)) {
    line('folds', folds(now));
    break;
  }
  if (failures(now) > failures(statusBefore)) {
    throw new Error('the fold was discarded — the server log names the gate that refused it');
  }
  if (Date.now() > deadline) throw new Error('the fold did not publish within thirty minutes');
  await new Promise((resolve) => setTimeout(resolve, 1000));
}

console.log('\n4. and afterwards');
const afterFold = await artifacts(token, [clusterLayer, labelLayer]);
line('cluster', describe(afterFold.get(`${clusterLayer}::c0`)));
line('label', describe(afterFold.get(`${labelLayer}::l-c0`)));

const labelGone = !afterFold.has(`${labelLayer}::l-c0`);
const clusterCount = afterFold.get(`${clusterLayer}::c0`)?.masked ?? null;
const countFell = clusterCount === members.length - 1;
console.log(
  `\n${labelGone && countFell ? 'PASS' : 'FAIL'}: the label ${
    labelGone ? 'stayed gone across the fold' : 'CAME BACK at the fold'
  }, and the cluster is ${clusterCount ?? 'absent'} of ${members.length}`
);
if (!labelGone || !countFell) process.exitCode = 1;
console.log(
  'The fold wrote what it degraded to reports/fold-<prefix>.json in the bundle root — the notice\n' +
    'the publisher is owed, written before anything retired.'
);
