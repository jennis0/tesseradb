import {afterEach, describe, expect, it, vi} from 'vitest';
import {activeCount, type FilterDraft, type FiltersProjection, type Meta} from '@tesseradb/client';
import '../src/item-card.js';
import '../src/filter.js';
import '../src/filter-panel.js';
import type {TesseraItemCard} from '../src/item-card.js';
import type {TesseraFilter} from '../src/filter.js';
import type {TesseraFilterPanel} from '../src/filter-panel.js';
import {deep, deepAll, fakeStore, mount, settle, status, meta, scalar} from './fake-store.js';
import {UNNAMED, dateRangeText, parseDateText} from '../src/base.js';

afterEach(() => {
  document.body.innerHTML = '';
  vi.useRealTimers();
});

const META = meta({
  declaredScalars: [
    scalar('archive', 'u16', {category: {vocabulary: 'a', kind: 'declared', visibility: 'public'}, render: true, homes: ['rendered']}),
    scalar('submitted_at', 'timestamp_us', {render: true, homes: ['rendered']}),
    scalar('title', 'utf8')
  ],
  filterOperands: [
    {column: 'archive', family: 'category', operands: ['eq', 'in']},
    {column: 'title', family: 'text', operands: ['match', 'phrase']},
    {column: 'submitted_at', family: 'numeric', operands: ['range']}
  ]
});

describe('<tessera-item-card>', () => {
  it('renders fields by name in declaration order, presented by type, with a slot per field', async () => {
    const host = await mount('<tessera-item-card title-field="title"><a slot="field-title" href="#">my link</a></tessera-item-card>');
    const el = host.querySelector('tessera-item-card') as TesseraItemCard;
    el.meta = META;
    // `archive` absent from the record, the extra `note` undeclared: order is declared-then-extra,
    // and the gap is named rather than shifting the fields after it.
    el.item = {id: 12345678901234567890n, detail: {fields: {note: 'x', title: 'A title', submitted_at: 1_700_000_000_000_000}, labels: [], views: [], scoped: {}}};
    await settle(host);
    // The headline is the field the host named; the rest in declared-then-extra order, then the
    // id; the absent `archive` is not rendered.
    expect(deep(host, '[part="headline"]')?.getAttribute('data-name')).toBe('title');
    expect(deepAll(host, '[part="field"]').map((f) => f.getAttribute('data-name'))).toEqual(['submitted_at', 'note', 'tessera_id']);
    expect(deep(host, '[part="field"][data-name="tessera_id"] [part="value"]')?.textContent).toBe('12345678901234567890');
    expect(deep(host, '[part="field"][data-name="submitted_at"] [part="value"]')?.textContent).toBe('14 November 2023, 22:13:20 UTC');
    expect(deep(host, 'slot[name="field-title"]')).not.toBeNull();
    expect(host.textContent).toContain('my link');
    expect(deep(host, '[part="field"][data-name="archive"]')).toBeNull();
  });

  it('with no title field, heads the card with the id, once, and shows every other field in the grid', async () => {
    const host = await mount('<tessera-item-card></tessera-item-card>');
    const el = host.querySelector('tessera-item-card') as TesseraItemCard;
    el.meta = META;
    el.item = {id: 42n, detail: {fields: {note: 'x', title: 'A title'}, labels: [], views: [], scoped: {}}};
    await settle(host);
    expect(deep(host, '[part="headline"]')?.textContent).toBe('42');
    expect(deepAll(host, '[part="field"]').map((f) => f.getAttribute('data-name'))).toEqual(['title', 'note']);
  });

  it('fires tessera-open with the id as a decimal string, bubbling and composed', async () => {
    const host = await mount('<tessera-item-card></tessera-item-card>');
    const el = host.querySelector('tessera-item-card') as TesseraItemCard;
    el.item = {id: 2n ** 63n + 1n, detail: {fields: {}, labels: [], views: [], scoped: {}}};
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

    el.pick = {kind: 'broken', index: 7, layer: 'tessera-marks-p1', hasIds: false, idCount: 0};
    await settle(host);
    expect(deep(host, '[part="state"]')?.getAttribute('data-state')).toBe('refused');
    expect(deep(host, '[part="refusal"]')).not.toBeNull();

    el.pick = null;
    el.refusal = {code: 'not-found', detail: 'nope'};
    await settle(host);
    expect(deep(host, '[part="state"]')?.getAttribute('data-state')).toBe('refused');
    expect(deep(host, '[part="refusal"]')).not.toBeNull();
  });

  it('reads the store’s selection when nothing is given by property', async () => {
    const host = await mount('<tessera-item-card></tessera-item-card>');
    const el = host.querySelector('tessera-item-card') as TesseraItemCard;
    const store = fakeStore({meta: META, status: status({})});
    el.store = store;
    await settle(host);
    store.set('selection', {item: {id: 5n, detail: {fields: {archive: 'cs'}, labels: [], views: [], scoped: {}}}, itemRefusal: null, artifact: null, artifactRefusal: null});
    await settle(host);
    expect(deep(host, '[part="field"][data-name="archive"] [part="value"]')?.textContent).toBe('cs');
  });
});

