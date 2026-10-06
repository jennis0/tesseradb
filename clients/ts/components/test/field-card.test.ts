import {afterEach, describe, expect, it} from 'vitest';
import type {FilterDraft, FiltersProjection, Meta} from '@tesseradb/client';
import '../src/field-card.js';
import type {TesseraFieldCard} from '../src/field-card.js';
import {aggregateEntry, answerAggregate, deep, deepAll, fakeStore, meta, mount, registered, scalar, settle, status, type FakeStore} from './fake-store.js';

afterEach(() => {
  document.body.innerHTML = '';
});

const topicsLayer = {
  name: 'topics',
  title: 'Topics',
  views: ['s0'],
  membership: 'enumerated',
  hierarchy: {kind: 'nested', pruneChildren: false},
  levels: [],
  computedContent: ['centroid'],
  shape: null,
  suppliedContent: ['name'],
  depsOn: [],
  colourable: true,
  version: 1
} as unknown as Meta['layers'][number];

const META = meta({
  declaredScalars: [
    scalar('archive', 'u16', {category: {vocabulary: 'a', kind: 'declared', visibility: 'public'}, render: true, homes: ['rendered']}),
    scalar('submitted_at', 'timestamp_us', {render: true, homes: ['rendered']}),
    scalar('authors', 'u16', {render: false}),
    scalar('title', 'utf8')
  ],
  filterOperands: [
    {column: 'archive', family: 'category', operands: ['eq', 'in']},
    {column: 'submitted_at', family: 'numeric', operands: ['range']},
    {column: 'authors', family: 'numeric', operands: ['range']},
    {column: 'title', family: 'text', operands: ['match', 'phrase']}
  ],
  layers: [topicsLayer]
});

const filtersOf = (draft: FilterDraft, over: Partial<FiltersProjection> = {}): FiltersProjection => ({draft, expr: null, highlight: null, members: [], suggestions: {}, suggestErrors: {}, suggestEpoch: 0, ...over});

async function mountCard(field: string, draft: FilterDraft = {filter: {}, highlight: {}}, over: Partial<FiltersProjection> = {}, attrs = '') {
  const host = await mount(`<tessera-field-card field="${field}" ${attrs}></tessera-field-card>`);
  const card = host.querySelector('tessera-field-card') as TesseraFieldCard;
  const store = fakeStore({meta: META, status: status({}), filters: filtersOf(draft, over)});
  store.set('view', {...store.get('view'), id: 's0'});
  card.store = store;
  await settle(host);
  return {host, card, store};
}

/** The spec registered under the id beginning `prefix`. */
const spec = (store: FakeStore, prefix: string) => [...registered(store)].find(([id]) => id.startsWith(prefix))?.[1];
const lastFilters = (store: FakeStore) => store.calls.filter((c) => c.name === 'setFilters').at(-1)?.args[0] as FilterDraft | undefined;
const rows = (host: HTMLElement) => deepAll(host, '[part~="row"]') as HTMLElement[];

/** Answer the card's counts in view and over everything matching. */
async function answer(host: HTMLElement, store: FakeStore, subject: Parameters<typeof aggregateEntry>[0], match: Parameters<typeof aggregateEntry>[0]) {
  answerAggregate(store, 'field-subject', aggregateEntry(subject));
  answerAggregate(store, 'field-match', aggregateEntry(match));
  await settle(host);
}

