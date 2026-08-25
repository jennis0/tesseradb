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

/** Whether a world-space point falls inside a world-space box (closed on every side). */
export function insideBox(x: number, y: number, box: [number, number, number, number]): boolean {
  return x >= box[0] && x <= box[2] && y >= box[1] && y <= box[3];
}
