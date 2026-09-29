/**
 * The control plane client. This file imports nothing, so the operator scripts can load it under
 * Node's type stripping.
 */

const ARROW = 'application/vnd.apache.arrow.stream';
const JSON_TYPE = 'application/json';

/**
 * The shortest wait between attempts after a `429`, in seconds. A `429` that names no wait, or
 * one shorter than this, waits this long.
 */
export const MIN_BACKOFF = 1;

/**
 * The longest wait between attempts after a `429`, in seconds. A longer `Retry-After` is cut to
 * this.
 */
export const MAX_BACKOFF = 30;

/**
 * How many times one call sends its request while the server answers `429`. The last `429` is
 * returned as the answer.
 */
export const MAX_ATTEMPTS = 600;

/**
 * The `status` of an {@link Answer} to a request that reached no server. It is not an HTTP status.
 *
 * @category Control plane
 */
export const UNANSWERED = 0;

/**
 * One call's answer. Every status, a refusal included, is returned here; none is thrown. Where
 * `ok`, `body` is the route's answer, `B`; otherwise it is the refusal, with `error` and `detail`.
 *
 * @category Control plane
 */
export type Answer<B extends object = Record<string, unknown>> = {
  /** The HTTP status, or {@link UNANSWERED} where no server answered. */
  status: number;
  /** The body as text. Where no server answered, a sentence naming the URL and the error. */
  text: string;
  /** How many times the request was sent, counting each retry after a `429`. */
  attempts: number;
  /** Seconds from the first attempt to the answer, including every wait. */
  seconds: number;
} & (
  | {
      /** `status` is in the 2xx range. */
      ok: true;
      /** The body parsed as JSON. */
      body: B;
    }
  | {
      /** `status` is outside the 2xx range. */
      ok: false;
      /**
       * The body parsed as JSON: the object itself, any other JSON value wrapped as `{value}`, or
       * `{}` where the body is not JSON. A refusal carries `error` and `detail`.
       */
      body: Record<string, unknown>;
    }
);

/**
 * The answer of `ingest`, with the batch id the request carried.
 *
 * @category Control plane
 */
export type RowAnswer = Answer<IngestResponse> & {
  /** The batch id every attempt carried. Pass it as `batch` to send the same body again. */
  batch: string;
};

/**
 * Why the identity rule refused a row. `names_two_items`: the row's values name more than one
 * item. `unknown_tessera_id`: its `tessera_id` names no live or suppressed item. `names_no_item`:
 * its values name no item, in a request that creates none. `one_item_twice`: an earlier row of the
 * batch names the same item. `one_value_twice`: an earlier row of the batch sets the same unique
 * value. The last two are given by `ingest` only.
 *
 * @category Control plane
 */
export type RefusalReason = 'names_two_items' | 'unknown_tessera_id' | 'names_no_item' | 'one_item_twice' | 'one_value_twice';

/**
 * A row the identity rule refused.
 *
 * @category Control plane
 */
export type RefusedRow = {
  /** The row's position in the request, from 0. */
  row: number;
  /** Why it was refused. */
  reason: RefusalReason;
};

/**
 * A member the identity rule refused, which the artifact was published or grown without.
 *
 * @category Control plane
 */
export type RefusedMember = {
  /** The artifact's position in the request, from 0. */
  artifact: number;
  /** The member table the member was in. */
  list: 'members' | 'excluding' | 'leaving';
  /** The member's row in that table, from 0. */
  row: number;
  /** Why it was refused. */
  reason: RefusalReason;
};

/**
 * One item, named by its `tessera_id` and the values of fields declared unique, keyed by column.
 * A unique field's value is a string for a keyword, and a number or a string of decimal digits
 * for an integer or a timestamp. `null` names nothing. Every value names the item that holds it;
 * a row naming no item, or two, is refused. A key that is neither `tessera_id` nor a unique field
 * is refused with `422`.
 *
 * @category Control plane
 */
