import {afterEach, describe, expect, it} from 'vitest';
import {emptyDraft, type FilterDraft, type LegendProjection} from '@tesseradb/client';
import {CATEGORY_PALETTES} from '@tesseradb/deck';
import {hexOf} from '@tesseradb/deck/internal';
import '../src/legend.js';
import '../src/map.js';
import {colouringOf} from '../src/colouring.js';
import {deep, deepAll, fakeStore, meta, mount, scalar, settle, status, type FakeStore} from './fake-store.js';

/**
 * The legend as a filter and a highlight, and as the place colours are chosen. What it asks of the
 * store is read off the fake store's calls, what it reports off its events, and what it shows off
 * its parts.
 */

afterEach(() => {
  document.body.innerHTML = '';
});

const META = meta({
  declaredScalars: [
    scalar('field', 'u16', {category: {vocabulary: 'f', kind: 'declared', visibility: 'public'}, render: true, homes: ['rendered']}),
    scalar('citations', 'u32', {render: true, homes: ['rendered']}),
    scalar('venue', 'u16', {category: {vocabulary: 'v', kind: 'declared', visibility: 'public'}, render: true, homes: ['rendered']})
  ],
  filterOperands: [
    {column: 'field', family: 'category', operands: ['in']},
    {column: 'citations', family: 'numeric', operands: ['range']}
  ]
});

const VALUES = [
  {code: 1, key: 'cs.LG', title: 'Machine Learning'},
  {code: 2, key: 'cs.CV', title: 'Computer Vision'},
  {code: 3, key: 'hep-th', title: 'High Energy Physics'}
];

function legend(colourBy: string, over: Partial<LegendProjection> = {}): LegendProjection {
  return {
    ranks: {field: {1: 0, 2: 1, 3: 2}, venue: {1: 0}},
    domains: {citations: {min: 0, max: 1000}},
    categories: {field: VALUES, venue: [{code: 1, key: 'neurips', title: 'NeurIPS'}]},
    categoryErrors: {},
    colourBy,
    ...over
  };
}

const filtersOf = (draft: FilterDraft) => ({draft, expr: null, highlight: null, members: [], suggestions: {}, suggestErrors: {}, suggestEpoch: 0});

async function mountLegend(colourBy: string, markup = '<tessera-legend></tessera-legend>', over: Partial<LegendProjection> = {}) {
  const host = await mount(markup);
  const el = host.querySelector('tessera-legend') as HTMLElement & {store: unknown};
  const store = fakeStore({meta: META, status: status({}), legend: legend(colourBy, over), filters: filtersOf(emptyDraft(META.filterOperands))});
  el.store = store;
  await settle(host);
  return {host, el, store};
}

/** The draft the legend last sent. */
const sent = (store: FakeStore) => store.calls.filter((c) => c.name === 'setFilters').at(-1)?.args[0] as FilterDraft | undefined;

/** Hand the store's answer back, as the real store publishes the draft it was sent. */
async function answer(host: HTMLElement, store: FakeStore): Promise<void> {
  store.set('filters', filtersOf(sent(store)!));
  await settle(host);
}

const entry = (host: HTMLElement, key: string) => deep(host, `[part~="entry"][data-key="${key}"]`)!;
const press = (host: HTMLElement, key: string, verb: 'filter' | 'highlight') => (entry(host, key).querySelector(`[part="${verb}"]`) as HTMLButtonElement).click();

