import {compressionRegistry, CompressionType, tableFromIPC, type Table} from 'apache-arrow';
import {decompress} from 'fzstd';
import {FRAME_PAGE_END, FRAME_RECORDS, FRAME_TRAILER, FrameReader, type Frame} from './frame.js';
import {parseRegionVerdict} from './region.js';
import type {PageEnd, RecordsTrailer, RegionVerdict, Timings} from './types.js';

/**
 * One response of `POST /v1/items` or `POST /v1/artifacts`: the head, then each page as an Arrow
 * table as it arrives, then the trailer.
 *
 * A page is yielded once its page end has arrived, so a records frame cut off before its page end
 * is never yielded. A body that ends or is cut without its trailer throws after the last whole
 * page, and {@link cursor} is then the last page end's cursor, from which the read resumes without
 * repeating a row.
 *
 * The body is read only as the caller iterates. Iterating to the end, breaking out of the loop,
 * calling `return()` or aborting the request's signal each release the connection. A `return()`
 * while a page is awaited ends that wait as the end of the read.
 */
export class RecordsRead<Head> implements AsyncIterableIterator<Table> {
  /** The viewport's identity coordinate for this principal and view; empty where the response carried none. */
  readonly identityKey: string;
  /** The `x-tessera-region` verdict, present where `filters` carried a `region` leaf. */
  readonly region: RegionVerdict | null;
  /** The server's time from admission to the head, and the time admission took, in microseconds. */
  readonly timings: Pick<Timings, 'serverUs' | 'admissionUs'>;
  private lastEnd: PageEnd | null = null;
  private ended: RecordsTrailer | null = null;
  private readonly pages: AsyncGenerator<Table, undefined, undefined>;

  constructor(
    readonly head: Head,
    headers: Headers,
    /** The cursor the request carried. */
    private readonly from: string | undefined,
    private readonly frames: Frames
  ) {
    this.identityKey = headers.get('x-tessera-identity-key') ?? '';
    this.region = parseRegionVerdict(headers.get('x-tessera-region'));
    this.timings = {
      serverUs: Number(headers.get('x-tessera-server-us') ?? 0),
      admissionUs: Number(headers.get('x-tessera-admission-us') ?? 0)
    };
    this.pages = this.read();
  }

  /** The page end after the page last yielded; `null` before the first. */
  get pageEnd(): PageEnd | null {
    return this.lastEnd;
  }

  /** `null` until the whole body has been read. */
  get trailer(): RecordsTrailer | null {
    return this.ended;
  }

  /**
   * Where the read continues: the trailer's `next` once the body is whole, the last page end's
   * before that, and the request's own `cursor` before any page end. `null` means no row remains;
   * `undefined` means from the beginning.
   */
  get cursor(): string | null | undefined {
    if (this.ended) return this.ended.next;
    if (this.lastEnd) return this.lastEnd.next;
    return this.from;
  }

  next(): Promise<IteratorResult<Table, undefined>> {
    return this.pages.next();
  }

  return(): Promise<IteratorResult<Table, undefined>> {
    // A generator that never started skips its `finally`, so the body is released here too.
    this.frames.release();
    return this.pages.return(undefined);
  }

  [Symbol.asyncIterator](): this {
    return this;
  }

  private async *read(): AsyncGenerator<Table, undefined, undefined> {
    let open: Uint8Array | null = null;
    let trailer: RecordsTrailer | null = null;
    let pages = 0;
    let rows = 0;
    try {
      for (let frame = await this.frames.next(); frame !== null; frame = await this.frames.next()) {
        if (frame.kind === FRAME_RECORDS) {
          open = frame.payload;
        } else if (frame.kind === FRAME_PAGE_END) {
          // The grammar puts a records frame before every page end.
          const table = tableFromIPC(open!);
          open = null;
          this.lastEnd = pageEndOf(frame.payload);
          pages += 1;
          rows += table.numRows;
          yield table;
        } else if (frame.kind === FRAME_TRAILER) {
          trailer = trailerOf(frame.payload);
        }
      }
    } finally {
      this.frames.release();
    }
    if (this.frames.released) return undefined;
    // A complete body has a trailer: the grammar refuses one without.
    if (trailer!.pages !== pages || trailer!.rows !== rows) {
      throw new Error(`the trailer counts ${trailer!.pages} pages and ${trailer!.rows} rows, but the body carried ${pages} and ${rows}`);
    }
    this.ended = trailer;
    return undefined;
  }
}

