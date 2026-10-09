import {afterEach, describe, expect, it} from 'vitest';
import type {FilterDraft, FiltersProjection, Meta, Rgba} from '@mosaica/client';
import '../src/field-card.js';
import type {MosaicaFieldCard} from '../src/field-card.js';
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
  const host = await mount(`<mosaica-field-card field="${field}" ${attrs}></mosaica-field-card>`);
  const card = host.querySelector('mosaica-field-card') as MosaicaFieldCard;
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

describe('<mosaica-field-card> on a category', () => {
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

  it('names no more values than the server counts by name', async () => {
    const host = await mount('<mosaica-field-card field="archive"></mosaica-field-card>');
    const card = host.querySelector('mosaica-field-card') as MosaicaFieldCard;
    const store = fakeStore({meta: {...META, selection: {...META.selection, maxAggregateNamed: 2}}, status: status({}), filters: filtersOf({filter: {archive: {family: 'category', keys: ['a', 'b', 'c']}}, highlight: {}})});
    card.store = store;
    await settle(host);
    expect(spec(store, 'field-subject')).toMatchObject({groupings: [{by: {field: 'archive', top: 5}}, {by: {field: 'archive', values: ['a', 'b']}}]});
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

  it('asks for the values in view outside the commonest matching by name, once, and keeps the whole match’s request as it was', async () => {
    const {host, store} = await mountCard('archive');
    // As the real store: a registration made again drops the answer held, and each request is
    // answered from what the view holds.
    let inView = [{key: 'cs', count: 3}, {key: 'rare', count: 7}];
    const answerFor = (id: string, spec: {groupings: {by?: {values?: string[]}}[]}) =>
      id.startsWith('field-subject')
        ? aggregateEntry([{rows: inView, total: 10, groups: inView.length}])
        : id.startsWith('field-match')
          ? aggregateEntry([{rows: [{key: 'cs', count: 1000}], total: 5000}])
          : aggregateEntry([{rows: (spec.groupings[0]!.by!.values ?? []).map((key) => ({key, count: 50}))}]);
    store.setAggregate = (id: string, given: unknown) => {
      store.calls.push({name: 'setAggregate', args: [id, given]});
      const held = new Map(store.get('aggregates'));
      held.delete(id);
      store.set('aggregates', held);
      if (given === null) return;
      queueMicrotask(() => store.set('aggregates', new Map(store.get('aggregates')).set(id, answerFor(id, given as never))));
    };
    const asked = (prefix: string) => store.calls.filter((c) => c.name === 'setAggregate' && (c.args[0] as string).startsWith(prefix) && c.args[1] !== null);
    // The registrations the card made on mounting are answered as the store would.
    for (const [id, given] of registered(store)) store.set('aggregates', new Map(store.get('aggregates')).set(id, answerFor(id, given as never)));
    for (let i = 0; i < 4; i++) await settle(host);
    expect(asked('field-match').map((c) => c.args[1])).toEqual([{groupings: [{by: {field: 'archive', top: 100}}], without: 'archive'}]);
    expect(asked('field-outside').map((c) => c.args[1])).toEqual([{groupings: [{by: {field: 'archive', values: ['rare']}}], without: 'archive'}]);
    const rare = rows(host).find((r) => r.dataset.key === 'rare')!;
    expect(rare.querySelector('[part="count"]')!.textContent).toBe('7 / 50');
    expect(parseFloat((rare.querySelector('[part="bar-match"]') as HTMLElement).style.width)).toBe(1);
    // A pan whose view holds the same values outside asks nothing more for them.
    inView = [{key: 'cs', count: 2}, {key: 'rare', count: 5}];
    store.set('aggregates', new Map(store.get('aggregates')).set([...registered(store).keys()].find((id) => id.startsWith('field-subject'))!, answerFor('field-subject', {groupings: []})));
    for (let i = 0; i < 4; i++) await settle(host);
    expect(asked('field-outside')).toHaveLength(1);
    expect(asked('field-match')).toHaveLength(1);
  });

  it('puts a value in or out of the filter and the highlight from its row', async () => {
    const {host, store} = await mountCard('archive');
    await answer(host, store, [{rows: [{key: 'cs', count: 10}], total: 10}], [{rows: [{key: 'cs', count: 20}], total: 20}]);
    const changes: unknown[] = [];
    host.addEventListener('mosaica-filterchange', (e) => changes.push((e as CustomEvent).detail));
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
    host.addEventListener('mosaica-colourchange', (e) => colours.push((e as CustomEvent).detail));
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

  it('gives a value the colour chosen in its picker, drawn at once and reported as one change', async () => {
    const {host, store} = await mountCard('archive');
    const chosen: unknown[] = [];
    host.addEventListener('mosaica-valuecolour', (e) => chosen.push((e as CustomEvent).detail));
    store.set('legend', {...store.get('legend'), colourBy: 'archive', categories: {archive: [{key: 'cs', code: 1, title: null}]}, ranks: {archive: {1: 0}}});
    await answer(host, store, [{rows: [{key: 'cs', count: 10}], total: 10}], [{rows: [{key: 'cs', count: 10}], total: 10}]);
    (rows(host)[0]!.querySelector('button[part="swatch"]') as HTMLButtonElement).click();
    await settle(host);
    const choices = deepAll(host, '[part="choice"]') as HTMLButtonElement[];
    choices[2]!.click();
    await settle(host);
    const picked = /#[0-9a-f]{6}/.exec(choices[2]!.getAttribute('aria-label')!)![0];
    expect(rows(host)[0]!.querySelector('[part="swatch"]')!.getAttribute('style')).toContain(picked);
    (deep(host, '[part="reset"]') as HTMLButtonElement).click();
    await settle(host);
    expect(rows(host)[0]!.querySelector('[part="swatch"]')!.getAttribute('style')).not.toContain(picked);
    expect(chosen).toEqual([
      {column: 'archive', changes: [{value: 'cs', colour: picked}]},
      {column: 'archive', changes: [{value: 'cs', colour: null}]}
    ]);
  });

  it('shows a custom colour as it is dragged, and puts back the colour from before when the picker closes before the drag ends', async () => {
    const {host, store} = await mountCard('archive');
    const chosen: unknown[] = [];
    host.addEventListener('mosaica-valuecolour', (e) => chosen.push((e as CustomEvent).detail));
    store.set('legend', {...store.get('legend'), colourBy: 'archive', categories: {archive: [{key: 'cs', code: 1, title: null}]}, ranks: {archive: {1: 0}}});
    await answer(host, store, [{rows: [{key: 'cs', count: 10}], total: 10}], [{rows: [{key: 'cs', count: 10}], total: 10}]);
    const swatchStyle = () => rows(host)[0]!.querySelector('[part="swatch"]')!.getAttribute('style');
    const before = swatchStyle();
    (rows(host)[0]!.querySelector('button[part="swatch"]') as HTMLButtonElement).click();
    await settle(host);
    const sv = deep(host, '[part="sv"]') as HTMLElement;
    sv.getBoundingClientRect = () => ({left: 0, top: 0, right: 100, bottom: 100, width: 100, height: 100, x: 0, y: 0, toJSON: () => ({})});
    sv.dispatchEvent(new PointerEvent('pointerdown', {clientX: 90, clientY: 10, bubbles: true}));
    await new Promise((r) => setTimeout(r, 50));
    await settle(host);
    expect(swatchStyle()).not.toBe(before);
    sv.dispatchEvent(new KeyboardEvent('keydown', {key: 'Escape', bubbles: true, composed: true}));
    await settle(host);
    expect(deep(host, '[part="colour-popover"]')).toBeNull();
    expect(swatchStyle()).toBe(before);
    expect(chosen).toEqual([]);
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
    card.addEventListener('mosaica-fold', (e) => toggled.push((e as CustomEvent).detail));
    (deep(host, '[part="fold"]') as HTMLButtonElement).click();
    await settle(host);
    expect(toggled).toEqual([{field: 'archive', folded: false}]);
    expect(spec(store, 'field-match')).toBeDefined();
  });
});

describe('<mosaica-field-card> on a date', () => {
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

describe('<mosaica-field-card> on a layer', () => {
  const row = (id: bigint, name: string, parentIds: bigint[] = []) => ({tesseraId: id, key: null, name, maskedCount: 10n, matchedCount: null, rung: 1, parentIds, childCount: 0, slot: null});

  it('ranks the clusters the map draws by their counts in view, leaving out its own clauses, and names each as its table does, with its path from browse pages', async () => {
    const host = await mount('<mosaica-field-card field="cluster:topics"></mosaica-field-card>');
    const card = host.querySelector('mosaica-field-card') as MosaicaFieldCard;
    const store = fakeStore({meta: META, status: status({}), filters: filtersOf({filter: {}, highlight: {}})});
    store.set('view', {...store.get('view'), id: 's0'});
    store.setBrowse('p:7', {artifacts: [], parents: [row(1n, 'physics', [0n])], next: null});
    store.setBrowse('p:1', {artifacts: [], parents: [row(0n, 'science')], next: null});
    store.setBrowse('p:8', {artifacts: [], parents: [], next: null});
    card.store = store;
    await settle(host);
    // Each asks for the clusters' slots in the palette the map colours from, which the swatches read.
    const ranked = (top: number) => ({by: {layer: 'topics', top, cut: 'drawn', paletteSize: 'drawn'}});
    expect(spec(store, 'field-subject')).toEqual({groupings: [ranked(5)], subject: 'view', highlighted: true, withoutMembersOf: 'topics'});
    // The whole match counts how many clusters are drawn, which the search box names.
    expect(spec(store, 'field-match')).toEqual({groupings: [ranked(1)], withoutMembersOf: 'topics'});
    answerAggregate(store, 'field-subject', aggregateEntry([{rows: [{key: 7n, count: 9, title: 'optics'}, {key: 8n, count: 5, title: 'lasers'}], total: 14}]));
    await settle(host);
    await new Promise((r) => setTimeout(r, 0));
    await settle(host);
    // And then the clusters the subject lists, by name, so each row has both counts.
    expect(spec(store, 'field-match')).toEqual({groupings: [{by: {layer: 'topics', artifacts: [7n, 8n], paletteSize: 'drawn'}}, ranked(1)], withoutMembersOf: 'topics'});
    answerAggregate(store, 'field-match', aggregateEntry([{rows: [{key: 8n, count: 50}, {key: 7n, count: 90}], total: 140}, {rows: [{key: 7n, count: 90}], groups: 12}]));
    await settle(host);
    expect(deep(host, 'mosaica-cluster-filter')!.getAttribute('placeholder')).toBe('Search 12 clusters');
    // A share of a few per cent still reads as a bar; plain percentages otherwise.
    const optics = rows(host)[0]!;
    expect((optics.querySelector('[part="bar-match"]') as HTMLElement).getAttribute('style')).toContain('max(3px, 64.3%)');
    for (let i = 0; i < 3; i++) {
      await new Promise((r) => setTimeout(r, 0));
      await settle(host);
    }
    expect(rows(host).map((r) => [r.querySelector('[part="name"]')!.textContent, r.querySelector('[part="path"]')?.textContent ?? ''])).toEqual([
      ['optics', 'science › physics'],
      ['lasers', '']
    ]);
    // Each cluster's parents, and its first parent's, and nothing else.
    expect(store.calls.filter((c) => c.name === 'browse').map((c) => (c.args[0] as {parent?: bigint}).parent ?? 'roots')).toEqual([7n, 8n, 1n]);
    const clauses: unknown[] = [];
    host.addEventListener('mosaica-clausechange', (e) => clauses.push((e as CustomEvent).detail));
    (rows(host)[1]!.querySelector('[part="highlight"]') as HTMLButtonElement).click();
    expect(store.calls.filter((c) => c.name === 'setMembers').at(-1)!.args[0]).toEqual([{layer: 'topics', artifact: 8n, outside: false, verb: 'highlight', label: 'lasers'}]);
    expect(clauses).toEqual([{id: '8', layer: 'topics', outside: false, verb: 'highlight', on: true}]);
  });

  it('colours each cluster’s swatch from the slot its row carries, a chosen colour over it, in the store’s palette', async () => {
    const host = await mount('<mosaica-field-card field="cluster:topics"></mosaica-field-card>');
    const card = host.querySelector('mosaica-field-card') as MosaicaFieldCard;
    const store = fakeStore({meta: META, status: status({}), filters: filtersOf({filter: {}, highlight: {}})});
    store.set('view', {...store.get('view'), id: 's0'});
    store.set('legend', {...store.get('legend'), colourBy: 'cluster:topics'});
    card.store = store;
    await settle(host);
    const answerBoth = async (palette: 'tableau10' | 'okabe-ito') => {
      const none = undefined;
      answerAggregate(store, 'field-subject', aggregateEntry([{rows: [{key: 7n, count: 9, title: 'optics', slot: 2}, {key: 8n, count: 5, title: 'lasers', slot: null}], total: 14}], 's0', none, palette));
      answerAggregate(store, 'field-match', aggregateEntry([{rows: [{key: 7n, count: 9, slot: 2}, {key: 8n, count: 5, slot: null}]}, {rows: [{key: 7n, count: 9, slot: 2}], groups: 2}], 's0', none, palette));
      for (let i = 0; i < 3; i++) {
        await new Promise((r) => setTimeout(r, 0));
        await settle(host);
      }
    };
    await answerBoth('tableau10');
    const swatches = () => rows(host).map((r) => /--c:([^;]+)/.exec(r.querySelector('[part="swatch"]')!.getAttribute('style')!)![1]);
    // Slot 2 of Tableau 10 is #e15759; a cluster served with no slot is grey.
    expect(swatches()).toEqual(['rgb(225, 87, 89)', 'rgb(118, 126, 140)']);
    // A colour the host chose for a cluster is drawn in place of its slot's.
    store.set('artifacts', {...store.get('artifacts'), overrides: new Map([['topics', new Map([[8n, [1, 2, 3, 255] as const]])]])});
    await settle(host);
    expect(swatches()).toEqual(['rgb(225, 87, 89)', 'rgb(1, 2, 3)']);
    // Under Okabe-Ito the answers held keep Tableau 10's colours until answers for eight colours
    // land; the spec, which names the map's palette, does not change.
    const before = spec(store, 'field-subject');
    store.set('artifacts', {...store.get('artifacts'), palette: 'okabe-ito', overrides: new Map()});
    await settle(host);
    expect(spec(store, 'field-subject')).toEqual(before);
    expect(swatches()[0]).toBe('rgb(225, 87, 89)');
    // Slot 2 of Okabe-Ito is #009e73.
    await answerBoth('okabe-ito');
    expect(swatches()[0]).toBe('rgb(0, 158, 115)');
  });

  it('opens the colour picker from a cluster’s swatch, sets the colour chosen on the store over its slot’s, and reports it', async () => {
    const host = await mount('<mosaica-field-card field="cluster:topics"></mosaica-field-card>');
    const card = host.querySelector('mosaica-field-card') as MosaicaFieldCard;
    const store = fakeStore({meta: META, status: status({}), filters: filtersOf({filter: {}, highlight: {}})});
    store.set('view', {...store.get('view'), id: 's0'});
    store.set('legend', {...store.get('legend'), colourBy: 'cluster:topics'});
    // 9 has a colour on this layer; 7 has one on another layer, which is not this cluster's.
    store.set('artifacts', {...store.get('artifacts'), overrides: new Map([['topics', new Map([[9n, [1, 2, 3, 220] satisfies Rgba]])], ['other', new Map([[7n, [4, 5, 6, 220] satisfies Rgba]])]])});
    card.store = store;
    await settle(host);
    answerAggregate(store, 'field-subject', aggregateEntry([{rows: [{key: 7n, count: 9, title: 'optics', slot: 2}], total: 9}], 's0', undefined, 'tableau10'));
    await settle(host);
    const chosen: unknown[] = [];
    host.addEventListener('mosaica-clustercolour', (e) => chosen.push((e as CustomEvent).detail));
    const swatch = () => rows(host)[0]!.querySelector('[part="swatch"]') as HTMLButtonElement;
    expect(swatch().tagName).toBe('BUTTON');
    swatch().click();
    await settle(host);
    // The picker offers the layer's palette, with the cluster's own colour, slot 2 of Tableau 10, pressed.
    expect(deep(host, '[part="colour-popover"]')!.getAttribute('aria-label')).toBe('Colour of optics');
    expect(deep(host, '[part="choice"][aria-pressed="true"]')!.getAttribute('aria-label')).toContain('#e15759');
    (deep(host, '[part="hex"]') as HTMLInputElement).value = '0A0B0C';
    deep(host, '[part="hex"]')!.dispatchEvent(new Event('change'));
    await settle(host);
    // Set over the slot's colour, beside the colours chosen before.
    const ids = (layer: string) => [...(store.get('artifacts').overrides.get(layer)?.keys() ?? [])].sort();
    expect(ids('topics')).toEqual([7n, 9n]);
    expect(ids('other')).toEqual([7n]);
    expect(swatch().getAttribute('style')).toContain('rgb(10, 11, 12)');
    (deep(host, '[part="reset"]') as HTMLButtonElement).click();
    await settle(host);
    expect(ids('topics')).toEqual([9n]);
    expect(ids('other')).toEqual([7n]);
    expect(swatch().getAttribute('style')).toContain('rgb(225, 87, 89)');
    expect(chosen).toEqual([
      {layer: 'topics', changes: [{tesseraId: '7', colour: '#0a0b0c'}]},
      {layer: 'topics', changes: [{tesseraId: '7', colour: null}]}
    ]);
  });

  it('ranks a flat layer’s clusters with no cut', async () => {
    const flat = {...topicsLayer, name: 'groups', hierarchy: {kind: 'flat', pruneChildren: false}} as Meta['layers'][number];
    const host = await mount('<mosaica-field-card field="cluster:groups"></mosaica-field-card>');
    const card = host.querySelector('mosaica-field-card') as MosaicaFieldCard;
    const store = fakeStore({meta: {...META, layers: [flat]}, status: status({}), filters: filtersOf({filter: {}, highlight: {}})});
    store.set('view', {...store.get('view'), id: 's0'});
    card.store = store;
    await settle(host);
    expect(spec(store, 'field-subject')).toMatchObject({groupings: [{by: {layer: 'groups', top: 5}}]});
  });
});

describe('<mosaica-field-card> across a change of viewer', () => {
  it('drops a browse page still loading for the viewer before, so nothing it met names a cluster to the next', async () => {
    const row = (id: bigint, name: string, parentIds: bigint[] = []) => ({tesseraId: id, key: null, name, maskedCount: 10n, matchedCount: null, rung: 0, parentIds, childCount: 0, slot: null});
    const host = await mount('<mosaica-field-card field="cluster:topics"></mosaica-field-card>');
    const card = host.querySelector('mosaica-field-card') as MosaicaFieldCard;
    const before = fakeStore({meta: META, status: status({}), filters: filtersOf({filter: {}, highlight: {}})});
    before.set('view', {...before.get('view'), id: 's0'});
    // The page of the cluster's parents waits until the viewer has changed.
    let release: () => void = () => {};
    const held = new Promise<void>((r) => (release = r));
    const browse = before.browse.bind(before);
    before.browse = async (req) => {
      if (req.parent === 3n) {
        await held;
        return {artifacts: [], parents: [row(1n, 'seen only by the first viewer')], next: null};
      }
      return browse(req);
    };
    card.store = before;
    await settle(host);
    answerAggregate(before, 'field-subject', aggregateEntry([{rows: [{key: 3n, count: 4}], total: 4}]));
    await settle(host);

    const after = fakeStore({meta: META, status: status({}), filters: filtersOf({filter: {}, highlight: {}})});
    after.set('view', {...after.get('view'), id: 's0'});
    after.setBrowse('p:3', {artifacts: [], parents: [], next: null});
    card.store = after;
    await settle(host);
    release();
    answerAggregate(after, 'field-subject', aggregateEntry([{rows: [{key: 3n, count: 4, title: 'theirs'}], total: 4}]));
    for (let i = 0; i < 3; i++) {
      await new Promise((r) => setTimeout(r, 0));
      await settle(host);
    }
    expect(rows(host).map((r) => [r.querySelector('[part="name"]')!.textContent, r.querySelector('[part="path"]')?.textContent ?? ''])).toEqual([['theirs', '']]);
    expect(deepAll(host, '[part="name"], [part="path"]').map((e) => e.textContent).join(' ')).not.toContain('first viewer');
  });
});

describe('<mosaica-field-card> on text', () => {
  it('is the field’s search box, and counts nothing', async () => {
    const {host, store} = await mountCard('title');
    expect(deep(host, 'mosaica-filter')).not.toBeNull();
    expect(deep(host, '[part="paint"]')).toBeNull();
    expect(registered(store).size).toBe(0);
  });
});
