import {BandBudget, BandCache, bandSplitter, tableCleared, type Band, type Resolved} from './bands.js';
import type {SessionArtifactTable} from './artifactTable.js';
import {rectArea, type TileRect} from './rects.js';
import {rectToRequestBbox, tileXY} from './coords.js';
import type {Quantisation, TileCounts, ViewportPart, ViewportResponse, RegionVerdict} from './types.js';

/**
 * The read-through replica: callers ask for a region of tiles at a depth and get bands back.
 *
 * Which tiles to want (the depth budget, the prefetch ring, anticipation) belongs to the layer
 * above. Keeping that out lets a consumer with its own tile scheduler, such as deck.gl's
 * `TileLayer` or a MapLibre source, use this without two schedulers working against each other.
 */

/** The byte budget for held bands where none is given. */
export const DEFAULT_CACHE_BYTES = 512 * 1024 * 1024;

/** @internal */
export type ReplicaOptions = {
  view: string;
  /** The byte budget for held bands. */
  cacheBytes?: number;
  /**
   * A budget shared with every other view's replica: one number bounds the total, and eviction may
   * take a band from any of them. Absent, the replica has {@link cacheBytes} to itself.
   */
  budget?: BandBudget;
  /**
   * When false the store holds nothing, and every ask becomes a request, as a client without a
   * replica would make. For measuring what the replica saves.
   */
  cache?: boolean;
  /**
   * How long an ask answered wholly from held tiles may go without touching the server.
   *
   * A client that answers pans from held tiles alone never sees a new content key, so accepted
   * changes and new counts would not appear. Past this age such an ask issues a count-only request
   * (`k = 0`), which refreshes the counts and the content key without fetching marks.
   */
  revalidateAfterMs?: number;
  now?: () => number;
  /** The session artifact table the bands' membership is named through. */
  table?: SessionArtifactTable;
  /**
   * Reports how long a named phase inside the replica took, and over how many items. Splitting a
   * response into bands runs between the fetch resolving and the frame returning, so it cannot be
   * timed from outside. Changes no behaviour.
   */
  onPhase?: (kind: string, ms: number, n: number) => void;
};

/** What one {@link Replica.fetchRegion} call resolved to. @internal */
export type ReplicaFrame = {
  depth: number;
  /** The region asked about, in tile-index space at `depth`. */
  want: TileRect;
  /** Bands at this depth inside the region: the served set, drawable with counts. */
  exact: Band[];
  /**
   * Bands from another depth, each with the rectangle it may be drawn over: the part of the region
   * not held at this depth. A superset of what is served there, so drawn stale and not counted.
   */
  fallback: {band: Band; clip: TileRect}[];
  /**
   * The store's change counter when this frame was derived. A caller compares it with
   * `Replica.version` to know whether a frame it has is still current.
   */
  version: number;
  /** Null when the region was answered entirely from the store. */
  response: ViewportResponse | null;
  /**
   * How the ask was split. `wanted - novel` tiles were answered from the store without a request.
   */
  plan: {
    /** Tiles the region spans, and how many of them had to be asked for. */
    wanted: number;
    novel: number;
    /** How many requests were issued: pieces sent, not rectangles planned. */
    requests: number;
    /** Response bytes across every piece. `response` keeps only the last piece. */
    bytes: number;
  };
};

/**
 * Tiles per request, so one response does not take seconds to decode. In tiles because the client
 * does not know the point count before asking.
 */
const MAX_TILES_PER_REQUEST = 25_000;

/** How long one absorb slice may hold the thread: half a 60 Hz frame. */
const ABSORB_SLICE_MS = 6;

/**
 * A frame's worth of time. Yielding a macrotask between slices, by `setTimeout(0)` or
 * `scheduler.yield()`, does not let an animation frame run while a large response is split. So when
 * nothing has drawn for a frame's time the absorb waits for the next animation frame, which paints
 * what the slices so far stored, and otherwise yields a macrotask. Loading is slower and the first
 * marks appear as the response lands.
 */
