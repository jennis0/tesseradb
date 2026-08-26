import {afterEach, describe, expect, it} from 'vitest';
import {SessionArtifactTable, servedLineage, type Artifact, type ArtifactsProjection, type Layer, type Meta} from '@tesseradb/client';
import '../src/layer-picker.js';
import '../src/artifact-list.js';
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
  derivedContent: ['centroid'],
  suppliedContent: [],
  depsOn,
  version: 1
});

const META: Meta = {
  apiVersion: 1,
  idset: 0,
  views: [{id: 's0', displayName: 'default'}],
  quantisation: {xMin: 0, xMax: 1, yMin: 0, yMax: 1},
  declaredScalars: [{name: 'archive', arrowType: 'u16', category: {vocabulary: 'a', kind: 'declared', visibility: 'public'}, render: true, index: true}],
  layers: [layer('clusters'), layer('labels', ['clusters']), layer('districts')],
  selection: {kMin: 1, kMaxMarks: 500, maxK: 5000, thetaTargetMarks: 10, maxUnderlayOffset: 0, maxCategoryValues: 1000},
  maxTilesPerRequest: 4096,
  filterOperands: []
};

const artifact = (id: bigint, count: bigint, parentId: bigint | null = null, content: string[] = []): Artifact => ({
  layer: 'clusters',
  tesseraId: id,
  key: `c-${id}`,
  maskedCount: count,
  centroid: [2 ** 31, 2 ** 31],
  box: null,
  hull: null,
  content,
  parentId
});

function artifactsProjection(served: Artifact[], layers = ['clusters', 'labels']): ArtifactsProjection {
  const table = new SessionArtifactTable();
  const ordinals = table.take(served.map((a) => ({tesseraId: a.tesseraId, layer: a.layer, parentId: a.parentId})));
  return {
    layer: layers[0] ?? null,
    layers,
    served,
    lineage: servedLineage(served),
    status: 'shown',
    refusal: null,
    version: 1,
    table,
    servedOrdinals: new Set(ordinals),
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
  it('builds the tree from parentId, a row beneath what contains it, with Masked counts, and opens on click', async () => {
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
    store.set('selection', {item: null, itemRefusal: null, artifact: {id: 1n, detail: {layer: 'clusters', key: 'c-1', maskedCount: 100n}}, artifactRefusal: null});
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
    store.set('selection', {item: null, itemRefusal: null, artifact: {id: 1n, detail: {layer: 'clusters', key: 'c-1', maskedCount: 100n}}, artifactRefusal: null});
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
    store.set('selection', {item: null, itemRefusal: null, artifact: {id: 1n, detail: {layer: 'clusters', key: 'c-1', maskedCount: 100n}}, artifactRefusal: null});
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
