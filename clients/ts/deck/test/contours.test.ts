import {describe, expect, it} from 'vitest';
import {SessionArtifactTable, servedLineage, type Artifact, type ArtifactsProjection} from '@tesseradb/client';
import {outlineOf, servedDepths, smoothClosed} from '../src/layer.js';

/** The boards' contours: a served hull smoothed, and nesting read from the served tree. */

const artifact = (id: bigint, parentId: bigint | null, count = 10n): Artifact => ({
  layer: 'clusters',
  tesseraId: id,
  key: `c-${id}`,
  maskedCount: count,
  centroid: [2 ** 31, 2 ** 31],
  box: [0, 0, 2 ** 32 - 1, 2 ** 32 - 1],
  hull: [[0, 0], [2 ** 32 - 1, 0], [2 ** 32 - 1, 2 ** 32 - 1], [0, 2 ** 32 - 1]],
  content: [],
  parentId
});

function projection(served: Artifact[]): ArtifactsProjection {
  const table = new SessionArtifactTable();
  const ordinals = table.take(served.map((a) => ({tesseraId: a.tesseraId, layer: a.layer, parentId: a.parentId})));
  return {layer: 'clusters', layers: ['clusters'], served, lineage: servedLineage(served), status: 'shown', refusal: null, version: 1, table, servedOrdinals: new Set(ordinals), colours: new Map(), palette: 'positional', coverage: {current: 0, stale: 0}};
}

describe('smoothClosed', () => {
  it('rounds a square into a closed curve that stays inside the square', () => {
    const square: [number, number][] = [[0, 0], [10, 0], [10, 10], [0, 10]];
    const out = smoothClosed(square, 3);
    expect(out.length).toBe(4 * 2 ** 3);
    for (const [x, y] of out) {
      expect(x).toBeGreaterThanOrEqual(0);
      expect(x).toBeLessThanOrEqual(10);
      expect(y).toBeGreaterThanOrEqual(0);
      expect(y).toBeLessThanOrEqual(10);
    }
    // The corners are cut: no vertex sits on a corner any more.
    expect(out.some(([x, y]) => (x === 0 || x === 10) && (y === 0 || y === 10))).toBe(false);
    // Fewer than three vertices is not a polygon and passes through untouched.
    expect(smoothClosed([[0, 0], [1, 1]])).toEqual([[0, 0], [1, 1]]);
  });

  it('outlineOf smooths the wire hull by default and leaves it exact on request', () => {
    const a = artifact(1n, null);
    expect(outlineOf(a, false)!.length).toBe(4);
    expect(outlineOf(a)!.length).toBe(32);
    expect(outlineOf(a)![0]![0]).toBeGreaterThan(0);
  });
});

describe('servedDepths', () => {
  it('is the depth in the served tree — a root 0, a child one deeper, a child of an unserved parent a root', () => {
    const served = [artifact(1n, null), artifact(2n, 1n), artifact(3n, 2n), artifact(4n, 99n)];
    const depths = servedDepths(projection(served));
    expect([...depths.entries()].map(([id, d]) => [String(id), d])).toEqual([['1', 0], ['2', 1], ['3', 2], ['4', 0]]);
  });
});
