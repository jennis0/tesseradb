import {Table, tableFromArrays} from 'apache-arrow';
import {describe, expect, it, vi} from 'vitest';
import {TesseraError, type TesseraClient} from '../src/client.js';
import type {FilterDraft} from '../src/filters.js';
import {createStore, DEFAULT_BUDGET} from '../src/store.js';
import type {AggregateRequest, AggregateResult, Meta} from '../src/types.js';
import {camera, fakeClock, fakeScheduler, meta, response, result as viewportResult, view} from './support.js';

/**
 * The store's `aggregates` projection against a fake client: what each registered aggregate asks
 * for, when it asks again, and what it publishes.
 */

const META = meta({
  views: [view('s0'), view('s1')],
  filterOperands: [
    {column: 'archive', family: 'category', operands: ['in']},
    {column: 'year', family: 'numeric', operands: ['range']}
  ]
});

const DRAFT: FilterDraft = {filter: {archive: {family: 'category', keys: ['cs']}}, highlight: {}};

/** An answer to `req`: one table per grouping, `total` its number. */
function answer(req: AggregateRequest, total: number, identityKey = 'ik'): AggregateResult {
  return {
    tables: req.groupings.map((_, grouping) => ({grouping, total, referenceTotal: null, groups: null, sample: null, rows: new Table()})),
    region: null,
    recomposed: false,
    identityKey,
    next: null
  };
}

/**
 * A store over a fake client whose `aggregate` waits until the test releases it, so a request in
 * flight is a state the test can hold.
 */
async function storeWith(
  opts: {
    refuse?: boolean;
    identityKey?: string;
    /** The meta each read returns in turn, the last one from then on. */
    meta?: Meta | Meta[];
    answer?: (req: AggregateRequest, total: number, identityKey?: string) => AggregateResult;
    /** The content key each viewport answer carries. */
    contentKey?: () => string;
    revalidateAfterMs?: number;
  } = {}
) {
  const clock = fakeClock();
  const scheduler = fakeScheduler();
  const metas = opts.meta === undefined ? [META] : Array.isArray(opts.meta) ? opts.meta : [opts.meta];
  let reads = 0;
  const pending: {req: AggregateRequest; signal: AbortSignal; release: (total: number, identityKey?: string) => void; fail: (error: unknown) => void}[] = [];
  const aggregate = vi.fn(
    (_token: string, req: AggregateRequest, signal: AbortSignal) =>
      new Promise<AggregateResult>((resolve, reject) => {
        signal.addEventListener('abort', () => reject(signal.reason));
        pending.push({
          req,
          signal,
          fail: reject,
          release: (total, identityKey) => (opts.refuse ? reject(new TesseraError(422, 'contract', 'refused')) : resolve((opts.answer ?? answer)(req, total, identityKey ?? opts.identityKey)))
        });
      })
  );
  const client = {
    meta: async () => metas[Math.min(reads++, metas.length - 1)],
    viewport: async () => response(viewportResult(), opts.contentKey ? {contentKey: opts.contentKey()} : {}),
    aggregate,
    close: () => {}
  } as unknown as TesseraClient;
  const store = createStore({
    viewerUrl: 'http://viewer',
    token: 'tok',
    client,
    clock,
    scheduler,
    prefetch: false,
    ...(opts.revalidateAfterMs === undefined ? {} : {replica: {revalidateAfterMs: opts.revalidateAfterMs}})
  });
  await clock.advance(1);
  return {store, pending, aggregate, clock, scheduler};
}

const flush = () => new Promise((resolve) => setTimeout(resolve, 0));

