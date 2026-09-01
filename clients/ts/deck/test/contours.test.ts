import {describe, expect, it} from 'vitest';
import {SessionArtifactTable, gridToWorldXY, servedLineage, type Artifact, type ArtifactsProjection, type Meta} from '@tesseradb/client';
import {contourShapes, focusOutlines, outlineOf} from '../src/layer.js';
import {ringWithin, shapeContains} from '../src/contours.js';

/** The served shape as it is drawn, and which of the served shapes the map actually draws. */

const artifact = (id: bigint, parent: bigint | null, count = 10n): Artifact => ({
  layer: 'clusters',
  tesseraId: id,
  key: `c-${id}`,
  maskedCount: count,
  centroid: [2 ** 31, 2 ** 31],
  box: [0, 0, 2 ** 32 - 1, 2 ** 32 - 1],
  shape: [[[[0, 0], [2 ** 32 - 1, 0], [2 ** 32 - 1, 2 ** 32 - 1], [0, 2 ** 32 - 1]]]],
  content: [],
  parentIds: parent === null ? [] : [parent],
  rung: 0,
  matched: null
});

/**
 * The layer roster as `/v1/meta` publishes it: each layer's declared computed set — the wire's
 * `computed_content` (contracts §3.2 r42) — the kind of shape it draws (`polygon-membership.md`
 * §7.1), and what it depends on.
 */
const meta = (layers: {name: string; computedContent?: string[]; shape?: 'derived' | 'predicate' | 'authored' | null; depsOn?: string[]}[]): Meta =>
  ({layers: layers.map((l) => ({name: l.name, computedContent: l.computedContent ?? [], shape: l.shape ?? null, depsOn: l.depsOn ?? []}))}) as unknown as Meta;

/**
 * The served set as the wire delivers it (contracts §3.2 r44): on this treed fixture `rung` is the
 * response-local parent-chain depth, computed here exactly as the server computes it after the cut
 * — a root, and a child of an unserved parent, at 0. It stands in for the server; nothing under
 * test derives it again.
 */
function withRungs(served: Artifact[]): Artifact[] {
  const byId = new Map(served.map((a) => [a.tesseraId, a]));
  const depthOf = (a: Artifact, guard = 0): number => {
    const parent = a.parentIds.length === 0 ? undefined : byId.get(a.parentIds[0]!);
    return parent && guard < 1024 ? depthOf(parent, guard + 1) + 1 : 0;
  };
  return served.map((a) => ({...a, rung: depthOf(a)}));
}

