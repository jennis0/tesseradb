import {
  checkTrailerCounts,
  decodeViewport,
  parseTrailer,
  type PointsPart,
  type ViewportHead
} from './decode.js';
import {createDecoder, type Decoder, type HeadFrames} from './decoder.js';
import {parseRegionVerdict} from './region.js';
import {FRAME_ARTIFACTS, FRAME_POINTS, FRAME_SUB_CELLS, FRAME_TILES, FRAME_TRAILER, FrameReader} from './frame.js';
import type {ArrowType, ArtifactDetail, CategoryValue, FilterOperandSet, ItemDetail, Layer, Meta, ProjectionName, Session, Shape, ShapeKind, TileCounts, TileScheme, ViewMetadataValue, ViewportPart, ViewportRequest, ViewportResponse, ViewportResult} from './types.js';

/** Where a streamed response's points go, one frame's worth at a time. */
export type PartSink = (part: ViewportPart) => void | Promise<void>;

/** What either decode path hands back, before the headers are folded in around it. */
type Decoded = {
  result: ViewportResult;
  bytes: number;
  points: number;
  ms: number;
  workerMs: number | null;
};

/**
 * The point columns of a response that carried none — every streamed response's own result.
 *
 * Fresh buffers each time rather than one shared empty set: a caller that holds a result holds
 * these, and two results sharing a `scalars` object is an aliasing fault waiting for the day
 * something writes to one.
 */
function emptyPoints() {
  return {
    ids: new BigUint64Array(0),
    codes: new BigUint64Array(0),
    positions: new Float64Array(0),
    world: new Float32Array(0),
    scalars: {} as Record<string, never>,
    membership: {} as Record<string, never>
  };
}

/** The same result with its points removed — they were delivered to the sink instead. */
function headOnly(result: ViewportResult): ViewportResult {
  return {...result, ...emptyPoints()};
}

/**
 * The run of tiles whose served counts add up to one points frame's rows.
 *
 * **A chunk boundary never splits a tile** (`streamed-serving.md` §2), so the frame's rows are
 * exactly some run of the tiles batch and the run is found by adding up the counts the server
 * already sent. A frame whose rows land inside a tile is a wire the client cannot attribute, and
 * it refuses rather than assigning the points to a tile they may not belong to.
 */
function tilesFor(tiles: readonly TileCounts[], from: number, rows: number): {tiles: TileCounts[]; next: number} {
  let at = from;
  let sum = 0;
  while (at < tiles.length && sum < rows) sum += Number(tiles[at++]!.served);
  if (sum !== rows) {
    throw new Error(
      `a points frame of ${rows} rows does not end on a tile boundary (tiles ${from}..${at} serve ${sum})`
    );
  }
  return {tiles: tiles.slice(from, at), next: at};
}

/**
 * A Tessera error body, `{"error": code, "detail": string}`, with its HTTP status.
 *
 * Typed rather than a bare `Error` because the viewer must be able to tell a refusal from a
 * transport failure: an empty region and a failed region are semantic opposites, and only a typed
 * error lets a caller render them differently.
 */
export class TesseraError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    readonly detail: string
  ) {
    super(`${status} ${code}: ${detail}`);
    this.name = 'TesseraError';
  }
}

async function fail(response: Response): Promise<never> {
  let code = 'unknown';
  let detail = response.statusText;
  try {
    const body = (await response.json()) as {error?: string; detail?: string};
    code = body.error ?? code;
    detail = body.detail ?? detail;
  } catch {
    // A non-JSON body (a proxy's, say) still deserves a typed error rather than a parse crash.
  }
  throw new TesseraError(response.status, code, detail);
}

