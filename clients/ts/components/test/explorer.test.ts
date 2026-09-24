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
  it('forwards every part each inner element renders, under a name prefixed by the element', async () => {
    const {shadow} = await explorer();
    const inner = [...shadow.querySelectorAll('*')].filter((e) => e.tagName.startsWith('TESSERA-'));
    expect(inner.length).toBeGreaterThan(5);
    for (const el of inner) {
      const map = forwardedBy(el);
      const prefix = el.tagName.toLowerCase().replace(/^tessera-/, '');
      for (const part of partsIn(el.shadowRoot!)) expect(map.get(part), `${prefix} renders ${part}`).toBe(`${prefix}-${part}`);
    }
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
