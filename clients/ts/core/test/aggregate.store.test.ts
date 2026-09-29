import {Table} from 'apache-arrow';
import {describe, expect, it, vi} from 'vitest';
import {TesseraError, type TesseraClient} from '../src/client.js';
import type {FilterDraft} from '../src/filters.js';
import {createStore} from '../src/store.js';
import type {AggregateRequest, AggregateResult} from '../src/types.js';
import {fakeClock, fakeScheduler, meta, response, result as viewportResult, view} from './support.js';

/**
 * The store's `aggregates` projection against a fake client: what each registered aggregate asks
 * for, when it asks again, and what it publishes.
 */

const META = meta({
  views: [view('s0'), view('s1')],
  filterOperands: [{column: 'archive', family: 'category', operands: ['in']}]
});

const DRAFT: FilterDraft = {filter: {archive: {family: 'category', keys: ['cs']}}, highlight: {}};

/** An answer to `req`: one table per grouping, `total` its number. */
function answer(req: AggregateRequest, total: number, identityKey = 'ik'): AggregateResult {
  return {
    tables: req.groupings.map((_, grouping) => ({grouping, total, referenceTotal: null, groups: null, rows: new Table()})),
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
async function storeWith(opts: {refuse?: boolean; identityKey?: string} = {}) {
  const clock = fakeClock();
  const pending: {req: AggregateRequest; signal: AbortSignal; release: (total: number, identityKey?: string) => void}[] = [];
  const aggregate = vi.fn(
    (_token: string, req: AggregateRequest, signal: AbortSignal) =>
      new Promise<AggregateResult>((resolve, reject) => {
        signal.addEventListener('abort', () => reject(signal.reason));
        pending.push({
          req,
          signal,
          release: (total, identityKey) => (opts.refuse ? reject(new TesseraError(422, 'contract', 'refused')) : resolve(answer(req, total, identityKey ?? opts.identityKey)))
        });
      })
  );
  const client = {
    meta: async () => META,
    viewport: async () => response(viewportResult()),
    aggregate,
    close: () => {}
  } as unknown as TesseraClient;
  const store = createStore({viewerUrl: 'http://viewer', token: 'tok', client, clock, scheduler: fakeScheduler(), prefetch: false});
  await clock.advance(1);
  return {store, pending, aggregate, clock};
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

  it('sends no filters where none is set, and publishes loading, then the answer with its view', async () => {
    const {store, pending} = await storeWith();
    store.setAggregate('a', {groupings: [{}]});
    expect(store.get('aggregates').get('a')).toEqual({status: 'loading', result: null, view: null, refusal: null});
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
    store.setMembers([]);
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
    expect(store.get('aggregates').get('a')).toEqual({status: 'loading', result: null, view: null, refusal: null});
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
    expect(store.get('aggregates').get('a')).toEqual({status: 'loading', result: null, view: null, refusal: null});
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
