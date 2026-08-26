import {describe, expect, it} from 'vitest';
import {SessionArtifactTable, servedLineage, type Artifact, type ArtifactsProjection} from '@tesseradb/client';
import {MIN_OUTLINE_PX, outlineData, outlineOf, outlinePixels, servedDepths, smoothClosed} from '../src/layer.js';

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

describe('outlineData', () => {
  const meta = (kind: 'flat' | 'nested') =>
    ({layers: [{name: 'clusters', hierarchy: {kind, pruneChildren: false}, depsOn: []}]}) as unknown as import('@tesseradb/client').Meta;
  const alphas = (data: ReturnType<typeof outlineData>) => Object.fromEntries(data.map((d) => [String(d.id), [d.fill, d.line, d.width]]));

  it('a flat layer outlines only the hovered and the opened artifact, and keeps the rest pickable at zero alpha', () => {
    const p = projection([artifact(1n, null), artifact(2n, null), artifact(3n, null)]);
    const none = outlineData(p, meta('flat'), {opened: null, hovered: null, level: undefined, scheme: 'light', zoom: 0});
    expect(none.length).toBe(3);
    expect(none.every((d) => d.flat && d.fill === 0 && d.line === 0)).toBe(true);
    const some = alphas(outlineData(p, meta('flat'), {opened: 2n, hovered: 3n, level: undefined, scheme: 'light', zoom: 0}));
    expect(some['1']).toEqual([0, 0, 0.8]);
    expect(some['2']).toEqual([41, 200, 1.2]);
    expect(some['3']![0]).toBeGreaterThan(0);
    expect(some['3']![1]).toBe(150);
  });

  it('a nested layer keeps its contours: a hairline each, the boards’ 6–10% fill at the leaves and none above', () => {
    const p = projection([artifact(1n, null), artifact(2n, 1n), artifact(3n, 2n)]);
    for (const scheme of ['light', 'dark'] as const) {
      const data = outlineData(p, meta('nested'), {opened: null, hovered: null, level: undefined, scheme, zoom: 0});
      expect(data.map((d) => String(d.id))).toEqual(['1', '2', '3']);
      const leaf = data[2]!;
      expect(leaf.fill / 255).toBeGreaterThanOrEqual(0.06);
      expect(leaf.fill / 255).toBeLessThanOrEqual(0.1);
      expect(leaf.line).toBeGreaterThan(0);
      expect(leaf.line / 255).toBeLessThan(0.25);
      expect(leaf.width).toBe(0.8);
      for (const above of data.slice(0, 2)) {
        expect(above.fill).toBe(0);
        expect(above.line).toBe(leaf.line);
      }
    }
  });

  it('the opened artifact is strong whatever its layer, and the hovered one is firmer than a hairline', () => {
    const p = projection([artifact(1n, null), artifact(2n, 1n)]);
    const data = alphas(outlineData(p, meta('nested'), {opened: 1n, hovered: 2n, level: undefined, scheme: 'dark', zoom: 0}));
    expect(data['1']).toEqual([41, 200, 1.2]);
    expect(data['2']![1]).toBeGreaterThan(56);
    // Hovering the opened one changes nothing: opened wins.
    const same = alphas(outlineData(p, meta('nested'), {opened: 1n, hovered: 1n, level: undefined, scheme: 'dark', zoom: 0}));
    expect(same['1']).toEqual([41, 200, 1.2]);
  });

  it('a level cuts the outlines below it', () => {
    const p = projection([artifact(1n, null), artifact(2n, 1n), artifact(3n, 2n)]);
    const data = outlineData(p, meta('nested'), {opened: null, hovered: null, level: 1, scheme: 'light', zoom: 0});
    expect(data.map((d) => String(d.id))).toEqual(['1', '2']);
    expect(data[1]!.fill).toBeGreaterThan(0);
  });

  it('an outline under 12 px on both axes is kept at zero alpha — it answers a pick and draws nothing', () => {
    // A shard: 2⁻⁸ of the grid a side — two world units — is 8 px at zoom 2 and 32 px at zoom 4.
    const side = 2 ** 24;
    const shard: Artifact = {...artifact(2n, null), box: [0, 0, side, side], hull: [[0, 0], [side, 0], [side, side]]};
    const p = projection([artifact(1n, null), shard]);
    expect(outlinePixels(shard, 2)!.map((v) => Math.round(v))).toEqual([8, 8]);
    expect(outlinePixels(shard, 4)!.map((v) => Math.round(v))).toEqual([32, 32]);
    const at = (zoom: number, opts: Partial<Parameters<typeof outlineData>[2]> = {}) => outlineData(p, meta('nested'), {opened: null, hovered: null, level: undefined, scheme: 'dark', zoom, ...opts});
    const small = at(2);
    expect(small.map((d) => String(d.id))).toEqual(['1', '2']);
    expect(small[1]!.tiny).toBe(true);
    expect([small[1]!.fill, small[1]!.line]).toEqual([0, 0]);
    expect(small[0]!.tiny).toBe(false);
    expect(small[0]!.line).toBeGreaterThan(0);
    // Zoomed in until it is 32 px across, it draws as any leaf does.
    const large = at(4);
    expect(large[1]!.tiny).toBe(false);
    expect(large[1]!.fill).toBeGreaterThan(0);
    expect(large[1]!.line).toBeGreaterThan(0);
    // Tiny on one axis only is not tiny: a long thin hull still draws.
    const strip: Artifact = {...artifact(3n, null), box: [0, 0, side * 8, side], hull: null};
    const thin = outlineData(projection([strip]), meta('nested'), {opened: null, hovered: null, level: undefined, scheme: 'dark', zoom: 2});
    expect(thin[0]!.tiny).toBe(false);
    // Hovered or opened, a tiny outline draws — the pick found it, so it is shown.
    expect(at(2, {hovered: 2n})[1]!.line).toBe(150);
    expect(at(2, {opened: 2n})[1]!.line).toBe(200);
    // The hull's extent stands in where there is no box.
    const hullOnly: Artifact = {...shard, box: null};
    expect(outlinePixels(hullOnly, 2)!.map((v) => Math.round(v))).toEqual([8, 8]);
    expect(MIN_OUTLINE_PX).toBe(12);
  });
});
