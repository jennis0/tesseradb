import {afterEach, describe, expect, it} from 'vitest';
import type {Meta, Quantisation, ViewInfo, ViewMetadataValue} from '@tesseradb/client';
import '../src/view-picker.js';
import '../src/key-picker.js';
import '../src/item-card.js';
import '../src/map.js';
import '../src/explorer.js';
import type {TesseraItemCard} from '../src/item-card.js';
import {deep, deepAll, fakeStore, mount, settle, status, type FakeStore} from './fake-store.js';

/**
 * What the two pickers, the map and the item card **draw and wire**, against the fake store
 * (`view-switching.md` §9, the V2 row).
 *
 * The rules the pickers apply — the entries and their order, which view a group is entered at, a
 * view's label — are `@tesseradb/client`'s and are tested there (`core/test/views.test.ts`). What
 * is tested here is that each element asks for the right one, renders it as the boards draw it,
 * and calls `setCurrentView` with what came back.
 *
 * The fixture is one bundle with two plain views and two groups over one key set — an owner and a
 * `members` layout of it — with the keys in an order key sorting would not produce (`2026-Q2`
 * before `2026-Q10`).
 */

afterEach(() => {
  document.body.innerHTML = '';
});

const FLAT: Quantisation = {xMin: 0, xMax: 1, yMin: 0, yMax: 1};
/** A composition with nothing in it — a drawn frame, which is what the follow waits for. */
const DRAWN = {depth: 0, want: {x0: 0, y0: 0, x1: 0, y1: 0}, version: 1, exact: [], standIn: [], tiles: [], exactDrawn: 0, exactServed: 0, visibleInView: 0, provisional: 0, folded: false} as never;
const GEO: Quantisation = {xMin: -180, xMax: 180, yMin: -90, yMax: 90};

/** 12:00 UTC, so a formatter running in any plausible zone still draws the day the test names. */
const at = (y: number, m: number, d: number) => Date.UTC(y, m - 1, d, 12) * 1000;

function view(id: string, displayName: string, q: Quantisation, roster: ViewInfo['roster'] = null): ViewInfo {
  return {id, displayName, quantisation: q, projection: 'none', worldAspect: null, tileScheme: null, tile: null, roster};
}

const QUARTERS: {key: string; metadata: Record<string, ViewMetadataValue>}[] = [
  {key: '2026-Q2', metadata: {starts: {type: 'timestamp_us', value: at(2026, 4, 1)}, ends: {type: 'timestamp_us', value: at(2026, 6, 30)}}},
  {key: '2026-Q10', metadata: {label: {type: 'text', value: 'Long quarter'}}},
  {key: '2026-Q3', metadata: {title: {type: 'text', value: 'Third quarter'}}},
  {key: '2026-Q4', metadata: {}}
];

function meta(over: Partial<Meta> = {}): Meta {
  const owner = QUARTERS.map((q) => view(`quarter:${q.key}`, q.key, FLAT, {group: 'quarter', key: q.key, metadata: q.metadata}));
  const members = QUARTERS.map((q) => view(`world:${q.key}`, q.key, GEO, {group: 'world', key: q.key, metadata: {}}));
  return {
    apiVersion: 1,
    idset: 0,
    views: [view('knn', 'knn', FLAT), view('pca64', 'pca64', GEO), ...owner, ...members],
    groups: [
      {name: 'quarter', title: 'Quarter', membersOf: null, views: owner.map((v) => v.id)},
      {name: 'world', title: 'Quarterly map', membersOf: 'quarter', views: members.map((v) => v.id)}
    ],
    declaredScalars: [{name: 'title', arrowType: 'utf8', category: null, render: false, index: true}],
    layers: [],
    selection: {kMin: 1, kMaxMarks: 500, maxK: 5000, thetaTargetMarks: 10, maxUnderlayOffset: 0, maxCategoryValues: 1000, maxRegionVertices: 10_000, maxRegionCells: 262_144},
    maxTilesPerRequest: 4096,
    filterOperands: [],
    ...over
  };
}

/** A store on this fixture, already showing `id`, and the element that reads it. */
async function picker(tag: 'tessera-view-picker' | 'tessera-key-picker', id: string, m: Meta = meta()) {
  const store = fakeStore({meta: m, status: status({})});
  store.set('view', {...store.get('view'), id});
  store.setFrame(m.views.find((v) => v.id === id)?.quantisation ?? FLAT);
  const host = await mount(`<${tag}></${tag}>`);
  (host.querySelector(tag) as unknown as {store: unknown}).store = store;
  await settle(host);
  return {host, store, el: host.querySelector(tag) as HTMLElement};
}

