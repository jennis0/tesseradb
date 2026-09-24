/**
 * Split a framed response into its frames.
 *
 * A body is a sequence of `u8 kind, u32 LE payload length, <payload>`. A `/v1/viewport` body:
 *
 *       kind 1  tiles      Arrow IPC stream: tile, visible, matched, served (all uint64)
 *       kind 2  sub-cells  Arrow IPC stream: cell uint64, count uint64; present when the
 *                          underlay was requested, schema-only when it is empty
 *       kind 3  points     Arrow IPC stream: tessera_id uint64, code uint64, ...scalars;
 *                          zero or more frames, concatenating to the full points stream
 *       kind 4  trailer    JSON; exactly one, last
 *       kind 5  artifacts  Arrow IPC stream: the fixed columns `layer` (dictionary u16/utf8)
 *                          through `matched`, then hull columns when a served layer declares
 *                          one, or the identity projection's columns; at most one, after tiles
 *                          and before any points frame, absent when the response served none
 *
 * A `/v1/items` or `/v1/artifacts` body:
 *
 *       kind 6  head       JSON; exactly one, first
 *       kind 7  records    Arrow IPC stream of one page; zero or more, each followed by a page end
 *       kind 8  page end   JSON: the cursor to resume after the page before it
 *       kind 4  trailer    JSON; exactly one, last
 *
 * Every failure throws: a truncated body, an unknown kind or a frame out of place never decodes
 * to a shorter response. A body without its trailer is incomplete, since the server ends a body
 * that fails part-way without one.
 */
export type FramedStreams = {
  tiles: Uint8Array;
  /** One entry per kind-3 frame, in arrival order — decode each alone, concatenate the rows. */
  points: Uint8Array[];
  subCells: Uint8Array | null;
  /**
   * The kind-5 artifacts payload, or `null` when the response served none.
   *
   * `null` and an empty table are the same fact here — the server omits the frame rather than
   * sending an empty one, so a deployment with no layers pays nothing for the channel — which is
   * why this does not carry the request/result distinction {@link subCells} does.
   */
  artifacts: Uint8Array | null;
  /** The kind-4 trailer's raw JSON bytes. */
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

/** Which body a reader expects: a viewport's, or a bulk read's from `/v1/items` or `/v1/artifacts`. */
export type FrameGrammar = 'viewport' | 'records';

const FRAME_HEADER_BYTES = 5;

/** One frame off the wire: its kind, and its payload bytes. */
export type Frame = {kind: number; payload: Uint8Array};

/**
 * The frame grammar, over a body that may arrive in any number of pieces. It is the only reader
 * of frames in this client: {@link splitFramedStreams} pushes it a whole body, and the streaming
 * paths push it each network chunk.
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
   * No more bytes are coming: what is left must be nothing, and what arrived must be a whole
   * response.
   *
   * A response missing its trailer is incomplete whatever the transport said. What a caller has
   * already taken from it stays sound; this throw denies it only a complete response.
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
      if (!this.sawTrailer) throw new Error('records payload has no trailer: the response is incomplete');
      return;
    }
    if (!this.sawTiles) throw new Error('viewport payload has no tiles frame');
    if (!this.sawTrailer) {
      throw new Error('viewport payload has no trailer: the response is incomplete');
    }
  }

  /** Whether a whole response has been read — the trailer's presence, which is the signal. */
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
 * Split a whole body into its frames — {@link FrameReader} over one chunk.
 *
 * Every payload is a view onto `buf` rather than a copy, which is what a batch decoder wants; the
 * streaming path takes the same frames one network chunk at a time.
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