describe('the aggregates projection', () => {
  it('asks with the groupings as given and the store’s filters, and no reference unless one is asked for', async () => {
    const {store, pending} = await storeWith();
    store.setFilters(DRAFT);
    store.setAggregate('legend', {groupings: [{by: {field: 'archive', top: 5}}]});
    store.setAggregate('panel', {groupings: [{}], reference: 'visible'});
    store.setAggregate('other', {groupings: [{}], reference: {archive: {in: ['hep']}}});
    await flush();
    expect(pending.map((p) => p.req)).toEqual([
      {view: 's0', groupings: [{by: {field: 'archive', top: 5}}], filters: store.requestFilters()},
      {view: 's0', groupings: [{}], filters: store.requestFilters(), reference: {}},
      {view: 's0', groupings: [{}], filters: store.requestFilters(), reference: {archive: {in: ['hep']}}}
    ]);
    expect(store.requestFilters()).not.toBeNull();
  });

  it('sends the aggregates asked for in one task over the same set as one request, and gives each its own tables', async () => {
    const {store, pending} = await storeWith();
    store.setAggregate('legend', {groupings: [{by: {field: 'archive', top: 5}}]});
    store.setAggregate('list', {groupings: [{}]});
    await flush();
    expect(pending.map((p) => p.req)).toEqual([{view: 's0', groupings: [{by: {field: 'archive', top: 5}}, {}]}]);
    pending[0]!.release(7);
    await flush();
    for (const id of ['legend', 'list']) {
      const entry = store.get('aggregates').get(id)!;
      expect(entry.status).toBe('shown');
      expect(entry.result!.tables.map((t) => t.grouping)).toEqual([0]);
    }
  });

  it('sends each joined aggregate alone where the joined request is refused, so one bad grouping refuses only its own', async () => {
    const {store, pending} = await storeWith();
    store.setAggregate('good', {groupings: [{}]});
    store.setAggregate('bad', {groupings: [{by: {field: 'archive', top: 5}}]});
    await flush();
    expect(pending).toHaveLength(1);
    pending[0]!.fail(new TesseraError(422, 'contract', 'no such field'));
    await flush();
    expect(pending.slice(1).map((p) => p.req.groupings)).toEqual([[{}], [{by: {field: 'archive', top: 5}}]]);
    pending[1]!.release(7);
    pending[2]!.fail(new TesseraError(422, 'contract', 'no such field'));
    await flush();
    expect(store.get('aggregates').get('good')).toMatchObject({status: 'shown'});
    expect(store.get('aggregates').get('bad')).toMatchObject({status: 'refused', refusal: {code: 'contract'}});
  });

  it('joins the highlight to the filters by all_of under highlighted, and sends the filters alone without a highlight', async () => {
    const {store, pending} = await storeWith();
    store.setAggregate('lit', {groupings: [{}], highlighted: true});
    await flush();
    expect(pending[0]!.req).toEqual({view: 's0', groupings: [{}]});
    store.setFilters({filter: DRAFT.filter, highlight: {year: {family: 'numeric', gte: 2020, lte: null}}});
    await flush();
    const filters = store.requestFilters();
    expect(filters).not.toBeNull();
    expect(pending.at(-1)!.req.filters).toEqual({all_of: [filters, {year: {range: {gte: 2020}}}]});
    store.setFilters({filter: {}, highlight: {year: {family: 'numeric', gte: 2020, lte: null}}});
    await flush();
    expect(pending.at(-1)!.req.filters).toEqual({year: {range: {gte: 2020}}});
  });

  it('sends no filters where none is set, and publishes loading, then the answer with its view', async () => {
    const {store, pending} = await storeWith();
    store.setAggregate('a', {groupings: [{}]});
    expect(store.get('aggregates').get('a')).toEqual({status: 'loading', result: null, view: null, refusal: null, summaries: []});
    await flush();
    expect(pending[0]!.req).toEqual({view: 's0', groupings: [{}]});
    pending[0]!.release(7);
    await flush();
    const entry = store.get('aggregates').get('a')!;
    expect(entry.status).toBe('shown');
    expect(entry.view).toBe('s0');
    expect(entry.result!.tables[0]!.total).toBe(7);
  });

  it('asks again when the filters change, aborting the request it replaces and keeping the last answer while it loads', async () => {
    const {store, pending} = await storeWith();
    store.setAggregate('a', {groupings: [{}]});
    await flush();
    pending[0]!.release(7);
    await flush();
    store.setFilters(DRAFT);
    expect(store.get('aggregates').get('a')).toMatchObject({status: 'loading', view: 's0'});
    expect(store.get('aggregates').get('a')!.result!.tables[0]!.total).toBe(7);
    await flush();
    expect(pending[1]!.req.filters).toEqual(store.requestFilters());
    // A second change before the answer supersedes the request in flight.
    store.setMembers([{layer: 'topics', artifact: 7n, outside: false, verb: 'filter'}]);
    await flush();
    expect(pending[1]!.signal.aborted).toBe(true);
    expect(pending).toHaveLength(3);
    // The superseded answer landing late publishes nothing.
    pending[1]!.release(1);
    pending[2]!.release(3);
    await flush();
    expect(store.get('aggregates').get('a')!.result!.tables[0]!.total).toBe(3);
  });

  it('asks again in the new view at a view switch, dropping the answer from the old one', async () => {
    const {store, pending} = await storeWith();
    store.setAggregate('a', {groupings: [{}]});
    await flush();
    pending[0]!.release(7);
    await flush();
    store.setCurrentView('s1');
    expect(store.get('aggregates').get('a')).toEqual({status: 'loading', result: null, view: null, refusal: null, summaries: []});
    await flush();
    expect(pending[1]!.req.view).toBe('s1');
  });

  it('asks again when the region is selected, with the region in the filters', async () => {
    const {store, pending} = await storeWith();
    store.setAggregate('a', {groupings: [{}]});
    await flush();
    store.select({kind: 'box', bbox: [0, 0, 0.5, 0.5]});
    await flush();
    expect(pending[0]!.signal.aborted).toBe(true);
    expect(JSON.stringify(pending[1]!.req.filters)).toContain('region');
  });

  it('drops an answer under another identity key, forgets what it held and asks again', async () => {
    const {store, pending} = await storeWith();
    store.setAggregate('a', {groupings: [{}]});
    await flush();
    pending[0]!.release(7, 'first');
    await flush();
    store.setFilters(DRAFT);
    await flush();
    // Another viewer's key: the answer is not published, and the store reads meta and asks again.
    pending[1]!.release(9, 'second');
    await flush();
    await flush();
    expect(store.get('aggregates').get('a')).toEqual({status: 'loading', result: null, view: null, refusal: null, summaries: []});
    expect(pending).toHaveLength(3);
    pending[2]!.release(9, 'second');
    await flush();
    expect(store.get('aggregates').get('a')).toMatchObject({status: 'shown', view: 's0'});
    expect(store.get('aggregates').get('a')!.result!.tables[0]!.total).toBe(9);
  });

  it('publishes a refusal, and drops an id set to null', async () => {
    const {store, pending} = await storeWith({refuse: true});
    store.setAggregate('a', {groupings: [{}]});
    await flush();
    pending[0]!.release(0);
    await flush();
    expect(store.get('aggregates').get('a')).toMatchObject({status: 'refused', result: null, refusal: {code: 'contract'}});
    store.setAggregate('a', null);
    expect(store.get('aggregates').has('a')).toBe(false);
    store.setFilters(DRAFT);
    await flush();
    expect(pending).toHaveLength(1);
  });

  it('asks nothing after dispose, and aborts what is in flight', async () => {
    const {store, pending, aggregate} = await storeWith();
    store.setAggregate('a', {groupings: [{}]});
    await flush();
    store.dispose();
    expect(pending[0]!.signal.aborted).toBe(true);
    store.setAggregate('b', {groupings: [{}]});
    await flush();
    expect(aggregate).toHaveBeenCalledTimes(1);
  });
});

