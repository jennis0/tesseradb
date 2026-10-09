#!/usr/bin/env node
// Measure each candidate term's exact visible-set size, and emit viewer/presets.json.
//
// The size comes from the server: a zoom-0, full-extent viewport call returns `visible` for the
// root tile, which is the principal's visible-set cardinality.
//
//   MOSAICA_OPERATOR_CRED=… node clients/ts/scripts/measure-principals.mjs \
//     --viewer http://127.0.0.1:37585 --session http://127.0.0.1:49303 \
//     --control http://127.0.0.1:45721 --terms 0..200 \
//     [--ranks <pairs>.term-ranks.json] [--out PATH] [--key-out PATH]
//
// Each term set is measured with a session the operator credential mints for those terms. Each
// preset written is held by a local principal of the deployment's catalogue, created on the control
// plane and named by a digest of its terms, and the preset names it. `--key-out` writes a new API
// key of the service principal `demo-viewer`, which holds `authorise-as` and which the viewer uses
// to mint the presets' sessions.
//
// `--terms-file PATH` is `--terms` for a corpus whose keys carry commas: one term per line, blank
// lines skipped.
//
// Run it per bundle, since term dictionaries differ; `--out` writes each bundle's list.
//
// With `--ranks` (scripts/rank_terms.py's output) it also composes coverage principals, sparse
// (about 1%), medium (about 10%) and heavy (about 85%) of the corpus, since in a large dictionary
// one term sees very little. Each is a run of ranked terms whose measured visible set reaches the
// target, found by binary search on the run's length. `full` holds every ranked term and is the
// denominator; if authorising that many terms is refused, it falls back to the top 4096 terms and
// its label says so.
//
// It decodes only the tiles frame, which comes first in the response.
import {readFile, writeFile} from 'node:fs/promises';
import {dirname, join} from 'node:path';
import {fileURLToPath} from 'node:url';
import {tableFromIPC} from 'apache-arrow';
// Loading a `.ts` module needs Node 22.18 or later, which strips its types.
import {Control} from '../core/src/control.ts';
import {authorise as mint, integratorKey, principalHolding} from './operator.ts';

const args = Object.fromEntries(
  process.argv
    .slice(2)
    .reduce((acc, a, i, all) => (a.startsWith('--') ? [...acc, [a.slice(2), all[i + 1]]] : acc), [])
);
const viewer = args.viewer ?? 'http://127.0.0.1:37585';
const session = args.session ?? 'http://127.0.0.1:49303';
const operatorCred = process.env.MOSAICA_OPERATOR_CRED;
if (!operatorCred) throw new Error('set MOSAICA_OPERATOR_CRED');
const control = new Control({controlUrl: args.control ?? 'http://127.0.0.1:45721', credential: operatorCred});

// `--terms` splits on commas, which a term may contain (a publisher called "Royal Botanic
// Gardens, Kew"); `--terms-file` takes one term per line. `--terms` also takes the `lo..hi` form.
if (args.terms && args['terms-file']) {
  throw new Error('--terms and --terms-file both name the candidate list; pass one');
}
const candidates = args['terms-file']
  ? (await readFile(args['terms-file'], 'utf8'))
      .split('\n')
      .map((line) => line.trim())
      .filter((line) => line.length > 0)
  : (() => {
      const spec = args.terms ?? '0..200';
      if (!spec.includes('..')) return spec.split(',');
      const [lo, hi] = spec.split('..').map(Number);
      return Array.from({length: hi - lo + 1}, (_, i) => String(lo + i));
    })();

async function authorise(terms) {
  return (await mint(session, operatorCred, {terms})).token;
}

const probeToken = await authorise([candidates[0]]);
const metaResp = await fetch(`${viewer}/v1/meta`, {
  headers: {authorization: `Bearer ${probeToken}`}
});
if (!metaResp.ok) throw new Error(`meta: ${metaResp.status} ${await metaResp.text()}`);
const meta = await metaResp.json();
const q = meta.views[0].quantisation;
const view = meta.views[0].id;

