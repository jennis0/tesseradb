import {describe, expect, it, vi} from 'vitest';
import {ArtifactChannel, type ArtifactChannelClock, type ArtifactChannelState} from '../src/artifactChannel.js';
import {SessionArtifactTable} from '../src/artifactTable.js';
import {TesseraClient} from '../src/client.js';
import type {Artifact, Quantisation, ViewportResponse, ViewportResult} from '../src/types.js';

const Q: Quantisation = {xMin: 0, xMax: 100, yMin: 0, yMax: 100};

const artifact = (id: bigint, parentId: bigint | null = null, matched: boolean | null = null): Artifact => ({
  layer: 'clusters/x',
  tesseraId: id,
  key: `c-${id}`,
  maskedCount: 10n,
  centroid: null,
  box: null,
  hull: null,
  content: [],
  parentId,
  level: 0,
  matched
});

function responseWith(artifacts: Artifact[], keys?: {identityKey?: string; contentKey?: string}): ViewportResponse {
  const result: ViewportResult = {
    tiles: [],
    ids: new BigUint64Array(0),
    codes: new BigUint64Array(0),
    positions: new Float64Array(0),
    world: new Float32Array(0),
    scalars: {},
    membership: {},
    subCells: null,
    artifacts
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

function fakeClient(behaviour: () => ViewportResponse | Promise<ViewportResponse>) {
  const viewport = vi.fn(async () => behaviour());
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

  it('evicts the least recently served first, and never what is on screen', async () => {
    const clock = manualClock();
    const table = new SessionArtifactTable();
    let served = [artifact(1n), artifact(2n)];
    const {client} = fakeClient(() => responseWith(served));
    const states: ArtifactChannelState[] = [];
    const ch = new ArtifactChannel(client, {
      view: 's0',
      quantisation: Q,
      token: () => 'tok',
      depth: () => 5,
      clock,
      table,
      heldMax: 3,
      onChange: (s) => states.push(s)
    });
    ch.setLayer('clusters/x');
    ch.refresh(view, 400, 300);
    await Promise.resolve();
    await Promise.resolve();

    served = [artifact(3n)];
    ch.refresh(view, 400, 300);
    await Promise.resolve();
    await Promise.resolve();
    expect(ch.current.held).toBe(3);

    // A fourth: the cap bites, and what goes is the pair served longest ago — never `4`, which is
    // what the viewer is looking at.
    served = [artifact(4n)];
    ch.refresh(view, 400, 300);
    await Promise.resolve();
    await Promise.resolve();
    expect(ch.current.held).toBe(3);
    expect(table.live).toBe(3);
    expect(table.ordinalOf('clusters/x', 1n)).toBe(0);
    expect(table.ordinalOf('clusters/x', 3n)).toBeGreaterThan(0);
    expect(table.ordinalOf('clusters/x', 4n)).toBeGreaterThan(0);
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
