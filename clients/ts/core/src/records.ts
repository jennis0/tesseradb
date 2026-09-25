import {compressionRegistry, CompressionType, tableFromIPC, type Table} from 'apache-arrow';
import {decompress} from 'fzstd';
import {FRAME_PAGE_END, FRAME_RECORDS, FRAME_TRAILER, FrameReader, type Frame} from './frame.js';
import {parseRegionVerdict} from './region.js';
import type {PageEnd, RecordsTrailer, RegionVerdict, Timings} from './types.js';

/**
 * Sends the read's request, from `cursor` where one is given and as the caller wrote it where not,
 * and answers the response, or throws the refusal.
 */
export type RecordsRequest = (cursor?: string) => Promise<Response>;

/**
 * A whole bulk read of `POST /v1/items` or `POST /v1/artifacts`, as {@link TesseraClient.items}
 * and {@link TesseraClient.artifacts} return it: each page as an Arrow table as it arrives,
 * response after response, each response requested from the cursor the one before it ended with,
 * until that cursor is null.
 *
 * A page is yielded once its page end has arrived, so a records frame cut off before its page end
 * is never yielded. A response that found no row yields one table of no rows, which carries the
 * read's columns. A body that ends or is cut without its trailer throws after its last whole page,
 * and so does a follow-up request the server refuses. {@link cursor} is then where the read
 * resumes without repeating a row.
 *
 * The bodies are read only as the caller iterates. Iterating to the end, breaking out of the loop,
 * calling `return()` or aborting the request's signal each release the connection and send no
 * further request. A `return()` while a page is awaited ends that wait as the end of the read.
 *
 * ```ts
 * const read = await client.items(token, {view: 'papers', fields: ['title', 'year']});
 * for await (const page of read) console.log(page.numRows);
 * ```
 *
 * @typeParam Head - {@link ItemsHead} or {@link ArtifactsHead}.
 * @category HTTP client
 */
export class RecordsRead<Head> implements AsyncIterableIterator<Table> {
  private at: string | null | undefined;
  private lastEnd: PageEnd | null = null;
  private ended: RecordsTrailer | null = null;
  private stopped = false;
  private readonly pages: AsyncGenerator<Table, undefined, undefined>;

  /** @internal */
  constructor(
    /** The first response's head. */
    readonly head: Head,
    /** The latest response's headers. */
    private headers: Headers,
    private frames: Frames,
    /** The cursor the caller's request carried. */
    from: string | undefined,
    private readonly request: RecordsRequest,
    private readonly signal: AbortSignal | undefined,
    private readonly parseHead: (raw: unknown) => Head
  ) {
    this.at = from;
    this.pages = this.read();
  }

  /** The latest response's identity coordinate for this principal and view; empty where it carried none. */
  get identityKey(): string {
    return this.headers.get('x-tessera-identity-key') ?? '';
  }

  /** The latest response's `x-tessera-region` verdict, present where `filters` carried a `region` leaf. */
  get region(): RegionVerdict | null {
    return parseRegionVerdict(this.headers.get('x-tessera-region'));
  }

  /** The latest response's time from admission to its head, and the time admission took, in microseconds. */
  get timings(): Pick<Timings, 'serverUs' | 'admissionUs'> {
    return {
      serverUs: Number(this.headers.get('x-tessera-server-us') ?? 0),
      admissionUs: Number(this.headers.get('x-tessera-admission-us') ?? 0)
    };
  }

  /** The page end after the page last yielded; `null` before the first. */
  get pageEnd(): PageEnd | null {
    return this.lastEnd;
  }

  /** The last response's trailer once the read has ended, and `null` until then. */
  get trailer(): RecordsTrailer | null {
    return this.ended;
  }

  /**
   * Where the read continues: after the last page yielded, and past any stretch the server scanned
   * beyond it once that response was whole. Before any page it is the caller's own `cursor`, so
   * `undefined` means from the beginning. `null` means no row remains.
   */
  get cursor(): string | null | undefined {
    return this.at;
  }

