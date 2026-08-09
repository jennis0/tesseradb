import {OrthographicView, type Layer} from '@deck.gl/core';
import {ScatterplotLayer} from '@deck.gl/layers';
import {
  MARGIN,
  RING_MARGIN,
  MAX_DEPTH,
  WORLD_SIZE,
  calibrate,
  chooseDepth,
  plan,
  worldBbox,
  TesseraError,
  type Replica
} from '@tessera/client';
import {assemble, assertDrawsEveryServedMark, type Assembled} from './assemble.js';
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
 * How still the view must be before anticipatory work starts.
 *
 * Long enough that a continuous drag never triggers it — every frame of a drag cancels and re-arms
 * — and short enough that the pause between two flicks is used.
 */
const IDLE_MS = 250;

/**
 * How far the view must move, as a fraction of its own width, before another ring is worth buying.
 *
 * The ring already reaches `RING_MARGIN` beyond the visible box, so a drift well inside that is
 * already covered by what was fetched last time.
 */
const RING_RESEEK_FRACTION = 0.25;

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
 * - **The cache lives below this, in the replica.** This layer decides *which tiles to want*; the
 *   replica decides which of those need asking for and answers the rest from held bands. Keeping
 *   the two apart is what lets a consumer with its own tile scheduler use the replica without
 *   running two schedulers against each other (client-interaction §10).
 * - **At most one request outstanding**; a view change aborts the previous one. That is the whole
 *   of the coalescing story at this layer, and it relies on the server's D-C cancellation
 *   (`viewer.rs`: a `CancelGuard` flips a `CancelToken` when axum drops the handler), without which
 *   an abandoned pan would still cost the server a full request.
 * - **Marks render as one binary attribute buffer**, assembled across bands. See `assemble.ts` for
 *   why this is a concat rather than a slab, and at what mark budget that stops being true.
 */
export class ViewportController {
  private timer: ReturnType<typeof setTimeout> | null = null;
  private inFlight: AbortController | null = null;
  /** The bbox and depth last asked for, for the covered-view check. */
  private held: {bbox: [number, number, number, number]; depth: number} | null = null;
  /** Monotonic; a response from an older request is dropped rather than rendered. */
  private generation = 0;

  constructor(
    private readonly store: Store,
    private readonly replica: Replica,
    /**
     * Whether to buy the anticipatory ring at all.
     *
     * Separate from the replica's own switch, because they are different things: the cache decides
     * what a request is *answered from*, look-ahead decides what is *asked for*. An operator
     * worried about aggregate select CPU wants to turn off the second without losing the first,
     * and the measurement wants each arm on its own.
     */
    private readonly prefetch = true
  ) {}

  /** When the view last moved — the clock a user's sense of lag actually starts on. */
  private movedAt = 0;
  /** The previous `schedule` call, for telling a discrete gesture from a continuous one. */
  private lastScheduleAt = 0;
  /** When a request last went out, so the leading edge cannot become a request storm. */
  private lastRequestAt = 0;
  /**
   * Recent movement, in world units per millisecond, which biases the anticipatory ring downwind.
   *
   * A pan continues in the direction it started far more often than it reverses, so a ring shifted
   * along recent movement buys the next second of panning at the same tile cost as a centred one.
   */
  private velocity: [number, number] | undefined;
  private lastTarget: [number, number] | null = null;
  /** Where the last ring was centred, so a view that has barely moved asks for nothing. */
  private ringAt: {x: number; y: number; depth: number} | null = null;
  /** The idle timer that starts anticipatory work, and the request it started. */
  private idleTimer: ReturnType<typeof setTimeout> | null = null;
  private background: AbortController | null = null;

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
    const elapsed = now - this.lastScheduleAt;
    const target: [number, number] = [view.target[0], view.target[1]];
    this.velocity =
      this.lastTarget && elapsed > 0 && elapsed < 200
        ? [(target[0] - this.lastTarget[0]) / elapsed, (target[1] - this.lastTarget[1]) / elapsed]
        : undefined;
    this.lastTarget = target;
    this.lastScheduleAt = now;

