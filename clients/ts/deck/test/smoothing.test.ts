import {describe, expect, it} from 'vitest';
import {cutCorners, distanceToRing, hoverAt, pointInRing, ringWithin, shapeBbox, signedArea2, smoothRing, type ContourShape, type Ring} from '../src/contours.js';

/**
 * The smoothing, and the containment it is built to keep.
 *
 * **Containment is the test, not area.** The corner cutting that was deleted from `layer.ts` took
 * area off at convex corners and put it on at reflex ones; its own note records that the total
 * *fell* while the boundary crossed into ground with no visible member in it. Every claim below
 * is about where the boundary is.
 */

/** A ring's unsigned area — used only to say that a smoothed ring is smaller, never that it is right. */
const area = (ring: readonly [number, number][]) => Math.abs(signedArea2(ring)) / 2;

/** The interior angle at each vertex, in degrees — the measure of "smooth" this file uses. */
function turns(ring: readonly [number, number][]): number[] {
  const out: number[] = [];
  for (let i = 0; i < ring.length; i++) {
    const p = ring[(i + ring.length - 1) % ring.length]!;
    const v = ring[i]!;
    const q = ring[(i + 1) % ring.length]!;
    const a = Math.atan2(v[1] - p[1], v[0] - p[0]);
    const b = Math.atan2(q[1] - v[1], q[0] - v[0]);
    let d = ((b - a) * 180) / Math.PI;
    while (d > 180) d -= 360;
    while (d < -180) d += 360;
    out.push(Math.abs(d));
  }
  return out;
}

/** The notched square that caught the deleted implementation: three quadrants of a unit square. */
const NOTCHED: Ring = [
  [0, 0],
  [1, 0],
  [1, 1],
  [0.5, 1],
  [0.5, 0.5],
  [0, 0.5]
];

/** A star: five deep reflex corners between five sharp convex ones. */
function star(points = 5, outer = 1, inner = 0.38): Ring {
  const ring: Ring = [];
  for (let i = 0; i < points * 2; i++) {
    const r = i % 2 === 0 ? outer : inner;
    const t = (Math.PI * i) / points;
    ring.push([r * Math.cos(t), r * Math.sin(t)]);
  }
  return ring;
}

/** A blob with no reflex corner at all — the shape a hull of one dense cloud produces. */
function circle(n = 24, r = 1): Ring {
  return Array.from({length: n}, (_, i): [number, number] => [r * Math.cos((2 * Math.PI * i) / n), r * Math.sin((2 * Math.PI * i) / n)]);
}

