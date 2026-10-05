import {afterEach, describe, expect, it} from 'vitest';
import {type Artifact, type ArtifactsProjection, type Layer, type Meta} from '@tesseradb/client';
import {SessionArtifactTable, servedLineage, attachedTextOf} from '@tesseradb/client/internal';
import '../src/layer-picker.js';
import '../src/artifact-card.js';
import '../src/legend.js';
import '../src/explorer.js';
import '../src/artifact-list.js';
import '../src/item-card.js';
import '../src/selection.js';
import '../src/status.js';
import {aggregateEntry, answerAggregate, deep, deepAll, deepText, fakeStore, mount, registered, settle, status, meta, scalar} from './fake-store.js';
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
  target: null
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
    expect(deep(host, 'tessera-count')).toBeNull();
    const box = deep(host, '[part="entry"][data-layer="clusters"] input') as HTMLInputElement;
    box.checked = true;
    box.dispatchEvent(new Event('change'));
    const sent = store.calls.find((c) => c.name === 'setLayers');
    expect(sent!.args[0]).toEqual(['clusters']);
  });

  it('names a filter layer in a note and gives it no checkbox', async () => {
    const host = await mount('<tessera-layer-picker></tessera-layer-picker>');
    const venues = {...layer('venues'), title: 'Venues', computedContent: []};
    const store = fakeStore({meta: {...META, layers: [...META.layers, venues]}, status: status({})});
    (host.querySelector('tessera-layer-picker') as unknown as {store: unknown}).store = store;
    await settle(host);
    expect(deepAll(host, '[part="entry"]').map((e) => e.getAttribute('data-layer'))).toEqual(['clusters', 'districts']);
    expect(deep(host, '[part="note"]')?.getAttribute('data-layers')).toBe('venues');
    expect(deepAll(host, 'input[type="checkbox"]')).toHaveLength(2);
  });
});

