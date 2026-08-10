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
    subCells: null
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
  hang?: (call: number) => boolean;
} = {}) {
  const clock = fakeClock();
  const calls: {zoom: number; k?: number; background?: boolean}[] = [];
  const traces: {kind: string; fields: Record<string, number>}[] = [];
  const hung: (() => void)[] = [];
  const replica = new Replica(
    async (req, _signal, background) => {
      const n = calls.length;
      calls.push({zoom: req.zoom, k: req.k, background});
      if (opts.hang?.(n)) await new Promise<void>((resolve) => hung.push(resolve));
      if (opts.fail?.(n)) throw new TesseraError(429, 'shed', 'saturated');
      return emptyResponse();
    },
    Q,
    {slice: 's', now: () => clock.now(), revalidateAfterMs: opts.revalidateAfterMs ?? Infinity}
  );
  replica.reset();
  const driver = new Driver(
    replica,
    {kMaxMarks: 500, maxTilesPerRequest: 4096, thetaTargetMarks: 10},
    clock,
    {
      onFrame: () => {},
      onTrace: (kind, fields) => traces.push({kind, fields})
    }
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

  it('a due revalidation never displaces a live fetch — latency-neutral by construction', async () => {
    const h = harness({revalidateAfterMs: 1, hang: () => true});
    h.driver.schedule(h.view, 400, 300);
    await h.clock.advance(5_000); // interval long lapsed; foreground still hung
    h.driver.schedule(h.view, 400, 300);
    await h.clock.advance(10);
    expect(h.calls.filter((c) => c.k === 0)).toHaveLength(0);
  });
});