describe('an aggregate that leaves out one clause', () => {
  const BOTH: FilterDraft = {filter: {archive: {family: 'category', keys: ['cs']}, year: {family: 'numeric', gte: 2020, lte: null}}, highlight: {}};

  it('sends every clause but the one it names, keeps its request when only that clause changes, and asks again when another does', async () => {
    const {store, pending} = await storeWith();
    store.setFilters(BOTH);
    store.setAggregate('archives', {groupings: [{by: {field: 'archive', top: 5}}], without: 'archive'});
    await flush();
    // The year clause still narrows the counts; the archive clause does not hide the other archives.
    expect(pending[0]!.req.filters).toEqual({year: {range: {gte: 2020}}});
    store.setFilters({...BOTH, filter: {...BOTH.filter, archive: {family: 'category', keys: ['hep']}}});
    await flush();
    expect(pending[0]!.signal.aborted).toBe(false);
    expect(pending).toHaveLength(1);
    store.setFilters({...BOTH, filter: {...BOTH.filter, archive: {family: 'category', keys: ['hep']}, year: {family: 'numeric', gte: 2021, lte: null}}});
    await flush();
    expect(pending[0]!.signal.aborted).toBe(true);
    expect(pending[1]!.req.filters).toEqual({year: {range: {gte: 2021}}});
    // Without `without`, every clause is sent.
    store.setAggregate('all', {groupings: [{}]});
    await flush();
    expect(pending[2]!.req.filters).toEqual(store.requestFilters());
    expect(JSON.stringify(pending[2]!.req.filters)).toContain('hep');
  });

  it('sends a grouping by bins as given, less the clause on its own field', async () => {
    const {store, pending} = await storeWith();
    store.setFilters(BOTH);
    store.setAggregate('years', {groupings: [{by: {field: 'year', bins: 20, range: [1990, 2030]}}], without: 'year', reference: 'visible'});
    await flush();
    expect(pending[0]!.req.groupings).toEqual([{by: {field: 'year', bins: 20, range: [1990, 2030]}}]);
    expect(pending[0]!.req.reference).toEqual({});
    expect(JSON.stringify(pending[0]!.req.filters)).not.toContain('year');
    expect(JSON.stringify(pending[0]!.req.filters)).toContain('archive');
  });
});

