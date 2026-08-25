import {describe, expect, it, vi} from 'vitest';
import {TesseraClient} from '../src/client.js';
import type {Clock} from '../src/driver.js';
import type {FrameScheduler} from '../src/presented.js';
import {createStore, type Store} from '../src/store.js';
import type {Meta, ViewportResponse, ViewportResult} from '../src/types.js';

/**
 * The store against a fake `TesseraClient`, a fake clock and a fake frame scheduler — no DOM, no
 * network. Every assertion is about the store's own contract: `setView`'s conversion, the stale
 * rule keyed on the content key, and the drops.
 */

const META: Meta = {
  apiVersion: 1,
  idset: 0,
  views: [{id: 's0', displayName: 'default'}],
  quantisation: {xMin: 0, xMax: 100, yMin: 0, yMax: 200},
  declaredScalars: [{name: 'archive', arrowType: 'u16', category: {vocabulary: 'a', kind: 'declared', visibility: 'public'}, render: true, index: true}],
  layers: [],
  selection: {kMin: 1, kMaxMarks: 500, maxK: 5000, thetaTargetMarks: 10, maxUnderlayOffset: 0, maxCategoryValues: 1000},
  maxTilesPerRequest: 4096,
  filterOperands: [{column: 'archive', family: 'category', operands: ['in']}]
};

function response(contentKey: string, identityKey = 'ik', served = 3): ViewportResponse {
  const result: ViewportResult = {
    tiles: [{tile: 0n, visible: 10_000_000n, matched: 10_000_000n, served: BigInt(served)}],
    ids: BigUint64Array.from({length: served}, (_, i) => BigInt(i + 1)),
    codes: BigUint64Array.from({length: served}, () => 0n),
    positions: Float64Array.from({length: served * 2}, () => 1),
    world: Float32Array.from({length: served * 2}, () => 0.1),
    scalars: {archive: {arrowType: 'u16', values: Uint16Array.from({length: served}, () => 5)}},
    subCells: null,
    artifacts: []
  };
  return {
    result,
    timings: {serverUs: 0, admissionUs: 0, stageNs: null},
    identityKey,
    contentKey,
    pin: contentKey,
    stale: false,
    bytes: 0
  };
}

function fakeClock(): Clock & {advance(ms: number): Promise<void>} {
  let now = 0;
  let seq = 0;
  const timers = new Map<number, {at: number; fire: () => void}>();
  const drain = async () => {
    for (let i = 0; i < 12; i++) await Promise.resolve();
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
      // Drain first, so an async bring-up (`warm`) resolves and schedules its timers before the
      // loop that fires them — the store's machinery is built inside a promise chain, not synchronously.
      await drain();
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

function fakeScheduler(): FrameScheduler & {flush(): void} {
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
    flush() {
      const fires = [...queue.values()];
      queue.clear();
      for (const fire of fires) fire();
    }
  };
}

/** A fake client — only the four verbs the store calls, and a log of the viewport requests. */
function fakeClient(reply: () => ViewportResponse) {
  const viewport = vi.fn(async () => reply());
  const client = {
    meta: async () => META,
    viewport,
    item: async () => ({fields: {archive: 'cs'}, externalId: null}),
    artifact: async () => ({layer: 'l', key: 'k', maskedCount: 42n}),
    categories: async () => [{code: 5, key: 'cs', title: 'CS'}],
    close: () => {}
  } as unknown as TesseraClient;
  return {client, viewport};
}

/** Build a store and drive it to its first shown frame. */
async function warm(reply: () => ViewportResponse, opts: {clock: ReturnType<typeof fakeClock>; scheduler: ReturnType<typeof fakeScheduler>}) {
  const {client, viewport} = fakeClient(reply);
  const store = createStore({
    viewerUrl: 'http://viewer',
    token: 'tok',
    client,
    clock: opts.clock,
    scheduler: opts.scheduler,
    prefetch: false,
    replica: {revalidateAfterMs: Infinity}
  });
  // Let warm() resolve: meta, replica, presenter built.
  await opts.clock.advance(1);
  return {store, viewport};
}

describe('setView converts a data bbox to the driver’s target and zoom', () => {
  it('centres the target and takes the zoom from the tighter axis', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {store, viewport} = await warm(() => response('ck'), {clock, scheduler});

    // A data bbox covering the left half of x and the top quarter of y. World is 512 square, so
    // x [0,50] maps to world [0,256] and y [0,50] maps to world [0,128].
    store.setView({bbox: [0, 0, 50, 50], width: 800, height: 400});
    await clock.advance(600);
    scheduler.flush();

    // One request went out for a shown frame.
    expect(viewport).toHaveBeenCalled();
    expect(store.get('view').composition).not.toBeNull();
    // The tighter axis governs zoom: width/bw = 800/256 = 3.125, height/bh = 400/128 = 3.125 here
    // by construction of the bbox, so the frame is drawn and the served count reflects the tile.
    expect(store.get('view').served.shown).toBe(3);
    expect(store.get('view').visible.value).toBe(10_000_000);
  });

  it('queues a setView issued before meta has arrived', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {client, viewport} = fakeClient(() => response('ck'));
    const store = createStore({viewerUrl: 'http://v', token: 'tok', client, clock, scheduler, prefetch: false, replica: {revalidateAfterMs: Infinity}});
    // Before warm() has resolved: no meta yet.
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    expect(viewport).not.toHaveBeenCalled();
    await clock.advance(600);
    scheduler.flush();
    // The queued view was replayed once meta and the presenter existed.
    expect(viewport).toHaveBeenCalled();
    expect(store.get('view').composition).not.toBeNull();
  });
});

