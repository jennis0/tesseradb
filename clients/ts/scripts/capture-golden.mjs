#!/usr/bin/env node
// Recaptures core's golden fixtures from two `tessera serve`s this script builds and starts.
//
//   node clients/ts/scripts/capture-golden.mjs
//
// The binary and the notebook corpus are found as the live tests find them (`core/test/served.ts`).
// `python3` with pyarrow writes the wide corpus's Parquet file. Every file in `core/test/fixtures`
// is rewritten except `viewport-artifacts-pre-r40.bin`, and `wire-example/test/expected.json` with
// them. Each capture checks the arrangement its tests rely on, and where the server did not
// provide it the script stops and writes nothing.
//
// Each build generates its own identity key, so a recapture serves new `tessera_id`s and rewrites
// every file that carries one.
import {spawnSync} from 'node:child_process';
import {mkdtempSync, rmSync, writeFileSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {
  Bool,
  Field,
  Float32,
  Float64,
  Int16,
  Int32,
  Int64,
  Int8,
  List,
  Table,
  TimestampMicrosecond,
  Uint16,
  Uint32,
  Uint64,
  Uint8,
  Utf8,
  tableFromIPC,
  tableToIPC,
  vectorFromArray
} from 'apache-arrow';
import {base64} from '../core/src/control.ts';
import {start} from '../core/test/served.ts';

const FIXTURES = join(import.meta.dirname, '..', 'core', 'test', 'fixtures');
const EXPECTED = join(import.meta.dirname, '..', 'wire-example', 'test', 'expected.json');

/** Every file to write, by path, held until every capture has passed its checks. */
const captured = new Map();

function check(condition, what) {
  if (!condition) throw new Error(`capture-golden: ${what}. Nothing was written`);
}

/** A viewport body's frames in order, each `{kind, payload}`. */
function frames(body) {
  const view = new DataView(body.buffer, body.byteOffset, body.byteLength);
  const out = [];
  for (let at = 0; at < body.length; ) {
    const length = view.getUint32(at + 1, true);
    out.push({kind: body[at], payload: body.subarray(at + 5, at + 5 + length)});
    at += 5 + length;
  }
  return out;
}

/** Each frame of `kind` read as an Arrow table. */
const tables = (body, kind) => frames(body).filter((f) => f.kind === kind).map((f) => tableFromIPC(f.payload));
const column = (table, name) => Array.from(table.getChild(name));
const total = (table, name) => column(table, name).reduce((sum, v) => sum + Number(v), 0);

/** A session for `terms` on `served`, and the three viewer routes the goldens come from. */
async function session(served, terms) {
  const authorised = await fetch(`${served.sessionUrl}/session/authorise`, {
    method: 'POST',
    headers: {authorization: `Bearer ${served.sessionCredential}`, 'content-type': 'application/json'},
    body: JSON.stringify({auth_data: base64(new TextEncoder().encode(JSON.stringify({terms})))})
  });
  if (!authorised.ok) throw new Error(`authorise: ${authorised.status} ${await authorised.text()}`);
  const {token} = await authorised.json();
  const call = async (path, body) => {
    const response = await fetch(`${served.viewerUrl}${path}`, {
      method: body === undefined ? 'GET' : 'POST',
      headers: {authorization: `Bearer ${token}`, 'content-type': 'application/json'},
      body: body === undefined ? undefined : JSON.stringify(body)
    });
    if (!response.ok) throw new Error(`${path}: ${response.status} ${await response.text()}`);
    return response;
  };
  return {
    meta: async () => (await call('/v1/meta')).json(),
    viewport: async (body) => new Uint8Array(await (await call('/v1/viewport', body)).arrayBuffer()),
    browse: async (body) => (await call('/v1/artifacts/browse', body)).json()
  };
}

// The wide corpus: every type a rendered column can have, with nulls in each.

const WIDE_ROWS = 1000;

/**
 * The wide corpus's rows. Each non-category column is null on every n-th row, n differing by
 * column, and each category is absent on some rows. The values reach past what a narrower type
 * would hold: `rank` past u16, `hash` and `long` past 2^53, `exact` past f32's precision.
 */
function wideTable() {
  const rows = Array.from({length: WIDE_ROWS}, (_, i) => i);
  const every = (n, value, type) => vectorFromArray(rows.map((i) => (i % n === n - 1 ? null : value(i))), type);
  return new Table({
    entity_id: vectorFromArray(rows.map((i) => BigInt(i + 1)), new Uint64()),
    x: vectorFromArray(rows.map((i) => ((i * 7919) % 1000) + 0.5), new Float64()),
    y: vectorFromArray(rows.map((i) => ((i * 104729) % 997) + 0.25), new Float64()),
    labels: vectorFromArray(rows.map(() => ['golden']), new List(new Field('item', new Utf8(), true))),
    colour: every(9, (i) => ['red', 'green', 'blue', 'amber'][i % 4], new Utf8()),
    family: every(11, (i) => `f${String(i % 20).padStart(3, '0')}`, new Utf8()),
    tag: every(13, (i) => `t${i % 37}`, new Utf8()),
    flag: every(4, (i) => i % 3 === 0, new Bool()),
    small: every(5, (i) => i % 256, new Uint8()),
    medium: every(6, (i) => (i * 61) % 65536, new Uint16()),
    rank: every(7, (i) => 70_000 + i * 1_000, new Uint32()),
    hash: every(8, (i) => 2n ** 63n + BigInt(i) * 7_919n, new Uint64()),
    tiny: every(10, (i) => (i % 256) - 128, new Int8()),
    short: every(12, (i) => i * 31 - 16_000, new Int16()),
    offset: every(14, (i) => i * 2_000_003 - 1_000_000_000, new Int32()),
    long: every(15, (i) => -(2n ** 60n) + BigInt(i), new Int64()),
    // apache-arrow takes a timestamp as epoch milliseconds and stores microseconds.
    at: every(16, (i) => 1_700_000_000_000 + i * 3_600_000, new TimestampMicrosecond()),
    ratio: every(17, (i) => i / 7, new Float32()),
    exact: every(18, (i) => i + 1 / 3, new Float64()),
    title: vectorFromArray(rows.map((i) => `item ${i}`), new Utf8()),
    code: vectorFromArray(rows.map((i) => `c-${i}`), new Utf8())
  });
}

const attribute = (name, type, extra = '') => `[[attribute]]\nname   = "${name}"\ntype   = "${type}"\n${extra}`;
const rendered = (name, type) => attribute(name, type, 'render = true\n');
const category = (name) => attribute(name, 'category', `vocabulary = "${name}"\nrender     = true\n`);

const WIDE_SCHEMA = `[sources]
points = "points.parquet"

[defaults]
source = "points"

[[view]]
name             = "s0"
extent           = { x = [0, 1000], y = [0, 1000] }
point_visibility = { field = "labels", default = "public" }

[[vocabulary]]
name       = "colour"
width      = "u8"
value_set  = "closed"
visibility = "public"
values     = { red = 1, green = 2, blue = 3, amber = 4 }

[[vocabulary]]
name       = "family"
width      = "u16"
value_set  = "closed"
visibility = "public"
values     = { ${Array.from({length: 20}, (_, i) => `f${String(i).padStart(3, '0')} = ${i + 1}`).join(', ')} }

[[vocabulary]]
name       = "tag"
width      = "u16"
value_set  = "open"
visibility = "derived"
values     = { ${Array.from({length: 37}, (_, i) => `t${i} = ${i + 1}`).join(', ')} }

${[
  category('colour'),
  category('family'),
  category('tag'),
  rendered('flag', 'bool'),
  rendered('small', 'u8'),
  rendered('medium', 'u16'),
  rendered('rank', 'u32'),
  rendered('hash', 'u64'),
  rendered('tiny', 'i8'),
  rendered('short', 'i16'),
  rendered('offset', 'i32'),
  rendered('long', 'i64'),
  rendered('at', 'timestamp_us'),
  rendered('ratio', 'f32'),
  rendered('exact', 'f64'),
  attribute('title', 'text', 'index  = true\n'),
  attribute('code', 'keyword', 'index  = true\n')
].join('\n')}`;

function writeParquet(table, path) {
  const script = 'import sys, pyarrow as pa, pyarrow.parquet as pq; pq.write_table(pa.ipc.open_stream(sys.stdin.buffer).read_all(), sys.argv[1])';
  const python = spawnSync('python3', ['-c', script, path], {input: tableToIPC(table, 'stream')});
  if (python.status !== 0) {
    throw new Error(`python3 could not write the wide corpus's Parquet file (${python.error ?? python.stderr}); install pyarrow for python3 and re-run`);
  }
}

/** `meta.json`, `viewport-plain.bin` and `viewport-underlay.bin`, over the wide corpus. */
async function captureWide() {
  const directory = mkdtempSync(join(tmpdir(), 'tessera-goldens-'));
  try {
    writeParquet(wideTable(), join(directory, 'points.parquet'));
    const served = await start({corpus: {directory, schema: WIDE_SCHEMA}});
    if (typeof served === 'string') throw new Error(served);
    try {
      const golden = await session(served, ['golden']);
      const meta = await golden.meta();
      const q = meta.views[0].quantisation;
      // `layers: []`, so these two carry no artifacts frame.
      const base = {view: 's0', zoom: 2, bbox: [q.x_min, q.y_min, q.x_max, q.y_max], k: 20, layers: []};
      const plain = await golden.viewport(base);
      const underlay = await golden.viewport({...base, underlay_offset: 2});

      const points = tables(plain, 3);
      check(points.length > 0, 'the plain capture served no points');
      const ids = points.flatMap((t) => column(t, 'tessera_id'));
      const rendered = meta.declared_scalars.filter((c) => c.render);
      for (const {name, category} of rendered) {
        check(points[0].getChild(name) !== null, `the points frame carries no column ${name}`);
        if (category === null) check(points.some((t) => t.getChild(name).nullCount > 0), `no served point is null in ${name}`);
      }
      // The worked decodes check a tessera_id survives past 2^53.
      check(ids[0] > 2n ** 53n, 'the first served tessera_id is below 2^53; run the capture again, which builds under a new key');
      captured.set('meta.json', JSON.stringify(meta, null, 2) + '\n');
      captured.set('viewport-plain.bin', plain);
      captured.set('viewport-underlay.bin', underlay);
    } finally {
      served.stop();
    }
  } finally {
    rmSync(directory, {recursive: true, force: true});
  }
}

// The notebook corpus: real clusters, a filter and a highlight, and the browse pages.

/** A principal seeing about half the notebook corpus, so its visible set is not the whole map. */
const PRINCIPAL = [
  'cs.LG', 'cs.CV', 'cs.AI', 'cs.CL', 'cs.RO', 'math.CO', 'math.AP', 'math.PR', 'math-ph', 'math.MP',
  'hep-th', 'hep-ph', 'quant-ph', 'astro-ph', 'gr-qc', 'cond-mat.mes-hall', 'cond-mat.mtrl-sci',
  'physics.optics', 'stat.ML', 'eess.SP'
];

/**
 * `viewport-artifacts.bin`, `viewport-membership.bin`, the three highlight bodies and the three
 * browse pages, over the notebook corpus as `PRINCIPAL`.
 */
async function captureNotebook() {
  const served = await start();
  if (typeof served === 'string') throw new Error(served);
  try {
    const reader = await session(served, PRINCIPAL);
    const q = (await reader.meta()).views.find((v) => v.id === 's0').quantisation;
    const full = [q.x_min, q.y_min, q.x_max, q.y_max];

    // The k-means layer declares centroid, box and hull, over clusters in different parts of the
    // map. At `k = 0` the body is tiles, artifacts and trailer; named with points it adds the
    // points frame and its membership column.
    const clusters = {view: 's0', zoom: 2, bbox: full, layers: ['clusters/kmeans']};
    const channel = await reader.viewport({...clusters, k: 0});
    const membership = await reader.viewport({...clusters, k: 50});
    check(frames(channel).map((f) => f.kind).join() === '1,5,4', 'the k = 0 capture is not tiles, artifacts and trailer');
    const artifacts = tables(membership, 5)[0];
    check(artifacts?.getChild('shape_x') != null, 'the clusters carry no shape');
    const centroids = new Set(column(artifacts, 'centroid_x').map((x, i) => `${x},${artifacts.getChild('centroid_y').get(i)}`));
    check(artifacts.numRows >= 3 && centroids.size === artifacts.numRows, 'the clusters do not have distinct centroids');
    // Every row of the `k = 0` body carries a shape, and between them they hold a shape of several
    // parts and a ring of several vertices, so a decoder reading only the first of either fails.
    const shapes = column(tables(channel, 5)[0], 'shape_x');
    check(shapes.every((parts) => parts !== null), 'an artifact of the k = 0 capture carries no shape');
    check(shapes.some((parts) => parts.length > 1), 'no artifact of the k = 0 capture has a shape of several parts');
    check(
      shapes.some((parts) => Array.from(parts).some((rings) => Array.from(rings).some((ring) => ring.length > 1))),
      'no ring of the k = 0 capture has several vertices'
    );
    const members = tables(membership, 3).flatMap((t) => column(t, 'membership:clusters/kmeans'));
    check(members.some((m) => m !== null), 'no served point is named a member');

    // One request in three shapes. The taxonomy's artifacts are archives and subject classes, so
    // the filter admits some of them and the highlight fewer.
    const request = {view: 's0', zoom: 3, bbox: full, k: 20, layers: ['taxonomy/arxiv'], computed: ['centroid'], filters: {archive: {in: ['cs', 'math']}}};
    const lit = {...request, highlight: {archive: {in: ['cs']}}};
    const highlight = await reader.viewport(lit);
    const pointRows = await reader.viewport({...lit, point_rows: 'highlight'});
    const plain = await reader.viewport(request);
    const tiles = tables(highlight, 1)[0];
    const [visible, matched, highlighted] = ['visible', 'matched', 'highlighted'].map((c) => total(tiles, c));
    check(0 < highlighted && highlighted < matched && matched < visible, `the tiles do not count highlighted < matched < visible, all above zero (${highlighted}, ${matched}, ${visible})`);
    const bits = tables(highlight, 3).flatMap((t) => column(t, 'highlighted'));
    check(bits.some((b) => b) && bits.some((b) => !b), 'the served points are all highlighted or none are');
    const litArtifacts = tables(highlight, 5)[0];
    const matchedBits = column(litArtifacts, 'matched');
    const highlightedBits = column(litArtifacts, 'highlighted');
    check(matchedBits.some((b) => b) && matchedBits.some((b) => !b), 'the filter admits every artifact or none');
    check(highlightedBits.some((b) => b) && highlightedBits.some((b, i) => !b && matchedBits[i]), 'the highlight lights no artifact, or every one the filter admits');

    // The browse pages. The roots page is paged; the children are a root's, under a filter that
    // admits nothing of at least one; the search is paged and carries no filter.
    const roots = await reader.browse({view: 's0', layer: 'clusters/kmeans', limit: 4});
    check(typeof roots.next === 'string', 'the roots page has no next page');
    const [root] = (await reader.browse({view: 's0', layer: 'clusters/hdbscan', limit: 1})).artifacts;
    const children = await reader.browse({view: 's0', layer: 'clusters/hdbscan', parent: root.tessera_id, filters: {archive: {in: ['q-fin']}}});
    check(children.artifacts.every((a) => a.parent_ids.includes(root.tessera_id)), 'a child does not name the root it was asked under');
    check(children.artifacts.every((a) => a.rung === 1), 'a child of the root is not at rung 1');
    check(children.artifacts.some((a) => a.matched_count === 0 && a.masked_count > 0), 'no child is one the filter admits nothing of');
    const search = await reader.browse({view: 's0', layer: 'clusters/hdbscan', q: 'hdb', limit: 2});
    check(typeof search.next === 'string', 'the search page has no next page');
    const browsed = [roots, children, search].flatMap((p) => [...p.artifacts, ...p.parents].flatMap((a) => [a.tessera_id, ...a.parent_ids]));
    check(browsed.some((id) => BigInt(id) > 2n ** 53n), 'no browsed tessera_id is past 2^53');

    captured.set('viewport-artifacts.bin', channel);
    captured.set('viewport-membership.bin', membership);
    captured.set('viewport-highlight.bin', highlight);
    captured.set('viewport-point-rows-highlight.bin', pointRows);
    captured.set('viewport-no-highlight.bin', plain);
    for (const [name, page] of [['roots', roots], ['children-filtered', children], ['search', search]]) {
      captured.set(`browse-${name}.json`, JSON.stringify(page, null, 2) + '\n');
    }
  } finally {
    served.stop();
  }
}

/** What both worked decodes must read from a body: frame kinds, row counts and first ids. */
function answer(body) {
  const points = tables(body, 3);
  const artifacts = tables(body, 5)[0] ?? null;
  const firstPoint = points.find((t) => t.numRows > 0)?.getChild('tessera_id').get(0);
  return {
    frames: frames(body).map((f) => f.kind),
    tiles: tables(body, 1)[0].numRows,
    sub_cells: tables(body, 2)[0]?.numRows ?? null,
    artifacts: artifacts?.numRows ?? null,
    points: points.reduce((n, t) => n + t.numRows, 0),
    first_point_tessera_id: firstPoint === undefined ? null : String(firstPoint),
    first_artifact_tessera_id: artifacts ? String(artifacts.getChild('tessera_id').get(0)) : null
  };
}

await captureWide();
await captureNotebook();

const expected = {
  _comment:
    'What both worked decodes, clients/ts/wire-example (apache-arrow) and reference/examples/decode_viewport.py (pyarrow), must agree on over the golden fixtures in clients/ts/core/test/fixtures: frame kinds in order, row counts per batch, and the first tessera_id of the points and artifacts batches as decimal strings. Written by clients/ts/scripts/capture-golden.mjs with the fixtures.'
};
for (const name of ['viewport-plain.bin', 'viewport-underlay.bin', 'viewport-artifacts.bin', 'viewport-membership.bin']) {
  expected[name] = answer(captured.get(name));
}
for (const [name, contents] of captured) writeFileSync(join(FIXTURES, name), contents);
writeFileSync(EXPECTED, JSON.stringify(expected, null, 2) + '\n');
for (const [name, contents] of captured) console.log(`${name}: ${contents.length} bytes`);
