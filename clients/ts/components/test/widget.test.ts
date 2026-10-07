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

const META = {
  apiVersion: 1,
  views: [{id: 's0', displayName: 'default', quantisation: {xMin: 0, xMax: 1, yMin: 0, yMax: 1}, roster: null}],
  // A one-view bundle declares no group; the explorer's pickers read this and draw nothing.
  groups: [],
  declaredScalars: [],
  layers: [],
  selection: {kMin: 1, kMaxMarks: 500, maxK: 5000, thetaTargetMarks: 10, maxUnderlayOffset: 0, maxCategoryValues: 1000, maxRegionVertices: 10_000, maxRegionCells: 262_144},
  maxTilesPerRequest: 4096,
  filterOperands: [] as FilterOperandSet[]
};

/** A layer points can be coloured by: it declares geometry and depends on nothing. */
const clusters = (name: string) => ({name, title: name, views: ['s0'], membership: 'enumerated', hierarchy: {kind: 'flat', pruneChildren: false}, levels: [], computedContent: ['centroid'], shape: null, suppliedContent: [], depsOn: [], version: 1});

const base = {url: 'http://tessera.test', view: null, layers: null, colour_by: null, palette: null, value_colours: null, cluster_colours: null, size_by: null, size_min: null, size_max: null, size_scale: null, filters: null, bbox: null, selected: null, selected_artifact: null, region: null, explorer_layout: 'docked', height: 400, title_field: null};

/** A model initialised and one view rendered, so there is a store: the store is per view. */
function setUp(initial: Record<string, unknown> = {}) {
  const model = fakeModel({...base, ...initial});
  let supplier: (() => Promise<{token: string; expiresAt: number}>) | null = null;
  const stores: FakeStore[] = [];
  const dispose = initialize({
    model,
    storeFactory: (opts) => {
      supplier = opts.authorise;
      const store = fakeStore();
      stores.push(store);
      return store as Store;
    }
  });
  const el = document.createElement('div');
  document.body.append(el);
  const unmount = render({model, el});
  return {model, store: stores[0]!, stores, el, unmount, supplier: () => supplier!, dispose};
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

  it('hands title_field to every view’s explorer, and follows a change to it', async () => {
    const {model, el} = setUp({title_field: 'title'});
    const explorer = el.querySelector('tessera-explorer') as unknown as {titleField: string};
    expect(explorer.titleField).toBe('title');
    model.set('title_field', null);
    expect(explorer.titleField).toBe('');
  });

  it('hands the point budget and its range to every view’s explorer, and follows a change to each', async () => {
    const {model, el} = setUp({budget: 40_000, budget_min: 500, budget_max: 90_000});
    const explorer = el.querySelector('tessera-explorer') as unknown as {budget: number; budgetMin: number; budgetMax: number};
    expect([explorer.budget, explorer.budgetMin, explorer.budgetMax]).toEqual([40_000, 500, 90_000]);
    model.set('budget', 60_000);
    model.set('budget_max', 100_000);
    expect([explorer.budget, explorer.budgetMin, explorer.budgetMax]).toEqual([60_000, 500, 100_000]);
  });

  it('hands the cluster budget and its range to every view’s explorer, and sends it up when Most clusters is let go', async () => {
    const {model, el} = setUp({cluster_budget: 300, cluster_budget_min: 5, cluster_budget_max: 5_000});
    const explorer = el.querySelector('tessera-explorer') as unknown as HTMLElement & {clusterBudget: number; clusterBudgetMin: number; clusterBudgetMax: number};
    expect([explorer.clusterBudget, explorer.clusterBudgetMin, explorer.clusterBudgetMax]).toEqual([300, 5, 5_000]);
    model.set('cluster_budget_max', 8_000);
    expect(explorer.clusterBudgetMax).toBe(8_000);
    explorer.dispatchEvent(new CustomEvent('tessera-clusterbudgetchange', {detail: {budget: 1_200}, bubbles: true, composed: true}));
    expect(model.get('cluster_budget')).toBe(1_200);
    expect(explorer.clusterBudget).toBe(1_200);
    model.set('cluster_budget', 300);
    expect(explorer.clusterBudget).toBe(300);
    model.set('cluster_budget', 0);
    expect(explorer.clusterBudget).toBe(0);
  });

  it('sends the budget up when Most points is let go, and takes the old one back from the kernel', async () => {
    const {model, el} = setUp({budget: 40_000});
    const explorer = el.querySelector('tessera-explorer') as unknown as HTMLElement & {budget: number};
    const saves = model.saves;
    explorer.dispatchEvent(new CustomEvent('tessera-budgetchange', {detail: {budget: 120_000}, bubbles: true, composed: true}));
    expect(model.get('budget')).toBe(120_000);
    expect(model.saves).toBe(saves + 1);
    expect(explorer.budget).toBe(120_000);
    // The kernel setting the old budget back reaches the explorer.
    model.set('budget', 40_000);
    expect(explorer.budget).toBe(40_000);
  });

  it('two views of one model: a store each, one supplier, and one ready', async () => {
    const {model, supplier, stores, el} = setUp();
    const el2 = document.createElement('div');
    document.body.append(el2);
    const unmount2 = render({model, el: el2});
    await settle(document.body);
    expect(stateOf(model)?.views).toBe(2);
    expect(stores).toHaveLength(2);
    expect(el.querySelector('tessera-explorer')).not.toBeNull();
    // Both stores ask; one ready goes out, and a held token is handed back without a round trip.
    const a = supplier()();
    const b = supplier()();
    model.fire('msg:custom', {type: 'token', token: 'shared', expires_at: null});
    expect((await a).token).toBe('shared');
    expect((await b).token).toBe('shared');
    expect((await supplier()()).token).toBe('shared');
    expect(model.sent.map((s) => s.content)).toEqual([{type: 'ready'}]);
    // A view's store is disposed with the view.
    unmount2();
    expect(stores[1]!.calls.map((c) => c.name)).toContain('dispose');
    expect(stateOf(model)?.views).toBe(1);
  });

  it('a token near expiry is renewed with reauthorise; one with no expiry is never asked for again', async () => {
    const {model, supplier} = setUp();
    const first = supplier()();
    model.fire('msg:custom', {type: 'token', token: 'a', expires_at: Date.now() / 1000 + 10});
    await first;
    const second = supplier()();
    expect(model.sent.map((s) => s.content)).toEqual([{type: 'ready'}, {type: 'reauthorise'}]);
    model.fire('msg:custom', {type: 'token', token: 'b', expires_at: Date.now() / 1000 + 3600});
    expect((await second).token).toBe('b');
    expect((await supplier()()).token).toBe('b');
    expect(model.sent).toHaveLength(2);
  });
});

