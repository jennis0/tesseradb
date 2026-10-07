import {describe, expect, it, vi} from 'vitest';
import type {TesseraClient} from '../src/client.js';
import {tileRectOfBbox} from '../src/budget.js';
import {WORLD_SIZE, dataToWorldXY, mortonOfTile, tileXY} from '../src/coords.js';
import {createStore, type Store} from '../src/store.js';
import type {ViewportResponse} from '../src/types.js';
import {camera, fakeClock, fakeScheduler, meta, response, scalar, servedResult, tile, view} from './support.js';

/**
 * What the map draws when the answer holds nothing: no marks, no stand-ins and no counted tiles
 * from the question before, while an answer still on its way leaves the last one drawn.
 */

const META = meta({
  views: [view('s0', {displayName: 'default', quantisation: {xMin: 0, xMax: 100, yMin: 0, yMax: 200}})],
  declaredScalars: [scalar('archive', 'u16', {category: {vocabulary: 'a', kind: 'declared', visibility: 'public'}, render: true, homes: ['rendered']})],
  filterOperands: [{column: 'archive', family: 'category', operands: ['in']}]
});

const Q = META.views[0]!.quantisation;
const FILTER = {filter: {archive: {family: 'category' as const, keys: ['none']}}, highlight: {}};

type Asked = {filters?: unknown; highlight?: unknown; zoom: number; tiles?: bigint[]; bbox?: [number, number, number, number]};

/** Three points on the whole-world tile, every one matching. */
function three(): ViewportResponse {
  return response(servedResult(3, [tile(0n, 1000n)], {scalars: {archive: {arrowType: 'u16', values: Uint16Array.from([5, 5, 5])}}}));
}

/** The whole-world tile holds 1,000 visible items and none matches: the server serves no point. */
function noneMatch(): ViewportResponse {
  return response(servedResult(0, [tile(0n, 1000n, {matched: 0n, highlighted: 0n})], {scalars: {archive: {arrowType: 'u16', values: new Uint16Array(0)}}}));
}

async function shown(reply: (req: Asked) => ViewportResponse | Promise<ViewportResponse>, revalidateAfterMs = Infinity, cacheBytes?: number): Promise<{store: Store; clock: ReturnType<typeof fakeClock>; scheduler: ReturnType<typeof fakeScheduler>; asked: () => number}> {
  const clock = fakeClock();
  const scheduler = fakeScheduler();
  const viewport = vi.fn(async (_t: string, req: Asked) => ({...(await reply(req)), region: null}));
  const client = {meta: async () => META, viewport, viewportArtifacts: async () => ({}), close: () => {}} as unknown as TesseraClient;
  const store = createStore({viewerUrl: 'http://v', token: 'tok', client, clock, scheduler, prefetch: false, replica: {revalidateAfterMs, ...(cacheBytes === undefined ? {} : {cacheBytes})}});
  await clock.advance(1);
  store.setView(camera(Q, [0, 0, 100, 200], 400, 400));
  await clock.advance(600);
  scheduler.flush();
  return {store, clock, scheduler, asked: () => viewport.mock.calls.length};
}

/** Every id the marks projection draws, exact and stand-in. */
function drawn(store: Store): bigint[] {
  const marks = store.get('marks');
  return [...marks.bands.flatMap((b) => [...b.ids]), ...marks.standIn.flatMap((p) => [...p.band.ids].slice(0, p.limit))];
}

