#!/usr/bin/env node
// Measure each candidate term's exact visible-set size, and emit viewer/presets.json.
//
// The size comes from the service itself: a zoom=0, full-extent viewport call returns `visible`
// for the single root tile, which IS that principal's visible-set cardinality. Nothing here is
// estimated, and nothing is derived from a drawn sample.
//
//   TESSERA_SESSION_CRED=… node clients/ts/scripts/measure-principals.mjs \
//     --viewer http://127.0.0.1:37585 --session http://127.0.0.1:49303 --terms 0..200 \
//     [--ranks <pairs>.term-ranks.json] [--out PATH]
//
// Re-run it per fixture: the term dictionary differs between bundles, so presets measured against
// 2m4 are meaningless against 1e8. That is what `--out` is for — the demo serves several bundles at
// once, and each needs its own measured list.
//
// With `--ranks` (scripts/rank_terms.py's output) it also composes COVERAGE principals — sparse
// ~1%, medium ~10%, heavy ~50% of the corpus — because at a 4.8 x 10^4-term dictionary any single
// term is a sliver and "switch principal" demonstrates nothing. Each is the shortest prefix of the
// ranked terms whose visible set reaches the target, found by binary search on the prefix length
// with the REAL visible measured per probe — the ranking orders candidates, the service decides
// sizes, and nothing here is estimated from pair counts.
//
// Note it decodes only the TILE stream, which is the response's first frame — so this script needs
// none of core's frame walking beyond one header, and stays plain JS.
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
  const view = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
  // `u8 kind, u32 LE length, <payload>` — every frame prefixed, tiles always first (`core/frame.ts`
  // carries the full table). Asserted rather than assumed: reading the length from byte 0 decodes
  // the kind tag as part of it, which yields a plausible-looking offset and an empty table, and an
  // empty table here reads as "this principal sees nothing" rather than as a broken parse.
  const kind = view.getUint8(0);
  if (kind !== 1) throw new Error(`expected a tiles frame first, got kind ${kind}`);
  const tileLength = view.getUint32(1, true);
  const tiles = tableFromIPC(buf.subarray(5, 5 + tileLength));
  const column = tiles.getChild('visible');
  if (!column) throw new Error('the tiles frame carries no `visible` column');
  return [...column.toArray()].reduce((a, b) => a + Number(b), 0);
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
const singles = args.ranks ? [['narrow', narrow]] : [['narrow', narrow], ['medium', medium], ['broad', broad]];
for (const [label, m] of singles) {
  if (chosen.some((c) => c.terms[0] === m.term)) continue;
  chosen.push({label: `${label} — term ${m.term}`, terms: [m.term], visible: m.visible});
}

if (args.ranks) {
  const {readFile} = await import('node:fs/promises');
  const rankedRows = JSON.parse(await readFile(args.ranks, 'utf8'));
  const ranked = rankedRows.map((r) => String(r.term));
  const pairShare = rankedRows.map((r) => r.pairs);
  const totalPairs = pairShare.reduce((a, b) => a + b, 0);
  // The denominator is the corpus a maximal principal can see, not the item count — measured the
  // same way as everything else. The whole dictionary in one authorise call would be a megabyte of
  // auth_data; the top slice of a Zipf-shaped ranking is within a hair of the same union.
  const CORPUS_PROBE_TERMS = Math.min(ranked.length, 4096);
  const corpus = await visibleFor(ranked.slice(0, CORPUS_PROBE_TERMS));
  console.log(`corpus visible (top ${CORPUS_PROBE_TERMS} ranked terms): ${corpus.toLocaleString()}`);

  // **A small target cannot start at the head of a Zipf ranking** — the head term alone was 5.5%
  // of this corpus, so no prefix is 1%. Each target instead starts at the first term whose own
  // pair share is at or below the target, and takes the shortest run of consecutive ranked terms
  // from there whose MEASURED union reaches it: pair shares choose where to start, the service
  // decides when the run is long enough. ~log2 service calls per target.
  // In PAIR space, scaled by the measured items-per-pair ratio: a term's pair share overstates
  // its visible share by the corpus's term multiplicity (~2.2x here), and without the scaling the
  // sparse preset started on a term twice its target.
  const startFor = (fraction) => {
    const pairFraction = fraction * (corpus / totalPairs);
    let i = 0;
    while (i < ranked.length - 1 && pairShare[i] / totalPairs > pairFraction) i++;
    return i;
  };
  const runReaching = async (start, target) => {
    const unions = new Map();
    const unionOf = async (n) => {
      if (!unions.has(n)) unions.set(n, await visibleFor(ranked.slice(start, start + n)));
      return unions.get(n);
    };
    let lo = 1;
    let hi = Math.min(4096, ranked.length - start);
    if ((await unionOf(hi)) < target) return {n: hi, visible: await unionOf(hi)};
    while (lo < hi) {
      const mid = Math.floor((lo + hi) / 2);
      if ((await unionOf(mid)) >= target) hi = mid;
      else lo = mid + 1;
    }
    return {n: lo, visible: await unionOf(lo)};
  };

  for (const [label, fraction] of [
    ['sparse', 0.01],
    ['medium', 0.1],
    ['heavy', 0.5]
  ]) {
    const start = startFor(fraction);
    const {n, visible} = await runReaching(start, corpus * fraction);
    const pct = (100 * visible) / corpus;
    chosen.push({
      label: `${label} — ${pct < 10 ? pct.toFixed(1) : Math.round(pct)}% (${n} terms)`,
      terms: ranked.slice(start, start + n),
      visible
    });
  }
  chosen.push({
    label: `full — top ${CORPUS_PROBE_TERMS} terms`,
    terms: ranked.slice(0, CORPUS_PROBE_TERMS),
    visible: corpus
  });
} else {
  const allTerms = measured.map((m) => m.term);
  chosen.push({
    label: `everything (${allTerms.length} terms)`,
    terms: allTerms,
    visible: await visibleFor(allTerms)
  });
}

// `--out` because presets are **per bundle** and the demo now serves more than one: a term id names
// a different set in each dictionary, so one shared file would mislabel every principal on whichever
// dataset it was not measured against. `run_demo.sh` composes the per-dataset files into
// `datasets.json`.
const out =
  args.out ?? join(dirname(fileURLToPath(import.meta.url)), '..', 'viewer', 'presets.json');
await writeFile(out, `${JSON.stringify(chosen, null, 2)}\n`);
console.table(chosen.map((p) => ({label: p.label, terms: p.terms.length, visible: p.visible})));
console.log(
  `measured ${measured.length} non-empty terms of ${candidates.length} candidates; wrote ${out}`
);