const FRAME_MS = 16;
/** The most an absorb slice waits for a frame before continuing anyway. */
const FRAME_WAIT_MAX_MS = 300;
/** How long after the last slice the frame pulse keeps listening; a quiet page runs no loop. */
const PULSE_MS = 500;
let lastFrameAt = 0;
let lastFrameGap = 0;
let pulseUntil = 0;
let pulsing = false;
/**
 * A frame slower than this means a renderer that cannot afford a paint per slice (software GL
 * draws a million marks in seconds). Slices are then stored without presenting, and the response
 * paints once.
 */
const SLOW_FRAME_MS = 250;
function framesFlowing(): boolean {
  if (typeof requestAnimationFrame === 'undefined') return true;
  // The last gap and the current one: a frame overdue past the threshold is a slow renderer
  // mid-paint.
  return pulsing && lastFrameGap > 0 && lastFrameGap < SLOW_FRAME_MS && performance.now() - lastFrameAt < SLOW_FRAME_MS;
}
/** Notes each animation frame's time while an absorb runs; stops itself when none does. */
function pulse(): void {
  if (pulsing || typeof requestAnimationFrame === 'undefined') return;
  pulsing = true;
  const note = () => {
    const now = performance.now();
    lastFrameGap = lastFrameAt > 0 ? now - lastFrameAt : 0;
    lastFrameAt = now;
    if (lastFrameAt < pulseUntil) requestAnimationFrame(note);
    else pulsing = false;
  };
  requestAnimationFrame(note);
}
function yieldToFrame(): Promise<void> {
  if (typeof requestAnimationFrame !== 'undefined') {
    const now = performance.now();
    pulseUntil = now + PULSE_MS;
    pulse();
    if (now - lastFrameAt > FRAME_MS) {
      // Waits for a frame or FRAME_WAIT_MAX_MS, whichever is first: under software GL one frame
      // can take seconds.
      return new Promise((resolve) => {
        let done = false;
        const finish = () => {
          if (done) return;
          done = true;
          resolve();
        };
        requestAnimationFrame(() => setTimeout(finish, 0));
        setTimeout(finish, FRAME_WAIT_MAX_MS);
      });
    }
  }
  return new Promise((resolve) => setTimeout(resolve, 0));
}

/** Splits a rectangle into full-width row strips of at most `maxTiles`. */
function splitRect(rect: TileRect, maxTiles: number): TileRect[] {
  const width = rect.x1 - rect.x0 + 1;
  const rowsPerPiece = Math.max(1, Math.floor(maxTiles / width));
  if ((rect.y1 - rect.y0 + 1) <= rowsPerPiece) return [rect];
  const out: TileRect[] = [];
  for (let y = rect.y0; y <= rect.y1; y += rowsPerPiece) {
    out.push({x0: rect.x0, y0: y, x1: rect.x1, y1: Math.min(rect.y1, y + rowsPerPiece - 1)});
  }
  return out;
}

/** @internal */
export class Replica {
  private readonly cache: BandCache;
  private readonly now: () => number;
  private identityKey = '';
  private contentKey = '';
  private regionVerdict: RegionVerdict | null = null;
  private validatedAt = Number.NEGATIVE_INFINITY;

  constructor(
    private readonly fetchViewport: (
      req: {
        view: string;
        zoom: number;
        bbox?: [number, number, number, number];
        tiles?: bigint[];
        k?: number;
      },
      signal?: AbortSignal,
      /** Speculative work goes to its own decode lane; see `Decoder.decode`. */
      background?: boolean,
      /**
       * Takes each points frame as it lands; see `TesseraClient.viewport`. The response then
       * carries no points. A transport that cannot stream may ignore this and answer whole.
       */
      onPart?: (part: ViewportPart) => void | Promise<void>
    ) => Promise<ViewportResponse>,
    private readonly quantisation: Quantisation,
    private readonly opts: ReplicaOptions
  ) {
    this.cache = new BandCache(opts.budget ?? opts.cacheBytes ?? DEFAULT_CACHE_BYTES, opts.table ?? null);
    this.now = opts.now ?? (() => performance.now());
  }

  /** Drops everything held, for a change of principal, filter or selection. */
  reset(): void {
    this.cache.dropIdentity();
    this.identityKey = '';
    this.contentKey = '';
    this.regionVerdict = null;
    this.validatedAt = Number.NEGATIVE_INFINITY;
  }