describe('an answer of no matches draws nothing', () => {
  it('drops the marks and the counts drawn under the previous filter', async () => {
    const {store, clock, scheduler} = await shown((req) => (req.filters ? noneMatch() : three()));
    expect(drawn(store)).toHaveLength(3);

    store.setFilters(FILTER);
    await clock.advance(600);
    scheduler.flush();

    expect(store.get('status').status).toBe('empty');
    expect(drawn(store)).toEqual([]);
    expect(store.get('marks').count.shown).toBe(0);
    // The tile is still counted: it holds visible items, none of which matches.
    expect(store.get('tiles').tiles.map((t) => [t.drawn, t.counts?.visible, t.counts?.matched])).toEqual([[0, 1000n, 0n]]);
    expect(store.get('view').matched.value).toBe(0);
    expect(store.get('view').served.shown).toBe(0);
  });

  it('keeps the previous frame drawn while the answer is on its way', async () => {
    const waiting: (() => void)[] = [];
    const {store, clock, scheduler} = await shown((req) => {
      if (!req.filters) return three();
      return new Promise((resolve) => waiting.push(() => resolve(noneMatch())));
    });

    store.setFilters(FILTER);
    await clock.advance(600);
    scheduler.flush();
    expect(store.get('status').status).toBe('loading');
    expect(drawn(store)).toHaveLength(3);

    // The answer may come as a count first and the points after it.
    while (waiting.length > 0) {
      for (const release of waiting.splice(0)) release();
      await clock.advance(600);
      scheduler.flush();
    }
    expect(store.get('status').status).toBe('empty');
    expect(drawn(store)).toEqual([]);
  });

  it('a highlight that matches nothing still draws every point, none of them highlighted', async () => {
    const {store, clock, scheduler} = await shown((req) =>
      req.highlight
        ? response(servedResult(3, [tile(0n, 1000n, {highlighted: 0n})], {scalars: {archive: {arrowType: 'u16', values: Uint16Array.from([5, 5, 5])}}, highlighted: new Uint8Array(3)}))
        : three()
    );

    store.setFilters({filter: {}, highlight: {archive: {family: 'category', keys: ['none']}}});
    await clock.advance(600);
    scheduler.flush();

    expect(store.get('status').status).toBe('shown');
    expect(drawn(store)).toHaveLength(3);
    expect(store.get('view').highlighted.value).toBe(0);
    for (const band of store.get('marks').bands) expect([...(band.highlightBits ?? [])].every((b) => b === 0)).toBe(true);
  });

  it('a camera moved to where the filter matches nothing draws nothing there', async () => {
    // Under the filter only the left half of the world matches, one point at the centre of each
    // matching tile asked for; the right half holds visible items and serves none.
    const {store, clock, scheduler} = await shown((req) => (req.filters ? leftHalfMatches(req) : three()));
    store.setFilters(FILTER);
    await clock.advance(600);
    scheduler.flush();
    expect(drawn(store).length).toBeGreaterThan(0);

    // Zoomed in on the top of the right half, where the filter matches nothing.
    const box: [number, number, number, number] = [60, 0, 100, 80];
    store.setView(camera(Q, box, 400, 400));
    await clock.advance(600);
    scheduler.flush();
    const [x0, y0] = dataToWorldXY(box[0], box[1], Q);
    const [x1, y1] = dataToWorldXY(box[2], box[3], Q);
    const marks = store.get('marks');
    const inView: number[] = [];
    for (const band of marks.bands) {
      for (let i = 0; i < band.ids.length; i++) {
        const x = band.positions[i * 2]!;
        const y = band.positions[i * 2 + 1]!;
        if (x >= x0 && x <= x1 && y >= y0 && y <= y1) inView.push(i);
      }
    }
    expect(inView).toEqual([]);
    expect(marks.standIn).toEqual([]);
  });
});

/**
 * Under the filter only the left half of the world matches: one point at the centre of each
 * matching tile asked for, 100 visible items in every tile, and none served or matched on the right.
 */
function leftHalfMatches(req: Asked, per = 100n, matches = (x: number, z: number) => x < 2 ** (z - 1)): ViewportResponse {
  const z = req.zoom;
  const asked = req.bbox ? tilesOf(req.bbox, z) : (req.tiles ?? []);
  const left = asked.filter((p) => matches(tileXY(p, z).x, z));
  const right = asked.filter((p) => !matches(tileXY(p, z).x, z));
  const span = WORLD_SIZE / 2 ** z;
  const world = Float32Array.from(left.flatMap((p) => [(tileXY(p, z).x + 0.5) * span, (tileXY(p, z).y + 0.5) * span]));
  const counts = [...left.map((p) => tile(p, per, {served: 1n})), ...right.map((p) => tile(p, per, {matched: 0n, highlighted: 0n}))];
  return response({
    ...servedResult(left.length, [], {scalars: {archive: {arrowType: 'u16', values: Uint16Array.from(left, () => 5)}}}),
    tiles: counts,
    world,
    positions: Float64Array.from(world)
  });
}

