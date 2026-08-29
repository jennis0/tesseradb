import {describe, expect, it, vi} from 'vitest';
import {
  ArtifactChannel,
  artifactInView,
  declaredLevelsAt,
  requestLevels,
  type ArtifactChannelClock,
  type ArtifactChannelState
} from '../src/artifactChannel.js';
import {SessionArtifactTable} from '../src/artifactTable.js';
import {TesseraClient} from '../src/client.js';
import {rectToRequestBbox} from '../src/coords.js';
import type {Artifact, ArtifactIdentity, FilterExpr, Layer, Quantisation, ViewportRequest, ViewportResponse, ViewportResult} from '../src/types.js';

const Q: Quantisation = {xMin: 0, xMax: 100, yMin: 0, yMax: 100};

const artifact = (id: bigint, parentId: bigint | null = null, matched: boolean | null = null): Artifact => ({
  layer: 'clusters/x',
  tesseraId: id,
  key: `c-${id}`,
  maskedCount: 10n,
  centroid: null,
  box: null,
  shape: null,
  content: [],
  parentId,
  rung: 0,
  matched
});

function responseWith(artifacts: Artifact[], keys?: {identityKey?: string; contentKey?: string}, identity: ArtifactIdentity[] | null = null): ViewportResponse {
  const result: ViewportResult = {
    tiles: [],
    ids: new BigUint64Array(0),
    codes: new BigUint64Array(0),
    positions: new Float64Array(0),
    world: new Float32Array(0),
    scalars: {},
    membership: {},
    subCells: null,
    artifacts,
    artifactsIdentity: identity
  };
  return {
    result,
    timings: {serverUs: 0, admissionUs: 0, stageNs: null},
    identityKey: keys?.identityKey ?? 'ik',
    contentKey: keys?.contentKey ?? 'ck',
    pin: 'ck',
    stale: false,
    bytes: 0
  };
}

function manualClock(): ArtifactChannelClock & {fire(): void; pending: number} {
  const timers = new Map<number, () => void>();
  let seq = 0;
  return {
    after(_ms, fire) {
      const id = ++seq;
      timers.set(id, fire);
      return id;
    },
    cancel(handle) {
      timers.delete(handle as number);
    },
    fire() {
      const fires = [...timers.values()];
      timers.clear();
      for (const f of fires) f();
    },
    get pending() {
      return timers.size;
    }
  };
}

function fakeClient(behaviour: (req: ViewportRequest) => ViewportResponse | Promise<ViewportResponse>) {
  const viewport = vi.fn(async (_token: string, req: ViewportRequest) => behaviour(req));
  return {client: {viewport} as unknown as TesseraClient, viewport};
}

const view = {target: [50, 50, 0] as [number, number, number], zoom: 4};

function channel(client: TesseraClient, clock: ArtifactChannelClock, table?: SessionArtifactTable) {
  const states: ArtifactChannelState[] = [];
  const ch = new ArtifactChannel(client, {
    view: 's0',
    quantisation: Q,
    token: () => 'tok',
    depth: () => 5, // a drawn frame exists
    clock,
    table,
    onChange: (s) => states.push(s)
  });
  return {ch, states};
}