describe('<tessera-field-card> on a category', () => {
  it('registers its counts in view with the highlight and over everything matching, each leaving out its own clause', async () => {
    const {store} = await mountCard('archive');
    expect(spec(store, 'field-subject')).toEqual({groupings: [{by: {field: 'archive', top: 5}}], subject: 'view', highlighted: true, without: 'archive'});
    expect(spec(store, 'field-match')).toEqual({groupings: [{by: {field: 'archive', top: 100}}], without: 'archive'});
    expect(spec(store, 'field-summary')).toBeUndefined();
  });

  it('counts the values its clauses name whether or not they are among the commonest', async () => {
    const {store} = await mountCard('archive', {filter: {archive: {family: 'category', keys: ['math']}}, highlight: {archive: {family: 'category', keys: ['cs']}}});
    expect(spec(store, 'field-subject')).toMatchObject({groupings: [{by: {field: 'archive', top: 5}}, {by: {field: 'archive', values: ['cs', 'math']}}]});
    expect(spec(store, 'field-match')).toMatchObject({groupings: [{by: {field: 'archive', top: 100}}, {by: {field: 'archive', values: ['cs', 'math']}}]});
  });

  it('draws each value with its two counts and bars, each a share of its own set', async () => {
    const {host, store} = await mountCard('archive');
    await answer(
      host,
      store,
      [{rows: [{key: 'hep', title: 'hep-ph', count: 60}, {key: 'astro', count: 40}], total: 200, groups: 7}],
      [{rows: [{key: 'cs', count: 1000}, {key: 'hep', count: 500}, {key: 'astro', count: 250}], total: 5000}]
    );
    expect(rows(host).map((r) => [r.dataset.key, r.querySelector('[part="name"]')!.textContent, r.querySelector('[part="count"]')!.textContent])).toEqual([
      ['hep', 'hep-ph', '60 / 500'],
      ['astro', 'astro', '40 / 250']
    ]);
    const widths = (r: HTMLElement) => [parseFloat((r.querySelector('[part="bar-subject"]') as HTMLElement).style.width), parseFloat((r.querySelector('[part="bar-match"]') as HTMLElement).style.width)];
    // A plain share of each set, never stretched to the largest row.
    expect(widths(rows(host)[0]!)).toEqual([30, 10]);
    expect(deep(host, '[part="more"]')!.textContent).toBe('5 more in view');
  });

  it('asks for a value in view that is not among the commonest matching by name, and shows both its counts', async () => {
    const {host, store} = await mountCard('archive');
    await answer(host, store, [{rows: [{key: 'rare', count: 7}], total: 7, groups: 1}], [{rows: [{key: 'cs', count: 1000}], total: 5000}]);
    expect(spec(store, 'field-match')).toEqual({groupings: [{by: {field: 'archive', top: 100}}, {by: {field: 'archive', values: ['rare']}}], without: 'archive'});
    answerAggregate(store, 'field-match', aggregateEntry([{rows: [{key: 'cs', count: 1000}], total: 5000}, {rows: [{key: 'rare', count: 50}]}]));
    await settle(host);
    // Its answer does not change what is asked.
    expect(spec(store, 'field-match')).toEqual({groupings: [{by: {field: 'archive', top: 100}}, {by: {field: 'archive', values: ['rare']}}], without: 'archive'});
    const [rare] = rows(host);
    expect(rare!.querySelector('[part="count"]')!.textContent).toBe('7 / 50');
    expect(parseFloat((rare!.querySelector('[part="bar-match"]') as HTMLElement).style.width)).toBe(1);
  });

  it('puts a value in or out of the filter and the highlight from its row', async () => {
    const {host, store} = await mountCard('archive');
    await answer(host, store, [{rows: [{key: 'cs', count: 10}], total: 10}], [{rows: [{key: 'cs', count: 20}], total: 20}]);
    const changes: unknown[] = [];
    host.addEventListener('tessera-filterchange', (e) => changes.push((e as CustomEvent).detail));
    const row = rows(host)[0]!;
    (row.querySelector('[part="filter"]') as HTMLButtonElement).click();
    expect(lastFilters(store)).toEqual({filter: {archive: {family: 'category', keys: ['cs']}}, highlight: {}});
    (row.querySelector('[part="highlight"]') as HTMLButtonElement).click();
    expect(lastFilters(store)).toEqual({filter: {}, highlight: {archive: {family: 'category', keys: ['cs']}}});
    expect(changes).toEqual([
      {column: 'archive', verb: 'filter', expr: {archive: {in: ['cs']}}},
      {column: 'archive', verb: 'highlight', expr: {archive: {in: ['cs']}}}
    ]);
    // Pressed again, a value comes out of the clause.
    store.set('filters', filtersOf({filter: {archive: {family: 'category', keys: ['cs']}}, highlight: {}}));
    await settle(host);
    const filter = rows(host)[0]!.querySelector('[part="filter"]') as HTMLButtonElement;
    expect(filter.getAttribute('aria-pressed')).toBe('true');
    filter.click();
    expect(lastFilters(store)).toEqual({filter: {archive: {family: 'category', keys: []}}, highlight: {}});
  });

  it('greys the values its filter leaves out, showing only how many they would add, and shares the rest within it', async () => {
    const {host, store} = await mountCard('archive', {filter: {archive: {family: 'category', keys: ['cs']}}, highlight: {}});
    await answer(
      host,
      store,
      [{rows: [{key: 'cs', count: 40}, {key: 'math', count: 30}], total: 70}, {rows: [{key: 'cs', count: 40}]}],
      [{rows: [{key: 'math', count: 900}, {key: 'cs', count: 100}], total: 1000}, {rows: [{key: 'cs', count: 100}]}]
    );
    const [cs, math] = rows(host);
    expect(cs!.dataset.state).toBe('in');
    expect(cs!.querySelector('[part="count"]')!.textContent).toBe('40 / 100');
    expect(parseFloat((cs!.querySelector('[part="bar-subject"]') as HTMLElement).style.width)).toBe(100);
    expect(math!.dataset.state).toBe('out');
    expect(math!.querySelector('[part="count"]')!.textContent).toBe('900');
    expect(parseFloat((math!.querySelector('[part="bar-match"]') as HTMLElement).style.width)).toBe(0);
  });

  it('colours the map by the field from its paint button, pressed while it does, and by nothing when pressed again', async () => {
    const {host, store} = await mountCard('archive');
    const colours: unknown[] = [];
    host.addEventListener('tessera-colourchange', (e) => colours.push((e as CustomEvent).detail));
    const paint = () => deep(host, '[part="paint"]') as HTMLButtonElement;
    expect(paint().getAttribute('aria-pressed')).toBe('false');
    paint().click();
    expect(store.calls.filter((c) => c.name === 'setColourBy').at(-1)!.args).toEqual(['archive']);
    store.set('legend', {...store.get('legend'), colourBy: 'archive', categories: {archive: [{key: 'cs', code: 1, title: null}]}, ranks: {archive: {1: 0}}});
    await answer(host, store, [{rows: [{key: 'cs', count: 10}], total: 10}], [{rows: [{key: 'cs', count: 10}], total: 10}]);
    expect(paint().getAttribute('aria-pressed')).toBe('true');
    // The colouring card shows each value's colour, which opens the colour picker.
    const swatch = rows(host)[0]!.querySelector('button[part="swatch"]') as HTMLButtonElement;
    expect(swatch.getAttribute('style')).toContain('--c:');
    // Its solid bar is drawn in the value's own colour.
    const colour = /--c:([^;]+)/.exec(swatch.getAttribute('style')!)![1]!;
    expect((rows(host)[0]!.querySelector('[part="bar-subject"]') as HTMLElement).getAttribute('style')).toContain(`background:${colour}`);
    swatch.click();
    await settle(host);
    expect(deep(host, '[part="colour-popover"]')).not.toBeNull();
    paint().click();
    expect(store.calls.filter((c) => c.name === 'setColourBy').at(-1)!.args).toEqual([null]);
    expect(colours).toEqual([{colourBy: 'archive'}, {colourBy: null}]);
  });

  it('offers no paint button on a field the map cannot be coloured by', async () => {
    const {host} = await mountCard('authors');
    expect(deep(host, '[part="paint"]')).toBeNull();
  });

  it('folds to its heading with a small chart, asking for its counts in view alone', async () => {
    const {host, card, store} = await mountCard('archive', {filter: {}, highlight: {}}, {}, 'folded');
    expect(spec(store, 'field-subject')).toBeDefined();
    expect(spec(store, 'field-match')).toBeUndefined();
    answerAggregate(store, 'field-subject', aggregateEntry([{rows: [{key: 'cs', count: 30}, {key: 'math', count: 10}], total: 40}]));
    await settle(host);
    expect(rows(host)).toHaveLength(0);
    expect(deepAll(host, '[part="spark"] span').map((b) => (b as HTMLElement).style.height)).toEqual(['14px', '5px']);
    const toggled: unknown[] = [];
    card.addEventListener('tessera-fold', (e) => toggled.push((e as CustomEvent).detail));
    (deep(host, '[part="fold"]') as HTMLButtonElement).click();
    await settle(host);
    expect(toggled).toEqual([{field: 'archive', folded: false}]);
    expect(spec(store, 'field-match')).toBeDefined();
  });
});