export type AddressRow = {
  /** The item's `tessera_id`, a string of decimal digits. */
  tessera_id?: string | null;
} & {[field: string]: string | number | null};

/**
 * Items, one row per item, as columns of equal length keyed by `tessera_id` and unique field
 * names. A cell is as in an {@link AddressRow}, and `null` where its column does not name the
 * row's item. `{}` holds no item. Columns of different lengths are refused with `422`.
 *
 * @category Control plane
 */
export type MemberTable = {
  /** Each row's `tessera_id`, a string of decimal digits. */
  tessera_id?: (string | null)[];
} & {[column: string]: (string | number | null)[]};

/**
 * One item of a `POST /control/changes` request, in the route's own field names.
 *
 * @category Control plane
 */
export type ChangeItem = {
  /**
   * `delete` removes the item from every answer, and a compaction removes its rows. `suppress`
   * hides it from every answer until an `unsuppress` lifts the suppression. Both apply from the
   * moment the request is accepted.
   */
  op: 'delete' | 'suppress' | 'unsuppress';
  /** The item. Many changes of one request may name one item. */
  match: AddressRow;
};

/**
 * The artifact another is attached to, such as the cluster a label names. The target exists
 * already, on a layer the attached one's `depends_on` names.
 *
 * @category Control plane
 */
export type Attachment = {
  /** The target's layer. */
  layer: string;
  /** The target's level; absent is 0. */
  level?: number;
  /** The target's key. */
  key: string;
};

/**
 * One artifact of a `publish` request, in the route's own field names. Its members are `members`
 * or `excluding`; an artifact with `attached_to` may carry neither, and is served over its
 * target's members.
 *
 * @category Control plane
 */
export type PublishedArtifact = {
  /** The publisher's key, unique in the level. Needed to name the artifact as a parent or target, or to grow it. */
  key?: string | null;
  /** The key of the view the artifact belongs to. Required on a layer scoped to a view group, refused on any other. */
  view?: string | null;
  /** The members. `{}` is a membership that holds nobody. */
  members?: MemberTable | null;
  /** The view's items the membership leaves out; the membership is every other item of the view. Refused beside `members`. */
  excluding?: MemberTable | null;
  /**
   * Ranked supplied content, most specific first: one value per supplied content the layer
   * declares, and in `generated_from` the items it was made from, where the content requires them.
   */
  content?: {values: string[]; generated_from?: MemberTable | null}[];
  /** The artifact this one is attached to. */
  attached_to?: Attachment | null;
  /** Parent keys, each held already or earlier in the request. */
  parent?: string[];
  /** `[min_x, min_y, max_x, max_y]`, on a layer whose `shape` is `bbox`. */
  bbox?: number[] | null;
  /** `[cx, cy, r]`, on a layer whose `shape` is `circle`. */
  circle?: number[] | null;
  /** `[cx, cy, a, b, angle_degrees]`, on a layer whose `shape` is `ellipse`. */
  ellipse?: number[] | null;
  /** A WKT polygon or multipolygon, on a layer whose `shape` is `polygon`. */
  wkt?: string | null;
  /** This artifact's space, in place of the request's `default_space`. */
  space?: 'view' | 'wgs84' | null;
  /** The artifact's own access labels. `null` or `[]` is no label. */
  access?: string[] | null;
};

/**
 * A `publish` request.
 *
 * @category Control plane
 */
export type PublishRequest = {
  /** The level the artifacts go into; absent is 0. */
  level?: number;
  /** The space of a shape whose artifact names none: `view` takes the view's coordinates, `wgs84` degrees. Absent is `view`. */
  default_space?: 'view' | 'wgs84';
  /** The artifacts, at least one. */
  artifacts: PublishedArtifact[];
};

/**
 * One artifact of a `grow` request, named by the key it was published under. A row with only a key
 * changes nothing; a part the artifact lacks is filled.
 *
 * @category Control plane
 */
