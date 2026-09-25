/**
 * A `/v1/viewport` body split into its frames' payloads, as {@link splitFramedStreams} returns
 * them. The body is a sequence of frames, each a `u8` kind, a `u32` little-endian payload length
 * and the payload:
 *
 * - Kind 1, tiles: an Arrow IPC stream of `tile`, `visible`, `matched`, `served` and
 *   `highlighted`, all `uint64`. Exactly one, first.
 * - Kind 2, sub-cells: an Arrow IPC stream of `cell` and `count`, both `uint64`. Present only where
 *   the request asked for an underlay, and schema-only where it asked and no cell had a count.
 * - Kind 5, artifacts: an Arrow IPC stream with one row per served artifact, in the full
 *   projection or the five-column identity projection. At most one, after the tiles frame and
 *   before any points frame. Absent where the response serves no artifact.
 * - Kind 3, points: an Arrow IPC stream of `tessera_id` and `code`, both `uint64`, then the
 *   rendered columns, a `highlighted` column where the request carried a highlight, and a
 *   `membership:<layer>` column per layer the response names; a request for the highlight
 *   projection gets `tessera_id` and `highlighted` alone. Zero or more, each holding whole tiles;
 *   their rows, in order, are the response's points.
 * - Kind 4, trailer: JSON, exactly one, last. A body without it is incomplete.
 *
 * @category HTTP client
 */
export type FramedStreams = {
  /** The kind-1 tiles payload. */
  tiles: Uint8Array;
  /**
   * One payload per kind-3 points frame, in the order received. Each is a complete Arrow stream:
   * decode each alone and concatenate the rows.
   */
  points: Uint8Array[];
  /** The kind-2 sub-cells payload, or `null` where the request asked for no underlay. */
  subCells: Uint8Array | null;
  /**
   * The kind-5 artifacts payload, or `null` where the response serves no artifact. The server
   * omits the frame instead of sending it empty, so `null` is the empty set.
   */
  artifacts: Uint8Array | null;
  /** The kind-4 trailer's JSON bytes. */
  trailer: Uint8Array;
};

export const FRAME_TILES = 1;
export const FRAME_SUB_CELLS = 2;
export const FRAME_POINTS = 3;
export const FRAME_TRAILER = 4;
export const FRAME_ARTIFACTS = 5;
export const FRAME_RECORDS_HEAD = 6;
export const FRAME_RECORDS = 7;
export const FRAME_PAGE_END = 8;

/**
 * Which body a reader expects. `'viewport'` is a `/v1/viewport` body, whose frames
 * {@link FramedStreams} lists. `'records'` is a `/v1/items` or `/v1/artifacts` body:
 *
 * - Kind 6, head: JSON, exactly one, first.
 * - Kind 7, records: an Arrow IPC stream of one page, of no rows where the response found none.
 *   One or more, each followed by a page end. None only in a response cancelled before its first
 *   page, by the stream deadline or because the client went away.
 * - Kind 8, page end: JSON carrying the cursor to resume after the page before it.
 * - Kind 4, trailer: JSON, exactly one, last. A body without it is incomplete.
 */
export type FrameGrammar = 'viewport' | 'records';

const FRAME_HEADER_BYTES = 5;

/** One frame off the wire: its kind, and its payload bytes. */
export type Frame = {kind: number; payload: Uint8Array};

/**
 * The frame grammar, over a body that may arrive in any number of pieces. It is the client's one
 * reader of frames: {@link splitFramedStreams} pushes it a whole body, and the streaming paths push
 * it each network chunk, so every path accepts the same bodies. Every failure throws, so a
 * truncated body, an unknown kind or a frame out of place never decodes to a shorter response.
 *
 * Chunk boundaries carry no meaning: a frame's five-byte header may be split across three chunks
 * and its payload across a hundred, and the reader emits the frame only when it is whole. A frame
 * that lies inside a single chunk is handed over as a view onto it; one that spans chunks is
 * assembled into a buffer of its own, so no frame is copied more than once.
 */
