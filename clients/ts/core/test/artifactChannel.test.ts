import {describe, expect, it, vi} from 'vitest';
import {
  ARTIFACT_TILES_PER_REQUEST,
  ArtifactChannel,
  declaredLevelsAt,
  requestLevels,
  type ArtifactChannelOptions,
  type ArtifactChannelState
} from '../src/artifactChannel.js';
import {SessionArtifactTable} from '../src/artifactTable.js';
import {MosaicaClient, type TileSink} from '../src/client.js';
import {GRID32, WORLD_SIZE, mortonOfTile} from '../src/coords.js';
import type {Artifact, Layer, Quantisation, ViewportArtifactsRequest} from '../src/types.js';
import {artifact, layer, manualClock, settle, tileAnswers} from './support.js';

const Q: Quantisation = {xMin: 0, xMax: 100, yMin: 0, yMax: 100};

/**
 * The view every test starts from, a 200 by 150 screen at zoom 3, so asked at depth 5. At depth 5 a
 * tile is 16 world units, and this box covers tiles x 2..3 and y 2..3.
 */
const view = {target: [50, 50, 0] as [number, number, number], zoom: 3};
const DEPTH = 5;
const at = (x: number, y: number) => mortonOfTile(x, y, DEPTH);
const VIEW_TILES = [at(2, 2), at(3, 2), at(2, 3), at(3, 3)];

const cluster = (id: bigint, over: Partial<Artifact> = {}): Artifact => artifact(id, {layer: 'clusters/x', key: `c-${id}`, maskedCount: 10n, ...over});
const FLAT = [layer('clusters/x')];

/** A fake client answering each tile with `rowsFor`'s rows, and recording each request. */
function fakeClient(rowsFor: (tile: bigint, req: ViewportArtifactsRequest) => Artifact[] = (tile) => [cluster(tile + 100n)], keys = () => ({identityKey: 'ik', contentKey: 'ck'}), treed?: (req: ViewportArtifactsRequest) => Artifact[]) {
  const viewportArtifacts = vi.fn(tileAnswers(rowsFor, keys, treed));
  const artifacts = vi.fn();
  return {client: {viewportArtifacts, artifacts} as unknown as MosaicaClient, viewportArtifacts, artifacts};
}

function channel(client: MosaicaClient, over: Partial<ArtifactChannelOptions> = {}) {
  const clock = manualClock();
  const states: ArtifactChannelState[] = [];
  const table = new SessionArtifactTable();
  const ch = new ArtifactChannel(client, {
    view: 's0',
    quantisation: Q,
    token: async () => 'tok',
    perTile: 8,
    heldTiles: 100,
    prefetch: false,
    declarations: FLAT,
    clock,
    table,
    onChange: (s) => states.push(s),
    ...over
  });
  return {ch, states, clock, table};
}

const asked = (fn: ReturnType<typeof fakeClient>['viewportArtifacts'], call = 0) => fn.mock.calls[call]![1];
const served = (ch: ArtifactChannel) => ch.current.artifacts.map((a) => a.mosaicaId).sort((a, b) => (a < b ? -1 : 1));

