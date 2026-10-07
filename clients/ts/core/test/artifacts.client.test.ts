import {afterEach, describe, expect, it, vi} from 'vitest';
import {TesseraClient, TesseraError} from '../src/client.js';
import type {TileSink} from '../src/client.js';
import {FRAME_TRAILER, FrameReader} from '../src/frame.js';
import {inlineDecoder, type Decoder} from '../src/decoder.js';
import {fixture, framed, rejectsAsRefused, result} from './support.js';

/** An artifacts trailer for `frames` frames of `rows` rows. */
const trailer = (frames: number, rows: number) => new TextEncoder().encode(JSON.stringify({stream_us: 0, arrow_serialise_ns: 0, rows, frames}));

/**
 * What the client puts on the wire for artifacts, and how it maps the reply. The transport is
 * stubbed: a live server that ignored `layers` would pass on a bundle with one layer.
 */

/** Answers every whole response with an empty result; these tests read the request, not the body. */
const empty: Decoder = {
  decode: async () => result(),
  decodeArtifacts: () => Promise.reject(new Error('a response without a part sink decodes whole')),
  decodePoints: () => Promise.reject(new Error('a response without a part sink decodes whole')),
  lastWorkerMs: null,
  close: () => {}
};

/** Captures every request the client makes, and answers each with the body given. */
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
    decoder: empty
  });

afterEach(() => vi.unstubAllGlobals());

