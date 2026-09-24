import {decodeHead, decodePoints, decodeViewport, type PointsPart, type ViewportHead} from './decode.js';
import type {ViewportResult} from './types.js';

/** The head frames of one response, as they came off the wire. */
export type HeadFrames = {tiles: Uint8Array; subCells: Uint8Array | null; artifacts: Uint8Array | null};

/**
 * Turns a response into typed arrays. `workerDecoder` does it off the render thread; `inlineDecoder`
 * is synchronous and correct, for a test, a script or a runtime without `Worker`.
 *
 * {@link decode} takes a whole body. {@link decodeHead} and {@link decodePoints} take a streamed
 * response's frames as they land, so a tile is drawable before the last frame arrives.
 */
export type Decoder = {
  /**
   * `background` sends speculative work to its own lane where there is one, so a response the user
   * is waiting for does not queue behind an anticipatory one.
   */
  decode(bytes: Uint8Array, background?: boolean): Promise<ViewportResult>;
  /** The counts, the underlay and the artifacts: everything before the first points frame. */
  decodeHead(frames: HeadFrames, background?: boolean): Promise<ViewportHead>;
  /** One kind-3 frame, decoded alone. Each frame is an independent Arrow stream. */
  decodePoints(frame: Uint8Array, background?: boolean): Promise<PointsPart>;
  /** Releases the workers, if there are any. */
  close(): void;
  /**
   * The last reply's decode time in the worker, in ms, excluding time queued in its lane. `null`
   * for the inline decoder. Per frame under a streamed response.
   */
  readonly lastWorkerMs: number | null;
};

export function inlineDecoder(): Decoder {
  // Synchronous, so `background` has no effect.
  return {
    decode: async (bytes) => decodeViewport(bytes),
    decodeHead: async (frames) => decodeHead(frames),
    decodePoints: async (frame) => decodePoints([frame]),
    close: () => {},
    lastWorkerMs: null
  };
}

/**
 * How the worker is made where the default cannot load it. The default loads `decode.worker.js`
 * from beside this module, which Vite and webpack follow and bundle. The package build writes that
 * file with Arrow bundled into it, because a page's import map does not apply inside a worker, so
 * it also loads unbundled. esbuild does not follow the URL: a host copies the file beside its
 * output or installs a factory here. The single-file bundle installs one that makes the worker
 * from a Blob or data URL.
 *
 * The factory is read each time a worker is made. A worker that fails to load after construction
 * rejects each decode sent to it.
 */
let workerFactory: (() => Worker) | null = null;

export function setWorkerFactory(factory: (() => Worker) | null): void {
  workerFactory = factory;
}

/**
 * A buffer the worker may take: the view's own where it spans the whole buffer, else a copy.
 * Transferring detaches the buffer, so a view onto a larger one is copied out.
 */
function detachable(bytes: Uint8Array): ArrayBuffer {
  const whole = bytes.byteOffset === 0 && bytes.byteLength === bytes.buffer.byteLength;
  return (
    whole ? bytes.buffer : bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength)
  ) as ArrayBuffer;
}

/**
 * Decodes in workers: two foreground lanes and a background lane made on first use. `null` where
 * `Worker` is unavailable.
 */
export function workerDecoder(): Decoder | null {
  if (typeof Worker === 'undefined') return null;

  let lastWorkerMs: number | null = null;
  /** One serial lane: a worker, its pending replies, and its id counter. */
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
      // A dead worker answers nothing outstanding, so every waiting caller is rejected.
      const failure = new Error(`decode worker failed: ${event.message}`);
      for (const waiter of pending.values()) waiter.reject(failure);
      pending.clear();
    };
    return {
      send<T>(request: object, transfer: Transferable[]): Promise<T> {
        // Every buffer named here is transferred; the caller does not read it afterwards.
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

  // Two foreground lanes, round-robin, so the pieces of a split response decode in parallel. Decode
  // is CPU-bound, and more lanes would multiply peak transferred memory. The caller keeps a stream's
  // frames in order by awaiting them in order.
  const foreground = [lane(), lane()].filter((l) => l !== null);
  if (foreground.length === 0) return null;
  let next = 0;
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
