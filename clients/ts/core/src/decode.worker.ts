/**
 * Arrow decoding, off the render thread.
 *
 * **Decode is the client's throughput limit, and it was competing with drawing.** The server answers
 * a wide viewport in single-digit milliseconds; turning that answer into typed arrays takes orders
 * longer, and doing it on the render thread means every anticipatory fetch queues behind it the
 * gestures the user is making. Measured on the demo corpus: a background fetch of 2.3 × 10^6 points
 * spent ~2.8 s decoding and absorbing, during which a pan whose own server time was 9 ms and whose
 * own response was 212 KB measured 8.8 s to paint. Nothing was slow but the thread.
 *
 * **Three request shapes, one worker.** A whole body is what a batch caller sends; a *head* (the
 * tiles, sub-cells and artifacts frames) and then one *points* frame at a time is what the
 * streaming client sends, so that each frame is typed arrays by the time the next has landed. The
 * frames are independent Arrow streams by contract (`streamed-serving.md` §2), which is exactly
 * what makes decoding them separately legal.
 *
 * **A plain worker, deliberately — no `SharedArrayBuffer`.** Shared memory would require the page to
 * be cross-origin isolated (`COOP: same-origin`, `COEP: require-corp`), which breaks third-party
 * resources that send no CORP header, interferes with OAuth popups, and cannot be set at all on
 * some static hosts. None of that is needed here: the boundary is bytes in, typed arrays out, and
 * typed arrays transfer at zero copy. So this costs nothing to deploy.
 *
 * **No authorisation happens here**, which is what keeps the worker outside the trust boundary: it
 * parses a response the server has already gated, and `tessera_id` crosses it opaque either way
 * (I10). Moving decode does not move a decision.
 */
import {decodeHead, decodePoints, decodeViewport} from './decode.js';
import type {MembershipColumn, ScalarColumn} from './types.js';

export type DecodeRequest =
  /** A whole framed body, trailer included. */
  | {id: number; kind?: 'body'; bytes: ArrayBuffer}
  /** The frames before the first points frame: tiles, and the two optional ones. */
  | {id: number; kind: 'head'; tiles: ArrayBuffer; subCells: ArrayBuffer | null; artifacts: ArrayBuffer | null}
  /** One kind-3 frame. */
  | {id: number; kind: 'points'; bytes: ArrayBuffer};

/** Every buffer in a decoded point block, so the reply transfers rather than copies. */
function transferables(result: {
  ids: BigUint64Array;
  world: Float32Array;
  scalars: Record<string, ScalarColumn>;
  membership: Record<string, MembershipColumn>;
}): Transferable[] {
  const out: Transferable[] = [result.ids.buffer, result.world.buffer];
  for (const column of Object.values(result.scalars)) {
    const values = column.values as unknown;
    if (ArrayBuffer.isView(values)) out.push((values as ArrayBufferView).buffer);
  }
  for (const column of Object.values(result.membership)) out.push(column.index.buffer, column.ids.buffer);
  // A buffer listed twice is a `DataCloneError`, and Arrow columns can share one.
  return [...new Set(out)];
}

self.onmessage = (event: MessageEvent<DecodeRequest>) => {
  const request = event.data;
  const {id} = request;
  try {
    const started = performance.now();
    if (request.kind === 'head') {
      const result = decodeHead({
        tiles: new Uint8Array(request.tiles),
        subCells: request.subCells ? new Uint8Array(request.subCells) : null,
        artifacts: request.artifacts ? new Uint8Array(request.artifacts) : null
      });
      // Nothing here is a typed array the main thread reads twice — the head is objects — so it
      // crosses by structured clone like the batch reply's tiles and artifacts always have.
      self.postMessage({id, result, ms: performance.now() - started});
      return;
    }
    if (request.kind === 'points') {
      const decoded = decodePoints([new Uint8Array(request.bytes)]);
      // **Cell space and the codes stay in the worker.** Nothing downstream reads them — a band
      // holds world positions and the region queries work there — so shipping them would double
      // the bytes crossing the boundary for no reader.
      const result = {...decoded, positions: new Float64Array(0), codes: new BigUint64Array(0)};
      self.postMessage({id, result, ms: performance.now() - started}, {transfer: transferables(result)});
      return;
    }
    const decoded = decodeViewport(new Uint8Array(request.bytes));
    const result = {...decoded, positions: new Float64Array(0), codes: new BigUint64Array(0)};
    // The worker's own time, bytes in to arrays out — so the main thread can tell decode from
    // the time a response spent queued behind another in this lane (design §5.10's measurement).
    const ms = performance.now() - started;
    self.postMessage({id, result, ms}, {transfer: transferables(result)});
  } catch (error) {
    self.postMessage({id, error: error instanceof Error ? error.message : String(error)});
  }
};
