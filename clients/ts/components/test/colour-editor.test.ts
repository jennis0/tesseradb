import {afterEach, describe, expect, it} from 'vitest';
import type {Meta} from '@tesseradb/client';
import '../src/colour-editor.js';
import type {TesseraColourEditor} from '../src/colour-editor.js';
import {aggregateEntry, answerAggregate, deep, deepAll, fakeStore, meta, mount, registered, scalar, settle, status} from './fake-store.js';

afterEach(() => {
  document.body.innerHTML = '';
});

const layer = (name: string, kind: string, levels: {level: number; title: string}[] = []) =>
  ({name, title: name === 'topics' ? 'Topics' : name, views: ['s0'], membership: 'enumerated', hierarchy: {kind, pruneChildren: false}, levels, computedContent: ['centroid'], shape: null, suppliedContent: ['name'], depsOn: [], version: 1}) as unknown as Meta['layers'][number];

const META = meta({
  declaredScalars: [scalar('archive', 'u16', {category: {vocabulary: 'a', kind: 'declared', visibility: 'public'}, render: true, homes: ['rendered']})],
  filterOperands: [{column: 'archive', family: 'category', operands: ['in']}],
  layers: [layer('topics', 'nested'), layer('bands', 'stacked', [{level: 0, title: 'Broad'}, {level: 1, title: 'Fine'}])]
});

async function open(field: string, attrs = '') {
  const host = await mount(`<button id="opener">Edit colours</button><tessera-colour-editor field="${field}" ${attrs}></tessera-colour-editor>`);
  const editor = host.querySelector('tessera-colour-editor') as TesseraColourEditor;
  const store = fakeStore({meta: META, status: status({})});
  store.set('view', {...store.get('view'), id: 's0'});
  editor.store = store;
  await settle(host);
  (host.querySelector('#opener') as HTMLButtonElement).focus();
  await editor.show();
  await settle(host);
  return {host, editor, store};
}

const specOf = (store: ReturnType<typeof fakeStore>, prefix: string) => [...registered(store)].find(([id]) => id.startsWith(prefix))?.[1];
const rowsOf = (host: HTMLElement) => deepAll(host, '[part~="row"]') as HTMLElement[];
const names = (host: HTMLElement) => rowsOf(host).map((r) => [r.dataset.key, r.querySelector('[part="name"]')!.textContent, r.querySelector('[part="count"]')!.textContent]);
const settleTwice = async (host: HTMLElement) => {
  for (let i = 0; i < 3; i++) {
    await new Promise((r) => setTimeout(r, 0));
    await settle(host);
  }
};

/** A tree layer's drawn cut, counted over everything the viewer may see. */
const CUT = [
  {key: 7n, count: 900, title: 'optics', slot: 2},
  {key: 8n, count: 500, title: 'lasers', slot: 0},
  {key: 9n, count: 40, title: 'fibres', slot: 1}
];