describe('smoothRing', () => {
  it('stays inside the source ring on the notched square that caught the deleted cutting', () => {
    const smoothed = smoothRing(NOTCHED);
    expect(ringWithin(smoothed, NOTCHED)).toBe(true);
    // The notch is the quadrant x < 0.5, y > 0.5, and nothing drawn may reach into it. (0.47, 0.55)
    // is where three unguarded Chaikin rounds put the boundary — the failure this replaces.
    for (const p of [[0.47, 0.55], [0.4, 0.6], [0.25, 0.75], [0.49, 0.51]] as [number, number][]) {
      expect(pointInRing(p, smoothed)).toBe(false);
    }
    // The three quadrants that are the shape are still the shape.
    for (const p of [[0.25, 0.25], [0.75, 0.25], [0.75, 0.75]] as [number, number][]) {
      expect(pointInRing(p, smoothed)).toBe(true);
    }
    // And the reflex corner is still where the wire put it: a notch the members leave is not
    // rounded away, in either direction.
    expect(smoothed.some((p) => Math.abs(p[0] - 0.5) < 1e-12 && Math.abs(p[1] - 0.5) < 1e-12)).toBe(true);
  });

  it('stays inside a star’s five reflex corners, where cutting every corner would not', () => {
    const source = star();
    const smoothed = smoothRing(source);
    expect(ringWithin(smoothed, source)).toBe(true);
    // What the unguarded cut does, for contrast: cutting every corner regardless leaves the ring.
    const naive = chaikin(source, 3);
    expect(ringWithin(naive, source)).toBe(false);
  });

  it('is visibly smoother where the shape is convex — the corners open out', () => {
    const source = circle(16);
    const smoothed = smoothRing(source);
    const before = Math.max(...turns(source));
    const after = Math.max(...turns(smoothed));
    // A 16-gon turns 22.5° a corner; three rounds put every turn under a fifth of that.
    expect(before).toBeGreaterThan(20);
    expect(after).toBeLessThan(before / 5);
    expect(ringWithin(smoothed, source)).toBe(true);
    // Convex all through, so nothing is kept: two new vertices a corner, three rounds.
    expect(smoothed.length).toBe(source.length * 8);
  });

  it('leaves a reflex corner sharp and smooths the convex ones around it', () => {
    const smoothed = smoothRing(NOTCHED);
    const sharp = turns(smoothed).filter((t) => t > 45);
    // One corner is still a right angle — the reflex one — and every other turn is gentle.
    expect(sharp.length).toBe(1);
    expect(sharp[0]).toBeCloseTo(90, 6);
  });

  it('never grows: a smoothed ring’s area is at most the source’s', () => {
    for (const source of [NOTCHED, star(), star(7, 1, 0.5), circle(20)]) {
      const smoothed = smoothRing(source);
      expect(area(smoothed)).toBeLessThanOrEqual(area(source) + 1e-12);
      // Area is not the check, though — containment is. Both, on every shape.
      expect(ringWithin(smoothed, source)).toBe(true);
    }
  });

  it('hands back a ring with no corner worth cutting', () => {
    const triangle: Ring = [[0, 0], [1, 0], [0, 1]];
    expect(smoothRing(triangle)).toEqual(triangle);
    expect(smoothRing([[0, 0], [1, 1]] as Ring)).toEqual([[0, 0], [1, 1]]);
  });

  it('one round keeps every vertex on the source boundary', () => {
    // The property the containment argument rests on: a cut point is on an edge, so no vertex the
    // construction produces is outside the ring, whatever the corner.
    for (const source of [NOTCHED, star()]) {
      for (const p of cutCorners(source)) {
        expect(distanceToRing(p, source)).toBeLessThan(1e-12);
      }
    }
  });

  it('costs what it is said to cost, on the largest hull the demo corpus serves', () => {
    // 757 vertices across 10 rings is the largest shape on `clusters/hdbscan` at the overview
    // (artifact-shapes §9). One shape's worth of smoothing, timed.
    const rings = Array.from({length: 10}, (_, i) => circle(14, 1 + i / 10));
    const started = performance.now();
    let vertices = 0;
    for (const ring of rings) vertices += smoothRing(ring).length;
    const ms = performance.now() - started;
    expect(vertices).toBe(140 * 8);
    // Generous — the point is the order of magnitude, and that it is per drawn shape, not per
    // served one. Measured at about 1 ms for the ten rings on this machine.
    expect(ms).toBeLessThan(50);
  });
});

/** Chaikin's corner cutting with no containment guard — the construction that was deleted. */
function chaikin(ring: readonly [number, number][], rounds: number): Ring {
  let current: Ring = ring.map((p) => [p[0], p[1]] as [number, number]);
  for (let r = 0; r < rounds; r++) {
    const next: Ring = [];
    for (let i = 0; i < current.length; i++) {
      const a = current[i]!;
      const b = current[(i + 1) % current.length]!;
      next.push([a[0] + (b[0] - a[0]) * 0.25, a[1] + (b[1] - a[1]) * 0.25]);
      next.push([a[0] + (b[0] - a[0]) * 0.75, a[1] + (b[1] - a[1]) * 0.75]);
    }
    current = next;
  }
  return current;
}

describe('ringWithin', () => {
  it('catches a boundary that left the source though its area fell — which an area check does not', () => {
    const naive = chaikin(NOTCHED, 3);
    expect(area(naive)).toBeLessThan(area(NOTCHED));
    expect(ringWithin(naive, NOTCHED)).toBe(false);
  });

  it('accepts a ring that sits exactly on the source’s boundary', () => {
    expect(ringWithin(NOTCHED, NOTCHED)).toBe(true);
    expect(ringWithin(cutCorners(NOTCHED), NOTCHED)).toBe(true);
  });
});

