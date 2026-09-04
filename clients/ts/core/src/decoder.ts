import {decodeHead, decodePoints, decodeViewport, type PointsPart, type ViewportHead} from './decode.js';
import type {ViewportResult} from './types.js';

/** The head frames of one response, as they came off the wire. */
export type HeadFrames = {tiles: Uint8Array; subCells: Uint8Array | null; artifacts: Uint8Array | null};

/**
 * Where a response is turned into typed arrays.
 *
 * Two implementations behind one interface, because the same code runs in a browser and in Node.
 * `workerDecoder` moves the work off the render thread; `inlineDecoder` is what a test, a script or
 * a runtime without `Worker` gets, and is the behaviour this client had throughout.
 *
 * **The fallback is not a degraded mode to be avoided** — it is correct, just synchronous. A
 * consumer that never draws (the golden capture, a Node script) wants it.
 *
 * Three entry points, and which a caller uses is about *when the bytes arrive*, not about what
 * they mean. {@link decode} takes a whole body; {@link decodeHead} and {@link decodePoints} take
 * a streamed response's frames as they land, so a tile is drawable before the last frame of a
 * hundred-megabyte answer has been received.
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
  /** The counts, the underlay and the artifacts — everything before the first points frame. */
  decodeHead(frames: HeadFrames, background?: boolean): Promise<ViewportHead>;
  /** One kind-3 frame, decoded alone. Frames are independent Arrow streams by contract. */
  decodePoints(frame: Uint8Array, background?: boolean): Promise<PointsPart>;
  /** Release the workers, if there are any. */
  close(): void;
  /**
   * The last reply's own decode time in the worker, in ms — what the caller measured from the
   * outside minus the time the request spent queued in its lane. `null` where nothing was
   * queued or measured (the inline decoder, whose calls *are* the work).
   *
   * Under a streamed response this is per *frame*, so a caller wanting the response's figure
   * accumulates it across the frames it awaited.
   */
  readonly lastWorkerMs: number | null;
};

export function inlineDecoder(): Decoder {
  // Synchronous, so there is no queue to invert and the flag is meaningless here.
  return {
    decode: async (bytes) => decodeViewport(bytes),
    decodeHead: async (frames) => decodeHead(frames),
    decodePoints: async (frame) => decodePoints([frame]),
    close: () => {},
    lastWorkerMs: null
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

/**
 * A buffer the worker may take: the view's own when it owns the whole thing, a copy otherwise.
 *
 * Transferring detaches, so a view onto a larger buffer must be copied out or the caller loses
 * bytes it still holds. A frame the reader assembled across network chunks owns its buffer and
 * transfers at zero copy; one that lay inside a single chunk is copied, which is a memcpy of at
 * most one flush.
 */
function detachable(bytes: Uint8Array): ArrayBuffer {
  const whole = bytes.byteOffset === 0 && bytes.byteLength === bytes.buffer.byteLength;
  return (
    whole ? bytes.buffer : bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength)
  ) as ArrayBuffer;
}

export function workerDecoder(): Decoder | null {
  if (typeof Worker === 'undefined') return null;

  /** One serial lane: a worker, its pending map, and its id counter. */
  let lastWorkerMs: number | null = null;
  function lane(): {send: <T>(request: object, transfer: Transferable[]) => Promise<T>; close: () => void} | null {
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
    const pending = new Map<number, {resolve: (r: never) => void; reject: (e: Error) => void}>();
    worker.onmessage = (event: MessageEvent<{id: number; result?: never; error?: string; ms?: number}>) => {
      const {id, result, error, ms} = event.data;
      const waiter = pending.get(id);
      if (!waiter) return;
      pending.delete(id);
      if (ms !== undefined) lastWorkerMs = ms;
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
      send<T>(request: object, transfer: Transferable[]): Promise<T> {
        // Every buffer named here is transferred, so the caller must not read it afterwards —
        // `client.ts` reads each frame once, here, and never again.
        const id = nextId++;
        return new Promise<T>((resolve, reject) => {
          pending.set(id, {resolve: resolve as (r: never) => void, reject});
          worker.postMessage({id, ...request}, transfer);
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
  //
  // A streamed response's frames are dealt round-robin like anything else, so its own frames
  // decode two at a time; the caller keeps them in order by awaiting them in order.
  const foreground = [lane(), lane()].filter((l) => l !== null);
  if (foreground.length === 0) return null;
  let next = 0;
  // Created on first use: a consumer that never anticipates never pays for the third worker.
  let backgroundLane: ReturnType<typeof lane> | undefined;

  function send<T>(request: object, transfer: Transferable[], background: boolean): Promise<T> {
    if (background) {
      backgroundLane ??= lane();
      if (backgroundLane) return backgroundLane.send<T>(request, transfer);
    }
    next = (next + 1) % foreground.length;
    return foreground[next]!.send<T>(request, transfer);
  }

  return {
    decode(bytes, background = false) {
      const buffer = detachable(bytes);
      return send<ViewportResult>({bytes: buffer}, [buffer], background);
    },
    decodeHead(frames, background = false) {
      const tiles = detachable(frames.tiles);
      const subCells = frames.subCells ? detachable(frames.subCells) : null;
      const artifacts = frames.artifacts ? detachable(frames.artifacts) : null;
      const transfer = [tiles, subCells, artifacts].filter((b) => b !== null);
      return send<ViewportHead>({kind: 'head', tiles, subCells, artifacts}, transfer, background);
    },
    decodePoints(frame, background = false) {
      const buffer = detachable(frame);
      return send<PointsPart>({kind: 'points', bytes: buffer}, [buffer], background);
    },
    close() {
      for (const l of foreground) l.close();
      backgroundLane?.close();
    },
    get lastWorkerMs() {
      return lastWorkerMs;
    }
  };
}

/** A worker when one can be had, the inline decoder otherwise. */
export function createDecoder(): Decoder {
  return workerDecoder() ?? inlineDecoder();
}
