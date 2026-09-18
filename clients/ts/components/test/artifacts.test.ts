import {afterEach, describe, expect, it} from 'vitest';
import {SessionArtifactTable, servedLineage, type Artifact, type ArtifactsProjection, type Layer, type Meta} from '@tesseradb/client';
import '../src/layer-picker.js';
import {flatten} from '../src/artifact-list.js';
import '../src/artifact-card.js';
import '../src/legend.js';
import '../src/explorer.js';
import {deep, deepAll, deepText, fakeStore, mount, settle, status} from './fake-store.js';

afterEach(() => {
  document.body.innerHTML = '';
});

const layer = (name: string, depsOn: string[] = []): Layer => ({
  name,
  title: name,
  views: ['s0'],
  membership: 'enumerated',
  hierarchy: {kind: 'flat', pruneChildren: false},
  levels: [],
  computedContent: ['centroid'],
  shape: null,
  suppliedContent: [],
  depsOn,
  version: 1
});

const META: Meta = {
  apiVersion: 1,
  idset: 0,
  views: [{id: 's0', displayName: 'default', quantisation: {xMin: 0, xMax: 1, yMin: 0, yMax: 1}, projection: 'none', worldAspect: null, tileScheme: null, tile: null, roster: null}],
  groups: [],
  declaredScalars: [{name: 'archive', arrowType: 'u16', category: {vocabulary: 'a', kind: 'declared', visibility: 'public'}, render: true, index: true}],
  layers: [layer('clusters'), layer('labels', ['clusters']), layer('districts')],
  selection: {kMin: 1, kMaxMarks: 500, maxK: 5000, thetaTargetMarks: 10, maxUnderlayOffset: 0, maxCategoryValues: 1000, maxRegionVertices: 10_000, maxRegionCells: 262_144, maxBrowseRows: 200},
  maxTilesPerRequest: 4096,
  filterOperands: []
};

const artifact = (id: bigint, count: bigint, parent: bigint | null = null, content: string[] = []): Artifact => ({
  layer: 'clusters',
  tesseraId: id,
  key: `c-${id}`,
  maskedCount: count,
  centroid: [2 ** 31, 2 ** 31],
  box: null,
  shape: null,
  content,
  parentIds: parent === null ? [] : [parent],
  rung: 0,
  matched: null,
  highlighted: null,
  target: null
});

function artifactsProjection(served: Artifact[], layers = ['clusters', 'labels']): ArtifactsProjection {
  const table = new SessionArtifactTable();
  const ordinals = table.take(served.map((a) => ({tesseraId: a.tesseraId, layer: a.layer, parentIds: a.parentIds})));
  return {
    layer: layers[0] ?? null,
    layers,
    served,
    lineage: servedLineage(served),
    status: 'shown',
    refusal: null,
    version: 1,
    held: served.length,
    table,
    servedOrdinals: new Set(ordinals),
    shapes: new Map(),
    colours: new Map([...ordinals].map((o) => [o, [10, 20, 30, 255] as const])),
    palette: 'positional',
    coverage: {current: 3, stale: 0}
  };
}

describe('<tessera-layer-picker>', () => {
  it('offers one entry per root with its closure, never a count, and names the closure on toggle', async () => {
    const host = await mount('<tessera-layer-picker></tessera-layer-picker>');
    const store = fakeStore({meta: META, status: status({})});
    (host.querySelector('tessera-layer-picker') as unknown as {store: unknown}).store = store;
    await settle(host);
    const entries = deepAll(host, '[part="entry"]').map((e) => e.getAttribute('data-layer'));
    // `labels` depends on `clusters`, so it is inside that entry and not one of its own.
    expect(entries).toEqual(['clusters', 'districts']);
    expect(deep(host, '[part="entry"][data-layer="clusters"]')?.getAttribute('title')).toContain('labels');
    expect(host.shadowRoot?.textContent ?? deepAll(host, '*').map((e) => e.textContent).join(' ')).not.toMatch(/\d+ artifacts/);
    const box = deep(host, '[part="entry"][data-layer="clusters"] input') as HTMLInputElement;
    box.checked = true;
    box.dispatchEvent(new Event('change'));
    const sent = store.calls.find((c) => c.name === 'setLayers');
    expect(sent!.args[0]).toEqual(['clusters']);
  });
});