// ---- the hover resolution ---------------------------------------------------------------------

const shape = (id: bigint, rung: number, rings: Ring[]): ContourShape => ({id, rung, rings, bbox: shapeBbox(rings)});
const box = (x0: number, y0: number, x1: number, y1: number): Ring => [[x0, y0], [x1, y0], [x1, y1], [x0, y1]];

describe('hoverAt', () => {
  const parent = shape(1n, 0, [box(0, 0, 10, 10)]);
  const child = shape(2n, 1, [box(2, 2, 6, 6)]);
  const sibling = shape(3n, 1, [box(7, 7, 9, 9)]);

  it('answers the deepest shape containing the pointer, not the first or the largest', () => {
    expect(hoverAt([parent, child, sibling], [4, 4], null)).toBe(2n);
    // Order in is not order out: the same three shapes, shuffled, answer the same.
    expect(hoverAt([child, sibling, parent], [4, 4], null)).toBe(2n);
    expect(hoverAt([parent, child, sibling], [8, 8], null)).toBe(3n);
    expect(hoverAt([parent, child, sibling], [1, 1], null)).toBe(1n);
    expect(hoverAt([parent, child, sibling], [50, 50], null)).toBeNull();
  });

  it('holds the hovered artifact while the pointer is inside it, whatever else it is inside', () => {
    // Inside the child *and* the parent: the parent held stays held, though the child is deeper.
    expect(hoverAt([parent, child], [4, 4], 1n)).toBe(1n);
    // And once the pointer leaves the parent entirely, the hover lets go.
    expect(hoverAt([parent, child], [40, 40], 1n)).toBeNull();
  });

  it('holds it for a margin past the edge, so a few pixels never change the answer', () => {
    // Just outside the child, inside the parent: without hysteresis this is the flip.
    expect(hoverAt([parent, child], [6.2, 4], 2n, 0)).toBe(1n);
    expect(hoverAt([parent, child], [6.2, 4], 2n, 0.5)).toBe(2n);
    // Far enough out and the margin does not save it.
    expect(hoverAt([parent, child], [8, 4], 2n, 0.5)).toBe(1n);
  });

  it('prefers the artifact the mark under the pointer belongs to, where its shape is drawn', () => {
    // Two shapes at one depth over the same ground: the mark's own artifact decides.
    const a = shape(4n, 1, [box(0, 0, 5, 5)]);
    const b = shape(5n, 1, [box(1, 1, 6, 6)]);
    expect(hoverAt([a, b], [3, 3], null, 0, 5n)).toBe(5n);
    expect(hoverAt([a, b], [3, 3], null, 0, 4n)).toBe(4n);
    // A preference for a shape the pointer is not in is not an answer.
    expect(hoverAt([a, b], [5.5, 5.5], null, 0, 4n)).toBe(5n);
    // And a held hover still outranks it.
    expect(hoverAt([a, b], [3, 3], 4n, 0, 5n)).toBe(4n);
  });

  it('answers one artifact for a shape in several rings, whichever ring is under the pointer', () => {
    const two = shape(9n, 1, [box(0, 0, 2, 2), box(5, 5, 7, 7)]);
    expect(hoverAt([two], [1, 1], null)).toBe(9n);
    expect(hoverAt([two], [6, 6], null)).toBe(9n);
    expect(hoverAt([two], [3.5, 3.5], null)).toBeNull();
  });

  it('breaks a tie on depth by the smaller shape, then by the identifier — never by order', () => {
    const big = shape(6n, 1, [box(0, 0, 10, 10)]);
    const small = shape(7n, 1, [box(0, 0, 4, 4)]);
    expect(hoverAt([big, small], [1, 1], null)).toBe(7n);
    expect(hoverAt([small, big], [1, 1], null)).toBe(7n);
    const same = shape(8n, 1, [box(0, 0, 4, 4)]);
    expect(hoverAt([small, same], [1, 1], null)).toBe(7n);
    expect(hoverAt([same, small], [1, 1], null)).toBe(7n);
  });
});
