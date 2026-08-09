import {decodeViewport} from './decode.js';
import type {ViewportResult} from './types.js';

/**
 * Where a response is turned into typed arrays.
 *
 * Two implementations behind one interface, because the same code runs in a browser and in Node.
 * `workerDecoder` moves the work off the render thread; `inlineDecoder` is what a test, a script or
 * a runtime without `Worker` gets, and is the behaviour this client had throughout.
 *
 * **The fallback is not a degraded mode to be avoided** — it is correct, just synchronous. A
 * consumer that never draws (the golden capture, a Node script) wants it.
 */
export type Decoder = {
  decode(bytes: Uint8Array): Promise<ViewportResult>;
  /** Release the worker, if there is one. */
  close(): void;
};

export function inlineDecoder(): Decoder {
  return {
    decode: async (bytes) => decodeViewport(bytes),
    close: () => {}
  };
}

/**
 * Decode in a worker, one request at a time.
 *
 * **Serialised rather than pooled.** Decoding is CPU-bound, so a pool would only help on a machine
 * with cores to spare and would multiply peak memory by its size; what matters here is that the
 * work is off the *render* thread, not that it is parallel. One worker also keeps ordering trivial.
 *
 * Returns `null` where `Worker` is unavailable, so the caller falls back rather than failing.
 */
export function workerDecoder(): Decoder | null {
  if (typeof Worker === 'undefined') return null;

  let worker: Worker;
  try {
    worker = new Worker(new URL('./decode.worker.js', import.meta.url), {type: 'module'});
  } catch {
    // A bundler that cannot resolve the worker URL, or a runtime that forbids module workers.
    return null;
  }

  let nextId = 1;
  const pending = new Map<number, {resolve: (r: ViewportResult) => void; reject: (e: Error) => void}>();

  worker.onmessage = (event: MessageEvent<{id: number; result?: ViewportResult; error?: string}>) => {
    const {id, result, error} = event.data;
    const waiter = pending.get(id);
    if (!waiter) return;
    pending.delete(id);
    if (error !== undefined) waiter.reject(new Error(error));
    else waiter.resolve(result!);
  };
  worker.onerror = (event) => {
    // A worker that has died cannot answer anything outstanding, and leaving those promises pending
    // would hang every caller rather than surfacing the failure.
    const failure = new Error(`decode worker failed: ${event.message}`);
    for (const waiter of pending.values()) waiter.reject(failure);
    pending.clear();
  };

  return {
    decode(bytes) {
      // The buffer is transferred, so the caller must not read it afterwards — `client.ts` reads it
      // once, here, and never again.
      const copy = bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
      const id = nextId++;
      return new Promise<ViewportResult>((resolve, reject) => {
        pending.set(id, {resolve, reject});
        worker.postMessage({id, bytes: copy}, [copy]);
      });
    },
    close() {
      worker.terminate();
      for (const waiter of pending.values()) waiter.reject(new Error('decoder closed'));
      pending.clear();
    }
  };
}

/** A worker when one can be had, the inline decoder otherwise. */
export function createDecoder(): Decoder {
  return workerDecoder() ?? inlineDecoder();
}