describe('an aggregate that leaves out one layer’s clauses', () => {
  it('sends every clause but that layer’s filter-position member_of clauses, and keeps its request when only they change', async () => {
    const {store, pending} = await storeWith();
    store.setFilters(DRAFT);
    const topic = {layer: 'topics', artifact: 7n, outside: false, verb: 'filter' as const};
    const venue = {layer: 'venues', artifact: 9n, outside: false, verb: 'filter' as const};
    const lit = {layer: 'topics', artifact: 8n, outside: false, verb: 'highlight' as const};
    store.setMembers([topic, venue, lit]);
    store.setAggregate('topics', {groupings: [{by: {layer: 'topics', top: 5}}], withoutMembersOf: 'topics'});
    await flush();
    // The archive clause and the other layer's clause still narrow the counts; the topic clause does not.
    expect(pending[0]!.req.filters).toEqual({all_of: [{archive: {in: ['cs']}}, {member_of: {layer: 'venues', artifact: '9'}}]});
    store.setMembers([venue, lit]);
    await flush();
    expect(pending[0]!.signal.aborted).toBe(false);
    expect(pending).toHaveLength(1);
    store.setMembers([lit]);
    await flush();
    expect(pending[0]!.signal.aborted).toBe(true);
    expect(pending[1]!.req.filters).toEqual({archive: {in: ['cs']}});
  });
});

describe('an aggregate the server sheds', () => {
  it('is sent again after the backoff, or the Retry-After where longer, and shows retrying meanwhile', async () => {
    const {store, pending, clock} = await storeWith();
    store.setAggregate('a', {groupings: [{}]});
    await flush();
    pending[0]!.fail(new TesseraError(429, 'backpressure', 'shed', 3));
    await flush();
    expect(store.get('aggregates').get('a')).toMatchObject({status: 'retrying', refusal: {code: 'backpressure'}});
    await clock.advance(2_999);
    expect(pending).toHaveLength(1);
    await clock.advance(1);
    await flush();
    expect(pending).toHaveLength(2);
    pending[1]!.release(4);
    await flush();
    expect(store.get('aggregates').get('a')).toMatchObject({status: 'shown', refusal: null});
  });

  it('is refused after the retries run out', async () => {
    const {store, pending, clock} = await storeWith();
    store.setAggregate('a', {groupings: [{}]});
    for (let i = 0; i < 3; i++) {
      await flush();
      pending[i]!.fail(new TesseraError(503, 'not-ready', 'starting'));
      await flush();
      await clock.advance(10_000);
    }
    await flush();
    expect(pending).toHaveLength(3);
    expect(store.get('aggregates').get('a')).toMatchObject({status: 'refused', refusal: {code: 'not-ready'}});
  });

  it('drops a waiting retry when a newer change asks again', async () => {
    const {store, pending, clock} = await storeWith();
    store.setAggregate('a', {groupings: [{}]});
    await flush();
    pending[0]!.fail(new TesseraError(429, 'backpressure', 'shed', 1));
    await flush();
    store.setFilters(DRAFT);
    await flush();
    expect(pending).toHaveLength(2);
    await clock.advance(5_000);
    await flush();
    expect(pending).toHaveLength(2);
    expect(pending[1]!.req.filters).toEqual(store.requestFilters());
  });
});

