import {afterEach, describe, expect, it} from 'vitest';
import type {Meta, RegionProjection} from '@tesseradb/client';
import '../src/explorer.js';
import {aggregateEntry, answerAggregate, deep, deepAll, fakeStore, mount, registered, settle, status, meta, scalar, type FakeStore} from './fake-store.js';

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
    store.setBrowse('roots', {artifacts: [{tesseraId: 1n, key: 'k-1', name: 'Neoplasms', maskedCount: 9n, matchedCount: null, rung: 0, parentIds: [], childCount: 0, slot: null}], parents: [], next: 'more'});
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

  it('forwards the field cards’ parts, and their search boxes’, through the field column', async () => {
    const {host, shadow, store} = await explorer();
    store.set('filters', {...store.get('filters'), draft: {filter: {archive: {family: 'category', keys: ['cs']}, author: {family: 'keyword', needle: 'Ada', op: 'eq'}}, highlight: {}}});
    await settle(host);
    const panel = shadow.querySelector('tessera-filter-panel')!;
    const outer = forwardedBy(panel);
    const cards = [...panel.shadowRoot!.querySelectorAll('tessera-field-card')];
    expect(cards.length).toBe(2);
    for (const card of cards) {
      const byCard = forwardedBy(card);
      for (const part of partsIn(card.shadowRoot!)) {
        expect(byCard.get(part)).toBe(`field-card-${part}`);
        expect(outer.get(`field-card-${part}`)).toBe(`field-card-${part}`);
      }
      for (const f of card.shadowRoot!.querySelectorAll('tessera-filter')) {
        const map = forwardedBy(f);
        for (const part of partsIn(f.shadowRoot!)) {
          expect(map.get(part)).toBe(`filter-${part}`);
          expect(byCard.get(`filter-${part}`)).toBe(`filter-${part}`);
          expect(outer.get(`filter-${part}`)).toBe(`filter-${part}`);
        }
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
    const {host, el, shadow, store} = await explorer('<tessera-explorer layout="overlay" pinned-filters="archive"></tessera-explorer>');
    (el as unknown as {narrow: boolean}).narrow = true;
    await settle(host);
    expect(shadow.querySelector('[part="panel"], [part="sidebar"], tessera-filter-panel')).toBeNull();
    expect([...registered(store).keys()]).toEqual([]);
    shadow.querySelector<HTMLButtonElement>('[role="tab"][data-sheet="filters"]')!.click();
    await settle(host);
    // The sheet's field column folds its cards to a line, so they ask for their counts in view alone.
    expect([...registered(store).keys()].map((id) => id.split('#')[0]).sort()).toEqual(['field-subject', 'fields-match']);
  });

  it('gives the Layers sheet the Points, Colour and Density sections over the layer picker', async () => {
    const {host, el, shadow} = await explorer();
    (el as unknown as {narrow: boolean}).narrow = true;
    await settle(host);
    shadow.querySelector<HTMLButtonElement>('[role="tab"][data-sheet="layers"]')!.click();
    await settle(host);
    const sheet = shadow.querySelector('[part="sheet"]')!;
    expect([...sheet.querySelectorAll('.sec > .hd, .sec .sec-head .hd')].map((h) => h.textContent)).toEqual(['Points', 'Colour', 'Density']);
    expect(sheet.querySelector('[part="most-points"]')).not.toBeNull();
    expect(sheet.querySelector('tessera-layer-picker')).not.toBeNull();
  });

  const tabs = (shadow: ShadowRoot) => [...shadow.querySelectorAll<HTMLButtonElement>('[part="tabs"] [role="tab"]')];
  const key = (el: Element, k: string) => el.dispatchEvent(new KeyboardEvent('keydown', {key: k, bubbles: true, composed: true, cancelable: true}));

  it('moves between tabs with the arrow keys, Home and End, keeping one tab in the tab order', async () => {
    const {host, shadow} = await explorer();
    const all = tabs(shadow);
    expect(all.map((t) => t.textContent!.trim())).toEqual(['Fields', 'Layers', 'Item']);
    expect(all.map((t) => t.tabIndex)).toEqual([0, -1, -1]);
    all[0]!.focus();
    key(all[0]!, 'ArrowRight');
    await settle(host);
    expect(shadow.activeElement).toBe(tabs(shadow)[1]);
    expect(tabs(shadow).map((t) => t.tabIndex)).toEqual([-1, 0, -1]);
    key(tabs(shadow)[1]!, 'ArrowLeft');
    key(tabs(shadow)[0]!, 'ArrowLeft');
    await settle(host);
    expect(shadow.activeElement).toBe(tabs(shadow)[2]);
    key(tabs(shadow)[2]!, 'Home');
    await settle(host);
    expect(shadow.activeElement).toBe(tabs(shadow)[0]);
    key(tabs(shadow)[0]!, 'End');
    await settle(host);
    expect(shadow.activeElement).toBe(tabs(shadow)[2]);
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

  it('stands in the top-right corner where the map holds no position for the selection', async () => {
    const {host, shadow, store, map} = await picked([256, 256]);
    map.pickedAt = null;
    store.set('selection', item(5n));
    await settle(host);
    expect(callouts(shadow)).toHaveLength(0);
    const corner = shadow.querySelector('.right')!;
    expect([...corner.children].map((c) => c.getAttribute('part')).filter(Boolean)).toEqual(['detail']);
  });
});

describe('<tessera-explorer> layouts', () => {
  it('puts the field column on the left, docked in a sidebar or in a card over the map, and nothing on the right', async () => {
    const docked = await explorer('<tessera-explorer layout="docked"></tessera-explorer>');
    expect(docked.shadow.querySelector('[part="sidebar"] tessera-filter-panel')).not.toBeNull();
    expect(docked.shadow.querySelector('[part="panel"]')).toBeNull();
    expect([...docked.shadow.querySelector('.right')!.children].filter((c) => c.tagName !== 'SLOT')).toEqual([]);
    document.body.innerHTML = '';
    const overlay = await explorer('<tessera-explorer layout="overlay"></tessera-explorer>');
    expect(overlay.shadow.querySelector('[part="sidebar"]')).toBeNull();
    expect(overlay.shadow.querySelector('[part="panel"] tessera-filter-panel')).not.toBeNull();
    // No hierarchy panel, and no legend or In view list.
    expect(deep(overlay.host, 'tessera-hierarchy')).toBeNull();
    expect(deep(overlay.host, 'tessera-legend, tessera-artifact-list')).toBeNull();
  });

  it('shows a card per field holding a clause in the left card, the clauses as chips, and no Filter / Highlight switch', async () => {
    const {host, shadow, store} = await explorer('<tessera-explorer layout="overlay"></tessera-explorer>');
    store.set('filters', {...store.get('filters'), draft: {filter: {archive: {family: 'category', keys: ['cs']}}, highlight: {archive: {family: 'category', keys: ['cs']}}}});
    await settle(host);
    const card = shadow.querySelector('[part="panel"]')!;
    expect(deepAll(card, 'tessera-field-card')).toHaveLength(1);
    expect(deepAll(card, '[part="chip"]').map((c) => c.getAttribute('data-verb'))).toEqual(['filter', 'highlight']);
    expect(deep(card, '[part="mode"]')).toBeNull();
  });

  it('heads the card with the dataset title over the view’s name, or the view’s name alone', async () => {
    const titled = await explorer('<tessera-explorer layout="overlay" dataset-title="arXiv abstracts"></tessera-explorer>');
    expect(titled.shadow.querySelector('[part="dataset-title"]')?.textContent).toBe('arXiv abstracts');
    expect(titled.shadow.querySelector('[part="view-name"]')?.textContent).toBe('default');
    document.body.innerHTML = '';
    const plain = await explorer('<tessera-explorer layout="overlay"></tessera-explorer>');
    expect(plain.shadow.querySelector('[part="dataset-title"]')).toBeNull();
    expect(plain.shadow.querySelector('[part="view-name"]')?.textContent).toBe('default');
    // A name that is only the view's id says nothing, and is left out.
    plain.store.set('meta', {...META, views: [{...META.views[0]!, displayName: 's0'}]});
    await settle(plain.host);
    expect(plain.shadow.querySelector('[part="view-name"]')).toBeNull();
    plain.store.set('meta', META);
    await settle(plain.host);
    // With several views the name is the view choice.
    plain.store.set('meta', {...META, views: [...META.views, {...META.views[0]!, id: 's1', displayName: 'other'}]});
    await settle(plain.host);
    expect(plain.shadow.querySelector('[part="view-name"]')).toBeNull();
    expect(plain.shadow.querySelector('tessera-view-picker')?.shadowRoot?.querySelector('select')).not.toBeNull();
  });

  const REGION: RegionProjection = {shape: {kind: 'box' as const, bbox: [0, 0, 1, 1] as [number, number, number, number]}, status: 'shown' as const, refusal: null, visible: {value: 3, exact: true}, matched: {value: 3, exact: true}, served: {shown: 1, total: 3, exact: true}, verdict: {exact: true, depth: null}, held: {ids: BigUint64Array.of(5n), positions: new Float32Array(2), count: 1}};

  it('puts a selected region in the right column', async () => {
    const {host, shadow, store} = await explorer('<tessera-explorer layout="overlay"></tessera-explorer>');
    store.set('region', REGION);
    await settle(host);
    const corner = shadow.querySelector('.right')!;
    expect([...corner.children].map((c) => c.getAttribute('part')).filter(Boolean)).toEqual(['selection-card']);
  });

  it('folds every field card to one line in the compact form, the column 264 px wide', async () => {
    const {host, el, shadow} = await explorer('<tessera-explorer layout="overlay" pinned-filters="archive author"></tessera-explorer>');
    (el as unknown as {compact: boolean}).compact = true;
    await settle(host);
    const panel = shadow.querySelector('tessera-filter-panel') as HTMLElement & {compact: boolean};
    expect(panel.compact).toBe(true);
    expect(deepAll(panel.shadowRoot!, 'tessera-field-card').map((c) => c.hasAttribute('folded'))).toEqual([true, true]);
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
      expect(part('point-size')!.getAttribute('aria-valuetext')).toBe('5 px');
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

describe('<tessera-explorer> the point budget', () => {
  const budgets = (store: FakeStore) => store.calls.filter((c) => c.name === 'setBudget').map((c) => c.args[0]);
  const mapOf = (shadow: ShadowRoot) => shadow.querySelector('tessera-map') as unknown as {budget: number; budgetMin: number; budgetMax: number};

  it('leaves the store’s budget unless the host sets one, and offers 1,000 to 2,000,000', async () => {
    const {store, shadow} = await explorer();
    expect(budgets(store)).toEqual([]);
    expect(mapOf(shadow)).toMatchObject({budget: 0, budgetMin: 1_000, budgetMax: 2_000_000});
  });

  it('offers Most points on a log scale from budget-min to budget-max, and sets the store’s budget only when it is let go', async () => {
    const {host, shadow, store} = await explorer('<tessera-explorer budget-min="1000" budget-max="2000000"></tessera-explorer>');
    const seen: unknown[] = [];
    host.addEventListener('tessera-budgetchange', (e) => seen.push((e as CustomEvent).detail));
    shadow.querySelector<HTMLButtonElement>('[part="layers-toggle"]')!.click();
    await settle(host);
    const slider = shadow.querySelector<HTMLInputElement>('[part="most-points"]')!;
    const value = () => shadow.querySelector('[part="most-points-value"]')!.textContent;
    expect(value()).toBe('250K');
    expect([...shadow.querySelectorAll('.ticks span')].map((t) => t.textContent)).toEqual(['1K', '10K', '100K', '2M']);
    // Dragging moves the number shown and asks nothing of the store.
    slider.value = '1000';
    slider.dispatchEvent(new Event('input'));
    await settle(host);
    expect(value()).toBe('2M');
    slider.value = '500';
    slider.dispatchEvent(new Event('input'));
    await settle(host);
    expect(budgets(store)).toEqual([]);
    // Letting go sets it, on a round number.
    slider.dispatchEvent(new Event('change'));
    await settle(host);
    expect(budgets(store)).toEqual([45_000]);
    expect(seen).toEqual([{budget: 45_000}]);
  });

  it('passes the host’s budget and range to the map and the budget to the store, and a change of budget too', async () => {
    const {host, el, store, shadow} = await explorer('<tessera-explorer budget="40000" budget-min="500" budget-max="90000"></tessera-explorer>');
    expect(budgets(store)).toEqual([40_000]);
    expect(mapOf(shadow)).toMatchObject({budget: 40_000, budgetMin: 500, budgetMax: 90_000});
    (el as unknown as {budget: number}).budget = 60_000;
    await settle(host);
    expect(budgets(store)).toEqual([40_000, 60_000]);
  });
});

describe('<tessera-explorer> the Colour section', () => {
  const NUMBERED = meta({
    declaredScalars: [...META.declaredScalars, scalar('year', 'u16', {render: true, homes: ['rendered']})],
    filterOperands: META.filterOperands
  });

  async function colour() {
    const host = await mount('<tessera-explorer></tessera-explorer>');
    const el = host.querySelector('tessera-explorer') as HTMLElement & {store: unknown};
    const store = fakeStore({meta: NUMBERED, status: status({})});
    el.store = store;
    await settle(host);
    const shadow = el.shadowRoot!;
    shadow.querySelector<HTMLButtonElement>('[part="layers-toggle"]')!.click();
    await settle(host);
    return {host, store, shadow, part: (name: string) => shadow.querySelector<HTMLElement>(`[part~="${name}"]`)};
  }

  it('lists None and the rendered columns in Colour by, and colours by the one chosen', async () => {
    const {host, store, shadow, part} = await colour();
    const seen: unknown[] = [];
    host.addEventListener('tessera-colourchange', (e) => seen.push((e as CustomEvent).detail));
    expect(part('colour-by')!.textContent!.trim()).toBe('None');
    part('colour-by')!.click();
    await settle(host);
    const options = [...shadow.querySelectorAll<HTMLButtonElement>('[part~="colour-option"]')];
    expect(options.map((o) => [o.getAttribute('data-value'), o.getAttribute('aria-checked')])).toEqual([
      ['', 'true'],
      ['archive', 'false'],
      ['year', 'false']
    ]);
    options[1]!.click();
    expect(store.calls.filter((c) => c.name === 'setColourBy').map((c) => c.args)).toEqual([['archive']]);
    expect(seen).toEqual([{colourBy: 'archive'}]);
  });

  it('offers the palette for a category, and the ramp, its scale and Reverse for a number', async () => {
    const {host, store, part} = await colour();
    const changes: unknown[] = [];
    host.addEventListener('tessera-palettechange', (e) => changes.push((e as CustomEvent).detail));
    store.set('legend', {...store.get('legend'), colourBy: 'archive'});
    await settle(host);
    expect(part('palette')!.textContent).toContain('Tableau 10');
    expect(part('ramp')).toBeNull();
    store.set('legend', {...store.get('legend'), colourBy: 'year'});
    await settle(host);
    expect(part('palette')).toBeNull();
    expect(part('ramp')!.textContent).toContain('Viridis');
    part('ramp-reverse')!.click();
    await settle(host);
    expect(part('ramp-reverse')!.getAttribute('aria-pressed')).toBe('true');
    expect(changes).toEqual([{palette: 'tableau10', ramp: 'viridis', scale: 'linear', reverse: true}]);
  });
});

describe('<tessera-explorer> Edit colours', () => {
  const topics = {name: 'topics', title: 'Topics', views: ['s0'], membership: 'enumerated', hierarchy: {kind: 'flat', pruneChildren: false}, levels: [], computedContent: ['centroid'], shape: null, suppliedContent: ['name'], depsOn: [], version: 1} as unknown as Meta['layers'][number];

  async function withEditor(markup = '<tessera-explorer></tessera-explorer>') {
    const host = await mount(markup);
    const el = host.querySelector('tessera-explorer') as HTMLElement & {store: unknown; clusterColours: Record<string, Record<string, string>> | null; valueColours: unknown};
    const store = fakeStore({meta: {...META, layers: [topics]}, status: status({})});
    el.store = store;
    await settle(host);
    const shadow = el.shadowRoot!;
    shadow.querySelector<HTMLButtonElement>('[part="layers-toggle"]')!.click();
    await settle(host);
    return {host, el, store, shadow, button: () => shadow.querySelector<HTMLButtonElement>('[part="edit-colours"]')!};
  }

  it('is offered while the points are coloured by a category or a layer, and opens the dialog over the popover, which stays open beneath it', async () => {
    const {host, store, shadow, button} = await withEditor();
    expect(button().disabled).toBe(true);
    store.set('legend', {...store.get('legend'), colourBy: 'archive'});
    await settle(host);
    expect(button().disabled).toBe(false);
    store.set('legend', {...store.get('legend'), colourBy: 'cluster:topics'});
    await settle(host);
    button().focus();
    button().click();
    await settle(host);
    const dialog = deep(host, '[part="dialog"]') as HTMLDialogElement;
    expect(dialog.open).toBe(true);
    expect(deep(host, '[part="title"]#title')!.textContent).toBe('Topics');
    expect([...registered(store).values()]).toContainEqual({groupings: [{by: {layer: 'topics', top: 1000, paletteSize: 'drawn'}}], subject: 'visible'});
    // A press inside the dialog leaves the Layers popover open, so Done gives focus back to the button.
    dialog.dispatchEvent(new PointerEvent('pointerdown', {bubbles: true, composed: true}));
    await settle(host);
    expect(shadow.querySelector('[part="layers-popover"]')).not.toBeNull();
    (deep(host, '[part="done"]') as HTMLButtonElement).click();
    await settle(host);
    expect(dialog.open).toBe(false);
    expect(shadow.activeElement).toBe(button());
  });

  it('sets the cluster colours a host keeps on the store, and reports a change made in the dialog for the host to keep', async () => {
    const {host, el, store, button} = await withEditor();
    el.clusterColours = {topics: {'7': '#112233', '8': '#445566', nonsense: '#000000', '9': 'red'}, other: {'7': '#000000'}};
    await settle(host);
    const set = () => store.calls.filter((c) => c.name === 'setArtifactColours').at(-1)!.args[0] as Map<string, Map<bigint, readonly number[]>>;
    expect([...set().get('topics')!].map(([id, c]) => [id, c.slice(0, 3)])).toEqual([
      [7n, [0x11, 0x22, 0x33]],
      [8n, [0x44, 0x55, 0x66]]
    ]);
    store.set('legend', {...store.get('legend'), colourBy: 'cluster:topics'});
    await settle(host);
    button().click();
    await settle(host);
    const kept: unknown[] = [];
    host.addEventListener('tessera-clustercolour', (e) => kept.push((e as CustomEvent).detail));
    (deep(host, '[part="reset-all"]') as HTMLButtonElement).click();
    await settle(host);
    expect(kept).toEqual([
      {
        layer: 'topics',
        changes: [
          {tesseraId: '7', colour: null},
          {tesseraId: '8', colour: null}
        ]
      }
    ]);
    // Another layer's colours stay.
    expect([...store.get('artifacts').overrides.keys()]).toEqual(['other']);
    // The host's copy is the host's: setting it again replaces the store's.
    el.clusterColours = {topics: {'8': '#445566'}};
    await settle(host);
    expect([...set().keys()]).toEqual(['topics']);
    expect([...set().get('topics')!.keys()]).toEqual([8n]);
  });
});

describe('<tessera-explorer> the Palette menu while the points are coloured by a layer', () => {
  it('lists each palette as a row of its colours, its name and its size, and sets the one chosen on the store', async () => {
    const host = await mount('<tessera-explorer></tessera-explorer>');
    const el = host.querySelector('tessera-explorer') as HTMLElement & {store: unknown; palette: string};
    const store = fakeStore({meta: META, status: status({})});
    store.set('legend', {...store.get('legend'), colourBy: 'cluster:clusters'});
    el.store = store;
    await settle(host);
    const shadow = el.shadowRoot!;
    shadow.querySelector<HTMLButtonElement>('[part="layers-toggle"]')!.click();
    await settle(host);
    const part = (name: string) => shadow.querySelector<HTMLElement>(`[part~="${name}"]`);
    expect(part('palette')!.getAttribute('aria-label')).toBe('Palette: Tableau 10');
    part('palette')!.click();
    await settle(host);
    expect(part('palette-menu')!.getAttribute('role')).toBe('menu');
    const options = [...shadow.querySelectorAll<HTMLButtonElement>('[part~="palette-option"]')];
    expect(
      options.map((o) => ({
        value: o.getAttribute('data-value'),
        role: o.getAttribute('role'),
        checked: o.getAttribute('aria-checked'),
        name: o.querySelector('.t')!.textContent,
        line: o.querySelector('.line')!.textContent,
        swatches: o.querySelectorAll('.swatches > span').length
      }))
    ).toEqual([
      {value: 'okabe-ito', role: 'menuitemradio', checked: 'false', name: 'Okabe-Ito', line: '8 colours · colour-blind safe', swatches: 8},
      {value: 'tableau10', role: 'menuitemradio', checked: 'true', name: 'Tableau 10', line: '10 colours', swatches: 10},
      {value: 'tableau20', role: 'menuitemradio', checked: 'false', name: 'Tableau 20', line: '20 colours, in light and dark pairs', swatches: 20},
      {value: 'kelly', role: 'menuitemradio', checked: 'false', name: 'Kelly', line: '22 colours, most distinct', swatches: 22}
    ]);
    expect((options[0]!.querySelector('.swatches > span') as HTMLElement).style.background).toBe('#e69f00');
    const chosen: unknown[] = [];
    host.addEventListener('tessera-clusterpalettechange', (e) => chosen.push((e as CustomEvent).detail));
    options[3]!.click();
    await settle(host);
    // Set on the store, and again through the map the explorer passes it to; the store ignores the second.
    expect(new Set(store.calls.filter((c) => c.name === 'setPalette').map((c) => c.args[0]))).toEqual(new Set(['kelly']));
    expect(chosen).toEqual([{palette: 'kelly'}]);
    expect(el.palette).toBe('kelly');
    expect(part('palette-menu')).toBeNull();
  });
});

describe('<tessera-explorer> the cluster budget', () => {
  const tree = {name: 'tree', title: 'tree', views: ['s0'], membership: 'enumerated', hierarchy: {kind: 'nested', pruneChildren: true}, levels: [], computedContent: ['centroid'], shape: null, suppliedContent: ['name'], depsOn: [], version: 1} as unknown as Meta['layers'][number];
  const flat = {...tree, name: 'flat', hierarchy: {kind: 'flat', pruneChildren: false}} as unknown as Meta['layers'][number];
  const budgets = (store: FakeStore) => store.calls.filter((c) => c.name === 'setClusterBudget').map((c) => c.args[0]);

  async function withLayers(markup = '<tessera-explorer></tessera-explorer>') {
    const host = await mount(markup);
    const el = host.querySelector('tessera-explorer') as HTMLElement & {store: unknown; clusterBudget: number};
    const store = fakeStore({meta: {...META, layers: [tree, flat]}, status: status({})});
    el.store = store;
    await settle(host);
    el.shadowRoot!.querySelector<HTMLButtonElement>('[part="layers-toggle"]')!.click();
    await settle(host);
    return {host, el, store, shadow: el.shadowRoot!};
  }

  it('cuts a tree layer to 1,000 clusters unless the host says otherwise, and passes a change on', async () => {
    const {host, el, store} = await withLayers();
    expect(budgets(store)).toEqual([1_000]);
    el.clusterBudget = 250;
    await settle(host);
    expect(budgets(store)).toEqual([1_000, 250]);
    const other = await withLayers('<tessera-explorer cluster-budget="40"></tessera-explorer>');
    expect(budgets(other.store)).toEqual([40]);
  });

  it('goes back to the finest cut when the cluster budget is set to 0', async () => {
    const {host, el, store} = await withLayers();
    el.clusterBudget = 0;
    await settle(host);
    expect(budgets(store)).toEqual([1_000, null]);
    const none = await withLayers('<tessera-explorer cluster-budget="0"></tessera-explorer>');
    expect(budgets(none.store)).toEqual([]);
  });

  it('offers Most clusters only while a tree layer is drawn, from cluster-budget-min to cluster-budget-max, and sets the cut only when it is let go', async () => {
    const {host, shadow, store} = await withLayers('<tessera-explorer cluster-budget-min="10" cluster-budget-max="10000"></tessera-explorer>');
    const slider = () => shadow.querySelector<HTMLInputElement>('[part="most-clusters"]');
    expect(slider()).toBeNull();
    store.set('artifacts', {...store.get('artifacts'), layers: ['flat']});
    await settle(host);
    expect(slider()).toBeNull();
    store.set('artifacts', {...store.get('artifacts'), layers: ['flat', 'tree']});
    await settle(host);
    expect(slider()).not.toBeNull();
    const value = () => shadow.querySelector('[part="most-clusters-value"]')!.textContent;
    expect(value()).toBe('1K');
    const seen: unknown[] = [];
    host.addEventListener('tessera-clusterbudgetchange', (e) => seen.push((e as CustomEvent).detail));
    slider()!.value = '1000';
    slider()!.dispatchEvent(new Event('input'));
    await settle(host);
    expect(value()).toBe('10K');
    slider()!.value = '500';
    slider()!.dispatchEvent(new Event('input'));
    await settle(host);
    expect(budgets(store)).toEqual([1_000]);
    slider()!.dispatchEvent(new Event('change'));
    await settle(host);
    expect(budgets(store).at(-1)).toBe(320);
    expect(seen).toEqual([{budget: 320}]);
  });
});