function projection(input: Artifact[]): ArtifactsProjection {
  const served = withRungs(input);
  const table = new SessionArtifactTable();
  const ordinals = table.take(served.map((a) => ({tesseraId: a.tesseraId, layer: a.layer, parentIds: a.parentIds, rung: a.rung})));
  return {layer: 'clusters', layers: ['clusters'], served, lineage: servedLineage(served), status: 'shown', refusal: null, version: 1, held: 0, table, servedOrdinals: new Set(ordinals), shapes: new Map(), colours: new Map(), palette: 'positional', coverage: {current: 0, stale: 0}};
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
    const outline = outlineOf({...artifact(1n, null), shape: [[notched]]})!;
    expect(outline.source).toBe('shape');
    expect(outline.parts.length).toBe(1);
    const drawn = outline.parts[0]![0]!;
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

  it('says which shape it answered with, and falls back to the box, then to nothing', () => {
    // The two are indistinguishable by counting vertices — a box is four corners and so is a
    // square hull — and they are drawn differently, so the caller is told rather than left to
    // guess (a box through the smoothing is an oval).
    const a = artifact(1n, null);
    expect(outlineOf(a)!.source).toBe('shape');
    expect(outlineOf({...a, shape: null})!.source).toBe('box');
    expect(outlineOf({...a, shape: null})!.parts.map((p) => p.map((r) => r.length))).toEqual([[4]]);
    expect(outlineOf({...a, shape: null, box: null})).toBeNull();
    // A degenerate group is its own members (`artifact-shapes.md` §1) — a ring of one or two
    // vertices has no area to draw or to pick, so the box answers for the artifact instead.
    expect(outlineOf({...a, shape: [[[[0, 0], [1, 1]]]]})!.source).toBe('box');
    expect(outlineOf({...a, shape: []})!.source).toBe('box');
    // A part whose outer is degenerate goes whole, its holes with it: a surviving hole drawn
    // first would be the polygon.
    expect(outlineOf({...a, shape: [[[[0, 0], [1, 1]], [[2, 2], [3, 2], [3, 3]]]]})!.source).toBe('box');
    // A shape that arrived by identifier wins over the artifact's own, and is still a shape.
    const fetched = outlineOf({...a, shape: null}, [[[[0, 0], [1, 0], [1, 1]]]])!;
    expect([fetched.source, fetched.parts.length]).toEqual(['shape', 1]);
  });

  it('draws every part of a shape, and drops only the degenerate ones', () => {
    const g = 2 ** 32 - 1;
    const left: [number, number][] = [[0, 0], [g / 4, 0], [g / 4, g / 4], [0, g / 4]];
    const right: [number, number][] = [[(3 * g) / 4, (3 * g) / 4], [g, (3 * g) / 4], [g, g], [(3 * g) / 4, g]];
    // Two separated clouds and a one-member group: the wire's three parts, of which two draw.
    const parts = outlineOf({...artifact(1n, null), shape: [[left], [right], [[[g / 2, g / 2]]]]})!.parts;
    expect(parts).toEqual([[left.map(gridToWorldXY)], [right.map(gridToWorldXY)]]);
    // No part reaches the ground between them, which is the whole reason the wire is nested.
    const middle: [number, number] = [gridToWorldXY([g / 2, g / 2])[0], gridToWorldXY([g / 2, g / 2])[1]];
    for (const part of parts) expect(inside(middle, part[0]!)).toBe(false);
  });

  it('keeps a hole with its part — a boundary’s enclave is not a second shape', () => {
    // A part is its outer ring and then its holes (`polygon-membership.md` §7.1): one drawn
    // polygon with a hole, which is what a renderer's polygon-with-holes takes. Flattened to a
    // ring list, the hole would draw as a second, smaller shape over the first.
    const g = 2 ** 32 - 1;
    const outer: [number, number][] = [[0, 0], [g, 0], [g, g], [0, g]];
    const hole: [number, number][] = [[g / 4, g / 4], [(3 * g) / 4, g / 4], [(3 * g) / 4, (3 * g) / 4], [g / 4, (3 * g) / 4]];
    const outline = outlineOf({...artifact(1n, null), shape: [[outer, hole]]})!;
    expect(outline.parts.length).toBe(1);
    expect(outline.parts[0]!.map((r) => r.length)).toEqual([4, 4]);
    // A degenerate hole is dropped and the part stays.
    expect(outlineOf({...artifact(1n, null), shape: [[outer, [[1, 1], [2, 2]]]]})!.parts[0]!.length).toBe(1);
  });
});

