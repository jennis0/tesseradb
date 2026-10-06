import {Dictionary, Int8, Int32, Table, tableToIPC, TimestampMicrosecond, Uint64, Uint8, Utf8, vectorFromArray} from 'apache-arrow';
import {describe, expect, it} from 'vitest';
import {PartialAggregate} from '../src/aggregate.js';
import {TesseraClient, TesseraError} from '../src/client.js';
import type {Frame} from '../src/frame.js';
import type {AggregateRequest} from '../src/types.js';
import {chunked, framed, manual, u64} from './support.js';

/**
 * `TesseraClient.aggregate` over bodies built here from the wire layout: per table a head (kind 9)
 * and records frames (7) each followed by a page end (8), then a trailer (4).
 */

const TABLE_HEAD = 9;
const RECORDS = 7;
const PAGE_END = 8;
const TRAILER = 4;

const json = (kind: number, value: unknown): Frame => ({kind, payload: new TextEncoder().encode(JSON.stringify(value))});

/** A page of a field grouping: `group`, `key`, `title` and `count`, each page with its own dictionaries. */
function fieldPage(rows: [group: string, key: string | null, count: bigint][]): Table {
  return new Table({
    group: vectorFromArray(rows.map(([g]) => g), new Dictionary(new Utf8(), new Int8())),
    key: vectorFromArray(rows.map(([, k]) => k), new Dictionary(new Utf8(), new Int32())),
    title: vectorFromArray(rows.map(([, k]) => (k === null ? null : k.toUpperCase())), new Dictionary(new Utf8(), new Int32())),
    count: u64(rows.map(([, , c]) => c))
  });
}

const countPage = (count: bigint) => new Table({count: u64([count])});

type Page = {table: Table; next: string | null};
type Part = {head?: Record<string, unknown>; pages: Page[]};

/** One response: each part's head where it has one, its pages, then a trailer naming `next`. */
function responseOf(parts: Part[], next: string | null, trailer: Record<string, unknown> = {}): Uint8Array {
  const frames: Frame[] = [];
  let pages = 0;
  let rows = 0;
  for (const part of parts) {
    if (part.head) frames.push(json(TABLE_HEAD, part.head));
    for (const {table, next: end} of part.pages) {
      frames.push({kind: RECORDS, payload: tableToIPC(table, 'stream')}, json(PAGE_END, {next: end, ended_by: end === null ? 'end' : 'rows'}));
      pages += 1;
      rows += table.numRows;
    }
  }
  frames.push(json(TRAILER, {pages, rows, next, ended_by: next === null ? 'end' : 'pages', stream_us: 9, ...trailer}));
  return framed(frames);
}

/** A client whose `fetch` answers each request by the cursor it carried, `''` for none, once each. */
function clientFor(answers: Record<string, () => Response>) {
  const sent: {url: string; body: Record<string, unknown>}[] = [];
  const asked = new Set<string>();
  const client = new TesseraClient({
    viewerUrl: 'http://viewer',
    sessionUrl: 'http://session',
    fetch: (async (url: string, init?: RequestInit) => {
      const body = JSON.parse(init!.body as string) as Record<string, unknown>;
      sent.push({url, body});
      const key = (body.cursor as string | undefined) ?? '';
      if (asked.has(key) || !answers[key]) throw new Error(`a request from cursor ${JSON.stringify(body.cursor)} was not expected`);
      asked.add(key);
      return answers[key]!();
    }) as typeof fetch
  });
  return {client, sent};
}

/** The bytes of a body's last frame, its trailer. */
function trailerBytes(body: Uint8Array): number {
  const view = new DataView(body.buffer, body.byteOffset, body.byteLength);
  let at = 0;
  let last = 0;
  while (at < body.byteLength) {
    last = at;
    at += 5 + view.getUint32(at + 1, true);
  }
  return body.byteLength - last;
}

/** The rows of a table as plain objects. */
const rowsOf = (table: Table) => table.toArray().map((row) => ({...row.toJSON()}));

const REQUEST: AggregateRequest = {view: 's0', groupings: [{}, {by: {field: 'archive', top: 2}}]};

/**
 * Two responses: the size of the set and the first page of the breakdown in the first, the rest of
 * the breakdown in the second, which opens with its head again.
 */
