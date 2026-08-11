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
import {decodeViewport} from './decode.js';
import type {ScalarColumn} from './types.js';

export type DecodeRequest = {id: number; bytes: ArrayBuffer};

/** Every buffer in a decoded result, so the reply transfers rather than copies. */
function transferables(result: {
  ids: BigUint64Array;
  world: Float32Array;
  scalars: Record<string, ScalarColumn>;
}): Transferable[] {
  const out: Transferable[] = [result.ids.buffer, result.world.buffer];
  for (const column of Object.values(result.scalars)) {
    const values = column.values as unknown;
    if (ArrayBuffer.isView(values)) out.push((values as ArrayBufferView).buffer);
  }
  // A buffer listed twice is a `DataCloneError`, and Arrow columns can share one.
  return [...new Set(out)];
}

self.onmessage = (event: MessageEvent<DecodeRequest>) => {
  const {id, bytes} = event.data;
  try {
    const decoded = decodeViewport(new Uint8Array(bytes));
    // **Cell space and the codes stay in the worker.** Nothing downstream reads them — a band holds
    // world positions and the region queries work there — so shipping them would double the bytes
    // crossing the boundary for no reader.
    const result = {...decoded, positions: new Float64Array(0), codes: new BigUint64Array(0)};
    self.postMessage({id, result}, {transfer: transferables(result)});
  } catch (error) {
    self.postMessage({id, error: error instanceof Error ? error.message : String(error)});
  }
};
