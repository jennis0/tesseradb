import {OrthographicView, type Layer} from '@deck.gl/core';
import {ScatterplotLayer} from '@deck.gl/layers';
import {
  MAX_DEPTH,
  WORLD_SIZE,
  calibrate,
  chooseDepth,
  positionsToWorld,
  TesseraError,
  type TesseraClient,
  type ViewportResult
} from '@tessera/client';
import type {Store} from './state.js';

export const VIEW = new OrthographicView({id: 'ortho', flipY: true});

export const INITIAL_VIEW_STATE = {
  target: [WORLD_SIZE / 2, WORLD_SIZE / 2, 0] as [number, number, number],
  zoom: 0,
  minZoom: -2,
  maxZoom: MAX_DEPTH
};

/** A pan emits per frame; the request must not. */
const DEBOUNCE_MS = 120;
/** The server sends `Retry-After: 1`. Bounded, because an unbounded retry amplifies saturation. */
const MAX_RETRIES = 2;

export type ViewState = {
  target: [number, number, number];
  zoom: number;
};

/**
 * One request per view, not one per tile.
 *
 * `POST /v1/viewport` is viewport-addressed: a bbox spanning many tiles returns every tile's counts
 * plus a flat points batch. deck.gl's `TileLayer` is tile-addressed and issues one fetch per tile,
 * which at 10⁹ shed 12 of 23 requests from a single browser tab (client-interaction §8.2's
 * annotation). This keeps the verb's own shape.
 *
 * Consequences, all deliberate:
 * - **No per-tile cache.** A pan refetches the view — one request. Caching belongs to the replica
 *   store (client-interaction §10), not smuggled in here.
 * - **At most one request outstanding**; a view change aborts the previous one. That is the whole
 *   of the coalescing story at this layer, and it relies on the server's D-C cancellation
 *   (`viewer.rs`: a `CancelGuard` flips a `CancelToken` when axum drops the handler), without which
 *   an abandoned pan would still cost the server a full request.
 * - **Marks render as one binary attribute buffer.** `served` still splits the batch per tile for
 *   the counts panel; rendering does not need the split.
 */
export class ViewportController {
  private timer: ReturnType<typeof setTimeout> | null = null;
  private inFlight: AbortController | null = null;
  /** Monotonic; a response from an older request is dropped rather than rendered. */
  private generation = 0;

  constructor(
    private readonly store: Store,
    private readonly client: TesseraClient
  ) {}

  /** Called on every view-state change; coalesces to one request. */
  schedule(view: ViewState, width: number, height: number) {
    if (this.timer) clearTimeout(this.timer);
    this.timer = setTimeout(() => void this.request(view, width, height), DEBOUNCE_MS);
  }

  /** Abort anything outstanding — used on principal change, where the token itself changes. */
  cancel() {
    if (this.timer) clearTimeout(this.timer);
    this.inFlight?.abort();
    this.inFlight = null;
  }

  private worldBbox(view: ViewState, width: number, height: number): [number, number, number, number] {
    // OrthographicView: `zoom` is log2 pixels-per-world-unit.
    const scale = 2 ** view.zoom;
    const halfW = width / 2 / scale;
    const halfH = height / 2 / scale;
    const clamp = (v: number) => Math.min(WORLD_SIZE, Math.max(0, v));
    return [
      clamp(view.target[0] - halfW),
      clamp(view.target[1] - halfH),
      clamp(view.target[0] + halfW),
      clamp(view.target[1] + halfH)
    ];
  }