export type TesseraClientOptions = {
  viewerUrl: string;
  sessionUrl: string;
  /**
   * Needed only by `authorise`. Holding it in a browser is a development shape — see
   * `crates/tessera-server/src/cors.rs` for why the server key that permits it is off unless
   * typed, and client-interaction §7 for the topology that is actually recommended.
   */
  sessionCredential?: string;
  /**
   * Per-response decode time — an instrument's hook (design §5.10's measurements), never a
   * behaviour: `ms` from bytes-in to typed-arrays-out as seen from this thread, with the
   * response's size and point count, and `workerMs` — the worker's own decode time, so the
   * difference is what the response spent queued behind another in its lane (`null` where it
   * decoded inline).
   *
   * **On a streamed response `ms` is head-to-last-part and so includes the wire**, there being no
   * moment at which the bytes are all in hand and none of them decoded; `workerMs` is then the sum
   * of the frames' own decode times, and is the decode figure.
   */
  onDecode?: (ms: number, bytes: number, points: number, workerMs: number | null) => void;
  /**
   * Override where responses are decoded. Defaults to a worker in a browser, inline elsewhere.
   *
   * Exists for tests and for a consumer that already owns a worker pool — not as a switch anyone
   * needs to think about.
   */
  decoder?: Decoder;
};

/**
 * The five viewer/session verbs, and nothing else.
 *
 * No cache, no view key, no session lifetime, no replica state — client-interaction §10's session
 * client layer, which is what a REST user would have written anyway. The replica store goes
 * *above* this, not inside it, so that this file stays a thing you can read in one sitting and
 * check against the contracts spec.
 */
export class TesseraClient {
  /**
   * Where responses are turned into typed arrays.
   *
   * Created lazily and shared across requests. In a browser this is a worker, so decode does not
   * compete with drawing; everywhere else it is the same synchronous call this always made.
   */
  private decoder: Decoder | null = null;

  constructor(private readonly opts: TesseraClientOptions) {}

  /** Release the decode worker, if one was created. */
  close(): void {
    this.decoder?.close();
    this.decoder = null;
  }

  async authorise(terms: string[]): Promise<Session> {
    if (!this.opts.sessionCredential) {
      throw new Error('authorise needs a sessionCredential');
    }
    const authData = btoa(JSON.stringify({terms}));
    const response = await fetch(`${this.opts.sessionUrl}/session/authorise`, {
      method: 'POST',
      headers: {
        authorization: `Bearer ${this.opts.sessionCredential}`,
        'content-type': 'application/json'
      },
      body: JSON.stringify({auth_data: authData})
    });
    if (!response.ok) await fail(response);
    const body = (await response.json()) as {token: string; token_id: number; expires_at: number};
    return {token: body.token, tokenId: body.token_id, expiresAt: body.expires_at};
  }

