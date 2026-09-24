import {afterEach, describe, expect, it, vi} from 'vitest';
import type {FiltersProjection, Meta} from '@tesseradb/client';
import '../src/item-card.js';
import '../src/filter.js';
import '../src/filter-panel.js';
import type {TesseraItemCard} from '../src/item-card.js';
import type {TesseraFilter} from '../src/filter.js';
import {deep, deepAll, fakeStore, mount, settle, status, meta, scalar} from './fake-store.js';
import {UNNAMED} from '../src/base.js';

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
    el.item = {id: 12345678901234567890n, detail: {fields: {note: 'x', title: 'A title', submitted_at: 1_700_000_000_000_000}, externalId: null, labels: [], views: [], scoped: {}}};
    await settle(host);
    // The headline is the field the host named; the rest in declared-then-extra order, then the
    // id; the absent `archive` is not rendered.
    expect(deep(host, '[part="headline"]')?.getAttribute('data-name')).toBe('title');
    expect(deepAll(host, '[part="field"]').map((f) => f.getAttribute('data-name'))).toEqual(['submitted_at', 'note', 'tessera_id']);
    expect(deep(host, '[part="field"][data-name="tessera_id"] [part="value"]')?.textContent).toBe('12345678901234567890');
    expect(deep(host, '[part="field"][data-name="submitted_at"] [part="value"]')?.textContent).toBe('2023-11-14T22:13:20.000Z');
    expect(deep(host, 'slot[name="field-title"]')).not.toBeNull();
    expect(host.textContent).toContain('my link');
    expect(deep(host, '[part="field"][data-name="archive"]')).toBeNull();
  });

  it('with no title field, heads the card with the id, once, and shows every other field in the grid', async () => {
    const host = await mount('<tessera-item-card></tessera-item-card>');
    const el = host.querySelector('tessera-item-card') as TesseraItemCard;
    el.meta = META;
    el.item = {id: 42n, detail: {fields: {note: 'x', title: 'A title'}, externalId: null, labels: [], views: [], scoped: {}}};
    await settle(host);
    expect(deep(host, '[part="headline"]')?.textContent).toBe('42');
    expect(deepAll(host, '[part="field"]').map((f) => f.getAttribute('data-name'))).toEqual(['title', 'note']);
  });

  it('fires tessera-open with the id as a decimal string, bubbling and composed', async () => {
    const host = await mount('<tessera-item-card></tessera-item-card>');
    const el = host.querySelector('tessera-item-card') as TesseraItemCard;
    el.item = {id: 2n ** 63n + 1n, detail: {fields: {}, externalId: null, labels: [], views: [], scoped: {}}};
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
    store.set('selection', {item: {id: 5n, detail: {fields: {archive: 'cs'}, externalId: null, labels: [], views: [], scoped: {}}}, itemRefusal: null, artifact: null, artifactRefusal: null});
    await settle(host);
    expect(deep(host, '[part="field"][data-name="archive"] [part="value"]')?.textContent).toBe('cs');
  });
});

