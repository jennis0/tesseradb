import {describe, expect, it} from 'vitest';
import {Driver} from '../src/driver.js';
import {Replica} from '../src/replica.js';
import {TesseraError} from '../src/client.js';
import type {Quantisation, ViewportResponse} from '../src/types.js';
import {mortonOfTile} from '../src/coords.js';
import {fakeClock, response, result, tile} from './support.js';

/**
 * The driver under a fake clock: the staleness bound is reachable from the covered path,
 * anticipation budgets re-arm, and a retry timer does not outlive `cancel()`.
 */

const Q: Quantisation = {xMin: 0, xMax: 1, yMin: 0, yMax: 1};

function emptyResponse(pin = 'p1'): ViewportResponse {
  return response(result(), {contentKey: pin});
}

/**
 * A block of saturated tiles, prefixes `0 … n-1` (for `n = 8`, tiles (0..3, 0..1) at any depth),
 * each carrying one served point and a visible count far above the cap.
 *
 * An empty response reports no counts and `visibleInView = 0`, which reads as saturation and pins
 * the depth at the floor, so tests of the depth arithmetic need ground. A capped tile costs `k`
 * wherever it sits, so the arithmetic follows from the tile layout alone.
 */
function servedResponse(visible: bigint, tiles = 1): ViewportResponse {
  const prefixes = Array.from({length: tiles}, (_, i) => BigInt(i));
  return response(
    result({
      tiles: prefixes.map((p) => tile(p, visible, {served: 1n})),
      ids: BigUint64Array.from(prefixes.map((p) => p + 1n)),
      codes: BigUint64Array.from(prefixes.map((p) => p + 1n)),
      positions: Float64Array.from(prefixes.flatMap(() => [1, 1])),
      world: Float32Array.from(prefixes.flatMap(() => [0.1, 0.1]))
    }),
    {contentKey: 'p1'}
  );
}

function harness(opts: {
  revalidateAfterMs?: number;
  fail?: (call: number) => boolean;
  hang?: (call: number, k?: number) => boolean;
  prefetch?: boolean;
  respond?: () => ViewportResponse;
  /** Serve as under a filter: every match up to `k`, with no thinning. */
  filtered?: boolean;
} = {}) {
  const clock = fakeClock();
  const calls: {zoom: number; k?: number; background?: boolean}[] = [];
  const traces: {kind: string; fields: Record<string, number | string>}[] = [];
  const hung: (() => void)[] = [];
  const replica = new Replica(
    async (req, _signal, background) => {
      const n = calls.length;
      calls.push({zoom: req.zoom, k: req.k, background});
      if (opts.hang?.(n, req.k)) await new Promise<void>((resolve) => hung.push(resolve));
      if (opts.fail?.(n)) throw new TesseraError(429, 'shed', 'saturated');
      return opts.respond ? opts.respond() : emptyResponse();
    },
    Q,
    {view: 's', now: () => clock.now(), revalidateAfterMs: opts.revalidateAfterMs ?? Infinity}
  );
  replica.reset();
  const driver = new Driver(
    replica,
    {kMaxMarks: 500, maxTilesPerRequest: 4096, thetaTargetMarks: 10, filtered: () => opts.filtered ?? false},
    clock,
    {
      onFrame: () => {},
      onTrace: (kind, fields) => traces.push({kind, fields})
    },
    {},
    opts.prefetch ?? true
  );
  const view = {target: [0.5, 0.5, 0] as [number, number, number], zoom: 3};
  return {clock, calls, traces, driver, replica, view, hung};
}