  async meta(token: string): Promise<Meta> {
    const response = await fetch(`${this.opts.viewerUrl}/v1/meta`, {
      headers: {authorization: `Bearer ${token}`}
    });
    if (!response.ok) await fail(response);
    const m = (await response.json()) as RawMeta;
    return {
      apiVersion: m.api_version,
      idset: m.idset,
      // The frame and the four projection fields ride with the view they describe, because that
      // is where the server declares them: both belong to a view, and two views of one bundle may
      // quantise and project differently (decision 0040). `projection.ts` is what a client does
      // with them.
      views: m.views.map((s) => ({
        id: s.id,
        displayName: s.display_name,
        quantisation: {
          xMin: s.quantisation.x_min,
          xMax: s.quantisation.x_max,
          yMin: s.quantisation.y_min,
          yMax: s.quantisation.y_max
        },
        projection: s.projection,
        worldAspect: s.world_aspect,
        tileScheme: s.tile_scheme,
        tile: s.tile,
        // The three roster fields are one record on the wire and one object here — a plain view
        // has all three null, and `group` alone decides which case this is (`views.md` §3.2).
        roster: s.group === null ? null : {group: s.group, key: s.key!, metadata: s.metadata ?? {}}
      })),
      // The orderings, so a client can offer previous-and-next without interpreting a key. Empty
      // is what a deployment of plain views alone publishes, and it wants the same rendering as
      // "no groups here" — no group picker.
      groups: m.groups.map((g) => ({
        name: g.name,
        membersOf: g.members_of,
        views: g.views
      })),
      declaredScalars: m.declared_scalars.map((s) => ({
        name: s.name,
        arrowType: s.arrow_type,
        // Null for a plain column, and the absence is the whole signal: without it a `u16`
        // category is indistinguishable from a `u16` integer, since the hot path ships the code.
        category: s.category
          ? {vocabulary: s.category.vocabulary, kind: s.category.kind, visibility: s.category.visibility}
          : null,
        render: s.render,
        index: s.index
      })),
      selection: {
        kMin: m.selection.k_min,
        kMaxMarks: m.selection.k_max_marks,
        maxK: m.selection.max_k,
        thetaTargetMarks: m.selection.theta_target_marks,
        maxUnderlayOffset: m.selection.max_underlay_offset,
        maxCategoryValues: m.selection.max_category_values ?? 1_000,
        maxRegionVertices: m.selection.max_region_vertices ?? 10_000,
        maxRegionCells: m.selection.max_region_cells ?? 262_144
      },
      // Older servers do not publish it; fall back to the documented default rather than
      // refusing to run against them.
      maxTilesPerRequest: m.selection.max_tiles_per_request ?? 262_144,
      // Absent, not merely empty, when the schema declares nothing filterable — so the fallback is
      // the empty list and a client draws no filter controls, which is the correct rendering of a
      // bundle that has none.
      filterOperands: (m.filter_operands ?? []).map((f) => ({
        column: f.column,
        family: f.family,
        operands: f.operands,
        // Absent for an entity-scoped column, which is every column of a bundle that declares no
        // view group — so the field is omitted rather than nulled, and a client that never met a
        // scoped attribute reads the list exactly as it did before.
        ...(f.scope ? {scope: {group: f.scope.group}} : {})
      })),
      // Gate-filtered by the server, so this list *is* what this principal may know about — and
      // the empty list is the honest rendering of both "no layers here" and "none you may reach".
      layers: (m.layers ?? []).map((l) => ({
        name: l.name,
        title: l.title,
        views: l.views,
        membership: l.membership,
        hierarchy: {kind: l.hierarchy.kind, pruneChildren: l.hierarchy.prune_children},
        levels: l.levels.map((v) => ({level: v.level, title: v.title, zoom: v.zoom ?? null})),
        computedContent: l.computed_content,
        // The kind of the layer's one drawn geometry, or null; a server that publishes none is
        // a server older than the shape columns, and the map then draws boxes for good.
        shape: l.shape ?? null,
        suppliedContent: l.supplied_content,
        depsOn: l.depends_on,
        version: l.version
      }))
    };
  }

