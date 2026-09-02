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
  views: [{id: 's0', displayName: 'default', quantisation: {xMin: 0, xMax: 1, yMin: 0, yMax: 1}, projection: 'none', worldAspect: null, tileScheme: null, tile: null, roster: null}],
  groups: [],
  declaredScalars: [
    {name: 'archive', arrowType: 'u16', category: {vocabulary: 'a', kind: 'declared', visibility: 'public'}, render: true, index: true},
    {name: 'submitted_at', arrowType: 'timestamp_us', category: null, render: true, index: true},
    {name: 'title', arrowType: 'utf8', category: null, render: false, index: true}
  ],
  layers: [],
  selection: {kMin: 1, kMaxMarks: 500, maxK: 5000, thetaTargetMarks: 10, maxUnderlayOffset: 0, maxCategoryValues: 1000, maxRegionVertices: 10_000, maxRegionCells: 262_144, maxBrowseRows: 200},
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
    el.item = {id: 12345678901234567890n, detail: {fields: {note: 'x', title: 'A title', submitted_at: 1_700_000_000_000_000}, externalId: null, labels: [], views: [], scoped: {}}};
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
    store.set('filters', {draft: {archive: {family: 'category', keys: [], verb: 'filter'}}, expr: null, highlight: null, members: [], suggestions: {}, suggestErrors: {}});
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
    // `more: true` — the lookahead shape, so the control keeps its search box (round 2, §5.1: a
    // `more: false` empty-`q` page renders a checklist instead, covered below).
    store.set('filters', {
      draft: {archive: {family: 'category', keys: [], verb: 'filter'}},
      expr: null,
      highlight: null,
      members: [],
      suggestions: {archive: {q: '', values: [{code: 1, key: 'cs', title: 'Computer Science', match: {field: 'key', start: 0, len: 0}}], more: true}},
      suggestErrors: {}
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
    expect(host.textContent + [...deepAll(host, '*')].map((e) => e.textContent).join(' ')).not.toMatch(/no such value/i);
    // The typed key is submitted and shows up as a chip, chosen, unresolved.
    expect(deep(host, '[part="value-chip"]')?.textContent).toContain('zz.unlisted');
  });

  it('renders a checklist, not a search box, when the empty-q page says more: false', async () => {
    const host = await mount('<tessera-filter column="archive"></tessera-filter>');
    const el = host.querySelector('tessera-filter') as TesseraFilter;
    const store = fakeStore({meta: META, status: status({})});
    // Nine values on one page and nothing left to page through — a small, public column
    // (`value-suggestion.md` §5.1 round 2): the whole visible set fits, so a checkbox per value
    // rather than a typeahead.
    const values = Array.from({length: 9}, (_, i) => ({code: i + 1, key: `v${i}`, title: `Value ${i}`, match: {field: 'key' as const, start: 0, len: 0}}));
    store.set('filters', {
      draft: {archive: {family: 'category', keys: ['v2'], verb: 'filter'}},
      expr: null,
      highlight: null,
      members: [],
      suggestions: {archive: {q: '', values, more: false}},
      suggestErrors: {}
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
    store.set('filters', {draft: {archive: {family: 'category', keys: [], verb: 'filter'}}, expr: null, highlight: null, members: [], suggestions: {archive: {q: '', values: [], more: true}}, suggestErrors: {}});
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

  it('re-decides the shape once the store invalidates the column’s suggestion — a view change or a re-authorise', async () => {
    const host = await mount('<tessera-filter column="archive"></tessera-filter>');
    const el = host.querySelector('tessera-filter') as TesseraFilter;
    const store = fakeStore({meta: META, status: status({})});
    const values = [{code: 1, key: 'v0', title: 'Value 0', match: {field: 'key' as const, start: 0, len: 0}}];
    store.set('filters', {draft: {archive: {family: 'category', keys: [], verb: 'filter'}}, expr: null, highlight: null, members: [], suggestions: {archive: {q: '', values, more: false}}, suggestErrors: {}});
    el.store = store;
    await settle(host);
    expect(deep(host, '[part="entry"]')).toBeNull(); // checklist

    // The store clears the column's page and raises no refusal for it — `resetSuggestions`'s own
    // shape on a view switch or a re-authorise, distinct from a fetch failure.
    store.set('filters', {...store.get('filters'), suggestions: {}, suggestErrors: {}});
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

  it('asks the store on every keystroke, and renders only the page that answers the box in front of it', async () => {
    const host = await mount('<tessera-filter column="archive"></tessera-filter>');
    const el = host.querySelector('tessera-filter') as TesseraFilter;
    const store = fakeStore({meta: META, status: status({})});
    store.set('filters', {draft: {archive: {family: 'category', keys: [], verb: 'filter'}}, expr: null, highlight: null, members: [], suggestions: {}, suggestErrors: {}});
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
    expect(deep(host, '[part="more"]')?.textContent).toBe('type more to narrow');
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
    store.set('filters', {draft: {archive: {family: 'category', keys: [], verb: 'filter'}}, expr: null, highlight: null, members: [], suggestions: {}, suggestErrors: {archive: {code: 'derived', detail: 'not listable'}}});
    el.store = store;
    await settle(host);
    expect(deep(host, '[part="entry"]')).not.toBeNull();
    expect(deep(host, '[part="refusal"]')?.textContent).toContain('derived');
  });

  it('a text operand offers phrase only when the column publishes it, and debounces typing', async () => {
    const host = await mount('<tessera-filter column="title"></tessera-filter>');
    const el = host.querySelector('tessera-filter') as TesseraFilter;
    const store = fakeStore({meta: META, status: status({})});
    store.set('filters', {draft: {title: {family: 'text', query: '', mode: 'all', verb: 'filter'}}, expr: null, highlight: null, members: [], suggestions: {}, suggestErrors: {}});
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
  /**
   * §5.2's two verbs on the chip: the word says which of the request's two expressions the clause
   * joins, and clicking it moves the clause **without the predicate being re-entered** — which is
   * exactly what is asserted, the draft the panel sends back carrying the same keys.
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
      suggestErrors: {}
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
      suggestErrors: {}
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
    store.set('filters', {draft: {archive: {family: 'category', keys: ['cs'], verb: 'filter'}, title: {family: 'text', query: '', mode: 'all', verb: 'filter'}, submitted_at: {family: 'numeric', gte: null, lte: null, verb: 'filter'}}, expr: null, highlight: null, members: [], suggestions: {}, suggestErrors: {}});
    (host.querySelector('tessera-filter-panel') as unknown as {store: unknown}).store = store;
    await settle(host);
    const filters = deepAll(host, 'tessera-filter');
    expect(filters.map((f) => f.getAttribute('column'))).toEqual(['archive', 'title', 'submitted_at']);
    // The chip's own text, past the verb toggle it now carries.
    expect(deepAll(host, '[part="chip"]').map((c) => c.textContent?.replace(/\s+/g, ' ').trim())).toEqual(['filter archive: cs']);
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
