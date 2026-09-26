import {
  checkTrailerCounts,
  decodeViewport,
  parseTrailer,
  stageNsOf,
  type PointsPart,
  type ViewportHead
} from './decode.js';
import {base64} from './control.js';
import {createDecoder, type Decoder, type HeadFrames} from './decoder.js';
import {parseRegionVerdict} from './region.js';
import {FRAME_ARTIFACTS, FRAME_POINTS, FRAME_SUB_CELLS, FRAME_TILES, FRAME_TRAILER, FrameReader} from './frame.js';
import {openRecords, type RecordsRead} from './records.js';
import type {ArrowType, ArtifactDetail, ArtifactsHead, ArtifactsRequest, BrowsePage, BrowseRequest, BrowseRow, CategoryValue, FilterOperandSet, ItemDetail, ItemsHead, ItemsRequest, Layer, MapProjection, Meta, Session, Shape, ShapeKind, SuggestResult, TileCounts, TileScheme, ViewMetadataValue, ViewportPart, ViewportRequest, ViewportResponse, ViewportResult} from './types.js';

/**
 * Receives a streamed `/v1/viewport` response's points, one points frame at a time, as
 * {@link TesseraClient.viewport} decodes them. Parts arrive in the order the server sent them, and
 * the next part is not handed over until a returned promise settles.
 *
 * Each part's `result` holds whole tiles: the counts of the tiles whose points it carries, those
 * points, and the response's artifacts. Its `subCells` is `null`.
 *
 * @category HTTP client
 */
export type PartSink = (part: ViewportPart) => void | Promise<void>;

/** What either decode path hands back, before the headers are folded in around it. */
type Decoded = {
  result: ViewportResult;
  bytes: number;
  points: number;
  ms: number;
  workerMs: number | null;
  /** The trailer's `stage_ns`. */
  stageNs: number[] | null;
};

/**
 * Empty point columns, for a result whose points went to a part sink. Each call makes fresh
 * buffers, so no two results share a `scalars` object.
 */
function emptyPoints() {
  return {
    ids: new BigUint64Array(0),
    codes: new BigUint64Array(0),
    positions: new Float64Array(0),
    world: new Float32Array(0),
    scalars: {} as Record<string, never>,
    membership: {} as Record<string, never>,
    // No points, so no highlight column. `null` says that; an empty array would say nothing is
    // highlighted.
    highlighted: null,
    pointsProjection: 'full' as const
  };
}

/**
 * The trailer's `stage_ns` in a whole body, found by walking the frame headers to the trailer, or
 * `null` where the walk finds none. The decoder checks the body's grammar.
 */
function stageNsOfBody(bytes: Uint8Array): number[] | null {
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  for (let at = 0; at + 5 <= bytes.byteLength; ) {
    const end = at + 5 + view.getUint32(at + 1, true);
    if (end > bytes.byteLength) return null;
    if (bytes[at] === FRAME_TRAILER) return stageNsOf(parseTrailer(bytes.subarray(at + 5, end)));
    at = end;
  }
  return null;
}

/** The same result with its points removed, since they went to the part sink. */
function headOnly(result: ViewportResult): ViewportResult {
  return {...result, ...emptyPoints()};
}

/**
 * The run of tiles whose served counts add up to one points frame's rows.
 *
 * The server does not split a tile across frames, so a frame's rows are some run of the tiles
 * batch, found by adding up the counts the server already sent. A frame whose rows end inside a
 * tile cannot be attributed, and is refused rather than assigned to a tile it may not belong to.
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
 * A refusal from the server: the HTTP status and the `error` and `detail` of its JSON error body.
 * A request that reaches no server rejects with the `fetch` error instead, so a caller can tell a
 * refusal from a transport failure. The `message` is `"<status> <code>: <detail>"`.
 *
 * @category HTTP client
 */