  /**
   * The `x-tessera-region` verdict the last response carried, or `null` where it carried none. A
   * frame derived from held bands has no response, so the store reads the verdict here. `reset`
   * forgets it.
   */
  get lastRegionVerdict(): RegionVerdict | null {
    return this.regionVerdict;
  }

  /**
   * Bytes held across every view sharing this replica's budget, which a look-ahead sizes its ring
   * against: another view's bands spend the same budget.
   */
  get bytes(): number {
    return this.cache.sharedBytes;
  }

  /** How many views hold any band. */
  get heldViews(): number {
    return this.cache.heldViews;
  }

  /** Points held; see {@link BandCache.points}. */
  get points(): number {
    return this.cache.points;
  }

  get bandCount(): number {
    return this.cache.bandCount;
  }

  /** See {@link BandCache.exactIn}. */
  exactIn(want: TileRect, depth: number): Band[] {
    return this.cache.exactIn(want, depth);
  }

  /** See {@link BandCache.retract}; the caller schedules the refetch. */
  retract(bands: readonly Band[]): void {
    this.cache.retract(bands);
  }

  /** The store's change counter; see {@link BandCache.version}. */
  get version(): number {
    return this.cache.version;
  }

  /**
   * How many tiles of a region are not yet held, without fetching. Lets a scheduler skip a ring
   * already covered.
   */
  novelIn(want: TileRect, depth: number, k: number): number {
    return this.cache.planRegion(want, depth, this.contentKey, k).novel;
  }

  /**
   * A region's per-tile masked counts, without marks: `k = 0`, the tile list and the content key.
   *
   * Nothing is stored. The response carries no points, so absorbing it would store empty bands and
   * let a later plan skip ground whose marks never arrived. The keys are observed and the counts
   * returned to the caller.
   */
  async counts(want: TileRect, depth: number, signal?: AbortSignal): Promise<TileCounts[]> {
    const bbox = rectToRequestBbox(want, depth, this.quantisation);
    const response = await this.fetchViewport({view: this.opts.view, zoom: depth, bbox, k: 0}, signal);
    this.observe(response);
    return response.result.tiles;
  }

  /** The byte budget the store was given, so a caller can size its look-ahead against it. */
  get budgetBytes(): number {
    return this.cache.budgetBytes;
  }

  /** The content coordinate last observed, for a caller that wants to stale-mark against it. */
  get currentContentKey(): string {
    return this.contentKey;
  }

  /**
   * What the store can draw for a region now, without the network. Separate from
   * {@link fetchRegion} because fetching is rate-limited and reading the store is not: a view
   * change draws from a warm store at once.
   */
  frameFromCache(want: TileRect, depth: number, k: number): ReplicaFrame {
    // Timed in two halves, which scale differently: the coverage subtraction with how fragmented
    // the held region is, the band walk with how much is held.
    const started = this.opts.onPhase ? performance.now() : 0;
    const plan = this.cache.planRegion(want, depth, this.contentKey, k);
    const planned = this.opts.onPhase ? performance.now() : 0;
    this.opts.onPhase?.('plan', planned - started, plan.wanted);
    const {exact, fallback} = this.cache.bandsForRegion(want, depth, this.contentKey, k);
    this.opts.onPhase?.('walk', performance.now() - planned, this.cache.bandCount);
    return {
      depth,
      want,
      exact,
      fallback,
      version: this.cache.version,
      response: null,
      plan: {wanted: plan.wanted, novel: plan.novel, requests: 0, bytes: 0}
    };
  }

