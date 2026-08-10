import {BandCache, bandSplitter, type Band, type Resolved} from './bands.js';
import {rectArea, type TileRect} from './rects.js';
import {rectToRequestBbox, tileXY} from './coords.js';
import type {Quantisation, ViewportResponse} from './types.js';

/**
 * Layer 1: the read-through replica.
 *
 * **Tile-shaped, with no opinion about which tiles.** Callers ask for a tile set and get bands
 * back; deciding *which* tiles to want — depth budget, prefetch ring, anticipation — belongs to the
 * layer above, and keeping the two apart is what lets a consumer with its own tile scheduler
 * (deck.gl's `TileLayer`, a MapLibre source) use this one without running two schedulers against
 * each other. It is also the shape the tile-addressed route batches, so serving these asks over
 * per-tile `GET`s later is a transport swap beneath an unchanged interface.
 *
 * **Coalescing, not just caching.** A tile-addressed consumer asks per tile, and a 1–2 × 10^6-mark
 * view spans 60–125 × 10^3 of them — the self-DoS `caching.md` §10 names. Asks made within one
 * microtask are gathered into a single request, which is what makes the per-tile interface
 * affordable at all.
 */

export type ReplicaOptions = {
  slice: string;
  /** The byte budget for held bands. `caching.md` §5 sizes C1 at 512 MB–1 GB. */
  cacheBytes?: number;
  /**
   * When false the store holds nothing: every ask becomes a request and the wire traffic is
   * byte-for-byte what a client without a replica produces. The A/B for the novelty-rate
   * measurement, and the fallback if delta serving is ever suspected of a hole.
   */
  cache?: boolean;
  /**
   * How long an all-held ask may be answered without touching the server.
   *
   * A client that answers pans entirely from held tiles never observes a rotation, so an accepted
   * change stays invisible indefinitely and cached counts are presented as current forever
   * (`client-interaction.md` §4: *don't re-download* is free, *don't re-request* needs a bound).
   * Past this age an otherwise-empty ask issues a counts-only request — `k = 0`, the tile stream
   * and the validator alone, for the price of the counting stage — which refreshes the number
   * channel and the content key without re-fetching a single mark.
   *
   * The owner's budget for this is minutes, in both directions.
   */
  revalidateAfterMs?: number;
  now?: () => number;
  /**
   * Observability hook: how long a named phase inside the replica took, and over how many items.
   *
   * **Here because absorbing a response cannot be timed from outside.** Splitting a response into
   * bands runs between the fetch resolving and the frame returning, on the calling thread, and it is
   * the largest single block of main-thread work this client does — 41–119 ms per response at a 10^6
   * mark budget. A consumer that wants to see it has nowhere else to stand.
   *
   * Never called when absent, and nothing here changes behaviour.
   */
  onPhase?: (kind: string, ms: number, n: number) => void;
};

/** What one {@link Replica.fetchRegion} call resolved to. */
export type ReplicaFrame = {
  depth: number;
  /** The region asked about, in tile-index space at `depth`. */
  want: TileRect;
  /** Bands at this depth inside the region — the served set, drawable with counts. */
  exact: Band[];
  /**
   * Bands from another depth, each with the rectangle it may be drawn over — the part of the region
   * not held at this depth. A superset of what the definition serves there: drawn, stale-marked,
   * and never counted.
   */
  fallback: {band: Band; clip: TileRect}[];
  /**
   * The store's change counter when this frame was derived.
   *
   * A caller redrawing from the cache can compare it against `Replica.version` to know whether a
   * frame it already has still answers, and skip re-deriving one — which is per-band work over
   * every stand-in band, on every animation frame.
   */
  version: number;
  /** Null when the region was answered entirely from the store. */
  response: ViewportResponse | null;
  /**
   * How the ask was split — the cache's effectiveness, made visible rather than inferred.
   *
   * `omitted` is what the store proved it already held and so never asked for; that number is the
   * whole point of the replica, and a client showing marks while this stays at zero has a cache
   * that is costing memory and buying nothing.
   */
  plan: {
    /** Tiles the region spans, and how many of them had to be asked for. */
    wanted: number;
    novel: number;
    /** How many requests were actually issued — pieces sent, not rectangles planned. */
    requests: number;
    /**
     * Response bytes across every piece. `response` keeps only the last piece under pipelining,
     * so this is the only honest byte figure — the anticipation byte budget and the traces both
     * read it, and both undercounted by the piece count before it existed.
     */
    bytes: number;
  };
};

/**
 * Tiles per request, so one response cannot decode for seconds.
 *
 * Sized in tiles rather than points because the client cannot know the point count before asking;
 * at the demo corpus's density this is ~2 × 10^5 points, a few hundred milliseconds of decode.
 */
