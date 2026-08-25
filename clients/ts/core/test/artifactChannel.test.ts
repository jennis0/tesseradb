import {describe, expect, it, vi} from 'vitest';
import {ArtifactChannel, type ArtifactChannelClock, type ArtifactChannelState} from '../src/artifactChannel.js';
import {SessionArtifactTable} from '../src/artifactTable.js';
import {TesseraClient} from '../src/client.js';
import type {Artifact, Quantisation, ViewportResponse, ViewportResult} from '../src/types.js';

const Q: Quantisation = {xMin: 0, xMax: 100, yMin: 0, yMax: 100};

const artifact = (id: bigint, parentId: bigint | null = null): Artifact => ({
  layer: 'clusters/x',
  tesseraId: id,
  key: `c-${id}`,
  maskedCount: 10n,
  centroid: null,
  box: null,
  hull: null,
  content: [],
  parentId
});

function responseWith(artifacts: Artifact[]): ViewportResponse {
  const result: ViewportResult = {
    tiles: [],
    ids: new BigUint64Array(0),
    codes: new BigUint64Array(0),
    positions: new Float64Array(0),
    world: new Float32Array(0),
    scalars: {},
    subCells: null,
    artifacts
  };
  return {
    result,
    timings: {serverUs: 0, admissionUs: 0, stageNs: null},
    identityKey: 'ik',
    contentKey: 'ck',
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

  it('feeds the session table its served set and releases the previous reference on rotation', async () => {
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

    // The set rotates to a disjoint one: the previous reference is released, the new one taken.
    served = [artifact(3n)];
    ch.refresh(view, 400, 300);
    await Promise.resolve();
    await Promise.resolve();
    expect(table.live).toBe(1);
    expect(table.ordinalOf('clusters/x', 3n)).toBeGreaterThan(0);
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