describe('<tessera-colour-editor> on a tree layer', () => {
  it('lists the clusters of the drawn cut by their counts over everything visible, as a modal dialog named by the layer', async () => {
    const {host, store} = await open('cluster:topics');
    expect(specOf(store, 'colours-ranked')).toEqual({groupings: [{by: {layer: 'topics', top: 1000, cut: 'drawn', paletteSize: 'drawn'}}], subject: 'visible'});
    answerAggregate(store, 'colours-ranked', aggregateEntry([{rows: CUT, groups: 3}], 's0', undefined, 'tableau10'));
    await settle(host);
    const dialog = deep(host, '[part="dialog"]') as HTMLDialogElement;
    expect(dialog.open).toBe(true);
    expect(deep(host, `#${dialog.getAttribute('aria-labelledby')}`)!.textContent).toBe('Topics');
    expect(deep(host, '[part="sub"]')!.textContent).toBe('Clusters drawn · Tableau 10');
    expect(names(host)).toEqual([
      ['7', 'optics', '900'],
      ['8', 'lasers', '500'],
      ['9', 'fibres', '40']
    ]);
    // Each swatch is its slot's colour: slot 2 of Tableau 10.
    expect(rowsOf(host)[0]!.querySelector('[part="swatch"]')!.getAttribute('style')).toContain('#e15759');
    expect(deep(host, 'tessera-cluster-filter')!.getAttribute('placeholder')).toBe('Search 3 clusters');
  });

  it('keeps one request however the filters, the highlight and the camera change, so the order holds', async () => {
    const {host, store} = await open('cluster:topics');
    const before = specOf(store, 'colours-ranked');
    store.set('filters', {...store.get('filters'), draft: {filter: {archive: {family: 'category', keys: ['cs']}}, highlight: {}}, members: [{layer: 'topics', artifact: 7n, outside: false, verb: 'filter'}]});
    store.set('view', {...store.get('view'), inView: {status: 'shown', visible: {value: 9, exact: true}, matched: {value: 9, exact: true}, highlighted: {value: 9, exact: true}, shown: 9}});
    await settle(host);
    expect(specOf(store, 'colours-ranked')).toEqual(before);
    expect(store.calls.filter((c) => c.name === 'setAggregate' && String(c.args[0]).startsWith('colours-ranked'))).toHaveLength(1);
  });

  it('finds a cluster outside the listed rows with the search box over the whole tree, and lists it first with its count', async () => {
    const {host, store} = await open('cluster:topics');
    answerAggregate(store, 'colours-ranked', aggregateEntry([{rows: CUT, groups: 3}], 's0', undefined, 'tableau10'));
    store.setBrowse('q:holo', {artifacts: [{tesseraId: 42n, key: null, name: 'holography', maskedCount: 3n, matchedCount: null, rung: 4, parentIds: [], childCount: 0, slot: null}], parents: [], next: null});
    await settle(host);
    const input = deep(host, 'tessera-cluster-filter')!.shadowRoot!.querySelector('[part="entry"]') as HTMLInputElement;
    input.focus();
    input.value = 'holo';
    input.dispatchEvent(new Event('input'));
    await new Promise((r) => setTimeout(r, 300));
    await settle(host);
    expect(store.calls.some((c) => c.name === 'browse' && (c.args[0] as {q?: string}).q === 'holo')).toBe(true);
    (deep(host, '[part="option"][data-id="42"]') as HTMLButtonElement).click();
    await settle(host);
    // Choosing it puts no clause on.
    expect(store.calls.filter((c) => c.name === 'setMembers')).toEqual([]);
    expect(specOf(store, 'colours-found')).toEqual({groupings: [{by: {layer: 'topics', artifacts: [42n], paletteSize: 'drawn'}}], subject: 'visible'});
    answerAggregate(store, 'colours-found', aggregateEntry([{rows: [{key: 42n, count: 3, slot: 4}]}], 's0', undefined, 'tableau10'));
    await settleTwice(host);
    expect(names(host)[0]).toEqual(['42', 'holography', '3']);
    expect(rowsOf(host)[0]!.hasAttribute('data-found')).toBe(true);
    expect(rowsOf(host)).toHaveLength(4);
  });

  it('gives every ticked cluster the colour chosen with Set colour, as one change', async () => {
    const {host, store} = await open('cluster:topics');
    answerAggregate(store, 'colours-ranked', aggregateEntry([{rows: CUT, groups: 3}], 's0', undefined, 'tableau10'));
    await settle(host);
    const seen: unknown[] = [];
    host.addEventListener('tessera-clustercolour', (e) => seen.push((e as CustomEvent).detail));
    for (const i of [0, 2]) {
      const box = rowsOf(host)[i]!.querySelector('[part="check"]') as HTMLInputElement;
      box.checked = true;
      box.dispatchEvent(new Event('change'));
      await settle(host);
    }
    expect(deep(host, '[part="selected"]')!.textContent).toContain('2 selected');
    (deep(host, '[part="set-colour"]') as HTMLButtonElement).click();
    await settle(host);
    (deep(host, '[part="hex"]') as HTMLInputElement).value = '#123456';
    deep(host, '[part="hex"]')!.dispatchEvent(new Event('change'));
    await settle(host);
    expect(seen).toEqual([
      {
        layer: 'topics',
        changes: [
          {tesseraId: '7', colour: '#123456'},
          {tesseraId: '9', colour: '#123456'}
        ]
      }
    ]);
    expect([...store.get('artifacts').overrides.keys()].sort()).toEqual([7n, 9n]);
    expect(rowsOf(host).map((r) => r.querySelector('[part="swatch"]')!.getAttribute('style')!.includes('#123456'))).toEqual([true, false, true]);
    expect(deep(host, '[part="changed"]')!.textContent).toBe('2 colours changed');
    (deep(host, '[part="deselect"]') as HTMLButtonElement).click();
    await settle(host);
    expect(deep(host, '[part="selected"]')).toBeNull();
  });

  it('resets every colour chosen for the layer’s clusters with one change, and none of another layer’s', async () => {
    const {host, store} = await open('cluster:topics');
    const red = [200, 0, 0, 220] as const;
    // 7 is listed; 12 is the layer's but not drawn now; 99 is another layer's.
    store.setArtifactColours(new Map([[7n, red], [12n, red], [99n, red]]));
    answerAggregate(store, 'colours-ranked', aggregateEntry([{rows: CUT, groups: 3}], 's0', undefined, 'tableau10'));
    await settle(host);
    expect(specOf(store, 'colours-chosen')).toEqual({groupings: [{by: {layer: 'topics', artifacts: [7n, 12n, 99n]}}], subject: 'visible'});
    answerAggregate(store, 'colours-chosen', aggregateEntry([{rows: [{key: 7n, count: 900}, {key: 12n, count: 4}]}]));
    await settle(host);
    expect(deep(host, '[part="changed"]')!.textContent).toBe('2 colours changed');
    const seen: unknown[] = [];
    host.addEventListener('tessera-clustercolour', (e) => seen.push((e as CustomEvent).detail));
    (deep(host, '[part="reset-all"]') as HTMLButtonElement).click();
    await settle(host);
    expect(seen).toHaveLength(1);
    expect(new Set((seen[0] as {changes: unknown[]}).changes)).toEqual(
      new Set([
        {tesseraId: '7', colour: null},
        {tesseraId: '12', colour: null}
      ])
    );
    expect([...store.get('artifacts').overrides.keys()]).toEqual([99n]);
    expect((deep(host, '[part="reset-all"]') as HTMLButtonElement).disabled).toBe(true);
  });

  it('closes on Escape and on Done, gives focus back to what opened it, and stops asking', async () => {
    const {host, editor, store} = await open('cluster:topics');
    const dialog = deep(host, '[part="dialog"]') as HTMLDialogElement;
    dialog.dispatchEvent(new KeyboardEvent('keydown', {key: 'Escape', bubbles: true, composed: true}));
    await settle(host);
    expect(dialog.open).toBe(false);
    expect(document.activeElement).toBe(host.querySelector('#opener'));
    expect(specOf(store, 'colours-ranked')).toBeUndefined();
    await editor.show();
    await settle(host);
    expect(dialog.open).toBe(true);
    (deep(host, '[part="done"]') as HTMLButtonElement).click();
    await settle(host);
    expect(dialog.open).toBe(false);
  });
});

