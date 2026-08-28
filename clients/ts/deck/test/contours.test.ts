import {describe, expect, it} from 'vitest';
import {SessionArtifactTable, gridToWorldXY, servedLineage, type Artifact, type ArtifactsProjection, type Meta} from '@tesseradb/client';
import {hoverShapes, outlineData, outlineOf} from '../src/layer.js';
import {ringWithin, shapeContains} from '../src/contours.js';

/** The served shape as it is drawn, and which of the served shapes the map actually draws. */

const artifact = (id: bigint, parentId: bigint | null, count = 10n): Artifact => ({
  layer: 'clusters',
  tesseraId: id,
  key: `c-${id}`,
  maskedCount: count,
  centroid: [2 ** 31, 2 ** 31],
  box: [0, 0, 2 ** 32 - 1, 2 ** 32 - 1],
  hull: [[[0, 0], [2 ** 32 - 1, 0], [2 ** 32 - 1, 2 ** 32 - 1], [0, 2 ** 32 - 1]]],
  content: [],
  parentId,
  rung: 0,
  matched: null
});

/**
 * The served set as the wire delivers it (contracts §3.2 r43): on this treed fixture `rung` is the
 * response-local parent-chain depth, computed here exactly as the server computes it after the cut
 * — a root, and a child of an unserved parent, at 0. It stands in for the server; nothing under
 * test derives it again.
 */
function withRungs(served: Artifact[]): Artifact[] {
  const byId = new Map(served.map((a) => [a.tesseraId, a]));
  const depthOf = (a: Artifact, guard = 0): number => {
    const parent = a.parentId === null ? undefined : byId.get(a.parentId);
    return parent && guard < 1024 ? depthOf(parent, guard + 1) + 1 : 0;
  };
  return served.map((a) => ({...a, rung: depthOf(a)}));
}

function projection(input: Artifact[]): ArtifactsProjection {
  const served = withRungs(input);
  const table = new SessionArtifactTable();
  const ordinals = table.take(served.map((a) => ({tesseraId: a.tesseraId, layer: a.layer, parentId: a.parentId, rung: a.rung})));
  return {layer: 'clusters', layers: ['clusters'], served, lineage: servedLineage(served), status: 'shown', refusal: null, version: 1, held: 0, table, servedOrdinals: new Set(ordinals), colours: new Map(), palette: 'positional', coverage: {current: 0, stale: 0}};
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
    const rings = outlineOf({...artifact(1n, null), hull: [notched]})!;
    expect(rings.length).toBe(1);
    const drawn = rings[0]!;
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
    expect(outlineOf(a)!.map((r) => r.length)).toEqual([4]);
    expect(outlineOf({...a, hull: null})!.map((r) => r.length)).toEqual([4]);
    expect(outlineOf({...a, hull: null, box: null})).toBeNull();
    // A degenerate group is its own members (`artifact-shapes.md` §1) — a ring of one or two
    // vertices has no area to draw or to pick, so the box answers for the artifact instead.
    expect(outlineOf({...a, hull: [[[0, 0], [1, 1]]]})!.map((r) => r.length)).toEqual([4]);
    expect(outlineOf({...a, hull: []})!.map((r) => r.length)).toEqual([4]);
  });

  it('draws every ring of a hull, and drops only the degenerate ones', () => {
    const g = 2 ** 32 - 1;
    const left: [number, number][] = [[0, 0], [g / 4, 0], [g / 4, g / 4], [0, g / 4]];
    const right: [number, number][] = [[(3 * g) / 4, (3 * g) / 4], [g, (3 * g) / 4], [g, g], [(3 * g) / 4, g]];
    // Two separated clouds and a one-member group: the wire's three rings, of which two draw.
    const rings = outlineOf({...artifact(1n, null), hull: [left, right, [[g / 2, g / 2]]]})!;
    expect(rings).toEqual([left.map(gridToWorldXY), right.map(gridToWorldXY)]);
    // No ring reaches the ground between them, which is the whole reason the wire is nested.
    const middle: [number, number] = [gridToWorldXY([g / 2, g / 2])[0], gridToWorldXY([g / 2, g / 2])[1]];
    for (const ring of rings) expect(inside(middle, ring)).toBe(false);
  });
});