describe('<tessera-field-card> on a date', () => {
  const YEAR = (y: number) => Date.UTC(y, 0, 1);
  const bins = (counts: number[]) => counts.map((count, i) => ({lower: YEAR(2019 + i), upper: YEAR(2020 + i), count}));

  it('registers a sampled histogram in view and over everything matching, and the field’s figures over what the viewer may see', async () => {
    const {store} = await mountCard('submitted_at');
    const histogram = {by: {field: 'submitted_at', bins: 20, sample: 100_000}};
    expect(spec(store, 'field-subject')).toEqual({groupings: [histogram], subject: 'view', highlighted: true, without: 'submitted_at'});
    expect(spec(store, 'field-match')).toEqual({groupings: [histogram], without: 'submitted_at'});
    expect(spec(store, 'field-summary')).toEqual({groupings: [{by: {field: 'submitted_at', summary: true}}], subject: 'visible'});
  });

  it('turns a range dragged across the bins into the field’s range clause, in the position chosen', async () => {
    const {host, store} = await mountCard('submitted_at');
    await answer(host, store, [{rows: bins([1, 2, 3, 4]), total: 10}], [{rows: bins([10, 20, 30, 40]), total: 100}]);
    const plot = deep(host, '[part="plot"]') as HTMLElement;
    plot.getBoundingClientRect = () => ({left: 0, top: 0, width: 400, height: 56, right: 400, bottom: 56, x: 0, y: 0, toJSON: () => ({})}) as DOMRect;
    const pointer = (type: string, x: number) => plot.dispatchEvent(new PointerEvent(type, {clientX: x, button: 0, pointerId: 1, bubbles: true}));
    pointer('pointerdown', 150);
    pointer('pointermove', 250);
    await settle(host);
    expect(deep(host, '[part~="band"][data-verb="brush"]')).not.toBeNull();
    pointer('pointerup', 250);
    await settle(host);
    const box = deep(host, '[part="brush"]')!;
    expect(box.textContent!.replace(/\s+/g, ' ').trim()).toContain('2020 – 2021 · 50 items');
    (deep(host, '[part="brush-filter"]') as HTMLButtonElement).click();
    // The whole years, in microseconds: from the first instant of 2020 to the last of 2021.
    expect(lastFilters(store)).toEqual({filter: {submitted_at: {family: 'numeric', gte: YEAR(2020) * 1000, lte: YEAR(2022) * 1000 - 1}}, highlight: {}});
    // A range reaching the last bin leaves its upper end open, since the bins span every value.
    store.set('filters', filtersOf({filter: {}, highlight: {}}));
    pointer('pointerdown', 350);
    pointer('pointerup', 399);
    await settle(host);
    expect(deep(host, '[part="brush"]')!.textContent).toContain('2022');
    (deep(host, '[part="brush-filter"]') as HTMLButtonElement).click();
    expect(lastFilters(store)).toEqual({filter: {submitted_at: {family: 'numeric', gte: YEAR(2022) * 1000, lte: null}}, highlight: {}});
  });

  it('chooses a range with the arrow keys and Enter, and highlights it', async () => {
    const {host, store} = await mountCard('submitted_at');
    await answer(host, store, [{rows: bins([1, 2, 3, 4]), total: 10}], [{rows: bins([10, 20, 30, 40]), total: 100}]);
    const plot = deep(host, '[part="plot"]') as HTMLElement;
    const key = (k: string, shiftKey = false) => plot.dispatchEvent(new KeyboardEvent('keydown', {key: k, shiftKey, bubbles: true}));
    key('ArrowRight');
    key('ArrowRight');
    key('ArrowRight', true);
    key('Enter');
    await settle(host);
    (deep(host, '[part="brush-highlight"]') as HTMLButtonElement).click();
    // The first press takes the first bin, the next moves on, and Shift widens: 2020 and 2021.
    expect(lastFilters(store)).toEqual({filter: {}, highlight: {submitted_at: {family: 'numeric', gte: YEAR(2020) * 1000, lte: YEAR(2022) * 1000 - 1}}});
  });

  it('outlines a range set on the field, and marks counts taken from a sample as estimates', async () => {
    const {host, store} = await mountCard('submitted_at', {filter: {}, highlight: {submitted_at: {family: 'numeric', gte: YEAR(2020) * 1000, lte: null}}});
    await answer(host, store, [{rows: bins([1, 2]), total: 3}], [{rows: bins([1000, 2000]), total: 3000, sample: {sampled: true, items: 100}}]);
    const band = deep(host, '[part~="band"][data-verb="highlight"]') as HTMLElement;
    // From the first instant of 2020, half way along 2019 to 2021, to the open end.
    expect(band.style.left).toMatch(/^calc\(49\.9\d% - 1px\)$/);
    expect(band.style.width).toMatch(/^calc\(50\.0\d% \+ 2px\)$/);
    expect((deep(host, '[part~="bin"]') as HTMLElement).title).toContain('≈1,000 matching');
    const plot = deep(host, '[part="plot"]') as HTMLElement;
    plot.dispatchEvent(new KeyboardEvent('keydown', {key: 'ArrowRight', bubbles: true}));
    plot.dispatchEvent(new KeyboardEvent('keydown', {key: 'Enter', bubbles: true}));
    await settle(host);
    expect(deep(host, '[part="brush"]')!.textContent).toContain('≈1,000 items');
    // A press anywhere else closes the box.
    document.body.dispatchEvent(new PointerEvent('pointerdown', {bubbles: true, composed: true}));
    await settle(host);
    expect(deep(host, '[part="brush"]')).toBeNull();
  });

  it('says the range the arrow keys move and its count, for a reader, while it is chosen', async () => {
    const {host, store} = await mountCard('submitted_at');
    await answer(host, store, [{rows: bins([1, 2, 3, 4]), total: 10}], [{rows: bins([10, 20, 30, 40]), total: 100}]);
    const plot = deep(host, '[part="plot"]') as HTMLElement;
    const live = () => deep(host, '[role="status"][aria-live="polite"]')!.textContent!.trim();
    expect(live()).toBe('');
    plot.dispatchEvent(new KeyboardEvent('keydown', {key: 'ArrowRight', bubbles: true}));
    plot.dispatchEvent(new KeyboardEvent('keydown', {key: 'ArrowRight', shiftKey: true, bubbles: true}));
    await settle(host);
    expect(live()).toBe('2019 – 2020, 30 items');
  });

  it('gives the field’s figures in its heading', async () => {
    const {host, store} = await mountCard('submitted_at');
    answerAggregate(store, 'field-summary', aggregateEntry([{rows: []}], 's0', [{items: 30n, count: 25n, none: 5n, min: YEAR(1990), max: YEAR(2020), mean: YEAR(2011)}]));
    await settle(host);
    expect(deep(host, '[part="sub"]')!.textContent).toBe('n 25 · mean 1 Jan 2011');
  });
});

