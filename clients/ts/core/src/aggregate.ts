import {Table, tableFromIPC} from 'apache-arrow';
import {FRAME_PAGE_END, FRAME_RECORDS, FRAME_TABLE_HEAD, FRAME_TRAILER} from './frame.js';
import {Frames, pageEndOf, registerZstd, trailerFrom, type RecordsRequest} from './records.js';
import {parseRegionVerdict} from './region.js';
import type {AggregateResult, AggregateSample, AggregateTable, RegionVerdict} from './types.js';

type RawTableHead = {grouping?: unknown; total?: unknown; reference_total?: unknown; groups?: unknown; resumed?: unknown; sample?: unknown};

/** One table as it is read: the figures of its first head, and its pages so far. */
type Reading = Omit<AggregateTable, 'rows'> & {pages: Table[]};

/**
 * Read a `POST /v1/aggregate` result: the first response, and with `follow` each response after it
 * from the cursor the one before it ended with, until that cursor is null. A table carried over
 * several responses opens each one with its head again, and its pages are joined in order.
 *
 * A body that ends or is cut without its trailer throws {@link PartialAggregate}, holding the whole
 * pages read before it and the cursor to resume from. A trailer that counts other pages or rows
 * than the body carried throws `Error`. An abort of `signal` throws its reason.
 */
export async function readAggregate(
  given: {cursor?: string; compression?: 'zstd'},
  request: RecordsRequest,
  signal: AbortSignal | undefined,
  follow: boolean
): Promise<AggregateResult> {
  if (given.compression === 'zstd') registerZstd();
  const tables = new Map<number, Reading>();
  let cursor = given.cursor;
  let region: RegionVerdict | null = null;
  let identityKey = '';
  let recomposed = false;
  for (;;) {
    const response = await request(cursor);
    region ??= parseRegionVerdict(response.headers.get('x-tessera-region'));
    identityKey = response.headers.get('x-tessera-identity-key') ?? identityKey;
    const frames = new Frames(response.body?.getReader(), signal, 'aggregate');
    let table: Reading | null = null;
    let pending: Uint8Array | null = null;
    let trailer: Uint8Array | null = null;
    // Where a read cut in this response resumes: after its last whole page.
    let resume: string | null = cursor ?? null;
    let pages = 0;
    let rows = 0;
    try {
      for (let frame = await frames.next(); frame !== null; frame = await frames.next()) {
        if (frame.kind === FRAME_TABLE_HEAD) {
          const head = tableHeadOf(frame.payload);
          table = tables.get(head.grouping) ?? null;
          if (table === null) {
            table = {...head, pages: []};
            tables.set(head.grouping, table);
          }
        } else if (frame.kind === FRAME_RECORDS) {
          pending = frame.payload;
        } else if (frame.kind === FRAME_PAGE_END) {
          // The grammar puts a table head before every records frame, and a records frame before
          // every page end.
          const page = tableFromIPC(pending!);
          pending = null;
          resume = pageEndOf(frame.payload).next;
          table!.pages.push(page);
          pages += 1;
          rows += page.numRows;
        } else if (frame.kind === FRAME_TRAILER) {
          trailer = frame.payload;
        }
      }
    } catch (error) {
      if (signal?.aborted) throw error;
      const why = error instanceof Error ? error.message : String(error);
      throw new PartialAggregate(why, {tables: whole(tables), region, recomposed, identityKey, next: resume});
    } finally {
      frames.release();
    }
    // A complete body has a trailer: the grammar refuses one without.
    const raw = JSON.parse(new TextDecoder().decode(trailer!)) as Parameters<typeof trailerFrom>[0] & {recomposed?: unknown};
    const ended = trailerFrom(raw);
    if (ended.pages !== pages || ended.rows !== rows) {
      throw new Error(`the trailer counts ${ended.pages} pages and ${ended.rows} rows, but the body carried ${pages} and ${rows}`);
    }
    if (raw.recomposed === true) recomposed = true;
    cursor = ended.next ?? undefined;
    if (ended.next === null || !follow) return {tables: whole(tables), region, recomposed, identityKey, next: ended.next};
    signal?.throwIfAborted();
  }
}

/**
 * An aggregate read whose response ended or was cut before its trailer. {@link result} holds every
 * whole page read before the stop, and its `next` is the cursor to pass as `cursor` to read on
 * without repeating a row; `null` there means read again from the start.
 *
 * @category HTTP client
 */
export class PartialAggregate extends Error {
  constructor(
    why: string,
    /** The tables as far as they were read. */
    readonly result: AggregateResult
  ) {
    super(`${why}; the whole pages read before the stop are this error's result, and its next is the cursor to read on from`);
    this.name = 'PartialAggregate';
  }
}

/** Each table read, in grouping order, its pages joined. */
function whole(tables: Map<number, Reading>): AggregateTable[] {
  return [...tables.values()].sort((a, b) => a.grouping - b.grouping).map(({pages, ...head}) => ({...head, rows: joined(pages)}));
}

/** A table's pages as one table. Each page keeps its own dictionaries. */
function joined(pages: Table[]): Table {
  const [first, ...rest] = pages;
  return first === undefined ? new Table() : first.concat(...rest);
}

function tableHeadOf(payload: Uint8Array): Omit<AggregateTable, 'rows'> {
  const raw = (JSON.parse(new TextDecoder().decode(payload)) ?? {}) as RawTableHead;
  if (!Number.isInteger(raw.grouping) || !Number.isInteger(raw.total)) {
    throw new Error('a table head has no `grouping` or `total`, which the contract requires; the server and this client are from different versions');
  }
  return {
    grouping: raw.grouping as number,
    total: raw.total as number,
    referenceTotal: Number.isInteger(raw.reference_total) ? (raw.reference_total as number) : null,
    groups: Number.isInteger(raw.groups) ? (raw.groups as number) : null,
    sample: sampleOf(raw.sample)
  };
}

function sampleOf(raw: unknown): AggregateSample | null {
  if (raw === undefined || raw === null) return null;
  const sample = raw as {sampled?: unknown; items?: unknown; reference_items?: unknown};
  if (typeof sample.sampled !== 'boolean' || !Number.isInteger(sample.items)) {
    throw new Error('a table head\'s `sample` has no `sampled` or `items`, which the contract requires; the server and this client are from different versions');
  }
  return {
    sampled: sample.sampled,
    items: sample.items as number,
    referenceItems: Number.isInteger(sample.reference_items) ? (sample.reference_items as number) : null
  };
}
