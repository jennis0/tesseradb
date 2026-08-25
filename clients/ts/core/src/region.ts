import {MAX_DEPTH, WORLD_SIZE, mortonOfTile} from './coords.js';
import {tileRectOfBbox} from './budget.js';

/**
 * A drawn region as a counting request (design client-components §5.11).
 *
 * The wire has no spatial operand, so a box is answered by one `k = 0` request in the **`tiles`
 * form** (contracts §3.2): the shape is rasterised to Morton prefixes at a depth chosen so the
 * request stays under a client-side bound, and the sum over the returned tiles is the answer to
 * the question asked — never a bbox count subsetted afterwards. The sum is licensed by
 * derivability (client-interaction P3): it equals what the request returns.
 *
 * **Never "as deep as a pixel."** The cell grid is 2¹⁶ per axis, so a full-screen box at pixel
 * depth is 10⁶ tiles against `max_tiles_per_request` of 262,144 and a response of megabytes.
 * {@link REGION_TILE_BOUND} caps the request at a few thousand tiles; where the cell that bound
 * forces is coarser than a screen pixel, the count is exact for the cell cover and **not** for the
 * shape the user drew, and {@link cellExceedsPixel} is what types it so (decision 0097: the grid
 * is never drawn; the inexactness is stated in the number).
 */

/**
 * The most tiles a region request names. 4,096 is a 64 × 64 cover: coarse enough that a
 * full-extent box costs one counting pass of the same order as a viewport request, fine enough
 * that a box drawn a few zoom levels in is counted at pixel depth. Under every deployment's
 * `max_tiles_per_request` (262,144 by default).
 */
export const REGION_TILE_BOUND = 4096;

export type RegionRequest = {
  /** The depth the box was rasterised at. */
  depth: number;
  /** The depth-`depth` Morton prefixes of every tile the box intersects, in raster order. */
  tiles: bigint[];
};

/**
 * Rasterise a world-space box to Morton prefixes at the deepest depth whose cover stays under
 * `bound` tiles.
 *
 * Tile count over a fixed box is monotone in depth, so the walk starts at the grid's floor and
 * stops one before the first depth that overflows. A degenerate box (zero area) is still a box:
 * it names the tiles its edges fall in.
 */
export function rasteriseBox(
  world: [number, number, number, number],
  bound = REGION_TILE_BOUND
): RegionRequest {
  const clamp = (v: number) => Math.min(WORLD_SIZE, Math.max(0, v));
  const bbox: [number, number, number, number] = [
    clamp(Math.min(world[0], world[2])),
    clamp(Math.min(world[1], world[3])),
    clamp(Math.max(world[0], world[2])),
    clamp(Math.max(world[1], world[3]))
  ];
  let depth = 0;
  for (let d = 1; d <= MAX_DEPTH; d++) {
    const r = tileRectOfBbox(bbox, d);
    if ((r.x1 - r.x0 + 1) * (r.y1 - r.y0 + 1) > bound) break;
    depth = d;
  }
  const rect = tileRectOfBbox(bbox, depth);
  const tiles: bigint[] = [];
  for (let y = rect.y0; y <= rect.y1; y++) {
    for (let x = rect.x0; x <= rect.x1; x++) tiles.push(mortonOfTile(x, y, depth));
  }
  return {depth, tiles};
}

/**
 * The exactness rule: whether a depth-`depth` cell is wider than one screen pixel at `zoom`.
 *
 * A depth-`d` tile is `WORLD_SIZE / 2^d` world units, and at zoom `z` one world unit is `2^z`
 * pixels (`coords.ts`: viewport zoom maps 1:1 onto tile `z`, measured). So the cell spans
 * `512 · 2^(z − d)` pixels and exceeds a pixel while `d < z + 9`. A count over such cells is exact
 * for the cells and not for the shape the user can see, so `Masked.exact` is false.
 */
export function cellExceedsPixel(depth: number, zoom: number): boolean {
  return WORLD_SIZE * 2 ** (zoom - depth) > 1;
}

/** A polygon in world space, as the lasso draws it: at least three vertices, implicitly closed. */
export type WorldPolygon = readonly (readonly [number, number])[];

/**
 * Whether a world-space point falls inside a polygon — the even-odd rule over its edges, so a
 * self-crossing lasso still answers, and the same rule for every point so the held sample and
 * the count agree on what *inside* means.
 */
export function insidePolygon(x: number, y: number, polygon: WorldPolygon): boolean {
  let inside = false;
  for (let i = 0, j = polygon.length - 1; i < polygon.length; j = i++) {
    const [xi, yi] = polygon[i]!;
    const [xj, yj] = polygon[j]!;
    if (yi > y !== yj > y && x < ((xj - xi) * (y - yi)) / (yj - yi) + xi) inside = !inside;
  }
  return inside;
}

