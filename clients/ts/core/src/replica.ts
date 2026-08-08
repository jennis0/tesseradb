import {BandCache, bandsOfResult, type Band, type Resolved} from './bands.js';
import {tileToCellBox, tileXY, CELL_GRID} from './coords.js';
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

/** What one `fetchTiles` call resolved to, tile by tile, with the provenance the renderer needs. */
export type ReplicaFrame = {
  depth: number;
  tiles: {prefix: bigint; resolved: Resolved}[];
  /** Tiles asked for that neither the store nor the response could answer. */
  missing: bigint[];
  /** Null when every tile was answered from the store. */
  response: ViewportResponse | null;
  /**
   * How the ask was split — the cache's effectiveness, made visible rather than inferred.
   *
   * `omitted` is what the store proved it already held and so never asked for; that number is the
   * whole point of the replica, and a client showing marks while this stays at zero has a cache
   * that is costing memory and buying nothing.
   */
  plan: {omitted: number; fetched: number};
};

type PendingAsk = {
  prefix: bigint;
  resolve: (band: Resolved | null) => void;
};

/**
 * The identity coordinate, and a conservative stand-in for the content coordinate.
 *
 * **⊘ Neither is served by the engine yet.** `delta-serving.md` §2 specifies an `identity_key` over
 * the idset, auth-data hash, fragment identity and slice, and a `content_key` over the watermark of
 * the geometry served plus the overlay version, delivered as an entity tag. Until those exist:
 *
 * - identity is keyed on `(tokenId, slice)`, which is sound for the partition it governs — a token
 *   binds one principal to one authorisation — but does not notice a key rotation, so a client must
 *   still drop its store on re-authorisation, which {@link Replica.setSession} does;
 * - content is keyed on `x-tessera-pin`, the advisory geometry stamp. It moves on every flush *and*
 *   on every merge and compaction, so it **over-rotates**: declarations lapse where the real content
 *   coordinate would have held them. That costs bytes and never correctness, which is the right
 *   direction for a stand-in, and it is why no completeness claim here can outlive a flush.
 */