describe('the artifact channel asks by tile', () => {
  it('debounces: a request goes out once the view settles, not per schedule', async () => {
    const {client, viewportArtifacts} = fakeClient();
    const {ch, clock} = channel(client);
    ch.setLayer('clusters/x');
    ch.schedule(view, 200, 150);
    ch.schedule(view, 200, 150);
    ch.schedule(view, 200, 150);
    expect(viewportArtifacts).not.toHaveBeenCalled();
    expect(clock.pending).toBe(1);
    clock.fire();
    await settle();
    expect(viewportArtifacts).toHaveBeenCalledTimes(1);
  });

  it('names the tiles of the view at map zoom + 2, the quota it was given and the layers, and no filter or budget it was not given', async () => {
    const {client, viewportArtifacts} = fakeClient();
    const {ch} = channel(client);
    ch.setLayer('clusters/x');
    ch.refresh(view, 200, 150);
    await settle();
    const req = asked(viewportArtifacts);
    expect(req).toMatchObject({view: 's0', zoom: DEPTH, layers: ['clusters/x'], perTile: 8});
    expect([...req.tiles!].sort()).toEqual([...VIEW_TILES].sort());
    expect('filters' in req || 'budget' in req || 'levels' in req).toBe(false);
    expect(served(ch)).toEqual(VIEW_TILES.map((t) => t + 100n).sort((a, b) => (a < b ? -1 : 1)));
    expect(ch.current.status).toBe('shown');
  });

  it('asks for the tiles nearest the camera first', async () => {
    const {client, viewportArtifacts} = fakeClient();
    const {ch} = channel(client);
    ch.setLayer('clusters/x');
    // Centred inside tile (3, 3).
    ch.refresh({target: [56, 56, 0], zoom: 3}, 200, 150);
    await settle();
    expect(asked(viewportArtifacts).tiles![0]).toBe(at(3, 3));
  });

  it('asks only for the tiles it does not hold, and asks nothing for a view it holds whole', async () => {
    const {client, viewportArtifacts} = fakeClient();
    const {ch, table} = channel(client);
    ch.setLayer('clusters/x');
    ch.refresh(view, 200, 150);
    await settle();
    const ordinal = table.ordinalOf('clusters/x', at(2, 2) + 100n);

    // One tile to the right: x 3..4, so only x = 4 is new.
    ch.refresh({target: [66, 50, 0], zoom: 3}, 200, 150);
    await settle();
    expect([...asked(viewportArtifacts, 1).tiles!].sort()).toEqual([at(4, 2), at(4, 3)].sort());
    expect(served(ch)).toEqual([at(3, 2), at(4, 2), at(3, 3), at(4, 3)].map((t) => t + 100n).sort((a, b) => (a < b ? -1 : 1)));

    // Back: every tile is held, nothing is asked, and the artifact keeps its ordinal.
    const version = table.version;
    ch.refresh(view, 200, 150);
    await settle();
    expect(viewportArtifacts).toHaveBeenCalledTimes(2);
    expect(table.version).toBe(version);
    expect(table.ordinalOf('clusters/x', at(2, 2) + 100n)).toBe(ordinal);
  });

  it('serves an artifact held in several tiles once, matched where any tile matched it', async () => {
    const {client} = fakeClient((tile) => [cluster(1n, {matched: tile === at(3, 3), parentIds: tile === at(2, 2) ? [9n] : []})]);
    const {ch} = channel(client);
    ch.setLayer('clusters/x');
    ch.refresh(view, 200, 150);
    await settle();
    expect(ch.current.artifacts).toHaveLength(1);
    expect(ch.current.artifacts[0]).toMatchObject({mosaicaId: 1n, matched: true, parentIds: [9n]});
  });

  it('draws each tile as it lands, before the response resolves', async () => {
    const {client} = fakeClient();
    const {ch, states} = channel(client);
    ch.setLayer('clusters/x');
    ch.refresh(view, 200, 150);
    await settle();
    // Drawn at the first tile and at each doubling after it.
    const loading = states.filter((s) => s.status === 'loading').map((s) => s.artifacts.length);
    expect(loading).toEqual(expect.arrayContaining([1, 2, 4]));
  });

  it('clears the served set on a refusal and keeps the tiles: a refusal is not an empty view', async () => {
    const {client, viewportArtifacts} = fakeClient();
    const {ch} = channel(client);
    ch.setLayer('clusters/x');
    ch.refresh(view, 200, 150);
    await settle();
    viewportArtifacts.mockRejectedValueOnce(Object.assign(new Error('gone'), {code: 'refused', detail: 'gone'}));
    ch.refresh({target: [66, 50, 0], zoom: 3}, 200, 150);
    await settle();
    expect(ch.current.status).toBe('refused');
    expect(ch.current.artifacts).toEqual([]);
    expect(ch.heldTiles).toBe(4);
  });

  it('refuses to ask with no quota, saying what to set', async () => {
    const {client, viewportArtifacts} = fakeClient();
    const {ch} = channel(client, {perTile: null});
    ch.setLayer('clusters/x');
    ch.refresh(view, 200, 150);
    await settle();
    expect(viewportArtifacts).not.toHaveBeenCalled();
    expect(ch.current.status).toBe('refused');
    expect(ch.current.refusal?.code).toBe('per-tile');
  });

  it('asks nothing and shows nothing with no layer, and keeps the tiles for when one is back', async () => {
    const {client, viewportArtifacts} = fakeClient();
    const {ch} = channel(client);
    ch.setLayer('clusters/x');
    ch.refresh(view, 200, 150);
    await settle();
    ch.setLayer(null);
    ch.refresh(view, 200, 150);
    await settle();
    expect(ch.current).toMatchObject({status: 'idle', artifacts: []});
    ch.setLayer('clusters/x');
    ch.refresh(view, 200, 150);
    await settle();
    expect(viewportArtifacts).toHaveBeenCalledTimes(1);
    expect(ch.current.artifacts).toHaveLength(4);
  });
});

