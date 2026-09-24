import type {Table} from 'apache-arrow';
import {afterAll, beforeAll, describe, expect, it, type TestContext} from 'vitest';
import {TesseraClient, TesseraError} from '../src/client.js';
import {Control} from '../src/control.js';
import {refusalOf} from '../src/presented.js';
import type {RecordsRead} from '../src/records.js';
import type {ArtifactsRequest, BrowseRow, ItemsRequest, Meta, Session, ViewportRequest} from '../src/types.js';
import {start, type Served} from './served.js';

/**
 * `TesseraClient.items` and `artifacts` against a real `tessera serve` over the notebook corpus
 * (`served.ts`). Each read is carried across several small pages and several responses, and
 * compared with what the viewport, the item card or browse serves for the same data.
 */

const TERMS = ['cs.LG', 'cs.CV', 'hep-ph'];

let served: Served | string = 'the server has not started';
let client: TesseraClient;
let session: Session;
let meta: Meta;

beforeAll(async () => {
  served = await start();
  if (typeof served === 'string') return;
  client = new TesseraClient({viewerUrl: served.viewerUrl, sessionUrl: served.sessionUrl, sessionCredential: served.sessionCredential});
  session = await client.authorise(TERMS);
  meta = await client.meta(session.token);
}, 120_000);

afterAll(() => {
  if (typeof served !== 'string') served.stop();
  client?.close();
});

function live(ctx: TestContext): void {
  if (typeof served === 'string') ctx.skip(served);
}

/** The viewport's counts over the whole of view `s0`. */
async function viewportCounts(extra: Partial<ViewportRequest> = {}) {
  const q = meta.views.find((v) => v.id === 's0')!.quantisation;
  const response = await client.viewport(session.token, {view: 's0', zoom: 2, bbox: [q.xMin, q.yMin, q.xMax, q.yMax], k: 0, ...extra});
  const sum = (field: 'visible' | 'matched') => Number(response.result.tiles.reduce((n, t) => n + t[field], 0n));
  return {visible: sum('visible'), matched: sum('matched'), identityKey: response.identityKey};
}

/**
 * A whole read: `open` sends one request from `cursor`, and the read follows each response's
 * cursor until it is null. Returns every page, and each response's head and identity key.
 */
async function readAll<Head>(open: (cursor: string | undefined) => Promise<RecordsRead<Head>>) {
  const tables: Table[] = [];
  const heads: Head[] = [];
  const identityKeys: string[] = [];
  let cursor: string | undefined;
  for (;;) {
    const read = await open(cursor);
    heads.push(read.head);
    identityKeys.push(read.identityKey);
    for await (const table of read) tables.push(table);
    expect(read.trailer).not.toBeNull();
    if (read.cursor === null) break;
    cursor = read.cursor;
  }
  return {tables, heads, identityKeys};
}

const items = (request: ItemsRequest) =>
  readAll((cursor) => client.items(session.token, {...request, cursor, ...(cursor === undefined ? {} : {count: undefined})}));

const artifacts = (request: ArtifactsRequest) =>
  readAll((cursor) => client.artifacts(session.token, {...request, cursor, ...(cursor === undefined ? {} : {count: undefined})}));

/** Every value of column `name` across `tables`, in order. */
const column = (tables: Table[], name: string): unknown[] => tables.flatMap((t) => [...t.getChild(name)!]);

/** Each table's rows as one comparable string, a 64-bit value as its decimal digits. */
const dump = (tables: Table[]) => tables.map((t) => JSON.stringify(t.toArray(), (_, v: unknown) => (typeof v === 'bigint' ? v.toString() : v)));