describe('the artifact channel', () => {
  it('debounces: a request goes out once the view settles, not per schedule', async () => {
    const clock = manualClock();
    const {client, viewport} = fakeClient(() => responseWith([artifact(1n)]));
    const {ch} = channel(client, clock);
    ch.setLayer('clusters/x');
    ch.schedule(view, 400, 300);
    ch.schedule(view, 400, 300);
    ch.schedule(view, 400, 300);
    expect(viewport).not.toHaveBeenCalled(); // still settling
    expect(clock.pending).toBe(1); // one timer, the latest — the earlier ones were cancelled
    clock.fire();
    await Promise.resolve();
    expect(viewport).toHaveBeenCalledTimes(1);
  });

  it('asks with k = 0 and exactly the on layer named — the artifact channel’s own request shape', async () => {
    const clock = manualClock();
    const {client, viewport} = fakeClient(() => responseWith([artifact(1n)]));
    const {ch} = channel(client, clock);
    ch.setLayer('clusters/x');
    ch.refresh(view, 400, 300);
    await Promise.resolve();
    const body = viewport.mock.calls[0]![1] as {k?: number; layers?: string[]};
    expect(body.k).toBe(0);
    expect(body.layers).toEqual(['clusters/x']);
  });

  it('clears the held set on a refusal — a refusal is not an empty view', async () => {
    const clock = manualClock();
    let fail = false;
    const {client} = fakeClient(() => {
      if (fail) throw Object.assign(new Error('gone'), {code: 'refused', detail: 'gone'});
      return responseWith([artifact(1n), artifact(2n)]);
    });
    const {ch, states} = channel(client, clock);
    ch.setLayer('clusters/x');
    ch.refresh(view, 400, 300);
    await Promise.resolve();
    await Promise.resolve();
    expect(ch.current.artifacts).toHaveLength(2);

    fail = true;
    ch.refresh(view, 400, 300);
    await Promise.resolve();
    await Promise.resolve();
    expect(ch.current.status).toBe('refused');
    expect(ch.current.artifacts).toHaveLength(0);
    expect(states.at(-1)!.refusal?.code).toBe('refused');
  });

  it('replaces the served set wholesale and holds the payloads beside it', async () => {
    const clock = manualClock();
    const table = new SessionArtifactTable();
    let served = [artifact(1n), artifact(2n)];
    const {client} = fakeClient(() => responseWith(served));
    const {ch} = channel(client, clock, table);
    ch.setLayer('clusters/x');
    ch.refresh(view, 400, 300);
    await Promise.resolve();
    await Promise.resolve();
    expect(table.live).toBe(2);
    const ordinalOfOne = table.ordinalOf('clusters/x', 1n);

    // Panned onto disjoint ground. **The served set is only what is in view** — merging it would
    // draw clusters for ground the user has left — and the payloads of what was left are held.
    served = [artifact(3n)];
    ch.refresh(view, 400, 300);
    await Promise.resolve();
    await Promise.resolve();
    expect(ch.current.artifacts.map((a) => a.tesseraId)).toEqual([3n]);
    expect(ch.current.held).toBe(3);
    expect(table.live).toBe(3);
    // And the ordinal an artifact was named under survives the pan, which is what stops the
    // colours and the lookup texture being rebuilt for ground already seen.
    expect(table.ordinalOf('clusters/x', 1n)).toBe(ordinalOfOne);
  });

  it('names a held artifact once — a pan back to it moves nothing in the session table', async () => {
    const clock = manualClock();
    const table = new SessionArtifactTable();
    let served = [artifact(1n), artifact(2n)];
    const {client} = fakeClient(() => responseWith(served));
    const {ch} = channel(client, clock, table);
    ch.setLayer('clusters/x');
    ch.refresh(view, 400, 300);
    await Promise.resolve();
    await Promise.resolve();
    const first = ch.current.artifacts[0]!;

    served = [artifact(3n)];
    ch.refresh(view, 400, 300);
    await Promise.resolve();
    await Promise.resolve();
    const settled = table.version;

    // Back over the original ground: the table does not move, so nothing downstream of its version
    // — the colour map, the lookup texture — is rebuilt.
    served = [artifact(1n), artifact(2n)];
    ch.refresh(view, 400, 300);
    await Promise.resolve();
    await Promise.resolve();
    expect(table.version).toBe(settled);
    // The payload is the *same object*, so a consumer memoising on identity does no work either.
    expect(ch.current.artifacts[0]).toBe(first);
  });

  it('drops the store when the content key rotates, and when the identity key does', async () => {
    for (const rotate of [{contentKey: 'ck2'}, {identityKey: 'ik2'}]) {
      const clock = manualClock();
      const table = new SessionArtifactTable();
      let served = [artifact(1n), artifact(2n)];
      let keys: {identityKey?: string; contentKey?: string} | undefined;
      const {client} = fakeClient(() => responseWith(served, keys));
      const {ch} = channel(client, clock, table);
      ch.setLayer('clusters/x');
      ch.refresh(view, 400, 300);
      await Promise.resolve();
      await Promise.resolve();
      expect(ch.current.held).toBe(2);

      // What is held answered a question about a generation, or a principal, that has moved.
      keys = rotate;
      served = [artifact(3n)];
      ch.refresh(view, 400, 300);
      await Promise.resolve();
      await Promise.resolve();
      expect(ch.current.held).toBe(1);
      expect(table.live).toBe(1);
      expect(table.ordinalOf('clusters/x', 1n)).toBe(0);
    }
  });

  it('takes the filter bit from the response and never from what it holds', async () => {
    const clock = manualClock();
    const table = new SessionArtifactTable();
    let served = [artifact(1n, null, true), artifact(2n, null, false)];
    const {client} = fakeClient(() => responseWith(served));
    const {ch} = channel(client, clock, table);
    ch.setLayer('clusters/x');
    ch.refresh(view, 400, 300);
    await Promise.resolve();
    await Promise.resolve();
    expect(ch.current.artifacts.map((a) => a.matched)).toEqual([true, false]);

    // The filter changed; the payloads did not (decision 0104), and the bit is the one thing that
    // must not be answered from the store.
    served = [artifact(1n, null, false), artifact(2n, null, true)];
    ch.refresh(view, 400, 300);
    await Promise.resolve();
    await Promise.resolve();
    expect(ch.current.artifacts.map((a) => a.matched)).toEqual([false, true]);
    expect(ch.current.held).toBe(2);
    expect(table.version).toBeGreaterThan(0);
    // Nothing was renamed: a filter change costs no ordinal and no colour.
    expect(table.live).toBe(2);
  });

  it('keeps the store when a layer is switched off — a question not asked is not an answer gone stale', async () => {
    const clock = manualClock();
    const table = new SessionArtifactTable();
    const {client} = fakeClient(() => responseWith([artifact(1n), artifact(2n)]));
    const {ch} = channel(client, clock, table);
    ch.setLayer('clusters/x');
    ch.refresh(view, 400, 300);
    await Promise.resolve();
    await Promise.resolve();

    ch.setLayer(null);
    ch.refresh(view, 400, 300);
    await Promise.resolve();
    await Promise.resolve();
    expect(ch.current.artifacts).toHaveLength(0);
    expect(ch.current.held).toBe(2);

    // A reset is the other half of rule 7: a new principal, and nothing held may be named again.
    ch.reset();
    expect(ch.current.held).toBe(0);
    expect(table.live).toBe(0);
  });

  it('does not ask, and clears, when no layer is selected', async () => {
    const clock = manualClock();
    const {client, viewport} = fakeClient(() => responseWith([artifact(1n)]));
    const {ch} = channel(client, clock);
    ch.setLayer(null);
    ch.refresh(view, 400, 300);
    await Promise.resolve();
    expect(viewport).not.toHaveBeenCalled();
    expect(ch.current.artifacts).toHaveLength(0);
  });
});

