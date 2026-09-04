import {WORLD_SIZE} from './coords.js';
import type {FilterExpr, RegionOperand, RegionVerdict} from './types.js';

/**
 * A drawn region as a **filter leaf** (design `selection-operand.md`; client-components §5.11).
 *
 * The shape travels as a shape: `{"region": {"polygon": [[x, y], …]}}` on the viewport request
 * the client was sending anyway, beside the other filters, and the server intersects it with
 * the Morton cell structure — cells wholly inside are whole row ranges, cells the boundary
 * crosses take a per-point test against each point's stored position — so the count is **exact
 * for the shape** and composes with every other leaf. What this module still owns is the client's
 * half of that contract: the leaf's spelling, the exactness verdict read off `x-tessera-region`,
 * and the live highlight's predicate, which must be the server's.
 *
 * **What went** (selection-operand §8): the depth walk, `REGION_TILE_BOUND`, the `k = 0`
 * `tiles`-form counting request, and `cellExceedsPixel`. A count is no longer exact for a cell
 * cover the user cannot see; it is exact for the shape, or a **cover** at a depth the server
 * states when a shape's perimeter exceeds the deployment's `max_region_cells` (`/v1/meta`).
 */

/** A polygon in world space, as the lasso draws it: at least three vertices, implicitly closed. */
export type WorldPolygon = readonly (readonly [number, number])[];

/**
 * A world coordinate on the server's 32-bit-per-axis grid — the same `fixed32` the tiler applies
 * to a point, restated over the 512-unit world: `floor(v / WORLD_SIZE · 2³²)`, clamped.
 *
 * **The highlight tests quantised positions against quantised vertices**, because that is what
 * the count is over (selection-operand §8's obligation, third part): a mark is drawn at its
 * stored position and counted at it, and a predicate over the float the client happens to hold
 * would disagree with the server's along the edge.
 */
export function quantise(v: number): number {
  const scaled = Math.floor((v / WORLD_SIZE) * 4294967296);
  return scaled <= 0 ? 0 : scaled >= 4294967295 ? 4294967295 : scaled;
}

/**
 * Whether a world-space point falls inside a polygon — **the server's predicate**: even-odd over
 * the edges of the quantised polygon, a point on an edge inside, the horizontal ray counting an
 * edge where exactly one end is strictly above the ray (`tessera_spatial::shape::polygon`'s tie
 * rule; `polygon-membership.md` §7.3). A self-crossing lasso still answers, and the same rule for
 * every point so the held sample and the count agree on what *inside* means.
 *
 * Integer arithmetic throughout, in `BigInt` where a product of two 32-bit coordinates would
 * leave the double's exact range — the same exactness the server has, so the two cannot differ
 * by a rounding.
 *
 * **Never a membership test against a served shape.** A served `shape` is a drawing generalised
 * to the pixel (`polygon-membership.md` §7.1); the wire's `membership:<layer>` column says which
 * artifact a point belongs to, and this function is not asked that question.
 */
export function insidePolygon(x: number, y: number, polygon: WorldPolygon): boolean {
  const px = BigInt(quantise(x));
  const py = BigInt(quantise(y));
  let inside = false;
  for (let i = 0, j = polygon.length - 1; i < polygon.length; j = i++) {
    const ax = BigInt(quantise(polygon[j]![0]));
    const ay = BigInt(quantise(polygon[j]![1]));
    const bx = BigInt(quantise(polygon[i]![0]));
    const by = BigInt(quantise(polygon[i]![1]));
    // On the edge: collinear and within the edge's box.
    if ((bx - ax) * (py - ay) - (by - ay) * (px - ax) === 0n) {
      const minX = ax < bx ? ax : bx;
      const maxX = ax < bx ? bx : ax;
      const minY = ay < by ? ay : by;
      const maxY = ay < by ? by : ay;
      if (px >= minX && px <= maxX && py >= minY && py <= maxY) return true;
    }
    // The half-open rule: an edge counts where exactly one end is above the ray, and the
    // crossing is strictly right of the point — compared without division.
    if (ay > py !== by > py) {
      const lhs = (bx - ax) * (py - ay);
      const rhs = (px - ax) * (by - ay);
      if (by - ay > 0n ? lhs > rhs : lhs < rhs) inside = !inside;
    }
  }
  return inside;
}

/** Whether a world-space point falls inside a world-space box, closed on every side, on the grid. */
export function insideBox(x: number, y: number, box: [number, number, number, number]): boolean {
  const px = quantise(x);
  const py = quantise(y);
  return px >= quantise(box[0]) && px <= quantise(box[2]) && py >= quantise(box[1]) && py <= quantise(box[3]);
}

/**
 * A selection as the wire's `region` leaf, in **data coordinates** — the view's own space, the one
 * `/v1/meta`'s quantisation extent defines (`selection-operand.md` §2). A box is normalised so
 * either corner may come first; a lasso is sent as the vertex list the user drew, unchanged, so
 * the server's canonical form — and its cache key — is a function of the drawing alone; an
 * artifact is named by its `tessera_id` (`polygon-membership.md` §8).
 */
export function regionOperand(
  shape: {kind: 'box'; bbox: [number, number, number, number]} | {kind: 'lasso'; points: [number, number][]} | {kind: 'artifact'; id: bigint}
): RegionOperand {
  switch (shape.kind) {
    case 'box': {
      const [x0, y0, x1, y1] = shape.bbox;
      return {bbox: [Math.min(x0, x1), Math.min(y0, y1), Math.max(x0, x1), Math.max(y0, y1)]};
    }
    case 'lasso':
      return {polygon: shape.points.map(([x, y]) => [x, y])};
    case 'artifact':
      return {artifact: shape.id.toString()};
  }
}

/**
 * The region composed with the other filters: `all_of` of the two, or the leaf alone, or the
 * expression alone. *Outside* is `none_of` over the leaf (`polygon-membership.md` §8) — every
 * rowed item carries a position, so the complement is well defined.
 */
export function withRegion(expr: FilterExpr | null, operand: RegionOperand | null, outside = false): FilterExpr | null {
  if (!operand) return expr;
  const leaf: FilterExpr = outside ? {none_of: [{region: operand}]} : {region: operand};
  return expr ? {all_of: [expr, leaf]} : leaf;
}

/**
 * `x-tessera-region`'s value (`selection-operand.md` §6): `exact`, or `cover; depth=<d>` — the
 * answer is exact for a cover of the shape taken at that depth, a superset. `null` when the
 * request carried no region leaf, or the header is not one this client understands.
 */
export function parseRegionVerdict(header: string | null): RegionVerdict | null {
  if (!header) return null;
  const value = header.trim();
  if (value === 'exact') return {exact: true, depth: null};
  const m = /^cover;\s*depth=(\d+)$/.exec(value);
  if (m) return {exact: false, depth: Number(m[1])};
  return null;
}