describe('an aggregate counted in view', () => {
  const BOX: [number, number, number, number] = [0.1, 0.2, 0.6, 0.7];
  const NEXT: [number, number, number, number] = [0.2, 0.2, 0.7, 0.7];
  const TOP = {by: {field: 'archive', top: 5}};
  /** A registered aggregate's request, which carries no reference; the counts in view carry one. */
  const registered = (p: {req: AggregateRequest}) => p.req.reference === undefined;
  const inView = (p: {req: AggregateRequest}) => registered(p) && JSON.stringify(p.req.filters ?? null).includes('bbox');
  const whole = (p: {req: AggregateRequest}) => registered(p) && !inView(p);

  it('is asked for once the camera has rested, over the camera’s box, and never while it moves; one over everything matched is not asked for again on a pan', async () => {
    const {store, pending, clock} = await storeWith();
    store.setAggregate('subject', {groupings: [TOP], subject: 'view'});
    store.setAggregate('match', {groupings: [TOP]});
    await flush();
    // No camera yet: only the one over everything matched goes out.
    expect(pending.filter(registered).map((p) => p.req)).toEqual([{view: 's0', groupings: [TOP]}]);
    expect(store.get('aggregates').get('subject')).toMatchObject({status: 'loading', result: null});
    store.setView(camera(store.frame(), BOX, 400, 400));
    await clock.advance(100);
    store.setView(camera(store.frame(), NEXT, 400, 400));
    await clock.advance(200);
    await flush();
    expect(pending.filter(inView)).toHaveLength(0);
    await clock.advance(50);
    await flush();
    expect(pending.filter(inView).map((p) => p.req)).toEqual([{view: 's0', groupings: [TOP], filters: {region: {bbox: NEXT}}}]);
    // A pan moves only the counts in view.
    store.setView(camera(store.frame(), BOX, 400, 400));
    await clock.advance(300);
    await flush();
    expect(pending.filter(inView).map((p) => p.req.filters)).toEqual([{region: {bbox: NEXT}}, {region: {bbox: BOX}}]);
    expect(pending.filter(whole)).toHaveLength(1);
    // Registered while the camera moves, it waits for the rest.
    store.setView(camera(store.frame(), NEXT, 400, 400));
    store.setAggregate('late', {groupings: [{by: {field: 'archive', top: 3}}], subject: 'view'});
    await flush();
    expect(pending.filter(inView)).toHaveLength(2);
    await clock.advance(300);
    await flush();
    expect(pending.filter(inView).at(-1)!.req.groupings).toEqual([TOP, {by: {field: 'archive', top: 3}}]);
  });

  it('keeps the last answer while the next loads, and aborts the request a later rest supersedes', async () => {
    const {store, pending, clock} = await storeWith();
    store.setAggregate('subject', {groupings: [TOP], subject: 'view'});
    store.setView(camera(store.frame(), BOX, 400, 400));
    await clock.advance(300);
    await flush();
    pending.filter(inView)[0]!.release(7);
    await flush();
    expect(store.get('aggregates').get('subject')).toMatchObject({status: 'shown'});
    store.setView(camera(store.frame(), NEXT, 400, 400));
    await clock.advance(300);
    await flush();
    const second = pending.filter(inView)[1]!;
    expect(store.get('aggregates').get('subject')!.status).toBe('loading');
    expect(store.get('aggregates').get('subject')!.result!.tables[0]!.total).toBe(7);
    store.setView(camera(store.frame(), BOX, 400, 400));
    await clock.advance(300);
    await flush();
    expect(second.signal.aborted).toBe(true);
    expect(pending.filter(inView)).toHaveLength(3);
    // The superseded answer landing late publishes nothing.
    second.release(1);
    pending.filter(inView)[2]!.release(3);
    await flush();
    expect(store.get('aggregates').get('subject')!.result!.tables[0]!.total).toBe(3);
  });

  it('sends one request a rest for every aggregate counted in view, and one a filter change for every one over everything matched', async () => {
    const {store, pending, clock} = await storeWith();
    const tops = [1, 2, 3, 4, 5].map((top) => ({by: {field: 'archive', top}}));
    tops.forEach((g, i) => store.setAggregate(`subject${i}`, {groupings: [g], subject: 'view'}));
    tops.forEach((g, i) => store.setAggregate(`match${i}`, {groupings: [g]}));
    store.setView(camera(store.frame(), BOX, 400, 400));
    await clock.advance(300);
    await flush();
    expect(pending.filter(inView).map((p) => p.req.groupings)).toEqual([tops]);
    expect(pending.filter(whole).map((p) => p.req.groupings)).toEqual([tops]);
    store.setFilters(DRAFT);
    await flush();
    expect(pending.filter(inView)).toHaveLength(2);
    expect(pending.filter(whole)).toHaveLength(2);
    expect(pending.filter(whole)[1]!.req.groupings).toEqual(tops);
    expect(pending.filter(inView)[1]!.req.groupings).toEqual(tops);
  });

  it('sends apart only the aggregates whose own clause is set, since a request carries one set of filters', async () => {
    const {store, pending} = await storeWith();
    store.setFilters(DRAFT);
    store.setAggregate('archive', {groupings: [TOP], without: 'archive'});
    store.setAggregate('year', {groupings: [{by: {field: 'year', bins: 10}}], without: 'year'});
    store.setAggregate('count', {groupings: [{}]});
    await flush();
    expect(pending.map((p) => p.req)).toEqual([
      {view: 's0', groupings: [TOP]},
      {view: 's0', groupings: [{by: {field: 'year', bins: 10}}, {}], filters: store.requestFilters()}
    ]);
  });

  it('starts another request where the groupings would pass the server’s limit', async () => {
    const {store, pending} = await storeWith({meta: meta({...META, selection: {...META.selection, maxAggregateGroupings: 2}})});
    for (let i = 0; i < 5; i++) store.setAggregate(`m${i}`, {groupings: [{by: {field: 'archive', top: i + 1}}]});
    await flush();
    expect(pending.map((p) => p.req.groupings.length)).toEqual([2, 2, 1]);
  });

  it('counts the highlighted items in view while a highlight is set, and the matched ones once it is cleared', async () => {
    const {store, pending, clock} = await storeWith();
    store.setAggregate('subject', {groupings: [TOP], subject: 'view', highlighted: true});
    store.setAggregate('match', {groupings: [TOP]});
    store.setView(camera(store.frame(), BOX, 400, 400));
    await clock.advance(300);
    await flush();
    expect(pending.filter(inView).map((p) => p.req.filters)).toEqual([{region: {bbox: BOX}}]);
    store.setFilters({filter: {}, highlight: {archive: {family: 'category', keys: ['cs']}}});
    await flush();
    expect(pending.filter(inView).at(-1)!.req.filters).toEqual({all_of: [{archive: {in: ['cs']}}, {region: {bbox: BOX}}]});
    store.setFilters({filter: {}, highlight: {}});
    await flush();
    expect(pending.filter(inView).map((p) => p.req.filters)).toEqual([{region: {bbox: BOX}}, {all_of: [{archive: {in: ['cs']}}, {region: {bbox: BOX}}]}, {region: {bbox: BOX}}]);
    // The highlight plays no part in the counts over everything matched.
    expect(pending.filter(whole)).toHaveLength(1);
  });

  it('drops every answer when the viewer changes, and asks again under the next one', async () => {
    const {store, pending, clock} = await storeWith();
    store.setAggregate('subject', {groupings: [TOP], subject: 'view'});
    store.setAggregate('match', {groupings: [TOP]});
    store.setView(camera(store.frame(), BOX, 400, 400));
    await clock.advance(300);
    await flush();
    for (const p of pending.filter(registered)) p.release(7);
    await flush();
    expect(store.get('aggregates').get('subject')).toMatchObject({status: 'shown'});
    store.clear();
    for (const id of ['subject', 'match']) expect(store.get('aggregates').get(id)).toMatchObject({status: 'loading', result: null, view: null});
    await clock.advance(1);
    await flush();
    expect(pending.filter(inView)).toHaveLength(2);
    expect(pending.filter(whole)).toHaveLength(2);
    // An answer under another viewer's key is dropped as well.
    store.setFilters(DRAFT);
    await flush();
    for (const p of pending.filter(registered).slice(4)) p.release(5, 'second');
    await flush();
    await flush();
    for (const id of ['subject', 'match']) expect(store.get('aggregates').get(id)).toMatchObject({result: null});
  });
});