describe('what the held tiles answer', () => {
  it('drops them when a response comes under another content key or identity key', async () => {
    let keys = {identityKey: 'ik', contentKey: 'ck'};
    const {client, viewportArtifacts} = fakeClient(undefined, () => keys);
    const {ch, table} = channel(client);
    ch.setLayer('clusters/x');
    ch.refresh(view, 200, 150);
    await settle();
    keys = {identityKey: 'ik', contentKey: 'ck2'};
    ch.refresh({target: [66, 50, 0], zoom: 3}, 200, 150);
    await settle();
    // The new tiles came under a new key, so the old ones were dropped and those of the view asked
    // for again.
    expect([...asked(viewportArtifacts, 1).tiles!].sort()).toEqual([at(4, 2), at(4, 3)].sort());
    expect([...asked(viewportArtifacts, 2).tiles!].sort()).toEqual([at(3, 2), at(3, 3)].sort());
    expect(ch.heldTiles).toBe(4);
    expect(table.live).toBe(4);
    expect(table.ordinalOf('clusters/x', at(2, 2) + 100n)).toBe(0);
    keys = {identityKey: 'other', contentKey: 'ck2'};
    ch.refresh({target: [50, 82, 0], zoom: 3}, 200, 150);
    await settle();
    expect(ch.heldTiles).toBe(4);
    expect(served(ch)).toEqual([at(2, 4), at(3, 4), at(2, 5), at(3, 5)].map((t) => t + 100n).sort((a, b) => (a < b ? -1 : 1)));
  });

  it('drops them when the point path observes a new content key, and asks again for the view', async () => {
    const {client, viewportArtifacts} = fakeClient();
    const {ch} = channel(client);
    ch.setLayer('clusters/x');
    ch.refresh(view, 200, 150);
    await settle();
    ch.observeContentKey('ck');
    expect(viewportArtifacts).toHaveBeenCalledTimes(1);
    ch.observeContentKey('ck-new');
    await settle();
    expect(viewportArtifacts).toHaveBeenCalledTimes(2);
    expect(asked(viewportArtifacts, 1).tiles).toHaveLength(4);
  });

  it('holds a tile per filter: a filtered view asks, with the filter, and the unfiltered tiles still answer once it is dropped', async () => {
    let filter: {archive: {in: string[]}} | null = null;
    const {client, viewportArtifacts} = fakeClient();
    const {ch} = channel(client, {filters: () => filter});
    ch.setLayer('clusters/x');
    ch.refresh(view, 200, 150);
    await settle();
    filter = {archive: {in: ['cs']}};
    ch.refresh(view, 200, 150);
    await settle();
    expect(asked(viewportArtifacts, 1)).toMatchObject({filters: filter});
    expect(asked(viewportArtifacts, 1).tiles).toHaveLength(4);
    filter = null;
    ch.refresh(view, 200, 150);
    await settle();
    expect(viewportArtifacts).toHaveBeenCalledTimes(2);
  });

  it('asks again for a tile held for some of the layers now asked', async () => {
    const {client, viewportArtifacts} = fakeClient((tile, req) => (req.layers as string[]).map((l) => cluster(tile, {layer: l})));
    const {ch} = channel(client, {declarations: [...FLAT, layer('clusters/y')]});
    ch.setLayer('clusters/x');
    ch.refresh(view, 200, 150);
    await settle();
    ch.setLayers(['clusters/x', 'clusters/y']);
    ch.refresh(view, 200, 150);
    await settle();
    expect(asked(viewportArtifacts, 1)).toMatchObject({layers: ['clusters/x', 'clusters/y']});
    expect(asked(viewportArtifacts, 1).tiles).toHaveLength(4);
    expect(new Set(ch.current.artifacts.map((a) => a.layer))).toEqual(new Set(['clusters/x', 'clusters/y']));
  });

  it('keys a levelled layer’s tiles by level: a zoom that moves the levels asks again', async () => {
    const tiered = layer('admin', {
      hierarchy: {kind: 'tiered', pruneChildren: false},
      levels: [
        {level: 0, title: '', zoom: [0, 3]},
        {level: 1, title: '', zoom: [4, 16]}
      ]
    });
    const {client, viewportArtifacts} = fakeClient((tile, req) => (req.levels as number[]).map((level) => cluster(tile * 10n + BigInt(level), {layer: 'admin', rung: level})));
    const {ch} = channel(client, {declarations: [tiered]});
    ch.setLayer('admin');
    ch.refresh(view, 200, 150);
    await settle();
    expect(asked(viewportArtifacts)).toMatchObject({levels: [0]});
    expect(ch.current.artifacts.every((a) => a.rung === 0)).toBe(true);
    ch.refresh({...view, zoom: 4}, 400, 300);
    await settle();
    expect(asked(viewportArtifacts, 1)).toMatchObject({levels: [1]});
    expect(ch.current.artifacts.every((a) => a.rung === 1)).toBe(true);
  });

  it('evicts the tiles least recently shown past its cap, never the view’s own', async () => {
    const {client} = fakeClient();
    const {ch, table} = channel(client, {heldTiles: 5});
    ch.setLayer('clusters/x');
    ch.refresh(view, 200, 150);
    await settle();
    ch.refresh({target: [82, 50, 0], zoom: 3}, 200, 150);
    await settle();
    expect(ch.heldTiles).toBe(5);
    // The view's four are held, and the fifth is the most recent of the old view's.
    for (const t of [at(4, 2), at(5, 2), at(4, 3), at(5, 3)]) expect(table.ordinalOf('clusters/x', t + 100n)).not.toBe(0);
    expect(table.live).toBe(5);
  });
});

