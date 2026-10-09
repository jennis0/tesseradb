import {PolygonLayer} from '@deck.gl/layers';
import {MAX_DEPTH, WORLD_SIZE} from '@mosaica/client';

/** The host's camera: a deck.gl `OrthographicView` view state over the 512-unit world square. */
export type ViewState = {target: [number, number, number]; zoom: number; minZoom: number; maxZoom: number};

/** The camera that fits the whole world square in a canvas of this size. */
export function fitWorld(width: number, height: number): ViewState {
  return {target: [WORLD_SIZE / 2, WORLD_SIZE / 2, 0], zoom: Math.log2(Math.min(width, height) / WORLD_SIZE), minZoom: -2, maxZoom: MAX_DEPTH};
}

/** The host's own layer: the world square's edge, drawn in the same coordinates as the marks. */
export function worldEdge(): PolygonLayer<{polygon: [number, number][]}> {
  return new PolygonLayer({
    id: 'world-edge',
    data: [{polygon: [[0, 0], [WORLD_SIZE, 0], [WORLD_SIZE, WORLD_SIZE], [0, WORLD_SIZE]]}],
    getPolygon: (d) => d.polygon,
    filled: false,
    stroked: true,
    getLineColor: [200, 160, 60, 200],
    getLineWidth: 1,
    lineWidthUnits: 'pixels'
  });
}