/** The tiles at `zoom` a data bbox spans, as the store asks for them. */
function tilesOf(bbox: [number, number, number, number], zoom: number): bigint[] {
  const [x0, y0] = dataToWorldXY(bbox[0], bbox[1], Q);
  const [x1, y1] = dataToWorldXY(bbox[2], bbox[3], Q);
  const rect = tileRectOfBbox([Math.min(x0, x1), Math.min(y0, y1), Math.max(x0, x1), Math.max(y0, y1)], zoom);
  const out: bigint[] = [];
  for (let y = rect.y0; y <= rect.y1; y++) for (let x = rect.x0; x <= rect.x1; x++) out.push(mortonOfTile(x, y, zoom));
  return out;
}

describe('a frame counts every tile it covers', () => {
  it('counts the tiles a filter leaves with no point to serve', async () => {
    const {store, clock, scheduler} = await shown((req) => (req.filters ? leftHalfMatches(req) : three()));
    store.setFilters(FILTER);
    await clock.advance(600);
    scheduler.flush();

    const v = store.get('view');
    const want = v.composition!.want;
    const tiles = (want.x1 - want.x0 + 1) * (want.y1 - want.y0 + 1);
    const half = 2 ** (v.depth - 1);
    const leftTiles = (Math.min(want.x1, half - 1) - want.x0 + 1) * (want.y1 - want.y0 + 1);
    expect(leftTiles).toBeGreaterThan(0);
    expect(leftTiles).toBeLessThan(tiles);
    expect(v.visible.value).toBe(100 * tiles);
    expect(v.matched.value).toBe(100 * leftTiles);
  });
});

describe('a frame counts every tile it covers, with no count-only answer before it', () => {
  it('counts the tiles a filter leaves with no point to serve when the count-only request fails', async () => {
    const {store, clock, scheduler} = await shown((req) => {
      if (!req.filters) return three();
      if ((req as {k?: number}).k === 0) throw new Error('the count-only request failed');
      return leftHalfMatches(req);
    });
    store.setFilters(FILTER);
    await clock.advance(600);
    scheduler.flush();

    const v = store.get('view');
    const want = v.composition!.want;
    expect(v.visible.value).toBe(100 * (want.x1 - want.x0 + 1) * (want.y1 - want.y0 + 1));
  });
});

describe('a camera answered from held tiles', () => {
  it('reports empty where the filter matches nothing in view, and shown where it matches again', async () => {
    const {store, clock, scheduler, asked} = await shown((req) => (req.filters ? leftHalfMatches(req) : three()));
    store.setFilters(FILTER);
    await clock.advance(600);
    scheduler.flush();
    // Down to the depth the zoomed views draw at, so the pans below are answered from held tiles.
    store.setView(camera(Q, [0, 0, 25, 50], 400, 400));
    await clock.advance(600);
    scheduler.flush();
    expect(store.get('status').status).toBe('shown');
    const before = asked();

    store.setView(camera(Q, [75, 0, 100, 50], 400, 400));
    await clock.advance(600);
    scheduler.flush();
    expect(asked()).toBe(before);
    expect(store.get('status').status).toBe('empty');

    store.setView(camera(Q, [0, 150, 25, 200], 400, 400));
    await clock.advance(600);
    scheduler.flush();
    expect(asked()).toBe(before);
    expect(store.get('status').status).toBe('shown');
  });
});

describe('a camera answered from held tiles while the slot is busy', () => {
  it('reports the held status once the margin fetch clears the slot', async () => {
    // Only the first column of tiles at depth 8 matches, and every tile is dense, so the camera
    // below is asked at depth 8 and its margin after it.
    const answer = (req: Asked) => leftHalfMatches(req, 1_000_000n, (x, z) => z < 8 || x === 0);
    const waiting: (() => void)[] = [];
    let hold = false;
    const {store, clock, scheduler} = await shown((req) => {
      if (!req.filters || !hold) return req.filters ? answer(req) : three();
      return new Promise<ViewportResponse>((resolve) => waiting.push(() => resolve(answer(req))));
    });
    store.setFilters(FILTER);
    await clock.advance(600);
    scheduler.flush();
    // The ground to the right is held first, whole.
    store.setView(camera(Q, [3, 0, 6, 6], 400, 400));
    await clock.advance(600);
    scheduler.flush();
    hold = true;
    store.setView(camera(Q, [0, 0, 3, 6], 400, 400));
    await clock.advance(600);
    waiting.shift()!();
    await clock.advance(600);
    scheduler.flush();
    expect(store.get('status').status).toBe('shown');
    // The margin is out. The camera moves to ground the primary answer holds, where nothing matches.
    const margin = waiting.length;
    expect(margin).toBeGreaterThan(0);
    store.setView(camera(Q, [1.5, 0, 4.5, 6], 400, 400));
    await clock.advance(600);
    scheduler.flush();
    // Answered from held tiles: nothing more is asked.
    expect(waiting.length).toBe(margin);
    while (waiting.length > 0) {
      waiting.shift()!();
      await clock.advance(600);
      scheduler.flush();
    }
    expect(store.get('status').status).toBe('empty');
  });
});