/** A filters projection over `draft`, with whatever else `over` names. */
const filtersOf = (draft: FilterDraft, over: Partial<FiltersProjection> = {}): FiltersProjection => ({
  draft,
  expr: null,
  highlight: null,
  members: [],
  suggestions: {},
  suggestErrors: {},
  suggestEpoch: 0,
  ...over
});

const value = (code: number, key: string, title: string | null, count?: number) => ({code, key, title, match: {field: 'key' as const, start: 0, len: 0}, ...(count === undefined ? {} : {count})});

async function mountFilter(column: string, filters: FiltersProjection, attrs = '') {
  const host = await mount(`<tessera-filter column="${column}" ${attrs}></tessera-filter>`);
  const el = host.querySelector('tessera-filter') as TesseraFilter;
  const store = fakeStore({meta: META, status: status({}), filters});
  el.store = store;
  await settle(host);
  return {host, el, store};
}

/** Type into the control's box, as a viewer does. */
async function type(host: HTMLElement, text: string) {
  const entry = deep(host, '[part="entry"]') as HTMLInputElement;
  entry.value = text;
  entry.dispatchEvent(new Event('input'));
  await settle(host);
  return entry;
}

const drafts = (store: ReturnType<typeof fakeStore>) => store.calls.filter((c) => c.name === 'setFilters').map((c) => c.args[0] as FilterDraft);

