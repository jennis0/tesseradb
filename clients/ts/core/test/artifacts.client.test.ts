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
  membership: {},
  artifacts: [],
  artifactsIdentity: null
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

    await c.viewport('tok', {view: 's0', zoom: 4, layers: []});
    await c.viewport('tok', {view: 's0', zoom: 4});

    // The wire is `string[] | 'all'`: `[]` and absent both mean *none* now, so a point-fetching
    // client sends `[]` (or omits) and pays nothing for artifacts.
    expect(seen[0]!.body.layers).toEqual([]);
    expect('layers' in seen[1]!.body).toBe(false);
  });

  it("sends the string 'all' verbatim, and never substitutes it for an array", async () => {
    const seen = stubFetch(() => new Response(new ArrayBuffer(0), {status: 200}));
    const c = client();
    await c.viewport('tok', {view: 's0', zoom: 4, layers: 'all'});
    await c.viewport('tok', {view: 's0', zoom: 4, layers: ['clusters/x']});
    // `'all'` is every reachable layer; an array is those ∩ reachable. Each reaches the wire as
    // exactly what the caller gave — the store must name the on layers, never rely on a default.
    expect(seen[0]!.body.layers).toBe('all');
    expect(seen[1]!.body.layers).toEqual(['clusters/x']);
  });

  it('carries the layer selection and the artifact budget under their wire names', async () => {
    const seen = stubFetch(() => new Response(new ArrayBuffer(0), {status: 200}));

    await client().viewport('tok', {
      view: 's0',
      zoom: 4,
      layers: ['clusters/hdbscan-2026-08'],
      artifactBudget: 500
    });

    expect(seen[0]!.body).toMatchObject({
      layers: ['clusters/hdbscan-2026-08'],
      artifact_budget: 500
    });
  });

  it('carries the row projection under its wire name, and only when named', async () => {
    const seen = stubFetch(() => new Response(new ArrayBuffer(0), {status: 200}));
    const c = client();
    await c.viewport('tok', {view: 's0', zoom: 4, layers: ['clusters/x'], artifactRows: 'identity'});
    await c.viewport('tok', {view: 's0', zoom: 4, layers: ['clusters/x']});
    // `"identity"` is the same rows in four columns (contracts §3.2 r43); absent leaves the
    // server's own default, `"full"`, and the request shape a caller who never asks always sent.
    expect(seen[0]!.body.artifact_rows).toBe('identity');
    expect('artifact_rows' in seen[1]!.body).toBe(false);
  });
});

