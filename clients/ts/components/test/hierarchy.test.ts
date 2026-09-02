import {afterEach, describe, expect, it} from 'vitest';
import type {BrowseRow, Layer, Meta} from '@tesseradb/client';
import '../src/hierarchy.js';
import type {TesseraHierarchy} from '../src/hierarchy.js';
import {deep, deepAll, deepText, fakeStore, mount, settle, status} from './fake-store.js';

/**
 * `<tessera-hierarchy>` over `POST /v1/artifacts/browse` (`highlight-and-hierarchy.md` §5.1): the
 * roots without a viewport, lazy children, *also under*, the two counts under a filter, and the
 * click that is a highlight.
 */

afterEach(() => {
  document.body.innerHTML = '';
});

const layer = (name: string, kind: Layer['hierarchy']['kind'], computedContent: string[]): Layer =>
  ({
    name,
    title: name,
    views: ['s0'],
    hierarchy: {kind, pruneChildren: false},
    levels: [],
    computedContent,
    shape: computedContent.includes('hull') ? 'derived' : null,
    suppliedContent: ['name'],
    depsOn: [],
    version: 1
  }) as unknown as Layer;

const META = {
  apiVersion: 1,
  idset: 0,
  views: [{id: 's0', displayName: 'default', quantisation: {xMin: 0, xMax: 1, yMin: 0, yMax: 1}, projection: 'none', worldAspect: null, tileScheme: null, tile: null, roster: null}],
  groups: [],
  declaredScalars: [],
  layers: [layer('clusters/kmeans', 'flat', ['centroid', 'box', 'hull']), layer('mesh/descriptors', 'dag', [])],
  selection: {kMin: 1, kMaxMarks: 500, maxK: 5000, thetaTargetMarks: 10, maxUnderlayOffset: 0, maxCategoryValues: 1000, maxRegionVertices: 10_000, maxRegionCells: 262_144, maxBrowseRows: 200},
  maxTilesPerRequest: 4096,
  filterOperands: []
} as unknown as Meta;

const row = (id: bigint, name: string, masked: bigint, extra: Partial<BrowseRow> = {}): BrowseRow => ({
  tesseraId: id,
  key: `d-${id}`,
  name,
  maskedCount: masked,
  matchedCount: null,
  rung: 0,
  parentIds: [],
  ...extra
});

async function panel(overrides: Partial<Parameters<typeof fakeStore>[0]> = {}) {
  const host = await mount('<tessera-hierarchy></tessera-hierarchy>');
  const el = host.querySelector('tessera-hierarchy') as TesseraHierarchy;
  const store = fakeStore({meta: META, status: status({}), ...overrides});
  store.setBrowse('roots', {artifacts: [row(1n, 'Neoplasms', 27_000_000n), row(2n, 'Anatomy', 3_400n)], parents: [], next: 'p2'});
  el.store = store;
  await settle(host);
  await settle(host);
  return {host, el, store};
}

