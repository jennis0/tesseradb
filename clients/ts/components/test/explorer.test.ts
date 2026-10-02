import {afterEach, describe, expect, it} from 'vitest';
import type {Meta, RegionProjection} from '@tesseradb/client';
import '../src/explorer.js';
import {aggregateEntry, answerAggregate, deep, deepAll, fakeStore, mount, registered, settle, status, meta, scalar, type FakeStore} from './fake-store.js';

/** The folded sections of the right card, with their hints. */
const folds = (shadow: ShadowRoot) => [...shadow.querySelectorAll('[part="info"] [part="fold"][aria-expanded="false"]')].map((f) => [f.getAttribute('data-section'), f.querySelector('[part="fold-hint"]')!.textContent]);
/** Where a callout was placed: its offset from the map's top-left corner. */
const placedAt = (el: HTMLElement): [number, number] => {
  const m = /translate\((-?[\d.]+)px, (-?[\d.]+)px\)/.exec(el.style.transform);
  return m ? [Number(m[1]), Number(m[2])] : [NaN, NaN];
};

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
    store.setBrowse('roots', {artifacts: [{tesseraId: 1n, key: 'k-1', name: 'Neoplasms', maskedCount: 9n, matchedCount: null, rung: 0, parentIds: [], childCount: 0}], parents: [], next: 'more'});
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
    for (const part of ['item-card-headline', 'artifact-card-headline', 'selection-items', 'status-refusal', 'map-refusal', 'status-refresh', 'status-reauthorise']) expect(seen, part).toContain(part);
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
  it('draws none of the cards over the map, so nothing hidden asks for counts; a sheet asks for its own', async () => {
    const {host, el, shadow, store} = await explorer('<tessera-explorer layout="overlay"></tessera-explorer>');
    store.set('legend', {...store.get('legend'), colourBy: 'archive', categories: {archive: [{code: 1, key: 'cs', title: null}]}});
    (el as unknown as {narrow: boolean}).narrow = true;
    await settle(host);
    expect(shadow.querySelector('[part="panel"], [part="sidebar"], [part="info"], tessera-legend, tessera-filter-panel')).toBeNull();
    expect([...registered(store).keys()]).toEqual([]);
    shadow.querySelector<HTMLButtonElement>('[role="tab"][data-sheet="layers"]')!.click();
    await settle(host);
    expect([...registered(store).keys()].map((id) => id.split('#')[0])).toEqual(['legend']);
  });

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

  it('clears the controls and the member_of clauses from the filters sheet, and offers Clear only while one applies', async () => {
    const {host, shadow, store} = await explorer();
    tabs(shadow)[0]!.click();
    await settle(host);
    const clear = () => shadow.querySelector('.sheet-footer .btn:not(.primary)') as HTMLButtonElement;
    expect(clear().disabled).toBe(true);
    store.set('filters', {...store.get('filters'), members: [{layer: 'clusters', artifact: 1n, outside: false, verb: 'filter'}]});
    await settle(host);
    expect(clear().disabled).toBe(false);
    clear().click();
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

describe('<tessera-explorer> the item card beside its point', () => {
  type Map = HTMLElement & {pickedAt: {kind: 'item' | 'artifact'; id: bigint; world: [number, number]} | null; screenOf(w: [number, number]): [number, number]; lookAt(x: number, y: number): boolean};
  const item = (id: bigint) => ({item: {id, detail: {fields: {author: `A${id}`}, labels: [], views: [], scoped: {}}}, itemRefusal: null, artifact: null, artifactRefusal: null});

  /** An explorer whose map is 1000 × 600 px, with an item picked at `world`. */
  async function picked(world: [number, number]) {
    const ctx = await explorer('<tessera-explorer layout="overlay"></tessera-explorer>');
    const map = ctx.shadow.querySelector('tessera-map') as Map;
    Object.defineProperty(map, 'clientWidth', {configurable: true, value: 1000});
    Object.defineProperty(map, 'clientHeight', {configurable: true, value: 600});
    map.pickedAt = {kind: 'item', id: 5n, world};
    ctx.store.set('selection', item(5n));
    await settle(ctx.host);
    await settle(ctx.host);
    return {...ctx, map};
  }
  /** The callouts shown: those whose point is on the map. */
  const callouts = (shadow: ShadowRoot) => [...shadow.querySelectorAll<HTMLElement>('[part~="callout"]')].filter((c) => !c.hidden);

  it('opens beside the point, compact, and not in the right column', async () => {
    const {shadow, map} = await picked([256, 256]);
    const [card] = callouts(shadow);
    expect(card).toBeDefined();
    expect(card!.querySelector('tessera-item-card')!.hasAttribute('compact')).toBe(true);
    expect(shadow.querySelector('.right [part="detail"]')).toBeNull();
    const [px, py] = map.screenOf([256, 256]);
    const [left, top] = placedAt(card!);
    // Beside the point, on one side of it, and joined to it by a leader that starts at the point.
    expect(left > px || left + 300 < px || top > py).toBe(true);
    const line = shadow.querySelector('[part="leaders"] line')!;
    expect([Number(line.getAttribute('x1')), Number(line.getAttribute('y1'))]).toEqual([px, py]);
  });

  it('follows its point as the camera moves, without drawing the explorer again, and hides while the point is off the map', async () => {
    const {host, shadow, map} = await picked([256, 256]);
    const el = host.querySelector('tessera-explorer') as HTMLElement & {requestUpdate(): void};
    const before = placedAt(callouts(shadow)[0]!);
    let renders = 0;
    const render = (el as unknown as {render: () => unknown}).render.bind(el);
    (el as unknown as {render: () => unknown}).render = () => {
      renders += 1;
      return render();
    };
    // A pan: the zoom holds, so the level drawn does not change.
    map.lookAt(0.3, 0.5);
    await new Promise((r) => requestAnimationFrame(r));
    await settle(host);
    expect(placedAt(callouts(shadow)[0]!)).not.toEqual(before);
    expect(renders).toBe(0);
    map.pickedAt = {kind: 'item', id: 5n, world: [-100_000, 0]};
    (host.querySelector('tessera-explorer') as HTMLElement & {requestUpdate(): void}).requestUpdate();
    await settle(host);
    expect(callouts(shadow)).toHaveLength(0);
    // The selection stays; the card returns with its point.
    map.pickedAt = {kind: 'item', id: 5n, world: [256, 256]};
    (host.querySelector('tessera-explorer') as HTMLElement & {requestUpdate(): void}).requestUpdate();
    await settle(host);
    expect(callouts(shadow)).toHaveLength(1);
  });

  it('is replaced by the next pick unless pinned; a pinned card stays, and closing it drops only it', async () => {
    const {host, shadow, store, map} = await picked([256, 256]);
    const pin = () => callouts(shadow).find((c) => !c.hasAttribute('data-pinned'))!.querySelector<HTMLButtonElement>('[part="pin"]')!;
    pin().click();
    await settle(host);
    expect(callouts(shadow).map((c) => [c.getAttribute('data-callout'), c.hasAttribute('data-pinned')])).toEqual([['item:5', true]]);
    map.pickedAt = {kind: 'item', id: 6n, world: [200, 300]};
    store.set('selection', item(6n));
    await settle(host);
    await settle(host);
    expect(callouts(shadow).map((c) => c.getAttribute('data-callout'))).toEqual(['item:5', 'live']);
    // Closing the pinned card leaves the selection.
    const pinned = callouts(shadow)[0]!.querySelector('tessera-item-card')!;
    (pinned.shadowRoot!.querySelector('[part="close"]') as HTMLButtonElement).click();
    await settle(host);
    expect(store.calls.filter((c) => c.name === 'clearSelection')).toHaveLength(0);
    expect(callouts(shadow).map((c) => c.getAttribute('data-callout'))).toEqual(['live']);
    // Without a pin the next pick replaces the card.
    map.pickedAt = {kind: 'item', id: 7n, world: [300, 300]};
    store.set('selection', item(7n));
    await settle(host);
    expect(callouts(shadow).map((c) => c.getAttribute('data-callout'))).toEqual(['live']);
    // Closing the live card drops the selection.
    (callouts(shadow)[0]!.querySelector('tessera-item-card')!.shadowRoot!.querySelector('[part="close"]') as HTMLButtonElement).click();
    expect(store.calls.filter((c) => c.name === 'clearSelection')).toHaveLength(1);
  });

  /** An explorer with the item picked at 256, 256 pinned, and another item picked and shown live. */
  async function pinnedAndLive() {
    const ctx = await picked([256, 256]);
    callouts(ctx.shadow)[0]!.querySelector<HTMLButtonElement>('[part="pin"]')!.click();
    await settle(ctx.host);
    ctx.map.pickedAt = {kind: 'item', id: 6n, world: [200, 300]};
    ctx.store.set('selection', item(6n));
    await settle(ctx.host);
    await settle(ctx.host);
    expect(callouts(ctx.shadow).map((c) => c.getAttribute('data-callout'))).toEqual(['item:5', 'live']);
    return ctx;
  }

  it('drops every card, pinned or not, when the store forgets what the server answered, as clear() and an identity change do', async () => {
    const {host, shadow, store, map} = await pinnedAndLive();
    store.set('meta', null);
    await settle(host);
    store.set('meta', META);
    await settle(host);
    expect(callouts(shadow)).toEqual([]);
    expect(map.pickedAt).toBeNull();
  });

  it('closes the card that is not pinned and drops the selection on a click that finds nothing; a pinned card stays', async () => {
    const {host, shadow, store, map} = await pinnedAndLive();
    map.pickedAt = null;
    map.dispatchEvent(new CustomEvent('tessera-miss', {detail: {}, bubbles: true, composed: true}));
    await settle(host);
    expect(store.calls.filter((c) => c.name === 'clearSelection')).toHaveLength(1);
    store.set('selection', {item: null, itemRefusal: null, artifact: null, artifactRefusal: null});
    await settle(host);
    expect(callouts(shadow).map((c) => c.getAttribute('data-callout'))).toEqual(['item:5']);
    expect(shadow.querySelector('.right [part="detail"]')).toBeNull();
  });

  it('places two cards clear of each other', async () => {
    const {shadow} = await pinnedAndLive();
    const [a, b] = callouts(shadow).map((c) => placedAt(c));
    // The default size, 300 × 220, where nothing has measured a card.
    const apart = a![0] + 300 <= b![0] || b![0] + 300 <= a![0] || a![1] + 220 <= b![1] || b![1] + 220 <= a![1];
    expect(apart).toBe(true);
  });

  it('is named by its item’s title, is the next stop after the map, closes on Escape with the selection, and gives focus back to the map', async () => {
    const ctx = await explorer('<tessera-explorer layout="overlay" title-field="author"></tessera-explorer>');
    const map = ctx.shadow.querySelector('tessera-map') as Map;
    Object.defineProperty(map, 'clientWidth', {configurable: true, value: 1000});
    Object.defineProperty(map, 'clientHeight', {configurable: true, value: 600});
    map.pickedAt = {kind: 'item', id: 5n, world: [256, 256]};
    ctx.store.set('selection', item(5n));
    await settle(ctx.host);
    const [card] = callouts(ctx.shadow);
    expect(card!.getAttribute('aria-label')).toBe('A5');
    // The left card comes first in the tab order, then the map, then the card beside the point.
    const order = [...ctx.shadow.querySelectorAll('tessera-map, [part~="callout"], [part="panel"]')].map((e) => e.getAttribute('part') ?? e.tagName.toLowerCase());
    expect(order).toEqual(['panel', 'tessera-map', 'callout']);
    // Tab from the map goes to the card.
    map.focus();
    map.dispatchEvent(new KeyboardEvent('keydown', {key: 'Tab', bubbles: true, composed: true, cancelable: true}));
    expect(ctx.shadow.activeElement).toBe(card);
    card!.querySelector('tessera-item-card')!.shadowRoot!.querySelector<HTMLButtonElement>('[part="close"]')!.focus();
    // The ring the map draws round the picked point.
    (map as unknown as {selectedWorldXY: [number, number] | null}).selectedWorldXY = [256, 256];
    card!.dispatchEvent(new KeyboardEvent('keydown', {key: 'Escape', bubbles: true, composed: true}));
    await settle(ctx.host);
    expect(ctx.store.calls.filter((c) => c.name === 'clearSelection')).toHaveLength(1);
    expect(ctx.shadow.activeElement).toBe(map);
    expect(map.pickedAt).toBeNull();
    expect((map as unknown as {selectedWorldXY: [number, number] | null}).selectedWorldXY).toBeNull();
  });

  it('drops every card when it is given another store', async () => {
    const {host, el, shadow} = await pinnedAndLive();
    const other = fakeStore({meta: META, status: status({})});
    other.set('selection', item(6n));
    el.store = other;
    await settle(host);
    await settle(host);
    expect(callouts(shadow)).toEqual([]);
  });

  it('goes first in the right column, folding Colour and In view, where the map holds no position for the selection', async () => {
    const {host, shadow, store, map} = await picked([256, 256]);
    map.pickedAt = null;
    store.set('selection', item(5n));
    await settle(host);
    expect(callouts(shadow)).toHaveLength(0);
    const corner = shadow.querySelector('.right')!;
    expect([...corner.children].map((c) => c.getAttribute('part')).filter(Boolean)).toEqual(['detail', 'info']);
    expect(folds(shadow).map(([s]) => s)).toEqual(['colour', 'in-view']);
  });
});

describe('<tessera-explorer> layouts', () => {
  it('puts the filters on the left, docked in a sidebar or in a card over the map, and Colour and In view in a card on the right in both', async () => {
    const docked = await explorer('<tessera-explorer layout="docked"></tessera-explorer>');
    expect(docked.shadow.querySelector('[part="sidebar"] tessera-filter-panel')).not.toBeNull();
    expect(docked.shadow.querySelector('[part="panel"]')).toBeNull();
    expect(docked.shadow.querySelector('.right [part="info"] tessera-legend')).not.toBeNull();
    expect(docked.shadow.querySelector('.right [part="info"] tessera-artifact-list')).not.toBeNull();
    document.body.innerHTML = '';
    const overlay = await explorer('<tessera-explorer layout="overlay"></tessera-explorer>');
    expect(overlay.shadow.querySelector('[part="sidebar"]')).toBeNull();
    expect(overlay.shadow.querySelector('[part="panel"] tessera-filter-panel')).not.toBeNull();
    expect(overlay.shadow.querySelector('[part="panel"] tessera-legend')).toBeNull();
    expect(overlay.shadow.querySelector('.right [part="info"] tessera-legend')).not.toBeNull();
    // No hierarchy panel.
    expect(deep(overlay.host, 'tessera-hierarchy')).toBeNull();
  });

  it('shows the filter controls inline in the left card, with the clauses in each position counted on the switch', async () => {
    const {host, shadow, store} = await explorer('<tessera-explorer layout="overlay"></tessera-explorer>');
    store.set('filters', {...store.get('filters'), draft: {filter: {archive: {family: 'category', keys: ['cs']}}, highlight: {archive: {family: 'category', keys: ['cs']}}}});
    await settle(host);
    const card = shadow.querySelector('[part="panel"]')!;
    expect(deepAll(card, 'tessera-filter')).toHaveLength(1);
    expect(deepAll(card, '[part="mode"] [part="mode-count"]').map((c) => c.textContent)).toEqual(['1', '1']);
    expect(shadow.querySelector('[part="filters-popover"]')).toBeNull();
    expect(shadow.querySelector('[part="filters-toggle"]')).toBeNull();
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

  const REGION: RegionProjection = {shape: {kind: 'box' as const, bbox: [0, 0, 1, 1] as [number, number, number, number]}, status: 'shown' as const, refusal: null, visible: {value: 3, exact: true}, matched: {value: 3, exact: true}, served: {shown: 1, total: 3, exact: true}, verdict: {exact: true, depth: null}, held: {ids: BigUint64Array.of(5n), positions: new Float32Array(2), count: 1}};

  it('puts a selected region first in the right column and folds Colour and In view to their headings, each opening again, until the region is cleared', async () => {
    const {host, shadow, store} = await explorer('<tessera-explorer layout="overlay"></tessera-explorer>');
    store.set('legend', {...store.get('legend'), colourBy: 'archive', categories: {archive: [{code: 1, key: 'cs', title: null}, {code: 2, key: 'math', title: null}]}});
    await settle(host);
    expect(folds(shadow)).toEqual([]);
    store.set('region', REGION);
    await settle(host);
    const corner = shadow.querySelector('.right')!;
    expect([...corner.children].map((c) => c.getAttribute('part')).filter(Boolean)).toEqual(['selection-card', 'info']);
    // The number of values is the aggregate's, and is left out until it answers.
    expect(folds(shadow)).toEqual([
      ['colour', 'Archive'],
      ['in-view', '0 clusters']
    ]);
    expect([...registered(store).entries()].filter(([id]) => id.startsWith('colour-hint')).map(([, spec]) => spec)).toEqual([{groupings: [{by: {field: 'archive', top: 1}}]}]);
    answerAggregate(store, 'colour-hint', aggregateEntry([{rows: [{key: 'cs', count: 3}], groups: 7}]));
    await settle(host);
    expect(folds(shadow)[0]).toEqual(['colour', 'Archive · 7 values']);
    expect(shadow.querySelector('[part="info"] tessera-legend')).toBeNull();
    shadow.querySelector<HTMLButtonElement>('[part="fold"][data-section="colour"]')!.click();
    await settle(host);
    expect(shadow.querySelector('[part="info"] tessera-legend')).not.toBeNull();
    expect(folds(shadow).map(([s]) => s)).toEqual(['in-view']);
    // The opened section folds again from its chevron, and its count is no longer asked for.
    expect(registered(store).size).toBeGreaterThan(0);
    expect([...registered(store).keys()].some((id) => id.startsWith('colour-hint'))).toBe(false);
    const refold = shadow.querySelector<HTMLButtonElement>('[part="fold"][data-section="colour"][aria-expanded="true"]')!;
    refold.click();
    await settle(host);
    expect(folds(shadow).map(([s]) => s)).toEqual(['colour', 'in-view']);
    shadow.querySelector<HTMLButtonElement>('[part="fold"][data-section="colour"]')!.click();
    await settle(host);
    store.set('region', null);
    await settle(host);
    expect(folds(shadow)).toEqual([]);
    // A second region folds them again.
    store.set('region', REGION);
    await settle(host);
    expect(folds(shadow).map(([s]) => s)).toEqual(['colour', 'in-view']);
  });

  it('folds the right card to its headings by default in the compact form', async () => {
    const {host, el, shadow} = await explorer('<tessera-explorer layout="overlay"></tessera-explorer>');
    (el as unknown as {compact: boolean}).compact = true;
    await settle(host);
    expect(folds(shadow).map(([s]) => s)).toEqual(['colour', 'in-view']);
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

  describe('Size by', () => {
    const SIZED = meta({
      declaredScalars: [
        ...META.declaredScalars,
        scalar('citations', 'u32', {render: true, homes: ['rendered']}),
        scalar('pages', 'u32'),
        scalar('score', 'f32', {render: true, homes: ['rendered']}),
        scalar('published_at', 'timestamp_us', {render: true, homes: ['rendered']})
      ]
    });

    async function popover(markup = '<tessera-explorer></tessera-explorer>') {
      const host = await mount(markup);
      const el = host.querySelector('tessera-explorer') as HTMLElement & {store: unknown};
      const store = fakeStore({meta: SIZED, status: status({})});
      el.store = store;
      await settle(host);
      const shadow = el.shadowRoot!;
      shadow.querySelector<HTMLButtonElement>('[part="layers-toggle"]')!.click();
      await settle(host);
      const map = shadow.querySelector('tessera-map') as unknown as {sizeBy: string; sizeMin: number | null; sizeMax: number | null; sizeScale: string; radius: number | null};
      const seen: unknown[] = [];
      host.addEventListener('tessera-sizechange', (e) => seen.push((e as CustomEvent).detail));
      const part = <T extends Element = HTMLElement>(name: string) => shadow.querySelector<T & Element>(`[part~="${name}"]`);
      return {host, store, shadow, map, seen, part};
    }

    it('lists None and the number columns the points arrive with, and nothing else', async () => {
      const {host, shadow, part} = await popover();
      expect(part('size-by')!.textContent!.trim()).toBe('None');
      part<HTMLButtonElement>('size-by')!.click();
      await settle(host);
      const options = [...shadow.querySelectorAll('[part~="size-option"]')];
      expect(options.map((o) => o.getAttribute('data-value'))).toEqual(['', 'citations', 'score']);
      expect(options.map((o) => o.getAttribute('aria-checked'))).toEqual(['true', 'false', 'false']);
    });

    /** The store's answer to a Size by choice, as the real store publishes it. */
    async function answer(host: HTMLElement, store: FakeStore, sizeBy: string | null) {
      store.set('legend', {...store.get('legend'), sizeBy});
      await settle(host);
    }

    it('sizes by the column chosen, which the store is asked for, reports the choice, and shows what the store draws', async () => {
      const {host, store, shadow, map, seen, part} = await popover();
      part<HTMLButtonElement>('size-by')!.click();
      await settle(host);
      shadow.querySelector<HTMLButtonElement>('[part~="size-option"][data-value="citations"]')!.click();
      await settle(host);
      expect(map.sizeBy).toBe('citations');
      expect(store.calls.filter((c) => c.name === 'setSizeBy').map((c) => c.args)).toEqual([['citations', {rank: false}]]);
      expect(seen).toEqual([{sizeBy: 'citations', min: 2, max: 9, scale: 'linear'}]);
      expect(shadow.querySelector('[part~="size-menu"]')).toBeNull();
      // The popover shows what the store draws, which has not answered yet.
      expect(part('point-size')).not.toBeNull();
      await answer(host, store, 'citations');
      // Under a column the one Size slider gives way to the range and the scale.
      expect(part('point-size')).toBeNull();
      expect(part('size-range')!.textContent!.trim()).toBe('2 – 9 px');

      const low = part<HTMLInputElement>('size-min')!;
      low.value = '10';
      low.dispatchEvent(new Event('input'));
      await settle(host);
      // Moving the smallest past the largest takes the largest with it.
      expect([map.sizeMin, map.sizeMax]).toEqual([10, 10]);
      expect(part('size-range')!.textContent!.trim()).toBe('10 – 10 px');
      shadow.querySelector<HTMLButtonElement>('[part="size-scale"] [data-scale="rank"]')!.click();
      await settle(host);
      expect(map.sizeScale).toBe('rank');
      expect(shadow.querySelector('[part="size-scale"] [data-scale="rank"]')!.getAttribute('aria-checked')).toBe('true');
      expect(seen.at(-1)).toEqual({sizeBy: 'citations', min: 10, max: 10, scale: 'rank'});

      part<HTMLButtonElement>('size-by')!.click();
      await settle(host);
      shadow.querySelector<HTMLButtonElement>('[part~="size-option"][data-value=""]')!.click();
      await settle(host);
      expect(map.sizeBy).toBe('none');
      expect(store.calls.filter((c) => c.name === 'setSizeBy').at(-1)!.args[0]).toBeNull();
      expect(seen.at(-1)).toEqual({sizeBy: null, min: 10, max: 10, scale: 'rank'});
      await answer(host, store, null);
      expect(part('point-size')).not.toBeNull();
    });

    it('shows a column the store is sized by from elsewhere, and the sizes chosen on the map', async () => {
      const {host, store, shadow, part} = await popover();
      await answer(host, store, 'score');
      expect(part('size-by')!.textContent!.trim()).toBe('Score');
      (shadow.querySelector('tessera-map') as unknown as {sizeMax: number}).sizeMax = 7;
      await settle(host);
      expect(part('size-range')!.textContent!.trim()).toBe('2 – 7 px');
    });

    it('shows the size a host sets, passes it to the map, and leaves the slider out under hide-size', async () => {
      const {map, part} = await popover('<tessera-explorer radius="5"></tessera-explorer>');
      expect(map.radius).toBe(5);
      expect(part<HTMLInputElement>('point-size')!.value).toBe('5');
      expect(part('point-size')!.nextElementSibling!.textContent).toBe('5 px');
      document.body.innerHTML = '';
      const hidden = await popover('<tessera-explorer radius="5" hide-size></tessera-explorer>');
      expect(hidden.part('point-size')).toBeNull();
      expect(hidden.part('size-by')).not.toBeNull();
      expect(hidden.map.radius).toBe(5);
    });

    it('opens the menu from the arrow keys, keeps one entry in the tab order, and closes on Escape or as focus leaves', async () => {
      const {host, shadow, part} = await popover();
      part('size-by')!.dispatchEvent(new KeyboardEvent('keydown', {key: 'ArrowDown', bubbles: true}));
      await settle(host);
      const options = () => [...shadow.querySelectorAll<HTMLElement>('[part~="size-option"]')];
      expect(options().map((o) => o.tabIndex)).toEqual([0, -1, -1]);
      options()[0]!.dispatchEvent(new KeyboardEvent('keydown', {key: 'ArrowDown', bubbles: true}));
      expect(options().map((o) => o.tabIndex)).toEqual([-1, 0, -1]);
      expect(shadow.activeElement).toBe(options()[1]);
      // Escape closes the menu alone and gives focus back to its button.
      options()[1]!.dispatchEvent(new KeyboardEvent('keydown', {key: 'Escape', bubbles: true, composed: true}));
      await settle(host);
      expect(shadow.querySelector('[part~="size-menu"]')).toBeNull();
      expect(shadow.querySelector('[part="layers-popover"]')).not.toBeNull();
      expect(shadow.activeElement).toBe(part('size-by'));
      // Focus leaving the menu, as Tab past it does, closes it.
      part<HTMLButtonElement>('size-by')!.click();
      await settle(host);
      shadow.querySelector('[part~="size-menu"]')!.dispatchEvent(new FocusEvent('focusout', {relatedTarget: part('size-by'), bubbles: true}));
      await settle(host);
      expect(shadow.querySelector('[part~="size-menu"]')).toBeNull();
    });
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
