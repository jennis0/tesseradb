import {afterEach, describe, expect, it} from 'vitest';
import '../src/status.js';
import '../src/selection.js';
import '../src/count.js';
import type {TesseraStatus} from '../src/status.js';
import {stateOf} from '../src/states.js';
import {deep, deepAll, fakeStore, mount, settle, status} from './fake-store.js';

/**
 * Every panel state through `part="state"`, on the strip and on a panel: only `shown` renders a
 * count, a stale view renders none and offers refresh, a refusal renders as one, and an expiry
 * fires `tessera-expired`.
 */

afterEach(() => {
  document.body.innerHTML = '';
});

const view = {
  id: 'v',
  composition: null,
  depth: 5,
  visible: {value: 12_040, exact: true},
  matched: {value: 3_210, exact: true},
  highlighted: {value: 3_210, exact: true},
  highlighting: false,
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
  // `count`: figures are rendered, current or greyed out. `action`: the one button the state offers.
  const cases: [string, Parameters<typeof status>[0] | null, {count: boolean; action: string | null}][] = [
    ['detached', null, {count: false, action: null}],
    ['loading', {status: 'loading', sessionWarm: false}, {count: false, action: null}],
    ['retrying', {status: 'retrying'}, {count: true, action: null}],
    ['shown', {status: 'shown'}, {count: true, action: null}],
    ['empty', {status: 'empty'}, {count: false, action: null}],
    ['refused', {status: 'refused', refusal: {code: 'unauthorised', detail: 'no'}}, {count: true, action: 'retry'}],
    ['expired', {status: 'refused', refusal: {code: 'expired-token', detail: ''}, expired: true}, {count: true, action: null}],
    ['stale', {status: 'shown', stale: true}, {count: false, action: 'refresh'}]
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
      for (const action of ['refresh', 'retry', 'reauthorise']) expect(deep(host, `[part="${action}"]`) !== null, action).toBe(want.action === action);
      if (name === 'refused') expect(deep(host, '[part="refusal"]')?.getAttribute('data-code')).toBe('unauthorised');
    });
  }

  /**
   * Counts outside `shown` are the last answer under the same token, greyed. A token change clears
   * the view in the store, so a view with no answer must render no figure in any state.
   */
  it('renders no count from a view with no answer, in any state', async () => {
    const empty = {...view, visible: {value: 0, exact: false}, matched: {value: 0, exact: false}, highlighted: {value: 0, exact: false}, served: {shown: 0, total: 0, exact: false}};
    for (const over of [{status: 'loading'}, {status: 'retrying'}, {status: 'refused', refusal: {code: 'x', detail: ''}}, {status: 'refused', refusal: {code: 'expired-token', detail: ''}, expired: true}, {status: 'shown'}] as const) {
      const host = await mount('<tessera-status></tessera-status>');
      (host.querySelector('tessera-status') as TesseraStatus).store = fakeStore({status: status(over), view: empty});
      await settle(host);
      expect(deepAll(host, '[part="count"]').filter((c) => c.getAttribute('data-empty') === 'false'), over.status).toHaveLength(0);
      host.remove();
    }
  });

  it('names the up-to-date state through the dot’s accessible name, and no other state that way', async () => {
    const host = await mount('<tessera-status></tessera-status>');
    const el = host.querySelector('tessera-status') as TesseraStatus;
    const store = fakeStore({status: status({}), view});
    el.store = store;
    await settle(host);
    expect(deep(host, '[part="state"] [role="img"]')?.getAttribute('aria-label')).toBeTruthy();
    store.set('status', status({status: 'retrying'}));
    await settle(host);
    expect(deep(host, '[part="state"]')?.getAttribute('data-state')).toBe('retrying');
    expect(deep(host, '[part="state"] [role="img"]')).toBeNull();
  });

  it('shows the matched count out of the visible count, then the shown count', async () => {
    const host = await mount('<tessera-status></tessera-status>');
    (host.querySelector('tessera-status') as TesseraStatus).store = fakeStore({status: status({}), view});
    await settle(host);
    const counts = deepAll(host, '[part="count"]');
    expect(counts.map((c) => c.textContent)).toEqual(['3,210', '12,040', '500']);
    // The shown cell carries the sample's total for a host to read.
    expect(counts[2]!.getAttribute('data-total')).toBe('12,040');
  });

  it('shortens the figures and drops the shown count when compact', async () => {
    const host = await mount('<tessera-status compact></tessera-status>');
    (host.querySelector('tessera-status') as TesseraStatus).store = fakeStore({status: status({}), view: {...view, matched: {value: 16_822_190, exact: true}, visible: {value: 21_406_522, exact: true}}});
    await settle(host);
    expect(deepAll(host, '[part="count"]').map((c) => c.textContent)).toEqual(['16.8M', '21.4M']);
    expect(deep(host, '[part="count-shown"]')).toBeNull();
  });

  /**
   * The highlight's count shows only under a highlight: without one `highlighted` equals `matched`
   * and the cell would repeat it. `highlighting` says whether one was asked.
   */
  it('adds the highlight’s own count only where a highlight was asked', async () => {
    const host = await mount('<tessera-status></tessera-status>');
    const el = host.querySelector('tessera-status') as TesseraStatus;
    el.store = fakeStore({status: status({}), view});
    await settle(host);
    expect(deep(host, '[part="count-highlighted"]')).toBeNull();

    el.store = fakeStore({status: status({}), view: {...view, highlighted: {value: 812, exact: true}, highlighting: true}});
    await settle(host);
    expect(deepAll(host, '[part="count"]').map((c) => c.textContent)).toEqual(['3,210', '12,040', '812', '500']);
    expect(deep(host, '[part="count-highlighted"]')).not.toBeNull();
  });

  it('retries a refused view and signs in again on expiry through the host’s renewal', async () => {
    const host = await mount('<tessera-status></tessera-status>');
    const el = host.querySelector('tessera-status') as TesseraStatus;
    const store = fakeStore({status: status({status: 'refused', refusal: {code: 'unauthorised', detail: ''}}), view});
    el.store = store;
    await settle(host);
    (deep(host, '[part="retry"]') as HTMLButtonElement).click();
    expect(store.calls.filter((c) => c.name === 'refresh')).toHaveLength(1);
    let renewed = 0;
    el.reauthorise = () => renewed++;
    store.set('status', status({status: 'refused', refusal: {code: 'expired-token', detail: ''}, expired: true}));
    await settle(host);
    (deep(host, '[part="reauthorise"]') as HTMLButtonElement).click();
    expect(renewed).toBe(1);
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
    expect(deep(host, '[part="refusal"]')).not.toBeNull();

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
