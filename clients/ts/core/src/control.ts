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
 * One call's answer. Every status, a refusal included, is returned here; none is thrown.
 *
 * @category Control plane
 */
export type Answer = {
  /** The HTTP status, or {@link UNANSWERED} where no server answered. */
  status: number;
  /** Whether `status` is in the 2xx range. */
  ok: boolean;
  /**
   * The body parsed as JSON: the object itself, any other JSON value wrapped as `{value}`, or `{}`
   * where the body is not JSON. A refusal carries `error` and `detail`.
   */
  body: Record<string, unknown>;
  /** The body as text. Where no server answered, a sentence naming the URL and the error. */
  text: string;
  /** How many times the request was sent, counting each retry after a `429`. */
  attempts: number;
  /** Seconds from the first attempt to the answer, including every wait. */
  seconds: number;
};

/**
 * The answer of `ingest`, with the batch id the request carried.
 *
 * @category Control plane
 */
export type RowAnswer = Answer & {
  /** The batch id every attempt carried. Pass it as `batch` to send the same body again. */
  batch: string;
};

/**
 * One item of a `POST /control/changes` request, in the route's own field names. The item is
 * addressed by `external_id`, base64 of the id's bytes as {@link addressed} makes it, or by
 * `tessera_id`, a decimal string.
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
} & (
  | {external_id: string}
  | {tessera_id: string}
);

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
 * What `ingest` takes.
 *
 * @category Control plane
 */
export type RowOptions = WriteOptions & {
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

const INTEGER_MIN = -(2n ** 63n);
const INTEGER_END = 2n ** 64n;

/**
 * An external id as a JSON control route carries it, such as {@link ChangeItem}'s `external_id`:
 * base64 of the bytes the id column holds. A string is taken as its UTF-8 bytes, an integer as
 * eight little-endian bytes (two's complement when negative), and a `Uint8Array` as it stands. A
 * build reads an id column to the same bytes, so a row has one address whichever path stored it.
 *
 * @param id - A string, a `Uint8Array`, a `bigint` in [-2^63, 2^64), or a `number` that is a safe
 *   integer.
 * @throws `RangeError` for a `number` that is not a safe integer, or a `bigint` outside
 *   [-2^63, 2^64).
 * @throws `TypeError` for a value of any other type.
 *
 * @category Control plane
 */
export function addressed(id: string | bigint | number | Uint8Array): string {
  if (typeof id === 'string') return base64(new TextEncoder().encode(id));
  if (id instanceof Uint8Array) return base64(id);
  if (typeof id === 'number') {
    if (!Number.isSafeInteger(id)) throw new RangeError(`${id} is not a safe integer; pass an integer id as a bigint`);
    id = BigInt(id);
  }
  if (typeof id !== 'bigint') throw new TypeError(`an external id is a string, a bigint, a number or a Uint8Array, not ${typeof id}`);
  if (id < INTEGER_MIN || id >= INTEGER_END) throw new RangeError(`${id} does not fit in eight bytes; an integer id is in [-2^63, 2^64)`);
  const bytes = new Uint8Array(8);
  new DataView(bytes.buffer).setBigUint64(0, BigInt.asUintN(64, id), true);
  return base64(bytes);
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
 * resolved twice: a row naming its item by `tessera_id`, `external_id` or a unique value names in
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
    return {...(await this.send('POST', withQuery(path, waiting(options)), options, body, headers)), batch};
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
   * its `tessera_id`, its `external_id` or a unique column's value; a row naming none creates an
   * item at its position, one naming an item it matches changes nothing, one naming an item with
   * no row in the view adds it there, and any other edits the item, which keeps its `tessera_id`.
   * A row without coordinates changes only what it carries. Any column may be left out, which
   * keeps what the item stores; a null clears it. The answer's body counts the rows `created`,
   * `edited`, `added`, `unchanged`, `clipped` and `clamped`, counts in `joined` the annotation
   * memberships the rows added, lists each row's `tessera_id` in request order as a decimal
   * string, and names the `publication` the rows become visible in. A row that only places its
   * item in an annotation changes the annotation, not the item, and is counted `unchanged`. A row
   * whose values name two items is refused with `409`, and nothing in the page is stored.
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
   * first page of each one's members. The request is applied whole or not at all, and the answer
   * gives each artifact a `tessera_id`.
   */
  publish(layer: string, body: object, options: WriteOptions = {}): Promise<Answer> {
    return this.sendJson('PUT', withQuery(`/control/layers/${segment(layer)}/artifacts`, waiting(options)), body, options);
  }

  /**
   * `PATCH /control/layers/{name}/artifacts`: adds a page of members to artifacts the layer holds,
   * and fills parts they lack. A part that differs from the one held is refused with `409`. The
   * request is applied whole or not at all.
   *
   * @param body - A `Uint8Array` is sent as an Arrow IPC stream, anything else as JSON.
   */
  grow(layer: string, body: object | Uint8Array, options: WriteOptions = {}): Promise<Answer> {
    const path = withQuery(`/control/layers/${segment(layer)}/artifacts`, waiting(options));
    if (body instanceof Uint8Array) return this.send('PATCH', path, options, body, {'content-type': ARROW});
    return this.sendJson('PATCH', path, body, options);
  }

  /**
   * `POST /control/changes`: deletes, suppresses and unsuppresses items. A deletion or suppression
   * applies to every request from the moment it is accepted, without waiting for a publication.
   * Every item is checked before any is applied, so a refused request applies nothing.
   */
  changes(items: ChangeItem[], options: WriteOptions = {}): Promise<Answer> {
    return this.sendJson('POST', withQuery('/control/changes', waiting(options)), items, options);
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
   * It deletes no item unless `deleteDangling` is set, which deletes the view's items that have a
   * row in no other view and reports how many in the answer's `deleted`.
   */
  dropView(group: string, key: string, options: WriteOptions & {deleteDangling?: boolean} = {}): Promise<Answer> {
    const path = withQuery(`/control/views/${segment(group)}/${segment(key)}`, {
      delete_dangling: options.deleteDangling ? 'true' : undefined,
      ...waiting(options)
    });
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