describe('the up-sync', () => {
  function shown(store: FakeStore, composition: object) {
    store.set('status', status({}));
    store.set('view', {...store.get('view'), composition: {tiles: [], exact: [], standIn: [], exactDrawn: 0, exactServed: 0, ...composition} as never});
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
    expect(model.state.palette).toBe('tableau10');
    expect(model.state.filters).toBeNull();
    expect([model.state.size_by, model.state.size_min, model.state.size_max, model.state.size_scale]).toEqual([null, 2, 9, 'linear']);
    // The same composition presented again is not a new settle.
    store.set('status', status({}));
    expect(model.saves).toBe(1);
  });

  it('ids cross as decimal strings, never numbers', () => {
    const {model, store} = setUp();
    const id = 2n ** 63n + 5n;
    store.set('selection', {item: {id, detail: {} as never}, itemRefusal: null, artifact: {id: 7n, detail: {layer: 'clusters', key: null, maskedCount: 1n, centroid: null, box: null, shape: null}}, artifactRefusal: null});
    expect(model.state.selected).toBe('9223372036854775813');
    expect(model.state.selected_artifact).toBe('7');
    expect(typeof model.state.selected).toBe('string');
  });

  it('a region syncs when its counts arrive, not while loading', () => {
    const {model, store} = setUp();
    const shape = {kind: 'box' as const, bbox: [0, 0, 1, 1] as [number, number, number, number]};
    const loading = {shape, status: 'loading' as const, refusal: null, visible: {value: 0, exact: true}, matched: {value: 0, exact: true}, served: {shown: 0, total: 0, exact: true}, verdict: {exact: true as const, depth: null}, held: {ids: new BigUint64Array(0), positions: new Float32Array(0), count: 0}};
    store.set('region', loading);
    expect(model.state.region).toBeNull();
    store.set('region', {...loading, status: 'shown', visible: {value: 42, exact: false}});
    expect(model.state.region).toMatchObject({status: 'shown', visible: {value: 42, exact: false}, verdict: {exact: true, depth: null}});
    expect((model.state.region as {held?: unknown}).held).toBeUndefined();
  });

  it('the view the map shows is up-synced at the settle', () => {
    const {model, store} = setUp();
    store.set('view', {...store.get('view'), id: 'quarter:2026-Q3'});
    shown(store, {});
    expect(model.state.view).toBe('quarter:2026-Q3');
    // And it does not come back down as a switch: the echo guard covers `view` as it does the
    // other controls.
    expect(store.calls.filter((c) => c.name === 'setCurrentView')).toHaveLength(0);
  });

  it('the echo guard: an up-synced layers change does not come back down as setLayers', () => {
    const {model, store} = setUp();
    expect(store.calls.filter((c) => c.name === 'setLayers')).toHaveLength(0);
    store.set('artifacts', {...store.get('artifacts'), layers: ['x']});
    shown(store, {});
    expect(store.calls.filter((c) => c.name === 'setLayers')).toHaveLength(0);
    model.set('layers', ['y']);
    expect(store.calls.filter((c) => c.name === 'setLayers').map((c) => c.args)).toEqual([[['y']]]);
  });
});

