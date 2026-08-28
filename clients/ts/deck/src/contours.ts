/**
 * The drawn contour's geometry: the smoothing that produces it, and what answers a hover over it.
 *
 * Two rules govern this module, and both are about not saying more than the server said.
 *
 * - **A smoothed ring is contained in the ring it came from.** Containment is the property, not
 *   area: corner cutting takes area off at convex corners and adds it at reflex ones, so a shape
 *   whose total area fell can still have crossed into ground with no visible member in it. Every
 *   construction here is `⊆` its input **by construction** — see {@link smoothRing}.
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
 * Whether the chord cutting the corner at `vertex` lies inside the ring: it crosses no edge of the
 * ring, and its midpoint is interior.
 *
 * The two edges meeting at `vertex` are skipped by index, because the chord's endpoints *are* on
 * them — a touch, not a crossing, and the one case where the orientation test is being asked about
 * points it constructed itself.
 */
function chordInside(from: readonly [number, number], to: readonly [number, number], ring: readonly [number, number][], vertex: number): boolean {
  const n = ring.length;
  const lo0 = Math.min(from[0], to[0]);
  const hi0 = Math.max(from[0], to[0]);
  const lo1 = Math.min(from[1], to[1]);
  const hi1 = Math.max(from[1], to[1]);
  const incoming = (vertex + n - 1) % n;
  for (let k = 0; k < n; k++) {
    if (k === incoming || k === vertex) continue;
    const a = ring[k]!;
    const b = ring[(k + 1) % n]!;
    // A cheap rejection first: most of a ring's edges are nowhere near a corner's chord, and the
    // orientation tests are what make this O(n²) per round rather than O(n).
    if (Math.max(a[0], b[0]) < lo0 || Math.min(a[0], b[0]) > hi0 || Math.max(a[1], b[1]) < lo1 || Math.min(a[1], b[1]) > hi1) continue;
    if (crossesProperly(from, to, a, b)) return false;
  }
  return pointInRing([(from[0] + to[0]) / 2, (from[1] + to[1]) / 2], ring);
}

/** How far along each adjacent edge a corner is cut — Chaikin's quarter. */
const CUT = 0.25;

const lerp = (a: readonly [number, number], b: readonly [number, number], t: number): [number, number] => [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t];

/**
 * One round of **containment-preserving corner cutting**.
 *
 * At each vertex `V`, with neighbours `P` and `N`, the two cut points `R = V + ¼(P − V)` and
 * `Q = V + ¼(N − V)` lie **on the ring's own edges**, so no vertex this produces is outside the
 * ring. The corner is replaced by the chord `R → Q` — which is what smooths it — and every other
 * edge of the result is a sub-segment of an edge of the input. So the only way out of the ring is
 * a chord, and a chord is emitted only when {@link chordInside} says it stays in:
 *
 * - at a **convex** corner the chord cuts across the corner and is inside, unless some other part
 *   of the boundary reaches into the corner — which the test catches rather than assumes;
 * - at a **reflex** corner the chord spans the notch and is outside, so the vertex is kept and
 *   the corner stays sharp. This is the case the deleted implementation got wrong: it cut every
 *   corner, and at a reflex vertex the triangle it removed lay outside the polygon, so the drawn
 *   contour reached into ground with no visible member in it, by up to a quarter of the shorter
 *   adjacent edge.
 *
 * A reflex corner is therefore never rounded, and that is not a shortfall to be fixed later: the
 * material at a reflex vertex fills more than half a turn, so any curve replacing the corner has
 * to pass on the far side of it, which is outside. A notch the members leave stays a notch.
 *
 * The result is `⊆` the input **by construction**, which composes: rounds are applied in sequence,
 * each testing against its own input, so the last is contained in the first.
 */
export function cutCorners(ring: Ring): Ring {
  const n = ring.length;
  if (n < 3) return ring.map((p) => [p[0], p[1]] as [number, number]);
  const out: Ring = [];
  for (let i = 0; i < n; i++) {
    const v = ring[i]!;
    const p = ring[(i + n - 1) % n]!;
    const q = ring[(i + 1) % n]!;
    const r = lerp(v, p, CUT);
    const s = lerp(v, q, CUT);
    if (chordInside(r, s, ring, i)) {
      out.push(r, s);
    } else {
      out.push(r, [v[0], v[1]], s);
    }
  }
  return out;
}

/**
 * A ring smoothed by {@link cutCorners}, `rounds` times, and **contained in the ring handed in**.
 *
 * Three rounds is what reads as a contour rather than a polygon at the zoom a hovered cluster is
 * looked at; a ring of fewer than four vertices is handed back as it is, having no corner worth
 * cutting. Vertex growth is at most 2× a round at a convex corner and 3× at a reflex one, so a
 * 144-vertex hull becomes about 1,200 — which is why only the shapes that **draw** are smoothed
 * ({@link outlineData} in `layer.ts`), one or two of them, and never every served shape.
 */
export function smoothRing(ring: readonly [number, number][], rounds = 3): Ring {
  let current: Ring = ring.map((p) => [p[0], p[1]] as [number, number]);
  if (current.length < 4) return current;
  for (let round = 0; round < rounds; round++) current = cutCorners(current);
  return current;
}

/**
 * Whether `inner` is contained in `outer` — **the test's oracle, and not an area comparison**.
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
 * One artifact's drawn shape, as the hover reads it: every ring it draws, its depth in the served
 * tree, and the bounding box of the lot for a cheap rejection.
 */
export type ContourShape = {
  id: bigint;
  depth: number;
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
 * - **only drawn shapes are candidates at all** — the caller passes the frontier, which is what
 *   carries a label and what draws a contour.
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
    if (best === null || shape.depth > best.depth) {
      best = shape;
      continue;
    }
    if (shape.depth < best.depth) continue;
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