// ---------------------------------------------------------------- the fetch model (design §6)

/** One depth-5 tile in wire grid units: the world is 2^32 per axis, 32 tiles across at depth 5. */
const S = 2 ** 27;

/** The partial view: bbox [37.5, 40.625, 62.5, 59.375] world units → tiles 2..3 × 2..3 at depth 5. */
const partialView = {target: [50, 50, 0] as [number, number, number], zoom: 4};
/** The whole-extent view: bbox [0, 0, 512, 512] — every tile at any depth. */
const wholeView = {target: [256, 256, 0] as [number, number, number], zoom: 0};

const geo = (
  id: bigint,
  g: {
    box?: [number, number, number, number];
    centroid?: [number, number];
    rung?: number;
    layer?: string;
    matched?: boolean | null;
  } = {}
): Artifact => ({
  layer: g.layer ?? 'clusters/x',
  tesseraId: id,
  key: `c-${id}`,
  maskedCount: 10n,
  centroid: g.centroid ?? null,
  box: g.box ?? null,
  shape: null,
  content: [],
  parentId: null,
  rung: g.rung ?? 0,
  matched: g.matched ?? null
});

// The partial view's grid box is [2S, 2S, 4S, 4S]. Four fixtures against it:
/** Wholly inside the viewport. */
const inside = () => geo(1n, {box: [3 * S, 3 * S, 3.5 * S, 3.5 * S], centroid: [3.2 * S, 3.2 * S]});
/** Wholly outside it. */
const outside = () => geo(2n, {box: [6 * S, 6 * S, 7 * S, 7 * S], centroid: [6.5 * S, 6.5 * S]});
/** Its edge crosses the viewport while its centroid sits outside — it is drawn, by its box. */
const edge = () => geo(3n, {box: [0.5 * S, 0.5 * S, 2.5 * S, 2.5 * S], centroid: [S, S]});
/** No geometry at all — always in view. */
const bare = () => geo(4n);

const decl = (over: Partial<Layer> = {}): Layer => ({
  name: 'clusters/x',
  title: 'X',
  views: ['s0'],
  membership: 'enumerated',
  hierarchy: {kind: 'tiered', pruneChildren: false},
  levels: [{level: 0, title: 'coarse', zoom: null}],
  computedContent: [],
  suppliedContent: [],
  depsOn: [],
  version: 1,
  ...over
});

function holdingChannel(
  client: TesseraClient,
  clock: ArtifactChannelClock,
  opts: {
    declarations?: Layer[];
    filters?: () => FilterExpr | null;
    table?: SessionArtifactTable;
  } = {}
) {
  const states: ArtifactChannelState[] = [];
  const ch = new ArtifactChannel(client, {
    view: 's0',
    quantisation: Q,
    token: () => 'tok',
    depth: () => 5,
    clock,
    onChange: (s) => states.push(s),
    ...opts
  });
  return {ch, states};
}

