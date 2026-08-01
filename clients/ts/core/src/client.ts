import {decodeViewport} from './decode.js';
import type {ItemDetail, Meta, Session, ViewportRequest, ViewportResponse} from './types.js';

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
};

/**
 * The four viewer/session verbs, and nothing else.
 *
 * No cache, no epoch, no session lifetime, no replica state — client-interaction §10's session
 * client layer, which is what a REST user would have written anyway. The replica store goes
 * *above* this, not inside it, so that this file stays a thing you can read in one sitting and
 * check against the contracts spec.
 */
export class TesseraClient {
  constructor(private readonly opts: TesseraClientOptions) {}

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
      identityEpoch: m.identity_epoch,
      slices: m.slices.map((s) => ({id: s.id, displayName: s.display_name})),
      quantisation: {
        xMin: m.quantisation.x_min,
        xMax: m.quantisation.x_max,
        yMin: m.quantisation.y_min,
        yMax: m.quantisation.y_max
      },
      declaredScalars: m.declared_scalars.map((s) => ({name: s.name, arrowType: s.arrow_type})),
      selection: {
        kMin: m.selection.k_min,
        kMaxMarks: m.selection.k_max_marks,
        maxK: m.selection.max_k,
        thetaTargetMarks: m.selection.theta_target_marks,
        maxUnderlayOffset: m.selection.max_underlay_offset
      }
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
    signal?: AbortSignal
  ): Promise<ViewportResponse> {
    const body: Record<string, unknown> = {slice: req.slice, zoom: req.zoom, bbox: req.bbox};
    if (req.k !== undefined) body.k = req.k;
    if (req.underlayOffset) body.underlay_offset = req.underlayOffset;

    const response = await fetch(`${this.opts.viewerUrl}/v1/viewport`, {
      method: 'POST',
      headers: {authorization: `Bearer ${token}`, 'content-type': 'application/json'},
      body: JSON.stringify(body),
      signal
    });
    if (!response.ok) await fail(response);
    const bytes = new Uint8Array(await response.arrayBuffer());
    const stage = response.headers.get('x-tessera-stage-ns');
    return {
      result: decodeViewport(bytes),
      timings: {
        serverUs: Number(response.headers.get('x-tessera-server-us') ?? 0),
        admissionUs: Number(response.headers.get('x-tessera-admission-us') ?? 0),
        stageNs: stage ? stage.split(',').map(Number) : null
      },
      pin: response.headers.get('x-tessera-pin'),
      bytes: bytes.byteLength
    };
  }

  async item(token: string, tesseraId: bigint): Promise<ItemDetail> {
    const response = await fetch(`${this.opts.viewerUrl}/v1/items/${tesseraId.toString()}`, {
      method: 'POST',
      headers: {authorization: `Bearer ${token}`, 'content-type': 'application/json'},
      body: '{}'
    });
    if (!response.ok) await fail(response);
    const body = (await response.json()) as {scalars: unknown[]; external_id?: string};
    return {scalars: body.scalars, externalId: body.external_id ?? null};
  }
}

/** `GET /v1/meta`'s snake_case wire shape, mapped to {@link Meta} above. */
type RawMeta = {
  api_version: number;
  identity_epoch: number;
  slices: {id: string; display_name: string}[];
  quantisation: {x_min: number; x_max: number; y_min: number; y_max: number};
  declared_scalars: {name: string; arrow_type: string}[];
  selection: {
    k_min: number;
    k_max_marks: number;
    max_k: number;
    theta_target_marks: number;
    max_underlay_offset: number;
  };
};