describe('<tessera-legend> rows as a filter and a highlight', () => {
  it('adds each value filtered to the column’s filter, and takes it out when pressed again', async () => {
    const {host, store} = await mountLegend('field');
    const changes: unknown[] = [];
    host.addEventListener('tessera-filterchange', (e) => changes.push((e as CustomEvent).detail));

    press(host, 'cs.LG', 'filter');
    expect(sent(store)!.filter['field']).toEqual({family: 'category', keys: ['cs.LG']});
    await answer(host, store);
    press(host, 'cs.CV', 'filter');
    expect(sent(store)!.filter['field']).toEqual({family: 'category', keys: ['cs.LG', 'cs.CV']});
    await answer(host, store);

    // The rows in the filter are pressed; the rest are out of it.
    expect(['cs.LG', 'cs.CV', 'hep-th'].map((k) => entry(host, k).getAttribute('data-state'))).toEqual(['filtered', 'filtered', 'out']);
    expect(entry(host, 'cs.LG').querySelector('[part="filter"]')!.getAttribute('aria-pressed')).toBe('true');

    press(host, 'cs.LG', 'filter');
    expect(sent(store)!.filter['field']).toEqual({family: 'category', keys: ['cs.CV']});
    expect(changes).toEqual([
      {column: 'field', verb: 'filter', expr: {field: {in: ['cs.LG']}}},
      {column: 'field', verb: 'filter', expr: {field: {in: ['cs.LG', 'cs.CV']}}},
      {column: 'field', verb: 'filter', expr: {field: {in: ['cs.CV']}}}
    ]);
  });

  it('highlights a value without filtering, and greys the other rows', async () => {
    const {host, store} = await mountLegend('field');
    const changes: unknown[] = [];
    host.addEventListener('tessera-filterchange', (e) => changes.push((e as CustomEvent).detail));
    press(host, 'cs.CV', 'highlight');
    expect(changes).toEqual([{column: 'field', verb: 'highlight', expr: {field: {in: ['cs.CV']}}}]);
    expect(sent(store)!.highlight['field']).toEqual({family: 'category', keys: ['cs.CV']});
    await answer(host, store);
    expect(['cs.LG', 'cs.CV', 'hep-th'].map((k) => entry(host, k).getAttribute('data-state'))).toEqual(['dim', 'lit', 'dim']);
    expect(entry(host, 'cs.CV').querySelector('[part="highlight"]')!.getAttribute('aria-pressed')).toBe('true');
  });

  it('holds a filter and a highlight on the column at once, each pressed on its own', async () => {
    const {host, store} = await mountLegend('field');
    press(host, 'cs.LG', 'filter');
    await answer(host, store);
    press(host, 'cs.CV', 'filter');
    await answer(host, store);
    press(host, 'cs.CV', 'highlight');
    expect(sent(store)!.filter['field']).toEqual({family: 'category', keys: ['cs.LG', 'cs.CV']});
    expect(sent(store)!.highlight['field']).toEqual({family: 'category', keys: ['cs.CV']});
    await answer(host, store);

    // Of the two filtered, the one highlighted is lit and the other dim; the rest are out.
    expect(['cs.LG', 'cs.CV', 'hep-th'].map((k) => entry(host, k).getAttribute('data-state'))).toEqual(['dim', 'lit', 'out']);
    const pressed = (key: string, verb: 'filter' | 'highlight') => entry(host, key).querySelector(`[part="${verb}"]`)!.getAttribute('aria-pressed');
    expect([pressed('cs.CV', 'filter'), pressed('cs.CV', 'highlight'), pressed('cs.LG', 'filter'), pressed('cs.LG', 'highlight')]).toEqual(['true', 'true', 'true', 'false']);

    // Taking the value out of the filter leaves the highlight.
    press(host, 'cs.CV', 'filter');
    expect(sent(store)!.filter['field']).toEqual({family: 'category', keys: ['cs.LG']});
    expect(sent(store)!.highlight['field']).toEqual({family: 'category', keys: ['cs.CV']});
  });

  it('offers the verbs from the published operands, with no filter control seeded', async () => {
    const {host, store} = await mountLegend('field');
    store.set('filters', filtersOf({filter: {}, highlight: {field: {family: 'category', keys: ['cs.CV']}}}));
    await settle(host);
    expect(entry(host, 'cs.CV').querySelector('[part="highlight"]')!.getAttribute('aria-pressed')).toBe('true');
    press(host, 'cs.LG', 'filter');
    expect(sent(store)!.filter['field']).toEqual({family: 'category', keys: ['cs.LG']});
    expect(sent(store)!.highlight['field']).toEqual({family: 'category', keys: ['cs.CV']});
  });

  it('offers no verbs on a column that cannot be filtered by value, and none on Other', async () => {
    const {host} = await mountLegend('venue');
    expect(deep(host, '[part="filter"]')).toBeNull();
    expect(deep(host, '[part="highlight"]')).toBeNull();
    const field = await mountLegend('field');
    expect(deepAll(field.host, '[part="filter"]')).toHaveLength(3);
  });

  it('shows a count only where the store holds an exact one, never from the marks', async () => {
    const without = await mountLegend('field');
    expect(deep(without.host, '[part="count"]')).toBeNull();
    const counts = {field: {'cs.LG': {value: 4_812_300, exact: true}, 'cs.CV': {value: 3_905_110, exact: true}}};
    const {host} = await mountLegend('field', undefined, {counts});
    expect(entry(host, 'cs.LG').querySelector('[part="count"]')!.textContent).toBe('4,812,300');
    expect(entry(host, 'hep-th').querySelector('[part="count"]')!.textContent).toBe('');
  });
});

