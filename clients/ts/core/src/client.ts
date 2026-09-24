import {
  checkTrailerCounts,
  decodeViewport,
  parseTrailer,
  type PointsPart,
  type ViewportHead
} from './decode.js';
import {base64} from './control.js';
import {createDecoder, type Decoder, type HeadFrames} from './decoder.js';
import {parseRegionVerdict} from './region.js';
import {FRAME_ARTIFACTS, FRAME_POINTS, FRAME_SUB_CELLS, FRAME_TILES, FRAME_TRAILER, FrameReader} from './frame.js';
import {openRecords, type RecordsRead} from './records.js';
import type {ArrowType, ArtifactDetail, ArtifactsHead, ArtifactsRequest, BrowsePage, BrowseRequest, BrowseRow, CategoryValue, FilterOperandSet, ItemDetail, ItemsHead, ItemsRequest, Layer, Meta, ProjectionName, Session, Shape, ShapeKind, SuggestResult, TileCounts, TileScheme, ViewMetadataValue, ViewportPart, ViewportRequest, ViewportResponse, ViewportResult} from './types.js';

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
    membership: {} as Record<string, never>,
    // No points, so no bits — and `null` is the honest value, being *no highlight column here*
    // rather than *nothing highlighted*.
    highlighted: null,
    pointsProjection: 'full' as const
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
   * Needed only by `authorise` and `revoke`. A browser holds it only where the server allows it,
   * which a deployment turns on for development.
   */
  sessionCredential?: string;
  /**
   * Called once per viewport response with its decode time. `ms` runs from bytes in to typed
   * arrays out as seen from this thread; `workerMs` is the worker's own decode time, `null` where
   * the response decoded inline, so the difference is time spent queued. On a streamed response
   * `ms` runs from the head to the last part and includes the wire, and `workerMs` is the sum of
   * the frames' decode times.
   */
  onDecode?: (ms: number, bytes: number, points: number, workerMs: number | null) => void;
  /** Where responses are decoded: a worker in a browser and inline elsewhere unless given. */
  decoder?: Decoder;
  /** Used for every request in place of the global `fetch`. */
  fetch?: typeof fetch;
  /** Sent on every request. A verb's own `authorization` and `content-type` are set after these. */
  headers?: Record<string, string>;
};