const MAX_TILES_PER_REQUEST = 25_000;

/**
 * How long one absorb slice may hold the thread before a queued frame gets to draw.
 *
 * Half a 60 Hz frame: a slice never costs more than it leaves, so an arrival mid-drag degrades the
 * frame it lands in rather than owning it.
 */
const ABSORB_SLICE_MS = 6;

/** A macrotask, which is what lets a pending `requestAnimationFrame` run. A microtask would not. */
function yieldToFrame(): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, 0));
}

/** Split a rectangle into row-strips of at most `maxTiles`, preserving full width. */
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

export class Replica {
  private readonly cache: BandCache;
  private readonly now: () => number;
  private identityKey = '';
  private contentKey = '';
  private validatedAt = Number.NEGATIVE_INFINITY;

  constructor(
    private readonly fetchViewport: (
      req: {
        slice: string;
        zoom: number;
        bbox?: [number, number, number, number];
        tiles?: bigint[];
        k?: number;
      },
      signal?: AbortSignal,
      /** Speculative work rides its own decode lane — see `Decoder.decode`. */
      background?: boolean
    ) => Promise<ViewportResponse>,
    private readonly quantisation: Quantisation,
    private readonly opts: ReplicaOptions
  ) {
    this.cache = new BandCache(opts.cacheBytes ?? 512 * 1024 * 1024);
    this.now = opts.now ?? (() => performance.now());
  }

  /**
   * Drop everything held. Called when the token changes, before the new principal's first response
   * can tell us its identity coordinate — so the store is never non-empty across a principal
   * change even for one request (`client-interaction.md` §10).
   */
  reset(): void {
    this.cache.dropIdentity();
    this.identityKey = '';
    this.contentKey = '';
    this.validatedAt = Number.NEGATIVE_INFINITY;
  }

  get bytes(): number {
    return this.cache.bytes;
  }

  /** Points held, and over how many bands — see {@link BandCache.points}. */
  get points(): number {
    return this.cache.points;
  }

  get bandCount(): number {
    return this.cache.bandCount;
  }

  /** See {@link BandCache.exactIn} — the fast half of a frame, for folding an arrival into one. */
  exactIn(want: TileRect, depth: number): Band[] {
    return this.cache.exactIn(want, depth);
  }

  /** See {@link BandCache.version} — the store's change counter, for reusing a derived frame. */
  get version(): number {
    return this.cache.version;
  }

  /**
   * How many tiles of a region are not yet held — without fetching anything.
   *
   * Lets a scheduler pick the nearest band with work in it rather than re-asking for one already
   * covered, which is what keeps a graded ring from re-fetching its inner bands forever.
   */
  novelIn(want: TileRect, depth: number, k: number): number {
    return this.cache.planRegion(want, depth, this.contentKey, k).novel;
  }

  /** The byte budget the store was given, so a caller can size its look-ahead against it. */
  get budgetBytes(): number {
    return this.opts.cacheBytes ?? 512 * 1024 * 1024;
  }

  /** The content coordinate last observed, for a caller that wants to stale-mark against it. */
  get currentContentKey(): string {
    return this.contentKey;
  }

  // The tile-addressed ask (`tile()` + a microtask-batched flush) was DELETED here — D3,
  // `client-architecture.md` §4: its batch coalesced by bounding rectangle and its error path
  // resolved refused asks to the same `null` as empty ones, the exact conflation
  // client-interaction §9 forbids. The supported path for a tile-based visualisation engine is
  // an adapter over `fetchRegion`/`frameFromCache`: batch per-tile asks into region fetches,
  // answer each from the store, and keep empty distinct from refused per ask.

