import {describe, expect, it, vi} from 'vitest';
import {TesseraClient} from '../src/client.js';
import {GRID32, gridToWorld, WORLD_SIZE} from '../src/coords.js';
import {outlineOf} from '../../deck/src/layer.js';
import {createStore} from '../src/store.js';
import {artifactBudgetFor} from '../src/artifactBudget.js';
import {artifact, fakeClock, fakeScheduler, layer, meta, response, servedResult, tile, view} from './support.js';

/**
 * `extentOf` reads the served `box` in the wire's units — 32 bits per axis (contracts §3.2 item
 * 4), the same as `code` — and the outlines read the same box. An artifact at the corpus's far
 * corner is the case that tells a right divisor from a wrong one: under 2^16 its box lands 65,536
 * extents away, and `fit` on it shows nothing.
 */

const META = meta({
  views: [view('s0', {quantisation: {xMin: 0, xMax: 100, yMin: 0, yMax: 200}})],
  layers: [layer('clusters/a', {title: 'a', computedContent: ['centroid', 'box']})]
});

/** The far corner: the last quarter of the grid on both axes. */
const FAR = artifact(7n, {
  layer: 'clusters/a',
  key: 'far',
  maskedCount: 10n,
  centroid: [GRID32 * 0.875, GRID32 * 0.875],
  box: [GRID32 * 0.75, GRID32 * 0.75, GRID32 - 1, GRID32 - 1]
});

const reply = () => response(servedResult(1, [tile(0n, 10n)], {artifacts: [FAR]}));

describe('extentOf reads the wire box in 32-bit grid units, as the outlines do', () => {
  it('fits an artifact at the far corner inside the corpus extent, and agrees with outlineOf', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const client = {
      meta: async () => META,
      viewport: vi.fn(async () => reply()),
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
