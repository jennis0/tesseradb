import {Message} from 'apache-arrow';

/**
 * Split a `/v1/viewport` response into its concatenated Arrow IPC streams.
 *
 * The wire frame (see `tessera-wire`'s `payload` module doc) is:
 *
 *     u32 LE  byte length of the tile stream
 *     <tile stream>       Arrow IPC stream: tile, visible, matched, served (all uint64)
 *     <points stream>     Arrow IPC stream: tessera_id uint64, x float32, y float32, ...scalars
 *     <sub-cell stream>   Arrow IPC stream: cell uint64, count uint64 — ABSENT ENTIRELY
 *                         (zero bytes) unless the underlay was requested
 *
 * Only the tile boundary carries a length prefix; the server documents that relaxation
 * deliberately, so the points stream's end must be found by walking its IPC messages. That walk is
 * what {@link streamLength} does, and it is why this file parses framing rather than calling
 * `tableFromIPC` and hoping.
 *
 * Every failure here throws. A short read would decode to fewer points than the tile batch's
 * `served` promised — a sample silently standing in for the set, which is the one failure mode
 * this client exists to make impossible.
 */
export type FramedStreams = {
  tiles: Uint8Array;
  points: Uint8Array;
  subCells: Uint8Array | null;
};

const CONTINUATION = 0xffffffff;

/**
 * Byte length of the single Arrow IPC stream beginning at `offset`, including its end-of-stream
 * marker.
 *
 * Walks encapsulated messages: continuation (u32 `0xffffffff`), metadata length (u32), the
 * metadata flatbuffer, then a body whose length only that flatbuffer knows — which is why this
 * decodes the message header rather than scanning for a byte pattern. Scanning would be shorter
 * and wrong: `ff ff ff ff 00 00 00 00` is a legal run of bytes inside a uint64 column.
 *
 * **No alignment arithmetic, and that is measured rather than assumed.** Arrow pads both metadata
 * and bodies to 8 bytes, but the writer folds that padding into the values it reports: against the
 * captured golden the schema message is `metadataLength = 248` at offset 1164 and the next message
 * begins at 1420, which is `8 + 248` later and is *not* itself 8-aligned. Rounding either figure
 * up here over-advances by 4 bytes and the walk desynchronises on the second message.
 */
export function streamLength(buf: Uint8Array, offset: number): number {
  const view = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
  let at = offset;
  for (;;) {
    if (at + 8 > buf.byteLength) {
      throw new Error(`truncated Arrow stream: no end-of-stream marker before byte ${at}`);
    }
    const continuation = view.getUint32(at, true);
    if (continuation !== CONTINUATION) {
      throw new Error(`bad Arrow continuation 0x${continuation.toString(16)} at byte ${at}`);
    }
    const metadataLength = view.getUint32(at + 4, true);
    if (metadataLength === 0) return at + 8 - offset; // end-of-stream marker
    const metadataEnd = at + 8 + metadataLength;
    if (metadataEnd > buf.byteLength) {
      throw new Error(`truncated Arrow message metadata at byte ${at}`);
    }
    const bodyLength = Message.decode(buf.subarray(at + 8, metadataEnd)).bodyLength;
    at = metadataEnd + bodyLength;
    if (at > buf.byteLength) {
      throw new Error(`truncated Arrow message body: needs ${at} bytes, have ${buf.byteLength}`);
    }
  }
}

export function splitFramedStreams(buf: Uint8Array): FramedStreams {
  if (buf.byteLength < 4) {
    throw new Error(`viewport payload is ${buf.byteLength} bytes: too short for its length prefix`);
  }
  const view = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
  const tileLength = view.getUint32(0, true);
  if (4 + tileLength > buf.byteLength) {
    throw new Error(
      `viewport payload claims a ${tileLength}-byte tile stream but is ${buf.byteLength} bytes`
    );
  }
  const tiles = buf.subarray(4, 4 + tileLength);

  const pointsStart = 4 + tileLength;
  const pointsLength = streamLength(buf, pointsStart);
  const points = buf.subarray(pointsStart, pointsStart + pointsLength);

  // Absent, not empty: a request that did not ask for the underlay produces zero trailing bytes,
  // so this payload is byte-identical to a pre-underlay one.
  const subStart = pointsStart + pointsLength;
  const subCells = subStart >= buf.byteLength ? null : buf.subarray(subStart);
  return {tiles, points, subCells};
}
