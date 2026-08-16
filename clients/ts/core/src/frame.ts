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
 *       kind 5  artifacts  Arrow IPC stream: layer utf8, tessera_id uint64, stable_key utf8
 *                          (nullable), masked_count uint64 — at most one, after tiles and before
 *                          any points frame; ABSENT when the response served none
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

export function splitFramedStreams(buf: Uint8Array): FramedStreams {
  const view = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
  let tiles: Uint8Array | null = null;
  const points: Uint8Array[] = [];
  let subCells: Uint8Array | null = null;
  let artifacts: Uint8Array | null = null;
  let trailer: Uint8Array | null = null;

  let at = 0;
  while (at < buf.byteLength) {
    if (at + FRAME_HEADER_BYTES > buf.byteLength) {
      throw new Error(`truncated frame header at byte ${at}`);
    }
    const kind = view.getUint8(at);
    const length = view.getUint32(at + 1, true);
    const start = at + FRAME_HEADER_BYTES;
    const end = start + length;
    if (end > buf.byteLength) {
      throw new Error(`frame at byte ${at} claims a payload past the end of the body`);
    }
    const payload = buf.subarray(start, end);
    switch (kind) {
      case FRAME_TILES:
        if (tiles) throw new Error('more than one tiles frame');
        if (at !== 0) throw new Error('the tiles frame must be first');
        tiles = payload;
        break;
      case FRAME_SUB_CELLS:
        if (subCells) throw new Error('more than one sub-cells frame');
        if (points.length > 0 || trailer) {
          throw new Error('the sub-cells frame must immediately follow tiles');
        }
        subCells = payload;
        break;
      case FRAME_ARTIFACTS:
        if (artifacts) throw new Error('more than one artifacts frame');
        if (points.length > 0) {
          throw new Error('the artifacts frame must precede every points frame');
        }
        artifacts = payload;
        break;
      case FRAME_POINTS:
        points.push(payload);
        break;
      case FRAME_TRAILER:
        if (trailer) throw new Error('more than one trailer frame');
        trailer = payload;
        break;
      default:
        // Refused, never skipped: skipping would let a future frame kind carry data an old
        // reader silently drops.
        throw new Error(`unknown frame kind ${kind} at byte ${at}`);
    }
    at = end;
  }

  if (!tiles) throw new Error('viewport payload has no tiles frame');
  if (!trailer) {
    throw new Error('viewport payload has no trailer: the response is incomplete');
  }
  return {tiles, points, subCells, artifacts, trailer};
}
