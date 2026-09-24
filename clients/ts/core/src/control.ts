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
 * retried. A request no server answered is returned with status {@link UNANSWERED}.
 *
 * This file imports nothing, so the operator scripts can load it under Node's type stripping.
 */

const ARROW = 'application/vnd.apache.arrow.stream';
const JSON_TYPE = 'application/json';

/** The longest a `429` makes one call wait between attempts, in seconds. */
export const MAX_BACKOFF = 30;

/** How many times one call sends its request before a `429` is returned as the answer. */
export const MAX_ATTEMPTS = 600;

/** The status of an answer to a request that reached no server. It is not an HTTP status. */
export const UNANSWERED = 0;

/** One request's answer: the status, the body decoded where it was a JSON object, and the text. */
export type Answer = {
  status: number;
  ok: boolean;
  body: Record<string, unknown>;
  detail: string;
  attempts: number;
  ms: number;
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

/** Where a row page goes: the batch id to send it under, and the view it is a page of. */
export type RowOptions = {batch?: string; view?: string};

/**
 * An external id as a JSON route carries it: base64 of the bytes the id column holds.
 *
 * A string is its UTF-8, a bigint its eight little-endian bytes (two's complement when negative),
 * and bytes are taken as they stand. The build reads an id column to the same bytes, so one row
 * has one address at both.
 */
export function addressed(id: string | bigint | Uint8Array): string {
  let bytes: Uint8Array;
  if (typeof id === 'string') {
    bytes = new TextEncoder().encode(id);
  } else if (typeof id === 'bigint') {
    bytes = new Uint8Array(8);
    new DataView(bytes.buffer).setBigUint64(0, BigInt.asUintN(64, id), true);
  } else {
    bytes = id;
  }
  let binary = '';
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

const waiting = (wait: boolean | undefined) => (wait ? '?wait=visible' : '');

function decoded(text: string): Record<string, unknown> {
  try {
    const value: unknown = JSON.parse(text);
    if (value !== null && typeof value === 'object' && !Array.isArray(value)) return value as Record<string, unknown>;
    return {value};
  } catch {
    return {};
  }
}

/** How long a `429` asks the caller to wait, in seconds: the header, else the body's figure, else 1. */
function retryAfter(response: Response, body: Record<string, unknown>): number {
  const named = response.headers.get('retry-after') ?? body.retry_after_s;
  const seconds = Number(named ?? 1);
  return Number.isFinite(seconds) ? Math.max(0, Math.min(seconds, MAX_BACKOFF)) : 1;
}

export class Control {
  private readonly base: string;
  private readonly credential: string;

  constructor(options: ControlOptions) {
    this.base = options.controlUrl.replace(/\/+$/, '');
    this.credential = options.operatorCredential;
  }

  private async send(method: string, path: string, body?: string | Uint8Array, headers: Record<string, string> = {}): Promise<Answer> {
    const url = this.base + path;
    const init: RequestInit = {method, headers: {authorization: `Bearer ${this.credential}`, ...headers}};
    if (body !== undefined) init.body = body as BodyInit;
    const started = Date.now();
    let attempts = 0;
    for (;;) {
      attempts += 1;
      let response: Response;
      let text: string;
      try {
        response = await fetch(url, init);
        text = await response.text();
      } catch (error) {
        return {status: UNANSWERED, ok: false, body: {}, detail: `${url} did not answer: ${String(error)}`, attempts, ms: Date.now() - started};
      }
      const answered = decoded(text);
      if (response.status === 429 && attempts < MAX_ATTEMPTS) {
        const seconds = retryAfter(response, answered);
        await new Promise((resolve) => setTimeout(resolve, seconds * 1000));
        continue;
      }
      const ok = response.status >= 200 && response.status < 300;
      return {status: response.status, ok, body: answered, detail: text, attempts, ms: Date.now() - started};
    }
  }

  private sendJson(method: string, path: string, body: unknown): Promise<Answer> {
    return this.send(method, path, JSON.stringify(body), {'content-type': JSON_TYPE});
  }

  private async rows(path: string, body: Uint8Array, options: RowOptions): Promise<RowAnswer> {
    const batch = options.batch ?? freshBatch();
    const headers: Record<string, string> = {'content-type': ARROW, 'x-tessera-batch-id': batch};
    if (options.view !== undefined) headers['x-tessera-view'] = options.view;
    return {...(await this.send('POST', path, body, headers)), batch};
  }

  /** `GET /control/status`. */
  status(): Promise<Answer> {
    return this.send('GET', '/control/status');
  }

  /** The `limits` block of `/control/status`, or `{}` where the status request was refused. */
  async limits(): Promise<Record<string, unknown>> {
    const limits = (await this.status()).body.limits;
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
  declareLayer(declaration: object): Promise<Answer> {
    return this.sendJson('PUT', '/control/layers', declaration);
  }

  /** `PUT /control/view_groups/{name}`: the group, with an empty roster. */
  declareViewGroup(name: string, body: object): Promise<Answer> {
    return this.sendJson('PUT', `/control/view_groups/${segment(name)}`, body);
  }

  /** `PUT /control/attributes`: one column, named in the body. */
  declareAttribute(body: object): Promise<Answer> {
    return this.sendJson('PUT', '/control/attributes', body);
  }

  /** `PUT /control/vocabularies/{name}`: the value set, with a closed set's values on it. */
  declareVocabulary(name: string, body: object): Promise<Answer> {
    return this.sendJson('PUT', `/control/vocabularies/${segment(name)}`, body);
  }

  /** `PATCH /control/vocabularies/{name}/values`: one page of `{key, title?}` values. */
  vocabularyValues(name: string, body: object): Promise<Answer> {
    return this.sendJson('PATCH', `/control/vocabularies/${segment(name)}/values`, body);
  }

  /** `PUT /control/views/{name}`: one plain view. */
  declareView(name: string, body: object): Promise<Answer> {
    return this.sendJson('PUT', `/control/views/${segment(name)}`, body);
  }

  /** `PUT /control/views/{group}/{key}`: the roster record, which creates the group's view. */
  createView(group: string, key: string, body: object): Promise<Answer> {
    return this.sendJson('PUT', `/control/views/${segment(group)}/${segment(key)}`, body);
  }

  /** `PUT /control/layers/{name}/artifacts`: publish artifacts into one level of a layer. */
  publish(layer: string, body: object): Promise<Answer> {
    return this.sendJson('PUT', `/control/layers/${segment(layer)}/artifacts`, body);
  }

  /**
   * `PATCH /control/layers/{name}/artifacts`: page members into artifacts the layer holds, and fill
   * parts they lack. Bytes are sent as an Arrow IPC stream, anything else as JSON.
   */
  grow(layer: string, body: object | Uint8Array): Promise<Answer> {
    const path = `/control/layers/${segment(layer)}/artifacts`;
    if (body instanceof Uint8Array) return this.send('PATCH', path, body, {'content-type': ARROW});
    return this.sendJson('PATCH', path, body);
  }

  /** `POST /control/changes`: deletions, suppressions and the lifting of suppressions. */
  changes(items: ChangeItem[]): Promise<Answer> {
    return this.sendJson('POST', '/control/changes', items);
  }

  /**
   * `DELETE /control/layers/{name}`: the inverse of `declareLayer`. The name stays taken, so a
   * later declaration under it is refused.
   */
  dropLayer(name: string, options: {wait?: boolean} = {}): Promise<Answer> {
    return this.send('DELETE', `/control/layers/${segment(name)}${waiting(options.wait)}`);
  }

  /**
   * `DELETE /control/views/{group}/{key}`: the inverse of `createView`. It deletes no entity unless
   * `deleteDangling` is set, which deletes those holding a row in no other view and reports how
   * many in `deleted`.
   */
  dropView(group: string, key: string, options: {deleteDangling?: boolean; wait?: boolean} = {}): Promise<Answer> {
    const query = new URLSearchParams();
    if (options.deleteDangling) query.set('delete_dangling', 'true');
    if (options.wait) query.set('wait', 'visible');
    const suffix = query.toString();
    return this.send('DELETE', `/control/views/${segment(group)}/${segment(key)}${suffix ? `?${suffix}` : ''}`);
  }

  /** `POST /control/compact`: ask for a fold, which removes deleted rows. Answered before it runs. */
  compact(): Promise<Answer> {
    return this.send('POST', '/control/compact', '');
  }

  /**
   * `POST /control/flush`: arm a publication cycle. With `wait` the answer is held until the cycle
   * that covers every request sent before it is published, and `visible` says whether it was.
   */
  flush(options: {wait?: boolean} = {}): Promise<Answer> {
    return this.send('POST', `/control/flush${waiting(options.wait)}`, '');
  }
}
