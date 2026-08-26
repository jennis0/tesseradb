import {describe, expect, it} from 'vitest';
import {SessionArtifactTable, servedLineage, type Artifact, type ArtifactsProjection, type Meta} from '@tesseradb/client';
import {labelBudget, labelCandidates} from '../src/layer.js';
import {placeLabels} from '../src/labels.js';

/** Which artifacts get a label, and how many (§5.10): a text to draw, the top N by masked count. */

const artifact = (id: bigint, count: bigint, content: string[] = [], layer = 'clusters', parentId: bigint | null = null): Artifact => ({
  layer,
  tesseraId: id,
  key: `c-${id}`,
  maskedCount: count,
  centroid: [Number(id) * 2 ** 24, Number(id) * 2 ** 24],
  box: null,
  hull: null,
  content,
  parentId
});

function projection(served: Artifact[]): ArtifactsProjection {
  const table = new SessionArtifactTable();
  const ordinals = table.take(served.map((a) => ({tesseraId: a.tesseraId, layer: a.layer, parentId: a.parentId})));
  return {layer: 'clusters', layers: ['clusters'], served, lineage: servedLineage(served), status: 'shown', refusal: null, version: 1, table, servedOrdinals: new Set(ordinals), colours: new Map(), palette: 'positional', coverage: {current: 0, stale: 0}};
}

const META = {layers: [{name: 'clusters', hierarchy: {kind: 'flat', pruneChildren: false}, depsOn: []}, {name: 'topics', hierarchy: {kind: 'flat', pruneChildren: false}, depsOn: ['clusters']}]} as unknown as Meta;

describe('labelCandidates', () => {
  it('an artifact with no text draws no label — never its key', () => {
    const p = projection([artifact(1n, 100n, ['quantum error correction']), artifact(2n, 900n), artifact(3n, 50n, [''])]);
    const {candidates, byId} = labelCandidates(p, META, undefined, 0, 10);
    expect(candidates.map((c) => String(c.id))).toEqual(['1']);
    // Wrapped to short lines, and the whole name is still there.
    expect(byId.get(1n)!.lines).toEqual(['quantum error', 'correction']);
    expect([...byId.values()].some((t) => t.lines.join(' ').startsWith('c-'))).toBe(false);
  });

  it('a nameless cluster with a topic attached takes the topic as its name', () => {
    const p = projection([artifact(1n, 100n), artifact(2n, 40n), artifact(9n, 100n, ['decoders, thresholds'], 'topics')]);
    const {candidates, byId} = labelCandidates(p, META, undefined, 0, 10);
    expect(candidates.map((c) => String(c.id))).toEqual(['1']);
    expect(byId.get(1n)!.lines.join(' ')).toBe('decoders, thresholds');
    expect(byId.get(1n)!.topic).toBeNull();
  });

  it('takes the top N by masked count, and the placement then drops what overlaps', () => {
    const served = Array.from({length: 30}, (_, i) => artifact(BigInt(i + 1), BigInt(1000 - i), [`cluster ${i}`]));
    const p = projection(served);
    const {candidates} = labelCandidates(p, META, undefined, 0, 12);
    expect(candidates.length).toBe(12);
    expect(candidates.map((c) => Number(c.id))).toEqual([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
    expect(candidates[0]!.priority).toBe(1000);
    // Every candidate has a box the spatial hash can refuse.
    expect(candidates.every((c) => c.width > 0 && c.height > 0)).toBe(true);
    // Thirty of them on one spot: the placement keeps some and drops the rest, no two overlapping.
    const stacked = labelCandidates(p, META, undefined, 0, 30).candidates.map((c) => ({...c, x: 100, y: 100}));
    const placed = placeLabels(stacked);
    expect(placed.length).toBeLessThan(stacked.length);
    expect(placed.length).toBeGreaterThan(0);
  });

  it('a zero budget places nothing', () => {
    const p = projection([artifact(1n, 100n, ['a'])]);
    expect(labelCandidates(p, META, undefined, 0, 0).candidates).toEqual([]);
  });
});

describe('labelBudget', () => {
  it('is one label per 36,000 px² and never fewer than eight', () => {
    expect(labelBudget(1280, 800)).toBe(28);
    expect(labelBudget(1440, 900)).toBe(36);
    expect(labelBudget(300, 200)).toBe(8);
  });
});
