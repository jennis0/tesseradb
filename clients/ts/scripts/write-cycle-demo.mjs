#!/usr/bin/env node
// **Stage 4's write cycle, on a running deployment, in the order an operator would live it.**
//
//   TESSERA_SESSION_CRED=… TESSERA_OPERATOR_CRED=… node clients/ts/scripts/write-cycle-demo.mjs
//
// `publish-clusters.mjs` gives the viewer something to draw and `check-labels.mjs` proves *who is
// served which description*. This one proves what happens when the corpus underneath a description
// changes, which is the whole of Stage 4.
//
// ## Why it publishes its own pair rather than driving an existing one
//
// A generating set never crosses the trust boundary, so a client cannot tell which documents a
// label was written about — that is the design working. Driving somebody else's labels therefore
// means deleting documents at random and hoping one lands in a generating set, which on a 2.4M
// corpus means deleting a great many documents to demonstrate one thing. So this publishes a small
// cluster and one label over documents it picked itself, and then deletes **one** of them. Nothing
// is guessed and exactly one document dies.
//
// ## What it demonstrates, in four steps
//
// 1. **The label is served** to a principal who can see every document it was generated from.
// 2. **One of those documents is deleted.** One `POST /control/changes`, and the label is gone at
//    the ack — nothing was stored to make that happen, because containment is evaluated per
//    request and the deleted document left every mask the moment the deny was accepted. The
//    cluster stays, one member lighter: a deletion moves a count *and* withdraws a description,
//    and those are two consequences of one event rather than one.
// 3. **A fold runs** — the operation a node holding artifacts used to refuse outright, and the one
//    whose whole job here is to make step 2 structural.
// 4. **The label is still gone.** That is the assertion the stage exists for: an earlier draft had
//    the fold re-base generating sets into row space, which brought the withheld label back —
//    served on a set that no longer named what its text was derived from.
//
// ## What to watch in the viewer while this runs
//
// At step 2 the label stops being served and its cluster stays; at step 3 nothing visibly happens,
// which is the point — a fold is maintenance, and a viewer should not be able to tell one ran.
//
// ## What it does to the deployment
//
// It registers two layers (names are minted per run and cannot be reused), **deletes one document**
// irreversibly — a delete is not a suppress — and **runs a fold**, which writes a new prefix and
// reclaims the one it replaces, so it needs free disc of about the bundle's own size. `--dry-run`
// stops before the first write.
import {Buffer} from 'node:buffer';
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

const dryRun = 'dry-run' in args;

// ------------------------------------------------------------------------------- the plumbing
// Deliberately the same shapes `check-labels.mjs` uses — a second decoding of the same frames is a
// second place for the two to disagree about what the server said.

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
  const view = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
  const out = [];
  let at = 0;
  while (at < buf.byteLength) {
    const kind = view.getUint8(at);
    const length = view.getUint32(at + 1, true);
    out.push({kind, payload: buf.subarray(at + 5, at + 5 + length)});
    at += 5 + length;
  }
  return out;
}