/** Enough microtask turns for a mocked response to land and its state to settle. */
async function settled(): Promise<void> {
  for (let i = 0; i < 4; i++) await Promise.resolve();
}

describe('held-whole tracking and the local serve', () => {
  it('marks a levelled scope on a whole-extent unfiltered response, then serves views locally', async () => {
    const clock = manualClock();
    const served = [inside(), outside(), edge(), bare()];
    const {client, viewport} = fakeClient(() => responseWith(served));
    const {ch} = holdingChannel(client, clock, {declarations: [decl()]});
    ch.setLayer('clusters/x');
    ch.refresh(wholeView, 512, 512);
    await settled();
    expect(viewport).toHaveBeenCalledTimes(1);
    expect(ch.isHeldWhole('clusters/x', 0)).toBe(true);
    // Every scope the view touches is held whole, so there is nothing left to promote.
    expect(clock.pending).toBe(0);

    ch.refresh(partialView, 400, 300);
    await settled();
    // No request: the pick is local, and it is the set the server would have named — the artifact
    // whose edge crosses the viewport with its centroid outside is drawn, the bare one always is.
    expect(viewport).toHaveBeenCalledTimes(1);
    expect(ch.current.status).toBe('shown');
    expect(ch.current.artifacts.map((a) => a.tesseraId)).toEqual([1n, 3n, 4n]);
    expect(ch.current.artifacts.every((a) => a.matched === null)).toBe(true);
  });

  it('serves locally the same set a server answer to the same view carried', async () => {
    const clock = manualClock();
    let served = [inside(), edge(), bare()]; // what the server serves for the partial view
    const {client, viewport} = fakeClient(() => responseWith(served));
    const {ch} = holdingChannel(client, clock, {declarations: [decl()]});
    ch.setLayer('clusters/x');
    ch.refresh(partialView, 400, 300);
    await settled();
    const fromServer = ch.current.artifacts.map((a) => a.tesseraId);

    served = [inside(), outside(), edge(), bare()]; // the whole extent
    ch.refresh(wholeView, 512, 512);
    await settled();
    const asked = viewport.mock.calls.length;

    ch.refresh(partialView, 400, 300);
    await settled();
    expect(viewport.mock.calls.length).toBe(asked);
    expect(ch.current.artifacts.map((a) => a.tesseraId)).toEqual(fromServer);
  });

  it('marks a flat layer as its single scope and serves it locally', async () => {
    const clock = manualClock();
    const {client, viewport} = fakeClient(() => responseWith([inside(), outside(), bare()]));
    const flat = decl({hierarchy: {kind: 'flat', pruneChildren: false}, levels: []});
    const {ch} = holdingChannel(client, clock, {declarations: [flat]});
    ch.setLayer('clusters/x');
    ch.refresh(wholeView, 512, 512);
    await settled();
    expect(ch.isHeldWhole('clusters/x', 0)).toBe(true);

    ch.refresh(partialView, 400, 300);
    await settled();
    expect(viewport).toHaveBeenCalledTimes(1);
    expect(ch.current.artifacts.map((a) => a.tesseraId)).toEqual([1n, 4n]);
  });

  it('does not mark on a partial response', async () => {
    const clock = manualClock();
    const {client, viewport} = fakeClient(() => responseWith([inside()]));
    const {ch} = holdingChannel(client, clock, {declarations: [decl()]});
    ch.setLayer('clusters/x');
    ch.refresh(partialView, 400, 300);
    await settled();
    expect(ch.isHeldWhole('clusters/x', 0)).toBe(false);

    ch.refresh(partialView, 400, 300);
    await settled();
    expect(viewport).toHaveBeenCalledTimes(2); // still asking per view
  });

  it('never marks a treed layer, even on a whole-extent response', async () => {
    const clock = manualClock();
    const {client, viewport} = fakeClient(() => responseWith([inside(), edge()]));
    const treed = decl({hierarchy: {kind: 'nested', pruneChildren: true}, levels: []});
    const {ch} = holdingChannel(client, clock, {declarations: [treed]});
    ch.setLayer('clusters/x');
    ch.refresh(wholeView, 512, 512);
    await settled();
    expect(ch.isHeldWhole('clusters/x', 0)).toBe(false);
    expect(clock.pending).toBe(0); // and it is never a promotion candidate either

    ch.refresh(partialView, 400, 300);
    await settled();
    expect(viewport).toHaveBeenCalledTimes(2); // per view always
  });

  it('a layer with no declaration is never served locally', async () => {
    const clock = manualClock();
    const {client, viewport} = fakeClient(() => responseWith([inside()]));
    const {ch} = holdingChannel(client, clock, {declarations: []});
    ch.setLayer('clusters/x');
    ch.refresh(wholeView, 512, 512);
    await settled();
    ch.refresh(partialView, 400, 300);
    await settled();
    expect(viewport).toHaveBeenCalledTimes(2);
  });

  it('a filter always asks the server, whatever is held; dropping it serves locally again', async () => {
    const clock = manualClock();
    let filter: FilterExpr | null = null;
    const {client, viewport} = fakeClient(() => responseWith([inside(), outside(), edge(), bare()]));
    const {ch} = holdingChannel(client, clock, {declarations: [decl()], filters: () => filter});
    ch.setLayer('clusters/x');
    ch.refresh(wholeView, 512, 512);
    await settled();
    expect(ch.isHeldWhole('clusters/x', 0)).toBe(true);

    filter = {archive: {eq: 'x'}};
    ch.refresh(partialView, 400, 300);
    await settled();
    expect(viewport).toHaveBeenCalledTimes(2);
    expect((viewport.mock.calls.at(-1)![1] as {filters?: FilterExpr}).filters).toEqual({archive: {eq: 'x'}});

    filter = null;
    ch.refresh(partialView, 400, 300);
    await settled();
    expect(viewport).toHaveBeenCalledTimes(2); // the unfiltered question is answered from the hold
  });

  it('drops the marks with the store when a response rotates the content key', async () => {
    const clock = manualClock();
    let filter: FilterExpr | null = null;
    let keys: {contentKey?: string} | undefined;
    const {client, viewport} = fakeClient(() => responseWith([inside(), outside(), edge(), bare()], keys));
    const {ch} = holdingChannel(client, clock, {declarations: [decl()], filters: () => filter});
    ch.setLayer('clusters/x');
    ch.refresh(wholeView, 512, 512);
    await settled();
    expect(ch.isHeldWhole('clusters/x', 0)).toBe(true);

    // A filtered ask reaches the network and comes back under a new generation: rule 7 drops the
    // store, and a mark must never outlive it.
    filter = {archive: {eq: 'x'}};
    keys = {contentKey: 'ck2'};
    ch.refresh(partialView, 400, 300);
    await settled();
    expect(ch.isHeldWhole('clusters/x', 0)).toBe(false);

    filter = null;
    ch.refresh(partialView, 400, 300);
    await settled();
    expect(viewport).toHaveBeenCalledTimes(3); // back to the network — nothing is held whole
  });

  it('drops the marks when another channel observes a rotation, and re-asks for the noted view', async () => {
    const clock = manualClock();
    const {client, viewport} = fakeClient(() => responseWith([inside(), outside(), edge(), bare()]));
    const {ch} = holdingChannel(client, clock, {declarations: [decl()]});
    ch.setLayer('clusters/x');
    ch.refresh(wholeView, 512, 512);
    await settled();
    ch.refresh(partialView, 400, 300);
    await settled();
    expect(viewport).toHaveBeenCalledTimes(1); // the partial view was local

    ch.observeContentKey('ck2'); // the point path saw the content move
    await settled();
    expect(ch.isHeldWhole('clusters/x', 0)).toBe(false);
    expect(viewport).toHaveBeenCalledTimes(2); // the drawn view was re-asked, not left stale

    ch.observeContentKey('ck'); // the store's own key is not a rotation
    await settled();
    expect(viewport).toHaveBeenCalledTimes(2);
  });

  it('drops the marks on reset', async () => {
    const clock = manualClock();
    const {client} = fakeClient(() => responseWith([inside()]));
    const {ch} = holdingChannel(client, clock, {declarations: [decl()]});
    ch.setLayer('clusters/x');
    ch.refresh(wholeView, 512, 512);
    await settled();
    expect(ch.isHeldWhole('clusters/x', 0)).toBe(true);
    ch.reset();
    expect(ch.isHeldWhole('clusters/x', 0)).toBe(false);
  });
});

