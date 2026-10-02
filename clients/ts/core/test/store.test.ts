import {describe, expect, it, vi} from 'vitest';
import {TesseraClient, TesseraError} from '../src/client.js';
import {createStore, type Store} from '../src/store.js';
import type {FilterDraft} from '../src/filters.js';
import {withMember} from '../src/members.js';
import {artifactName} from '../src/names.js';
import type {Artifact, Layer, MembershipColumn, Meta, ViewportPart, ViewportResponse} from '../src/types.js';
import {artifact, fakeClock, fakeScheduler, layer, meta, response as responseOf, servedResult, tile, view, scalar} from './support.js';
import {dataToWorldXY, mortonOfTile} from '../src/coords.js';
import {tileRectOfBbox} from '../src/budget.js';

/**
 * The store against a fake `TesseraClient`, a fake clock and a fake frame scheduler, with no DOM
 * and no network.
 */

const META = meta({
  views: [view('s0', {displayName: 'default', quantisation: {xMin: 0, xMax: 100, yMin: 0, yMax: 200}})],
  declaredScalars: [scalar('archive', 'u16', {category: {vocabulary: 'a', kind: 'declared', visibility: 'public'}, render: true, homes: ['rendered']})],
  filterOperands: [{column: 'archive', family: 'category', operands: ['in']}]
});

/** What the fake sees of a request. */
type FakeRequest = {view?: string; zoom: number; bbox?: [number, number, number, number]; tiles?: bigint[]; filters?: unknown; k?: number; layers?: string[] | 'all'; pointRows?: 'full' | 'highlight' | readonly string[]};

/**
 * A response that answers every tile the request spans, 1,000 items each with the served points on
 * the first, so a frame's coverage of a region is the client's arithmetic. `response()` answers one
 * tile whatever was asked.
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

/** The `region` leaf anywhere in a filter expression, or null. */
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
 * A fake client with the verbs the store calls and a log of viewport requests. A request with a
 * `region` leaf is answered with the verdict `exact`.
 */
function fakeClient(reply: (req: FakeRequest) => ViewportResponse, meta: Meta = META) {
  const viewport = vi.fn(async (_token: string, req: FakeRequest) => ({...reply(req), region: regionOf(req.filters) ? {exact: true as const, depth: null} : null}));
  const client = {
    meta: async () => meta,
    viewport,
    item: async () => ({fields: {archive: 'cs'}}),
    artifact: async () => ({layer: 'l', key: 'k', maskedCount: 42n, centroid: null, box: null, shape: null}),
    categories: async () => [{code: 5, key: 'cs', title: 'CS'}],
    suggest: async () => ({status: 'ok' as const, column: 'admin4', q: '', values: [], more: false}),
    close: () => {}
  } as unknown as TesseraClient;
  return {client, viewport};
}

/** Builds a store and drives it to its first shown frame. */
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
    // The tighter axis sets the zoom: 800/256 = 400/128 = 3.125.
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

    // The interval lapses and the same view is scheduled: a count-only request observes ck-2
    // without redrawing, so the store marks itself stale.
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
  it('drops the replica and marks a refetch on setFilters: the identity key excludes filters', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {store, viewport} = await warm(() => response('ck'), {clock, scheduler});
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await clock.advance(600);
    scheduler.flush();
    const before = viewport.mock.calls.length;

    // A filter changes what is served but not the identity key, so the store drops its bands itself
    // and asks again.
    store.setFilters({filter: {archive: {family: 'category', keys: ['cs']}}, highlight: {}});
    await clock.advance(600);
    scheduler.flush();
    expect(store.get('filters').expr).toEqual({archive: {in: ['cs']}});
    expect(viewport.mock.calls.length).toBeGreaterThan(before);
    // The filter reached the wire.
    const last = viewport.mock.calls.at(-1)!;
    expect((last[1] as {filters?: unknown}).filters).toEqual({archive: {in: ['cs']}});
  });

  /**
   * A filter does not move the mask and a highlight does not move the draw: the request's `filters`
   * is the same whether a clause is in the highlight position or absent.
   */
  it('sends a clause in the highlight position as `highlight`, leaving `filters` untouched', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {store, viewport} = await warm(() => response('ck'), {clock, scheduler});
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await clock.advance(600);
    scheduler.flush();

    store.setFilters({filter: {}, highlight: {archive: {family: 'category', keys: ['cs']}}});
    await clock.advance(600);
    scheduler.flush();
    const body = viewport.mock.calls.at(-1)![1] as {filters?: unknown; highlight?: unknown};
    expect(body.highlight).toEqual({archive: {in: ['cs']}});
    // No highlight at all, as an unhighlighted client sends.
    expect(body.filters ?? null).toBeNull();
    expect(store.get('filters').highlight).toEqual({archive: {in: ['cs']}});
    expect(store.get('filters').expr).toBeNull();
  });

  it('sends a filter and a highlight on one column as both expressions', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {store, viewport} = await warm(() => response('ck'), {clock, scheduler});
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await clock.advance(600);
    scheduler.flush();

    store.setFilters({filter: {archive: {family: 'category', keys: ['cs.LG', 'cs.CV']}}, highlight: {archive: {family: 'category', keys: ['cs.CV']}}});
    await clock.advance(600);
    scheduler.flush();
    const body = viewport.mock.calls.at(-1)![1] as {filters?: unknown; highlight?: unknown};
    expect(body.filters).toEqual({archive: {in: ['cs.LG', 'cs.CV']}});
    expect(body.highlight).toEqual({archive: {in: ['cs.CV']}});
    expect(store.get('view').highlighting).toBe(true);
  });

  it('sends a member_of filter and highlight on one artifact as both expressions', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {store, viewport} = await warm(() => response('ck'), {clock, scheduler});
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await clock.advance(600);
    scheduler.flush();

    const filtered = withMember([], {layer: 'l', artifact: 7n, outside: false, verb: 'filter'});
    store.setMembers(withMember(filtered, {layer: 'l', artifact: 7n, outside: false, verb: 'highlight'}));
    await clock.advance(600);
    scheduler.flush();
    const body = viewport.mock.calls.at(-1)![1] as {filters?: unknown; highlight?: unknown};
    expect(body.filters).toEqual({member_of: {layer: 'l', artifact: '7'}});
    expect(body.highlight).toEqual({member_of: {layer: 'l', artifact: '7'}});
  });

  it('sends a member_of clause in whichever position it carries, the artifact as a decimal string', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {store, viewport} = await warm(() => response('ck'), {clock, scheduler});
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await clock.advance(600);
    scheduler.flush();

    // An id past 2^53, which a number would round; the leaf spells it as a string.
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
    // The fixture's `highlighted` equals `matched`, as for a request with no highlight, so
    // `highlighting` is what says there was none.
    expect(store.get('view').highlighted.value).toBe(store.get('view').matched.value);
    expect(store.get('view').highlighting).toBe(false);

    store.setFilters({filter: {}, highlight: {archive: {family: 'category', keys: ['cs']}}});
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

    // A caller passing `view: undefined` still gets a view: a masked count is per view.
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

    // `filters.expr` is one of three sources; the others are the member clauses and the region.
    store.setMembers([{layer: 'l', artifact: 7n, outside: false, verb: 'filter'}]);
    await clock.advance(600);
    scheduler.flush();
    expect(store.get('filters').expr).toBeNull();
    expect(store.requestFilters()).toEqual({member_of: {layer: 'l', artifact: '7'}});
    // The identifier is a decimal string, so the expression is JSON-safe.
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

    store.suggest('archive', '', 'filter');
    await clock.advance(200);
    expect(suggest).toHaveBeenCalledWith('tok', 'archive', '', {view: 's0', counts: true, signal: expect.any(AbortSignal)});
    expect(store.get('filters').suggestions['archive']).toEqual({q: '', verb: 'filter', values: [], more: false, total: null});

    store.clear();
    expect(store.get('filters').suggestions).toEqual({});
    expect(store.get('filters').suggestErrors).toEqual({});

    // The dedupe is cleared with the projection, so asking again for the same q reaches the client.
    store.suggest('archive', '', 'filter');
    await clock.advance(200);
    expect(suggest).toHaveBeenCalledTimes(2);
    expect(store.get('filters').suggestions['archive']).toEqual({q: '', verb: 'filter', values: [], more: false, total: null});
  });

  it('counts a filter-position ask under the filter less its own clause, a highlight-position ask under the whole filter, and asks again when the filter changes', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {client} = fakeClient(() => response('ck'));
    const suggest = vi.fn(async (_token: string, column: string, q: string, _opts: {filters?: unknown}) => ({status: 'ok' as const, column, q, values: [], more: false, total: 7}));
    (client as unknown as {suggest: typeof suggest}).suggest = suggest;
    const store = createStore({viewerUrl: 'http://viewer', token: 'tok', client, clock, scheduler, prefetch: false, replica: {revalidateAfterMs: Infinity}});
    await clock.advance(1);
    const filtersOfLastAsk = () => suggest.mock.calls.at(-1)?.[3].filters;

    store.setFilters({
      filter: {archive: {family: 'category', keys: ['cs']}, primary_category: {family: 'category', keys: ['cs.LG']}},
      highlight: {}
    });
    store.suggest('archive', 'c', 'filter');
    await clock.advance(200);
    expect(filtersOfLastAsk()).toEqual({primary_category: {in: ['cs.LG']}});
    expect(store.get('filters').suggestions['archive']).toEqual({q: 'c', verb: 'filter', values: [], more: false, total: 7});

    store.suggest('archive', 'c', 'highlight');
    await clock.advance(200);
    expect(filtersOfLastAsk()).toEqual({all_of: [{archive: {in: ['cs']}}, {primary_category: {in: ['cs.LG']}}]});

    // The filter changed, so the held ask is asked again under the new one.
    store.setFilters({filter: {archive: {family: 'category', keys: ['cs']}}, highlight: {}});
    await clock.advance(200);
    expect(suggest).toHaveBeenCalledTimes(3);
    expect(filtersOfLastAsk()).toEqual({archive: {in: ['cs']}});

    // In the filter position the column's own clause is all there is, so nothing narrows the count.
    store.suggest('archive', 'c', 'filter');
    await clock.advance(200);
    expect(suggest).toHaveBeenCalledTimes(4);
    expect(suggest.mock.calls.at(-1)?.[3]).not.toHaveProperty('filters');
  });
});