describe('a view change', () => {
  it('switches every mounted view\u2019s store rather than rebuilding it', () => {
    const {model, stores, store} = setUp();
    const el2 = document.createElement('div');
    document.body.append(el2);
    render({model, el: el2});
    expect(stores).toHaveLength(2);
    model.set('view', 'quarter:2026-Q3');
    // A view change rebuilds nothing: no third store, and nothing disposed.
    expect(stores).toHaveLength(2);
    expect(store.calls.filter((c) => c.name === 'dispose')).toHaveLength(0);
    for (const s of stores) expect(s.calls.filter((c) => c.name === 'setCurrentView').map((c) => c.args)).toEqual([['quarter:2026-Q3']]);
  });

  it('a url change is still a rebuild: a tessera_id minted by one bundle means nothing to another', () => {
    const {model, stores, store} = setUp();
    model.set('url', 'http://other.test');
    expect(stores).toHaveLength(2);
    expect(store.calls.filter((c) => c.name === 'dispose')).toHaveLength(1);
  });
});

describe('the down-sync', () => {
  it('a bbox set in the kernel before meta is fitted when meta arrives, and echoes nothing back', async () => {
    const {model, store, el} = setUp();
    await settle(document.body);
    const explorer = el.querySelector('tessera-explorer') as unknown as {map: {fitBbox(b: number[]): boolean} | null};
    const fitted: number[][] = [];
    const map = explorer.map;
    expect(map).not.toBeNull();
    // As the real one: nothing fits before meta, and the box is pending.
    map!.fitBbox = (b) => {
      if (!store.get('meta')) return false;
      fitted.push(b);
      return true;
    };
    model.set('bbox', [1, 2, 3, 4]);
    expect(fitted).toEqual([]);
    store.set('meta', META as never);
    expect(fitted).toEqual([[1, 2, 3, 4]]);
  });

  it('layers null leaves the default and [] is none', () => {
    expect(setUp({layers: null}).store.calls.filter((c) => c.name === 'setLayers')).toHaveLength(0);
    expect(setUp({layers: []}).store.calls.filter((c) => c.name === 'setLayers').map((c) => c.args)).toEqual([[[]]]);
  });

  it('applies layers and colour_by set in the kernel, and filters once meta has the operand list', () => {
    const {model, store} = setUp({layers: ['clusters/a'], colour_by: 'cluster:clusters/a'});
    expect(store.calls.map((c) => c.name)).toEqual(['setLayers', 'setColourBy']);
    // Only the active view syncs up; a settle on it after meta carries what the store applied.
    const operands: FilterOperandSet[] = [{column: 'year', family: 'numeric', operands: ['range']}];
    store.set('meta', {...META, layers: [clusters('clusters/a')], filterOperands: operands} as never);
    expect(model.sent).toEqual([]);
    // The traitlet is the filter expression; a highlight the page holds is kept beside it.
    const lit = {year: {family: 'numeric' as const, gte: 1990, lte: 1999}};
    store.set('filters', {...store.get('filters'), draft: {filter: {}, highlight: lit}});
    model.set('filters', {year: {range: {gte: 2000}}});
    const applied = store.calls.filter((c) => c.name === 'setFilters');
    expect(applied).toHaveLength(1);
    expect(applied[0]!.args[0]).toEqual({filter: {year: {family: 'numeric', gte: 2000, lte: null}}, highlight: lit});
    // An expression the draft cannot hold is refused to the kernel, and applies nothing.
    model.set('filters', {any_of: [{year: {range: {gte: 1}}}]});
    expect(store.calls.filter((c) => c.name === 'setFilters')).toHaveLength(1);
    expect(model.sent.at(-1)?.content).toMatchObject({type: 'error', what: 'filters'});
  });

  it('applies the palette set in the kernel and follows a change; one chosen in the explorer goes up at once', () => {
    const {model, store, el} = setUp({palette: 'kelly'});
    expect(store.calls.filter((c) => c.name === 'setPalette').map((c) => c.args)).toEqual([['kelly']]);
    model.set('palette', 'okabe-ito');
    expect(store.calls.filter((c) => c.name === 'setPalette').map((c) => c.args)).toEqual([['kelly'], ['okabe-ito']]);
    const explorer = el.querySelector('tessera-explorer')!;
    explorer.dispatchEvent(new CustomEvent('tessera-clusterpalettechange', {detail: {palette: 'tableau20'}, bubbles: true, composed: true}));
    expect(model.state.palette).toBe('tableau20');
    // The up-sync does not come back down.
    expect(store.calls.filter((c) => c.name === 'setPalette')).toHaveLength(2);
  });

  it('applies the value and cluster colours set in the kernel and follows a change; colours chosen in the explorer go up at once', async () => {
    const {colouringOf} = await import('../src/colouring.js');
    const {model, store, el} = setUp({value_colours: {venue: {nips: '#112233'}}, cluster_colours: {'7': '#445566'}});
    const explorer = el.querySelector('tessera-explorer')!;
    await settle(el);
    expect(colouringOf(store).values).toEqual({venue: {nips: '#112233'}});
    expect([...store.get('artifacts').overrides.keys()]).toEqual([7n]);
    model.set('cluster_colours', {'8': '#000000'});
    await settle(el);
    expect([...store.get('artifacts').overrides.keys()]).toEqual([8n]);
    const saves = model.saves;
    explorer.dispatchEvent(new CustomEvent('tessera-valuecolour', {detail: {column: 'venue', changes: [{value: 'icml', colour: '#abcdef'}, {value: 'nips', colour: null}]}, bubbles: true, composed: true}));
    expect(model.state.value_colours).toEqual({venue: {icml: '#abcdef'}});
    explorer.dispatchEvent(new CustomEvent('tessera-valuecolour', {detail: {column: 'venue', changes: [{value: 'icml', colour: null}]}, bubbles: true, composed: true}));
    expect(model.state.value_colours).toEqual({});
    explorer.dispatchEvent(new CustomEvent('tessera-clustercolour', {detail: {layer: 'topics', changes: [{tesseraId: '9', colour: '#fedcba'}]}, bubbles: true, composed: true}));
    expect(model.state.cluster_colours).toEqual({'8': '#000000', '9': '#fedcba'});
    expect(model.saves).toBe(saves + 3);
    // The up-sync does not come back down to replace what the store holds.
    await settle(el);
    expect([...store.get('artifacts').overrides.keys()]).toEqual([8n]);
  });

  it('applies the size settings set in the kernel, the scale before the column, and follows each change', async () => {
    const {sizingOf} = await import('../src/colouring.js');
    const {model, store} = setUp({size_by: 'citations', size_min: 3, size_max: 11, size_scale: 'rank'});
    // Sized by rank, the store is asked to keep a sample of the column's values.
    expect(store.calls.filter((c) => c.name === 'setSizeBy').map((c) => c.args)).toEqual([['citations', {rank: true}]]);
    expect(sizingOf(store)).toEqual({min: 3, max: 11, scale: 'rank'});
    model.set('size_max', 7);
    model.set('size_scale', 'log');
    expect(sizingOf(store)).toEqual({min: 3, max: 7, scale: 'log'});
    // A radius that is not a number above zero leaves the one drawn, which the next settle reports.
    model.set('size_min', -1);
    expect(sizingOf(store).min).toBe(3);
    model.set('size_by', null);
    expect(store.calls.filter((c) => c.name === 'setSizeBy').at(-1)!.args).toEqual([null, {rank: false}]);
  });

  it('reports a colour_by naming no layer this view can colour by, once meta lists the layers', () => {
    const {model, store} = setUp({colour_by: 'cluster:nope'});
    // The store takes it as given; before meta there is nothing to check it against.
    expect(store.calls.filter((c) => c.name === 'setColourBy').map((c) => c.args)).toEqual([['cluster:nope']]);
    expect(model.sent).toEqual([]);
    store.set('meta', {...META, layers: [clusters('topics'), {...clusters('topic_names'), computedContent: [], depsOn: ['topics']}]} as never);
    expect(model.sent.map((s) => s.content)).toMatchObject([{type: 'error', what: 'colour_by'}]);
    // A layer that can colour is not reported, drawn or not; a labels layer cannot colour.
    model.set('colour_by', 'cluster:topics');
    model.set('colour_by', 'archive');
    expect(model.sent).toHaveLength(1);
    model.set('colour_by', 'cluster:topic_names');
    expect(model.sent.map((s) => s.content)).toMatchObject([{what: 'colour_by'}, {type: 'error', what: 'colour_by'}]);
    expect(store.calls.filter((c) => c.name === 'setLayers')).toHaveLength(0);
  });

  it('a filters expression set before meta is applied at meta', () => {
    const {model, store} = setUp({filters: {year: {range: {lte: 5}}}});
    expect(store.calls.filter((c) => c.name === 'setFilters')).toHaveLength(0);
    store.set('meta', {...META, filterOperands: [{column: 'year', family: 'numeric', operands: ['range']}]} as never);
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
  it('reads back every text expression the box writes, alone and beside other columns', () => {
    for (const query of ['graph neural', '"graph neural"', 'networks "graph neural"', 'graph OR lattice', '"neural net" OR gnn OR graph lattice']) {
      const alone = composeFilters({filter: {title: {family: 'text', query, phrase: true}}, highlight: {}})!;
      const draft = draftOf(alone, operands);
      expect(draft.filter['title']).toEqual({family: 'text', query, phrase: true});
      expect(composeFilters(draft)).toEqual(alone);
      const beside = composeFilters({filter: {title: {family: 'text', query, phrase: true}, archive: {family: 'category', keys: ['a']}}, highlight: {}})!;
      expect(composeFilters(draftOf(beside, operands))).toEqual(beside);
    }
  });
  it('keeps a text expression the box cannot write as it was sent', () => {
    for (const expr of [
      {title: {match: 'salt OR pepper'}},
      {title: {match: {query: 'sea sky', minimum_should_match: 1}}},
      {title: {match: 'say "hello"'}},
      {any_of: [{title: {phrase: 'the sea'}}, {title: {match: {query: 'a b c', minimum_should_match: 2}}}]}
    ] as FilterExpr[]) {
      const draft = draftOf(expr, operands);
      expect(draft.filter['title']).toMatchObject({family: 'text', expr});
      expect(composeFilters(draft)).toEqual(expr);
    }
  });
  it('null is the unfiltered request', () => {
    expect(composeFilters(draftOf(null, operands))).toBeNull();
  });
  it('refuses what a draft cannot hold', () => {
    expect(() => draftOf({none_of: [{title: {match: 'x'}}]}, operands)).toThrow();
    expect(() => draftOf({nope: {eq: 'x'}}, operands)).toThrow();
    expect(() => draftOf({year: {eq: 3}}, operands)).toThrow();
    expect(() => draftOf({all_of: [{year: {range: {gte: 1}}}, {year: {range: {lte: 2}}}]}, operands)).toThrow();
    expect(() => draftOf({year: {range: {gt: 1}}}, operands)).toThrow();
    expect(() => draftOf({any_of: [{title: {match: 'x'}}, {archive: {in: ['a']}}]}, operands)).toThrow();
  });
});
