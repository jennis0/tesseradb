import {zstdCompressSync} from 'node:zlib';
import {
  compressionRegistry,
  CompressionType,
  Dictionary,
  Float64,
  Int32,
  makeData,
  makeVector,
  RecordBatchStreamWriter,
  Table,
  tableToIPC,
  Uint64,
  Utf8,
  vectorFromArray
} from 'apache-arrow';
import {describe, expect, it} from 'vitest';
import {TesseraClient, TesseraError} from '../src/client.js';
import {refusalOf} from '../src/presented.js';
import type {ArtifactsHead, ItemsHead, ItemsRequest} from '../src/types.js';
import type {RecordsRead} from '../src/records.js';
import {rejectsAsRefused} from './support.js';

/**
 * `TesseraClient.items` and `artifacts` over bodies built here from the wire layout: a head
 * (kind 6), records frames (7) each followed by a page end (8), and a trailer (4).
 */

const HEAD = 6;
const RECORDS = 7;
const PAGE_END = 8;
const TRAILER = 4;

type Part = [kind: number, payload: Uint8Array];

const json = (value: unknown) => new TextEncoder().encode(JSON.stringify(value));

function framed(parts: Part[]): Uint8Array {
  const out = new Uint8Array(parts.reduce((n, [, p]) => n + 5 + p.byteLength, 0));
  const view = new DataView(out.buffer);
  let at = 0;
  for (const [kind, payload] of parts) {
    out[at] = kind;
    view.setUint32(at + 1, payload.byteLength, true);
    out.set(payload, at + 5);
    at += 5 + payload.byteLength;
  }
  return out;
}

const u64 = (values: bigint[]) => makeVector(makeData({type: new Uint64(), data: BigUint64Array.from(values)}));

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
function parts(ipc: (table: Table) => Uint8Array = (t) => tableToIPC(t, 'stream')): Part[] {
  return [
    [HEAD, json({order: 'map', page_rows: 3, visible: 8, matched: 8})],
    ...PAGES.flatMap((table, i): Part[] => [
      [RECORDS, ipc(table)],
      [PAGE_END, json({next: CURSORS[i], ended_by: i < 2 ? 'rows' : 'end'})]
    ]),
    [TRAILER, json({pages: 3, rows: 8, next: null, ended_by: 'end', stream_us: 12})]
  ];
}

/** `table`'s rows as plain objects, a category as its key. */
const rowsOf = (table: Table) => table.toArray().map((row) => ({...row.toJSON()}));

/** A response whose body arrives in chunks of `size` bytes, and whose `cancel` is recorded. */
function respond(body: Uint8Array, size = body.byteLength, headers: Record<string, string> = {}) {
  const seen = {cancelled: false};
  let at = 0;
  const stream = new ReadableStream<Uint8Array>({
    pull(controller) {
      if (at >= body.byteLength) return controller.close();
      controller.enqueue(body.slice(at, at + size));
      at += size;
    },
    cancel() {
      seen.cancelled = true;
    }
  });
  return {response: new Response(stream, {status: 200, headers}), seen};
}

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

/** `table` as an Arrow stream whose buffers are zstd-compressed, as the server writes them. */
function compressed(table: Table): Uint8Array {
  const before = compressionRegistry.get(CompressionType.ZSTD);
  // An encoder only: decoding is left to the client under test.
  compressionRegistry.set(CompressionType.ZSTD, {encode: (data) => new Uint8Array(zstdCompressSync(data))});
  try {
    return RecordBatchStreamWriter.writeAll(table, {compressionType: CompressionType.ZSTD}).toUint8Array(true);
  } finally {
    compressionRegistry.set(CompressionType.ZSTD, before ?? {});
  }
}