describe('suggest in the store, with requests held in flight', () => {
  type Held = {column: string; q: string; filters: unknown; signal: AbortSignal; answer: (over?: Partial<{total: number; shed: boolean}>) => void};

  /** A store over a client whose suggest requests wait until the test answers them. */
  async function holding() {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {client} = fakeClient(() => response('ck'));
    const sent: Held[] = [];
    const suggest = (_token: string, column: string, q: string, opts: {filters?: unknown; signal?: AbortSignal}) =>
      new Promise((resolve, reject) => {
        const signal = opts.signal!;
        signal.addEventListener('abort', () => reject(new DOMException('aborted', 'AbortError')));
        sent.push({
          column,
          q,
          filters: opts.filters ?? null,
          signal,
          answer: (over = {}) =>
            resolve(over.shed ? {status: 'shed', retryAfterS: 0, detail: 'one suggest in flight'} : {status: 'ok', column, q, values: [], more: false, total: over.total ?? 1})
        });
      });
    Object.assign(client, {suggest});
    const store = createStore({viewerUrl: 'http://viewer', token: 'tok', client, clock, scheduler, prefetch: false, replica: {revalidateAfterMs: Infinity}});
    await clock.advance(1);
    const inFlight = () => sent.filter((h) => !h.signal.aborted && !answered.has(h));
    const answered = new Set<Held>();
    const answer = (h: Held, over?: Partial<{total: number; shed: boolean}>) => {
      answered.add(h);
      h.answer(over);
    };
    return {clock, store, sent, inFlight, answer};
  }

  const filterOn = (keys: Record<string, string[]>) => ({
    filter: Object.fromEntries(Object.entries(keys).map(([column, k]) => [column, {family: 'category' as const, keys: k}])),
    highlight: {}
  });

  it('keeps the page while a refresh is in flight, cancels a superseded refresh, and lands the latest', async () => {
    const {clock, store, sent, inFlight, answer} = await holding();
    store.suggest('archive', 'c', 'filter');
    await clock.advance(200);
    answer(sent[0]!, {total: 10});
    await clock.advance(1);
    expect(store.get('filters').suggestions['archive']?.total).toBe(10);

    store.setFilters(filterOn({primary_category: ['cs.LG']}));
    await clock.advance(200);
    expect(sent).toHaveLength(2);
    expect(sent[1]!.filters).toEqual({primary_category: {in: ['cs.LG']}});
    expect(store.get('filters').suggestions['archive']?.total).toBe(10);

    store.setFilters(filterOn({primary_category: ['cs.CV']}));
    expect(sent[1]!.signal.aborted).toBe(true);
    await clock.advance(200);
    expect(inFlight()).toEqual([sent[2]]);
    expect(sent[2]!.filters).toEqual({primary_category: {in: ['cs.CV']}});
    answer(sent[2]!, {total: 4});
    await clock.advance(1);
    expect(store.get('filters').suggestions['archive']).toEqual({q: 'c', verb: 'filter', values: [], more: false, total: 4});
  });

  it('keeps the page when a refresh is shed until its retries run out, and reports nothing', async () => {
    const {clock, store, sent, inFlight, answer} = await holding();
    store.suggest('archive', 'c', 'filter');
    await clock.advance(200);
    answer(sent[0]!, {total: 10});
    await clock.advance(1);

    store.setFilters(filterOn({primary_category: ['cs.LG']}));
    for (let i = 0; i < 10; i++) {
      await clock.advance(1_000);
      for (const h of inFlight()) answer(h, {shed: true});
    }
    expect(sent.length).toBe(7);
    expect(store.get('filters').suggestions['archive']?.total).toBe(10);
    expect(store.get('filters').suggestErrors).toEqual({});
  });

  it('reports backpressure when an ask typed into the box is still shed after its retries', async () => {
    const {clock, store, inFlight, answer} = await holding();
    store.suggest('archive', 'c', 'filter');
    for (let i = 0; i < 10; i++) {
      await clock.advance(1_000);
      for (const h of inFlight()) answer(h, {shed: true});
    }
    expect(store.get('filters').suggestErrors['archive']).toEqual({code: 'backpressure', detail: 'one suggest in flight'});
  });

  it('does not ask again for a change the request would not carry', async () => {
    const {clock, store, sent, answer} = await holding();
    store.setFilters(filterOn({archive: ['cs'], primary_category: ['cs.LG']}));
    store.suggest('archive', 'c', 'filter');
    await clock.advance(200);
    answer(sent[0]!);
    await clock.advance(1);

    // A highlight clause, and a chip on the asking column's own filter clause.
    store.setFilters({...filterOn({archive: ['cs'], primary_category: ['cs.LG']}), highlight: {archive: {family: 'category', keys: ['cs']}}});
    store.setFilters(filterOn({archive: ['cs', 'math'], primary_category: ['cs.LG']}));
    await clock.advance(1_000);
    expect(sent).toHaveLength(1);
  });

  it('sends one request at a time, the latest ask per column', async () => {
    const {clock, store, sent, inFlight, answer} = await holding();
    store.suggest('archive', 'c', 'filter');
    store.suggest('primary_category', 'cs', 'filter');
    await clock.advance(200);
    expect(inFlight().map((h) => h.column)).toEqual(['archive']);
    // Typed on while it waits: the queued ask for the column is replaced.
    store.suggest('primary_category', 'cs.', 'filter');
    await clock.advance(200);
    answer(sent[0]!);
    await clock.advance(1);
    expect(inFlight().map((h) => [h.column, h.q])).toEqual([['primary_category', 'cs.']]);
    expect(sent).toHaveLength(2);
  });

  it('forgets a column: its page goes and a change of filter asks nothing for it', async () => {
    const {clock, store, sent, answer} = await holding();
    store.suggest('archive', 'c', 'filter');
    await clock.advance(200);
    answer(sent[0]!);
    await clock.advance(1);

    store.forgetSuggestions('archive');
    expect(store.get('filters').suggestions['archive']).toBeUndefined();
    store.setFilters(filterOn({primary_category: ['cs.LG']}));
    await clock.advance(1_000);
    expect(sent).toHaveLength(1);
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
    // Before meta has landed.
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
    // Two artifacts served on every request, as for a layer scattered through row space.
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

    // A settle naming a third artifact. A positional colour depends on one centroid, so the map is
    // extended in place: the same object, with the existing colours as the same objects.
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

    // A palette change moves every colour, so the map is rebuilt.
    store.setPalette('spread');
    expect(store.get('artifacts').colours).not.toBe(first);
    expect(store.get('artifacts').colours.size).toBe(3);
  });
});