describe('an aggregate over the whole visible set', () => {
  it('sends no filters, and is asked for again at a refresh, not at a change of filter or camera', async () => {
    const {store, pending, clock} = await storeWith();
    store.setFilters(DRAFT);
    store.setAggregate('figures', {groupings: [{by: {field: 'year', summary: true}}], subject: 'visible', without: 'year', highlighted: true});
    await flush();
    expect(pending.map((p) => p.req)).toEqual([{view: 's0', groupings: [{by: {field: 'year', summary: true}}]}]);
    pending[0]!.release(7);
    await flush();
    store.setFilters({filter: {}, highlight: DRAFT.filter});
    store.setView(camera(store.frame(), [0.1, 0.1, 0.5, 0.5], 400, 400));
    await clock.advance(300);
    await flush();
    expect(pending.filter((p) => p.req.reference === undefined)).toHaveLength(1);
    store.refresh();
    await flush();
    expect(pending.filter((p) => p.req.reference === undefined)).toHaveLength(2);
  });

  it('publishes a summary’s figures beside its table', async () => {
    const figures = (req: AggregateRequest, total: number, identityKey = 'ik'): AggregateResult => ({
      ...answer(req, total, identityKey),
      tables: req.groupings.map((g, grouping) => ({
        grouping,
        total,
        referenceTotal: null,
        groups: null,
        sample: null,
        rows:
          g.by && 'summary' in g.by
            ? tableFromArrays({items: BigInt64Array.of(10n), count: BigInt64Array.of(8n), none: BigInt64Array.of(2n), min: BigInt64Array.of(1990n), max: BigInt64Array.of(2024n), mean: Float64Array.of(2011.5)})
            : new Table()
      }))
    });
    const {store, pending} = await storeWith({answer: figures});
    store.setAggregate('card', {groupings: [{by: {field: 'year', bins: 10, sample: 1000}}, {by: {field: 'year', summary: true}}], subject: 'visible'});
    await flush();
    pending[0]!.release(10);
    await flush();
    expect(store.get('aggregates').get('card')!.summaries).toEqual([null, {items: 10n, count: 8n, none: 2n, min: 1990n, max: 2024n, mean: 2011.5}]);
  });
});

