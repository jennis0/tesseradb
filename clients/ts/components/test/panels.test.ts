import {afterEach, describe, expect, it, vi} from 'vitest';
import {activeCount, type FilterDraft, type FiltersProjection, type Meta} from '@mosaica/client';
import '../src/item-card.js';
import '../src/filter.js';
import '../src/filter-panel.js';
import type {MosaicaItemCard} from '../src/item-card.js';
import type {MosaicaFilter} from '../src/filter.js';
import type {MosaicaFilterPanel} from '../src/filter-panel.js';
import type {MosaicaFieldCard} from '../src/field-card.js';
import {aggregateEntry, answerAggregate, deep, deepAll, fakeStore, mount, registered, settle, status, meta, scalar} from './fake-store.js';
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

describe('<mosaica-item-card>', () => {
  it('renders fields by name in declaration order, presented by type, with a slot per field', async () => {
    const host = await mount('<mosaica-item-card title-field="title"><a slot="field-title" href="#">my link</a></mosaica-item-card>');
    const el = host.querySelector('mosaica-item-card') as MosaicaItemCard;
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
    const host = await mount('<mosaica-item-card></mosaica-item-card>');
    const el = host.querySelector('mosaica-item-card') as MosaicaItemCard;
    el.meta = META;
    el.item = {id: 42n, detail: {fields: {note: 'x', title: 'A title'}, labels: [], views: [], scoped: {}}};
    await settle(host);
    expect(deep(host, '[part="headline"]')?.textContent).toBe('42');
    expect(deepAll(host, '[part="field"]').map((f) => f.getAttribute('data-name'))).toEqual(['title', 'note']);
  });

  it('fires mosaica-open with the id as a decimal string, bubbling and composed', async () => {
    const host = await mount('<mosaica-item-card></mosaica-item-card>');
    const el = host.querySelector('mosaica-item-card') as MosaicaItemCard;
    el.item = {id: 2n ** 63n + 1n, detail: {fields: {}, labels: [], views: [], scoped: {}}};
    await settle(host);
    let detail: {id?: string} | null = null;
    document.body.addEventListener('mosaica-open', (e) => (detail = (e as CustomEvent).detail));
    (deep(host, '[part="open"]') as HTMLButtonElement).click();
    expect(detail!.id).toBe('9223372036854775809');
    expect(typeof detail!.id).toBe('string');
  });

  it('distinguishes a miss from a broken pick from a refusal', async () => {
    const host = await mount('<mosaica-item-card></mosaica-item-card>');
    const el = host.querySelector('mosaica-item-card') as MosaicaItemCard;
    el.pick = {kind: 'miss'};
    await settle(host);
    expect(deep(host, '[part="state"]')?.getAttribute('data-state')).toBe('empty');

    el.pick = {kind: 'broken', index: 7, layer: 'mosaica-marks-p1', hasIds: false, idCount: 0};
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
    const host = await mount('<mosaica-item-card></mosaica-item-card>');
    const el = host.querySelector('mosaica-item-card') as MosaicaItemCard;
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
  const host = await mount(`<mosaica-filter column="${column}" ${attrs}></mosaica-filter>`);
  const el = host.querySelector('mosaica-filter') as MosaicaFilter;
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

describe('<mosaica-filter> on a category', () => {
  const empty = () => filtersOf({filter: {archive: {family: 'category', keys: []}}, highlight: {}});

  it('asks for nothing and lists nothing until something is typed', async () => {
    const {host, store} = await mountFilter('archive', empty());
    expect(store.calls.filter((c) => c.name === 'suggest')).toHaveLength(0);
    expect(deep(host, '[part="values"]')).toBeNull();
    await type(host, 'ma');
    expect(store.calls.filter((c) => c.name === 'suggest').map((c) => c.args)).toEqual([['archive', 'ma', 'filter']]);
  });

  it('chooses the suggestion the arrow keys reach with Enter, the first by default, and nothing for text that suggests nothing', async () => {
    const {host, store, el} = await mountFilter('archive', empty());
    const entry = await type(host, 'zz.unlisted');
    const key = async (k: string) => {
      entry.dispatchEvent(new KeyboardEvent('keydown', {key: k}));
      await settle(host);
    };
    await key('Enter');
    expect(drafts(store)).toHaveLength(0);
    await type(host, 'c');
    store.set('filters', {...store.get('filters'), suggestions: {archive: {q: 'c', verb: 'filter', values: [value(1, 'cs', 'CS'), value(2, 'cond', null), value(3, 'chem', null)], more: false, total: null}}});
    await settle(host);
    await key('Enter');
    expect(drafts(store).at(-1)!.filter['archive']).toEqual({family: 'category', keys: ['cs']});
    await key('ArrowDown');
    await key('ArrowDown');
    expect(el.shadowRoot!.querySelector('[part~="tick"][data-active]')!.id).toBe(entry.getAttribute('aria-activedescendant'));
    await key('Enter');
    expect(drafts(store).at(-1)!.filter['archive']).toEqual({family: 'category', keys: ['cs', 'chem']});
    // Past the last it wraps to the first, and Enter on a chosen value takes it out.
    await key('ArrowDown');
    await key('Enter');
    expect(drafts(store).at(-1)!.filter['archive']).toEqual({family: 'category', keys: ['chem']});
  });

  it('renders only the page that answers the box in front of it, and chooses a suggestion', async () => {
    const {host, store} = await mountFilter('archive', empty());
    await type(host, 'mach');
    expect(store.calls.filter((c) => c.name === 'suggest').at(-1)!.args).toEqual(['archive', 'mach', 'filter']);
    // A page for a different `q` (an earlier keystroke, landed late) is not shown for the box.
    store.set('filters', {...store.get('filters'), suggestions: {archive: {q: 'mac', verb: 'filter', values: [value(9, 'stat.ML', 'Machine Learning (Statistics)')], more: false, total: null}}});
    await settle(host);
    expect(deepAll(host, '[part~="tick"]')).toHaveLength(0);
    store.set('filters', {
      ...store.get('filters'),
      suggestions: {archive: {q: 'mach', verb: 'filter', values: [{code: 41207, key: 'cs.LG', title: 'Machine Learning', match: {field: 'title', start: 0, len: 4}}], more: true, total: null}}
    });
    await settle(host);
    const ticks = deepAll(host, '[part~="tick"]');
    expect(ticks).toHaveLength(1);
    expect(deep(host, '[part~="tick"] mark')?.textContent).toBe('Mach');
    expect(deep(host, '[part="more"]')).not.toBeNull();
    (ticks[0] as HTMLElement).click();
    expect((drafts(store)[0]!.filter['archive'] as {keys: string[]}).keys).toEqual(['cs.LG']);
  });

  it('shows each value’s count and its share of the total the server counted over', async () => {
    const {host, store} = await mountFilter('archive', empty());
    // The view's own totals are not what the counts are over.
    store.set('view', {...store.get('view'), visible: {value: 1000, exact: true}, matched: {value: 400, exact: true}});
    await type(host, 'c');
    store.set('filters', {...store.get('filters'), suggestions: {archive: {q: 'c', verb: 'filter', values: [value(1, 'cs', 'CS', 250), value(2, 'cond', null, 0)], more: false, total: 500}}});
    await settle(host);
    expect(deepAll(host, '[part="value-count"]').map((c) => c.textContent)).toEqual(['250', '0']);
    expect(deepAll(host, '[part="bar"]').map((b) => (b as HTMLElement).style.width)).toEqual(['50.0%', '0.0%']);
  });

  it('draws no bar where the page carries no total', async () => {
    const {host, store} = await mountFilter('archive', empty());
    await type(host, 'c');
    store.set('filters', {...store.get('filters'), suggestions: {archive: {q: 'c', verb: 'filter', values: [value(1, 'cs', 'CS', 250)], more: false, total: null}}});
    await settle(host);
    expect(deepAll(host, '[part="value-count"]').map((c) => c.textContent)).toEqual(['250']);
    expect(deepAll(host, '[part="bar"]')).toHaveLength(0);
  });

  it('has the store forget the column when the box is emptied and when the control goes away', async () => {
    const {host, store} = await mountFilter('archive', empty());
    const forgot = () => store.calls.filter((c) => c.name === 'forgetSuggestions').map((c) => c.args);
    await type(host, 'c');
    expect(forgot()).toEqual([]);
    await type(host, '');
    expect(forgot()).toEqual([['archive']]);
    await type(host, 'ma');
    host.querySelector('mosaica-filter')!.remove();
    expect(forgot()).toEqual([['archive'], ['archive']]);
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
      ['archive', 'ma', 'filter'],
      ['archive', 'ma', 'filter']
    ]);
  });
});

describe('<mosaica-filter> on a category, its suggestions', () => {
  it('show while the box has focus and close as focus leaves it', async () => {
    const {host, store} = await mountFilter('archive', filtersOf({filter: {archive: {family: 'category', keys: []}}, highlight: {}}));
    const entry = await type(host, 'c');
    store.set('filters', {...store.get('filters'), suggestions: {archive: {q: 'c', verb: 'filter', values: [value(1, 'cs', 'CS')], more: false, total: null}}});
    await settle(host);
    expect(deepAll(host, '[part~="tick"]')).toHaveLength(1);
    entry.dispatchEvent(new Event('blur'));
    await settle(host);
    expect(deep(host, '[part="values"]')).toBeNull();
    entry.dispatchEvent(new Event('focus'));
    await settle(host);
    expect(deepAll(host, '[part~="tick"]')).toHaveLength(1);
  });
});

describe('<mosaica-filter> on text', () => {
  it('sends one box’s words, phrases and alternatives after the typing pause', async () => {
    vi.useFakeTimers();
    const {host, store} = await mountFilter('title', filtersOf({filter: {title: {family: 'text', query: '', phrase: true}}, highlight: {}}));
    const changes: unknown[] = [];
    host.addEventListener('mosaica-filterchange', (e) => changes.push((e as CustomEvent).detail));
    await type(host, '"graph neural" OR lattice');
    expect(drafts(store)).toHaveLength(0);
    await vi.runAllTimersAsync();
    expect(drafts(store)[0]!.filter['title']).toEqual({family: 'text', query: '"graph neural" OR lattice', phrase: true});
    expect(changes).toEqual([{column: 'title', verb: 'filter', expr: {any_of: [{title: {phrase: 'graph neural'}}, {title: {match: 'lattice'}}]}}]);
  });

  it('shows a clause set from outside that no query writes read-only', async () => {
    const expr = {title: {match: 'salt OR pepper'}};
    const {host} = await mountFilter('title', filtersOf({filter: {title: {family: 'text', query: '', phrase: true, expr}}, highlight: {}}));
    const entry = deep(host, '[part="entry"]') as HTMLInputElement;
    expect(entry.readOnly).toBe(true);
    expect(entry.value).toBe(JSON.stringify(expr));
  });

});

describe('<mosaica-filter> on a keyword column', () => {
  const meta: Meta = {...META, filterOperands: [{column: 'author', family: 'keyword', operands: ['eq', 'prefix']}]};

  it('offers only the operators the column publishes, and sends the draft’s operator', async () => {
    vi.useFakeTimers();
    const host = await mount('<mosaica-filter column="author"></mosaica-filter>');
    const el = host.querySelector('mosaica-filter') as MosaicaFilter;
    const store = fakeStore({meta, status: status({})});
    store.set('filters', filtersOf({filter: {author: {family: 'keyword', needle: '', op: 'eq'}}, highlight: {}}));
    el.store = store;
    await settle(host);
    const button = deep(host, '[part="mode"]') as HTMLButtonElement;
    expect(button.textContent?.trim()).toBe('is');
    button.click();
    await settle(host);
    const operators = () => deepAll(host, '[part~="operator"]');
    expect(operators().map((o) => [o.getAttribute('data-op'), o.getAttribute('aria-checked')])).toEqual([
      ['eq', 'true'],
      ['prefix', 'false']
    ]);
    button.click();
    await settle(host);
    expect(operators()).toHaveLength(0);
    await type(host, 'Knuth');
    await vi.runAllTimersAsync();
    const sent = () => store.calls.filter((c) => c.name === 'setFilters').at(-1)!.args[0] as {filter: {author: {op: string; needle: string}}};
    expect(sent().filter.author).toMatchObject({op: 'eq', needle: 'Knuth'});
    // Choosing an operator closes the menu and sends it at once.
    (deep(host, '[part="mode"]') as HTMLButtonElement).click();
    await settle(host);
    (operators()[1] as HTMLButtonElement).click();
    await settle(host);
    expect(operators()).toHaveLength(0);
    expect(sent().filter.author).toMatchObject({op: 'prefix', needle: 'Knuth'});
  });
});

describe('<mosaica-filter-panel>', () => {
  async function mountPanel(draft: FilterDraft, attrs = '', over: Partial<FiltersProjection> = {}) {
    const host = await mount(`<mosaica-filter-panel ${attrs}></mosaica-filter-panel>`);
    const panel = host.querySelector('mosaica-filter-panel') as MosaicaFilterPanel;
    const store = fakeStore({meta: META, status: status({}), filters: filtersOf(draft, over)});
    panel.store = store;
    await settle(host);
    const sent = () => store.calls.filter((c) => c.name === 'setFilters').at(-1)!.args[0] as FilterDraft;
    const cards = () => deepAll(host, 'mosaica-field-card') as MosaicaFieldCard[];
    const fields = () => cards().map((c) => [c.field, !c.folded]);
    return {host, panel, store, sent, cards, fields};
  }
  const none = (): FilterDraft => ({filter: {archive: {family: 'category', keys: []}, title: {family: 'text', query: '', phrase: true}, submitted_at: {family: 'numeric', gte: null, lte: null}}, highlight: {}});

  it('lists no card until one is pinned, holds a clause or is added', async () => {
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
    expect(deep(host, '[part="add-note"]')!.textContent).toBe('2 more fields');
  });

  it('adds a card at the end of the list, so the cards shown do not move, and marks it Shown in Add field', async () => {
    const {host, fields} = await mountPanel(none());
    const add = async (column: string) => {
      (deep(host, '[part="add"]') as HTMLButtonElement).click();
      await settle(host);
      (deep(host, `[part~="add-option"][data-column="${column}"]`) as HTMLButtonElement).click();
      await settle(host);
    };
    await add('submitted_at');
    await add('archive');
    expect(fields().map(([c]) => c)).toEqual(['submitted_at', 'archive']);
    (deep(host, '[part="add"]') as HTMLButtonElement).click();
    await settle(host);
    const said = deepAll(host, '[part~="add-option"]').map((o) => [o.getAttribute('data-column'), o.querySelector('.kind')?.textContent ?? '']);
    expect(said).toEqual([
      ['archive', 'Shown'],
      ['title', ''],
      ['submitted_at', 'Shown']
    ]);
  });

  it('checks the listed fields in Add field, and choosing one takes its card off with its clause in both positions', async () => {
    const draft: FilterDraft = {filter: {...none().filter, archive: {family: 'category', keys: ['cs']}}, highlight: {archive: {family: 'category', keys: ['math']}}};
    const {host, store, sent, fields} = await mountPanel(draft);
    const changes: unknown[] = [];
    host.addEventListener('mosaica-filterchange', (e) => changes.push((e as CustomEvent).detail));
    const open = async () => {
      (deep(host, '[part="add"]') as HTMLButtonElement).click();
      await settle(host);
    };
    const options = () => deepAll(host, '[part~="add-option"]').map((o) => [o.getAttribute('data-column'), o.getAttribute('aria-checked')]);
    await open();
    (deep(host, '[part~="add-option"][data-column="title"]') as HTMLButtonElement).click();
    await settle(host);
    expect(fields()).toEqual([
      ['archive', true],
      ['title', true]
    ]);
    await open();
    expect(options()).toEqual([
      ['archive', 'true'],
      ['title', 'true'],
      ['submitted_at', 'false']
    ]);
    (deep(host, '[part~="add-option"][data-column="archive"]') as HTMLButtonElement).click();
    expect(sent()).toEqual({filter: {...draft.filter, archive: {family: 'category', keys: []}}, highlight: {archive: {family: 'category', keys: []}}});
    expect(changes).toEqual([
      {column: 'archive', verb: 'filter', expr: null},
      {column: 'archive', verb: 'highlight', expr: null}
    ]);
    store.set('filters', filtersOf(sent()));
    await settle(host);
    expect(fields()).toEqual([['title', true]]);
    // A field holding no clause comes off without a write.
    const writes = store.calls.filter((c) => c.name === 'setFilters').length;
    await open();
    (deep(host, '[part~="add-option"][data-column="title"]') as HTMLButtonElement).click();
    await settle(host);
    expect(fields()).toEqual([]);
    expect(store.calls.filter((c) => c.name === 'setFilters')).toHaveLength(writes);
  });

  it('adds the first unlisted match on Enter in the Add field search, and never takes a card off', async () => {
    const draft: FilterDraft = {filter: {...none().filter, archive: {family: 'category', keys: ['cs']}}, highlight: {}};
    const {host, store, fields} = await mountPanel(draft);
    const enter = async (text: string) => {
      (deep(host, '[part="add"]') as HTMLButtonElement).click();
      await settle(host);
      const search = deep(host, '[part="add-search"]') as HTMLInputElement;
      search.value = text;
      search.dispatchEvent(new Event('input'));
      await settle(host);
      search.dispatchEvent(new KeyboardEvent('keydown', {key: 'Enter', bubbles: true, composed: true}));
      await settle(host);
    };
    // `archive` is listed and matches first; Enter passes over it to `title`.
    await enter('i');
    expect(fields()).toEqual([
      ['archive', true],
      ['title', true]
    ]);
    // Only listed fields match: Enter does nothing, and the clause stays.
    await enter('archive');
    expect(fields()).toEqual([
      ['archive', true],
      ['title', true]
    ]);
    expect(store.calls.filter((c) => c.name === 'setFilters')).toHaveLength(0);
  });

  it('puts the first match in the tab order again after the search changes', async () => {
    const {host, panel} = await mountPanel(none());
    (deep(host, '[part="add"]') as HTMLButtonElement).click();
    await settle(host);
    const search = deep(host, '[part="add-search"]') as HTMLInputElement;
    const press = (key: string) => panel.shadowRoot!.activeElement!.dispatchEvent(new KeyboardEvent('keydown', {key, bubbles: true, composed: true}));
    press('ArrowDown');
    press('End');
    await settle(host);
    const tabbable = () => deepAll(host, '[part~="add-option"]').filter((o) => o.getAttribute('tabindex') === '0').map((o) => o.getAttribute('data-column'));
    expect(tabbable()).toEqual(['submitted_at']);
    search.value = 't';
    search.dispatchEvent(new Event('input'));
    await settle(host);
    expect(tabbable()).toEqual(['title']);
  });

  it('keeps a pinned card listed when it is chosen in Add field', async () => {
    const {host, store, fields} = await mountPanel(none(), 'pinned="title"');
    (deep(host, '[part="add"]') as HTMLButtonElement).click();
    await settle(host);
    const option = deep(host, '[part~="add-option"][data-column="title"]') as HTMLButtonElement;
    expect(option.getAttribute('aria-checked')).toBe('true');
    expect(option.getAttribute('aria-disabled')).toBe('true');
    option.click();
    await settle(host);
    expect(fields()).toEqual([['title', true]]);
    expect(store.calls.filter((c) => c.name === 'setFilters')).toHaveLength(0);
  });

  it('moves through Add field with the arrow keys, Home and End, and closes on Escape', async () => {
    const {host, panel} = await mountPanel(none());
    (deep(host, '[part="add"]') as HTMLButtonElement).click();
    await settle(host);
    const focused = () => (panel.shadowRoot!.activeElement as HTMLElement | null)?.getAttribute('data-column') ?? panel.shadowRoot!.activeElement?.getAttribute('part');
    const press = (key: string) => panel.shadowRoot!.activeElement!.dispatchEvent(new KeyboardEvent('keydown', {key, bubbles: true, composed: true}));
    expect(focused()).toBe('add-search');
    press('ArrowDown');
    expect(focused()).toBe('archive');
    press('ArrowDown');
    expect(focused()).toBe('title');
    press('End');
    expect(focused()).toBe('submitted_at');
    press('Home');
    expect(focused()).toBe('archive');
    press('ArrowUp');
    expect(focused()).toBe('add-search');
    press('Escape');
    await settle(host);
    expect(deep(host, '[part="add-list"]')).toBeNull();
    expect(focused()).toBe('add');
  });

  it('keeps a card listed while its clause is emptied under the user', async () => {
    vi.useFakeTimers();
    const draft: FilterDraft = {filter: {...none().filter, title: {family: 'text', query: 'graph', phrase: true}}, highlight: {}};
    const {host, store, fields} = await mountPanel(draft);
    expect(fields()).toEqual([['title', true]]);
    await type(host, '');
    await vi.runAllTimersAsync();
    // The store publishes the emptied clause; the card the user is editing stays.
    store.set('filters', filtersOf(drafts(store).at(-1)!));
    await settle(host);
    expect(fields()).toEqual([['title', true]]);
  });

  it('lists the pinned cards in meta’s order, and folds one to a line from its own button', async () => {
    const {host, fields} = await mountPanel(none(), 'pinned="title archive"');
    expect(fields()).toEqual([
      ['archive', true],
      ['title', true]
    ]);
    (deep(host, 'mosaica-field-card[data-field="archive"]')!.shadowRoot!.querySelector('[part="fold"]') as HTMLButtonElement).click();
    await settle(host);
    expect(fields()).toEqual([
      ['archive', false],
      ['title', true]
    ]);
  });

  it('lists the card of the field the map is coloured by, which says what its colours mean', async () => {
    const {host, store, fields} = await mountPanel(none());
    store.set('legend', {...store.get('legend'), colourBy: 'submitted_at'});
    await settle(host);
    expect(fields()).toEqual([['submitted_at', true]]);
  });

  it('has no Filter / Highlight switch: a card’s row sets either clause', async () => {
    const {host} = await mountPanel(none(), 'pinned="archive"');
    expect(deep(host, '[part="mode"]')).toBeNull();
    expect(deepAll(host, '[role="radiogroup"]')).toHaveLength(0);
  });

  it('folds every card but one in the compact layout, and opening one folds the one open before', async () => {
    const {host, panel, fields} = await mountPanel(none(), 'pinned="title archive submitted_at" compact');
    expect(fields()).toEqual([
      ['archive', false],
      ['title', false],
      ['submitted_at', false]
    ]);
    const fold = (field: string) => deep(host, `mosaica-field-card[data-field="${field}"]`)!.shadowRoot!.querySelector('[part="fold"]') as HTMLButtonElement;
    expect(fold('archive').getAttribute('aria-expanded')).toBe('false');
    fold('title').click();
    await settle(host);
    expect(fields()).toEqual([
      ['archive', false],
      ['title', true],
      ['submitted_at', false]
    ]);
    fold('archive').click();
    await settle(host);
    expect(fields()).toEqual([
      ['archive', true],
      ['title', false],
      ['submitted_at', false]
    ]);
    panel.compact = false;
    await settle(host);
    expect(fields().every(([, open]) => open)).toBe(true);
  });

  describe('the subject row', () => {
    it('reads In view and the matching count in view, beside All matching from its own aggregate', async () => {
      const {host, store} = await mountPanel(none());
      expect(registered(store).get([...registered(store).keys()].find((k) => k.startsWith('fields-match'))!)).toEqual({groupings: [{}]});
      store.set('view', {...store.get('view'), inView: {status: 'shown', visible: {value: 900, exact: true}, matched: {value: 206, exact: true}, highlighted: {value: 206, exact: true}, shown: 206}});
      answerAggregate(store, 'fields-match', aggregateEntry([{rows: [], total: 50_000}]));
      await settle(host);
      expect(deep(host, '[part="subject-name"]')!.textContent).toBe('In view');
      expect(deep(host, '[part="subject-count"]')!.textContent).toBe('206');
      expect(deep(host, '[part="all-count"]')!.textContent).toBe('50,000');
      expect(deep(host, '[part="clear-highlight"]')).toBeNull();
    });

    it('reads Highlighted while a highlight is set, and its × clears every highlight clause', async () => {
      const draft: FilterDraft = {filter: {...none().filter, archive: {family: 'category', keys: ['cs']}}, highlight: {submitted_at: {family: 'numeric', gte: 1, lte: 2}}};
      const {host, store, sent} = await mountPanel(draft, '', {members: [{layer: 'topics', artifact: 3n, outside: false, verb: 'highlight'}]});
      store.set('view', {...store.get('view'), inView: {status: 'shown', visible: {value: 900, exact: true}, matched: {value: 500, exact: true}, highlighted: {value: 43, exact: true}, shown: 43}});
      await settle(host);
      expect(deep(host, '[part="subject-name"]')!.textContent).toBe('Highlighted');
      expect(deep(host, '[part="subject-count"]')!.textContent).toBe('43');
      const changes: unknown[] = [];
      host.addEventListener('mosaica-filterchange', (e) => changes.push((e as CustomEvent).detail));
      (deep(host, '[part="clear-highlight"]') as HTMLButtonElement).click();
      expect(activeCount(sent(), 'highlight')).toBe(0);
      expect(activeCount(sent(), 'filter')).toBe(1);
      expect(store.calls.filter((c) => c.name === 'setMembers').at(-1)!.args[0]).toEqual([]);
      expect(changes).toEqual([{column: null, verb: 'highlight', expr: null}]);
    });
  });

  describe('a column both filtered and highlighted', () => {
    const both: FilterDraft = {
      filter: {archive: {family: 'category', keys: ['cs.LG', 'cs.CV']}, title: {family: 'text', query: '', phrase: true}},
      highlight: {archive: {family: 'category', keys: ['cs.CV', 'stat.ML']}}
    };
    const chip = (host: HTMLElement, verb: string) => deep(host, `[part="chip"][data-column="archive"][data-verb="${verb}"]`)!;

    it('shows a chip per clause in one line, filter first, and × removes one and leaves the other', async () => {
      const {host, sent} = await mountPanel(both);
      expect(deepAll(host, '[part="chip"]').map((c) => c.getAttribute('data-verb'))).toEqual(['filter', 'highlight']);
      (chip(host, 'highlight').querySelector(':scope > button:last-child') as HTMLButtonElement).click();
      expect(sent()).toEqual({filter: both.filter, highlight: {archive: {family: 'category', keys: []}}});
      (chip(host, 'filter').querySelector(':scope > button:last-child') as HTMLButtonElement).click();
      expect(sent()).toEqual({filter: {...both.filter, archive: {family: 'category', keys: []}}, highlight: both.highlight});
    });

    it('opens and focuses a card on show()', async () => {
      const {host, panel, cards} = await mountPanel({...both, highlight: {}}, 'pinned="title"');
      (cards()[0]!.shadowRoot!.querySelector('[part="fold"]') as HTMLButtonElement).click();
      await settle(host);
      panel.show('title');
      await settle(host);
      const card = cards().find((c) => c.field === 'title')!;
      expect(card.folded).toBe(false);
      expect(panel.shadowRoot!.activeElement).toBe(card);
    });

    it('asks for the cards under chips-only, and show() lists the chip’s card', async () => {
      const {host, panel} = await mountPanel(both, 'chips-only');
      expect(deepAll(host, 'mosaica-field-card')).toHaveLength(0);
      const asked: unknown[] = [];
      host.addEventListener('mosaica-chipopen', (e) => asked.push((e as CustomEvent).detail));
      (chip(host, 'highlight').querySelector('[part="edit"]') as HTMLButtonElement).click();
      expect(asked).toEqual([{column: 'archive', verb: 'highlight'}]);
      panel.chipsOnly = false;
      panel.show('archive');
      await settle(host);
      expect(deepAll(host, 'mosaica-field-card').map((c) => (c as MosaicaFieldCard).field)).toContain('archive');
    });

    it('empties both positions on Clear all', async () => {
      const {host, sent} = await mountPanel(both);
      (deep(host, '[part="clear"]') as HTMLButtonElement).click();
      expect(activeCount(sent())).toBe(0);
      expect(sent().highlight).toEqual({});
    });
  });

  it('leaves the subject row and the chips out under controls-only, and the cards out under chips-only', async () => {
    const draft: FilterDraft = {filter: {...none().filter, archive: {family: 'category', keys: ['cs']}}, highlight: {}};
    const controls = await mountPanel(draft, 'controls-only');
    expect(deepAll(controls.host, '[part="chip"]')).toHaveLength(0);
    expect(deep(controls.host, '[part="subject"]')).toBeNull();
    expect(deepAll(controls.host, 'mosaica-field-card')).toHaveLength(1);
    document.body.innerHTML = '';
    const chips = await mountPanel(draft, 'chips-only');
    expect(deepAll(chips.host, '[part="chip"]')).toHaveLength(1);
    expect(deepAll(chips.host, 'mosaica-field-card')).toHaveLength(0);
  });

  it('does not rebuild a card under the user on a store tick', async () => {
    const {store, cards, host} = await mountPanel({...none(), filter: {...none().filter, archive: {family: 'category', keys: ['cs']}}});
    const before = cards()[0];
    store.set('status', status({status: 'loading'}));
    await settle(host);
    expect(cards()[0]).toBe(before);
  });

  it('marks a member_of chip with no label and no served name as unnamed, never by its key', async () => {
    const {host} = await mountPanel({filter: {}, highlight: {}}, 'chips-only', {members: [{layer: 'clusters', artifact: 4n, outside: false, verb: 'filter'}]});
    const chip = deep(host, '[part="chip"][data-artifact="4"]')!;
    expect(chip.textContent).toContain(UNNAMED);
    expect(chip.textContent).not.toContain('clusters');
  });

  it('draws an artifact filtered and highlighted as two chips, and removes one of them', async () => {
    const clause = {layer: 'mesh/descriptors', artifact: 546_790n, outside: false};
    const {host, store} = await mountPanel({filter: {}, highlight: {}}, 'chips-only', {
      members: [
        {...clause, verb: 'highlight'},
        {...clause, verb: 'filter'}
      ]
    });
    const chips = deepAll(host, '[part="chip"][data-artifact="546790"]');
    expect(chips.map((c) => c.getAttribute('data-verb'))).toEqual(['filter', 'highlight']);
    (chips[0]!.querySelector('button') as HTMLButtonElement).click();
    expect(store.calls.filter((c) => c.name === 'setMembers').at(-1)!.args[0]).toEqual([{...clause, verb: 'highlight'}]);
  });
});

describe('<mosaica-filter-panel> cluster fields', () => {
  const layer = (name: string, over: Partial<Meta['layers'][number]> = {}) =>
    ({name, title: `${name} title`, views: ['s0'], membership: 'enumerated', hierarchy: {kind: 'nested', pruneChildren: false}, levels: [], computedContent: ['centroid'], shape: null, suppliedContent: ['name'], depsOn: [], version: 1, ...over}) as Meta['layers'][number];
  const LAYERED = {...META, layers: [layer('topics'), layer('labels', {depsOn: ['topics']}), layer('elsewhere', {views: ['s9']})]};
  const topic = (id: bigint, verb: 'filter' | 'highlight', label?: string) => ({layer: 'topics', artifact: id, outside: false, verb, ...(label ? {label} : {})});

  async function mountLayered(members: ReturnType<typeof topic>[] = []) {
    const host = await mount('<mosaica-filter-panel></mosaica-filter-panel>');
    const panel = host.querySelector('mosaica-filter-panel') as MosaicaFilterPanel;
    const store = fakeStore({meta: LAYERED, status: status({}), filters: filtersOf({filter: {}, highlight: {}}, {members})});
    store.set('view', {...store.get('view'), id: 's0'});
    panel.store = store;
    await settle(host);
    return {host, panel, store};
  }

  it('offers each layer of the view that attaches to no other in Add field, after the columns', async () => {
    const {host} = await mountLayered();
    (deep(host, '[part="add"]') as HTMLButtonElement).click();
    await settle(host);
    const options = deepAll(host, '[part~="add-option"]');
    expect(options.map((o) => o.getAttribute('data-column') ?? `layer:${o.getAttribute('data-layer')}`)).toEqual(['archive', 'title', 'submitted_at', 'layer:topics']);
    expect(options.at(-1)!.textContent).toContain('topics title');
    (options.at(-1) as HTMLButtonElement).click();
    await settle(host);
    expect(deep(host, 'mosaica-field-card[data-field="cluster:topics"]')).not.toBeNull();
  });

  it('lists a layer holding a clause, and shows its clauses as chips, the filter’s first', async () => {
    const {host} = await mountLayered([topic(8n, 'highlight', 'Optics'), topic(7n, 'filter', 'Neural networks'), topic(9n, 'filter')]);
    expect(deep(host, 'mosaica-field-card[data-field="cluster:topics"]')).not.toBeNull();
    const chips = deepAll(host, '[part="chip"]').map((c) => [c.getAttribute('data-verb'), c.textContent!.trim()]);
    expect(chips).toEqual([
      ['filter', 'Neural networks'],
      ['filter', UNNAMED],
      ['highlight', 'Optics']
    ]);
  });

  it('shows a clause on a layer with no card, in either position, as a chip that takes it off', async () => {
    const lit = {layer: 'labels', artifact: 3n, outside: false, verb: 'highlight' as const, label: 'diffusion, guidance'};
    const elsewhere = {layer: 'elsewhere', artifact: 4n, outside: true, verb: 'filter' as const, label: 'North'};
    const {host, store} = await mountLayered([topic(7n, 'filter', 'Neural networks'), lit as never, elsewhere as never]);
    const chip = (artifact: string) => deep(host, `[part="chip"][data-artifact="${artifact}"]`)!;
    expect(chip('3').textContent!.trim()).toBe('diffusion, guidance');
    expect(chip('4').textContent!.trim()).toBe('Outside North');
    (chip('3').querySelector('button') as HTMLButtonElement).click();
    expect(store.calls.filter((c) => c.name === 'setMembers').at(-1)!.args[0]).toEqual([topic(7n, 'filter', 'Neural networks'), elsewhere]);
  });

  it('takes a layer off with every clause on it, in both positions, when its checked entry is chosen again', async () => {
    const {host, store} = await mountLayered([topic(7n, 'filter'), topic(8n, 'highlight')]);
    (deep(host, '[part="add"]') as HTMLButtonElement).click();
    await settle(host);
    const entry = deep(host, '[part~="add-option"][data-layer="topics"]') as HTMLButtonElement;
    expect(entry.getAttribute('aria-checked')).toBe('true');
    entry.click();
    await settle(host);
    expect(store.calls.filter((c) => c.name === 'setMembers').at(-1)!.args[0]).toEqual([]);
  });
});

describe('<mosaica-filter-panel chips-only>', () => {
  it('renders nothing while no clause is applied, and the chips once one is', async () => {
    const host = await mount('<mosaica-filter-panel chips-only></mosaica-filter-panel>');
    const store = fakeStore({meta: META, status: status({})});
    (host.querySelector('mosaica-filter-panel') as unknown as {store: unknown}).store = store;
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