describe('TesseraClient.items and artifacts', () => {
  it('sends the request as given, each field that is set under its wire name', async () => {
    const empty = framed([
      [HEAD, json({order: 'stored', page_rows: 10})],
      [TRAILER, json({pages: 0, rows: 0, next: null, ended_by: 'end', stream_us: 1})]
    ]);
    const {client, sent} = clientFor(() => respond(empty).response);
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
    for (const size of [body.byteLength, 64, 3, 1]) {
      const {client} = clientFor(respond(body, size, {'x-tessera-region': 'exact'}).response);
      const read = await client.items('tok', REQUEST);
      expect(read.head).toEqual<ItemsHead>({order: 'map', pageRows: 3, visible: 8, matched: 8});
      expect(read.region).toEqual({exact: true, depth: null});
      expect(read.cursor).toBeUndefined();
      const pages = await drain(read);
      expect(pages).toEqual(
        PAGES.map((table, i) => ({rows: rowsOf(table), next: CURSORS[i], endedBy: i < 2 ? 'rows' : 'end'}))
      );
      expect(read.trailer).toEqual({pages: 3, rows: 8, next: null, endedBy: 'end', streamUs: 12});
      expect(read.cursor).toBeNull();
    }
  });

  it('decodes the columns a page carries: a category as its keys and an absent value as null', async () => {
    const {client} = clientFor(respond(framed(parts())).response);
    const [first] = await drain(await client.items('tok', REQUEST));
    expect(first!.rows[0]).toEqual({tessera_id: 1n, archive: 'hep', title: 'paper 1', score: 0.125});
    expect(first!.rows.map((r) => r.archive)).toEqual(['hep', 'math', 'cs']);
    const [, second] = await drain(await clientFor(respond(framed(parts())).response).client.items('tok', REQUEST));
    expect(second!.rows[0]!.title).toBeNull();
  });

  it('decodes pages whose buffers are zstd-compressed to the tables sent uncompressed', async () => {
    const plain = await drain(await clientFor(respond(framed(parts())).response).client.items('tok', REQUEST));
    const zstd = framed(parts(compressed));
    expect(zstd.byteLength).not.toBe(framed(parts()).byteLength);
    for (const size of [zstd.byteLength, 5]) {
      const read = await clientFor(respond(zstd, size).response).client.items('tok', REQUEST);
      expect(await drain(read)).toEqual(plain);
    }
  });

  it('reads the artifacts head, whose counts are of artifacts served', async () => {
    const body = framed([
      [HEAD, json({page_rows: 5, served: 3, matched: 1})],
      [TRAILER, json({pages: 0, rows: 0, next: 'c4', ended_by: 'budget_time', stream_us: 1})]
    ]);
    const read = await clientFor(respond(body).response).client.artifacts('tok', {view: 's0', layer: 'l', fields: ['key'], count: true});
    expect(read.head).toEqual<ArtifactsHead>({pageRows: 5, served: 3, matched: 1});
    expect(await drain(read)).toEqual([]);
    // A response that found no row can still move the cursor on.
    expect(read.cursor).toBe('c4');
    const counted = await clientFor(respond(framed([[HEAD, json({page_rows: 5})], [TRAILER, json({pages: 0, rows: 0, next: null, ended_by: 'end', stream_us: 1})]])).response).client.artifacts('tok', {view: 's0', layer: 'l', fields: []});
    expect(counted.head).toEqual<ArtifactsHead>({pageRows: 5, served: null, matched: null});
  });

  it('yields the whole pages of a body cut before its trailer, then refuses it, resuming after the last page end', async () => {
    const all = parts();
    // Cut after the second records frame, whose page end never arrived; and after its page end.
    for (const [cut, whole] of [[4, 1], [5, 2]] as const) {
      const {client} = clientFor(respond(framed(all.slice(0, cut))).response);
      const read = await client.items('tok', {...REQUEST, cursor: 'c0'});
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
    const read = await clientFor(respond(cut).response).client.items('tok', {...REQUEST, cursor: 'c0'});
    await rejectsAsRefused(drain(read));
    expect(read.cursor).toBe('c0');
    const fresh = await clientFor(respond(cut).response).client.items('tok', REQUEST);
    await rejectsAsRefused(drain(fresh));
    expect(fresh.cursor).toBeUndefined();
  });

  it('refuses a body whose frames are out of order, or whose trailer does not count what arrived', async () => {
    const [head, records, end, , , , , trailer] = parts();
    const tiles: Part = [1, new Uint8Array(8)];
    const cases: Record<string, Part[]> = {
      'no head': [records!, end!, trailer!],
      'a viewport frame': [head!, tiles, trailer!],
      'an unknown kind': [head!, [9, new Uint8Array(0)], trailer!],
      'two heads': [head!, head!, trailer!],
      'two records frames with no page end between': [head!, records!, records!, end!, trailer!],
      'a page end with no records frame': [head!, end!, trailer!],
      'a trailer after a records frame with no page end': [head!, records!, trailer!],
      'a frame after the trailer': [head!, trailer!, records!, end!],
      'a trailer counting a page that did not arrive': [head!, records!, end!, trailer!],
      'a trailer with no cursor': [head!, [TRAILER, json({pages: 0, rows: 0, ended_by: 'end', stream_us: 1})]],
      'an empty body': []
    };
    for (const [name, body] of Object.entries(cases)) {
      const {client} = clientFor(respond(framed(body)).response);
      await rejectsAsRefused(client.items('tok', REQUEST).then(drain)).catch((error: unknown) => {
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
    const all = parts();
    let controller!: ReadableStreamDefaultController<Uint8Array>;
    const stream = new ReadableStream<Uint8Array>({
      start(c) {
        controller = c;
      }
    });
    const abort = new AbortController();
    // What a real `fetch` does to a body whose request is aborted.
    abort.signal.addEventListener('abort', () => controller.error(new DOMException('The operation was aborted.', 'AbortError')));
    const {client, sent} = clientFor(new Response(stream, {status: 200}));
    controller.enqueue(framed(all.slice(0, -1)));
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
    const broken = respond(body, 16);
    const read = await clientFor(broken.response).client.items('tok', REQUEST);
    for await (const table of read) {
      expect(table.numRows).toBe(3);
      break;
    }
    expect(broken.seen.cancelled).toBe(true);
    expect(read.cursor).toBe('c1');

    const unread = respond(body, 16);
    const untouched = await clientFor(unread.response).client.items('tok', REQUEST);
    await untouched.return();
    expect(unread.seen.cancelled).toBe(true);
    expect(await untouched.next()).toEqual({done: true, value: undefined});
  });
});
