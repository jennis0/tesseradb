import {afterEach, describe, expect, it} from 'vitest';
import {type Artifact, type ArtifactsProjection, type Layer} from '@mosaica/client';
import {SessionArtifactTable, servedLineage, attachedTextOf} from '@mosaica/client/internal';
import '../src/layer-picker.js';
import '../src/artifact-card.js';
import '../src/explorer.js';
import '../src/item-card.js';
import '../src/selection.js';
import '../src/status.js';
import {deep, deepAll, deepText, fakeStore, mount, settle, status, meta, scalar} from './fake-store.js';
import {UNNAMED} from '../src/base.js';

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

const META = meta({
  declaredScalars: [scalar('archive', 'u16', {category: {vocabulary: 'a', kind: 'declared', visibility: 'public'}, render: true, homes: ['rendered']})],
  layers: [layer('clusters'), layer('labels', ['clusters']), layer('districts')]
});

const artifact = (id: bigint, count: bigint, parent: bigint | null = null, content: string[] = []): Artifact => ({
  layer: 'clusters',
  tesseraId: id,
  key: `c-${id}`,
  maskedCount: count,
  centroid: [2 ** 31, 2 ** 31],
  box: null,
  content,
  parentIds: parent === null ? [] : [parent],
  rung: 0,
  matched: null,
  highlighted: null,
  target: null,
  slot: null
});

function artifactsProjection(served: Artifact[], layers = ['clusters', 'labels']): ArtifactsProjection {
  const table = new SessionArtifactTable();
  const ordinals = table.take(served.map((a) => ({tesseraId: a.tesseraId, layer: a.layer, parentIds: a.parentIds})));
  return {
    layer: layers[0] ?? null,
    layers,
    served,
    colourServed: [],
    attached: attachedTextOf(served, META.layers),
    lineage: servedLineage(served),
    status: 'shown',
    refusal: null,
    version: 1,
    held: served.length,
    table,
    servedOrdinals: new Set(ordinals),
    shapes: new Map(),
    colours: new Map([...ordinals].map((o) => [o, [10, 20, 30, 255] as const])),
    palette: 'tableau10', overrides: new Map(),
    coverage: {current: 3, stale: 0}
  };
}

describe('<mosaica-layer-picker>', () => {
  it('offers one entry per root with its closure, never a count, and names the closure on toggle', async () => {
    const host = await mount('<mosaica-layer-picker></mosaica-layer-picker>');
    const store = fakeStore({meta: META, status: status({})});
    (host.querySelector('mosaica-layer-picker') as unknown as {store: unknown}).store = store;
    await settle(host);
    const entries = deepAll(host, '[part="entry"]').map((e) => e.getAttribute('data-layer'));
    // `labels` depends on `clusters`, so it is inside that entry and not one of its own.
    expect(entries).toEqual(['clusters', 'districts']);
    expect(deep(host, '[part="entry"][data-layer="clusters"]')?.getAttribute('title')).toContain('labels');
    expect(deep(host, 'mosaica-count')).toBeNull();
    const box = deep(host, '[part="entry"][data-layer="clusters"] input') as HTMLInputElement;
    box.checked = true;
    box.dispatchEvent(new Event('change'));
    const sent = store.calls.find((c) => c.name === 'setLayers');
    expect(sent!.args[0]).toEqual(['clusters']);
  });

  it('names a filter layer in a note and gives it no checkbox', async () => {
    const host = await mount('<mosaica-layer-picker></mosaica-layer-picker>');
    const venues = {...layer('venues'), title: 'Venues', computedContent: []};
    const store = fakeStore({meta: {...META, layers: [...META.layers, venues]}, status: status({})});
    (host.querySelector('mosaica-layer-picker') as unknown as {store: unknown}).store = store;
    await settle(host);
    expect(deepAll(host, '[part="entry"]').map((e) => e.getAttribute('data-layer'))).toEqual(['clusters', 'districts']);
    expect(deep(host, '[part="note"]')?.getAttribute('data-layers')).toBe('venues');
    expect(deepAll(host, 'input[type="checkbox"]')).toHaveLength(2);
  });
});

