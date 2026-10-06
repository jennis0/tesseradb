import {makeData, makeVector, Uint64} from 'apache-arrow';
import {expect} from 'vitest';
import type {ArtifactChannelClock} from '../src/artifactChannel.js';
import type {Band} from '../src/bands.js';
import {dataToWorldXY, tileXY} from '../src/coords.js';
import type {Clock} from '../src/driver.js';
import type {FrameScheduler} from '../src/presented.js';
import type {Artifact, DeclaredScalar, Layer, Meta, Quantisation, TileCounts, ViewInfo, ViewportArtifactsFrame, ViewportArtifactsRequest, ViewportArtifactsResponse, ViewportResponse, ViewportResult} from '../src/types.js';
import type {TileSink} from '../src/client.js';

/** The fixtures the core tests share, the fake clocks, and `settle`. */

// Held at import, so a test that fakes the global timers still settles on the real event loop.
const realSetTimeout = globalThis.setTimeout;
const realClearTimeout = globalThis.clearTimeout;

/**
 * The zero-delay timers set and not yet fired. The replica yields one between absorb slices, and a
 * slice's budget is wall-clock time, so a loaded machine takes more of them.
 */
const yields = new Set<unknown>();

globalThis.setTimeout = ((fire: (...args: unknown[]) => void, ms?: number, ...args: unknown[]) => {
  if (ms) return realSetTimeout(fire, ms, ...args);
  const handle = realSetTimeout(() => {
    yields.delete(handle);
    fire(...args);
  }, 0);
  yields.add(handle);
  return handle;
}) as typeof setTimeout;

globalThis.clearTimeout = ((handle?: Parameters<typeof clearTimeout>[0]) => {
  yields.delete(handle);
  realClearTimeout(handle);
}) as typeof clearTimeout;

/**
 * Resolves once every promise chain already started has run as far as it can, and every
 * zero-delay timer the code under test set along the way has fired.
 *
 * A macrotask turn runs only after the microtask queue is empty, however long the chains in it,
 * so this does not depend on how many `await`s the code under test takes. It takes turns until no
 * zero-delay timer is left, so an absorb that yields any number of times finishes first.
 */
export async function settle(): Promise<void> {
  for (let turns = 0; turns < 10_000; turns++) {
    await new Promise((resolve) => realSetTimeout(resolve, 0));
    if (yields.size === 0) return;
  }
  throw new Error('settle: zero-delay timers were still being set after 10,000 turns');
}

/**
 * Asserts that `fn` throws a refusal: a plain `Error`, which is what the decoders raise on input
 * they check. A `TypeError` or `RangeError` is a crash on input nothing checked, and fails this.
 */
export function refused(fn: () => unknown): void {
  let thrown: unknown = null;
  try {
    fn();
  } catch (error) {
    thrown = error;
  }
  expect(thrown).toBeInstanceOf(Error);
  expect((thrown as Error).constructor).toBe(Error);
}

/** {@link refused} for a promise: it rejects with a plain `Error`. */
export async function rejectsAsRefused(promise: Promise<unknown>): Promise<void> {
  const thrown = await promise.then(
    () => null,
    (error: unknown) => error
  );
  expect(thrown).toBeInstanceOf(Error);
  expect((thrown as Error).constructor).toBe(Error);
}

/** A clock whose timers fire only inside `advance`, each followed by a `settle`. */
export function fakeClock(): Clock & {advance(ms: number): Promise<void>; readonly pending: number} {
  let now = 0;
  let seq = 0;
  const timers = new Map<number, {at: number; fire: () => void}>();
  return {
    now: () => now,
    after(ms, fire) {
      const id = ++seq;
      timers.set(id, {at: now + ms, fire});
      return id;
    },
    cancel(handle) {
      timers.delete(handle as number);
    },
    get pending() {
      return timers.size;
    },
    async advance(ms) {
      // Settled first, so work started before the call has scheduled its timers.
      await settle();
      const target = now + ms;
      for (;;) {
        let next: number | null = null;
        for (const [id, t] of timers) {
          if (t.at <= target && (next === null || t.at < timers.get(next)!.at)) next = id;
        }
        if (next === null) break;
        const t = timers.get(next)!;
        timers.delete(next);
        now = t.at;
        t.fire();
        await settle();
      }
      now = target;
      await settle();
    }
  };
}