describe('<tessera-filter>', () => {
  it('asks the store to suggest on mount, with an empty q — the picker’s list before typing', async () => {
    const host = await mount('<tessera-filter column="archive"></tessera-filter>');
    const el = host.querySelector('tessera-filter') as TesseraFilter;
    const store = fakeStore({meta: META, status: status({})});
    store.set('filters', {draft: {archive: {family: 'category', keys: [], verb: 'filter'}}, expr: null, highlight: null, members: [], suggestions: {}, suggestErrors: {}, suggestEpoch: 0});
    el.store = store;
    await settle(host);
    const asked = store.calls.filter((c) => c.name === 'suggest');
    expect(asked.length).toBe(1);
    expect(asked[0]!.args).toEqual(['archive', '']);
  });

  it('submits a typed category key never listed, and never renders "no such value"', async () => {
    const host = await mount('<tessera-filter column="archive"></tessera-filter>');
    const el = host.querySelector('tessera-filter') as TesseraFilter;
    const store = fakeStore({meta: META, status: status({})});
    // `more: true` gives the lookahead, with its search box; `more: false` gives a checklist,
    // covered below.
    store.set('filters', {
      draft: {archive: {family: 'category', keys: [], verb: 'filter'}},
      expr: null,
      highlight: null,
      members: [],
      suggestions: {archive: {q: '', values: [{code: 1, key: 'cs', title: 'Computer Science', match: {field: 'key', start: 0, len: 0}}], more: true}},
      suggestErrors: {},
      suggestEpoch: 0
    });
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
    expect(deep(host, '[part="refusal"]')).toBeNull();
    // The typed key is submitted and shows up as a chip, chosen, unresolved.
    expect(deep(host, '[part="value-chip"]')?.textContent).toContain('zz.unlisted');
  });

  it('renders a checklist, not a search box, when the empty-q page says more: false', async () => {
    const host = await mount('<tessera-filter column="archive"></tessera-filter>');
    const el = host.querySelector('tessera-filter') as TesseraFilter;
    const store = fakeStore({meta: META, status: status({})});
    // Nine values on one page and no more to page through: a checkbox per value, not a typeahead.
    const values = Array.from({length: 9}, (_, i) => ({code: i + 1, key: `v${i}`, title: `Value ${i}`, match: {field: 'key' as const, start: 0, len: 0}}));
    store.set('filters', {
      draft: {archive: {family: 'category', keys: ['v2'], verb: 'filter'}},
      expr: null,
      highlight: null,
      members: [],
      suggestions: {archive: {q: '', values, more: false}},
      suggestErrors: {},
      suggestEpoch: 0
    });
    el.store = store;
    await settle(host);

    expect(deep(host, '[part="entry"]')).toBeNull();
    expect(deep(host, '[part="more"]')).toBeNull();
    const boxes = deepAll(host, '[part="tick"] input[type="checkbox"]') as HTMLInputElement[];
    expect(boxes.length).toBe(9);
    // No match span in a checklist: nothing was typed for the server to have marked.
    expect(deep(host, '[part="tick"] mark')).toBeNull();
    expect(boxes[2]!.checked).toBe(true); // v2 is chosen
    expect(boxes[0]!.checked).toBe(false);

    boxes[0]!.click();
    await settle(host);
    const sent = store.calls.find((c) => c.name === 'setFilters');
    expect((sent!.args[0] as {archive: {keys: string[]}}).archive.keys).toEqual(['v2', 'v0']);
  });

  it('does not switch shape mid-typing: a lookahead page narrowing to more: false stays a lookahead', async () => {
    const host = await mount('<tessera-filter column="archive"></tessera-filter>');
    const el = host.querySelector('tessera-filter') as TesseraFilter;
    const store = fakeStore({meta: META, status: status({})});
    store.set('filters', {draft: {archive: {family: 'category', keys: [], verb: 'filter'}}, expr: null, highlight: null, members: [], suggestions: {archive: {q: '', values: [], more: true}}, suggestErrors: {}, suggestEpoch: 0});
    el.store = store;
    await settle(host);
    expect(deep(host, '[part="entry"]')).not.toBeNull();

    const entry = deep(host, '[part="entry"]') as HTMLInputElement;
    entry.value = 'fr';
    entry.dispatchEvent(new Event('input'));
    // The typed prefix narrows to a short page that would, on its own, say "checklist" — but the
    // shape was already decided from the empty-q page and is not re-decided from this one.
    store.set('filters', {...store.get('filters'), suggestions: {archive: {q: 'fr', values: [{code: 1, key: 'fr.abc', title: null, match: {field: 'key', start: 0, len: 2}}], more: false}}});
    await settle(host);
    expect(deep(host, '[part="entry"]')).not.toBeNull();
    expect(deepAll(host, '[part="tick"] input[type="checkbox"]').length).toBe(0);
  });

  it('re-decides the shape once the store’s suggestEpoch moves — a view change or a re-authorise', async () => {
    const host = await mount('<tessera-filter column="archive"></tessera-filter>');
    const el = host.querySelector('tessera-filter') as TesseraFilter;
    const store = fakeStore({meta: META, status: status({})});
    const values = [{code: 1, key: 'v0', title: 'Value 0', match: {field: 'key' as const, start: 0, len: 0}}];
    store.set('filters', {draft: {archive: {family: 'category', keys: [], verb: 'filter'}}, expr: null, highlight: null, members: [], suggestions: {archive: {q: '', values, more: false}}, suggestErrors: {}, suggestEpoch: 0});
    el.store = store;
    await settle(host);
    expect(deep(host, '[part="entry"]')).toBeNull(); // checklist

    // `resetSuggestions` clears the column's page, raises no refusal for it, and bumps the epoch —
    // the epoch moving is the signal this element acts on, not the page's absence on its own
    // (`store.ts`'s own doc on why the `q` echo cannot tell the two apart unaided).
    store.set('filters', {...store.get('filters'), suggestions: {}, suggestErrors: {}, suggestEpoch: 1});
    await settle(host);
    // Re-decided from scratch: the control asks again with an empty q for the (column, view) it
    // is now under, and shows neither shape until that page answers.
    const asked = store.calls.filter((c) => c.name === 'suggest' && c.args[0] === 'archive' && c.args[1] === '');
    expect(asked.length).toBe(2); // the mount's ask, and this one

    // The new view's column answers wide open — the other shape entirely.
    store.set('filters', {...store.get('filters'), suggestions: {archive: {q: '', values: [], more: true}}});
    await settle(host);
    expect(deep(host, '[part="entry"]')).not.toBeNull(); // lookahead this time
  });

  it('re-asks under the new epoch even where nothing about this column changed at the reset — mid-flight or on a refusal', async () => {
    // The column that never finished deciding a shape carries no signal of its own that an
    // invalidation happened: `suggestions[column]` and `suggestErrors[column]` are both already
    // absent before and after a reset alike, so the earlier `shape !== null` gate left this column
    // sitting on its skeleton forever. Two starting states, both stuck with `shape === null`.
    const starts: Pick<FiltersProjection, 'suggestions' | 'suggestErrors'>[] = [
      // Still in flight: no page and no refusal yet, the first ask not answered.
      {suggestions: {}, suggestErrors: {}},
      // Sitting on a refusal: the empty-q ask came back refused.
      {suggestions: {}, suggestErrors: {archive: {code: 'derived', detail: 'not listable'}}}
    ];
    for (const initial of starts) {
      const host = await mount('<tessera-filter column="archive"></tessera-filter>');
      const el = host.querySelector('tessera-filter') as TesseraFilter;
      const store = fakeStore({meta: META, status: status({})});
      store.set('filters', {draft: {archive: {family: 'category', keys: [], verb: 'filter'}}, expr: null, highlight: null, members: [], suggestions: initial.suggestions, suggestErrors: initial.suggestErrors, suggestEpoch: 0});
      el.store = store;
      await settle(host);
      const askedBefore = store.calls.filter((c) => c.name === 'suggest' && c.args[0] === 'archive' && c.args[1] === '').length;
      expect(askedBefore).toBe(1); // the mount's own ask

      // The reset: still no page, still no fresh refusal, but the epoch has moved.
      store.set('filters', {...store.get('filters'), suggestions: {}, suggestErrors: {}, suggestEpoch: 1});
      await settle(host);
      const askedAfter = store.calls.filter((c) => c.name === 'suggest' && c.args[0] === 'archive' && c.args[1] === '').length;
      expect(askedAfter).toBe(2); // a second ask reached the store under the new epoch

      // And it decides a shape once that ask answers, exactly as a fresh mount would.
      store.set('filters', {...store.get('filters'), suggestions: {archive: {q: '', values: [], more: false}}});
      await settle(host);
      expect(deep(host, '[part="entry"]')).toBeNull(); // checklist
    }
  });

  it('asks the store on every keystroke, and renders only the page that answers the box in front of it', async () => {
    const host = await mount('<tessera-filter column="archive"></tessera-filter>');
    const el = host.querySelector('tessera-filter') as TesseraFilter;
    const store = fakeStore({meta: META, status: status({})});
    store.set('filters', {draft: {archive: {family: 'category', keys: [], verb: 'filter'}}, expr: null, highlight: null, members: [], suggestions: {}, suggestErrors: {}, suggestEpoch: 0});
    el.store = store;
    await settle(host);
    const entry = deep(host, '[part="entry"]') as HTMLInputElement;
    entry.value = 'mach';
    entry.dispatchEvent(new Event('input'));
    await settle(host);
    expect(store.calls.filter((c) => c.name === 'suggest').at(-1)!.args).toEqual(['archive', 'mach']);
    // A page for a different `q` (an earlier keystroke, landed late) is not shown for the box.
    store.set('filters', {
      ...store.get('filters'),
      suggestions: {archive: {q: 'mac', values: [{code: 9, key: 'stat.ML', title: 'Machine Learning (Statistics)', match: {field: 'title', start: 0, len: 3}}], more: false}}
    });
    await settle(host);
    expect(deepAll(host, '[part="tick"]').length).toBe(0);
    // The page that echoes the box's own text renders, matched span marked.
    store.set('filters', {
      ...store.get('filters'),
      suggestions: {archive: {q: 'mach', values: [{code: 41207, key: 'cs.LG', title: 'Machine Learning', match: {field: 'title', start: 0, len: 4}}], more: true}}
    });
    await settle(host);
    const ticks = deepAll(host, '[part="tick"]');
    expect(ticks.length).toBe(1);
    expect(deep(host, '[part="tick"] mark')?.textContent).toBe('Mach');
    expect(deep(host, '[part="more"]')).not.toBeNull();
    // Clicking the suggestion chooses it, and its title is remembered for the chip.
    (ticks[0] as HTMLElement).click();
    await settle(host);
    const sent = store.calls.find((c) => c.name === 'setFilters');
    expect((sent!.args[0] as {archive: {keys: string[]}}).archive.keys).toEqual(['cs.LG']);
  });

  it('renders a refused suggestion as a refusal beside a free entry, not as an absent control', async () => {
    const host = await mount('<tessera-filter column="archive"></tessera-filter>');
    const el = host.querySelector('tessera-filter') as TesseraFilter;
    const store = fakeStore({meta: META, status: status({})});
    store.set('filters', {draft: {archive: {family: 'category', keys: [], verb: 'filter'}}, expr: null, highlight: null, members: [], suggestions: {}, suggestErrors: {archive: {code: 'derived', detail: 'not listable'}}, suggestEpoch: 0});
    el.store = store;
    await settle(host);
    expect(deep(host, '[part="entry"]')).not.toBeNull();
    expect(deep(host, '[part="refusal"]')).not.toBeNull();
  });

  it('a text operand offers phrase only when the column publishes it, and debounces typing', async () => {
    vi.useFakeTimers();
    const host = await mount('<tessera-filter column="title"></tessera-filter>');
    const el = host.querySelector('tessera-filter') as TesseraFilter;
    const store = fakeStore({meta: META, status: status({})});
    store.set('filters', {draft: {title: {family: 'text', query: '', mode: 'all', verb: 'filter'}}, expr: null, highlight: null, members: [], suggestions: {}, suggestErrors: {}, suggestEpoch: 0});
    el.store = store;
    await settle(host);
    expect(deepAll(host, '[part="mode"] button').map((o) => o.getAttribute('data-mode'))).toEqual(['all', 'phrase']);
    const entry = deep(host, '[part="entry"]') as HTMLInputElement;
    entry.value = 'graph';
    entry.dispatchEvent(new Event('input'));
    expect(store.calls.some((c) => c.name === 'setFilters')).toBe(false);
    await vi.runAllTimersAsync();
    const sent = store.calls.find((c) => c.name === 'setFilters');
    expect((sent!.args[0] as {title: {query: string}}).title.query).toBe('graph');
    vi.useRealTimers();
  });
});