describe('<mosaica-artifact-card>', () => {
  it('holds its count across a pan — the served set moves, the card does not', async () => {
    const host = await mount('<mosaica-artifact-card></mosaica-artifact-card>');
    const store = fakeStore({meta: META, status: status({}), artifacts: artifactsProjection([artifact(1n, 100n, null, ['Alpha']), artifact(2n, 40n, 1n)])});
    (host.querySelector('mosaica-artifact-card') as unknown as {store: unknown}).store = store;
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
    const host = await mount('<mosaica-artifact-card></mosaica-artifact-card>');
    const store = fakeStore({meta: META, status: status({}), artifacts: artifactsProjection([artifact(1n, 100n), artifact(2n, 40n, 1n)])});
    (host.querySelector('mosaica-artifact-card') as unknown as {store: unknown}).store = store;
    store.set('selection', {item: null, itemRefusal: null, artifact: {id: 1n, detail: {layer: 'clusters', key: 'c-1', maskedCount: 100n, centroid: null, box: null, shape: null}}, artifactRefusal: null});
    await settle(host);
    expect(deep(host, '[part="headline"]')?.textContent?.trim()).toBe(UNNAMED);
    expect(deepAll(host, '[part="child"] [part="name"]').map((n) => n.textContent?.trim())).toEqual([UNNAMED]);
    // The key is still there — under the field that says it is a key, which is where it belongs.
    const fields = deepAll(host, '[part="value"]').map((v) => v.textContent);
    expect(fields).toContain('c-1');
    // And the artifact the card was opened on before the channel answered shows the placeholder
    // rather than falling back to the key it carries in the drill-down.
    store.set('artifacts', artifactsProjection([]));
    await settle(host);
    expect(deep(host, '[part="headline"]')?.textContent?.trim()).toBe(UNNAMED);
  });

  it('renders a refusal as one', async () => {
    const host = await mount('<mosaica-artifact-card></mosaica-artifact-card>');
    const store = fakeStore({meta: META, status: status({})});
    (host.querySelector('mosaica-artifact-card') as unknown as {store: unknown}).store = store;
    store.set('selection', {item: null, itemRefusal: null, artifact: null, artifactRefusal: {code: 'not-found', detail: 'no'}});
    await settle(host);
    expect(deep(host, '[part="state"]')?.getAttribute('data-state')).toBe('refused');
    expect(deep(host, '[part="refusal"]')).not.toBeNull();
  });
});

