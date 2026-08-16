import {readFileSync} from 'node:fs';
import {join} from 'node:path';
import {afterEach, describe, expect, it, vi} from 'vitest';
import {TesseraClient, TesseraError} from '../src/client.js';
import {decodeViewport} from '../src/decode.js';
import type {ViewportResult} from '../src/types.js';

/**
 * What the artifact channel puts on the wire, and what it makes of what comes back.
 *
 * The transport is stubbed rather than live: every assertion here is about the *shape* of the
 * request the client composes and the mapping of the reply, which is precisely the part a live
 * test cannot pin — a server that ignored `layers` would pass the live test on a bundle with one
 * layer.
 */

const empty: ViewportResult = {
  tiles: [],
  ids: new BigUint64Array(),
  codes: new BigUint64Array(),
  positions: new Float64Array(),
  world: new Float32Array(),
  scalars: {},
  subCells: null,
  artifacts: []
};

/** Capture every request the client makes, and answer each with the body given. */
function stubFetch(answer: (url: string) => Response) {
  const seen: {url: string; body: Record<string, unknown>}[] = [];
  vi.stubGlobal('fetch', async (url: string, init?: RequestInit) => {
    seen.push({url, body: init?.body ? JSON.parse(String(init.body)) : {}});
    return answer(url);
  });
  return seen;
}

const client = () =>
  new TesseraClient({
    viewerUrl: 'http://viewer',
    sessionUrl: 'http://session',
    decoder: {decode: async () => empty, close: () => {}}
  });

afterEach(() => vi.unstubAllGlobals());

describe('the viewport request', () => {
  it('sends an empty layer selection but omits an absent one', async () => {
    const seen = stubFetch(() => new Response(new ArrayBuffer(0), {status: 200}));
    const c = client();

    await c.viewport('tok', {slice: 's0', zoom: 4, layers: []});
    await c.viewport('tok', {slice: 's0', zoom: 4});

    // The distinction the server acts on: `[]` costs nothing, absent answers for every layer this
    // principal reaches. A client meaning the first and sending neither pays for the others.
    expect(seen[0]!.body.layers).toEqual([]);
    expect('layers' in seen[1]!.body).toBe(false);
  });

  it('carries the layer selection and the artifact budget under their wire names', async () => {
    const seen = stubFetch(() => new Response(new ArrayBuffer(0), {status: 200}));

    await client().viewport('tok', {
      slice: 's0',
      zoom: 4,
      layers: ['clusters/hdbscan-2026-08'],
      artifactBudget: 500
    });

    expect(seen[0]!.body).toMatchObject({
      layers: ['clusters/hdbscan-2026-08'],
      artifact_budget: 500
    });
  });
});

describe('/v1/meta', () => {
  it('maps the layers this principal reaches, and reads an absent list as none', async () => {
    const base = {
      api_version: 1,
      idset: 7,
      slices: [{id: 's0', display_name: 'S0'}],
      quantisation: {x_min: 0, x_max: 65536, y_min: 0, y_max: 65536},
      declared_scalars: [],
      selection: {
        k_min: 1,
        k_max_marks: 5000,
        max_k: 5000,
        theta_target_marks: 16,
        max_underlay_offset: 3
      }
    };
    const withLayer = {
      ...base,
      layers: [
        {
          name: 'clusters/hdbscan-2026-08',
          title: 'HDBSCAN clusters',
          slices: ['s0'],
          membership: 'enumerated',
          hierarchy: {kind: 'flat', prune_children: false},
          levels: [{level: 0, title: 'clusters', zoom: null}],
          derived_content: [],
          supplied_content: [],
          depends_on: [],
          version: 3
        }
      ]
    };

    let body = withLayer;
    stubFetch(() => new Response(JSON.stringify(body), {status: 200}));
    const c = client();

    const reached = await c.meta('tok');
    expect(reached.layers).toEqual([
      {
        name: 'clusters/hdbscan-2026-08',
        title: 'HDBSCAN clusters',
        slices: ['s0'],
        membership: 'enumerated',
        hierarchy: {kind: 'flat', pruneChildren: false},
        levels: [{level: 0, title: 'clusters', zoom: null}],
        derivedContent: [],
        suppliedContent: [],
        depsOn: [],
        version: 3
      }
    ]);

    body = base as typeof withLayer;
    // No layers reached and no layers registered are one answer, and neither is a failure.
    expect((await c.meta('tok')).layers).toEqual([]);
  });
});

describe('the artifacts frame, decoded from a captured response', () => {
  const fixture = (name: string) =>
    new Uint8Array(readFileSync(join(import.meta.dirname, 'fixtures', name)));

  it('carries one row per served artifact, and no points beside them', () => {
    const result = decodeViewport(fixture('viewport-artifacts.bin'));
    expect(result.artifacts.length).toBeGreaterThan(0);
    // Captured at `k = 0` — the annotation channel's own request shape. A body with an artifacts
    // frame and no points frame at all is the case a decoder is most likely to get wrong.
    expect(result.ids.length).toBe(0);

    for (const artifact of result.artifacts) {
      expect(artifact.layer.length).toBeGreaterThan(0);
      // u64 on the wire and kept as one: a `tessera_id` does not survive a double.
      expect(artifact.tesseraId).toBeTypeOf('bigint');
      expect(artifact.maskedCount).toBeTypeOf('bigint');
      // Served at all means it cleared its layer's criterion, so nothing here is a zero-count row
      // this principal cannot see any of.
      expect(artifact.maskedCount).toBeGreaterThan(0n);
    }

    // Identity is what the drill-down addresses, so a repeated one would make two clusters one.
    const ids = new Set(result.artifacts.map((a) => a.tesseraId));
    expect(ids.size).toBe(result.artifacts.length);
  });

  it('reads a response with no artifacts frame as no artifacts, not as a failure', () => {
    expect(decodeViewport(fixture('viewport-plain.bin')).artifacts).toEqual([]);
  });
});

describe('the drill-down', () => {
  it('requires the slice on the wire, and widens the count the frame delivers as u64', async () => {
    const seen = stubFetch(
      () =>
        new Response(JSON.stringify({layer: 'clusters/x', stable_key: 'c-0001', masked_count: 143}), {
          status: 200
        })
    );

    const detail = await client().artifact('tok', 42n, {slice: 's0'});

    expect(seen[0]!.url).toBe('http://viewer/v1/artifacts/42');
    expect(seen[0]!.body).toEqual({slice: 's0'});
    expect(detail).toEqual({layer: 'clusters/x', stableKey: 'c-0001', maskedCount: 143n});
  });

  it('reads an absent stable key as none rather than as a missing field', async () => {
    stubFetch(() => new Response(JSON.stringify({layer: 'clusters/x', masked_count: 2}), {status: 200}));
    expect((await client().artifact('tok', 9n, {slice: 's0'})).stableKey).toBeNull();
  });

  it('surfaces the one refusal as a typed error, with nothing else to read from it', async () => {
    stubFetch(
      () =>
        new Response(JSON.stringify({error: 'unknown', detail: 'unknown artifact'}), {status: 404})
    );

    // Every withheld case arrives here identically — an identifier naming nothing, one naming a
    // point, one gated, one suppressed, one below its layer's criterion. A caller that branched on
    // the detail string would be inventing a distinction the server refuses to make.
    await expect(client().artifact('tok', 1n, {slice: 's0'})).rejects.toThrow(TesseraError);
  });
});
