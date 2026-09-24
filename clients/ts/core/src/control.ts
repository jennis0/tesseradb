/**
 * The control plane of one served database, as an operator calls it with the operator credential.
 *
 * `Control` is the routes and nothing else. It keeps no record of what it sent, so what a database
 * holds is asked of the database. A body is sent as the caller gave it: Arrow IPC stream bytes on
 * the two row routes, JSON on declarations, publications and changes, and on a growth whichever the
 * caller passed. A JSON body is serialised once per call, so every attempt of one call sends the
 * same bytes.
 *
 * A batch id names one request on the row routes. It is the caller's, or a fresh random id made
 * once per call, and it is never derived from the body: the same rows sent in two calls land twice.
 * The server holds a batch id against the SHA-256 of its body, so an attempt after a `429` resends
 * the same id and the same bytes, and the answer names the id so the caller can resend it too.
 *
 * A `429` is backpressure: the call waits the `Retry-After` it carries and resends, at most
 * {@link MAX_ATTEMPTS} times. Every other status is returned as an {@link Answer}, not thrown and not
 * retried. A request no server answered is returned with status {@link UNANSWERED}. A call whose
 * `signal` aborts rejects with the signal's reason, during a request or a wait.
 *
 * `wait: true` on a write asks the server to hold its answer until the write is visible, and the
 * answer's `visible` says whether it was. It is sent only where the caller sets it.
 *
 * This file imports nothing, so the operator scripts can load it under Node's type stripping.
 */

const ARROW = 'application/vnd.apache.arrow.stream';
const JSON_TYPE = 'application/json';

/** The shortest a `429` makes one call wait between attempts, in seconds: the server's own floor. */
export const MIN_BACKOFF = 1;

/** The longest a `429` makes one call wait between attempts, in seconds. */
export const MAX_BACKOFF = 30;

/** How many times one call sends its request before a `429` is returned as the answer. */
export const MAX_ATTEMPTS = 600;

/** The status of an answer to a request that reached no server. It is not an HTTP status. */
export const UNANSWERED = 0;

/**
 * One request's answer: the status, the body decoded where it was a JSON object, and the whole
 * response text. `JSON.parse` rounds an integer past 2^53 in `body`; `text` holds it as it arrived.
 */
export type Answer = {
  status: number;
  ok: boolean;
  body: Record<string, unknown>;
  text: string;
  attempts: number;
  seconds: number;
};

/** A row route's answer, with the batch id every attempt carried. */
export type RowAnswer = Answer & {batch: string};

/** One `POST /control/changes` item, in the wire's own names. */
export type ChangeItem = {op: 'delete' | 'suppress' | 'unsuppress'} & (
  | {external_id: string}
  | {tessera_id: string; idset: number}
);

export type ControlOptions = {
  controlUrl: string;
  operatorCredential: string;
};

/** What every call takes: a signal that ends it. */
export type CallOptions = {signal?: AbortSignal};

/** What a write takes: `wait` holds the answer until the write is visible. */
export type WriteOptions = CallOptions & {wait?: boolean};

/** Where a row page goes: the batch id to send it under, and the view it is a page of. */
export type RowOptions = WriteOptions & {batch?: string; view?: string};

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
 * An external id as a JSON route carries it: base64 of the bytes the id column holds.
 *
 * A string is its UTF-8, an integer its eight little-endian bytes (two's complement when
 * negative), and bytes are taken as they stand. The build reads an id column to the same bytes, so
 * one row has one address at both. An integer is a bigint in [-2^63, 2^64), or a number that is a
 * safe integer; anything else is refused, since it has no eight bytes of its own.
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

export class Control {
  private readonly base: string;
  private readonly credential: string;

  constructor(options: ControlOptions) {
    this.base = options.controlUrl.replace(/\/+$/, '');
    this.credential = options.operatorCredential;
  }

