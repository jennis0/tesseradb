import {
  checkArtifactsTrailer,
  checkTrailerCounts,
  decodeSubCells,
  decodeTiles,
  decodeViewport,
  parseTrailer,
  stageNsOf,
  type ArtifactsFramePart,
  type PointsPart
} from './decode.js';
import {createDecoder, type Decoder} from './decoder.js';
import {parseRegionVerdict} from './region.js';
import {FRAME_ARTIFACTS, FRAME_POINTS, FRAME_SUB_CELLS, FRAME_TILES, FRAME_TRAILER, FrameReader, type Frame} from './frame.js';
import {readAggregate, type PartialAggregate} from './aggregate.js';
import {openRecords, type RecordsRead} from './records.js';
import type {AggregateRequest, AggregateResult, ArrowType, ArtifactDetail, ArtifactsHead, ArtifactsRequest, BrowsePage, BrowseRequest, BrowseRow, CategoryValue, CountsSink, FilterExpr, FilterOperandSet, Grouping, ItemDetail, ItemsHead, ItemsRequest, Layer, Login, LoginCredential, AuthoriseTarget, MapProjection, Meta, RegionVerdict, Session, Shape, ShapeKind, SuggestResult, TileCounts, TileScheme, ViewMetadataValue, ViewportArtifactsFrame, ViewportArtifactsRequest, ViewportArtifactsResponse, ViewportCounts, ViewportPart, ViewportRequest, ViewportResponse, ViewportResult} from './types.js';

/**
 * Receives a streamed `/v1/viewport` response's points, one points frame at a time, as
 * {@link TesseraClient.viewport} decodes them. Parts arrive in the order the server sent them, and
 * the next part is not handed over until a returned promise settles.
 *
 * Each part's `result` holds whole tiles: the counts of the tiles whose points it carries, and those
 * points. Its `subCells` is `null`.
 *
 * @category HTTP client
 */
export type PartSink = (part: ViewportPart) => void | Promise<void>;

/** A viewport body's counts, decoded. */
type Counts = Pick<ViewportCounts, 'tiles' | 'subCells'>;

/**
 * Receives a `/v1/artifacts/viewport` response's frames, one at a time, as
 * {@link TesseraClient.viewportArtifacts} decodes them, in wire order, with the response's keys, so
 * a frame can be held under the right principal before the response resolves. The next frame is
 * not handed over until a returned promise settles.
 *
 * @category HTTP client
 */
export type TileSink = (frame: ViewportArtifactsFrame, keys: {identityKey: string; contentKey: string}) => void | Promise<void>;

/**
 * Decodes a viewport body's counts once they are whole: at the tiles frame, or at the sub-cells
 * frame the server sends straight after it where the request asked for an underlay. Returns a
 * function to pass each frame in wire order until it answers; it answers `null` before the counts
 * are whole and the counts at the frame that completes them. Any other frame after the tiles
 * completes them without sub-cells.
 */
function countsReader(underlay: boolean): (frame: Frame) => Counts | null {
  let tiles: Uint8Array | null = null;
  return (frame) => {
    if (frame.kind === FRAME_TILES) {
      tiles = frame.payload;
      return underlay ? null : {tiles: decodeTiles(tiles), subCells: null};
    }
    if (tiles === null) return null;
    return {tiles: decodeTiles(tiles), subCells: frame.kind === FRAME_SUB_CELLS ? decodeSubCells(frame.payload) : null};
  };
}

/** A watch for {@link readWatching} that hands the counts to `sink` with the response's keys. */
function countsWatch(sink: CountsSink, underlay: boolean, keys: {identityKey: string; contentKey: string}): (frame: Frame) => boolean {
  const read = countsReader(underlay);
  return (frame) => {
    const counts = read(frame);
    if (counts) sink({...counts, ...keys});
    return counts !== null;
  };
}

/**
 * Reads a whole body, passing each frame to `watch` as it completes until `watch` returns `true`.
 * The frames are checked against the viewport grammar as they pass.
 */