describe('<tessera-artifact-list>', () => {
  it('builds the tree from parentIds, a row beneath what contains it, with Masked counts, and opens on click', async () => {
    const host = await mount('<tessera-artifact-list></tessera-artifact-list>');
    const served = [artifact(1n, 100n, null, ['Alpha']), artifact(2n, 40n, 1n), artifact(3n, 60n, 1n), artifact(4n, 5n)];
    const store = fakeStore({meta: META, status: status({}), artifacts: artifactsProjection(served)});
    (host.querySelector('tessera-artifact-list') as unknown as {store: unknown}).store = store;
    await settle(host);
    const rows = deepAll(host, '[part="item"]');
    expect(rows.map((r) => r.getAttribute('data-id'))).toEqual(['1', '3', '2', '4']);
    expect(rows.map((r) => (r as HTMLElement).style.getPropertyValue('--depth'))).toEqual(['0', '1', '1', '0']);
    expect(deep(host, '[part="item"][data-id="1"] [part="name"]')?.textContent).toBe('Alpha');
    expect(deepText(deep(host, '[part="item"][data-id="3"] [part="count"]')).trim()).toBe('60');
    (rows[1] as HTMLElement).click();
    expect(store.calls.find((c) => c.name === 'openArtifact')?.args[0]).toBe(3n);
  });

  /**
   * A `dag` layer's child served under two parents (decision 0117): it is beneath both in the
   * lineage, and the list shows it **once**, under the first parent the count-ordered walk
   * reaches — the larger parent here — with nothing under the other. What a second position
   * would look like is the components work's, not this test's.
   */
  it('lists a child of two served parents once, beneath the first reached, and the walk is by count then id', async () => {
    const host = await mount('<tessera-artifact-list></tessera-artifact-list>');
    const served = [artifact(2n, 90n, null, ['Beta']), artifact(1n, 100n, null, ['Alpha']), {...artifact(3n, 10n, null, ['Gamma']), parentIds: [1n, 2n]}];
    const lineage = servedLineage(served);
    expect(lineage.roots.map((a) => a.tesseraId)).toEqual([2n, 1n]);
    expect(lineage.childrenOf.get(1n)?.map((a) => a.tesseraId)).toEqual([3n]);
    expect(lineage.childrenOf.get(2n)?.map((a) => a.tesseraId)).toEqual([3n]);
    const store = fakeStore({meta: META, status: status({}), artifacts: artifactsProjection(served)});
    (host.querySelector('tessera-artifact-list') as unknown as {store: unknown}).store = store;
    await settle(host);
    const rows = deepAll(host, '[part="item"]');
    expect(rows.map((r) => r.getAttribute('data-id'))).toEqual(['1', '3', '2']);
    expect(rows.map((r) => (r as HTMLElement).style.getPropertyValue('--depth'))).toEqual(['0', '1', '0']);
    // Two roots of equal count list by lowest id, so the order is the served set's and not the wire's row order.
    const tied = flatten(servedLineage([artifact(5n, 7n), artifact(4n, 7n)]));
    expect(tied.map(({artifact}) => artifact.tesseraId)).toEqual([4n, 5n]);
  });

  it('shows a count and a neutral placeholder where a row has no name — never the key', async () => {
    const host = await mount('<tessera-artifact-list></tessera-artifact-list>');
    // `c-2` has no supplied text and no topic attached: its key is an id and is not a name.
    const served = [artifact(1n, 100n, null, ['Alpha']), artifact(2n, 40n)];
    const store = fakeStore({meta: META, status: status({}), artifacts: artifactsProjection(served)});
    (host.querySelector('tessera-artifact-list') as unknown as {store: unknown}).store = store;
    await settle(host);
    const nameless = deep(host, '[part="item"][data-id="2"] [part="name"]');
    expect(nameless?.textContent?.trim()).toBe('\u2014');
    expect(nameless?.hasAttribute('data-unnamed')).toBe(true);
    expect(deepText(deep(host, '[part="item"][data-id="2"] [part="count"]')).trim()).toBe('40');
    // Nowhere in the list — not in a title, not in a row — does the key appear.
    expect(deepText(host)).not.toContain('c-2');
    expect(deepAll(host, '[part="item"]').map((r) => r.outerHTML).join(' ')).not.toContain('c-2');
    // A named row is unmarked, so the placeholder can be styled apart from a real name.
    expect(deep(host, '[part="item"][data-id="1"] [part="name"]')?.hasAttribute('data-unnamed')).toBe(false);
  });

  it('renders no layer on, loading, a refusal and an empty answer as themselves', async () => {
    const host = await mount('<tessera-artifact-list></tessera-artifact-list>');
    const store = fakeStore({meta: META, status: status({})});
    (host.querySelector('tessera-artifact-list') as unknown as {store: unknown}).store = store;
    await settle(host);
    expect(deep(host, '[part="state"]')?.getAttribute('data-state')).toBe('empty');
    store.set('artifacts', {...artifactsProjection([]), status: 'refused', refusal: {code: 'x', detail: 'y'}});
    await settle(host);
    expect(deep(host, '[part="state"]')?.getAttribute('data-state')).toBe('refused');
    store.set('artifacts', artifactsProjection([]));
    await settle(host);
    expect(deep(host, '[part="state"]')?.textContent).toContain('Nothing in this view');
  });
});

