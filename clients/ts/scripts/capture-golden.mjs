#!/usr/bin/env node
// Capture golden payloads from a live `tessera serve` for core's decoder tests.
//
// Usage:
//   TESSERA_SESSION_CRED=… node clients/ts/scripts/capture-golden.mjs \
//     --viewer http://127.0.0.1:37585 --session http://127.0.0.1:49303 --terms 0
//
// Writes core/test/fixtures/{meta.json,viewport-plain.bin,viewport-underlay.bin}. Re-run it
// whenever the wire format changes; a decoder test passing against a stale golden is worse than
// no test.
import {writeFile, mkdir} from 'node:fs/promises';
import {dirname, join} from 'node:path';
import {fileURLToPath} from 'node:url';

const args = Object.fromEntries(
  process.argv
    .slice(2)
    .reduce((acc, a, i, all) => (a.startsWith('--') ? [...acc, [a.slice(2), all[i + 1]]] : acc), [])
);
const viewer = args.viewer ?? 'http://127.0.0.1:37585';
const session = args.session ?? 'http://127.0.0.1:49303';
const terms = (args.terms ?? '0').split(',');
const cred = process.env.TESSERA_SESSION_CRED;
if (!cred) throw new Error('set TESSERA_SESSION_CRED to the session credential');

const authorise = await fetch(`${session}/session/authorise`, {
  method: 'POST',
  headers: {authorization: `Bearer ${cred}`, 'content-type': 'application/json'},
  body: JSON.stringify({auth_data: Buffer.from(JSON.stringify({terms})).toString('base64')})
});
if (!authorise.ok) throw new Error(`authorise: ${authorise.status} ${await authorise.text()}`);
const {token} = await authorise.json();

const metaResp = await fetch(`${viewer}/v1/meta`, {headers: {authorization: `Bearer ${token}`}});
if (!metaResp.ok) throw new Error(`meta: ${metaResp.status} ${await metaResp.text()}`);
const meta = await metaResp.json();

async function viewport(body) {
  const r = await fetch(`${viewer}/v1/viewport`, {
    method: 'POST',
    headers: {authorization: `Bearer ${token}`, 'content-type': 'application/json'},
    body: JSON.stringify(body)
  });
  if (!r.ok) throw new Error(`viewport: ${r.status} ${await r.text()}`);
  return Buffer.from(await r.arrayBuffer());
}

const q = meta.quantisation;
const full = [q.x_min, q.y_min, q.x_max, q.y_max];
const base = {slice: meta.slices[0].id, zoom: 2, bbox: full, k: 50};

const dir = join(dirname(fileURLToPath(import.meta.url)), '..', 'core', 'test', 'fixtures');
await mkdir(dir, {recursive: true});
const plain = await viewport(base);
const underlay = await viewport({...base, underlay_offset: 2});
await writeFile(join(dir, 'meta.json'), JSON.stringify(meta, null, 2));
await writeFile(join(dir, 'viewport-plain.bin'), plain);
await writeFile(join(dir, 'viewport-underlay.bin'), underlay);
console.log(
  `captured to ${dir}: plain ${plain.length} B, underlay ${underlay.length} B (delta ${underlay.length - plain.length} B)`
);
