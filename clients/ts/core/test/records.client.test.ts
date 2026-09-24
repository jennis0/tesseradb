import {zstdCompressSync, zstdDecompressSync} from 'node:zlib';
import {
  compressionRegistry,
  CompressionType,
  Dictionary,
  Float64,
  Int32,
  RecordBatchStreamWriter,
  Table,
  tableFromIPC,
  tableToIPC,
  Utf8,
  vectorFromArray
} from 'apache-arrow';
import {afterEach, describe, expect, it} from 'vitest';
import {TesseraClient, TesseraError} from '../src/client.js';
import type {Frame} from '../src/frame.js';
import {refusalOf} from '../src/presented.js';
import type {RecordsRead} from '../src/records.js';
import type {ArtifactsHead, ItemsHead, ItemsRequest} from '../src/types.js';
import {chunked, framed, manual, rejectsAsRefused, u64} from './support.js';

/**
 * `TesseraClient.items` and `artifacts` over bodies built here from the wire layout: a head
 * (kind 6), records frames (7) each followed by a page end (8), and a trailer (4).
 */

const HEAD = 6;
const RECORDS = 7;
const PAGE_END = 8;
const TRAILER = 4;

const json = (kind: number, value: unknown): Frame => ({kind, payload: new TextEncoder().encode(JSON.stringify(value))});
const trailer = (pages: number, rows: number, next: string | null = null) =>
  json(TRAILER, {pages, rows, next, ended_by: next === null ? 'end' : 'pages', stream_us: 12});

/** A page of `ids` with a category, a string with a null, and a number. */
function page(ids: bigint[]): Table {
  return new Table({
    tessera_id: u64(ids),
    archive: vectorFromArray(
      ids.map((id) => ['cs', 'hep', 'math'][Number(id % 3n)]!),
      new Dictionary(new Utf8(), new Int32())
    ),
    title: vectorFromArray(
      ids.map((id) => (id % 4n === 0n ? null : `paper ${id}`)),
      new Utf8()
    ),
    score: vectorFromArray(
      ids.map((id) => Number(id) / 8),
      new Float64()
    )
  });
}

const PAGES = [page([1n, 2n, 3n]), page([4n, 5n, 6n]), page([7n, 8n])];
const CURSORS = ['c1', 'c2', null];

/** Every frame of a three-page read that ends the result. */
function parts(ipc: (table: Table) => Uint8Array = (t) => tableToIPC(t, 'stream')): Frame[] {
  return [
    json(HEAD, {order: 'map', page_rows: 3, visible: 8, matched: 8}),
    ...PAGES.flatMap((table, i): Frame[] => [
      {kind: RECORDS, payload: ipc(table)},
      json(PAGE_END, {next: CURSORS[i], ended_by: i < 2 ? 'rows' : 'end'})
    ]),
    trailer(3, 8)
  ];
}

/** `table`'s rows as plain objects, a category as its key. */
const rowsOf = (table: Table) => table.toArray().map((row) => ({...row.toJSON()}));

/** A client whose `fetch` answers `response` and records the request it was sent. */
function clientFor(response: Response | (() => Response)) {
  const sent: {url: string; body: unknown; signal: AbortSignal | null | undefined}[] = [];
  const client = new TesseraClient({
    viewerUrl: 'http://viewer',
    sessionUrl: 'http://session',
    fetch: (async (url: string, init?: RequestInit) => {
      sent.push({url, body: JSON.parse(init!.body as string), signal: init?.signal});
      return typeof response === 'function' ? response() : response;
    }) as typeof fetch
  });
  return {client, sent};
}

/** Every page of `read`, with the page end the read reported after each. */
async function drain<Head>(read: RecordsRead<Head>) {
  const pages: {rows: ReturnType<typeof rowsOf>; next: string | null | undefined; endedBy: string | undefined}[] = [];
  for await (const table of read) pages.push({rows: rowsOf(table), next: read.pageEnd?.next, endedBy: read.pageEnd?.endedBy});
  return pages;
}

const REQUEST: ItemsRequest = {view: 's0', fields: ['archive', 'title', 'score']};
const ZSTD: ItemsRequest = {...REQUEST, compression: 'zstd'};

const zstd = (data: Uint8Array) => new Uint8Array(zstdCompressSync(data));

