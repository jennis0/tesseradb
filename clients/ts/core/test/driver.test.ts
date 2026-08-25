import {describe, expect, it} from 'vitest';
import {Driver, type Clock} from '../src/driver.js';
import {Replica} from '../src/replica.js';
import {TesseraError} from '../src/client.js';
import type {Quantisation, ViewportResponse, ViewportResult} from '../src/types.js';

/**
 * The driver under a fake clock — the tests the old shape could not have.
 *
 * The first three pin the live defects the 2026-08-10 review found in the timer-soup controller:
 * a staleness bound the covered path starved, anticipation budgets nothing ever re-armed, and a
 * retry `setTimeout` that survived `cancel()`. Each was invisible precisely because scheduling
 * lived in the viewer where no clock could be injected.
 */

const Q: Quantisation = {xMin: 0, xMax: 1, yMin: 0, yMax: 1};

function emptyResponse(pin = 'p1'): ViewportResponse {
  const result: ViewportResult = {
    tiles: [],
    ids: new BigUint64Array(0),
    codes: new BigUint64Array(0),
    positions: new Float64Array(0),
    world: new Float32Array(0),
    scalars: {},
    subCells: null,
    artifacts: []
  };
  return {
    result,
    timings: {serverUs: 0, admissionUs: 0, stageNs: null},
    identityKey: 'ik',
    contentKey: pin,
    pin,
    stale: false,
    bytes: 0
  };
}

/**
 * One served point in tile (0,0) with a large visible count. An all-empty response reports
 * `visibleInView = 0`, which reads as saturation and pins every later depth choice at the floor —
 * so any test about the budget's depth arithmetic needs the planner to stay unsaturated.
 */
function servedResponse(visible: bigint): ViewportResponse {
  const result: ViewportResult = {
    tiles: [{tile: 0n, visible, matched: visible, served: 1n}],
    ids: new BigUint64Array([1n]),
    codes: new BigUint64Array([1n]),
    positions: new Float64Array([1, 1]),
    world: new Float32Array([0.1, 0.1]),
    scalars: {},
    subCells: null,
    artifacts: []
  };
  return {
    result,
    timings: {serverUs: 0, admissionUs: 0, stageNs: null},
    identityKey: 'ik',
    contentKey: 'p1',
    pin: 'p1',
    stale: false,
    bytes: 0
  };
}

function fakeClock(): Clock & {advance(ms: number): Promise<void>; t(): number} {
  let now = 0;
  let seq = 0;
  const timers = new Map<number, {at: number; fire: () => void}>();
  const drain = async () => {
    for (let i = 0; i < 8; i++) await Promise.resolve();
  };
  return {
    now: () => now,
    t: () => now,
    after(ms, fire) {
      const id = ++seq;
      timers.set(id, {at: now + ms, fire});
      return id;
    },
    cancel(handle) {
      timers.delete(handle as number);
    },
    async advance(ms) {
      const target = now + ms;
      for (;;) {
        let nextId = -1;
        for (const [id, t] of timers) {
          if (t.at <= target && (nextId < 0 || t.at < timers.get(nextId)!.at)) nextId = id;
        }
        if (nextId < 0) break;
        const t = timers.get(nextId)!;
        timers.delete(nextId);
        now = t.at;
        t.fire();
        await drain();
      }
      now = target;
      await drain();
    }
  };
}

