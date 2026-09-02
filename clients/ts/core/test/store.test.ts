import {describe, expect, it, vi} from 'vitest';
import {TesseraClient} from '../src/client.js';
import type {Clock} from '../src/driver.js';
import type {FrameScheduler} from '../src/presented.js';
import {createStore, type Store} from '../src/store.js';
import type {Meta, ViewportResponse, ViewportResult} from '../src/types.js';
import {dataToWorldXY, mortonOfTile} from '../src/coords.js';
import {tileRectOfBbox} from '../src/budget.js';

/**
 * The store against a fake `TesseraClient`, a fake clock and a fake frame scheduler — no DOM, no
 * network. Every assertion is about the store's own contract: `setView`'s conversion, the stale
 * rule keyed on the content key, and the drops.
 */

const META: Meta = {
  apiVersion: 1,
  idset: 0,
  views: [{id: 's0', displayName: 'default', quantisation: {xMin: 0, xMax: 100, yMin: 0, yMax: 200}}],
  declaredScalars: [{name: 'archive', arrowType: 'u16', category: {vocabulary: 'a', kind: 'declared', visibility: 'public'}, render: true, index: true}],
  layers: [],
  selection: {kMin: 1, kMaxMarks: 500, maxK: 5000, thetaTargetMarks: 10, maxUnderlayOffset: 0, maxCategoryValues: 1000, maxRegionVertices: 10_000, maxRegionCells: 262_144},
  maxTilesPerRequest: 4096,
  filterOperands: [{column: 'archive', family: 'category', operands: ['in']}]
};

/** What the fake sees of a request: enough to answer for every tile it asked about. */
type FakeRequest = {zoom: number; bbox?: [number, number, number, number]; tiles?: bigint[]; filters?: unknown; k?: number};

/**
 * A response that answers **every** tile the request spans — one tile of 1,000 items each, the
 * served points on the first — so a frame's coverage of a region is the client's arithmetic and
 * not the fixture's shape. `response()` below answers one tile whatever was asked, which every
 * other test relies on.
 */
function responseCovering(req: FakeRequest, contentKey: string, served = 3): ViewportResponse {
  const base = response(contentKey, 'ik', served);
  const q = META.views[0].quantisation;
  let prefixes: bigint[];
  if (req.tiles) prefixes = req.tiles;
  else {
    const [x0, y0] = dataToWorldXY(req.bbox![0], req.bbox![1], q);
    const [x1, y1] = dataToWorldXY(req.bbox![2], req.bbox![3], q);
    const rect = tileRectOfBbox([Math.min(x0, x1), Math.min(y0, y1), Math.max(x0, x1), Math.max(y0, y1)], req.zoom);
    prefixes = [];
    for (let y = rect.y0; y <= rect.y1; y++) for (let x = rect.x0; x <= rect.x1; x++) prefixes.push(mortonOfTile(x, y, req.zoom));
  }
  const tiles = prefixes.map((tile, i) => ({tile, visible: 1000n, matched: 1000n, served: i === 0 ? BigInt(served) : 0n}));
  return {...base, result: {...base.result, tiles}};
}

