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
import {buildColourAttribute, widenDomain, type Encoding} from './colour.js';
import type {Store} from './state.js';

export const VIEW = new OrthographicView({id: 'ortho', flipY: true});

export const INITIAL_VIEW_STATE = {
  target: [WORLD_SIZE / 2, WORLD_SIZE / 2, 0] as [number, number, number],
  zoom: 0,
  minZoom: -2,
  maxZoom: MAX_DEPTH
};

/**
 * A pan emits per frame; the request must not. **Trailing only** — the leading edge fires
 * immediately when nothing is in flight, so a single discrete gesture (one wheel notch, a click-
 * drag that has ended) pays no debounce at all. The wait is only for the *next* change while a
 * request is already running.
 */
const DEBOUNCE_MS = 140;

/**
 * Fetch this much more than the visible box, linearly, so that small pans need no request at all.
 *
 * Costs `MARGIN²` in tiles (1.3 → 1.69×) and buys the common interaction for free: measured, most
 * drags move the view by well under 30% of its width. The alternative — requesting exactly the
 * visible box — guarantees a round trip for every pixel of movement.
 */
const MARGIN = 1.3;

/** Floor on the interval between leading-edge requests. Trailing debounce still applies between. */
const LEADING_EDGE_MIN_GAP_MS = 400;
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
  /** The bbox and depth of the response currently held, for the covered-view check. */
  private held: {bbox: [number, number, number, number]; depth: number} | null = null;
  /** Monotonic; a response from an older request is dropped rather than rendered. */
  private generation = 0;

  constructor(
    private readonly store: Store,
    private readonly client: TesseraClient
  ) {}

  /** When the view last moved — the clock a user's sense of lag actually starts on. */
  private movedAt = 0;
  /** The previous `schedule` call, for telling a discrete gesture from a continuous one. */
  private lastScheduleAt = 0;
  /** When a request last went out, so the leading edge cannot become a request storm. */
  private lastRequestAt = 0;

  /**
   * Called on every view-state change.
   *
   * Three things keep this off the wire: a **covered-view** check that skips entirely when the held
   * response already spans the new view at the same depth; a **leading edge** that fires at once
   * when nothing is in flight; and a trailing debounce for everything else.
   */
  schedule(view: ViewState, width: number, height: number) {
    const now = performance.now();
    // Measured from the LAST movement, so it answers "how long after I stopped did it appear"
    // rather than accumulating an entire abandoned interaction.
    this.movedAt = now;
    const wasStill = now - this.lastScheduleAt > DEBOUNCE_MS;
    this.lastScheduleAt = now;

    if (this.covers(view, width, height)) {
      // Already held. deck.gl re-projects the marks we have, so this pan costs nothing at all.
      this.movedAt = 0;
      return;
    }

    if (this.timer) clearTimeout(this.timer);
    // Leading edge only for a gesture that STARTS from stillness — one wheel notch, a click. A
    // continuous drag emits every frame, so it never qualifies and pays the trailing debounce
    // once. Re-arming the leading edge whenever a request settles instead turns a single 600 px
    // drag into ~28 requests, which is measured, not hypothetical.
    // The leading edge is additionally rate-limited on WALL TIME, not on frame gaps. Judging
    // stillness by the gap between view-state events is only sound when frames are fast: on a slow
    // renderer every drag step is separated by more than the debounce and each one then looks like
    // a fresh gesture, which measured out at 27 requests for one 600 px drag.
    const notRecent = now - this.lastRequestAt > LEADING_EDGE_MIN_GAP_MS;
    if (wasStill && notRecent && !this.inFlight) {
      void this.request(view, width, height);
      return;
    }
    this.timer = setTimeout(() => void this.request(view, width, height), DEBOUNCE_MS);
  }

  /** Does the held response already answer this view, at the depth the budget would ask for? */
  private covers(view: ViewState, width: number, height: number): boolean {
    if (!this.held || !this.store.state.result) return false;
    const want = this.worldBbox(view, width, height, 1);
    const [hx0, hy0, hx1, hy1] = this.held.bbox;
    const inside = want[0] >= hx0 && want[1] >= hy0 && want[2] <= hx1 && want[3] <= hy1;
    if (!inside) return false;
    // **Asymmetric, and the asymmetry is the point.** If the view now wants a DEEPER depth than
    // what is held, the user has zoomed in and is looking at fewer marks than the budget promises
    // — refetch. If it wants a SHALLOWER one, what is held is a superset of what was asked for
    // (§7.2's nesting), so it is strictly better than the request would be: keep it, and spend no
    // round trip discovering that.
    const choice = chooseDepth({
      budget: this.store.state.budget,
      mTarget: this.store.state.mTarget,
      worldBbox: want,
      maxTiles: this.store.state.meta?.maxTilesPerRequest ?? 262_144,
      visibleInView: this.store.state.lastVisibleInView ?? undefined
    });
    return choice.depth <= this.held.depth;
  }

  /** Abort anything outstanding — used on principal change, where the token itself changes. */
  cancel() {
    if (this.timer) clearTimeout(this.timer);
    this.inFlight?.abort();
    this.inFlight = null;
    this.held = null;
    this.movedAt = 0;
  }

  private worldBbox(
    view: ViewState,
    width: number,
    height: number,
    margin = MARGIN
  ): [number, number, number, number] {
    // OrthographicView: `zoom` is log2 pixels-per-world-unit.
    const scale = 2 ** view.zoom;
    const halfW = (width / 2 / scale) * margin;
    const halfH = (height / 2 / scale) * margin;
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

    // Depth is chosen for what is VISIBLE; the margin is then fetched at that depth. Choosing it
    // for the margined box instead would spend the budget on off-screen marks and quietly lower
    // the resolution of what the user is actually looking at.
    const visibleBbox = this.worldBbox(view, width, height, 1);
    const worldBbox = this.worldBbox(view, width, height);
    const choice = chooseDepth({
      budget,
      mTarget,
      worldBbox: visibleBbox,
      maxTiles: meta.maxTilesPerRequest,
      visibleInView: lastVisibleInView ?? undefined
    });

    // A superseded request must not leave the lag clock running, or every later measurement
    // accumulates the whole abandoned interaction.
    this.inFlight?.abort();
    const controller = new AbortController();
    this.inFlight = controller;
    const generation = ++this.generation;

    const movedAt = this.movedAt || performance.now();
    const startedAt = performance.now();
    this.lastRequestAt = startedAt;
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


      const arrivedAt = performance.now();
      this.held = {bbox: worldBbox, depth: choice.depth};
      const visible = response.result.tiles.reduce((a, t) => a + Number(t.visible), 0);
      const actual = response.result.ids.length;

      this.store.update((s) => {
        s.result = response.result;
        s.worldPositions = positionsToWorld(response.result.positions);
        // Widened for the coloured column only. Widening every column would walk eighteen arrays
        // per response to build ramps nothing is displaying; the cost is paid when a column is
        // chosen, which is also when the domain first has a reader.
        if (s.colourBy) {
          const column = response.result.scalars[s.colourBy];
          const widened = column ? widenDomain(s.domains[s.colourBy] ?? null, column) : null;
          if (widened) s.domains[s.colourBy] = widened;
        }
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
        // The breakdown a user's "it feels laggy" actually decomposes into. `waited` is time the
        // client chose to spend before asking; `server` is the server's own figure; the remainder
        // of `fetch` is transport plus Arrow decode.
        s.latency = {
          waited: Math.round(startedAt - movedAt),
          fetch: Math.round(arrivedAt - startedAt),
          server: Math.round(response.timings.serverUs / 1000),
          total: Math.round(performance.now() - movedAt)
        };
      });
      this.movedAt = 0;
      this.inFlight = null;
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

      this.movedAt = 0;
      this.held = null;
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
 * The current colour encoding, resolved from state.
 *
 * **Falls back to uniform rather than throwing** at every step where the state is not yet ready —
 * a column chosen before its values have resolved, a refused `/v1/categories`. Colour is
 * presentation, so an incomplete encoding must degrade to a drawn map, never to no map.
 */
function encodingOf(store: Store): Encoding {
  const {colourBy, meta, categories, categoryErrors, ranks, domains} = store.state;
  if (!colourBy || !meta) return {kind: 'uniform'};
  const column = meta.declaredScalars.find((c) => c.name === colourBy);
  if (!column) return {kind: 'uniform'};

  // A refused column colours every mark unmapped, not uniform. The distinction is the whole point:
  // uniform means "no encoding chosen", unmapped means "this value could not be named" — and the
  // legend says the latter, so the map must not quietly show the former.
  if (categoryErrors[colourBy]) return {kind: 'unmapped'};

  if (column.category) {
    const values = categories[colourBy];
    // Not yet resolved. Uniform rather than unmapped, because this state is transient and
    // flashing the whole map grey on the way to a legend is worse than leaving it alone.
    if (!values) return {kind: 'uniform'};
    return {kind: 'category', column: colourBy, rankOfCode: ranks[colourBy] ?? {}};
  }
  const domain = domains[colourBy];
  if (!domain) return {kind: 'uniform'};
  return {kind: 'numeric', column: colourBy, domain};
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
    // One entry per served mark by construction — see `buildColourAttribute`. Asserted anyway,
    // because a short buffer is the one way colour could silently drop marks: deck.gl reads
    // `length` from `data`, so a short attribute renders garbage rather than failing.
    const colours = buildColourAttribute(result.ids.length, result.scalars, encodingOf(store));
    if (colours.length !== result.ids.length * 4) {
      throw new Error(
        `I7: colour buffer covers ${colours.length / 4} of ${result.ids.length} marks. ` +
          `Colour is presentation and must never decide what is drawn.`
      );
    }
    layers.push(
      new ScatterplotLayer({
        id: 'marks',
        data: {
          length: result.ids.length,
          attributes: {
            getPosition: {value: worldPositions, size: 2},
            getFillColor: {value: colours, size: 4, normalized: true}
          }
        },
        tesseraIds: result.ids,
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
