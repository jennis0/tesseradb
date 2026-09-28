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
    expect(sent(store)!['field']).toEqual({family: 'category', keys: ['cs.LG'], verb: 'filter'});
    await answer(host, store);
    press(host, 'cs.CV', 'filter');
    expect(sent(store)!['field']).toEqual({family: 'category', keys: ['cs.LG', 'cs.CV'], verb: 'filter'});
    await answer(host, store);

    // The rows in the filter are pressed; the rest are out of it.
    expect(['cs.LG', 'cs.CV', 'hep-th'].map((k) => entry(host, k).getAttribute('data-state'))).toEqual(['filtered', 'filtered', 'out']);
    expect(entry(host, 'cs.LG').querySelector('[part="filter"]')!.getAttribute('aria-pressed')).toBe('true');

    press(host, 'cs.LG', 'filter');
    expect(sent(store)!['field']).toEqual({family: 'category', keys: ['cs.CV'], verb: 'filter'});
    expect(changes).toEqual([
      {column: 'field', expr: {field: {in: ['cs.LG']}}},
      {column: 'field', expr: {field: {in: ['cs.LG', 'cs.CV']}}},
      {column: 'field', expr: {field: {in: ['cs.CV']}}}
    ]);
  });

  it('highlights a value without filtering, and greys the other rows', async () => {
    const {host, store} = await mountLegend('field');
    press(host, 'cs.CV', 'highlight');
    expect(sent(store)!['field']).toEqual({family: 'category', keys: ['cs.CV'], verb: 'highlight'});
    await answer(host, store);
    expect(['cs.LG', 'cs.CV', 'hep-th'].map((k) => entry(host, k).getAttribute('data-state'))).toEqual(['dim', 'lit', 'dim']);
    expect(entry(host, 'cs.CV').querySelector('[part="highlight"]')!.getAttribute('aria-pressed')).toBe('true');
  });

  it('moves the column’s clause to the other verb with the value pressed alone', async () => {
    const {host, store} = await mountLegend('field');
    press(host, 'cs.LG', 'filter');
    await answer(host, store);
    press(host, 'cs.CV', 'filter');
    await answer(host, store);
    press(host, 'hep-th', 'highlight');
    expect(sent(store)!['field']).toEqual({family: 'category', keys: ['hep-th'], verb: 'highlight'});
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
  it('sets the column’s range filter from its handles, and follows the filter it is given', async () => {
    const {host, store} = await mountLegend('citations');
    expect(deep(host, '[part="range"]')).toBeNull();
    const low = deep(host, '[part="range-low"]') as HTMLElement;
    low.dispatchEvent(new KeyboardEvent('keydown', {key: 'ArrowRight', shiftKey: true, bubbles: true}));
    expect(sent(store)!['citations']).toEqual({family: 'numeric', gte: 100, lte: 1000, verb: 'filter'});

    // The chip moved elsewhere: the range follows it.
    store.set('filters', filtersOf({...emptyDraft(META.filterOperands), citations: {family: 'numeric', gte: 250, lte: 500, verb: 'filter'}}));
    await settle(host);
    expect(deep(host, '[part="range-low"]')!.getAttribute('aria-valuenow')).toBe('250');
    expect(deep(host, '[part="range-high"]')!.getAttribute('aria-valuenow')).toBe('500');
    expect(deepAll(host, '[part="range-value"]').map((v) => v.textContent)).toEqual(['250', '500']);
    expect(deep(host, '[part="range"]')).not.toBeNull();
  });
});