describe('<tessera-legend> colours', () => {
  it('gives a value past the palette a row of its own once it has a colour, and names Other only while some value falls past', async () => {
    const many = Array.from({length: 12}, (_, i) => ({code: i + 1, key: `k${i}`, title: `Value ${i}`}));
    const ranks = Object.fromEntries(many.map((v, i) => [v.code, i]));
    const {host, store} = await mountLegend('field', undefined, {categories: {field: many}, ranks: {field: ranks}});
    const names = () => deepAll(host, '[part="name"]').map((n) => n.textContent);
    // Tableau 10 holds ten; the eleventh and twelfth share "Other".
    expect(names()).toEqual([...many.slice(0, 10).map((v) => v.title), 'Other']);
    const {setColouring} = await import('../src/colouring.js');
    setColouring(store, {values: {field: {k11: '#123456'}}});
    await settle(host);
    expect(names()).toEqual([...many.slice(0, 10).map((v) => v.title), 'Value 11', 'Other']);
    setColouring(store, {values: {field: {k10: '#654321', k11: '#123456'}}});
    await settle(host);
    expect(names()).toEqual([...many.slice(0, 10).map((v) => v.title), 'Value 10', 'Value 11']);
  });

  it('names each palette colour by its palette and place, and closes when focus leaves it', async () => {
    const {host} = await mountLegend('field');
    (entry(host, 'cs.LG').querySelector('[part="swatch"]') as HTMLButtonElement).click();
    await settle(host);
    const choices = deepAll(host, '[part="choice"]');
    const second = hexOf(CATEGORY_PALETTES.tableau10.colours[1]!);
    expect(choices[1]!.getAttribute('aria-label')).toBe(`Tableau 10, colour 2 of 10, ${second}`);
    expect(choices[11]!.getAttribute('aria-label')).toMatch(/^Tableau 10, lighter colour 2 of 10, #/);
    // Focus moving within the picker keeps it open; moving out of it, as Tab past its end does, closes it.
    const popover = deep(host, '[part="colour-popover"]')!;
    popover.dispatchEvent(new FocusEvent('focusout', {relatedTarget: choices[2]!, bubbles: true}));
    await settle(host);
    expect(deep(host, '[part="colour-popover"]')).not.toBeNull();
    popover.dispatchEvent(new FocusEvent('focusout', {relatedTarget: document.body, bubbles: true}));
    await settle(host);
    expect(deep(host, '[part="colour-popover"]')).toBeNull();
  });

  it('closes the colour picker when Colour by is opened', async () => {
    const {host} = await mountLegend('field', '<tessera-legend selectable readout></tessera-legend>');
    (entry(host, 'cs.LG').querySelector('[part="swatch"]') as HTMLButtonElement).click();
    await settle(host);
    expect(deep(host, '[part="colour-popover"]')).not.toBeNull();
    (deep(host, '[part="colour-by"]') as HTMLButtonElement).click();
    await settle(host);
    expect(deep(host, '[part="colour-popover"]')).toBeNull();
    expect(deep(host, '[part="colour-menu"]')).not.toBeNull();
  });

  it('applies a colour chosen in the picker at once and reports it; Reset gives the palette colour back', async () => {
    const {host, store} = await mountLegend('field');
    const picked: unknown[] = [];
    host.addEventListener('tessera-valuecolour', (e) => picked.push((e as CustomEvent).detail));
    (entry(host, 'cs.CV').querySelector('[part="swatch"]') as HTMLButtonElement).click();
    await settle(host);
    const popover = deep(host, '[part="colour-popover"]')!;
    expect(popover.getAttribute('role')).toBe('dialog');
    const choices = [...popover.querySelectorAll('[part="choice"]')];
    // The palette's colours and a lighter row.
    expect(choices).toHaveLength(2 * CATEGORY_PALETTES.tableau10.colours.length);
    (choices[4] as HTMLButtonElement).click();
    await settle(host);
    const chosen = hexOf(CATEGORY_PALETTES.tableau10.colours[4]!);
    expect(colouringOf(store).values).toEqual({field: {'cs.CV': chosen}});
    expect((entry(host, 'cs.CV').querySelector('[part="swatch"]') as HTMLElement).style.getPropertyValue('--c')).toBe(chosen);

    (deep(host, '[part="reset"]') as HTMLButtonElement).click();
    await settle(host);
    expect(colouringOf(store).values).toEqual({});
    expect(picked).toEqual([
      {column: 'field', value: 'cs.CV', colour: chosen},
      {column: 'field', value: 'cs.CV', colour: null}
    ]);
  });

  it('takes a colour from its hex field and from the arrow keys on its hue bar, and ignores a hex that is not one', async () => {
    const {host, store} = await mountLegend('field');
    (entry(host, 'cs.LG').querySelector('[part="swatch"]') as HTMLButtonElement).click();
    await settle(host);
    const hex = deep(host, '[part="hex"]') as HTMLInputElement;
    hex.value = '#12AB34';
    hex.dispatchEvent(new Event('change'));
    await settle(host);
    expect(colouringOf(store).values['field']?.['cs.LG']).toBe('#12ab34');
    hex.value = 'green';
    hex.dispatchEvent(new Event('change'));
    await settle(host);
    expect(colouringOf(store).values['field']?.['cs.LG']).toBe('#12ab34');
    const hue = deep(host, '[part="hue"]') as HTMLElement;
    const before = Number(hue.getAttribute('aria-valuenow'));
    hue.dispatchEvent(new KeyboardEvent('keydown', {key: 'ArrowRight', shiftKey: true, bubbles: true}));
    await settle(host);
    expect(Number(deep(host, '[part="hue"]')!.getAttribute('aria-valuenow'))).toBe(before + 10);
    expect(colouringOf(store).values['field']?.['cs.LG']).not.toBe('#12ab34');
  });

  it('closes the picker on Escape and puts focus back on the swatch', async () => {
    const {host} = await mountLegend('field');
    const swatch = entry(host, 'cs.LG').querySelector('[part="swatch"]') as HTMLButtonElement;
    swatch.click();
    await settle(host);
    const popover = deep(host, '[part="colour-popover"]')!;
    popover.dispatchEvent(new KeyboardEvent('keydown', {key: 'Escape', bubbles: true, composed: true}));
    await settle(host);
    expect(deep(host, '[part="colour-popover"]')).toBeNull();
    expect((swatch.getRootNode() as ShadowRoot).activeElement).toBe(swatch);
  });

  it('draws the value colours a host sets on the map, as it restores a saved choice', async () => {
    const host = await mount('<div><tessera-map></tessera-map><tessera-legend></tessera-legend></div>');
    const store = fakeStore({meta: META, status: status({}), legend: legend('field'), filters: filtersOf(emptyDraft(META.filterOperands))});
    const map = host.querySelector('tessera-map') as HTMLElement & {store: unknown; valueColours: unknown};
    const el = host.querySelector('tessera-legend') as HTMLElement & {store: unknown};
    map.store = store;
    el.store = store;
    map.valueColours = {field: {'hep-th': '#010203'}};
    await settle(host);
    expect(colouringOf(store).values).toEqual({field: {'hep-th': '#010203'}});
    expect((entry(host, 'hep-th').querySelector('[part="swatch"]') as HTMLElement).style.getPropertyValue('--c')).toBe('#010203');
  });

  it('offers the palettes for a category column and colours the rows from the one chosen', async () => {
    const {host, store} = await mountLegend('field', '<tessera-legend selectable readout></tessera-legend>');
    const chosen: unknown[] = [];
    host.addEventListener('tessera-palettechange', (e) => chosen.push((e as CustomEvent).detail));
    (deep(host, '[part="colour-by"]') as HTMLButtonElement).click();
    await settle(host);
    expect(deepAll(host, '[part="palette"]').map((p) => p.getAttribute('data-palette'))).toEqual(['tableau10', 'okabe-ito', 'set2', 'dark2']);
    expect(deep(host, '[part="palette"][aria-checked="true"]')!.getAttribute('data-palette')).toBe('tableau10');
    expect(deep(host, '[part="ramp-option"]')).toBeNull();
    (deep(host, '[part="palette"][data-palette="okabe-ito"]') as HTMLButtonElement).click();
    await settle(host);
    expect(colouringOf(store).palette).toBe('okabe-ito');
    expect(chosen).toEqual([{palette: 'okabe-ito', ramp: 'viridis', scale: 'linear', reverse: false}]);
    const first = (entry(host, 'cs.LG').querySelector('[part="swatch"]') as HTMLElement).style.getPropertyValue('--c');
    const [r, g, b] = CATEGORY_PALETTES['okabe-ito'].colours[0]!;
    expect(first).toBe(`rgb(${r}, ${g}, ${b})`);
  });

  it('offers the ramps, the scale and reversal for a number column', async () => {
    const {host, store} = await mountLegend('citations', '<tessera-legend selectable readout></tessera-legend>');
    (deep(host, '[part="colour-by"]') as HTMLButtonElement).click();
    await settle(host);
    expect(deep(host, '[part="palette"]')).toBeNull();
    expect(deepAll(host, '[part="ramp-option"]').map((p) => p.getAttribute('data-ramp'))).toEqual(['viridis', 'cividis', 'magma', 'greys', 'red-blue']);
    (deep(host, '[part="ramp-option"][data-ramp="magma"]') as HTMLButtonElement).click();
    (deep(host, '[part="scale"] [data-scale="log"]') as HTMLButtonElement).click();
    (deep(host, '[part="reverse"]') as HTMLButtonElement).click();
    await settle(host);
    const {ramp, scale, reverse} = colouringOf(store);
    expect({ramp, scale, reverse}).toEqual({ramp: 'magma', scale: 'log', reverse: true});
    expect(deep(host, '[part="reverse"]')!.getAttribute('aria-checked')).toBe('true');
  });

  it('leaves the palettes and ramps out under hide-palettes', async () => {
    const {host} = await mountLegend('field', '<tessera-legend selectable readout hide-palettes></tessera-legend>');
    (deep(host, '[part="colour-by"]') as HTMLButtonElement).click();
    await settle(host);
    expect(deepAll(host, '[part="option"]').length).toBeGreaterThan(0);
    expect(deep(host, '[part="palette"]')).toBeNull();
  });
});

describe('<tessera-legend> a number’s range', () => {
  /** The draft the store holds for citations, as the filter panel's chip reads it. */
  const withRange = (store: FakeStore, gte: number | null, lte: number | null) =>
    store.set('filters', filtersOf({filter: {...emptyDraft(META.filterOperands).filter, citations: {family: 'numeric', gte, lte}}, highlight: {}}));

  it('leaves an end open that is not moved off the ramp’s edge, since the ramp spans only the values drawn', async () => {
    const {host, store} = await mountLegend('citations');
    expect(deep(host, '[part="range"]')).toBeNull();
    const low = deep(host, '[part="range-low"]') as HTMLElement;
    low.dispatchEvent(new KeyboardEvent('keydown', {key: 'ArrowRight', shiftKey: true, bubbles: true}));
    expect(sent(store)!.filter['citations']).toEqual({family: 'numeric', gte: 100, lte: null});
    await answer(host, store);
    // Home takes the low end back to the edge, which opens it.
    (deep(host, '[part="range-low"]') as HTMLElement).dispatchEvent(new KeyboardEvent('keydown', {key: 'Home', bubbles: true}));
    expect(sent(store)!.filter['citations']).toEqual({family: 'numeric', gte: null, lte: null});
  });

  it('follows the filter it is given, and shows a bound beyond the values drawn as its own value', async () => {
    const {host, store} = await mountLegend('citations');
    withRange(store, 250, 500);
    await settle(host);
    expect(deep(host, '[part="range-low"]')!.getAttribute('aria-valuenow')).toBe('250');
    expect(deep(host, '[part="range-high"]')!.getAttribute('aria-valuenow')).toBe('500');
    expect(deepAll(host, '[part="range-value"]').map((v) => v.textContent)).toEqual(['250', '500']);

    // The domain drawn is 0 to 1,000; the filter reaches past it on both sides.
    withRange(store, -50, 5000);
    await settle(host);
    const low = deep(host, '[part="range-low"]') as HTMLElement;
    const high = deep(host, '[part="range-high"]') as HTMLElement;
    expect([low.getAttribute('aria-valuenow'), high.getAttribute('aria-valuenow')]).toEqual(['-50', '5000']);
    expect([low.style.left, high.style.left]).toEqual(['0.00%', '100.00%']);
    expect(deepAll(host, '[part="range-value"]').map((v) => v.textContent)).toEqual(['-50', '5,000']);
    // Moving one end keeps the other end's own bound.
    high.dispatchEvent(new KeyboardEvent('keydown', {key: 'ArrowLeft', shiftKey: true, bubbles: true}));
    expect(sent(store)!.filter['citations']).toEqual({family: 'numeric', gte: -50, lte: 900});
  });

  it('sets a range from a drag across the ramp, open where the drag reaches its end', async () => {
    const {host, store} = await mountLegend('citations');
    const track = deep(host, '.track') as HTMLElement;
    track.getBoundingClientRect = () => ({left: 0, top: 0, right: 200, bottom: 22, width: 200, height: 22, x: 0, y: 0, toJSON: () => ({})});
    const pointer = (type: string, clientX: number) => track.dispatchEvent(new PointerEvent(type, {clientX, button: 0, pointerId: 1, bubbles: true}));
    pointer('pointerdown', 50);
    pointer('pointermove', 150);
    pointer('pointerup', 150);
    expect(sent(store)!.filter['citations']).toEqual({family: 'numeric', gte: 250, lte: 750});
    pointer('pointerdown', 100);
    pointer('pointerup', 260);
    expect(sent(store)!.filter['citations']).toEqual({family: 'numeric', gte: 500, lte: null});
    // A press without a drag changes nothing.
    const before = store.calls.length;
    pointer('pointerdown', 120);
    pointer('pointerup', 120);
    expect(store.calls.length).toBe(before);
  });
});

describe('the colour choices across a change of viewer', () => {
  it('forget the value colours when the store forgets what the server answered, and keep the palette', async () => {
    const {host, store} = await mountLegend('field');
    (entry(host, 'cs.CV').querySelector('[part="swatch"]') as HTMLButtonElement).click();
    await settle(host);
    (deep(host, '[part="choice"]') as HTMLButtonElement).click();
    const {setColouring} = await import('../src/colouring.js');
    setColouring(store, {palette: 'dark2'});
    expect(Object.keys(colouringOf(store).values)).toEqual(['field']);

    // `clear()`: meta goes to null, then the next viewer's answer arrives.
    store.set('meta', null);
    store.set('meta', META);
    store.set('legend', legend('field'));
    await settle(host);
    expect(colouringOf(store).values).toEqual({});
    expect(colouringOf(store).palette).toBe('dark2');
    const [r, g, b] = CATEGORY_PALETTES.dark2.colours[1]!;
    expect((entry(host, 'cs.CV').querySelector('[part="swatch"]') as HTMLElement).style.getPropertyValue('--c')).toBe(`rgb(${r}, ${g}, ${b})`);
    const {encodingOf} = await import('@tesseradb/deck/internal');
    const encoding = encodingOf(META, legend('field'), colouringOf(store));
    expect(encoding.kind === 'category' && encoding.chosen.size).toBe(0);
  });
});
