/**
 * The drawn contour's geometry: the smoothing that produces it, and what answers a hover over it.
 *
 * Two rules govern this module.
 *
 * - **The served ring is the shape; the drawn curve is a smoothing of it.** {@link smoothRing} is
 *   a periodic cubic B-spline through the served vertices, which is how DataMapPlot's contours are
 *   made (`alpha_shapes.py`: `splprep(..., per=True)` then `splev`). It does not interpolate, so
 *   the curve sits a little inside a convex corner and a little outside a reflex one — bounded
 *   locally, and permitted since the owner's ruling of 2026-08-28 that a shape is a summary of
 *   where a cluster is rather than a per-point assertion (`artifact-shapes.md` §4). What is
 *   forbidden is claiming ground the members do not occupy, and a curve that follows the ring
 *   within a fraction of its own edges does not. **Anything that reasons about containment reads
 *   the served ring, never the drawn curve.**
 * - **Only a shape that is drawn answers a hover.** The resolution is {@link hoverAt}: deepest
 *   wins where shapes overlap, and a hovered shape holds the hover until the pointer leaves it.
 */

/** A closed ring in world space — the first vertex is not repeated at the end. */
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

/** Whether a point is inside a closed ring — an even-odd crossing count. */
export function pointInRing(p: readonly [number, number], ring: readonly [number, number][]): boolean {
  let odd = false;
  for (let i = 0, j = ring.length - 1; i < ring.length; j = i++) {
    const [xi, yi] = ring[i]!;
    const [xj, yj] = ring[j]!;
    if (yi > p[1] !== yj > p[1] && p[0] < ((xj - xi) * (p[1] - yi)) / (yj - yi) + xi) odd = !odd;
  }
  return odd;
}

/** The distance from a point to a ring's boundary — the nearest edge, never the interior. */
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
 * The orientation of the triple, by the sign of the cross product; 0 is collinear.
 *
 * The comparison is against a **relative** epsilon, not against zero. The points this is asked
 * about are constructed — a cut point is a quarter of the way along an edge — so a point that is
 * collinear by construction lands a few units in the last place off the line, and an exact test
 * reads that as a turn. It did: a cut chord's own endpoint read as crossing the edge it sits on,
 * and every corner of a circle was refused.
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

/**
 * Whether two segments cross at a point interior to **both** — a touch at an endpoint is not a
 * crossing. That is the predicate the chord test below needs: a cut chord's endpoints lie on the
 * ring's own edges by construction, and those touches must not read as exits.
 */
function crossesProperly(p1: readonly [number, number], p2: readonly [number, number], q1: readonly [number, number], q2: readonly [number, number]): boolean {
  const d1 = turn(p1, p2, q1);
  const d2 = turn(p1, p2, q2);
  const d3 = turn(q1, q2, p1);
  const d4 = turn(q1, q2, p2);
  return d1 !== 0 && d2 !== 0 && d3 !== 0 && d4 !== 0 && d1 !== d2 && d3 !== d4;
}

/**
 * How many curve samples each ring vertex contributes.
 *
 * Four is where a 16-gon stops looking like a polygon at the zoom a hovered cluster is read at,
 * and it makes the drawn ring four times the served one — against the eight times three rounds of
 * corner cutting cost. Only the one or two shapes that draw are smoothed, and only where the wire
 * answered with a hull ({@link focusOutlines} in `layer.ts` — four box corners through a periodic
 * spline is an oval). So this is a per-interaction cost and never a per-served-artifact one: the
 * largest shape on the measurement layer is 757 vertices across 10 rings, and 3,028 vertices is
 * one `PolygonLayer` call either way.
 */
const SAMPLES_PER_SPAN = 4;

/**
 * A ring smoothed as a **periodic uniform cubic B-spline** through its vertices.
 *
 * This is DataMapPlot's construction rather than an approximation of it in spirit only: their
 * `alpha_shapes.py` fits `scipy.interpolate.splprep(..., s=spline_coeff, per=True)` through the
 * α-shape's boundary and evaluates it with `splev` at a multiple of the vertex density, and the
 * α shape underneath is as angular as ours. The periodic uniform cubic B-spline is the closed,
 * knot-free member of that family: no fitting, no parameter, and `C²` everywhere.
 *
 * **It smooths rather than interpolates**, which is the whole difference from the corner cutting
 * it replaces. Each span between two ring vertices is the cubic
 * `(b₀P₀ + b₁P₁ + b₂P₂ + b₃P₃)`, so the curve passes near a vertex rather than through it — at a
 * knot it sits at `(Pᵢ₋₁ + 4Pᵢ + Pᵢ₊₁)/6`, a sixth of the second difference away from `Pᵢ`. At a
 * convex corner that is inward and at a reflex corner outward, and the outward case is what the
 * deleted implementation refused to draw at all.
 *
 * **Why the outward case is now allowed.** The bar is no longer containment but *does the shape
 * claim ground the members do not occupy* (`artifact-shapes.md` §4, owner ruling 2026-08-28). The
 * excursion is bounded by a third of the longer adjacent served edge, and the served edges of a
 * dug ring are α-scale lengths in the cloud's own units — so the curve reaches at most a fraction
 * of the members' own spacing past their outline. That is an imprecise summary of where the
 * cluster is, not a claim about empty ground, and it buys a boundary that reads as a contour at
 * every corner rather than at the convex ones only. The refused reflex corner was visible: a dug
 * shape's concavities stayed as angular as the wire while its convex arcs rounded.
 *
 * **The served ring is unaffected**, and it is what the pick reads, what
 * {@link ringWithin} is asked about, and what any containment reasoning uses. A ring of fewer than
 * four vertices is handed back as it is: a triangle or a degenerate group has no span to fit.
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
 * Whether `inner` is contained in `outer` — **an oracle for tests, and not an area comparison**.
 *
 * Nothing in the drawing path asks this any more: {@link smoothRing} is a smoothing and not a
 * containment-preserving cut, and the ruling it was written under has moved. It is kept because
 * containment is still the right question to ask of the *served* rings — a ring is inside its
 * group's convex wrap, and a narrow principal's shape is inside a broad one's — and an area
 * comparison cannot answer it.
 *
 * Every vertex and every edge midpoint of `inner` must be inside `outer` or on its boundary, and
 * no edge of `inner` may cross an edge of `outer`. On the boundary counts as inside: a
 * containment-preserving cut keeps whole sub-segments of the source ring, whose points sit exactly
 * on it, and an inside-only test would reject the very construction it is there to check.
 *
 * An area comparison would not do: the deleted implementation's own note records that its total
 * area *fell* while its boundary crossed into empty ground, because a convex corner gives up more
 * than a reflex corner takes.
 */