export type GrowingArtifact = {
  /** The key the artifact was published under. */
  key: string;
  /** The key of the view the artifact belongs to, as on a publication. */
  view?: string | null;
  /** Absent, `members` join the membership. Present, `members` and `leaving` move the generating set of the content at this rank. */
  rank?: number | null;
  /** Members joining. */
  members?: MemberTable | null;
  /** Members leaving a generating set, after the joins. Needs a `rank`. */
  leaving?: MemberTable | null;
  /** Parent keys. */
  parent?: string[];
  /** The artifact this one is attached to. */
  attached_to?: Attachment | null;
  /** Contents by rank. */
  content?: {rank: number; values: string[]}[];
  /** As on a publication. */
  bbox?: number[] | null;
  /** As on a publication. */
  circle?: number[] | null;
  /** As on a publication. */
  ellipse?: number[] | null;
  /** As on a publication. */
  wkt?: string | null;
  /** This row's space for authored shape content, in place of the request's `default_space`. */
  space?: 'view' | 'wgs84' | null;
  /** The artifact's access labels. */
  access?: string[] | null;
};

/**
 * A `grow` request in its JSON form.
 *
 * @category Control plane
 */
export type GrowRequest = {
  /** As on a publication. */
  level?: number;
  /** As on a publication. */
  default_space?: 'view' | 'wgs84';
  /** The artifacts, at least one. */
  artifacts: GrowingArtifact[];
};

/**
 * Where a write becomes visible.
 *
 * @category Control plane
 */
export type PublicationAck = {
  /** The first publication that includes the write. */
  publication: number;
  /** With `wait`, whether that publication was published before the answer. */
  visible?: boolean;
};

/**
 * The answer of `ingest`. The counts leave out refused rows.
 *
 * @category Control plane
 */
export type IngestResponse = PublicationAck & {
  /** Rows the batch carried. */
  rows: number;
  /** Rows that named no item and created one. */
  created: number;
  /** Rows that edited an item they named. */
  edited: number;
  /** Rows that added an item they named to the batch's view. */
  added: number;
  /** Rows that named an item and changed nothing. */
  unchanged: number;
  /** Rows creating an item whose labels resolve to more terms than the auth plugin's per-item bound. They are stored. */
  over_bound: number;
  /** The positions of the first 100 of those rows. */
  over_bound_rows: number[];
  /** Rows whose coordinates lay outside the projection's domain, stored on the frame's edge. */
  clipped: number;
  /** Rows whose coordinates lay outside the view's extent, stored on its edge. */
  clamped: number;
  /** One per row in the order sent: the item the row created or named, and `null` for a refused row. */
  tessera_ids: (string | null)[];
  /** The rows the identity rule refused. Empty with `strict`, which refuses the batch instead. */
  refused: RefusedRow[];
  /** Artifacts the batch's layer columns created. */
  minted: number;
  /** Memberships the batch's layer columns added. */
  joined: number;
  /** Present where the body was already accepted under this batch id, and nothing was written. */
  replayed?: true;
};

/**
 * The answer of `changes`.
 *
 * @category Control plane
 */
export type ChangesApplied = PublicationAck & {
  /** Changes applied. */
  accepted: number;
  /** The changes refused, in the order sent. Empty with `strict`. */
  refused: RefusedRow[];
};

/**
 * What fitting one shape to each of its layer's views did.
 *
 * @category Control plane
 */
export type ShapeReport = {
  /** The artifact's key. */
  key: string | null;
  /** The rank of the authored shape content reported on; absent for the artifact's own shape. */
  content?: number;
  /** One per view of the layer. */
  views: {
    view: string;
    clipped: boolean;
    outside: boolean;
    rings_dropped: number;
    degrees_looking: boolean;
    vertices_in: number;
    vertices_out: number;
    parts: number;
    rings: number;
    interior_tiles: number;
    boundary_cells: number;
  }[];
};

