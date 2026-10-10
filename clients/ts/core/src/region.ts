import {WORLD_SIZE} from './coords.js';
import type {FilterExpr, RegionOperand, RegionVerdict} from './types.js';

/**
 * A drawn region as a filter leaf.
 *
 * The shape is sent as `{"region": {"polygon": [[x, y], …]}}` on the viewport request, beside the
 * other filters. The server intersects it with the Morton cells: cells wholly inside are whole row
 * ranges, and cells on the boundary test each point's stored position, so the count is exact for
 * the shape. Where the perimeter crosses more than the deployment's `max_region_cells`, the answer
 * is for a cover at a depth the server states. This module spells the leaf, reads the verdict from
 * `x-mosaica-region`, and holds the client's copy of the server's point-in-polygon predicate.
 */

/** A polygon in world space, as the lasso draws it: at least three vertices, implicitly closed. @internal */
export type WorldPolygon = readonly (readonly [number, number])[];

/**
 * A world coordinate on the server's 32-bit-per-axis grid, as the tiler quantises a point:
 * `floor(v / WORLD_SIZE · 2³²)`, clamped. The predicate tests quantised positions against
 * quantised vertices, as the count does, so the two agree along the edge.
 *
 * @internal
 */
export function quantise(v: number): number {
  const scaled = Math.floor((v / WORLD_SIZE) * 4294967296);
  return scaled <= 0 ? 0 : scaled >= 4294967295 ? 4294967295 : scaled;
}

/**
 * Whether a world-space point is inside a polygon, by the server's predicate: even-odd over the
 * quantised edges, a point on an edge inside, and the ray counting an edge where exactly one end is
 * strictly above it (as `mosaica_spatial::shape::polygon`). A self-crossing lasso still answers.
 * Integer arithmetic throughout, in `BigInt` where a product leaves a double's exact range.
 *
 * This is for a drawn selection. Which artifact a point belongs to is the `membership:<layer>`
 * column; a served `shape` is generalised for drawing.
 *
 * @internal
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
    // An edge counts where exactly one end is above the ray and the crossing is strictly right of the
    // point, compared without division.
    if (ay > py !== by > py) {
      const lhs = (bx - ax) * (py - ay);
      const rhs = (px - ax) * (by - ay);
      if (by - ay > 0n ? lhs > rhs : lhs < rhs) inside = !inside;
    }
  }
  return inside;
}

/** Whether a world-space point falls inside a world-space box, closed on every side, on the grid. @internal */
export function insideBox(x: number, y: number, box: [number, number, number, number]): boolean {
  const px = quantise(x);
  const py = quantise(y);
  return px >= quantise(box[0]) && px <= quantise(box[2]) && py >= quantise(box[1]) && py <= quantise(box[3]);
}

/**
 * A selection as the operand of a `region` filter leaf, in the view's data coordinates. A box is
 * normalised so either corner may come first. A lasso's points are sent as drawn. An artifact is
 * named by its `mosaica_id` as a decimal string.
 *
 * @category Filters
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
 * Joins a `region` leaf for `operand` to `expr` with `all_of`, or returns the leaf alone where
 * `expr` is `null`. Returns `expr` unchanged where `operand` is `null`.
 *
 * @param outside - Whether to select everything outside the region, by wrapping the leaf in
 *   `none_of`. Defaults to `false`.
 *
 * @category Filters
 */
export function withRegion(expr: FilterExpr | null, operand: RegionOperand | null, outside = false): FilterExpr | null {
  if (!operand) return expr;
  const leaf: FilterExpr = outside ? {none_of: [{region: operand}]} : {region: operand};
  return expr ? {all_of: [expr, leaf]} : leaf;
}

/**
 * Parses `x-mosaica-region`: `exact`, or `cover; depth=<d>`, an answer exact for a cover of the shape
 * at that depth, which is a superset. `null` where the request carried no region leaf or the header
 * is not understood.
 *
 * @internal
 */
export function parseRegionVerdict(header: string | null): RegionVerdict | null {
  if (!header) return null;
  const value = header.trim();
  if (value === 'exact') return {exact: true, depth: null};
  const m = /^cover;\s*depth=(\d+)$/.exec(value);
  if (m) return {exact: false, depth: Number(m[1])};
  return null;
}
