// A fast, browser-free loop over the client pipeline: plan -> fetch -> decode -> absorb -> assemble.
//
//   cd clients/ts && npx tsx ../../probes/2026-08-09-client-pipeline/pipeline.mts \
//     [--steps 8] [--budget 500000] [--ring] [--bites 3] [--zoom 4.3]
//
// Needs a running `tessera serve` (run_demo.sh --no-viewer). ~15 s per run against ~2 min for the
// browser probes, and it attributes time per phase instead of leaving it to be inferred from panel
// text. It cannot see deck.gl upload or frame scheduling — but the seconds have never been there.
import {TesseraClient} from '../../clients/ts/core/src/client.js';
import {Replica} from '../../clients/ts/core/src/replica.js';
import {plan} from '../../clients/ts/core/src/prefetch.js';
import {inlineDecoder} from '../../clients/ts/core/src/decoder.js';
import {assemble} from '../../clients/ts/viewer/src/assemble.js';
import {bandsOfResult} from '../../clients/ts/core/src/bands.js';
import {buildColourAttribute} from '../../clients/ts/viewer/src/colour.js';

const arg = (n: string, d: number) => {
  const i = process.argv.indexOf(`--${n}`);
  return i >= 0 ? Number(process.argv[i + 1]) : d;
};
const STEPS = arg('steps', 8);
const BUDGET = arg('budget', 500_000);

const client = new TesseraClient({
  viewerUrl: process.env.TESSERA_VIEWER_URL ?? 'http://127.0.0.1:37585',
  sessionUrl: process.env.TESSERA_SESSION_URL ?? 'http://127.0.0.1:49303',
  sessionCredential: process.env.TESSERA_SESSION_CRED ?? 'dev-session-credential',
  decoder: inlineDecoder()
});

// The broad principal: the narrow ones saturate, so the budget never binds and nothing here
// is exercised.
const terms = Array.from({length: 201}, (_, i) => String(i));
const session = await client.authorise(terms);
const meta = await client.meta(session.token);

let requests = 0;
let bytes = 0;
let wireMs = 0;
let absorbMs = 0;
const replica = new Replica(
  async (req, signal) => {
    const t = performance.now();
    const r = await client.viewport(session.token, {...req, slice: meta.slices[0]!.id}, signal);
    wireMs += performance.now() - t;
    requests += 1;
    bytes += r.bytes;
    // What the main thread does with a response once it has arrived: split it into bands. Timed
    // separately because it lands as a burst rather than spread over frames, which is what a hitch
    // is. Decoding is already off-thread; this is not.
    const ta = performance.now();
    bandsOfResult(r.result, req.zoom, {identityKey: '', contentKey: '', capUsed: 500, now: 0});
    absorbMs += performance.now() - ta;
    return r;
  },
  meta.quantisation,
  {slice: meta.slices[0]!.id}
);

let mTarget = meta.selection.thetaTargetMarks;
let visibleInView: number | undefined;
// zoom 4.3 is roughly where the demo lands after a few wheel notches: ~16k tiles in the
// margined box at the depth the budget picks.
const viewport = {target: [256, 256] as [number, number], zoom: arg('zoom', 4.3), width: 1280, height: 800};