describe('<mosaica-artifact-card> follows the served set', () => {
  it('lists the children served for the view as the channel answers, and invents no place for a root', async () => {
    const host = await mount('<mosaica-artifact-card></mosaica-artifact-card>');
    // Opened before the channel has answered for this view: the served set is empty.
    const store = fakeStore({meta: META, status: status({}), artifacts: artifactsProjection([])});
    (host.querySelector('mosaica-artifact-card') as unknown as {store: unknown}).store = store;
    store.set('selection', {item: null, itemRefusal: null, artifact: {id: 1n, detail: {layer: 'clusters', key: 'c-1', maskedCount: 100n, centroid: null, box: null, shape: null}}, artifactRefusal: null});
    await settle(host);
    expect(deepAll(host, '[part="child"]').length).toBe(0);
    // The channel answers: the artifact and two children are served. The card re-reads.
    store.set('artifacts', artifactsProjection([artifact(1n, 100n, null, ['Alpha']), artifact(2n, 40n, 1n, ['Beta']), artifact(3n, 60n, 1n, ['Gamma'])]));
    await settle(host);
    expect(deepAll(host, '[part="child"] [part="name"]').map((n) => n.textContent)).toEqual(['Gamma', 'Beta']);
    // A root of a flat layer has no parent, and the card draws no parents section for it.
    expect(deep(host, '[part="parents"]')).toBeNull();
  });

  it('puts no label on a clause made from an artifact with no name', async () => {
    const host = await mount('<mosaica-artifact-card></mosaica-artifact-card>');
    const store = fakeStore({meta: META, status: status({}), artifacts: artifactsProjection([artifact(1n, 100n)])});
    (host.querySelector('mosaica-artifact-card') as unknown as {store: unknown}).store = store;
    store.set('selection', {item: null, itemRefusal: null, artifact: {id: 1n, detail: {layer: 'clusters', key: 'c-1', maskedCount: 100n, centroid: null, box: null, shape: null}}, artifactRefusal: null});
    await settle(host);
    (deep(host, '[part="filter"]') as HTMLButtonElement).click();
    const [clause] = store.calls.find((c) => c.name === 'setMembers')!.args[0] as {label?: string}[];
    expect(clause).not.toHaveProperty('label');
  });

  it('opens a parent or a child row on Enter and on Space, as on a click', async () => {
    const host = await mount('<mosaica-artifact-card></mosaica-artifact-card>');
    const served = [artifact(1n, 100n, null, ['Alpha']), artifact(2n, 40n, 1n, ['Beta']), artifact(3n, 10n, 2n, ['Gamma'])];
    const store = fakeStore({meta: META, status: status({}), artifacts: artifactsProjection(served)});
    (host.querySelector('mosaica-artifact-card') as unknown as {store: unknown}).store = store;
    store.set('selection', {item: null, itemRefusal: null, artifact: {id: 2n, detail: {layer: 'clusters', key: 'c-2', maskedCount: 40n, centroid: null, box: null, shape: null}}, artifactRefusal: null});
    await settle(host);
    const press = (selector: string, key: string) => {
      const row = deep(host, selector)!;
      const e = new KeyboardEvent('keydown', {key, bubbles: true, cancelable: true});
      row.dispatchEvent(e);
      return e.defaultPrevented;
    };
    expect(press('[part="parent"]', 'Enter')).toBe(true);
    expect(press('[part="child"]', ' ')).toBe(true);
    expect(press('[part="child"]', 'a')).toBe(false);
    expect(store.calls.filter((c) => c.name === 'openArtifact').map((c) => c.args[0])).toEqual([1n, 3n]);
  });

  it('lists a child on the card of each served parent it names (decision 0117)', async () => {
    const host = await mount('<mosaica-artifact-card></mosaica-artifact-card>');
    const served = [artifact(1n, 100n, null, ['Alpha']), artifact(2n, 90n, null, ['Beta']), {...artifact(3n, 10n, null, ['Gamma']), parentIds: [1n, 2n]}];
    const store = fakeStore({meta: META, status: status({}), artifacts: artifactsProjection(served)});
    (host.querySelector('mosaica-artifact-card') as unknown as {store: unknown}).store = store;
    for (const id of [1n, 2n]) {
      store.set('selection', {item: null, itemRefusal: null, artifact: {id, detail: {layer: 'clusters', key: `c-${id}`, maskedCount: 100n, centroid: null, box: null, shape: null}}, artifactRefusal: null});
      await settle(host);
      expect(deepAll(host, '[part="child"] [part="name"]').map((n) => n.textContent)).toEqual(['Gamma']);
    }
  });
});

describe('<mosaica-explorer> and the map’s tooltip slot', () => {
  it('forwards the tooltip slot only when the host supplied one, so the map’s fallback survives', async () => {
    // A slot assigned an empty slot counts as filled and would hide the fallback, leaving an
    // empty box beside the pointer.
    const bare = await mount('<mosaica-explorer></mosaica-explorer>');
    (bare.querySelector('mosaica-explorer') as unknown as {store: unknown}).store = fakeStore({meta: META, status: status({})});
    await settle(bare);
    const map = deep(bare, 'mosaica-map') as HTMLElement | null;
    expect(map).not.toBeNull();
    expect(map!.querySelector('slot[name="tooltip"]')).toBeNull();

    const given = await mount('<mosaica-explorer><div slot="tooltip">mine</div></mosaica-explorer>');
    (given.querySelector('mosaica-explorer') as unknown as {store: unknown}).store = fakeStore({meta: META, status: status({})});
    await settle(given);
    expect((deep(given, 'mosaica-map') as HTMLElement).querySelector('slot[name="tooltip"]')).not.toBeNull();
  });
});

describe('<mosaica-explorer> on an artifact selection', () => {
  it('selects — the card — and never moves the camera; fit is the card’s own button', async () => {
    const host = await mount('<mosaica-explorer></mosaica-explorer>');
    const explorer = host.querySelector('mosaica-explorer') as unknown as {store: unknown; map: {fitTo(id: bigint): boolean} | null};
    const store = fakeStore({meta: META, status: status({}), artifacts: artifactsProjection([artifact(1n, 100n, null, ['Alpha'])])});
    explorer.store = store;
    await settle(host);
    const fitted: bigint[] = [];
    explorer.map!.fitTo = (id) => {
      fitted.push(id);
      return true;
    };
    store.set('selection', {item: null, itemRefusal: null, artifact: {id: 1n, detail: {layer: 'clusters', key: 'c-1', maskedCount: 100n, centroid: null, box: null, shape: null}}, artifactRefusal: null});
    await settle(host);
    const card = deep(host, 'mosaica-artifact-card') as HTMLElement;
    expect(card).not.toBeNull();
    const fit = deep(card.shadowRoot!, '[part="fit"]') as HTMLButtonElement;
    expect(fit).not.toBeNull();
    fit.click();
    await settle(host);
    expect(fitted).toEqual([1n]);
  });
});