export class TesseraError extends Error {
  constructor(
    /** The HTTP status, such as `404` or `422`. */
    readonly status: number,
    /**
     * The body's `error` code: `bad-credential`, `expired-token`, `unknown`, `conflict`,
     * `contract`, `backpressure`, `fail-closed` or `not-ready`. `unknown` also where the body is not
     * JSON or has no code.
     */
    readonly code: string,
    /** The body's `detail`, saying what was wrong. The HTTP status text where the body has none. */
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
    // A body that is not JSON, such as a proxy's, still gives a TesseraError.
  }
  throw new TesseraError(response.status, code, detail);
}

/**
 * Where a {@link TesseraClient} sends its requests, and how.
 *
 * @category HTTP client
 */
export type TesseraClientOptions = {
  /** The viewer listener's base URL, without a trailing slash. Routes under `/v1/` are appended to it. */
  viewerUrl: string;
  /** The session listener's base URL, without a trailing slash. Routes under `/session/` are appended to it. */
  sessionUrl: string;
  /**
   * The session credential, sent as the bearer token by `authorise` and `revoke`; no other method
   * uses it. A browser page can reach the session listener only from an origin the deployment
   * lists in `serve.dev_cors_origins`, a development setting.
   */
  sessionCredential?: string;
  /**
   * Called once per viewport response with its decode figures. `ms` runs from the response's
   * headers to the last typed array, as this thread sees it, and includes reading the body.
   * `bytes` is the body's size and `points` the points decoded. `workerMs` is the worker's own
   * decode time, or `null` where the response decoded on this thread, so `ms - workerMs` is time
   * spent reading and queued. On a streamed response `workerMs` is the sum over the frames.
   */
  onDecode?: (ms: number, bytes: number, points: number, workerMs: number | null) => void;
  /**
   * Where responses are decoded. Defaults to a worker where one can be made, and this thread
   * otherwise, made at the first `viewport` call.
   */
  decoder?: Decoder;
  /** Used for every request in place of the global `fetch`. */
  fetch?: typeof fetch;
  /**
   * Headers sent on every request. A method's own `authorization` and `content-type` replace a
   * header of the same name here.
   */
  headers?: Record<string, string>;
};

/**
 * Calls the viewer and session routes, one method per route. It holds no cache or session state;
 * {@link createStore} holds those.
 *
 * A method rejects with {@link TesseraError} when the server refuses, and with the `fetch` error
 * when no server answers. On a viewer route, `401 bad-credential` for a token that worked before
 * and `403 expired-token` both mean the session has ended. Every count a viewer route returns is
 * over the items the token's session may see.
 *
 * @category HTTP client
 */
export class TesseraClient {
  /**
   * Where responses are turned into typed arrays.
   *
   * Created lazily and shared across requests. In a browser this is a worker, so decode does not
   * compete with drawing; elsewhere it decodes on this thread.
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

  /** Closes the decoder, terminating its workers if it has any. */
  close(): void {
    this.decoder?.close();
    this.decoder = null;
  }

  /**
   * `POST /session/authorise`: mints a viewer session. The request's `auth_data` is base64 of the
   * JSON `{"terms": [...]}`, the form the built-in `builtin:passthrough` auth plugin reads.
   *
   * @param terms - The access labels to grant, under `builtin:passthrough`. Another auth plugin
   *   reads them by its own rules.
   * @returns The session: `token` for the viewer methods, `tokenId` for `revoke`, and `expiresAt`
   *   in seconds since the Unix epoch.
   * @throws `Error` when the options carry no `sessionCredential`.
   * @throws {@link TesseraError} when the server refuses: `401` for a wrong session credential,
   *   `422` where the auth plugin refuses `auth_data`, `429` under load.
   */
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

  /**
   * `POST /session/revoke`: ends the session `tokenId` names, so the token itself is not sent
   * again. An id naming no live session is not refused.
   *
   * @throws `Error` when the options carry no `sessionCredential`.
   * @throws {@link TesseraError} when the server refuses: `401` for a wrong session credential,
   *   `422` for a malformed request.
   */
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

