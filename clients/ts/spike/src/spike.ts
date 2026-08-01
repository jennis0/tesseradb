import {Deck, OrthographicView} from '@deck.gl/core';
import {TileLayer} from '@deck.gl/geo-layers';
import {PathLayer, TextLayer} from '@deck.gl/layers';
import {MAX_DEPTH, TILE_SIZE, WORLD_SIZE, tileToCellBox} from './convention.js';

type TileData = {label: string; box: [number, number][]; centre: [number, number]};

new Deck({
  views: new OrthographicView({flipY: true}),
  initialViewState: {target: [WORLD_SIZE / 2, WORLD_SIZE / 2, 0], zoom: 0},
  controller: true,
  layers: [
    new TileLayer<TileData>({
      id: 'spike',
      tileSize: TILE_SIZE,
      minZoom: 0,
      maxZoom: MAX_DEPTH,
      extent: [0, 0, WORLD_SIZE, WORLD_SIZE],
      refinementStrategy: 'best-available',
      getTileData: async ({index, bbox, signal}) => {
        // A deliberate delay, so fast panning exercises the abort path.
        await new Promise((r) => setTimeout(r, 300));
        if (signal?.aborted) throw new Error('aborted');
        const b = bbox as {left: number; top: number; right: number; bottom: number};
        const cells = tileToCellBox(index);
        return {
          label: `${index.z}/${index.x}/${index.y}\ncells ${cells.cx0},${cells.cy0}`,
          box: [
            [b.left, b.top],
            [b.right, b.top],
            [b.right, b.bottom],
            [b.left, b.bottom],
            [b.left, b.top]
          ],
          centre: [(b.left + b.right) / 2, (b.top + b.bottom) / 2]
        };
      },
      renderSubLayers: (props) => {
        const data = props.data as TileData | null;
        if (!data) return null;
        return [
          new PathLayer({
            id: `${props.id}-box`,
            data: [data.box],
            getPath: (d: [number, number][]) => d,
            getColor: [90, 130, 180],
            getWidth: 1,
            widthUnits: 'pixels'
          }),
          new TextLayer({
            id: `${props.id}-label`,
            data: [data],
            getPosition: (d: TileData) => d.centre,
            getText: (d: TileData) => d.label,
            getSize: 14,
            getColor: [230, 230, 230]
          })
        ];
      }
    })
  ]
});