function contentKeyOf(response: ViewportResponse): string {
  return response.pin ?? 'unpinned';
}

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
      req: {slice: string; zoom: number; bbox: [number, number, number, number]; k?: number},
      signal?: AbortSignal
    ) => Promise<ViewportResponse>,
    private readonly quantisation: Quantisation,
    private readonly opts: ReplicaOptions
  ) {
    this.cache = new BandCache(opts.cacheBytes ?? 512 * 1024 * 1024);
    this.now = opts.now ?? (() => performance.now());
  }

  /**
   * Bind the store to a principal. A different one drops everything held, so cross-principal reuse
   * is impossible by construction rather than by discipline (`client-interaction.md` §10).
   */
  setSession(tokenId: number): void {
    const key = `${tokenId}:${this.opts.slice}`;
    if (key !== this.identityKey) {
      this.cache.dropIdentity();
      this.identityKey = key;
      this.contentKey = '';
      this.validatedAt = Number.NEGATIVE_INFINITY;
    }
  }

  get bytes(): number {
    return this.cache.bytes;
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
   * **⊘ The request is still bbox-shaped.** `delta-serving.md` §13's tile-list operand does not
   * exist yet, so the tiles that cannot be answered from the store are covered by their bounding
   * box — a superset, so the answer is correct and the elision is only as good as the box is tight.
   * Passing the list itself is what makes server work scale with novelty, and is Stage 2's change.
   */
  async fetchTiles(
    tiles: bigint[],
    depth: number,
    k: number,
    signal?: AbortSignal
  ): Promise<ReplicaFrame> {
    const plan = this.opts.cache === false
      ? {omit: [], fetch: tiles.map((prefix) => ({prefix, below: 0n, count: 0}))}
      : this.cache.plan(tiles, depth, this.contentKey, k);

    let response: ViewportResponse | null = null;
    let fetched: Band[] = [];
    if (plan.fetch.length > 0) {
      response = await this.fetchViewport(
        {
          slice: this.opts.slice,
          zoom: depth,
          bbox: boundingBbox(plan.fetch.map((f) => f.prefix), depth, this.quantisation),
          k
        },
        signal
      );
      fetched = this.absorb(response, depth, k);
      // Every tile asked for that did not come back holds nothing: the engine omits a tile whose
      // visible count is zero. Recording that is what stops a mostly-empty viewport re-asking for
      // its empty tiles on every single view.
      if (this.opts.cache !== false) {
        const returned = new Set(fetched.map((b) => b.prefix));
        for (const {prefix} of plan.fetch) {
          if (!returned.has(prefix)) this.cache.markEmpty(depth, prefix, this.contentKey);
        }
      }
    } else if (this.dueForRevalidation()) {
      response = await this.fetchViewport(
        {
          slice: this.opts.slice,
          zoom: depth,
          bbox: boundingBbox(tiles, depth, this.quantisation),
          k: 0
        },
        signal
      );
      this.observe(response);
    }

    // With the store bypassed there is nothing to resolve against, so the frame is built from the
    // response alone. The mode has to stay renderable — it is the measurement A/B, not a way to
    // turn the client off.
    const answer = this.opts.cache === false
      ? (prefix: bigint): Resolved | null => {
          const band = fetched.find((b) => b.prefix === prefix);
          return band ? {provenance: 'exact', bands: [band], exact: true} : null;
        }
      : (prefix: bigint): Resolved | null => this.cache.resolve(depth, prefix);

    const resolvedTiles: ReplicaFrame['tiles'] = [];
    const missing: bigint[] = [];
    for (const prefix of tiles) {
      const resolved = answer(prefix);
      if (resolved) resolvedTiles.push({prefix, resolved});
      else missing.push(prefix);
    }
    return {
      depth,
      tiles: resolvedTiles,
      missing,
      response,
      plan: {omitted: plan.omit.length, fetched: plan.fetch.length}
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
   * Take a response's content coordinate without taking its points.
   *
   * A counts-only response carries no marks, so there is nothing to absorb — but its validator is
   * the whole point of having asked, and observing it is what bounds staleness.
   */
  private observe(response: ViewportResponse): void {
    this.contentKey = contentKeyOf(response);
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
        const frame = await this.fetchTiles(batch.map((a) => a.prefix), depth, k);
        const byPrefix = new Map(frame.tiles.map((t) => [t.prefix, t.resolved]));
        for (const ask of batch) ask.resolve(byPrefix.get(ask.prefix) ?? null);
      } catch {
        // A failed batch resolves every ask to null rather than rejecting each: a tile-addressed
        // consumer treats a null tile as "not yet", and rejecting would surface one transport
        // failure as N unhandled rejections.
        for (const ask of batch) ask.resolve(null);
      }
    });
  }
}

/** The data-space box covering a tile set at one depth. */
function boundingBbox(
  tiles: bigint[],
  depth: number,
  q: Quantisation
): [number, number, number, number] {
  let cx0 = Infinity;
  let cy0 = Infinity;
  let cx1 = -Infinity;
  let cy1 = -Infinity;
  for (const prefix of tiles) {
    const {x, y} = tileXY(prefix, depth);
    const cells = tileToCellBox({x, y, z: depth});
    cx0 = Math.min(cx0, cells.cx0);
    cy0 = Math.min(cy0, cells.cy0);
    cx1 = Math.max(cx1, cells.cx1);
    cy1 = Math.max(cy1, cells.cy1);
  }
  const spanX = q.xMax - q.xMin;
  const spanY = q.yMax - q.yMin;
  // Inset to cell centres, for the reason `tileToRequestBbox` documents at length: the server's
  // bbox is closed, so a corner on a tile boundary selects the neighbouring row and column too.
  return [
    q.xMin + ((cx0 + 0.5) / CELL_GRID) * spanX,
    q.yMin + ((cy0 + 0.5) / CELL_GRID) * spanY,
    q.xMin + ((cx1 - 0.5) / CELL_GRID) * spanX,
    q.yMin + ((cy1 - 0.5) / CELL_GRID) * spanY
  ];
}

export type {Band, Resolved};