async function readWatching(response: Response, watch: (frame: Frame) => boolean): Promise<Uint8Array> {
  const frames = new FrameReader();
  if (!response.body) {
    const bytes = new Uint8Array(await response.arrayBuffer());
    for (const frame of frames.push(bytes)) if (watch(frame)) break;
    return bytes;
  }
  const reader = response.body.getReader();
  const chunks: Uint8Array[] = [];
  let size = 0;
  let watching = true;
  try {
    for (;;) {
      const {done, value} = await reader.read();
      if (done) break;
      chunks.push(value);
      size += value.byteLength;
      if (!watching) continue;
      for (const frame of frames.push(value)) {
        if (watch(frame)) {
          watching = false;
          break;
        }
      }
    }
  } catch (error) {
    void reader.cancel().catch(() => {});
    throw error;
  }
  const bytes = new Uint8Array(size);
  let at = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, at);
    at += chunk.byteLength;
  }
  return bytes;
}

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
    readonly detail: string,
    /**
     * The wait the server asked for before the request is sent again, in seconds: its `Retry-After`
     * header, else the body's `retry_after_s`; `null` where it named none.
     */
    readonly retryAfterS: number | null = null
  ) {
    super(`${status} ${code}: ${detail}`);
    this.name = 'TesseraError';
  }
}