/**
 * The viewer and session routes, one method each. It holds no cache, session or replica state;
 * the store above it holds those.
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

  /**
   * One request, through the host's `fetch`, with the host's headers and then the route's own,
   * which replace a host header of the same name in any case.
   */
  private send(url: string, init: Omit<RequestInit, 'headers'> & {headers?: Record<string, string>} = {}): Promise<Response> {
    const headers = new Headers(this.opts.headers);
    for (const [name, value] of Object.entries(init.headers ?? {})) headers.set(name, value);
    return (this.opts.fetch ?? fetch)(url, {...init, headers});
  }

  /** Release the decode worker, if one was created. */
  close(): void {
    this.decoder?.close();
    this.decoder = null;
  }

  async authorise(terms: string[], signal?: AbortSignal): Promise<Session> {
    if (!this.opts.sessionCredential) {
      throw new Error('authorise needs a sessionCredential');
    }
    // The session plane reads `auth_data` as base64 of UTF-8 JSON.
    const authData = base64(new TextEncoder().encode(JSON.stringify({terms})));
    const response = await this.send(`${this.opts.sessionUrl}/session/authorise`, {
      method: 'POST',
      headers: {
        authorization: `Bearer ${this.opts.sessionCredential}`,
        'content-type': 'application/json'
      },
      body: JSON.stringify({auth_data: authData}),
      signal
    });
    if (!response.ok) await fail(response);
    const body = (await response.json()) as {token: string; token_id: number; expires_at: number};
    return {token: body.token, tokenId: body.token_id, expiresAt: body.expires_at};
  }

  /** End a session by its `tokenId`, so the token itself is not sent again. An id naming no live session is not refused. */
  async revoke(tokenId: number, signal?: AbortSignal): Promise<void> {
    if (!this.opts.sessionCredential) {
      throw new Error('revoke needs a sessionCredential');
    }
    const response = await this.send(`${this.opts.sessionUrl}/session/revoke`, {
      method: 'POST',
      headers: {
        authorization: `Bearer ${this.opts.sessionCredential}`,
        'content-type': 'application/json'
      },
      body: JSON.stringify({token_id: tokenId}),
      signal
    });
    if (!response.ok) await fail(response);
  }

  /** `GET /v1/meta`. A body missing a field the contract requires is refused. */
  async meta(token: string, signal?: AbortSignal): Promise<Meta> {
    const response = await this.send(`${this.opts.viewerUrl}/v1/meta`, {
      headers: {authorization: `Bearer ${token}`},
      signal
    });
    if (!response.ok) await fail(response);
    const m = (await response.json()) as RawMeta;
    requireFields(m, META_FIELDS, '/v1/meta');
    requireFields(m.selection, SELECTION_FIELDS, "/v1/meta's selection");
    for (const v of m.views) requireFields(v, VIEW_FIELDS, 'a view in /v1/meta');
    for (const g of m.groups) requireFields(g, GROUP_FIELDS, 'a group in /v1/meta');
    for (const c of m.declared_scalars) requireFields(c, DECLARED_FIELDS, 'a declared scalar in /v1/meta');
    for (const c of m.scoped_scalars) requireFields(c, SCOPED_FIELDS, 'a scoped scalar in /v1/meta');
    for (const f of m.filter_operands) requireFields(f, OPERAND_FIELDS, 'a filter operand in /v1/meta');
    for (const l of m.layers) {
      requireFields(l, LAYER_FIELDS, 'a layer in /v1/meta');
      for (const v of l.levels) requireFields(v, LEVEL_FIELDS, `a level of layer ${l.name} in /v1/meta`);
    }
    const category = (c: RawCategory | null) => (c ? {vocabulary: c.vocabulary, kind: c.kind, visibility: c.visibility} : null);
    return {
      apiVersion: m.api_version,
      bundleFormat: m.bundle_format,
      idset: m.idset,
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
        // The three roster fields are null together on a plain view, so `group` decides the case.
        roster: s.group === null ? null : {group: s.group, key: s.key!, metadata: s.metadata!}
      })),
      groups: m.groups.map((g) => ({
        name: g.name,
        title: g.title,
        membersOf: g.members_of,
        views: g.views
      })),
      declaredScalars: m.declared_scalars.map((s) => ({
        name: s.name,
        arrowType: s.arrow_type,
        category: category(s.category),
        render: s.render,
        index: s.index,
        analyser: s.analyser,
        homes: s.homes
      })),
      scopedScalars: m.scoped_scalars.map((s) => ({
        name: s.name,
        arrowType: s.arrow_type,
        scope: {group: s.scope.group},
        category: category(s.category),
        analyser: s.analyser,
        render: s.render,
        index: s.index,
        views: s.views
      })),
      selection: {
        kMin: m.selection.k_min,
        kMaxMarks: m.selection.k_max_marks,
        maxK: m.selection.max_k,
        thetaTargetMarks: m.selection.theta_target_marks,
        maxUnderlayOffset: m.selection.max_underlay_offset,
        maxCategoryValues: m.selection.max_category_values,
        maxRegionVertices: m.selection.max_region_vertices,
        maxRegionCells: m.selection.max_region_cells,
        maxBrowseRows: m.selection.max_browse_rows,
        maxShapeVertices: m.selection.max_shape_vertices,
        maxSuggestions: m.selection.max_suggestions,
        maxSuggestionWalk: m.selection.max_suggestion_walk,
        maxSuggestSetEntities: m.selection.max_suggest_set_entities,
        maxPageRows: m.selection.max_page_rows,
        maxPageBytes: m.selection.max_page_bytes
      },
      maxTilesPerRequest: m.selection.max_tiles_per_request,
      filterOperands: m.filter_operands.map((f) => ({
        column: f.column,
        family: f.family,
        operands: f.operands,
        // Present only on a group-scoped column.
        ...(f.scope ? {scope: {group: f.scope.group}} : {})
      })),
      // Filtered per principal by the server: the layers this principal may know exist.
      layers: m.layers.map((l) => ({
        name: l.name,
        title: l.title,
        views: l.views,
        membership: l.membership,
        hierarchy: {kind: l.hierarchy.kind, pruneChildren: l.hierarchy.prune_children},
        levels: l.levels.map((v) => ({level: v.level, title: v.title, zoom: v.zoom})),
        computedContent: l.computed_content,
        shape: l.shape,
        suppliedContent: l.supplied_content,
        depsOn: l.depends_on,
        version: l.version
      }))
    };
  }

  /**
   * `POST /v1/viewport`. A field the caller leaves unset is left out of the body, so the server's
   * own default applies; `k` defaults to the deployment's ceiling.
   */
  async viewport(
    token: string,
    req: ViewportRequest,
    signal?: AbortSignal,
    /** Decode on the speculative lane; see {@link Decoder.decode}. */
    background = false,
    /**
     * Takes each points frame as it lands. With a sink the returned response carries no points,
     * since each went to the sink. A request with `k = 0` has no points frames.
     */
    onPart?: PartSink
  ): Promise<ViewportResponse> {
    // Each optional field is sent only when the caller set it, so an unset one takes the server's
    // default. For `layers`, `levels` and `computed` an empty array is a request for none, which
    // differs from leaving the field out.
    const body: Record<string, unknown> = {view: req.view, zoom: req.zoom};
    if (req.bbox) body.bbox = req.bbox;
    // A tile prefix at depth 16 needs 32 bits, which a JSON number holds exactly.
    if (req.tiles) body.tiles = req.tiles.map(Number);
    if (req.k !== undefined) body.k = req.k;
    if (req.underlayOffset) body.underlay_offset = req.underlayOffset;
    if (req.filters) body.filters = req.filters;
    if (req.highlight) body.highlight = req.highlight;
    if (req.pointRows !== undefined) body.point_rows = req.pointRows;
    if (req.layers !== undefined) body.layers = req.layers;
    if (req.artifactBudget !== undefined) body.artifact_budget = req.artifactBudget;
    if (req.artifactRows !== undefined) body.artifact_rows = req.artifactRows;
    if (req.levels !== undefined) body.levels = req.levels;
    if (req.computed !== undefined) body.computed = req.computed;
    // The stamp is held as the string the server sent and travels as the object it parses to.
    if (req.stamp) body.pin = JSON.parse(req.stamp);

    const response = await this.send(`${this.opts.viewerUrl}/v1/viewport`, {
      method: 'POST',
      headers: {authorization: `Bearer ${token}`, 'content-type': 'application/json'},
      body: JSON.stringify(body),
      signal
    });
    if (!response.ok) await fail(response);
    const stage = response.headers.get('x-tessera-stage-ns');
    const coordinates = {
      identityKey: response.headers.get('x-tessera-identity-key') ?? '',
      // The quotes are the entity-tag syntax and not part of the key.
      contentKey: (response.headers.get('etag') ?? '').replace(/^"|"$/g, ''),
      pin: response.headers.get('x-tessera-pin'),
      stale: response.headers.get('x-tessera-stale') === '1',
      // Absent unless the request carried a region leaf.
      region: parseRegionVerdict(response.headers.get('x-tessera-region'))
    };
    this.decoder ??= this.opts.decoder ?? createDecoder();
    // A counts-only response (`k = 0`) has no points and decodes in milliseconds, so it decodes on
    // this thread rather than queueing behind a point decode in a worker lane. It has no points
    // frames to hand a part sink.
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
        // The server's time to its first flush, not to the end of the stream.
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
            // `part` carries `projection`; the result names it `pointsProjection`, the frames'
            // own answer either way.
            ...part,
            pointsProjection: part.projection,
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
   * `GET /v1/categories/{column}`: what this column's codes stand for. Passing `codes` resolves
   * those codes alone; omitting them enumerates the whole value set, a page at a time until the
   * server returns no cursor.
   *
   * A code missing from the answer is either unknown or a value this principal cannot see, and
   * the server does not say which. A code the client drew always resolves, since a visible point
   * carries it. A refusal throws {@link TesseraError}.
   */
  async categories(
    token: string,
    column: string,
    opts: {codes?: readonly number[]; limit?: number; view?: string; signal?: AbortSignal} = {}
  ): Promise<CategoryValue[]> {
    const base = `${this.opts.viewerUrl}/v1/categories/${encodeURIComponent(column)}`;
    const out: CategoryValue[] = [];
    // A group-scoped category's codes are per view, so the view decides which column answers.
    const view = opts.view ? `view=${encodeURIComponent(opts.view)}` : '';

    if (opts.codes) {
      // An empty `codes=` would be read as the enumeration form.
      if (opts.codes.length === 0) return out;
      const url = `${base}?codes=${[...opts.codes].join(',')}${view ? `&${view}` : ''}`;
      const page = await this.categoryPage(token, url, opts.signal);
      return page.values;
    }

    let cursor: string | null = null;
    do {
      const params = new URLSearchParams();
      if (opts.limit !== undefined) params.set('limit', String(opts.limit));
      if (cursor !== null) params.set('after', cursor);
      if (opts.view !== undefined) params.set('view', opts.view);
      const query = params.toString();
      const page = await this.categoryPage(token, query ? `${base}?${query}` : base, opts.signal);
      out.push(...page.values);
      cursor = page.next;
    } while (cursor !== null);
    return out;
  }

  private async categoryPage(
    token: string,
    url: string,
    signal: AbortSignal | undefined
  ): Promise<{values: CategoryValue[]; next: string | null}> {
    const response = await this.send(url, {headers: {authorization: `Bearer ${token}`}, signal});
    if (!response.ok) await fail(response);
    const body = (await response.json()) as RawCategories;
    return {
      values: body.values.map((v) => ({code: v.code, key: v.key, title: v.title ?? null})),
      next: body.next
    };
  }

  /**
   * `GET /v1/categories/{column}/suggest`: at most `limit` values of a category whose folded key,
   * folded title or a word start of either begins with `q`, ordered by the matched text and
   * visible to this principal on the rule `categories` follows.
   *
   * A session has one suggestion in flight at a time. The server refuses a second with `429`, and
   * this returns `{status: 'superseded', retryAfterS}` for it rather than throwing, since the
   * caller's answer is to retry. The response echoes `q` as sent, so a caller can match a page to
   * the request it answers.
   */
  async suggest(
    token: string,
    column: string,
    q: string,
    opts: {limit?: number; counts?: boolean; view?: string; signal?: AbortSignal} = {}
  ): Promise<SuggestResult> {
    const params = new URLSearchParams({q});
    if (opts.limit !== undefined) params.set('limit', String(opts.limit));
    if (opts.counts) params.set('counts', 'true');
    if (opts.view !== undefined) params.set('view', opts.view);
    const url = `${this.opts.viewerUrl}/v1/categories/${encodeURIComponent(column)}/suggest?${params.toString()}`;
    const response = await this.send(url, {headers: {authorization: `Bearer ${token}`}, signal: opts.signal});
    if (response.status === 429) {
      // The body's `retry_after_s` is what the contract requires; the header is the fallback for
      // a body that does not parse.
      let retryAfterS = Number(response.headers.get('retry-after') ?? '1');
      try {
        const body = (await response.json()) as {retry_after_s?: number};
        if (typeof body.retry_after_s === 'number') retryAfterS = body.retry_after_s;
      } catch {
        // A non-JSON 429 (a proxy's) still yields a retryable outcome from the header alone.
      }
      return {status: 'superseded', retryAfterS: Number.isFinite(retryAfterS) ? retryAfterS : 1};
    }
    if (!response.ok) await fail(response);
    const body = (await response.json()) as RawSuggest;
    return {
      status: 'ok',
      column: body.column,
      q: body.q,
      values: body.values.map((v) => ({
        code: v.code,
        key: v.key,
        title: v.title ?? null,
        match: {field: v.match.field, start: v.match.start, len: v.match.len},
        ...(v.count !== undefined ? {count: v.count} : {})
      })),
      more: body.more
    };
  }

  /**
   * `POST /v1/items/{tessera_id}`: the whole record, keyed by column name, a category as its
   * vocabulary key. A column the item has no value for is absent from `fields`. `labels` names
   * the item's access labels this session satisfies and no others.
   */
  async item(token: string, tesseraId: bigint, signal?: AbortSignal): Promise<ItemDetail> {
    const response = await this.send(`${this.opts.viewerUrl}/v1/items/${tesseraId.toString()}`, {
      method: 'POST',
      headers: {authorization: `Bearer ${token}`, 'content-type': 'application/json'},
      body: '{}',
      signal
    });
    if (!response.ok) await fail(response);
    const body = (await response.json()) as {
      fields: Record<string, unknown>;
      external_id?: string;
      labels: string[];
      views: {id: string; x: number; y: number}[];
      scoped: Record<string, Record<string, unknown>>;
    };
    return {
      fields: body.fields ?? {},
      externalId: body.external_id ?? null,
      // Required by the response schema and read as given: an empty list is an answer.
      views: body.views,
      scoped: body.scoped,
      labels: body.labels
    };
  }

  /**
   * `POST /v1/artifacts/{tessera_id}`: one artifact's layer, key, masked count and geometry under
   * `view`, since a masked count is per view.
   *
   * An identifier naming nothing, a point, an artifact of a layer this principal cannot reach, a
   * suppressed one and one below its layer's existence criterion all answer the same `404`, which
   * throws {@link TesseraError}.
   */
  async artifact(
    token: string,
    tesseraId: bigint,
    opts: {view: string; idset?: number; zoom?: number; signal?: AbortSignal}
  ): Promise<ArtifactDetail> {
    const body: Record<string, unknown> = {view: opts.view};
    if (opts.idset !== undefined) body.idset = opts.idset;
    // The depth the shape is drawn at, which the server simplifies it to. Left out, the whole
    // stored shape is served.
    if (opts.zoom !== undefined) body.zoom = Math.max(0, Math.min(16, Math.floor(opts.zoom)));
    const response = await this.send(`${this.opts.viewerUrl}/v1/artifacts/${tesseraId.toString()}`, {
      method: 'POST',
      headers: {authorization: `Bearer ${token}`, 'content-type': 'application/json'},
      body: JSON.stringify(body),
      signal: opts.signal
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

  /**
   * `POST /v1/artifacts/browse`: a layer's hierarchy by lineage (its roots, one artifact's
   * children and parents, or a name search), each row with its masked count and, under a filter,
   * its matched count. It takes no bbox, tiles or zoom. Identifiers travel as decimal strings,
   * since JSON has no 64-bit integer.
   *
   * An unknown layer is a `422`. An artifact this principal was never served answers an empty
   * page.
   */
  async browse(token: string, req: BrowseRequest, signal?: AbortSignal): Promise<BrowsePage> {
    const body: Record<string, unknown> = {view: req.view, layer: req.layer};
    if (req.level !== undefined) body.level = req.level;
    if (req.parent !== undefined) body.parent = req.parent.toString();
    if (req.q !== undefined) body.q = req.q;
    if (req.filters) body.filters = req.filters;
    if (req.limit !== undefined) body.limit = req.limit;
    if (req.cursor !== undefined) body.cursor = req.cursor;
    const response = await this.send(`${this.opts.viewerUrl}/v1/artifacts/browse`, {
      method: 'POST',
      headers: {authorization: `Bearer ${token}`, 'content-type': 'application/json'},
      body: JSON.stringify(body),
      signal
    });
    if (!response.ok) await fail(response);
    const page = (await response.json()) as RawBrowsePage;
    return {
      artifacts: (page.artifacts ?? []).map(browseRow),
      // Filled on the children form only.
      parents: (page.parents ?? []).map(browseRow),
      next: page.next ?? null
    };
  }

  /**
   * `POST /v1/items`: one response of a bulk read of the items this principal may see in a view.
   * Resolves when the head arrives; iterating the result then yields each page as an Arrow table.
   * A refusal throws {@link TesseraError} here, before any page.
   *
   * The request is sent as given. The read stops at this response's trailer, and the caller
   * continues it by sending the request again with `cursor` set to the result's `cursor`, until
   * that is `null`.
   */
  items(token: string, req: ItemsRequest, signal?: AbortSignal): Promise<RecordsRead<ItemsHead>> {
    return this.bulkRead('items', token, req, signal, (raw) => {
      requireFields(raw, ['order', 'page_rows'], 'the head of a /v1/items response');
      const head = raw as {order: ItemsHead['order']; page_rows: number; visible?: number; matched?: number};
      return {order: head.order, pageRows: head.page_rows, visible: head.visible ?? null, matched: head.matched ?? null};
    });
  }

  /**
   * `POST /v1/artifacts`: one response of a bulk read of the artifacts of one layer this principal
   * is served, as {@link items} reads items.
   */
  artifacts(token: string, req: ArtifactsRequest, signal?: AbortSignal): Promise<RecordsRead<ArtifactsHead>> {
    return this.bulkRead('artifacts', token, req, signal, (raw) => {
      requireFields(raw, ['page_rows'], 'the head of a /v1/artifacts response');
      const head = raw as {page_rows: number; served?: number; matched?: number};
      return {pageRows: head.page_rows, served: head.served ?? null, matched: head.matched ?? null};
    });
  }

  private async bulkRead<Head>(
    route: 'items' | 'artifacts',
    token: string,
    req: ItemsRequest | ArtifactsRequest,
    signal: AbortSignal | undefined,
    parseHead: (raw: unknown) => Head
  ): Promise<RecordsRead<Head>> {
    // Each field that is set, under its wire name. A `tessera_id` travels as a decimal string.
    const body: Record<string, unknown> = {};
    for (const [name, value] of Object.entries(req)) {
      if (value === undefined) continue;
      body[name.replace(/[A-Z]/g, (c) => `_${c.toLowerCase()}`)] = typeof value === 'bigint' ? value.toString() : value;
    }
    const response = await this.send(`${this.opts.viewerUrl}/v1/${route}`, {
      method: 'POST',
      headers: {authorization: `Bearer ${token}`, 'content-type': 'application/json'},
      body: JSON.stringify(body),
      signal
    });
    if (!response.ok) await fail(response);
    return openRecords(response, req, signal, parseHead);
  }
}

/** One row of `POST /v1/artifacts/browse`, as the JSON carries it. */
type RawBrowseRow = {
  tessera_id: string;
  key?: string | null;
  name?: string | null;
  masked_count: number | string;
  matched_count?: number | string | null;
  rung: number;
  parent_ids?: string[];
};

type RawBrowsePage = {artifacts?: RawBrowseRow[]; parents?: RawBrowseRow[]; next?: string | null};

function browseRow(r: RawBrowseRow): BrowseRow {
  return {
    tesseraId: BigInt(r.tessera_id),
    // Absent rather than null where the publisher supplied none, and where this principal may not
    // read the text — the two are one state, as they are on the artifacts frame.
    key: r.key ?? null,
    name: r.name ?? null,
    maskedCount: BigInt(r.masked_count),
    matchedCount: r.matched_count === undefined || r.matched_count === null ? null : BigInt(r.matched_count),
    rung: r.rung,
    // A null cell is the empty list, which is the fail-closed direction: no parent is invented.
    parentIds: (r.parent_ids ?? []).map((v) => BigInt(v))
  };
}

/** The fields `/v1/meta` must carry, and those each of its blocks and list elements must. */
const VIEW_FIELDS = ['id', 'display_name', 'quantisation', 'projection', 'world_aspect', 'tile_scheme', 'tile', 'group', 'key', 'metadata'];
const GROUP_FIELDS = ['name', 'title', 'members_of', 'views'];
const DECLARED_FIELDS = ['name', 'arrow_type', 'category', 'analyser', 'render', 'index', 'homes'];
const SCOPED_FIELDS = ['name', 'arrow_type', 'scope', 'category', 'analyser', 'render', 'index', 'views'];
const OPERAND_FIELDS = ['column', 'family', 'operands'];
const LAYER_FIELDS = ['name', 'title', 'views', 'membership', 'hierarchy', 'levels', 'computed_content', 'shape', 'supplied_content', 'depends_on', 'version'];
const LEVEL_FIELDS = ['level', 'title', 'zoom'];
const META_FIELDS = ['api_version', 'bundle_format', 'idset', 'views', 'groups', 'declared_scalars', 'scoped_scalars', 'filter_operands', 'selection', 'layers'] as const;
const SELECTION_FIELDS = [
  'k_min',
  'k_max_marks',
  'max_k',
  'theta_target_marks',
  'max_underlay_offset',
  'max_tiles_per_request',
  'max_category_values',
  'max_shape_vertices',
  'max_region_vertices',
  'max_region_cells',
  'max_suggestions',
  'max_suggestion_walk',
  'max_suggest_set_entities',
  'max_browse_rows',
  'max_page_rows',
  'max_page_bytes'
] as const;

/** Throws where `body` is not an object or lacks one of `fields`. */
function requireFields(body: unknown, fields: readonly string[], where: string): void {
  if (typeof body !== 'object' || body === null) throw new Error(`${where} is not an object; the server and this client are from different versions`);
  for (const field of fields) {
    if (!(field in body)) throw new Error(`${where} has no \`${field}\`, which the contract requires; the server and this client are from different versions`);
  }
}

type RawCategory = {vocabulary: string; kind: 'declared' | 'discovered'; visibility: 'derived' | 'public'};

/** `GET /v1/meta`'s snake_case wire shape, mapped to {@link Meta} above. */
type RawMeta = {
  api_version: number;
  bundle_format: number;
  idset: number;
  views: {
    id: string;
    display_name: string;
    quantisation: {x_min: number; x_max: number; y_min: number; y_max: number};
    projection: ProjectionName;
    world_aspect: number | null;
    tile_scheme: TileScheme | null;
    tile: {z: number; x: number; y: number} | null;
    /** The roster record; all three null together on a plain view. */
    group: string | null;
    key: string | null;
    metadata: Record<string, ViewMetadataValue> | null;
  }[];
  groups: {name: string; title: string | null; members_of: string | null; views: string[]}[];
  declared_scalars: {
    name: string;
    arrow_type: ArrowType;
    category: RawCategory | null;
    analyser: string | null;
    render: boolean;
    index: boolean;
    homes: ('rendered' | 'value_column' | 'record')[];
  }[];
  scoped_scalars: {
    name: string;
    arrow_type: ArrowType;
    scope: {group: string};
    category: RawCategory | null;
    analyser: string | null;
    render: boolean;
    index: boolean;
    views: string[];
  }[];
  filter_operands: {
    column: string;
    family: FilterOperandSet['family'];
    operands: string[];
    scope?: {group: string};
  }[];
  layers: {
    name: string;
    title: string | null;
    views: string[];
    membership: Layer['membership'];
    hierarchy: {kind: Layer['hierarchy']['kind']; prune_children: boolean};
    levels: {level: number; title: string | null; zoom: [number, number] | null}[];
    computed_content: string[];
    shape: ShapeKind | null;
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
    max_tiles_per_request: number;
    max_category_values: number;
    max_shape_vertices: number;
    max_region_vertices: number;
    max_region_cells: number;
    max_suggestions: number;
    max_suggestion_walk: number;
    max_suggest_set_entities: number;
    max_browse_rows: number;
    max_page_rows: number;
    max_page_bytes: number;
  };
};

/** `GET /v1/categories/{column}`'s wire shape. */
type RawCategories = {
  column: string;
  values: {code: number; key: string; title?: string | null}[];
  next: string | null;
};

/** `GET /v1/categories/{column}/suggest`'s wire shape. */
type RawSuggest = {
  column: string;
  q: string;
  values: {
    code: number;
    key: string;
    title?: string | null;
    match: {field: 'key' | 'title'; start: number; len: number};
    count?: number;
  }[];
  more: boolean;
};
