/**
 * Contour geometry: the smoothing that draws a served ring, and which shape answers a hover.
 *
 * The served ring is the shape; the drawn curve is a smoothing of it that strays a fraction of an
 * edge either side. Anything that reasons about containment reads the served ring.
 */

/** A closed ring in world space; the first vertex is not repeated at the end. */
export type Ring = [number, number][];

/** Twice a ring's signed area; positive counter-clockwise in a y-up frame. */
export function signedArea2(ring: readonly [number, number][]): number {
  let sum = 0;
  for (let i = 0; i < ring.length; i++) {
    const a = ring[i]!;
    const b = ring[(i + 1) % ring.length]!;
    sum += a[0] * b[1] - b[0] * a[1];
  }
  return sum;
}

/** Whether a point is inside a closed ring, by an even-odd crossing count. */
export function pointInRing(p: readonly [number, number], ring: readonly [number, number][]): boolean {
  let odd = false;
  for (let i = 0, j = ring.length - 1; i < ring.length; j = i++) {
    const [xi, yi] = ring[i]!;
    const [xj, yj] = ring[j]!;
    if (yi > p[1] !== yj > p[1] && p[0] < ((xj - xi) * (p[1] - yi)) / (yj - yi) + xi) odd = !odd;
  }
  return odd;
}