  /**
   * The next page, or `done` once the read has ended.
   *
   * @throws `Error` for a response cut or ended without its trailer, or whose trailer counts other
   *   pages or rows than it carried, after the pages before it; {@link TesseraError} for a
   *   follow-up request the server refuses; and the signal's reason once it aborts.
   */
  next(): Promise<IteratorResult<Table, undefined>> {
    return this.pages.next();
  }

  /** Stop the read: the response being read is released and no other is requested. */
  return(): Promise<IteratorResult<Table, undefined>> {
    // A generator that never started skips its `finally`, so the body is released here too.
    this.stopped = true;
    this.frames.release();
    return this.pages.return(undefined);
  }

  /** This read, so that `for await` iterates it. */
  [Symbol.asyncIterator](): this {
    return this;
  }

  private async *read(): AsyncGenerator<Table, undefined, undefined> {
    for (;;) {
      const trailer = yield* this.response();
      if (trailer === null) return undefined;
      this.at = trailer.next;
      if (trailer.next === null) {
        this.ended = trailer;
        return undefined;
      }
      if (this.stopped) return undefined;
      const response = await this.request(trailer.next);
      if (this.stopped) {
        void response.body?.cancel().catch(() => {});
        return undefined;
      }
      const {frames} = await open(response, this.signal, this.parseHead);
      if (this.stopped) {
        frames.release();
        return undefined;
      }
      this.frames = frames;
      this.headers = response.headers;
    }
  }

  /** The current response's pages, then its trailer, or `null` where the caller stopped the read. */
  private async *response(): AsyncGenerator<Table, RecordsTrailer | null, undefined> {
    const frames = this.frames;
    let pending: Uint8Array | null = null;
    let trailer: RecordsTrailer | null = null;
    let pages = 0;
    let rows = 0;
    try {
      for (let frame = await frames.next(); frame !== null; frame = await frames.next()) {
        if (frame.kind === FRAME_RECORDS) {
          pending = frame.payload;
        } else if (frame.kind === FRAME_PAGE_END) {
          // The grammar puts a records frame before every page end.
          const table = tableFromIPC(pending!);
          pending = null;
          this.lastEnd = pageEndOf(frame.payload);
          this.at = this.lastEnd.next;
          pages += 1;
          rows += table.numRows;
          yield table;
        } else if (frame.kind === FRAME_TRAILER) {
          trailer = trailerOf(frame.payload);
        }
      }
    } finally {
      frames.release();
    }
    if (frames.released) return null;
    // A complete body has a trailer: the grammar refuses one without.
    if (trailer!.pages !== pages || trailer!.rows !== rows) {
      throw new Error(`the trailer counts ${trailer!.pages} pages and ${trailer!.rows} rows, but the body carried ${pages} and ${rows}`);
    }
    return trailer;
  }
}

/**
 * Start a bulk read: send the caller's request and read its response up to the head. `parseHead`
 * checks and translates a head's JSON. A refusal, a body that fails before its head, and a head
 * `parseHead` refuses each throw. A zstd decoder is registered only where the request asked for
 * `compression: 'zstd'`.
 */
export async function openRecords<Head>(
  given: {cursor?: string; compression?: 'zstd'},
  request: RecordsRequest,
  signal: AbortSignal | undefined,
  parseHead: (raw: unknown) => Head
): Promise<RecordsRead<Head>> {
  if (given.compression === 'zstd') registerZstd();
  const response = await request();
  const {head, frames} = await open(response, signal, parseHead);
  return new RecordsRead(head, response.headers, frames, given.cursor, request, signal, parseHead);
}

/** One response read up to its head. */
async function open<Head>(response: Response, signal: AbortSignal | undefined, parseHead: (raw: unknown) => Head) {
  const frames = new Frames(response.body?.getReader(), signal);
  try {
    // The grammar refuses a body whose first frame is not the head, and one that ends before it.
    const first = (await frames.next())!;
    return {head: parseHead(JSON.parse(new TextDecoder().decode(first.payload))), frames};
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
