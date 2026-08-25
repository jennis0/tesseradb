import {describe, expect, it} from 'vitest';
import {composeFilters, type FilterExpr, type FilterOperandSet, type Store} from '@tesseradb/client';
import {draftOf, initialize, render, stateOf, type KernelMessage, type WidgetModel} from '../src/widget.js';
import {fakeStore, settle, status, type FakeStore} from './fake-store.js';

/**
 * The widget's JavaScript half against a fake model and a fake store: the token protocol (never
 * model state), the settle-only up-sync, the echo guard, and the expression-to-draft inversion.
 */

type Sent = {content: unknown};

function fakeModel(initial: Record<string, unknown>): WidgetModel & {sent: Sent[]; saves: number; fire(event: string, ...args: unknown[]): void; state: Record<string, unknown>} {
  const state = {...initial};
  const handlers = new Map<string, ((...args: unknown[]) => void)[]>();
  const sent: Sent[] = [];
  const m = {
    state,
    sent,
    saves: 0,
    get: (k: string) => state[k],
    set(k: string, v: unknown) {
      state[k] = v;
      m.fire(`change:${k}`);
    },
    save_changes() {
      m.saves += 1;
    },
    on(event: string, cb: (...args: unknown[]) => void) {
      handlers.set(event, [...(handlers.get(event) ?? []), cb]);
    },
    off(event: string, cb?: (...args: unknown[]) => void) {
      handlers.set(event, (handlers.get(event) ?? []).filter((h) => h !== cb));
    },
    send(content: unknown) {
      sent.push({content});
    },
    fire(event: string, ...args: unknown[]) {
      for (const h of handlers.get(event) ?? []) h(...args);
    }
  };
  return m;
}

const base = {url: 'http://tessera.test', view: null, layers: [], colour_by: null, filters: null, bbox: null, selected: null, selected_artifact: null, region: null, layout: 'docked', height: 400};

function setUp(initial: Record<string, unknown> = {}) {
  const model = fakeModel({...base, ...initial});
  let supplier: (() => Promise<{token: string; expiresAt: number}>) | null = null;
  const store = fakeStore();
  const dispose = initialize({
    model,
    storeFactory: (opts) => {
      supplier = opts.authorise;
      return store as Store;
    }
  });
  return {model, store, supplier: () => supplier!, dispose};
}

describe('the token protocol', () => {
  it('sends ready once, answers the supplier from the custom message, and never sets a token on the model', async () => {
    const {model, supplier} = setUp();
    expect(model.sent).toEqual([]);
    const p = supplier()();
    expect(model.sent.map((s) => s.content)).toEqual([{type: 'ready'}]);
    model.fire('msg:custom', {type: 'token', token: 'tok-1', expires_at: 1_800_000_000} satisfies KernelMessage);
    expect(await p).toEqual({token: 'tok-1', expiresAt: 1_800_000_000});
    expect(Object.values(model.state)).not.toContain('tok-1');
    expect(model.saves).toBe(0);
  });

  it('sends reauthorise on every later call, and shares one outstanding request', async () => {
    const {model, supplier} = setUp();
    const first = supplier()();
    model.fire('msg:custom', {type: 'token', token: 'a', expires_at: 1});
    await first;
    const second = supplier()();
    const third = supplier()();
    expect(model.sent.map((s) => s.content)).toEqual([{type: 'ready'}, {type: 'reauthorise'}]);
    model.fire('msg:custom', {type: 'token', token: 'b', expires_at: 2});
    expect((await second).token).toBe('b');
    expect((await third).token).toBe('b');
  });

  it('a refusal rejects the supplier', async () => {
    const {model, supplier} = setUp();
    const p = supplier()();
    model.fire('msg:custom', {type: 'refused', detail: 'no credential'});
    await expect(p).rejects.toThrow('no credential');
  });

  it('initialize is once per model: two renders share one store and one ready', async () => {
    const {model, supplier} = setUp();
    const el1 = document.createElement('div');
    const el2 = document.createElement('div');
    document.body.append(el1, el2);
    render({model, el: el1});
    render({model, el: el2});
    await settle(document.body);
    expect(stateOf(model)?.views).toBe(2);
    expect(el1.querySelector('tessera-explorer')).not.toBeNull();
    supplier()();
    supplier()();
    expect(model.sent.filter((s) => (s.content as {type: string}).type === 'ready')).toHaveLength(1);
  });
});