describe('<tessera-colour-editor> on a levelled layer', () => {
  it('lists the clusters of the level coloured, the deepest unless one is set', async () => {
    const deepest = await open('cluster:bands');
    expect(specOf(deepest.store, 'colours-ranked')).toEqual({groupings: [{by: {layer: 'bands', level: 1, top: 1000, paletteSize: 'drawn'}}], subject: 'visible'});
    expect(deep(deepest.host, '[part="sub"]')!.textContent).toBe('Fine · Tableau 10');
    document.body.innerHTML = '';
    const broad = await open('cluster:bands', 'cluster-level="0"');
    expect(specOf(broad.store, 'colours-ranked')).toEqual({groupings: [{by: {layer: 'bands', level: 0, top: 1000, paletteSize: 'drawn'}}], subject: 'visible'});
  });
});

describe('<tessera-colour-editor> on a category', () => {
  it('lists the values by their counts over everything visible, and gives one the colour chosen in its picker', async () => {
    const {host, store} = await open('archive');
    expect(specOf(store, 'colours-ranked')).toEqual({groupings: [{by: {field: 'archive', top: 1000}}], subject: 'visible'});
    answerAggregate(store, 'colours-ranked', aggregateEntry([{rows: [{key: 'cs', title: 'Computer science', count: 70}, {key: 'math', count: 30}], groups: 2}]));
    await settle(host);
    expect(names(host)).toEqual([
      ['cs', 'Computer science', '70'],
      ['math', 'math', '30']
    ]);
    const seen: unknown[] = [];
    host.addEventListener('tessera-valuecolour', (e) => seen.push((e as CustomEvent).detail));
    (rowsOf(host)[1]!.querySelector('[part="swatch"]') as HTMLButtonElement).click();
    await settle(host);
    expect(deep(host, '[part="colour-popover"]')!.getAttribute('aria-label')).toBe('Colour of math');
    const choice = deepAll(host, '[part="choice"]')[3] as HTMLButtonElement;
    const hex = /#[0-9a-f]{6}/.exec(choice.getAttribute('aria-label')!)![0];
    choice.click();
    await settle(host);
    expect(seen).toEqual([{column: 'archive', changes: [{value: 'math', colour: hex}]}]);
    expect(rowsOf(host)[1]!.querySelector('[part="swatch"]')!.getAttribute('style')).toContain(hex);
    expect(deep(host, '[part="changed"]')!.textContent).toBe('1 colour changed');
    (deep(host, '[part="reset-all"]') as HTMLButtonElement).click();
    await settle(host);
    expect(seen.at(-1)).toEqual({column: 'archive', changes: [{value: 'math', colour: null}]});
  });
});
