import {describe, expect, it, vi} from 'vitest';
import {TesseraClient} from '../src/client.js';
import type {Clock} from '../src/driver.js';
import type {FrameScheduler} from '../src/presented.js';
import {GRID32, gridToWorld, WORLD_SIZE} from '../src/coords.js';
import {outlineOf} from '../../deck/src/layer.js';
import {createStore} from '../src/store.js';
import {artifactBudgetFor} from '../src/artifactBudget.js';
import type {Artifact, Meta, ViewportResponse, ViewportResult} from '../src/types.js';

/**
 * `extentOf` reads the served `box` in the wire's units — 32 bits per axis (contracts §3.2 item
 * 4), the same as `code` — and the outlines read the same box. An artifact at the corpus's far
 * corner is the case that tells a right divisor from a wrong one: under 2^16 its box lands 65,536
 * extents away, and `fit` on it shows nothing.
 */

const META: Meta = {
  apiVersion: 1,
  idset: 0,
  views: [{id: 's0', displayName: 'default'}],
  quantisation: {xMin: 0, xMax: 100, yMin: 0, yMax: 200},
  declaredScalars: [],
  layers: [{name: 'clusters/a', title: 'a', views: ['s0'], membership: 'enumerated', hierarchy: {kind: 'flat', pruneChildren: false}, levels: [], computedContent: ['centroid', 'box'], suppliedContent: [], depsOn: [], version: 1}],
  selection: {kMin: 1, kMaxMarks: 500, maxK: 5000, thetaTargetMarks: 10, maxUnderlayOffset: 0, maxCategoryValues: 1000},
  maxTilesPerRequest: 4096,
  filterOperands: []
};

/** The far corner: the last quarter of the grid on both axes. */
const FAR: Artifact = {
  layer: 'clusters/a',
  tesseraId: 7n,
  key: 'far',
  maskedCount: 10n,
  centroid: [GRID32 * 0.875, GRID32 * 0.875],
  box: [GRID32 * 0.75, GRID32 * 0.75, GRID32 - 1, GRID32 - 1],
  shape: null,
  content: [],
  parentId: null,
  rung: 0
};

function response(): ViewportResponse {
  const result: ViewportResult = {
    tiles: [{tile: 0n, visible: 10n, matched: 10n, served: 1n}],
    ids: BigUint64Array.from([1n]),
    codes: BigUint64Array.from([0n]),
    positions: Float64Array.from([1, 1]),
    world: Float32Array.from([0.1, 0.1]),
    scalars: {},
    subCells: null,
    membership: {},
    artifacts: [FAR]
  };
  return {result, timings: {serverUs: 0, admissionUs: 0, stageNs: null}, identityKey: 'ik', contentKey: 'ck', pin: 'ck', stale: false, bytes: 0};
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
      await drain();
      const target = now + ms;
      for (;;) {
        let nextId = -1;
        for (const [id, t] of timers) if (t.at <= target && (nextId < 0 || t.at < timers.get(nextId)!.at)) nextId = id;
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

describe('extentOf reads the wire box in 32-bit grid units, as the outlines do', () => {
  it('fits an artifact at the far corner inside the corpus extent, and agrees with outlineOf', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const client = {
      meta: async () => META,
      viewport: vi.fn(async () => response()),
      item: async () => ({fields: {}, externalId: null}),
      artifact: async () => ({layer: 'clusters/a', key: 'far', maskedCount: 10n}),
      categories: async () => [],
      close: () => {}
    } as unknown as TesseraClient;
    const store = createStore({viewerUrl: 'http://viewer', token: 'tok', client, clock, scheduler, prefetch: false, replica: {revalidateAfterMs: Infinity}});
    store.setLayers(['clusters/a']);
    await clock.advance(1);
    store.setView({bbox: [0, 0, 100, 200], width: 800, height: 800});
    await clock.advance(600);
    scheduler.flush();
    await clock.advance(600);
    expect(store.get('artifacts').served.map((a) => a.tesseraId)).toEqual([7n]);
    // Both asks — the channel's `k = 0` and the point path's — carry the view's artifact budget.
    const asks = (client.viewport as unknown as {mock: {calls: [string, {k?: number; layers?: string[]; artifactBudget?: number}][]}}).mock.calls.map((c) => c[1]);
    const channel = asks.filter((r) => r.k === 0 && Array.isArray(r.layers) && r.layers.length > 0);
    const points = asks.filter((r) => r.k !== 0);
    expect(channel.length).toBeGreaterThan(0);
    expect(points.length).toBeGreaterThan(0);
    for (const r of [...channel, ...points]) expect(r.artifactBudget).toBe(artifactBudgetFor(Math.log2(800 / 512)));

    const extent = store.extentOf(7n)!;
    expect(extent).not.toBeNull();
    // The last quarter of a 100 × 200 extent: x in [75, 100], y in [150, 200].
    expect(extent[0]).toBeCloseTo(75, 6);
    expect(extent[1]).toBeCloseTo(150, 6);
    expect(extent[2]).toBeCloseTo(100, 3);
    expect(extent[3]).toBeCloseTo(200, 3);
    for (const v of extent) expect(v).toBeLessThanOrEqual(200);

    // The same box, as the outline draws it: world units, one conversion for both readers. With no
    // shape the outline is the box, which is one part of one ring — `extentOf` reads the served
    // `box` whatever the shape is, so nothing here moved when the shape became parts of rings.
    const shape = outlineOf(FAR)!;
    expect([shape.source, shape.parts.length]).toEqual(['box', 1]);
    const outline = shape.parts[0]![0]!;
    expect(outline[0]).toEqual([gridToWorld(FAR.box![0]), gridToWorld(FAR.box![1])]);
    expect(outline[0]![0]).toBeCloseTo(WORLD_SIZE * 0.75, 6);
    const [wx0, wy0] = outline[0]!;
    expect(store.dataXY(wx0, wy0)).toEqual([extent[0], extent[1]]);
  });
});