export class FrameReader {
  /** Chunks pushed and not yet consumed, oldest first. */
  private queue: Uint8Array[] = [];
  private queued = 0;
  /** Bytes of complete frames taken, which is the offset the grammar's errors are reported at. */
  private consumed = 0;
  private frames = 0;
  private sawTiles = false;
  private sawSubCells = false;
  private sawArtifacts = false;
  private sawPoints = false;
  private sawTrailer = false;
  /** A records frame has arrived and its page end has not. */
  private openPage = false;

  constructor(private readonly grammar: FrameGrammar = 'viewport') {}

  /** Every complete frame the pushed bytes finish, in wire order. */
  push(chunk: Uint8Array): Frame[] {
    if (chunk.byteLength > 0) {
      this.queue.push(chunk);
      this.queued += chunk.byteLength;
    }
    const out: Frame[] = [];
    for (;;) {
      if (this.queued < FRAME_HEADER_BYTES) break;
      const header = this.peek(FRAME_HEADER_BYTES);
      const kind = header[0]!;
      const length = new DataView(header.buffer, header.byteOffset, FRAME_HEADER_BYTES).getUint32(
        1,
        true
      );
      if (this.queued < FRAME_HEADER_BYTES + length) break;
      this.take(FRAME_HEADER_BYTES);
      const payload = this.take(length);
      this.check(kind);
      this.consumed += FRAME_HEADER_BYTES + length;
      this.frames += 1;
      out.push({kind, payload});
    }
    return out;
  }

  /**
   * No more bytes are coming. Throws unless nothing is left over and what arrived is a whole
   * response.
   *
   * A response without its trailer is incomplete whatever the transport said, since the server
   * ends a failed stream without one. A reader that took the frames received as the answer would
   * present a sample as the set. The frames already delivered are correct and a caller may keep
   * them, but the response is not complete.
   */
  end(): void {
    if (this.queued > 0) {
      if (this.queued < FRAME_HEADER_BYTES) {
        throw new Error(`truncated frame header at byte ${this.consumed}`);
      }
      throw new Error(`frame at byte ${this.consumed} claims a payload past the end of the body`);
    }
    if (this.grammar === 'records') {
      if (this.frames === 0) throw new Error('records payload has no head frame');
      if (!this.sawTrailer) throw new Error('records payload has no trailer: the response is incomplete; resume the read from its cursor');
      return;
    }
    if (!this.sawTiles) throw new Error('viewport payload has no tiles frame');
    if (!this.sawTrailer) {
      throw new Error('viewport payload has no trailer: the response is incomplete');
    }
  }

  /** Whether a whole response has been read, which the trailer's arrival marks. */
  get complete(): boolean {
    return this.sawTrailer;
  }

  private check(kind: number): void {
    if (this.grammar === 'records') {
      this.checkRecords(kind);
      return;
    }
    switch (kind) {
      case FRAME_TILES:
        if (this.sawTiles) throw new Error('more than one tiles frame');
        if (this.frames !== 0) throw new Error('the tiles frame must be first');
        this.sawTiles = true;
        break;
      case FRAME_SUB_CELLS:
        if (this.sawSubCells) throw new Error('more than one sub-cells frame');
        if (this.sawPoints || this.sawTrailer) {
          throw new Error('the sub-cells frame must immediately follow tiles');
        }
        this.sawSubCells = true;
        break;
      case FRAME_ARTIFACTS:
        if (this.sawArtifacts) throw new Error('more than one artifacts frame');
        if (this.sawPoints) throw new Error('the artifacts frame must precede every points frame');
        this.sawArtifacts = true;
        break;
      case FRAME_POINTS:
        // Checked here rather than only at the end, because a streaming reader decodes this frame
        // now: without the tiles batch there is nothing to attribute its points to, and a reader
        // that discovered the absence at the trailer would already have drawn them.
        if (!this.sawTiles) throw new Error('a points frame before the tiles frame');
        this.sawPoints = true;
        break;
      case FRAME_TRAILER:
        if (this.sawTrailer) throw new Error('more than one trailer frame');
        this.sawTrailer = true;
        break;
      default:
        // Refused, never skipped: skipping would let a future frame kind carry data an old
        // reader silently drops.
        throw new Error(`unknown frame kind ${kind} at byte ${this.consumed}`);
    }
  }