describe('<tessera-filter> on a category', () => {
  const empty = () => filtersOf({filter: {archive: {family: 'category', keys: []}}, highlight: {}});

  it('asks for nothing and lists nothing until something is typed', async () => {
    const {host, store} = await mountFilter('archive', empty());
    expect(store.calls.filter((c) => c.name === 'suggest')).toHaveLength(0);
    expect(deep(host, '[part="values"]')).toBeNull();
    await type(host, 'ma');
    expect(store.calls.filter((c) => c.name === 'suggest').map((c) => c.args)).toEqual([['archive', 'ma']]);
  });

  it('submits a typed key never listed, and never renders "no such value"', async () => {
    const {host, store} = await mountFilter('archive', empty());
    const entry = await type(host, 'zz.unlisted');
    entry.dispatchEvent(new KeyboardEvent('keydown', {key: 'Enter'}));
    await settle(host);
    expect((drafts(store)[0]!.filter['archive'] as {keys: string[]}).keys).toEqual(['zz.unlisted']);
    expect(deep(host, '[part="refusal"]')).toBeNull();
  });

  it('renders only the page that answers the box in front of it, and chooses a suggestion', async () => {
    const {host, store} = await mountFilter('archive', empty());
    await type(host, 'mach');
    expect(store.calls.filter((c) => c.name === 'suggest').at(-1)!.args).toEqual(['archive', 'mach']);
    // A page for a different `q` (an earlier keystroke, landed late) is not shown for the box.
    store.set('filters', {...store.get('filters'), suggestions: {archive: {q: 'mac', values: [value(9, 'stat.ML', 'Machine Learning (Statistics)')], more: false}}});
    await settle(host);
    expect(deepAll(host, '[part~="tick"]')).toHaveLength(0);
    store.set('filters', {
      ...store.get('filters'),
      suggestions: {archive: {q: 'mach', values: [{code: 41207, key: 'cs.LG', title: 'Machine Learning', match: {field: 'title', start: 0, len: 4}}], more: true}}
    });
    await settle(host);
    const ticks = deepAll(host, '[part~="tick"]');
    expect(ticks).toHaveLength(1);
    expect(deep(host, '[part~="tick"] mark')?.textContent).toBe('Mach');
    expect(deep(host, '[part="more"]')).not.toBeNull();
    (ticks[0] as HTMLElement).click();
    expect((drafts(store)[0]!.filter['archive'] as {keys: string[]}).keys).toEqual(['cs.LG']);
  });

  it('shows each value’s count and its share of what the map matches now', async () => {
    const {host, store} = await mountFilter('archive', empty());
    store.set('view', {...store.get('view'), matched: {value: 1000, exact: true}});
    await type(host, 'c');
    store.set('filters', {...store.get('filters'), suggestions: {archive: {q: 'c', values: [value(1, 'cs', 'CS', 250), value(2, 'cond', null, 0)], more: false}}});
    await settle(host);
    expect(deepAll(host, '[part="value-count"]').map((c) => c.textContent)).toEqual(['250', '0']);
    // The bar is the value's share of the matched total, not of the commonest value.
    expect(deepAll(host, '[part="bar"]').map((b) => (b as HTMLElement).style.width)).toEqual(['25.0%', '0.0%']);
  });

  it('in the highlight, greys a value the filter leaves out, counts it 0 and does not choose it', async () => {
    const {host, store} = await mountFilter('archive', filtersOf({filter: {archive: {family: 'category', keys: ['cs']}}, highlight: {}}), 'verb="highlight"');
    await type(host, 'c');
    store.set('filters', {...store.get('filters'), suggestions: {archive: {q: 'c', values: [value(1, 'cs', 'CS', 250), value(2, 'cond', null, 90)], more: false}}});
    await settle(host);
    const ticks = deepAll(host, '[part~="tick"]') as HTMLElement[];
    expect(ticks.map((t) => t.getAttribute('aria-disabled'))).toEqual(['false', 'true']);
    expect(deepAll(host, '[part="value-count"]').map((c) => c.textContent)).toEqual(['250', '0']);
    ticks[1]!.click();
    expect(drafts(store)).toHaveLength(0);
    ticks[0]!.click();
    expect(drafts(store)[0]!.highlight['archive']).toEqual({family: 'category', keys: ['cs']});
    expect(drafts(store)[0]!.filter['archive']).toEqual({family: 'category', keys: ['cs']});
  });

  it('shows the values chosen as chips under the box, each removable', async () => {
    const {host, store} = await mountFilter('archive', filtersOf({filter: {archive: {family: 'category', keys: ['cs', 'math']}}, highlight: {}}));
    const chips = deepAll(host, '[part="chosen"]');
    expect(chips).toHaveLength(2);
    (chips[0]!.querySelector('button') as HTMLButtonElement).click();
    expect(drafts(store)[0]!.filter['archive']).toEqual({family: 'category', keys: ['math']});
  });

  it('shows a refusal beside the box, with its code, and keeps the box', async () => {
    const {host} = await mountFilter('archive', filtersOf({filter: {}, highlight: {}}, {suggestErrors: {archive: {code: 'vocabulary-withheld', detail: ''}}}));
    await type(host, 'c');
    expect(deep(host, '[part="entry"]')).not.toBeNull();
    expect(deep(host, '[part="refusal"]')?.getAttribute('data-code')).toBe('vocabulary-withheld');
  });

  it('asks again for what is typed once the store’s suggestEpoch moves', async () => {
    const {host, store} = await mountFilter('archive', empty());
    await type(host, 'ma');
    store.set('filters', {...store.get('filters'), suggestions: {}, suggestErrors: {}, suggestEpoch: 1});
    await settle(host);
    expect(store.calls.filter((c) => c.name === 'suggest').map((c) => c.args)).toEqual([
      ['archive', 'ma'],
      ['archive', 'ma']
    ]);
  });
});

