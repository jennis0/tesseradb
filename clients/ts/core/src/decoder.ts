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
 * The factory is read each time a worker is made.
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

/** One decode: the message for the worker, and the same decode on the main thread. */
type Job<T> = {request: () => {message: object; transfer: ArrayBuffer[]}; inline: () => Promise<T>};

/** The worker's messages: `ready` once, when its module has evaluated, then one reply per request. */
type Reply = {ready: true} | {id: number; result?: never; error?: string; ms?: number};

/**
 * Decodes in workers: two foreground lanes and a background lane made on first use. `null` where
 * `Worker` is unavailable or no worker can be constructed.
 *
 * A lane holds its requests until its worker says `ready`, without transferring their buffers. A
 * worker that fails before `ready` did not load, so the lane decodes what it holds, and everything
 * after, on the main thread. A worker that fails after `ready` loses the requests it holds, which
 * are rejected, and the lane decodes everything after on the main thread.
 */
export function workerDecoder(): Decoder | null {
  if (typeof Worker === 'undefined') return null;

  let lastWorkerMs: number | null = null;
  const inline = inlineDecoder();

  /** One serial lane: a worker, its pending replies, and its id counter. */
  function lane(): {send: <T>(job: Job<T>) => Promise<T>; close: () => void} | null {
    let worker: Worker;
    try {
      worker = workerFactory
        ? workerFactory()
        : new Worker(new URL('./decode.worker.js', import.meta.url), {type: 'module'});
    } catch {
      // A bundler that cannot resolve the worker URL, or a runtime that forbids module workers.
      return null;
    }
    let state: 'loading' | 'ready' | 'dead' = 'loading';
    let nextId = 1;
    const held: {job: Job<never>; resolve: (r: never) => void; reject: (e: Error) => void}[] = [];
    const pending = new Map<number, {resolve: (r: never) => void; reject: (e: Error) => void}>();

    function post<T>(job: Job<T>, resolve: (r: T) => void, reject: (e: Error) => void): void {
      const id = nextId++;
      const {message, transfer} = job.request();
      pending.set(id, {resolve: resolve as (r: never) => void, reject});
      // Every buffer named here is transferred; the caller does not read it afterwards.
      worker.postMessage({id, ...message}, transfer);
    }

    worker.onmessage = (event: MessageEvent<Reply>) => {
      const reply = event.data;
      if ('ready' in reply) {
        if (state !== 'loading') return;
        state = 'ready';
        for (const h of held.splice(0)) post(h.job, h.resolve, h.reject);
        return;
      }
      const waiter = pending.get(reply.id);
      if (!waiter) return;
      pending.delete(reply.id);
      if (reply.ms !== undefined) lastWorkerMs = reply.ms;
      if (reply.error !== undefined) waiter.reject(new Error(reply.error));
      else waiter.resolve(reply.result!);
    };
    worker.onerror = () => {
      const loaded = state === 'ready';
      state = 'dead';
      worker.terminate();
      for (const h of held.splice(0)) h.job.inline().then(h.resolve, h.reject);
      if (!loaded) return;
      const failure = new Error('the decode worker stopped before replying; later decodes run on the main thread');
      for (const waiter of pending.values()) waiter.reject(failure);
      pending.clear();
    };

    return {
      send<T>(job: Job<T>): Promise<T> {
        if (state === 'dead') return job.inline();
        return new Promise<T>((resolve, reject) => {
          if (state === 'ready') post(job, resolve, reject);
          else held.push({job: job as Job<never>, resolve: resolve as (r: never) => void, reject});
        });
      },
      close() {
        state = 'dead';
        worker.terminate();
        const closed = new Error('decoder closed');
        for (const h of held.splice(0)) h.reject(closed);
        for (const waiter of pending.values()) waiter.reject(closed);
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

  function send<T>(job: Job<T>, background: boolean): Promise<T> {
    if (background) {
      backgroundLane ??= lane();
      if (backgroundLane) return backgroundLane.send(job);
    }
    next = (next + 1) % foreground.length;
    return foreground[next]!.send(job);
  }

  return {
    decode(bytes, background = false) {
      return send(
        {
          request: () => {
            const buffer = detachable(bytes);
            return {message: {bytes: buffer}, transfer: [buffer]};
          },
          inline: () => inline.decode(bytes)
        },
        background
      );
    },
    decodeHead(frames, background = false) {
      return send(
        {
          request: () => {
            const tiles = detachable(frames.tiles);
            const subCells = frames.subCells ? detachable(frames.subCells) : null;
            const artifacts = frames.artifacts ? detachable(frames.artifacts) : null;
            const transfer = [tiles, subCells, artifacts].filter((b) => b !== null);
            return {message: {kind: 'head', tiles, subCells, artifacts}, transfer};
          },
          inline: () => inline.decodeHead(frames)
        },
        background
      );
    },
    decodePoints(frame, background = false) {
      return send(
        {
          request: () => {
            const buffer = detachable(frame);
            return {message: {kind: 'points', bytes: buffer}, transfer: [buffer]};
          },
          inline: () => inline.decodePoints(frame)
        },
        background
      );
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