/** A timer queue with no time in it: `fire` runs everything pending, whatever its delay. */
export function manualClock(): ArtifactChannelClock & {fire(): void; readonly pending: number} {
  const timers = new Map<number, () => void>();
  let seq = 0;
  return {
    after(_ms, fire) {
      const id = ++seq;
      timers.set(id, fire);
      return id;
    },
    cancel(handle) {
      timers.delete(handle as number);
    },
    fire() {
      const fires = [...timers.values()];
      timers.clear();
      for (const f of fires) f();
    },
    get pending() {
      return timers.size;
    }
  };
}

/** A frame scheduler whose frames run only when the test calls `flush`. */
export function fakeScheduler(): FrameScheduler & {flush(): void; readonly pending: number} {
  const queue = new Map<number, () => void>();
  let seq = 0;
  return {
    request(fire) {
      const id = ++seq;
      queue.set(id, fire);
      return id;
    },
    cancel(handle) {
      queue.delete(handle as number);
    },
    flush() {
      const fires = [...queue.values()];
      queue.clear();
      for (const fire of fires) fire();
    },
    get pending() {
      return queue.size;
    }
  };
}

/** One tile's counts; `matched` and `highlighted` default to `visible`, `served` to zero. */
export function tile(prefix: bigint, visible: bigint, over: Partial<TileCounts> = {}): TileCounts {
  return {tile: prefix, visible, matched: visible, highlighted: visible, served: 0n, ...over};
}

/**
 * A held band of `n` points with ids `1..n`, every one served, at the origin of world space.
 *
 * `bytes` is what the store charges: 8 for the id, 8 for the code and 16 for the position pair.
 */
export function band(depth: number, prefix: bigint, n: number, over: Partial<Band> = {}): Band {
  const {x, y} = tileXY(prefix, depth);
  return {
    depth,
    prefix,
    x,
    y,
    ids: BigUint64Array.from({length: n}, (_, i) => BigInt(i + 1)),
    positions: new Float32Array(n * 2),
    scalars: {},
    membership: {},
    highlightBits: null,
    served: n,
    capUsed: 500,
    visible: BigInt(n),
    matched: BigInt(n),
    highlighted: BigInt(n),
    heldBelow: BigInt(n + 1),
    identityKey: 'ik',
    contentKey: 'ck',
    bytes: n * 32,
    touchedAt: 0,
    ...over
  };
}

/** A result with no tiles or points, and whichever fields `over` names. */
export function result(over: Partial<ViewportResult> = {}): ViewportResult {
  return {
    tiles: [],
    ids: new BigUint64Array(0),
    codes: new BigUint64Array(0),
    positions: new Float64Array(0),
    world: new Float32Array(0),
    scalars: {},
    membership: {},
    highlighted: null,
    pointsProjection: 'full',
    subCells: null,
    ...over
  };
}

/**
 * `n` points, ids `1..n`, all on the first of `tiles`, which is given `served: n`.
 *
 * Positions and world coordinates are one constant, which is all a test of the store's
 * bookkeeping needs of them.
 */
export function servedResult(n: number, tiles: TileCounts[], over: Partial<ViewportResult> = {}): ViewportResult {
  const [first, ...rest] = tiles;
  return result({
    tiles: first ? [{...first, served: BigInt(n)}, ...rest] : [],
    ids: BigUint64Array.from({length: n}, (_, i) => BigInt(i + 1)),
    codes: BigUint64Array.from({length: n}, () => 0n),
    positions: Float64Array.from({length: n * 2}, () => 1),
    world: Float32Array.from({length: n * 2}, () => 0.1),
    ...over
  });
}

/** A response carrying `result`, under content key `ck` and identity key `ik` unless `over` says. */
export function response(res: ViewportResult = result(), over: Partial<ViewportResponse> = {}): ViewportResponse {
  const contentKey = over.contentKey ?? 'ck';
  return {
    result: res,
    timings: {serverUs: 0, admissionUs: 0, stageNs: null},
    identityKey: 'ik',
    contentKey,
    pin: contentKey,
    stale: false,
    region: null,
    bytes: 0,
    ...over
  };
}