describe('tiles that serve nothing, under a small budget', () => {
  it('are evicted, and their ground is asked again', async () => {
    const answer = (req: Asked) => leftHalfMatches(req, 1_000_000n, (x, z) => z < 8 || x === 0);
    const {store, clock, scheduler, asked} = await shown((req) => (req.filters ? answer(req) : three()), Infinity, 40_000);
    store.setFilters(FILTER);
    await clock.advance(600);
    scheduler.flush();
    const here = camera(Q, [0, 0, 3, 6], 400, 400);
    store.setView(here);
    await clock.advance(600);
    scheduler.flush();
    // Held whole: the same camera asks for nothing.
    store.setView({...here, bbox: [0, 0, 3, 6.001]});
    await clock.advance(600);
    scheduler.flush();
    const held = asked();
    store.setView({...here, bbox: [0, 0, 3, 6]});
    await clock.advance(600);
    scheduler.flush();
    expect(asked()).toBe(held);

    // Elsewhere, then back: the ground's bands of no points made way, so it is asked again.
    store.setView(camera(Q, [50, 100, 53, 106], 400, 400));
    await clock.advance(600);
    scheduler.flush();
    const away = asked();
    store.setView(here);
    await clock.advance(600);
    scheduler.flush();
    expect(asked()).toBeGreaterThan(away);
  });
});

describe('an answer that lists no tile', () => {
  it('drops the marks drawn under the previous filter', async () => {
    const {store, clock, scheduler} = await shown((req) => (req.filters ? response() : three()));
    expect(drawn(store)).toHaveLength(3);

    store.setFilters(FILTER);
    await clock.advance(600);
    scheduler.flush();

    expect(store.get('status').status).toBe('empty');
    expect(drawn(store)).toEqual([]);
    expect(store.get('tiles').tiles).toEqual([]);
  });
});

describe('a tile that serves nothing under a new content key', () => {
  it('replaces the points the tile served under the older key', async () => {
    let key = 'ck-1';
    const {store, clock, scheduler} = await shown(() => (key === 'ck-1' ? three() : {...noneMatch(), contentKey: key, pin: key}), 100);
    expect(drawn(store)).toHaveLength(3);

    // A revalidation observes the new key, and the refresh asks again under it.
    key = 'ck-2';
    await clock.advance(200);
    store.setView(camera(Q, [0, 0, 100, 200], 400, 400));
    await clock.advance(600);
    scheduler.flush();
    expect(store.get('status').stale).toBe(true);
    store.refresh();
    await clock.advance(600);
    scheduler.flush();

    expect(drawn(store)).toEqual([]);
    expect(store.get('view').matched.value).toBe(0);
  });
});

describe('an answer to a question no longer asked', () => {
  it('draws nothing from a request a filter change abandoned', async () => {
    const waiting: (() => void)[] = [];
    let hold = false;
    const {store, clock, scheduler} = await shown((req) => {
      if (req.filters) return new Promise<ViewportResponse>(() => {});
      if (!hold) return three();
      // The pan's answer, with ids of its own.
      const late = response(servedResult(3, [tile(0n, 1000n)], {ids: BigUint64Array.from([101n, 102n, 103n]), scalars: {archive: {arrowType: 'u16', values: Uint16Array.from([5, 5, 5])}}}));
      return new Promise<ViewportResponse>((resolve) => waiting.push(() => resolve(late)));
    });
    hold = true;
    store.setView(camera(Q, [0, 0, 20, 40], 400, 400));
    await clock.advance(600);
    expect(waiting.length).toBeGreaterThan(0);

    store.setFilters(FILTER);
    for (const release of waiting.splice(0)) release();
    await clock.advance(600);
    scheduler.flush();

    expect(drawn(store).filter((id) => id > 100n)).toEqual([]);
    expect(store.get('status').status).toBe('loading');
  });
});
