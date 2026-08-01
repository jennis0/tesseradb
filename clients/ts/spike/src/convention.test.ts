import {describe, expect, it} from 'vitest';
import {OrthographicViewport} from '@deck.gl/core';
import {_Tileset2D as Tileset2D} from '@deck.gl/geo-layers';
import {CELL_GRID, MAX_DEPTH, TILE_SIZE, WORLD_SIZE, tileToCellBox} from './convention.js';

/** The tile indices deck.gl itself would request for a viewport, via its own Tileset2D. */
function indicesFor(zoom: number, width = 1024, height = 1024) {
  const viewport = new OrthographicViewport({
    width,
    height,
    target: [WORLD_SIZE / 2, WORLD_SIZE / 2, 0],
    zoom
  });
  const tileset = new Tileset2D({
    tileSize: TILE_SIZE,
    maxZoom: MAX_DEPTH,
    minZoom: 0,
    extent: [0, 0, WORLD_SIZE, WORLD_SIZE],
    getTileData: async () => null
  });
  tileset.update(viewport);
  return tileset.tiles.map((t) => ({x: t.index.x, y: t.index.y, z: t.index.z, bbox: t.bbox}));
}

describe('the deck.gl non-geospatial tile convention', () => {
  it('covers the whole world with a single tile at z = 0', () => {
    const tiles = indicesFor(0);
    expect(tiles.every((t) => t.z === 0)).toBe(true);
    expect(tiles).toHaveLength(1);
    expect(tiles[0]!.x).toBe(0);
    expect(tiles[0]!.y).toBe(0);
  });

  it('maps deck tile z one-to-one onto Morton depth', () => {
    // A tile at depth z spans CELL_GRID / 2^z cells on each axis.
    for (const z of [0, 1, 4, 8, 16]) {
      const box = tileToCellBox({x: 0, y: 0, z});
      expect(box.cx1 - box.cx0).toBe(CELL_GRID / 2 ** z);
      expect(box.cy1 - box.cy0).toBe(CELL_GRID / 2 ** z);
    }
  });

  it('agrees with deck.gl on every tile bbox it requests', () => {
    for (const zoom of [0, 1, 2, 3, 4]) {
      for (const tile of indicesFor(zoom)) {
        const box = tileToCellBox({x: tile.x, y: tile.y, z: tile.z});
        const b = tile.bbox as {left: number; top: number; right: number; bottom: number};
        // Both axes converted to cell space; top/bottom are compared as a sorted pair so the
        // assertion does not itself assume a y direction — the next test pins that.
        expect(box.cx0).toBeCloseTo(b.left * (CELL_GRID / WORLD_SIZE), 6);
        expect(box.cx1).toBeCloseTo(b.right * (CELL_GRID / WORLD_SIZE), 6);
        const ys = [b.top, b.bottom].sort((p, q) => p - q).map((v) => v * (CELL_GRID / WORLD_SIZE));
        const boxYs = [box.cy0, box.cy1].sort((p, q) => p - q);
        expect(boxYs[0]!).toBeCloseTo(ys[0]!, 6);
        expect(boxYs[1]!).toBeCloseTo(ys[1]!, 6);
      }
    }
  });

  it('increases tile y in the same direction as cell y', () => {
    const lower = tileToCellBox({x: 0, y: 0, z: 4});
    const higher = tileToCellBox({x: 0, y: 1, z: 4});
    expect(higher.cy0).toBeGreaterThan(lower.cy0);
  });

  it('nests: a tile’s four children exactly partition it', () => {
    const parent = tileToCellBox({x: 3, y: 5, z: 4});
    const children = [
      tileToCellBox({x: 6, y: 10, z: 5}),
      tileToCellBox({x: 7, y: 10, z: 5}),
      tileToCellBox({x: 6, y: 11, z: 5}),
      tileToCellBox({x: 7, y: 11, z: 5})
    ];
    expect(Math.min(...children.map((c) => c.cx0))).toBe(parent.cx0);
    expect(Math.max(...children.map((c) => c.cx1))).toBe(parent.cx1);
    expect(Math.min(...children.map((c) => c.cy0))).toBe(parent.cy0);
    expect(Math.max(...children.map((c) => c.cy1))).toBe(parent.cy1);
  });
});