describe('<tessera-artifact-card>', () => {
  it('holds its count across a pan — the served set moves, the card does not', async () => {
    const host = await mount('<tessera-artifact-card></tessera-artifact-card>');
    const store = fakeStore({meta: META, status: status({}), artifacts: artifactsProjection([artifact(1n, 100n, null, ['Alpha']), artifact(2n, 40n, 1n)])});
    (host.querySelector('tessera-artifact-card') as unknown as {store: unknown}).store = store;
    store.set('selection', {item: null, itemRefusal: null, artifact: {id: 1n, detail: {layer: 'clusters', key: 'c-1', maskedCount: 100n, centroid: null, box: null, shape: null}}, artifactRefusal: null});
    await settle(host);
    expect(deepText(deep(host, '[part="count"]'))).toContain('100');
    expect(deep(host, '[part="headline"]')?.textContent).toBe('Alpha');
    expect(deepAll(host, '[part="child"]').length).toBe(1);
    // A pan: the served set is now other artifacts entirely. The count is the drill-down's and stays.
    store.set('artifacts', artifactsProjection([artifact(9n, 7n)]));
    await settle(host);
    expect(deepText(deep(host, '[part="count"]'))).toContain('100');
    expect(deepAll(host, '[part="child"]').length).toBe(0);
    expect(deep(host, '[part="state"]')?.getAttribute('data-state')).toBe('shown');
  });

  it('shows the placeholder in the headline where a cluster has no name, and the key only as the key', async () => {
    const host = await mount('<tessera-artifact-card></tessera-artifact-card>');
    const store = fakeStore({meta: META, status: status({}), artifacts: artifactsProjection([artifact(1n, 100n), artifact(2n, 40n, 1n)])});
    (host.querySelector('tessera-artifact-card') as unknown as {store: unknown}).store = store;
    store.set('selection', {item: null, itemRefusal: null, artifact: {id: 1n, detail: {layer: 'clusters', key: 'c-1', maskedCount: 100n, centroid: null, box: null, shape: null}}, artifactRefusal: null});
    await settle(host);
    expect(deep(host, '[part="headline"]')?.textContent?.trim()).toBe('\u2014');
    expect(deepAll(host, '[part="child"] [part="name"]').map((n) => n.textContent?.trim())).toEqual(['\u2014']);
    // The key is still there — under the field that says it is a key, which is where it belongs.
    const fields = deepAll(host, '[part="value"]').map((v) => v.textContent);
    expect(fields).toContain('c-1');
    // And the artifact the card was opened on before the channel answered shows the placeholder
    // rather than falling back to the key it carries in the drill-down.
    store.set('artifacts', artifactsProjection([]));
    await settle(host);
    expect(deep(host, '[part="headline"]')?.textContent?.trim()).toBe('\u2014');
  });

  it('renders a refusal as one', async () => {
    const host = await mount('<tessera-artifact-card></tessera-artifact-card>');
    const store = fakeStore({meta: META, status: status({})});
    (host.querySelector('tessera-artifact-card') as unknown as {store: unknown}).store = store;
    store.set('selection', {item: null, itemRefusal: null, artifact: null, artifactRefusal: {code: 'not-found', detail: 'no'}});
    await settle(host);
    expect(deep(host, '[part="refusal"]')?.textContent).toContain('not-found');
  });
});