describe('the viewport request', () => {
  it('sends an empty layer selection but omits an absent one', async () => {
    const seen = stubFetch(() => new Response(new ArrayBuffer(0), {status: 200}));
    const c = client();

    await c.viewport('tok', {view: 's0', zoom: 4, layers: []});
    await c.viewport('tok', {view: 's0', zoom: 4});

    // `layers` is `string[] | 'all'`: `[]` and absent both mean none, so a client fetching points
    // pays nothing for artifacts.
    expect(seen[0]!.body.layers).toEqual([]);
    expect('layers' in seen[1]!.body).toBe(false);
  });

  it("sends the string 'all' verbatim, and never substitutes it for an array", async () => {
    const seen = stubFetch(() => new Response(new ArrayBuffer(0), {status: 200}));
    const c = client();
    await c.viewport('tok', {view: 's0', zoom: 4, layers: 'all'});
    await c.viewport('tok', {view: 's0', zoom: 4, layers: ['clusters/x']});
    // `'all'` is every reachable layer and an array those reachable; each is sent as given.
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

});

describe('the artifacts viewport request', () => {
  it('always sends the quota, and every other field only when set, under its wire name', async () => {
    const seen = stubFetch(() => new Response(framed([{kind: FRAME_TRAILER, payload: trailer(0, 0)}]), {status: 200}));
    const c = client();
    await c.viewportArtifacts('tok', {view: 's0', zoom: 4, tiles: [3n, 1n], perTile: 25});
    await c.viewportArtifacts('tok', {
      view: 's0',
      zoom: 4,
      bbox: [0, 0, 1, 1],
      perTile: 0,
      layers: 'all',
      levels: [1],
      computed: [],
      filters: {archive: {eq: 'cs'}},
      highlight: {archive: {eq: 'cs'}},
      budget: 64,
      paletteSize: 10
    });
    expect(seen[0]!.url).toBe('http://viewer/v1/artifacts/viewport');
    expect(seen[0]!.body).toEqual({view: 's0', zoom: 4, tiles: [3, 1], per_tile: 25});
    expect(seen[1]!.body).toEqual({
      view: 's0',
      zoom: 4,
      bbox: [0, 0, 1, 1],
      per_tile: 0,
      layers: 'all',
      levels: [1],
      computed: [],
      filters: {archive: {eq: 'cs'}},
      highlight: {archive: {eq: 'cs'}},
      budget: 64,
      palette_size: 10
    });
  });
});

describe('the bulk read by identifier', () => {
  it('sends the ids as decimal strings, so an id past 2^53 survives', async () => {
    const seen = stubFetch(() => new Response(JSON.stringify({error: 'contract', detail: 'stop here'}), {status: 422}));
    await expect(client().artifacts('tok', {view: 's0', layer: 'l', fields: ['level'], ids: [2n ** 60n + 1n, 5n]})).rejects.toThrow(TesseraError);
    expect(seen[0]!.url).toBe('http://viewer/v1/artifacts');
    expect(seen[0]!.body).toMatchObject({view: 's0', layer: 'l', fields: ['level'], ids: [String(2n ** 60n + 1n), '5']});
  });
});

describe('/v1/meta', () => {
  const selection = {
    k_min: 1,
    k_max_marks: 5000,
    max_k: 5000,
    theta_target_marks: 16,
    max_underlay_offset: 3,
    max_tiles_per_request: 4096,
    max_artifacts_per_tile: 200,
    max_category_values: 1000,
    max_shape_vertices: 50_000,
    max_region_vertices: 10_000,
    max_region_cells: 262_144,
    max_suggestions: 20,
    max_suggestion_walk: 100_000,
    max_suggest_set_entities: 10_000_000,
    max_browse_rows: 200,
    max_page_rows: 65_536,
    max_page_bytes: 16_777_216,
    max_aggregate_groupings: 16,
    max_aggregate_top: 1000,
    max_aggregate_named: 1000,
    max_aggregate_bins: 1000,
    max_aggregate_cells: 1_048_576
  };
  const body = {
    api_version: 1,
    bundle_format: 9,
    views: [{id: 's0', display_name: 'S0', quantisation: {x_min: 0, x_max: 65536, y_min: 0, y_max: 65536}, projection: 'none', world_aspect: null, tile_scheme: null, tile: null, group: null, key: null, metadata: null}],
    groups: [{name: 'quarter', title: null, members_of: null, views: []}],
    declared_scalars: [{name: 'abstract', arrow_type: 'text', category: null, analyser: 'unicode/1', render: false, index: true, unique: false, homes: ['record']}],
    scoped_scalars: [
      {name: 'mood', arrow_type: 'u8', scope: {group: 'quarter'}, category: {vocabulary: 'moods', kind: 'declared', visibility: 'public'}, analyser: null, render: true, index: true, views: ['quarter:q1']}
    ],
    filter_operands: [
      {column: 'abstract', family: 'text', operands: ['match', 'phrase']},
      {column: 'mood', family: 'category', operands: ['eq', 'in'], scope: {group: 'quarter'}}
    ],
    selection,
    layers: [
      {
        name: 'clusters/hdbscan-2026-08',
        title: null,
        views: ['s0'],
        membership: 'enumerated',
        hierarchy: {kind: 'flat', prune_children: false},
        levels: [{level: 0, title: null, zoom: [0, 4]}],
        computed_content: ['centroid'],
        shape: 'derived',
        supplied_content: [],
        depends_on: [],
        version: 3
      }
    ]
  };

  it('decodes every field the contract publishes', async () => {
    stubFetch(() => new Response(JSON.stringify(body), {status: 200}));
    expect(await client().meta('tok')).toEqual({
      apiVersion: 1,
      bundleFormat: 9,
      views: [{id: 's0', displayName: 'S0', quantisation: {xMin: 0, xMax: 65536, yMin: 0, yMax: 65536}, projection: 'none', worldAspect: null, tileScheme: null, tile: null, roster: null}],
      groups: [{name: 'quarter', title: null, membersOf: null, views: []}],
      declaredScalars: [{name: 'abstract', arrowType: 'text', category: null, analyser: 'unicode/1', render: false, index: true, unique: false, homes: ['record']}],
      scopedScalars: [
        {name: 'mood', arrowType: 'u8', scope: {group: 'quarter'}, category: {vocabulary: 'moods', kind: 'declared', visibility: 'public'}, analyser: null, render: true, index: true, views: ['quarter:q1']}
      ],
      filterOperands: [
        {column: 'abstract', family: 'text', operands: ['match', 'phrase']},
        {column: 'mood', family: 'category', operands: ['eq', 'in'], scope: {group: 'quarter'}}
      ],
      selection: {
        kMin: 1,
        kMaxMarks: 5000,
        maxK: 5000,
        thetaTargetMarks: 16,
        maxUnderlayOffset: 3,
        maxArtifactsPerTile: 200,
        maxCategoryValues: 1000,
        maxShapeVertices: 50_000,
        maxRegionVertices: 10_000,
        maxRegionCells: 262_144,
        maxSuggestions: 20,
        maxSuggestionWalk: 100_000,
        maxSuggestSetEntities: 10_000_000,
        maxBrowseRows: 200,
        maxPageRows: 65_536,
        maxPageBytes: 16_777_216,
        maxAggregateGroupings: 16,
        maxAggregateTop: 1000,
        maxAggregateNamed: 1000,
        maxAggregateBins: 1000,
        maxAggregateCells: 1_048_576
      },
      maxTilesPerRequest: 4096,
      layers: [
        {
          name: 'clusters/hdbscan-2026-08',
          title: null,
          views: ['s0'],
          membership: 'enumerated',
          hierarchy: {kind: 'flat', pruneChildren: false},
          levels: [{level: 0, title: null, zoom: [0, 4]}],
          computedContent: ['centroid'],
          shape: 'derived',
          suppliedContent: [],
          depsOn: [],
          version: 3
        }
      ]
    });
  });

  it('refuses a body missing any field the contract requires, at the top, in selection or in any list element', async () => {
    type Json = Record<string, unknown>;
    const at = (root: Json, path: (string | number)[]): Json => path.reduce<Json>((node, step) => node[step] as Json, root);
    const blocks: (string | number)[][] = [[], ['selection'], ['views', 0], ['groups', 0], ['declared_scalars', 0], ['scoped_scalars', 0], ['filter_operands', 0], ['layers', 0], ['layers', 0, 'levels', 0]];
    const missing: Json[] = [];
    for (const path of blocks) {
      for (const field of Object.keys(at(body, path))) {
        // `scope` is present only on a group-scoped operand.
        if (path[0] === 'filter_operands' && field === 'scope') continue;
        const copy = structuredClone(body) as Json;
        delete at(copy, path)[field];
        missing.push(copy);
      }
    }
    let answer: Record<string, unknown> = body;
    stubFetch(() => new Response(JSON.stringify(answer), {status: 200}));
    const c = client();
    for (const without of missing) {
      answer = without;
      await rejectsAsRefused(c.meta('tok'));
    }
  });
});

/**
 * An artifacts body as a server sent it: the k-means layer of the notebook corpus, which declares
 * centroid and box over clusters in different parts of the map, at zoom 2 over the whole of view
 * `s0`, captured by `scripts/capture-golden.mjs`.
 */
describe('the artifacts viewport, read from a captured response', () => {
  const read = async (name: string, onTile?: TileSink) => {
    stubFetch(() => new Response(fixture(name), {status: 200, headers: {'x-tessera-identity-key': 'ik', etag: '"ck"'}}));
    return new TesseraClient({viewerUrl: 'http://viewer', sessionUrl: 'http://session', decoder: inlineDecoder()}).viewportArtifacts('tok', {view: 's0', zoom: 2, bbox: [0, 0, 1, 1], perTile: 50}, {onTile});
  };

  it('hands over one frame per tile in wire order, with the response’s keys, and returns the same frames', async () => {
    const handed: unknown[] = [];
    const response = await read('viewport-artifacts.bin', (frame, keys) => {
      handed.push(frame);
      expect(keys).toEqual({identityKey: 'ik', contentKey: 'ck'});
    });
    expect(response.frames).toEqual(handed);
    expect(response.frames).toHaveLength(16);
    expect(response.frames.every((f) => !f.treed)).toBe(true);
    // A tile with no artifact is a frame of no rows; one with any names its tile.
    expect(response.frames.some((f) => f.artifacts.length === 0)).toBe(true);
    for (const f of response.frames) if (f.artifacts.length > 0) expect(f.tile).not.toBeNull();
  });

  it('serves an artifact in each tile it has members in, with the same figures, largest first within a tile', async () => {
    const {frames} = await read('viewport-artifacts.bin');
    const seen = new Map<bigint, string>();
    let repeated = 0;
    for (const {artifacts} of frames) {
      for (let i = 1; i < artifacts.length; i++) expect(artifacts[i - 1]!.maskedCount >= artifacts[i]!.maskedCount).toBe(true);
      for (const a of artifacts) {
        expect(a.tesseraId).toBeTypeOf('bigint');
        expect(a.maskedCount).toBeGreaterThan(0n);
        const figures = `${a.maskedCount}|${a.centroid}|${a.box}`;
        if (seen.has(a.tesseraId)) {
          repeated += 1;
          expect(figures).toBe(seen.get(a.tesseraId));
        }
        seen.set(a.tesseraId, figures);
      }
    }
    expect(repeated).toBeGreaterThan(0);
  });

  it('carries each cluster’s slot below the palette size it was asked with, the same in every tile', async () => {
    const {frames} = await read('viewport-artifacts.bin');
    const slots = new Map<bigint, number | null>();
    for (const a of frames.flatMap((f) => f.artifacts)) {
      expect(a.slot).toBeTypeOf('number');
      expect(a.slot!).toBeLessThan(10);
      if (slots.has(a.tesseraId)) expect(a.slot).toBe(slots.get(a.tesseraId));
      slots.set(a.tesseraId, a.slot);
    }
    // One slot on every row would mean row 0 was read for all.
    expect(new Set(slots.values()).size).toBeGreaterThan(1);
  });

  it('carries the derived geometry in the same grid units as the points', async () => {
    const {frames} = await read('viewport-artifacts.bin');
    const artifacts = new Map(frames.flatMap((f) => f.artifacts).map((a) => [a.tesseraId, a]));
    for (const a of artifacts.values()) {
      const [cx, cy] = a.centroid!;
      const [minX, minY, maxX, maxY] = a.box!;
      // A mean of the members' positions lies inside their bounds; this catches transposed axes.
      expect(cx).toBeGreaterThanOrEqual(minX);
      expect(cx).toBeLessThanOrEqual(maxX);
      expect(cy).toBeGreaterThanOrEqual(minY);
      expect(cy).toBeLessThanOrEqual(maxY);
      expect(maxX).toBeLessThanOrEqual(2 ** 32);
    }
    // Different clusters, different centroids; one on every row would mean row 0 was read for all.
    expect(new Set([...artifacts.values()].map((a) => a.centroid!.join(','))).size).toBe(artifacts.size);
  });

  it('refuses a body cut before its trailer, after handing over the frames that were whole', async () => {
    const body = fixture('viewport-artifacts.bin');
    const reader = new FrameReader('artifacts');
    const frames = reader.push(body);
    const cut = body.subarray(0, body.byteLength - 5 - frames.at(-1)!.payload.byteLength);
    stubFetch(() => new Response(cut, {status: 200}));
    const handed: unknown[] = [];
    await rejectsAsRefused(
      new TesseraClient({viewerUrl: 'http://viewer', sessionUrl: 'http://session', decoder: inlineDecoder()}).viewportArtifacts(
        'tok',
        {view: 's0', zoom: 2, bbox: [0, 0, 1, 1], perTile: 50},
        {onTile: (f) => void handed.push(f)}
      )
    );
    expect(handed).toHaveLength(16);
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
    expect(detail).toEqual({layer: 'clusters/x', key: 'c-0001', maskedCount: 143n, centroid: null, box: null, shape: null});
  });

  it('carries the geometry, which is what the viewport is no longer asked for', async () => {
    // The route the drawn shape comes from: the viewport serves centroids and boxes, and this
    // answers with the one shape drawn.
    const seen = stubFetch(
      () =>
        new Response(
          JSON.stringify({
            layer: 'clusters/x',
            masked_count: 3,
            centroid: [10.5, 20.5],
            box: [0, 0, 20, 40],
            shape: [[[[0, 0], [20, 0], [20, 40]]], [[[100, 100], [110, 100], [110, 110]]]]
          }),
          {status: 200}
        )
    );
    const detail = await client().artifact('tok', 7n, {view: 's0', zoom: 5.7});
    expect(detail.centroid).toEqual([10.5, 20.5]);
    expect(detail.box).toEqual([0, 0, 20, 40]);
    // Parts of rings, not a list of vertices: a membership that is two clouds is two parts.
    expect(detail.shape?.length).toBe(2);
    expect(detail.shape?.[1]?.[0]?.[0]).toEqual([100, 100]);
    // The zoom is sent as the whole depth the server's vertex rule reads.
    expect(seen[0]!.body).toEqual({view: 's0', zoom: 5});
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

    // Every withheld case arrives alike: an id naming nothing, one naming a point, one gated, one
    // suppressed, one below its layer's criterion.
    await expect(client().artifact('tok', 1n, {view: 's0'})).rejects.toThrow(TesseraError);
  });
});
