import type {Quantisation} from './types.js';

/**
 * The mapping between deck.gl's non-geospatial tile indices and Tessera's Morton cell grid.
 *
 * The engine's world is a 2^16 x 2^16 cell grid, quantised per axis, so a tile is square in cell
 * space and rectangular in data space. The viewer uses cell space, scaled by `CELLS_PER_WORLD_UNIT`,
 * as its deck.gl world, so a tile is `TILE_SIZE` world units at depth 0 and a deck tile `z` is a
 * Morton depth. deck.gl's viewport zoom maps 1:1 onto tile `z`, tile `(0,0,0)` covers the 512-unit
 * world, and tile y and cell y increase together.
 */
export const CELL_GRID = 65536;
export const MAX_DEPTH = 16;
export const TILE_SIZE = 512;
export const WORLD_SIZE = 512;
export const CELLS_PER_WORLD_UNIT = CELL_GRID / WORLD_SIZE; // 128
/**
 * Wire artifact geometry (`centroid`, `box`, shapes) is 32 bits per axis, in the same units as
 * `code`: 2^16 finer than the cell grid. Every reader of that geometry uses this one constant.
 */
export const GRID32 = 2 ** 32;
export const GRID32_PER_WORLD_UNIT = GRID32 / WORLD_SIZE;

/** A wire geometry coordinate (32-bit grid units) to deck.gl world units. */
export function gridToWorld(v: number): number {
  return v / GRID32_PER_WORLD_UNIT;
}

/** A wire geometry point to a world point. */
export function gridToWorldXY(p: readonly [number, number]): [number, number] {
  return [gridToWorld(p[0]), gridToWorld(p[1])];
}

export type TileIndex = {x: number; y: number; z: number};
export type CellBox = {cx0: number; cy0: number; cx1: number; cy1: number};

/**
 * The depth-`z` tile containing a point, from its 64-bit position code. A tile prefix is over the
 * 32-bit Morton cell, which is the code's high half, so the shift is `64 - 2z`. Returns 0 at `z = 0`.
 */
export function tileOfCode(code: bigint, z: number): bigint {
  return code >> BigInt(64 - 2 * z);
}

/** Whether a depth-`za` tile contains a depth-`zb` one: prefix containment. */
export function tileContains(a: bigint, za: number, b: bigint, zb: number): boolean {
  return zb >= za && b >> BigInt(2 * (zb - za)) === a;
}

/**
 * A tile's Morton prefix from its `(x, y)` index at a depth: x on the even bits, y on the odd, as
 * the server tiles. The inverse of {@link tileXY}.
 */
export function mortonOfTile(x: number, y: number, depth: number): bigint {
  let prefix = 0n;
  for (let bit = 0; bit < depth; bit++) {
    prefix |= BigInt((x >> bit) & 1) << BigInt(2 * bit);
    prefix |= BigInt((y >> bit) & 1) << BigInt(2 * bit + 1);
  }
  return prefix;
}

/** The `(x, y)` index of a tile from its Morton prefix; the inverse of {@link mortonOfTile}. */
export function tileXY(prefix: bigint, depth: number): {x: number; y: number} {
  let x = 0;
  let y = 0;
  for (let bit = 0; bit < depth; bit++) {
    x |= Number((prefix >> BigInt(2 * bit)) & 1n) << bit;
    y |= Number((prefix >> BigInt(2 * bit + 1)) & 1n) << bit;
  }
  return {x, y};
}

/** The half-open cell block `[cx0, cx1) x [cy0, cy1)` a tile index covers. */
export function tileToCellBox({x, y, z}: TileIndex): CellBox {
  const span = CELL_GRID / 2 ** z;
  return {cx0: x * span, cy0: y * span, cx1: (x + 1) * span, cy1: (y + 1) * span};
}

/**
 * The data-space bbox `[x0, y0, x1, y1]` a tile covers, half-open like {@link tileToCellBox}. Not
 * what to send to `/v1/viewport`; see {@link tileToRequestBbox}.
 */
export function tileToDataBbox(
  index: TileIndex,
  q: Quantisation
): [number, number, number, number] {
  const cells = tileToCellBox(index);
  const spanX = q.xMax - q.xMin;
  const spanY = q.yMax - q.yMin;
  return [
    q.xMin + (cells.cx0 / CELL_GRID) * spanX,
    q.yMin + (cells.cy0 / CELL_GRID) * spanY,
    q.xMin + (cells.cx1 / CELL_GRID) * spanX,
    q.yMin + (cells.cy1 / CELL_GRID) * spanY
  ];
}

/**
 * The bbox to request for a tile: its cell block, inset to the centres of its first and last cells.
 *
 * The server's bbox is closed: it quantises both corners to cells and includes both. A bbox whose
 * upper corner lies on the tile boundary also selects the next row and column of tiles, so their
 * points are counted and drawn twice. Inset to cell centres, the bbox names one tile. At depth 16
 * both corners fall on one cell's centre, which is a valid bbox.
 */
export function tileToRequestBbox(
  index: TileIndex,
  q: Quantisation
): [number, number, number, number] {
  return rectToRequestBbox({x0: index.x, y0: index.y, x1: index.x, y1: index.y}, index.z, q);
}

/**
 * {@link tileToRequestBbox} for a rectangle of tiles: a closed bbox naming exactly the tiles in
 * the rectangle, sent as four numbers rather than a tile list.
 */
export function rectToRequestBbox(
  rect: {x0: number; y0: number; x1: number; y1: number},
  depth: number,
  q: Quantisation
): [number, number, number, number] {
  const span = CELL_GRID / 2 ** depth;
  const spanX = q.xMax - q.xMin;
  const spanY = q.yMax - q.yMin;
  const toX = (cell: number) => q.xMin + (cell / CELL_GRID) * spanX;
  const toY = (cell: number) => q.yMin + (cell / CELL_GRID) * spanY;
  return [
    toX(rect.x0 * span + 0.5),
    toY(rect.y0 * span + 0.5),
    toX((rect.x1 + 1) * span - 0.5),
    toY((rect.y1 + 1) * span - 0.5)
  ];
}

/**
 * Data space to deck.gl world space. Each axis is scaled separately, as quantisation does, so a
 * tile is square in world space whatever the data extent.
 */
export function dataToWorldXY(x: number, y: number, q: Quantisation): [number, number] {
  return [
    ((x - q.xMin) / (q.xMax - q.xMin)) * WORLD_SIZE,
    ((y - q.yMin) / (q.yMax - q.yMin)) * WORLD_SIZE
  ];
}

/**
 * Converts an interleaved x, y buffer from cell space to world space, narrowing to the
 * `Float32Array` a deck.gl binary attribute takes. Needs no quantisation extent: cell space is the
 * grid's own units, and world space is a uniform scaling of it.
 *
 * Precision is lost here: below about a 2^-8 fraction of a world unit `f32` runs out. Recovering it
 * would take deck.gl's `fp64` emulation, a `position64Low` attribute, which is a change to the layer
 * and not to the wire.
 */
export function positionsToWorld(positions: Float64Array): Float32Array {
  const out = new Float32Array(positions.length);
  for (let i = 0; i < positions.length; i++) {
    out[i] = positions[i]! / CELLS_PER_WORLD_UNIT;
  }
  return out;
}