/** Whether two closed segments intersect (touching counts). */
function segmentsCross(ax: number, ay: number, bx: number, by: number, cx: number, cy: number, dx: number, dy: number): boolean {
  const orient = (px: number, py: number, qx: number, qy: number, rx: number, ry: number) => {
    const v = (qy - py) * (rx - qx) - (qx - px) * (ry - qy);
    return v > 0 ? 1 : v < 0 ? -1 : 0;
  };
  const o1 = orient(ax, ay, bx, by, cx, cy);
  const o2 = orient(ax, ay, bx, by, dx, dy);
  const o3 = orient(cx, cy, dx, dy, ax, ay);
  const o4 = orient(cx, cy, dx, dy, bx, by);
  if (o1 !== o2 && o3 !== o4) return true;
  const on = (px: number, py: number, qx: number, qy: number, rx: number, ry: number) =>
    Math.min(px, rx) <= qx && qx <= Math.max(px, rx) && Math.min(py, ry) <= qy && qy <= Math.max(py, ry);
  return (o1 === 0 && on(ax, ay, cx, cy, bx, by)) || (o2 === 0 && on(ax, ay, dx, dy, bx, by)) || (o3 === 0 && on(cx, cy, ax, ay, dx, dy)) || (o4 === 0 && on(cx, cy, bx, by, dx, dy));
}

/** Whether a world-space cell `[x0, y0, x1, y1]` meets the polygon — inside it, around it, or crossed by it. */
function cellMeetsPolygon(x0: number, y0: number, x1: number, y1: number, polygon: WorldPolygon): boolean {
  if (insidePolygon((x0 + x1) / 2, (y0 + y1) / 2, polygon)) return true;
  for (const [px, py] of polygon) if (px >= x0 && px <= x1 && py >= y0 && py <= y1) return true;
  const corners: [number, number][] = [[x0, y0], [x1, y0], [x1, y1], [x0, y1]];
  for (let i = 0, j = polygon.length - 1; i < polygon.length; j = i++) {
    const [ax, ay] = polygon[i]!;
    const [bx, by] = polygon[j]!;
    for (let c = 0; c < 4; c++) {
      const [cx, cy] = corners[c]!;
      const [dx, dy] = corners[(c + 1) % 4]!;
      if (segmentsCross(ax, ay, bx, by, cx, cy, dx, dy)) return true;
    }
  }
  return false;
}

/**
 * Rasterise a lasso to the tiles it meets, at the deepest depth whose **bounding box's** cover
 * stays under `bound` — the same depth rule as the box, so the exactness typing is the same
 * (§5.11): the cells are what is counted, the shape is what is highlighted (decision 0097), and
 * where a cell exceeds a pixel the count is exact for the cells and not for the shape. A cell on
 * the polygon's edge is counted whole, so the cover is a superset of the shape, never a subset.
 */
export function rasterisePolygon(polygon: WorldPolygon, bound = REGION_TILE_BOUND): RegionRequest {
  let x0 = Infinity;
  let y0 = Infinity;
  let x1 = -Infinity;
  let y1 = -Infinity;
  for (const [x, y] of polygon) {
    if (x < x0) x0 = x;
    if (y < y0) y0 = y;
    if (x > x1) x1 = x;
    if (y > y1) y1 = y;
  }
  const box = rasteriseBox([x0, y0, x1, y1], bound);
  const span = WORLD_SIZE / 2 ** box.depth;
  const tiles: bigint[] = [];
  const rect = tileRectOfBbox(
    [Math.min(WORLD_SIZE, Math.max(0, x0)), Math.min(WORLD_SIZE, Math.max(0, y0)), Math.min(WORLD_SIZE, Math.max(0, x1)), Math.min(WORLD_SIZE, Math.max(0, y1))],
    box.depth
  );
  for (let y = rect.y0; y <= rect.y1; y++) {
    for (let x = rect.x0; x <= rect.x1; x++) {
      if (cellMeetsPolygon(x * span, y * span, (x + 1) * span, (y + 1) * span, polygon)) tiles.push(mortonOfTile(x, y, box.depth));
    }
  }
  return {depth: box.depth, tiles};
}

/** Whether a world-space point falls inside a world-space box (closed on every side). */
export function insideBox(x: number, y: number, box: [number, number, number, number]): boolean {
  return x >= box[0] && x <= box[2] && y >= box[1] && y <= box[3];
}
