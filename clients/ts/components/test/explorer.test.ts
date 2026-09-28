import {afterEach, describe, expect, it} from 'vitest';
import type {Meta} from '@tesseradb/client';
import '../src/explorer.js';
import {deep, deepAll, fakeStore, mount, settle, status, meta, scalar} from './fake-store.js';

afterEach(() => {
  document.body.innerHTML = '';
});

const META = meta({
  declaredScalars: [
    scalar('archive', 'u16', {category: {vocabulary: 'a', kind: 'declared', visibility: 'public'}, render: true, homes: ['rendered']}),
    scalar('author', 'utf8')
  ],
  filterOperands: [
    {column: 'archive', family: 'category', operands: ['in']},
    {column: 'author', family: 'keyword', operands: ['eq', 'prefix']}
  ]
});

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
    store.set('selection', {item: {id: 5n, detail: {fields: {author: 'Ada'}, labels: [], views: [], scoped: {}}}, itemRefusal: null, artifact: null, artifactRefusal: null});
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
    const {host, shadow, store} = await explorer();
    store.set('filters', {...store.get('filters'), draft: {filter: {archive: {family: 'category', keys: ['cs']}, author: {family: 'keyword', needle: 'Ada', op: 'eq'}}, highlight: {}}});
    await settle(host);
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
    store.set('selection', {item: {id: 5n, detail: {fields: {author: 'Ada'}, labels: [], views: [], scoped: {}}}, itemRefusal: null, artifact: null, artifactRefusal: null});
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

  it('clears the controls and the member_of clauses from the filters sheet', async () => {
    const {host, shadow, store} = await explorer();
    tabs(shadow)[0]!.click();
    await settle(host);
    (shadow.querySelector('.sheet-footer .btn:not(.primary)') as HTMLButtonElement).click();
    expect(store.calls.filter((c) => c.name === 'setFilters')).toHaveLength(1);
    expect(store.calls.filter((c) => c.name === 'setMembers').map((c) => c.args[0])).toEqual([[]]);
  });
});

describe('<tessera-explorer> detail', () => {
  it('drops the selection when the card is closed', async () => {
    const {host, shadow, store} = await explorer();
    store.set('selection', {item: {id: 5n, detail: {fields: {}, labels: [], views: [], scoped: {}}}, itemRefusal: null, artifact: null, artifactRefusal: null});
    await settle(host);
    (shadow.querySelector('tessera-item-card')!.shadowRoot!.querySelector('[part="close"]') as HTMLButtonElement).click();
    expect(store.calls.filter((c) => c.name === 'clearSelection')).toHaveLength(1);
  });
});

