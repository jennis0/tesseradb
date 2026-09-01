import {describe, expect, it, vi} from 'vitest';
import {TesseraClient} from '../src/client.js';
import type {Clock} from '../src/driver.js';
import type {FrameScheduler} from '../src/presented.js';
import {createStore} from '../src/store.js';
import type {Meta, ViewInfo, ViewportPart, ViewportResponse, ViewportResult} from '../src/types.js';
import {dataToWorldXY, mortonOfTile} from '../src/coords.js';
import {tileRectOfBbox} from '../src/budget.js';

/**
 * The store across several views (`view-switching.md` §3–§4): one store, one byte budget, a
 * replica, a presenter and a channel per view, and a switch that is a pointer change.
 *
 * The fixture is the two cases that differ. `v0`, `v1` and `v2` share one frame and one group —
 * the slider — and `far` quantises against another, which is what a plain view or a second layout
 * over the same keys looks like. Everything here runs against a fake client, a fake clock and a
 * fake frame scheduler: what is asserted is which requests went out, under which view, and what
 * the projections said in between.
 */

const FRAME = {xMin: 0, xMax: 100, yMin: 0, yMax: 200};
const OTHER_FRAME = {xMin: -1000, xMax: 1000, yMin: -500, yMax: 500};

function view(id: string, quantisation: typeof FRAME, group: string | null): ViewInfo {
  return {
    id,
    displayName: id,
    quantisation,
    projection: 'none',
    worldAspect: null,
    tileScheme: null,
    tile: null,
    roster: group ? {group, key: id, metadata: {}} : null
  };
}

const META: Meta = {
  apiVersion: 1,
  idset: 0,
  views: [view('far', OTHER_FRAME, null), view('v0', FRAME, 'g'), view('v1', FRAME, 'g'), view('v2', FRAME, 'g')],
  groups: [{name: 'g', title: 'quarters', membersOf: null, views: ['v0', 'v1', 'v2']}],
  declaredScalars: [{name: 'archive', arrowType: 'u16', category: {vocabulary: 'a', kind: 'declared', visibility: 'public'}, render: true, index: true}],
  layers: [
    {name: 'l', title: 'l', views: ['v0', 'v1', 'v2', 'far'], membership: 'enumerated', hierarchy: {kind: 'flat', pruneChildren: false}, levels: [], computedContent: ['centroid', 'box'], suppliedContent: [], shape: null, depsOn: [], version: 1},
    {name: 'm', title: 'm', views: ['v0', 'v1', 'v2', 'far'], membership: 'enumerated', hierarchy: {kind: 'flat', pruneChildren: false}, levels: [], computedContent: ['centroid', 'box'], suppliedContent: [], shape: null, depsOn: [], version: 1}
  ],
  selection: {kMin: 1, kMaxMarks: 500, maxK: 5000, thetaTargetMarks: 10, maxUnderlayOffset: 0, maxCategoryValues: 1000, maxRegionVertices: 10_000, maxRegionCells: 262_144},
  maxTilesPerRequest: 4096,
  filterOperands: [{column: 'archive', family: 'category', operands: ['in']}]
};

type FakeRequest = {view: string; zoom: number; bbox?: [number, number, number, number]; tiles?: bigint[]; filters?: unknown; k?: number; layers?: string[]};

