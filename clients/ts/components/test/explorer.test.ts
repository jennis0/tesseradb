import {afterEach, describe, expect, it} from 'vitest';
import type {Meta} from '@tesseradb/client';
import '../src/explorer.js';
import {fakeStore, mount, settle, status} from './fake-store.js';

afterEach(() => {
  document.body.innerHTML = '';
});

const META: Meta = {
  apiVersion: 1,
  idset: 0,
  views: [{id: 's0', displayName: 'default', quantisation: {xMin: 0, xMax: 1, yMin: 0, yMax: 1}, projection: 'none', worldAspect: null, tileScheme: null, tile: null, roster: null}],
  groups: [],
  declaredScalars: [
    {name: 'archive', arrowType: 'u16', category: {vocabulary: 'a', kind: 'declared', visibility: 'public'}, render: true, index: true},
    {name: 'author', arrowType: 'utf8', category: null, render: false, index: true}
  ],
  layers: [],
  selection: {kMin: 1, kMaxMarks: 500, maxK: 5000, thetaTargetMarks: 10, maxUnderlayOffset: 0, maxCategoryValues: 1000, maxRegionVertices: 10_000, maxRegionCells: 262_144, maxBrowseRows: 200},
  maxTilesPerRequest: 4096,
  filterOperands: [
    {column: 'archive', family: 'category', operands: ['in']},
    {column: 'author', family: 'keyword', operands: ['eq', 'prefix']}
  ]
};

async function explorer(markup = '<tessera-explorer></tessera-explorer>') {
  const host = await mount(markup);
  const el = host.querySelector('tessera-explorer') as HTMLElement & {store: unknown};
  const store = fakeStore({meta: META, status: status({})});
  el.store = store;
  await settle(host);
  return {host, el, store, shadow: el.shadowRoot!};
}

/** The names an element's `exportparts` forwards, inner name to outer name. */
function forwardedBy(el: Element): Map<string, string> {
  const out = new Map<string, string>();
  for (const entry of (el.getAttribute('exportparts') ?? '').split(',')) {
    const [inner, outer] = entry.split(':').map((s) => s.trim());
    if (inner) out.set(inner, outer || inner);
  }
  return out;
}

const partsIn = (root: ParentNode): string[] => [...root.querySelectorAll('[part]')].flatMap((e) => e.getAttribute('part')!.split(/\s+/));

describe('<tessera-explorer> parts', () => {
  it('forwards every part each inner element renders, under a name prefixed by the element, in every state', async () => {
    const mesh = {name: 'mesh', title: 'mesh', views: ['s0'], membership: 'enumerated', hierarchy: {kind: 'dag', pruneChildren: false}, levels: [], computedContent: ['centroid'], shape: null, suppliedContent: ['name'], depsOn: [], version: 1} as unknown as Meta['layers'][number];
    const host = await mount('<tessera-explorer></tessera-explorer>');
    const el = host.querySelector('tessera-explorer') as HTMLElement & {store: unknown};
    const store = fakeStore({meta: {...META, layers: [mesh]}, status: status({})});
    store.setBrowse('roots', {artifacts: [{tesseraId: 1n, key: 'k-1', name: 'Neoplasms', maskedCount: 9n, matchedCount: null, rung: 0, parentIds: []}], parents: [], next: 'more'});
    el.store = store;
    await settle(host);
    const shadow = el.shadowRoot!;
    const seen = new Set<string>();
    const check = async () => {
      await settle(host);
      await settle(host);
      for (const inner of [...shadow.querySelectorAll('*')].filter((e) => e.tagName.startsWith('TESSERA-'))) {
        const map = forwardedBy(inner);
        const prefix = inner.tagName.toLowerCase().replace(/^tessera-/, '');
        for (const part of partsIn(inner.shadowRoot!)) {
          seen.add(`${prefix}-${part}`);
          expect(map.get(part), `${prefix} renders ${part}`).toBe(`${prefix}-${part}`);
        }
      }
    };
    await check();
    // Each state below renders an element or a part the one before did not.
    store.set('selection', {item: {id: 5n, detail: {fields: {author: 'Ada'}, externalId: null, labels: [], views: [], scoped: {}}}, itemRefusal: null, artifact: null, artifactRefusal: null});
    await check();
    store.set('selection', {item: null, itemRefusal: null, artifact: {id: 1n, detail: {layer: 'mesh', key: 'k-1', maskedCount: 9n, centroid: null, box: null, shape: null}}, artifactRefusal: null});
    await check();
    store.set('region', {shape: {kind: 'box', bbox: [0, 0, 1, 1]}, status: 'shown', refusal: null, visible: {value: 3, exact: true}, matched: {value: 3, exact: true}, served: {shown: 1, total: 3, exact: true}, verdict: {exact: true, depth: null}, held: {ids: BigUint64Array.of(5n), positions: new Float32Array(2), count: 1}});
    await check();
    store.set('status', status({status: 'refused', refusal: {code: 'unauthorised', detail: ''}}));
    await check();
    store.set('status', status({stale: true}));
    await check();
    for (const s of shadow.querySelectorAll('tessera-status')) (s as unknown as {reauthorise: () => void}).reauthorise = () => {};
    store.set('status', status({status: 'refused', refusal: {code: 'expired-token', detail: ''}, expired: true}));
    await check();
    for (const part of ['item-card-headline', 'artifact-card-headline', 'hierarchy-row', 'selection-items', 'status-refusal', 'map-refusal', 'status-refresh', 'status-reauthorise']) expect(seen, part).toContain(part);
  });

  it('forwards the filter controls’ parts through the filter panel', async () => {
    const {shadow} = await explorer();
    const panel = shadow.querySelector('tessera-filter-panel')!;
    const outer = forwardedBy(panel);
    const filters = [...panel.shadowRoot!.querySelectorAll('tessera-filter')];
    expect(filters.length).toBe(2);
    for (const f of filters) {
      const map = forwardedBy(f);
      for (const part of partsIn(f.shadowRoot!)) {
        expect(map.get(part)).toBe(`filter-${part}`);
        expect(outer.get(`filter-${part}`)).toBe(`filter-${part}`);
      }
    }
  });
});