export function ringWithin(inner: readonly [number, number][], outer: readonly [number, number][]): boolean {
  const box = shapeBbox([outer]);
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

// ---- what answers a hover ---------------------------------------------------------------------

/**
 * One artifact's drawn shape, as the hover reads it: every ring it draws, the wire's `rung` it is
 * drawn at (contracts §3.2 r44), and the bounding box of the lot for a cheap rejection.
 */
export type ContourShape = {
  id: bigint;
  rung: number;
  rings: readonly (readonly [number, number][])[];
  bbox: [number, number, number, number];
};

export function shapeBbox(rings: readonly (readonly [number, number][])[]): [number, number, number, number] {
  let x0 = Number.POSITIVE_INFINITY;
  let y0 = Number.POSITIVE_INFINITY;
  let x1 = Number.NEGATIVE_INFINITY;
  let y1 = Number.NEGATIVE_INFINITY;
  for (const ring of rings) {
    for (const [x, y] of ring) {
      if (x < x0) x0 = x;
      if (y < y0) y0 = y;
      if (x > x1) x1 = x;
      if (y > y1) y1 = y;
    }
  }
  return [x0, y0, x1, y1];
}

/** Whether a point is in any of a shape's rings. A hull's rings are separated groups (§1). */
export function shapeContains(shape: ContourShape, p: readonly [number, number]): boolean {
  if (p[0] < shape.bbox[0] || p[0] > shape.bbox[2] || p[1] < shape.bbox[1] || p[1] > shape.bbox[3]) return false;
  for (const ring of shape.rings) if (pointInRing(p, ring)) return true;
  return false;
}

/** The distance from a point to a shape's nearest boundary. */
export function shapeDistance(shape: ContourShape, p: readonly [number, number]): number {
  let best = Number.POSITIVE_INFINITY;
  for (const ring of shape.rings) {
    const d = distanceToRing(p, ring);
    if (d < best) best = d;
  }
  return best;
}

/**
 * The artifact under the pointer: **the deepest drawn shape containing it**, with the one already
 * hovered held until the pointer leaves it.
 *
 * The hover used to be whatever deck's pick pass answered over a polygon layer that kept every
 * served artifact pickable at zero alpha. An ancestor is served alongside its children, its ring
 * contains theirs, and nothing is drawn for it — so the pointer crossed invisible shapes and the
 * answer flipped between a cluster and its sub-cluster on a pixel of movement. Four rules, in
 * order, and each is needed:
 *
 * - **hysteresis**: `sticky` — the artifact already hovered — is kept while the pointer is inside
 *   its shape, and for `margin` world units beyond it, so a hand resting on a boundary does not
 *   oscillate and a few pixels of movement inside one cluster never change the answer;
 * - **`prefer`** — the artifact the mark under the pointer belongs to, where the caller has one —
 *   wins next, so the highlighted contour is the cluster whose point the tooltip is describing.
 *   Membership is a fact the wire carries; where hulls interleave, geometry alone would answer
 *   with whichever shape the point happens to fall in;
 * - **deepest wins** where shapes still overlap — two rings of one artifact, or a frontier
 *   artifact inside another's ring — so the answer is the most specific thing under the cursor,
 *   and it is the served tree that decides rather than paint order;
 * - **only shapes a viewer can point at are candidates at all** — the caller passes the frontier,
 *   which is what carries a label and what draws a contour when the pointer reaches it.
 *
 * Ties on depth are broken by the smaller shape and then by the identifier, so the answer is a
 * function of the pointer, the served set and the mark beneath, and of nothing else.
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
  // The pointer has left the held shape, but not by much: the hover is kept until it is clear of
  // it. Without this a boundary is a place where two answers alternate under an unmoving hand.
  if (sticky !== null && margin > 0) {
    for (const shape of shapes) {
      if (shape.id === sticky && shapeDistance(shape, p) <= margin) return sticky;
    }
  }
  if (preferred) return preferred.id;
  return best?.id ?? null;
}

const boxArea = (s: ContourShape): number => (s.bbox[2] - s.bbox[0]) * (s.bbox[3] - s.bbox[1]);