describe('<tessera-legend selectable>', () => {
  it('offers cluster colour only for a layer that is on, and sends cluster:<layer>', async () => {
    const host = await mount('<tessera-legend selectable readout></tessera-legend>');
    const store = fakeStore({meta: META, status: status({})});
    (host.querySelector('tessera-legend') as unknown as {store: unknown}).store = store;
    await settle(host);
    const colourOptions = () => deepAll(host, '[part="select"] option').map((o) => o.getAttribute('value'));
    expect(colourOptions()).toEqual(['', 'archive']);
    store.set('artifacts', artifactsProjection([artifact(1n, 100n)], ['clusters']));
    await settle(host);
    expect(colourOptions()).toEqual(['', 'cluster:clusters', 'archive']);
    // The Layers select beside it: one of three on, and each root offered.
    expect(deepAll(host, '[part="layers-select"] option').map((o) => o.textContent)).toEqual(['1 of 2 on', 'clusters', 'districts']);
    const select = deep(host, '[part="select"]') as HTMLSelectElement;
    select.value = 'cluster:clusters';
    select.dispatchEvent(new Event('change'));
    expect(store.calls.find((c) => c.name === 'setColourBy')?.args[0]).toBe('cluster:clusters');
    store.set('legend', {ranks: {}, domains: {}, categories: {}, categoryErrors: {}, colourBy: 'cluster:clusters'});
    await settle(host);
    expect(deepAll(host, '[part="swatch"]').length).toBe(2); // the served artifact and the neutral
  });

  it('is a readout without selectable', async () => {
    const host = await mount('<tessera-legend></tessera-legend>');
    const store = fakeStore({meta: META, status: status({})});
    (host.querySelector('tessera-legend') as unknown as {store: unknown}).store = store;
    await settle(host);
    expect(deep(host, 'select')).toBeNull();
  });
});

describe('<tessera-artifact-card> follows the served set', () => {
  it('lists the children served for the view as the channel answers, and invents no place for a root', async () => {
    const host = await mount('<tessera-artifact-card></tessera-artifact-card>');
    // Opened before the channel has answered for this view: the served set is empty.
    const store = fakeStore({meta: META, status: status({}), artifacts: artifactsProjection([])});
    (host.querySelector('tessera-artifact-card') as unknown as {store: unknown}).store = store;
    store.set('selection', {item: null, itemRefusal: null, artifact: {id: 1n, detail: {layer: 'clusters', key: 'c-1', maskedCount: 100n, centroid: null, box: null, shape: null}}, artifactRefusal: null});
    await settle(host);
    expect(deepAll(host, '[part="child"]').length).toBe(0);
    // The channel answers: the artifact and two children are served. The card re-reads.
    store.set('artifacts', artifactsProjection([artifact(1n, 100n, null, ['Alpha']), artifact(2n, 40n, 1n, ['Beta']), artifact(3n, 60n, 1n, ['Gamma'])]));
    await settle(host);
    expect(deepAll(host, '[part="child"] [part="name"]').map((n) => n.textContent)).toEqual(['Gamma', 'Beta']);
    // A root of a flat layer has no parent; the card says nothing about it rather than
    // "nothing you were served", which read as a claim about the principal.
    const text = deepText(host);
    expect(text).not.toMatch(/nothing you were served/);
    expect(text).not.toMatch(/inside/);
  });

  it('lists a child on the card of each served parent it names (decision 0117)', async () => {
    const host = await mount('<tessera-artifact-card></tessera-artifact-card>');
    const served = [artifact(1n, 100n, null, ['Alpha']), artifact(2n, 90n, null, ['Beta']), {...artifact(3n, 10n, null, ['Gamma']), parentIds: [1n, 2n]}];
    const store = fakeStore({meta: META, status: status({}), artifacts: artifactsProjection(served)});
    (host.querySelector('tessera-artifact-card') as unknown as {store: unknown}).store = store;
    for (const id of [1n, 2n]) {
      store.set('selection', {item: null, itemRefusal: null, artifact: {id, detail: {layer: 'clusters', key: `c-${id}`, maskedCount: 100n, centroid: null, box: null, shape: null}}, artifactRefusal: null});
      await settle(host);
      expect(deepAll(host, '[part="child"] [part="name"]').map((n) => n.textContent)).toEqual(['Gamma']);
    }
  });
});