describe('a treed layer', () => {
  const TREED = [layer('tree', {hierarchy: {kind: 'nested', pruneChildren: true}})];

  it('asks for every tile of the view with the budget it was given, and shows the treed frame', async () => {
    const treed = vi.fn(() => [cluster(1n, {layer: 'tree'}), cluster(2n, {layer: 'tree', parentIds: [1n], rung: 1})]);
    const {client, viewportArtifacts} = fakeClient(() => [], undefined, treed);
    const {ch} = channel(client, {declarations: TREED, budget: 48});
    ch.setLayer('tree');
    ch.refresh(view, 200, 150);
    await settle();
    expect(asked(viewportArtifacts)).toMatchObject({layers: ['tree'], budget: 48});
    expect(served(ch)).toEqual([1n, 2n]);
    // Its cut answers one request, so the same view asks again.
    ch.refresh(view, 200, 150);
    await settle();
    expect(viewportArtifacts).toHaveBeenCalledTimes(2);
    expect(asked(viewportArtifacts, 1).tiles).toHaveLength(4);
  });

  it('sends no budget it was not given', async () => {
    const {client, viewportArtifacts} = fakeClient(() => [], undefined, () => [cluster(1n, {layer: 'tree'})]);
    const {ch} = channel(client, {declarations: TREED});
    ch.setLayer('tree');
    ch.refresh(view, 200, 150);
    await settle();
    expect('budget' in asked(viewportArtifacts)).toBe(false);
  });
});