  /**
   * `k` is omitted from the body unless the caller sets it, so the deployment's own ceiling is the
   * default — contracts §3.2's rule, and the reason a caller who never mentions `k` cannot
   * decrease it.
   */
  async viewport(
    token: string,
    req: ViewportRequest,
    signal?: AbortSignal,
    /** Route decode to the speculative lane — see {@link Decoder.decode}. */
    background = false,
    /**
     * Take each points frame as it lands, rather than the whole response at the end.
     *
     * **With a sink the returned response carries no points**: every one of them went to the sink,
     * and handing them over twice would double both the memory and the work. Without one the
     * response is what it always was. `k = 0` has no points frames and ignores this.
     */
    onPart?: PartSink
  ): Promise<ViewportResponse> {
    const body: Record<string, unknown> = {view: req.view, zoom: req.zoom};
    if (req.bbox) body.bbox = req.bbox;
    // JSON has no 64-bit integer, and a Morton prefix at depth 16 needs 32 bits — inside `Number`'s
    // exact range, so the narrowing is lossless here and stays so for every depth the grid allows.
    if (req.tiles) body.tiles = req.tiles.map(Number);
    if (req.k !== undefined) body.k = req.k;
    if (req.underlayOffset) body.underlay_offset = req.underlayOffset;
    // Omitted when null, which is the unfiltered request. An empty `all_of` would *also* be
    // unfiltered, but sending one makes every caller's "no filters" state a distinct request shape
    // from the one a caller who never mentioned filters sends — and a cache keyed on the body would
    // then hold two entries for one question.
    if (req.filters) body.filters = req.filters;
    // Sent exactly as given, `[]` included: the wire is `string[] | 'all'`, where `[]` (or absent)
    // is "no layers, charge me nothing", `'all'` is "every layer I reach", and an array is those ∩
    // the reachable set. The server's old convention that absent meant *all* is gone; this sends
    // what the caller passed and never substitutes one for the other.
    if (req.layers !== undefined) body.layers = req.layers;
    if (req.artifactBudget !== undefined) body.artifact_budget = req.artifactBudget;
    // Sent only when named, so the default stays the server's own (`"full"`) and a caller who
    // never mentions it sends the request shape it always sent.
    if (req.artifactRows !== undefined) body.artifact_rows = req.artifactRows;
    // **Absent is a real selection here, not a missing one.** Omitting `levels` asks the server to
    // follow each layer's own declared zoom ranges against this request's depth, so a client that
    // never thinks about levels gets the one a map would draw. Sent only when the caller named
    // something, so "follow the declaration" and "give me these" stay distinct request shapes.
    if (req.levels !== undefined) body.levels = req.levels;
    // **Absent is the layer's declaration and an empty array is *none***, so this is sent only
    // when the caller named something — an omitted field and `[]` mean opposite things here.
    if (req.computed !== undefined) body.computed = req.computed;
    // The stamp travels as the parsed object the server sent, under the wire name `pin`. Kept as
    // an opaque string on this side so a client never has to know its shape.
    if (req.stamp) body.pin = JSON.parse(req.stamp);

    const response = await fetch(`${this.opts.viewerUrl}/v1/viewport`, {
      method: 'POST',
      headers: {authorization: `Bearer ${token}`, 'content-type': 'application/json'},
      body: JSON.stringify(body),
      signal
    });
    if (!response.ok) await fail(response);
    const stage = response.headers.get('x-tessera-stage-ns');
    const coordinates = {
      identityKey: response.headers.get('x-tessera-identity-key') ?? '',
      // Unquoted here: the quotes are HTTP's entity-tag syntax, not part of the value, and every
      // comparison this client makes is against another value it took from this same header.
      contentKey: (response.headers.get('etag') ?? '').replace(/^"|"$/g, ''),
      pin: response.headers.get('x-tessera-pin'),
      stale: response.headers.get('x-tessera-stale') === '1',
      // Absent when the request carried no region leaf; `exact` or a cover at a depth otherwise
      // (`selection-operand.md` §6). A header, so a counts-only reader sees it without a decode.
      region: parseRegionVerdict(response.headers.get('x-tessera-region'))
    };
    this.decoder ??= this.opts.decoder ?? createDecoder();
    const decodeStarted = performance.now();
    // **A counts-only response decodes on this thread.** `k = 0` carries tiles and artifacts and
    // no points (contracts §3.2) — a few kilobytes to a couple of megabytes of fixed-width rows,
    // milliseconds to decode — and the worker lanes are serial: measured on the demo, a region's
    // count queued 7.9 s behind a million-point decode in the lane it was dealt, for a response
    // the server answered in 5 ms. The channel's and the region's asks are exactly the requests
    // whose latency the user is waiting on, so they never queue behind a point sweep. It has no
    // points frames either, so there is nothing for a part sink to be handed.
    const counts = req.k === 0;
    const decoded =
      onPart && !counts
        ? await this.streamed(
            response,
            {identityKey: coordinates.identityKey, contentKey: coordinates.contentKey},
            onPart,
            background
          )
        : await this.whole(response, counts, background);
    this.opts.onDecode?.(decoded.ms, decoded.bytes, decoded.points, decoded.workerMs);
    return {
      result: decoded.result,
      timings: {
        // Time-to-**first-flush**, not the whole response (`streamed-serving.md` §6): the server's
        // own cost is the sweep, and the trailer's `stream_us` — which includes every wait on this
        // client's own reading — is deliberately not this number.
        serverUs: Number(response.headers.get('x-tessera-server-us') ?? 0),
        admissionUs: Number(response.headers.get('x-tessera-admission-us') ?? 0),
        stageNs: stage ? stage.split(',').map(Number) : null
      },
      ...coordinates,
      bytes: decoded.bytes
    };
  }