/** A response answering every tile the request spans, so coverage is the client's arithmetic. */
function responseCovering(req: FakeRequest, served = 3): ViewportResponse {
  // One artifact per view, identified by the view that served it — the client does not match
  // artifact identity across views (§8), and a projection carrying the wrong one is visible here.
  // Served only where the request named a layer, as the wire serves them.
  const artifacts = (req.layers ?? []).length === 0 ? [] : [
    {
      layer: 'l',
      tesseraId: BigInt(META.views.findIndex((v) => v.id === req.view) + 1),
      key: req.view,
      maskedCount: 5n,
      centroid: null,
      box: null,
      shape: null,
      content: [],
      parentId: null,
      rung: 0,
      matched: null
    }
  ];
  const result: ViewportResult = {
    tiles: [],
    ids: BigUint64Array.from({length: served}, (_, i) => BigInt(i + 1)),
    codes: BigUint64Array.from({length: served}, () => 0n),
    positions: Float64Array.from({length: served * 2}, () => 1),
    world: Float32Array.from({length: served * 2}, () => 0.1),
    scalars: {archive: {arrowType: 'u16', values: Uint16Array.from({length: served}, () => 5)}},
    subCells: null,
    // The membership column each named layer's pass puts on the band — its absence is what makes
    // a band colour-stale when a layer goes on while a view is held.
    membership: {},
    artifacts
  };
  const q = META.views.find((v) => v.id === req.view)!.quantisation;
  let prefixes: bigint[];
  if (req.tiles) prefixes = req.tiles;
  else {
    const [x0, y0] = dataToWorldXY(req.bbox![0], req.bbox![1], q);
    const [x1, y1] = dataToWorldXY(req.bbox![2], req.bbox![3], q);
    const rect = tileRectOfBbox([Math.min(x0, x1), Math.min(y0, y1), Math.max(x0, x1), Math.max(y0, y1)], req.zoom);
    prefixes = [];
    for (let y = rect.y0; y <= rect.y1; y++) for (let x = rect.x0; x <= rect.x1; x++) prefixes.push(mortonOfTile(x, y, req.zoom));
  }
  return {
    result: {...result, tiles: prefixes.map((tile, i) => ({tile, visible: 1000n, matched: 1000n, served: i === 0 ? BigInt(served) : 0n}))},
    timings: {serverUs: 0, admissionUs: 0, stageNs: null},
    identityKey: 'ik',
    contentKey: 'ck',
    pin: 'ck',
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

/** The ground a view was asked about, over every piece its requests were split into. */
function union(boxes: [number, number, number, number][]): [number, number, number, number] {
  return boxes.reduce<[number, number, number, number]>(
    (a, b) => [Math.min(a[0], b[0], b[2]), Math.min(a[1], b[1], b[3]), Math.max(a[2], b[0], b[2]), Math.max(a[3], b[1], b[3])],
    [Infinity, Infinity, -Infinity, -Infinity]
  );
}

/** The `region` leaf anywhere in a filter expression — what says the selection rode the request. */
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

function open(opts: {
  view?: string;
  clock: ReturnType<typeof fakeClock>;
  scheduler: ReturnType<typeof fakeScheduler>;
  prefetch?: boolean;
  /**
   * A view whose requests hand their points over as a **part** and then never resolve — the wire
   * mid-stream. What is drawn from it is a partial frame, presented while the request that
   * carries it is still in flight.
   */
  streaming?: string;
}) {
  const viewport = vi.fn(
    async (
      _token: string,
      req: FakeRequest,
      _signal?: AbortSignal,
      _background?: boolean,
      onPart?: (part: ViewportPart) => void | Promise<void>
    ) => {
      const answer = {...responseCovering(req), region: regionOf(req.filters) ? {exact: true as const, depth: null} : null};
      if (opts.streaming === req.view && onPart) {
        await onPart({result: answer.result, identityKey: answer.identityKey, contentKey: answer.contentKey});
        // The points are the sink's now, and the response never comes: the frame on screen is the
        // part, and the request is still open.
        return new Promise<never>(() => {});
      }
      return answer;
    }
  );
  const traces: {kind: string; fields: Record<string, number | string>}[] = [];
  const client = {
    meta: async () => META,
    viewport,
    item: async () => ({fields: {}, externalId: null}),
    artifact: async () => ({layer: 'l', key: 'k', maskedCount: 42n, centroid: null, box: null, shape: null}),
    categories: async () => [],
    close: () => {}
  } as unknown as TesseraClient;
  const store = createStore({
    viewerUrl: 'http://viewer',
    token: 'tok',
    client,
    view: opts.view,
    clock: opts.clock,
    scheduler: opts.scheduler,
    prefetch: opts.prefetch ?? false,
    replica: {revalidateAfterMs: Infinity},
    instruments: {onTrace: (kind, fields) => traces.push({kind, fields})}
  });
  /** Every viewport request issued for one view — the requests are the thing under test. */
  const asked = (id: string) => viewport.mock.calls.filter((c) => (c[1] as FakeRequest).view === id);
  return {store, viewport, asked, traces};
}

/** Bring a store to its first shown frame in whichever view it opened on. */
async function shown(
  store: ReturnType<typeof open>['store'],
  clock: ReturnType<typeof fakeClock>,
  scheduler: ReturnType<typeof fakeScheduler>
): Promise<void> {
  store.setView({bbox: [0, 0, 50, 50], width: 800, height: 400});
  await clock.advance(600);
  scheduler.flush();
}

describe('a switch within a group keeps the camera and the selection (§4)', () => {
  it('re-asks the same tiles under the new view, with the region leaf still on the request', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {store, asked} = open({view: 'v0', clock, scheduler});
    await clock.advance(1);
    await shown(store, clock, scheduler);
    store.select({kind: 'box', bbox: [10, 10, 20, 20]});
    await clock.advance(600);
    scheduler.flush();
    // The camera's own ground, which the requests for v0 covered between them.
    const camera: [number, number, number, number] = [0, 0, 50, 50];
    expect(store.get('region')).not.toBeNull();

    store.setCurrentView('v1');

    // Published immediately, with no marks: view v0's marks must never be drawn under v1's id.
    expect(store.get('view').id).toBe('v1');
    expect(store.get('view').composition).toBeNull();
    expect(store.get('marks').bands).toEqual([]);
    // The selection is a shape in the shared frame and means the same thing here.
    expect(store.get('region')!.shape).toEqual({kind: 'box', bbox: [10, 10, 20, 20]});
    expect(asked('v1')).toHaveLength(0);

    await clock.advance(600);
    scheduler.flush();

    // The camera did not move, so the view arrived at is asked about the ground the view left was
    // looking at. **Compared by the ground the requests span, not rectangle for rectangle**: the
    // depth is not pinned across a group switch (§4, decided at r2) — the budget chooses it per
    // view — and a request rectangle is tile-aligned and inset by half a grid unit, so the
    // comparison is to within one.
    const ground = union(asked('v1').map((c) => (c[1] as FakeRequest).bbox!));
    expect(ground[0]).toBeLessThan(0.01);
    expect(ground[1]).toBeLessThan(0.01);
    expect(ground[2]).toBeGreaterThanOrEqual(camera[2]);
    expect(ground[3]).toBeGreaterThanOrEqual(camera[3]);
    // Its counts are re-asked under the new view: the region is composed into the filters.
    const after = asked('v1').at(-1)![1] as FakeRequest;
    expect(regionOf(after.filters)).toBe(true);
    expect(store.get('view').composition).not.toBeNull();
    expect(store.get('status').status).toBe('shown');
  });

  it('draws a view returned to from its own held bands, without asking (§3)', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {store, asked} = open({view: 'v0', clock, scheduler});
    await clock.advance(1);
    await shown(store, clock, scheduler);
    store.setCurrentView('v1');
    await clock.advance(600);
    scheduler.flush();
    const askedV0 = asked('v0').length;

    store.setCurrentView('v0');
    // One scheduler tick, no clock: the frame comes from what v0 already holds.
    scheduler.flush();

    expect(store.get('view').id).toBe('v0');
    expect(store.get('view').composition).not.toBeNull();
    expect(store.get('status').status).toBe('shown');
    expect(asked('v0')).toHaveLength(askedV0);
  });
});