  /**
   * `GET /v1/meta`: the deployment's views, groups, columns, filter operands and selection limits,
   * and the layers this token's session may know of. The layers differ between principals, so a
   * result is not shared across tokens.
   *
   * @throws {@link TesseraError} when the server refuses.
   * @throws `Error` when the body lacks a field this client requires, which means the server and
   *   the client are from different versions.
   */
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
        unique: s.unique,
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
   * `POST /v1/viewport`: for one region at one depth, each tile's counts, a sample of up to `k`
   * points per tile, and the served artifacts. A field of `req` left unset is left out of the
   * body, so the server's default applies: `k` defaults to `selection.kMaxMarks` from
   * {@link TesseraClient.meta}, and the server caps it at `selection.maxK`. `k = 0` asks for the
   * counts alone, which decode on this thread.
   *
   * @param background - Decode on the decoder's background worker, so a response the user is
   *   waiting for does not queue behind this one. Defaults to `false`.
   * @param onPart - Receives the points as each points frame is decoded, before the body has
   *   finished. With a sink the returned `result` carries the counts, sub-cells and artifacts and
   *   no points. Not called for `k = 0`.
   * @returns The decoded result, the server's timings, the response's keys (`identityKey`,
   *   `contentKey`, `pin`, `stale`, `region`) and the body's size in `bytes`.
   * @throws {@link TesseraError} when the server refuses: `404` for an unknown view, `422` for a
   *   malformed request, `429` under load.
   * @throws `Error` when the body is malformed or ends before its trailer. Parts already passed to
   *   `onPart` hold whole tiles and stay correct.
   */
  async viewport(
    token: string,
    req: ViewportRequest,
    signal?: AbortSignal,
    background = false,
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
      body: jsonBody(body),
      signal
    });
    if (!response.ok) await fail(response);
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
        stageNs: decoded.stageNs
      },
      ...coordinates,
      bytes: decoded.bytes
    };
  }

  /** Reads the whole body, then decodes it: the path for a caller with no part sink. */
  private async whole(
    response: Response,
    counts: boolean,
    background: boolean
  ): Promise<Decoded> {
    const started = performance.now();
    const bytes = new Uint8Array(await response.arrayBuffer());
    // Read before decoding: the worker path transfers the buffer, which detaches it and leaves
    // `byteLength` at 0.
    const size = bytes.byteLength;
    const stageNs = stageNsOfBody(bytes);
    const result = counts ? decodeViewport(bytes) : await this.decoder!.decode(bytes, background);
    return {
      result,
      bytes: size,
      points: result.ids.length,
      ms: performance.now() - started,
      workerMs: counts ? null : this.decoder!.lastWorkerMs,
      stageNs
    };
  }

  /**
   * Reads the body as it arrives and hands each points frame to the sink once it is whole, so a
   * wide view draws before its last byte arrives. The server sends whole tiles per frame, in tile
   * order, and each frame is an Arrow stream that decodes alone.
   *
   * The tiles frame comes first, so each part carries the counts of the tiles its points belong
   * to. A part is a set of whole bands, and the replica stores it as it stores a whole response.
   *
   * A body that stops without its trailer throws here. The parts already delivered stay correct:
   * they come from one generation, and each tile's points are an id-order prefix of its served
   * set. The request does not resolve, so nothing marks the region covered.
   */
  private async streamed(
    response: Response,
    coordinates: {identityKey: string; contentKey: string},
    onPart: PartSink,
    background: boolean
  ): Promise<Decoded> {
    const started = performance.now();
    if (!response.body) {
      // A `fetch` that gives no stream, such as a polyfill or a mock. The sink gets the whole
      // result as one part.
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
    // Summed over the frames, and null where the decoder measures nothing, as the inline one does.
    let workerMs: number | null = null;
    // The tiles frame's rows, consumed in step with the point frames that satisfy them.
    let tileAt = 0;
    // Parts are delivered in wire order by awaiting each frame's decode in turn, whichever worker
    // decodes it. Reading continues while the sink takes a part, but the next part waits until the
    // sink's promise settles.
    let delivering: Promise<void> = Promise.resolve();
    // The chain is awaited only after the body has been read. Until then a rejection needs a
    // handler, or it is reported as unhandled.
    delivering.catch(() => {});

    // Set when the read fails: an abort, a transport fault, or a frame the grammar refuses. No part
    // is delivered after it, so an abandoned request does not keep filling the store.
    let abandoned = false;

    const startHead = () => {
      decodingHead ??= this.decoder!.decodeHead(head, background);
    };
    const deliver = (decoding: Promise<PointsPart>) => {
      delivering = delivering.then(async () => {
        const part = await decoding;
        // Read here: the decoder reports the last reply's time, and the head's reply sets the same
        // counter.
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
            // The part's `projection` is the result's `pointsProjection`.
            ...part,
            pointsProjection: part.projection,
            subCells: null,
            // Every part carries the response's artifacts, since a point's membership column
            // names its artifacts through them.
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
              // The grammar puts every head frame before the first points frame, so the head is
              // complete here and is decoded before the points.
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
    // Every whole frame is delivered before the body is checked. A body that stopped without its
    // trailer is refused below, but the whole tiles it delivered are correct and the caller keeps
    // them.
    await delivering;
    // Throws on a body that stopped inside a frame or without its trailer.
    frames.end();
    const decodedHead = await decodingHead!;
    const trailer = parseTrailer(trailerBytes!);
    checkTrailerCounts(trailer, flushes, points);
    // Every tile the server served points for has had them.
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
      workerMs,
      stageNs: stageNsOf(trailer)
    };
  }

  /**
   * `GET /v1/categories/{column}`: what a category column's codes stand for, as `code`, `key` and
   * `title`. With `codes`, resolves those codes alone, in the order given; an empty list returns
   * `[]` without a request. Without `codes`, lists the whole value set in key order, requesting
   * page after page until the server returns no cursor.
   *
   * A code missing from the answer is unknown or is a value this principal may not see, and the
   * server does not say which. A code on a point this session was served always resolves.
   *
   * @param column - The column's name, or `<column>@<key>` for a group-scoped category pinned to
   *   one view.
   * @param opts.codes - The codes to resolve. Left out, the whole value set is listed.
   * @param opts.limit - The page size when listing, capped at `selection.maxCategoryValues`, which
   *   is also the default.
   * @param opts.view - The view whose value set a group-scoped column answers from.
   * @throws {@link TesseraError} when the server refuses: `404` for a column that is not a category,
   *   `422` for a malformed request such as `limit: 0` or a group-scoped column named with no view.
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
   * `GET /v1/categories/{column}/suggest`: up to `limit` values of a category whose key, title or a
   * word start of either begins with `q`, compared after case folding. Values are ordered by the
   * matched text, and only values this principal may see are offered, as for
   * {@link TesseraClient.categories}. The page echoes `q` as sent, so a caller can match a page to
   * its request.
   *
   * A session has one suggestion in flight at a time. The server refuses a second with `429`, and
   * this returns `{status: 'superseded', retryAfterS}` for it instead of throwing.
   *
   * @param column - As for {@link TesseraClient.categories}.
   * @param q - The text typed. Empty matches every value.
   * @param opts.limit - The page size, capped at `selection.maxSuggestions`, which is also the
   *   default.
   * @param opts.counts - `true` adds each value's count of the items carrying it that this
   *   principal may see.
   * @param opts.view - As for `categories`.
   * @throws {@link TesseraError} for any other refusal: `404` for a column that is not a category,
   *   `422` for a `q` over 256 bytes or a malformed request.
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
   * `POST /v1/items/{tessera_id}`: one item's whole record. `fields` is keyed by column name, with
   * a category given as its vocabulary key and a column the item has no value for left out.
   * `labels` lists the item's access labels this session satisfies and no others. `views` and
   * `scoped` cover only the views this principal may reach.
   *
   * @param tesseraId - The item's `tessera_id`, as a viewport result's `ids` carries it.
   * @throws {@link TesseraError} when the server refuses: `404` both for an id that names nothing
   *   and for an item this principal may not see.
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
   * `POST /v1/artifacts/{tessera_id}`: one artifact's layer, key, masked count and geometry in a
   * view. The count and the geometry are over the members this principal may see. Geometry is in
   * 32-bit grid units (see {@link GRID32}), and a geometry field is `null` where the layer declares
   * no such property.
   *
   * @param opts.view - The view to count and place the artifact in.
   * @param opts.zoom - The depth to simplify the shape for, floored and clamped to 0 to 16. Left
   *   out, the whole stored shape is served.
   * @throws {@link TesseraError} when the server refuses: one `404` alike for an id naming nothing,
   *   a point, an artifact of a layer this principal cannot reach, a suppressed artifact and one
   *   below its layer's existence criterion.
   */
  async artifact(
    token: string,
    tesseraId: bigint,
    opts: {view: string; zoom?: number; signal?: AbortSignal}
  ): Promise<ArtifactDetail> {
    const body: Record<string, unknown> = {view: opts.view};
    // The depth the shape is drawn at, which the server simplifies it to. Left out, the whole
    // stored shape is served.
    if (opts.zoom !== undefined) body.zoom = Math.max(0, Math.min(16, Math.floor(opts.zoom)));
    const response = await this.send(`${this.opts.viewerUrl}/v1/artifacts/${tesseraId.toString()}`, {
      method: 'POST',
      headers: {authorization: `Bearer ${token}`, 'content-type': 'application/json'},
      body: jsonBody(body),
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
      // Absent where the layer declares no such property. A served artifact always has the
      // geometry its layer declares.
      centroid: served.centroid ?? null,
      box: served.box ?? null,
      shape: served.shape ?? null,
      // A `bigint`, as the same count is on the Arrow wire, so a panel prints both one way.
      maskedCount: BigInt(served.masked_count)
    };
  }

  /**
   * `POST /v1/artifacts/browse`: one page of a layer's hierarchy, read by parent and child links
   * whatever the map shows: its roots, one artifact's children with that artifact's parents, or a
   * search by name. Each row carries its masked count and, where `filters` is set, its matched
   * count. Rows are ordered by count, highest first, then by `tessera_id`; the count is the matched
   * one under `filters` and the masked one otherwise. Pass `next` back as `cursor` for the
   * following page; it is `null` on the last.
   *
   * A parent is listed only where this principal is served both ends of the link. A `parent` this
   * principal is not served answers an empty page.
   *
   * @throws {@link TesseraError} when the server refuses: `404` for an unknown view, `422` for an
   *   unknown layer, both `parent` and `q`, `limit: 0` or a malformed filter.
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
      body: jsonBody(body),
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
   * `POST /v1/items`: a bulk read of the items this principal may see in a view. Resolves when the
   * first response's head arrives. Iterating the result yields each page as an Arrow table,
   * following each response's cursor until the read ends; {@link RecordsRead} says what ends it
   * early.
   *
   * Each request after the first is the caller's request with `cursor` set to the one the response
   * before it ended with, and without `count`, which the server takes only on a read's first
   * request.
   *
   * @throws {@link TesseraError} when the server refuses the first request: `404` for an unknown
   *   view, `422` for a request {@link ItemsRequest} says is refused or a malformed filter, and
   *   `429` past the bulk-read admission limit.
   */
  items(token: string, req: ItemsRequest, signal?: AbortSignal): Promise<RecordsRead<ItemsHead>> {
    return this.bulkRead('items', token, req, signal, (raw) => {
      requireFields(raw, ['order', 'page_rows'], 'the head of a /v1/items response');
      const head = raw as {order: ItemsHead['order']; page_rows: number; visible?: number; matched?: number};
      return {order: head.order, pageRows: head.page_rows, visible: head.visible ?? null, matched: head.matched ?? null};
    });
  }

  /**
   * `POST /v1/artifacts`: a bulk read of the artifacts of one layer this principal is served, as
   * {@link items} reads items.
   *
   * @throws {@link TesseraError} when the server refuses the first request: `404` for an unknown
   *   view, `422` for a request {@link ArtifactsRequest} says is refused or a malformed filter, and
   *   `429` past the bulk-read admission limit.
   */
  artifacts(token: string, req: ArtifactsRequest, signal?: AbortSignal): Promise<RecordsRead<ArtifactsHead>> {
    return this.bulkRead('artifacts', token, req, signal, (raw) => {
      requireFields(raw, ['page_rows'], 'the head of a /v1/artifacts response');
      const head = raw as {page_rows: number; served?: number; matched?: number};
      return {pageRows: head.page_rows, served: head.served ?? null, matched: head.matched ?? null};
    });
  }

  private bulkRead<Head>(
    route: 'items' | 'artifacts',
    token: string,
    req: ItemsRequest | ArtifactsRequest,
    signal: AbortSignal | undefined,
    parseHead: (raw: unknown) => Head
  ): Promise<RecordsRead<Head>> {
    const request = async (cursor?: string) => {
      const given = cursor === undefined ? req : {...req, cursor, count: undefined};
      // Each field that is set, under its wire name. A `tessera_id` travels as a decimal string.
      const body: Record<string, unknown> = {};
      for (const [name, value] of Object.entries(given)) {
        if (value === undefined) continue;
        body[name.replace(/[A-Z]/g, (c) => `_${c.toLowerCase()}`)] = typeof value === 'bigint' ? value.toString() : value;
      }
      const response = await this.send(`${this.opts.viewerUrl}/v1/${route}`, {
        method: 'POST',
        headers: {authorization: `Bearer ${token}`, 'content-type': 'application/json'},
        body: jsonBody(body),
        signal
      });
      if (!response.ok) await fail(response);
      return response;
    };
    return openRecords(req, request, signal, parseHead);
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
    // Absent where the publisher supplied none and where this principal may not read the text.
    // The two look the same, as they do on the artifacts frame.
    key: r.key ?? null,
    name: r.name ?? null,
    maskedCount: BigInt(r.masked_count),
    matchedCount: r.matched_count === undefined || r.matched_count === null ? null : BigInt(r.matched_count),
    rung: r.rung,
    // A null cell is the empty list.
    parentIds: (r.parent_ids ?? []).map((v) => BigInt(v))
  };
}

/** The fields `/v1/meta` must carry, and those each of its blocks and list elements must. */
const VIEW_FIELDS = ['id', 'display_name', 'quantisation', 'projection', 'world_aspect', 'tile_scheme', 'tile', 'group', 'key', 'metadata'];
const GROUP_FIELDS = ['name', 'title', 'members_of', 'views'];
/**
 * A request body as JSON, a `bigint` written as its decimal digits in a string: a filter
 * comparand past 2^53, which a JSON number read as a double would round, is sent that way.
 */
function jsonBody(body: unknown): string {
  return JSON.stringify(body, (_, value) => (typeof value === 'bigint' ? value.toString() : value));
}

const DECLARED_FIELDS = ['name', 'arrow_type', 'category', 'analyser', 'render', 'index', 'unique', 'homes'];
const SCOPED_FIELDS = ['name', 'arrow_type', 'scope', 'category', 'analyser', 'render', 'index', 'views'];
const OPERAND_FIELDS = ['column', 'family', 'operands'];
const LAYER_FIELDS = ['name', 'title', 'views', 'membership', 'hierarchy', 'levels', 'computed_content', 'shape', 'supplied_content', 'depends_on', 'version'];
const LEVEL_FIELDS = ['level', 'title', 'zoom'];
const META_FIELDS = ['api_version', 'bundle_format', 'views', 'groups', 'declared_scalars', 'scoped_scalars', 'filter_operands', 'selection', 'layers'] as const;
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
  views: {
    id: string;
    display_name: string;
    quantisation: {x_min: number; x_max: number; y_min: number; y_max: number};
    projection: MapProjection;
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
    unique: boolean;
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