/**
 * A fake `viewportArtifacts`: one frame per requested tile, holding what `rowsFor` gives it, each
 * handed to the sink before the response resolves, under `ik` and `ck` unless `keys` says.
 */
export function tileAnswers(
  rowsFor: (tile: bigint, req: ViewportArtifactsRequest) => Artifact[],
  keys: () => {identityKey: string; contentKey: string} = () => ({identityKey: 'ik', contentKey: 'ck'}),
  treed: (req: ViewportArtifactsRequest) => Artifact[] = () => []
) {
  return async (_token: string, req: ViewportArtifactsRequest, opts: {signal?: AbortSignal; onTile?: TileSink} = {}): Promise<ViewportArtifactsResponse> => {
    const k = keys();
    const frames: ViewportArtifactsFrame[] = [];
    const walked = treed(req);
    if (walked.length > 0) frames.push({treed: true, tile: null, artifacts: walked});
    for (const tile of req.tiles ?? []) frames.push({treed: false, tile, artifacts: rowsFor(tile, req)});
    for (const frame of frames) {
      if (opts.signal?.aborted) throw new DOMException('The operation was aborted.', 'AbortError');
      await opts.onTile?.(frame, k);
    }
    return {frames, timings: {serverUs: 0, admissionUs: 0, stageNs: null}, ...k, pin: k.contentKey, stale: false, region: null, bytes: 0};
  };
}

/** An artifact on layer `l` at rung 0 with no geometry, parents or content. */
export function artifact(tesseraId: bigint, over: Partial<Artifact> = {}): Artifact {
  return {
    layer: 'l',
    tesseraId,
    key: `a${tesseraId}`,
    maskedCount: 1n,
    centroid: null,
    box: null,
    content: [],
    parentIds: [],
    rung: 0,
    matched: null,
    highlighted: null,
    target: null,
    ...over
  };
}

/** A flat enumerated layer on view `s0` with no levels, content or dependencies. */
export function layer(name: string, over: Partial<Layer> = {}): Layer {
  return {
    name,
    title: name,
    views: ['s0'],
    membership: 'enumerated',
    hierarchy: {kind: 'flat', pruneChildren: false},
    levels: [],
    computedContent: [],
    shape: null,
    suppliedContent: [],
    depsOn: [],
    version: 1,
    ...over
  };
}

/** A plain, unprojected view over the unit square. */
export function view(id: string, over: Partial<ViewInfo> = {}): ViewInfo {
  return {
    id,
    displayName: id,
    quantisation: {xMin: 0, xMax: 1, yMin: 0, yMax: 1},
    projection: 'none',
    worldAspect: null,
    tileScheme: null,
    tile: null,
    roster: null,
    ...over
  };
}

/** A declared column that is indexed, not rendered and read from the record, unless `over` says. */
export function scalar(name: string, arrowType: DeclaredScalar['arrowType'], over: Partial<DeclaredScalar> = {}): DeclaredScalar {
  return {name, arrowType, category: null, render: false, index: true, unique: false, analyser: null, homes: ['record'], ...over};
}

/** `/v1/meta`'s `selection` block with the server's defaults for the ceilings. */
export const SELECTION: Meta['selection'] = {
  kMin: 1,
  kMaxMarks: 500,
  maxK: 5000,
  thetaTargetMarks: 10,
  maxUnderlayOffset: 0,
  maxArtifactsPerTile: 1000,
  maxCategoryValues: 1000,
  maxRegionVertices: 10_000,
  maxRegionCells: 262_144,
  maxBrowseRows: 200,
  maxShapeVertices: 50_000,
  maxSuggestions: 20,
  maxSuggestionWalk: 100_000,
  maxSuggestSetEntities: 10_000_000,
  maxPageRows: 65_536,
  maxPageBytes: 16_777_216,
  maxAggregateGroupings: 16,
  maxAggregateTop: 1000,
  maxAggregateNamed: 1000,
  maxAggregateBins: 1000,
  maxAggregateCells: 1_048_576
};