function response(contentKey: string, identityKey = 'ik', served = 3): ViewportResponse {
  const result: ViewportResult = {
    tiles: [{tile: 0n, visible: 10_000_000n, matched: 10_000_000n, served: BigInt(served)}],
    ids: BigUint64Array.from({length: served}, (_, i) => BigInt(i + 1)),
    codes: BigUint64Array.from({length: served}, () => 0n),
    positions: Float64Array.from({length: served * 2}, () => 1),
    world: Float32Array.from({length: served * 2}, () => 0.1),
    scalars: {archive: {arrowType: 'u16', values: Uint16Array.from({length: served}, () => 5)}},
    subCells: null,
    membership: {},
    artifacts: []
  };
  return {
    result,
    timings: {serverUs: 0, admissionUs: 0, stageNs: null},
    identityKey,
    contentKey,
    pin: contentKey,
    stale: false,
    region: null,
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

/** The `region` leaf anywhere in a filter expression, or null — what the fake answers a verdict for. */
function regionOf(expr: unknown): boolean {
  if (!expr || typeof expr !== 'object') return false;
  const node = expr as Record<string, unknown>;
  if (node.region) return true;
  for (const key of ['all_of', 'any_of', 'none_of']) {
    const kids = node[key];
    if (Array.isArray(kids) && kids.some(regionOf)) return true;
  }
  return false;
}

/**
 * A fake client — only the four verbs the store calls, and a log of the viewport requests. A
 * request carrying a `region` leaf is answered with the wire's `exact` verdict, as the server
 * would say on `x-tessera-region`.
 */
function fakeClient(reply: (req: FakeRequest) => ViewportResponse) {
  const viewport = vi.fn(async (_token: string, req: FakeRequest) => ({...reply(req), region: regionOf(req.filters) ? {exact: true as const, depth: null} : null}));
  const client = {
    meta: async () => META,
    viewport,
    item: async () => ({fields: {archive: 'cs'}, externalId: null}),
    artifact: async () => ({layer: 'l', key: 'k', maskedCount: 42n, centroid: null, box: null, shape: null}),
    categories: async () => [{code: 5, key: 'cs', title: 'CS'}],
    close: () => {}
  } as unknown as TesseraClient;
  return {client, viewport};
}

/** Build a store and drive it to its first shown frame. */
async function warm(reply: (req: FakeRequest) => ViewportResponse, opts: {clock: ReturnType<typeof fakeClock>; scheduler: ReturnType<typeof fakeScheduler>}) {
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
  it('enumerates a column’s values once, however many controls ask while the walk is in flight', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {client, viewport} = fakeClient(() => response('ck'));
    // A slow enumeration — the shape of a large `derived` vocabulary paged from the server — that
    // resolves only when told to.
    let release: (() => void) | null = null;
    const categories = vi.fn(
      () => new Promise<{code: number; key: string; title: string | null}[]>((resolve) => {
        release = () => resolve([{code: 1, key: 'FR.84', title: null}]);
      })
    );
    (client as unknown as {categories: typeof categories}).categories = categories;
    const store = createStore({viewerUrl: 'http://viewer', token: 'tok', client, clock, scheduler, prefetch: false, replica: {revalidateAfterMs: Infinity}});
    await clock.advance(1);
    void viewport;
    // Every store change re-asks, as `<tessera-filter>` does until the values land.
    for (let i = 0; i < 25; i++) void store.loadFilterValues('admin4');
    expect(categories).toHaveBeenCalledTimes(1);
    release!();
    await clock.advance(1);
    expect(store.get('filters').values['admin4']?.map((v) => v.key)).toEqual(['FR.84']);
    // Landed: a further ask is answered from the projection, not the wire.
    void store.loadFilterValues('admin4');
    expect(categories).toHaveBeenCalledTimes(1);
  });

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

describe('the colours are rebuilt when the table moves and not per response', () => {
  it('reuses the same colour map when a settle names no artifact the session had not held', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    // Two artifacts, served identically on every request — the shape of a layer whose artifacts
    // are scattered through row space and so are served in full whatever the viewport.
    const served = [
      {layer: 'clusters/a', tesseraId: 1n, key: 'c1', maskedCount: 5n, centroid: [1, 2] as [number, number], box: null, shape: null, content: [], parentIds: [], rung: 0, matched: null},
      {layer: 'clusters/a', tesseraId: 2n, key: 'c2', maskedCount: 7n, centroid: [3, 4] as [number, number], box: null, shape: null, content: [], parentIds: [], rung: 0, matched: null}
    ];
    const {client} = fakeClient(() => {
      const r = response('ck');
      return {...r, result: {...r.result, artifacts: served}};
    });
    const store = createStore({
      viewerUrl: 'http://viewer',
      token: 'tok',
      client,
      clock,
      scheduler,
      prefetch: false,
      replica: {revalidateAfterMs: Infinity}
    });
    store.setLayers(['clusters/a']);
    store.setView({bbox: [0, 0, 100, 200], width: 800, height: 800});
    await clock.advance(600);
    scheduler.flush();
    await clock.advance(600);
    const first = store.get('artifacts');
    expect(first.held).toBe(2);
    expect(first.colours.size).toBe(2);

    // A second settle over other ground, answered with the same artifacts: nothing is named, so
    // the colour map is the same object and the lookup texture built from it is not rewritten.
    store.setView({bbox: [50, 100, 100, 200], width: 800, height: 800});
    await clock.advance(600);
    scheduler.flush();
    await clock.advance(600);
    expect(store.get('artifacts').colours).toBe(first.colours);
    expect(store.get('artifacts').held).toBe(2);
  });

  it('extends the map for what a settle named and recomputes nothing already in it', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const artifact = (id: bigint, centroid: [number, number]) => ({
      layer: 'clusters/a',
      tesseraId: id,
      key: `c${id}`,
      maskedCount: 5n,
      centroid,
      box: null,
      shape: null,
      content: [],
      parentIds: [],
      rung: 0,
      matched: null
    });
    let served = [artifact(1n, [1, 2]), artifact(2n, [3, 4])];
    const {client} = fakeClient(() => {
      const r = response('ck');
      return {...r, result: {...r.result, artifacts: served, artifactsIdentity: null}};
    });
    const store = createStore({viewerUrl: 'http://viewer', token: 'tok', client, clock, scheduler, prefetch: false, replica: {revalidateAfterMs: Infinity}});
    store.setLayers(['clusters/a']);
    store.setView({bbox: [0, 0, 100, 200], width: 800, height: 800});
    await clock.advance(600);
    scheduler.flush();
    await clock.advance(600);
    const table = store.get('artifacts').table;
    const first = store.get('artifacts').colours;
    const one = table.ordinalOf('clusters/a', 1n);
    const held = first.get(one);
    expect(first.size).toBe(2);

    // A settle that names a third artifact. Under the positional palette a colour is a pure
    // function of one centroid, so the map is extended where the table moved: the same object,
    // and the colours already in it are the same objects — not recomputed to the same values.
    served = [...served, artifact(3n, [5, 6])];
    store.setView({bbox: [50, 100, 100, 200], width: 800, height: 800});
    await clock.advance(600);
    scheduler.flush();
    await clock.advance(600);
    scheduler.flush();
    await clock.advance(600);
    const next = store.get('artifacts').colours;
    expect(next.size).toBe(3);
    expect(next).toBe(first);
    expect(next.get(one)).toBe(held);
    expect(next.get(table.ordinalOf('clusters/a', 3n))).toBeDefined();

    // A palette change is not an extension: every colour moves, so the map is rebuilt and its
    // identity says so.
    store.setPalette('spread');
    expect(store.get('artifacts').colours).not.toBe(first);
    expect(store.get('artifacts').colours.size).toBe(3);
  });
});