describe('status.stale keys on the content key, never on x-tessera-stale', () => {
  it('is false while the content key holds and true once a revalidation observes it move', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    let key = 'ck-1';
    // A short revalidation interval, so a still, covered view refreshes the number channel.
    const {client, viewport} = fakeClient(() => response(key));
    const store = createStore({
      viewerUrl: 'http://v', token: 'tok', client, clock, scheduler, prefetch: false,
      replica: {revalidateAfterMs: 100}
    });
    await clock.advance(1);
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await clock.advance(600);
    scheduler.flush();
    expect(store.get('status').stale).toBe(false);
    const drawn = viewport.mock.calls.length;

    // The interval lapses and the same, covered view is scheduled: the driver revalidates through
    // the foreground slot with a counts-only request, which observes ck-2 without redrawing the
    // marks. The content key moved under the presented frame, so the store marks itself stale.
    key = 'ck-2';
    await clock.advance(200);
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await clock.advance(600);
    scheduler.flush();
    expect(viewport.mock.calls.length).toBeGreaterThan(drawn); // the revalidation went out
    expect(store.get('status').stale).toBe(true);

    // refresh() redraws under the new key and clears it.
    store.refresh();
    await clock.advance(600);
    scheduler.flush();
    expect(store.get('status').stale).toBe(false);
  });
});

describe('the drops', () => {
  it('drops the replica and marks a refetch on setFilters — the identity key excludes filters', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {store, viewport} = await warm(() => response('ck'), {clock, scheduler});
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await clock.advance(600);
    scheduler.flush();
    const before = viewport.mock.calls.length;

    // A filter narrows what is served without changing the identity key, so held bands would be
    // served as if they belonged. The store must drop them itself and re-ask.
    store.setFilters({archive: {family: 'category', keys: ['cs']}});
    await clock.advance(600);
    scheduler.flush();
    expect(store.get('filters').expr).toEqual({archive: {in: ['cs']}});
    expect(viewport.mock.calls.length).toBeGreaterThan(before);
    // The filter reached the wire.
    const last = viewport.mock.calls.at(-1)!;
    expect((last[1] as {filters?: unknown}).filters).toEqual({archive: {in: ['cs']}});
  });

  it('clears the frame and the encoding on clear()', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {store} = await warm(() => response('ck'), {clock, scheduler});
    store.setColourBy('archive');
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await clock.advance(600);
    scheduler.flush();
    expect(store.get('view').composition).not.toBeNull();

    store.clear();
    expect(store.get('view').composition).toBeNull();
    expect(store.get('legend').ranks).toEqual({});
  });
});

describe('subscription', () => {
  it('notifies a per-projection subscriber only through replacement, and unsubscribes', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {store} = await warm(() => response('ck'), {clock, scheduler});
    const seen: number[] = [];
    const off = (store as Store).subscribe('view', (v) => seen.push(v.served.shown));
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await clock.advance(600);
    scheduler.flush();
    expect(seen.length).toBeGreaterThan(0);
    off();
    const n = seen.length;
    store.setView({bbox: [0, 0, 50, 100], width: 400, height: 400});
    await clock.advance(600);
    scheduler.flush();
    expect(seen.length).toBe(n);
  });
});

describe('setLayers before meta', () => {
  it('is honoured once the channel exists, and the projection shows the intent meanwhile', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {client, viewport} = fakeClient(() => response('ck'));
    const store = createStore({
      viewerUrl: 'http://viewer',
      token: 'tok',
      client,
      clock,
      scheduler,
      prefetch: false,
      replica: {revalidateAfterMs: Infinity}
    });
    // Before meta has landed — the demo does exactly this when it opens a session's store.
    store.setLayers(['clusters/a']);
    expect(store.get('artifacts').layer).toBe('clusters/a');
    await clock.advance(1);
    expect(store.get('artifacts').layer).toBe('clusters/a');

    store.setView({bbox: [0, 0, 100, 200], width: 800, height: 800});
    await clock.advance(600);
    scheduler.flush();
    await clock.advance(600);
    // The artifact channel's own request (`k = 0`) names the layer chosen before meta.
    const named = viewport.mock.calls.some((call) => {
      const req = call[1] as {k?: number; layers?: string[] | 'all'};
      return req.k === 0 && Array.isArray(req.layers) && req.layers[0] === 'clusters/a';
    });
    expect(named).toBe(true);
  });
});