/** The distance from a point to a ring's nearest edge, whether the point is inside or out. */
export function distanceToRing(p: readonly [number, number], ring: readonly [number, number][]): number {
  let best = Number.POSITIVE_INFINITY;
  for (let i = 0, j = ring.length - 1; i < ring.length; j = i++) {
    const a = ring[j]!;
    const b = ring[i]!;
    const dx = b[0] - a[0];
    const dy = b[1] - a[1];
    const len2 = dx * dx + dy * dy;
    const t = len2 === 0 ? 0 : Math.max(0, Math.min(1, ((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / len2));
    const ex = a[0] + t * dx - p[0];
    const ey = a[1] + t * dy - p[1];
    const d = Math.hypot(ex, ey);
    if (d < best) best = d;
  }
  return best;
}

/**
 * The orientation of the triple, by the sign of the cross product; 0 is collinear. The test uses
 * a relative epsilon because a constructed point on an edge lands a few ulps off the line.
 */
function turn(a: readonly [number, number], b: readonly [number, number], c: readonly [number, number]): number {
  const ux = b[0] - a[0];
  const uy = b[1] - a[1];
  const vx = c[0] - a[0];
  const vy = c[1] - a[1];
  const cross = ux * vy - uy * vx;
  const scale = (Math.abs(ux) + Math.abs(uy)) * (Math.abs(vx) + Math.abs(vy));
  return Math.abs(cross) <= scale * 1e-12 ? 0 : Math.sign(cross);
}

/** Whether two segments cross at a point interior to both; a touch at an endpoint does not count. */
function crossesProperly(p1: readonly [number, number], p2: readonly [number, number], q1: readonly [number, number], q2: readonly [number, number]): boolean {
  const d1 = turn(p1, p2, q1);
  const d2 = turn(p1, p2, q2);
  const d3 = turn(q1, q2, p1);
  const d4 = turn(q1, q2, p2);
  return d1 !== 0 && d2 !== 0 && d3 !== 0 && d4 !== 0 && d1 !== d2 && d3 !== d4;
}

/**
 * How many curve samples each ring vertex contributes. Four is enough for a 16-gon to stop looking
 * like a polygon. Only the one or two shapes drawn at a time are smoothed.
 */
const SAMPLES_PER_SPAN = 4;

/**
 * A ring smoothed as a periodic uniform cubic B-spline over its vertices: closed, with no fitting
 * parameter, and `C²` everywhere. DataMapPlot draws its contours the same way.
 *
 * The curve passes near each vertex, not through it: at a knot it sits at
 * `(Pᵢ₋₁ + 4Pᵢ + Pᵢ₊₁)/6`, inside a convex corner and outside a reflex one. The excursion is at
 * most a third of the longer adjacent edge, a fraction of the members' own spacing, so the curve
 * does not claim ground the members do not occupy. A ring of fewer than four vertices is returned
 * as it is.
 */
export function smoothRing(ring: readonly [number, number][], samplesPerSpan = SAMPLES_PER_SPAN): Ring {
  const n = ring.length;
  if (n < 4) return ring.map((p) => [p[0], p[1]] as [number, number]);
  const out: Ring = [];
  for (let i = 0; i < n; i++) {
    const p0 = ring[(i + n - 1) % n]!;
    const p1 = ring[i]!;
    const p2 = ring[(i + 1) % n]!;
    const p3 = ring[(i + 2) % n]!;
    for (let s = 0; s < samplesPerSpan; s++) {
      const t = s / samplesPerSpan;
      const t2 = t * t;
      const t3 = t2 * t;
      const b0 = (1 - 3 * t + 3 * t2 - t3) / 6;
      const b1 = (4 - 6 * t2 + 3 * t3) / 6;
      const b2 = (1 + 3 * t + 3 * t2 - 3 * t3) / 6;
      const b3 = t3 / 6;
      out.push([
        b0 * p0[0] + b1 * p1[0] + b2 * p2[0] + b3 * p3[0],
        b0 * p0[1] + b1 * p1[1] + b2 * p2[1] + b3 * p3[1]
      ]);
    }
  }
  return out;
}

/**
 * Whether `inner` is contained in `outer`, for tests of served rings (a ring inside its group's
 * convex wrap, a narrow principal's shape inside a broad one's). Every vertex and edge midpoint of
 * `inner` must be inside `outer` or on its boundary, and no edge of `inner` may cross an edge of
 * `outer`. A smaller area does not imply containment, so areas are not compared.
 */
export function ringWithin(inner: readonly [number, number][], outer: readonly [number, number][]): boolean {
  const box = shapeBbox([[outer]]);
  const eps = Math.max(Math.hypot(box[2] - box[0], box[3] - box[1]), 1) * 1e-9;
  const on = (p: [number, number]) => pointInRing(p, outer) || distanceToRing(p, outer) <= eps;
  for (let i = 0; i < inner.length; i++) {
    const a = inner[i]!;
    const b = inner[(i + 1) % inner.length]!;
    if (!on([a[0], a[1]])) return false;
    if (!on([(a[0] + b[0]) / 2, (a[1] + b[1]) / 2])) return false;
    for (let k = 0, j = outer.length - 1; k < outer.length; j = k++) {
      if (crossesProperly(a, b, outer[j]!, outer[k]!)) return false;
    }
  }
  return true;
}

/** One part of a drawn shape: its outer ring first, then its holes. */
export type Part = readonly (readonly [number, number][])[];

/**
 * One artifact's drawn shape, as the hover reads it: its parts, the served `rung` (its level, or
 * its depth in a tree), and the bounding box of all parts. It answers a hover and a click only. A served
 * shape is generalised to the pixel, so whether a point is a member comes from the served
 * `membership:<layer>` column.
 */
export type ContourShape = {
  id: bigint;
  rung: number;
  parts: readonly Part[];
  bbox: [number, number, number, number];
};

export function shapeBbox(parts: readonly Part[]): [number, number, number, number] {
  let x0 = Number.POSITIVE_INFINITY;
  let y0 = Number.POSITIVE_INFINITY;
  let x1 = Number.NEGATIVE_INFINITY;
  let y1 = Number.NEGATIVE_INFINITY;
  for (const ring of parts.flat()) {
    for (const [x, y] of ring) {
      if (x < x0) x0 = x;
      if (y < y0) y0 = y;
      if (x > x1) x1 = x;
      if (y > y1) y1 = y;
    }
  }
  return [x0, y0, x1, y1];
}

/** Whether a point is in any part of a shape, by the even-odd rule over each part's rings. */
export function shapeContains(shape: ContourShape, p: readonly [number, number]): boolean {
  if (p[0] < shape.bbox[0] || p[0] > shape.bbox[2] || p[1] < shape.bbox[1] || p[1] > shape.bbox[3]) return false;
  for (const part of shape.parts) {
    let odd = false;
    for (const ring of part) if (pointInRing(p, ring)) odd = !odd;
    if (odd) return true;
  }
  return false;
}

/** The distance from a point to a shape's nearest boundary, holes included. */
export function shapeDistance(shape: ContourShape, p: readonly [number, number]): number {
  let best = Number.POSITIVE_INFINITY;
  for (const ring of shape.parts.flat()) {
    const d = distanceToRing(p, ring);
    if (d < best) best = d;
  }
  return best;
}

/**
 * The artifact under the pointer, among `shapes` (the caller passes the frontier). In order:
 *
 * - `sticky`, the artifact already hovered, is kept while the pointer is inside its shape or
 *   within `margin` world units of it, so a hand resting on a boundary does not flicker;
 * - `prefer`, the artifact of the mark under the pointer, wins next if its shape contains the
 *   pointer, so the contour matches the point the tooltip describes where shapes interleave;
 * - otherwise the deepest containing shape wins, then the smaller bounding box, then the smaller
 *   identifier.
 */
export function hoverAt(
  shapes: readonly ContourShape[],
  p: readonly [number, number],
  sticky: bigint | null,
  margin = 0,
  prefer: bigint | null = null
): bigint | null {
  let held: ContourShape | null = null;
  let preferred: ContourShape | null = null;
  let best: ContourShape | null = null;
  for (const shape of shapes) {
    const inside = shapeContains(shape, p);
    if (!inside) continue;
    if (shape.id === sticky) held = shape;
    if (shape.id === prefer) preferred = shape;
    if (best === null || shape.rung > best.rung) {
      best = shape;
      continue;
    }
    if (shape.rung < best.rung) continue;
    const a = boxArea(shape);
    const b = boxArea(best);
    if (a < b || (a === b && shape.id < best.id)) best = shape;
  }
  if (held) return held.id;
  if (sticky !== null && margin > 0) {
    for (const shape of shapes) {
      if (shape.id === sticky && shapeDistance(shape, p) <= margin) return sticky;
    }
  }
  if (preferred) return preferred.id;
  return best?.id ?? null;
}

const boxArea = (s: ContourShape): number => (s.bbox[2] - s.bbox[0]) * (s.bbox[3] - s.bbox[1]);