console.log('step | reqs | wire+dec | absorb | redraw | bands  | held pts | drawn  | bytes');
for (let step = 0; step < STEPS; step++) {
  // A third of a viewport per step, which is what a drag moves.
  const stride = (viewport.width / 2 ** viewport.zoom) / 3;
  viewport.target = [viewport.target[0] + stride, viewport.target[1]];
  const r0 = requests;
  const b0 = bytes;
  const w0 = wireMs;
  const a0 = absorbMs;

  const p = plan({
    viewport,
    budget: BUDGET,
    mTarget,
    maxTiles: meta.maxTilesPerRequest,
    visibleInView,
    heldBytes: replica.bytes,
    budgetBytes: replica.budgetBytes
  });
  const frame = await replica.fetchRegion(p.visible.rect, p.choice.depth, meta.selection.kMaxMarks, undefined, p.render);
  // The render-side half: turning held bands into the buffers deck.gl uploads. Runs on every
  // redraw, so it is per gesture rather than per fetch.
  const ta = performance.now();
  const assembled = assemble(frame, ['primary_category']);
  const asmMs = performance.now() - ta;
  const drawn = assembled.ids.length;

  // The redraw path on its own: what a *frame* costs when nothing is fetched. This is what runs on
  // every animation frame of a drag, so it is the number that decides whether panning is smooth.
  const phase = (label: string, fn: () => void) => {
    fn();
    const t = performance.now();
    for (let i = 0; i < 5; i++) fn();
    return {label, ms: (performance.now() - t) / 5};
  };
  const selectP = phase('select', () => {
    replica.frameFromCache(p.render, p.choice.depth, meta.selection.kMaxMarks);
  });
  const f = replica.frameFromCache(p.render, p.choice.depth, meta.selection.kMaxMarks);
  // The inconsistency to hunt: coarse stand-in bands present while the plan says nothing is novel,
  // which is the client believing a provisional patch is the final answer.
  const visNovel = replica.novelIn(p.visible.rect, p.choice.depth, meta.selection.kMaxMarks);
  if (f.fallback.length > 0 || visNovel > 0) {
    console.log(
      `      [step ${step}] d=${p.choice.depth} fallbackBands=${f.fallback.length} ` +
        `novel(visible)=${visNovel} novel(render)=${f.plan.novel}`
    );
  }
  const asmNoCols = phase('asm(0col)', () => void assemble(f, []));
  const asmCols = phase('asm(1col)', () => void assemble(f, ['primary_category']));
  const a = assemble(f, ['primary_category']);
  const colourP = phase('colour', () =>
    void buildColourAttribute(a.ids.length, a.scalars, {kind: 'uniform'})
  );
  const redrawMs = selectP.ms + asmCols.ms + colourP.ms;
  if (step === STEPS - 1) {
    console.log(
      `\n  breakdown at ${a.ids.length} marks / ${replica.bandCount} bands: ` +
        `select ${selectP.ms.toFixed(1)} | assemble ${asmCols.ms.toFixed(1)} ` +
        `(without scalars ${asmNoCols.ms.toFixed(1)}) | colour ${colourP.ms.toFixed(1)} ms`
    );
  }

  // The anticipatory ring, as the viewer schedules it: the nearest band with novel work, one
  // bounded bite, up to a budget per still period.
  if (process.argv.includes('--ring')) {
    let spent = 0;
    for (let bite = 0; bite < arg('bites', 3); bite++) {
      if (spent >= arg('bitebytes', 2_000_000)) break;
      const band = p.background.find(
        (b) => replica.novelIn(b.rect, b.depth, meta.selection.kMaxMarks) > 0
      );
      if (!band) break;
      const b0 = bytes;
      await replica.fetchRegion(band.rect, band.depth, meta.selection.kMaxMarks, undefined, undefined, 1);
      spent += bytes - b0;
    }
  }
  visibleInView = frame.exact.reduce((n, b) => n + Number(b.visible), 0) || visibleInView;

  console.log(
    `${String(step).padStart(4)} | ${String(requests - r0).padStart(4)} | ` +
      `${(wireMs - w0).toFixed(0).padStart(8)} | ${(absorbMs - a0).toFixed(1).padStart(6)} | ` +
      `${redrawMs.toFixed(1).padStart(6)} | ` +
      `${String(replica.bandCount).padStart(6)} | ` +
      `${String(replica.points).padStart(8)} | ${String(drawn).padStart(6)} | ` +
      `${((bytes - b0) / 1e6).toFixed(2)} MB`
  );
}
console.log(`total: ${requests} requests, ${(bytes / 1e6).toFixed(1)} MB, ${wireMs.toFixed(0)} ms wire+decode`);
