import {describe, expect, it} from 'vitest';
import {SessionArtifactTable, servedLineage, type Artifact, type ArtifactsProjection, type Meta} from '@tesseradb/client';
import {artifactName, displayName, frontier, labelBudget, labelCandidates} from '../src/layer.js';
import {LEVEL_SIZES, placeLabels} from '../src/labels.js';

/**
 * Which artifacts get a label (§5.10, the owner's review 2026-08-26): the frontier of the served
 * set, a text to draw, the top N by masked count — and a size that says which level it is.
 */

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

const ids = (s: Iterable<bigint>) => [...s].map(String).sort();

describe('frontier', () => {
  it('is every served artifact with no served child — an ancestor of something drawn is not on it', () => {
    // A chain 1 → 2 → 3 and a sibling 4 under 2: the frontier is 3 and 4, not the chain above.
    const p = projection([artifact(1n, 900n), artifact(2n, 500n, [], 'clusters', 1n), artifact(3n, 300n, [], 'clusters', 2n), artifact(4n, 200n, [], 'clusters', 2n)]);
    expect(ids(frontier(p, undefined))).toEqual(['3', '4']);
  });

  it('a flat layer is all frontier — nothing has a served child', () => {
    const p = projection([artifact(1n, 9n), artifact(2n, 8n), artifact(3n, 7n)]);
    expect(ids(frontier(p, undefined))).toEqual(['1', '2', '3']);
  });

  it('an artifact whose parent was withheld is a root, and a leaf if it has no served child', () => {
    const p = projection([artifact(1n, 9n), artifact(7n, 8n, [], 'clusters', 99n)]);
    expect(ids(frontier(p, undefined))).toEqual(['1', '7']);
  });

  it('at a chosen level it is that level and every branch that stopped above it', () => {
    // 1 → 2 → 3 is three deep; 4 is a child of 1 and stops there. At level 1 the frontier is 2
    // (the level) and 4 (a branch the level did not reach) — never 1, whose child 2 is drawn.
    const p = projection([artifact(1n, 900n), artifact(2n, 500n, [], 'clusters', 1n), artifact(3n, 300n, [], 'clusters', 2n), artifact(4n, 200n, [], 'clusters', 1n)]);
    expect(ids(frontier(p, 1))).toEqual(['2', '4']);
    expect(ids(frontier(p, 0))).toEqual(['1']);
    expect(ids(frontier(p, 9))).toEqual(['3', '4']);
  });
});

describe('naming', () => {
  it('an artifact with no supplied text has no name — never its key', () => {
    expect(artifactName(artifact(1n, 5n, ['quantum error correction']))).toBe('quantum error correction');
    expect(artifactName(artifact(1n, 5n))).toBeNull();
    expect(artifactName(artifact(1n, 5n, ['']))).toBeNull();
    const topics = new Map([[2n, 'decoders, thresholds']]);
    expect(displayName(artifact(2n, 5n), topics)).toBe('decoders, thresholds');
    expect(displayName(artifact(3n, 5n), topics)).toBeNull();
  });
});

describe('labelCandidates', () => {
  it('an artifact with no text draws no label — never its key', () => {
    const p = projection([artifact(1n, 100n, ['quantum error correction']), artifact(2n, 900n), artifact(3n, 50n, [''])]);
    const {candidates, byId} = labelCandidates(p, META, undefined, 0, 10);
    expect(candidates.map((c) => String(c.id))).toEqual(['1']);
    // Wrapped to short lines, and the whole name is still there.
    expect(byId.get(1n)!.lines).toEqual(['quantum error', 'correction']);
    expect([...byId.values()].some((t) => t.lines.join(' ').startsWith('c-'))).toBe(false);
  });

  it('only the frontier is labelled — an ancestor of something drawn draws nothing', () => {
    const p = projection([
      artifact(1n, 900n, ['the whole corpus']),
      artifact(2n, 500n, ['a big split'], 'clusters', 1n),
      artifact(3n, 300n, ['a small split'], 'clusters', 2n),
      artifact(4n, 200n, ['another small split'], 'clusters', 2n)
    ]);
    const {candidates} = labelCandidates(p, META, undefined, 0, 10);
    expect(candidates.map((c) => String(c.id))).toEqual(['3', '4']);
  });

  it('a nameless cluster with a topic attached takes the topic as its name', () => {
    const p = projection([artifact(1n, 100n), artifact(2n, 40n), artifact(9n, 100n, ['decoders, thresholds'], 'topics')]);
    const {candidates, byId} = labelCandidates(p, META, undefined, 0, 10);
    expect(candidates.map((c) => String(c.id))).toEqual(['1']);
    expect(byId.get(1n)!.lines.join(' ')).toBe('decoders, thresholds');
    expect(byId.get(1n)!.topic).toBeNull();
  });

  it('size encodes level first: a step per level drawn, the count only ordering within one', () => {
    // Two branches: 2 stops at depth 1, 3 and 4 are at depth 2. The frontier holds two levels,
    // so it draws two sizes — the coarser one larger, whatever the counts say.
    const p = projection([
      artifact(1n, 900n, ['root']),
      artifact(2n, 400n, ['stops here'], 'clusters', 1n),
      artifact(3n, 500n, ['deeper and bigger'], 'clusters', 1n),
      artifact(5n, 300n, ['deeper still'], 'clusters', 3n),
      artifact(6n, 100n, ['deeper too'], 'clusters', 3n)
    ]);
    const {byId} = labelCandidates(p, META, undefined, 0, 10);
    expect([...byId.keys()].map(String).sort()).toEqual(['2', '5', '6']);
    const coarse = byId.get(2n)!.size;
    const fine = [byId.get(5n)!.size, byId.get(6n)!.size];
    // The shallower name is a step larger than either deeper one, though 5 outweighs it 300:400
    // only within its own level.
    expect(coarse).toBeCloseTo(LEVEL_SIZES[0]! + 1.5, 6);
    for (const f of fine) expect(f).toBeLessThan(coarse);
    expect(fine[0]).toBeGreaterThan(fine[1]!);
  });

  it('one level drawn is one size — a flat layer reads as a flat layer', () => {
    const served = Array.from({length: 5}, (_, i) => artifact(BigInt(i + 1), BigInt(1000 - i * 100), [`cluster ${i}`]));
    const {byId} = labelCandidates(projection(served), META, undefined, 0, 10);
    const sizes = [...byId.values()].map((t) => t.size);
    expect(Math.max(...sizes) - Math.min(...sizes)).toBeLessThan(1.6);
    expect(Math.max(...sizes)).toBeCloseTo(LEVEL_SIZES[0]! + 1.5, 6);
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