const TWO = {
  '': () =>
    chunked(
      responseOf(
        [
          {head: {grouping: 0, total: 10, resumed: false}, pages: [{table: countPage(10n), next: 'p1'}]},
          {head: {grouping: 1, total: 10, groups: 5, resumed: false}, pages: [{table: fieldPage([['listed', 'cs', 4n], ['listed', 'hep', 3n]]), next: 'p2'}]}
        ],
        'c1'
      ),
      11,
      {headers: {'x-tessera-identity-key': 'ik1', 'x-tessera-region': 'cover; depth=9'}}
    ),
  c1: () =>
    chunked(responseOf([{head: {grouping: 1, total: 11, groups: 6, resumed: true}, pages: [{table: fieldPage([['rest', null, 2n], ['none', null, 1n]]), next: null}]}], null, {recomposed: true}), 64, {
      headers: {'x-tessera-identity-key': 'ik2'}
    })
};

describe('TesseraClient.aggregate', () => {
  it('sends the request as given, each field that is set under its wire name, and each follow-up from the cursor', async () => {
    const {client, sent} = clientFor(TWO);
    await client.aggregate('tok', {
      view: 's0',
      filters: {archive: {in: ['cs']}},
      reference: {},
      groupings: [{by: {layer: 'clusters', level: 1, artifacts: [2n ** 63n + 5n]}, cells: {depth: 8, area: [0, 0, 10, 10]}}],
      pageRows: 100,
      pages: 2,
      compression: 'zstd'
    });
    expect(sent.map((s) => s.url)).toEqual(['http://viewer/v1/aggregate', 'http://viewer/v1/aggregate']);
    expect(sent[0]!.body).toEqual({
      view: 's0',
      filters: {archive: {in: ['cs']}},
      reference: {},
      groupings: [{by: {layer: 'clusters', level: 1, artifacts: ['9223372036854775813']}, cells: {depth: 8, area: [0, 0, 10, 10]}}],
      page_rows: 100,
      pages: 2,
      compression: 'zstd'
    });
    expect(sent[1]!.body).toEqual({...sent[0]!.body, cursor: 'c1'});
  });

  it('reads every table through the cursor, joining a table carried over two responses', async () => {
    const {client} = clientFor(TWO);
    const result = await client.aggregate('tok', REQUEST);
    expect(result.tables.map(({rows, ...head}) => head)).toEqual([
      {grouping: 0, total: 10, referenceTotal: null, groups: null, sample: null},
      // The figures of the first head, whose page ranked the groups.
      {grouping: 1, total: 10, referenceTotal: null, groups: 5, sample: null}
    ]);
    expect(rowsOf(result.tables[0]!.rows)).toEqual([{count: 10n}]);
    expect(rowsOf(result.tables[1]!.rows)).toEqual([
      {group: 'listed', key: 'cs', title: 'CS', count: 4n},
      {group: 'listed', key: 'hep', title: 'HEP', count: 3n},
      {group: 'rest', key: null, title: null, count: 2n},
      {group: 'none', key: null, title: null, count: 1n}
    ]);
    expect(result.recomposed).toBe(true);
    expect(result.region).toEqual({exact: false, depth: 9});
    expect(result.identityKey).toBe('ik2');
    expect(result.next).toBeNull();
  });

  it('reads the first response alone without follow, and says where the read continues', async () => {
    const {client, sent} = clientFor(TWO);
    const first = await client.aggregate('tok', REQUEST, undefined, {follow: false});
    expect(sent).toHaveLength(1);
    expect(first.next).toBe('c1');
    expect(first.recomposed).toBe(false);
    expect(first.tables[1]!.rows.numRows).toBe(2);
    const rest = await client.aggregate('tok', {...REQUEST, cursor: first.next!});
    expect(rest.tables.map((t) => [t.grouping, t.total, t.rows.numRows])).toEqual([[1, 11, 2]]);
  });

  it('sends a histogram\'s sample size and reads how its table was counted', async () => {
    const table = new Table({count: u64([7n])});
    const head = {grouping: 0, total: 900, reference_total: 1000, groups: 1, resumed: false, sample: {sampled: true, items: 96, reference_items: 101}};
    const {client, sent} = clientFor({'': () => chunked(responseOf([{head, pages: [{table, next: null}]}], null))});
    const result = await client.aggregate('tok', {view: 's0', reference: {}, groupings: [{by: {field: 'year', bins: 10, sample: 100}}]});
    expect(sent[0]!.body).toMatchObject({groupings: [{by: {field: 'year', bins: 10, sample: 100}}]});
    expect(result.tables[0]!.sample).toEqual({sampled: true, items: 96, referenceItems: 101});
  });

  it('sends a summary and reads its one row of figures', async () => {
    const table = new Table({
      items: u64([900n]),
      count: u64([850n]),
      none: u64([40n]),
      min: vectorFromArray([-3.5]),
      max: vectorFromArray([99.5]),
      mean: vectorFromArray([41.25])
    });
    const head = {grouping: 0, total: 300, resumed: false};
    const {client, sent} = clientFor({'': () => chunked(responseOf([{head, pages: [{table, next: null}]}], null))});
    const result = await client.aggregate('tok', {view: 's0', filters: {kind: {eq: 'a'}}, groupings: [{by: {field: 'score', summary: true}}]});
    expect(sent[0]!.body).toMatchObject({groupings: [{by: {field: 'score', summary: true}}]});
    expect(result.tables[0]).toMatchObject({total: 300, groups: null, sample: null});
    expect(rowsOf(result.tables[0]!.rows)).toEqual([{items: 900n, count: 850n, none: 40n, min: -3.5, max: 99.5, mean: 41.25}]);
  });

  it('sends a layer grouping’s palette size under its wire name, and reads its slot column', async () => {
    const table = new Table({
      group: vectorFromArray(['listed', 'listed', 'rest'], new Dictionary(new Utf8(), new Int8())),
      key: vectorFromArray([5n, 6n, null], new Uint64()),
      slot: vectorFromArray([4, 1, null], new Uint8()),
      count: u64([3n, 2n, 1n])
    });
    const {client, sent} = clientFor({'': () => chunked(responseOf([{head: {grouping: 0, total: 6, groups: 3, resumed: false}, pages: [{table, next: null}]}], null))});
    const result = await client.aggregate('tok', {view: 's0', groupings: [{by: {layer: 'l', top: 2, paletteSize: 20}}, {by: {field: 'f', top: 1}}]});
    expect(sent[0]!.body).toMatchObject({groupings: [{by: {layer: 'l', top: 2, palette_size: 20}}, {by: {field: 'f', top: 1}}]});
    expect(rowsOf(result.tables[0]!.rows).map((r) => r.slot)).toEqual([4, 1, null]);
  });

  it('carries the reference columns and a layer key as they arrive', async () => {
    const table = new Table({
      group: vectorFromArray(['listed', 'none'], new Dictionary(new Utf8(), new Int8())),
      key: vectorFromArray([2n ** 64n - 1n, null], new Uint64()),
      count: u64([3n, 0n]),
      reference_count: u64([6n, 4n]),
      lift: vectorFromArray([1.5, null])
    });
    const {client} = clientFor({'': () => chunked(responseOf([{head: {grouping: 0, total: 3, reference_total: 10, groups: 1, resumed: false}, pages: [{table, next: null}]}], null))});
    const result = await client.aggregate('tok', {view: 's0', reference: {}, groupings: [{by: {layer: 'l', top: 1}}]});
    expect(result.tables[0]).toMatchObject({total: 3, referenceTotal: 10, groups: 1});
    expect(rowsOf(result.tables[0]!.rows)).toEqual([
      {group: 'listed', key: 2n ** 64n - 1n, count: 3n, reference_count: 6n, lift: 1.5},
      {group: 'none', key: null, count: 0n, reference_count: 4n, lift: null}
    ]);
    expect(result.region).toBeNull();
    expect(result.recomposed).toBe(false);
  });

  it('sends a grouping by bins with its range exact, and reads each bin with its edges', async () => {
    const day = 86_400_000;
    const table = new Table({
      group: vectorFromArray(['listed', 'listed', 'none'], new Dictionary(new Utf8(), new Int8())),
      lower: vectorFromArray([0, day, null], new TimestampMicrosecond('UTC')),
      upper: vectorFromArray([day, 2 * day, null], new TimestampMicrosecond('UTC')),
      count: u64([3n, 0n, 2n])
    });
    const {client, sent} = clientFor({'': () => chunked(responseOf([{head: {grouping: 0, total: 5, groups: 1, resumed: false}, pages: [{table, next: null}]}], null))});
    const result = await client.aggregate('tok', {view: 's0', groupings: [{by: {field: 'seen', bins: 2, range: [0, 2n ** 60n + 1n]}}]});
    expect(sent[0]!.body.groupings).toEqual([{by: {field: 'seen', bins: 2, range: [0, '1152921504606846977']}}]);
    expect(rowsOf(result.tables[0]!.rows)).toEqual([
      {group: 'listed', lower: 0, upper: day, count: 3n},
      {group: 'listed', lower: day, upper: 2 * day, count: 0n},
      {group: 'none', lower: null, upper: null, count: 2n}
    ]);
  });

  it('continues past a response cancelled before its first page, which is a trailer alone', async () => {
    const {client, sent} = clientFor({
      '': () => chunked(responseOf([], 'c0')),
      c0: () => chunked(responseOf([{head: {grouping: 0, total: 1, resumed: false}, pages: [{table: countPage(1n), next: null}]}], null))
    });
    const result = await client.aggregate('tok', {view: 's0', groupings: [{}]});
    expect(sent).toHaveLength(2);
    expect(rowsOf(result.tables[0]!.rows)).toEqual([{count: 1n}]);
  });

  it('throws a body cut before its trailer as a partial result, which resumes from its cursor without repeating a row', async () => {
    const second = responseOf([{head: {grouping: 1, total: 10, groups: 5, resumed: false}, pages: [{table: fieldPage([['listed', 'cs', 4n], ['listed', 'hep', 3n]]), next: 'p2'}]}], 'c1');
    const firstOnly = responseOf([{head: {grouping: 0, total: 10, resumed: false}, pages: [{table: countPage(10n), next: 'p1'}]}], null);
    // The first table's page and its page end, then the second table's head and part of its page.
    const cut = new Uint8Array([...firstOnly.slice(0, firstOnly.byteLength - trailerBytes(firstOnly)), ...second.slice(0, 60)]);
    const {client} = clientFor({'': () => chunked(cut), p1: TWO.c1 as () => Response});
    const stopped = await client.aggregate('tok', REQUEST).catch((error: unknown) => error);
    expect(stopped).toBeInstanceOf(PartialAggregate);
    const partial = (stopped as PartialAggregate).result;
    expect(partial.next).toBe('p1');
    expect(partial.tables.map((t) => [t.grouping, t.rows.numRows])).toEqual([[0, 1], [1, 0]]);
    const rest = await client.aggregate('tok', {...REQUEST, cursor: partial.next!});
    expect(rest.tables.map((t) => [t.grouping, t.rows.numRows])).toEqual([[1, 2]]);
  });

  it('throws on a trailer that miscounts the pages, and on a records frame before any table head', async () => {
    const miscounted = responseOf([{head: {grouping: 0, total: 1, resumed: false}, pages: [{table: countPage(1n), next: null}]}], null, {pages: 2});
    await expect(clientFor({'': () => chunked(miscounted)}).client.aggregate('tok', {view: 's0', groupings: [{}]})).rejects.toThrow(Error);
    // A records frame with no table head before it.
    const headless = framed([{kind: RECORDS, payload: tableToIPC(countPage(1n), 'stream')}, json(PAGE_END, {next: null, ended_by: 'end'}), json(TRAILER, {pages: 1, rows: 1, next: null, ended_by: 'end', stream_us: 1})]);
    await expect(clientFor({'': () => chunked(headless)}).client.aggregate('tok', {view: 's0', groupings: [{}]})).rejects.toThrow(Error);
  });

  it('rejects with the refusal, first request or follow-up', async () => {
    const refusal = () => new Response(JSON.stringify({error: 'contract', detail: 'more cells than selection.max_aggregate_cells'}), {status: 422});
    const first = clientFor({'': refusal}).client.aggregate('tok', REQUEST);
    await expect(first).rejects.toBeInstanceOf(TesseraError);
    await expect(first).rejects.toMatchObject({status: 422, code: 'contract'});
    const later = clientFor({'': TWO[''], c1: refusal}).client.aggregate('tok', REQUEST);
    await expect(later).rejects.toMatchObject({status: 422, retryAfterS: null});
    const shed = () => new Response(JSON.stringify({error: 'backpressure', detail: 'shed', retry_after_s: 1}), {status: 429, headers: {'retry-after': '2'}});
    await expect(clientFor({'': shed}).client.aggregate('tok', REQUEST)).rejects.toMatchObject({status: 429, retryAfterS: 2});
  });

  it('stops at an abort of its signal, mid-body', async () => {
    const controller = new AbortController();
    const body = manual(controller.signal);
    const client = new TesseraClient({viewerUrl: 'http://viewer', sessionUrl: '', fetch: (async () => body.response) as typeof fetch});
    const read = client.aggregate('tok', REQUEST, controller.signal);
    const whole = TWO['']();
    body.push(new Uint8Array(await whole.arrayBuffer()).slice(0, 30));
    controller.abort();
    await expect(read).rejects.toThrow();
  });
});