describe('the idle prefetch', () => {
  it('fetches the ring round the view, then the parent depth, and draws neither', async () => {
    const {client, viewportArtifacts} = fakeClient();
    const {ch, clock} = channel(client, {prefetch: true});
    ch.setLayer('clusters/x');
    ch.refresh(view, 200, 150);
    await settle();
    const shown = ch.current.artifacts;
    clock.fire();
    await settle();
    // The ring: x 1..4, y 1..4 less the view's four.
    expect(asked(viewportArtifacts, 1).zoom).toBe(DEPTH);
    expect(asked(viewportArtifacts, 1).tiles).toHaveLength(12);
    clock.fire();
    await settle();
    expect(asked(viewportArtifacts, 2).zoom).toBe(DEPTH - 1);
    expect(asked(viewportArtifacts, 2).tiles).toEqual([mortonOfTile(1, 1, DEPTH - 1)]);
    expect(ch.current.artifacts).toBe(shown);
    // Nothing is left to fetch, so nothing is armed.
    expect(clock.pending).toBe(0);
    // A pan onto the ring draws from what was fetched.
    ch.refresh({target: [66, 50, 0], zoom: 3}, 200, 150);
    await settle();
    expect(viewportArtifacts).toHaveBeenCalledTimes(3);
  });

  it('files none of a streaming prefetch’s remaining frames once the keys move under it', async () => {
    let keys = {identityKey: 'ik', contentKey: 'ck'};
    let channelRef: ArtifactChannel | null = null;
    let calls = 0;
    // A transport that finishes a body whatever the signal says, as one already in the socket does.
    const viewportArtifacts = vi.fn(async (_token: string, req: ViewportArtifactsRequest, opts: {onTile?: TileSink} = {}) => {
      const call = calls++;
      const k = keys;
      for (const [i, tile] of (req.tiles ?? []).entries()) {
        // The prefetch's second frame: a suppression has moved the content key meanwhile.
        if (call === 1 && i === 1) {
          keys = {identityKey: 'ik', contentKey: 'ck2'};
          channelRef!.observeContentKey('ck2');
        }
        await opts.onTile?.({treed: false, tile, artifacts: [cluster(tile + 100n)]}, k);
      }
      return {frames: [], timings: {serverUs: 0, admissionUs: 0, stageNs: null}, ...k, pin: k.contentKey, stale: false, region: null, bytes: 0};
    });
    const client = {viewportArtifacts, artifacts: vi.fn()} as unknown as MosaicaClient;
    const {ch, clock, table} = channel(client, {prefetch: true});
    channelRef = ch;
    ch.setLayer('clusters/x');
    ch.refresh(view, 200, 150);
    await settle();
    clock.fire();
    await settle();
    // The view's four tiles under the new key, asked for again, and nothing of the ring.
    expect(ch.heldTiles).toBe(4);
    expect(table.live).toBe(4);
    for (const t of VIEW_TILES) expect(table.ordinalOf('clusters/x', t + 100n)).not.toBe(0);
  });

  it('never fetches during interaction: a gesture disarms the idle timer', async () => {
    const {client, viewportArtifacts} = fakeClient();
    const {ch, clock} = channel(client, {prefetch: true, settleMs: 200, idleMs: 1500});
    ch.setLayer('clusters/x');
    ch.refresh(view, 200, 150);
    await settle();
    expect(clock.pending).toBe(1);
    ch.schedule(view, 200, 150);
    // The settle timer is armed again and the idle one is gone; firing the settle asks nothing new.
    expect(clock.pending).toBe(1);
    clock.fire();
    await settle();
    expect(viewportArtifacts).toHaveBeenCalledTimes(1);
  });
});