/**
 * The answer of `publish`.
 *
 * @category Control plane
 */
export type ArtifactsPublished = PublicationAck & {
  /** One per artifact in the order sent, with the held artifact's own `tessera_id` where the key was held. */
  artifacts: {key: string | null; tessera_id: string}[];
  /** Artifacts the request created. */
  created: number;
  /** Of those, the ones with no content on a layer that declares some. */
  without_content: number;
  /** Parts filled on held artifacts. */
  filled: number;
  /** Memberships added, to created and held artifacts alike. */
  joined: number;
  /** The members the identity rule refused, which the artifacts were published without. Empty with `strict`. */
  refused: RefusedMember[];
  /** Present where the request carried a shape or authored shape content. */
  shapes?: ShapeReport[];
};

/**
 * The answer of `grow`.
 *
 * @category Control plane
 */
export type MembershipsGrown = PublicationAck & {
  /** One per artifact in the order sent. */
  artifacts: {key: string; tessera_id: string; joined: number; filled: number; left: number; withdrawn?: number}[];
  /** The members the identity rule refused, which the artifacts were grown without. Empty with `strict`. */
  refused: RefusedMember[];
  /** Present where the request filled authored shape content. */
  shapes?: ShapeReport[];
};

/**
 * Where a {@link Control} sends its requests, and how.
 *
 * @category Control plane
 */
export type ControlOptions = {
  /** The control listener's base URL. Trailing slashes are removed. */
  controlUrl: string;
  /** The operator credential, sent as the bearer token on every request. */
  operatorCredential: string;
  /** Used for every request in place of the global `fetch`. */
  fetch?: typeof fetch;
  /**
   * Headers sent on every request. The headers a route sets itself (`authorization`,
   * `content-type`, and on the row routes `x-tessera-batch-id` and `x-tessera-view`) replace one of
   * the same name here.
   */
  headers?: Record<string, string>;
};

/**
 * What every {@link Control} call takes.
 *
 * @category Control plane
 */
export type CallOptions = {
  /**
   * Aborts the call, during a request or a wait between attempts. The call then rejects with the
   * signal's reason.
   */
  signal?: AbortSignal;
};

/**
 * What a write takes.
 *
 * @category Control plane
 */
export type WriteOptions = CallOptions & {
  /**
   * Sends `wait=visible`. The server brings the next publication forward and holds its answer until
   * the write is visible to viewers, or until `serve.visible_wait_max_secs` passes; the answer's
   * `visible` says which. Defaults to `false`.
   */
  wait?: boolean;
};

/**
 * What a write of rows that name items takes: `ingest`, `changes`, `publish` and `grow`.
 *
 * @category Control plane
 */
export type StrictOptions = WriteOptions & {
  /**
   * Sent as `strict` where given. `true` refuses the whole request at its first refused row, and
   * nothing is written. Left out, the server applies the rows it accepts and lists each refused
   * one in the answer's `refused`.
   */
  strict?: boolean;
};

/**
 * What `ingest` takes.
 *
 * @category Control plane
 */
export type RowOptions = StrictOptions & {
  /**
   * The batch id, sent in `x-tessera-batch-id`. Defaults to a random id made once per call. Pass
   * the `batch` of an earlier {@link RowAnswer} to send its body again.
   */
  batch?: string;
  /**
   * The view the rows belong to, sent in `x-tessera-view`. Required where the database has more
   * than one view; an unknown view is refused with `404`.
   */
  view?: string;
};

/** Base64 of bytes. `btoa` takes one byte per character, so text is encoded to UTF-8 before this. */
export function base64(bytes: Uint8Array): string {
  let binary = '';
  // Chunked, because spreading one argument per byte overflows the stack on a large array.
  for (let i = 0; i < bytes.length; i += 0x8000) {
    binary += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  }
  return btoa(binary);
}