  /**
   * Ask for a tile set, answering from the store where it can and issuing at most one request for
   * the rest.
   *
   * The request names the tiles it actually needs, so a tile the store already holds is not merely
   * discarded on arrival — it is never derived, counted, selected or gathered. That is what makes
   * server work scale with what is new rather than with the area on screen.
   */
  /**
   * What the store can draw for a region **right now**, without touching the network.
   *
   * Separated from {@link fetchRegion} because drawing and fetching want opposite treatment.
   * Fetching is rate-limited — a drag emits per frame and must not become a request per frame.
   * Reading the store is local work costing microseconds, and gating it behind the same debounce
   * makes every view change wait for a network policy before consulting a cache that could have
   * answered at once. That is pop-in with a warm cache and nothing to fetch.
   */
  frameFromCache(want: TileRect, depth: number, k: number): ReplicaFrame {
    // Split, because deriving a frame became the second-largest cost in a recorded session (mean
    // 42.6 ms, growing from 4 ms to 111 ms as the cache filled, and 14.7x worse on a zoom out than
    // a zoom in) and its two halves fail differently: the coverage subtraction is bounded by how
    // fragmented the held region is, the band walk by how much is held at all. A synthetic at
    // 6.6 x 10^4 bands and 800 coverage rectangles reproduced neither, so this is measured where it
    // actually happens rather than modelled.
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

  async fetchRegion(
    want: TileRect,
    depth: number,
    k: number,
    signal?: AbortSignal,
    /**
     * The region to *draw*, if wider than the region to fetch.
     *
     * They are different questions. What to fetch is bounded by what the budget will pay for; what
     * to draw is bounded by what is already held, and drawing only the fetched box means the drawn
     * buffer ends 30% beyond the screen — so a pan of more than 15% of the viewport runs off the
     * edge of the marks and waits for a re-assembly even though every point was already in memory.
     */
    render: TileRect = want,
    /**
     * Most requests to issue in one call, for a caller that must not monopolise the main thread.
     *
     * **Decode is synchronous and on the render thread**, so the ceiling on anticipation is not the
     * network or the server — measured, the server answers a ring in single-digit milliseconds —
     * but how long the client spends turning the answer into typed arrays. A wide ring asked for
     * 2.3 × 10^6 points, and the ~2.8 s of decode and absorb that followed queued behind it every
     * gesture the user made: pan-to-paint went from 19 ms to 8.8 s while the server's own share
     * stayed at 9 ms. Splitting the request does not help on its own — the total work is the same
     * and the thread is the same.
     *
     * So a background caller takes one bite per idle pause and the region fills over several. The
     * cache still reaches its target; it just stops doing it all at once.
     */
    maxRequests = Infinity,
    /**
     * Whether to derive the stand-in set for the returned frame.
     *
     * The stand-in walk is the expensive half of deriving a frame, and a caller holding an
     * already-drawn frame for this region does not need it again to fold in an arrival — the held
     * stand-ins are one arrival stale, which is drawable, and the full derivation runs when the
     * gesture pauses. `false` returns `fallback: []`; it never changes what is fetched or stored.
     */
    standIns = true,
    /**
     * Whether these fetches are anticipation rather than something the user is waiting for.
     *
     * Routed to the decoder's speculative lane, so a multi-megabyte ring response never delays a
     * foreground decode behind it — the priority inversion a serial worker otherwise builds in.
     */
    background = false
  ): Promise<ReplicaFrame> {
    const plan =
      this.opts.cache === false
        ? {fetch: [want], wanted: rectArea(want), novel: rectArea(want)}
        : this.cache.planRegion(want, depth, this.contentKey, k);

    let response: ViewportResponse | null = null;
    let fetched: Band[] = [];

    // **One request per novel rectangle, and no rectangle large enough to block a frame.** A pan
    // yields a single strip, so this is one request in the common case — `rectSubtractAll` bounds
    // the fragmentation rather than letting it grow with the number of past fetches.
    //
    // Each is then split so no single response decodes for longer than a frame or two. Measured
    // before this existed: a wide anticipatory ring asked for one rectangle of 188 × 10^3 tiles,
    // got 2.3 × 10^6 points back, and spent 2.5 s decoding and 0.3 s absorbing them **on the main
    // thread** — during which a pan the user had already made sat queued behind it and measured
    // 7.5 s, against its own server time of 5 ms and its own response of 212 KB. Nothing was slow
    // except the size of one bite.
    // **Centre-first, and pipelined.** The first piece to land is the first thing painted, so the
    // pieces are ordered by distance from the render centre — the part of the screen being looked
    // at fills first, and the periphery follows. And piece N+1 goes on the wire while piece N
    // decodes and absorbs: fetched serially, a four-piece viewport paid Σ(wire + decode + absorb)
    // with the server idle between pieces — measured as 0.6–2 s pan-to-paint on novel ground over
    // 60–100 ms of server time. The decode pool has two foreground lanes for exactly this overlap;
    // absorbing stays serial on this thread, so main-thread pressure is unchanged.
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
    const request = (rect: TileRect) => {
      const bbox = rectToRequestBbox(rect, depth, this.quantisation);
      const fetching = this.fetchViewport({slice: this.opts.slice, zoom: depth, bbox, k}, signal, background);
      // The loop below may throw out of an earlier piece (an abort, a shed request) while this one
      // is still flying; its refusal is then nobody's answer and must not surface as unhandled.
      fetching.catch(() => {});
      return fetching;
    };
    let issued = 0;
    let responseBytes = 0;
    let pending = pieces.length > 0 ? request(pieces[0]!) : null;
    for (let i = 0; i < pieces.length; i++) {
      response = await pending!;
      issued += 1;
      responseBytes += response.bytes;
      pending = i + 1 < pieces.length ? request(pieces[i + 1]!) : null;
      fetched = fetched.concat(await this.absorb(response, depth, k));
      // Marked only after the bands are in. A region marked covered before its points are held
      // would let the next plan subtract ground whose data never arrived.
      if (this.opts.cache !== false && k > 0) {
        this.cache.markCovered(pieces[i]!, depth, this.contentKey, k);
      }
    }

    if (plan.fetch.length === 0 && this.dueForRevalidation()) {
      // Everything is held, so the only thing left to refresh is the number channel and the content
      // key — which is what keeps the staleness bound reachable for a client panning entirely from
      // its replica (`delta-serving.md` §8).
      const revalidatedAt = performance.now();
      const bbox = rectToRequestBbox(want, depth, this.quantisation);
      response = await this.fetchViewport(
        {slice: this.opts.slice, zoom: depth, bbox, k: 0},
        signal
      );
      this.observe(response);
      // Made visible because its *absence* is the finding that matters: the 2026-08-10 review
      // showed the caller's covered-view path starves this branch entirely, so a trace with zero
      // `revalidate` events over minutes of settled panning is the staleness bound failing.
      this.opts.onPhase?.('revalidate', performance.now() - revalidatedAt, 1);
    }

    // With the store bypassed there is nothing to draw from but this response. The mode has to stay
    // renderable — it is the measurement A/B, not a way to turn the client off.
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

  /**
   * Take a response into the store.
   *
   * A response under a content key different from the held one **replaces** the bands it covers
   * rather than merging into them, which {@link BandCache.put} does by construction. Merging would
   * let an item suppressed since the held band was fetched survive into a band the client now marks
   * fresh (`delta-serving.md` §7).
   */
  dueForRevalidation(): boolean {
    const after = this.opts.revalidateAfterMs ?? 60_000;
    return this.now() - this.validatedAt >= after;
  }

  /**
   * Take a response's coordinates without taking its points.
   *
   * A counts-only response carries no marks, so there is nothing to absorb — but its validator is
   * the whole point of having asked, and observing it is what bounds staleness.
   *
   * **A moved identity coordinate empties the store**, whatever the caller did or did not do about
   * the token. That is the belt to `reset`'s braces: the partition key comes from the server, so a
   * client cannot hold one principal's bands under another's by forgetting to call anything.
   */
  private observe(response: ViewportResponse): void {
    if (response.identityKey !== this.identityKey) {
      this.cache.dropIdentity();
      this.identityKey = response.identityKey;
    }
    this.contentKey = response.contentKey;
    this.validatedAt = this.now();
  }

  /**
   * Split a response into bands and store them — in slices, so a frame can paint in between.
   *
   * Splitting was the last big block of per-response main-thread work: 18.5 ms mean, 49 ms max,
   * landing in the same frame as deriving and uploading — which is where the p95 frame time lived.
   * The work cannot leave this thread (bands must be copies, and 10^4 of them will not transfer to
   * a worker cheaply), but nothing requires it to happen in one frame: each slice runs for
   * {@link ABSORB_SLICE_MS}, then yields a macrotask so a queued animation frame draws.
   *
   * The coverage invariant is unchanged: the caller marks a region covered only after this
   * resolves, so a redraw between slices sees the arriving bands as extra exact ground and the rest
   * still answered by stand-ins — never a hole.
   */
  private async absorb(response: ViewportResponse, depth: number, k: number): Promise<Band[]> {
    this.observe(response);
    const contentKey = this.contentKey;

    const splitter = bandSplitter(response.result, depth, {
      identityKey: this.identityKey,
      contentKey,
      capUsed: k,
      now: this.now()
    });
    const bands: Band[] = [];
    let splitMs = 0;
    let storeMs = 0;
    let slices = 0;
    while (!splitter.done()) {
      const started = performance.now();
      const slice = splitter.step(started + ABSORB_SLICE_MS);
      splitMs += performance.now() - started;
      // Stored slice by slice: a redraw between slices then draws what has arrived so far, which
      // is strictly more picture, not less.
      const stored = performance.now();
      if (this.opts.cache !== false) {
        for (const band of slice) this.cache.put(band);
      }
      for (const band of slice) bands.push(band);
      storeMs += performance.now() - stored;
      slices++;
      if (!splitter.done()) await yieldToFrame();
    }
    this.opts.onPhase?.('split', splitMs, bands.length);
    this.opts.onPhase?.('store', storeMs, slices);

    if (this.opts.cache !== false && bands.length > 0) {
      this.cache.evict({depth, prefix: bands[0]!.prefix});
    }
    return bands;
  }

}

export type {Band, Resolved};