describe('the point budget', () => {
  it('is 250,000 unless the store is made with another', () => {
    const client = {meta: async () => META, viewport: async () => response(viewportResult()), close: () => {}} as unknown as TesseraClient;
    const made = (budget?: number) =>
      createStore({viewerUrl: 'http://viewer', token: 'tok', client, clock: fakeClock(), scheduler: fakeScheduler(), prefetch: false, ...(budget === undefined ? {} : {budget})});
    expect(DEFAULT_BUDGET).toBe(250_000);
    expect(made().budget).toBe(250_000);
    expect(made(1_000).budget).toBe(1_000);
  });
});

describe('an aggregate across a change of viewer or corpus', () => {
  const BOX: [number, number, number, number] = [0.1, 0.2, 0.6, 0.7];
  const TOP = {by: {field: 'archive', top: 5}};
  const registered = (p: {req: AggregateRequest}) => p.req.reference === undefined;
  const inView = (p: {req: AggregateRequest}) => registered(p) && JSON.stringify(p.req.filters ?? null).includes('bbox');

  it('is composed under the next viewer’s meta, so a clause on a column it does not list is not sent', async () => {
    const next = meta({...META, filterOperands: [{column: 'archive', family: 'category', operands: ['in']}]});
    const {store, pending, clock} = await storeWith({meta: [META, next]});
    store.setFilters({filter: {year: {family: 'numeric', gte: 2020, lte: null}}, highlight: {}});
    store.setAggregate('a', {groupings: [TOP]});
    await flush();
    expect(JSON.stringify(pending[0]!.req.filters)).toContain('year');
    store.clear();
    await clock.advance(1);
    await flush();
    expect(pending).toHaveLength(2);
    expect(JSON.stringify(pending[1]!.req.filters ?? null)).not.toContain('year');
  });

  it('is not sent over the old camera’s box in a view the next viewer is not offered', async () => {
    const next = meta({...META, views: [view('s1')]});
    const {store, pending, clock} = await storeWith({meta: [META, next]});
    store.setAggregate('subject', {groupings: [TOP], subject: 'view'});
    store.setView(camera(store.frame(), BOX, 400, 400));
    await clock.advance(300);
    await flush();
    pending.filter(inView)[0]!.release(7);
    await flush();
    const before = pending.length;
    store.clear();
    await clock.advance(1);
    await flush();
    await clock.advance(300);
    await flush();
    expect(pending.slice(before).filter((p) => JSON.stringify(p.req).includes('bbox'))).toEqual([]);
    expect(store.get('aggregates').get('subject')).toMatchObject({status: 'loading', result: null});
  });

  it('shows nothing for the next viewer from requests in flight across clear(), released afterwards', async () => {
    const {store, pending, clock} = await storeWith();
    store.setAggregate('subject', {groupings: [TOP], subject: 'view'});
    store.setAggregate('subject2', {groupings: [{by: {field: 'archive', top: 3}}], subject: 'view'});
    store.setAggregate('match', {groupings: [TOP]});
    store.setView(camera(store.frame(), BOX, 400, 400));
    await clock.advance(300);
    await flush();
    const old = pending.filter(registered);
    expect(old.filter(inView)).toHaveLength(1);
    expect(old.filter(inView)[0]!.req.groupings).toHaveLength(2);
    store.clear();
    for (const p of old) p.release(7);
    await clock.advance(1);
    await flush();
    for (const id of ['subject', 'subject2', 'match']) expect(store.get('aggregates').get(id)).toMatchObject({status: 'loading', result: null});
  });

  it('sends nothing when the rest the camera was waiting for falls after clear()', async () => {
    const {store, pending, clock} = await storeWith();
    store.setAggregate('subject', {groupings: [TOP], subject: 'view'});
    store.setView(camera(store.frame(), BOX, 400, 400));
    await clock.advance(100);
    store.clear();
    await clock.advance(1);
    await flush();
    const sent = pending.length;
    await clock.advance(300);
    await flush();
    expect(pending.length).toBe(sent);
  });

  it('is asked again, its request unchanged, when a frame observes a new content key', async () => {
    let key = 'ck-1';
    const {store, pending, clock, scheduler} = await storeWith({contentKey: () => key, revalidateAfterMs: 100});
    store.setAggregate('match', {groupings: [TOP]});
    store.setView(camera(store.frame(), BOX, 400, 400));
    await clock.advance(600);
    scheduler.flush();
    await flush();
    const whole = () => pending.filter((p) => registered(p) && !inView(p));
    expect(whole()).toHaveLength(1);
    whole()[0]!.release(7);
    await flush();
    key = 'ck-2';
    await clock.advance(200);
    store.setView(camera(store.frame(), BOX, 400, 400));
    await clock.advance(600);
    scheduler.flush();
    await flush();
    expect(whole()).toHaveLength(2);
    expect(whole()[1]!.req).toEqual(whole()[0]!.req);
    // The answer held stays while the new one loads.
    expect(store.get('aggregates').get('match')!.result!.tables[0]!.total).toBe(7);
  });

  it('asks the counts in view again, their request unchanged, when a frame observes a new content key', async () => {
    let key = 'ck-1';
    const {store, pending, clock, scheduler} = await storeWith({contentKey: () => key, revalidateAfterMs: 100});
    store.setView(camera(store.frame(), BOX, 400, 400));
    await clock.advance(600);
    scheduler.flush();
    await flush();
    const strip = () => pending.filter((p) => p.req.reference !== undefined);
    expect(strip()).toHaveLength(1);
    strip()[0]!.release(7);
    await flush();
    // A rest on the same box with the same corpus asks nothing.
    store.setView(camera(store.frame(), BOX, 400, 400));
    await clock.advance(50);
    await flush();
    expect(strip()).toHaveLength(1);
    key = 'ck-2';
    await clock.advance(200);
    store.setView(camera(store.frame(), BOX, 400, 400));
    await clock.advance(600);
    scheduler.flush();
    await flush();
    expect(strip()).toHaveLength(2);
    expect(strip()[1]!.req).toEqual(strip()[0]!.req);
  });

  it('limits a view aggregate’s reference to the same area, so its lift compares with what is visible there', async () => {
    const {store, pending, clock} = await storeWith();
    store.setAggregate('lift', {groupings: [TOP], subject: 'view', reference: 'visible'});
    store.setAggregate('other', {groupings: [TOP], subject: 'view', reference: {archive: {in: ['hep']}}});
    store.setView(camera(store.frame(), BOX, 400, 400));
    await clock.advance(300);
    await flush();
    const asked = pending.filter((p) => JSON.stringify(p.req.filters ?? null).includes('bbox') && p.req.groupings[0] === TOP);
    expect(asked.map((p) => p.req.reference)).toEqual([{region: {bbox: BOX}}, {all_of: [{archive: {in: ['hep']}}, {region: {bbox: BOX}}]}]);
  });
});