describe('a refusal on screen', () => {
  const code = async (markup: string, over: Parameters<typeof fakeStore>[0], ready?: (host: HTMLElement) => Promise<void>) => {
    const host = await mount(markup);
    (host.firstElementChild as unknown as {store: unknown}).store = fakeStore({meta: META, status: status({}), ...over});
    await settle(host);
    await ready?.(host);
    const found = deep(host, '[part="refusal"]')?.getAttribute('data-code') ?? null;
    host.remove();
    return found;
  };

  it('carries the server’s code on the refusal part, in every element that shows one', async () => {
    const refusal = {code: 'withheld', detail: 'not for you'};
    const selection = {item: null, itemRefusal: null, artifact: null, artifactRefusal: null};
    expect(await code('<mosaica-item-card></mosaica-item-card>', {selection: {...selection, itemRefusal: refusal}})).toBe('withheld');
    expect(await code('<mosaica-artifact-card></mosaica-artifact-card>', {selection: {...selection, artifactRefusal: refusal}})).toBe('withheld');
    const box = {kind: 'box' as const, bbox: [0, 0, 1, 1] as [number, number, number, number]};
    const held = {ids: new BigUint64Array(0), positions: new Float32Array(0), count: 0};
    expect(await code('<mosaica-selection></mosaica-selection>', {region: {shape: box, status: 'refused', refusal, visible: null, matched: {value: 0, exact: false}, served: {shown: 0, total: 0, exact: false}, verdict: null, held}})).toBe('withheld');
    expect(await code('<mosaica-status></mosaica-status>', {status: status({status: 'refused', refusal})})).toBe('withheld');
  });
});

describe('a cluster named by the label attached to it', () => {
  // Clusters 1 and 2 carry no text of their own. The label on 1 is served; the label on 2 is
  // withheld from this viewer, so the server never sent it.
  const label: Artifact = {...artifact(9n, 100n, null, ['spin magnetic effect']), layer: 'labels', centroid: null, target: 1n};
  const clusters = [artifact(1n, 100n), artifact(2n, 40n)];

  async function shown(markup: string) {
    const host = await mount(markup);
    const store = fakeStore({
      meta: META,
      status: status({}),
      artifacts: {...artifactsProjection([...clusters, label]), colourServed: clusters},
      legend: {ranks: {}, domains: {}, categories: {}, categoryErrors: {}, colourBy: 'cluster:clusters', samples: {}, missing: {}, sizeBy: null}
    });
    for (const el of host.querySelectorAll('*')) (el as unknown as {store: unknown}).store = store;
    await settle(host);
    return {host, store};
  }

  it('in the artifact card, and on the clause the card makes', async () => {
    const {host, store} = await shown('<mosaica-artifact-card></mosaica-artifact-card>');
    store.set('selection', {item: null, itemRefusal: null, artifact: {id: 1n, detail: {layer: 'clusters', key: 'c-1', maskedCount: 100n, centroid: null, box: null, shape: null}}, artifactRefusal: null});
    await settle(host);
    expect(deep(host, '[part="headline"]')?.textContent).toBe('spin magnetic effect');
    (deep(host, '[part="filter"]') as HTMLButtonElement).click();
    const [clause] = store.calls.find((c) => c.name === 'setMembers')!.args[0] as {label?: string}[];
    expect(clause?.label).toBe('spin magnetic effect');
    store.set('selection', {item: null, itemRefusal: null, artifact: {id: 2n, detail: {layer: 'clusters', key: 'c-2', maskedCount: 40n, centroid: null, box: null, shape: null}}, artifactRefusal: null});
    await settle(host);
    expect(deep(host, '[part="headline"]')?.textContent).toBe(UNNAMED);
    expect(deep(host, '[part="headline"]')?.hasAttribute('data-unnamed')).toBe(true);
  });
});
