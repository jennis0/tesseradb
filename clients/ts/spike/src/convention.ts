/**
 * The mapping between deck.gl's non-geospatial tile indices and Tessera's Morton cell grid.
 *
 * The engine's world is a 2^16 x 2^16 cell grid, quantised per axis (design §2.5), so a tile is
 * square in CELL space and rectangular in data space. The viewer therefore uses cell space as its
 * deck.gl world, scaled down by CELLS_PER_WORLD_UNIT so that a tile is TILE_SIZE world units at
 * depth 0 — which makes a deck tile `z` identically a Morton depth.
 *
 * Every constant here was checked against deck.gl's own Tileset2D in convention.test.ts. Change
 * them only with that test.
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