describe('contourShapes — what may be hovered', () => {
  const ids = (a: ArtifactsProjection, o: {level?: number; meta?: Meta} = {}) =>
    contourShapes(a, {level: o.level, meta: o.meta ?? null}).map((s) => String(s.id));

  it('holds the frontier and nothing above it — an ancestor answers nothing', () => {
    // Three levels of one chain. Only the leaf is on the map, so only the leaf may be pointed at:
    // an artifact nobody can see is not a thing a viewer can point at (the owner's review,
    // 2026-08-27). Keeping the ancestors made the hover flip between a cluster and its
    // sub-cluster as the pointer crossed the child's ring inside the parent's.
    const p = projection([artifact(1n, null), artifact(2n, 1n), artifact(3n, 2n)]);
    expect(ids(p)).toEqual(['3']);
    // A flat layer is all frontier.
    expect(ids(projection([artifact(1n, null), artifact(2n, null), artifact(3n, null)]))).toEqual(['1', '2', '3']);
  });

  it('a level moves the frontier up: the deepest artifact the level admits answers', () => {
    const p = projection([artifact(1n, null), artifact(2n, 1n), artifact(3n, 2n)]);
    expect(ids(p, {level: 1})).toEqual(['2']);
    // A branch that stops above the level is still on the frontier — `frontier`'s own rule.
    const stops = projection([artifact(1n, null), artifact(2n, 1n), artifact(3n, 2n), artifact(4n, 1n)]);
    expect(new Set(ids(stops, {level: 1}))).toEqual(new Set(['2', '4']));
  });

  it('leaves a dependent layer’s artifacts out — they have no shape, and their box is not one', () => {
    // A clustering's topic labels carry no shape, so `outlineOf` would fall back to their box and
    // put a rectangle over the map with nothing drawn on it, hoverable and pointing at a thing
    // the viewer cannot see. Their text is drawn beneath the name they attach to (§5.10, D13).
    const topic: Artifact = {...artifact(9n, null), layer: 'topics', shape: null};
    const p = projection([artifact(1n, null), topic]);
    expect(ids(p, {meta: meta([{name: 'clusters'}, {name: 'topics', depsOn: ['clusters']}])})).toEqual(['1']);
    // With no roster to say which layer depends on which, every served layer answers.
    expect(ids(p)).toEqual(['1', '9']);
  });

  it('is one entry per artifact, gathering its parts — the unit a hover answers in', () => {
    const g = 2 ** 32 - 1;
    const left: [number, number][] = [[0, 0], [g / 4, 0], [g / 4, g / 4], [0, g / 4]];
    const right: [number, number][] = [[(3 * g) / 4, (3 * g) / 4], [g, (3 * g) / 4], [g, g], [(3 * g) / 4, g]];
    const flat = projection([{...artifact(1n, null), shape: [[left], [right]]}, artifact(2n, null)]);
    const shapes = contourShapes(flat, {level: undefined});
    expect(shapes.map((s) => [String(s.id), s.parts.length, s.rung])).toEqual([['1', 2, 0], ['2', 1, 0]]);
    // The box is the parts', so a pointer between two separated groups is in neither.
    const one = shapes[0]!;
    expect(shapeContains(one, gridToWorldXY([g / 8, g / 8]))).toBe(true);
    expect(shapeContains(one, gridToWorldXY([g / 2, g / 2]))).toBe(false);
  });

  it('a pointer in a hole is outside the part, and inside a hole’s own island', () => {
    // Even-odd over a part's rings: an enclave is not the enclosing artifact, and a served
    // island inside it — its own part — is.
    const g = 2 ** 32 - 1;
    const outer: [number, number][] = [[0, 0], [g, 0], [g, g], [0, g]];
    const hole: [number, number][] = [[g / 4, g / 4], [(3 * g) / 4, g / 4], [(3 * g) / 4, (3 * g) / 4], [g / 4, (3 * g) / 4]];
    const island: [number, number][] = [[(3 * g) / 8, (3 * g) / 8], [(5 * g) / 8, (3 * g) / 8], [(5 * g) / 8, (5 * g) / 8], [(3 * g) / 8, (5 * g) / 8]];
    const p = projection([{...artifact(1n, null), shape: [[outer, hole], [island]]}]);
    const one = contourShapes(p, {level: undefined})[0]!;
    expect(shapeContains(one, gridToWorldXY([g / 8, g / 8]))).toBe(true);
    expect(shapeContains(one, gridToWorldXY([(5 * g) / 16, (5 * g) / 16]))).toBe(false);
    expect(shapeContains(one, gridToWorldXY([g / 2, g / 2]))).toBe(true);
  });

  it('answers against the box until the shape fetched by identifier arrives', () => {
    // The viewport carries no shape (`artifactChannel.ts` asks for centroid and box), so a served
    // row's shape is its box until `TesseraStore.needShape` answers for it — which is what the
    // hover over that box asks for.
    const g = 2 ** 32 - 1;
    const ring: [number, number][] = [[0, 0], [g / 2, 0], [g / 2, g / 2], [g / 4, g / 3], [0, g / 2]];
    const p = projection([{...artifact(1n, null), shape: null}, {...artifact(2n, null), shape: null}]);
    expect(contourShapes(p, {level: undefined}).map((s) => s.parts[0]![0]!.length)).toEqual([4, 4]);
    const after = contourShapes({...p, shapes: new Map([[1n, [[ring]]]])}, {level: undefined});
    expect(after.find((s) => s.id === 1n)!.parts[0]![0]).toEqual(ring.map(gridToWorldXY));
    expect(after.find((s) => s.id === 2n)!.parts[0]![0]!.length).toBe(4);
  });

  it('carries the wire’s own vertices — the served ring, never the drawn curve', () => {
    const g = 2 ** 32 - 1;
    const notched: [number, number][] = [[0, 0], [g, 0], [g, g], [g / 2, g], [g / 2, g / 2], [0, g / 2]];
    const p = projection([{...artifact(1n, null), shape: [[notched]]}]);
    expect(contourShapes(p, {level: undefined})[0]!.parts[0]![0]).toEqual(notched.map(gridToWorldXY));
  });
});