/**
 * Read a bulk read's response up to its head. `parseHead` checks and translates the head's JSON.
 * A body that fails before its head, or whose head `parseHead` refuses, throws. The request's
 * `cursor` is where the read resumes before any page end, and a zstd decoder is registered only
 * where it asked for `compression: 'zstd'`.
 */
export async function openRecords<Head>(
  response: Response,
  request: {cursor?: string; compression?: 'zstd'},
  signal: AbortSignal | undefined,
  parseHead: (raw: unknown) => Head
): Promise<RecordsRead<Head>> {
  if (request.compression === 'zstd') registerZstd();
  const frames = new Frames(response.body?.getReader(), signal);
  try {
    // The grammar refuses a body whose first frame is not the head, and one that ends before it.
    const first = (await frames.next())!;
    const head = parseHead(JSON.parse(new TextDecoder().decode(first.payload)));
    return new RecordsRead(head, response.headers, request.cursor, frames);
  } catch (error) {
    frames.release();
    throw error;
  }
}

/** The frames of one body, read from the network as they are asked for. */
class Frames {
  private readonly grammar = new FrameReader('records');
  private ready: Frame[] = [];
  /** `released` once the caller has stopped the read, `ended` once the body has been read to its end. */
  private state: 'reading' | 'ended' | 'released' = 'reading';

  constructor(
    private readonly reader: ReadableStreamDefaultReader<Uint8Array> | undefined,
    private readonly signal: AbortSignal | undefined
  ) {}

  /**
   * The next whole frame, or `null` at the end of a complete body or once the read is released.
   * Throws on a body that is not complete, whether it ended or was cut, and once the signal has
   * aborted, even where frames already read remain.
   */
  async next(): Promise<Frame | null> {
    this.signal?.throwIfAborted();
    while (this.ready.length === 0) {
      let chunk: ReadableStreamReadResult<Uint8Array> = {done: true, value: undefined};
      try {
        if (this.reader) chunk = await this.reader.read();
      } catch (error) {
        // The caller's abort stops the read. Any other failure is a connection cut, which is how
        // the server ends a body that fails part-way, so it is judged as the end of the body.
        if (this.signal?.aborted) throw error;
      }
      if (chunk.done) {
        // A release while this read waited ends the body early, and that is not a fault.
        if (this.released) return null;
        this.state = 'ended';
        this.grammar.end();
        return null;
      }
      this.ready = this.grammar.push(chunk.value);
    }
    return this.ready.shift()!;
  }

  get released(): boolean {
    return this.state === 'released';
  }

  /** Stop reading the body, which closes the connection and stops the server's response. */
  release(): void {
    if (this.state !== 'reading') return;
    this.state = 'released';
    void this.reader?.cancel().catch(() => {});
  }
}

/**
 * Give Arrow's shared codec registry a zstd decoder. A decoder the host registered is kept, and so
 * is the host's encoder where it registered only that.
 */
function registerZstd(): void {
  const registered = compressionRegistry.get(CompressionType.ZSTD);
  if (registered?.decode) return;
  compressionRegistry.set(CompressionType.ZSTD, {encode: registered?.encode?.bind(registered), decode: zstdDecode});
}

function zstdDecode(data: Uint8Array): Uint8Array {
  const out = decompress(data);
  // Arrow reads a 64-bit column in place, which needs its buffer to start on an 8-byte boundary.
  return out.byteOffset % 8 === 0 ? out : out.slice();
}

type RawPageEnd = {next?: unknown; ended_by: PageEnd['endedBy']};
type RawTrailer = {pages?: unknown; rows?: unknown; next?: unknown; ended_by: RecordsTrailer['endedBy']; stream_us: number};

const isCursor = (value: unknown): value is string | null => value === null || typeof value === 'string';

function pageEndOf(payload: Uint8Array): PageEnd {
  const raw = (JSON.parse(new TextDecoder().decode(payload)) ?? {}) as RawPageEnd;
  if (!isCursor(raw.next)) throw new Error('a page end has no `next`, which must be a cursor or null');
  return {next: raw.next, endedBy: raw.ended_by};
}

function trailerOf(payload: Uint8Array): RecordsTrailer {
  const raw = (JSON.parse(new TextDecoder().decode(payload)) ?? {}) as RawTrailer;
  if (!isCursor(raw.next)) throw new Error('the trailer has no `next`, which must be a cursor or null');
  if (!Number.isInteger(raw.pages) || !Number.isInteger(raw.rows)) throw new Error('the trailer does not count its pages and rows');
  return {pages: raw.pages as number, rows: raw.rows as number, next: raw.next, endedBy: raw.ended_by, streamUs: raw.stream_us};
}