describe('the idle promotion ratchet', () => {
  it('promotes a served, un-held level whole in idle time, then serves locally', async () => {
    const clock = manualClock();
    let served = [inside()];
    const {client, viewport} = fakeClient(() => responseWith(served));
    const {ch} = holdingChannel(client, clock, {declarations: [decl()]});
    ch.setLayer('clusters/x');
    ch.refresh(partialView, 400, 300);
    await settled();
    expect(clock.pending).toBe(1); // the idle timer is armed, nothing has been fetched

    served = [inside(), outside(), edge(), bare()];
    clock.fire();
    await settled();
    expect(viewport).toHaveBeenCalledTimes(2);
    const req = viewport.mock.calls.at(-1)![1] as Record<string, unknown>;
    // The whole-extent bbox, the level named, no budget and no filter — the promotion's shape.
    expect(req.bbox).toEqual(rectToRequestBbox({x0: 0, y0: 0, x1: 0, y1: 0}, 0, Q));
    expect(req.zoom).toBe(0);
    expect(req.k).toBe(0);
    expect(req.layers).toEqual(['clusters/x']);
    expect(req.levels).toEqual([0]);
    expect(req.artifactBudget).toBeUndefined();
    expect(req.filters).toBeUndefined();
    expect(ch.isHeldWhole('clusters/x', 0)).toBe(true);
    // The served set never moves on a promotion — it fed the store and the marks alone.
    expect(ch.current.artifacts.map((a) => a.tesseraId)).toEqual([1n]);

    ch.refresh(partialView, 400, 300);
    await settled();
    expect(viewport).toHaveBeenCalledTimes(2); // local from here
  });

  it('promotes a flat layer without naming levels', async () => {
    const clock = manualClock();
    let served = [inside()];
    const {client, viewport} = fakeClient(() => responseWith(served));
    const flat = decl({hierarchy: {kind: 'flat', pruneChildren: false}, levels: []});
    const {ch} = holdingChannel(client, clock, {declarations: [flat]});
    ch.setLayer('clusters/x');
    ch.refresh(partialView, 400, 300);
    await settled();

    served = [inside(), outside(), bare()];
    clock.fire();
    await settled();
    expect(viewport).toHaveBeenCalledTimes(2);
    const req = viewport.mock.calls.at(-1)![1] as Record<string, unknown>;
    expect(req.levels).toBeUndefined(); // inert on a flat layer, so it is not named
    expect(ch.isHeldWhole('clusters/x', 0)).toBe(true);
  });

  it('never promotes during interaction — a gesture disarms the idle timer', async () => {
    const clock = manualClock();
    const {client, viewport} = fakeClient(() => responseWith([inside()]));
    const {ch} = holdingChannel(client, clock, {declarations: [decl()]});
    ch.setLayer('clusters/x');
    ch.refresh(partialView, 400, 300);
    await settled();
    expect(clock.pending).toBe(1); // the promotion, armed

    ch.schedule(partialView, 400, 300); // the user moves
    expect(clock.pending).toBe(1); // the settle timer alone — the promotion was disarmed
    clock.fire();
    await settled();
    // One per-view request and no promotion: had both timers been live, this would be 3.
    expect(viewport).toHaveBeenCalledTimes(2);
    const req = viewport.mock.calls.at(-1)![1] as Record<string, unknown>;
    expect(req.artifactBudget).toBeDefined(); // the per-view shape, not the promotion's

    await settled();
    expect(clock.pending).toBe(1); // and the ratchet re-arms once the view is served
  });
});