describe('/v1/meta', () => {
  it('maps the layers this principal reaches, and reads an absent list as none', async () => {
    const base = {
      api_version: 1,
      idset: 7,
      views: [{id: 's0', display_name: 'S0'}],
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
          views: ['s0'],
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
        views: ['s0'],
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

/**
 * **The two artifact goldens are r43 captures** (2026-08-28) — taken against a `tessera serve`
 * built from this tree over the notebook corpus's `clusters/hdbscan` (`./run_demo.sh --scale
 * notebook --no-viewer`, then `scripts/capture-golden.mjs --artifacts-only` as the corpus's
 * *medium* preset principal, whose terms are arXiv categories; `--terms 0` sees nothing there), so
 * `layer` is dictionary-encoded, `rung` stands where `level` did, and `hull_x`/`hull_y` trail the
 * fixed prefix as the nested list of rings contracts §3.2 item 4 specifies.
 *
 * **`viewport-artifacts-pre-r40.bin` is kept deliberately and is never recaptured.** It is a real
 * body from before a hull was a list of rings, and the refusal below is the only test that can use
 * one: read a level shallow, those bytes yield one ring of one vertex per artifact, which draws as
 * nothing, picks as nothing and errors nowhere. A hand-assembled body cannot stand in for that,
 * because the shape of the mistake is the point.
 */
describe('the artifacts frame, decoded from a captured response', () => {
  const fixture = (name: string) =>
    new Uint8Array(readFileSync(join(import.meta.dirname, 'fixtures', name)));

  it('refuses a body captured before a hull was a list of rings, rather than reading it one level shallow', () => {
    // A real pre-r40 body, not a hand-assembled one — the only reading of these bytes a decoder
    // written for the nested wire could otherwise reach is one ring of one vertex per artifact,
    // drawn as nothing and picked as nothing, with no error anywhere.
    expect(() => decodeViewport(fixture('viewport-artifacts-pre-r40.bin'))).toThrow(/hull_x.*list of rings/s);
  });

  it('carries one row per served artifact, and no points beside them', () => {
    const result = decodeViewport(fixture('viewport-artifacts.bin'));
    expect(result.artifacts.length).toBeGreaterThan(0);
    // Captured at `k = 0` — the annotation channel's own request shape. A body with an artifacts
    // frame and no points frame at all is the case a decoder is most likely to get wrong.
    expect(result.ids.length).toBe(0);
    expect(result.membership).toEqual({});

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

  it('carries the derived geometry in the same grid units as the points', () => {
    const result = decodeViewport(fixture('viewport-artifacts.bin'));
    // The captured layer declares all three, so every row carries all three. A null here would be
    // *the layer declares none* and never *withheld* — content is never withheld from a served
    // artifact — so a null on a layer that declares the property is a decoder or a server bug.
    for (const a of result.artifacts) {
      expect(a.centroid).not.toBeNull();
      expect(a.box).not.toBeNull();
      expect(a.hull).not.toBeNull();

      const [cx, cy] = a.centroid!;
      const [minX, minY, maxX, maxY] = a.box!;
      // The centroid is a mean of the members' positions, so it lies inside their bounds. This
      // catches the axis transposition a two-column-per-shape wire invites.
      expect(cx).toBeGreaterThanOrEqual(minX);
      expect(cx).toBeLessThanOrEqual(maxX);
      expect(cy).toBeGreaterThanOrEqual(minY);
      expect(cy).toBeLessThanOrEqual(maxY);

      // One ring per separated group of the visible members, and every vertex a real member's
      // position — so every vertex of every ring sits on the box's bounds or inside them.
      expect(a.hull!.length).toBeGreaterThan(0);
      for (const ring of a.hull!) {
        expect(ring.length).toBeGreaterThan(0);
        for (const [x, y] of ring) {
          expect(x).toBeGreaterThanOrEqual(minX);
          expect(x).toBeLessThanOrEqual(maxX);
          expect(y).toBeGreaterThanOrEqual(minY);
          expect(y).toBeLessThanOrEqual(maxY);
        }
      }
      // Grid units, not data coordinates: the axes span 2^32, exactly as `codes` does.
      expect(maxX).toBeLessThanOrEqual(2 ** 32);
    }
    // Different clusters, different shapes — one geometry repeated across rows would mean the
    // decoder read row 0 for everybody.
    const centroids = new Set(result.artifacts.map((a) => a.centroid!.join(',')));
    expect(centroids.size).toBe(result.artifacts.length);
  });
});

describe('the drill-down', () => {
  it('requires the view on the wire, and widens the count the frame delivers as u64', async () => {
    const seen = stubFetch(
      () =>
        new Response(JSON.stringify({layer: 'clusters/x', key: 'c-0001', masked_count: 143}), {
          status: 200
        })
    );

    const detail = await client().artifact('tok', 42n, {view: 's0'});

    expect(seen[0]!.url).toBe('http://viewer/v1/artifacts/42');
    expect(seen[0]!.body).toEqual({view: 's0'});
    expect(detail).toEqual({layer: 'clusters/x', key: 'c-0001', maskedCount: 143n});
  });

  it('reads an absent key as none rather than as a missing field', async () => {
    stubFetch(() => new Response(JSON.stringify({layer: 'clusters/x', masked_count: 2}), {status: 200}));
    expect((await client().artifact('tok', 9n, {view: 's0'})).key).toBeNull();
  });

  it('surfaces the one refusal as a typed error, with nothing else to read from it', async () => {
    stubFetch(
      () =>
        new Response(JSON.stringify({error: 'unknown', detail: 'unknown artifact'}), {status: 404})
    );

    // Every withheld case arrives here identically — an identifier naming nothing, one naming a
    // point, one gated, one suppressed, one below its layer's criterion. A caller that branched on
    // the detail string would be inventing a distinction the server refuses to make.
    await expect(client().artifact('tok', 1n, {view: 's0'})).rejects.toThrow(TesseraError);
  });
});
