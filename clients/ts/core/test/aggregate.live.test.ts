import type {Table} from 'apache-arrow';
import {afterAll, beforeAll, describe, expect, it, type TestContext} from 'vitest';
import {TesseraClient} from '../src/client.js';
import {createStore} from '../src/store.js';
import type {AggregateRequest, FilterExpr, Meta, Session} from '../src/types.js';
import {start, type Served} from './served.js';

/**
 * `TesseraClient.aggregate` and the store's `aggregates` projection against a real `tessera serve`
 * over the notebook corpus (`served.ts`). Each count is compared with what the viewport or the items
 * read counts for the same set, and a result carried over many small responses with the same result
 * read in one.
 */

const TERMS = ['cs.LG', 'cs.CV', 'hep-ph'];
const CS: FilterExpr = {archive: {in: ['cs']}};

let served: Served | string = 'the server has not started';
let client: TesseraClient;
let session: Session;
let meta: Meta;
/** The aggregate requests `client` has sent, one per response. */
let requests = 0;

beforeAll(async () => {
  served = await start();
  if (typeof served === 'string') return;
  const counting = (url: string | URL | Request, init?: RequestInit) => {
    if (String(url).endsWith('/v1/aggregate')) requests += 1;
    return fetch(url, init);
  };
  client = new TesseraClient({viewerUrl: served.viewerUrl, sessionUrl: served.sessionUrl, sessionCredential: served.operatorCredential, fetch: counting});
  session = await client.authorise({terms: TERMS});
  meta = await client.meta(session.token);
}, 120_000);

afterAll(() => {
  if (typeof served !== 'string') served.stop();
  client?.close();
});

function live(ctx: TestContext): void {
  if (typeof served === 'string') ctx.skip(served);
}

/** The viewport's matched count over the whole of view `s0`. */
async function matched(filters?: FilterExpr): Promise<number> {
  const q = meta.views.find((v) => v.id === 's0')!.quantisation;
  const response = await client.viewport(session.token, {view: 's0', zoom: 0, bbox: [q.xMin, q.yMin, q.xMax, q.yMax], k: 0, ...(filters ? {filters} : {})});
  return Number(response.result.tiles.reduce((n, t) => n + t.matched, 0n));
}

const sum = (table: Table, name = 'count') => [...table.getChild(name)!].reduce((n: bigint, v) => n + (v as bigint), 0n);

/** Each table's rows as one comparable string, a 64-bit value as its decimal digits. */
const dump = (table: Table) => JSON.stringify(table.toArray(), (_, v: unknown) => (typeof v === 'bigint' ? v.toString() : v));

describe('aggregates against a live server', () => {
  it('counts what the viewport counts: the size of the set, a breakdown and a density surface', async (ctx) => {
    live(ctx);
    const whole = await matched(CS);
    const {tables, identityKey} = await client.aggregate(session.token, {
      view: 's0',
      filters: CS,
      groupings: [{}, {by: {field: 'primary_category', top: 3}}, {cells: {depth: 4}}]
    });
    expect(tables.map((t) => t.total)).toEqual([whole, whole, whole]);
    expect(sum(tables[0]!.rows)).toBe(BigInt(whole));
    // One value per item, so the rows of a breakdown sum to the set.
    expect(sum(tables[1]!.rows)).toBe(BigInt(whole));
    expect([...tables[1]!.rows.getChild('group')!].slice(0, 3)).toEqual(['listed', 'listed', 'listed']);
    expect(tables[1]!.groups).toBeGreaterThan(3);
    expect(sum(tables[2]!.rows)).toBe(BigInt(whole));
    expect(identityKey).not.toBe('');
  });

  it('reads in one call over many small responses what one response carries', async (ctx) => {
    live(ctx);
    const request: AggregateRequest = {view: 's0', reference: {}, groupings: [{by: {field: 'archive', top: 2}, cells: {depth: 3}}, {cells: {depth: 5}}]};
    const before = requests;
    const one = await client.aggregate(session.token, request);
    expect(requests - before).toBe(1);
    const several = await client.aggregate(session.token, {...request, pageRows: 7, pages: 2});
    expect(requests - before).toBeGreaterThan(3);
    expect(several.next).toBeNull();
    expect(several.tables.map((t) => [t.grouping, t.total, t.referenceTotal, t.groups])).toEqual(one.tables.map((t) => [t.grouping, t.total, t.referenceTotal, t.groups]));
    expect(several.tables.map((t) => dump(t.rows))).toEqual(one.tables.map((t) => dump(t.rows)));
    // The whole visible set as the reference: every lift is 1.
    expect(one.tables[0]!.referenceTotal).toBe(await matched());
    expect(new Set([...one.tables[0]!.rows.getChild('lift')!])).toEqual(new Set([1]));
  });

  it('keeps the store’s aggregate in step with its filters', async (ctx) => {
    live(ctx);
    const {viewerUrl} = served as Served;
    const store = createStore({viewerUrl, token: session.token, prefetch: false});
    try {
      const landed = (total: number) =>
        new Promise<void>((resolve) => {
          const check = () => {
            const entry = store.get('aggregates').get('size');
            if (entry?.status === 'shown' && entry.result!.tables[0]!.total === total) resolve();
          };
          store.subscribe('aggregates', check);
          check();
        });
      store.setAggregate('size', {groupings: [{}]});
      await landed(await matched());
      store.setFilters({filter: {archive: {family: 'category', keys: ['cs']}}, highlight: {}});
      await landed(await matched(CS));

      // A filter control's own values: its clause set, the alternatives still show.
      const listed = () =>
        new Promise<string[]>((resolve) => {
          const check = () => {
            const entry = store.get('aggregates').get('archives');
            if (entry?.status === 'shown') resolve([...entry.result!.tables[0]!.rows.getChild('key')!].filter((k) => k !== null) as string[]);
          };
          store.subscribe('aggregates', check);
          check();
        });
      store.setAggregate('archives', {groupings: [{by: {field: 'archive', top: 5}}], without: 'archive'});
      expect((await listed()).length).toBeGreaterThan(1);
      // Another clause still narrows them.
      const LG: FilterExpr = {primary_category: {in: ['cs.LG']}};
      store.setFilters({filter: {archive: {family: 'category', keys: ['cs']}, primary_category: {family: 'category', keys: ['cs.LG']}}, highlight: {}});
      await landed(await matched({all_of: [CS, LG]}));
      expect(await listed()).toEqual(['cs']);
      expect(store.get('aggregates').get('archives')!.result!.tables[0]!.total).toBe(await matched(LG));
    } finally {
      store.dispose();
    }
  });
});