describe('<tessera-filter> on text', () => {
  it('sends one box’s words, phrases and alternatives after the typing pause', async () => {
    vi.useFakeTimers();
    const {host, store} = await mountFilter('title', filtersOf({filter: {title: {family: 'text', query: '', phrase: true}}, highlight: {}}));
    expect(deep(host, '[part="hint"]')).not.toBeNull();
    const changes: unknown[] = [];
    host.addEventListener('tessera-filterchange', (e) => changes.push((e as CustomEvent).detail));
    await type(host, '"graph neural" OR lattice');
    expect(drafts(store)).toHaveLength(0);
    await vi.runAllTimersAsync();
    expect(drafts(store)[0]!.filter['title']).toEqual({family: 'text', query: '"graph neural" OR lattice', phrase: true});
    expect(changes).toEqual([{column: 'title', verb: 'filter', expr: {any_of: [{title: {phrase: 'graph neural'}}, {title: {match: 'lattice'}}]}}]);
  });

  it('shows a clause set from outside that no query writes read-only, and Clear empties it', async () => {
    const expr = {title: {match: 'salt OR pepper'}};
    const {host, store} = await mountFilter('title', filtersOf({filter: {title: {family: 'text', query: '', phrase: true, expr}}, highlight: {}}));
    expect((deep(host, '[part="entry"]') as HTMLInputElement).readOnly).toBe(true);
    (deep(host, '[part="aside"]') as HTMLButtonElement).click();
    expect(drafts(store)[0]!.filter['title']).toEqual({family: 'text', query: '', phrase: true});
  });

  it('sends typing still waiting when its position changes, to the position it was typed in', async () => {
    vi.useFakeTimers();
    const filter = {title: {family: 'text' as const, query: 'graph', phrase: true}};
    const {host, el, store} = await mountFilter('title', filtersOf({filter, highlight: {}}));
    el.verb = 'highlight';
    await settle(host);
    const entry = await type(host, 'guidance');
    expect(entry.value).toBe('guidance');
    el.verb = 'filter';
    await settle(host);
    expect(drafts(store)).toEqual([{filter, highlight: {title: {family: 'text', query: 'guidance', phrase: true}}}]);
    expect((deep(host, '[part="entry"]') as HTMLInputElement).value).toBe('graph');
    await vi.runAllTimersAsync();
    expect(drafts(store)).toHaveLength(1);
  });
});

describe('<tessera-filter> on a date', () => {
  const empty = () => filtersOf({filter: {submitted_at: {family: 'numeric', gte: null, lte: null}}, highlight: {}});
  const commit = (input: HTMLInputElement, text: string) => {
    input.value = text;
    input.dispatchEvent(new Event('change'));
  };

  it('reads a typed day as its first instant below and a typed year as its last above', async () => {
    const {host, store} = await mountFilter('submitted_at', empty(), 'verb="highlight"');
    commit(deepAll(host, '[part="entry"]')[0] as HTMLInputElement, '1 Jan 2019');
    expect(drafts(store).at(-1)!.highlight['submitted_at']).toEqual({family: 'numeric', gte: Date.UTC(2019, 0, 1) * 1000, lte: null});
    await settle(host);
    commit(deepAll(host, '[part="entry"]')[1] as HTMLInputElement, '2024');
    expect(drafts(store).at(-1)!.highlight['submitted_at']).toEqual({family: 'numeric', gte: Date.UTC(2019, 0, 1) * 1000, lte: Date.UTC(2025, 0, 1) * 1000 - 1});
    // The filter is left alone.
    expect(drafts(store).at(-1)!.filter).toEqual(empty().draft.filter);
  });

  it('shows the bounds back as dates, and sends nothing for text that is not one', async () => {
    const {host, store} = await mountFilter('submitted_at', filtersOf({filter: {submitted_at: {family: 'numeric', gte: Date.UTC(2019, 0, 1) * 1000, lte: Date.UTC(2025, 0, 1) * 1000 - 1}}, highlight: {}}));
    const [from, to] = deepAll(host, '[part="entry"]') as HTMLInputElement[];
    expect([from!.value, to!.value]).toEqual(['1 Jan 2019', '31 Dec 2024']);
    commit(from!, 'the spring');
    await settle(host);
    expect(drafts(store)).toHaveLength(0);
    expect((deepAll(host, '[part="entry"]')[0] as HTMLInputElement).getAttribute('aria-invalid')).toBe('true');
  });
});