describe('the identity projection over a held scope (protocol §5.2)', () => {
  /** The rows the server answers a filtered view with, in the identity projection. */
  const identity = (rows: {id: bigint; matched: boolean | null; rung?: number}[]): ArtifactIdentity[] =>
    rows.map((r) => ({layer: 'clusters/x', tesseraId: r.id, rung: r.rung ?? 0, matched: r.matched}));

  /** A client that answers an identity ask with identity rows and a full ask with full rows. */
  function server(full: () => Artifact[], rows: () => ArtifactIdentity[], keys?: {identityKey?: string; contentKey?: string}) {
    return fakeClient((req) => (req.artifactRows === 'identity' ? responseWith([], keys, rows()) : responseWith(full(), keys)));
  }

  it('asks for identity rows when a filter lands over scopes all held whole, and dresses the held payloads in the response’s bits', async () => {
    const clock = manualClock();
    let filter: FilterExpr | null = null;
    const {client, viewport} = server(
      () => [inside(), outside(), edge(), bare()],
      () => identity([{id: 1n, matched: true}, {id: 3n, matched: false}, {id: 4n, matched: true}])
    );
    const {ch} = holdingChannel(client, clock, {declarations: [decl()], filters: () => filter});
    ch.setLayer('clusters/x');
    ch.refresh(wholeView, 512, 512);
    await settled();
    expect(ch.isHeldWhole('clusters/x', 0)).toBe(true);
    const heldOne = ch.current.artifacts.find((a) => a.tesseraId === 1n)!;

    filter = {archive: {eq: 'x'}};
    ch.refresh(partialView, 400, 300);
    await settled();
    expect(viewport).toHaveBeenCalledTimes(2);
    const req = viewport.mock.calls.at(-1)![1] as ViewportRequest;
    expect(req.artifactRows).toBe('identity');
    expect(req.filters).toEqual({archive: {eq: 'x'}});
    // The served set is the response's rows, wholesale — not the local pick, not the whole hold.
    expect(ch.current.artifacts.map((a) => a.tesseraId)).toEqual([1n, 3n, 4n]);
    // The bit is the response's; the payload is the store's.
    expect(ch.current.artifacts.map((a) => a.matched)).toEqual([true, false, true]);
    expect(ch.current.artifacts[0]!.key).toBe(heldOne.key);
    expect(ch.current.artifacts[0]!.box).toEqual(heldOne.box);
    // Nothing entered the store — an identity row carries no payload to hold.
    expect(ch.current.held).toBe(4);
  });

  it('takes the rung from the response as well as the bit', async () => {
    const clock = manualClock();
    let filter: FilterExpr | null = null;
    const {client} = server(() => [inside(), bare()], () => identity([{id: 1n, matched: true, rung: 0}, {id: 4n, matched: null, rung: 0}]));
    const {ch} = holdingChannel(client, clock, {declarations: [decl()], filters: () => filter});
    ch.setLayer('clusters/x');
    ch.refresh(wholeView, 512, 512);
    await settled();
    const held = ch.current.artifacts;

    filter = {archive: {eq: 'x'}};
    ch.refresh(partialView, 400, 300);
    await settled();
    // A row whose bit and rung agree with the held payload IS the held object — a consumer
    // memoising on identity does no work for it; a row whose bit moved is a copy wearing it.
    expect(ch.current.artifacts[1]).toBe(held[1]);
    expect(ch.current.artifacts[0]).not.toBe(held[0]);
    expect(ch.current.artifacts[0]!.matched).toBe(true);
    expect(ch.current.artifacts[0]!.rung).toBe(0);
  });

  it('re-asks once with full rows when an identity row cannot be resolved — one round trip, never a wrong map', async () => {
    const clock = manualClock();
    let filter: FilterExpr | null = null;
    let full = [inside(), bare()];
    const {client, viewport} = server(
      () => full,
      // The server names an artifact the store never held: a payload the hold did not cover.
      () => identity([{id: 1n, matched: true}, {id: 9n, matched: true}])
    );
    const {ch} = holdingChannel(client, clock, {declarations: [decl()], filters: () => filter});
    ch.setLayer('clusters/x');
    ch.refresh(wholeView, 512, 512);
    await settled();
    expect(viewport).toHaveBeenCalledTimes(1);

    filter = {archive: {eq: 'x'}};
    full = [geo(1n, {box: inside().box!, matched: true}), geo(9n, {box: inside().box!, matched: true})];
    ch.refresh(partialView, 400, 300);
    await settled();
    // Two asks for the one view: the identity one, then the full one it fell back to.
    expect(viewport).toHaveBeenCalledTimes(3);
    const asks = viewport.mock.calls.slice(1).map((c) => (c[1] as ViewportRequest).artifactRows);
    expect(asks).toEqual(['identity', undefined]);
    expect((viewport.mock.calls.at(-1)![1] as ViewportRequest).filters).toEqual({archive: {eq: 'x'}});
    // The map is the full answer's, and the payload it carried is now held.
    expect(ch.current.artifacts.map((a) => a.tesseraId)).toEqual([1n, 9n]);
    expect(ch.current.artifacts.every((a) => a.matched === true)).toBe(true);
    expect(ch.current.held).toBe(3);
  });

  it('re-asks with full rows when the identity answer comes under keys the store was not filled under', async () => {
    const clock = manualClock();
    let filter: FilterExpr | null = null;
    let keys: {contentKey?: string} | undefined;
    // The identity answer arrives under a rotated content key; the full ask that follows does too.
    const rotating = fakeClient((req) => (req.artifactRows === 'identity' ? responseWith([], {contentKey: 'ck2'}, identity([{id: 1n, matched: true}])) : responseWith([inside(), bare()], keys)));
    const {ch} = holdingChannel(rotating.client, clock, {declarations: [decl()], filters: () => filter});
    ch.setLayer('clusters/x');
    ch.refresh(wholeView, 512, 512);
    await settled();

    filter = {archive: {eq: 'x'}};
    keys = {contentKey: 'ck2'};
    ch.refresh(partialView, 400, 300);
    await settled();
    // A held payload under a rotated key is another generation's answer: the identity rows are
    // not dressed in it, the full ask follows, and rule 7 rotates the store on its answer.
    expect(rotating.viewport).toHaveBeenCalledTimes(3);
    expect(ch.current.artifacts.map((a) => a.tesseraId)).toEqual([1n, 4n]);
    expect(ch.isHeldWhole('clusters/x', 0)).toBe(false);
  });

  it('never asks for identity rows where a scope is not held whole, and a filtered response never marks one', async () => {
    const clock = manualClock();
    let filter: FilterExpr | null = {archive: {eq: 'x'}};
    const {client, viewport} = server(() => [inside(), outside(), edge(), bare()], () => identity([]));
    const {ch} = holdingChannel(client, clock, {declarations: [decl()], filters: () => filter});
    ch.setLayer('clusters/x');
    // A whole-extent view, filtered from the start: full rows are asked for, and the answer —
    // whole extent or not — marks nothing, its row set having answered a narrower question.
    ch.refresh(wholeView, 512, 512);
    await settled();
    expect((viewport.mock.calls[0]![1] as ViewportRequest).artifactRows).toBeUndefined();
    expect(ch.isHeldWhole('clusters/x', 0)).toBe(false);

    ch.refresh(partialView, 400, 300);
    await settled();
    expect((viewport.mock.calls.at(-1)![1] as ViewportRequest).artifactRows).toBeUndefined();

    // Dropping the filter still goes to the network: nothing was ever held whole.
    filter = null;
    ch.refresh(partialView, 400, 300);
    await settled();
    expect(viewport).toHaveBeenCalledTimes(3);
  });
});