  private async request(view: ViewState, width: number, height: number, attempt = 0) {
    const {meta, session, slice, budget, mTarget, lastVisibleInView} = this.store.state;
    if (!meta || !session) return;

    const worldBbox = this.worldBbox(view, width, height);
    const choice = chooseDepth({
      budget,
      mTarget,
      worldBbox,
      maxTiles: meta.maxTilesPerRequest,
      visibleInView: lastVisibleInView ?? undefined
    });

    this.inFlight?.abort();
    const controller = new AbortController();
    this.inFlight = controller;
    const generation = ++this.generation;

    this.store.update((s) => {
      s.view = {...choice, requestedAt: Date.now()};
      s.status = 'loading';
      s.inFlight = 1;
    });

    try {
      const dataBbox = worldToDataBbox(worldBbox, meta.quantisation);
      const response = await this.client.viewport(
        session.token,
        {slice, zoom: choice.depth, bbox: dataBbox, k: meta.selection.kMaxMarks},
        controller.signal
      );
      if (generation !== this.generation) return; // a newer request won; drop this one

      const visible = response.result.tiles.reduce((a, t) => a + Number(t.visible), 0);
      const actual = response.result.ids.length;

      this.store.update((s) => {
        s.result = response.result;
        s.worldPositions = positionsToWorld(response.result.positions, meta.quantisation);
        s.lastTimings = response.timings;
        s.lastBytes = response.bytes;
        s.lastVisibleInView = visible;
        s.mTarget = calibrate(
          {predictedMarks: choice.predictedMarks, actualMarks: actual, visibleInView: visible},
          s.mTarget,
          meta.selection.thetaTargetMarks
        );
        s.inFlight = 0;
        // Empty and loaded are different answers, and both differ from refused.
        s.status = actual === 0 && visible === 0 ? 'empty' : 'shown';
        s.lastError = null;
      });
    } catch (error) {
      if (controller.signal.aborted || generation !== this.generation) return;

      // 429 is now whole-viewport rather than one tile, so a shed request blanks the map. The
      // server sends `Retry-After: 1`; honour it, bounded, because retrying without backoff
      // amplifies the very saturation being reported.
      const shed = error instanceof TesseraError && error.status === 429;
      if (shed && attempt < MAX_RETRIES) {
        const delay = 1000 * 2 ** attempt;
        this.store.update((s) => {
          s.status = 'retrying';
        });
        setTimeout(() => void this.request(view, width, height, attempt + 1), delay);
        return;
      }

      const e = error as {code?: string; detail?: string; message?: string};
      this.store.update((s) => {
        s.inFlight = 0;
        // REFUSED, not empty. The marks from the previous view are now geometrically wrong for
        // this one, so they are dropped rather than left under a new transform — an empty region
        // and a failed one are semantic opposites (client-interaction §9).
        s.status = 'refused';
        s.result = null;
        s.worldPositions = null;
        s.lastError = {
          code: e.code ?? 'fetch-failed',
          detail: e.detail ?? e.message ?? String(error)
        };
        s.failures.push({
          tileId: `view d=${choice.depth}`,
          code: e.code ?? 'fetch-failed',
          detail: e.detail ?? e.message ?? String(error),
          at: Date.now()
        });
      });
    }
  }
}

function worldToDataBbox(
  world: [number, number, number, number],
  q: {xMin: number; xMax: number; yMin: number; yMax: number}
): [number, number, number, number] {
  const sx = (q.xMax - q.xMin) / WORLD_SIZE;
  const sy = (q.yMax - q.yMin) / WORLD_SIZE;
  return [
    q.xMin + world[0] * sx,
    q.yMin + world[1] * sy,
    q.xMin + world[2] * sx,
    q.yMin + world[3] * sy
  ];
}

/**
 * The mark layer.
 *
 * **Every served mark is drawn.** The length handed to deck.gl is the served count, unconditionally
 * — no budget, no cap, no filter applies here. `buildViewportLayers` is the only place that could
 * violate I7 by omission, so the invariant is asserted rather than assumed.
 */
export function buildViewportLayers(store: Store): Layer[] {
  const {result, worldPositions, selectedWorldXY, status} = store.state;
  const layers: Layer[] = [];

  if (result && worldPositions && result.ids.length > 0 && status !== 'refused') {
    const served = result.tiles.reduce((a, t) => a + Number(t.served), 0);
    if (result.ids.length !== served) {
      throw new Error(
        `I7: drawing ${result.ids.length} marks but the server served ${served}. ` +
          `The client must draw every mark it is served.`
      );
    }
    layers.push(
      new ScatterplotLayer({
        id: 'marks',
        data: {
          length: result.ids.length,
          attributes: {getPosition: {value: worldPositions, size: 2}}
        },
        tesseraIds: result.ids,
        getFillColor: [120, 190, 255, 200],
        radiusUnits: 'pixels' as const,
        getRadius: 1.6,
        radiusMinPixels: 1,
        pickable: true,
        parameters: {depthCompare: 'always' as const}
      })
    );
  }

  if (selectedWorldXY) {
    layers.push(
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
    );
  }

  return layers;
}
