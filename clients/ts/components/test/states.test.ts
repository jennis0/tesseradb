import {afterEach, describe, expect, it} from 'vitest';
import '../src/status.js';
import '../src/selection.js';
import '../src/count.js';
import type {TesseraStatus} from '../src/status.js';
import {stateOf} from '../src/states.js';
import {deep, deepAll, fakeStore, mount, settle, status} from './fake-store.js';

/**
 * Every §5.4 state, through `part="state"`, on the strip and on a panel; and the harness's
 * first claims at the unit level: only `shown` renders a count, no count against a stale view
 * and a refresh control present, a refusal renders as one, an expiry fires `tessera-expired`.
 */

afterEach(() => {
  document.body.innerHTML = '';
});

const view = {
  composition: null,
  depth: 5,
  visible: {value: 12_040, exact: true},
  matched: {value: 3_210, exact: true},
  served: {shown: 500, total: 12_040, exact: true},
  provisional: 0
};

describe('stateOf maps status onto the eight states exactly as the table says', () => {
  it('detached, loading, retrying, shown, empty, refused, expired, stale', () => {
    expect(stateOf(null)).toBe('detached');
    expect(stateOf(status({status: 'idle'}))).toBe('loading');
    expect(stateOf(status({status: 'loading'}))).toBe('loading');
    expect(stateOf(status({status: 'retrying'}))).toBe('retrying');
    expect(stateOf(status({status: 'shown'}))).toBe('shown');
    expect(stateOf(status({status: 'empty'}))).toBe('empty');
    expect(stateOf(status({status: 'refused', refusal: {code: 'x', detail: 'y'}}))).toBe('refused');
    expect(stateOf(status({status: 'refused', refusal: {code: 'expired-token', detail: ''}, expired: true}))).toBe('expired');
    expect(stateOf(status({status: 'shown', stale: true}))).toBe('stale');
  });
});

describe('<tessera-status> renders every state through part="state"', () => {
  const cases: [string, Parameters<typeof status>[0] | null, {count: boolean; refresh: boolean}][] = [
    ['detached', null, {count: false, refresh: false}],
    ['loading', {status: 'loading', sessionWarm: false}, {count: false, refresh: false}],
    ['retrying', {status: 'retrying'}, {count: false, refresh: false}],
    ['shown', {status: 'shown'}, {count: true, refresh: false}],
    ['empty', {status: 'empty'}, {count: false, refresh: false}],
    ['refused', {status: 'refused', refusal: {code: 'unauthorised', detail: 'no'}}, {count: false, refresh: false}],
    ['expired', {status: 'refused', refusal: {code: 'expired-token', detail: ''}, expired: true}, {count: false, refresh: false}],
    ['stale', {status: 'shown', stale: true}, {count: false, refresh: true}]
  ];
  for (const [name, over, want] of cases) {
    it(`renders ${name}`, async () => {
      const host = await mount('<tessera-status></tessera-status>');
      const el = host.querySelector('tessera-status') as TesseraStatus;
      if (over) {
        const store = fakeStore({status: status(over), view});
        el.store = store;
        await settle(host);
      }
      const state = deep(host, '[part="state"]')!;
      expect(state.getAttribute('data-state')).toBe(name);
      const counts = deepAll(host, '[part="count"]').filter((c) => c.getAttribute('data-empty') === 'false');
      expect(counts.length > 0).toBe(want.count);
      expect(deep(host, '[part="refresh"]') !== null).toBe(want.refresh);
      if (name === 'refused') expect(deep(host, '[part="refusal"]')?.textContent).toContain('unauthorised');
    });
  }

  it('shows the three counts in the order shown · matched · visible, both figures for the sample', async () => {
    const host = await mount('<tessera-status></tessera-status>');
    (host.querySelector('tessera-status') as TesseraStatus).store = fakeStore({status: status({}), view});
    await settle(host);
    const counts = deepAll(host, '[part="count"]');
    expect(counts.map((c) => c.textContent)).toEqual(['500', '3,210', '12,040']);
    // The sample's total is the visible cell beside it, and the cell carries it: both figures.
    expect(counts[0]!.getAttribute('data-total')).toBe('12,040');
  });

  it('fires tessera-expired once, composed, on the expired transition', async () => {
    const host = await mount('<div><tessera-status></tessera-status></div>');
    const el = host.querySelector('tessera-status') as TesseraStatus;
    const store = fakeStore({status: status({}), view});
    el.store = store;
    await settle(host);
    const fired: Event[] = [];
    document.body.addEventListener('tessera-expired', (e) => fired.push(e));
    store.set('status', status({status: 'refused', refusal: {code: 'expired-token', detail: ''}, expired: true}));
    await settle(host);
    store.set('status', status({status: 'refused', refusal: {code: 'expired-token', detail: 'again'}, expired: true}));
    await settle(host);
    expect(fired.length).toBe(1);
    expect(fired[0]!.composed).toBe(true);
  });

  it('the refresh control calls refresh() on the store', async () => {
    const host = await mount('<tessera-status></tessera-status>');
    const store = fakeStore({status: status({stale: true}), view});
    (host.querySelector('tessera-status') as TesseraStatus).store = store;
    await settle(host);
    (deep(host, '[part="refresh"]') as HTMLButtonElement).click();
    expect(store.calls.some((c) => c.name === 'refresh')).toBe(true);
  });
});

