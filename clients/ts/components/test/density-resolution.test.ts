import {afterEach, beforeEach, describe, expect, it, vi} from 'vitest';
import type {AggregateSpec} from '@tesseradb/client';
import {DENSITY_SETTLE_MS, cellDepth} from '@tesseradb/deck';
import '../src/explorer.js';
import type {TesseraMap} from '../src/map.js';
import {fakeStore, meta, mount, registered, settle, status, type FakeStore} from './fake-store.js';
import {SELECTION} from '../../core/test/support.js';

/**
 * Density's resolution through the elements: the map asks for the depth the chosen cell size
 * gives at its camera once the camera rests, and the explorer's Resolution slider sets that size,
 * offers only the sizes the server will count, and reports what it set.
 */

/** A map 1000 × 800 px wide, fitted, so the whole 512-unit world is 800 px across at zoom log2(800 / 512). */
function sized(map: TesseraMap): number {
  Object.defineProperty(map, 'clientWidth', {configurable: true, value: 1000});
  Object.defineProperty(map, 'clientHeight', {configurable: true, value: 800});
  map.fit();
  return map.zoom;
}

/** The cell depths density has asked for, in order. */
const depthsAsked = (store: FakeStore) =>
  store.calls.filter((c) => c.name === 'setAggregate' && String(c.args[0]).startsWith('density#') && c.args[1] !== null).map((c) => (c.args[1] as AggregateSpec).groupings[0]!.cells!.depth);

describe('density resolution', () => {
  beforeEach(() => vi.useFakeTimers({toFake: ['setTimeout', 'clearTimeout']}));
  afterEach(() => vi.useRealTimers());

  it('asks the map for the depth nearest its cell size once the camera has rested, and again when the size changes', async () => {
    const host = await mount('<tessera-map density="grid" density-resolution="8"></tessera-map>');
    const map = host.querySelector('tessera-map') as TesseraMap;
    const store = fakeStore({meta: meta(), status: status({})});
    map.store = store;
    await settle(host);
    expect(map.densityResolution).toBe(8);
    const zoom = sized(map);
    expect(depthsAsked(store)).toEqual([]);
    vi.advanceTimersByTime(DENSITY_SETTLE_MS);
    expect(depthsAsked(store)).toEqual([cellDepth(zoom, 8)]);
    map.densityResolution = 32;
    await settle(host);
    vi.advanceTimersByTime(DENSITY_SETTLE_MS);
    expect(depthsAsked(store)).toEqual([cellDepth(zoom, 8), cellDepth(zoom, 32)]);
    // None drops the registration.
    map.density = 'none';
    await settle(host);
    expect([...registered(store).keys()].filter((id) => id.startsWith('density#'))).toEqual([]);
  });

  it('sets the cell size from the Resolution slider, stops at the finest the server counts, and reports it', async () => {
    const host = await mount('<tessera-explorer density="grid"></tessera-explorer>');
    const el = host.querySelector('tessera-explorer') as HTMLElement & {store: unknown; densityResolution: number};
    // The whole world at depth 5 is 1,024 cells and at depth 6 4,096: a limit of 2,000 admits
    // the 32 and 24 px stops, which ask for depth 5 at this zoom, and none finer.
    const store = fakeStore({meta: meta({selection: {...SELECTION, maxAggregateCells: 2000}}), status: status({})});
    el.store = store;
    await settle(host);
    const shadow = el.shadowRoot!;
    const map = shadow.querySelector('tessera-map') as TesseraMap;
    sized(map);
    shadow.querySelector<HTMLButtonElement>('[part="layers-toggle"]')!.click();
    await settle(host);
    expect(map.densityStops().map((s) => [s.px, s.enabled])).toEqual([
      [32, true],
      [24, true],
      [16, false],
      [12, false],
      [8, false],
      [6, false],
      [4, false]
    ]);

    const seen: {densityResolution: number}[] = [];
    host.addEventListener('tessera-displaychange', (e) => seen.push((e as CustomEvent).detail));
    const slider = () => shadow.querySelector<HTMLInputElement>('[part="density-resolution"]')!;
    const readout = () => [...shadow.querySelectorAll('.readout')].map((r) => r.textContent).find((t) => t?.startsWith('cells'));
    // At this zoom the seven sizes ask for four depths, so the slider has four stops.
    expect(new Set(map.densityStops().map((s) => s.depth)).size).toBe(4);
    expect(slider().max).toBe('3');
    // The default, 12 px, asks for a depth past the limit, so the slider shows the finest it can
    // count, and the readout gives the size of that depth's cells on screen.
    expect(slider().value).toBe('0');
    const depth = map.densityStops()[0]!.depth;
    expect(readout()).toBe(`cells ≈ ${Math.round((512 * 2 ** map.zoom) / 2 ** depth)} px`);
    slider().value = '0';
    slider().dispatchEvent(new Event('input'));
    await settle(host);
    expect(el.densityResolution).toBe(32);
    expect(map.densityResolution).toBe(32);
    // A stop past the limit is kept as asked, and the slider shows the finest the map can draw.
    slider().value = '3';
    slider().dispatchEvent(new Event('input'));
    await settle(host);
    expect(slider().value).toBe('0');
    expect(map.densityResolution).toBe(4);
    expect(seen.map((d) => d.densityResolution)).toEqual([32, 4]);

    vi.advanceTimersByTime(DENSITY_SETTLE_MS);
    expect(depthsAsked(store).every((d) => d <= 5)).toBe(true);
    expect(depthsAsked(store).length).toBeGreaterThan(0);
  });

  it('counts over the highlight, and a new highlight sends no request of the map’s own', async () => {
    const host = await mount('<tessera-map density="hex"></tessera-map>');
    const map = host.querySelector('tessera-map') as TesseraMap;
    const store = fakeStore({meta: meta(), status: status({})});
    map.store = store;
    await settle(host);
    sized(map);
    vi.advanceTimersByTime(DENSITY_SETTLE_MS);
    const specs = () => store.calls.filter((c) => c.name === 'setAggregate' && String(c.args[0]).startsWith('density#')).map((c) => c.args[1] as AggregateSpec | null);
    expect(specs().map((s) => s?.highlighted)).toEqual([true]);
    // The store asks again for the registration it holds when the highlight changes.
    store.set('view', {...store.get('view'), highlighting: true});
    store.set('filters', {...store.get('filters'), highlight: {archive: {in: ['cs']}}});
    await settle(host);
    vi.advanceTimersByTime(DENSITY_SETTLE_MS);
    expect(specs()).toHaveLength(1);
  });

  it('shows the slider for every mode but None', async () => {
    const host = await mount('<tessera-explorer density="none"></tessera-explorer>');
    const el = host.querySelector('tessera-explorer') as HTMLElement & {store: unknown; density: string};
    el.store = fakeStore({meta: meta(), status: status({})});
    await settle(host);
    const shadow = el.shadowRoot!;
    shadow.querySelector<HTMLButtonElement>('[part="layers-toggle"]')!.click();
    await settle(host);
    for (const mode of ['none', 'smooth', 'hex', 'grid', 'contours']) {
      el.density = mode;
      await settle(host);
      expect(shadow.querySelector('[part="density-resolution"]') !== null).toBe(mode !== 'none');
    }
  });
});
