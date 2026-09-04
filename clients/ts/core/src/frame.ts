/**
 * Split a `/v1/viewport` response into its frames.
 *
 * The wire frame (contracts §3.2 r26; `tessera-wire`'s `payload` module doc) is a sequence of
 * tagged, length-prefixed frames — every frame prefixed, which is client-interaction §8.6(2)'s
 * owner-annotated item and what retired this file's previous Arrow-message boundary walk and the
 * padding-arithmetic desynchronisation bug it documented:
 *
 *     u8 kind, u32 LE payload length, <payload>     -- repeated
 *       kind 1  tiles      Arrow IPC stream: tile, visible, matched, served (all uint64)
 *       kind 2  sub-cells  Arrow IPC stream: cell uint64, count uint64 — present iff the
 *                          underlay was requested (schema-only when requested-but-empty)
 *       kind 3  points     Arrow IPC stream: tessera_id uint64, code uint64, ...scalars —
 *                          zero or more frames, concatenating to the full points stream
 *       kind 4  trailer    JSON; exactly one, last — its presence marks the response complete
 *       kind 5  artifacts  Arrow IPC stream: the fourteen fixed columns `layer` (dictionary
 *                          u16/utf8) through `matched`, hull columns trailing when a served
 *                          layer declares one — or the identity projection's four (contracts
 *                          §3.2 r44) — at most one, after tiles and before any points frame;
 *                          ABSENT when the response served none
 *
 * Every failure here throws, and strictly: a truncated body, an unknown kind, a missing trailer
 * or a misplaced tiles frame must never decode to a plausible shorter response — a sample
 * silently standing in for the set is the one failure mode this client exists to make
 * impossible. A response missing its trailer is incomplete BY CONTRACT, whatever the transport
 * said (the server aborts mid-body streams without one).
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

const FRAME_HEADER_BYTES = 5;

/** One frame off the wire: its kind, and its payload bytes. */
export type Frame = {kind: number; payload: Uint8Array};

/**
 * The frame grammar, enforced once, over a body that may arrive in any number of pieces.
 *
 * **This is the only statement of the grammar in this client.** {@link splitFramedStreams} is this
 * reader pushed a whole body; the streaming path in `client.ts` is this reader pushed each network
 * chunk. Two readers would be two chances to disagree about what a legal response is, and the
 * disagreement would be silent in exactly the direction that matters — a body one accepts and the
 * other refuses is a body whose points were drawn by one code path and not the other.
 *
 * Chunk boundaries carry no meaning: a frame's five-byte header may be split across three chunks
 * and its payload across a hundred, and the reader emits the frame only when it is whole. A frame
 * that lies inside a single chunk is handed over as a view onto it; one that spans chunks is
 * assembled into a buffer of its own, so no frame is ever copied more than once.
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
   * **A response missing its trailer is incomplete BY CONTRACT** (`streamed-serving.md` §6),
   * whatever the transport said — the server aborts a mid-stream failure without one, and a reader
   * that accepted the prefix as an answer would present a sample as the set. The delivered prefix
   * is still sound and a caller may keep what it has already landed; what it must not do is call
   * the response complete, which is what this throw denies it.
   */
  end(): void {
    if (this.queued > 0) {
      if (this.queued < FRAME_HEADER_BYTES) {
        throw new Error(`truncated frame header at byte ${this.consumed}`);
      }
      throw new Error(`frame at byte ${this.consumed} claims a payload past the end of the body`);
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