describe('<tessera-explorer> layouts', () => {
  it('draws the panels in a sidebar when docked and in one card over the map as an overlay', async () => {
    const docked = await explorer('<tessera-explorer layout="docked"></tessera-explorer>');
    expect(docked.shadow.querySelector('[part="sidebar"] tessera-filter-panel')).not.toBeNull();
    expect(docked.shadow.querySelector('[part="panel"]')).toBeNull();
    document.body.innerHTML = '';
    const overlay = await explorer('<tessera-explorer layout="overlay"></tessera-explorer>');
    expect(overlay.shadow.querySelector('[part="sidebar"]')).toBeNull();
    expect(overlay.shadow.querySelector('[part="panel"] tessera-filter-panel')).not.toBeNull();
    expect(overlay.shadow.querySelector('[part="panel"] tessera-legend')).not.toBeNull();
  });

  it('opens the filter controls beside the card from its Filters button, which counts the clauses applied', async () => {
    const {host, shadow, store} = await explorer('<tessera-explorer layout="overlay"></tessera-explorer>');
    const popover = () => shadow.querySelector<HTMLElement>('[part="filters-popover"]')!;
    const open = () => !popover().hidden;
    const card = () => shadow.querySelector('[part="panel"]')!;
    expect(open()).toBe(false);
    store.set('filters', {...store.get('filters'), draft: {filter: {archive: {family: 'category', keys: ['cs']}}, highlight: {}}});
    await settle(host);
    const toggle = shadow.querySelector<HTMLButtonElement>('[part="filters-toggle"]')!;
    expect(toggle.getAttribute('data-count')).toBe('1');
    // The chips show in the card with the controls closed; the card holds no control.
    expect(deepAll(card(), '[part="chip"]')).toHaveLength(1);
    // A highlight on the same column is a second clause and a second chip.
    store.set('filters', {...store.get('filters'), draft: {filter: {archive: {family: 'category', keys: ['cs']}}, highlight: {archive: {family: 'category', keys: ['cs']}}}});
    await settle(host);
    expect(toggle.getAttribute('data-count')).toBe('2');
    expect(deepAll(card(), '[part="chip"]')).toHaveLength(2);
    // Pressing a chip opens the panel at the chip's control, in the chip's position.
    (deepAll(card(), '[part="chip"][data-verb="highlight"] [part="edit"]')[0] as HTMLButtonElement).click();
    await settle(host);
    await settle(host);
    expect(toggle.getAttribute('aria-expanded')).toBe('true');
    expect(shadow.getElementById(toggle.getAttribute('aria-controls')!)).toBe(popover());
    expect(deepAll(card(), 'tessera-filter')).toHaveLength(0);
    const controls = deepAll(popover(), 'tessera-filter') as (HTMLElement & {column: string; verb: string})[];
    expect(controls.map((c) => [c.column, c.verb])).toEqual([['archive', 'highlight']]);
    // The Filters button closes it and opens it again, with focus in it and its position kept;
    // its close button and Escape close it too, and focus goes back to the button.
    toggle.click();
    await settle(host);
    expect(open()).toBe(false);
    toggle.click();
    await settle(host);
    await settle(host);
    const panel = popover().querySelector('tessera-filter-panel')!;
    expect(panel.getAttribute('mode')).toBe('highlight');
    expect(panel.shadowRoot!.activeElement?.getAttribute('data-verb')).toBe('highlight');
    (popover().querySelector('[part="filters-close"]') as HTMLButtonElement).click();
    await settle(host);
    expect(open()).toBe(false);
    expect(shadow.activeElement).toBe(toggle);
    toggle.click();
    await settle(host);
    popover().dispatchEvent(new KeyboardEvent('keydown', {key: 'Escape', bubbles: true, composed: true}));
    await settle(host);
    expect(open()).toBe(false);
    expect(shadow.activeElement).toBe(toggle);
  });

  it('heads the card with the dataset title over the view’s name, or the view’s name alone', async () => {
    const titled = await explorer('<tessera-explorer layout="overlay" dataset-title="arXiv abstracts"></tessera-explorer>');
    expect(titled.shadow.querySelector('[part="dataset-title"]')?.textContent).toBe('arXiv abstracts');
    expect(titled.shadow.querySelector('[part="view-name"]')?.textContent).toBe('default');
    document.body.innerHTML = '';
    const plain = await explorer('<tessera-explorer layout="overlay"></tessera-explorer>');
    expect(plain.shadow.querySelector('[part="dataset-title"]')).toBeNull();
    expect(plain.shadow.querySelector('[part="view-name"]')?.textContent).toBe('default');
    // With several views the name is the view choice.
    plain.store.set('meta', {...META, views: [...META.views, {...META.views[0]!, id: 's1', displayName: 'other'}]});
    await settle(plain.host);
    expect(plain.shadow.querySelector('[part="view-name"]')).toBeNull();
    expect(plain.shadow.querySelector('tessera-view-picker')?.shadowRoot?.querySelector('select')).not.toBeNull();
  });

  it('shows a selected region in the map’s top-right corner, above the detail card', async () => {
    const {host, shadow, store} = await explorer('<tessera-explorer layout="overlay"></tessera-explorer>');
    store.set('selection', {item: {id: 5n, detail: {fields: {}, labels: [], views: [], scoped: {}}}, itemRefusal: null, artifact: null, artifactRefusal: null});
    store.set('region', {shape: {kind: 'box', bbox: [0, 0, 1, 1]}, status: 'shown', refusal: null, visible: {value: 3, exact: true}, matched: {value: 3, exact: true}, served: {shown: 1, total: 3, exact: true}, verdict: {exact: true, depth: null}, held: {ids: BigUint64Array.of(5n), positions: new Float32Array(2), count: 1}});
    await settle(host);
    const corner = shadow.querySelector('.right')!;
    expect([...corner.children].map((c) => c.getAttribute('part')).filter(Boolean)).toEqual(['selection-card', 'detail']);
    expect(shadow.querySelector('[part="panel"] tessera-selection')).toBeNull();
  });

  it('opens the layer picker from the Layers button, passes its change on, and closes on Escape', async () => {
    const clusters = {name: 'clusters', title: 'Clusters', views: ['s0'], membership: 'enumerated', hierarchy: {kind: 'flat', pruneChildren: false}, levels: [], computedContent: ['centroid'], shape: null, suppliedContent: ['name'], depsOn: [], version: 1} as unknown as Meta['layers'][number];
    const host = await mount('<tessera-explorer></tessera-explorer>');
    const el = host.querySelector('tessera-explorer') as HTMLElement & {store: unknown};
    const store = fakeStore({meta: {...META, layers: [clusters]}, status: status({})});
    el.store = store;
    await settle(host);
    const shadow = el.shadowRoot!;
    const toggle = shadow.querySelector<HTMLButtonElement>('[part="layers-toggle"]')!;
    expect(shadow.querySelector('[part="layers-popover"]')).toBeNull();
    toggle.click();
    await settle(host);
    expect(toggle.getAttribute('aria-expanded')).toBe('true');
    const seen: unknown[] = [];
    host.addEventListener('tessera-layerchange', (e) => seen.push((e as CustomEvent).detail));
    const box = deep(shadow.querySelector('[part="layers-popover"]')!, '[part="entry"] input') as HTMLInputElement;
    box.checked = true;
    box.dispatchEvent(new Event('change', {bubbles: true}));
    expect(seen).toEqual([{layers: ['clusters']}]);
    toggle.focus();
    box.dispatchEvent(new KeyboardEvent('keydown', {key: 'Escape', bubbles: true, composed: true}));
    await settle(host);
    expect(shadow.querySelector('[part="layers-popover"]')).toBeNull();
    expect(shadow.activeElement).toBe(toggle);
    // A press anywhere else on the page closes it too, and a press inside it does not.
    toggle.click();
    await settle(host);
    shadow.querySelector('[part="layers-popover"]')!.dispatchEvent(new PointerEvent('pointerdown', {bubbles: true, composed: true}));
    await settle(host);
    expect(shadow.querySelector('[part="layers-popover"]')).not.toBeNull();
    document.body.dispatchEvent(new PointerEvent('pointerdown', {bubbles: true, composed: true}));
    await settle(host);
    expect(shadow.querySelector('[part="layers-popover"]')).toBeNull();
    expect(toggle.getAttribute('aria-expanded')).toBe('false');
  });

  it('changes its map’s display from the Display section and reports every setting', async () => {
    const {host, shadow} = await explorer('<tessera-explorer density="smooth"></tessera-explorer>');
    const seen: {density: string; points: boolean; densityColours: string | null}[] = [];
    host.addEventListener('tessera-displaychange', (e) => seen.push((e as CustomEvent).detail));
    shadow.querySelector<HTMLButtonElement>('[part="layers-toggle"]')!.click();
    await settle(host);
    const map = shadow.querySelector('tessera-map') as unknown as {density: string; noPoints: boolean; densityColours: string; densityStrength: number};
    const mode = (m: string) => shadow.querySelector<HTMLButtonElement>(`[part="density-mode"] [data-mode="${m}"]`)!;
    // The host's setting is the one shown checked.
    expect(mode('smooth').getAttribute('aria-checked')).toBe('true');

    mode('hex').click();
    await settle(host);
    expect(map.density).toBe('hex');
    expect(mode('hex').getAttribute('aria-checked')).toBe('true');

    // The arrow keys move the choice, as in any radio group.
    mode('hex').dispatchEvent(new KeyboardEvent('keydown', {key: 'ArrowRight', bubbles: true}));
    await settle(host);
    expect(map.density).toBe('grid');

    const points = shadow.querySelector<HTMLButtonElement>('[part="points-toggle"]')!;
    points.click();
    await settle(host);
    expect(map.noPoints).toBe(true);
    expect(points.getAttribute('aria-checked')).toBe('false');

    shadow.querySelector<HTMLButtonElement>('[part="density-colours"]')!.click();
    await settle(host);
    shadow.querySelector<HTMLButtonElement>('#density-colour-list [data-colours="magma"]')!.click();
    await settle(host);
    expect(map.densityColours).toBe('magma');

    const strength = shadow.querySelector<HTMLInputElement>('[part="density-strength"]')!;
    strength.value = '0.5';
    strength.dispatchEvent(new Event('input'));
    await settle(host);
    expect(map.densityStrength).toBe(0.5);

    expect(seen.map((d) => [d.density, d.points, d.densityColours])).toEqual([
      ['hex', true, null],
      ['grid', true, null],
      ['grid', false, null],
      ['grid', false, 'magma'],
      ['grid', false, 'magma']
    ]);
  });

  it('marks the Layers button while any layer is drawn', async () => {
    const {host, shadow, store} = await explorer();
    const toggle = () => shadow.querySelector('[part="layers-toggle"]')!;
    expect(toggle().hasAttribute('data-on')).toBe(false);
    store.set('artifacts', {...store.get('artifacts'), layers: ['clusters']});
    await settle(host);
    expect(toggle().hasAttribute('data-on')).toBe(true);
  });
});
