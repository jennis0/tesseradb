import {BandCache, bandsOfResult, type Band, type Resolved} from './bands.js';
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
};

/** What one {@link Replica.fetchRegion} call resolved to. */
export type ReplicaFrame = {
  depth: number;
  /** The region asked about, in tile-index space at `depth`. */
  want: TileRect;
  /** Bands at this depth inside the region — the served set, drawable with counts. */
  exact: Band[];
  /**
   * Bands from another depth, admitted only over the part of the region not held at this one. A
   * superset of what the definition serves there: drawn, stale-marked, and never counted.
   */
  fallback: Band[];
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
    /** How many requests that took — one per novel rectangle. */
    requests: number;
  };
};

type PendingAsk = {
  prefix: bigint;
  resolve: (band: Resolved | null) => void;
};

export class Replica {
  private readonly cache: BandCache;
  private readonly now: () => number;
  private identityKey = '';
  private contentKey = '';
  private validatedAt = Number.NEGATIVE_INFINITY;
  private pending: PendingAsk[] = [];
  private flush: Promise<void> | null = null;

  constructor(
    private readonly fetchViewport: (
      req: {
        slice: string;
        zoom: number;
        bbox?: [number, number, number, number];
        tiles?: bigint[];
        k?: number;
      },
      signal?: AbortSignal
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

  /** The byte budget the store was given, so a caller can size its look-ahead against it. */
  get budgetBytes(): number {
    return this.opts.cacheBytes ?? 512 * 1024 * 1024;
  }

  /** The content coordinate last observed, for a caller that wants to stale-mark against it. */
  get currentContentKey(): string {
    return this.contentKey;
  }

  /**
   * Ask for one tile. Asks made in the same microtask are answered by one request.
   *
   * Resolves to `null` where the tile holds nothing — an empty tile is a legitimate answer, and is
   * distinct from one that could not be fetched.
   */
  tile(prefix: bigint, depth: number, k: number): Promise<Resolved | null> {
    return new Promise((resolve) => {
      this.pending.push({prefix, resolve});
      this.scheduleFlush(depth, k);
    });
  }

  /**
   * Ask for a tile set, answering from the store where it can and issuing at most one request for
   * the rest.
   *
   * The request names the tiles it actually needs, so a tile the store already holds is not merely
   * discarded on arrival — it is never derived, counted, selected or gathered. That is what makes
   * server work scale with what is new rather than with the area on screen.
   */
  async fetchRegion(
    want: TileRect,
    depth: number,
    k: number,
    signal?: AbortSignal
  ): Promise<ReplicaFrame> {
    const plan =
      this.opts.cache === false
        ? {fetch: [want], wanted: rectArea(want), novel: rectArea(want)}
        : this.cache.planRegion(want, depth, this.contentKey, k);

    let response: ViewportResponse | null = null;
    let fetched: Band[] = [];

    // **One request per novel rectangle, not one per tile.** A pan yields a single strip, so this
    // is one request in the common case and never more than a handful — `rectSubtractAll` bounds
    // the fragmentation rather than letting it grow with the number of past fetches.
    for (const rect of plan.fetch) {
      const bbox = rectToRequestBbox(rect, depth, this.quantisation);
      response = await this.fetchViewport({slice: this.opts.slice, zoom: depth, bbox, k}, signal);
      fetched = fetched.concat(this.absorb(response, depth, k));
      // Marked only after the bands are in. A region marked covered before its points are held
      // would let the next plan subtract ground whose data never arrived.
      if (this.opts.cache !== false && k > 0) {
        this.cache.markCovered(rect, depth, this.contentKey, k);
      }
    }

    if (plan.fetch.length === 0 && this.dueForRevalidation()) {
      // Everything is held, so the only thing left to refresh is the number channel and the content
      // key — which is what keeps the staleness bound reachable for a client panning entirely from
      // its replica (`delta-serving.md` §8).
      const bbox = rectToRequestBbox(want, depth, this.quantisation);
      response = await this.fetchViewport(
        {slice: this.opts.slice, zoom: depth, bbox, k: 0},
        signal
      );
      this.observe(response);
    }

    // With the store bypassed there is nothing to draw from but this response. The mode has to stay
    // renderable — it is the measurement A/B, not a way to turn the client off.
    const {exact, fallback} =
      this.opts.cache === false
        ? {exact: fetched, fallback: [] as Band[]}
        : this.cache.bandsForRegion(want, depth, this.contentKey, k);

    return {
      depth,
      want,
      exact,
      fallback,
      response,
      plan: {wanted: plan.wanted, novel: plan.novel, requests: plan.fetch.length}
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
  private dueForRevalidation(): boolean {
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

  private absorb(response: ViewportResponse, depth: number, k: number): Band[] {
    this.observe(response);
    const contentKey = this.contentKey;

    const bands = bandsOfResult(response.result, depth, {
      identityKey: this.identityKey,
      contentKey,
      capUsed: k,
      now: this.now()
    });
    if (this.opts.cache === false) return bands;

    for (const band of bands) this.cache.put(band);
    if (bands.length > 0) {
      this.cache.evict({depth, prefix: bands[0]!.prefix});
    }
    return bands;
  }

  private scheduleFlush(depth: number, k: number): void {
    if (this.flush) return;
    this.flush = Promise.resolve().then(async () => {
      const batch = this.pending;
      this.pending = [];
      this.flush = null;
      try {
        // **Coalesced into the bounding rectangle of the batch.** A tile-addressed consumer asks
        // for a contiguous viewport, so the bound is tight; where it is not, the surplus is ground
        // the consumer is about to ask for anyway. Answering each ask from the store afterwards is
        // what keeps this a fetch of a region and a resolution per tile, rather than both per tile.
        let rect: TileRect | null = null;
        for (const ask of batch) {
          const {x, y} = tileXY(ask.prefix, depth);
          rect = rect
            ? {
                x0: Math.min(rect.x0, x),
                y0: Math.min(rect.y0, y),
                x1: Math.max(rect.x1, x),
                y1: Math.max(rect.y1, y)
              }
            : {x0: x, y0: y, x1: x, y1: y};
        }
        if (rect) await this.fetchRegion(rect, depth, k);
        for (const ask of batch) ask.resolve(this.cache.resolve(depth, ask.prefix));
      } catch {
        // A failed batch resolves every ask to null rather than rejecting each: a tile-addressed
        // consumer treats a null tile as "not yet", and rejecting would surface one transport
        // failure as N unhandled rejections.
        for (const ask of batch) ask.resolve(null);
      }
    });
  }
}

export type {Band, Resolved};