describe('<tessera-explorer title-field>', () => {
  it('reaches the map’s hover and the default item card', async () => {
    const {host, shadow, store} = await explorer('<tessera-explorer title-field="author"></tessera-explorer>');
    store.set('selection', {item: {id: 5n, detail: {fields: {author: 'Ada'}, externalId: null, labels: [], views: [], scoped: {}}}, itemRefusal: null, artifact: null, artifactRefusal: null});
    await settle(host);
    expect((shadow.querySelector('tessera-map') as unknown as {titleField: string}).titleField).toBe('author');
    const card = shadow.querySelector('tessera-item-card')!;
    expect(card.shadowRoot!.querySelector('[part="headline"]')?.getAttribute('data-name')).toBe('author');
  });
});

describe('<tessera-explorer> narrow layout', () => {
  const tabs = (shadow: ShadowRoot) => [...shadow.querySelectorAll<HTMLButtonElement>('[part="tabs"] [role="tab"]')];
  const key = (el: Element, k: string) => el.dispatchEvent(new KeyboardEvent('keydown', {key: k, bubbles: true, composed: true, cancelable: true}));

  it('moves between tabs with the arrow keys, Home and End, keeping one tab in the tab order', async () => {
    const {host, shadow} = await explorer();
    const all = tabs(shadow);
    expect(all.map((t) => t.tabIndex)).toEqual([0, -1, -1, -1]);
    all[0]!.focus();
    key(all[0]!, 'ArrowRight');
    await settle(host);
    expect(shadow.activeElement).toBe(tabs(shadow)[1]);
    expect(tabs(shadow).map((t) => t.tabIndex)).toEqual([-1, 0, -1, -1]);
    key(tabs(shadow)[1]!, 'ArrowLeft');
    key(tabs(shadow)[0]!, 'ArrowLeft');
    await settle(host);
    expect(shadow.activeElement).toBe(tabs(shadow)[3]);
    key(tabs(shadow)[3]!, 'Home');
    await settle(host);
    expect(shadow.activeElement).toBe(tabs(shadow)[0]);
    key(tabs(shadow)[0]!, 'End');
    await settle(host);
    expect(shadow.activeElement).toBe(tabs(shadow)[3]);
  });

  it('moves focus into a sheet as it opens, closes it on Escape, and returns focus to its tab', async () => {
    const {host, shadow} = await explorer();
    const filters = tabs(shadow)[0]!;
    filters.click();
    await settle(host);
    const sheet = shadow.querySelector('[part="sheet"]')!;
    expect(sheet.getAttribute('role')).toBe('dialog');
    expect(shadow.activeElement).toBe(sheet);
    expect(filters.getAttribute('aria-selected')).toBe('true');
    key(sheet, 'Escape');
    await settle(host);
    expect(shadow.querySelector('[part="sheet"]')).toBeNull();
    expect(shadow.activeElement).toBe(tabs(shadow)[0]);
  });
});

describe('<tessera-explorer> detail', () => {
  it('drops the selection when the card is closed', async () => {
    const {host, shadow, store} = await explorer();
    store.set('selection', {item: {id: 5n, detail: {fields: {}, externalId: null, labels: [], views: [], scoped: {}}}, itemRefusal: null, artifact: null, artifactRefusal: null});
    await settle(host);
    (shadow.querySelector('tessera-item-card')!.shadowRoot!.querySelector('[part="close"]') as HTMLButtonElement).click();
    expect(store.calls.filter((c) => c.name === 'clearSelection')).toHaveLength(1);
  });
});