describe('<tessera-field-card> on a layer', () => {
  it('ranks the layer’s largest clusters by their counts in view, leaving out its own clauses', async () => {
    const host = await mount('<tessera-field-card field="cluster:topics"></tessera-field-card>');
    const card = host.querySelector('tessera-field-card') as TesseraFieldCard;
    const store = fakeStore({meta: META, status: status({}), filters: filtersOf({filter: {}, highlight: {}})});
    store.set('view', {...store.get('view'), id: 's0'});
    const row = (id: bigint, name: string, parentIds: bigint[] = []) => ({tesseraId: id, key: null, name, maskedCount: 10n, matchedCount: null, rung: 1, parentIds, childCount: 0});
    store.setBrowse('roots', {artifacts: [row(7n, 'optics', [1n]), row(8n, 'lasers')], parents: [], next: null});
    store.setBrowse('p:7', {artifacts: [], parents: [row(1n, 'physics')], next: null});
    card.store = store;
    await settle(host);
    await new Promise((r) => setTimeout(r, 0));
    await settle(host);
    expect(spec(store, 'field-subject')).toEqual({groupings: [{by: {layer: 'topics', artifacts: [7n, 8n]}}], subject: 'view', highlighted: true, withoutMembersOf: 'topics'});
    // The whole match also counts the layer's clusters, which the search box names.
    expect(spec(store, 'field-match')).toEqual({groupings: [{by: {layer: 'topics', artifacts: [7n, 8n]}}, {by: {layer: 'topics', top: 1}}], withoutMembersOf: 'topics'});
    await answer(host, store, [{rows: [{key: 8n, count: 5}, {key: 7n, count: 9}], total: 14}], [{rows: [{key: 8n, count: 50}, {key: 7n, count: 90}], total: 140}, {rows: [{key: 7n, count: 90}], groups: 12}]);
    expect(deep(host, 'tessera-cluster-filter')!.getAttribute('placeholder')).toBe('Search 12 clusters');
    // A share of a few per cent still reads as a bar; plain percentages otherwise.
    const optics = rows(host)[0]!;
    expect((optics.querySelector('[part="bar-match"]') as HTMLElement).getAttribute('style')).toContain('max(3px, 64.3%)');
    await new Promise((r) => setTimeout(r, 0));
    await settle(host);
    expect(rows(host).map((r) => [r.querySelector('[part="name"]')!.textContent, r.querySelector('[part="path"]')?.textContent ?? ''])).toEqual([
      ['optics', 'physics'],
      ['lasers', '']
    ]);
    const clauses: unknown[] = [];
    host.addEventListener('tessera-clausechange', (e) => clauses.push((e as CustomEvent).detail));
    (rows(host)[1]!.querySelector('[part="highlight"]') as HTMLButtonElement).click();
    expect(store.calls.filter((c) => c.name === 'setMembers').at(-1)!.args[0]).toEqual([{layer: 'topics', artifact: 8n, outside: false, verb: 'highlight', label: 'lasers'}]);
    expect(clauses).toEqual([{id: '8', layer: 'topics', outside: false, verb: 'highlight', on: true}]);
  });
});

