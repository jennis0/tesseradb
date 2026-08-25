import {describe, expect, it} from 'vitest';
import {TesseraError} from '../src/client.js';
import type {Clock} from '../src/driver.js';
import {Presenter, defaultFrameScheduler, type FrameScheduler, type Presented} from '../src/presented.js';
import {Replica} from '../src/replica.js';
import type {Quantisation, ViewportResponse, ViewportResult} from '../src/types.js';

/**
 * The presented frame under a fake clock and a fake frame scheduler — the two injected clocks, so
 * the whole path from a driver verdict to a held composition runs in node.
 */

const Q: Quantisation = {xMin: 0, xMax: 1, yMin: 0, yMax: 1};

function servedResponse(n: number): ViewportResponse {
  const result: ViewportResult = {
    // A large visible count, so the budget does not read the response as saturation and move
    // the depth under the test — what is under test is the frame, not the calibration.
    tiles: [{tile: 0n, visible: 10_000_000n, matched: 10_000_000n, served: BigInt(n)}],
    ids: BigUint64Array.from({length: n}, (_, i) => BigInt(i + 1)),
    codes: BigUint64Array.from({length: n}, () => 0n),
    positions: Float64Array.from({length: n * 2}, () => 1),
    world: Float32Array.from({length: n * 2}, () => 0.1),
    scalars: {},
    subCells: null,
    membership: {},
    artifacts: []
  };
  return {
    result,
    timings: {serverUs: 0, admissionUs: 0, stageNs: null},
    identityKey: 'ik',
    contentKey: 'ck',
    pin: 'ck',
    stale: false,
    bytes: 0
  };
}

function fakeClock(): Clock & {advance(ms: number): Promise<void>} {
  let now = 0;
  let seq = 0;
  const timers = new Map<number, {at: number; fire: () => void}>();
  const drain = async () => {
    for (let i = 0; i < 8; i++) await Promise.resolve();
  };
  return {
    now: () => now,
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

/** A scheduler whose ticks fire only when the test says so — the paint, under test control. */
function fakeScheduler(): FrameScheduler & {tick(): void; pending: number} {
  const queue = new Map<number, () => void>();
  let seq = 0;
  return {
    request(fire) {
      const id = ++seq;
      queue.set(id, fire);
      return id;
    },
    cancel(handle) {
      queue.delete(handle as number);
    },
    tick() {
      const fires = [...queue.values()];
      queue.clear();
      for (const fire of fires) fire();
    },
    get pending() {
      return queue.size;
    }
  };
}

function harness() {
  const clock = fakeClock();
  const scheduler = fakeScheduler();
  const presented: Presented[] = [];
  const statuses: string[] = [];
  let refusal: {code: string; detail: string} | null = null;
  const failWith = {error: null as Error | null};
  const replica = new Replica(
    async () => {
      if (failWith.error) throw failWith.error;
      return servedResponse(3);
    },
    Q,
    {view: 's', now: () => clock.now(), revalidateAfterMs: Infinity}
  );
  const presenter = new Presenter(
    replica,
    {kMaxMarks: 500, maxTilesPerRequest: 4096, thetaTargetMarks: 10},
    clock,
    scheduler,
    {
      onPresented: (p) => presented.push(p),
      onStatus: (status, r) => {
        statuses.push(status);
        refusal = r;
      }
    },
    {},
    false
  );
  const view = {target: [0.5, 0.5, 0] as [number, number, number], zoom: 3};
  return {clock, scheduler, presenter, presented, statuses, refusal: () => refusal, view, replica, failWith};
}

describe('the presented frame', () => {
  it('derives the composition the driver hands over, once the scheduler ticks', async () => {
    const h = harness();
    h.presenter.schedule(h.view, 400, 300);
    // The fetch, then the settle's derive.
    await h.clock.advance(600);
    // The verdict is queued for the paint, not applied on the driver's thread of control.
    expect(h.presenter.frame).toBeNull();
    expect(h.scheduler.pending).toBe(1);
    h.scheduler.tick();
    expect(h.presenter.frame).not.toBeNull();
    expect(h.presenter.frame!.exactDrawn).toBe(3);
    expect(h.presenter.frame!.exactServed).toBe(3);
    expect(h.presented).toHaveLength(1);
    expect(h.presented[0]!.tier).toBe('derive');
    expect(h.presenter.currentStatus).toBe('shown');
  });

  it('never lets a fold replace a pending derive', async () => {
    const h = harness();
    h.presenter.schedule(h.view, 400, 300);
    await h.clock.advance(600);
    // A fold arriving while a derive is queued rides along with it: the derive is what the
    // driver's handle now describes, and dropping it would leave the screen behind the handle.
    (h.presenter as unknown as {apply(v: unknown): void}).apply({
      tier: 'fold',
      plan: h.presented[0]?.plan
    });
    expect(h.scheduler.pending).toBe(1);
    h.scheduler.tick();
    expect(h.presented).toHaveLength(1);
    expect(h.presented[0]!.tier).toBe('derive');
  });

  it('drops a fold that has nothing drawn to fold into', () => {
    const h = harness();
    (h.presenter as unknown as {apply(v: unknown): void}).apply({tier: 'fold', plan: {}});
    h.scheduler.tick();
    expect(h.presented).toHaveLength(0);
    expect(h.presenter.frame).toBeNull();
  });

  it('reports a refusal by code and drops the frame, keeping the replica', async () => {
    const h = harness();
    h.presenter.schedule(h.view, 400, 300);
    await h.clock.advance(600);
    h.scheduler.tick();
    expect(h.presenter.frame).not.toBeNull();
    const heldBands = h.replica.bandCount;

    // Ground nothing is held for, so the next request cannot be answered from the replica.
    h.failWith.error = new TesseraError(403, 'expired-token', 'gone');
    h.presenter.schedule({target: [400, 400, 0], zoom: 8}, 400, 300);
    await h.clock.advance(1000);
    expect(h.presenter.currentStatus).toBe('refused');
    expect(h.refusal()).toEqual({code: 'expired-token', detail: 'gone'});
    expect(h.presenter.frame).toBeNull();
    expect(h.replica.bandCount).toBe(heldBands);
  });

  it('cancel drops the frame and the queued paint', async () => {
    const h = harness();
    h.presenter.schedule(h.view, 400, 300);
    await h.clock.advance(600);
    expect(h.scheduler.pending).toBe(1);
    h.presenter.cancel();
    expect(h.scheduler.pending).toBe(0);
    h.scheduler.tick();
    expect(h.presented).toHaveLength(0);
    expect(h.presenter.frame).toBeNull();
  });

  it('falls back to a timeout where there is no animation frame', () => {
    // Node has no `requestAnimationFrame`; the default must still be a scheduler.
    const scheduler = defaultFrameScheduler();
    const handle = scheduler.request(() => {});
    scheduler.cancel(handle);
    expect(handle).toBeDefined();
  });
});