    // Movement cancels *pending* anticipatory work — it was chosen for a view that no longer
    // exists — but never an in-flight ring. A ring already on the wire yields bands that stay valid
    // whatever the view does next, so aborting it discards server work already spent and buys
    // nothing; the request is the cost, and it has been paid.
    if (this.idleTimer) clearTimeout(this.idleTimer);
    if (this.prefetch) {
      this.idleTimer = setTimeout(() => void this.anticipate(view, width, height), IDLE_MS);
    }

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
    if (!this.held || !this.store.state.assembled) return false;
    const want = worldBbox({target: [view.target[0], view.target[1]], zoom: view.zoom, width, height}, 1);
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
    this.cancelBackground();
    this.inFlight?.abort();
    this.inFlight = null;
    this.held = null;
    this.movedAt = 0;
    this.velocity = undefined;
    this.lastTarget = null;
    this.ringAt = null;
  }

  /**
   * Has the view moved far enough since the last ring to be worth another?
   *
   * The threshold is a fraction of the viewport, so it scales with zoom automatically: at depth 14
   * a viewport is a thousandth of the width it is at depth 4, and a fixed world-space distance
   * would be either meaningless or paralysing at one end or the other.
   */
  private ringIsStillGood(viewport: {target: [number, number]; zoom: number; width: number}, depth: number): boolean {
    if (!this.ringAt || this.ringAt.depth !== depth) return false;
    const worldWidth = viewport.width / 2 ** viewport.zoom;
    const moved = Math.hypot(viewport.target[0] - this.ringAt.x, viewport.target[1] - this.ringAt.y);
    return moved < worldWidth * RING_RESEEK_FRACTION;
  }

  /** Used only on a principal change, where the in-flight ring's bands would be unrenderable. */
  private cancelBackground() {
    if (this.idleTimer) clearTimeout(this.idleTimer);
    this.idleTimer = null;
    this.background?.abort();
    this.background = null;
  }

  /**
   * Fetch the ring around a view that has stopped moving, so the next pan is answered from held
   * bands rather than from the wire.
   *
   * **This buys latency with server work; it does not avoid work.** Most of the ring is already
   * held, so the request is far smaller than a second viewport — but the remainder is speculative,
   * for tiles the user may never visit. Measured (`probes/2026-08-08-lookahead-contention/`): the
   * share of pans needing no request at all roughly doubles — 42–51% to ~92% — for roughly half
   * again the server CPU per pan, with per-request server latency flat up to sixteen clients.
   *
   * The premium *grows* the more a user revisits ground, because the replica already makes a
   * revisit free without any anticipation. And what this helps is fast movement: a slow drag
   * outlives the debounce and is answered during the movement, so it never needed anticipating.
   *
   * Never retried and never allowed to delay the foreground: a shed background request costs the
   * user nothing but a round trip they were going to pay anyway, and retrying into a saturated
   * gate is how anticipation becomes the reason the view is slow.
   */
  private async anticipate(view: ViewState, width: number, height: number) {
    const {meta, session, budget, mTarget, lastVisibleInView} = this.store.state;
    // Never two rings at once, and never one alongside a foreground request: the foreground is what
    // the user is waiting for, and anticipation must not queue ahead of it at the admission gate.
    if (!meta || !session || this.inFlight || this.background) return;

    const viewport = {
      target: [view.target[0], view.target[1]] as [number, number],
      zoom: view.zoom,
      width,
      height
    };
    const planned = plan({
      viewport,
      budget,
      mTarget,
      maxTiles: meta.maxTilesPerRequest,
      visibleInView: lastVisibleInView ?? undefined,
      velocity: this.velocity,
      heldBytes: this.replica.bytes,
      budgetBytes: this.replica.budgetBytes
    });
    const ring = planned.background.find((b) => b.kind === 'ring');
    if (!ring) return;

    // **Hysteresis, not containment, and the distinction is what makes this work.** deck emits
    // view-state events continuously while a drag's inertia decays, and the values drift by small
    // amounts rather than repeating — so neither an equality guard nor a containment guard catches
    // them: a box shifted by a hair is not inside the previous one. Left ungated, the ring's own
    // store update re-arms the idle timer and a fresh ring fires a quarter-second later, over and
    // over: measured at seven rings per idle pause.
    //
    // The foreground never showed this because `covers` answers a repeated view outright.
    // Anticipation cannot borrow that check — its whole purpose is to fetch what the current view
    // does *not* cover — so it needs its own, and the honest form is a movement threshold: a ring
    // is bought to cover the *next* pan, and a drift of a fraction of the viewport does not need
    // another one.
    if (this.ringIsStillGood(viewport, ring.depth)) return;

    const controller = new AbortController();
    this.background = controller;
    try {
      const frame = await this.replica.fetchRegion(
        ring.rect,
        ring.depth,
        meta.selection.kMaxMarks,
        controller.signal
      );
      // The ring never draws and never calibrates. It is at a margin the user is not looking at,
      // so folding it into either would report a view that is not on screen.
      this.ringAt = {x: viewport.target[0], y: viewport.target[1], depth: ring.depth};
      this.store.update((s) => {
        s.replicaBytes = this.replica.bytes;
        s.prefetched = frame.plan.novel;
      });
    } catch {
      // Including a 429: the foreground's retry budget is the one that matters.
    } finally {
      if (this.background === controller) this.background = null;
    }
  }

  private async request(view: ViewState, width: number, height: number, attempt = 0) {
    const {meta, session, slice, budget, mTarget, lastVisibleInView} = this.store.state;
    if (!meta || !session) return;

    const inputs = {
      viewport: {target: [view.target[0], view.target[1]] as [number, number], zoom: view.zoom, width, height},
      budget,
      mTarget,
      maxTiles: meta.maxTilesPerRequest,
      visibleInView: lastVisibleInView ?? undefined,
      velocity: this.velocity,
      heldBytes: this.replica.bytes,
      budgetBytes: this.replica.budgetBytes
    };
    const planned = plan(inputs);
    const choice = planned.choice;

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
      const frame = await this.replica.fetchRegion(
        planned.foreground.rect,
        choice.depth,
        meta.selection.kMaxMarks,
        controller.signal
      );
      if (generation !== this.generation) return; // a newer request won; drop this one

      const arrivedAt = performance.now();
      this.held = {bbox: worldBbox(inputs.viewport, MARGIN), depth: choice.depth};
      const assembled = assemble(frame);
      // The one place a client could violate I7 by omission, so it throws rather than warns.
      assertDrawsEveryServedMark(assembled);

      // **Calibration reads `served`, not the drawn count.** They agree today, but a drawn count
      // also carries provisional marks from other depths and would, under elision, be the delta
      // rather than the tile's size — either of which ratchets the budget the wrong way and, being
      // one-directional, never recovers.
      const visible = assembled.visibleInView;
      const actual = assembled.exactServed;

      this.store.update((s) => {
        s.assembled = assembled;
        // Widened for the coloured column only. Widening every column would walk eighteen arrays
        // per response to build ramps nothing is displaying; the cost is paid when a column is
        // chosen, which is also when the domain first has a reader.
        if (s.colourBy) {
          const column = assembled.scalars[s.colourBy];
          const widened = column ? widenDomain(s.domains[s.colourBy] ?? null, column) : null;
          if (widened) s.domains[s.colourBy] = widened;
        }
        if (frame.response) {
          s.lastTimings = frame.response.timings;
          s.lastBytes = frame.response.bytes;
        }
        s.replicaBytes = this.replica.bytes;
        s.replicaPoints = this.replica.points;
        s.replicaBands = this.replica.bandCount;
        s.lastPlan = {omitted: frame.plan.wanted - frame.plan.novel, fetched: frame.plan.novel};
        s.lastVisibleInView = visible;
        s.mTarget = calibrate(
          {predictedMarks: choice.predictedMarks, actualMarks: actual, visibleInView: visible},
          s.mTarget,
          meta.selection.thetaTargetMarks
        );
        s.inFlight = 0;
        // Empty and loaded are different answers, and both differ from refused.
        s.status = assembled.ids.length === 0 && visible === 0 ? 'empty' : 'shown';
        s.lastError = null;
        // The breakdown a user's "it feels laggy" actually decomposes into. `waited` is time the
        // client chose to spend before asking; `server` is the server's own figure; the remainder
        // of `fetch` is transport plus Arrow decode.
        s.latency = {
          waited: Math.round(startedAt - movedAt),
          fetch: Math.round(arrivedAt - startedAt),
          // Zero when the view was answered entirely from held bands — which is the point of
          // holding them, and reads correctly as "the server did no work for this".
          server: Math.round((frame.response?.timings.serverUs ?? 0) / 1000),
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
        //
        // The replica keeps its bands: they are still valid for the tiles they name, and dropping
        // them would turn one shed request into a cold cache. What is discarded is the *assembly*,
        // which is this view's answer and is the thing now known to be incomplete.
        s.status = 'refused';
        s.assembled = null;
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
 * Fade the marks a tile borrowed from another depth.
 *
 * Provisional marks are drawn — that is the whole point of best-available rendering, and it is what
 * makes a zoom feel instant — but they must be visibly *not* the answer, because they are a
 * superset of what the definition serves for that tile. Alpha in the existing RGBA buffer rather
 * than a second layer, which would double the assembly the concat exists to keep cheap.
 */
const PROVISIONAL_ALPHA = 0.45;

function fadeProvisional(colours: Uint8Array, assembled: Assembled): void {
  if (assembled.provisional === 0) return;
  for (const tile of assembled.tiles) {
    if (tile.exact) continue;
    for (let i = tile.from; i < tile.to; i++) {
      colours[i * 4 + 3] = Math.round((colours[i * 4 + 3] ?? 255) * PROVISIONAL_ALPHA);
    }
  }
}

/**
 * The mark layer.
 *
 * **Every served mark is drawn.** The length handed to deck.gl is the served count, unconditionally
 * — no budget, no cap, no filter applies here. `buildViewportLayers` is the only place that could
 * violate I7 by omission, so the invariant is asserted rather than assumed.
 */
export function buildViewportLayers(store: Store): Layer[] {
  const {assembled, selectedWorldXY, status} = store.state;
  const layers: Layer[] = [];

  if (assembled && assembled.ids.length > 0 && status !== 'refused') {
    assertDrawsEveryServedMark(assembled);
    // One entry per drawn mark by construction — see `buildColourAttribute`. Asserted anyway,
    // because a short buffer is the one way colour could silently drop marks: deck.gl reads
    // `length` from `data`, so a short attribute renders garbage rather than failing.
    const colours = buildColourAttribute(assembled.ids.length, assembled.scalars, encodingOf(store));
    if (colours.length !== assembled.ids.length * 4) {
      throw new Error(
        `I7: colour buffer covers ${colours.length / 4} of ${assembled.ids.length} marks. ` +
          `Colour is presentation and must never decide what is drawn.`
      );
    }
    fadeProvisional(colours, assembled);
    layers.push(
      new ScatterplotLayer({
        id: 'marks',
        data: {
          length: assembled.ids.length,
          attributes: {
            getPosition: {value: assembled.positions, size: 2},
            getFillColor: {value: colours, size: 4, normalized: true}
          }
        },
        tesseraIds: assembled.ids,
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