/** A fresh batch id: 128 random bits in hex. */
function freshBatch(): string {
  const bytes = crypto.getRandomValues(new Uint8Array(16));
  return Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('');
}

/** One path segment, percent-encoded, so a name holding `/` reaches the router whole. */
const segment = encodeURIComponent;

/** A path with its query, where the query has anything in it. */
function withQuery(path: string, query: Record<string, string | undefined>): string {
  const params = new URLSearchParams();
  for (const [name, value] of Object.entries(query)) if (value !== undefined) params.set(name, value);
  const text = params.toString();
  return text ? `${path}?${text}` : path;
}

const waiting = (options: WriteOptions) => ({wait: options.wait ? 'visible' : undefined});

const naming = (options: StrictOptions) => ({...waiting(options), strict: options.strict === undefined ? undefined : String(options.strict)});

function decoded(text: string): Record<string, unknown> {
  try {
    const value: unknown = JSON.parse(text);
    if (value !== null && typeof value === 'object' && !Array.isArray(value)) return value as Record<string, unknown>;
    return {value};
  } catch {
    return {};
  }
}

/** How long a `429` asks the caller to wait, in seconds: the header, else the body's figure, else the floor. */
function retryAfter(response: Response, body: Record<string, unknown>): number {
  const seconds = Number(response.headers.get('retry-after') ?? body.retry_after_s);
  return Number.isFinite(seconds) ? Math.max(MIN_BACKOFF, Math.min(seconds, MAX_BACKOFF)) : MIN_BACKOFF;
}

/** Resolves after `seconds`, or rejects with the signal's reason when it aborts first. */
function pause(seconds: number, signal: AbortSignal | undefined): Promise<void> {
  return new Promise((resolve, reject) => {
    if (signal?.aborted) return reject(signal.reason);
    const abort = () => {
      clearTimeout(timer);
      reject(signal!.reason);
    };
    const timer = setTimeout(() => {
      signal?.removeEventListener('abort', abort);
      resolve();
    }, seconds * 1000);
    signal?.addEventListener('abort', abort, {once: true});
  });
}

/**
 * The control plane of one served database, called with the operator credential: one method per
 * route. It keeps no record of what it sent, so what a database holds is asked of the database.
 * A body is sent as the caller gave it: Arrow IPC stream bytes on `ingest`, JSON on
 * declarations, publications and changes, and either on `grow`. A JSON body is serialised once per
 * call, so every attempt of one call sends the same bytes.
 *
 * A `429` is backpressure. The call waits the `Retry-After` it carries, between 1
 * and 30 seconds, and sends the request again, at most 600 times in all. Every other status is returned as an {@link Answer}, neither
 * thrown nor retried. A request that reaches no server is returned with status
 * {@link UNANSWERED}. A call whose `signal` aborts rejects with the signal's reason.
 *
 * `ingest` sends a batch id: the caller's `batch`, or a random id made once per call.
 * The server holds each accepted batch id against its body, so the same bytes sent again under it
 * are answered as a replay with `replayed: true` and no effect, and different bytes under it are
 * refused with `409`. The id is never derived from the body. The same rows sent in two calls are
 * resolved twice: a row naming its item by `tessera_id` or a unique value names in
 * the second call the item the first created, and changes nothing.
 *
 * @category Control plane
 */
export class Control {
  private readonly base: string;
  private readonly credential: string;
  private readonly fetch: typeof fetch | undefined;
  private readonly headers: Record<string, string> | undefined;

  constructor(options: ControlOptions) {
    this.base = options.controlUrl.replace(/\/+$/, '');
    this.credential = options.operatorCredential;
    this.fetch = options.fetch;
    this.headers = options.headers;
  }