async function viewport(token, body) {
  const meta = await metaOf(token);
  const q = meta.quantisation;
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
  // `k = 0` is the annotation channel's own request shape: no points come back, so nothing here
  // depends on which documents the sampler happened to draw.
  const frame = (await viewport(token, {k: 0, layers})).find((f) => f.kind === 5);
  const out = new Map();
  if (!frame) return out;
  const table = tableFromIPC(frame.payload);
  const layerCol = table.getChild('layer');
  const keys = table.getChild('stable_key');
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

/** `k` served point identifiers — the only route to something the operator plane will act on. */
async function points(token, k) {
  // Kind 3 is the points frame (`tessera-wire`'s `FRAME_POINTS`); kind 5 is the artifacts one.
  // Each is a complete Arrow stream of its own.
  const frame = (await viewport(token, {k})).find((f) => f.kind === 3);
  if (!frame) throw new Error('the viewport served no points frame');
  return [...tableFromIPC(frame.payload).getChild('tessera_id').toArray()];
}

async function register(declaration) {
  const r = await fetch(`${control}/control/layers`, {
    method: 'PUT',
    headers: {authorization: `Bearer ${operatorCred}`, 'content-type': 'application/json'},
    body: JSON.stringify(declaration)
  });
  if (!r.ok) throw new Error(`register ${declaration.name}: ${r.status} ${await r.text()}`);
  return r.json();
}

async function publish(layer, artifactsToPublish) {
  const r = await fetch(`${control}/control/layers/${encodeURIComponent(layer)}/artifacts`, {
    method: 'PUT',
    headers: {authorization: `Bearer ${operatorCred}`, 'content-type': 'application/json'},
    body: JSON.stringify({
      level: 0,
      addressing: 'tessera',
      idset: (await metaOf(await authorise(['0']))).idset,
      artifacts: artifactsToPublish
    })
  });
  if (!r.ok) throw new Error(`publish into ${layer}: ${r.status} ${await r.text()}`);
  return r.json();
}

async function status() {
  const r = await fetch(`${control}/control/status`, {
    headers: {authorization: `Bearer ${operatorCred}`}
  });
  if (!r.ok) throw new Error(`status: ${r.status} ${await r.text()}`);
  return r.json();
}

const line = (label, value) => console.log(`  ${String(label).padEnd(34)} ${value}`);
const describe = (row) => (row ? `${row.masked}${row.text ? ` — "${row.text}"` : ''}` : 'absent');

// ------------------------------------------------------------------------------------- the run

const stamp = Date.now().toString(36);
const clusterLayer = args.layer ?? `clusters/write-cycle-${stamp}`;
const labelLayer = args.labels ?? `topics/write-cycle-${stamp}`;

const token = await authorise(['0']);
const meta = await metaOf(token);
const keysOf = (rows, layer) =>
  new Set([...rows.values()].filter((r) => r.layer === layer).map((r) => r.key));

// ---- publish a cluster and one label over documents we picked ------------------------------
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

await register({
  name: clusterLayer,
  title: 'write-cycle demo clusters',
  views: [meta.views[0].id],
  membership: 'enumerated',
  content: {derived: ['centroid'], supplied: [], on_member_deletion: 'withdraw_content'},
  access: {label: null, artifacts_carry_own: false},
  visible_when: null,
  hierarchy: {kind: 'flat', prune_children: false}
});
await register({
  name: labelLayer,
  title: 'write-cycle demo labels',
  views: [meta.views[0].id],
  membership: 'enumerated',
  content: {
    derived: [],
    supplied: [{kind: 'label_text', corpus_derived: true}],
    // **The declaration under test.** Strict: a description whose source is deleted is withdrawn at
    // the fold rather than re-based onto the survivors. `shrink_generating_set` is the other choice
    // and would put the label back at step 4 — deliberately, and only because the publisher said so.
    on_member_deletion: 'withdraw_content'
  },
  access: {label: null, artifacts_carry_own: false},
  visible_when: null,
  hierarchy: {kind: 'flat', prune_children: false},
  depends_on: [clusterLayer]
});

await publish(clusterLayer, [{stable_key: 'c0', members: members.map(String)}]);
await publish(labelLayer, [
  {
    stable_key: 'l-c0',
    members: members.map(String),
    content: [{values: ['written from three documents'], generated_from: sources.map(String)}],
    attached_to: {layer: clusterLayer, level: 0, stable_key: 'c0'}
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
const deleted = await fetch(`${control}/control/changes`, {
  method: 'POST',
  headers: {authorization: `Bearer ${operatorCred}`, 'content-type': 'application/json'},
  body: JSON.stringify([{tessera_id: sources[0].toString(), idset: meta.idset, op: 'delete'}])
});
if (!deleted.ok) throw new Error(`delete: ${deleted.status} ${await deleted.text()}`);

const afterDelete = await artifacts(token, [clusterLayer, labelLayer]);
line('cluster', describe(afterDelete.get(`${clusterLayer}::c0`)));
line('label', describe(afterDelete.get(`${labelLayer}::l-c0`)));

console.log('\n3. run a fold — which a node holding artifacts used to refuse outright');
const statusBefore = await status();
const compacted = await fetch(`${control}/control/compact`, {
  method: 'POST',
  headers: {authorization: `Bearer ${operatorCred}`}
});
if (!compacted.ok) throw new Error(`compact: ${compacted.status} ${await compacted.text()}`);
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