describe('<tessera-filter> on a keyword column', () => {
  const meta: Meta = {...META, filterOperands: [{column: 'author', family: 'keyword', operands: ['eq', 'prefix']}]};

  it('offers only the operators the column publishes, and sends the draft’s operator', async () => {
    vi.useFakeTimers();
    const host = await mount('<tessera-filter column="author"></tessera-filter>');
    const el = host.querySelector('tessera-filter') as TesseraFilter;
    const store = fakeStore({meta, status: status({})});
    store.set('filters', {draft: {author: {family: 'keyword', needle: '', op: 'eq', verb: 'filter'}}, expr: null, highlight: null, members: [], suggestions: {}, suggestErrors: {}, suggestEpoch: 0});
    el.store = store;
    await settle(host);
    const select = deep(host, 'select[part="mode"]') as HTMLSelectElement;
    expect([...select.options].map((o) => o.value)).toEqual(['eq', 'prefix']);
    expect(select.value).toBe('eq');
    const entry = deep(host, '[part="entry"]') as HTMLInputElement;
    entry.value = 'Knuth';
    entry.dispatchEvent(new Event('input'));
    await vi.runAllTimersAsync();
    const sent = store.calls.find((c) => c.name === 'setFilters')!.args[0] as {author: {op: string; needle: string}};
    expect(sent.author).toMatchObject({op: 'eq', needle: 'Knuth'});
  });
});