describe('an aggregate ranking a layer at the drawn cut', () => {
  const BOX: [number, number, number, number] = [0.1, 0.2, 0.6, 0.7];
  const TREE = {by: {layer: 'tree', top: 5, cut: 'drawn' as const}};
  type Cut = {zoom: number; bbox: number[]; budget?: number};
  const ranked = (p: {req: AggregateRequest}) => p.req.groupings.some((g) => g.by !== undefined && 'cut' in g.by);
  const cutOf = (p: {req: AggregateRequest}) => (p.req.groupings[0]!.by as {cut: Cut}).cut;

  it('waits for the camera, sends the cut the map draws, and asks again when the cluster budget changes', async () => {
    const {store, pending, clock} = await storeWith();
    store.setAggregate('card', {groupings: [TREE]});
    await flush();
    expect(pending.filter(ranked)).toHaveLength(0);
    expect(store.get('aggregates').get('card')).toMatchObject({status: 'loading', result: null});
    store.setView(camera(store.frame(), BOX, 400, 400));
    await clock.advance(300);
    await flush();
    expect(pending.filter(ranked)).toHaveLength(1);
    const cut = cutOf(pending.filter(ranked)[0]!);
    expect(cut.zoom).toBeGreaterThan(0);
    expect(cut.bbox[0]).toBeLessThanOrEqual(BOX[0]);
    expect(cut.bbox[2]).toBeGreaterThanOrEqual(BOX[2]);
    expect('budget' in cut).toBe(false);
    expect(store.clusterBudget).toBeNull();
    store.setClusterBudget(40);
    await flush();
    expect(pending.filter(ranked).map(cutOf)).toEqual([cut, {...cut, budget: 40}]);
    expect(store.clusterBudget).toBe(40);
    // Not a whole number above zero, or the same budget: nothing changes and nothing is asked.
    store.setClusterBudget(0);
    store.setClusterBudget(2.5);
    store.setClusterBudget(40);
    await flush();
    expect(pending.filter(ranked)).toHaveLength(2);
  });
});