describe('a switch across frames publishes no camera and drops the selection (§4)', () => {
  it('publishes an empty frame at loading, asks nothing, and answers the map’s refit', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {store, asked} = open({view: 'v0', clock, scheduler});
    await clock.advance(1);
    await shown(store, clock, scheduler);
    store.select({kind: 'box', bbox: [10, 10, 20, 20]});
    await clock.advance(600);
    scheduler.flush();

    store.setCurrentView('far');

    expect(store.get('view').id).toBe('far');
    expect(store.get('view').composition).toBeNull();
    expect(store.get('marks').bands).toEqual([]);
    expect(store.get('tiles').tiles).toEqual([]);
    expect(store.get('region')).toBeNull();
    expect(store.get('status').status).toBe('loading');

    // No camera means no question: nothing is asked until the map refits and says where it is.
    await clock.advance(2000);
    scheduler.flush();
    expect(asked('far')).toHaveLength(0);

    store.setView({bbox: [-1000, -500, 1000, 500], width: 800, height: 400});
    await clock.advance(600);
    scheduler.flush();
    expect(asked('far').length).toBeGreaterThan(0);
    expect(store.get('view').composition).not.toBeNull();
  });
});

describe('a view that is not current asks for nothing (§8)', () => {
  it('issues no request for a view stepped through and left before its settle', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {store, asked} = open({view: 'v0', clock, scheduler});
    await clock.advance(1);
    await shown(store, clock, scheduler);

    store.setCurrentView('v1');
    await clock.advance(50);
    store.setCurrentView('v2');
    await clock.advance(600);
    scheduler.flush();

    // The step the slider passed over cost nothing; the one it stopped on asked once.
    expect(asked('v1')).toHaveLength(0);
    expect(asked('v2').length).toBeGreaterThan(0);
  });

  it('asks nothing for a held view when the layers change', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {store, asked} = open({view: 'v0', clock, scheduler});
    await clock.advance(1);
    await shown(store, clock, scheduler);
    store.setCurrentView('v1');
    await clock.advance(600);
    scheduler.flush();
    store.setCurrentView('v0');
    await clock.advance(600);
    scheduler.flush();
    const held = asked('v1').length;
    expect(held).toBeGreaterThan(0);

    store.setLayers(['l']);
    await clock.advance(2000);
    scheduler.flush();

    expect(asked('v1')).toHaveLength(held);
    expect(store.get('artifacts').layers).toEqual(['l']);
  });
});