describe('a tag no held tile carries', () => {
  it('is read by identifier once, and the table learns its level, parents and centroid in grid units', async () => {
    const {client, artifacts} = fakeClient(() => []);
    const page = {
      numRows: 1,
      getChild: (name: string) => ({get: () => ({mosaica_id: 77n, level: 2, parents: [5n], centroid_x: 25, centroid_y: 75})[name]})
    };
    artifacts.mockImplementation(async () => ({
      async *[Symbol.asyncIterator]() {
        yield page;
      }
    }));
    const {ch, table} = channel(client);
    ch.setLayer('clusters/x');
    ch.refresh(view, 200, 150);
    await settle();
    // The point path named it, with nothing but its identity.
    const [ordinal] = table.take([{mosaicaId: 77n, layer: 'clusters/x', parentIds: []}]);
    await ch.lookUp([ordinal!]);
    await ch.lookUp([ordinal!]);
    expect(artifacts).toHaveBeenCalledTimes(1);
    expect(artifacts.mock.calls[0]![1]).toEqual({view: 's0', layer: 'clusters/x', ids: [77n], fields: ['level', 'parents', 'centroid']});
    expect(table.entry(ordinal!)).toMatchObject({rung: 2, centroid: [GRID32 / 4, (GRID32 * 3) / 4]});
    // The read takes nothing it does not give back.
    expect(table.live).toBe(1);
  });

  it('is read again after a failed read, once the wait is out', async () => {
    const {client, artifacts} = fakeClient(() => []);
    artifacts.mockRejectedValueOnce(Object.assign(new Error('shed'), {status: 429}));
    artifacts.mockImplementation(async () => ({async *[Symbol.asyncIterator]() {}}));
    const {ch, table, clock} = channel(client);
    ch.setLayer('clusters/x');
    ch.refresh(view, 200, 150);
    await settle();
    const [ordinal] = table.take([{mosaicaId: 77n, layer: 'clusters/x', parentIds: []}]);
    await ch.lookUp([ordinal!]);
    // Within the wait nothing is asked, so a shed read is not repeated at once.
    await ch.lookUp([ordinal!]);
    expect(artifacts).toHaveBeenCalledTimes(1);
    clock.fire();
    await ch.lookUp([ordinal!]);
    expect(artifacts).toHaveBeenCalledTimes(2);
  });

  it('is not read where a held tile carries it', async () => {
    const {client, artifacts} = fakeClient();
    const {ch, table} = channel(client);
    ch.setLayer('clusters/x');
    ch.refresh(view, 200, 150);
    await settle();
    await ch.lookUp([table.ordinalOf('clusters/x', at(2, 2) + 100n)]);
    expect(artifacts).not.toHaveBeenCalled();
  });
});

describe('the declared-map mirror', () => {
  const decl = (over: Partial<Layer>) => layer('admin', {hierarchy: {kind: 'tiered', pruneChildren: true}, ...over});

  it('requestLevels names the union of the levelled layers’ maps at the camera zoom, floored, and nothing for flat ones', () => {
    const admin = decl({
      levels: [
        {level: 0, title: '', zoom: [0, 4]},
        {level: 1, title: '', zoom: [3, 7]},
        {level: 2, title: '', zoom: [6, 10]}
      ]
    });
    const flat = layer('k');
    // A zoom-6.9 view the budget sent to depth 11 still asks for what zoom 6 declares.
    expect(requestLevels([admin, flat], ['admin', 'k'], 6.9)).toEqual([1, 2]);
    expect(requestLevels([admin, flat], ['k'], 6.9)).toBeUndefined();
    expect(requestLevels(new Map([['admin', admin]]), ['admin'], 3)).toEqual([0, 1]);
  });

  it('declaredLevelsAt mirrors the server: no ranges anywhere means every level', () => {
    const levels = decl({levels: [{level: 0, title: 'a', zoom: null}, {level: 1, title: 'b', zoom: null}]});
    expect(declaredLevelsAt(levels, 3)).toEqual([0, 1]);
  });

  it('declaredLevelsAt follows ranges inclusively, and a range-less level answers everywhere', () => {
    const levels = decl({
      levels: [
        {level: 0, title: 'a', zoom: [0, 4]},
        {level: 1, title: 'b', zoom: [5, 16]},
        {level: 2, title: 'c', zoom: null}
      ]
    });
    expect(declaredLevelsAt(levels, 4)).toEqual([0, 2]);
    expect(declaredLevelsAt(levels, 5)).toEqual([1, 2]);
    // A camera framed at zoom 5 that the round trip brought back a hair under it.
    expect(declaredLevelsAt(levels, 4.9999999)).toEqual([1, 2]);
    // Zoomed out past 0: what zoom 0 declares.
    expect(declaredLevelsAt(levels, -0.92)).toEqual([0, 2]);
    expect(declaredLevelsAt(levels, -2)).toEqual([0, 2]);
  });
});


