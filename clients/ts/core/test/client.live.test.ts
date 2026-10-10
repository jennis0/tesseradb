import {afterAll, beforeAll, describe, expect, it, type TestContext} from 'vitest';
import {MosaicaClient, MosaicaError} from '../src/client.js';
import {tileOfCode} from '../src/coords.js';
import type {Meta, Session, ViewportRequest} from '../src/types.js';
import {GROUP, start, type Served} from './served.js';

/**
 * `MosaicaClient` against a real `mosaica serve` over the notebook corpus (`served.ts` builds and
 * starts it). Each test checks a decoded value against another route's answer or against the
 * declaration, so a field read from the wrong place, or not at all, fails here.
 *
 * Run it with `npm --prefix clients/ts/core run test:live`. Where the binary or the corpus is not
 * on this machine every test is skipped with the reason.
 */

/** The corpus labels each paper with its arXiv categories; these three admit several thousand. */
const TERMS = ['cs.LG', 'cs.CV', 'hep-ph'];

let served: Served | string = 'the server has not started';
let client: MosaicaClient;
let session: Session;
let meta: Meta;

beforeAll(async () => {
  served = await start();
  if (typeof served === 'string') return;
  client = new MosaicaClient({viewerUrl: served.viewerUrl, sessionUrl: served.sessionUrl, sessionCredential: served.operatorCredential});
  session = await client.authorise({terms: TERMS});
  meta = await client.meta(session.token);
}, 120_000);

afterAll(() => {
  if (typeof served !== 'string') served.stop();
  client?.close();
});

/** Skips the calling test, with the reason, where no server could be started. */
function live(ctx: TestContext): void {
  if (typeof served === 'string') ctx.skip(served);
}

/** The whole of view `s0`, one request. */
function whole(extra: Partial<ViewportRequest> = {}): ViewportRequest {
  const q = meta.views.find((v) => v.id === 's0')!.quantisation;
  return {view: 's0', zoom: 2, bbox: [q.xMin, q.yMin, q.xMax, q.yMax], ...extra};
}

const total = (tiles: readonly {visible: bigint; matched: bigint; served: bigint}[], field: 'visible' | 'matched' | 'served') =>
  tiles.reduce((sum, t) => sum + t[field], 0n);