describe('<tessera-field-card> across a change of viewer', () => {
  it('drops a walk still running for the viewer before, so nothing it met names a cluster to the next', async () => {
    const row = (id: bigint, name: string, childCount: number, parentIds: bigint[] = []) => ({tesseraId: id, key: null, name, maskedCount: 10n, matchedCount: null, rung: 0, parentIds, childCount});
    const host = await mount('<tessera-field-card field="cluster:topics"></tessera-field-card>');
    const card = host.querySelector('tessera-field-card') as TesseraFieldCard;
    const before = fakeStore({meta: META, status: status({}), filters: filtersOf({filter: {}, highlight: {}})});
    before.set('view', {...before.get('view'), id: 's0'});
    before.setBrowse('roots', {artifacts: [row(1n, 'seen only by the first viewer', 1)], parents: [], next: null});
    // The page under the root waits until the viewer has changed.
    let release: () => void = () => {};
    const held = new Promise<void>((r) => (release = r));
    const browse = before.browse.bind(before);
    before.browse = async (req) => {
      if (req.parent === 1n) {
        await held;
        return {artifacts: [row(2n, 'also the first viewer’s', 0, [1n])], parents: [], next: null};
      }
      return browse(req);
    };
    card.store = before;
    await settle(host);

    const after = fakeStore({meta: META, status: status({}), filters: filtersOf({filter: {}, highlight: {}})});
    after.set('view', {...after.get('view'), id: 's0'});
    // The next viewer is served a cluster under one the first viewer saw and it is not served.
    after.setBrowse('roots', {artifacts: [row(3n, 'theirs', 0, [1n])], parents: [], next: null});
    card.store = after;
    await settle(host);
    release();
    await new Promise((r) => setTimeout(r, 0));
    await settle(host);
    answerAggregate(after, 'field-subject', aggregateEntry([{rows: [{key: 3n, count: 4}], total: 4}]));
    await settle(host);
    expect(rows(host).map((r) => [r.querySelector('[part="name"]')!.textContent, r.querySelector('[part="path"]')?.textContent ?? ''])).toEqual([['theirs', '']]);
    expect(deepAll(host, '[part="name"], [part="path"]').map((e) => e.textContent).join(' ')).not.toContain('first viewer');
  });
});

describe('<tessera-field-card> on text', () => {
  it('is the field’s search box, and counts nothing', async () => {
    const {host, store} = await mountCard('title');
    expect(deep(host, 'tessera-filter')).not.toBeNull();
    expect(deep(host, '[part="paint"]')).toBeNull();
    expect(registered(store).size).toBe(0);
  });
});