describe('<tessera-filter-panel>', () => {
  it('marks a member_of chip with no label and no served name as unnamed, never by its key', async () => {
    const host = await mount('<tessera-filter-panel></tessera-filter-panel>');
    const store = fakeStore({meta: META, status: status({})});
    store.set('filters', {draft: {}, expr: null, highlight: null, members: [{layer: 'clusters', artifact: 4n, outside: false, verb: 'filter'}], suggestions: {}, suggestErrors: {}, suggestEpoch: 0});
    (host.querySelector('tessera-filter-panel') as unknown as {store: unknown}).store = store;
    await settle(host);
    const chip = deep(host, '[part="chip"][data-artifact="4"]')!;
    expect(chip.textContent).toContain(UNNAMED);
    expect(chip.textContent).not.toContain('clusters');
  });

  /**
   * The chip's verb says which of the request's two expressions the clause joins, and clicking it
   * moves the clause with its predicate: the draft sent back carries the same keys.
   */
  it('moves a clause between filter and highlight from the chip, keeping its predicate', async () => {
    const host = await mount('<tessera-filter-panel></tessera-filter-panel>');
    const store = fakeStore({meta: META, status: status({})});
    store.set('filters', {
      draft: {archive: {family: 'category', keys: ['cs'], verb: 'filter'}},
      expr: {archive: {in: ['cs']}},
      highlight: null,
      members: [],
      suggestions: {},
      suggestErrors: {},
      suggestEpoch: 0
    });
    (host.querySelector('tessera-filter-panel') as unknown as {store: unknown}).store = store;
    await settle(host);
    (deep(host, '[part="verb"]') as HTMLButtonElement).click();
    const sent = store.calls.find((c) => c.name === 'setFilters');
    expect(sent!.args[0]).toEqual({archive: {family: 'category', keys: ['cs'], verb: 'highlight'}});
  });

  it('draws a member_of clause as a chip carrying the same verb, and removes it', async () => {
    const host = await mount('<tessera-filter-panel></tessera-filter-panel>');
    const store = fakeStore({meta: META, status: status({})});
    store.set('filters', {
      draft: {},
      expr: null,
      highlight: null,
      members: [{layer: 'mesh/descriptors', artifact: 546_790n, outside: false, verb: 'highlight'}],
      suggestions: {},
      suggestErrors: {},
      suggestEpoch: 0
    });
    (host.querySelector('tessera-filter-panel') as unknown as {store: unknown}).store = store;
    await settle(host);
    const chip = deep(host, '[part="chip"]')!;
    expect(chip.getAttribute('data-verb')).toBe('highlight');
    (chip.querySelector('[part="verb"]') as HTMLButtonElement).click();
    expect((store.calls.find((c) => c.name === 'setMembers')!.args[0] as {verb: string}[])[0]!.verb).toBe('filter');
  });


  it('renders one control per operand meta offers, keyed, with chips and clear all', async () => {
    const host = await mount('<tessera-filter-panel></tessera-filter-panel>');
    const store = fakeStore({meta: META, status: status({})});
    store.set('filters', {draft: {archive: {family: 'category', keys: ['cs'], verb: 'filter'}, title: {family: 'text', query: '', mode: 'all', verb: 'filter'}, submitted_at: {family: 'numeric', gte: null, lte: null, verb: 'filter'}}, expr: null, highlight: null, members: [], suggestions: {}, suggestErrors: {}, suggestEpoch: 0});
    (host.querySelector('tessera-filter-panel') as unknown as {store: unknown}).store = store;
    await settle(host);
    const filters = deepAll(host, 'tessera-filter');
    expect(filters.map((f) => f.getAttribute('column'))).toEqual(['archive', 'title', 'submitted_at']);
    // One chip, naming the column and the chosen key.
    const chips = deepAll(host, '[part="chip"]');
    expect(chips).toHaveLength(1);
    expect(chips[0]!.textContent).toContain('archive');
    expect(chips[0]!.textContent).toContain('cs');
    expect(deepAll(host, '[part="verb"]').map((v) => v.getAttribute('data-verb'))).toEqual(['filter']);
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