describe('select — the selection is the region leaf on every request (§5.11)', () => {
  /** The `region` leaf of the request's filters, or null. */
  const leafOf = (req: unknown): unknown => {
    const body = (req as [string, {filters?: unknown}])[1].filters;
    const find = (expr: unknown): unknown => {
      if (!expr || typeof expr !== 'object') return null;
      const node = expr as Record<string, unknown>;
      if (node.region) return node.region;
      for (const key of ['all_of', 'any_of', 'none_of']) {
        const kids = node[key];
        if (Array.isArray(kids)) for (const kid of kids) {
          const found = find(kid);
          if (found) return found;
        }
      }
      return null;
    };
    return find(body);
  };

  it('puts a box on the next request as a bbox leaf, reads the count off the frame, and types it by the verdict', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {store, viewport} = await warm((req) => responseCovering(req, 'ck1'), {clock, scheduler});
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await clock.advance(600);
    scheduler.flush();
    expect(store.get('marks').count.shown).toBeGreaterThan(0);
    const before = viewport.mock.calls.length;

    // The whole extent, drawn from the far corner: the leaf is the box normalised.
    store.select({kind: 'box', bbox: [100, 200, 0, 0]});
    const loading = store.get('region')!;
    expect(loading.status).toBe('loading');
    expect(loading.verdict).toBeNull();
    // The held marks inside are the client's own fact, known before the server answers.
    expect(loading.held.count).toBe(3);

    await clock.advance(600);
    scheduler.flush();
    expect(viewport.mock.calls.length).toBeGreaterThan(before);
    expect(leafOf(viewport.mock.calls[before])).toEqual({bbox: [0, 0, 100, 200]});
    // No counting request of its own: every request since the selection carries the leaf.
    for (const call of viewport.mock.calls.slice(before)) expect(leafOf(call)).not.toBeNull();

    const shown = store.get('region')!;
    expect(shown.status).toBe('shown');
    expect(shown.verdict).toEqual({exact: true, depth: null});
    // Exact: the server said so, and the replica holds every tile of the box — the whole extent
    // at the depth the driver chose, 1,000 matched in each of the tiles that drew a point, and
    // the frame's sum is over those.
    expect(shown.matched.exact).toBe(true);
    expect(shown.matched.value).toBeGreaterThan(0);
    // No other filter is on, so the region alone is the same number.
    expect(shown.visible).toEqual(shown.matched);
    expect(shown.served).toEqual({shown: 3, total: shown.matched.value, exact: true});
  });

  it('clears the region on select(null), composes it with the filters, and re-reads on setFilters', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {store, viewport} = await warm(() => response('ck1'), {clock, scheduler});
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await clock.advance(600);
    scheduler.flush();
    store.select({kind: 'box', bbox: [0, 0, 1, 1]});
    await clock.advance(600);
    scheduler.flush();
    expect(store.get('region')?.status).toBe('shown');
    const asked = viewport.mock.calls.length;
    store.setFilters({archive: {family: 'category', keys: ['cs']}});
    expect(store.get('region')?.status).toBe('loading');
    await clock.advance(600);
    scheduler.flush();
    expect(viewport.mock.calls.length).toBeGreaterThan(asked);
    const body = (viewport.mock.calls[asked] as unknown as [string, {filters: unknown}])[1].filters;
    expect(body).toEqual({all_of: [{archive: {in: ['cs']}}, {region: {bbox: [0, 0, 1, 1]}}]});
    const shown = store.get('region')!;
    expect(shown.status).toBe('shown');
    // Under another filter, the frame answered the narrower question: `visible` is not it.
    expect(shown.visible).toBeNull();
    expect(shown.matched.value).toBe(10_000_000);
    // The one held tile covers a box this small: exact for the shape.
    expect(shown.matched.exact).toBe(true);

    const cleared = viewport.mock.calls.length;
    store.select(null);
    expect(store.get('region')).toBeNull();
    await clock.advance(600);
    scheduler.flush();
    expect(viewport.mock.calls.length).toBeGreaterThan(cleared);
    expect(leafOf(viewport.mock.calls[cleared])).toBeNull();
  });

  it('sends a lasso as its polygon, and outside as none_of over it', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {store, viewport} = await warm(() => response('ck1'), {clock, scheduler});
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await clock.advance(600);
    scheduler.flush();
    const before = viewport.mock.calls.length;
    // A triangle over the lower-left half of the extent; the held mark at world (0.1, 0.1) is inside.
    store.select({kind: 'lasso', points: [[0, 0], [100, 0], [0, 200]]});
    const loading = store.get('region')!;
    expect(loading.status).toBe('loading');
    expect(loading.held.count).toBe(3);
    await clock.advance(600);
    scheduler.flush();
    expect(leafOf(viewport.mock.calls[before])).toEqual({polygon: [[0, 0], [100, 0], [0, 200]]});
    expect(store.get('region')?.status).toBe('shown');

    const flipped = viewport.mock.calls.length;
    store.select({kind: 'lasso', points: [[0, 0], [100, 0], [0, 200]], outside: true});
    // Outside the triangle: none of the held marks, and the complement is never covered by one frame.
    expect(store.get('region')?.held.count).toBe(0);
    await clock.advance(600);
    scheduler.flush();
    const body = (viewport.mock.calls[flipped] as unknown as [string, {filters: unknown}])[1].filters;
    expect(body).toEqual({none_of: [{region: {polygon: [[0, 0], [100, 0], [0, 200]]}}]});
    expect(store.get('region')?.matched.exact).toBe(false);

    // A lasso too thin to be a polygon is no selection.
    store.select({kind: 'lasso', points: [[0, 0], [1, 1]]});
    expect(store.get('region')).toBeNull();
  });

  it('filters to an artifact by its id', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {store, viewport} = await warm(() => response('ck1'), {clock, scheduler});
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await clock.advance(600);
    scheduler.flush();
    const before = viewport.mock.calls.length;
    store.select({kind: 'artifact', id: 42n});
    await clock.advance(600);
    scheduler.flush();
    expect(leafOf(viewport.mock.calls[before])).toEqual({artifact: '42'});
    const shown = store.get('region')!;
    expect(shown.status).toBe('shown');
    expect(shown.matched.value).toBe(10_000_000);
    // The store holds no extent for an artifact it was not served, so it cannot say the frame
    // covered it: the number is the frame's and is not claimed exact.
    expect(shown.matched.exact).toBe(false);
  });
});