describe('the declared-map mirror and the in-view test', () => {
  it('requestLevels names the union of the levelled layers’ maps at the camera zoom, floored, and nothing for flat ones', () => {
    const admin = {name: 'admin', hierarchy: {kind: 'tiered', pruneChildren: true}, levels: [{level: 0, title: '', zoom: [0, 4] as [number, number]}, {level: 1, title: '', zoom: [3, 7] as [number, number]}, {level: 2, title: '', zoom: [6, 10] as [number, number]}]} as never;
    const flat = {name: 'k', hierarchy: {kind: 'flat', pruneChildren: false}, levels: []} as never;
    // A zoom-6.9 view the budget sent to depth 11 still asks for what zoom 6 declares.
    expect(requestLevels([admin, flat], ['admin', 'k'], 6.9)).toEqual([1, 2]);
    expect(requestLevels([admin, flat], ['k'], 6.9)).toBeUndefined();
    expect(requestLevels(new Map([['admin', admin]]), ['admin'], 3)).toEqual([0, 1]);
  });

  it('declaredLevelsAt mirrors the server: no ranges anywhere means every level', () => {
    const layer = decl({
      levels: [
        {level: 0, title: 'a', zoom: null},
        {level: 1, title: 'b', zoom: null}
      ]
    });
    expect(declaredLevelsAt(layer, 3)).toEqual([0, 1]);
  });

  it('declaredLevelsAt follows ranges inclusively, and a range-less level answers everywhere', () => {
    const layer = decl({
      levels: [
        {level: 0, title: 'a', zoom: [0, 4]},
        {level: 1, title: 'b', zoom: [5, 16]},
        {level: 2, title: 'c', zoom: null}
      ]
    });
    expect(declaredLevelsAt(layer, 4)).toEqual([0, 2]);
    expect(declaredLevelsAt(layer, 5)).toEqual([1, 2]);
  });

  it('artifactInView: box beats centroid, and no geometry is always in view', () => {
    const view = {x0: 2 * S, y0: 2 * S, x1: 4 * S, y1: 4 * S};
    expect(artifactInView(inside(), view)).toBe(true);
    expect(artifactInView(outside(), view)).toBe(false);
    expect(artifactInView(edge(), view)).toBe(true); // the box crosses; the centroid is outside
    expect(artifactInView(bare(), view)).toBe(true);
    expect(artifactInView({box: null, centroid: [3 * S, 3 * S]}, view)).toBe(true);
    expect(artifactInView({box: null, centroid: [S, S]}, view)).toBe(false);
  });
});

describe('the depth clamp', () => {
  it('asks no deeper than max_tiles_per_request allows for the view it was handed', async () => {
    const clock = manualClock();
    const {client, viewport} = fakeClient(() => responseWith([artifact(1n)]));
    const states: ArtifactChannelState[] = [];
    const ch = new ArtifactChannel(client, {
      view: 's0',
      quantisation: Q,
      token: () => 'tok',
      // A frame drawn at depth 10 while the camera sits at the full extent — the pairing the
      // harness produced on a principal switch, which asked for 2^20 tiles and was refused (422).
      depth: () => 10,
      maxTiles: 4096,
      clock,
      onChange: (s) => states.push(s)
    });
    ch.setLayer('clusters/x');
    ch.refresh({target: [256, 256, 0], zoom: 0}, 512, 512);
    await Promise.resolve();
    expect(viewport).toHaveBeenCalledTimes(1);
    const req = viewport.mock.calls[0]![1] as {zoom: number};
    // 4^6 = 4096 tiles over the whole world fits; 4^7 does not.
    expect(req.zoom).toBe(6);
  });
});
