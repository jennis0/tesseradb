import {afterEach, describe, expect, it} from 'vitest';
import type {Meta} from '@tesseradb/client';
import '../src/item-card.js';
import '../src/filter.js';
import '../src/filter-panel.js';
import type {TesseraItemCard} from '../src/item-card.js';
import type {TesseraFilter} from '../src/filter.js';
import {deep, deepAll, fakeStore, mount, settle, status} from './fake-store.js';

afterEach(() => {
  document.body.innerHTML = '';
});

const META: Meta = {
  apiVersion: 1,
  idset: 0,
  views: [{id: 's0', displayName: 'default', quantisation: {xMin: 0, xMax: 1, yMin: 0, yMax: 1}, projection: 'none', worldAspect: null, tileScheme: null, tile: null}],
  declaredScalars: [
    {name: 'archive', arrowType: 'u16', category: {vocabulary: 'a', kind: 'declared', visibility: 'public'}, render: true, index: true},
    {name: 'submitted_at', arrowType: 'timestamp_us', category: null, render: true, index: true},
    {name: 'title', arrowType: 'utf8', category: null, render: false, index: true}
  ],
  layers: [],
  selection: {kMin: 1, kMaxMarks: 500, maxK: 5000, thetaTargetMarks: 10, maxUnderlayOffset: 0, maxCategoryValues: 1000, maxRegionVertices: 10_000, maxRegionCells: 262_144},
  maxTilesPerRequest: 4096,
  filterOperands: [
    {column: 'archive', family: 'category', operands: ['eq', 'in']},
    {column: 'title', family: 'text', operands: ['match', 'phrase']},
    {column: 'submitted_at', family: 'numeric', operands: ['range']}
  ]
};

describe('<tessera-item-card>', () => {
  it('renders fields by name in declaration order, presented by type, with a slot per field', async () => {
    const host = await mount('<tessera-item-card><a slot="field-title" href="#">my link</a></tessera-item-card>');
    const el = host.querySelector('tessera-item-card') as TesseraItemCard;
    el.meta = META;
    // `archive` absent from the record, the extra `note` undeclared: order is declared-then-extra,
    // and the gap is named rather than shifting the fields after it.
    el.item = {id: 12345678901234567890n, detail: {fields: {note: 'x', title: 'A title', submitted_at: 1_700_000_000_000_000}, externalId: null}};
    await settle(host);
    const names = deepAll(host, '[part="field"]').map((f) => f.getAttribute('data-name'));
    // The title first, then the fields in declared-then-extra order, then the id; the absent
    // `archive` is not rendered — position never lies.
    expect(names).toEqual(['title', 'submitted_at', 'note', 'tessera_id']);
    expect(deep(host, '[part="field"][data-name="tessera_id"] [part="value"]')?.textContent).toBe('12345678901234567890');
    expect(deep(host, '[part="field"][data-name="submitted_at"] [part="value"]')?.textContent).toBe('2023-11-14');
    expect(deep(host, 'slot[name="field-title"]')).not.toBeNull();
    expect(host.textContent).toContain('my link');
    expect(deep(host, '[part="field"][data-name="archive"]')).toBeNull();
  });

  it('fires tessera-open with the id as a decimal string, bubbling and composed', async () => {
    const host = await mount('<tessera-item-card></tessera-item-card>');
    const el = host.querySelector('tessera-item-card') as TesseraItemCard;
    el.item = {id: 2n ** 63n + 1n, detail: {fields: {}, externalId: null}};
    await settle(host);
    let detail: {id?: string} | null = null;
    document.body.addEventListener('tessera-open', (e) => (detail = (e as CustomEvent).detail));
    (deep(host, '[part="open"]') as HTMLButtonElement).click();
    expect(detail!.id).toBe('9223372036854775809');
    expect(typeof detail!.id).toBe('string');
  });

  it('distinguishes a miss from a broken pick from a refusal', async () => {
    const host = await mount('<tessera-item-card></tessera-item-card>');
    const el = host.querySelector('tessera-item-card') as TesseraItemCard;
    el.pick = {kind: 'miss'};
    await settle(host);
    expect(deep(host, '[part="state"]')?.getAttribute('data-state')).toBe('empty');
    expect(deep(host, '[part="state"]')?.textContent).toContain('Nothing under the cursor');

    el.pick = {kind: 'broken', index: 7, layer: 'tessera-marks-p1', hasIds: false, idCount: 0};
    await settle(host);
    expect(deep(host, '[part="state"]')?.getAttribute('data-state')).toBe('refused');
    expect(deep(host, '[part="refusal"]')?.textContent).toContain('Layer fault');

    el.pick = null;
    el.refusal = {code: 'not-found', detail: 'nope'};
    await settle(host);
    expect(deep(host, '[part="refusal"]')?.textContent).toContain('not-found');
  });

  it('reads the store’s selection when nothing is given by property', async () => {
    const host = await mount('<tessera-item-card></tessera-item-card>');
    const el = host.querySelector('tessera-item-card') as TesseraItemCard;
    const store = fakeStore({meta: META, status: status({})});
    el.store = store;
    await settle(host);
    store.set('selection', {item: {id: 5n, detail: {fields: {archive: 'cs'}, externalId: null}}, itemRefusal: null, artifact: null, artifactRefusal: null});
    await settle(host);
    expect(deep(host, '[part="field"][data-name="archive"] [part="value"]')?.textContent).toBe('cs');
  });
});

