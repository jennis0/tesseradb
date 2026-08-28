import {describe, expect, it} from 'vitest';
import {layerClosure, layerEntries} from '../src/layers.js';
import type {Layer} from '../src/types.js';

const layer = (name: string, depsOn: string[] = []): Layer => ({
  name,
  title: name,
  views: ['s0'],
  membership: 'enumerated',
  hierarchy: {kind: 'flat', pruneChildren: false},
  levels: [],
  computedContent: [],
  suppliedContent: [],
  depsOn,
  version: 1
});

/**
 * A clustering's labels are a second layer that `depends_on` it (decision 0096): the picker
 * offers the clustering with its labels as one entry, and the request names both.
 */
describe('the layer closure', () => {
  const layers = [layer('clusters'), layer('labels', ['clusters']), layer('districts'), layer('routes', ['districts', 'clusters'])];

  it('names a layer with its dependents and their dependencies, in meta order', () => {
    expect(layerClosure(layers, ['clusters'])).toEqual(['clusters', 'labels', 'districts', 'routes']);
    expect(layerClosure(layers, ['districts'])).toEqual(['clusters', 'labels', 'districts', 'routes']);
    expect(layerClosure([layer('a'), layer('b', ['a']), layer('c')], ['a'])).toEqual(['a', 'b']);
  });

  it('keeps a name meta does not list, and names nothing twice', () => {
    expect(layerClosure(layers, ['nope', 'nope'])).toEqual(['nope']);
    expect(layerClosure([], [])).toEqual([]);
  });

  it('offers one entry per root, each carrying its closure and never a count', () => {
    const entries = layerEntries([layer('a'), layer('b', ['a']), layer('c')]);
    expect(entries.map((e) => e.root.name)).toEqual(['a', 'c']);
    expect(entries[0]!.closure).toEqual(['a', 'b']);
    expect(Object.keys(entries[0]!)).toEqual(['root', 'closure']);
  });

  it('offers a dependent whose dependency this principal does not reach as a root of what they were given', () => {
    const entries = layerEntries([layer('labels', ['clusters'])]);
    expect(entries.map((e) => e.root.name)).toEqual(['labels']);
  });
});
