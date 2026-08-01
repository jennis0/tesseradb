#!/usr/bin/env node
// Measure each candidate term's exact visible-set size, and emit viewer/presets.json.
//
// The size comes from the service itself: a zoom=0, full-extent viewport call returns `visible`
// for the single root tile, which IS that principal's visible-set cardinality. Nothing here is
// estimated, and nothing is derived from a drawn sample.
//
//   TESSERA_SESSION_CRED=… node clients/ts/scripts/measure-principals.mjs \
//     --viewer http://127.0.0.1:37585 --session http://127.0.0.1:49303 --terms 0..200
//
// Re-run it per fixture: the term dictionary differs between bundles, so presets measured against
// 2m4 are meaningless against 1e8.
//
// Note it decodes only the TILE stream, which is the one carrying an explicit length prefix at
// byte 0 — so this script needs none of core's message-walking, and stays plain JS.
import {writeFile} from 'node:fs/promises';
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
const cred = process.env.TESSERA_SESSION_CRED;
if (!cred) throw new Error('set TESSERA_SESSION_CRED');

const spec = args.terms ?? '0..200';
const candidates = spec.includes('..')
  ? (() => {
      const [lo, hi] = spec.split('..').map(Number);
      return Array.from({length: hi - lo + 1}, (_, i) => String(lo + i));
    })()
  : spec.split(',');

async function authorise(terms) {
  const r = await fetch(`${session}/session/authorise`, {
    method: 'POST',
    headers: {authorization: `Bearer ${cred}`, 'content-type': 'application/json'},
    body: JSON.stringify({auth_data: Buffer.from(JSON.stringify({terms})).toString('base64')})
  });
  if (!r.ok) throw new Error(`authorise ${terms}: ${r.status} ${await r.text()}`);
  return (await r.json()).token;
}

const probeToken = await authorise([candidates[0]]);
const metaResp = await fetch(`${viewer}/v1/meta`, {
  headers: {authorization: `Bearer ${probeToken}`}
});
if (!metaResp.ok) throw new Error(`meta: ${metaResp.status} ${await metaResp.text()}`);
const meta = await metaResp.json();
const q = meta.quantisation;
const slice = meta.slices[0].id;

/** The `visible` total from a zoom-0, full-extent call: this principal's visible-set size. */
async function visibleFor(terms) {
  const token = await authorise(terms);
  const r = await fetch(`${viewer}/v1/viewport`, {
    method: 'POST',
    headers: {authorization: `Bearer ${token}`, 'content-type': 'application/json'},
    // k = 1 because we want the counts, not the marks: the tile batch carries exact masked
    // figures regardless of how many points the response gathers.
    body: JSON.stringify({slice, zoom: 0, bbox: [q.x_min, q.y_min, q.x_max, q.y_max], k: 1})
  });
  if (!r.ok) throw new Error(`viewport ${terms}: ${r.status} ${await r.text()}`);
  const buf = new Uint8Array(Buffer.from(await r.arrayBuffer()));
  const tileLength = new DataView(buf.buffer, buf.byteOffset, buf.byteLength).getUint32(0, true);
  const tiles = tableFromIPC(buf.subarray(4, 4 + tileLength));
  const visible = tiles.getChild('visible').toArray();
  return [...visible].reduce((a, b) => a + Number(b), 0);
}

const measured = [];
for (const term of candidates) {
  try {
    const visible = await visibleFor([term]);
    if (visible > 0) measured.push({term, visible});
  } catch (e) {
    console.error(`skipping ${term}: ${e.message}`);
  }
}
measured.sort((a, b) => a.visible - b.visible);
if (measured.length === 0) throw new Error('no candidate term is visible to anyone');

const at = (fraction) =>
  measured[Math.min(measured.length - 1, Math.floor(fraction * measured.length))];
const narrow = at(0.05);
const medium = at(0.5);
const broad = measured[measured.length - 1];

// Distinct terms only: at a small dictionary the quantiles can collide, and three identical
// presets would make "switch principal and watch the map change" untestable while looking fine.
const chosen = [];
for (const [label, m] of [
  ['narrow', narrow],
  ['medium', medium],
  ['broad', broad]
]) {
  if (chosen.some((c) => c.terms[0] === m.term)) continue;
  chosen.push({label: `${label} — term ${m.term}`, terms: [m.term], visible: m.visible});
}

const allTerms = measured.map((m) => m.term);
chosen.push({
  label: `everything (${allTerms.length} terms)`,
  terms: allTerms,
  visible: await visibleFor(allTerms)
});

const out = join(dirname(fileURLToPath(import.meta.url)), '..', 'viewer', 'presets.json');
await writeFile(out, `${JSON.stringify(chosen, null, 2)}\n`);
console.table(chosen.map((p) => ({label: p.label, terms: p.terms.length, visible: p.visible})));
console.log(
  `measured ${measured.length} non-empty terms of ${candidates.length} candidates; wrote ${out}`
);
