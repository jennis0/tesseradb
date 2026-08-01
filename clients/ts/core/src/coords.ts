import type {Quantisation} from './types.js';

/**
 * The mapping between deck.gl's non-geospatial tile indices and Tessera's Morton cell grid.
 *
 * The engine's world is a 2^16 x 2^16 cell grid, quantised per axis (design §2.5), so a tile is
 * square in CELL space and rectangular in data space. The viewer therefore uses cell space as its
 * deck.gl world, scaled down by CELLS_PER_WORLD_UNIT so that a tile is TILE_SIZE world units at
 * depth 0 — which makes a deck tile `z` identically a Morton depth.
 *
 * **Measured, not assumed** (`spike/src/convention.test.ts`, run against deck.gl's own
 * `Tileset2D`): viewport zoom maps 1:1 onto tile `z`; tile `(0,0,0)` covers the whole 512-unit
 * world; and a tile bbox arrives as `{left, top, right, bottom}` with `top` numerically below
 * `bottom`, so tile y and cell y increase together. Change these constants only with that test.
 */
export const CELL_GRID = 65536;
export const MAX_DEPTH = 16;
export const TILE_SIZE = 512;
export const WORLD_SIZE = 512;
export const CELLS_PER_WORLD_UNIT = CELL_GRID / WORLD_SIZE; // 128

export type TileIndex = {x: number; y: number; z: number};
export type CellBox = {cx0: number; cy0: number; cx1: number; cy1: number};

/** The half-open cell block `[cx0, cx1) x [cy0, cy1)` a tile index covers. */
export function tileToCellBox({x, y, z}: TileIndex): CellBox {
  const span = CELL_GRID / 2 ** z;
  return {cx0: x * span, cy0: y * span, cx1: (x + 1) * span, cy1: (y + 1) * span};
}

/** The data-space bbox `[x0, y0, x1, y1]` a tile covers, for `POST /v1/viewport`. */
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
 * Data space to deck.gl world space. Each axis is scaled independently, exactly as §2.5 quantises
 * them — which is what makes a tile square in world space despite a rectangular data extent.
 */
export function dataToWorldXY(x: number, y: number, q: Quantisation): [number, number] {
  return [
    ((x - q.xMin) / (q.xMax - q.xMin)) * WORLD_SIZE,
    ((y - q.yMin) / (q.yMax - q.yMin)) * WORLD_SIZE
  ];
}

/**
 * In-place data→world conversion of an interleaved x,y buffer. One pass, no allocation — this runs
 * once per tile response and the buffer is handed straight to a deck.gl binary attribute.
 */
export function positionsToWorld(positions: Float32Array, q: Quantisation): Float32Array {
  const sx = WORLD_SIZE / (q.xMax - q.xMin);
  const sy = WORLD_SIZE / (q.yMax - q.yMin);
  for (let i = 0; i < positions.length; i += 2) {
    positions[i] = (positions[i]! - q.xMin) * sx;
    positions[i + 1] = (positions[i + 1]! - q.yMin) * sy;
  }
  return positions;
}