describe('<tessera-filter>', () => {
  it('submits a typed category key never listed, and never renders "no such value"', async () => {
    const host = await mount('<tessera-filter column="archive"></tessera-filter>');
    const el = host.querySelector('tessera-filter') as TesseraFilter;
    const store = fakeStore({meta: META, status: status({})});
    store.set('filters', {draft: {archive: {family: 'category', keys: []}}, expr: null, values: {archive: [{code: 1, key: 'cs', title: 'Computer Science'}]}, valueErrors: {}});
    el.store = store;
    await settle(host);
    const entry = deep(host, '[part="entry"]') as HTMLInputElement;
    entry.value = 'zz.unlisted';
    entry.dispatchEvent(new Event('input'));
    entry.dispatchEvent(new KeyboardEvent('keydown', {key: 'Enter'}));
    await settle(host);
    const sent = store.calls.find((c) => c.name === 'setFilters');
    expect(sent).toBeDefined();
    expect((sent!.args[0] as {archive: {keys: string[]}}).archive.keys).toEqual(['zz.unlisted']);
    expect(host.textContent + [...deepAll(host, '*')].map((e) => e.textContent).join(' ')).not.toMatch(/no such value/i);
    // The typed key appears as a tick, chosen, beside the listed one.
    const ticks = deepAll(host, '[part="tick"]').map((t) => t.textContent?.trim());
    expect(ticks[0]).toBe('zz.unlisted');
  });

  it('a category is a search over the enumeration with the top few as checkboxes and Show N more', async () => {
    const host = await mount('<tessera-filter column="archive"></tessera-filter>');
    const el = host.querySelector('tessera-filter') as TesseraFilter;
    const store = fakeStore({meta: META, status: status({})});
    const values = Array.from({length: 171}, (_, i) => ({code: i, key: `cat-${String(i).padStart(3, '0')}`, title: null}));
    store.set('filters', {draft: {archive: {family: 'category', keys: []}}, expr: null, values: {archive: values}, valueErrors: {}});
    el.store = store;
    await settle(host);
    // Never a scrolling list of 171: four checkboxes and the rest behind one button.
    expect(deepAll(host, '[part="tick"]').length).toBe(4);
    expect(deep(host, '[part="more"]')?.textContent).toBe('Show 167 more…');
    // Typing narrows the list to what matches.
    const entry = deep(host, '[part="entry"]') as HTMLInputElement;
    entry.value = 'cat-16';
    entry.dispatchEvent(new Event('input'));
    await settle(host);
    expect(deepAll(host, '[part="tick"]').length).toBe(10);
    expect(deep(host, '[part="more"]')).toBeNull();
  });

  it('renders a refused enumeration as a refusal beside a free entry, not as an absent control', async () => {
    const host = await mount('<tessera-filter column="archive"></tessera-filter>');
    const el = host.querySelector('tessera-filter') as TesseraFilter;
    const store = fakeStore({meta: META, status: status({})});
    store.set('filters', {draft: {archive: {family: 'category', keys: []}}, expr: null, values: {}, valueErrors: {archive: {code: 'derived', detail: 'not listable'}}});
    el.store = store;
    await settle(host);
    expect(deep(host, '[part="entry"]')).not.toBeNull();
    expect(deep(host, '[part="refusal"]')?.textContent).toContain('derived');
  });

  it('a text operand offers phrase only when the column publishes it, and debounces typing', async () => {
    const host = await mount('<tessera-filter column="title"></tessera-filter>');
    const el = host.querySelector('tessera-filter') as TesseraFilter;
    const store = fakeStore({meta: META, status: status({})});
    store.set('filters', {draft: {title: {family: 'text', query: '', mode: 'all'}}, expr: null, values: {}, valueErrors: {}});
    el.store = store;
    await settle(host);
    expect(deepAll(host, '[part="mode"] button').map((o) => o.getAttribute('data-mode'))).toEqual(['all', 'phrase']);
    const entry = deep(host, '[part="entry"]') as HTMLInputElement;
    entry.value = 'graph';
    entry.dispatchEvent(new Event('input'));
    expect(store.calls.some((c) => c.name === 'setFilters')).toBe(false);
    await new Promise((r) => setTimeout(r, 400));
    const sent = store.calls.find((c) => c.name === 'setFilters');
    expect((sent!.args[0] as {title: {query: string}}).title.query).toBe('graph');
  });
});

describe('<tessera-filter-panel>', () => {
  it('renders one control per operand meta offers, keyed, with chips and clear all', async () => {
    const host = await mount('<tessera-filter-panel></tessera-filter-panel>');
    const store = fakeStore({meta: META, status: status({})});
    store.set('filters', {draft: {archive: {family: 'category', keys: ['cs']}, title: {family: 'text', query: '', mode: 'all'}, submitted_at: {family: 'numeric', gte: null, lte: null}}, expr: null, values: {}, valueErrors: {}});
    (host.querySelector('tessera-filter-panel') as unknown as {store: unknown}).store = store;
    await settle(host);
    const filters = deepAll(host, 'tessera-filter');
    expect(filters.map((f) => f.getAttribute('column'))).toEqual(['archive', 'title', 'submitted_at']);
    expect(deepAll(host, '[part="chip"]').map((c) => c.textContent)).toEqual(['archive: cs']);
    const before = filters[0];
    store.set('status', status({status: 'loading'}));
    await settle(host);
    // A store tick does not rebuild the control under the user.
    expect(deepAll(host, 'tessera-filter')[0]).toBe(before);
    (deep(host, '[part="clear"]') as HTMLButtonElement).click();
    const sent = store.calls.find((c) => c.name === 'setFilters');
    expect((sent!.args[0] as {archive: {keys: string[]}}).archive.keys).toEqual([]);
  });
});