describe('<tessera-artifact-list>', () => {
  it('lists the deepest artifact of each branch, largest first, each naming its parent, and a press asks to fit the map to it', async () => {
    const host = await mount('<tessera-artifact-list></tessera-artifact-list>');
    const served = [artifact(1n, 100n, null, ['Alpha']), {...artifact(2n, 40n, 1n), rung: 1}, {...artifact(3n, 60n, 1n), rung: 1}, artifact(4n, 5n)];
    const store = fakeStore({meta: META, status: status({}), artifacts: artifactsProjection(served)});
    (host.querySelector('tessera-artifact-list') as unknown as {store: unknown}).store = store;
    await settle(host);
    const rows = deepAll(host, '[part="item"]');
    expect(rows.map((r) => r.getAttribute('data-id'))).toEqual(['3', '2', '4']);
    expect(deep(host, '[part="item"][data-id="3"] [part="parent"]')?.textContent).toBe('Alpha');
    expect(deep(host, '[part="item"][data-id="4"] [part="parent"]')).toBeNull();
    // The counts are the aggregate's, under the store's filters, over the artifacts listed.
    expect(deepText(deep(host, '[part="item"][data-id="3"] [part="count"]')).trim()).toBe('');
    expect([...registered(store).values()]).toEqual([{groupings: [{by: {layer: 'clusters', artifacts: [2n, 3n, 4n]}}]}]);
    // Not coloured by the layer listed, so the rows carry no swatch.
    expect(deepAll(host, '[part="swatch"]')).toHaveLength(0);
    answerAggregate(store, 'in-view', aggregateEntry([{rows: [{key: 3n, count: 6}, {key: 2n, count: 9}, {key: 4n, count: 0}]}]));
    await settle(host);
    expect(deepText(deep(host, '[part="item"][data-id="3"] [part="count"]')).trim()).toBe('6');
    // A cluster none of whose members pass the filters is greyed.
    expect(deepAll(host, '[part="item"][data-empty]').map((r) => r.getAttribute('data-id'))).toEqual(['4']);
    // Largest first by the exact counts.
    expect(deepAll(host, '[part="item"]').map((r) => r.getAttribute('data-id'))).toEqual(['2', '3', '4']);
    const fits: unknown[] = [];
    host.addEventListener('tessera-artifactfit', (e) => fits.push((e as CustomEvent).detail));
    (deep(host, '[part="item"][data-id="3"]') as HTMLElement).click();
    expect(fits).toEqual([{id: '3'}]);
    expect(store.calls.find((c) => c.name === 'openArtifact')).toBeUndefined();
  });

  it('is one tab stop: the arrow keys, Home and End move among the rows', async () => {
    const host = await mount('<tessera-artifact-list></tessera-artifact-list>');
    const served = [artifact(1n, 100n), artifact(2n, 60n), artifact(3n, 40n)];
    const store = fakeStore({meta: META, status: status({}), artifacts: artifactsProjection(served)});
    (host.querySelector('tessera-artifact-list') as unknown as {store: unknown}).store = store;
    await settle(host);
    const rows = () => deepAll(host, '[part="item"]') as HTMLElement[];
    const tabbable = () => rows().map((r) => r.tabIndex);
    expect(tabbable()).toEqual([0, -1, -1]);
    const press = async (key: string) => {
      const active = rows().find((r) => r.tabIndex === 0)!;
      active.dispatchEvent(new KeyboardEvent('keydown', {key, bubbles: true, composed: true, cancelable: true}));
      await settle(host);
    };
    await press('ArrowDown');
    expect(tabbable()).toEqual([-1, 0, -1]);
    await press('End');
    expect(tabbable()).toEqual([-1, -1, 0]);
    await press('Home');
    expect(tabbable()).toEqual([0, -1, -1]);
  });

  it('lists the level the map draws: each branch cut at it, or on a levelled layer that level alone', async () => {
    const host = await mount('<tessera-artifact-list level="0"></tessera-artifact-list>');
    const el = host.querySelector('tessera-artifact-list') as unknown as {store: unknown; level: number | null};
    const served = [artifact(1n, 100n, null, ['Alpha']), {...artifact(2n, 40n, 1n), rung: 1}, artifact(4n, 5n)];
    const store = fakeStore({meta: META, status: status({}), artifacts: artifactsProjection(served)});
    el.store = store;
    await settle(host);
    const ids = () => deepAll(host, '[part="item"]').map((r) => r.getAttribute('data-id'));
    expect(ids()).toEqual(['1', '4']);
    el.level = null;
    await settle(host);
    expect(ids()).toEqual(['2', '4']);
    const tiers = {...layer('clusters'), hierarchy: {kind: 'tiered' as const, pruneChildren: false}, levels: [{level: 0, title: null, zoom: null}, {level: 1, title: null, zoom: null}]};
    store.set('meta', {...META, layers: [tiers, layer('labels', ['clusters'])]});
    // A tiered layer's coarse artifact with no child served is not listed at the finer level.
    el.level = 1;
    await settle(host);
    expect(ids()).toEqual(['2']);
  });

  it('lists the colouring layer’s artifacts while the points are coloured by a layer, drawn or not', async () => {
    const host = await mount('<tessera-artifact-list></tessera-artifact-list>');
    const colour = [{...artifact(7n, 12n, null, ['Coloured']), layer: 'districts'}];
    const store = fakeStore({
      meta: META,
      status: status({}),
      artifacts: {...artifactsProjection([artifact(1n, 100n, null, ['Drawn'])]), colourServed: colour},
      legend: {ranks: {}, domains: {}, categories: {}, categoryErrors: {}, colourBy: 'cluster:districts', samples: {}, missing: {}, sizeBy: null}
    });
    (host.querySelector('tessera-artifact-list') as unknown as {store: unknown}).store = store;
    await settle(host);
    expect(deepAll(host, '[part="item"]').map((r) => r.getAttribute('data-id'))).toEqual(['7']);
    expect(deepAll(host, '[part="swatch"]')).toHaveLength(1);
  });

  it('puts a member_of clause on a row’s artifact from its Highlight and Filter buttons, named by the row, without fitting', async () => {
    const host = await mount('<tessera-artifact-list></tessera-artifact-list>');
    const store = fakeStore({meta: META, status: status({}), artifacts: artifactsProjection([artifact(1n, 100n, null, ['Alpha'])])});
    (host.querySelector('tessera-artifact-list') as unknown as {store: unknown}).store = store;
    await settle(host);
    const fits: unknown[] = [];
    host.addEventListener('tessera-artifactfit', (e) => fits.push(e));
    (deep(host, '[part="item"] [part="filter"]') as HTMLButtonElement).click();
    expect(store.calls.find((c) => c.name === 'setMembers')!.args[0]).toEqual([{layer: 'clusters', artifact: 1n, outside: false, verb: 'filter', label: 'Alpha'}]);
    expect(fits).toEqual([]);
    store.set('filters', {...store.get('filters'), members: [{layer: 'clusters', artifact: 1n, outside: false, verb: 'highlight'}]});
    await settle(host);
    expect(deep(host, '[part="item"]')!.getAttribute('data-clause')).toBe('highlight');
    expect(deep(host, '[part="item"] [part="highlight"]')!.getAttribute('aria-pressed')).toBe('true');
  });

  it('shows a count and a neutral placeholder where a row has no name — never the key', async () => {
    const host = await mount('<tessera-artifact-list></tessera-artifact-list>');
    // `c-2` has no supplied text and no topic attached: its key is an id and is not a name.
    const served = [artifact(1n, 100n, null, ['Alpha']), artifact(2n, 40n)];
    const store = fakeStore({meta: META, status: status({}), artifacts: artifactsProjection(served)});
    (host.querySelector('tessera-artifact-list') as unknown as {store: unknown}).store = store;
    await settle(host);
    const nameless = deep(host, '[part="item"][data-id="2"] [part="name"]');
    expect(nameless?.textContent?.trim()).toBe(UNNAMED);
    expect(nameless?.hasAttribute('data-unnamed')).toBe(true);
    answerAggregate(store, 'in-view', aggregateEntry([{rows: [{key: 1n, count: 70}, {key: 2n, count: 40}]}]));
    await settle(host);
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
    expect(deep(host, '[part="state"]')?.getAttribute('data-state')).toBe('empty');
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
    const host = await mount('<tessera-artifact-card></tessera-artifact-card>');
    const store = fakeStore({meta: META, status: status({})});
    (host.querySelector('tessera-artifact-card') as unknown as {store: unknown}).store = store;
    store.set('selection', {item: null, itemRefusal: null, artifact: null, artifactRefusal: {code: 'not-found', detail: 'no'}});
    await settle(host);
    expect(deep(host, '[part="state"]')?.getAttribute('data-state')).toBe('refused');
    expect(deep(host, '[part="refusal"]')).not.toBeNull();
  });
});

