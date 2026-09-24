import {describe, expect, it, vi} from 'vitest';
import {TesseraClient, TesseraError} from '../src/client.js';
import {createStore, type Store} from '../src/store.js';
import {withVerb, type FilterDraft} from '../src/filters.js';
import type {Artifact, Layer, MembershipColumn, Meta, ViewportResponse} from '../src/types.js';
import {artifact, fakeClock, fakeScheduler, layer, meta, response as responseOf, servedResult, tile, view, scalar} from './support.js';
import {dataToWorldXY, mortonOfTile} from '../src/coords.js';
import {tileRectOfBbox} from '../src/budget.js';

/**
 * The store against a fake `TesseraClient`, a fake clock and a fake frame scheduler — no DOM, no
 * network. Every assertion is about the store's own contract: `setView`'s conversion, the stale
 * rule keyed on the content key, and the drops.
 */

const META = meta({
  views: [view('s0', {displayName: 'default', quantisation: {xMin: 0, xMax: 100, yMin: 0, yMax: 200}})],
  declaredScalars: [scalar('archive', 'u16', {category: {vocabulary: 'a', kind: 'declared', visibility: 'public'}, render: true, homes: ['rendered']})],
  filterOperands: [{column: 'archive', family: 'category', operands: ['in']}]
});

/** What the fake sees of a request: enough to answer for every tile it asked about. */
type FakeRequest = {zoom: number; bbox?: [number, number, number, number]; tiles?: bigint[]; filters?: unknown; k?: number; layers?: string[] | 'all'};

/**
 * A response that answers **every** tile the request spans — one tile of 1,000 items each, the
 * served points on the first — so a frame's coverage of a region is the client's arithmetic and
 * not the fixture's shape. `response()` below answers one tile whatever was asked, which every
 * other test relies on.
 */
function responseCovering(req: FakeRequest, contentKey: string, served = 3): ViewportResponse {
  const base = response(contentKey, 'ik', served);
  const q = META.views[0]!.quantisation;
  let prefixes: bigint[];
  if (req.tiles) prefixes = req.tiles;
  else {
    const [x0, y0] = dataToWorldXY(req.bbox![0], req.bbox![1], q);
    const [x1, y1] = dataToWorldXY(req.bbox![2], req.bbox![3], q);
    const rect = tileRectOfBbox([Math.min(x0, x1), Math.min(y0, y1), Math.max(x0, x1), Math.max(y0, y1)], req.zoom);
    prefixes = [];
    for (let y = rect.y0; y <= rect.y1; y++) for (let x = rect.x0; x <= rect.x1; x++) prefixes.push(mortonOfTile(x, y, req.zoom));
  }
  const tiles = prefixes.map((prefix, i) => tile(prefix, 1000n, {served: i === 0 ? BigInt(served) : 0n}));
  return {...base, result: {...base.result, tiles}};
}