  /** The whole body, then the decoder — what a caller holding no part sink gets. */
  private async whole(
    response: Response,
    counts: boolean,
    background: boolean
  ): Promise<Decoded> {
    const started = performance.now();
    const bytes = new Uint8Array(await response.arrayBuffer());
    // Read BEFORE decode: the worker path transfers the buffer zero-copy, which detaches it —
    // `byteLength` afterwards is 0, and every byte ledger downstream (the anticipation budget,
    // the traces, the ring-spend measurement) silently read that zero.
    const size = bytes.byteLength;
    const result = counts ? decodeViewport(bytes) : await this.decoder!.decode(bytes, background);
    return {
      result,
      bytes: size,
      points: result.ids.length,
      ms: performance.now() - started,
      workerMs: counts ? null : this.decoder!.lastWorkerMs
    };
  }

  /**
   * Consume the body as it arrives, landing each points frame the moment it is whole.
   *
   * **Slow data should cause pop-in, not lag.** A wide view is around a hundred point frames and a
   * hundred megabytes; reading the body to its end before decoding the first frame means nothing
   * is drawn until the last byte has landed, and then every tile arrives at once — the wire
   * streams and the client waits. The server already emits whole tiles per frame, in tile order,
   * each frame an independently decodable Arrow stream (`streamed-serving.md` §2, §3), which is
   * precisely the licence to decode and draw one as it completes.
   *
   * The tiles frame comes first, so each part carries the run of tile counts its own points
   * satisfy — walked off the counts the server already sent, since a chunk boundary never splits a
   * tile. A part is therefore a set of *whole* bands, not a fragment of one, and the replica
   * stores it exactly as it stores a whole response.
   *
   * **What ends the response is the trailer, not the socket.** A body that stops without one is
   * incomplete by contract and throws here as it always did (`streamed-serving.md` §6); what the
   * caller has already landed stays landed and stays sound — every part came from the one
   * generation snapshot and each tile's points are an id-order prefix of `served(T)` — but the
   * request never resolves, so nothing downstream marks the region covered.
   */
  private async streamed(
    response: Response,
    coordinates: {identityKey: string; contentKey: string},
    onPart: PartSink,
    background: boolean
  ): Promise<Decoded> {
    const started = performance.now();
    if (!response.body) {
      // A runtime whose `fetch` gives no stream — a polyfill, a mocked transport. The sink is
      // still handed everything, in one piece; the difference is when, never what.
      const whole = await this.whole(response, false, background);
      await onPart({result: whole.result, ...coordinates});
      return {...whole, result: headOnly(whole.result)};
    }
    const reader = response.body.getReader();
    const frames = new FrameReader();
    const head: HeadFrames = {tiles: new Uint8Array(0), subCells: null, artifacts: null};
    let decodingHead: Promise<ViewportHead> | null = null;
    let trailerBytes: Uint8Array | null = null;
    let bytes = 0;
    let flushes = 0;
    let points = 0;
    // Accumulated across the frames, and left null where the decoder measures nothing (the inline
    // one, whose calls *are* the work) so a zero is never reported as a measurement.
    let workerMs: number | null = null;
    // The tiles frame's rows, consumed in step with the point frames that satisfy them.
    let tileAt = 0;
    // Parts are delivered in wire order however the decode lanes deal the frames, by awaiting
    // each frame's decode in turn. The chain also carries the sink's own backpressure: reading
    // continues while a part is absorbed, so the socket is never held for the absorb lane, but a
    // second part is never handed over before the first has been taken.
    let delivering: Promise<void> = Promise.resolve();
    // Nobody awaits this chain until the body has been read, and a rejection with no handler in
    // between is an unhandled rejection rather than this call's failure.
    delivering.catch(() => {});

    // Set the moment the read fails — an abort, a transport fault, a frame the grammar refuses.
    // Nothing lands after it: a request the caller has abandoned must not keep filling a store
    // behind the view that superseded it, which is the discard a whole-body abort got for free.
    let abandoned = false;

    const startHead = () => {
      decodingHead ??= this.decoder!.decodeHead(head, background);
    };
    const deliver = (decoding: Promise<PointsPart>) => {
      delivering = delivering.then(async () => {
        const part = await decoding;
        // Read here, next to the reply it belongs to: the decoder reports the *last* reply's
        // time, and the head's own reply lands on the same counter.
        const frameMs = this.decoder!.lastWorkerMs;
        if (abandoned) return;
        const decodedHead = await decodingHead!;
        if (frameMs !== null) workerMs = (workerMs ?? 0) + frameMs;
        const rows = part.ids.length;
        points += rows;
        const run = tilesFor(decodedHead.tiles, tileAt, rows);
        tileAt = run.next;
        await onPart({
          result: {
            tiles: run.tiles,
            ...part,
            subCells: null,
            // Every part carries the response's artifacts, because that is what a point's
            // membership column is named through (`bands.ts`) and the frame precedes them all.
            artifacts: decodedHead.artifacts,
            artifactsIdentity: decodedHead.artifactsIdentity
          },
          ...coordinates
        });
      });
      delivering.catch(() => {});
    };

    try {
      for (;;) {
        const {done, value} = await reader.read();
        if (done) break;
        bytes += value.byteLength;
        for (const frame of frames.push(value)) {
          switch (frame.kind) {
            case FRAME_TILES:
              head.tiles = frame.payload;
              break;
            case FRAME_SUB_CELLS:
              head.subCells = frame.payload;
              break;
            case FRAME_ARTIFACTS:
              head.artifacts = frame.payload;
              break;
            case FRAME_POINTS:
              // The head is complete at the first points frame — the grammar puts every other
              // frame before it — so this is the earliest the counts and the artifacts can be
              // decoded, and they are decoded before the points they name.
              startHead();
              flushes += 1;
              deliver(this.decoder!.decodePoints(frame.payload, background));
              break;
            case FRAME_TRAILER:
              // A counts-only or empty response has no points frame to have started it.
              startHead();
              trailerBytes = frame.payload;
              break;
          }
        }
      }
    } catch (error) {
      abandoned = true;
      // Releases the body for a fault the transport did not itself raise; an aborted stream is
      // already errored and this is a no-op on it.
      void reader.cancel().catch(() => {});
      throw error;
    }
    // **Every whole frame is handed over before the response is judged.** A body that stopped
    // without its trailer is refused below, and what it did deliver is sound — whole tiles from
    // the one generation snapshot — so the refusal denies the caller a *complete* response, not
    // the points it already has.
    await delivering;
    // Throws on a body that stopped mid-frame or without its trailer, which is what makes a
    // truncated response loud rather than short.
    frames.end();
    const decodedHead = await decodingHead!;
    checkTrailerCounts(parseTrailer(trailerBytes!), flushes, points);
    // Every tile the server said it served points for has had them. The batch decoder gets this
    // for free by walking one array; here the walk is spread over the parts, so its end is
    // checked rather than assumed.
    for (let i = tileAt; i < decodedHead.tiles.length; i++) {
      if (decodedHead.tiles[i]!.served !== 0n) {
        throw new Error(
          `tile ${decodedHead.tiles[i]!.tile} was served ${decodedHead.tiles[i]!.served} points that no frame carried`
        );
      }
    }
    return {
      result: {
        tiles: decodedHead.tiles,
        ...emptyPoints(),
        subCells: decodedHead.subCells,
        artifacts: decodedHead.artifacts,
        artifactsIdentity: decodedHead.artifactsIdentity
      },
      bytes,
      points,
      ms: performance.now() - started,
      workerMs
    };
  }