describe('select: the selection is the region leaf on every request', () => {
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
    // Exact: the server said so, and the replica holds every tile of the box, the whole extent at
    // the depth the driver chose.
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
    store.setFilters({filter: {archive: {family: 'category', keys: ['cs']}}, highlight: {}});
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
      item: async () => ({fields: {}}),
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
    // A subscriber that throws must not stop the ones after it from being told.
    const quiet = vi.spyOn(console, 'error').mockImplementation(() => {});
    const traced: string[] = [];
    const store = createStore({viewerUrl: 'http://x', token: 't', instruments: {onTrace: (kind) => traced.push(kind)}});
    const seen: string[] = [];
    store.subscribe('filters', () => {
      throw new Error('boom');
    });
    store.subscribe('filters', () => seen.push('named-after'));
    store.subscribe(() => seen.push('all'));
    store.setFilters({filter: {}, highlight: {}});
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
  const ROWS: Record<string, Artifact[]> = {topics: [row('topics', 11n)], topic_names: [row('topic_names', 21n, 11n)], kmeans: [row('kmeans', 31n)], subtopics: [row('subtopics', 41n)]};

  /**
   * Answers as the server does: the artifacts of every layer named, and on a point request a
   * membership column per named layer with members, each point in its first artifact.
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

  async function open(traces: {kind: string; fields: Record<string, number | string>}[] = [], declared: Meta = LAYERED) {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {client, viewport} = fakeClient(answer, declared);
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

  it('colours by a layer with nothing drawn: points carry its column and not its labels’, the channel asks for both, and nothing of it is drawn', async () => {
    const {store, settle, asked} = await open();
    store.setColourBy('cluster:topics');
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await settle();

    const points = asked().filter((r) => r.k !== 0);
    expect(points.length).toBeGreaterThan(0);
    for (const r of points) {
      // The colour layer alone: a label's membership column is read by nothing.
      expect(r.layers).toEqual(['topics']);
      expect(r.levels).toEqual([0, 1, 2, 3]);
      expect(r.artifactBudget).toBeGreaterThan(0);
    }
    // The channel asks for the labels too, which name the legend's rows.
    expect(asked().some((r) => r.k === 0 && Array.isArray(r.layers) && r.layers.includes('topics') && r.layers.includes('topic_names'))).toBe(true);

    const bands = store.get('marks').bands;
    expect(bands.length).toBeGreaterThan(0);
    const a = store.get('artifacts');
    for (const band of bands) {
      expect(Object.keys(band.membership)).toEqual(['topics']);
      const m = band.membership['topics']!;
      expect(m).toBeDefined();
      for (const ordinal of m.distinct) expect(a.table.resolve(ordinal, a.colours)).not.toBe(0);
    }
    // Drawn: nothing. Coloured: the layer's rows.
    expect(a.layers).toEqual([]);
    expect(a.served).toEqual([]);
    expect(a.lineage.roots).toEqual([]);
    expect(layersOf(a.colourServed)).toEqual(['topics']);
    // The coloured cluster is named by the label attached to it.
    expect(a.colourServed.map((x) => artifactName(x, a.attached))).toEqual(['a name']);
  });

  it('keeps a drawn dependent layer that declares geometry on the point path, and drops only its labels', async () => {
    // `subtopics` depends on `topics` and draws clusters of its own, so it is not a label layer.
    const declared: Meta = {...LAYERED, layers: [...LAYERED.layers.slice(0, 2), drawn('subtopics', {depsOn: ['topics']}), ...LAYERED.layers.slice(2)]};
    const {store, settle, asked} = await open([], declared);
    store.setLayers(['topics']);
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await settle();
    for (const r of asked().filter((x) => x.k !== 0)) expect(r.layers).toEqual(['topics', 'subtopics']);
    expect(Object.keys(store.get('marks').bands[0]!.membership).sort()).toEqual(['subtopics', 'topics']);
  });

  it('draws a layer with its labels and colours by another, and each surface sees its own', async () => {
    const {store, settle, asked} = await open();
    store.setLayers(['topics']);
    store.setColourBy('cluster:kmeans');
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await settle();

    for (const r of asked().filter((x) => x.k !== 0)) expect(r.layers).toEqual(['topics', 'kmeans']);
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

  it('answers every verb called before the first token lands, from one supplier call', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {client} = fakeClient(() => response('ck'));
    const shape: [number, number][][][] = [[[[0, 0], [10, 0], [10, 10]]]];
    const detail = {fields: {archive: 'cs'}, views: [], scoped: {}, labels: []};
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
    store.suggest('archive', 'c', 'filter');
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
    store.suggest('archive', 'c', 'filter');
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
    store.suggest('archive', 'c', 'filter');
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
    store.suggest('archive', 'c', 'filter');
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

describe('a store serves one viewer', () => {
  /** Two viewers' answers, told apart by identity key, by the visible count of a tile and by artifact. */
  const VIEWERS = {
    a: {identityKey: 'ik-a', visible: 1_000n, artifact: 1n},
    b: {identityKey: 'ik-b', visible: 7n, artifact: 2n}
  } as const;
  type Who = keyof typeof VIEWERS;

  const SHAPED = meta({...META, layers: [layer('l', {computedContent: ['centroid', 'box', 'hull'], shape: 'derived'})]});

  /** One viewer's answer: three points spread over the corner of world space, and its artifact where a layer is asked for. */
  function answerOf(who: Who, req: FakeRequest): ViewportResponse {
    const v = VIEWERS[who];
    const scalars = {archive: {arrowType: 'u16' as const, values: Uint16Array.from({length: 3}, () => 5)}};
    const world = Float32Array.from([0.1, 0.1, 4.5, 4.5, 9, 1]);
    const artifacts = (req.layers ?? []).length === 0 ? [] : [artifact(v.artifact)];
    return responseOf(servedResult(3, [tile(0n, v.visible)], {scalars, world, artifacts}), {contentKey: `ck-${who}`, identityKey: v.identityKey});
  }

  /** Whose answers a store is showing: the identity key of every band drawn, every tile counted and every artifact served. */
  function showing(store: Store): Set<string> {
    const who = new Set<string>();
    const marks = store.get('marks');
    for (const band of [...marks.bands, ...marks.standIn.map((piece) => piece.band)]) who.add(band.identityKey);
    for (const t of store.get('tiles').tiles) {
      if (t.counts) who.add(t.counts.visible === VIEWERS.a.visible ? 'ik-a' : 'ik-b');
    }
    for (const a of store.get('artifacts').served) who.add(a.tesseraId === VIEWERS.a.artifact ? 'ik-a' : 'ik-b');
    return who;
  }

  /**
   * A store behind a supplier that hands out `t1` and then `t2`, each for a minute, so the renewal
   * falls 30 s in. `t1` is viewer A's; `t2` is viewer B's unless `renewal` says it is A's again.
   * `gate` holds a `t2` request that fetches points open after its first part has been handed over.
   * `metaOf` answers `/v1/meta` per token.
   */
  function twoTokens(opts: {renewal: Who; gate?: Promise<void>; metaOf?: (token: string) => Meta}) {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const whose = (token: string): Who => (token === 't1' ? 'a' : opts.renewal);
    const viewport = vi.fn(
      async (token: string, req: FakeRequest, {onPart}: {onPart?: (part: ViewportPart) => void | Promise<void>} = {}) => {
        const answer = answerOf(whose(token), req);
        if (opts.gate && token === 't2' && (req.k ?? 1) > 0 && onPart) {
          await onPart({result: answer.result, identityKey: answer.identityKey, contentKey: answer.contentKey});
          await opts.gate;
          return {...answer, result: {...answer.result, ids: new BigUint64Array(0), codes: new BigUint64Array(0), positions: new Float64Array(0), world: new Float32Array(0), tiles: []}};
        }
        return answer;
      }
    );
    const parts: [number, number][][][] = [[[[0, 0], [10, 0], [10, 10]]]];
    const metaRead = vi.fn(async (token: string) => opts.metaOf?.(token) ?? SHAPED);
    const client = {
      meta: metaRead,
      viewport,
      item: async (token: string) => ({fields: {asked: token}, views: [], scoped: {}, labels: []}),
      artifact: async () => ({layer: 'l', key: null, maskedCount: 1n, centroid: null, box: null, shape: parts}),
      categories: async () => [],
      suggest: async () => ({status: 'ok' as const, column: 'archive', q: '', values: [], more: false}),
      close: () => {}
    } as unknown as TesseraClient;
    let issued = 0;
    const authorise = vi.fn(async () => ({token: `t${++issued}`, expiresAt: (Date.now() + 60_000) / 1000}));
    const store = createStore({viewerUrl: 'http://viewer', authorise, client, clock, scheduler, prefetch: false, replica: {revalidateAfterMs: Infinity}});
    return {store, viewport, authorise, metaRead, clock, scheduler};
  }

  async function settled(clock: ReturnType<typeof fakeClock>, scheduler: ReturnType<typeof fakeScheduler>): Promise<void> {
    for (let i = 0; i < 4; i++) {
      await clock.advance(600);
      scheduler.flush();
    }
  }

  it('keeps what is drawn through a renewal for the same viewer, and asks with the new token', async () => {
    const {store, viewport, authorise, clock, scheduler} = twoTokens({renewal: 'a'});
    store.setLayers(['l']);
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await settled(clock, scheduler);
    store.needShape(VIEWERS.a.artifact);
    await clock.advance(1);
    expect(showing(store)).toEqual(new Set(['ik-a']));
    expect(store.get('artifacts').shapes.has(VIEWERS.a.artifact)).toBe(true);
    const drawn = store.get('marks').bands.length;
    expect(drawn).toBeGreaterThan(0);

    const blanks: string[] = [];
    store.subscribe(() => {
      if (store.get('marks').bands.length === 0) blanks.push('marks');
      if (store.get('artifacts').served.length === 0) blanks.push('artifacts');
      if (store.get('view').composition === null) blanks.push('counts');
    });
    await clock.advance(30_000);
    await settled(clock, scheduler);

    expect(authorise).toHaveBeenCalledTimes(2);
    expect(viewport.mock.calls.at(-1)![0]).toBe('t2');
    expect(blanks).toEqual([]);
    expect(showing(store)).toEqual(new Set(['ik-a']));
    expect(store.get('marks').bands.length).toBe(drawn);
    expect(store.get('artifacts').shapes.has(VIEWERS.a.artifact)).toBe(true);
  });

  it('drops everything held for viewer A before viewer B’s first answer is drawn', async () => {
    const {store, authorise, clock, scheduler} = twoTokens({renewal: 'b'});
    store.setLayers(['l']);
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await settled(clock, scheduler);
    store.needShape(VIEWERS.a.artifact);
    await clock.advance(1);
    expect(showing(store)).toEqual(new Set(['ik-a']));

    const mixed: Set<string>[] = [];
    const late: Set<string>[] = [];
    let sawB = false;
    store.subscribe(() => {
      const who = showing(store);
      if (who.size > 1) mixed.push(who);
      if (who.has('ik-b')) sawB = true;
      if (sawB && who.has('ik-a')) late.push(who);
    });
    // The renewal hands over viewer B's token; nothing moves the camera.
    await clock.advance(30_000);
    await settled(clock, scheduler);

    expect(authorise).toHaveBeenCalledTimes(2);
    expect(mixed).toEqual([]);
    expect(late).toEqual([]);
    expect(store.get('marks').bands.length).toBeGreaterThan(0);
    expect(showing(store)).toEqual(new Set(['ik-b']));
    expect(store.get('artifacts').served.map((a) => a.tesseraId)).toEqual([VIEWERS.b.artifact]);
    expect(store.get('artifacts').shapes.has(VIEWERS.a.artifact)).toBe(false);
    expect(await store.describe(7n)).toEqual({asked: 't2'});
  });

  it('publishes none of viewer A’s meta once viewer B’s first answer arrives, and reads B’s', async () => {
    // Viewer A is told of a layer and a view viewer B may not reach.
    const metaA = meta({
      ...SHAPED,
      views: [...SHAPED.views, view('hidden', {quantisation: SHAPED.views[0]!.quantisation})],
      layers: [...SHAPED.layers, layer('secret', {computedContent: ['centroid', 'box']})]
    });
    const metaB = SHAPED;
    const {store, metaRead, clock, scheduler} = twoTokens({renewal: 'b', metaOf: (token) => (token === 't1' ? metaA : metaB)});
    store.setLayers(['l']);
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await settled(clock, scheduler);
    expect(store.get('meta')).toBe(metaA);

    const late: (Meta | null)[] = [];
    let sawB = false;
    store.subscribe(() => {
      if (showing(store).has('ik-b')) sawB = true;
      if (sawB && store.get('meta') === metaA) late.push(store.get('meta'));
    });
    const published: (Meta | null)[] = [];
    store.subscribe('meta', (m) => published.push(m));
    await clock.advance(30_000);
    await settled(clock, scheduler);

    expect(sawB).toBe(true);
    expect(late).toEqual([]);
    // Dropped when B's first answer came, then read again under B's token.
    expect(published).toEqual([null, metaB]);
    expect(metaRead.mock.calls.at(-1)![0]).toBe('t2');
    expect(store.get('meta')?.layers.map((l) => l.name)).toEqual(['l']);
    expect(store.get('meta')?.views.map((v) => v.id)).toEqual(['s0']);
    expect(showing(store)).toEqual(new Set(['ik-b']));
  });

  it('draws no stand-in from viewer A beside the first points streamed to viewer B', async () => {
    let now = Date.now();
    const dateNow = vi.spyOn(Date, 'now').mockImplementation(() => now);
    try {
      let open: () => void = () => {};
      const gate = new Promise<void>((resolve) => (open = resolve));
      const {store, authorise, clock, scheduler} = twoTokens({renewal: 'b', gate});
      store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
      await settled(clock, scheduler);
      expect(showing(store)).toEqual(new Set(['ik-a']));

      const mixed: Set<string>[] = [];
      store.subscribe(() => {
        const who = showing(store);
        if (who.size > 1) mixed.push(who);
      });
      // Viewer A's token has four seconds left, so the next request renews it first, and the
      // points for the zoomed view are asked for under viewer B's token. Until they come, viewer
      // A's coarser points stand in.
      now += 56_000;
      store.setView({bbox: [0, 0, 1, 2], width: 400, height: 400});
      for (let i = 0; i < 4; i++) {
        await clock.advance(1);
        scheduler.flush();
      }
      expect(authorise).toHaveBeenCalledTimes(2);
      expect(mixed).toEqual([]);
      expect(showing(store).has('ik-a')).toBe(false);

      open();
      await settled(clock, scheduler);
      expect(mixed).toEqual([]);
      expect(store.get('marks').bands.length).toBeGreaterThan(0);
      expect(showing(store)).toEqual(new Set(['ik-b']));
    } finally {
      dateNow.mockRestore();
    }
  });

  it('keeps the region and the filters through an identity change under the same meta, and counts them again', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    // A compaction: the same token and meta, and answers under a new identity key from a point on.
    let compacted = false;
    const viewport = vi.fn(async (_token: string, req: FakeRequest) => answerOf(compacted ? 'b' : 'a', req));
    const metaRead = vi.fn(async () => SHAPED);
    const {client} = fakeClient(() => response('ck'), SHAPED);
    Object.assign(client, {viewport, meta: metaRead});
    // Held tiles are revalidated after a second, so the same camera asks again.
    const store = createStore({viewerUrl: 'http://viewer', token: 'tok', client, clock, scheduler, prefetch: false, replica: {revalidateAfterMs: 1_000}});
    await clock.advance(1);
    store.setColourBy('archive');
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    const filters: FilterDraft = {filter: {archive: {family: 'category', keys: ['cs']}}, highlight: {}};
    store.setFilters(filters);
    const shape = {kind: 'box' as const, bbox: [0, 0, 50, 50] as [number, number, number, number]};
    store.select(shape);
    await settled(clock, scheduler);
    expect(store.get('region')).toMatchObject({status: 'shown', matched: {value: Number(VIEWERS.a.visible)}});

    compacted = true;
    let changed = false;
    const stale: string[] = [];
    store.subscribe(() => {
      if (store.get('meta') === null || showing(store).has('ik-b')) changed = true;
      if (!changed) return;
      if (showing(store).has('ik-a')) stale.push('mark');
      const r = store.get('region');
      if (r?.status === 'shown' && r.matched.value === Number(VIEWERS.a.visible)) stale.push('region count');
      if (store.get('view').matched.value === Number(VIEWERS.a.visible)) stale.push('view count');
    });
    await clock.advance(2_000);
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await settled(clock, scheduler);

    expect(changed).toBe(true);
    expect(stale).toEqual([]);
    expect(metaRead).toHaveBeenCalledTimes(2);
    expect(store.get('region')?.shape).toBe(shape);
    expect(store.get('region')).toMatchObject({status: 'shown', matched: {value: Number(VIEWERS.b.visible)}});
    expect(store.get('filters').draft.filter.archive).toEqual(filters.filter.archive);
    expect(store.get('legend').colourBy).toBe('archive');
    const asked = viewport.mock.calls.at(-1)![1];
    expect(JSON.stringify(asked.filters)).toContain('region');
    expect(JSON.stringify(asked.filters)).toContain('archive');
    expect(showing(store)).toEqual(new Set(['ik-b']));
  });

  /**
   * Viewer A behind `t1` and viewer B behind `t2`, each with its own meta, whose identity keys name
   * the viewer and the view. `hold` leaves a request unanswered; `refuse` refuses it as the server
   * refuses a filter on a column the viewer cannot reach.
   */
  function perViewer(opts: {
    metaOf: (token: string) => Meta;
    hold?: (token: string, req: FakeRequest) => boolean;
    refuse?: (token: string, req: FakeRequest) => boolean;
    authorise?: () => Promise<{token: string; expiresAt: number}>;
  }) {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const whose = (token: string): Who => (token === 't1' || token === 't-a' ? 'a' : 'b');
    const viewport = vi.fn(async (token: string, req: FakeRequest) => {
      if (opts.refuse?.(token, req)) throw new TesseraError(422, 'bad-filter', 'this principal cannot filter on that column');
      if (opts.hold?.(token, req)) return new Promise<never>(() => {});
      const who = whose(token);
      return {...answerOf(who, req), identityKey: `${who}:${req.view}`};
    });
    const {client} = fakeClient(() => response('ck'));
    Object.assign(client, {viewport, meta: async (token: string) => opts.metaOf(token)});
    let issued = 0;
    const authorise = vi.fn(opts.authorise ?? (async () => ({token: `t${++issued}`, expiresAt: (Date.now() + 60_000) / 1000})));
    const store = createStore({viewerUrl: 'http://viewer', authorise, client, clock, scheduler, prefetch: false, replica: {revalidateAfterMs: Infinity}});
    return {store, client, viewport, authorise, clock, scheduler};
  }

  /** Whose answers a store shows, by the viewer each band, tile count, artifact and meta came from. */
  function whoShows(store: Store, metas: {a: Meta; b: Meta}): Set<Who> {
    const who = new Set<Who>();
    const marks = store.get('marks');
    for (const band of [...marks.bands, ...marks.standIn.map((piece) => piece.band)]) who.add(band.identityKey.split(':')[0] as Who);
    for (const t of store.get('tiles').tiles) if (t.counts) who.add(t.counts.visible === VIEWERS.a.visible ? 'a' : 'b');
    for (const a of store.get('artifacts').served) who.add(a.tesseraId === VIEWERS.a.artifact ? 'a' : 'b');
    if (store.get('meta') === metas.a) who.add('a');
    if (store.get('meta') === metas.b) who.add('b');
    return who;
  }

  const TWO_VIEWS = meta({...SHAPED, views: [...SHAPED.views, view('s1', {quantisation: SHAPED.views[0]!.quantisation})]});

  it('forgets when the renewed token is refused the view the previous token reached', async () => {
    // Viewer B is not served s0, which viewer A was drawing.
    const metas = {a: SHAPED, b: meta({...SHAPED, views: [view('s1', {quantisation: SHAPED.views[0]!.quantisation})]})};
    const refused: string[] = [];
    const {store, clock, scheduler} = perViewer({
      metaOf: (token) => (token === 't1' ? metas.a : metas.b),
      refuse: (token, req) => {
        const no = token === 't2' && req.view === 's0';
        if (no) refused.push(req.view!);
        return no;
      }
    });
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await settled(clock, scheduler);
    expect(whoShows(store, metas)).toEqual(new Set(['a']));

    await clock.advance(30_000);
    await settled(clock, scheduler);

    expect(refused.length).toBeGreaterThan(0);
    expect(store.get('meta')).toBe(metas.b);
    expect(store.get('view').id).toBe('s1');
    expect(whoShows(store, metas)).toEqual(new Set(['b']));
  });

  it('forgets on a streamed part under a new key for a view it has left, and holds none of it for the return', async () => {
    const metas = {a: meta({...TWO_VIEWS}), b: meta({...TWO_VIEWS})};
    // Whose answers the server gives, and whose meta, from here on.
    let who: Who = 'a';
    let gate: Promise<void> | null = null;
    const {store, viewport, clock, scheduler} = perViewer({metaOf: () => (who === 'a' ? metas.a : metas.b)});
    const answer = (req: FakeRequest) => ({...answerOf(who, req), identityKey: `${who}:${req.view}`});
    type Fetch = (token: string, req: FakeRequest, opts?: {onPart?: (part: ViewportPart) => void | Promise<void>}) => Promise<ViewportResponse>;
    (viewport as unknown as {mockImplementation(f: Fetch): void}).mockImplementation(async (_token, req, {onPart} = {}) => {
      // One request for s0 hands over a part only once the gate opens, and never answers.
      if (gate && req.view === 's0' && (req.k ?? 1) > 0 && onPart) {
        const held = gate;
        gate = null;
        await held;
        const a = answer(req);
        await onPart({result: a.result, identityKey: a.identityKey, contentKey: a.contentKey});
        return new Promise<never>(() => {});
      }
      return answer(req);
    });
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await settled(clock, scheduler);
    expect(whoShows(store, metas)).toEqual(new Set(['a']));

    // A zoom on s0 whose points are still on the way when the host moves to s1.
    let open: () => void = () => {};
    gate = new Promise<void>((resolve) => (open = resolve));
    store.setView({bbox: [0, 0, 1, 2], width: 400, height: 400});
    await clock.advance(1);
    scheduler.flush();
    store.setCurrentView('s1');
    await settled(clock, scheduler);
    expect(whoShows(store, metas)).toEqual(new Set(['a']));

    const mixed: Set<Who>[] = [];
    store.subscribe(() => {
      const shown = whoShows(store, metas);
      if (shown.size > 1) mixed.push(shown);
    });
    // The part for s0 arrives under viewer B's key after s0 was left.
    who = 'b';
    open();
    await settled(clock, scheduler);
    // The key changed, so the store forgot at once and read meta again, with nothing asked for s1.
    expect(store.get('meta')).toBe(metas.b);

    // Back on s0, nothing from the part is held to draw from before s0 asks.
    store.setCurrentView('s0');
    scheduler.flush();
    expect(store.get('replica').bands).toBe(0);
    expect(store.get('marks').bands).toEqual([]);
    await settled(clock, scheduler);

    expect(mixed).toEqual([]);
    expect(store.get('meta')).toBe(metas.b);
    expect(whoShows(store, metas)).toEqual(new Set(['b']));
  });

  it('reads meta again at a renewal when it holds meta and no answer yet', async () => {
    const metas = {a: SHAPED, b: meta({...SHAPED})};
    const {store, authorise, clock} = perViewer({metaOf: (token) => (token === 't1' ? metas.a : metas.b)});
    // A store with no camera, as behind a panel or a map not yet sized.
    await clock.advance(1);
    expect(store.get('meta')).toBe(metas.a);

    await clock.advance(30_000);
    await clock.advance(1);
    expect(authorise).toHaveBeenCalledTimes(2);
    expect(store.get('meta')).toBe(metas.b);
  });

  it('empties the colours of every held view’s artifacts when it forgets', async () => {
    const both = {...TWO_VIEWS, layers: [layer('l', {views: ['s0', 's1'], computedContent: ['centroid', 'box', 'hull'], shape: 'derived'})]};
    const metas = {a: meta(both), b: meta(both)};
    const {store, viewport, clock, scheduler} = perViewer({metaOf: (token) => (token === 't1' ? metas.a : metas.b)});
    // Each view serves its own artifact, and its points are members of it, so each view's rows
    // hold an ordinal in the table.
    const answered = viewport.getMockImplementation()!;
    viewport.mockImplementation(async (token: string, req: FakeRequest) => {
      const r = await answered(token, req);
      const id = VIEWERS.a.artifact + (req.view === 's1' ? 100n : 0n);
      const membership = {l: {index: Uint16Array.from({length: r.result.ids.length}, () => 1), ids: BigUint64Array.of(id)}};
      return {...r, result: {...r.result, membership, artifacts: r.result.artifacts.map((a) => ({...a, tesseraId: id}))}};
    });
    store.setLayers(['l']);
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await settled(clock, scheduler);
    store.setCurrentView('s1');
    await settled(clock, scheduler);
    const table = store.get('artifacts').table;
    expect(table.ordinalOf('l', VIEWERS.a.artifact)).not.toBe(0);
    expect(table.ordinalOf('l', VIEWERS.a.artifact + 100n)).not.toBe(0);
    expect(store.get('artifacts').colours.size).toBeGreaterThanOrEqual(2);

    const atForget: number[] = [];
    store.subscribe('meta', (m) => {
      if (m === null) atForget.push(store.get('artifacts').colours.size);
    });
    store.clear();
    expect(atForget).toEqual([0]);
  });

  it('names nothing in the artifact table from an absorb a clear interrupts between slices', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const N = 200;
    /** `N` one-point tiles, each point a member of its own artifact: ids from `base`. */
    const many = (req: FakeRequest, base: bigint, identityKey: string): ViewportResponse => {
      const scalars = {archive: {arrowType: 'u16' as const, values: Uint16Array.from({length: N}, () => 5)}};
      return responseOf(
        servedResult(N, Array.from({length: N}, (_, i) => tile(BigInt(i), 1n, {served: 1n})), {
          scalars,
          membership: {l: {index: Uint16Array.from({length: N}, (_, i) => i + 1), ids: BigUint64Array.from({length: N}, (_, i) => base + BigInt(i))}}
        }),
        {identityKey, contentKey: `ck-${identityKey}`}
      );
    };
    // Each servedResult puts all served points on its first tile; spread them one per tile.
    const spread = (r: ViewportResponse): ViewportResponse => ({...r, result: {...r.result, tiles: r.result.tiles.map((t) => ({...t, served: 1n}))}});
    let signedIn: Who = 'a';
    const viewport = vi.fn(async (token: string, req: FakeRequest) => spread(many(req, token === 't-a' ? 1_000n : 5_000n, `ik-${token}`)));
    const {client} = fakeClient(() => response('ck'), SHAPED);
    Object.assign(client, {viewport});
    const authorise = vi.fn(async () => ({token: `t-${signedIn}`, expiresAt: (Date.now() + 3_600_000) / 1000}));
    // Every read of the clock is far past the last, so each slice ends after its first 64 tiles.
    let now = 0;
    const clockRead = vi.spyOn(performance, 'now').mockImplementation(() => (now += 1_000));
    try {
      let interrupted = false;
      const store: Store = createStore({
        viewerUrl: 'http://viewer',
        authorise,
        client,
        clock,
        scheduler,
        prefetch: false,
        replica: {
          revalidateAfterMs: Infinity,
          // The first slice of viewer A's answer is stored; the next waits for a frame, and the
          // store is cleared for viewer B in between.
          onPhase: (kind) => {
            if (kind !== 'piece' || interrupted) return;
            interrupted = true;
            signedIn = 'b';
            store.clear();
          }
        }
      });
      store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
      for (let i = 0; i < 20; i++) {
        await new Promise((resolve) => setTimeout(resolve, 0));
        await clock.advance(50);
        scheduler.flush();
      }
      expect(interrupted).toBe(true);
      const table = store.get('artifacts').table;
      const ids = table.liveEntries().map(({entry}) => entry.tesseraId);
      expect(ids.length).toBe(N);
      expect(ids.every((id) => id >= 5_000n)).toBe(true);
    } finally {
      clockRead.mockRestore();
    }
  });

  it('resolves a hover’s record asked for before a forget to nothing', async () => {
    const metas = {a: SHAPED, b: meta({...SHAPED})};
    const {store, client, clock} = perViewer({metaOf: (token) => (token === 't1' ? metas.a : metas.b)});
    let answer: () => void = () => {};
    Object.assign(client, {
      item: async () => {
        await new Promise<void>((resolve) => (answer = resolve));
        return {fields: {title: 'viewer A’s record'}, views: [], scoped: {}, labels: []};
      }
    });
    await clock.advance(1);
    const described = store.describe(7n);
    await clock.advance(1);
    store.clear();
    answer();
    expect(await described).toBeNull();
  });

  it('does not trust the first answer on an unvisited view after a renewal, before a held key is matched', async () => {
    const metas = {a: meta({...TWO_VIEWS}), b: meta({...TWO_VIEWS})};
    // The ask the renewal sends at once goes unanswered, so the first answer under B's token is on s1.
    const {store, authorise, clock, scheduler} = perViewer({
      metaOf: (token) => (token === 't1' ? metas.a : metas.b),
      hold: (token, req) => token === 't2' && req.view === 's0'
    });
    store.setLayers(['l']);
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await settled(clock, scheduler);
    expect(whoShows(store, metas)).toEqual(new Set(['a']));

    const mixed: Set<Who>[] = [];
    store.subscribe(() => {
      const who = whoShows(store, metas);
      if (who.size > 1) mixed.push(who);
    });
    await clock.advance(30_000);
    expect(authorise).toHaveBeenCalledTimes(2);
    store.setCurrentView('s1');
    await settled(clock, scheduler);

    expect(mixed).toEqual([]);
    expect(whoShows(store, metas)).toEqual(new Set(['b']));
    expect(store.get('view').id).toBe('s1');
    expect(store.get('marks').bands.length).toBeGreaterThan(0);
  });

  it('sees a new key after a renewal although the previous viewer’s filter is refused under the new token', async () => {
    const metaA = meta({...SHAPED, filterOperands: [...SHAPED.filterOperands, {column: 'secret', family: 'category', operands: ['in']}]});
    const metas = {a: metaA, b: SHAPED};
    const {store, viewport, clock, scheduler} = perViewer({
      metaOf: (token) => (token === 't1' ? metas.a : metas.b),
      refuse: (token, req) => token === 't2' && JSON.stringify(req.filters ?? null).includes('secret')
    });
    await clock.advance(1);
    store.setFilters({filter: {secret: {family: 'category', keys: ['x']}, archive: {family: 'category', keys: ['cs']}}, highlight: {secret: {family: 'category', keys: ['y']}}});
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await settled(clock, scheduler);
    expect(whoShows(store, metas)).toEqual(new Set(['a']));

    await clock.advance(30_000);
    await settled(clock, scheduler);

    expect(whoShows(store, metas)).toEqual(new Set(['b']));
    expect(Object.keys(store.get('filters').draft.filter)).toEqual(['archive']);
    expect(Object.keys(store.get('filters').draft.highlight)).toEqual([]);
    expect(store.get('marks').bands.length).toBeGreaterThan(0);
    expect(JSON.stringify(viewport.mock.calls.at(-1)![1].filters)).not.toContain('secret');
  });

  it('drops the member_of clauses, a region selected from an artifact and the old colours when the key changes', async () => {
    const metas = {a: SHAPED, b: meta({...SHAPED})};
    const {store, clock, scheduler} = perViewer({metaOf: (token) => (token === 't1' ? metas.a : metas.b)});
    store.setLayers(['l']);
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await settled(clock, scheduler);
    store.setMembers([{layer: 'l', artifact: VIEWERS.a.artifact, outside: false, verb: 'filter', label: 'a1'}]);
    store.select({kind: 'artifact', id: VIEWERS.a.artifact});
    await settled(clock, scheduler);
    expect(store.get('artifacts').colours.size).toBeGreaterThan(0);

    const atForget: {colours: number; members: number; region: unknown}[] = [];
    store.subscribe('meta', (m) => {
      if (m === null) atForget.push({colours: store.get('artifacts').colours.size, members: store.get('filters').members.length, region: store.get('region')});
    });
    await clock.advance(30_000);
    await settled(clock, scheduler);

    expect(atForget).toEqual([{colours: 0, members: 0, region: null}]);
    expect(store.get('filters').members).toEqual([]);
    expect(store.get('region')).toBeNull();
    expect(whoShows(store, metas)).toEqual(new Set(['b']));
  });

  it('drops a filter, a layer and a colouring the next viewer’s meta does not list, and a view it does not list', async () => {
    const hidden = view('hidden', {quantisation: SHAPED.views[0]!.quantisation});
    const metaA = meta({
      ...SHAPED,
      views: [...SHAPED.views, hidden],
      layers: [...SHAPED.layers, layer('secret-layer', {views: ['s0', 'hidden'], computedContent: ['centroid', 'box']})],
      declaredScalars: [...SHAPED.declaredScalars, scalar('secretcol', 'u16', {category: {vocabulary: 'b', kind: 'declared', visibility: 'public'}, render: true, homes: ['rendered']})],
      filterOperands: [...SHAPED.filterOperands, {column: 'secret', family: 'category', operands: ['in']}]
    });
    const metas = {a: metaA, b: SHAPED};
    let signedIn: Who = 'a';
    const {store, viewport, clock, scheduler} = perViewer({
      metaOf: (token) => (token === 't-a' ? metas.a : metas.b),
      authorise: async () => ({token: `t-${signedIn}`, expiresAt: (Date.now() + 3_600_000) / 1000})
    });
    await clock.advance(1);
    store.setCurrentView('hidden');
    store.setLayers(['l', 'secret-layer']);
    store.setColourBy('secretcol');
    store.setFilters({filter: {secret: {family: 'category', keys: ['x']}, archive: {family: 'category', keys: ['cs']}}, highlight: {secret: {family: 'category', keys: ['y']}}});
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    store.select({kind: 'box', bbox: [0, 0, 50, 50]});
    await settled(clock, scheduler);
    expect(store.get('view').id).toBe('hidden');
    expect(store.get('marks').bands.length).toBeGreaterThan(0);

    signedIn = 'b';
    store.clear();
    const asked = viewport.mock.calls.length;
    await settled(clock, scheduler);

    expect(store.get('meta')).toBe(metas.b);
    expect(Object.keys(store.get('filters').draft.filter)).toEqual(['archive']);
    expect(Object.keys(store.get('filters').draft.highlight)).toEqual([]);
    expect(store.get('artifacts').layers).toEqual(['l']);
    expect(store.get('legend').colourBy).toBeNull();
    // The view is not offered, so the store opens on the first and drops the camera with it.
    expect(store.get('view').id).toBe('s0');
    expect(store.get('region')).toBeNull();
    expect(viewport.mock.calls.slice(asked).filter((c) => (c[1].k ?? 1) > 0)).toEqual([]);
  });

  it('clear() forgets viewer A and serves viewer B from B’s own token and meta', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    // Viewer A may filter on a column viewer B is not told of.
    const metaOf = (token: string) =>
      token === 't-a' ? meta({...SHAPED, filterOperands: [...SHAPED.filterOperands, {column: 'secret', family: 'category', operands: ['in']}]}) : SHAPED;
    const whose = (token: string): Who => (token === 't-a' ? 'a' : 'b');
    const viewport = vi.fn(async (token: string, req: FakeRequest) => answerOf(whose(token), req));
    const parts: [number, number][][][] = [[[[0, 0], [10, 0], [10, 10]]]];
    const client = {
      meta: async (token: string) => metaOf(token),
      viewport,
      item: async (token: string) => ({fields: {asked: token}, views: [], scoped: {}, labels: []}),
      artifact: async () => ({layer: 'l', key: null, maskedCount: 1n, centroid: null, box: null, shape: parts}),
      categories: async () => [{code: 5, key: 'cs', title: 'CS'}],
      suggest: async () => ({status: 'ok' as const, column: 'archive', q: '', values: [], more: false}),
      close: () => {}
    } as unknown as TesseraClient;
    // The host's supplier answers for whoever is signed in.
    let signedIn: Who = 'a';
    const authorise = vi.fn(async () => ({token: `t-${signedIn}`, expiresAt: (Date.now() + 3_600_000) / 1000}));
    const store = createStore({viewerUrl: 'http://viewer', authorise, client, clock, scheduler, prefetch: false, replica: {revalidateAfterMs: Infinity}});
    store.setLayers(['l']);
    store.setColourBy('archive');
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await settled(clock, scheduler);
    store.needShape(VIEWERS.a.artifact);
    await store.pick(7n);
    store.select({kind: 'box', bbox: [0, 0, 50, 50]});
    await settled(clock, scheduler);
    expect(showing(store)).toEqual(new Set(['ik-a']));
    expect(Object.keys(store.get('filters').draft.filter)).toContain('secret');

    signedIn = 'b';
    store.clear();
    const late: Set<string>[] = [];
    store.subscribe(() => {
      if (showing(store).has('ik-a')) late.push(showing(store));
    });

    // Nothing of viewer A's is published or drawn once the call returns.
    expect(store.get('meta')).toBeNull();
    expect(showing(store)).toEqual(new Set());
    expect(store.get('marks')).toMatchObject({bands: [], standIn: []});
    expect(store.get('view').composition).toBeNull();
    expect(store.get('artifacts').shapes.size).toBe(0);
    expect(store.get('selection').item).toBeNull();
    expect(store.get('region')).toBeNull();
    expect(store.get('legend').categories).toEqual({});

    await settled(clock, scheduler);
    expect(late).toEqual([]);
    expect(authorise).toHaveBeenCalledTimes(2);
    expect(store.get('meta')?.filterOperands.map((f) => f.column)).toEqual(['archive']);
    expect(Object.keys(store.get('filters').draft.filter)).not.toContain('secret');
    expect(viewport.mock.calls.at(-1)![0]).toBe('t-b');
    expect(store.get('marks').bands.length).toBeGreaterThan(0);
    expect(showing(store)).toEqual(new Set(['ik-b']));
  });
});