function response(contentKey: string, identityKey = 'ik', served = 3): ViewportResponse {
  const scalars = {archive: {arrowType: 'u16' as const, values: Uint16Array.from({length: served}, () => 5)}};
  return responseOf(servedResult(served, [tile(0n, 10_000_000n)], {scalars}), {contentKey, identityKey});
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
function fakeClient(reply: (req: FakeRequest) => ViewportResponse, meta: Meta = META) {
  const viewport = vi.fn(async (_token: string, req: FakeRequest) => ({...reply(req), region: regionOf(req.filters) ? {exact: true as const, depth: null} : null}));
  const client = {
    meta: async () => meta,
    viewport,
    item: async () => ({fields: {archive: 'cs'}, externalId: null}),
    artifact: async () => ({layer: 'l', key: 'k', maskedCount: 42n, centroid: null, box: null, shape: null}),
    categories: async () => [{code: 5, key: 'cs', title: 'CS'}],
    suggest: async () => ({status: 'ok' as const, column: 'admin4', q: '', values: [], more: false}),
    close: () => {}
  } as unknown as TesseraClient;
  return {client, viewport};
}

/** Build a store and drive it to its first shown frame. */
async function warm(reply: (req: FakeRequest) => ViewportResponse, opts: {clock: ReturnType<typeof fakeClock>; scheduler: ReturnType<typeof fakeScheduler>; meta?: Meta}) {
  const {client, viewport} = fakeClient(reply, opts.meta);
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
    store.setFilters({archive: {family: 'category', keys: ['cs'], verb: 'filter'}});
    await clock.advance(600);
    scheduler.flush();
    expect(store.get('filters').expr).toEqual({archive: {in: ['cs']}});
    expect(viewport.mock.calls.length).toBeGreaterThan(before);
    // The filter reached the wire.
    const last = viewport.mock.calls.at(-1)!;
    expect((last[1] as {filters?: unknown}).filters).toEqual({archive: {in: ['cs']}});
  });

  /**
   * §5.2's two verbs, and the two claims the design turns on: **a filter never moves the mask and
   * a highlight never moves the draw.** The first is the older one — a filter narrows what is
   * served, never `visible` — and the second is what makes a highlight a highlight: the request's
   * `filters` is the same expression whether a clause is in the highlight position or absent, so
   * the cap clause, the sampling and `served` cannot see it.
   */
  it('sends a clause in the highlight position as `highlight`, leaving `filters` untouched', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {store, viewport} = await warm(() => response('ck'), {clock, scheduler});
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await clock.advance(600);
    scheduler.flush();

    store.setFilters({archive: {family: 'category', keys: ['cs'], verb: 'highlight'}});
    await clock.advance(600);
    scheduler.flush();
    const body = viewport.mock.calls.at(-1)![1] as {filters?: unknown; highlight?: unknown};
    expect(body.highlight).toEqual({archive: {in: ['cs']}});
    // Not merely different from the highlight — *nothing at all*, which is the request an
    // unhighlighted client sends and the one the draw is defined against.
    expect(body.filters ?? null).toBeNull();
    expect(store.get('filters').highlight).toEqual({archive: {in: ['cs']}});
    expect(store.get('filters').expr).toBeNull();
  });

  it('moves a clause between the two positions without it being re-entered', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {store, viewport} = await warm(() => response('ck'), {clock, scheduler});
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await clock.advance(600);
    scheduler.flush();

    const filtered: FilterDraft = {archive: {family: 'category', keys: ['cs'], verb: 'filter'}};
    store.setFilters(filtered);
    await clock.advance(600);
    scheduler.flush();
    expect((viewport.mock.calls.at(-1)![1] as {filters?: unknown}).filters).toEqual({archive: {in: ['cs']}});

    // The predicate is untouched; only the verb moves — which is the whole of §5.2's claim.
    store.setFilters(withVerb(filtered, 'archive', 'highlight'));
    await clock.advance(600);
    scheduler.flush();
    const moved = viewport.mock.calls.at(-1)![1] as {filters?: unknown; highlight?: unknown};
    expect(moved.filters ?? null).toBeNull();
    expect(moved.highlight).toEqual({archive: {in: ['cs']}});
    expect(store.get('filters').draft['archive']).toEqual({family: 'category', keys: ['cs'], verb: 'highlight'});
  });

  it('sends a member_of clause in whichever position it carries, the artifact as a decimal string', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {store, viewport} = await warm(() => response('ck'), {clock, scheduler});
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await clock.advance(600);
    scheduler.flush();

    // An id past 2^53, which is why the leaf spells it as a string: a number would round it.
    const id = 18_064_038_920_082_622_571n;
    store.setMembers([{layer: 'mesh/descriptors', artifact: id, outside: false, verb: 'highlight'}]);
    await clock.advance(600);
    scheduler.flush();
    const lit = viewport.mock.calls.at(-1)![1] as {filters?: unknown; highlight?: unknown};
    expect(lit.highlight).toEqual({member_of: {layer: 'mesh/descriptors', artifact: '18064038920082622571'}});
    expect(lit.filters ?? null).toBeNull();

    store.setMembers([{layer: 'mesh/descriptors', artifact: id, outside: true, verb: 'filter'}]);
    await clock.advance(600);
    scheduler.flush();
    const narrowed = viewport.mock.calls.at(-1)![1] as {filters?: unknown; highlight?: unknown};
    expect(narrowed.filters).toEqual({none_of: [{member_of: {layer: 'mesh/descriptors', artifact: '18064038920082622571'}}]});
    expect(narrowed.highlight ?? null).toBeNull();
  });

  it('sums the tiles’ highlighted into the strip’s third line, and says whether a highlight was asked', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {store} = await warm(() => response('ck'), {clock, scheduler});
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await clock.advance(600);
    scheduler.flush();
    // The fixture's tiles carry `highlighted` equal to `matched`, which is what the wire carries
    // for a request with no highlight — so the figure is right and `highlighting` is what says
    // there was no question.
    expect(store.get('view').highlighted.value).toBe(store.get('view').matched.value);
    expect(store.get('view').highlighting).toBe(false);

    store.setFilters({archive: {family: 'category', keys: ['cs'], verb: 'highlight'}});
    await clock.advance(600);
    scheduler.flush();
    expect(store.get('view').highlighting).toBe(true);
  });

  it('never names a filter layer in a viewport request, however it is asked for', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const layers = [
      layer('clusters/kmeans', {computedContent: ['centroid', 'box', 'hull'], shape: 'derived', suppliedContent: ['topic']}),
      layer('mesh/descriptors', {hierarchy: {kind: 'dag', pruneChildren: false}, suppliedContent: ['name']})
    ];
    const {store, viewport} = await warm(() => response('ck'), {clock, scheduler, meta: {...META, layers}});
    // Asked for by name, which is what a host driving `setLayers` directly would do.
    store.setLayers(['clusters/kmeans', 'mesh/descriptors']);
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await clock.advance(600);
    scheduler.flush();
    const asked = viewport.mock.calls.map((c) => (c[1] as {layers?: string[]}).layers ?? []);
    expect(asked.every((l) => !l.includes('mesh/descriptors'))).toBe(true);
    // It is still a layer: the roster keeps it, and the client's own list does too.
    expect(store.get('meta')!.layers.map((l) => l.name)).toContain('mesh/descriptors');
    expect(store.get('artifacts').layers).toEqual(['clusters/kmeans']);
  });

  it('browses in the store’s own view, and a caller’s explicit undefined does not clobber it', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const browse = vi.fn(async () => ({artifacts: [], parents: [], next: null}));
    const {client} = fakeClient(() => response('ck'));
    (client as unknown as {browse: typeof browse}).browse = browse;
    const store = createStore({viewerUrl: 'http://viewer', token: 'tok', client, clock, scheduler, prefetch: false, replica: {revalidateAfterMs: Infinity}});
    await clock.advance(1);

    await store.browse({layer: 'mesh/descriptors'});
    expect((browse.mock.calls[0] as unknown as [string, {view: string}])[1].view).toBe('s0');

    // A caller passing the field explicitly absent — which a spread of a partial request produces
    // — must not leave the request without a view: a masked count is per view.
    await store.browse({layer: 'mesh/descriptors', view: undefined});
    expect((browse.mock.calls[1] as unknown as [string, {view: string}])[1].view).toBe('s0');

    // And a caller naming another view is answered in it.
    await store.browse({layer: 'mesh/descriptors', view: 'other'});
    expect((browse.mock.calls[2] as unknown as [string, {view: string}])[1].view).toBe('other');
  });

  it('composes the filter every request carries, the drawn region included', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {store} = await warm(() => response('ck'), {clock, scheduler});
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await clock.advance(600);
    scheduler.flush();
    expect(store.requestFilters()).toBeNull();

    // `filters.expr` is one of the three sources; a reader taking it for the whole would miss the
    // other two, which is what the hierarchy panel's staleness check did.
    store.setMembers([{layer: 'l', artifact: 7n, outside: false, verb: 'filter'}]);
    await clock.advance(600);
    scheduler.flush();
    expect(store.get('filters').expr).toBeNull();
    expect(store.requestFilters()).toEqual({member_of: {layer: 'l', artifact: '7'}});
    // JSON-safe by construction: the identifier is a decimal string, so this hashes and logs.
    expect(() => JSON.stringify(store.requestFilters())).not.toThrow();
  });

  it('clears the stand-in marks and the tiles on clear(), as it does the exact bands', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {store, viewport} = await warm(() => response('ck'), {clock, scheduler});
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await clock.advance(600);
    scheduler.flush();
    // Zoomed in with the request left unanswered: the held coarser bands stand in.
    viewport.mockImplementation(() => new Promise(() => {}));
    store.setView({bbox: [0, 0, 1, 2], width: 400, height: 400});
    await clock.advance(600);
    scheduler.flush();
    expect(store.get('marks').standIn.length).toBeGreaterThan(0);
    expect(store.get('tiles').tiles.length).toBeGreaterThan(0);

    store.clear();
    expect(store.get('marks').standIn).toEqual([]);
    expect(store.get('tiles').tiles).toEqual([]);
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

describe('suggest in the store', () => {
  it('asks under the store’s token and view, and clear() drops every column’s held page, refusal and debounce dedupe', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {client} = fakeClient(() => response('ck'));
    const suggest = vi.fn(async (_token: string, column: string, q: string) => ({status: 'ok' as const, column, q, values: [], more: false}));
    (client as unknown as {suggest: typeof suggest}).suggest = suggest;
    const store = createStore({viewerUrl: 'http://viewer', token: 'tok', client, clock, scheduler, prefetch: false, replica: {revalidateAfterMs: Infinity}});
    await clock.advance(1);

    store.suggest('archive', '');
    await clock.advance(200);
    expect(suggest).toHaveBeenCalledWith('tok', 'archive', '', {view: 's0'});
    expect(store.get('filters').suggestions['archive']).toEqual({q: '', values: [], more: false});

    store.clear();
    expect(store.get('filters').suggestions).toEqual({});
    expect(store.get('filters').suggestErrors).toEqual({});

    // The dedupe is cleared alongside the projection — a re-ask for the identical q the mask
    // change just invalidated must still reach the client, not read as already-answered.
    store.suggest('archive', '');
    await clock.advance(200);
    expect(suggest).toHaveBeenCalledTimes(2);
    expect(store.get('filters').suggestions['archive']).toEqual({q: '', values: [], more: false});
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
      artifact(1n, {layer: 'clusters/a', key: 'c1', maskedCount: 5n, centroid: [1, 2]}),
      artifact(2n, {layer: 'clusters/a', key: 'c2', maskedCount: 7n, centroid: [3, 4]})
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
    const cluster = (id: bigint, centroid: [number, number]) =>
      artifact(id, {layer: 'clusters/a', key: `c${id}`, maskedCount: 5n, centroid});
    let served = [cluster(1n, [1, 2]), cluster(2n, [3, 4])];
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
    served = [...served, cluster(3n, [5, 6])];
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
    store.setFilters({archive: {family: 'category', keys: ['cs'], verb: 'filter'}});
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

  it('gives no region-alone figure while a member_of clause in the filter position narrows the frame', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {store} = await warm(() => response('ck1'), {clock, scheduler});
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await clock.advance(600);
    scheduler.flush();
    store.select({kind: 'box', bbox: [0, 0, 1, 1]});
    await clock.advance(600);
    scheduler.flush();
    expect(store.get('region')?.visible).toEqual(store.get('region')?.matched);

    store.setMembers([{layer: 'l', artifact: 7n, outside: false, verb: 'filter'}]);
    await clock.advance(600);
    scheduler.flush();
    expect(store.get('region')?.status).toBe('shown');
    expect(store.get('region')?.visible).toBeNull();

    // In the highlight position the clause moves no count, so the region alone is the figure again.
    store.setMembers([{layer: 'l', artifact: 7n, outside: false, verb: 'highlight'}]);
    await clock.advance(600);
    scheduler.flush();
    expect(store.get('region')?.visible).toEqual(store.get('region')?.matched);
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


describe('the store holds the drawn shape by identifier', () => {
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

  /** The drill-down answers the shape beside the box, and the map draws from `artifacts.shapes`. */
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

  it('forgets every held shape on clear', async () => {
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

describe('clear() and a refused request reach the region and the shapes', () => {
  const SHAPED = meta({
    ...META,
    layers: [layer('regions', {membership: 'spatial', computedContent: ['centroid', 'box'], shape: 'predicate'})]
  });

  it('drops the region and every shape on clear, a predicate shape included', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const parts: [number, number][][][] = [[[[0, 0], [10, 0], [10, 10]]]];
    const {client} = fakeClient((req) => {
      const r = response('ck');
      return (req.layers ?? []).length === 0 ? r : {...r, result: {...r.result, artifacts: [artifact(2n, {layer: 'regions'})]}};
    }, SHAPED);
    Object.assign(client, {artifact: vi.fn(async () => ({layer: 'regions', key: null, maskedCount: 1n, centroid: null, box: null, shape: parts}))});
    const store = createStore({viewerUrl: 'http://viewer', token: 'tok', client, clock, scheduler, prefetch: false, replica: {revalidateAfterMs: Infinity}});
    store.setLayers(['regions']);
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await clock.advance(600);
    scheduler.flush();
    await clock.advance(600);
    expect(store.get('artifacts').served.map((a) => a.tesseraId)).toEqual([2n]);
    store.needShape(2n);
    await clock.advance(1);
    store.select({kind: 'box', bbox: [0, 0, 1, 1]});
    expect(store.get('artifacts').shapes.size).toBe(1);
    expect(store.get('region')).not.toBeNull();

    store.clear();
    expect(store.get('artifacts').shapes.size).toBe(0);
    expect(store.get('region')).toBeNull();
  });

  it('shows the region as refused when the request carrying it is refused', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    let refuse = false;
    const {store} = await warm(
      () => {
        if (refuse) throw new TesseraError(400, 'bad-region', 'too many vertices');
        return response('ck');
      },
      {clock, scheduler}
    );
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await clock.advance(600);
    scheduler.flush();

    refuse = true;
    store.select({kind: 'box', bbox: [0, 0, 1, 1]});
    await clock.advance(5_000);
    scheduler.flush();
    expect(store.get('status').status).toBe('refused');
    expect(store.get('region')).toMatchObject({status: 'refused', refusal: {code: 'bad-region'}, visible: null});
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

describe('the layers drawn and the layer coloured by are two settings', () => {
  const drawn = (name: string, extra: Partial<Layer> = {}): Layer => layer(name, {computedContent: ['centroid', 'box'], ...extra});
  const levels = [0, 1, 2, 3].map((level) => ({level, title: `level ${level}`, zoom: null}));
  const LAYERED: Meta = {
    ...META,
    layers: [
      drawn('topics', {hierarchy: {kind: 'tiered', pruneChildren: false}, levels}),
      drawn('topic_names', {computedContent: [], suppliedContent: ['label'], depsOn: ['topics']}),
      drawn('kmeans'),
      layer('mesh')
    ]
  };
  const row = (layerName: string, id: bigint, target: bigint | null = null): Artifact =>
    artifact(id, {
      layer: layerName,
      key: `${layerName}-${id}`,
      maskedCount: 3n,
      centroid: target === null ? [2 ** 30, 2 ** 30] : null,
      content: target === null ? [] : ['a name'],
      target
    });
  const ROWS: Record<string, Artifact[]> = {topics: [row('topics', 11n)], topic_names: [row('topic_names', 21n, 11n)], kmeans: [row('kmeans', 31n)]};

  /**
   * Answers as the server does: the artifacts of every layer named, and on a point request a
   * membership column per named layer that has members, every point in its first artifact.
   */
  function answer(req: FakeRequest): ViewportResponse {
    const r = response('ck');
    const named = Array.isArray(req.layers) ? req.layers : [];
    const membership: Record<string, MembershipColumn> = {};
    if (req.k !== 0) {
      for (const name of named) {
        const first = ROWS[name]?.[0];
        if (first && first.target === null) membership[name] = {index: Uint16Array.from(r.result.ids, () => 1), ids: BigUint64Array.of(first.tesseraId)};
      }
    }
    return {...r, result: {...r.result, membership, artifacts: named.flatMap((n) => ROWS[n] ?? []), artifactsIdentity: null}};
  }

  async function open(traces: {kind: string; fields: Record<string, number | string>}[] = []) {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {client, viewport} = fakeClient(answer, LAYERED);
    const store = createStore({viewerUrl: 'http://viewer', token: 'tok', client, clock, scheduler, prefetch: false, replica: {revalidateAfterMs: Infinity}, instruments: {onTrace: (kind, fields) => traces.push({kind, fields})}});
    await clock.advance(1);
    const settle = async () => {
      await clock.advance(600);
      scheduler.flush();
      await clock.advance(600);
      scheduler.flush();
    };
    const asked = () => viewport.mock.calls.map((c) => c[1] as FakeRequest & {levels?: number[]; artifactBudget?: number});
    return {store, settle, asked};
  }

  const layersOf = (artifacts: readonly Artifact[]) => [...new Set(artifacts.map((a) => a.layer))];

  it('colours by a layer with nothing drawn: the request names the layer alone, points carry it, and nothing of it is drawn', async () => {
    const {store, settle, asked} = await open();
    store.setColourBy('cluster:topics');
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await settle();

    const points = asked().filter((r) => r.k !== 0);
    expect(points.length).toBeGreaterThan(0);
    for (const r of points) {
      // The colour layer alone: its labels layer would be drawn, and nothing of it is.
      expect(r.layers).toEqual(['topics']);
      expect(r.levels).toEqual([0, 1, 2, 3]);
      expect(r.artifactBudget).toBeGreaterThan(0);
    }
    expect(asked().some((r) => r.k === 0 && Array.isArray(r.layers) && r.layers.includes('topics'))).toBe(true);

    const bands = store.get('marks').bands;
    expect(bands.length).toBeGreaterThan(0);
    const a = store.get('artifacts');
    for (const band of bands) {
      const m = band.membership['topics']!;
      expect(m).toBeDefined();
      for (const ordinal of m.distinct) expect(a.table.resolve(ordinal, a.colours)).not.toBe(0);
    }
    // Drawn: nothing. Coloured: the layer's rows.
    expect(a.layers).toEqual([]);
    expect(a.served).toEqual([]);
    expect(a.lineage.roots).toEqual([]);
    expect(layersOf(a.colourServed)).toEqual(['topics']);
  });

  it('draws a layer with its labels and colours by another, and each surface sees its own', async () => {
    const {store, settle, asked} = await open();
    store.setLayers(['topics']);
    store.setColourBy('cluster:kmeans');
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await settle();

    for (const r of asked().filter((x) => x.k !== 0)) expect(r.layers).toEqual(['topics', 'topic_names', 'kmeans']);
    const a = store.get('artifacts');
    expect(a.layers).toEqual(['topics', 'topic_names']);
    expect(layersOf(a.served)).toEqual(['topics', 'topic_names']);
    expect(layersOf(a.colourServed)).toEqual(['kmeans']);
    expect(Object.keys(store.get('marks').bands[0]!.membership).sort()).toEqual(['kmeans', 'topics']);
  });

  it('asks for nothing when the colour layer is already drawn, and for the layer when it is not', async () => {
    const {store, settle, asked} = await open();
    store.setLayers(['topics']);
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await settle();
    const before = asked().length;

    store.setColourBy('cluster:topics');
    await settle();
    expect(asked().length).toBe(before);
    expect(layersOf(store.get('artifacts').colourServed)).toEqual(['topics']);

    store.setColourBy('cluster:kmeans');
    await settle();
    const after = asked().slice(before);
    expect(after.some((r) => r.k === 0 && Array.isArray(r.layers) && r.layers.includes('kmeans'))).toBe(true);
    expect(after.some((r) => r.k !== 0 && Array.isArray(r.layers) && r.layers.includes('kmeans'))).toBe(true);
    expect(store.get('artifacts').layers).toEqual(['topics', 'topic_names']);
  });

  it('ignores and traces a colour layer meta does not list as one that can colour', async () => {
    const traces: {kind: string; fields: Record<string, number | string>}[] = [];
    const {store, settle, asked} = await open(traces);
    store.setColourBy('cluster:nope');
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await settle();
    store.setColourBy('cluster:topic_names');
    await settle();
    expect(asked().every((r) => !Array.isArray(r.layers) || r.layers.length === 0)).toBe(true);
    expect(traces.filter((t) => t.kind === 'colour-by').map((t) => t.fields.layer)).toEqual(['nope', 'topic_names']);
    expect(store.get('legend').colourBy).toBe('cluster:topic_names');
  });
});

describe('the token', () => {
  it('asks the supplier for a new token before the one it holds expires, and asks with it', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {client, viewport} = fakeClient(() => response('ck'));
    let issued = 0;
    const authorise = vi.fn(async () => ({token: `t${++issued}`, expiresAt: (Date.now() + 60_000) / 1000}));
    const store = createStore({viewerUrl: 'http://viewer', authorise, client, clock, scheduler, prefetch: false, replica: {revalidateAfterMs: Infinity}});
    await clock.advance(1);
    expect(authorise).toHaveBeenCalledTimes(1);

    // A minute's token is renewed within its minute, with no request in between to prompt it.
    await clock.advance(59_000);
    expect(authorise).toHaveBeenCalledTimes(2);

    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await clock.advance(600);
    scheduler.flush();
    expect(viewport.mock.calls.at(-1)![0]).toBe('t2');
  });

  it('stops renewing once disposed, even with a renewal in flight', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {client} = fakeClient(() => response('ck'));
    let answer: (() => void) | null = null;
    const authorise = vi.fn(async () => {
      // The second ask is held open until the store has been disposed.
      if (authorise.mock.calls.length === 2) await new Promise<void>((resolve) => (answer = resolve));
      return {token: `t${authorise.mock.calls.length}`, expiresAt: (Date.now() + 60_000) / 1000};
    });
    const store = createStore({viewerUrl: 'http://viewer', authorise, client, clock, scheduler, prefetch: false, replica: {revalidateAfterMs: Infinity}});
    await clock.advance(1);
    await clock.advance(59_000);
    expect(authorise).toHaveBeenCalledTimes(2);

    store.dispose();
    answer!();
    await clock.advance(600_000);
    expect(authorise).toHaveBeenCalledTimes(2);
    expect(clock.pending).toBe(0);
  });

  it('forgets derived shapes and hovered records when the token changes, and keeps predicate shapes', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const parts: [number, number][][][] = [[[[0, 0], [10, 0], [10, 10]]]];
    const served = [artifact(1n, {layer: 'hulls'}), artifact(2n, {layer: 'regions'})];
    const {client} = fakeClient((req) => {
      const r = response('ck');
      return (req.layers ?? []).length === 0 ? r : {...r, result: {...r.result, artifacts: served}};
    }, meta({
      ...META,
      layers: [
        layer('hulls', {computedContent: ['centroid', 'box', 'hull'], shape: 'derived'}),
        layer('regions', {membership: 'spatial', computedContent: ['centroid', 'box'], shape: 'predicate'})
      ]
    }));
    const shapeOf = vi.fn(async () => ({layer: 'l', key: null, maskedCount: 1n, centroid: null, box: null, shape: parts}));
    const item = vi.fn(async (token: string) => ({fields: {asked: token}, externalId: null, views: [], scoped: {}, labels: []}));
    Object.assign(client, {artifact: shapeOf, item});
    let issued = 0;
    const authorise = vi.fn(async () => ({token: `t${++issued}`, expiresAt: (Date.now() + 60_000) / 1000}));
    const store = createStore({viewerUrl: 'http://viewer', authorise, client, clock, scheduler, prefetch: false, replica: {revalidateAfterMs: Infinity}});
    store.setLayers(['hulls', 'regions']);
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await clock.advance(600);
    scheduler.flush();
    await clock.advance(600);
    expect(store.get('artifacts').served.map((a) => a.tesseraId)).toEqual([1n, 2n]);
    store.needShape(1n);
    store.needShape(2n);
    await clock.advance(1);
    expect([...store.get('artifacts').shapes.keys()]).toEqual([1n, 2n]);
    expect(await store.describe(7n)).toEqual({asked: 't1'});

    // The renewal hands over another token, so another principal as far as the store knows.
    await clock.advance(30_000);
    expect(authorise).toHaveBeenCalledTimes(2);
    expect([...store.get('artifacts').shapes.keys()]).toEqual([2n]);
    expect(await store.describe(7n)).toEqual({asked: 't2'});
  });

  it('answers every verb called before the first token lands, from one supplier call', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {client} = fakeClient(() => response('ck'));
    const shape: [number, number][][][] = [[[[0, 0], [10, 0], [10, 10]]]];
    const detail = {fields: {archive: 'cs'}, externalId: null, views: [], scoped: {}, labels: []};
    Object.assign(client, {
      item: vi.fn(async () => detail),
      artifact: vi.fn(async () => ({layer: 'l', key: null, maskedCount: 1n, centroid: null, box: null, shape})),
      suggest: vi.fn(async (_t: string, column: string, q: string) => ({status: 'ok' as const, column, q, values: [], more: false}))
    });
    let land: (() => void) | null = null;
    const authorise = vi.fn(async () => {
      await new Promise<void>((resolve) => (land = resolve));
      return {token: 't1', expiresAt: (Date.now() + 3_600_000) / 1000};
    });
    const store = createStore({viewerUrl: 'http://viewer', authorise, client, clock, scheduler, prefetch: false, replica: {revalidateAfterMs: Infinity}});
    // Opening an artifact clears the picked item, so the item is read as it lands.
    const items: unknown[] = [];
    store.subscribe('selection', (selection) => items.push(selection.item));

    const picked = store.pick(7n);
    const described = store.describe(8n);
    const opened = store.openArtifact(9n);
    store.needShape(10n);
    store.suggest('archive', 'c');
    await clock.advance(1_000);
    land!();
    await Promise.all([picked, opened]);
    await clock.advance(1_000);

    expect(authorise).toHaveBeenCalledTimes(1);
    expect(items).toContainEqual({id: 7n, detail});
    expect(await described).toEqual(detail.fields);
    expect(store.get('selection').artifact?.id).toBe(9n);
    expect(new Set(store.get('artifacts').shapes.keys())).toEqual(new Set([9n, 10n]));
    expect(store.get('filters').suggestions.archive?.q).toBe('c');
  });

  it('refuses every verb called before a first token that is refused', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {client} = fakeClient(() => response('ck'));
    const authorise = vi.fn(async () => {
      throw new TesseraError(401, 'bad-credential', 'refused');
    });
    const store = createStore({viewerUrl: 'http://viewer', authorise, client, clock, scheduler, prefetch: false, replica: {revalidateAfterMs: Infinity}});

    await store.pick(7n);
    expect(await store.describe(8n)).toBeNull();
    store.suggest('archive', 'c');
    await clock.advance(1_000);

    expect(store.get('selection').itemRefusal?.code).toBe('bad-credential');
    expect(store.get('filters').suggestErrors.archive?.code).toBe('bad-credential');
    // The artifact route names the view, which the store knows once it has read its meta.
    await store.openArtifact(9n);
    expect(store.get('selection').artifactRefusal?.code).toBe('bad-credential');
  });

  it('asks, after the first token, under the view the store read from meta', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {client} = fakeClient(() => response('ck'), meta({...META, views: [view('s1', {quantisation: META.views[0]!.quantisation})]}));
    const artifact = vi.fn(async () => ({layer: 'l', key: null, maskedCount: 1n, centroid: null, box: null, shape: null}));
    const suggest = vi.fn(async (_t: string, column: string, q: string) => ({status: 'ok' as const, column, q, values: [], more: false}));
    const browse = vi.fn(async () => ({artifacts: [], parents: [], next: null}));
    Object.assign(client, {artifact, suggest, browse});
    let land: (() => void) | null = null;
    const authorise = vi.fn(async () => {
      await new Promise<void>((resolve) => (land = resolve));
      return {token: 't1', expiresAt: (Date.now() + 3_600_000) / 1000};
    });
    const store = createStore({viewerUrl: 'http://viewer', authorise, client, clock, scheduler, prefetch: false, replica: {revalidateAfterMs: Infinity}});

    const opened = store.openArtifact(9n);
    store.needShape(10n);
    store.suggest('archive', 'c');
    const browsed = store.browse({layer: 'l'});
    await clock.advance(1_000);
    land!();
    await Promise.all([opened, browsed]);
    await clock.advance(1_000);

    const views = [...artifact.mock.calls, ...suggest.mock.calls, ...browse.mock.calls].map((call) => {
      const last = call.at(-1) as {view?: string};
      return last.view;
    });
    expect(views).toEqual(['s1', 's1', 's1', 's1']);
  });

  it('stops every verb waiting on the first token when the store is disposed', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {client, viewport} = fakeClient(() => response('ck'));
    const calls: string[] = [];
    for (const verb of ['meta', 'item', 'artifact', 'suggest', 'browse'] as const) {
      const original = (client as unknown as Record<string, (...args: unknown[]) => unknown>)[verb]!;
      Object.assign(client, {[verb]: (...args: unknown[]) => (calls.push(verb), original(...args))});
    }
    let land: (() => void) | null = null;
    const authorise = vi.fn(async () => {
      await new Promise<void>((resolve) => (land = resolve));
      return {token: 't1', expiresAt: (Date.now() + 3_600_000) / 1000};
    });
    const store = createStore({viewerUrl: 'http://viewer', authorise, client, clock, scheduler, prefetch: false, replica: {revalidateAfterMs: Infinity}});
    const before = store.get('selection');

    const waiting = [store.pick(7n), store.openArtifact(9n), store.describe(8n), store.browse({layer: 'l'}).catch(() => null)];
    store.suggest('archive', 'c');
    await clock.advance(1_000);
    store.dispose();
    land!();
    await Promise.all(waiting);
    await clock.advance(1_000);
    // Asked after the dispose, each returns at once.
    await Promise.all([store.pick(7n), store.openArtifact(9n), store.describe(8n)]);

    expect(authorise).toHaveBeenCalledTimes(1);
    expect(calls).toEqual([]);
    expect(viewport).not.toHaveBeenCalled();
    expect(store.get('selection')).toBe(before);
    expect(store.get('filters').suggestErrors).toEqual({});
  });

  it('warms up again once a supplier call succeeds after the first failed', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {client} = fakeClient(() => response('ck'));
    const artifact = vi.fn(async () => ({layer: 'l', key: null, maskedCount: 1n, centroid: null, box: null, shape: null}));
    Object.assign(client, {artifact});
    let refuse = true;
    const authorise = vi.fn(async () => {
      if (refuse) throw new TesseraError(401, 'bad-credential', 'refused');
      return {token: 't1', expiresAt: (Date.now() + 3_600_000) / 1000};
    });
    const store = createStore({viewerUrl: 'http://viewer', authorise, client, clock, scheduler, prefetch: false, replica: {revalidateAfterMs: Infinity}});
    await clock.advance(1);
    expect(store.get('status').status).toBe('refused');
    expect(store.get('meta')).toBeNull();

    refuse = false;
    await store.openArtifact(9n);
    expect(store.get('meta')).not.toBeNull();
    expect(store.get('selection').artifact?.id).toBe(9n);
    expect((artifact.mock.calls[0] as unknown[])[2]).toMatchObject({view: 's0'});
  });

  it('refuses the artifact channel’s ask when its token cannot be renewed', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {client} = fakeClient(() => response('ck'), meta({...META, layers: [layer('l', {computedContent: ['centroid', 'box']})]}));
    let issued = 0;
    // A token a second from expiry is renewed at every ask; the second renewal is refused.
    const authorise = vi.fn(async () => {
      if (++issued > 1) throw new TesseraError(401, 'bad-credential', 'refused');
      return {token: 't1', expiresAt: (Date.now() + 1_000) / 1000};
    });
    const store = createStore({viewerUrl: 'http://viewer', authorise, client, clock, scheduler, prefetch: false, replica: {revalidateAfterMs: Infinity}});
    await clock.advance(1);
    store.setLayers(['l']);
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await clock.advance(600);
    scheduler.flush();
    await clock.advance(600);
    expect(store.get('artifacts')).toMatchObject({status: 'refused', refusal: {code: 'bad-credential'}});
  });

  it('reports the session expired when the server refuses the token it holds as expired', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    let refuse = false;
    const {store, viewport} = await warm(
      () => {
        if (refuse) throw new TesseraError(401, 'expired-token', 'the token has expired');
        return response('ck');
      },
      {clock, scheduler}
    );
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await clock.advance(600);
    scheduler.flush();
    expect(store.get('status').expired).toBe(false);
    const asked = viewport.mock.calls.length;

    // Zoomed far in, so the held frame cannot answer and a request goes out.
    refuse = true;
    store.setView({bbox: [0, 0, 1, 2], width: 400, height: 400});
    await clock.advance(5_000);
    scheduler.flush();
    expect(viewport.mock.calls.length).toBeGreaterThan(asked);
    expect(store.get('status')).toMatchObject({status: 'refused', expired: true, refusal: {code: 'expired-token'}});
  });
});

describe('the item a click opens and the record a hover names', () => {
  it('puts a picked item in the selection, and a refusal in its place', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {client} = fakeClient(() => response('ck'));
    const detail = {fields: {archive: 'cs'}, externalId: null, views: [], scoped: {}, labels: []};
    const item = vi.fn(async (_token: string, id: bigint) => {
      if (id === 404n) throw new TesseraError(404, 'not-found', 'no such item');
      return detail;
    });
    (client as unknown as {item: typeof item}).item = item;
    const store = createStore({viewerUrl: 'http://viewer', token: 'tok', client, clock, scheduler, prefetch: false, replica: {revalidateAfterMs: Infinity}});
    await clock.advance(1);

    await store.pick(7n);
    expect(store.get('selection')).toMatchObject({item: {id: 7n, detail}, itemRefusal: null});

    await store.pick(404n);
    expect(store.get('selection')).toMatchObject({item: null, itemRefusal: {code: 'not-found'}});
  });

  it('forgets the item and the artifact on a clear, including one still on its way', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {client} = fakeClient(() => response('ck'));
    let answer: (() => void) | null = null;
    const item = vi.fn(async (_token: string, id: bigint) => {
      if (id === 8n) await new Promise<void>((resolve) => (answer = resolve));
      return {fields: {title: `paper ${id}`}, externalId: null, views: [], scoped: {}, labels: []};
    });
    (client as unknown as {item: typeof item}).item = item;
    const store = createStore({viewerUrl: 'http://viewer', token: 'tok', client, clock, scheduler, prefetch: false, replica: {revalidateAfterMs: Infinity}});
    await clock.advance(1);

    await store.openArtifact(9n);
    await store.pick(7n);
    expect(store.get('selection').item?.id).toBe(7n);
    expect(store.get('selection').artifact?.id).toBe(9n);

    store.clear();
    expect(store.get('selection')).toEqual({item: null, itemRefusal: null, artifact: null, artifactRefusal: null});

    // Asked before the next clear and answered after it.
    const late = store.pick(8n);
    await clock.advance(1);
    store.clear();
    answer!();
    await late;
    expect(store.get('selection').item).toBeNull();
  });

  it('does not show a refusal or an artifact that lands after a clear', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {client} = fakeClient(() => response('ck'));
    const answers: (() => void)[] = [];
    const held = () => new Promise<void>((resolve) => answers.push(resolve));
    Object.assign(client, {
      item: vi.fn(async () => {
        await held();
        throw new TesseraError(404, 'not-found', 'no such item');
      }),
      artifact: vi.fn(async () => {
        await held();
        return {layer: 'l', key: null, maskedCount: 1n, centroid: null, box: null, shape: null};
      })
    });
    const store = createStore({viewerUrl: 'http://viewer', token: 'tok', client, clock, scheduler, prefetch: false, replica: {revalidateAfterMs: Infinity}});
    await clock.advance(1);

    const picked = store.pick(7n);
    const opened = store.openArtifact(9n);
    await clock.advance(1);
    expect(answers).toHaveLength(2);
    store.clear();
    for (const answer of answers) answer();
    await Promise.all([picked, opened]);
    expect(store.get('selection')).toEqual({item: null, itemRefusal: null, artifact: null, artifactRefusal: null});
  });

  it('writes no projection for a hovered record, and asks again after a clear', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {client} = fakeClient(() => response('ck'));
    const item = vi.fn(async (_token: string, id: bigint) => ({fields: {title: `paper ${id}`}, externalId: null, views: [], scoped: {}, labels: []}));
    (client as unknown as {item: typeof item}).item = item;
    const store = createStore({viewerUrl: 'http://viewer', token: 'tok', client, clock, scheduler, prefetch: false, replica: {revalidateAfterMs: Infinity}});
    await clock.advance(1);

    expect(await store.describe(7n)).toEqual({title: 'paper 7'});
    expect(await store.describe(7n)).toEqual({title: 'paper 7'});
    expect(item).toHaveBeenCalledTimes(1);
    expect(store.get('selection').item).toBeNull();

    store.clear();
    await clock.advance(1);
    await store.describe(7n);
    expect(item).toHaveBeenCalledTimes(2);
  });
});