describe('focusOutlines — what draws', () => {
  const rows = (data: ReturnType<typeof focusOutlines>) => Object.fromEntries(data.map((d) => [String(d.id), [d.fill, d.line, d.width]]));
  const options = (o: Partial<Parameters<typeof focusOutlines>[1]>) => ({opened: null, hovered: null, level: undefined, scheme: 'light' as const, ...o});

  it('holds the hovered and the opened artifact, and nothing else', () => {
    // The rest of the frontier used to be in this data at zero alpha so that it answered deck's
    // pick, which put every served ring through the tessellator on every hover change. The pick
    // is resolved against `contourShapes` now, so the layer holds only what draws.
    const p = projection([artifact(1n, null), artifact(2n, null), artifact(3n, null)]);
    expect(focusOutlines(p, options({}))).toEqual([]);
    expect(focusOutlines(p, options({hovered: 2n})).map((d) => String(d.id))).toEqual(['2']);
    expect(focusOutlines(p, options({opened: 1n, hovered: 3n})).map((d) => String(d.id))).toEqual(['1', '3']);
  });

  it('the opened artifact is strong, the hovered one fainter, and opened wins where they are one', () => {
    const p = projection([artifact(1n, null), artifact(2n, null)]);
    const data = rows(focusOutlines(p, options({opened: 1n, hovered: 2n, scheme: 'dark'})));
    expect(data['1']).toEqual([41, 200, 1.2]);
    expect(data['2']![1]).toBe(150);
    expect(data['2']![0]).toBeGreaterThan(0);
    const same = focusOutlines(p, options({opened: 1n, hovered: 1n, scheme: 'dark'}));
    expect(same.map((d) => [String(d.id), d.opened, d.fill, d.line])).toEqual([['1', true, 41, 200]]);
  });

  it('draws nothing for an artifact that is not on the frontier, or that no response served', () => {
    const p = projection([artifact(1n, null), artifact(2n, 1n)]);
    // 1 is an ancestor of 2: it is not on the map, so opening it draws nothing.
    expect(focusOutlines(p, options({opened: 1n}))).toEqual([]);
    expect(focusOutlines(p, options({opened: 2n})).map((d) => String(d.id))).toEqual(['2']);
    expect(focusOutlines(p, options({hovered: 404n}))).toEqual([]);
    // A level moves the frontier up, and with it what may draw.
    expect(focusOutlines(p, options({opened: 2n, level: 0}))).toEqual([]);
    expect(focusOutlines(p, options({opened: 1n, level: 0})).map((d) => String(d.id))).toEqual(['1']);
  });

  it('leaves a dependent layer’s artifacts out — their box is not a shape', () => {
    const topic: Artifact = {...artifact(9n, null), layer: 'topics', shape: null};
    const p = projection([artifact(1n, null), topic]);
    const roster = meta([{name: 'clusters'}, {name: 'topics', depsOn: ['clusters']}]);
    expect(focusOutlines(p, options({hovered: 9n, meta: roster}))).toEqual([]);
    expect(focusOutlines(p, options({hovered: 9n})).map((d) => String(d.id))).toEqual(['9']);
  });

  it('gives one row per part, both drawing alike, parents ordered first', () => {
    const g = 2 ** 32 - 1;
    const left: [number, number][] = [[0, 0], [g / 4, 0], [g / 4, g / 4], [0, g / 4]];
    const right: [number, number][] = [[(3 * g) / 4, (3 * g) / 4], [g, (3 * g) / 4], [g, g], [(3 * g) / 4, g]];
    const p = projection([{...artifact(1n, null), shape: [[left], [right]]}, artifact(2n, null)]);
    // Both of the opened artifact's parts draw alike: the parts are one shape in pieces, and
    // highlighting half of a cluster would say something false about where its members are.
    const opened = focusOutlines(p, options({opened: 1n, scheme: 'dark'}));
    expect(opened.map((d) => [d.fill, d.line])).toEqual([[41, 200], [41, 200]]);
    expect(opened[0]!.polygon).not.toEqual(opened[1]!.polygon);
    // A hovered child over an opened parent: the parent's rows come first, so the child draws over.
    const nested = projection([artifact(1n, null), artifact(2n, 1n), artifact(3n, 1n)]);
    expect(focusOutlines(nested, options({opened: 3n, hovered: 2n})).map((d) => d.rung)).toEqual([1, 1]);
  });

  it('smooths a derived shape and never a box: four corners through the spline is an oval', () => {
    const g = 2 ** 32 - 1;
    const notched: [number, number][] = [[0, 0], [g, 0], [g, g], [g / 2, g], [g / 2, g / 2], [0, g / 2]];
    const p = projection([{...artifact(1n, null), shape: [[notched]]}, {...artifact(2n, null), shape: null}]);
    const drawn = focusOutlines(p, options({hovered: 1n}))[0]!;
    expect(drawn.source).toBe('shape');
    expect(drawn.polygon[0]!.length).toBe(notched.length * 4);
    // The smoothed curve is a summary of the served ring, not a claim beyond it at the convex
    // corners: every vertex of the served ring's own quadrants stays where the members are.
    expect(ringWithin(notched.map(gridToWorldXY), notched.map(gridToWorldXY))).toBe(true);
    // The box: the wire's four corners, unchanged and unsmoothed, and drawn as a hairline with
    // no fill — a box is the bounds of the visible members and not their shape.
    const box = focusOutlines(p, options({hovered: 2n}))[0]!;
    expect(box.source).toBe('box');
    expect(box.polygon).toEqual([[
      gridToWorldXY([0, 0]),
      gridToWorldXY([g, 0]),
      gridToWorldXY([g, g]),
      gridToWorldXY([0, g])
    ]]);
    expect([box.fill, box.line, box.width]).toEqual([0, 150, 0.8]);
    expect(focusOutlines(p, options({opened: 2n}))[0]!.width).toBe(0.8);
  });

  it('draws a predicate or an authored shape as the wire sent it — unsmoothed, holes kept — in the hull’s style', () => {
    // A boundary somebody drew is already generalised to the pixel by the server's vertex rule
    // (`polygon-membership.md` §7.2); a spline through it would move a border and could cross its
    // own holes. It draws through the same path and in the same style as a hull.
    const g = 2 ** 32 - 1;
    const outer: [number, number][] = [[0, 0], [g, 0], [g, g], [0, g]];
    const hole: [number, number][] = [[g / 4, g / 4], [(3 * g) / 4, g / 4], [(3 * g) / 4, (3 * g) / 4], [g / 4, (3 * g) / 4]];
    const p = projection([{...artifact(1n, null), shape: [[outer, hole]]}]);
    for (const kind of ['predicate', 'authored'] as const) {
      const roster = meta([{name: 'clusters', computedContent: ['centroid', 'box'], shape: kind}]);
      const drawn = focusOutlines(p, options({opened: 1n, scheme: 'dark', meta: roster}));
      expect(drawn.length).toBe(1);
      expect(drawn[0]!.polygon).toEqual([outer.map(gridToWorldXY), hole.map(gridToWorldXY)]);
      expect([drawn[0]!.fill, drawn[0]!.line, drawn[0]!.width]).toEqual([41, 200, 1.2]);
    }
    // The derived kind is smoothed, every ring of the part.
    const derived = meta([{name: 'clusters', computedContent: ['centroid', 'box', 'hull'], shape: 'derived'}]);
    const smoothed = focusOutlines(p, options({opened: 1n, meta: derived}))[0]!;
    expect(smoothed.polygon.map((r) => r.length)).toEqual([16, 16]);
  });

  it('draws nothing where the layer draws a shape and none has arrived, and the shape when it has', () => {
    // The viewport carries no shape, so a served row on a shape-drawing layer has a box and a
    // shape on its way by identifier. A rectangle that becomes the shape a moment later reads as
    // the shape changing under the pointer, so nothing is drawn until it lands — while the hover
    // still resolves against that box, which is what asks for the shape.
    const g = 2 ** 32 - 1;
    const ring: [number, number][] = [[0, 0], [g / 2, 0], [g / 2, g / 2], [g / 4, g / 3], [0, g / 2]];
    const p = projection([{...artifact(1n, null), shape: null}]);
    for (const kind of ['derived', 'predicate', 'authored'] as const) {
      const declares = meta([{name: 'clusters', computedContent: ['centroid', 'box'], shape: kind}]);
      expect(focusOutlines(p, options({hovered: 1n, meta: declares}))).toEqual([]);
      expect(contourShapes(p, {level: undefined, meta: declares}).map((s) => String(s.id))).toEqual(['1']);
      const arrived = focusOutlines({...p, shapes: new Map([[1n, [[ring]]]])}, options({hovered: 1n, meta: declares}));
      expect(arrived.length).toBe(1);
      expect(arrived[0]!.source).toBe('shape');
      expect(arrived[0]!.polygon[0]!.length).toBe(kind === 'derived' ? ring.length * 4 : ring.length);
    }
    // A layer that draws no shape draws its box, which is its shape for good.
    const plain = meta([{name: 'clusters', computedContent: ['centroid', 'box'], shape: null}]);
    expect(focusOutlines(p, options({hovered: 1n, meta: plain})).map((d) => d.polygon[0]!.length)).toEqual([4]);
    // With no roster in hand the box draws: a client that cannot read the declaration draws what
    // the wire gave it rather than nothing at all.
    expect(focusOutlines(p, options({hovered: 1n})).map((d) => d.polygon[0]!.length)).toEqual([4]);
  });
});