  /**
   * `GET /v1/categories/{column}`: what this column's codes stand for.
   *
   * Two forms, and **the first is the one to reach for**. Passing `codes` resolves exactly those —
   * which is what a viewer wants, since it knows which codes it drew, and it means a 60,000-value
   * vocabulary never crosses the wire. Omitting them enumerates the whole set, paging until the
   * server stops handing back a cursor.
   *
   * **A code that comes back unresolved is not an error.** "No such code" and "a value you cannot
   * see" are one outcome by contract (contracts §3.2), so the caller gets a shorter list rather
   * than a refusal, and must not treat a missing code as a failure. A code the client actually
   * *drew* always resolves: its point was admitted by the mask, so the value has a visible member.
   *
   * Throws {@link TesseraError} for a real refusal — notably `500 fail-closed` on a `derived`
   * column, whose gate is specified and unbuilt.
   */
  async categories(
    token: string,
    column: string,
    opts: {codes?: readonly number[]; limit?: number} = {}
  ): Promise<CategoryValue[]> {
    // Encoded, because a column name reaches this from `/v1/meta` rather than from a literal.
    const base = `${this.opts.viewerUrl}/v1/categories/${encodeURIComponent(column)}`;
    const out: CategoryValue[] = [];

    if (opts.codes) {
      // Nothing to ask about. Returning early rather than sending `codes=` keeps an empty request
      // from being read as the *enumeration* form, which would fetch the whole vocabulary.
      if (opts.codes.length === 0) return out;
      const url = `${base}?codes=${[...opts.codes].join(',')}`;
      const page = await this.categoryPage(token, url);
      return page.values;
    }

    let cursor: string | null = null;
    do {
      const params = new URLSearchParams();
      if (opts.limit !== undefined) params.set('limit', String(opts.limit));
      if (cursor !== null) params.set('after', cursor);
      const query = params.toString();
      const page = await this.categoryPage(token, query ? `${base}?${query}` : base);
      out.push(...page.values);
      cursor = page.next;
    } while (cursor !== null);
    return out;
  }