async function fail(response: Response): Promise<never> {
  let code = 'unknown';
  let detail = response.statusText;
  let wait = response.headers.get('retry-after');
  try {
    const body = (await response.json()) as {error?: string; detail?: string; retry_after_s?: number};
    code = body.error ?? code;
    detail = body.detail ?? detail;
    wait ??= body.retry_after_s === undefined ? null : String(body.retry_after_s);
  } catch {
    // A body that is not JSON, such as a proxy's, still gives a TesseraError.
  }
  const seconds = wait === null ? NaN : Number(wait);
  throw new TesseraError(response.status, code, detail, Number.isFinite(seconds) && seconds >= 0 ? seconds : null);
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
   * The bearer token `authorise` and `revoke` send; no other method uses it. It is an API key whose
   * principal holds `authorise-as`, or the operator credential, which alone may name a session's
   * terms. Either mints a session for any principal, so it belongs to an integrator's backend or an
   * operator's script. A browser page can reach the session listener only from an origin the
   * deployment lists in `serve.dev_cors_origins`, a development setting.
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
   * otherwise, made at the first `viewport` call. A decoder passed here belongs to the host, which
   * closes it; {@link TesseraClient.close} closes only a decoder the client built.
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

  /**
   * Closes the decoder the client built, terminating its workers if it has any. A decoder passed
   * as `decoder` is left open for the host to close.
   */
  close(): void {
    if (!this.opts.decoder) this.decoder?.close();
    this.decoder = null;
  }

  /**
   * `POST /v1/login`: mints a session for the principal the credential authenticates, which must
   * hold `read`. The credential travels in the body, and no bearer is sent.
   *
   * @param credential - Exactly one credential: a local principal's name and password, an API key,
   *   or an OIDC access token.
   * @returns The session: `token` for the viewer methods and `logout`, and `expiresAt` in seconds
   *   since the Unix epoch.
   * @throws {@link TesseraError} when the server refuses: `401` for every credential it does not
   *   accept, `403` where the principal does not hold `read`, `429` under load.
   */
  async login(credential: LoginCredential, signal?: AbortSignal): Promise<Login> {
    const body =
      'password' in credential
        ? {password: {principal: credential.principal, password: credential.password}}
        : 'apiKey' in credential
          ? {api_key: credential.apiKey}
          : {access_token: credential.accessToken};
    const response = await this.send(`${this.opts.viewerUrl}/v1/login`, {
      method: 'POST',
      headers: {'content-type': 'application/json'},
      body: JSON.stringify(body),
      signal
    });
    if (!response.ok) await fail(response);
    const answer = (await response.json()) as {token: string; expires_at: number};
    return {token: answer.token, expiresAt: answer.expires_at};
  }

  /**
   * `POST /v1/logout`: ends the session of `token`.
   *
   * @throws {@link TesseraError} when the server refuses: `401` or `403` for a token whose session
   *   has already ended.
   */
  async logout(token: string, signal?: AbortSignal): Promise<void> {
    const response = await this.send(`${this.opts.viewerUrl}/v1/logout`, {
      method: 'POST',
      headers: {authorization: `Bearer ${token}`},
      signal
    });
    if (!response.ok) await fail(response);
  }

  /**
   * `POST /session/authorise`: mints a session with the `sessionCredential` option. For a principal,
   * the session carries the target's terms and its `read` and `write`, and the target must hold
   * `read`. For `terms`, which only the operator credential may name, the session holds those terms
   * and `read`. A term is trimmed, and one that is empty, holds a control character or is `public`
   * is not held.
   *
   * @param target - The local principal to act as, by name, the OIDC identity whose access token is
   *   passed on, or the terms the session holds.
   * @returns The session: `token` for the viewer methods, `tokenId` for `revoke`, and `expiresAt`
   *   in seconds since the Unix epoch.
   * @throws `Error` when the options carry no `sessionCredential`.
   * @throws {@link TesseraError} when the server refuses: `401` for a credential it does not
   *   accept, `403` where the key's principal does not hold `authorise-as`, the target does not
   *   hold `read`, or a key names `terms`, `404` where `principal` names no enabled principal,
   *   `422` for an access token it does not accept, `429` under load.
   */
  async authorise(target: AuthoriseTarget, signal?: AbortSignal): Promise<Session> {
    const credential = this.requireSessionCredential('authorise');
    const body =
      'principal' in target
        ? {principal: target.principal}
        : 'terms' in target
          ? {terms: target.terms}
          : 'readAll' in target
            ? {read_all: true}
            : {access_token: target.accessToken};
    const response = await this.send(`${this.opts.sessionUrl}/session/authorise`, {
      method: 'POST',
      headers: {authorization: `Bearer ${credential}`, 'content-type': 'application/json'},
      body: JSON.stringify(body),
      signal
    });
    if (!response.ok) await fail(response);
    const answer = (await response.json()) as {token: string; token_id: number; expires_at: number};
    return {token: answer.token, tokenId: answer.token_id, expiresAt: answer.expires_at};
  }

  /**
   * `POST /session/revoke`: ends the session `tokenId` names, so the token itself is not sent
   * again. An API key ends a session a key of its own principal minted, and the operator credential
   * ends any session. An id naming no such live session is not refused.
   *
   * @throws `Error` when the options carry no `sessionCredential`.
   * @throws {@link TesseraError} when the server refuses: `401` for a credential it does not
   *   accept, `403` where the key's principal does not hold `authorise-as`, `422` for a malformed
   *   request.
   */
  async revoke(tokenId: number, signal?: AbortSignal): Promise<void> {
    const credential = this.requireSessionCredential('revoke');
    const response = await this.send(`${this.opts.sessionUrl}/session/revoke`, {
      method: 'POST',
      headers: {authorization: `Bearer ${credential}`, 'content-type': 'application/json'},
      body: JSON.stringify({token_id: tokenId}),
      signal
    });
    if (!response.ok) await fail(response);
  }

  private requireSessionCredential(verb: string): string {
    if (!this.opts.sessionCredential) {
      throw new Error(
        `${verb} needs the sessionCredential option: an API key whose principal holds authorise-as, or the operator credential`
      );
    }
    return this.opts.sessionCredential;
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
        maxArtifactsPerTile: m.selection.max_artifacts_per_tile,
        maxCategoryValues: m.selection.max_category_values,
        maxRegionVertices: m.selection.max_region_vertices,
        maxRegionCells: m.selection.max_region_cells,
        maxBrowseRows: m.selection.max_browse_rows,
        maxShapeVertices: m.selection.max_shape_vertices,
        maxSuggestions: m.selection.max_suggestions,
        maxSuggestionWalk: m.selection.max_suggestion_walk,
        maxSuggestSetEntities: m.selection.max_suggest_set_entities,
        maxPageRows: m.selection.max_page_rows,
        maxPageBytes: m.selection.max_page_bytes,
        maxAggregateGroupings: m.selection.max_aggregate_groupings,
        maxAggregateTop: m.selection.max_aggregate_top,
        maxAggregateNamed: m.selection.max_aggregate_named,
        maxAggregateBins: m.selection.max_aggregate_bins,
        maxAggregateCells: m.selection.max_aggregate_cells
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
   * `POST /v1/viewport`: for one region at one depth, each tile's counts and a sample of up to `k`
   * points per tile, each point tagged with the artifacts of the layers `layers` names. A field of `req` left unset is left out of the
   * body, so the server's default applies: `k` defaults to `selection.kMaxMarks` from
   * {@link TesseraClient.meta}, and the server caps it at `selection.maxK`. `k = 0` asks for the
   * counts alone, which decode on this thread.
   *
   * @param opts.signal - Aborts the request and the read of its body.
   * @param opts.background - Decode on the decoder's background worker, so a response the user is
   *   waiting for does not queue behind this one. Defaults to `false`.
   * @param opts.onPart - Receives the points as each points frame is decoded, before the body has
   *   finished. With a sink the returned `result` carries the counts and sub-cells and no points.
   *   Not called for `k = 0`.
   * @param opts.onCounts - Receives the tiles' counts, and the sub-cells where `underlayOffset` asked
   *   for them, as soon as they land: before any point, and before the body has
   *   finished, with or without `onPart` and at any `k`. Called at most once. The returned
   *   `result` carries the same counts.
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
    opts: {signal?: AbortSignal; background?: boolean; onPart?: PartSink; onCounts?: CountsSink} = {}
  ): Promise<ViewportResponse> {
    const {signal, background = false, onPart, onCounts} = opts;
    // Each optional field is sent only when the caller set it, so an unset one takes the server's
    // default. For `layers` and `levels` an empty array is a request for none, which differs from
    // leaving the field out.
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
    if (req.levels !== undefined) body.levels = req.levels;
    // The stamp is held as the string the server sent and travels as the object it parses to.
    if (req.stamp) body.pin = JSON.parse(req.stamp);

    const response = await this.send(`${this.opts.viewerUrl}/v1/viewport`, {
      method: 'POST',
      headers: {authorization: `Bearer ${token}`, 'content-type': 'application/json'},
      body: jsonBody(body),
      signal
    });
    if (!response.ok) await fail(response);
    const coordinates = coordinatesOf(response);
    const decoder = (this.decoder ??= this.opts.decoder ?? createDecoder());
    // A counts-only response (`k = 0`) has no points and decodes in milliseconds, so it decodes on
    // this thread rather than queueing behind a point decode in a worker lane. It has no points
    // frames to hand a part sink.
    const counts = req.k === 0;
    const keys = {identityKey: coordinates.identityKey, contentKey: coordinates.contentKey};
    const columnsAsked = Array.isArray(req.pointRows) ? [...req.pointRows] : null;
    const layersAsked: readonly string[] | 'all' = req.layers === 'all' ? 'all' : [...(req.layers ?? [])];
    const underlay = Boolean(req.underlayOffset);
    const decoded =
      onPart && !counts
        ? await this.streamed(decoder, response, {...keys, columnsAsked, layersAsked}, onPart, background, underlay, onCounts)
        : await this.whole(decoder, response, counts, background, onCounts && countsWatch(onCounts, underlay, keys));
    this.opts.onDecode?.(decoded.ms, decoded.bytes, decoded.points, decoded.workerMs);
    return {
      result: decoded.result,
      timings: {...timingsOf(response), stageNs: decoded.stageNs},
      ...coordinates,
      columnsAsked,
      layersAsked,
      bytes: decoded.bytes
    };
  }

  /**
   * Reads the whole body, then decodes it: the path for a caller with no part sink. `watch`, where
   * given, sees the frames as they arrive.
   */
  private async whole(
    decoder: Decoder,
    response: Response,
    counts: boolean,
    background: boolean,
    watch?: (frame: Frame) => boolean
  ): Promise<Decoded> {
    const started = performance.now();
    const bytes = watch ? await readWatching(response, watch) : new Uint8Array(await response.arrayBuffer());
    // Read before decoding: the worker path transfers the buffer, which detaches it and leaves
    // `byteLength` at 0.
    const size = bytes.byteLength;
    const stageNs = stageNsOfBody(bytes);
    const result = counts ? decodeViewport(bytes) : await decoder.decode(bytes, background);
    return {
      result,
      bytes: size,
      points: result.ids.length,
      ms: performance.now() - started,
      workerMs: counts ? null : decoder.lastWorkerMs,
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
    decoder: Decoder,
    response: Response,
    coordinates: {identityKey: string; contentKey: string; columnsAsked: readonly string[] | null; layersAsked: readonly string[] | 'all'},
    onPart: PartSink,
    background: boolean,
    underlay: boolean,
    onCounts?: CountsSink
  ): Promise<Decoded> {
    const started = performance.now();
    if (!response.body) {
      // A `fetch` that gives no stream, such as a polyfill or a mock. The sink gets the whole
      // result as one part.
      const whole = await this.whole(decoder, response, false, background, onCounts && countsWatch(onCounts, underlay, coordinates));
      await onPart({result: whole.result, ...coordinates});
      return {...whole, result: headOnly(whole.result)};
    }
    const reader = response.body.getReader();
    const frames = new FrameReader();
    // The counts are decoded here, once, as they land.
    const readCounts = countsReader(underlay);
    let counts: Counts | null = null;
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

    const deliver = (decoding: Promise<PointsPart>) => {
      // Awaited only when the chain reaches it, which is never once an earlier frame has failed.
      decoding.catch(() => {});
      delivering = delivering.then(async () => {
        const part = await decoding;
        // Read here: the decoder reports the last reply's time.
        const frameMs = decoder.lastWorkerMs;
        if (abandoned) return;
        if (frameMs !== null) workerMs = (workerMs ?? 0) + frameMs;
        const rows = part.ids.length;
        points += rows;
        // The grammar puts the tiles frame before every points frame, so the counts are whole.
        const run = tilesFor(counts!.tiles, tileAt, rows);
        tileAt = run.next;
        await onPart({
          result: {
            tiles: run.tiles,
            // The part's `projection` is the result's `pointsProjection`.
            ...part,
            pointsProjection: part.projection,
            subCells: null
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
          if (counts === null) {
            counts = readCounts(frame);
            if (counts) onCounts?.({...counts, ...coordinates});
          }
          switch (frame.kind) {
            case FRAME_POINTS:
              flushes += 1;
              deliver(decoder.decodePoints(frame.payload, background));
              break;
            case FRAME_TRAILER:
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
    const whole = counts!;
    const trailer = parseTrailer(trailerBytes!);
    checkTrailerCounts(trailer, flushes, points);
    // Every tile the server served points for has had them.
    for (let i = tileAt; i < whole.tiles.length; i++) {
      if (whole.tiles[i]!.served !== 0n) {
        throw new Error(`tile ${whole.tiles[i]!.tile} was served ${whole.tiles[i]!.served} points that no frame carried`);
      }
    }
    return {
      result: {tiles: whole.tiles, ...emptyPoints(), subCells: whole.subCells},
      bytes,
      points,
      ms: performance.now() - started,
      workerMs,
      stageNs: stageNsOf(trailer)
    };
  }

  /**
   * `POST /v1/artifacts/viewport`: for one region at one depth, the artifacts of the named layers
   * in each tile: those this principal is served that hold a member it can see inside the tile, at
   * most `perTile` per level, largest first. An artifact in several tiles is in each tile's frame,
   * with the same figures. A `nested` or `dag` layer is answered over every tile together, as one
   * frame first. A field of `req` left unset is left out of the body, so the server's default
   * applies; `perTile` has none and is always sent.
   *
   * @param opts.signal - Aborts the request and the read of its body.
   * @param opts.background - Decode on the decoder's background worker, as for
   *   {@link TesseraClient.viewport}.
   * @param opts.onTile - Receives each frame as it is decoded, in wire order, before the body has
   *   finished. A frame handed over is whole and correct even where the body later fails.
   * @returns Every frame, the response's keys (`identityKey`, `contentKey`, `pin`, `stale`,
   *   `region`), the server's timings and the body's size in `bytes`.
   * @throws {@link TesseraError} when the server refuses: `404` for an unknown view, `422` for a
   *   malformed request or a `perTile` over `selection.maxArtifactsPerTile`, `429` under load.
   * @throws `Error` when the body is malformed or ends before its trailer.
   */
  async viewportArtifacts(
    token: string,
    req: ViewportArtifactsRequest,
    opts: {signal?: AbortSignal; background?: boolean; onTile?: TileSink} = {}
  ): Promise<ViewportArtifactsResponse> {
    const {signal, background = false, onTile} = opts;
    const body: Record<string, unknown> = {view: req.view, zoom: req.zoom, per_tile: req.perTile};
    if (req.bbox) body.bbox = req.bbox;
    if (req.tiles) body.tiles = req.tiles.map(Number);
    if (req.layers !== undefined) body.layers = req.layers;
    if (req.levels !== undefined) body.levels = req.levels;
    if (req.computed !== undefined) body.computed = req.computed;
    if (req.filters) body.filters = req.filters;
    if (req.highlight) body.highlight = req.highlight;
    if (req.budget !== undefined) body.budget = req.budget;
    if (req.paletteSize !== undefined) body.palette_size = req.paletteSize;
    if (req.stamp) body.pin = JSON.parse(req.stamp);

    const started = performance.now();
    const response = await this.send(`${this.opts.viewerUrl}/v1/artifacts/viewport`, {
      method: 'POST',
      headers: {authorization: `Bearer ${token}`, 'content-type': 'application/json'},
      body: jsonBody(body),
      signal
    });
    if (!response.ok) await fail(response);
    const decoder = (this.decoder ??= this.opts.decoder ?? createDecoder());
    const coordinates = coordinatesOf(response);
    const keys = {identityKey: coordinates.identityKey, contentKey: coordinates.contentKey};
    // A frame of no rows names no tile, so a tile frame is matched to the request's tiles by
    // position, as the server answers them: in order, with repeats removed.
    const asked = req.tiles ? [...new Set(req.tiles)] : null;
    const reader = new FrameReader('artifacts');
    const out: ViewportArtifactsFrame[] = [];
    let trailer: Uint8Array | null = null;
    let bytes = 0;
    let rows = 0;
    let kind5 = 0;
    let tileFrames = 0;
    let workerMs: number | null = null;
    let abandoned = false;
    // Frames are decoded in the lanes in turn and handed over in wire order by awaiting each in turn.
    let delivering: Promise<void> = Promise.resolve();
    const deliver = (decoding: Promise<ArtifactsFramePart>, first: boolean) => {
      // Awaited only when the chain reaches it, which is never once an earlier frame has failed.
      decoding.catch(() => {});
      delivering = delivering.then(async () => {
        const part = await decoding;
        const frameMs = decoder.lastWorkerMs;
        if (abandoned) return;
        if (frameMs !== null) workerMs = (workerMs ?? 0) + frameMs;
        // The treed frame is first and holds a row; every row of a tile frame names its tile.
        const treed = first && part.artifacts.length > 0 && part.tile === null;
        const frame: ViewportArtifactsFrame = {treed, tile: treed ? null : (part.tile ?? asked?.[tileFrames] ?? null), artifacts: part.artifacts};
        if (!treed) tileFrames += 1;
        rows += part.artifacts.length;
        out.push(frame);
        await onTile?.(frame, keys);
      });
      delivering.catch(() => {});
    };
    try {
      const consume = (chunk: Uint8Array) => {
        bytes += chunk.byteLength;
        for (const frame of reader.push(chunk)) {
          if (frame.kind === FRAME_ARTIFACTS) {
            kind5 += 1;
            deliver(decoder.decodeArtifacts(frame.payload, background), kind5 === 1);
          } else if (frame.kind === FRAME_TRAILER) {
            trailer = frame.payload;
          }
        }
      };
      if (!response.body) consume(new Uint8Array(await response.arrayBuffer()));
      else {
        const body = response.body.getReader();
        try {
          for (;;) {
            const {done, value} = await body.read();
            if (done) break;
            consume(value);
          }
        } catch (error) {
          void body.cancel().catch(() => {});
          throw error;
        }
      }
    } catch (error) {
      abandoned = true;
      throw error;
    }
    await delivering;
    reader.end();
    checkArtifactsTrailer(trailer!, kind5, rows);
    this.opts.onDecode?.(performance.now() - started, bytes, 0, workerMs);
    return {frames: out, timings: {...timingsOf(response), stageNs: null}, ...coordinates, bytes};
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
   * `/v1/categories/{column}/suggest`: up to `limit` values of a category whose key, title or a
   * word start of either begins with `q`, compared after case folding. Values are ordered by the
   * matched text, and only values this principal may see are offered, as for
   * {@link TesseraClient.categories}. The page echoes `q` as sent, so a caller can match a page to
   * its request. Without `filters` this is a `GET`; with it, a `POST` carrying the same fields.
   *
   * Every `429` the server answers is returned as `{status: 'shed', retryAfterS, detail}`
   * rather than thrown; {@link SuggestResult} lists the causes.
   *
   * @param column - As for {@link TesseraClient.categories}.
   * @param q - The text typed. Empty matches every value.
   * @param opts.limit - The page size, capped at `selection.maxSuggestions`, which is also the
   *   default.
   * @param opts.counts - `true` adds each value's count of the items carrying it that this
   *   principal may see, and the page's `total`, the number of items the counts are taken over.
   * @param opts.view - As for `categories`, and the view `filters` is evaluated in.
   * @param opts.filters - The filter expression the viewport takes. Each count is then of the items
   *   in `view` that pass it. It changes nothing else: a value it excludes is offered with count
   *   `0`. Needs `view`.
   * @throws {@link TesseraError} for any other refusal: `404` for a column that is not a category,
   *   `422` for a `q` over 256 bytes, `filters` without `view`, or a malformed request.
   */
  async suggest(
    token: string,
    column: string,
    q: string,
    opts: {limit?: number; counts?: boolean; view?: string; filters?: FilterExpr; signal?: AbortSignal} = {}
  ): Promise<SuggestResult> {
    const route = `${this.opts.viewerUrl}/v1/categories/${encodeURIComponent(column)}/suggest`;
    let response: Response;
    if (opts.filters !== undefined) {
      const body: Record<string, unknown> = {q, filters: opts.filters};
      if (opts.limit !== undefined) body.limit = opts.limit;
      if (opts.counts) body.counts = true;
      if (opts.view !== undefined) body.view = opts.view;
      response = await this.send(route, {
        method: 'POST',
        headers: {authorization: `Bearer ${token}`, 'content-type': 'application/json'},
        body: JSON.stringify(body),
        signal: opts.signal
      });
    } else {
      const params = new URLSearchParams({q});
      if (opts.limit !== undefined) params.set('limit', String(opts.limit));
      if (opts.counts) params.set('counts', 'true');
      if (opts.view !== undefined) params.set('view', opts.view);
      response = await this.send(`${route}?${params.toString()}`, {
        headers: {authorization: `Bearer ${token}`},
        signal: opts.signal
      });
    }
    if (response.status === 429) {
      // The body's `retry_after_s` is what the contract requires; the header is the fallback for
      // a body that does not parse.
      let retryAfterS = Number(response.headers.get('retry-after') ?? '1');
      let detail: string | null = null;
      try {
        const body = (await response.json()) as {retry_after_s?: number; detail?: string};
        if (typeof body.retry_after_s === 'number') retryAfterS = body.retry_after_s;
        if (typeof body.detail === 'string') detail = body.detail;
      } catch {
        // A non-JSON 429 (a proxy's) still yields a retryable outcome from the header alone.
      }
      return {status: 'shed', retryAfterS: Number.isFinite(retryAfterS) ? retryAfterS : 1, detail};
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
      more: body.more,
      ...(body.total !== undefined ? {total: body.total} : {}),
      ...regionOf(response)
    };
  }

  /**
   * `POST /v1/items/{tessera_id}`: one item's whole record. `fields` is keyed by column name, with
   * a category given as its vocabulary key and a column the item has no value for left out.
   * `labels` is why this session sees the item: its labels read as one disjunction, each held term
   * among its operands and one satisfied clause of each conjunction among them, as label text, and
   * nothing else. A held term that appears only inside a conjunction is not listed. `views` and
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
      labels: string[];
      views: {id: string; x: number; y: number}[];
      scoped: Record<string, Record<string, unknown>>;
    };
    return {
      fields: body.fields ?? {},
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
    if (req.paletteSize !== undefined) body.palette_size = req.paletteSize;
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
    const request = (cursor?: string) => this.post(route, token, cursor === undefined ? req : {...req, cursor, count: undefined}, signal);
    return openRecords(req, request, signal, parseHead);
  }

  /**
   * `POST /v1/aggregate`: how the items this principal may see in a view are distributed, as one
   * table of exact counts per grouping. Every page of every table is read, response after response,
   * each requested from the cursor the one before it ended with, and each table's pages are joined
   * into one Arrow table. {@link AggregateTable} lists the columns.
   *
   * Each response composes the visible set again, so a deletion or suppression accepted during the
   * read applies from the next response, and {@link AggregateResult.recomposed} says where a page
   * counted a different state of the corpus.
   *
   * ```ts
   * const {tables} = await client.aggregate(token, {
   *   view: 'papers',
   *   filters: {year: {range: {gte: 2020}}},
   *   groupings: [{}, {by: {field: 'venue', top: 10}}]
   * });
   * console.log(tables[0]!.total, tables[1]!.rows.toArray());
   * ```
   *
   * @param options - `follow`: read the responses after the first. Defaults to `true`. With `false`
   *   the result holds the first response's pages, and {@link AggregateResult.next} is where the
   *   read continues, to be passed back as `cursor`.
   * @throws {@link TesseraError} when the server refuses a request: `404` for an unknown view,
   *   `422` for a request the contract refuses, naming the limit where one is exceeded, and `429`
   *   under load.
   * @throws {@link PartialAggregate} for a body cut or ended without its trailer, holding the pages
   *   read before it and the cursor to read on from; `Error` for a trailer that miscounts the body;
   *   and the signal's reason once it aborts.
   */
  aggregate(token: string, req: AggregateRequest, signal?: AbortSignal, options: {follow?: boolean} = {}): Promise<AggregateResult> {
    const sent = {...req, groupings: req.groupings.map(wireGrouping)};
    const request = (cursor?: string) => this.post('aggregate', token, cursor === undefined ? sent : {...sent, cursor}, signal);
    return readAggregate(req, request, signal, options.follow ?? true);
  }

  /** One bulk read's request: each field of `given` that is set, under its wire name. */
  private async post(route: string, token: string, given: object, signal: AbortSignal | undefined): Promise<Response> {
    const body: Record<string, unknown> = {};
    for (const [name, value] of Object.entries(given)) {
      if (value !== undefined) body[name.replace(/[A-Z]/g, (c) => `_${c.toLowerCase()}`)] = value;
    }
    const response = await this.send(`${this.opts.viewerUrl}/v1/${route}`, {
      method: 'POST',
      headers: {authorization: `Bearer ${token}`, 'content-type': 'application/json'},
      // A `tessera_id` travels as a decimal string.
      body: jsonBody(body),
      signal
    });
    if (!response.ok) await fail(response);
    return response;
  }
}

/** A grouping as the wire names it: a layer's `paletteSize` is `palette_size`. */
function wireGrouping(grouping: Grouping): object {
  const by = grouping.by;
  if (!by || !('layer' in by) || by.paletteSize === undefined) return grouping;
  const {paletteSize, ...rest} = by;
  return {...grouping, by: {...rest, palette_size: paletteSize}};
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
  child_count: number;
  slot?: number | null;
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
    parentIds: (r.parent_ids ?? []).map((v) => BigInt(v)),
    childCount: r.child_count,
    slot: r.slot ?? null
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
  'max_artifacts_per_tile',
  'max_category_values',
  'max_shape_vertices',
  'max_region_vertices',
  'max_region_cells',
  'max_suggestions',
  'max_suggestion_walk',
  'max_suggest_set_entities',
  'max_browse_rows',
  'max_page_rows',
  'max_page_bytes',
  'max_aggregate_groupings',
  'max_aggregate_top',
  'max_aggregate_named',
  'max_aggregate_bins',
  'max_aggregate_cells'
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
    max_artifacts_per_tile: number;
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
    max_aggregate_groupings: number;
    max_aggregate_top: number;
    max_aggregate_named: number;
    max_aggregate_bins: number;
    max_aggregate_cells: number;
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
  total?: number;
};

/** A viewport-shaped response's keys, from its headers. */
function coordinatesOf(response: Response): Pick<ViewportResponse, 'identityKey' | 'contentKey' | 'pin' | 'stale' | 'region'> {
  return {
    identityKey: response.headers.get('x-tessera-identity-key') ?? '',
    // The quotes are the entity-tag syntax and not part of the key.
    contentKey: (response.headers.get('etag') ?? '').replace(/^"|"$/g, ''),
    pin: response.headers.get('x-tessera-pin'),
    stale: response.headers.get('x-tessera-stale') === '1',
    // Absent unless the request carried a region leaf.
    region: parseRegionVerdict(response.headers.get('x-tessera-region'))
  };
}

/** The server's times from a viewport-shaped response's headers: to its first flush, not to its end. */
function timingsOf(response: Response): Omit<ViewportResponse['timings'], 'stageNs'> {
  return {
    serverUs: Number(response.headers.get('x-tessera-server-us') ?? 0),
    admissionUs: Number(response.headers.get('x-tessera-admission-us') ?? 0)
  };
}

/** `{region}` where the response carries `x-tessera-region`, and nothing otherwise. */
function regionOf(response: Response): {region?: RegionVerdict} {
  const region = parseRegionVerdict(response.headers.get('x-tessera-region'));
  return region === null ? {} : {region};
}