describe('driver', () => {
  it('revalidates through the covered path once the interval lapses: the bound is reachable at a warm cache', async () => {
    const h = harness({revalidateAfterMs: 60_000});
    h.driver.schedule(h.view, 400, 300);
    await h.clock.advance(1);
    const cold = h.calls.length;
    expect(cold).toBeGreaterThan(0);

    // Warm: the same view again is covered and issues nothing new.
    await h.clock.advance(5_000);
    h.driver.schedule(h.view, 400, 300);
    await h.clock.advance(1_000);
    const warm = h.calls.length;

    // Past the interval, the covered pan carries the count-only refresh.
    await h.clock.advance(120_000);
    h.driver.schedule(h.view, 400, 300);
    await h.clock.advance(1_000);
    expect(h.traces.some((t) => t.kind === 'covered')).toBe(true);
    expect(h.calls.length).toBeGreaterThan(warm);
    expect(h.traces.some((t) => t.kind === 'revalidate')).toBe(true);
  });

  it('takes up to three anticipation bites per pause: deferral re-arms instead of discarding', async () => {
    const h = harness();
    h.driver.schedule(h.view, 400, 300);
    await h.clock.advance(2_000); // request settles, idle fires, bites chain
    const rings = h.traces.filter((t) => t.kind === 'ring').length;
    const budgetStops = h.traces.filter((t) => t.kind === 'ringskip' && t.fields.why === 3).length;
    const noNovel = h.traces.filter((t) => t.kind === 'ringskip' && t.fields.why === 5).length;
    // The ring runs to its per-pause allowance or exhausts the new ground first; firing once and
    // stopping would be wrong.
    expect(rings + noNovel).toBeGreaterThan(1);
    if (noNovel === 0) expect(rings === 3 ? budgetStops : rings).toBeGreaterThan(0);
  });

  it('a bite is deferred while the foreground flies, and follows its completion', async () => {
    const h = harness({hang: (n) => n === 0});
    h.driver.schedule(h.view, 400, 300);
    await h.clock.advance(300); // idle fires while the first fetch hangs
    expect(h.traces.filter((t) => t.kind === 'ring')).toHaveLength(0);
    expect(h.traces.some((t) => t.kind === 'ringskip' && t.fields.why === 1)).toBe(true);
    while (h.hung.length > 0) h.hung.shift()!();
    await h.clock.advance(2_000);
    // The arrival that cleared the slot re-evaluated eligibility, and anticipation ran.
    expect(h.calls.some((c) => c.background)).toBe(true);
  });

  it('cancel() kills a pending retry: no request fires after a principal switch', async () => {
    const h = harness({fail: () => true});
    h.driver.schedule(h.view, 400, 300);
    await h.clock.advance(10);
    const before = h.calls.length;
    expect(before).toBeGreaterThan(0);
    h.driver.cancel();
    await h.clock.advance(30_000);
    expect(h.calls.length).toBe(before);
  });

  it('retries a 503 not-ready on a short backoff and recovers: a starting server is not a refusal', async () => {
    // A 503 from a server still starting is retried like a 429.
    const statuses: string[] = [];
    const clock = fakeClock();
    let call = 0;
    const replica = new Replica(
      async () => {
        call++;
        if (call <= 2) throw new TesseraError(503, 'not-ready', 'unverified bundle');
        return emptyResponse();
      },
      Q,
      {view: 's', now: () => clock.now(), revalidateAfterMs: Infinity}
    );
    replica.reset();
    const driver = new Driver(
      replica,
      {kMaxMarks: 500, maxTilesPerRequest: 4096, thetaTargetMarks: 10},
      clock,
      {onFrame: () => {}, onStatus: (s) => statuses.push(s)},
      {notReadyBackoffMs: 250},
      false
    );
    driver.schedule({target: [0.5, 0.5, 0], zoom: 3}, 400, 300);
    await clock.advance(10); // first attempt fails
    expect(statuses).toContain('retrying');
    await clock.advance(250); // first backoff, second attempt fails
    await clock.advance(500); // second backoff, third attempt succeeds
    expect(call).toBeGreaterThanOrEqual(3); // two 503s, then a success (plus the margin leg)
    expect(statuses).not.toContain('refused');
  });

  it('a gesture pays the full derivation at most once per gap: the walk never runs per frame', async () => {
    const h = harness();
    const derives: number[] = [];
    const driver = new Driver(
      h.replica,
      {kMaxMarks: 500, maxTilesPerRequest: 4096, thetaTargetMarks: 10},
      h.clock,
      {
        onFrame: (v) => {
          if (v.tier === 'derive') derives.push(h.clock.now());
        }
      }
    );
    // A zoom: the depth changes on every emission, so the covered test fails on each one.
    for (let i = 0; i < 12; i++) {
      driver.schedule({target: [0.5, 0.5, 0], zoom: 3 + i * 0.2}, 400, 300);
      await h.clock.advance(16);
    }
    for (let i = 1; i < derives.length; i++) {
      // The settle's finalising derive is exempt; consecutive gesture derives are not.
      expect(derives[i]! - derives[i - 1]!).toBeGreaterThanOrEqual(119);
    }
    expect(derives.length).toBeGreaterThan(0);
    expect(derives.length).toBeLessThan(6);
  });

  it('an unchanged store under a covering frame reuses: no verdict reaches the consumer', async () => {
    const h = harness();
    const verdicts: string[] = [];
    const driver = new Driver(
      h.replica,
      {kMaxMarks: 500, maxTilesPerRequest: 4096, thetaTargetMarks: 10},
      h.clock,
      {
        onFrame: (v) => verdicts.push(v.tier),
        onTrace: (kind) => verdicts.push(kind === 'reuse' ? 'reuse' : `(${kind})`)
      }
    );
    const view = {target: [0.5, 0.5, 0] as [number, number, number], zoom: 3};
    driver.schedule(view, 400, 300);
    await h.clock.advance(3_000); // fetch, settle, stillness
    const before = verdicts.filter((v) => v === 'fold' || v === 'derive').length;
    driver.schedule(view, 400, 300); // identical view, untouched store
    await h.clock.advance(50);
    expect(verdicts.filter((v) => v === 'fold' || v === 'derive').length).toBe(before);
  });

  it('a due revalidation never displaces a live fetch: latency-neutral', async () => {
    const h = harness({revalidateAfterMs: 1, hang: () => true});
    h.driver.schedule(h.view, 400, 300);
    await h.clock.advance(5_000); // interval long lapsed; foreground still hung
    h.driver.schedule(h.view, 400, 300);
    await h.clock.advance(10);
    // The cold view's count seed is the first `k = 0` call (it hangs here too); a second, a
    // revalidation beside a live fetch, must not appear.
    expect(h.calls.slice(1).filter((c) => c.k === 0)).toHaveLength(0);
  });

  it('a real fetch never queues behind a running revalidation: it displaces it', async () => {
    // The refresh has its own slot, and a request aborts it, so a pan does not wait behind a
    // counting pass.
    const h = harness({revalidateAfterMs: 1, hang: (_n, k) => k === 0});
    h.driver.schedule(h.view, 400, 300);
    await h.clock.advance(3_000); // cold fetch settles; revalidation becomes due
    h.driver.schedule(h.view, 400, 300); // covered pan starts the (hanging) k=0 refresh
    await h.clock.advance(100);
    const before = h.calls.length;
    // A jump to novel ground must go straight to the wire, not into the queued slot.
    h.driver.schedule({target: [400, 400, 0], zoom: 6}, 400, 300);
    await h.clock.advance(600);
    expect(h.calls.length).toBeGreaterThan(before);
  });

  it('a settle readies the bank and the next motion suspends the depth-hold once', async () => {
    // The settle sets the bank, the next schedule consumes it into a one-shot suspension, and the
    // derive that adopts a depth re-arms the hold.
    const h = harness();
    const d = h.driver as unknown as {bankReady: boolean; holdSuspended: boolean};
    h.driver.schedule(h.view, 400, 300);
    await h.clock.advance(3_000); // fetch + settle
    expect(d.bankReady).toBe(true);
    h.driver.schedule({target: [0.6, 0.5, 0], zoom: h.view.zoom}, 400, 300);
    expect(d.bankReady).toBe(false);
    await h.clock.advance(3_000); // the motion resolves at some tier; the hold re-arms
    expect(d.holdSuspended).toBe(false);
  });

  it('a budget change replans at its depth on the very next schedule: no motion, no settle', async () => {
    // `setBudget` reaches the next plan and suspends the depth hold, which would otherwise keep a
    // one-step depth change at the presented depth.
    const h = harness({prefetch: false, filtered: true, respond: () => servedResponse(10_000_000n, 8)});
    h.driver.schedule(h.view, 400, 300);
    await h.clock.advance(600); // fetch, calibration, settle: presented at the budget's depth
    // Same view again: covered, and the settle's banked suspension is consumed and re-armed.
    h.driver.schedule(h.view, 400, 300);
    await h.clock.advance(50);
    expect(h.calls.length).toBeGreaterThan(0);
    expect(h.calls.every((c) => c.zoom === 10)).toBe(true);

    // At this view the default budget chooses depth 10 and 2,000 chooses depth 9: the eight capped
    // tiles cost 8 x k = 4,000 marks at depth 10 and fold into two tiles, 1,000 marks, at depth 9.
    h.driver.setBudget(2_000);
    h.driver.schedule(h.view, 400, 300);
    await h.clock.advance(1_000);
    expect(h.calls.some((c) => c.zoom === 9)).toBe(true);
  });
  it('a redraw asks for nothing, however uncovered it is: a view stepped through is silent', async () => {
    // The settle asks for a depth the replica holds nothing at, and `redraw` reconciles on the
    // settle's terms, so a view switch's redraw must stay silent while a settle asks.
    const h = harness({prefetch: false, respond: () => servedResponse(10_000_000n, 8)});
    h.driver.schedule(h.view, 400, 300);
    await h.clock.advance(3_000);
    const settled = h.calls.length;
    h.driver.setBudget(2_000);
    h.driver.redraw(h.view, 400, 300);
    await h.clock.advance(3_000);
    expect(h.calls.length).toBe(settled);
  });

  it('the cold view buys counts before marks: the first marks request is at the counted depth, not the average model\'s', async () => {
    // With no counts, the first view of a session would be planned by the average model; the
    // count-only seed plans it from counts.
    const clock = fakeClock();
    const calls: {zoom: number; k?: number}[] = [];
    const traces: {kind: string; fields: Record<string, number | string>}[] = [];
    const replica = new Replica(
      async (req) => {
        calls.push({zoom: req.zoom, k: req.k});
        if (req.k !== 0) return servedResponse(10_000n, 8);
        // Every tile saturated, so a filtered request costs `k` per tile and the budget bounds the
        // choice.
        const n = Math.min(4 ** req.zoom, 4096);
        const result = emptyResponse().result;
        return {
          ...emptyResponse(),
          result: {
            ...result,
            tiles: Array.from({length: n}, (_, i) => tile(BigInt(i), 10_000n))
          }
        };
      },
      Q,
      {view: 's', now: () => clock.now(), revalidateAfterMs: Infinity}
    );
    replica.reset();
    const driver = new Driver(
      replica,
      {kMaxMarks: 500, maxTilesPerRequest: 4096, thetaTargetMarks: 10, filtered: () => true},
      clock,
      {onFrame: () => {}, onTrace: (kind, fields) => traces.push({kind, fields})},
      {budget: 50_000},
      false
    );
    driver.schedule({target: [0.5, 0.5, 0], zoom: 3}, 400, 300);
    await clock.advance(50);

    // The seed is first, and it asks for counts only.
    expect(calls[0]!.k).toBe(0);
    expect(traces.some((t) => t.kind === 'seed')).toBe(true);
    const marks = calls.find((c) => c.k !== 0);
    expect(marks).toBeDefined();
    // The marks request was planned from those counts, not from the average.
    const request = traces.find((t) => t.kind === 'request')!;
    expect(request.fields.from).not.toBe('average');
    // Saturated ground: `sum min(k, count)` under a 50,000 budget admits 100 tiles, shallower than
    // the average model's choice at m_target = 10.
    expect(marks!.zoom).toBeLessThan(calls[0]!.zoom);
    expect(request.fields.depth).toBe(marks!.zoom);

    // Once per session: a second view asks for marks with no seed in front of it.
    const seeds = () => calls.filter((c) => c.k === 0).length;
    const seedsAfterFirst = seeds();
    driver.schedule({target: [0.1, 0.9, 0], zoom: 6}, 400, 300);
    await clock.advance(2_000);
    expect(seeds()).toBe(seedsAfterFirst);
  });

  it('a refused seed is not a refused view: the marks request the average model planned still goes', async () => {
    const clock = fakeClock();
    const calls: {zoom: number; k?: number}[] = [];
    const traces: {kind: string; fields: Record<string, number | string>}[] = [];
    const replica = new Replica(
      async (req) => {
        calls.push({zoom: req.zoom, k: req.k});
        if (req.k === 0) throw new TesseraError(500, 'internal', 'no counts for you');
        return servedResponse(10_000n, 8);
      },
      Q,
      {view: 's', now: () => clock.now(), revalidateAfterMs: Infinity}
    );
    replica.reset();
    const statuses: string[] = [];
    const driver = new Driver(
      replica,
      {kMaxMarks: 500, maxTilesPerRequest: 4096, thetaTargetMarks: 10},
      clock,
      {onFrame: () => {}, onStatus: (s) => statuses.push(s), onTrace: (kind, fields) => traces.push({kind, fields})},
      {budget: 50_000},
      false
    );
    driver.schedule({target: [0.5, 0.5, 0], zoom: 3}, 400, 300);
    await clock.advance(50);
    expect(traces.some((t) => t.kind === 'seedfail')).toBe(true);
    expect(calls.some((c) => c.k !== 0)).toBe(true);
    expect(statuses).not.toContain('refused');
  });

  it('an unfiltered corpus far below the cap is drawn whole as the view zooms in, where the cap alone stopped short', async () => {
    // 65,536 members, one per depth-8 cell over the whole map, and a server that thins as the real
    // one does: a tile of n serves θ·n with θ = 10 · occupied(d) / 65,536, at least 2 and at most k.
    // Every depth below 7 serves 10 marks a tile; from depth 7, where θ passes 1, every member.
    const clock = fakeClock();
    const asked: {zoom: number; k?: number}[] = [];
    const total = 4 ** 8;
    const replica = new Replica(
      async (req) => {
        asked.push({zoom: req.zoom, k: req.k});
        const z = req.zoom;
        const side = 2 ** z;
        const [x0, y0, x1, y1] = req.bbox!;
        const at = (v: number) => Math.min(side - 1, Math.floor(v * side));
        const n = total / 4 ** z;
        const theta = Math.min(1, (10 * 4 ** z) / total);
        const tiles = [];
        for (let x = at(x0); x <= at(x1); x++) {
          for (let y = at(y0); y <= at(y1); y++) {
            const served = req.k === 0 ? 0 : Math.min(n, 500, Math.max(Math.min(2, n), Math.floor(theta * n)));
            tiles.push({x, y, served});
          }
        }
        const points = tiles.flatMap((t) => Array.from({length: t.served}, () => [(t.x + 0.5) / side, (t.y + 0.5) / side] as const));
        return response(
          result({
            tiles: tiles.map((t) => tile(mortonOfTile(t.x, t.y, z), BigInt(n), {served: BigInt(t.served)})),
            ids: BigUint64Array.from(points.map((_, i) => BigInt(asked.length * 1_000_000 + i + 1))),
            codes: BigUint64Array.from(points.map(() => 0n)),
            positions: Float64Array.from(points.flat()),
            world: Float32Array.from(points.flatMap(([x, y]) => [x * 512, y * 512]))
          }),
          {contentKey: 'p1'}
        );
      },
      Q,
      {view: 's', now: () => clock.now(), revalidateAfterMs: Infinity}
    );
    replica.reset();
    const driver = new Driver(
      replica,
      {kMaxMarks: 500, maxTilesPerRequest: 65_536, thetaTargetMarks: 10, kMin: 2, filtered: () => false},
      clock,
      {onFrame: () => {}},
      {budget: 500_000},
      false
    );
    const marksDepths = () => asked.filter((c) => c.k !== 0).map((c) => c.zoom);

    driver.schedule({target: [256, 256, 0], zoom: 0}, 512, 512);
    await clock.advance(3_000);
    expect(marksDepths().at(-1)).toBe(7);
    expect(driver.presentedFrame?.depth).toBe(7);

    // Zoomed in on a quarter of the map: still every member, so still depth 7. Read as min(k, n),
    // the 256 members of a depth-4 tile looked served whole and the request stopped at depth 4.
    driver.schedule({target: [128, 128, 0], zoom: 1}, 512, 512);
    await clock.advance(3_000);
    expect(marksDepths().every((d) => d === 7)).toBe(true);
    expect(driver.presentedFrame?.depth).toBe(7);
  });
});