  private async categoryPage(
    token: string,
    url: string
  ): Promise<{values: CategoryValue[]; next: string | null}> {
    const response = await fetch(url, {headers: {authorization: `Bearer ${token}`}});
    if (!response.ok) await fail(response);
    const body = (await response.json()) as RawCategories;
    return {
      values: body.values.map((v) => ({code: v.code, key: v.key, title: v.title ?? null})),
      next: body.next
    };
  }

  /**
   * `POST /v1/items/{tessera_id}`: the whole record, by declared column name.
   *
   * **Named, not positional.** The response is an object keyed by column name covering all three
   * homes — rendered columns, indexed and category columns (a category as its vocabulary *key*,
   * already resolved), and blob-resident prose. A column the item carries no value for is **absent
   * from the object** rather than null, so `name in fields` is the presence test and a missing key
   * is a fact about the item rather than a gap in the response.
   *
   * Nothing here can be read positionally against `/v1/meta`'s `declared_scalars`: the absent
   * columns are omitted, so index *i* of the response is not column *i* of the schema.
   *
   * Beside the record, `labels` names the item's access labels **this session satisfies** and no
   * others (decision 0114) — the answer to *which of my grants admits me here*, and not to *what
   * this item is labelled*.
   */
  async item(token: string, tesseraId: bigint): Promise<ItemDetail> {
    const response = await fetch(`${this.opts.viewerUrl}/v1/items/${tesseraId.toString()}`, {
      method: 'POST',
      headers: {authorization: `Bearer ${token}`, 'content-type': 'application/json'},
      body: '{}'
    });
    if (!response.ok) await fail(response);
    const body = (await response.json()) as {
      fields: Record<string, unknown>;
      external_id?: string;
      labels: string[];
    };
    return {
      fields: body.fields ?? {},
      externalId: body.external_id ?? null,
      // **Not defaulted.** `labels` is required by the response schema and is always present,
      // empty included — a principal satisfying none of the item's labels is a real answer with a
      // real shape. A `?? []` here would give a server that omitted the field the same reading as
      // one that answered "none", which is a nonconforming server made to look correct.
      labels: body.labels
    };
  }

