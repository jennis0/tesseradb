import {describe, expect, it} from 'vitest';
import {SessionArtifactTable, servedLineage, type Artifact, type ArtifactsProjection} from '@tesseradb/client';
import {outlineData, outlineOf, servedDepths, smoothClosed} from '../src/layer.js';

/** The boards' contours: a served hull smoothed, and which of them the map actually draws. */

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

describe('outlineData', () => {
  const alphas = (data: ReturnType<typeof outlineData>) => Object.fromEntries(data.map((d) => [String(d.id), [d.fill, d.line, d.width]]));

  it('draws a hull only for the hovered and the opened artifact — a nested layer included', () => {
    // Three levels of one chain: at rest not one of them draws, though all three are in the data.
    const p = projection([artifact(1n, null), artifact(2n, 1n), artifact(3n, 2n)]);
    for (const scheme of ['light', 'dark'] as const) {
      const rest = outlineData(p, {opened: null, hovered: null, level: undefined, scheme});
      expect(rest.map((d) => String(d.id))).toEqual(['1', '2', '3']);
      expect(rest.every((d) => d.fill === 0 && d.line === 0)).toBe(true);
      // Every one still carries its polygon, which is what answers a pick.
      expect(rest.every((d) => d.polygon.length >= 3)).toBe(true);
    }
    const some = alphas(outlineData(p, {opened: 2n, hovered: 3n, level: undefined, scheme: 'light'}));
    expect(some['1']).toEqual([0, 0, 0.8]);
    expect(some['2']).toEqual([41, 200, 1.2]);
    expect(some['3']![0]).toBeGreaterThan(0);
    expect(some['3']![1]).toBe(150);
  });

  it('a flat layer draws the same way — one rule, not two', () => {
    const p = projection([artifact(1n, null), artifact(2n, null), artifact(3n, null)]);
    const none = outlineData(p, {opened: null, hovered: null, level: undefined, scheme: 'light'});
    expect(none.length).toBe(3);
    expect(none.every((d) => d.fill === 0 && d.line === 0)).toBe(true);
    const some = alphas(outlineData(p, {opened: 2n, hovered: 3n, level: undefined, scheme: 'light'}));
    expect(some['2']).toEqual([41, 200, 1.2]);
    expect(some['3']![1]).toBe(150);
  });

  it('the opened artifact is strong whatever its layer, and the hovered one is firmer than nothing', () => {
    const p = projection([artifact(1n, null), artifact(2n, 1n)]);
    const data = alphas(outlineData(p, {opened: 1n, hovered: 2n, level: undefined, scheme: 'dark'}));
    expect(data['1']).toEqual([41, 200, 1.2]);
    expect(data['2']![1]).toBe(150);
    expect(data['2']![0]).toBeGreaterThan(0);
    // Hovering the opened one changes nothing: opened wins.
    const same = alphas(outlineData(p, {opened: 1n, hovered: 1n, level: undefined, scheme: 'dark'}));
    expect(same['1']).toEqual([41, 200, 1.2]);
  });

  it('a level cuts the outlines below it, and the hover rule holds inside it', () => {
    const p = projection([artifact(1n, null), artifact(2n, 1n), artifact(3n, 2n)]);
    const data = outlineData(p, {opened: null, hovered: null, level: 1, scheme: 'light'});
    expect(data.map((d) => String(d.id))).toEqual(['1', '2']);
    expect(data.every((d) => d.fill === 0 && d.line === 0)).toBe(true);
    const hovered = outlineData(p, {opened: null, hovered: 2n, level: 1, scheme: 'light'});
    expect(hovered.find((d) => d.id === 2n)!.line).toBe(150);
  });

  it('parents are ordered before their children, so an opened child draws over an opened parent', () => {
    const p = projection([artifact(3n, 2n), artifact(1n, null), artifact(2n, 1n)]);
    const data = outlineData(p, {opened: null, hovered: null, level: undefined, scheme: 'dark'});
    expect(data.map((d) => d.depth)).toEqual([0, 1, 2]);
  });
});