  private async send(
    method: string,
    path: string,
    options: CallOptions,
    body?: string | Uint8Array,
    headers: Record<string, string> = {}
  ): Promise<Answer> {
    const url = this.base + path;
    // The route's own headers replace a host header of the same name in any case.
    const all = new Headers(this.headers);
    all.set('authorization', `Bearer ${this.credential}`);
    for (const [name, value] of Object.entries(headers)) all.set(name, value);
    const init: RequestInit = {method, headers: all};
    if (body !== undefined) init.body = body as BodyInit;
    if (options.signal) init.signal = options.signal;
    const started = performance.now();
    const seconds = () => (performance.now() - started) / 1000;
    let attempts = 0;
    for (;;) {
      attempts += 1;
      let response: Response;
      let text: string;
      try {
        response = await (this.fetch ?? fetch)(url, init);
        text = await response.text();
      } catch (error) {
        if (options.signal?.aborted) throw options.signal.reason;
        return {status: UNANSWERED, ok: false, body: {}, text: `${url} did not answer: ${String(error)}`, attempts, seconds: seconds()};
      }
      const answered = decoded(text);
      if (response.status === 429 && attempts < MAX_ATTEMPTS) {
        await pause(retryAfter(response, answered), options.signal);
        continue;
      }
      const ok = response.status >= 200 && response.status < 300;
      return {status: response.status, ok, body: answered, text, attempts, seconds: seconds()};
    }
  }

  private sendJson(method: string, path: string, body: unknown, options: CallOptions): Promise<Answer> {
    return this.send(method, path, options, JSON.stringify(body), {'content-type': JSON_TYPE});
  }

  private async rows(path: string, body: Uint8Array, options: RowOptions): Promise<RowAnswer> {
    const batch = options.batch ?? freshBatch();
    const headers: Record<string, string> = {'content-type': ARROW, 'x-tessera-batch-id': batch};
    if (options.view !== undefined) headers['x-tessera-view'] = options.view;
    const answer = (await this.send('POST', withQuery(path, naming(options)), options, body, headers)) as Answer<IngestResponse>;
    return {...answer, batch};
  }

  /**
   * `GET /control/status`: the running database's state as JSON in `body`, including its
   * publication counter, write executor, caches, admission and `limits`.
   */
  status(options: CallOptions = {}): Promise<Answer> {
    return this.send('GET', '/control/status', options);
  }

  /**
   * The `limits` object of `GET /control/status`: the caps each write route puts on rows, items
   * and body bytes, one object per route. `{}` where the status request was refused or reached no
   * server.
   */
  async limits(options: CallOptions = {}): Promise<Record<string, unknown>> {
    const limits = (await this.status(options)).body.limits;
    return limits !== null && typeof limits === 'object' ? (limits as Record<string, unknown>) : {};
  }

  /**
   * `POST /control/ingest`: one page of rows, given as an Arrow IPC stream. A row names an item by
   * its `tessera_id` and the value of every unique column it carries; a row naming none creates an
   * item at its position, one naming an item it matches changes nothing, one naming an item with
   * no row in the view adds it there, and any other edits the item, which keeps its `tessera_id`.
   * A row without coordinates changes only what it carries. Any column may be left out, which
   * keeps what the item stores; a null clears it. The answer's body counts the rows `created`,
   * `edited`, `added`, `unchanged`, `clipped` and `clamped`, counts in `joined` the annotation
   * memberships the rows added, lists each row's `tessera_id` in request order as a decimal
   * string, and names the `publication` the rows become visible in. A row that only places its
   * item in an annotation changes the annotation, not the item, and is counted `unchanged`.
   *
   * A row whose values name two items, whose `tessera_id` names no item, or which names an item or
   * sets a unique value an earlier row of the page does, is refused: it is listed in `refused`
   * with its reason, its `tessera_id` is `null`, and the other rows are stored. With `strict` the
   * page is refused with `409` at its first refused row, and nothing is stored.
   */
  ingest(body: Uint8Array, options: RowOptions = {}): Promise<RowAnswer> {
    return this.rows('/control/ingest', body, options);
  }