describe('<tessera-legend limit>', () => {
  it('names the first entries and shows the rest when "N more" is pressed', async () => {
    const values = Array.from({length: 7}, (_, i) => ({code: i + 1, key: `k${i}`, title: `Value ${i}`}));
    const store = fakeStore({
      meta: META,
      status: status({}),
      legend: {ranks: {archive: Object.fromEntries(values.map((v, i) => [v.code, i]))}, domains: {}, categories: {archive: values}, categoryErrors: {}, colourBy: 'archive', samples: {}, missing: {}, sizeBy: null}
    });
    const host = await mount('<tessera-legend limit="4"></tessera-legend>');
    (host.querySelector('tessera-legend') as unknown as {store: unknown}).store = store;
    await settle(host);
    // Seven values, all within the palette, so no entry for the rest; cut to four.
    expect(deepAll(host, '[part="swatch"]')).toHaveLength(4);
    (deep(host, '[part="more"]') as HTMLButtonElement).click();
    await settle(host);
    expect(deepAll(host, '[part="swatch"]')).toHaveLength(7);
    expect(deep(host, '[part="more"]')).toBeNull();
  });
});

describe('<tessera-legend selectable>', () => {
  it('offers the clusters of every layer that can colour, drawn or not, and choosing one draws nothing', async () => {
    const host = await mount('<tessera-legend selectable readout></tessera-legend>');
    const store = fakeStore({meta: META, status: status({})});
    (host.querySelector('tessera-legend') as unknown as {store: unknown}).store = store;
    await settle(host);
    (deep(host, '[part="colour-by"]') as HTMLButtonElement).click();
    await settle(host);
    const colourOptions = () => deepAll(host, '[part="option"]').map((o) => o.getAttribute('data-value'));
    // No layer is drawn. A labels layer has no clusters of its own, so it is not offered.
    expect(colourOptions()).toEqual(['', 'cluster:clusters', 'cluster:districts', 'archive']);
    (deep(host, '[part="option"][data-value="cluster:clusters"]') as HTMLButtonElement).click();
    expect(store.calls.find((c) => c.name === 'setColourBy')?.args[0]).toBe('cluster:clusters');
    expect(store.calls.filter((c) => c.name === 'setLayers')).toHaveLength(0);
    // The store answers with the colour layer's rows and still draws nothing: the swatches are
    // the colour layer's served artifacts, and the choice shows in the menu and on its button.
    store.set('legend', {ranks: {}, domains: {}, categories: {}, categoryErrors: {}, colourBy: 'cluster:clusters', samples: {}, missing: {}, sizeBy: null});
    store.set('artifacts', {...artifactsProjection([], []), colourServed: [artifact(1n, 100n)]});
    await settle(host);
    expect(deepAll(host, '[part="swatch"]').length).toBe(2); // the served artifact and the neutral
    // The choice closed the menu; it shows on the button, and as checked when the menu opens again.
    expect(deep(host, '[part="option"]')).toBeNull();
    expect(deep(host, '[part="colour-by"]')?.textContent?.trim()).toBe('clusters');
    (deep(host, '[part="colour-by"]') as HTMLButtonElement).click();
    await settle(host);
    expect(deep(host, '[part="option"][aria-checked="true"]')?.getAttribute('data-value')).toBe('cluster:clusters');
  });

  it('is a readout without selectable', async () => {
    const host = await mount('<tessera-legend></tessera-legend>');
    const store = fakeStore({meta: META, status: status({})});
    (host.querySelector('tessera-legend') as unknown as {store: unknown}).store = store;
    await settle(host);
    expect(deep(host, '[part="colour-by"]')).toBeNull();
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
    // A root of a flat layer has no parent, and the card draws no parents section for it.
    expect(deep(host, '[part="parents"]')).toBeNull();
  });

  it('puts no label on a clause made from an artifact with no name', async () => {
    const host = await mount('<tessera-artifact-card></tessera-artifact-card>');
    const store = fakeStore({meta: META, status: status({}), artifacts: artifactsProjection([artifact(1n, 100n)])});
    (host.querySelector('tessera-artifact-card') as unknown as {store: unknown}).store = store;
    store.set('selection', {item: null, itemRefusal: null, artifact: {id: 1n, detail: {layer: 'clusters', key: 'c-1', maskedCount: 100n, centroid: null, box: null, shape: null}}, artifactRefusal: null});
    await settle(host);
    (deep(host, '[part="filter"]') as HTMLButtonElement).click();
    const [clause] = store.calls.find((c) => c.name === 'setMembers')!.args[0] as {label?: string}[];
    expect(clause).not.toHaveProperty('label');
  });

  it('opens a parent or a child row on Enter and on Space, as on a click', async () => {
    const host = await mount('<tessera-artifact-card></tessera-artifact-card>');
    const served = [artifact(1n, 100n, null, ['Alpha']), artifact(2n, 40n, 1n, ['Beta']), artifact(3n, 10n, 2n, ['Gamma'])];
    const store = fakeStore({meta: META, status: status({}), artifacts: artifactsProjection(served)});
    (host.querySelector('tessera-artifact-card') as unknown as {store: unknown}).store = store;
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
    // A slot assigned an empty slot counts as filled and would hide the fallback, leaving an
    // empty box beside the pointer.
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
    expect(fitted).toEqual([1n]);
    fitted.length = 0;
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

describe('<tessera-artifact-list rows>', () => {
  it('offers the rest under "N more", and starts cut again when the layers change', async () => {
    const host = await mount('<tessera-artifact-list rows="2"></tessera-artifact-list>');
    const store = fakeStore({meta: META, status: status({}), artifacts: artifactsProjection([artifact(1n, 30n), artifact(2n, 20n), artifact(3n, 10n)])});
    (host.querySelector('tessera-artifact-list') as unknown as {store: unknown}).store = store;
    await settle(host);
    expect(deepAll(host, '[part="item"]')).toHaveLength(2);
    (deep(host, '[part="more"]') as HTMLButtonElement).click();
    await settle(host);
    expect(deepAll(host, '[part="item"]')).toHaveLength(3);
    expect(deep(host, '[part="more"]')).toBeNull();
    store.set('artifacts', artifactsProjection([artifact(1n, 30n), artifact(2n, 20n), artifact(3n, 10n)], ['districts']));
    await settle(host);
    expect(deepAll(host, '[part="item"]')).toHaveLength(2);
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
    expect(await code('<tessera-item-card></tessera-item-card>', {selection: {...selection, itemRefusal: refusal}})).toBe('withheld');
    expect(await code('<tessera-artifact-card></tessera-artifact-card>', {selection: {...selection, artifactRefusal: refusal}})).toBe('withheld');
    expect(await code('<tessera-artifact-list></tessera-artifact-list>', {artifacts: {...artifactsProjection([]), status: 'refused', refusal}})).toBe('withheld');
    expect(await code('<tessera-legend></tessera-legend>', {legend: {ranks: {}, domains: {}, categories: {}, categoryErrors: {archive: refusal}, colourBy: 'archive', samples: {}, missing: {}, sizeBy: null}})).toBe('withheld');
    const box = {kind: 'box' as const, bbox: [0, 0, 1, 1] as [number, number, number, number]};
    const held = {ids: new BigUint64Array(0), positions: new Float32Array(0), count: 0};
    expect(await code('<tessera-selection></tessera-selection>', {region: {shape: box, status: 'refused', refusal, visible: null, matched: {value: 0, exact: false}, served: {shown: 0, total: 0, exact: false}, verdict: null, held}})).toBe('withheld');
    expect(await code('<tessera-status></tessera-status>', {status: status({status: 'refused', refusal})})).toBe('withheld');
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

  it('in the legend: the label names its row, and a row whose label is withheld is unnamed', async () => {
    const {host} = await shown('<tessera-legend readout></tessera-legend>');
    const names = deepAll(host, '[part="entry"] [part="name"]');
    expect(names.map((n) => n.textContent)).toEqual(['spin magnetic effect', UNNAMED, 'Not yet coloured']);
    expect(names.map((n) => n.hasAttribute('data-unnamed'))).toEqual([false, true, false]);
  });

  it('in the artifact list', async () => {
    const {host} = await shown('<tessera-artifact-list></tessera-artifact-list>');
    expect(deep(host, '[part="item"][data-id="1"] [part="name"]')?.textContent).toBe('spin magnetic effect');
    expect(deep(host, '[part="item"][data-id="2"] [part="name"]')?.textContent).toBe(UNNAMED);
    expect(deep(host, '[part="item"][data-id="2"] [part="name"]')?.hasAttribute('data-unnamed')).toBe(true);
    // The label is the cluster's name, not a row of its own.
    expect(deep(host, '[part="item"][data-id="9"]')).toBeNull();
  });

  it('lists a drawn dependent layer’s rows where only the colour layer’s labels are served', async () => {
    const host = await mount('<tessera-artifact-list></tessera-artifact-list>');
    // The drawn label on 1 carries no text; the text in `attached` is the colour layer's, on 5.
    const bare: Artifact = {...artifact(9n, 100n), layer: 'labels', target: 1n};
    const store = fakeStore({meta: META, status: status({}), artifacts: {...artifactsProjection([artifact(1n, 100n), bare]), attached: new Map([[5n, 'a colour label']])}});
    (host.querySelector('tessera-artifact-list') as unknown as {store: unknown}).store = store;
    await settle(host);
    expect(deep(host, '[part="item"][data-id="9"]')).not.toBeNull();
  });

  it('in the artifact card, and on the clause the card makes', async () => {
    const {host, store} = await shown('<tessera-artifact-card></tessera-artifact-card>');
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
