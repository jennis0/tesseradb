import {afterEach, beforeEach, describe, expect, it, vi} from 'vitest';
import {DENSITY_SETTLE_MS} from '@mosaica/deck';
import '../src/explorer.js';
import type {MosaicaMap} from '../src/map.js';
import {aggregateEntry, answerAggregate, fakeStore, meta, mount, settle, status} from './fake-store.js';

/**
 * Density's colour scale through the elements: the map's `density-scale`, the explorer's Scale
 * choice under Resolution and what it reports, and the figures the map's key shows for each scale.
 */

/** A map 1000 × 800 px, fitted, so its camera has rested somewhere the counter can ask about. */
function sized(map: MosaicaMap): void {
  Object.defineProperty(map, 'clientWidth', {configurable: true, value: 1000});
  Object.defineProperty(map, 'clientHeight', {configurable: true, value: 800});
  map.fit();
}

/** The figures under the density key's ramp, left to right. */
const keyFigures = (map: MosaicaMap) => [...map.shadowRoot!.querySelectorAll('[part="density-key"] .ends > span')].map((s) => s.textContent);

describe('density scale', () => {
  beforeEach(() => vi.useFakeTimers({toFake: ['setTimeout', 'clearTimeout']}));
  afterEach(() => vi.useRealTimers());

  it('is log on the map and the explorer unless set', async () => {
    const host = await mount('<mosaica-map></mosaica-map><mosaica-explorer></mosaica-explorer><mosaica-map density-scale="linear"></mosaica-map>');
    const [plain, , set] = [...host.children] as (HTMLElement & {densityScale: string})[];
    expect(plain!.densityScale).toBe('log');
    expect((host.querySelector('mosaica-explorer') as HTMLElement & {densityScale: string}).densityScale).toBe('log');
    expect(set!.densityScale).toBe('linear');
  });

  /** A map drawing a grid without points, its counter answered with `counts` once the camera has rested. */
  async function keyed(counts: number[], scale = '') {
    const host = await mount(`<mosaica-map density="grid" no-points ${scale ? `density-scale="${scale}"` : ''}></mosaica-map>`);
    const map = host.querySelector('mosaica-map') as MosaicaMap;
    const store = fakeStore({meta: meta(), status: status({})});
    map.store = store;
    await settle(host);
    sized(map);
    vi.advanceTimersByTime(DENSITY_SETTLE_MS);
    const before = keyFigures(map);
    answerAggregate(store, 'density#', aggregateEntry([{rows: counts.map((count, i) => ({cell: BigInt(i), count}))}], store.get('view').id));
    await settle(host);
    return {host, map, before};
  }

  it('keys the counts drawn from 0 to the largest, with the count at the middle of the scale in force', async () => {
    const {host, map, before} = await keyed([1, 99]);
    // Before any counts arrive the key has no figures.
    expect(before).toEqual(['Fewer', '', 'More items']);
    // log1p(9) is half of log1p(99).
    expect(keyFigures(map)).toEqual(['0', '9', '99']);
    expect(map.shadowRoot!.querySelector('[part="density-key"] .ends > :last-child')!.getAttribute('title')).toBe('The densest cell in view and around it');
    map.densityScale = 'linear';
    await settle(host);
    expect(keyFigures(map)).toEqual(['0', '50', '99']);
  });

  it('shows the middle in whole items, and none where it is under one item or at the largest', async () => {
    expect(keyFigures((await keyed([1, 3], 'linear')).map)).toEqual(['0', '2', '3']);
    expect(keyFigures((await keyed([1, 3], 'log')).map)).toEqual(['0', '1', '3']);
    expect(keyFigures((await keyed([1], 'linear')).map)).toEqual(['0', '', '1']);
    expect(keyFigures((await keyed([1], 'log')).map)).toEqual(['0', '', '1']);
  });

  it('takes an unknown scale as log, in the key and in the explorer’s choice', async () => {
    expect(keyFigures((await keyed([1, 99], 'cubic')).map)).toEqual(['0', '9', '99']);
    const host = await mount('<mosaica-explorer density="grid" density-scale="cubic"></mosaica-explorer>');
    const el = host.querySelector('mosaica-explorer') as HTMLElement & {store: unknown};
    el.store = fakeStore({meta: meta(), status: status({})});
    await settle(host);
    const shadow = el.shadowRoot!;
    shadow.querySelector<HTMLButtonElement>('[part="layers-toggle"]')!.click();
    await settle(host);
    const radios = [...shadow.querySelectorAll('[part="density-scale"] [role="radio"]')];
    expect(radios.map((b) => [b.getAttribute('data-scale'), b.getAttribute('aria-checked'), b.getAttribute('tabindex')])).toEqual([
      ['linear', 'false', '-1'],
      ['log', 'true', '0']
    ]);
  });

  it('sets the scale from the Scale choice under Resolution, passes it to the map and reports it', async () => {
    const host = await mount('<mosaica-explorer density="hex"></mosaica-explorer>');
    const el = host.querySelector('mosaica-explorer') as HTMLElement & {store: unknown; densityScale: string};
    el.store = fakeStore({meta: meta(), status: status({})});
    await settle(host);
    const shadow = el.shadowRoot!;
    const map = shadow.querySelector('mosaica-map') as MosaicaMap;
    const seen: {densityScale: string; density: string}[] = [];
    host.addEventListener('mosaica-displaychange', (e) => seen.push((e as CustomEvent).detail));
    shadow.querySelector<HTMLButtonElement>('[part="layers-toggle"]')!.click();
    await settle(host);
    const choice = (scale: string) => shadow.querySelector<HTMLButtonElement>(`[part="density-scale"] [data-scale="${scale}"]`)!;
    expect([...shadow.querySelectorAll('[part="density-scale"] [role="radio"]')].map((b) => b.textContent)).toEqual(['Linear', 'Log']);
    expect(choice('log').getAttribute('aria-checked')).toBe('true');
    // It sits under the Resolution slider.
    const parts = [...shadow.querySelectorAll('[part="density-resolution"], [part="density-scale"]')].map((e) => e.getAttribute('part'));
    expect(parts).toEqual(['density-resolution', 'density-scale']);

    choice('linear').click();
    await settle(host);
    expect(el.densityScale).toBe('linear');
    expect(map.densityScale).toBe('linear');
    expect(choice('linear').getAttribute('aria-checked')).toBe('true');

    choice('linear').dispatchEvent(new KeyboardEvent('keydown', {key: 'ArrowRight', bubbles: true}));
    await settle(host);
    expect(map.densityScale).toBe('log');
    expect(seen.map((d) => [d.density, d.densityScale])).toEqual([
      ['hex', 'linear'],
      ['hex', 'log']
    ]);

    // Under None there is no scale to choose.
    el.setAttribute('density', 'none');
    await settle(host);
    expect(shadow.querySelector('[part="density-scale"]')).toBeNull();
  });

  it('passes a host’s density-scale to its map', async () => {
    const host = await mount('<mosaica-explorer density="grid" density-scale="linear"></mosaica-explorer>');
    const el = host.querySelector('mosaica-explorer') as HTMLElement & {store: unknown};
    el.store = fakeStore({meta: meta(), status: status({})});
    await settle(host);
    expect((el.shadowRoot!.querySelector('mosaica-map') as MosaicaMap).densityScale).toBe('linear');
  });
});
