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
  /**
   * `background` routes speculative work to its own lane where the implementation has one.
   *
   * **A foreground response must never queue behind an anticipatory one.** The worker decoder is
   * serial per worker, and anticipation moves multi-megabyte responses — so one shared lane is a
   * priority inversion: the bytes the user is waiting on sit behind bytes nobody asked for yet.
   * Two workers, one per lane, and the flag is the routing.
   */
  decode(bytes: Uint8Array, background?: boolean): Promise<ViewportResult>;
  /** Release the workers, if there are any. */
  close(): void;
};

export function inlineDecoder(): Decoder {
  // Synchronous, so there is no queue to invert and the flag is meaningless here.
  return {
    decode: async (bytes) => decodeViewport(bytes),
    close: () => {}
  };
}

/**
 * Decode in workers: two foreground lanes and a lazy background one.
 *
 * Each lane is serial — what matters is that the work is off the *render* thread and that a
 * split response's pieces can overlap, not that decoding is wide. See the lane notes below for
 * why two and not more.
 *
 * Returns `null` where `Worker` is unavailable, so the caller falls back rather than failing.
 */
/**
 * How the worker is made, when a bundle cannot resolve a relative worker file.
 *
 * The default is `new URL('./decode.worker.js', import.meta.url)`, which every bundler that
 * serves the package as files understands. A single-file distribution has no file to point at
 * — the built decoder would fall back to the main thread silently, at tens of milliseconds a
 * response — so it inlines the worker (a Blob URL, with a data URL where blob workers are
 * refused) and installs the factory here before any decoder is built (design §5.9).
 */
let workerFactory: (() => Worker) | null = null;

export function setWorkerFactory(factory: (() => Worker) | null): void {
  workerFactory = factory;
}

export function workerDecoder(): Decoder | null {
  if (typeof Worker === 'undefined') return null;

  /** One serial lane: a worker, its pending map, and its id counter. */
  function lane(): {decode: (bytes: Uint8Array) => Promise<ViewportResult>; close: () => void} | null {
    let worker: Worker;
    try {
      worker = workerFactory
        ? workerFactory()
        : new Worker(new URL('./decode.worker.js', import.meta.url), {type: 'module'});
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
      // A worker that has died cannot answer anything outstanding, and leaving those promises
      // pending would hang every caller rather than surfacing the failure.
      const failure = new Error(`decode worker failed: ${event.message}`);
      for (const waiter of pending.values()) waiter.reject(failure);
      pending.clear();
    };
    return {
      decode(bytes) {
        // The buffer is transferred, so the caller must not read it afterwards — `client.ts` reads
        // it once, here, and never again. When the view owns its whole buffer — the fetch path
        // always does, its bytes coming straight from `response.arrayBuffer()` — the transfer is
        // zero-copy; the slice exists only for a view into a larger buffer, where transferring
        // would detach bytes the caller still holds.
        const whole = bytes.byteOffset === 0 && bytes.byteLength === bytes.buffer.byteLength;
        const buffer = (
          whole ? bytes.buffer : bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength)
        ) as ArrayBuffer;
        const id = nextId++;
        return new Promise<ViewportResult>((resolve, reject) => {
          pending.set(id, {resolve, reject});
          worker.postMessage({id, bytes: buffer}, [buffer]);
        });
      },
      close() {
        worker.terminate();
        for (const waiter of pending.values()) waiter.reject(new Error('decoder closed'));
        pending.clear();
      }
    };
  }

  // **Two foreground lanes, round-robin.** A split viewport response arrives as several pieces,
  // and painting pieces as they land only helps if their decodes overlap — one serial lane made
  // piece 2 wait out piece 1's ~100-300 ms. Two is deliberate: decode is CPU-bound, so a wide pool
  // buys parallelism the cores may not have while multiplying peak transferred memory.
  const foreground = [lane(), lane()].filter((l) => l !== null);
  if (foreground.length === 0) return null;
  let next = 0;
  // Created on first use: a consumer that never anticipates never pays for the third worker.
  let backgroundLane: ReturnType<typeof lane> | undefined;

  return {
    decode(bytes, background = false) {
      if (background) {
        backgroundLane ??= lane();
        if (backgroundLane) return backgroundLane.decode(bytes);
      }
      next = (next + 1) % foreground.length;
      return foreground[next]!.decode(bytes);
    },
    close() {
      for (const l of foreground) l.close();
      backgroundLane?.close();
    }
  };
}

/** A worker when one can be had, the inline decoder otherwise. */
export function createDecoder(): Decoder {
  return workerDecoder() ?? inlineDecoder();
}