/** `table` as an Arrow stream whose buffers are zstd-compressed, as the server writes them. */
function compressed(table: Table): Uint8Array {
  const before = compressionRegistry.get(CompressionType.ZSTD);
  // An encoder only: decoding is left to the client under test.
  compressionRegistry.set(CompressionType.ZSTD, {encode: zstd});
  try {
    return RecordBatchStreamWriter.writeAll(table, {compressionType: CompressionType.ZSTD}).toUint8Array(true);
  } finally {
    compressionRegistry.set(CompressionType.ZSTD, before ?? {});
  }
}

/** A host's codec, its methods on the class, as a host library would write one. */
class HostEncoder {
  encode(data: Uint8Array): Uint8Array {
    return zstd(data);
  }
}

class HostCodec extends HostEncoder {
  decodes = 0;
  decode(data: Uint8Array): Uint8Array {
    this.decodes += 1;
    return new Uint8Array(zstdDecompressSync(data));
  }
}

// Each test starts with no zstd codec registered, as a host that registered none.
afterEach(() => compressionRegistry.set(CompressionType.ZSTD, {}));

describe('TesseraClient.items and artifacts', () => {
  it('sends the request as given, each field that is set under its wire name', async () => {
    const empty = framed([json(HEAD, {order: 'stored', page_rows: 10}), trailer(0, 0)]);
    const {client, sent} = clientFor(() => chunked(empty));
    const items: ItemsRequest = {
      view: 's0',
      fields: ['title'],
      systemFields: ['position', 'labels'],
      filters: {archive: {in: ['cs']}},
      keepUnmatched: true,
      count: true,
      order: 'stored',
      pageRows: 10,
      pages: 2,
      compression: 'zstd',
      idset: 4
    };
    await drain(await client.items('tok', items));
    await drain(await client.items('tok', {view: 's0', fields: [], cursor: 'c9'}));
    await drain(await client.artifacts('tok', {view: 's0', layer: 'clusters', fields: ['key', 'parents'], parent: 2n ** 63n + 5n, level: 1, q: 'ab'}));
    expect(sent.map((s) => s.url)).toEqual(['http://viewer/v1/items', 'http://viewer/v1/items', 'http://viewer/v1/artifacts']);
    expect(sent[0]!.body).toEqual({
      view: 's0',
      fields: ['title'],
      system_fields: ['position', 'labels'],
      filters: {archive: {in: ['cs']}},
      keep_unmatched: true,
      count: true,
      order: 'stored',
      page_rows: 10,
      pages: 2,
      compression: 'zstd',
      idset: 4
    });
    expect(sent[1]!.body).toEqual({view: 's0', fields: [], cursor: 'c9'});
    expect(sent[2]!.body).toEqual({view: 's0', layer: 'clusters', fields: ['key', 'parents'], parent: '9223372036854775813', level: 1, q: 'ab'});
  });

  it('yields each page as a table with the cursor after it, however the body is split', async () => {
    const body = framed(parts());
    const headers = {'x-tessera-region': 'exact', 'x-tessera-identity-key': 'ik', 'x-tessera-server-us': '41', 'x-tessera-admission-us': '3'};
    for (const size of [body.byteLength, 64, 3, 1]) {
      const read = await clientFor(chunked(body, size, {headers})).client.items('tok', REQUEST);
      expect(read.head).toEqual<ItemsHead>({order: 'map', pageRows: 3, visible: 8, matched: 8});
      expect(read.region).toEqual({exact: true, depth: null});
      expect(read.identityKey).toBe('ik');
      expect(read.timings).toEqual({serverUs: 41, admissionUs: 3});
      expect(read.cursor).toBeUndefined();
      const pages = await drain(read);
      expect(pages).toEqual(
        PAGES.map((table, i) => ({rows: rowsOf(table), next: CURSORS[i], endedBy: i < 2 ? 'rows' : 'end'}))
      );
      // A category arrives as its keys, and an absent value as null.
      expect(pages[0]!.rows[0]).toEqual({tessera_id: 1n, archive: 'hep', title: 'paper 1', score: 0.125});
      expect(pages[0]!.rows.map((r) => r.archive)).toEqual(['hep', 'math', 'cs']);
      expect(pages[1]!.rows[0]!.title).toBeNull();
      expect(read.trailer).toEqual({pages: 3, rows: 8, next: null, endedBy: 'end', streamUs: 12});
      expect(read.cursor).toBeNull();
    }
  });

  it('gives the page end’s cursor while the body is read and the trailer’s once it is whole', async () => {
    const body = framed([
      json(HEAD, {order: 'map', page_rows: 3}),
      {kind: RECORDS, payload: tableToIPC(PAGES[0]!, 'stream')},
      json(PAGE_END, {next: 'c2', ended_by: 'rows'}),
      trailer(1, 3, 'c3')
    ]);
    const read = await clientFor(chunked(body)).client.items('tok', REQUEST);
    const seen: (string | null | undefined)[] = [];
    for await (const _ of read) seen.push(read.cursor);
    expect(seen).toEqual(['c2']);
    expect(read.cursor).toBe('c3');
  });

  it('decodes pages whose buffers are zstd-compressed to the tables sent uncompressed', async () => {
    const plain = await drain(await clientFor(chunked(framed(parts()))).client.items('tok', REQUEST));
    const squeezed = framed(parts(compressed));
    expect(squeezed.byteLength).not.toBe(framed(parts()).byteLength);
    for (const size of [squeezed.byteLength, 5]) {
      const read = await clientFor(chunked(squeezed, size)).client.items('tok', ZSTD);
      expect(await drain(read)).toEqual(plain);
    }
  });

  it('leaves the shared codec registry alone on an uncompressed read, and keeps a host’s codec on a compressed one', async () => {
    const encoder = new HostEncoder();
    compressionRegistry.set(CompressionType.ZSTD, encoder);
    await drain(await clientFor(chunked(framed(parts()))).client.items('tok', REQUEST));
    expect(compressionRegistry.get(CompressionType.ZSTD)).toBe(encoder);

    // A host that registered only an encoder can still write compressed Arrow after a compressed read.
    const squeezed = framed(parts(compressed));
    compressionRegistry.set(CompressionType.ZSTD, encoder);
    await drain(await clientFor(chunked(squeezed)).client.items('tok', ZSTD));
    const written = RecordBatchStreamWriter.writeAll(PAGES[0]!, {compressionType: CompressionType.ZSTD}).toUint8Array(true);
    expect(rowsOf(tableFromIPC(written))).toEqual(rowsOf(PAGES[0]!));

    // A host's own decoder is kept, and decodes the pages.
    const codec = new HostCodec();
    compressionRegistry.set(CompressionType.ZSTD, codec);
    const pages = await drain(await clientFor(chunked(squeezed)).client.items('tok', ZSTD));
    expect(pages.map((p) => p.rows)).toEqual(PAGES.map(rowsOf));
    expect(compressionRegistry.get(CompressionType.ZSTD)).toBe(codec);
    expect(codec.decodes).toBeGreaterThan(0);
  });

  it('reads the artifacts head, whose counts are of artifacts served', async () => {
    const body = framed([json(HEAD, {page_rows: 5, served: 3, matched: 1}), trailer(0, 0, 'c4')]);
    const read = await clientFor(chunked(body)).client.artifacts('tok', {view: 's0', layer: 'l', fields: ['key'], count: true});
    expect(read.head).toEqual<ArtifactsHead>({pageRows: 5, served: 3, matched: 1});
    expect(await drain(read)).toEqual([]);
    // A response that found no row can still move the cursor on.
    expect(read.cursor).toBe('c4');
    const uncounted = await clientFor(chunked(framed([json(HEAD, {page_rows: 5}), trailer(0, 0)]))).client.artifacts('tok', {view: 's0', layer: 'l', fields: []});
    expect(uncounted.head).toEqual<ArtifactsHead>({pageRows: 5, served: null, matched: null});
  });

  it('yields the whole pages of a body cut before its trailer, then refuses it, resuming after the last page end', async () => {
    const all = parts();
    // Cut after the second records frame, whose page end never arrived; and after its page end.
    for (const [cut, whole] of [[4, 1], [5, 2]] as const) {
      const read = await clientFor(chunked(framed(all.slice(0, cut)))).client.items('tok', {...REQUEST, cursor: 'c0'});
      const got: Table[] = [];
      const reading = (async () => {
        for await (const table of read) got.push(table);
      })();
      await rejectsAsRefused(reading);
      expect(got.map(rowsOf)).toEqual(PAGES.slice(0, whole).map(rowsOf));
      expect(read.trailer).toBeNull();
      expect(read.cursor).toBe(CURSORS[whole - 1]);
    }
  });

  it('resumes from the request’s own cursor when the body is cut before any page end', async () => {
    const cut = framed(parts().slice(0, 2));
    const read = await clientFor(chunked(cut)).client.items('tok', {...REQUEST, cursor: 'c0'});
    await rejectsAsRefused(drain(read));
    expect(read.cursor).toBe('c0');
    const fresh = await clientFor(chunked(cut)).client.items('tok', REQUEST);
    await rejectsAsRefused(drain(fresh));
    expect(fresh.cursor).toBeUndefined();
  });

  it('refuses a body whose frames are out of order', async () => {
    const [head, records, end] = parts();
    // Each trailer counts exactly the pages its body carries, so only the order is wrong.
    const cases: Record<string, Frame[]> = {
      'no head': [records!, end!, trailer(1, 3)],
      'a viewport frame': [head!, {kind: 1, payload: new Uint8Array(8)}, trailer(0, 0)],
      'an unknown kind': [head!, {kind: 9, payload: new Uint8Array(0)}, trailer(0, 0)],
      'a second head': [head!, head!, trailer(0, 0)],
      'two records frames with no page end between': [head!, records!, records!, end!, trailer(1, 3)],
      'a page end with no records frame': [head!, end!, trailer(0, 0)],
      'a trailer while a page is open': [head!, records!, trailer(0, 0)],
      'a frame after the trailer': [head!, trailer(1, 3), records!, end!],
      'an empty body': []
    };
    for (const [name, body] of Object.entries(cases)) {
      await rejectsAsRefused(clientFor(chunked(framed(body))).client.items('tok', REQUEST).then(drain)).catch((error: unknown) => {
        throw new Error(`${name}: ${String(error)}`);
      });
    }
  });

  it('refuses a trailer that does not count what arrived, or names no cursor', async () => {
    const [head, records, end] = parts();
    const cases: Record<string, Frame[]> = {
      'a page that did not arrive': [head!, records!, end!, trailer(2, 6)],
      'the pages right and the rows wrong': [head!, records!, end!, trailer(1, 4)],
      'no cursor': [head!, json(TRAILER, {pages: 0, rows: 0, ended_by: 'end', stream_us: 1})]
    };
    for (const [name, body] of Object.entries(cases)) {
      await rejectsAsRefused(clientFor(chunked(framed(body))).client.items('tok', REQUEST).then(drain)).catch((error: unknown) => {
        throw new Error(`${name}: ${String(error)}`);
      });
    }
  });

  it('throws a refusal before any page, as a TesseraError with its code', async () => {
    const {client} = clientFor(new Response(JSON.stringify({error: 'contract', detail: 'unknown field'}), {status: 422}));
    const thrown = await client.items('tok', REQUEST).catch((error: unknown) => error);
    expect(thrown).toBeInstanceOf(TesseraError);
    expect(thrown).toMatchObject({status: 422});
    expect(refusalOf(thrown).code).toBe('contract');
  });

  it('stops when its signal aborts, and delivers nothing after, not even pages already received', async () => {
    const abort = new AbortController();
    const feed = manual(abort.signal);
    const {client, sent} = clientFor(feed.response);
    feed.push(framed(parts().slice(0, -1)));
    const read = await client.items('tok', REQUEST, abort.signal);
    expect(sent[0]!.signal).toBe(abort.signal);
    const first = await read.next();
    expect(first.done).toBe(false);
    abort.abort();
    const thrown = await read.next().catch((error: unknown) => error);
    expect(thrown).toMatchObject({name: 'AbortError'});
    expect(await read.next()).toEqual({done: true, value: undefined});
    expect(read.cursor).toBe('c1');
  });

  it('releases the body when the caller stops early, before or after the first page', async () => {
    const body = framed(parts());
    let cancelled = false;
    const read = await clientFor(chunked(body, 16, {onCancel: () => (cancelled = true)})).client.items('tok', REQUEST);
    for await (const table of read) {
      expect(table.numRows).toBe(3);
      break;
    }
    expect(cancelled).toBe(true);
    expect(read.cursor).toBe('c1');

    let unreadCancelled = false;
    const untouched = await clientFor(chunked(body, 16, {onCancel: () => (unreadCancelled = true)})).client.items('tok', REQUEST);
    await untouched.return();
    expect(unreadCancelled).toBe(true);
    expect(await untouched.next()).toEqual({done: true, value: undefined});
  });

  it('ends a page that is being waited for as the end of the read when the caller returns', async () => {
    const feed = manual();
    feed.push(framed(parts().slice(0, 3)));
    const read = await clientFor(feed.response).client.items('tok', REQUEST);
    expect((await read.next()).done).toBe(false);
    const waiting = read.next();
    await read.return();
    expect(await waiting).toEqual({done: true, value: undefined});
    expect(read.trailer).toBeNull();
    expect(read.cursor).toBe('c1');
  });
});
