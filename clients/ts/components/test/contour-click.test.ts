import {afterEach, describe, expect, it} from 'vitest';
import {GRID32_PER_WORLD_UNIT, SessionArtifactTable, servedLineage, type Artifact, type ArtifactsProjection} from '@tesseradb/client';
import '../src/map.js';
import {fakeStore, mount, settle, status} from './fake-store.js';

/**
 * A click on a contour, resolved the way the hover is.
 *
 * The outline layer draws the hovered and the opened artifact and nothing else, and it is not
 * pickable — so deck answers a click inside a cluster's shape with a miss, and the map resolves it
 * against the frontier's served shapes itself (`hoverAt`). One route for both, so what a click
 * opens is what the pointer was highlighting.
 */

afterEach(() => {
  document.body.innerHTML = '';
});

/** A box in world units, as the wire carries it: 32-bit grid units per axis (contracts §3.2). */
const boxOf = (x0: number, y0: number, x1: number, y1: number): [number, number, number, number] =>
  [x0, y0, x1, y1].map((v) => Math.round(v * GRID32_PER_WORLD_UNIT)) as [number, number, number, number];

const artifact = (id: bigint, parent: bigint | null, rung: number, box: [number, number, number, number]): Artifact => ({
  layer: 'clusters',
  tesseraId: id,
  key: `c-${id}`,
  maskedCount: 10n,
  centroid: [box[0], box[1]],
  box,
  shape: null,
  content: [],
  parentIds: parent === null ? [] : [parent],
  rung,
  matched: null,
  highlighted: null
});

function artifactsProjection(served: Artifact[]): ArtifactsProjection {
  const table = new SessionArtifactTable();
  const ordinals = table.take(served.map((a) => ({tesseraId: a.tesseraId, layer: a.layer, parentIds: a.parentIds, rung: a.rung})));
  return {
    layer: 'clusters',
    layers: ['clusters'],
    served,
    lineage: servedLineage(served),
    status: 'shown',
    refusal: null,
    version: 1,
    held: 0,
    table,
    servedOrdinals: new Set(ordinals),
    shapes: new Map(),
    colours: new Map(),
    palette: 'positional',
    coverage: {current: 0, stale: 0}
  };
}

/**
 * A wide root with two branches. The root and the branch that carries a child are ancestors and so
 * off the map; what is left is a small leaf inside a large one — two frontier shapes at different
 * rungs over the same ground, which is the case a click has to decide.
 */
const SERVED = [
  artifact(1n, null, 0, boxOf(0, 0, 100, 100)),
  artifact(2n, 1n, 1, boxOf(10, 10, 90, 90)),
  artifact(3n, 2n, 2, boxOf(40, 40, 60, 60)),
  artifact(4n, 1n, 1, boxOf(5, 5, 95, 95))
];

/** A map with a store and no deck: `worldAt` is stubbed, so a click carries its world point. */
async function map(): Promise<{el: {onClick(info: unknown): void}; store: ReturnType<typeof fakeStore>}> {
  const host = await mount('<tessera-map></tessera-map>');
  const el = host.querySelector('tessera-map') as unknown as {store: unknown; worldAt: unknown; onClick(info: unknown): void; lastPick: unknown};
  const store = fakeStore({status: status({}), artifacts: artifactsProjection(SERVED)});
  el.store = store;
  el.worldAt = (x: number, y: number) => [x, y];
  await settle(host);
  return {el: el as never, store};
}

const opened = (store: ReturnType<typeof fakeStore>) => store.calls.filter((c) => c.name === 'openArtifact').map((c) => String(c.args[0]));

describe('a click on a contour', () => {
  it('opens the deepest frontier shape containing the point, and asks for its hull', async () => {
    const {el, store} = await map();
    // Inside every box. 1 and 2 are ancestors and not on the map at all; of the two that are,
    // the deeper wins — the most specific thing under the cursor.
    el.onClick({index: -1, x: 50, y: 50});
    expect(opened(store)).toEqual(['3']);
    expect(store.calls.filter((c) => c.name === 'needShape').map((c) => String(c.args[0]))).toEqual(['3']);
    expect((el as unknown as {lastPick: unknown}).lastPick).toBeNull();
  });

  it('opens the shallower one where the deeper does not contain the point', async () => {
    const {el, store} = await map();
    el.onClick({index: -1, x: 20, y: 20});
    expect(opened(store)).toEqual(['4']);
  });

  it('is a miss where the point is in no drawn shape — an ancestor’s ground is not a shape', async () => {
    const {el, store} = await map();
    // Inside the root's box and outside every frontier shape: the root is served, it is nobody's
    // answer, and clicking where only it reaches opens nothing.
    el.onClick({index: -1, x: 2, y: 2});
    expect(opened(store)).toEqual([]);
    expect((el as unknown as {lastPick: {kind: string} | null}).lastPick).toEqual({kind: 'miss'});
  });

  it('leaves deck’s own answers alone: a mark under the pointer is picked, not the contour', async () => {
    const {el, store} = await map();
    const ids = new BigUint64Array([7n]);
    el.onClick({index: 0, x: 50, y: 50, sourceLayer: {id: 'marks-p0', props: {tesseraIds: ids}}, coordinate: [50, 50]});
    expect(opened(store)).toEqual([]);
    expect(store.calls.filter((c) => c.name === 'pick').map((c) => String(c.args[0]))).toEqual(['7']);
  });
});
