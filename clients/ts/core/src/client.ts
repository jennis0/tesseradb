import {createDecoder, type Decoder} from './decoder.js';
import type {
  ArrowType,
  CategoryValue,
  FilterOperandSet,
  ItemDetail,
  Meta,
  Session,
  ViewportRequest,
  ViewportResponse
} from './types.js';

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
      slices: m.slices.map((s) => ({id: s.id, displayName: s.display_name})),
      quantisation: {
        xMin: m.quantisation.x_min,
        xMax: m.quantisation.x_max,
        yMin: m.quantisation.y_min,
        yMax: m.quantisation.y_max
      },
      declaredScalars: m.declared_scalars.map((s) => ({
        name: s.name,
        arrowType: s.arrow_type,
        // Null for a plain column, and the absence is the whole signal: without it a `u16`
        // category is indistinguishable from a `u16` integer, since the hot path ships the code.
        category: s.category
          ? {vocabulary: s.category.vocabulary, kind: s.category.kind, listing: s.category.listing}
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
        maxCategoryValues: m.selection.max_category_values ?? 1_000
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
        operands: f.operands
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
    background = false
  ): Promise<ViewportResponse> {
    const body: Record<string, unknown> = {slice: req.slice, zoom: req.zoom};
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
    const bytes = new Uint8Array(await response.arrayBuffer());
    // Read BEFORE decode: the worker path transfers the buffer zero-copy, which detaches it —
    // `byteLength` afterwards is 0, and every byte ledger downstream (the anticipation budget,
    // the traces, the ring-spend measurement) silently read that zero.
    const size = bytes.byteLength;
    const stage = response.headers.get('x-tessera-stage-ns');
    this.decoder ??= this.opts.decoder ?? createDecoder();
    return {
      result: await this.decoder.decode(bytes, background),
      timings: {
        serverUs: Number(response.headers.get('x-tessera-server-us') ?? 0),
        admissionUs: Number(response.headers.get('x-tessera-admission-us') ?? 0),
        stageNs: stage ? stage.split(',').map(Number) : null
      },
      identityKey: response.headers.get('x-tessera-identity-key') ?? '',
      // Unquoted here: the quotes are HTTP's entity-tag syntax, not part of the value, and every
      // comparison this client makes is against another value it took from this same header.
      contentKey: (response.headers.get('etag') ?? '').replace(/^"|"$/g, ''),
      pin: response.headers.get('x-tessera-pin'),
      stale: response.headers.get('x-tessera-stale') === '1',
      bytes: size
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
   * Throws {@link TesseraError} for a real refusal — notably `500 fail-closed` on a `per_viewer`
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
      values: body.values.map((v) => ({code: v.code, key: v.key, label: v.label ?? null})),
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
    };
    return {fields: body.fields ?? {}, externalId: body.external_id ?? null};
  }
}

/** `GET /v1/meta`'s snake_case wire shape, mapped to {@link Meta} above. */
type RawMeta = {
  api_version: number;
  idset: number;
  slices: {id: string; display_name: string}[];
  quantisation: {x_min: number; x_max: number; y_min: number; y_max: number};
  declared_scalars: {
    name: string;
    arrow_type: ArrowType;
    category: {vocabulary: string; kind: 'declared' | 'discovered'; listing: 'per_viewer' | 'public'} | null;
    render: boolean;
    index: boolean;
  }[];
  filter_operands?: {column: string; family: FilterOperandSet['family']; operands: string[]}[];
  selection: {
    k_min: number;
    k_max_marks: number;
    max_k: number;
    theta_target_marks: number;
    max_underlay_offset: number;
    max_tiles_per_request?: number;
    max_category_values?: number;
  };
};

/** `GET /v1/categories/{column}`'s wire shape. */
type RawCategories = {
  column: string;
  values: {code: number; key: string; label?: string | null}[];
  next: string | null;
};