/** The `visible` total from a zoom-0, full-extent call: this principal's visible-set size. */
async function visibleFor(terms) {
  const token = await authorise(terms);
  const r = await fetch(`${viewer}/v1/viewport`, {
    method: 'POST',
    headers: {authorization: `Bearer ${token}`, 'content-type': 'application/json'},
    // k = 1: only the counts are wanted, and the tiles frame carries them exactly.
    body: JSON.stringify({view, zoom: 0, bbox: [q.x_min, q.y_min, q.x_max, q.y_max], k: 1})
  });
  if (!r.ok) throw new Error(`viewport ${terms}: ${r.status} ${await r.text()}`);
  const buf = new Uint8Array(Buffer.from(await r.arrayBuffer()));
  // Not `view`, which would shadow the module's view id used in the request above.
  const frame = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
  // Each frame is `u8 kind, u32 LE length, <payload>`, tiles first. The kind is checked, since a
  // misread header gives an empty table that would read as a principal seeing nothing.
  const kind = frame.getUint8(0);
  if (kind !== 1) throw new Error(`expected a tiles frame first, got kind ${kind}`);
  const tileLength = frame.getUint32(1, true);
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

// Distinct terms only: in a small dictionary the quantiles can collide.
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
  // The denominator is what a principal holding every ranked term sees, measured like the rest.
  let fullTerms = ranked;
  let fullLabel = `full — all ${ranked.length} terms`;
  let corpus;
  try {
    corpus = await visibleFor(fullTerms);
  } catch (e) {
    // The session refused a dictionary-sized term set: measure the head, and say so.
    fullTerms = ranked.slice(0, Math.min(ranked.length, 4096));
    fullLabel = `full — top ${fullTerms.length} of ${ranked.length} terms (session refused all: ${e.message})`;
    corpus = await visibleFor(fullTerms);
  }
  console.log(`corpus visible (${fullLabel}): ${corpus.toLocaleString()}`);

  // The head term alone may exceed a small target, so each run starts at the first term whose
  // pair share is at or below the target, and is the shortest run from there whose measured union
  // reaches it: about log2 calls per target. Pair shares are scaled by the measured items-per-pair
  // ratio, since a term's pair share overstates its visible share by the corpus's term multiplicity.
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
    // One term can overshoot the target a long way when terms overlap heavily, so take whichever
    // of this run and the one before is closer to it.
    const over = await unionOf(lo);
    if (lo > 1) {
      const under = await unionOf(lo - 1);
      if (Math.abs(target - under) < Math.abs(over - target)) return {n: lo - 1, visible: under};
    }
    return {n: lo, visible: over};
  };

  /** @type {[string, number][]} */
  const bands = [
    ['sparse', 0.01],
    ['medium', 0.1],
    ['heavy', 0.85]
  ];
  for (const [label, fraction] of bands) {
    const start = startFor(fraction);
    const {n, visible} = await runReaching(start, corpus * fraction);
    const pct = (100 * visible) / corpus;
    chosen.push({
      label: `${label} — ${pct < 10 || pct > 99 ? pct.toFixed(1) : Math.round(pct)}% (${n} terms)`,
      terms: ranked.slice(start, start + n),
      visible
    });
  }
  chosen.push({label: fullLabel, terms: fullTerms, visible: corpus});
} else {
  const allTerms = measured.map((m) => m.term);
  chosen.push({
    label: `everything (${allTerms.length} terms)`,
    terms: allTerms,
    visible: await visibleFor(allTerms)
  });
}

// Presets are per bundle. `run_demo.sh` composes the per-bundle files into the dataset document
// it names in the viewer URL it prints.
const presets = [];
for (const preset of chosen) presets.push({...preset, principal: await principalHolding(control, preset.terms)});
const out =
  args.out ?? join(dirname(fileURLToPath(import.meta.url)), '..', 'viewer', 'presets.json');
await writeFile(out, `${JSON.stringify(presets, null, 2)}\n`);
if (args['key-out']) await writeFile(args['key-out'], `${await integratorKey(control, 'demo-viewer')}\n`, {mode: 0o600});
console.table(chosen.map((p) => ({label: p.label, terms: p.terms.length, visible: p.visible})));
console.log(
  `measured ${measured.length} non-empty terms of ${candidates.length} candidates; wrote ${out}`
);
