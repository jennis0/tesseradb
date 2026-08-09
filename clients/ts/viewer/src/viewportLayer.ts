import {OrthographicView, type Layer} from '@deck.gl/core';
import {ScatterplotLayer} from '@deck.gl/layers';
import {
  MARGIN,
  RENDER_MARGIN,
  tileRectOfBbox,
  rectContains,
  RING_MARGIN,
  MAX_DEPTH,
  WORLD_SIZE,
  calibrate,

  plan,
  worldBbox,
  TesseraError,
  type Replica
} from '@tessera/client';
import {
  assemble,
  assembledMarks,
  assertAssemblyMatchesServed,
  foldBandColumn,
  refreshExact,
  type Assembled
} from './assemble.js';
import {buildColourAttribute, widenDomain, type Encoding} from './colour.js';
import {readConfig} from './config.js';
import {MarkSlab} from './slab.js';
import {trace} from './trace.js';
import type {Store} from './state.js';

/** The two debug knobs — see {@link ViewerConfig}. Read once: neither changes within a session. */
const RENDER = readConfig();

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
 * The wait when the store cannot answer the view at all.
 *
 * **A debounce is for suppressing redundant requests, and this request is not redundant.** The 140 ms
 * above assumes the next frame may make the pending request pointless — true while the view is
 * still inside ground the replica holds, false the moment it is not. An aggressive pan of a third
 * to a half of a viewport escapes both the drawn buffer and the fetched margin, and then the client
 * spends 140 ms deciding to ask for something it already knows it needs, against a fetch that
 * measures 14-45 ms. The delay was the larger half of the gap.
 *
 * Not zero: a drag still emits per frame, and at most one request is in flight with the previous
 * aborted, so this bounds how many the server is asked to start and then cancel.
 */
const UNCACHED_DEBOUNCE_MS = 30;

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
/**
 * Anticipatory fetches allowed between one movement and the next.
 *
 * Bounds what a still view costs. Each is one bounded request, so this is the number of bites taken
 * out of the graded ring before the client waits to be asked for something.
 */
const MAX_PREFETCH_PER_PAUSE = 3;

/**
 * Bytes anticipation may absorb between one movement and the next.
 *
 * **Bounded in bytes rather than in requests, because bytes are what costs.** Decode is in a worker,
 * but splitting a response into bands — the slicing and the cell-to-world pass — is on this thread,
 * and it scales with points rather than with responses. A request-count budget lets one pause pull
 * 12 MB on a dense corpus and 200 KB on a sparse one; a byte budget spends the same effort either
 * way and is what the user feels.
 *
 * Measured on the demo corpus at a 5 × 10^4 mark budget, three bites per pause moved 7.6–12.7 MB
 * per pan — an order more than the view itself needed.
 */
const MAX_PREFETCH_BYTES_PER_PAUSE = 2_000_000;

/**
 * How long after the last arrival the full stand-in derivation runs.
 *
 * An arrival folds its exact bands into the drawn frame at once and leaves the stand-ins one
 * arrival stale — drawable, since a stale stand-in is a superset of the same served points. The
 * settle pass is the real derivation, run when arrivals pause rather than inside their frames.
 */
const SETTLE_MS = 150;
/**
 * The most a stream of arrivals may defer the settle. A hard deadline, because during a continuous
 * drag every arrival re-arms the timer and stand-ins would otherwise stay stale for the whole
 * gesture — one derivation frame per this interval is the bounded cost of that.
 */
const SETTLE_MAX_MS = 700;
/**
 * The least time between two full derivations on the gesture path.
 *
 * A zoom changes depth on every frame it emits, and a depth change fails the covered test — so a
 * wheel gesture was paying the full derive (the walk, the stand-in concat, the colour build) once
 * per animation frame: 118 of the 122 derives in a zoom-heavy session, stacked into exactly the
 * frames that missed p95. Marks are in world space, so between derivations the drawn frame
 * re-projects correctly under the moving camera; re-deriving at ~8 Hz swaps in the new depth's
 * marks faster than a viewer can track, and the trailing settle catches the final state.
 */