  /**
   * `PUT /control/layers`: declares one annotation layer. The answer is `201` with the layer's
   * `name` and `tessera_id`; a declaration that breaks the deployment's rules is refused with `422`
   * saying why.
   */
  declareLayer(declaration: object, options: WriteOptions = {}): Promise<Answer> {
    return this.sendJson('PUT', withQuery('/control/layers', waiting(options)), declaration, options);
  }

  /**
   * `PUT /control/view_groups/{name}`: declares a view group, with no views yet. The answer is `201`
   * when the group is new and `200` when the name already has this declaration; another
   * declaration under the name is refused with `409`.
   */
  declareViewGroup(name: string, body: object, options: WriteOptions = {}): Promise<Answer> {
    return this.sendJson('PUT', withQuery(`/control/view_groups/${segment(name)}`, waiting(options)), body, options);
  }

  /**
   * `PUT /control/attributes`: declares one attribute column, named in the body. A batch sent after
   * the answer may carry it. `201` when new, `200` when the name already has this declaration,
   * `409` when it has another. A new column declared with `render: true` is refused with `422`: a
   * rendered column is declared at a build.
   *
   * `unique: true` says no two items hold one value. On a column that exists it is the one field
   * that may change: the answer waits while the column's index is built over the values it holds,
   * and is `409` naming up to ten values more than one item holds. `unique: false` drops the index
   * and keeps the values.
   */
  declareAttribute(body: object, options: WriteOptions = {}): Promise<Answer> {
    return this.sendJson('PUT', withQuery('/control/attributes', waiting(options)), body, options);
  }

  /**
   * `PUT /control/vocabularies/{name}`: declares a vocabulary, the value set of a category column,
   * with a closed set's values in the body. `201` when new, `200` when the name already has this
   * declaration and its values are added as a page, `409` when it has another.
   */
  declareVocabulary(name: string, body: object, options: WriteOptions = {}): Promise<Answer> {
    return this.sendJson('PUT', withQuery(`/control/vocabularies/${segment(name)}`, waiting(options)), body, options);
  }

  /**
   * `PATCH /control/vocabularies/{name}/values`: adds one page of `{key, title?}` values to a
   * vocabulary. No value is removed, and a title given for a held key replaces its title. An
   * unknown vocabulary is refused with `404`.
   */
  vocabularyValues(name: string, body: object, options: WriteOptions = {}): Promise<Answer> {
    return this.sendJson('PATCH', withQuery(`/control/vocabularies/${segment(name)}/values`, waiting(options)), body, options);
  }

  /**
   * `PUT /control/views/{name}`: declares one plain view, with no rows yet. The statuses are
   * `declareViewGroup`'s.
   */
  declareView(name: string, body: object, options: WriteOptions = {}): Promise<Answer> {
    return this.sendJson('PUT', withQuery(`/control/views/${segment(name)}`, waiting(options)), body, options);
  }

  /**
   * `PUT /control/views/{group}/{key}`: creates a view of `group` from its roster record in `body`.
   * An unknown group is refused with `404` and a key already in use with `409`. A dropped key may
   * be used again, and the new view starts empty.
   */
  createView(group: string, key: string, body: object, options: WriteOptions = {}): Promise<Answer> {
    return this.sendJson('PUT', withQuery(`/control/views/${segment(group)}/${segment(key)}`, waiting(options)), body, options);
  }