function harness(opts: {
  revalidateAfterMs?: number;
  fail?: (call: number) => boolean;
  hang?: (call: number, k?: number) => boolean;
  prefetch?: boolean;
  respond?: () => ViewportResponse;
} = {}) {
  const clock = fakeClock();
  const calls: {zoom: number; k?: number; background?: boolean}[] = [];
  const traces: {kind: string; fields: Record<string, number>}[] = [];
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
    {kMaxMarks: 500, maxTilesPerRequest: 4096, thetaTargetMarks: 10},
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
  it('revalidates through the covered path once the interval lapses — the bound is reachable at a warm cache', async () => {
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

    // Past the interval, the covered pan itself carries the counts-only refresh.
    await h.clock.advance(120_000);
    h.driver.schedule(h.view, 400, 300);
    await h.clock.advance(1_000);
    expect(h.traces.some((t) => t.kind === 'covered')).toBe(true);
    expect(h.calls.length).toBeGreaterThan(warm);
    expect(h.traces.some((t) => t.kind === 'revalidate')).toBe(true);
  });

  it('takes up to three anticipation bites per pause — deferral re-arms instead of discarding', async () => {
    const h = harness();
    h.driver.schedule(h.view, 400, 300);
    await h.clock.advance(2_000); // request settles, idle fires, bites chain
    const rings = h.traces.filter((t) => t.kind === 'ring').length;
    const budgetStops = h.traces.filter((t) => t.kind === 'ringskip' && t.fields.why === 3).length;
    const noNovel = h.traces.filter((t) => t.kind === 'ringskip' && t.fields.why === 5).length;
    // Either the ring runs to its bite budget, or it genuinely exhausted the novel ground first —
    // both are the designed behaviours; firing once and stopping for the pause is the defect.
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
    // The arrival that cleared the slot re-evaluated eligibility: anticipation ran after all.
    expect(h.calls.some((c) => c.background)).toBe(true);
  });

  it('cancel() kills a pending retry — no request fires after a principal switch', async () => {
    const h = harness({fail: () => true});
    h.driver.schedule(h.view, 400, 300);
    await h.clock.advance(10);
    const before = h.calls.length;
    expect(before).toBeGreaterThan(0);
    h.driver.cancel();
    await h.clock.advance(30_000);
    expect(h.calls.length).toBe(before);
  });

  it('retries a 503 not-ready on a short backoff and recovers — a starting server is not a refusal', async () => {
    // The built driver retried only 429; a 503 gave up at once, turning an unready bundle into a
    // refusal the client could not recover from.
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
    await clock.advance(250); // first backoff → second attempt, fails
    await clock.advance(500); // second backoff → third attempt, succeeds
    // Three attempts in all — two 503s and the success — never a refusal.
    expect(call).toBeGreaterThanOrEqual(3); // two 503s, then a success (plus the margin leg)
    expect(statuses).not.toContain('refused');
  });

  it('a gesture pays the full derivation at most once per gap — the walk never runs per frame', async () => {
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
    // A zoom: depth changes every emission, so the covered test fails on each one — the shape
    // that ran the stand-in walk at animation-frame rate before the reconciler existed.
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

  it('an unchanged store under a covering frame reuses — no verdict reaches the consumer', async () => {
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

  it('a due revalidation never displaces a live fetch — latency-neutral by construction', async () => {
    const h = harness({revalidateAfterMs: 1, hang: () => true});
    h.driver.schedule(h.view, 400, 300);
    await h.clock.advance(5_000); // interval long lapsed; foreground still hung
    h.driver.schedule(h.view, 400, 300);
    await h.clock.advance(10);
    expect(h.calls.filter((c) => c.k === 0)).toHaveLength(0);
  });

  it('a real fetch never queues behind a running revalidation — it displaces it', async () => {
    // Review finding 3: the refresh in the foreground slot made a warm-cache pan wait out a
    // full counting pass. It has its own slot now, and a request aborts it on sight.
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
    // Review finding 1: nothing released the hold, so the derive it forced wrote the held depth
    // back into `presented` and banked calibration was inert. The mechanics pinned here: settle
    // sets the bank, the next schedule consumes it into a one-shot suspension, and the derive
    // that adopts a depth re-arms the hold. (The end-to-end density-boundary test needs an
    // in-bbox serving fixture — a recorded follow-up, not a substitute for this.)
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

  it('a budget change replans at its depth on the very next schedule — no motion, no settle', async () => {
    // The budget was frozen into the options at construction, so the viewer's density control
    // wrote a store field no plan ever read again — the map's density could not be reduced at
    // all mid-session. Two mechanics are pinned together: `setBudget` reaches the next plan,
    // and it suspends the depth hold, which would otherwise pin a one-step depth change to the
    // presented depth and turn the covered path's early return into "the control does nothing".
    const h = harness({prefetch: false, respond: () => servedResponse(10_000_000n)});
    h.driver.schedule(h.view, 400, 300);
    await h.clock.advance(600); // fetch, calibration, settle → presented at the budget's depth
    // Same view again: covered, and the settle's banked suspension is consumed and re-armed.
    h.driver.schedule(h.view, 400, 300);
    await h.clock.advance(50);
    expect(h.calls.length).toBeGreaterThan(0);
    expect(h.calls.every((c) => c.zoom === 10)).toBe(true);

    // At this harness's view the default budget chooses depth 10 and 2,000 chooses depth 9 —
    // one step, exactly what the un-suspended hold would defer.
    h.driver.setBudget(2_000);
    h.driver.schedule(h.view, 400, 300);
    await h.clock.advance(1_000);
    expect(h.calls.some((c) => c.zoom === 9)).toBe(true);
  });
});