describe('outlineData', () => {
  const alphas = (data: ReturnType<typeof outlineData>) => Object.fromEntries(data.map((d) => [String(d.id), [d.fill, d.line, d.width]]));

  it('holds the frontier and nothing above it — an ancestor draws nothing and answers nothing', () => {
    // Three levels of one chain. Only the leaf is on the map, so only the leaf is in the data:
    // an artifact nobody can see is not a thing a viewer can point at (the owner's review,
    // 2026-08-27). Keeping the ancestors here at zero alpha is what made the hover flip between
    // a cluster and its sub-cluster as the pointer crossed the child's ring inside the parent's.
    const p = projection([artifact(1n, null), artifact(2n, 1n), artifact(3n, 2n)]);
    for (const scheme of ['light', 'dark'] as const) {
      const rest = outlineData(p, {opened: null, hovered: null, level: undefined, scheme});
      expect(rest.map((d) => String(d.id))).toEqual(['3']);
      expect(rest.every((d) => d.fill === 0 && d.line === 0)).toBe(true);
      // It still carries its polygon, which is what answers a pick and what the hover reads.
      expect(rest.every((d) => d.polygon.length >= 3)).toBe(true);
    }
    const some = alphas(outlineData(p, {opened: 3n, hovered: null, level: undefined, scheme: 'light'}));
    expect(some['3']).toEqual([41, 200, 1.2]);
    // Opening an ancestor draws nothing: it is not on the map to be opened.
    expect(outlineData(p, {opened: 1n, hovered: null, level: undefined, scheme: 'light'}).length).toBe(1);
  });

  it('a flat layer is all frontier — every artifact draws and answers', () => {
    const p = projection([artifact(1n, null), artifact(2n, null), artifact(3n, null)]);
    const none = outlineData(p, {opened: null, hovered: null, level: undefined, scheme: 'light'});
    expect(none.length).toBe(3);
    expect(none.every((d) => d.fill === 0 && d.line === 0)).toBe(true);
    const some = alphas(outlineData(p, {opened: 2n, hovered: 3n, level: undefined, scheme: 'light'}));
    expect(some['2']).toEqual([41, 200, 1.2]);
    expect(some['3']![1]).toBe(150);
  });

  it('the opened artifact is strong, and the hovered one firmer than nothing', () => {
    const p = projection([artifact(1n, null), artifact(2n, null)]);
    const data = alphas(outlineData(p, {opened: 1n, hovered: 2n, level: undefined, scheme: 'dark'}));
    expect(data['1']).toEqual([41, 200, 1.2]);
    expect(data['2']![1]).toBe(150);
    expect(data['2']![0]).toBeGreaterThan(0);
    // Hovering the opened one changes nothing: opened wins.
    const same = alphas(outlineData(p, {opened: 1n, hovered: 1n, level: undefined, scheme: 'dark'}));
    expect(same['1']).toEqual([41, 200, 1.2]);
  });

  it('a level moves the frontier up: the deepest artifact the level admits draws', () => {
    const p = projection([artifact(1n, null), artifact(2n, 1n), artifact(3n, 2n)]);
    const data = outlineData(p, {opened: null, hovered: null, level: 1, scheme: 'light'});
    expect(data.map((d) => String(d.id))).toEqual(['2']);
    expect(data.every((d) => d.fill === 0 && d.line === 0)).toBe(true);
    const hovered = outlineData(p, {opened: null, hovered: 2n, level: 1, scheme: 'light'});
    expect(hovered.find((d) => d.id === 2n)!.line).toBe(150);
    // A branch that stops above the level is still on the frontier — `frontier`'s own rule.
    const stops = projection([artifact(1n, null), artifact(2n, 1n), artifact(3n, 2n), artifact(4n, 1n)]);
    expect(new Set(outlineData(stops, {opened: null, hovered: null, level: 1, scheme: 'light'}).map((d) => String(d.id)))).toEqual(new Set(['2', '4']));
  });

  it('leaves a dependent layer’s artifacts out — they have no shape, and their box is not one', () => {
    // A clustering's topic labels carry no hull, so `outlineOf` would fall back to their box and
    // put a rectangle over the map with nothing drawn on it, hoverable and pointing at a thing
    // the viewer cannot see. Their text is drawn beneath the name they attach to (§5.10, D13).
    const topic: Artifact = {...artifact(9n, null), layer: 'topics', hull: null};
    const p = projection([artifact(1n, null), topic]);
    const meta = {layers: [{name: 'clusters', depsOn: []}, {name: 'topics', depsOn: ['clusters']}]} as unknown as Meta;
    expect(outlineData(p, {opened: null, hovered: null, level: undefined, scheme: 'light', meta}).map((d) => String(d.id))).toEqual(['1']);
    // With no roster to say which layer depends on which, every served layer draws.
    expect(outlineData(p, {opened: null, hovered: null, level: undefined, scheme: 'light'}).map((d) => String(d.id))).toEqual(['1', '9']);
  });

  it('gives one row per ring, every row carrying the artifact — the pick’s row-to-artifact map', () => {
    const g = 2 ** 32 - 1;
    const left: [number, number][] = [[0, 0], [g / 4, 0], [g / 4, g / 4], [0, g / 4]];
    const right: [number, number][] = [[(3 * g) / 4, (3 * g) / 4], [g, (3 * g) / 4], [g, g], [(3 * g) / 4, g]];
    const two = {...artifact(1n, null), hull: [left, right]};
    const p = projection([two, artifact(2n, null)]);
    const data = outlineData(p, {opened: null, hovered: null, level: undefined, scheme: 'dark'});
    // Three rows for two artifacts. An `artifactIds` array built from the served set would be one
    // short here, and every pick past the first artifact would answer the wrong cluster.
    expect(data.length).toBe(3);
    expect(data.map((d) => String(d.id))).toEqual(['1', '1', '2']);
    // Undrawn, so these are the wire's own vertices — which is what the hover reads.
    expect(data.filter((d) => d.id === 1n).map((d) => d.polygon)).toEqual([left.map(gridToWorldXY), right.map(gridToWorldXY)]);
    // Both of the opened artifact's rings draw alike: the rings are one shape in pieces, and
    // highlighting half of a cluster would say something false about where its members are.
    const opened = outlineData(p, {opened: 1n, hovered: null, level: undefined, scheme: 'dark'}).filter((d) => d.id === 1n);
    expect(opened.map((d) => [d.fill, d.line])).toEqual([[41, 200], [41, 200]]);
    expect(opened[0]!.polygon).not.toEqual(opened[1]!.polygon);
  });

  it('smooths the rings that draw, and only those, and the smoothed ring stays inside the served one', () => {
    const g = 2 ** 32 - 1;
    // A notched ring: the reflex corner an unguarded corner cut used to bulge across.
    const notched: [number, number][] = [[0, 0], [g, 0], [g, g], [g / 2, g], [g / 2, g / 2], [0, g / 2]];
    const p = projection([{...artifact(1n, null), hull: [notched]}, artifact(2n, null)]);
    const rest = outlineData(p, {opened: null, hovered: null, level: undefined, scheme: 'light'});
    expect(rest.find((d) => d.id === 1n)!.polygon).toEqual(notched.map(gridToWorldXY));
    for (const o of [{opened: 1n, hovered: null}, {opened: null, hovered: 1n}]) {
      const data = outlineData(p, {...o, level: undefined, scheme: 'light'});
      const drawn = data.find((d) => d.id === 1n)!.polygon;
      const source = notched.map(gridToWorldXY);
      expect(drawn.length).toBeGreaterThan(source.length);
      expect(ringWithin(drawn, source)).toBe(true);
      // The artifact that does not draw keeps the wire's vertices — smoothing is per drawn shape.
      expect(data.find((d) => d.id === 2n)!.polygon.length).toBe(4);
    }
  });

  it('a ring of one artifact overlapping a ring of another is answered by its own row', () => {
    // Two rings of one artifact may overlap (`artifact-shapes.md` §1), and so may rings of two
    // artifacts. Nothing here dedupes by id: the pick reads a row, and the row names the artifact.
    const g = 2 ** 32 - 1;
    const square: [number, number][] = [[0, 0], [g / 2, 0], [g / 2, g / 2], [0, g / 2]];
    const overlapping: [number, number][] = [[g / 4, g / 4], [(3 * g) / 4, g / 4], [(3 * g) / 4, (3 * g) / 4], [g / 4, (3 * g) / 4]];
    const p = projection([{...artifact(1n, null), hull: [square, overlapping]}]);
    const data = outlineData(p, {opened: null, hovered: null, level: undefined, scheme: 'light'});
    expect(data.length).toBe(2);
    expect(new Set(data.map((d) => d.id))).toEqual(new Set([1n]));
  });

  it('parents are ordered before their children, so an opened child draws over an opened parent', () => {
    // Two branches, so both a rung-0 and a rung-2 artifact are on the frontier; the rung is the wire's.
    const p = projection([artifact(3n, 2n), artifact(1n, null), artifact(2n, 1n)]);
    const data = outlineData(p, {opened: null, hovered: null, level: undefined, scheme: 'dark'});
    expect(data.map((d) => d.rung)).toEqual([2]);
    const branched = projection([artifact(3n, 2n), artifact(1n, null), artifact(2n, 1n), artifact(4n, null)]);
    expect(outlineData(branched, {opened: null, hovered: null, level: undefined, scheme: 'dark'}).map((d) => d.rung)).toEqual([0, 2]);
  });
});

