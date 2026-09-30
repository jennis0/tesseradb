import {describe, expect, it} from 'vitest';
import {TesseraError} from '../src/client.js';
import {Presenter, defaultFrameScheduler, type Presented} from '../src/presented.js';
import {Replica} from '../src/replica.js';
import type {Quantisation, ViewportResponse} from '../src/types.js';
import {fakeClock, fakeScheduler, response, servedResult, tile} from './support.js';

/** The presented frame under a fake clock and a fake frame scheduler, so the whole path runs in node. */

const Q: Quantisation = {xMin: 0, xMax: 1, yMin: 0, yMax: 1};

function servedResponse(n: number): ViewportResponse {
  // A large visible count, so the budget does not read saturation and move the depth.
  return response(servedResult(n, [tile(0n, 10_000_000n)]));
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
    h.scheduler.flush();
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
    // A fold arriving while a derive is queued does not replace it: the driver's handle describes the
    // derive.
    (h.presenter as unknown as {apply(v: unknown): void}).apply({
      tier: 'fold',
      plan: h.presented[0]?.plan
    });
    expect(h.scheduler.pending).toBe(1);
    h.scheduler.flush();
    expect(h.presented).toHaveLength(1);
    expect(h.presented[0]!.tier).toBe('derive');
  });

  it('drops a fold that has nothing drawn to fold into', () => {
    const h = harness();
    (h.presenter as unknown as {apply(v: unknown): void}).apply({tier: 'fold', plan: {}});
    h.scheduler.flush();
    expect(h.presented).toHaveLength(0);
    expect(h.presenter.frame).toBeNull();
  });

  it('reports a refusal by code and drops the frame, keeping the replica', async () => {
    const h = harness();
    h.presenter.schedule(h.view, 400, 300);
    await h.clock.advance(600);
    h.scheduler.flush();
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
    h.scheduler.flush();
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
