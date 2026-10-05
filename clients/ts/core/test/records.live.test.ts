import {createServer} from 'node:http';
import type {AddressInfo} from 'node:net';
import type {Table} from 'apache-arrow';
import {afterAll, beforeAll, describe, expect, it, type TestContext} from 'vitest';
import {TesseraClient, TesseraError} from '../src/client.js';
import {Control} from '../src/control.js';
import {artifactName} from '../src/names.js';
import {refusalOf} from '../src/presented.js';
import {createStore} from '../src/store.js';
import type {RecordsRead} from '../src/records.js';
import type {ArtifactsRequest, BrowseRow, ItemsRequest, Meta, Session, ViewportRequest} from '../src/types.js';
import {rejectsAsRefused} from './support.js';
import {start, type Served} from './served.js';

/**
 * `TesseraClient.items` and `artifacts` against a real `tessera serve` over the notebook corpus
 * (`served.ts`). Each read is carried across several small pages and several responses, and
 * compared with what the viewport, the item card or browse serves for the same data.
 */

const TERMS = ['cs.LG', 'cs.CV', 'hep-ph'];

let served: Served | string = 'the server has not started';
let client: TesseraClient;
/** The bulk-read requests `client` has sent, one per response. */
let requests = 0;
let session: Session;
let meta: Meta;

