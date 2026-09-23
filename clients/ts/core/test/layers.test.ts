import {describe, expect, it} from 'vitest';
import {colourLayers, drawableLayers, isFilterLayer, layerClosure, layerEntries} from '../src/layers.js';
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

describe('a filter layer', () => {
  it('is a layer declaring no computed geometry that depends on nothing', () => {
    expect(isFilterLayer(layer('descriptors'))).toBe(true);
    expect(isFilterLayer({...layer('clusters'), computedContent: ['centroid']})).toBe(false);
  });

  it('is never a labels layer, which declares no geometry and is drawn at the artifact it names', () => {
    const clusters = {...layer('clusters'), computedContent: ['centroid']};
    const labels = layer('labels', ['clusters']);
    expect(isFilterLayer(labels)).toBe(false);
    expect(drawableLayers([clusters, labels, layer('descriptors')]).map((l) => l.name)).toEqual(['clusters', 'labels']);
  });
});

describe('the layers points may be coloured by', () => {
  it('are the drawable layers that are not labels layers', () => {
    const clusters = {...layer('clusters'), computedContent: ['centroid']};
    const districts = {...layer('districts'), computedContent: ['box']};
    const roster = [clusters, layer('labels', ['clusters']), layer('descriptors'), districts];
    expect(colourLayers(roster).map((l) => l.name)).toEqual(['clusters', 'districts']);
  });
});