describe('MosaicaClient against a live server', () => {
  it('authorises a session whose expiry is within the deployment’s token lifetime', (ctx) => {
    live(ctx);
    expect(session.token).not.toBe('');
    expect(Number.isInteger(session.tokenId)).toBe(true);
    const now = Date.now() / 1000;
    expect(session.expiresAt).toBeGreaterThan(now);
    expect(session.expiresAt).toBeLessThanOrEqual(now + 3600 + 60);
  });

  it('translates /v1/meta: views with their rosters, the group, the declared scalars and the layers', (ctx) => {
    live(ctx);
    const plain = meta.views.find((v) => v.id === 's0')!;
    expect(plain.roster).toBeNull();
    expect(plain.projection).toBe('none');
    expect(plain.quantisation.xMin).toBeLessThan(plain.quantisation.xMax);
    expect(plain.quantisation.yMin).toBeLessThan(plain.quantisation.yMax);

    const ids = GROUP.keys.map((key) => `${GROUP.name}:${key}`);
    expect(meta.groups).toEqual([{name: GROUP.name, title: GROUP.title, membersOf: null, views: ids}]);
    for (const [i, id] of ids.entries()) {
      const view = meta.views.find((v) => v.id === id)!;
      expect(view.roster).toEqual({group: GROUP.name, key: GROUP.keys[i], metadata: {label: {type: 'text', value: GROUP.labels[i]}}});
    }

    expect(meta.declaredScalars.find((s) => s.name === 'archive')).toEqual({
      name: 'archive',
      arrowType: 'u8',
      category: {vocabulary: 'archive', kind: 'declared', visibility: 'public'},
      render: true,
      index: true,
      unique: false,
      analyser: null,
      homes: expect.arrayContaining(['rendered'])
    });
    expect(meta.declaredScalars.find((s) => s.name === 'submitted_at')).toMatchObject({arrowType: 'timestamp_us', category: null, render: true, index: false});
    expect(meta.filterOperands.find((f) => f.column === 'archive')).toMatchObject({family: 'category'});

    const taxonomy = meta.layers.find((l) => l.name === 'taxonomy/arxiv')!;
    expect(taxonomy.hierarchy).toEqual({kind: 'tiered', pruneChildren: false});
    expect(taxonomy.levels).toEqual([
      {level: 0, title: 'archive', zoom: null},
      {level: 1, title: 'subject class', zoom: null}
    ]);
    expect(meta.layers.find((l) => l.name === 'topics/kmeans')!.depsOn).toEqual(['clusters/kmeans']);
    expect(meta.selection.maxBrowseRows).toBeGreaterThan(0);
    expect(Number.isInteger(meta.bundleFormat)).toBe(true);
    expect(meta.scopedScalars).toEqual([]);
    for (const [name, ceiling] of Object.entries(meta.selection)) expect(Number.isInteger(ceiling), name).toBe(true);
  });

  it('decodes a viewport whose points sit in the tiles that served them, at the positions /v1/items gives', async (ctx) => {
    live(ctx);
    const response = await client.viewport(session.token, whole({k: 50}));
    const {tiles, ids, codes, positions} = response.result;
    expect(total(tiles, 'visible')).toBeGreaterThan(0n);
    expect(BigInt(ids.length)).toBe(total(tiles, 'served'));
    expect(new Set(ids).size).toBe(ids.length);
    for (const t of tiles) expect(t.served).toBeLessThanOrEqual(t.matched < 50n ? t.matched : 50n);
    // Each point's code names a tile of this depth that served points.
    const serving = new Set(tiles.filter((t) => t.served > 0n).map((t) => t.tile));
    for (const code of codes) expect(serving.has(tileOfCode(code, 2))).toBe(true);
    expect(response.contentKey).not.toBe('');
    expect(response.pin).not.toBeNull();

    // The item route places the same point where the viewport did: its grid units are the
    // viewport's cell space times 2^16.
    const item = await client.item(session.token, ids[0]!);
    const at = item.views.find((v) => v.id === 's0')!;
    expect(at.x).toBeCloseTo(positions[0]! * 65536, 0);
    expect(at.y).toBeCloseTo(positions[1]! * 65536, 0);
  });

  it('counts nothing for a principal holding none of the corpus’s labels', async (ctx) => {
    live(ctx);
    const nobody = await client.authorise({terms: []});
    const response = await client.viewport(nobody.token, whole({k: 50}));
    expect(total(response.result.tiles, 'visible')).toBe(0n);
    expect(response.result.ids.length).toBe(0);
  });

  it('resolves a served category code to the key the item card names', async (ctx) => {
    live(ctx);
    const response = await client.viewport(session.token, whole({k: 20}));
    const column = response.result.scalars.archive!;
    const code = Number(column.values[0]);
    const [value] = await client.categories(session.token, 'archive', {codes: [code]});
    expect(value!.code).toBe(code);
    const item = await client.item(session.token, response.result.ids[0]!);
    expect(item.fields.archive).toBe(value!.key);
    expect(item.fields.title).toBeTypeOf('string');
    // The labels admitting this session are some of the terms it holds, and at least one.
    expect(item.labels.length).toBeGreaterThan(0);
    for (const label of item.labels) expect(TERMS).toContain(label);
  });

  it('pages through a vocabulary and agrees with the codes form', async (ctx) => {
    live(ctx);
    const all = await client.categories(session.token, 'archive');
    const paged = await client.categories(session.token, 'archive', {limit: 3});
    expect(all.length).toBeGreaterThan(3);
    expect(paged).toEqual(all);
    expect(new Set(all.map((v) => v.code)).size).toBe(all.length);
    // The notebook's vocabularies give every value a title.
    for (const v of all) expect(v.title).toMatch(/\S/);
    const some = all.slice(0, 2);
    expect(await client.categories(session.token, 'archive', {codes: some.map((v) => v.code)})).toEqual(some);
  });

  it('suggests a value with the count the viewport matches under a filter on it', async (ctx) => {
    live(ctx);
    const page = await client.suggest(session.token, 'archive', 'cs', {counts: true});
    if (page.status !== 'ok') throw new Error(`suggest answered ${page.status}`);
    expect(page.q).toBe('cs');
    const cs = page.values.find((v) => v.key === 'cs')!;
    expect(cs.match).toEqual({field: 'key', start: 0, len: 2});
    expect(cs.title).toMatch(/\S/);
    const filtered = await client.viewport(session.token, whole({k: 0, filters: {archive: {in: ['cs']}}}));
    expect(BigInt(cs.count!)).toBe(total(filtered.result.tiles, 'matched'));
    expect(cs.count).toBeGreaterThan(0);
  });

  it('opens an artifact by identifier with the count and geometry the viewport served', async (ctx) => {
    live(ctx);
    const {zoom, bbox, view} = whole();
    const response = await client.viewportArtifacts(session.token, {view, zoom, bbox, layers: ['taxonomy/arxiv'], perTile: 5});
    const first = response.frames.flatMap((f) => f.artifacts)[0]!;
    expect(first.layer).toBe('taxonomy/arxiv');
    const opened = await client.artifact(session.token, first.mosaicaId, {view: 's0'});
    expect(opened).toMatchObject({layer: first.layer, key: first.key, maskedCount: first.maskedCount, centroid: first.centroid, box: first.box});
    expect(opened.maskedCount).toBeGreaterThan(0n);
    await expect(client.artifact(session.token, 1n, {view: 's0'})).rejects.toMatchObject({status: 404});
  });

  it('streams one artifacts frame per tile, each at most the quota per level, an artifact the same in every tile', async (ctx) => {
    live(ctx);
    const tiles = [0n, 1n, 2n, 3n];
    const frames: unknown[] = [];
    const response = await client.viewportArtifacts(session.token, {view: 's0', zoom: 1, tiles, layers: ['taxonomy/arxiv'], levels: [0], perTile: 3}, {onTile: (f) => void frames.push(f)});
    expect(response.frames).toEqual(frames);
    expect(response.frames.map((f) => f.tile)).toEqual(tiles);
    const figures = new Map<bigint, string>();
    for (const frame of response.frames) {
      expect(frame.artifacts.length).toBeLessThanOrEqual(3);
      for (const a of frame.artifacts) {
        const these = `${a.maskedCount}|${a.centroid}|${a.box}`;
        expect(figures.get(a.mosaicaId) ?? these).toBe(these);
        figures.set(a.mosaicaId, these);
      }
    }
    expect(figures.size).toBeGreaterThan(0);
    await expect(client.viewportArtifacts(session.token, {view: 's0', zoom: 1, tiles, perTile: meta.selection.maxArtifactsPerTile + 1})).rejects.toMatchObject({status: 422});
  });

  it('reads a point’s tag by identifier, with the level and count its artifact is served with', async (ctx) => {
    live(ctx);
    const points = await client.viewport(session.token, whole({k: 20, layers: ['taxonomy/arxiv'], levels: [1]}));
    const tags = points.result.membership['taxonomy/arxiv']!;
    expect(tags.ids.length).toBeGreaterThan(0);
    const ids = [...tags.ids];
    const read = await client.artifacts(session.token, {view: 's0', layer: 'taxonomy/arxiv', ids, fields: ['level', 'masked_count']});
    const rows: {id: bigint; level: number; count: bigint}[] = [];
    for await (const page of read) {
      for (let i = 0; i < page.numRows; i++) {
        rows.push({id: BigInt(page.getChild('mosaica_id')!.get(i)), level: Number(page.getChild('level')!.get(i)), count: BigInt(page.getChild('masked_count')!.get(i))});
      }
    }
    expect(rows.map((r) => r.id).sort()).toEqual([...ids].sort());
    for (const r of rows) expect(r.level).toBe(1);
    const opened = await client.artifact(session.token, rows[0]!.id, {view: 's0'});
    expect(opened.maskedCount).toBe(rows[0]!.count);
  });

  it('gives each artifact one slot below the palette size asked for, the same on every route, and none without one', async (ctx) => {
    live(ctx);
    const {zoom, bbox, view} = whole();
    const asked = {view, zoom, bbox, layers: ['clusters/kmeans'], perTile: 50};
    const plain = await client.viewportArtifacts(session.token, asked);
    expect(plain.frames.flatMap((f) => f.artifacts).every((a) => a.slot === null)).toBe(true);
    const response = await client.viewportArtifacts(session.token, {...asked, paletteSize: 8});
    const slots = new Map<bigint, number | null>();
    for (const a of response.frames.flatMap((f) => f.artifacts)) {
      expect(a.slot).toBeTypeOf('number');
      expect(a.slot!).toBeLessThan(8);
      expect(slots.get(a.mosaicaId) ?? a.slot).toBe(a.slot);
      slots.set(a.mosaicaId, a.slot);
    }
    expect(slots.size).toBeGreaterThan(1);
    const roots = await client.browse(session.token, {view: 's0', layer: 'clusters/kmeans', paletteSize: 8});
    for (const row of roots.artifacts) if (slots.has(row.mosaicaId)) expect(row.slot).toBe(slots.get(row.mosaicaId));
    const read = await client.artifacts(session.token, {view: 's0', layer: 'clusters/kmeans', ids: [...slots.keys()], fields: ['slot'], paletteSize: 8});
    for await (const page of read) {
      for (let i = 0; i < page.numRows; i++) expect(page.getChild('slot')!.get(i)).toBe(slots.get(BigInt(page.getChild('mosaica_id')!.get(i))));
    }
    const {tables} = await client.aggregate(session.token, {view: 's0', groupings: [{by: {layer: 'clusters/kmeans', top: 5, paletteSize: 8}}]});
    const rows = tables[0]!.rows;
    for (let i = 0; i < rows.numRows; i++) {
      if (rows.getChild('group')!.get(i) !== 'listed') continue;
      const id = BigInt(rows.getChild('key')!.get(i));
      if (slots.has(id)) expect(rows.getChild('slot')!.get(i)).toBe(slots.get(id));
    }
    await expect(client.viewportArtifacts(session.token, {...asked, paletteSize: 1})).rejects.toMatchObject({status: 422});
  });

  it('browses a tiered layer from its roots to one root’s children', async (ctx) => {
    live(ctx);
    const roots = await client.browse(session.token, {view: 's0', layer: 'taxonomy/arxiv', level: 0});
    expect(roots.artifacts.length).toBeGreaterThan(0);
    for (const row of roots.artifacts) expect(row.rung).toBe(0);
    const root = roots.artifacts[0]!;
    const opened = await client.artifact(session.token, root.mosaicaId, {view: 's0'});
    expect(root.maskedCount).toBe(opened.maskedCount);

    // The children form carries the named artifact's own parents, which a root has none of.
    const children = await client.browse(session.token, {view: 's0', layer: 'taxonomy/arxiv', parent: root.mosaicaId});
    expect(children.parents).toEqual([]);
    expect(children.artifacts.length).toBeGreaterThan(0);
    for (const child of children.artifacts) {
      expect(child.rung).toBe(1);
      expect(child.parentIds).toContain(root.mosaicaId);
    }
    const leaf = await client.browse(session.token, {view: 's0', layer: 'taxonomy/arxiv', parent: children.artifacts[0]!.mosaicaId});
    expect(leaf.parents.map((p) => p.mosaicaId)).toEqual([root.mosaicaId]);
    expect(leaf.parents[0]!.maskedCount).toBe(root.maskedCount);
  });

  it('refuses a bad token and a reversed bbox with typed errors', async (ctx) => {
    live(ctx);
    const refused = client.meta('not-a-real-token');
    await expect(refused).rejects.toBeInstanceOf(MosaicaError);
    await expect(refused).rejects.toMatchObject({status: 401, code: 'bad-credential'});
    await expect(client.viewport(session.token, {view: 's0', zoom: 2, bbox: [10, 10, 0, 0]})).rejects.toMatchObject({status: 422, code: 'contract'});
  });
});