/** A deployment of one view `s0` with nothing declared, and whichever fields `over` names. */
export function meta(over: Partial<Meta> = {}): Meta {
  return {
    apiVersion: 1,
    bundleFormat: 1,
    views: [view('s0')],
    groups: [],
    declaredScalars: [],
    scopedScalars: [],
    layers: [],
    selection: SELECTION,
    maxTilesPerRequest: 4096,
    filterOperands: [],
    ...over
  };
}

/** A request's headers as a record under lower-case names, whatever form `init` gave them in. */
export function headersOf(init?: RequestInit): Record<string, string> {
  const out: Record<string, string> = {};
  new Headers(init?.headers).forEach((value, name) => (out[name] = value));
  return out;
}

/** A framed body: `u8 kind, u32 LE length, payload` for each frame, in order. */
export function framed(frames: readonly {kind: number; payload: Uint8Array}[]): Uint8Array<ArrayBuffer> {
  const out = new Uint8Array(frames.reduce((n, f) => n + 5 + f.payload.byteLength, 0));
  const view = new DataView(out.buffer);
  let at = 0;
  for (const {kind, payload} of frames) {
    out[at] = kind;
    view.setUint32(at + 1, payload.byteLength, true);
    out.set(payload, at + 5);
    at += 5 + payload.byteLength;
  }
  return out;
}

/** A `uint64` Arrow vector. */
export function u64(values: bigint[]) {
  return makeVector(makeData({type: new Uint64(), data: BigUint64Array.from(values)}));
}

/**
 * A response whose body arrives in chunks of `size` bytes as it is read. `onCancel` is called when
 * the reader cancels the body. With `cut`, the body ends by failing with it, as `fetch` reports a
 * connection closed part-way through a chunked body, in place of ending cleanly.
 */
export function chunked(
  body: Uint8Array,
  size = body.byteLength,
  init: {headers?: Record<string, string>; onCancel?: () => void; cut?: Error} = {}
): Response {
  let at = 0;
  const stream = new ReadableStream<Uint8Array>({
    pull(controller) {
      if (at >= body.byteLength) return init.cut ? controller.error(init.cut) : controller.close();
      controller.enqueue(body.slice(at, at + size));
      at += size;
    },
    cancel() {
      init.onCancel?.();
    }
  });
  return new Response(stream, {status: 200, headers: init.headers ?? {}});
}

/**
 * A response whose body the test feeds by hand, so "not yet arrived" is a state it can hold. An
 * abort of `signal` errors the body, as a real `fetch` does to the body of an aborted request.
 */
export function manual(
  signal?: AbortSignal,
  headers: Record<string, string> = {}
): {response: Response; push: (bytes: Uint8Array) => void; close: () => void} {
  let controller!: ReadableStreamDefaultController<Uint8Array>;
  const stream = new ReadableStream<Uint8Array>({
    start(c) {
      controller = c;
    }
  });
  signal?.addEventListener('abort', () => {
    try {
      controller.error(new DOMException('The operation was aborted.', 'AbortError'));
    } catch {
      // Already closed: the abort came after the whole body.
    }
  });
  return {
    response: new Response(stream, {status: 200, headers}),
    push: (bytes) => {
      try {
        controller.enqueue(bytes);
      } catch {
        // Errored by an abort, which is the state an abort test asserts about.
      }
    },
    close: () => controller.close()
  };
}

/**
 * A camera that fits `bbox` to the tighter of the canvas's axes, as `store.setView` takes it: the
 * box, and the zoom over the 512-unit world at which frame `q` shows it so.
 */
export function camera(q: Quantisation | null, bbox: [number, number, number, number], width: number, height: number): {bbox: [number, number, number, number]; zoom: number; width: number; height: number} {
  if (!q) throw new Error('camera needs the frame the box is in: pass the view\'s quantisation before meta has landed');
  const [x0, y0] = dataToWorldXY(bbox[0], bbox[1], q);
  const [x1, y1] = dataToWorldXY(bbox[2], bbox[3], q);
  return {bbox, zoom: Math.log2(Math.min(width / (Math.abs(x1 - x0) || 1), height / (Math.abs(y1 - y0) || 1))), width, height};
}
