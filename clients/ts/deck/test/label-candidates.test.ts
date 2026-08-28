import {describe, expect, it} from 'vitest';
import {SessionArtifactTable, servedLineage, type Artifact, type ArtifactsProjection, type Meta} from '@tesseradb/client';
import {artifactName, displayName, frontier, labelBudget, labelCandidates} from '../src/layer.js';
import {LABEL_SIZE_MAX, LABEL_SIZE_MIN, placeLabels} from '../src/labels.js';

/**
 * Which artifacts get a label (§5.10, the owner's review 2026-08-26): the frontier of the served
 * set, a text to draw, the top N by masked count — and a size that is that count, on a
 * logarithmic band over the range the frontier drawn holds.
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
  parentId,
  level: 0,
  matched: null
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

  it('size is the masked count, and level says nothing: the deeper, larger name draws larger', () => {
    // The owner's case, in miniature. 2 stops at depth 1 with 400 members; 5 and 6 are a level
    // deeper and 5 is the biggest thing drawn. Size follows the counts, not the depths.
    const p = projection([
      artifact(1n, 9000n, ['root']),
      artifact(2n, 400n, ['stops here'], 'clusters', 1n),
      artifact(3n, 5000n, ['an ancestor'], 'clusters', 1n),
      artifact(5n, 4000n, ['deeper and bigger'], 'clusters', 3n),
      artifact(6n, 100n, ['deeper and smaller'], 'clusters', 3n)
    ]);
    const {byId} = labelCandidates(p, META, undefined, 0, 10);
    expect([...byId.keys()].map(String).sort()).toEqual(['2', '5', '6']);
    // The largest count on screen is the largest name on screen, whatever level it sits at.
    expect(byId.get(5n)!.size).toBeCloseTo(LABEL_SIZE_MAX, 6);
    expect(byId.get(6n)!.size).toBeCloseTo(LABEL_SIZE_MIN, 6);
    expect(byId.get(2n)!.size).toBeGreaterThan(byId.get(6n)!.size);
    expect(byId.get(2n)!.size).toBeLessThan(byId.get(5n)!.size);
  });

  it('spans the band over the counts drawn — the smallest at the floor, the largest at the top', () => {
    const served = Array.from({length: 5}, (_, i) => artifact(BigInt(i + 1), BigInt(10 ** (5 - i)), [`cluster ${i}`]));
    const {byId} = labelCandidates(projection(served), META, undefined, 0, 10);
    const sizes = [...byId.values()].map((t) => t.size);
    expect(Math.max(...sizes)).toBeCloseTo(LABEL_SIZE_MAX, 6);
    expect(Math.min(...sizes)).toBeCloseTo(LABEL_SIZE_MIN, 6);
    // Four decades over the band, so the decades are even steps.
    const ordered = [...byId.entries()].sort((a, b) => Number(a[0] - b[0])).map(([, t]) => t.size);
    for (let i = 1; i < ordered.length - 1; i++) {
      expect(ordered[i]! - ordered[i + 1]!).toBeCloseTo(ordered[i - 1]! - ordered[i]!, 6);
    }
  });

  it('a frontier with no range lands on the band rather than dividing by zero', () => {
    // One name on screen, and every count equal: both take the top of the band, and neither is NaN.
    const one = labelCandidates(projection([artifact(1n, 4812n, ['the only cluster'])]), META, undefined, 0, 10);
    expect(one.byId.get(1n)!.size).toBe(LABEL_SIZE_MAX);
    const flat = Array.from({length: 4}, (_, i) => artifact(BigInt(i + 1), 500n, [`cluster ${i}`]));
    const {byId} = labelCandidates(projection(flat), META, undefined, 0, 10);
    const sizes = [...byId.values()].map((t) => t.size);
    expect(sizes).toEqual([LABEL_SIZE_MAX, LABEL_SIZE_MAX, LABEL_SIZE_MAX, LABEL_SIZE_MAX]);
    // And an empty frontier asks nothing of the band at all.
    expect(labelCandidates(projection([]), META, undefined, 0, 10).candidates).toEqual([]);
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