describe('a view that is not current asks for nothing — the paths that reach the driver', () => {
  it('does not ask for a stepped-to view whose bands are colour-stale under a layer switched on', async () => {
    // The colour-stale refetch retracts coverage and reschedules, which re-enters the driver; a
    // view the slider is passing through must not ask through that route either (§4).
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {store, asked} = open({view: 'v0', clock, scheduler});
    await clock.advance(1);
    store.setLayers(['l']);
    await shown(store, clock, scheduler);
    // v1 is warmed with `l` on, so its channel has answered and its bands carry `l`'s column.
    store.setCurrentView('v1');
    await clock.advance(600);
    scheduler.flush();
    store.setCurrentView('v0');
    await clock.advance(600);
    scheduler.flush();
    // A second layer goes on while v1 is held: no band it holds carries `m`'s column, so every
    // one of them is colour-stale the moment v1 draws again.
    store.setLayers(['l', 'm']);
    await clock.advance(600);
    scheduler.flush();
    const held = asked('v1').length;

    // Key-repeat through v1: its redraw frame is presented while it is current, and the coverage
    // check runs against it.
    store.setCurrentView('v1');
    scheduler.flush();
    await clock.advance(50);
    store.setCurrentView('v2');
    await clock.advance(600);
    scheduler.flush();

    expect(asked('v1')).toHaveLength(held);
    expect(asked('v2').length).toBeGreaterThan(0);
  });

  it('issues no anticipation for a view left inside its idle window', async () => {
    // With prefetch on, a settled view arms the idle timer that buys its ring. Leaving before it
    // fires must take the timer with the view — as it takes the pan's own debounce.
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {store, asked} = open({view: 'v0', clock, scheduler, prefetch: true});
    await clock.advance(1);
    await shown(store, clock, scheduler);

    // A pan re-arms the idle timer that buys the ring; 100 ms is inside its window. The camera is
    // small against the frame, so the ring reaches ground the render rect did not cover.
    store.setView({bbox: [40, 90, 45, 95], width: 800, height: 400});
    await clock.advance(100);
    const before = asked('v0').length;
    store.setCurrentView('v1');
    await clock.advance(4000);
    scheduler.flush();

    expect(asked('v0')).toHaveLength(before);
    expect(asked('v1').length).toBeGreaterThan(0);

    // The control: the same store left alone through the same window does buy its ring, so what
    // the assertion above pins is the leaving and not an idle that never fires.
    const clock2 = fakeClock();
    const scheduler2 = fakeScheduler();
    const alone = open({view: 'v0', clock: clock2, scheduler: scheduler2, prefetch: true});
    await clock2.advance(1);
    await shown(alone.store, clock2, scheduler2);
    alone.store.setView({bbox: [40, 90, 45, 95], width: 800, height: 400});
    await clock2.advance(100);
    const stillHere = alone.asked('v0').length;
    await clock2.advance(4000);
    scheduler2.flush();
    expect(alone.asked('v0').length).toBeGreaterThan(stillHere);
  });
});