describe('<tessera-filter> on a keyword column', () => {
  const meta: Meta = {...META, filterOperands: [{column: 'author', family: 'keyword', operands: ['eq', 'prefix']}]};

  it('offers only the operators the column publishes, and sends the draft’s operator', async () => {
    vi.useFakeTimers();
    const host = await mount('<tessera-filter column="author"></tessera-filter>');
    const el = host.querySelector('tessera-filter') as TesseraFilter;
    const store = fakeStore({meta, status: status({})});
    store.set('filters', filtersOf({filter: {author: {family: 'keyword', needle: '', op: 'eq'}}, highlight: {}}));
    el.store = store;
    await settle(host);
    const select = deep(host, 'select[part="mode"]') as HTMLSelectElement;
    expect([...select.options].map((o) => o.value)).toEqual(['eq', 'prefix']);
    expect(select.value).toBe('eq');
    await type(host, 'Knuth');
    await vi.runAllTimersAsync();
    const sent = store.calls.find((c) => c.name === 'setFilters')!.args[0] as {filter: {author: {op: string; needle: string}}};
    expect(sent.filter.author).toMatchObject({op: 'eq', needle: 'Knuth'});
  });
});

describe('<tessera-filter-panel>', () => {
  async function mountPanel(draft: FilterDraft, attrs = '', over: Partial<FiltersProjection> = {}) {
    const host = await mount(`<tessera-filter-panel ${attrs}></tessera-filter-panel>`);
    const panel = host.querySelector('tessera-filter-panel') as TesseraFilterPanel;
    const store = fakeStore({meta: META, status: status({}), filters: filtersOf(draft, over)});
    panel.store = store;
    await settle(host);
    const sent = () => store.calls.filter((c) => c.name === 'setFilters').at(-1)!.args[0] as FilterDraft;
    const controls = () => deepAll(host, 'tessera-filter') as TesseraFilter[];
    const fields = () => deepAll(host, '[part~="field"]').map((f) => [f.getAttribute('data-column'), f.hasAttribute('data-open')]);
    return {host, panel, store, sent, controls, fields};
  }
  const none = (): FilterDraft => ({filter: {archive: {family: 'category', keys: []}, title: {family: 'text', query: '', phrase: true}, submitted_at: {family: 'numeric', gte: null, lte: null}}, highlight: {}});

  it('lists no field until one is pinned, holds a clause or is added', async () => {
    const {host, fields} = await mountPanel(none());
    expect(fields()).toEqual([]);
    (deep(host, '[part="add"]') as HTMLButtonElement).click();
    await settle(host);
    expect(deepAll(host, '[part~="add-option"]').map((o) => o.getAttribute('data-column'))).toEqual(['archive', 'title', 'submitted_at']);
    const search = deep(host, '[part="add-search"]') as HTMLInputElement;
    search.value = 'sub';
    search.dispatchEvent(new Event('input'));
    await settle(host);
    (deep(host, '[part~="add-option"]') as HTMLButtonElement).click();
    await settle(host);
    expect(fields()).toEqual([['submitted_at', true]]);
    expect(deep(host, '[part="add-list"]')).toBeNull();
  });

  it('lists the pinned fields closed, as Any, and opens one when pressed', async () => {
    const {host, fields} = await mountPanel(none(), 'pinned="title archive"');
    // In meta's order, whatever order they were pinned in.
    expect(fields()).toEqual([
      ['archive', false],
      ['title', false]
    ]);
    (deep(host, '[part~="field"][data-column="title"] [part="any"]') as HTMLButtonElement).click();
    await settle(host);
    expect(fields()).toEqual([
      ['archive', false],
      ['title', true]
    ]);
  });

  it('edits the position the switch names in every control, and opens the fields holding a clause there', async () => {
    const draft: FilterDraft = {filter: {...none().filter, archive: {family: 'category', keys: ['cs']}}, highlight: {title: {family: 'text', query: 'graph', phrase: true}}};
    const {host, panel, controls, fields} = await mountPanel(draft);
    expect(fields()).toEqual([
      ['archive', true],
      ['title', false]
    ]);
    expect(controls().map((c) => c.verb)).toEqual(['filter']);
    (deep(host, '[part="mode"] [data-verb="highlight"]') as HTMLButtonElement).click();
    await settle(host);
    expect(panel.mode).toBe('highlight');
    expect(fields()).toEqual([
      ['archive', false],
      ['title', true]
    ]);
    expect(controls().map((c) => [c.column, c.verb])).toEqual([['title', 'highlight']]);
  });

  describe('a column both filtered and highlighted', () => {
    const both: FilterDraft = {
      filter: {archive: {family: 'category', keys: ['cs.LG', 'cs.CV']}, title: {family: 'text', query: '', phrase: true}},
      highlight: {archive: {family: 'category', keys: ['cs.CV', 'stat.ML']}}
    };
    const chip = (host: HTMLElement, verb: string) => deep(host, `[part="chip"][data-column="archive"][data-verb="${verb}"]`)!;

    it('shows two chips, and removing one leaves the other', async () => {
      const {host, sent} = await mountPanel(both);
      expect(deepAll(host, '[part="chip"]').map((c) => c.getAttribute('data-verb'))).toEqual(['filter', 'highlight']);
      (chip(host, 'highlight').querySelector(':scope > button:last-child') as HTMLButtonElement).click();
      expect(sent()).toEqual({filter: both.filter, highlight: {archive: {family: 'category', keys: []}}});
      (chip(host, 'filter').querySelector(':scope > button:last-child') as HTMLButtonElement).click();
      expect(sent()).toEqual({filter: {...both.filter, archive: {family: 'category', keys: []}}, highlight: both.highlight});
    });

    it('opens a chip’s control in the chip’s position and focuses it', async () => {
      const {host, panel, controls} = await mountPanel(both);
      (chip(host, 'highlight').querySelector('[part="edit"]') as HTMLButtonElement).click();
      await settle(host);
      await controls()[0]!.updateComplete;
      expect(panel.mode).toBe('highlight');
      expect(controls()[0]!.verb).toBe('highlight');
      expect(panel.shadowRoot!.activeElement).toBe(controls()[0]);
    });

    it('asks for the controls under chips-only, and show() opens the chip’s control', async () => {
      const {host, panel} = await mountPanel(both, 'chips-only');
      expect(deepAll(host, 'tessera-filter')).toHaveLength(0);
      const asked: unknown[] = [];
      host.addEventListener('tessera-chipopen', (e) => asked.push((e as CustomEvent).detail));
      (chip(host, 'highlight').querySelector('[part="edit"]') as HTMLButtonElement).click();
      expect(asked).toEqual([{column: 'archive', verb: 'highlight'}]);
      panel.chipsOnly = false;
      panel.show('archive', 'highlight');
      await settle(host);
      const control = deepAll(host, 'tessera-filter').find((f) => f.getAttribute('column') === 'archive') as TesseraFilter;
      expect(control.verb).toBe('highlight');
    });

    it('empties both positions on Clear all', async () => {
      const {host, sent} = await mountPanel(both);
      (deep(host, '[part="clear"]') as HTMLButtonElement).click();
      expect(activeCount(sent())).toBe(0);
      expect(sent().highlight).toEqual({});
    });
  });

  it('leaves the chips out under controls-only, and the controls out under chips-only', async () => {
    const draft: FilterDraft = {filter: {...none().filter, archive: {family: 'category', keys: ['cs']}}, highlight: {}};
    const controls = await mountPanel(draft, 'controls-only');
    expect(deepAll(controls.host, '[part="chip"]')).toHaveLength(0);
    expect(deepAll(controls.host, 'tessera-filter')).toHaveLength(1);
    document.body.innerHTML = '';
    const chips = await mountPanel(draft, 'chips-only');
    expect(deepAll(chips.host, '[part="chip"]')).toHaveLength(1);
    expect(deepAll(chips.host, 'tessera-filter')).toHaveLength(0);
    expect(deep(chips.host, '[part="mode"]')).toBeNull();
  });

  it('does not rebuild a control under the user on a store tick', async () => {
    const {store, controls, host} = await mountPanel({...none(), filter: {...none().filter, archive: {family: 'category', keys: ['cs']}}});
    const before = controls()[0];
    store.set('status', status({status: 'loading'}));
    await settle(host);
    expect(controls()[0]).toBe(before);
  });

  it('marks a member_of chip with no label and no served name as unnamed, never by its key', async () => {
    const {host} = await mountPanel({filter: {}, highlight: {}}, '', {members: [{layer: 'clusters', artifact: 4n, outside: false, verb: 'filter'}]});
    const chip = deep(host, '[part="chip"][data-artifact="4"]')!;
    expect(chip.textContent).toContain(UNNAMED);
    expect(chip.textContent).not.toContain('clusters');
  });

  it('draws an artifact filtered and highlighted as two chips, and removes one of them', async () => {
    const clause = {layer: 'mesh/descriptors', artifact: 546_790n, outside: false};
    const {host, store} = await mountPanel({filter: {}, highlight: {}}, '', {
      members: [
        {...clause, verb: 'filter'},
        {...clause, verb: 'highlight'}
      ]
    });
    const chips = deepAll(host, '[part="chip"][data-artifact="546790"]');
    expect(chips.map((c) => c.getAttribute('data-verb'))).toEqual(['filter', 'highlight']);
    expect(chips[1]!.querySelector('[part="verb"]')).not.toBeNull();
    (chips[0]!.querySelector('button') as HTMLButtonElement).click();
    expect(store.calls.filter((c) => c.name === 'setMembers').at(-1)!.args[0]).toEqual([{...clause, verb: 'highlight'}]);
  });
});

