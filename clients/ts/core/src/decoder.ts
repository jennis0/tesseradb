import {decodeHead, decodePoints, decodeViewport, type PointsPart, type ViewportHead} from './decode.js';
import type {ViewportResult} from './types.js';

/** The head frames of one response, as they came off the wire. */
export type HeadFrames = {tiles: Uint8Array; subCells: Uint8Array | null; artifacts: Uint8Array | null};

/**
 * Turns a `/v1/viewport` body into typed arrays. The default decodes in web workers, off the thread
 * that draws; {@link inlineDecoder} decodes on the calling thread, for a test, a script or a
 * runtime without `Worker`. Pass one as a {@link TesseraClient}'s `decoder` option.
 *
 * @category HTTP client
 */
export type Decoder = {
  /**
   * Decodes a whole body. `background` sends the work to a separate worker where the decoder has
   * one, so a response the user is waiting for does not queue behind speculative work. Rejects with
   * `Error` for a malformed body.
   */
  decode(bytes: Uint8Array, background?: boolean): Promise<ViewportResult>;
  /** The counts, the underlay and the artifacts: everything before the first points frame. @internal */
  decodeHead(frames: HeadFrames, background?: boolean): Promise<ViewportHead>;
  /** One kind-3 frame, decoded alone. Each frame is an independent Arrow stream. @internal */
  decodePoints(frame: Uint8Array, background?: boolean): Promise<PointsPart>;
  /** Terminates the workers, if there are any, and rejects the decodes they hold. */
  close(): void;
  /**
   * The last reply's decode time inside a worker, in milliseconds, excluding time queued. On a
   * streamed response, the time of the last frame. `null` for the inline decoder and before the
   * first reply.
   */
  readonly lastWorkerMs: number | null;
};

/**
 * A decoder that runs on the calling thread. `background` has no effect and `lastWorkerMs` is
 * always `null`.
 *
 * @category HTTP client
 */
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

/** The factory {@link setWorkerFactory} installed, or `null` for the default. */
let workerFactory: (() => Worker) | null = null;

/**
 * Sets how the default decoder makes a worker, for a host whose bundler cannot load the default.
 * The default loads `decode.worker.js` from beside this module, which Vite and webpack follow and
 * bundle. That file has Arrow bundled into it, since a page's import map does not apply inside a
 * worker, so it also loads unbundled. esbuild does not follow the URL: a host copies the file
 * beside its output or installs a factory here. The single-file bundle installs one that makes the
 * worker from a Blob or data URL.
 *
 * @param factory - Makes one worker. Read each time a worker is made. `null` restores the default.
 *
 * @category HTTP client
 */
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
 * A decoder that decodes in web workers: two workers that take requests in turn, and a third for
 * `background` work, made on its first use. Returns `null` where `Worker` is undefined or no
 * worker can be made.
 *
 * A worker holds its requests until it has loaded. One that fails to load has its requests, and
 * every later one, decoded on the calling thread. One that fails after loading rejects the
 * decodes it holds, and later requests to it are decoded on the calling thread.
 *
 * @category HTTP client
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

/**
 * {@link workerDecoder} where a worker can be made, {@link inlineDecoder} otherwise. The decoder a
 * {@link TesseraClient} uses when given none.
 *
 * @category HTTP client
 */
export function createDecoder(): Decoder {
  return workerDecoder() ?? inlineDecoder();
}