  /**
   * Asks for a region, answering from the store where it can and requesting the rest. A request
   * names only the tiles it needs, so a held tile costs the server nothing.
   */
  async fetchRegion(
    want: TileRect,
    depth: number,
    k: number,
    signal?: AbortSignal,
    /**
     * The region to draw, where wider than the region to fetch. Drawing only the fetched box ends
     * the drawn buffer near the screen edge, so a modest pan would wait for a re-assembly although
     * every point is in memory.
     */
    render: TileRect = want,
    /**
     * The most requests to issue in one call. Decode and absorb run on the render thread, so a wide
     * anticipatory ring answered at once blocks every gesture behind it. A background caller takes
     * one piece per idle pause and the region fills over several.
     */
    maxRequests = Infinity,
    /**
     * Whether to derive stand-ins for the returned frame. A caller folding an arrival into a frame
     * already drawn does not need them. `false` returns `fallback: []` and changes nothing fetched
     * or stored.
     */
    standIns = true,
    /**
     * Whether these fetches are anticipation. They go to the decoder's speculative lane, so a large
     * anticipatory response does not delay a foreground decode.
     */
    background = false
  ): Promise<ReplicaFrame> {
    const generation = this.opts.table?.generation;
    const plan =
      this.opts.cache === false
        ? {fetch: [want], wanted: rectArea(want), novel: rectArea(want)}
        : this.cache.planRegion(want, depth, this.contentKey, k);

    let response: ViewportResponse | null = null;
    let fetched: Band[] = [];

    // One request per novel rectangle, each split so no response decodes for more than a frame or
    // two. Pieces are ordered by distance from the render centre, so the middle of the screen fills
    // first. Piece N+1 goes on the wire while piece N decodes and absorbs; fetched serially, the
    // server sits idle between pieces.
    const cx = (render.x0 + render.x1) / 2;
    const cy = (render.y0 + render.y1) / 2;
    const pieces = plan.fetch
      .flatMap((r) => splitRect(r, MAX_TILES_PER_REQUEST))
      .slice(0, maxRequests)
      .sort((a, b) => {
        const da = (a.x0 + a.x1) / 2 - cx;
        const db = (b.x0 + b.x1) / 2 - cx;
        const ea = (a.y0 + a.y1) / 2 - cy;
        const eb = (b.y0 + b.y1) / 2 - cy;
        return da * da + ea * ea - (db * db + eb * eb);
      });
    // A piece is absorbed frame by frame as the wire delivers it, so the first tiles of a large
    // answer are stored and drawable while the rest is received. The promise resolves to the
    // piece's keys and byte count, without points.
    const request = (rect: TileRect) => {
      const bbox = rectToRequestBbox(rect, depth, this.quantisation);
      const piece = {landed: [] as Band[], parts: 0, startedAt: 0, fetching: null as unknown as Promise<ViewportResponse>};
      // One touch time for the whole piece, so eviction sees its bands as one arrival.
      const startedAt = this.now();
      piece.startedAt = startedAt;
      piece.fetching = this.fetchViewport(
        {view: this.opts.view, zoom: depth, bbox, k},
        signal,
        background,
        async (part) => {
          piece.parts += 1;
          for (const band of await this.absorb(part, depth, k, generation, startedAt)) piece.landed.push(band);
        }
      );
      // The loop below may throw on an earlier piece while this one is in flight; this keeps this
      // piece's refusal from going unhandled.
      piece.fetching.catch(() => {});
      return piece;
    };
    let issued = 0;
    let responseBytes = 0;
    let pending = pieces.length > 0 ? request(pieces[0]!) : null;
    for (let i = 0; i < pieces.length; i++) {
      const piece = pending!;
      response = await piece.fetching;
      issued += 1;
      responseBytes += response.bytes;
      pending = i + 1 < pieces.length ? request(pieces[i + 1]!) : null;
      // A streamed piece has stored its points already and its response carries none, so only its
      // keys are observed. A transport that answered whole is absorbed here.
      if (piece.parts === 0) {
        for (const band of await this.absorb(response, depth, k, generation, piece.startedAt)) piece.landed.push(band);
      } else {
        this.observe(response);
      }
      fetched = fetched.concat(piece.landed);
      // Once per response, since eviction sorts every held band.
      if (this.opts.cache !== false && piece.landed.length > 0) {
        this.cache.evict({depth, prefix: piece.landed[0]!.prefix, protect: {depth, rect: render}});
      }
      // Marked only after the bands are in, or the next plan would skip ground whose data never
      // arrived. An aborted piece does not reach here, so its ground stays novel.
      if (this.opts.cache !== false && k > 0) {
        this.cache.markCovered(pieces[i]!, depth, this.contentKey, k);
      }
    }

    if (plan.fetch.length === 0 && this.dueForRevalidation()) {
      // Everything is held, so only the counts and the content key are refreshed.
      const revalidatedAt = performance.now();
      const bbox = rectToRequestBbox(want, depth, this.quantisation);
      response = await this.fetchViewport(
        {view: this.opts.view, zoom: depth, bbox, k: 0},
        signal
      );
      this.observe(response);
      // Reported so that minutes of settled panning with no revalidation show up in a trace.
      this.opts.onPhase?.('revalidate', performance.now() - revalidatedAt, 1);
    }

    // With the store bypassed the frame is this response alone.
    const {exact, fallback} =
      this.opts.cache === false
        ? {exact: fetched, fallback: [] as {band: Band; clip: TileRect}[]}
        : standIns
          ? this.cache.bandsForRegion(render, depth, this.contentKey, k)
          : {exact: this.cache.exactIn(render, depth), fallback: [] as {band: Band; clip: TileRect}[]};

    return {
      depth,
      want: render,
      exact,
      fallback,
      version: this.cache.version,
      response,
      plan: {wanted: plan.wanted, novel: plan.novel, requests: issued, bytes: responseBytes}
    };
  }