const DERIVE_MIN_GAP_MS = 120;

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
 * - **Marks render from a persistent slab**, one binary attribute buffer with a stable slot per
 *   band, so a frame that adds nothing re-uploads nothing. See `slab.ts` for what that buys and for
 *   the one thing it still does not do.
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
  /** The deferred full derivation — see {@link SETTLE_MS}. Latest view wins on re-arm. */
  private settleTimer: ReturnType<typeof setTimeout> | null = null;
  private settleDeadline = 0;
  /** When the gesture path last paid a full derivation — see {@link DERIVE_MIN_GAP_MS}. */
  private lastFullDeriveAt = 0;
  /** The most recent view the controller was asked about — what a deferred settle derives for. */
  private lastSchedule: {view: ViewState; width: number; height: number} | null = null;
  /** The idle timer that starts anticipatory work, and the request it started. */
  private idleTimer: ReturnType<typeof setTimeout> | null = null;
  private background: AbortController | null = null;
  /**
   * Anticipatory fetches issued since the view last moved.
   *
   * **A graded ring always has work left in it**, so "is anything novel" cannot be the whole guard:
   * without a budget the idle timer re-fires every quarter-second, finds the next strip, and issues
   * requests forever — starving the foreground at the connection pool and at the admission gate,
   * which measured as pan-to-paint in the tens of seconds while the server's own share stayed in
   * single-digit milliseconds. The budget resets when the user moves, because that is when what to
   * anticipate has changed.
   */
  private prefetchesSinceMove = 0;
  private prefetchBytesSinceMove = 0;
  /** The newest view awaiting a cache redraw, and the frame callback that will draw it. */
  private pendingRedraw: {view: ViewState; width: number; height: number} | null = null;
  private redrawHandle: number | null = null;

  /**
   * Called on every view-state change.
   *
   * Three things keep this off the wire: a **covered-view** check that skips entirely when the held
   * response already spans the new view at the same depth; a **leading edge** that fires at once
   * when nothing is in flight; and a trailing debounce for everything else.
   */
  schedule(view: ViewState, width: number, height: number) {
    const now = performance.now();
    this.lastSchedule = {view, width, height};
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
    this.prefetchesSinceMove = 0;
    this.prefetchBytesSinceMove = 0;

    // A fresh still period, so anticipation may spend again.
    // Movement cancels *pending* anticipatory work — it was chosen for a view that no longer
    // exists — but never an in-flight ring. A ring already on the wire yields bands that stay valid
    // whatever the view does next, so aborting it discards server work already spent and buys
    // nothing; the request is the cost, and it has been paid.
    if (this.idleTimer) clearTimeout(this.idleTimer);
    if (this.prefetch) {
      this.idleTimer = setTimeout(() => void this.anticipate(view, width, height), IDLE_MS);
    }

    if (this.covers(view, width, height)) {
      // Already drawn. deck.gl re-projects the marks we have, so this pan costs nothing at all.
      this.movedAt = 0;
      return;
    }

    // **Escaped the drawn buffer: redraw from the store at once, before deciding about fetching.**
    // The two are separate concerns and only one of them needs rate-limiting. A drag emits per
    // frame and must not become a request per frame, which is what the debounce below is for;
    // reading the store is local and costs microseconds, and putting it behind the same delay
    // makes every view change wait on a network policy before consulting a cache that could have
    // answered instantly. Measured before this existed: 65-197 ms to repaint a view that needed
    // **zero** requests, which is pop-in with a warm cache and nothing to fetch.
    //
    // It also covers the zoom case, which a wider drawn buffer cannot. A zoom lands at a depth
    // nothing is held at, but prefix nesting means the parent band restricted to the new view is a
    // legitimate superset — drawn immediately, stale-marked, and replaced when the fetch lands.
    this.scheduleRedraw(view, width, height);

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
    // Planning is a rectangle subtraction, so asking whether the store can answer this view costs
    // microseconds and can be done on the way past.
    this.timer = setTimeout(
      () => void this.request(view, width, height),
      this.storeCanAnswer(view, width, height) ? DEBOUNCE_MS : UNCACHED_DEBOUNCE_MS
    );
  }

  /** Can the replica answer this view outright — same question, for the debounce. */
  private storeCanAnswer(view: ViewState, width: number, height: number): boolean {
    return this.covers(view, width, height);
  }

  /**
   * Coalesce redraws to one per animation frame, keeping the newest view.
   *
   * **A redraw is cheap but not free, and view changes arrive faster than frames.** A wheel-zoom
   * or a drag emits several view states per frame, and running an assembly for each one puts tens
   * of milliseconds of main-thread work behind every intermediate state nobody ever sees — which
   * is how a 136 ms assembly at 10^6 marks became a multi-second freeze. Rate-limited on frames
   * rather than on a timer, because the frame is the only rate at which a redraw can be observed.
   */
  private scheduleRedraw(view: ViewState, width: number, height: number) {
    this.pendingRedraw = {view, width, height};
    if (this.redrawHandle !== null) return;
    this.redrawHandle = requestAnimationFrame(() => {
      this.redrawHandle = null;
      const next = this.pendingRedraw;
      this.pendingRedraw = null;
      if (next) this.redrawFromCache(next.view, next.width, next.height);
    });
  }

  /**
   * Draw whatever the store holds for this view, synchronously and without touching the network.
   *
   * Never touches the calibration: `m_target` corrects against what the *server* served, and
   * feeding it a cache read would ratchet the depth on evidence the server never gave. Nor does it
   * blank the view — a redraw with nothing held leaves the previous marks up, because an empty
   * cache is not an empty region and the two must not look alike (client-interaction §9).
   */
  /**
   * Fold what the store now holds into the drawn frame at the smallest cost that is correct, and
   * schedule the full derivation for when things go quiet.
   */
  private scheduleSettle(view: ViewState, width: number, height: number): void {
    const now = performance.now();
    if (this.settleTimer) clearTimeout(this.settleTimer);
    else this.settleDeadline = now + SETTLE_MAX_MS;
    const wait = Math.min(SETTLE_MS, Math.max(0, this.settleDeadline - now));
    this.settleTimer = setTimeout(() => {
      this.settleTimer = null;
      // The view the user is looking at NOW, not the one captured when the settle was scheduled —
      // a drag between the two would derive a region the screen has already left.
      const at = this.lastSchedule ?? {view, width, height};
      this.redrawFromCache(at.view, at.width, at.height, true);
    }, wait);
  }

  private redrawFromCache(view: ViewState, width: number, height: number, settle = false) {
    const {meta, session, budget, mTarget, lastVisibleInView} = this.store.state;
    if (!meta || !session) return;

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
      heldBytes: this.replica.bytes,
      budgetBytes: this.replica.budgetBytes
    });
    // **The frame already on screen may still be the answer, and re-deriving one is not free.**
    // Deriving a frame is per-band work — a region query, then a restriction and a column fold per
    // stand-in band — and it measured ~35 ms at 3.9 × 10^4 stand-in bands, on every animation frame
    // of a drag. Three things decide it, and all three are exact rather than approximate: the depth
    // must be the one being drawn, the store must not have changed under it, and the region wanted
    // must lie inside the region the held frame was built for. Marks are in world space, so a pan
    // inside that region is a deck.gl re-projection and needs nothing from this client at all.
    const held = this.store.state.assembled;
    const covered =
      held && held.depth === planned.choice.depth && rectContains(held.want, planned.render);
    // A settle may reuse too — but only a frame whose stand-ins were actually derived for it. A
    // folded frame matches the store's version while carrying stale stand-ins, which is the very
    // thing a settle repairs; a fully derived one leaves it nothing to do, and re-deriving it was
    // measured as two full derives in a single 327 ms frame.
    if (covered && held.version === this.replica.version && (!settle || !held.standInStale)) {
      trace.event('reuse', {depth: held.depth, n: held.bands.length});
      return;
    }
    // **An arrival folds in; it does not re-derive.** The store changed under a frame that still
    // covers this view, which before this meant the full derivation — the stand-in walk and concat,
    // 10^4 bands of it — inside the arrival's own frame. The exact half is recomputed from the
    // depth index (cheap and exact); the stand-ins ride along one arrival stale, and the settle
    // pass rebuilds them once arrivals pause.
    if (covered && !settle) {
      const refreshed = trace.phase(
        'refresh',
        () => refreshExact(held, this.replica.exactIn(held.want, held.depth), this.replica.version),
        {depth: held.depth}
      );
      assertAssemblyMatchesServed(refreshed);
      this.store.update((s) => {
        s.assembled = refreshed;
      });
      this.scheduleSettle(view, width, height);
      return;
    }

    // The floor between full derivations — see {@link DERIVE_MIN_GAP_MS}. The frame on screen
    // re-projects meanwhile, and the trailing settle guarantees the final state is derived.
    const now = performance.now();
    if (!settle && now - this.lastFullDeriveAt < DERIVE_MIN_GAP_MS) {
      this.scheduleSettle(view, width, height);
      return;
    }
    this.lastFullDeriveAt = now;

    const frame = trace.phase('select', () =>
      this.replica.frameFromCache(planned.render, planned.choice.depth, meta.selection.kMaxMarks)
    );
    const assembled = trace.phase(
      'derive',
      () => assemble(frame, this.store.state.colourBy ? [this.store.state.colourBy] : []),
      {depth: frame.depth, n: frame.exact.length, standIn: frame.fallback.length, from: 'cache'}
    );
    if (assembledMarks(assembled) === 0) return;
    assertAssemblyMatchesServed(assembled);

    this.held = {bbox: worldBbox(viewport, RENDER_MARGIN), depth: planned.choice.depth};
    this.store.update((s) => {
      s.assembled = assembled;
      s.status = 'shown';
    });
  }

  /** Does the drawn buffer already answer this view, at the depth the budget would ask for? */
  private covers(view: ViewState, width: number, height: number): boolean {
    if (!this.held || !this.store.state.assembled) return false;
    const want = worldBbox({target: [view.target[0], view.target[1]], zoom: view.zoom, width, height}, 1);
    const [hx0, hy0, hx1, hy1] = this.held.bbox;
    const inside = want[0] >= hx0 && want[1] >= hy0 && want[2] <= hx1 && want[3] <= hy1;
    if (!inside) return false;
    // **Being drawn is not the same as being answered, and conflating them stalls the refinement.**
    // A redraw paints whatever the store holds, including coarse bands standing in for ground never
    // fetched at this depth — legitimately, as a superset (§7.2's nesting). But if that satisfies
    // the covered test, no request is ever scheduled and the provisional patch stays provisional
    // for the rest of the session. A pan that exposes one, and a zoom out that lands on a depth
    // nothing is keyed to, both did exactly that.
    //
    // So the store is asked whether it can actually answer the visible box. It is a rectangle
    // subtraction and costs microseconds.
    //
    // **And it must be asked about the depth on screen, not the depth the budget would pick now.**
    // Those diverge: the budget re-chooses on every view change and flips at its thresholds, so a
    // view drawn at depth 9 was being declared covered because the store could answer it at
    // depth 8 — a depth nothing on screen was drawn at. Measured, that left a view of 1,918
    // stand-in bands and 8,208 novel tiles reporting itself as answered, permanently. A depth
    // change is itself a reason to redraw, so any disagreement fails the test.
    const depth = this.plannedDepth(view, width, height);
    if (depth === null || depth !== this.held.depth) return false;
    return this.replica.novelIn(
      tileRectOfBbox(want, depth),
      depth,
      this.store.state.meta?.selection.kMaxMarks ?? 0
    ) === 0;
  }

  /** The depth the budget would ask for, or null before `meta` has arrived. */
  private plannedDepth(view: ViewState, width: number, height: number): number | null {
    const {meta, budget, mTarget, lastVisibleInView} = this.store.state;
    if (!meta) return null;
    return plan({
      viewport: {target: [view.target[0], view.target[1]], zoom: view.zoom, width, height},
      budget,
      mTarget,
      maxTiles: meta.maxTilesPerRequest,
      visibleInView: lastVisibleInView ?? undefined,
      heldBytes: this.replica.bytes,
      budgetBytes: this.replica.budgetBytes
    }).choice.depth;
  }

  /** Abort anything outstanding — used on principal change, where the token itself changes. */
  cancel() {
    if (this.timer) clearTimeout(this.timer);
    if (this.settleTimer) {
      // A settle for the old principal would derive against the new one's store — drop it; the
      // next arrival schedules its own.
      clearTimeout(this.settleTimer);
      this.settleTimer = null;
    }
    this.cancelRedraw();
    this.cancelBackground();
    this.inFlight?.abort();
    this.inFlight = null;
    this.held = null;
    this.movedAt = 0;
    this.velocity = undefined;
    this.lastTarget = null;
  }

  /** Used only on a principal change, where the in-flight ring's bands would be unrenderable. */
  private cancelRedraw() {
    if (this.redrawHandle !== null) cancelAnimationFrame(this.redrawHandle);
    this.redrawHandle = null;
    this.pendingRedraw = null;
  }

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
    if (this.prefetchesSinceMove >= MAX_PREFETCH_PER_PAUSE) return;
    if (this.prefetchBytesSinceMove >= MAX_PREFETCH_BYTES_PER_PAUSE) return;

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
    // **The nearest band with anything novel in it, coarsest last.** The bands are ordered fine and
    // near to coarse and far, so taking the first that has work fills the neighbourhood before the
    // periphery — and taking only one leaves the thread free between pauses.
    let ring = null as (typeof planned.background)[number] | null;
    for (const band of planned.background) {
      if (this.replica.novelIn(band.rect, band.depth, meta.selection.kMaxMarks) > 0) {
        ring = band;
        break;
      }
    }
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

    const controller = new AbortController();
    this.background = controller;
    try {
      // **One bite per idle pause.** The ring may be much wider than a single response should be,
      // and decode is synchronous on this thread — so it takes the nearest novel strip and leaves
      // the rest for the next pause. The region still fills; it stops doing it in one block that
      // freezes the view.
      this.prefetchesSinceMove += 1;
      // `standIns: false` — the ring never draws its frame (next comment), so deriving the
      // stand-in set for it was a walk of the held store per bite, paid for a value nobody read.
      const frame = await this.replica.fetchRegion(
        ring.rect,
        ring.depth,
        meta.selection.kMaxMarks,
        controller.signal,
        undefined,
        1,
        false
      );
      // The ring never draws and never calibrates. It is at a margin the user is not looking at,
      // so folding it into either would report a view that is not on screen.
      this.prefetchBytesSinceMove += frame.response?.bytes ?? 0;
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

    const s0 = this.store.state.colourBy;
    const movedAt = this.movedAt || performance.now();
    const startedAt = performance.now();
    this.lastRequestAt = startedAt;
    trace.event('request', {depth: choice.depth, n: choice.tiles, waited: startedAt - movedAt});
    this.store.update((s) => {
      s.view = {...choice, requestedAt: Date.now()};
      s.status = 'loading';
      s.inFlight = 1;
    });

    // Whether the drawn frame can absorb this response by folding — in which case the stand-in
    // walk is skipped at the source rather than paid and discarded.
    const foldable = (): Assembled | null => {
      const held = this.store.state.assembled;
      return held && held.depth === choice.depth && rectContains(held.want, planned.render)
        ? held
        : null;
    };

    try {
      // **Two fetches, screen first.** The visible box is what the user is waiting for; the margin
      // exists so the *next* pan costs nothing. Asking for both at once makes the screen wait for
      // 1.69× the marks it will show. The second call subtracts the first's coverage, so it fetches
      // the annulus and nothing more.
      const light = foldable() !== null;
      const frame = await this.replica.fetchRegion(
        planned.visible.rect,
        choice.depth,
        meta.selection.kMaxMarks,
        controller.signal,
        planned.render,
        undefined,
        !light
      );
      if (generation !== this.generation) return; // a newer request won; drop this one

      const arrivedAt = performance.now();
      trace.event('arrived', {
        ms: arrivedAt - startedAt,
        n: frame.response?.bytes ?? 0,
        server: Math.round((frame.response?.timings.serverUs ?? 0) / 1000),
        depth: choice.depth
      });
      // **The covered-view test is against what was DRAWN, not what was fetched.** Panning inside
      // the drawn buffer is a deck re-projection and costs nothing; testing against the fetched box
      // instead sends every pan beyond 15% of the viewport through the debounce and a re-assembly
      // for marks that were already on screen.
      this.held = {bbox: worldBbox(inputs.viewport, RENDER_MARGIN), depth: choice.depth};
      // Fold if the drawn frame still covers; derive in full otherwise. The choice is re-made here
      // rather than trusted from before the await, because a margin arrival can have replaced the
      // frame while this one was in flight — and a frame fetched without stand-ins must never be
      // assembled as though it had them, which would pop every provisional mark off the screen.
      const heldNow = foldable();
      const assembled = heldNow
        ? trace.phase(
            'refresh',
            () =>
              refreshExact(
                heldNow,
                this.replica.exactIn(heldNow.want, choice.depth),
                this.replica.version
              ),
            {depth: choice.depth, from: 'response'}
          )
        : trace.phase(
            'derive',
            () => {
              const full = light
                ? this.replica.frameFromCache(planned.render, choice.depth, meta.selection.kMaxMarks)
                : frame;
              return assemble(full, this.store.state.colourBy ? [this.store.state.colourBy] : []);
            },
            {depth: frame.depth, n: frame.exact.length, standIn: frame.fallback.length, from: 'response'}
          );
      assertAssemblyMatchesServed(assembled);
      if (heldNow) this.scheduleSettle(view, width, height);

      // **Calibration reads `served`, not the drawn count.** They agree today, but a drawn count
      // also carries provisional marks from other depths and would, under elision, be the delta
      // rather than the tile's size — either of which ratchets the budget the wrong way and, being
      // one-directional, never recovers.
      const visible = assembled.visibleInView;
      const actual = assembled.exactServed;

      this.store.update((s) => {
        s.assembled = assembled;
        s.sessionWarm = true;
        // Widened for the coloured column only. Widening every column would walk eighteen arrays
        // per response to build ramps nothing is displaying; the cost is paid when a column is
        // chosen, which is also when the domain first has a reader.
        // Numeric columns only: a category's codes read as numbers, so folding them would scan
        // every mark of every band per arrival to widen a domain no encoding reads.
        if (s.colourBy && !meta.declaredScalars.find((c) => c.name === s.colourBy)?.category) {
          const widened = foldBandColumn(
            assembled,
            s.colourBy,
            s.domains[s.colourBy] ?? null,
            widenDomain
          );
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
        s.status = assembledMarks(assembled) === 0 && visible === 0 ? 'empty' : 'shown';
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

      // The margin, off the critical path: the screen is already drawn, and this only decides
      // whether the *next* small pan needs the wire. Failures are swallowed for the same reason a
      // shed ring is — the user has their view either way.
      if (planned.foreground.rect !== planned.visible.rect) {
        try {
          const lightMargin = foldable() !== null;
          const margin = await this.replica.fetchRegion(
            planned.foreground.rect,
            choice.depth,
            meta.selection.kMaxMarks,
            controller.signal,
            planned.render,
            undefined,
            !lightMargin
          );
          if (generation !== this.generation) return;
          const heldMargin = foldable();
          const widened = heldMargin
            ? refreshExact(
                heldMargin,
                this.replica.exactIn(heldMargin.want, choice.depth),
                this.replica.version
              )
            : assemble(
                lightMargin
                  ? this.replica.frameFromCache(planned.render, choice.depth, meta.selection.kMaxMarks)
                  : margin,
                s0 ? [s0] : []
              );
          if (assembledMarks(widened) > 0) {
            assertAssemblyMatchesServed(widened);
            this.store.update((s) => {
              s.assembled = widened;
              s.replicaBytes = this.replica.bytes;
              s.replicaPoints = this.replica.points;
              s.replicaBands = this.replica.bandCount;
            });
            if (heldMargin) this.scheduleSettle(view, width, height);
          }
        } catch {
          // Already drawn; the margin is an optimisation for the next gesture.
        }
      }
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
/**
 * What the current colouring *is*, as a string — the paint key's colour half.
 *
 * **The encoding changes without anything else changing.** Resolving `/v1/categories` moves the
 * ranks and nothing else: same marks, same depth, same store version. A repaint condition that
 * omitted this skipped the paint that would have applied the palette, so the map stayed uniform
 * until an unrelated change forced a redraw — which is what a zoom is.
 *
 * Sizes rather than contents, for the reason {@link MarkSlab} compares them that way: ranks and
 * domains are sticky accumulators that only ever grow.
 */
export function encodingSignature(store: Store): string {
  const encoding = encodingOf(store);
  switch (encoding.kind) {
    case 'uniform':
    case 'unmapped':
      return encoding.kind;
    case 'category':
      return `category|${encoding.column}|${Object.keys(encoding.rankOfCode).length}`;
    case 'numeric':
      return `numeric|${encoding.column}|${encoding.domain.min}|${encoding.domain.max}`;
  }
}

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
 * **Stand-in marks are drawn at full alpha, like any other mark.**
 *
 * They were faded to 45% so that a superset of what the definition serves could not be mistaken for
 * the answer. Measured against the thing being optimised for, that was the wrong trade: the fade is
 * the *main remaining signature of pop-in*, because the transition a reader notices is not marks
 * appearing — the stand-in already put them there — but the same marks jumping from half to full
 * opacity when the exact band lands. Removing it removes the visible event.
 *
 * What guards the reading is unchanged, and it is the part that was ever normative: **no
 * number-channel value is shown against a non-exact tile** (`delta-serving.md` §7). The count
 * channel, not the alpha channel, is what stops a superset being read as density; the panel reports
 * how many marks are provisional, and nothing displays a figure derived from them. The alpha was a
 * courtesy, and it cost more than it bought.
 */

/**
 * The stand-in colour buffer, reused while the stand-ins and the encoding are the same.
 *
 * The layers are rebuilt on every paint, and a paint happens per store change — arrivals, absorb
 * slices, panel updates. The stand-in *marks* survive most of those by reference (`refreshExact`
 * keeps `standIn` whole for exactly this reason), but their colour buffer was rebuilt from scratch
 * each time: 5.8 x 10^5 marks at the median and 2.7 x 10^6 at worst, measured at 50–70 ms of the
 * very frames that missed p95. Keyed on the stand-in object and the encoding signature — the same
 * two things that decide its contents — with a `WeakMap` so a departed frame frees its buffer.
 */
const heldStandInColours = new WeakMap<object, {key: string; colours: Uint8Array}>();

/** Frames whose fidelity checks have run — once per frame object, see the call site. */
const checkedFrames = new WeakSet<object>();

function standInColours(assembled: Assembled, encoding: Encoding, key: string): Uint8Array {
  const held = heldStandInColours.get(assembled.standIn);
  if (held && held.key === key) return held.colours;
  const colours = buildColourAttribute(assembled.provisional, assembled.standIn.scalars, encoding);
  heldStandInColours.set(assembled.standIn, {key, colours});
  return colours;
}

/**
 * A binary attribute descriptor, reused for as long as its buffer is the same object.
 *
 * **deck.gl's skip check is reference equality on this descriptor, not on the array inside it.**
 * `Attribute.setBinaryValue` returns early on `state.binaryValue === buffer` — where `buffer` is
 * this `{value, size}` object — so building a fresh literal each paint misses the check every time
 * and re-uploads an attribute whose bytes have not changed. That defeated the slab entirely at the
 * last step: measured at the WebGL call level, one pan uploaded 32.9 MB where 16.4 MB was needed,
 * and 121 of 144 paints in a recorded session re-uploaded a 1.4 x 10^6-mark buffer they had not
 * touched.
 *
 * **Keyed on the array, so a republished buffer always uploads.** The slab returns a *new* subarray
 * whenever its contents or extent change and the identical one when they have not, which is exactly
 * the signal wanted here — a `WeakMap` turns that into descriptor identity without keeping a buffer
 * alive or needing an invalidation rule of its own.
 *
 * This is narrower than memoising the whole `data` object, which is the change that rendered
 * rectangles of the view black: `data` identity suppresses `dataChanged` and with it every
 * downstream invalidation, including the picking colours. Here `dataChanged` still fires and only
 * the two binary attributes take deck's own documented skip.
 */
type BinaryAttribute<T extends ArrayBufferView> = {value: T; size: number; normalized?: boolean};
const descriptors = new WeakMap<ArrayBufferView, BinaryAttribute<ArrayBufferView>>();

function binary<T extends ArrayBufferView>(
  value: T,
  size: number,
  normalized?: boolean
): BinaryAttribute<T> {
  let held = descriptors.get(value);
  if (!held) {
    held = normalized === undefined ? {value, size} : {value, size, normalized};
    descriptors.set(value, held);
  }
  return held as BinaryAttribute<T>;
}

/**
 * The mark layers: served marks from the slab, stand-in marks beside them.
 *
 * **Every served mark is drawn.** The length handed to deck.gl is the resident count,
 * unconditionally — no budget, no cap, no filter applies here.
 *
 * **Two layers, not one, and not one per tile.** They exist because the two sets have different
 * lifetimes, not because they are drawn differently: exact bands accumulate and are retained across
 * frames, while stand-ins are re-clipped whenever a response lands. Splitting them is what lets the
 * first be written once. It also makes the fade uniform over a whole buffer rather than a per-tile
 * walk over ranges the slab no longer has.
 */
export function buildViewportLayers(store: Store, slab: MarkSlab): Layer[] {
  const {assembled, selectedWorldXY, status} = store.state;
  const layers: Layer[] = [];

  // A refusal draws no marks but keeps the slab: the held bands are still the answer to the last
  // view that succeeded, and discarding them would make recovery pay for a full rewrite.
  if (status === 'refused') return selectionLayers(selectedWorldXY);
  if (!assembled) {
    slab.clear();
    return selectionLayers(selectedWorldXY);
  }

  const encoding = encodingOf(store);
  const before = slab.drawn;
  const marks = trace.phase(
    'slab',
    () => slab.sync(assembled.bands, assembled.depth, encoding, store.state.colourBy),
    {n: assembled.bands.length}
  );
  // Whether deck.gl is handed the same buffers it already holds is the whole question for upload
  // cost, and it is invisible from outside — so it is recorded rather than inferred.
  trace.event('marks', {n: marks.length, added: marks.length - before, standIn: assembled.provisional});
  // **The fidelity checks run once per frame, not once per paint.** Both walk every band —
  // O(10^4-10^5) — and a paint happens on every store change, most of which change no band: the
  // pair measured as a real share of `layers` time doing the same arithmetic on the same objects.
  // A frame is immutable once assembled, and the slab was synced against it in the line above, so
  // one pass per frame object is the same guarantee at a fraction of the cost.
  if (!checkedFrames.has(assembled)) {
    checkedFrames.add(assembled);
    assertAssemblyMatchesServed(assembled);
    // Every exact band the frame draws must have reached the slab: a band written outside its slot,
    // or a slot gone stale under a partition change, would otherwise thin the picture in a way
    // nothing else notices.
    for (const band of assembled.bands) {
      if (!slab.holds(band)) {
        throw new Error(
          `assembly: exact band ${band.prefix} at depth ${band.depth} is drawn but has no slab slot.`
        );
      }
    }
  }

  // **Layers toggle `visible`; they are never omitted.** deck.gl retains a layer's buffers across
  // renders by matching `id` — a layer absent from one render is destroyed, and re-adding it
  // regenerates and re-uploads everything it held. The stand-in layer flips between empty and not
  // on every coverage change, which made each flip a full re-upload of up to 2.7 x 10^6 marks.
  // (deck.gl performance guide: "favor layer visibility over addition/removal".)
  //
  // **One layer per retained slab partition, addressed by slot.** A depth flip swaps which
  // partition is visible; the other keeps its layer, its buffers and its GPU residency, so
  // flipping back uploads nothing. The slot number is the layer id precisely because it is stable
  // for a partition's whole life — an id derived from the depth would make eviction reshuffle
  // identities and re-upload both.
  for (const held of slab.layers()) {
    layers.push(
      new ScatterplotLayer({
        id: `marks-p${held.slot}`,
        visible: held.active && held.draw.length > 0,
        data: {
          length: held.draw.length,
          attributes: {
            getPosition: binary(held.draw.positions, 2),
            getFillColor: binary(held.draw.colours, 4, true)
          }
        },
        tesseraIds: held.draw.ids,
        radiusUnits: 'pixels' as const,
        getRadius: RENDER.radius,
        radiusMinPixels: 1,
        pickable: RENDER.pickable && held.active,
        parameters: {depthCompare: 'always' as const}
      })
    );
  }

  {
    // One entry per drawn mark by construction — see `buildColourAttribute`. Asserted anyway,
    // because a short buffer is the one way colour could silently drop marks: deck.gl reads
    // `length` from `data`, so a short attribute renders garbage rather than failing.
    const colours = standInColours(assembled, encoding, encodingSignature(store));
    if (colours.length !== assembled.provisional * 4) {
      throw new Error(
        `colour buffer covers ${colours.length / 4} of ${assembled.provisional} stand-in marks. ` +
          `Colour is presentation and must never decide what is drawn.`
      );
    }
    layers.push(
      new ScatterplotLayer({
        id: 'marks-standin',
        visible: assembled.provisional > 0,
        data: {
          length: assembled.provisional,
          attributes: {
            getPosition: binary(assembled.standIn.positions, 2),
            getFillColor: binary(colours, 4, true)
          }
        },
        tesseraIds: assembled.standIn.ids,
        radiusUnits: 'pixels' as const,
        getRadius: RENDER.radius,
        radiusMinPixels: 1,
        pickable: RENDER.pickable,
        parameters: {depthCompare: 'always' as const}
      })
    );
  }

  layers.push(...selectionLayers(selectedWorldXY));
  return layers;
}

/**
 * **Reusing the `data` object across paints was tried and reverted.** The slab hands back identical
 * typed arrays when nothing moved, so wrapping them in a stable object looked like the last step
 * needed to stop deck.gl re-uploading — but a fresh `ScatterplotLayer` instance carrying a `data`
 * object deck considers unchanged rendered large rectangles of the view black until something else
 * forced them to refill. Whatever deck does with layer state in that case, it is not what the
 * reasoning assumed, and the reasoning is not worth another try without knowing which.
 */
function selectionLayers(selectedWorldXY: [number, number] | null): Layer[] {
  const layers: Layer[] = [];

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