describe('the switch’s own rules (§3)', () => {
  it('ignores an id the bundle does not declare, and reports it', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {store, traces} = open({view: 'v0', clock, scheduler});
    await clock.advance(1);
    await shown(store, clock, scheduler);

    store.setCurrentView('nope');

    expect(store.get('view').id).toBe('v0');
    expect(traces.filter((t) => t.kind === 'view-switch' && t.fields.refused === 1 && t.fields.id === 'nope')).toHaveLength(1);
    // Not a throw, and not a neighbour: the frame on screen is untouched.
    expect(store.get('view').composition).not.toBeNull();
  });

  it('applies a call made before meta in place of options.view', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {store, asked} = open({view: 'v0', clock, scheduler});
    store.setCurrentView('v2');
    await clock.advance(1);

    expect(store.get('view').id).toBe('v2');
    await shown(store, clock, scheduler);
    expect(asked('v0')).toHaveLength(0);
    expect(asked('v2').length).toBeGreaterThan(0);
  });

  it('does nothing when the id is already current', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {store, asked, traces} = open({view: 'v0', clock, scheduler});
    await clock.advance(1);
    await shown(store, clock, scheduler);
    const before = asked('v0').length;

    store.setCurrentView('v0');
    await clock.advance(600);
    scheduler.flush();

    expect(asked('v0')).toHaveLength(before);
    expect(traces.filter((t) => t.kind === 'view-switch')).toHaveLength(0);
  });
});

describe('the projections follow the view being entered (§3)', () => {
  it('carries the incoming view’s artifacts at the switch, never the outgoing view’s', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {store} = open({view: 'v0', clock, scheduler});
    await clock.advance(1);
    store.setLayers(['l']);
    await shown(store, clock, scheduler);
    await clock.advance(600);
    scheduler.flush();
    expect(store.get('artifacts').served.length).toBeGreaterThan(0);
    const inV0 = store.get('artifacts').served;

    // Cold: the view being entered has answered nothing, and says so.
    store.setCurrentView('far');
    expect(store.get('artifacts').served).toEqual([]);
    expect(store.get('artifacts').status).toBe('idle');

    // Warm: the view returned to carries its own held set, published at the switch.
    store.setView({bbox: [-1000, -500, 1000, 500], width: 800, height: 400});
    await clock.advance(600);
    scheduler.flush();
    const inFar = store.get('artifacts').served;
    expect(inFar.length).toBeGreaterThan(0);
    store.setCurrentView('v0');
    expect(store.get('artifacts').served).not.toBe(inFar);
    expect(store.get('artifacts').served.map((a) => a.tesseraId)).toEqual(inV0.map((a) => a.tesseraId));
  });

  it('holds a cold switch at loading through the first partial frame', async () => {
    // The frame that answers a switch's `loading` is the one derived from the view's own bands.
    // A cold view has none: what arrives first is a part of the request it triggered, and the
    // request's own status is the driver's to give.
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {store} = open({view: 'v0', clock, scheduler, streaming: 'far'});
    await clock.advance(1);
    await shown(store, clock, scheduler);

    store.setCurrentView('far');
    expect(store.get('status').status).toBe('loading');

    store.setView({bbox: [-1000, -500, 1000, 500], width: 800, height: 400});
    await clock.advance(600);
    scheduler.flush();

    // A frame is drawn — the part landed — and the request that carries it has not answered.
    expect(store.get('view').composition).not.toBeNull();
    expect(store.get('status').status).toBe('loading');
  });
});

describe('the replica projection reports across views, and clear() empties them all (§3, §4)', () => {
  it('counts every view holding a band, and the bytes of all of them', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {store} = open({view: 'v0', clock, scheduler});
    await clock.advance(1);
    await shown(store, clock, scheduler);
    const oneView = store.get('replica');
    expect(oneView.views).toBe(1);
    expect(oneView.bytes).toBeGreaterThan(0);

    store.setCurrentView('v1');
    await clock.advance(600);
    scheduler.flush();

    const two = store.get('replica');
    expect(two.views).toBe(2);
    expect(two.bytes).toBeGreaterThan(oneView.bytes);
    // `points` and `bands` are the current view's — what is drawable now.
    expect(two.bands).toBeGreaterThan(0);

    store.clear();

    expect(store.get('replica').views).toBe(0);
    expect(store.get('replica').bytes).toBe(0);
    expect(store.get('view').composition).toBeNull();
  });
});