  dueForRevalidation(): boolean {
    const after = this.opts.revalidateAfterMs ?? 60_000;
    return this.now() - this.validatedAt >= after;
  }

  /**
   * Takes a response's keys without its points. A changed identity key empties the store,
   * whatever the caller did about the token: the key comes from the server, so no call can be
   * forgotten that would leave one principal's bands under another's.
   */
  private observe(from: {identityKey: string; contentKey: string; region?: RegionVerdict | null}): void {
    if (from.identityKey !== this.identityKey) {
      this.cache.dropIdentity();
      this.identityKey = from.identityKey;
    }
    this.contentKey = from.contentKey;
    this.validatedAt = this.now();
    if (from.region !== undefined) this.regionVerdict = from.region;
  }

  /**
   * Splits an arrival into bands and stores them, in slices so a frame can paint in between.
   *
   * An arrival is one points frame or one whole response; they have the same shape because the
   * server flushes at whole tiles. Each slice runs for {@link ABSORB_SLICE_MS} and then yields.
   * The caller marks a region covered only after the whole response, so a redraw between slices
   * sees the arrived bands as exact and the rest as stand-ins, with no hole.
   */
  private async absorb(
    arrival: {result: ViewportResponse['result']; identityKey: string; contentKey: string},
    depth: number,
    k: number,
    /** The table's generation when the fetch began. An arrival after a clear holds nothing. */
    generation: number | undefined,
    /** The touch time every band of one piece shares: the piece's start. */
    at: number = this.now()
  ): Promise<Band[]> {
    const table = this.opts.table;
    if (table && table.generation !== generation) throw tableCleared();
    this.observe(arrival);
    const contentKey = this.contentKey;

    const splitter = bandSplitter(arrival.result, depth, {
      identityKey: this.identityKey,
      contentKey,
      capUsed: k,
      now: at,
      table: this.opts.table,
      onRemap: (ms) => this.opts.onPhase?.('remap', ms, arrival.result.ids.length)
    });
    const bands: Band[] = [];
    let splitMs = 0;
    let storeMs = 0;
    let slices = 0;
    let longestSliceMs = 0;
    while (!splitter.done()) {
      // A clear during the yield below ends this arrival: its ordinals name nothing now.
      if (table && table.generation !== generation) throw tableCleared();
      const started = performance.now();
      const slice = splitter.step(started + ABSORB_SLICE_MS);
      const took = performance.now() - started;
      splitMs += took;
      if (took > longestSliceMs) longestSliceMs = took;
      const stored = performance.now();
      if (this.opts.cache !== false) {
        for (const band of slice) this.cache.put(band);
      }
      for (const band of slice) bands.push(band);
      storeMs += performance.now() - stored;
      slices++;
      // The consumer may present between slices.
      if (framesFlowing()) this.opts.onPhase?.('piece', took, slice.length);
      if (!splitter.done()) await yieldToFrame();
    }
    this.opts.onPhase?.('split', splitMs, bands.length);
    this.opts.onPhase?.('store', storeMs, slices);
    // The longest slice says whether the slice budget held the thread.
    this.opts.onPhase?.('slice', longestSliceMs, slices);
    return bands;
  }

}

export type {Band, Resolved};
