import {describe, expect, it} from 'vitest';
import {drawableLayers, isFilterLayer, layerEntries} from '../src/layers.js';
import type {Layer} from '../src/types.js';
import {layer as declared} from './support.js';

/**
 * A layer declaring `computed = []` is a filter layer: in the roster, and not presented for
 * viewing.
 */
const layer = (name: string, computedContent: string[]): Layer =>
  declared(name, {
    views: ['knn'],
    computedContent,
    shape: computedContent.includes('hull') ? 'derived' : null,
    suppliedContent: ['name']
  });

const CLUSTERS = layer('clusters/kmeans', ['centroid', 'box', 'hull']);
const MESH = layer('mesh/descriptors', []);

describe('a filter layer', () => {
  it('is the declaration and nothing else: no field, no server change', () => {
    expect(isFilterLayer(MESH)).toBe(true);
    expect(isFilterLayer(CLUSTERS)).toBe(false);
  });

  it('is still a layer: it stays in the roster, and only the drawable ones may be drawn', () => {
    const roster = [CLUSTERS, MESH];
    // In `meta`'s list and the client's; a picker draws it in its own group.
    expect(layerEntries(roster).map((e) => e.root.name)).toEqual(['clusters/kmeans', 'mesh/descriptors']);
    // Not among the layers anything draws.
    expect(drawableLayers(roster).map((l) => l.name)).toEqual(['clusters/kmeans']);
  });
});