  /**
   * A bulk read's grammar. A records frame stays open until its page end arrives, so a body cut
   * between the two ends with a page open, and the caller discards that page.
   */
  private checkRecords(kind: number): void {
    const at = this.consumed;
    if (this.sawTrailer) throw new Error(`a frame after the trailer at byte ${at}`);
    if (this.frames === 0 && kind !== FRAME_RECORDS_HEAD) throw new Error('the head frame must be first');
    switch (kind) {
      case FRAME_RECORDS_HEAD:
        if (this.frames !== 0) throw new Error(`a second head frame at byte ${at}`);
        break;
      case FRAME_RECORDS:
        if (this.openPage) throw new Error(`a records frame at byte ${at} before the page end of the one before it`);
        this.openPage = true;
        break;
      case FRAME_PAGE_END:
        if (!this.openPage) throw new Error(`a page end at byte ${at} with no records frame before it`);
        this.openPage = false;
        break;
      case FRAME_TRAILER:
        if (this.openPage) throw new Error(`the trailer at byte ${at} follows a records frame with no page end`);
        this.sawTrailer = true;
        break;
      default:
        throw new Error(`unknown frame kind ${kind} at byte ${at}`);
    }
  }

  /** The next `n` queued bytes, without consuming them. Copies only across a chunk boundary. */
  private peek(n: number): Uint8Array {
    const first = this.queue[0]!;
    if (first.byteLength >= n) return first.subarray(0, n);
    const out = new Uint8Array(n);
    let at = 0;
    for (const chunk of this.queue) {
      const take = Math.min(n - at, chunk.byteLength);
      out.set(chunk.subarray(0, take), at);
      at += take;
      if (at === n) break;
    }
    return out;
  }

  /** Consume the next `n` bytes. A view onto one chunk where it can be, a fresh buffer where not. */
  private take(n: number): Uint8Array {
    // A zero-length payload is legal — a schema-only Arrow stream is not zero bytes, but a
    // trailing kind whose length is 0 is well-framed and must reach {@link check} to be refused.
    if (n === 0) return new Uint8Array(0);
    this.queued -= n;
    const first = this.queue[0]!;
    if (first.byteLength >= n) {
      const out = first.subarray(0, n);
      if (first.byteLength === n) this.queue.shift();
      else this.queue[0] = first.subarray(n);
      return out;
    }
    const out = new Uint8Array(n);
    let at = 0;
    while (at < n) {
      const chunk = this.queue[0]!;
      const take = Math.min(n - at, chunk.byteLength);
      out.set(chunk.subarray(0, take), at);
      at += take;
      if (chunk.byteLength === take) this.queue.shift();
      else this.queue[0] = chunk.subarray(take);
    }
    return out;
  }
}

/**
 * Splits a whole `/v1/viewport` body into its frames' payloads. Each payload is a view onto `buf`;
 * nothing is copied.
 *
 * @throws `Error` for a body that is truncated, lacks its tiles frame or its trailer, or has a
 *   frame of unknown kind, out of order or repeated.
 *
 * @category HTTP client
 */
export function splitFramedStreams(buf: Uint8Array): FramedStreams {
  const reader = new FrameReader();
  let tiles: Uint8Array | null = null;
  const points: Uint8Array[] = [];
  let subCells: Uint8Array | null = null;
  let artifacts: Uint8Array | null = null;
  let trailer: Uint8Array | null = null;
  for (const frame of reader.push(buf)) {
    switch (frame.kind) {
      case FRAME_TILES:
        tiles = frame.payload;
        break;
      case FRAME_SUB_CELLS:
        subCells = frame.payload;
        break;
      case FRAME_ARTIFACTS:
        artifacts = frame.payload;
        break;
      case FRAME_POINTS:
        points.push(frame.payload);
        break;
      case FRAME_TRAILER:
        trailer = frame.payload;
        break;
    }
  }
  reader.end();
  return {tiles: tiles!, points, subCells, artifacts, trailer: trailer!};
}