describe('the up-sync', () => {
  function shown(store: FakeStore, composition: object) {
    store.set('status', status({}));
    store.set('view', {...store.get('view'), composition: composition as never});
  }

  it('happens at the settle, not per projection change, and carries the control traits from the store', () => {
    const {model, store} = setUp();
    store.set('artifacts', {...store.get('artifacts'), layers: ['clusters/a']});
    store.set('legend', {...store.get('legend'), colourBy: 'cluster:clusters/a'});
    store.set('status', {...status({}), status: 'loading'});
    expect(model.saves).toBe(0);
    shown(store, {});
    expect(model.saves).toBe(1);
    expect(model.state.layers).toEqual(['clusters/a']);
    expect(model.state.colour_by).toBe('cluster:clusters/a');
    expect(model.state.filters).toBeNull();
    // The same composition presented again is not a new settle.
    store.set('status', status({}));
    expect(model.saves).toBe(1);
  });

  it('ids cross as decimal strings, never numbers', () => {
    const {model, store} = setUp();
    const id = 2n ** 63n + 5n;
    store.set('selection', {item: {id, detail: {} as never}, itemRefusal: null, artifact: {id: 7n, detail: {} as never}, artifactRefusal: null});
    expect(model.state.selected).toBe('9223372036854775813');
    expect(model.state.selected_artifact).toBe('7');
    expect(typeof model.state.selected).toBe('string');
  });

  it('a region syncs when its counts arrive, not while loading', () => {
    const {model, store} = setUp();
    const shape = {kind: 'box' as const, bbox: [0, 0, 1, 1] as [number, number, number, number]};
    const loading = {shape, status: 'loading' as const, refusal: null, visible: {value: 0, exact: true}, matched: {value: 0, exact: true}, served: {shown: 0, total: 0, exact: true}, depth: 7, tiles: 10, held: {ids: new BigUint64Array(0), positions: new Float32Array(0), count: 0}};
    store.set('region', loading);
    expect(model.state.region).toBeNull();
    store.set('region', {...loading, status: 'shown', visible: {value: 42, exact: false}});
    expect(model.state.region).toMatchObject({status: 'shown', visible: {value: 42, exact: false}, depth: 7, tiles: 10});
    expect((model.state.region as {held?: unknown}).held).toBeUndefined();
  });

  it('the echo guard: an up-synced layers change does not come back down as setLayers', () => {
    const {model, store} = setUp();
    store.set('artifacts', {...store.get('artifacts'), layers: ['x']});
    shown(store, {});
    expect(store.calls.filter((c) => c.name === 'setLayers')).toHaveLength(0);
    model.set('layers', ['y']);
    expect(store.calls.filter((c) => c.name === 'setLayers').map((c) => c.args)).toEqual([[['y']]]);
  });
});

describe('the down-sync', () => {
  it('applies layers and colour_by set in the kernel, and filters once meta has the operand list', () => {
    const {model, store} = setUp({layers: ['clusters/a'], colour_by: 'cluster:clusters/a'});
    expect(store.calls.map((c) => c.name)).toEqual(['setLayers', 'setColourBy']);
    const operands: FilterOperandSet[] = [{column: 'year', family: 'numeric', operands: ['range']}];
    store.set('meta', {filterOperands: operands} as never);
    model.set('filters', {year: {range: {gte: 2000}}});
    const applied = store.calls.filter((c) => c.name === 'setFilters');
    expect(applied).toHaveLength(1);
    expect(applied[0]!.args[0]).toEqual({year: {family: 'numeric', gte: 2000, lte: null}});
    // An expression the draft cannot hold is refused to the kernel, and applies nothing.
    model.set('filters', {any_of: [{year: {range: {gte: 1}}}]});
    expect(store.calls.filter((c) => c.name === 'setFilters')).toHaveLength(1);
    expect(model.sent.at(-1)?.content).toMatchObject({type: 'error', what: 'filters'});
  });

  it('a filters expression set before meta is applied at meta', () => {
    const {model, store} = setUp({filters: {year: {range: {lte: 5}}}});
    expect(store.calls.filter((c) => c.name === 'setFilters')).toHaveLength(0);
    store.set('meta', {filterOperands: [{column: 'year', family: 'numeric', operands: ['range']}]} as never);
    expect(store.calls.filter((c) => c.name === 'setFilters')).toHaveLength(1);
    expect(model.sent).toEqual([]);
  });
});

describe('draftOf inverts composeFilters', () => {
  const operands: FilterOperandSet[] = [
    {column: 'title', family: 'text', operands: ['match', 'phrase']},
    {column: 'archive', family: 'category', operands: ['in']},
    {column: 'author', family: 'keyword', operands: ['eq', 'prefix', 'contains']},
    {column: 'year', family: 'numeric', operands: ['range']}
  ];
  const cases: FilterExpr[] = [
    {title: {match: 'sea'}},
    {title: {match: {query: 'sea sky', minimum_should_match: 1}}},
    {title: {phrase: 'the sea'}},
    {archive: {in: ['a', 'b']}},
    {author: {prefix: 'Ke'}},
    {year: {range: {gte: 1900, lte: 1950}}},
    {all_of: [{title: {match: 'x'}}, {archive: {in: ['a']}}, {author: {eq: 'K'}}, {year: {range: {lte: 3}}}]}
  ];
  for (const expr of cases) {
    it(JSON.stringify(expr), () => {
      expect(composeFilters(draftOf(expr, operands))).toEqual(expr);
    });
  }
  it('null is the unfiltered request', () => {
    expect(composeFilters(draftOf(null, operands))).toBeNull();
  });
  it('refuses what a draft cannot hold, naming the reason', () => {
    expect(() => draftOf({none_of: [{title: {match: 'x'}}]}, operands)).toThrow(/none_of/);
    expect(() => draftOf({nope: {eq: 'x'}}, operands)).toThrow(/not a filterable column/);
    expect(() => draftOf({year: {eq: 3}}, operands)).toThrow(/numeric column cannot hold eq/);
    expect(() => draftOf({all_of: [{year: {range: {gte: 1}}}, {year: {range: {lte: 2}}}]}, operands)).toThrow(/two leaves/);
    expect(() => draftOf({year: {range: {gt: 1}}}, operands)).toThrow(/inclusive/);
  });
});
