#!/usr/bin/env node
// Capture golden payloads from a live `tessera serve` for core's decoder tests.
//
// Usage:
//   TESSERA_SESSION_CRED=… node clients/ts/scripts/capture-golden.mjs \
//     --viewer http://127.0.0.1:37585 --session http://127.0.0.1:49303 --terms 0
//
// Writes core/test/fixtures/{meta.json,viewport-plain.bin,viewport-underlay.bin} and, where the
// server carries a layer, viewport-artifacts.bin (the channel's counts-only shape, which the
// worked decodes in reference/examples pin byte for byte) and viewport-membership.bin (the layer
// named with points, so the per-point membership column is on it). Re-run it whenever the wire format changes; a
// decoder test passing against a stale golden is worse than no test. `--artifacts-only`
// recaptures the artifacts golden alone, against whatever corpus carries a layer, and leaves the
// wide-schema goldens untouched.
//
// **Capture against the WIDE fixture** (`data/scaled/attrs/schema-wide.toml`, nineteen columns),
// not against a demo bundle. `decode.test.ts` walks `meta.json`'s declared columns and checks each
// one decodes at its declared type, so the goldens' value is the breadth of the schema behind
// them: captured against the six-column demo bundle the same test still passes and silently stops
// covering two thirds of the Arrow types.
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
const artifactsOnly = 'artifacts-only' in args;
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

const q = meta.views[0].quantisation; // the frame is the view's (decision 0040)
const full = [q.x_min, q.y_min, q.x_max, q.y_max];
// **`layers: []` deliberately**, so these two goldens carry no artifacts frame however many layers
// the capturing server happens to hold. Omitting it would answer for every layer this principal
// reaches, and the pair exists to pin the *absent*-frame case — the ordinary shape of a response,
// and the one a decoder must read as "no artifacts" rather than as a truncated body.
const base = {view: meta.views[0].id, zoom: 2, bbox: full, k: 50, layers: []};

const dir = join(dirname(fileURLToPath(import.meta.url)), '..', 'core', 'test', 'fixtures');
await mkdir(dir, {recursive: true});
if (!artifactsOnly) {
  const plain = await viewport(base);
  const underlay = await viewport({...base, underlay_offset: 2});
  await writeFile(join(dir, 'meta.json'), JSON.stringify(meta, null, 2));
  await writeFile(join(dir, 'viewport-plain.bin'), plain);
  await writeFile(join(dir, 'viewport-underlay.bin'), underlay);
  console.log(
    `captured to ${dir}: plain ${plain.length} B, underlay ${underlay.length} B (delta ${underlay.length - plain.length} B)`
  );
}

/**
 * The artifacts frame, captured only when this server actually carries a layer this principal
 * reaches — which needs `scripts/publish-clusters.mjs` to have run.
 *
 * **The other two goldens deliberately carry no kind-5 frame**, which is the ordinary shape of a
 * response and the case worth pinning most: a decoder must read a body with no artifacts frame as
 * *no artifacts*, not as a parse failure. This one covers the other side, and is skipped rather
 * than faked when there is no layer — a hand-assembled frame would be a test of this script's idea
 * of the format rather than of the server's.
 */
if ((meta.layers ?? []).length > 0) {
  const layer = meta.layers[0].name;
  // The layer named with points: the artifacts frame **and** a points frame carrying the per-point
  // membership column (D12, contracts §3.2 r39) — the point path's own shape once a layer is on.
  // A larger `k` than the other goldens, so the clusters' members are among the points served
  // rather than only their tiles' heads.
  const membership = await viewport({...base, k: 200, layers: [layer]});
  await writeFile(join(dir, 'viewport-membership.bin'), membership);
  // And the annotation channel's own shape: `k = 0`, the tiles, the artifacts frame and the
  // trailer, no points frame at all — the body a decoder is most likely to misread as truncated.
  // Pinned by the worked decodes' answer sheet (`wire-example/test/expected.json`, re-derived on
  // every capture), so capture it as the same principal every time: on the notebook corpus the
  // terms are arXiv categories and `--terms 0` sees nothing, so the r43 goldens were taken as its
  // *medium* preset (`.dev/presets/notebook.json`, two terms).
  const channel = await viewport({...base, k: 0, layers: [layer]});
  await writeFile(join(dir, 'viewport-artifacts.bin'), channel);
  console.log(`captured viewport-membership.bin (${membership.length} B) and viewport-artifacts.bin (${channel.length} B) for layer ${layer}`);
} else {
  console.log('no layer reachable: viewport-artifacts.bin not re-captured');
}