describe('<tessera-filter-panel chips-only>', () => {
  it('renders nothing while no clause is applied, and the chips once one is', async () => {
    const host = await mount('<tessera-filter-panel chips-only></tessera-filter-panel>');
    const store = fakeStore({meta: META, status: status({})});
    (host.querySelector('tessera-filter-panel') as unknown as {store: unknown}).store = store;
    await settle(host);
    expect(deepAll(host, '[part]')).toHaveLength(0);
    store.set('filters', filtersOf({filter: {archive: {family: 'category', keys: ['cs']}}, highlight: {}}));
    await settle(host);
    expect(deepAll(host, '[part="chip"]')).toHaveLength(1);
  });
});

describe('a date range as a chip says it', () => {
  const at = (y: number, m: number, d: number) => Date.UTC(y, m, d) * 1000;
  const end = (y: number, m: number, d: number) => Date.UTC(y, m, d + 1) * 1000 - 1;

  it('writes whole years, days within a year, days across years and open ends', () => {
    expect(dateRangeText(at(2019, 0, 1), end(2024, 11, 31))).toBe('2019 – 2024');
    expect(dateRangeText(at(2024, 0, 1), end(2024, 11, 31))).toBe('2024');
    expect(dateRangeText(at(2024, 2, 3), end(2024, 5, 14))).toBe('3 Mar – 14 Jun 2024');
    expect(dateRangeText(at(2019, 2, 3), end(2024, 5, 14))).toBe('3 Mar 2019 – 14 Jun 2024');
    expect(dateRangeText(at(2019, 2, 3), null)).toBe('from 3 Mar 2019');
    expect(dateRangeText(null, end(2024, 5, 14))).toBe('until 14 Jun 2024');
  });

  it('reads back what it writes', () => {
    expect(parseDateText('3 Mar 2019', false)).toBe(at(2019, 2, 3));
    expect(parseDateText('14 June 2024', true)).toBe(end(2024, 5, 14));
    expect(parseDateText('2024-06-14', false)).toBe(at(2024, 5, 14));
    expect(parseDateText('Jun 2024', true)).toBe(end(2024, 5, 30));
    expect(parseDateText('31 Feb 2024', false)).toBeNull();
    expect(parseDateText('soon', false)).toBeNull();
    // The word typed is the start of a month's name, not a word that starts with one.
    expect(parseDateText('Junk 2019', false)).toBeNull();
    expect(parseDateText('Sept 2019', false)).toBe(at(2019, 8, 1));
    // A year below 100 is that year, not one in the 1900s.
    expect(dateRangeText(parseDateText('0050', false), null)).toBe('from 1 Jan 50');
  });

  it('keeps the day of an instant before 1970', () => {
    expect(dateRangeText(null, end(1969, 5, 14))).toBe('until 14 Jun 1969');
    expect(dateRangeText(at(1950, 0, 1), end(1959, 11, 31))).toBe('1950 – 1959');
  });

  it('calls a range whole years only from the first instant of one to the last instant of another', () => {
    expect(dateRangeText(at(2019, 0, 1), at(2024, 11, 31))).toBe('1 Jan 2019 – 31 Dec 2024');
  });
});