describe('the depth asked', () => {
  const centre = [WORLD_SIZE / 2, WORLD_SIZE / 2, 0] as [number, number, number];

  it('is map zoom + 2, the zoom rounded down, within 0 and 16', async () => {
    const depths: number[] = [];
    for (const zoom of [-1.5, 0, 0.9, 3, 3.7, 7, 13.99, 14, 15.5]) {
      const {client, viewportArtifacts} = fakeClient();
      const {ch} = channel(client);
      ch.setLayer('clusters/x');
      ch.refresh({target: centre, zoom}, 1280, 720);
      await settle();
      depths.push(asked(viewportArtifacts).zoom);
    }
    expect(depths).toEqual([0, 2, 2, 5, 5, 9, 15, 16, 16]);
  });

  it('asks for a 3840 by 2160 screen at zoom + 2, wherever the camera is', async () => {
    const {client, viewportArtifacts} = fakeClient();
    const {ch} = channel(client);
    ch.setLayer('clusters/x');
    for (const [x, y] of [[256, 256], [256.3, 255.9], [100.05, 400.11]] as const) {
      ch.refresh({target: [x, y, 0], zoom: 5}, 3840, 2160);
      await settle();
    }
    for (const call of viewportArtifacts.mock.calls) {
      expect(call[1].zoom).toBe(7);
      expect(call[1].tiles!.length).toBeLessThanOrEqual(ARTIFACT_TILES_PER_REQUEST);
    }
  });

  it('steps a larger screen down to a coarser depth until its tiles fit', async () => {
    const {client, viewportArtifacts} = fakeClient();
    const {ch} = channel(client);
    ch.setLayer('clusters/x');
    // At zoom + 2 and zoom + 1 this screen touches more tiles than the cap; at zoom it does not.
    ch.refresh({target: centre, zoom: 5}, 10_000, 5000);
    await settle();
    expect(asked(viewportArtifacts)).toMatchObject({zoom: 5});
    expect(asked(viewportArtifacts).tiles!.length).toBeLessThanOrEqual(ARTIFACT_TILES_PER_REQUEST);
  });

  it('takes the deployment’s max_tiles_per_request as the cap where it is fewer', async () => {
    const {client, viewportArtifacts} = fakeClient();
    const {ch} = channel(client, {maxTiles: 16});
    ch.setLayer('clusters/x');
    ch.refresh({target: centre, zoom: 5}, 1280, 720);
    await settle();
    expect(asked(viewportArtifacts).zoom).toBe(5);
    expect(asked(viewportArtifacts).tiles!.length).toBeLessThanOrEqual(16);
  });

  it('asks for at most the cap on a zoom out to the world, and nothing at the depth the last view was asked at', async () => {
    const {client, viewportArtifacts} = fakeClient();
    const {ch, clock} = channel(client, {prefetch: true});
    ch.setLayer('clusters/x');
    ch.refresh({target: [300.1, 200.7, 0], zoom: 14}, 3840, 2160);
    await settle();
    ch.refresh({target: centre, zoom: 0}, 3840, 2160);
    await settle();
    // The world at depth 2 is 16 tiles, then the idle ring (none: the view is the world) and the
    // parent depth.
    while (clock.pending > 0) {
      clock.fire();
      await settle();
    }
    const calls = viewportArtifacts.mock.calls.map((c) => c[1]);
    expect(calls[1]).toMatchObject({zoom: 2});
    expect(calls[1]!.tiles).toHaveLength(16);
    expect(calls.slice(2).map((c) => c.zoom)).toEqual([1]);
    for (const c of calls) expect(c.tiles!.length).toBeLessThanOrEqual(ARTIFACT_TILES_PER_REQUEST);
  });

  it('keeps every prefetch within the cap', async () => {
    const {client, viewportArtifacts} = fakeClient();
    const {ch, clock} = channel(client, {prefetch: true, maxTiles: 20});
    ch.setLayer('clusters/x');
    ch.refresh({target: centre, zoom: 6}, 1280, 720);
    await settle();
    for (let i = 0; i < 6 && clock.pending > 0; i++) {
      clock.fire();
      await settle();
    }
    expect(viewportArtifacts.mock.calls.length).toBeGreaterThan(2);
    for (const call of viewportArtifacts.mock.calls) expect(call[1].tiles!.length).toBeLessThanOrEqual(20);
  });

  it('sends nothing for a pan inside the tiles held, and only the new tiles, centre first, for a pan beyond them', async () => {
    const {client, viewportArtifacts} = fakeClient();
    const {ch} = channel(client);
    ch.setLayer('clusters/x');
    ch.refresh(view, 200, 150);
    await settle();
    // Still x 2..3, y 2..3 at depth 5.
    ch.refresh({target: [52, 49, 0], zoom: 3.4}, 200, 150);
    await settle();
    expect(viewportArtifacts).toHaveBeenCalledTimes(1);
    // Two tiles down: y 4..5 is new. The centre is in (3, 5), and (2, 4) is farthest from it.
    ch.refresh({target: [50, 82, 0], zoom: 3}, 200, 150);
    await settle();
    const tiles = asked(viewportArtifacts, 1).tiles!;
    expect([...tiles].sort()).toEqual([at(2, 4), at(3, 4), at(2, 5), at(3, 5)].sort());
    expect(tiles[0]).toBe(at(3, 5));
    expect(tiles[3]).toBe(at(2, 4));
  });

  it('asks at the camera’s own zoom + 2 across a camera script, never at the last view’s depth, and within the cap', async () => {
    const {client, viewportArtifacts} = fakeClient();
    const {ch, clock} = channel(client);
    ch.setLayer('clusters/x');
    const script: [number, number, number][] = [
      [256, 256, 0],
      [200, 180, 3],
      [200, 180, 6],
      [205, 180, 6],
      [200.4, 180.2, 9],
      [200.41, 180.21, 14],
      [256, 256, 0],
      [200, 180, 9],
      [256, 256, 3]
    ];
    for (const [x, y, zoom] of script) {
      ch.schedule({target: [x, y, 0], zoom}, 1280, 720);
      clock.fire();
      await settle();
    }
    expect(viewportArtifacts.mock.calls.map((c) => c[1].zoom)).toEqual([2, 5, 8, 8, 11, 16, 2, 11, 5]);
    for (const call of viewportArtifacts.mock.calls) expect(call[1].tiles!.length).toBeLessThanOrEqual(ARTIFACT_TILES_PER_REQUEST);
  });
});