function chooseOption(select: HTMLSelectElement, value: string): void {
  select.value = value;
  select.dispatchEvent(new Event('change'));
}

const switched = (store: FakeStore) => store.calls.filter((c) => c.name === 'setCurrentView').map((c) => c.args[0]);

describe('<tessera-view-picker>', () => {
  it('lists one entry per plain view then one per group, in the meta’s order', async () => {
    const {host} = await picker('tessera-view-picker', 'knn');
    const options = deepAll(host, 'option').map((o) => o.textContent?.trim());
    expect(options).toEqual(['knn', 'pca64', 'Quarter', 'Quarterly map']);
    expect((deep(host, 'select') as HTMLSelectElement).value).toBe('v:knn');
  });

  it('marks the group, not the view, when the current view is in one', async () => {
    const {host} = await picker('tessera-view-picker', 'quarter:2026-Q3');
    expect((deep(host, 'select') as HTMLSelectElement).value).toBe('g:quarter');
  });

  it('renders nothing — not an empty select — for a one-view corpus', async () => {
    const one = meta({views: [view('s0', 'default', FLAT)], groups: []});
    const {host} = await picker('tessera-view-picker', 's0', one);
    expect(deep(host, 'select')).toBeNull();
    expect(deep(host, '[part="field"]')).toBeNull();
  });

  it('keeps the key across a layout toggle, in either direction of membersOf', async () => {
    const {host, store} = await picker('tessera-view-picker', 'quarter:2026-Q3');
    chooseOption(deep(host, 'select') as HTMLSelectElement, 'g:world');
    expect(switched(store)).toEqual(['world:2026-Q3']);

    const back = await picker('tessera-view-picker', 'world:2026-Q10');
    chooseOption(deep(back.host, 'select') as HTMLSelectElement, 'g:quarter');
    expect(switched(back.store)).toEqual(['quarter:2026-Q10']);
  });

  it('re-enters a group at the key it was last left on', async () => {
    const {host, store} = await picker('tessera-view-picker', 'quarter:2026-Q4');
    const select = deep(host, 'select') as HTMLSelectElement;
    chooseOption(select, 'v:knn');
    store.set('view', {...store.get('view'), id: 'knn'});
    await settle(host);
    chooseOption(deep(host, 'select') as HTMLSelectElement, 'g:quarter');
    expect(switched(store)).toEqual(['knn', 'quarter:2026-Q4']);
  });

  it('else enters a group at its first view in creation order, never its first key by sort', async () => {
    const {host, store} = await picker('tessera-view-picker', 'knn');
    chooseOption(deep(host, 'select') as HTMLSelectElement, 'g:quarter');
    expect(switched(store)).toEqual(['quarter:2026-Q2']);
  });

  it('announces the switch with sameFrame from the two views’ quantisation', async () => {
    const seen: {from: string; to: string; sameFrame: boolean}[] = [];
    document.body.addEventListener('tessera-viewswitch', (e) => seen.push((e as CustomEvent).detail));
    const {host} = await picker('tessera-view-picker', 'quarter:2026-Q3');
    chooseOption(deep(host, 'select') as HTMLSelectElement, 'g:world');
    expect(seen).toEqual([{from: 'quarter:2026-Q3', to: 'world:2026-Q3', sameFrame: false}]);
  });
});