describe('bulk reads against a live server', () => {
  it('reads every item of a view once, across pages and responses, as the viewport counts and the item card holds them', async (ctx) => {
    live(ctx);
    const {visible, identityKey} = await viewportCounts();
    const {tables, heads, identityKeys} = await items({view: 's0', fields: ['archive', 'title'], systemFields: ['position'], count: true, pageRows: 400, pages: 2});
    expect(heads.length).toBeGreaterThan(2);
    expect(heads[0]!.visible).toBe(visible);
    // Every response names the principal and view as the viewport does.
    expect(new Set(identityKeys)).toEqual(new Set([identityKey]));
    expect(identityKey).not.toBe('');
    for (const t of tables) expect(t.numRows).toBeLessThanOrEqual(400);
    const ids = column(tables, 'tessera_id') as bigint[];
    expect(ids.length).toBe(visible);
    expect(new Set(ids).size).toBe(ids.length);

    // A sample of the rows against the item card: a rendered category, a record field and the position.
    const q = meta.views.find((v) => v.id === 's0')!.quantisation;
    const archive = column(tables, 'archive');
    const title = column(tables, 'title');
    const x = column(tables, 'tessera:x') as number[];
    const y = column(tables, 'tessera:y') as number[];
    for (let i = 0; i < ids.length; i += Math.floor(ids.length / 7)) {
      const card = await client.item(session.token, ids[i]!);
      expect(archive[i]).toBe(card.fields.archive);
      expect(title[i]).toBe(card.fields.title);
      const at = card.views.find((v) => v.id === 's0')!;
      expect(Math.abs(x[i]! - (q.xMin + (at.x / 2 ** 32) * (q.xMax - q.xMin)))).toBeLessThan((q.xMax - q.xMin) * 1e-6);
      expect(Math.abs(y[i]! - (q.yMin + (at.y / 2 ** 32) * (q.yMax - q.yMin)))).toBeLessThan((q.yMax - q.yMin) * 1e-6);
    }
  });

  it('reads the items a filter matches, as many as the viewport matches, and marks them under keepUnmatched', async (ctx) => {
    live(ctx);
    const filters = {archive: {in: ['cs']}};
    const {visible, matched} = await viewportCounts({filters});
    expect(matched).toBeGreaterThan(0);
    expect(matched).toBeLessThan(visible);
    const only = await items({view: 's0', fields: ['archive'], filters, count: true, pageRows: 300, pages: 3});
    expect(only.heads[0]).toMatchObject({visible, matched});
    expect(column(only.tables, 'archive')).toEqual(Array(matched).fill('cs'));

    const kept = await items({view: 's0', fields: [], filters, keepUnmatched: true, pageRows: 500});
    const marks = column(kept.tables, 'tessera:matched');
    expect(marks.length).toBe(visible);
    expect(marks.filter((m) => m === true).length).toBe(matched);
  });

  it('returns the same rows in either order', async (ctx) => {
    live(ctx);
    const ids = async (order: 'map' | 'stored') => {
      const {tables, heads} = await items({view: 's0', fields: [], order, pageRows: 600, pages: 2});
      expect(heads.every((h) => h.order === order)).toBe(true);
      return (column(tables, 'tessera_id') as bigint[]).sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
    };
    expect(await ids('stored')).toEqual(await ids('map'));
  });

  it('decodes zstd-compressed pages to the tables sent uncompressed', async (ctx) => {
    live(ctx);
    const request: ItemsRequest = {view: 's0', fields: ['archive', 'primary_category', 'submitted_at', 'title', 'arxiv_id'], systemFields: ['position', 'external_id', 'labels'], pageRows: 700, pages: 2};
    const plain = await items(request);
    const zstd = await items({...request, compression: 'zstd'});
    expect(plain.tables.length).toBeGreaterThan(2);
    expect(dump(zstd.tables)).toEqual(dump(plain.tables));

    const layer: ArtifactsRequest = {view: 's0', layer: 'clusters/kmeans', fields: ['key', 'masked_count', 'centroid', 'box', 'shape'], pageRows: 4};
    const layerPlain = await artifacts(layer);
    expect(dump((await artifacts({...layer, compression: 'zstd'})).tables)).toEqual(dump(layerPlain.tables));
  });

  it('reads every artifact of a layer once, across pages and responses, as browse serves them', async (ctx) => {
    live(ctx);
    const {tables, heads} = await artifacts({view: 's0', layer: 'taxonomy/arxiv', fields: ['key', 'level', 'masked_count', 'parents'], count: true, pageRows: 5, pages: 2});
    expect(heads.length).toBeGreaterThan(2);

    // Browse serves the layer by lineage: its roots, then each artifact's children.
    const browsed = new Map<bigint, BrowseRow>();
    const page = async (req: {level?: number; parent?: bigint}) => {
      const rows: BrowseRow[] = [];
      let cursor: string | undefined;
      do {
        const got = await client.browse(session.token, {view: 's0', layer: 'taxonomy/arxiv', ...req, cursor});
        rows.push(...got.artifacts);
        cursor = got.next ?? undefined;
      } while (cursor !== undefined);
      return rows;
    };
    const queue = await page({});
    while (queue.length > 0) {
      const row = queue.pop()!;
      if (browsed.has(row.tesseraId)) continue;
      browsed.set(row.tesseraId, row);
      queue.push(...(await page({parent: row.tesseraId})));
    }

    const ids = column(tables, 'tessera_id') as bigint[];
    expect(heads[0]!.served).toBe(ids.length);
    expect(new Set(ids).size).toBe(ids.length);
    expect(new Set(ids)).toEqual(new Set(browsed.keys()));
    const key = column(tables, 'key');
    const level = column(tables, 'level');
    const masked = column(tables, 'masked_count');
    const sorted = (ids: bigint[]) => [...ids].sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
    const parents = column(tables, 'parents').map((list) => sorted([...(list as Iterable<bigint>)]));
    for (const [i, id] of ids.entries()) {
      const row = browsed.get(id)!;
      expect({key: key[i], level: level[i], masked: masked[i], parents: parents[i]}).toEqual({
        key: row.key,
        level: row.rung,
        masked: row.maskedCount,
        parents: sorted(row.parentIds)
      });
    }
  });

  it('refuses a bad request with a TesseraError before any page', async (ctx) => {
    live(ctx);
    const first = await client.items(session.token, {view: 's0', fields: [], pageRows: 10, pages: 1});
    for await (const _ of first) void _;
    const cursor = first.cursor!;
    const refusals: (() => Promise<unknown>)[] = [
      () => client.items(session.token, {view: 's0', fields: ['no_such_field']}),
      () => client.items(session.token, {view: 's0', fields: [], cursor: 'not-a-cursor'}),
      () => client.artifacts(session.token, {view: 's0', layer: 'taxonomy/arxiv', fields: ['no_such_property' as 'key']}),
      // A cursor is bound to its route, and `count` goes only on a read's first request.
      () => client.artifacts(session.token, {view: 's0', layer: 'taxonomy/arxiv', fields: [], cursor}),
      () => client.items(session.token, {view: 's0', fields: [], cursor, count: true})
    ];
    for (const refusal of refusals) {
      const thrown = await refusal().then(
        () => null,
        (error: unknown) => error
      );
      expect(thrown).toBeInstanceOf(TesseraError);
      expect(thrown).toMatchObject({status: 422});
      expect(refusalOf(thrown).code).toBe('contract');
    }
  });

  it('stops reads whose signals abort, and the server frees their slots', async (ctx) => {
    live(ctx);
    // Two at once, which is every slot of the deployment's default bulk-read lane, each far larger
    // than the socket buffers, so a read the client does not close holds its slot.
    const aborts = [new AbortController(), new AbortController()];
    const request: ItemsRequest = {view: 's0', fields: ['title', 'abstract'], pageRows: 50};
    const reads = await Promise.all(aborts.map((abort) => client.items(session.token, request, abort.signal)));
    for (const [i, read] of reads.entries()) {
      expect((await read.next()).done).toBe(false);
      aborts[i]!.abort();
      const thrown = await read.next().catch((error: unknown) => error);
      expect(thrown).toMatchObject({name: 'AbortError'});
      expect(await read.next()).toEqual({done: true, value: undefined});
      expect(read.trailer).toBeNull();
    }

    // The server frees a slot once it sees the connection close, which takes a moment. The limit is
    // well inside the ten seconds after which the server sheds a response nobody reads.
    const {controlUrl, operatorCredential} = served as Served;
    const control = new Control({controlUrl, operatorCredential});
    const deadline = Date.now() + 5_000;
    while (((await control.status()).body.bulk as {in_flight: number}).in_flight > 0) {
      expect(Date.now(), 'the aborted reads still hold their slots').toBeLessThan(deadline);
    }
    const {visible} = await viewportCounts();
    const again = await items({view: 's0', fields: [], pageRows: 5000});
    expect(column(again.tables, 'tessera_id').length).toBe(visible);
  });
});
