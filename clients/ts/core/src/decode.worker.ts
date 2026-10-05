/**
 * Arrow decoding off the render thread. Decoding a large response takes far longer than the server
 * takes to answer it, and on the render thread it holds up every gesture.
 *
 * Three request shapes: a whole viewport body, from a batch caller; one points frame at a time,
 * from the streaming client, which decodes the counts itself; and one artifacts frame at a time,
 * from `viewportArtifacts`. Each frame is an independent Arrow stream, so they decode separately.
 *
 * A plain worker without `SharedArrayBuffer`: shared memory needs a cross-origin isolated page,
 * which breaks resources without a CORP header and cannot be set on some static hosts. Typed arrays
 * transfer without a copy.
 *
 * No authorisation happens here. The worker parses a response the server has already restricted,
 * and `tessera_id` is opaque on either side.
 */
import {decodeArtifactsFrame, decodePoints, decodeViewport} from './decode.js';
import type {MembershipColumn, ScalarColumn} from './types.js';

export type DecodeRequest =
  /** A whole framed body, trailer included. */
  | {id: number; kind?: 'body'; bytes: ArrayBuffer}
  /** One kind-5 frame of a `/v1/artifacts/viewport` body. */
  | {id: number; kind: 'artifacts'; bytes: ArrayBuffer}
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
    if (column.present) out.push(column.present.buffer);
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
    if (request.kind === 'artifacts') {
      const result = decodeArtifactsFrame(new Uint8Array(request.bytes));
      // The artifacts are plain objects, so they cross by structured clone.
      self.postMessage({id, result, ms: performance.now() - started});
      return;
    }
    if (request.kind === 'points') {
      const decoded = decodePoints([new Uint8Array(request.bytes)]);
      // Cell-space positions and codes stay in the worker: bands use world positions only.
      const result = {...decoded, positions: new Float64Array(0), codes: new BigUint64Array(0)};
      self.postMessage({id, result, ms: performance.now() - started}, {transfer: transferables(result)});
      return;
    }
    const decoded = decodeViewport(new Uint8Array(request.bytes));
    const result = {...decoded, positions: new Float64Array(0), codes: new BigUint64Array(0)};
    // The worker's own time, so the main thread can tell decode time from time queued in the lane.
    const ms = performance.now() - started;
    self.postMessage({id, result, ms}, {transfer: transferables(result)});
  } catch (error) {
    self.postMessage({id, error: error instanceof Error ? error.message : String(error)});
  }
};

// The module has evaluated, so the decoder may send requests and their buffers.
self.postMessage({ready: true});