  /**
   * `PUT /control/layers/{name}/artifacts`: publishes artifacts into one level of a layer, with a
   * first page of each one's members, and answers with each artifact's `tessera_id`. Members,
   * excluded items and a content's `generated_from` are each a {@link MemberTable}. A member
   * naming no item, or two, is left out and listed in `refused`, and the rest are published; with
   * `strict` the request is refused at the first, `404` where it names no item and `409` where it
   * names two. A `generated_from` member naming no item, or two, refuses the request, strict or
   * not. Every other refusal applies nothing.
   */
  publish(layer: string, body: PublishRequest, options: StrictOptions = {}): Promise<Answer<ArtifactsPublished>> {
    const path = withQuery(`/control/layers/${segment(layer)}/artifacts`, naming(options));
    return this.sendJson('PUT', path, body, options) as Promise<Answer<ArtifactsPublished>>;
  }

  /**
   * `PATCH /control/layers/{name}/artifacts`: adds a page of members to artifacts the layer holds,
   * or with a `rank` moves a content's generating set, and fills parts they lack. `members` and
   * `leaving` are {@link MemberTable}s, whose refused members are listed in `refused` as on
   * `publish`, or with `strict` refuse the request. A part that differs from the one held is
   * refused with `409`, and such a refusal applies nothing.
   *
   * @param body - A `Uint8Array` is sent as an Arrow IPC stream, anything else as JSON. The Arrow
   *   form has one row per artifact: `key` as `utf8`, `members` as a list of structs whose fields
   *   are the member table's columns, and optionally `view` and `access`, with `level` in the
   *   schema's metadata.
   */
  grow(layer: string, body: GrowRequest | Uint8Array, options: StrictOptions = {}): Promise<Answer<MembershipsGrown>> {
    const path = withQuery(`/control/layers/${segment(layer)}/artifacts`, naming(options));
    const sent = body instanceof Uint8Array ? this.send('PATCH', path, options, body, {'content-type': ARROW}) : this.sendJson('PATCH', path, body, options);
    return sent as Promise<Answer<MembershipsGrown>>;
  }

  /**
   * `POST /control/changes`: deletes, suppresses and unsuppresses items, each named by its
   * `match`. A deletion or suppression applies to every request from the moment it is accepted,
   * without waiting for a publication. A change naming no item, or two, is listed in `refused` and
   * the rest are applied; with `strict` the request is refused at the first, `404` where it names
   * no item and `409` where it names two. Every item is checked before any is applied, so any
   * other refusal applies nothing.
   */
  changes(items: ChangeItem[], options: StrictOptions = {}): Promise<Answer<ChangesApplied>> {
    return this.sendJson('POST', withQuery('/control/changes', naming(options)), items, options) as Promise<Answer<ChangesApplied>>;
  }

  /**
   * `DELETE /control/layers/{name}`: drops a layer that `declareLayer` declared. The name stays
   * taken, so a later declaration under it is refused.
   */
  dropLayer(name: string, options: WriteOptions = {}): Promise<Answer> {
    return this.send('DELETE', withQuery(`/control/layers/${segment(name)}`, waiting(options)), options);
  }

  /**
   * `DELETE /control/views/{group}/{key}`: drops a view that `createView` made, and frees its key.
   * It deletes the items it leaves with a row in no view and reports how many in the answer's
   * `deleted`.
   */
  dropView(group: string, key: string, options: WriteOptions = {}): Promise<Answer> {
    const path = withQuery(`/control/views/${segment(group)}/${segment(key)}`, waiting(options));
    return this.send('DELETE', path, options);
  }

  /**
   * `POST /control/compact`: asks for a compaction, which removes deleted rows, and is answered
   * `202` before it runs. A request made while a compaction runs is dropped; the `compaction`
   * object of `status()` shows what happened.
   */
  compact(options: CallOptions = {}): Promise<Answer> {
    return this.send('POST', '/control/compact', options, '');
  }

  /**
   * `POST /control/flush`: brings the next publication forward and answers `202`. The answer's
   * `publication` names the first publication that includes every write sent before the flush;
   * with `wait`, the answer is held until it is published.
   */
  flush(options: WriteOptions = {}): Promise<Answer> {
    return this.send('POST', withQuery('/control/flush', waiting(options)), options, '');
  }
}