describe('<tessera-key-picker>', () => {
  it('walks the roster in creation order, never by interpreting keys', async () => {
    const {host} = await picker('tessera-key-picker', 'quarter:2026-Q2');
    const values = deepAll(host, 'option').map((o) => (o as HTMLOptionElement).value);
    expect(values).toEqual(['quarter:2026-Q2', 'quarter:2026-Q10', 'quarter:2026-Q3', 'quarter:2026-Q4']);
  });

  it('draws the label the rule gives with the key after it, and the key alone where there is none', async () => {
    const {host} = await picker('tessera-key-picker', 'quarter:2026-Q2');
    const text = deepAll(host, 'option').map((o) => o.textContent?.trim() ?? '');
    expect(text[0]).toBe('Apr – Jun 2026 · 2026-Q2');
    expect(text[1]).toBe('Long quarter · 2026-Q10');
    expect(text[3]).toBe('2026-Q4');
  });

  it('draws a members group under its own heading, labelled through the owning group', async () => {
    const {host} = await picker('tessera-key-picker', 'world:2026-Q10');
    expect(deepAll(host, 'option').map((o) => o.textContent?.trim())[1]).toBe('Long quarter · 2026-Q10');
    expect(deep(host, '[part="label"]')?.textContent).toBe('Quarterly map');
  });

  it('heads the select with the group’s title', async () => {
    const {host} = await picker('tessera-key-picker', 'quarter:2026-Q2');
    expect(deep(host, '[part="label"]')?.textContent).toBe('Quarter');
  });

  it('disables previous at the first view and next at the last, and never wraps', async () => {
    const first = await picker('tessera-key-picker', 'quarter:2026-Q2');
    const buttons = () => deepAll(first.host, 'button') as HTMLButtonElement[];
    expect(buttons().map((b) => b.disabled)).toEqual([true, false]);
    buttons()[0]!.click();
    expect(switched(first.store)).toEqual([]);
    buttons()[1]!.click();
    expect(switched(first.store)).toEqual(['quarter:2026-Q10']);

    const last = await picker('tessera-key-picker', 'quarter:2026-Q4');
    const ends = deepAll(last.host, 'button') as HTMLButtonElement[];
    expect(ends.map((b) => b.disabled)).toEqual([false, true]);
    ends[1]!.click();
    expect(switched(last.store)).toEqual([]);
  });

  it('switches on the select, and announces a step within a group as the same frame', async () => {
    const seen: {sameFrame: boolean}[] = [];
    document.body.addEventListener('tessera-viewswitch', (e) => seen.push((e as CustomEvent).detail));
    const {host, store} = await picker('tessera-key-picker', 'quarter:2026-Q2');
    chooseOption(deep(host, 'select') as HTMLSelectElement, 'quarter:2026-Q3');
    expect(switched(store)).toEqual(['quarter:2026-Q3']);
    expect(seen).toEqual([{from: 'quarter:2026-Q2', to: 'quarter:2026-Q3', sameFrame: true}]);
  });

  it('names its restyling surface in the elements’ own convention', async () => {
    const {host} = await picker('tessera-key-picker', 'quarter:2026-Q3');
    const parts = (name: string) => deepAll(host, `[part="${name}"]`);
    expect(parts('label')).toHaveLength(1);
    expect(parts('entry')).toHaveLength(1);
    expect(parts('select')).toHaveLength(1);
    expect(parts('step').map((b) => b.getAttribute('data-direction'))).toEqual(['prev', 'next']);
    const view = await picker('tessera-view-picker', 'quarter:2026-Q3');
    expect(deepAll(view.host, '[part="select"]')).toHaveLength(1);
    expect(deepAll(view.host, '[part="label"]')).toHaveLength(1);
  });

  it('renders nothing for a plain view, which is in no group', async () => {
    const {host} = await picker('tessera-key-picker', 'pca64');
    expect(deep(host, 'select')).toBeNull();
  });
});

describe('<tessera-map> at a switch', () => {
  /** The map, holding a store that has already pushed its first camera. */
  async function map(id: string) {
    const m = meta();
    const store = fakeStore({meta: m, status: status({})});
    store.set('view', {...store.get('view'), id});
    store.setFrame(m.views.find((v) => v.id === id)!.quantisation);
    const host = await mount('<tessera-map></tessera-map>');
    (host.querySelector('tessera-map') as unknown as {store: unknown}).store = store;
    await settle(host);
    return {host, store, m};
  }

  const pushes = (store: FakeStore) => store.calls.filter((c) => c.name === 'setView').length;

  it('does not move the camera on a switch within a group — the frame is the same', async () => {
    const {host, store} = await map('quarter:2026-Q2');
    const before = pushes(store);
    store.set('view', {...store.get('view'), id: 'quarter:2026-Q3'});
    await settle(host);
    expect(pushes(store)).toBe(before);
  });

  it('refits on a switch across frames, and drops the hover', async () => {
    const {host, store, m} = await map('quarter:2026-Q2');
    const el = host.querySelector('tessera-map') as unknown as {hover: unknown; zoom: number};
    el.hover = {x: 1, y: 1, title: 't', lines: []};
    const before = pushes(store);
    store.setFrame(m.views.find((v) => v.id === 'world:2026-Q2')!.quantisation);
    store.set('view', {...store.get('view'), id: 'world:2026-Q2'});
    await settle(host);
    expect(pushes(store)).toBeGreaterThan(before);
    expect(el.hover).toBeNull();
  });
});

