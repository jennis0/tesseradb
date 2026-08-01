import {OrthographicView, type Layer} from '@deck.gl/core';
import {TileLayer} from '@deck.gl/geo-layers';
import {BitmapLayer, ScatterplotLayer} from '@deck.gl/layers';
import {
  MAX_DEPTH,
  TILE_SIZE,
  WORLD_SIZE,
  positionsToWorld,
  tileToRequestBbox,
  type TesseraClient,
  type ViewportResult
} from '@tessera/client';
import type {Store} from './state.js';
import {subCellsToImage} from './underlay.js';

export const VIEW = new OrthographicView({id: 'ortho', flipY: true});

export const INITIAL_VIEW_STATE = {
  target: [WORLD_SIZE / 2, WORLD_SIZE / 2, 0] as [number, number, number],
  zoom: 0,
  minZoom: -2,
  maxZoom: MAX_DEPTH
};

export type TilePayload = {result: ViewportResult; worldPositions: Float32Array};

/**
 * One TileLayer whose id encodes everything that invalidates its cache: k, the term set, the slice
 * and the underlay offset. Changing any of them mints a new layer, which drops the cache
 * wholesale. Crude and correct — this is exactly where the replica store lands later.
 */
export function buildLayers(store: Store, client: TesseraClient): Layer[] {
  const {meta, session, slice, k, underlayOffset, termsLabel} = store.state;
  if (!meta || !session) return [];

  const layerId = `tiles:${slice}:${termsLabel}:${k ?? 'default'}:${underlayOffset}`;
  const {selectedWorldXY} = store.state;

  return [
    new TileLayer<TilePayload>({
      id: layerId,
      tileSize: TILE_SIZE,
      minZoom: 0,
      maxZoom: MAX_DEPTH,
      extent: [0, 0, WORLD_SIZE, WORLD_SIZE],
      // Correct for us rather than merely tolerable: §7.2's nesting makes every child a superset
      // of its parent's marks, so showing a parent while children load is add-only and does not
      // pop. The rejected pre-r22 rank-position sampler would have popped on every refinement.
      refinementStrategy: 'best-available',
      maxRequests: 6,

      getTileData: async ({index, signal}) => {
        store.update((s) => {
          s.inFlight += 1;
        });
        try {
          const response = await client.viewport(
            session.token,
            {
              slice,
              zoom: index.z,
              bbox: tileToRequestBbox(index, meta.quantisation),
              k,
              underlayOffset
            },
            signal ?? undefined
          );
          // deck.gl's cancellation contract is explicitly fail-closed: on abort, throw or return
          // falsy so nothing incomplete is cached. Never return partial data.
          if (signal?.aborted) throw new Error('aborted');

          store.update((s) => {
            s.lastTimings = response.timings;
            s.lastBytes = response.bytes;
          });
          return {
            result: response.result,
            worldPositions: positionsToWorld(response.result.positions, meta.quantisation)
          };
        } catch (error) {
          // An abort is not a failure: deck.gl aborts tiles that left the viewport, and recording
          // those would bury real refusals under pan noise.
          if (!signal?.aborted) {
            const e = error as {code?: string; detail?: string; message?: string};
            store.update((s) => {
              s.failures.push({
                tileId: `${index.z}/${index.x}/${index.y}`,
                code: e.code ?? 'fetch-failed',
                detail: e.detail ?? e.message ?? String(error),
                at: Date.now()
              });
            });
          }
          throw error;
        } finally {
          store.update((s) => {
            s.inFlight -= 1;
          });
        }
      },

      onViewportLoad: (tiles) => {
        store.update((s) => {
          s.tiles.clear();
          for (const tile of tiles ?? []) {
            const payload = tile.content as TilePayload | null;
            if (!payload) continue;
            s.tiles.set(String(tile.id), {
              z: tile.index.z,
              counts: payload.result.tiles,
              pointCount: payload.result.ids.length
            });
          }
        });
      },

      renderSubLayers: (props) => {
        const payload = props.data as TilePayload | null;
        if (!payload) return null;
        const layers: Layer[] = [];
        const offset = store.state.underlayOffset;

        if (offset > 0 && payload.result.subCells) {
          const b = props.tile.bbox as {
            left: number;
            top: number;
            right: number;
            bottom: number;
          };
          layers.push(
            new BitmapLayer({
              id: `${props.id}-underlay`,
              image: subCellsToImage(payload.result.subCells, offset),
              // BitmapLayer takes [left, bottom, right, top]. Under `flipY` the tile's `top` is
              // numerically the smaller y, so passing the bbox's own fields in this order puts the
              // image's first row at the tile's first cell row.
              bounds: [b.left, b.bottom, b.right, b.top],
              // Nearest, not linear: these are exact per-cell counts, and interpolating between
              // them invents densities the server never reported. Crisp cells also make a
              // misaligned underlay obvious instead of blurring it into plausibility.
              textureParameters: {minFilter: 'nearest', magFilter: 'nearest'},
              // Translucent with depth off: tiles render in arbitrary order, so one tile's cells
              // could otherwise overdraw an adjacent tile's marks (client-interaction §8.2).
              opacity: 0.75,
              parameters: {depthCompare: 'always' as const}
            })
          );
        }

        if (payload.result.ids.length === 0) return layers;
        layers.push(
          new ScatterplotLayer({
            id: `${props.id}-marks`,
            data: {
              length: payload.result.ids.length,
              attributes: {getPosition: {value: payload.worldPositions, size: 2}}
            },
            // Carried explicitly rather than read back off `data`: picking returns a positional
            // index, and this is what resolves it to an identity app-side, so `tessera_id` never
            // enters the render path.
            tesseraIds: payload.result.ids,
            getFillColor: [120, 190, 255, 200],
            radiusUnits: 'pixels',
            getRadius: 1.6,
            radiusMinPixels: 1,
            pickable: true,
            parameters: {depthCompare: 'always' as const}
          })
        );
        return layers;
      }
    }),
    // A layer of its own, not a highlight prop: TileLayer overrides its sublayers'
    // `highlightedObjectIndex`, so a highlight set on the scatterplot would not survive
    // (client-interaction §8.2).
    ...(selectedWorldXY
      ? [
          new ScatterplotLayer({
            id: 'selection',
            data: [selectedWorldXY],
            getPosition: (d: [number, number]) => d,
            getFillColor: [255, 210, 90, 255],
            radiusUnits: 'pixels' as const,
            getRadius: 5,
            stroked: true,
            getLineColor: [20, 20, 20, 255],
            lineWidthUnits: 'pixels' as const,
            getLineWidth: 1.5,
            parameters: {depthCompare: 'always' as const}
          })
        ]
      : [])
  ];
}