describe('the drawn cut', () => {
  const TREED = [layer('tree', {hierarchy: {kind: 'nested', pruneChildren: true}})];

  it('is the depth and the tiles the treed frame is asked at, with its budget, and a new budget asks again', async () => {
    const {client, viewportArtifacts} = fakeClient(() => [], undefined, () => [cluster(1n, {layer: 'tree'})]);
    const {ch} = channel(client, {declarations: TREED, budget: 48});
    expect(ch.drawnCut()).toBeNull();
    ch.setLayer('tree');
    ch.refresh(view, 200, 150);
    await settle();
    const cut = ch.drawnCut()!;
    expect(cut.zoom).toBe(asked(viewportArtifacts).zoom);
    expect(cut.budget).toBe(48);
    // The box names exactly the tiles asked for: each one's centre is inside it, and no other's.
    const span = 100 / 2 ** DEPTH;
    const inside = (x: number, y: number) => {
      const [cx, cy] = [(x + 0.5) * span, (y + 0.5) * span];
      return cut.bbox[0] <= cx && cx <= cut.bbox[2] && cut.bbox[1] <= cy && cy <= cut.bbox[3];
    };
    const named: bigint[] = [];
    for (let y = 0; y < 2 ** DEPTH; y++) for (let x = 0; x < 2 ** DEPTH; x++) if (inside(x, y)) named.push(at(x, y));
    const order = (a: bigint, b: bigint) => (a < b ? -1 : a > b ? 1 : 0);
    expect(named.sort(order)).toEqual([...asked(viewportArtifacts).tiles!].sort(order));
    ch.setBudget(12);
    await settle();
    expect(viewportArtifacts).toHaveBeenCalledTimes(2);
    expect(asked(viewportArtifacts, 1).budget).toBe(12);
    expect(ch.drawnCut()!.budget).toBe(12);
    ch.setBudget(undefined);
    await settle();
    expect('budget' in asked(viewportArtifacts, 2)).toBe(false);
    expect('budget' in ch.drawnCut()!).toBe(false);
  });
});