describe('<tessera-hierarchy>', () => {
  it('offers only the layers with a lineage, and opens on the roots with no viewport', async () => {
    const {host, store} = await panel();
    // `clusters/kmeans` is flat and declares no levels: one page of roots and no row has children,
    // so it is not a tree to browse and the panel does not offer it one — which leaves one layer,
    // and one layer draws no picker at all.
    expect(deep(host, '[part="layer"]')).toBeNull();
    expect(deepAll(host, '[part="row"] [part="name"]').map((n) => n.textContent?.trim())).toEqual(['Neoplasms', 'Anatomy']);
    // The request carried the layer and a limit and nothing about where the map is looking.
    const asked = store.calls.filter((c) => c.name === 'browse').map((c) => c.args[0] as Record<string, unknown>);
    expect(asked[0]).toEqual({layer: 'mesh/descriptors', limit: 50});
  });

  it('fetches a node’s children on expansion, and pages them under More', async () => {
    const {host, el, store} = await panel();
    store.setBrowse('p:1', {artifacts: [row(11n, 'Cysts', 900n)], parents: [], next: 'c2'});
    store.setBrowse('p:1:c2', {artifacts: [row(12n, 'Hamartoma', 40n)], parents: [], next: null});
    (deep(host, '[part="expander"]') as HTMLButtonElement).click();
    await settle(host);
    await settle(host);
    expect(deepAll(host, '[part="row"] [part="name"]').map((n) => n.textContent?.trim())).toEqual(['Neoplasms', 'Cysts', 'Anatomy']);
    (deepAll(host, '[part="more"]')[0] as HTMLButtonElement).click();
    await settle(host);
    await settle(host);
    expect(deepAll(host, '[part="row"] [part="name"]').map((n) => n.textContent?.trim())).toEqual(['Neoplasms', 'Cysts', 'Hamartoma', 'Anatomy']);
    void el;
  });

  it('says *also under* for a dag node served beneath several parents', async () => {
    const {host, store} = await panel();
    store.setBrowse('p:1', {artifacts: [row(11n, 'Cysts', 900n, {parentIds: [1n, 2n]})], parents: [], next: null});
    (deep(host, '[part="expander"]') as HTMLButtonElement).click();
    await settle(host);
    await settle(host);
    // The parent it is drawn under is not repeated, and the other is named rather than numbered:
    // the walk is the only place a name for one of these artifacts exists on this client.
    expect(deepText(deep(host, '[part="also"]'))).toContain('also under Anatomy');
  });

  it('a click is a highlight, and the actions carry the filter beside it', async () => {
    const {host, store} = await panel();
    (deep(host, '[part="row"] [part="name"]') as HTMLButtonElement).click();
    const sent = store.calls.find((c) => c.name === 'setMembers')!.args[0] as {layer: string; artifact: bigint; verb: string; outside: boolean}[];
    // The label rides the clause: nothing downstream can resolve the identifier, the artifacts of
    // a filter layer never being served.
    expect(sent).toEqual([{layer: 'mesh/descriptors', artifact: 1n, outside: false, verb: 'highlight', label: 'Neoplasms'}]);
    // And *fit* is absent on a filter layer — its artifacts are spread and there is nothing to fit.
    expect(deep(host, '[part="fit"]')).toBeNull();
    expect(deep(host, '[part="filter"]')).not.toBeNull();
  });

  /**
   * The panel's counts answer the question the request carried, so the walk has to be dropped when
   * that question moves — and the question is `requestFilters()`, which carries the **drawn
   * region's leaf** as well as the controls and the clauses. Hashed from `filters.expr` alone, a
   * region drawn on the map left the tree showing counts to the question before it, with nothing
   * on screen saying so.
   */
  it('refetches when the drawn region changes, which the controls alone never say', async () => {
    const {host, store} = await panel();
    const before = store.calls.filter((c) => c.name === 'browse').length;
    expect(before).toBeGreaterThan(0);

    store.set('region', {
      shape: {kind: 'box', bbox: [0, 0, 1, 1], outside: false},
      status: 'shown', refusal: null, visible: null, matched: {value: 4, exact: true},
      served: {shown: 4, total: 4, exact: true}, held: {shown: 4, total: 4, exact: true}
    } as never);
    await settle(host);
    await settle(host);
    expect(store.calls.filter((c) => c.name === 'browse').length).toBeGreaterThan(before);
  });

  it('does not refetch when nothing about the question moved', async () => {
    const {host, store} = await panel();
    const before = store.calls.filter((c) => c.name === 'browse').length;
    // A store tick that changes nothing the request carries — the panel is told, and asks nothing.
    store.set('status', {status: 'shown', sessionWarm: true, refusal: null, stale: false, expired: false, retrying: false});
    await settle(host);
    await settle(host);
    expect(store.calls.filter((c) => c.name === 'browse').length).toBe(before);
  });

  it('shows the matched count beside the masked one where the map carries a filter', async () => {
    const {host} = await panel({
      filters: {draft: {}, expr: {archive: {in: ['cs']}}, highlight: null, members: [], suggestions: {}, suggestErrors: {}, suggestEpoch: 0}
    });
    // Existence and the masked count never move with the filter; the second figure is what does.
    expect(deep(host, '[part="count-masked"]')).not.toBeNull();
  });

  it('drives the search form and draws its matches as rows', async () => {
    const {host, store} = await panel();
    store.setBrowse('q:lymph', {artifacts: [row(7n, 'Lymphocytes', 400_000n)], parents: [], next: null});
    const box = deep(host, '[part="search"] input') as HTMLInputElement;
    box.value = 'lymph';
    box.dispatchEvent(new Event('input'));
    await new Promise((r) => setTimeout(r, 400));
    await settle(host);
    await settle(host);
    expect(deepAll(host, '[part="row"] [part="name"]').map((n) => n.textContent?.trim())).toEqual(['Lymphocytes']);
  });

  it('a panel that is not being shown asks for nothing, and browses when it is', async () => {
    // The roots are the widest request this panel makes, and a host with the panel in a closed
    // drawer was paying for it on the page's first meta — beside the first viewport, against a
    // server still materialising the session.
    const host = await mount('<tessera-hierarchy style="display:none"></tessera-hierarchy>');
    const el = host.querySelector('tessera-hierarchy') as TesseraHierarchy;
    const store = fakeStore({meta: META, status: status({})});
    store.setBrowse('roots', {artifacts: [row(1n, 'Neoplasms', 27_000_000n)], parents: [], next: null});
    el.store = store;
    await settle(host);
    await settle(host);
    expect(store.calls.filter((c) => c.name === 'browse')).toHaveLength(0);

    el.style.display = '';
    el.requestUpdate();
    await settle(host);
    await settle(host);
    expect(store.calls.filter((c) => c.name === 'browse')).toHaveLength(1);
    expect(deepAll(host, '[part="row"] [part="name"]').map((n) => n.textContent?.trim())).toEqual(['Neoplasms']);
  });

});