describe('the item a click opens and the record a hover names', () => {
  it('puts a picked item in the selection, and a refusal in its place', async () => {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {client} = fakeClient(() => response('ck'));
    const detail = {fields: {archive: 'cs'}, views: [], scoped: {}, labels: []};
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
      return {fields: {title: `paper ${id}`}, views: [], scoped: {}, labels: []};
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
    const item = vi.fn(async (_token: string, id: bigint) => ({fields: {title: `paper ${id}`}, views: [], scoped: {}, labels: []}));
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

describe('counts before points', () => {
  /**
   * A fake whose viewport hands over its counts at once and its points only when `land` is called,
   * as a streamed response does.
   */
  async function held(identityKey = 'ik') {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {store, viewport} = await warm(() => response('ck'), {clock, scheduler});
    let land: () => void = () => {};
    viewport.mockImplementation((async (_t: string, _req: FakeRequest, {onCounts}: {onCounts?: (c: unknown) => void} = {}) => {
      const answer = response('ck', identityKey);
      // A count-only request takes no sink and answers at once.
      if (!onCounts) return answer;
      onCounts({tiles: answer.result.tiles, subCells: null, identityKey, contentKey: 'ck'});
      await new Promise<void>((resolve) => (land = resolve));
      return answer;
    }) as never);
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await clock.advance(600);
    scheduler.flush();
    return {store, clock, scheduler, land: async () => {
      land();
      await clock.advance(600);
      scheduler.flush();
    }};
  }

  it('shows a region’s counts before its points, then the same counts with the points', async () => {
    const {store, land} = await held();
    // Counted and not drawn: the figures are the server's, and no mark is on screen.
    expect(store.get('view').visible.value).toBe(10_000_000);
    expect(store.get('view').served.shown).toBe(0);
    expect(store.get('marks').count.shown).toBe(0);
    expect(store.get('tiles').tiles.map((t) => ({exact: t.exact, drawn: t.drawn, visible: t.counts?.visible}))).toEqual([{exact: false, drawn: 0, visible: 10_000_000n}]);

    await land();
    // The points carry the same counts, which replace the early ones rather than add to them.
    expect(store.get('view').visible.value).toBe(10_000_000);
    expect(store.get('view').served.shown).toBe(3);
    expect(store.get('tiles').tiles.map((t) => ({exact: t.exact, drawn: t.drawn, visible: t.counts?.visible}))).toEqual([{exact: true, drawn: 3, visible: 10_000_000n}]);
  });

  it('drops counts whose points have not landed when the store forgets its answers', async () => {
    const {store} = await held();
    expect(store.get('tiles').tiles.length).toBe(1);
    store.clear();
    expect(store.get('tiles').tiles).toEqual([]);
    expect(store.get('view').visible.value).toBe(0);
  });
});

describe('a point request names the render columns the store reads', () => {
  const COLUMNS: Meta = {
    ...META,
    declaredScalars: [
      ...META.declaredScalars,
      scalar('score', 'f32', {render: true, homes: ['rendered']}),
      scalar('title', 'utf8', {render: false, index: false})
    ],
    scopedScalars: [
      {name: 'heat', arrowType: 'f32', scope: {group: 'g'}, category: null, analyser: null, render: true, index: false, views: ['s0']},
      {name: 'chill', arrowType: 'f32', scope: {group: 'h'}, category: null, analyser: null, render: true, index: false, views: ['s1']}
    ]
  };

  /** Answers as the server does: each point carries the render columns the request names, and only those. */
  function answer(req: FakeRequest): ViewportResponse {
    const r = response('ck');
    const n = r.result.ids.length;
    const all = {
      archive: {arrowType: 'u16' as const, values: Uint16Array.from({length: n}, () => 5)},
      score: {arrowType: 'f32' as const, values: Float32Array.from({length: n}, (_, i) => i + 1)},
      heat: {arrowType: 'f32' as const, values: Float32Array.from({length: n}, () => 2)}
    };
    const named = Array.isArray(req.pointRows) ? req.pointRows : Object.keys(all);
    const scalars = Object.fromEntries(Object.entries(all).filter(([name]) => named.includes(name)));
    return {...r, result: {...r.result, scalars}, columnsAsked: Array.isArray(req.pointRows) ? [...req.pointRows] : null};
  }

  async function open() {
    const clock = fakeClock();
    const scheduler = fakeScheduler();
    const {client, viewport} = fakeClient(answer, COLUMNS);
    /** Called once, the next time a whole answer has been stored and before its ground is marked held. */
    let onStored: (() => void) | null = null;
    const store = createStore({viewerUrl: 'http://viewer', token: 'tok', client, clock, scheduler, prefetch: false,
      replica: {
        revalidateAfterMs: Infinity,
        onPhase: (kind) => {
          if (kind !== 'store' || !onStored) return;
          const run = onStored;
          onStored = null;
          run();
        }
      }
    });
    await clock.advance(1);
    const settle = async () => {
      await clock.advance(600);
      scheduler.flush();
      await clock.advance(600);
      scheduler.flush();
    };
    /** The point requests sent, oldest first. */
    const asked = () => viewport.mock.calls.map((c) => c[1] as FakeRequest).filter((r) => r.k !== 0);
    const whenStored = (run: () => void) => (onStored = run);
    return {store, settle, asked, whenStored};
  }

  it('names no column while nothing colours, sizes or asks, then the colour, size and asked columns the view renders', async () => {
    const {store, settle, asked} = await open();
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await settle();
    expect(asked().length).toBeGreaterThan(0);
    for (const r of asked()) expect(r.pointRows).toEqual([]);

    const before = asked().length;
    store.setColourBy('archive');
    store.setSizeBy('score');
    // `title` is not rendered and `chill` renders under another view, so neither is named.
    store.setPointColumns('hover', ['chill', 'heat', 'title']);
    await settle();
    const after = asked().slice(before);
    expect(after.length).toBeGreaterThan(0);
    expect(after.at(-1)!.pointRows).toEqual(['archive', 'score', 'heat']);
    for (const band of store.get('marks').bands) expect(Object.keys(band.scalars).sort()).toEqual(['archive', 'heat', 'score']);

    // Withdrawing the ask leaves the colour and size columns.
    store.setPointColumns('hover', []);
    store.setView({bbox: [0, 0, 50, 50], width: 400, height: 400});
    await settle();
    expect(asked().at(-1)!.pointRows).toEqual(['archive', 'score']);
  });

  it('fetches the held bands again when the colour column changes to one they lack, and not when it changes back', async () => {
    const {store, settle, asked} = await open();
    store.setColourBy('archive');
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await settle();
    expect(asked().at(-1)!.pointRows).toEqual(['archive']);
    for (const band of store.get('marks').bands) expect('score' in band.scalars).toBe(false);

    const before = asked().length;
    store.setColourBy('score');
    await settle();
    const refetched = asked().slice(before);
    expect(refetched.length).toBeGreaterThan(0);
    for (const r of refetched) expect(r.pointRows).toEqual(['score']);
    const bands = store.get('marks').bands;
    expect(bands.length).toBeGreaterThan(0);
    for (const band of bands) expect('score' in band.scalars).toBe(true);
    expect(store.get('legend').domains.score).toBeDefined();

    // The refetched bands kept the column they were fetched without, so going back asks for nothing.
    const settled = asked().length;
    store.setColourBy('archive');
    await settle();
    expect(asked().length).toBe(settled);
  });

  it('fetches again a band stored under the old colour column and marked held after the colour changed', async () => {
    const {store, settle, asked, whenStored} = await open();
    store.setColourBy('archive');
    whenStored(() => store.setColourBy('score'));
    store.setView({bbox: [0, 0, 100, 200], width: 400, height: 400});
    await settle();
    expect(asked()[0]!.pointRows).toEqual(['archive']);
    await settle();
    await settle();
    expect(asked().some((r) => Array.isArray(r.pointRows) && r.pointRows.includes('score'))).toBe(true);
    const bands = store.get('marks').bands;
    expect(bands.length).toBeGreaterThan(0);
    for (const band of bands) expect('score' in band.scalars).toBe(true);
  });
});