describe('<tessera-selection> — a panel renders the states the same way', () => {
  it('is detached with no region, counting while loading, refused as a refusal, and inexact when the answer is a cover', async () => {
    const host = await mount('<tessera-selection></tessera-selection>');
    const store = fakeStore({status: status({}), view});
    const el = host.querySelector('tessera-selection')!;
    (el as unknown as {store: unknown}).store = store;
    await settle(host);
    expect(deep(host, '[part="state"]')?.getAttribute('data-state')).toBe('detached');

    const held = {ids: BigUint64Array.from([1n, 2n]), positions: new Float32Array(4), count: 2};
    const shape = {kind: 'box' as const, bbox: [0, 0, 1, 1] as [number, number, number, number]};
    store.set('region', {shape, status: 'loading', refusal: null, visible: {value: 0, exact: false}, matched: {value: 0, exact: false}, served: {shown: 2, total: 0, exact: false}, verdict: null, held});
    await settle(host);
    expect(deep(host, '[part="state"]')?.getAttribute('data-state')).toBe('loading');
    expect(deepAll(host, '[part="count"]').length).toBe(0);

    store.set('region', {shape, status: 'refused', refusal: {code: 'contract', detail: 'bad'}, visible: {value: 0, exact: false}, matched: {value: 0, exact: false}, served: {shown: 2, total: 0, exact: false}, verdict: null, held});
    await settle(host);
    expect(deep(host, '[part="state"]')?.getAttribute('data-state')).toBe('refused');
    expect(deep(host, '[part="refusal"]')?.textContent).toContain('contract');

    store.set('region', {shape, status: 'shown', refusal: null, visible: {value: 900, exact: false}, matched: {value: 800, exact: false}, served: {shown: 2, total: 800, exact: true}, verdict: {exact: false, depth: 6}, held});
    await settle(host);
    const counts = deepAll(host, '[part="count"]');
    expect(counts.map((c) => c.textContent)).toEqual(['2', '≈ 800', '≈ 900']);
    expect(counts[0]!.getAttribute('data-total')).toBe('800');
    expect(counts[1]!.getAttribute('data-exact')).toBe('false');
    expect(deepAll(host, '[part="item"]').length).toBe(2);
    // Two verbs wait on the server (export, save); *filter to this* is the selection itself, and
    // the outside toggle is live.
    const greyed = deepAll(host, '[part="action"][disabled]');
    expect(greyed.length).toBe(2);
    expect(greyed.every((b) => (b.getAttribute('title') ?? '').length > 0)).toBe(true);
  });
});
