import {afterEach, describe, expect, it} from 'vitest';
import type {BrowseRow, Meta} from '@tesseradb/client';
import '../src/cluster-filter.js';
import type {TesseraClusterFilter} from '../src/cluster-filter.js';
import {aggregateEntry, answerAggregate, deep, deepAll, fakeStore, meta, mount, registered, settle, status} from './fake-store.js';
import {UNNAMED} from '../src/base.js';

/**
 * A layer's clusters as a filter field: what it asks the store (browse pages, the aggregate over the
 * rows it lists), what it offers, and the clause a pick adds.
 */

afterEach(() => {
  document.body.innerHTML = '';
});

const layer = (name: string, over: Partial<Meta['layers'][number]> = {}) =>
  ({name, title: 'Topics', views: ['s0'], membership: 'enumerated', hierarchy: {kind: 'nested', pruneChildren: false}, levels: [], computedContent: ['centroid'], shape: null, suppliedContent: ['name'], depsOn: [], version: 1, ...over}) as Meta['layers'][number];

const row = (id: bigint, name: string | null, parentIds: bigint[] = [], rung = 0): BrowseRow => ({tesseraId: id, key: null, name, maskedCount: 10n, matchedCount: null, rung, parentIds, childCount: 0});

async function mountField(over: {layers?: Meta['layers']; verb?: 'filter' | 'highlight'; members?: unknown[]} = {}) {
  const host = await mount(`<tessera-cluster-filter layer="topics" verb="${over.verb ?? 'filter'}"></tessera-cluster-filter>`);
  const el = host.querySelector('tessera-cluster-filter') as TesseraClusterFilter;
  const store = fakeStore({meta: meta({layers: over.layers ?? [layer('topics')]}), status: status({})});
  store.set('view', {...store.get('view'), id: 's0'});
  if (over.members) store.set('filters', {...store.get('filters'), members: over.members as never});
  el.store = store;
  await settle(host);
  return {host, el, store, box: () => deep(host, '[part="entry"]') as HTMLInputElement};
}

const browses = (store: ReturnType<typeof fakeStore>) => store.calls.filter((c) => c.name === 'browse').map((c) => c.args[0]);
const names = (host: HTMLElement) => deepAll(host, '[part~="option"] [part="name"]').map((n) => n.textContent);
const wait = (ms: number) => new Promise((r) => setTimeout(r, ms));

describe('<tessera-cluster-filter> with the box empty', () => {
  it('offers the top-level clusters, asked unfiltered, and counts them through the aggregate without its own filter clauses', async () => {
    const {host, store, box} = await mountField();
    store.setBrowse('roots', {artifacts: [row(1n, 'Physics'), row(2n, 'Biology'), row(3n, null)], parents: [], next: null});
    expect(browses(store)).toHaveLength(0);
    box().dispatchEvent(new Event('focus'));
    await settle(host);
    expect(browses(store)).toEqual([{layer: 'topics', filters: null, limit: 50}]);
    expect(names(host)).toEqual(['Physics', 'Biology', UNNAMED]);
    expect([...registered(store).values()]).toEqual([{groupings: [{by: {layer: 'topics', artifacts: [1n, 2n, 3n]}}], withoutMembersOf: 'topics'}]);
    answerAggregate(store, 'clusters', aggregateEntry([{rows: [{key: 1n, count: 5}, {key: 2n, count: 90}, {key: 3n, count: 0}]}]));
    await settle(host);
    // Largest first once counted.
    expect(names(host)).toEqual(['Biology', 'Physics', UNNAMED]);
    expect(deepAll(host, '[part="value-count"]').map((c) => c.textContent)).toEqual(['90', '5', '0']);
  });

  it('asks the first level of a levelled layer, and counts one grouping per level', async () => {
    const tiers = layer('topics', {hierarchy: {kind: 'tiered', pruneChildren: false}, levels: [{level: 0, title: null, zoom: null}, {level: 1, title: null, zoom: null}]});
    const {host, store, box} = await mountField({layers: [tiers]});
    store.setBrowse('roots', {artifacts: [row(4n, 'Coarse', [], 0)], parents: [], next: null});
    box().dispatchEvent(new Event('focus'));
    await settle(host);
    expect(browses(store)).toEqual([{layer: 'topics', filters: null, limit: 50, level: 0}]);
    expect([...registered(store).values()]).toEqual([{groupings: [{by: {layer: 'topics', level: 0, artifacts: [4n]}}], withoutMembersOf: 'topics'}]);
  });
});

describe('<tessera-cluster-filter> asking', () => {
  it('keeps its counts registered only while its list is open', async () => {
    const {host, store, box} = await mountField();
    store.setBrowse('roots', {artifacts: [row(1n, 'Physics')], parents: [], next: null});
    box().dispatchEvent(new Event('focus'));
    await settle(host);
    expect(registered(store).size).toBe(1);
    box().dispatchEvent(new Event('blur'));
    await settle(host);
    expect(registered(store).size).toBe(0);
  });

  it('asks for the top-level clusters again on the next open after a refusal, and drops the refusal once answered', async () => {
    const {host, store, box} = await mountField();
    const answered = store.browse;
    let fail = true;
    store.browse = (async (req: never) => {
      if (fail) throw {code: 'backpressure', detail: 'busy'};
      return answered(req);
    }) as never;
    store.setBrowse('roots', {artifacts: [row(1n, 'Physics')], parents: [], next: null});
    box().dispatchEvent(new Event('focus'));
    await settle(host);
    expect(deep(host, '[part="refusal"]')).not.toBeNull();
    box().dispatchEvent(new Event('blur'));
    fail = false;
    box().dispatchEvent(new Event('focus'));
    await settle(host);
    await settle(host);
    expect(deep(host, '[part="refusal"]')).toBeNull();
    expect(names(host)).toEqual(['Physics']);
  });
});

describe('<tessera-cluster-filter> typing', () => {
  it('searches the names, shows each match’s nearest parents as its path, and a pick adds a member_of clause named by the cluster', async () => {
    const {host, store, box} = await mountField();
    store.setBrowse('q:x-ray', {artifacts: [row(30n, 'x-ray state', [20n])], parents: [], next: null});
    // The children form of each artifact on the way up carries its parents.
    store.setBrowse('p:30', {artifacts: [], parents: [row(20n, 'clusters x-ray', [10n])], next: null});
    store.setBrowse('p:20', {artifacts: [], parents: [row(10n, 'galaxies', [5n])], next: null});
    store.setBrowse('p:10', {artifacts: [], parents: [row(5n, 'physics', [1n])], next: null});
    box().value = 'x-ray';
    box().dispatchEvent(new Event('input'));
    await wait(300);
    await settle(host);
    await settle(host);
    expect(browses(store)[0]).toEqual({q: 'x-ray', layer: 'topics', filters: null, limit: 50});
    expect(names(host)).toEqual(['x-ray state']);
    // Three parents, nearest last, and more above them.
    expect(deep(host, '[part="path"]')!.textContent).toBe('… › physics › galaxies › clusters x-ray');
    (deep(host, '[part~="option"]') as HTMLButtonElement).click();
    expect(store.calls.filter((c) => c.name === 'setMembers').at(-1)!.args[0]).toEqual([{layer: 'topics', artifact: 30n, outside: false, verb: 'filter', label: 'x-ray state'}]);
  });
});