describe('<tessera-item-card> and the views it reaches', () => {
  const detail = {
    fields: {title: 'A paper'},
    externalId: null,
    labels: ['quant-ph', '2024'],
    views: [
      {id: 'knn', x: 2 ** 31, y: 2 ** 30},
      {id: 'pca64', x: 2 ** 31, y: 2 ** 30}
    ],
    scoped: {mood: {'2026-Q2': 'calm', '2026-Q3': 'busy'}, size: {'2026-Q2': 12}}
  };

  async function card(id = 'knn') {
    const m = meta();
    const store = fakeStore({meta: m, status: status({})});
    store.set('view', {...store.get('view'), id});
    const host = await mount('<tessera-item-card></tessera-item-card>');
    const el = host.querySelector('tessera-item-card') as TesseraItemCard;
    (el as unknown as {store: unknown}).store = store;
    el.item = {id: 7n, detail};
    await settle(host);
    return {host, el, store};
  }

  it('draws a chip per reachable view, the current one marked, and the satisfied labels', async () => {
    const {host} = await card();
    const chips = deepAll(host, '[part="view-chip"]');
    expect(chips.map((c) => c.textContent?.trim())).toEqual(['knn', 'pca64']);
    expect(chips.map((c) => c.getAttribute('aria-current'))).toEqual(['true', 'false']);
    expect(deepAll(host, '[part="label-chip"]').map((c) => c.textContent?.trim())).toEqual(['quant-ph', '2024']);
  });

  it('follows an item into another view, with the position dequantised under that view’s frame', async () => {
    const {host} = await card();
    let seen: {view: string; x: number; y: number} | null = null;
    document.body.addEventListener('tessera-viewfollow', (e) => (seen = (e as CustomEvent).detail));
    (deepAll(host, '[part="view-chip"]')[1] as HTMLButtonElement).click();
    // `pca64` is quantised against the geographic extent: the mid-point of a 32-bit axis is its
    // centre in data coordinates, which is a different number in each view.
    expect(seen).toEqual({view: 'pca64', x: 0, y: -45});
  });

  it('draws the group-scoped values as rows headed by the key, in the order served', async () => {
    const {host} = await card();
    expect(deepAll(host, '[part="scoped"] [part="key"]').map((k) => k.textContent?.trim())).toEqual(['2026-Q2', '2026-Q3']);
    const rows = deepAll(host, '[part="scoped"] [part="field"]');
    expect(rows.map((r) => [r.getAttribute('data-key'), r.getAttribute('data-name')])).toEqual([
      ['2026-Q2', 'mood'],
      ['2026-Q2', 'size'],
      ['2026-Q3', 'mood']
    ]);
  });

  it('draws no chips where the session reaches no view of the item', async () => {
    const {host, el} = await card();
    el.item = {id: 7n, detail: {...detail, views: [], labels: [], scoped: {}}};
    await settle(host);
    expect(deepAll(host, '[part="view-chip"]')).toEqual([]);
    expect(deep(host, '[part="scoped"]')).toBeNull();
  });
});

describe('<tessera-explorer>', () => {
  it('puts both pickers at the top of the toolbar slot', async () => {
    const host = await mount('<tessera-explorer></tessera-explorer>');
    const el = host.querySelector('tessera-explorer') as unknown as {store: unknown};
    const store = fakeStore({meta: meta(), status: status({})});
    store.set('view', {...store.get('view'), id: 'quarter:2026-Q3'});
    el.store = store;
    await settle(host);
    const toolbar = (host.querySelector('tessera-explorer') as HTMLElement).shadowRoot!.querySelector('slot[name="toolbar"]')!;
    const tags = [...toolbar.children].map((c) => c.tagName.toLowerCase());
    expect(tags.slice(0, 2)).toEqual(['tessera-view-picker', 'tessera-key-picker']);
    expect(tags).toContain('tessera-legend');
    expect(deep(host, '[part="view-chip"]')).toBeNull();
  });

  it('follows an item into another view: the switch, then the camera once the frame is drawn', async () => {
    const host = await mount('<tessera-explorer></tessera-explorer>');
    const el = host.querySelector('tessera-explorer') as unknown as {store: unknown; map: {lookAt(x: number, y: number): boolean} | null};
    const store = fakeStore({meta: meta(), status: status({})});
    store.set('view', {...store.get('view'), id: 'quarter:2026-Q2'});
    store.setFrame(FLAT);
    el.store = store;
    await settle(host);
    const looks: [number, number][] = [];
    const map = el.map!;
    map.lookAt = (x, y) => {
      looks.push([x, y]);
      return true;
    };
    deep(host, 'tessera-map')!.dispatchEvent(new CustomEvent('tessera-viewfollow', {detail: {view: 'world:2026-Q2', x: 10, y: 20}, bubbles: true, composed: true}));
    expect(store.calls.filter((c) => c.name === 'setCurrentView').map((c) => c.args[0])).toEqual(['world:2026-Q2']);
    // Across frames the camera waits for the new view's own composition.
    expect(looks).toEqual([]);
    store.setFrame(GEO);
    store.set('view', {...store.get('view'), id: 'world:2026-Q2'});
    await settle(host);
    expect(looks).toEqual([]);
    store.set('view', {...store.get('view'), id: 'world:2026-Q2', composition: DRAWN});
    await settle(host);
    expect(looks).toEqual([[10, 20]]);
  });
});