beforeAll(async () => {
  served = await start();
  if (typeof served === 'string') return;
  const counting = (url: string | URL | Request, init?: RequestInit) => {
    if (/\/v1\/(items|artifacts)$/.test(String(url))) requests += 1;
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

/** The viewport's counts over the whole of view `s0`. */
async function viewportCounts(extra: Partial<ViewportRequest> = {}) {
  const q = meta.views.find((v) => v.id === 's0')!.quantisation;
  const response = await client.viewport(session.token, {view: 's0', zoom: 2, bbox: [q.xMin, q.yMin, q.xMax, q.yMax], k: 0, ...extra});
  const sum = (field: 'visible' | 'matched') => Number(response.result.tiles.reduce((n, t) => n + t[field], 0n));
  return {visible: sum('visible'), matched: sum('matched'), identityKey: response.identityKey};
}

/** Every page of the read `opening` starts, which must end, and how many responses it took. */
async function readAll<Head>(opening: () => Promise<RecordsRead<Head>>) {
  const before = requests;
  const read = await opening();
  const tables: Table[] = [];
  for await (const table of read) tables.push(table);
  expect(read.trailer).not.toBeNull();
  expect(read.cursor).toBeNull();
  return {tables, read, responses: requests - before};
}

const items = (request: ItemsRequest) => readAll(() => client.items(session.token, request));
const artifacts = (request: ArtifactsRequest) => readAll(() => client.artifacts(session.token, request));

/** The server's bulk reads in flight reach none within five seconds, well inside its ten-second stall shed. */
async function lanesFree(): Promise<void> {
  const {controlUrl, operatorCredential} = served as Served;
  const control = new Control({controlUrl, credential: operatorCredential});
  const deadline = Date.now() + 5_000;
  while (((await control.status()).body.bulk as {in_flight: number}).in_flight > 0) {
    expect(Date.now(), 'bulk reads still hold their slots').toBeLessThan(deadline);
  }
}

/**
 * A proxy in front of the viewer plane that forwards responses whole, except the `nth` (from 1),
 * which it forwards as a chunked body and cuts at the byte `cutAt` chooses, as the server does to
 * a body that fails part-way.
 */
async function cuttingProxy(target: string, nth: number, cutAt: (body: Uint8Array) => number) {
  let seen = 0;
  const server = createServer(async (req, res) => {
    const sent: Buffer[] = [];
    for await (const chunk of req) sent.push(chunk as Buffer);
    const upstream = await fetch(`${target}${req.url}`, {
      method: req.method,
      headers: {authorization: req.headers.authorization!, 'content-type': 'application/json'},
      body: Buffer.concat(sent)
    });
    const body = new Uint8Array(await upstream.arrayBuffer());
    const headers: Record<string, string> = {};
    upstream.headers.forEach((value, name) => {
      if (name.startsWith('x-tessera-') || name === 'content-type') headers[name] = value;
    });
    res.writeHead(upstream.status, headers);
    seen += 1;
    if (seen !== nth) return void res.end(body);
    // No length, so Node sends the body chunked, and the connection closes before its last chunk.
    res.write(body.subarray(0, cutAt(body)), () => res.socket?.destroy());
  });
  await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve));
  return {
    url: `http://127.0.0.1:${(server.address() as AddressInfo).port}`,
    close: () => {
      server.closeAllConnections();
      server.close();
    }
  };
}

/** Where each frame of a body starts, and its kind. */
function frameStarts(body: Uint8Array): {at: number; kind: number; length: number}[] {
  const view = new DataView(body.buffer, body.byteOffset, body.byteLength);
  const out: {at: number; kind: number; length: number}[] = [];
  for (let at = 0; at < body.byteLength; at += 5 + view.getUint32(at + 1, true)) {
    out.push({at, kind: body[at]!, length: view.getUint32(at + 1, true)});
  }
  return out;
}

/** Every value of column `name` across `tables`, in order. */
const column = (tables: Table[], name: string): unknown[] => tables.flatMap((t) => [...t.getChild(name)!]);

/** Each column's name and Arrow type. */
const typesOf = (table: Table) => table.schema.fields.map((f) => `${f.name}: ${String(f.type)}`);

/** The first page of a read, and the read stopped after it. */
async function firstPage<Head>(opening: Promise<RecordsRead<Head>>): Promise<Table> {
  const read = await opening;
  const {value} = await read.next();
  await read.return();
  return value as Table;
}

/** Each table's rows as one comparable string, a 64-bit value as its decimal digits. */
const dump = (tables: Table[]) => tables.map((t) => JSON.stringify(t.toArray(), (_, v: unknown) => (typeof v === 'bigint' ? v.toString() : v)));

describe('bulk reads against a live server', () => {
  it('reads every item of a view once, across pages and responses, as the viewport counts and the item card holds them', async (ctx) => {
    live(ctx);
    const {visible, identityKey} = await viewportCounts();
    const {tables, read, responses} = await items({view: 's0', fields: ['archive', 'title'], systemFields: ['position'], count: true, pageRows: 400, pages: 2});
    expect(responses).toBeGreaterThan(2);
    expect(read.head.visible).toBe(visible);
    // The response names the principal and view as the viewport does.
    expect(read.identityKey).toBe(identityKey);
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

  it('reads in one call across several responses what one response carries, and resumes where a caller stopped', async (ctx) => {
    live(ctx);
    const request: ItemsRequest = {view: 's0', fields: ['archive', 'title', 'submitted_at'], systemFields: ['position', 'labels'], pageRows: 300};
    const one = await items(request);
    expect(one.responses).toBe(1);
    const several = await items({...request, pages: 2});
    expect(several.responses).toBeGreaterThan(2);
    expect(dump(several.tables)).toEqual(dump(one.tables));

    // Stopped after five pages, the read goes on from its cursor in a new call.
    const stopped = await client.items(session.token, {...request, pages: 2});
    const first: Table[] = [];
    for await (const table of stopped) {
      first.push(table);
      if (first.length === 5) break;
    }
    const rest = await items({...request, pages: 2, cursor: stopped.cursor!});
    expect(dump([...first, ...rest.tables])).toEqual(dump(one.tables));
  });

  it('throws a refused follow-up’s TesseraError, and resumes from its cursor', async (ctx) => {
    live(ctx);
    const request: ItemsRequest = {view: 's0', fields: [], pageRows: 500, pages: 1};
    const whole = column((await items(request)).tables, 'tessera_id');
    const read = await client.items(session.token, request);
    const first = await read.next();
    expect(first.done).toBe(false);
    await lanesFree();
    // Two reads the client does not consume hold both slots of the lane, so the follow-up is shed.
    const holders = await Promise.all([0, 1].map(() => client.items(session.token, {view: 's0', fields: ['title', 'abstract'], pageRows: 50})));
    const thrown = await read.next().catch((error: unknown) => error);
    expect(thrown).toBeInstanceOf(TesseraError);
    expect(thrown).toMatchObject({status: 429});
    expect(typeof read.cursor).toBe('string');
    for (const holder of holders) await holder.return();
    await lanesFree();
    const rest = await items({...request, cursor: read.cursor!});
    expect([...column([first.value as Table], 'tessera_id'), ...column(rest.tables, 'tessera_id')]).toEqual(whole);
  });

  it('gives a read that finds nothing one page of no rows, with the columns and types of a page with rows', async (ctx) => {
    live(ctx);
    const request: ItemsRequest = {view: 's0', fields: ['archive', 'title', 'submitted_at'], systemFields: ['position', 'labels'], pageRows: 100};
    const some = await firstPage(client.items(session.token, request));
    expect(some.numRows).toBe(100);
    const none = await items({...request, filters: {arxiv_id: {eq: 'no such paper'}}, count: true});
    expect(none.tables.map((t) => t.numRows)).toEqual([0]);
    expect(typesOf(none.tables[0]!)).toEqual(typesOf(some));
    expect(none.read.head).toMatchObject({matched: 0});
    expect(none.read.pageEnd).toEqual({next: null, endedBy: 'end'});
    expect(none.read.trailer).toMatchObject({pages: 1, rows: 0, next: null});

    // The children of an artifact that has none.
    const layer: ArtifactsRequest = {view: 's0', layer: 'taxonomy/arxiv', fields: ['key', 'level', 'masked_count', 'parents'], level: 1, pageRows: 5};
    const leaves = await firstPage(client.artifacts(session.token, layer));
    const leaf = leaves.getChild('tessera_id')!.get(0) as bigint;
    const children = await artifacts({...layer, level: undefined, parent: leaf});
    expect(children.tables.map((t) => t.numRows)).toEqual([0]);
    expect(typesOf(children.tables[0]!)).toEqual(typesOf(leaves));
  });

  it('carries a page of no rows from a response part-way through a read that finds nothing', async (ctx) => {
    live(ctx);
    const filters = {archive: {in: ['cs']}};
    const {matched} = await viewportCounts({filters});
    // One page that holds every match, so the response after it scans the rest and finds none.
    const {tables, responses, read} = await items({view: 's0', fields: ['archive'], filters, pageRows: matched, pages: 1});
    expect(tables.map((t) => t.numRows)).toEqual([matched, 0]);
    expect(responses).toBe(2);
    expect(typesOf(tables[1]!)).toEqual(typesOf(tables[0]!));
    expect(read.pageEnd).toEqual({next: null, endedBy: 'end'});
  });

  it('reads the items a filter matches, as many as the viewport matches, and marks them under keepUnmatched', async (ctx) => {
    live(ctx);
    const filters = {archive: {in: ['cs']}};
    const {visible, matched} = await viewportCounts({filters});
    expect(matched).toBeGreaterThan(0);
    expect(matched).toBeLessThan(visible);
    const only = await items({view: 's0', fields: ['archive'], filters, count: true, pageRows: 300, pages: 3});
    expect(only.responses).toBeGreaterThan(1);
    expect(only.read.head).toMatchObject({visible, matched});
    expect(column(only.tables, 'archive')).toEqual(Array(matched).fill('cs'));

    const kept = await items({view: 's0', fields: [], filters, keepUnmatched: true, pageRows: 500});
    const marks = column(kept.tables, 'tessera:matched');
    expect(marks.length).toBe(visible);
    expect(marks.filter((m) => m === true).length).toBe(matched);
  });

  it('returns the same rows in either order', async (ctx) => {
    live(ctx);
    const ids = async (order: 'map' | 'stored') => {
      const {tables, read} = await items({view: 's0', fields: [], order, pageRows: 600, pages: 2});
      expect(read.head.order).toBe(order);
      return (column(tables, 'tessera_id') as bigint[]).sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
    };
    expect(await ids('stored')).toEqual(await ids('map'));
  });

  it('decodes zstd-compressed pages to the tables sent uncompressed', async (ctx) => {
    live(ctx);
    const request: ItemsRequest = {view: 's0', fields: ['archive', 'primary_category', 'submitted_at', 'title', 'arxiv_id'], systemFields: ['position', 'labels'], pageRows: 700, pages: 2};
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
    const {tables, read, responses} = await artifacts({view: 's0', layer: 'taxonomy/arxiv', fields: ['key', 'level', 'masked_count', 'parents'], count: true, pageRows: 5, pages: 2});
    expect(responses).toBeGreaterThan(2);

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
    expect(read.head.served).toBe(ids.length);
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

  it('colours by a clustering with its labels naming the rows, and asks the point path for no label column', async (ctx) => {
    live(ctx);
    // A label's text is read only by a viewer who can see every paper it was drawn from. A paper
    // carries its primary category among its terms, so a viewer holding every primary category
    // sees every paper.
    const primary = (await client.categories(session.token, 'primary_category', {limit: 1000})).map((v) => v.key);
    const everything = (await client.authorise({terms: primary})).token;
    const bodies: {k?: number; layers?: string[]}[] = [];
    const tiles: {layers?: string[]; per_tile?: number}[] = [];
    const watching = new TesseraClient({
      viewerUrl: (served as Served).viewerUrl,
      sessionUrl: (served as Served).sessionUrl,
      fetch: (url, init) => {
        if (String(url).endsWith('/v1/viewport') && init?.body) bodies.push(JSON.parse(String(init.body)) as {k?: number; layers?: string[]});
        if (String(url).endsWith('/v1/artifacts/viewport') && init?.body) tiles.push(JSON.parse(String(init.body)) as {layers?: string[]; per_tile?: number});
        return fetch(url, init);
      }
    });
    const store = createStore({viewerUrl: (served as Served).viewerUrl, token: everything, client: watching, prefetch: false, artifacts: {perTile: meta.selection.maxArtifactsPerTile}});
    try {
      store.setColourBy('cluster:clusters/hdbscan');
      const q = meta.views.find((v) => v.id === 's0')!.quantisation;
      store.setView({bbox: [q.xMin, q.yMin, q.xMax, q.yMax], width: 800, height: 800});
      const deadline = Date.now() + 20_000;
      while (Date.now() < deadline && (store.get('artifacts').colourServed.length === 0 || store.get('marks').bands.length === 0)) await new Promise((r) => setTimeout(r, 50));
      const a = store.get('artifacts');
      expect(a.colourServed.length).toBeGreaterThan(0);
      const names = a.colourServed.map((x) => artifactName(x, a.attached));
      expect(names.filter((n) => n !== null).length).toBeGreaterThan(0);

      // The same labels a bulk read of the label layer serves this viewer.
      const {tables} = await readAll(() => client.artifacts(everything, {view: 's0', layer: 'topics/hdbscan', fields: ['target', 'content']}));
      const targets = column(tables, 'target') as bigint[];
      const texts = column(tables, 'content').map((list) => [...(list as Iterable<string>)][0]);
      for (const x of a.colourServed) {
        const i = targets.indexOf(x.tesseraId);
        expect(artifactName(x, a.attached)).toBe(i === -1 ? null : texts[i]);
      }

      // The points carry the coloured layer's column and not its labels'; the channel asks for both,
      // with the quota the store was given, the largest the deployment takes, so no label of a
      // served cluster is past it.
      const points = bodies.filter((b) => b.k !== 0);
      expect(points.length).toBeGreaterThan(0);
      for (const b of points) expect(b.layers).toEqual(['clusters/hdbscan']);
      expect(tiles.some((b) => b.layers?.includes('clusters/hdbscan') && b.layers.includes('topics/hdbscan') && b.per_tile === meta.selection.maxArtifactsPerTile)).toBe(true);
      for (const band of store.get('marks').bands) expect(Object.keys(band.membership)).toEqual(['clusters/hdbscan']);
    } finally {
      store.dispose();
      watching.close();
    }
  });

  it('refuses a bad request with a TesseraError before any page', async (ctx) => {
    live(ctx);
    const first = await client.items(session.token, {view: 's0', fields: [], pageRows: 10});
    for await (const _ of first) break;
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

  it('treats a connection cut before a trailer as an incomplete body, in the first response or a later one, and resumes from the last page end', async (ctx) => {
    live(ctx);
    const request: ItemsRequest = {view: 's0', fields: ['title'], pageRows: 200, pages: 5};
    const whole = column((await items(request)).tables, 'tessera_id');
    // After a response's second page end, and halfway through its third records frame.
    const cuts: Record<string, (body: Uint8Array) => number> = {
      'on a frame boundary': (body) => {
        const ends = frameStarts(body).filter((f) => f.kind === 8);
        return ends[1]!.at + 5 + ends[1]!.length;
      },
      'inside a frame': (body) => {
        const records = frameStarts(body).filter((f) => f.kind === 7);
        return records[2]!.at + 5 + Math.floor(records[2]!.length / 2);
      }
    };
    for (const nth of [1, 2]) {
      for (const [name, cutAt] of Object.entries(cuts)) {
        const label = `response ${nth}, ${name}`;
        const proxy = await cuttingProxy((served as Served).viewerUrl, nth, cutAt);
        try {
          const cutClient = new TesseraClient({viewerUrl: proxy.url, sessionUrl: (served as Served).sessionUrl});
          const read = await cutClient.items(session.token, request);
          const got: Table[] = [];
          await rejectsAsRefused(
            (async () => {
              for await (const table of read) got.push(table);
            })()
          );
          // The whole responses before the cut, then the cut one's two whole pages.
          expect(got.map((t) => t.numRows), label).toEqual(Array(5 * (nth - 1) + 2).fill(200));
          expect(read.trailer).toBeNull();
          expect(read.cursor, label).toBe(read.pageEnd!.next);
          const rest = await items({...request, cursor: read.cursor!});
          expect([...column(got, 'tessera_id'), ...column(rest.tables, 'tessera_id')], label).toEqual(whole);
        } finally {
          proxy.close();
        }
      }
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

    // The server frees a slot once it sees the connection close, which takes a moment.
    await lanesFree();
    const {visible} = await viewportCounts();
    const again = await items({view: 's0', fields: [], pageRows: 5000});
    expect(column(again.tables, 'tessera_id').length).toBe(visible);
  });
});
