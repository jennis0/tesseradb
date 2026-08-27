import {describe, expect, it} from 'vitest';
import {SessionArtifactTable, gridToWorldXY, servedLineage, type Artifact, type ArtifactsProjection} from '@tesseradb/client';
import {outlineData, outlineOf, servedDepths} from '../src/layer.js';

/** The served shape as it is drawn, and which of the served shapes the map actually draws. */

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

/** The area of a closed ring — the shoelace, unsigned. */
const area = (ring: readonly [number, number][]) => {
  let twice = 0;
  for (let i = 0; i < ring.length; i++) {
    const a = ring[i]!;
    const b = ring[(i + 1) % ring.length]!;
    twice += a[0] * b[1] - b[0] * a[1];
  }
  return Math.abs(twice) / 2;
};

/** Whether a point is inside a closed ring — a crossing count, for the containment check below. */
const inside = (p: [number, number], ring: readonly [number, number][]) => {
  let odd = false;
  for (let i = 0, j = ring.length - 1; i < ring.length; j = i++) {
    const [xi, yi] = ring[i]!;
    const [xj, yj] = ring[j]!;
    if (yi > p[1] !== yj > p[1] && p[0] < ((xj - xi) * (p[1] - yi)) / (yj - yi) + xi) odd = !odd;
  }
  return odd;
};

describe('outlineOf', () => {
  it('draws the wire’s own vertices, in order, and covers no ground the served shape does not', () => {
    // A notched ring — the shape the concave (alpha) hull now produces (annotations §4.2). Its
    // reflex corner is where corner cutting used to bulge: Chaikin replaces a corner with a chord
    // between points on the two edges, and at a **reflex** corner the triangle that chord spans
    // lies outside the polygon, so the drawn ring reached into the notch by up to a quarter of the
    // shorter adjacent edge. (The ring's total area still fell — the convex corners take more off
    // than the reflex one puts on — so an area comparison alone would have missed it.)
    const g = 2 ** 32 - 1;
    const notched: [number, number][] = [[0, 0], [g, 0], [g, g], [g / 2, g], [g / 2, g / 2], [0, g / 2]];
    const drawn = outlineOf({...artifact(1n, null), hull: notched})!;
    // The wire's vertices, in the wire's order, through the one grid-to-world conversion.
    expect(drawn).toEqual(notched.map(gridToWorldXY));
    const side = drawn[1]![0];
    // Three quarters of the square: the notch is out, and nothing rounded it back in.
    expect(area(drawn) / (side * side)).toBeCloseTo(0.75, 6);
    // And nothing in the notch is inside the drawn ring — the point three Chaikin rounds put
    // there sits at (0.47, 0.55) of the side, which is where this check is aimed.
    for (const [fx, fy] of [[0.47, 0.55], [0.4, 0.6], [0.25, 0.75], [0.49, 0.51]] as [number, number][]) {
      expect(inside([fx * side, fy * side], drawn)).toBe(false);
    }
    // The interior is still the interior: the three quadrants that are the shape.
    for (const [fx, fy] of [[0.25, 0.25], [0.75, 0.25], [0.75, 0.75]] as [number, number][]) {
      expect(inside([fx * side, fy * side], drawn)).toBe(true);
    }
  });

  it('falls back to the box, and to nothing where the wire carries neither', () => {
    const a = artifact(1n, null);
    expect(outlineOf(a)!.length).toBe(4);
    expect(outlineOf({...a, hull: null})!.length).toBe(4);
    expect(outlineOf({...a, hull: null, box: null})).toBeNull();
    // A degenerate hull is not a polygon, and the box answers instead.
    expect(outlineOf({...a, hull: [[0, 0], [1, 1]]})!.length).toBe(4);
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