describe('<tessera-explorer> and the map’s tooltip slot', () => {
  it('forwards the tooltip slot only when the host supplied one, so the map’s fallback survives', async () => {
    // A slot assigned an empty slot counts as filled and hides the fallback: the hover rendered as
    // an empty bordered box beside the pointer (the owner's review, 2026-08-28).
    const bare = await mount('<tessera-explorer></tessera-explorer>');
    (bare.querySelector('tessera-explorer') as unknown as {store: unknown}).store = fakeStore({meta: META, status: status({})});
    await settle(bare);
    const map = deep(bare, 'tessera-map') as HTMLElement | null;
    expect(map).not.toBeNull();
    expect(map!.querySelector('slot[name="tooltip"]')).toBeNull();

    const given = await mount('<tessera-explorer><div slot="tooltip">mine</div></tessera-explorer>');
    (given.querySelector('tessera-explorer') as unknown as {store: unknown}).store = fakeStore({meta: META, status: status({})});
    await settle(given);
    expect((deep(given, 'tessera-map') as HTMLElement).querySelector('slot[name="tooltip"]')).not.toBeNull();
  });
});

describe('<tessera-explorer> on an artifact selection', () => {
  it('selects — the card — and never moves the camera; fit is the card’s own button', async () => {
    const host = await mount('<tessera-explorer></tessera-explorer>');
    const explorer = host.querySelector('tessera-explorer') as unknown as {store: unknown; map: {fitTo(id: bigint): boolean} | null};
    const store = fakeStore({meta: META, status: status({}), artifacts: artifactsProjection([artifact(1n, 100n, null, ['Alpha'])])});
    explorer.store = store;
    await settle(host);
    const fitted: bigint[] = [];
    explorer.map!.fitTo = (id) => {
      fitted.push(id);
      return true;
    };
    const list = deep(host, 'tessera-artifact-list') as HTMLElement;
    const row = deep(list.shadowRoot!, '[part="item"]') as HTMLElement;
    expect(row).not.toBeNull();
    row.click();
    await settle(host);
    expect(store.calls.find((c) => c.name === 'openArtifact')?.args[0]).toBe(1n);
    expect(fitted).toEqual([]);
    store.set('selection', {item: null, itemRefusal: null, artifact: {id: 1n, detail: {layer: 'clusters', key: 'c-1', maskedCount: 100n, centroid: null, box: null, shape: null}}, artifactRefusal: null});
    await settle(host);
    const card = deep(host, 'tessera-artifact-card') as HTMLElement;
    expect(card).not.toBeNull();
    const fit = deep(card.shadowRoot!, '[part="fit"]') as HTMLButtonElement;
    expect(fit).not.toBeNull();
    fit.click();
    await settle(host);
    expect(fitted).toEqual([1n]);
  });
});