  /**
   * `POST /v1/artifacts/{tessera_id}`: one artifact's layer, key and masked count.
   *
   * **`view` is required here and optional on {@link item}**, and the asymmetry is real: a point's
   * record is the same wherever it is read from, but a masked count is an intersection in row
   * space and row space is per view.
   *
   * **`404` is the only failure shape, and it distinguishes nothing.** An identifier naming
   * nothing, one naming a point, one whose layer this principal cannot reach, one suppressed, and
   * one below its layer's existence criterion are the same answer byte for byte. A caller must not
   * build a surface that tells them apart; there is nothing to tell them apart by. It throws
   * {@link TesseraError} like every other refusal.
   */
  async artifact(
    token: string,
    tesseraId: bigint,
    opts: {view: string; idset?: number; zoom?: number}
  ): Promise<ArtifactDetail> {
    const body: Record<string, unknown> = {view: opts.view};
    if (opts.idset !== undefined) body.idset = opts.idset;
    // The depth the shape is drawn at, for the server's vertex rule (`polygon-membership.md`
    // §7.2): a predicate or an authored shape is generalised to the pixel at this zoom. Omitted,
    // the whole presimplified shape is served under the vertex guard alone.
    if (opts.zoom !== undefined) body.zoom = Math.max(0, Math.min(16, Math.floor(opts.zoom)));
    const response = await fetch(`${this.opts.viewerUrl}/v1/artifacts/${tesseraId.toString()}`, {
      method: 'POST',
      headers: {authorization: `Bearer ${token}`, 'content-type': 'application/json'},
      body: JSON.stringify(body)
    });
    if (!response.ok) await fail(response);
    const served = (await response.json()) as {
      layer: string;
      key?: string;
      masked_count: number;
      centroid?: [number, number];
      box?: [number, number, number, number];
      shape?: Shape;
    };
    return {
      layer: served.layer,
      // Absent rather than null when the publisher supplied none.
      key: served.key ?? null,
      // Absent where the layer declares the property, or rather does not: geometry is never
      // withheld from an artifact that is served at all, so an absence is a fact about the layer.
      centroid: served.centroid ?? null,
      box: served.box ?? null,
      shape: served.shape ?? null,
      // JSON carries it as a number, and a count is not an identifier: it is bounded by the
      // corpus, so nothing here can reach 2^53. Widened to `bigint` anyway, because it is the same
      // quantity the wire delivers as `u64` and a panel must be able to print the two the same way.
      maskedCount: BigInt(served.masked_count)
    };
  }
}

/** `GET /v1/meta`'s snake_case wire shape, mapped to {@link Meta} above. */
type RawMeta = {
  api_version: number;
  idset: number;
  views: {
    id: string;
    display_name: string;
    quantisation: {x_min: number; x_max: number; y_min: number; y_max: number};
    projection: ProjectionName;
    world_aspect: number | null;
    tile_scheme: TileScheme | null;
    tile: {z: number; x: number; y: number} | null;
    /** The roster record (`views.md` §3.2); all three null together on a plain view. */
    group: string | null;
    key: string | null;
    metadata: Record<string, ViewMetadataValue> | null;
  }[];
  /** The view groups, each its view ids in creation order. Empty where there are none. */
  groups: {name: string; members_of: string | null; views: string[]}[];
  declared_scalars: {
    name: string;
    arrow_type: ArrowType;
    category: {vocabulary: string; kind: 'declared' | 'discovered'; visibility: 'derived' | 'public'} | null;
    render: boolean;
    index: boolean;
  }[];
  filter_operands?: {
    column: string;
    family: FilterOperandSet['family'];
    operands: string[];
    scope?: {group: string};
  }[];
  /** Absent on a deployment whose server predates layers; empty when this principal reaches none. */
  layers?: {
    name: string;
    title: string;
    views: string[];
    membership: Layer['membership'];
    hierarchy: {kind: Layer['hierarchy']['kind']; prune_children: boolean};
    levels: {level: number; title: string; zoom: [number, number] | null}[];
    computed_content: string[];
    shape?: ShapeKind | null;
    supplied_content: string[];
    depends_on: string[];
    version: number;
  }[];
  selection: {
    k_min: number;
    k_max_marks: number;
    max_k: number;
    theta_target_marks: number;
    max_underlay_offset: number;
    max_tiles_per_request?: number;
    max_category_values?: number;
    max_region_vertices?: number;
    max_region_cells?: number;
  };
};

/** `GET /v1/categories/{column}`'s wire shape. */
type RawCategories = {
  column: string;
  values: {code: number; key: string; title?: string | null}[];
  next: string | null;
};