describe('needShape fetches the drawn shape by identifier', () => {
  /**
   * The viewport is asked for centroids and boxes (`artifactChannel.ts`), so the shape a map draws
   * comes from `/v1/artifacts/{id}`. What matters here is the *asking*: on a pointer move this is
   * called every frame, so a second request for a shape already held — or already in flight — is
   * the defect the two maps behind it exist to prevent.
   */
  function shapeClient(shape: [number, number][][][] | null) {
    const artifact = vi.fn(async () => ({layer: 'l', key: null, maskedCount: 7n, centroid: null, box: null, shape}));
    const client = {
      meta: async () => META,
      viewport: async () => response('k'),
      item: async () => ({fields: {}, externalId: null}),
      artifact,
      categories: async () => [],
      close: () => {}
    } as unknown as TesseraClient;
    return {client, artifact};
  }

  async function storeWith(shape: [number, number][][][] | null) {
    const clock = fakeClock();
    const {client, artifact} = shapeClient(shape);
    const store = createStore({
      viewerUrl: 'http://viewer',
      token: 'tok',
      client,
      clock,
      scheduler: fakeScheduler(),
      prefetch: false,
      replica: {revalidateAfterMs: Infinity}
    });
    await clock.advance(1);
    return {store, artifact, clock};
  }

  it('asks once per artifact and publishes the parts it gets back', async () => {
    const parts: [number, number][][][] = [[[[0, 0], [10, 0], [10, 10]]]];
    const {store, artifact, clock} = await storeWith(parts);
    store.needShape(5n);
    store.needShape(5n);
    await clock.advance(1);
    expect(artifact).toHaveBeenCalledTimes(1);
    expect(store.get('artifacts').shapes.get(5n)).toEqual(parts);
    // Held, so a later ask is free.
    store.needShape(5n);
    await clock.advance(1);
    expect(artifact).toHaveBeenCalledTimes(1);
  });

  it('holds nothing for an artifact whose layer draws no shape, and does not ask again', async () => {
    const {store, artifact, clock} = await storeWith(null);
    store.needShape(9n);
    await clock.advance(1);
    expect(store.get('artifacts').shapes.has(9n)).toBe(false);
    store.needShape(9n);
    await clock.advance(1);
    expect(artifact).toHaveBeenCalledTimes(1);
  });

  /**
   * The rung 3 defect (`client-delivery.md`): the drill-down answers the shape beside the box, and
   * opening an artifact read the box and dropped the shape on the floor. The map draws from
   * `artifacts.shapes`, so a cluster opened from the list drew nothing at all on a layer that
   * declares a hull — 253 served, 0 rings.
   */
  it('opening an artifact holds the shape its own answer carried, and asks for nothing more', async () => {
    const parts: [number, number][][][] = [[[[0, 0], [10, 0], [10, 10]]]];
    const {store, artifact, clock} = await storeWith(parts);
    await store.openArtifact(5n);
    expect(store.get('artifacts').shapes.get(5n)).toEqual(parts);
    expect(artifact).toHaveBeenCalledTimes(1);
    // The map calls `needShape` beside the open on its own click path; the shape is in hand.
    store.needShape(5n);
    await clock.advance(1);
    expect(artifact).toHaveBeenCalledTimes(1);
  });

  it('forgets every held shape on clear — a derived shape is this principal’s', async () => {
    const parts: [number, number][][][] = [[[[0, 0], [10, 0], [10, 10]]]];
    const {store, artifact, clock} = await storeWith(parts);
    store.needShape(5n);
    await clock.advance(1);
    expect(store.get('artifacts').shapes.size).toBe(1);
    store.clear();
    expect(store.get('artifacts').shapes.size).toBe(0);
    // And the identifier is askable again, because nothing is held for it now.
    store.needShape(5n);
    await clock.advance(1);
    expect(artifact).toHaveBeenCalledTimes(2);
  });
});

describe('the subscriber fan-out', () => {
  it('tells every subscriber even when an earlier one throws', () => {
    // The fault that motivated this: a viewer's basemap follower threw from inside the fan-out on
    // a view with no tile, and every element subscribed after the map kept drawing the previous
    // publish — a status strip at zero beside a million marks.
    const quiet = vi.spyOn(console, 'error').mockImplementation(() => {});
    const traced: string[] = [];
    const store = createStore({viewerUrl: 'http://x', token: 't', instruments: {onTrace: (kind) => traced.push(kind)}});
    const seen: string[] = [];
    store.subscribe('filters', () => {
      throw new Error('boom');
    });
    store.subscribe('filters', () => seen.push('named-after'));
    store.subscribe(() => seen.push('all'));
    store.setFilters({});
    expect(seen).toEqual(['named-after', 'all']);
    expect(traced).toContain('subscriber-fault');
    expect(quiet).toHaveBeenCalledTimes(1);
    quiet.mockRestore();
    store.dispose();
  });
});