describe('hoverShapes', () => {
  it('is one entry per artifact, gathering its rings — the unit a hover answers in', () => {
    const g = 2 ** 32 - 1;
    const left: [number, number][] = [[0, 0], [g / 4, 0], [g / 4, g / 4], [0, g / 4]];
    const right: [number, number][] = [[(3 * g) / 4, (3 * g) / 4], [g, (3 * g) / 4], [g, g], [(3 * g) / 4, g]];
    const p = projection([{...artifact(1n, null), hull: [left, right]}, artifact(2n, 1n)]);
    const shapes = hoverShapes(outlineData(p, {opened: null, hovered: null, level: undefined, scheme: 'dark'}));
    // Artifact 1 is an ancestor here, so the only shape is its child's.
    expect(shapes.map((s) => String(s.id))).toEqual(['2']);
    const flat = projection([{...artifact(1n, null), hull: [left, right]}, artifact(2n, null)]);
    const both = hoverShapes(outlineData(flat, {opened: null, hovered: null, level: undefined, scheme: 'dark'}));
    expect(both.map((s) => [String(s.id), s.rings.length, s.rung])).toEqual([['1', 2, 0], ['2', 1, 0]]);
    // The box is the rings', so a pointer between two separated groups is in neither.
    const one = both[0]!;
    expect(shapeContains(one, gridToWorldXY([g / 8, g / 8]))).toBe(true);
    expect(shapeContains(one, gridToWorldXY([g / 2, g / 2]))).toBe(false);
  });
});
