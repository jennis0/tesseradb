import {Table} from 'apache-arrow';
import {describe, expect, it} from 'vitest';
import type {TesseraClient} from '../src/client.js';
import type {FilterDraft} from '../src/filters.js';
import {createStore} from '../src/store.js';
import type {AggregateRequest, AggregateResult, FilterExpr} from '../src/types.js';
import {fakeClock, fakeScheduler, meta, response, result as viewportResult, view} from './support.js';

/**
 * The store's counts in view (`view.inView`) against a fake server that counts as the real one
 * does: the reference set is the area alone, so its total does not move with the filters.
 */

const META = meta({
  views: [view('s0')],
  filterOperands: [{column: 'archive', family: 'category', operands: ['in']}]
});

const BOX: [number, number, number, number] = [0.1, 0.2, 0.6, 0.7];
const LEAF = {region: {bbox: BOX}};
const CS: FilterDraft = {filter: {archive: {family: 'category', keys: ['cs']}}, highlight: {}};
const LIT: FilterDraft = {filter: {}, highlight: {archive: {family: 'category', keys: ['cs']}}};

/** Whether `expr` names the archive filter anywhere. */
function narrows(expr: FilterExpr | undefined): boolean {
  return expr !== undefined && JSON.stringify(expr).includes('archive');
}

/** 1,000 items visible in any area, 250 of them in `cs`. */
function counted(req: AggregateRequest): AggregateResult {
  const total = narrows(req.filters) ? 250 : 1_000;
  return {
    tables: req.groupings.map((_, grouping) => ({grouping, total, referenceTotal: req.reference === undefined ? null : 1_000, groups: null, rows: new Table()})),
    region: {exact: true, depth: null},
    recomposed: false,
    identityKey: 'ik',
    next: null
  };
}

async function storeWith() {
  const clock = fakeClock();
  const asked: AggregateRequest[] = [];
  const client = {
    meta: async () => META,
    viewport: async () => response(viewportResult()),
    aggregate: async (_token: string, req: AggregateRequest) => {
      asked.push(req);
      return counted(req);
    },
    close: () => {}
  } as unknown as TesseraClient;
  const store = createStore({viewerUrl: 'http://viewer', token: 'tok', client, clock, scheduler: fakeScheduler(), prefetch: false});
  await clock.advance(1);
  return {store, asked, clock};
}

const flush = () => new Promise((resolve) => setTimeout(resolve, 0));

describe('the counts in view', () => {
  it('asks once the camera has rested, over the camera’s box, with the box as the reference', async () => {
    const {store, asked, clock} = await storeWith();
    store.setView({bbox: BOX, width: 400, height: 400});
    await clock.advance(100);
    store.setView({bbox: BOX, width: 400, height: 400});
    await clock.advance(100);
    await flush();
    expect(asked).toHaveLength(0);
    await clock.advance(200);
    await flush();
    expect(asked).toEqual([{view: 's0', groupings: [{}], filters: LEAF, reference: LEAF}]);
    expect(store.get('view').inView).toMatchObject({status: 'shown', visible: {value: 1_000, exact: true}, matched: {value: 1_000, exact: true}});
  });

  it('keeps the visible count when a filter changes and the camera does not', async () => {
    const {store, asked, clock} = await storeWith();
    store.setView({bbox: BOX, width: 400, height: 400});
    await clock.advance(300);
    await flush();
    store.setFilters(CS);
    await flush();
    expect(asked.at(-1)!.reference).toEqual(LEAF);
    const n = store.get('view').inView!;
    expect(n.visible.value).toBe(1_000);
    expect(n.matched.value).toBe(250);
  });

  it('counts the highlight beside the filters while one is set', async () => {
    const {store, clock} = await storeWith();
    store.setView({bbox: BOX, width: 400, height: 400});
    await clock.advance(300);
    store.setFilters(LIT);
    await flush();
    const n = store.get('view').inView!;
    expect(n.matched.value).toBe(1_000);
    expect(n.highlighted.value).toBe(250);
  });

  it('counts the selected region in place of the box, and the region carries the same figures', async () => {
    const {store, asked, clock} = await storeWith();
    store.setView({bbox: [0, 0, 1, 1], width: 400, height: 400});
    await clock.advance(300);
    store.setFilters(CS);
    store.select({kind: 'box', bbox: BOX});
    await flush();
    const last = asked.at(-1)!;
    expect(last.reference).toEqual(LEAF);
    expect(JSON.stringify(last.filters)).toContain(JSON.stringify(LEAF));
    const n = store.get('view').inView!;
    expect(n.visible.value).toBe(1_000);
    expect(n.matched.value).toBe(250);
  });
});
