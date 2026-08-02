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

/**
 * The exact data-space bbox `[x0, y0, x1, y1]` a tile covers — half-open, matching
 * {@link tileToCellBox}.
 *
 * This is the honest geometry of the tile. It is **not** what to send to `/v1/viewport`; see
 * {@link tileToRequestBbox} for why.
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
 * The bbox to actually request for a tile: the tile's own cell block, inset to the **centres** of
 * its first and last cells.
 *
 * **The server's bbox is closed, not half-open.** `tessera-spatial`'s `tile_corners` quantises
 * both corners to cells and iterates `lo..=hi` inclusively, so a bbox whose upper corner lands on
 * the tile boundary — which is exactly what {@link tileToDataBbox} returns — selects the
 * neighbouring row and column of tiles as well. One request then answers for up to four tiles.
 *
 * That is not a cosmetic overlap. It was measured: summing the returned counts across loaded tiles
 * reported a `visible` of 5,128,867 against a corpus of 2,422,486, because every interior tile was
 * counted by itself and by three neighbours — and the neighbours' points were gathered, shipped
 * and drawn too, so the map over-plotted at the same time.
 *
 * Insetting to cell centres makes the request name exactly one tile at its own depth. At depth 16
 * a tile is a single cell and both corners collapse onto that cell's centre, which is a legal
 * degenerate bbox and still names one tile.
 */
export function tileToRequestBbox(
  index: TileIndex,
  q: Quantisation
): [number, number, number, number] {
  const cells = tileToCellBox(index);
  const spanX = q.xMax - q.xMin;
  const spanY = q.yMax - q.yMin;
  const centre = (cell: number) => cell + 0.5;
  return [
    q.xMin + (centre(cells.cx0) / CELL_GRID) * spanX,
    q.yMin + (centre(cells.cy0) / CELL_GRID) * spanY,
    q.xMin + (centre(cells.cx1 - 1) / CELL_GRID) * spanX,
    q.yMin + (centre(cells.cy1 - 1) / CELL_GRID) * spanY
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
 * Cell→world conversion of an interleaved x,y buffer, narrowing to the `Float32Array` a deck.gl
 * binary attribute takes. One pass, one allocation per tile response.
 *
 * **No quantisation extent, and that is the point.** `decodeViewport` returns cell space, which is
 * the grid's own units — the same units this world is a scaling of — so the conversion is one
 * constant factor per axis and there is no extent for it to disagree with the server about. The
 * scale is uniform because cell space is square; a rectangular data extent is already accounted
 * for by the quantisation that produced the cells.
 *
 * This is where the `f64` positions narrow to `f32`, and therefore where precision is actually
 * spent: below roughly a 2^-8 fraction of a world unit the mantissa runs out. Recovering it means
 * deck.gl's `fp64` emulation — a `position64Low` attribute carrying `p - Math.fround(p)` — which
 * is a layer-side change, not a wire one.
 */
export function positionsToWorld(positions: Float64Array): Float32Array {
  const out = new Float32Array(positions.length);
  for (let i = 0; i < positions.length; i++) {
    out[i] = positions[i]! / CELLS_PER_WORLD_UNIT;
  }
  return out;
}