  private async send(
    method: string,
    path: string,
    options: CallOptions,
    body?: string | Uint8Array,
    headers: Record<string, string> = {}
  ): Promise<Answer> {
    const url = this.base + path;
    const init: RequestInit = {method, headers: {authorization: `Bearer ${this.credential}`, ...headers}};
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
        response = await fetch(url, init);
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

  /** `GET /control/status`. */
  status(options: CallOptions = {}): Promise<Answer> {
    return this.send('GET', '/control/status', options);
  }

  /** The `limits` block of `/control/status`, or `{}` where the status request was refused. */
  async limits(options: CallOptions = {}): Promise<Record<string, unknown>> {
    const limits = (await this.status(options)).body.limits;
    return limits !== null && typeof limits === 'object' ? (limits as Record<string, unknown>) : {};
  }

  /** `POST /control/ingest`: one page of points, as an Arrow IPC stream. */
  ingest(body: Uint8Array, options: RowOptions = {}): Promise<RowAnswer> {
    return this.rows('/control/ingest', body, options);
  }

  /** `POST /control/values`: one page of cells on rows the database holds, as an Arrow IPC stream. */
  values(body: Uint8Array, options: RowOptions = {}): Promise<RowAnswer> {
    return this.rows('/control/values', body, options);
  }

  /** `PUT /control/layers`: one layer declaration. */
  declareLayer(declaration: object, options: WriteOptions = {}): Promise<Answer> {
    return this.sendJson('PUT', withQuery('/control/layers', waiting(options)), declaration, options);
  }

  /** `PUT /control/view_groups/{name}`: the group, with an empty roster. */
  declareViewGroup(name: string, body: object, options: WriteOptions = {}): Promise<Answer> {
    return this.sendJson('PUT', withQuery(`/control/view_groups/${segment(name)}`, waiting(options)), body, options);
  }

  /** `PUT /control/attributes`: one column, named in the body. */
  declareAttribute(body: object, options: WriteOptions = {}): Promise<Answer> {
    return this.sendJson('PUT', withQuery('/control/attributes', waiting(options)), body, options);
  }

  /** `PUT /control/vocabularies/{name}`: the value set, with a closed set's values on it. */
  declareVocabulary(name: string, body: object, options: WriteOptions = {}): Promise<Answer> {
    return this.sendJson('PUT', withQuery(`/control/vocabularies/${segment(name)}`, waiting(options)), body, options);
  }

  /** `PATCH /control/vocabularies/{name}/values`: one page of `{key, title?}` values. */
  vocabularyValues(name: string, body: object, options: WriteOptions = {}): Promise<Answer> {
    return this.sendJson('PATCH', withQuery(`/control/vocabularies/${segment(name)}/values`, waiting(options)), body, options);
  }

  /** `PUT /control/views/{name}`: one plain view. */
  declareView(name: string, body: object, options: WriteOptions = {}): Promise<Answer> {
    return this.sendJson('PUT', withQuery(`/control/views/${segment(name)}`, waiting(options)), body, options);
  }

  /** `PUT /control/views/{group}/{key}`: the roster record, which creates the group's view. */
  createView(group: string, key: string, body: object, options: WriteOptions = {}): Promise<Answer> {
    return this.sendJson('PUT', withQuery(`/control/views/${segment(group)}/${segment(key)}`, waiting(options)), body, options);
  }

  /** `PUT /control/layers/{name}/artifacts`: publish artifacts into one level of a layer. */
  publish(layer: string, body: object, options: WriteOptions = {}): Promise<Answer> {
    return this.sendJson('PUT', withQuery(`/control/layers/${segment(layer)}/artifacts`, waiting(options)), body, options);
  }

  /**
   * `PATCH /control/layers/{name}/artifacts`: page members into artifacts the layer holds, and fill
   * parts they lack. Bytes are sent as an Arrow IPC stream, anything else as JSON.
   */
  grow(layer: string, body: object | Uint8Array, options: WriteOptions = {}): Promise<Answer> {
    const path = withQuery(`/control/layers/${segment(layer)}/artifacts`, waiting(options));
    if (body instanceof Uint8Array) return this.send('PATCH', path, options, body, {'content-type': ARROW});
    return this.sendJson('PATCH', path, body, options);
  }

  /** `POST /control/changes`: deletions, suppressions and the lifting of suppressions. */
  changes(items: ChangeItem[], options: WriteOptions = {}): Promise<Answer> {
    return this.sendJson('POST', withQuery('/control/changes', waiting(options)), items, options);
  }

  /**
   * `DELETE /control/layers/{name}`: the inverse of `declareLayer`. The name stays taken, so a
   * later declaration under it is refused.
   */
  dropLayer(name: string, options: WriteOptions = {}): Promise<Answer> {
    return this.send('DELETE', withQuery(`/control/layers/${segment(name)}`, waiting(options)), options);
  }

  /**
   * `DELETE /control/views/{group}/{key}`: the inverse of `createView`. It deletes no entity unless
   * `deleteDangling` is set, which deletes those holding a row in no other view and reports how
   * many in `deleted`.
   */
  dropView(group: string, key: string, options: WriteOptions & {deleteDangling?: boolean} = {}): Promise<Answer> {
    const path = withQuery(`/control/views/${segment(group)}/${segment(key)}`, {
      delete_dangling: options.deleteDangling ? 'true' : undefined,
      ...waiting(options)
    });
    return this.send('DELETE', path, options);
  }

  /** `POST /control/compact`: ask for a fold, which removes deleted rows. Answered before it runs. */
  compact(options: CallOptions = {}): Promise<Answer> {
    return this.send('POST', '/control/compact', options, '');
  }

  /**
   * `POST /control/flush`: arm a publication cycle. With `wait` the answer is held until the cycle
   * that covers every request sent before it is published.
   */
  flush(options: WriteOptions = {}): Promise<Answer> {
    return this.send('POST', withQuery('/control/flush', waiting(options)), options, '');
  }
}
