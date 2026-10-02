import {afterEach, beforeEach, describe, expect, it, vi} from 'vitest';
import {DENSITY_SETTLE_MS} from '@tesseradb/deck';
import '../src/explorer.js';
import type {TesseraMap} from '../src/map.js';
import {aggregateEntry, answerAggregate, fakeStore, meta, mount, settle, status} from './fake-store.js';

/**
 * Density's colour scale through the elements: the map's `density-scale`, the explorer's Scale
 * choice under Resolution and what it reports, and the figures the map's key shows for each scale.
 */

/** A map 1000 × 800 px, fitted, so its camera has rested somewhere the counter can ask about. */
function sized(map: TesseraMap): void {
  Object.defineProperty(map, 'clientWidth', {configurable: true, value: 1000});
  Object.defineProperty(map, 'clientHeight', {configurable: true, value: 800});
  map.fit();
}

/** The figures under the density key's ramp, left to right. */
const keyFigures = (map: TesseraMap) => [...map.shadowRoot!.querySelectorAll('[part="density-key"] .ends > span')].map((s) => s.textContent);

describe('density scale', () => {
  beforeEach(() => vi.useFakeTimers({toFake: ['setTimeout', 'clearTimeout']}));
  afterEach(() => vi.useRealTimers());

  it('is log on the map and the explorer unless set', async () => {
    const host = await mount('<tessera-map></tessera-map><tessera-explorer></tessera-explorer><tessera-map density-scale="linear"></tessera-map>');
    const [plain, , set] = [...host.children] as (HTMLElement & {densityScale: string})[];
    expect(plain!.densityScale).toBe('log');
    expect((host.querySelector('tessera-explorer') as HTMLElement & {densityScale: string}).densityScale).toBe('log');
    expect(set!.densityScale).toBe('linear');
  });

  it('keys the counts drawn from 0 to the largest, with the count at the middle of the scale in force', async () => {
    const host = await mount('<tessera-map density="grid" no-points></tessera-map>');
    const map = host.querySelector('tessera-map') as TesseraMap;
    const store = fakeStore({meta: meta(), status: status({})});
    map.store = store;
    await settle(host);
    sized(map);
    vi.advanceTimersByTime(DENSITY_SETTLE_MS);
    // Before any counts arrive the key has no figures.
    expect(keyFigures(map)).toEqual(['Fewer', '', 'More items']);
    answerAggregate(store, 'density#', aggregateEntry([{rows: [{cell: 0n, count: 1}, {cell: 1n, count: 99}]}], store.get('view').id));
    await settle(host);
    // log1p(9) is half of log1p(99).
    expect(keyFigures(map)).toEqual(['0', '9', '99']);
    map.densityScale = 'linear';
    await settle(host);
    expect(keyFigures(map)).toEqual(['0', '50', '99']);
  });

  it('sets the scale from the Scale choice under Resolution, passes it to the map and reports it', async () => {
    const host = await mount('<tessera-explorer density="hex"></tessera-explorer>');
    const el = host.querySelector('tessera-explorer') as HTMLElement & {store: unknown; densityScale: string};
    el.store = fakeStore({meta: meta(), status: status({})});
    await settle(host);
    const shadow = el.shadowRoot!;
    const map = shadow.querySelector('tessera-map') as TesseraMap;
    const seen: {densityScale: string; density: string}[] = [];
    host.addEventListener('tessera-displaychange', (e) => seen.push((e as CustomEvent).detail));
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
    const host = await mount('<tessera-explorer density="grid" density-scale="linear"></tessera-explorer>');
    const el = host.querySelector('tessera-explorer') as HTMLElement & {store: unknown};
    el.store = fakeStore({meta: meta(), status: status({})});
    await settle(host);
    expect((el.shadowRoot!.querySelector('tessera-map') as TesseraMap).densityScale).toBe('linear');
  });
});
