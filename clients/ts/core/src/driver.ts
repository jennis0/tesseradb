import {calibrate, tileRectOfBbox, type CountCell, type CountField, type DepthChoice} from './budget.js';
import {tileXY} from './coords.js';
import {plan, worldBbox, type Plan, type PlannerInputs, type Viewport} from './prefetch.js';
import {rectContains, rectContainsTile, rectIntersection, type TileRect} from './rects.js';
import type {Replica, ReplicaFrame} from './replica.js';
import type {Band} from './bands.js';
import {TesseraError} from './client.js';

/**
 * The driver: the scheduler that decides when the client asks, retries, revalidates and fetches
 * ahead.
 *
 * Four regions run at once on one clock: motion (still or in a gesture), the foreground request
 * (idle, primary or margin, with one queued view and retries under the same generation count), the
 * settle, and anticipation (idle, eligible or a fetch in flight). A pan can be in motion with a
 * request in flight; an anticipatory fetch in flight survives movement because the server has
 * already done the work.
 *
 * The clock is injected and there is no DOM, so every behaviour is testable with a fake clock. The
 * driver holds `mTarget`, the last visible count and the presented-frame handle, and reports
 * through callbacks. What to draw arrives as a {@link ReplicaFrame} for the consumer to render.
 */

/** The injected clock, in `setTimeout`'s shape. @internal */
export type Clock = {
  now(): number;
  after(ms: number, fire: () => void): unknown;
  cancel(handle: unknown): void;
};

/** @internal */
export type ViewState = {target: [number, number, number]; zoom: number};

/**
 * What the driver needs to know from `/v1/meta`, injected once at session start, and whether the
 * requests carry a filter, which decides how the server thins a tile (see `budget.ts`). @internal
 */
export type DriverMeta = {
  kMaxMarks: number;
  maxTilesPerRequest: number;
  thetaTargetMarks: number;
  /** `selection.kMin`. Unset, 0. */
  kMin?: number;
  /** Whether requests carry a filter. Unset, they do not. */
  filtered?: () => boolean;
};

/** @internal */
export type DriverEvents = {
  /**
   * What this paint may cost.
   *
   * - `fold`: the presented frame still covers the view and only exact bands changed. The
   *   consumer refreshes those from the depth index and leaves stand-ins one step stale.
   * - `derive`: the depth or region changed, or the settle is finishing. `frame` is the full
   *   derivation, made at most once per `deriveMinGapMs` during a gesture.
   *
   * When nothing changed the driver emits nothing.
   */
  onFrame(verdict: {tier: 'fold'; plan: Plan} | {tier: 'derive'; plan: Plan; frame: ReplicaFrame}): void;
  onStatus?(status: 'loading' | 'shown' | 'empty' | 'refused' | 'retrying', detail?: unknown): void;
  /** Instrumentation: request, arrived, covered, ring, ringskip, revalidate and others. */
  onTrace?(kind: string, fields: Record<string, number | string>): void;
};

/** @internal */
export type DriverOptions = {
  /** Trailing debounce while the store can answer the view. */
  debounceMs?: number;
  /** Debounce when it cannot, which is short because the request is needed. */
  uncachedDebounceMs?: number;
  /** Stillness before anticipation starts, and the floor between leading-edge requests. */
  idleMs?: number;
  leadingEdgeMinGapMs?: number;
  settleMs?: number;
  settleMaxMs?: number;
  deriveMinGapMs?: number;
  inFlightMaxMs?: number;
  maxRetries?: number;
  /** First backoff after a 503 `not-ready`, doubled per attempt; a shorter wait than a 429's. */
  notReadyBackoffMs?: number;
  /** Ceiling on any retry backoff, so a late attempt does not wait minutes. */
  retryBackoffMaxMs?: number;
  /** Anticipatory fetches allowed per pause, by count and by bytes. */
  maxPrefetchPerPause?: number;
  maxPrefetchBytesPerPause?: number;
  /** Zoom layers fetched ahead and held undrawn: 0 for none, 2 or more for strong machines. */
  prefetchLayers?: number;
  /** Marks-on-screen budget, forwarded to the planner. */
  budget?: number;
};

const DEFAULTS = {
  debounceMs: 140,
  uncachedDebounceMs: 30,
  idleMs: 250,
  leadingEdgeMinGapMs: 400,
  settleMs: 150,
  settleMaxMs: 700,
  deriveMinGapMs: 120,
  inFlightMaxMs: 5_000,
  maxRetries: 2,
  notReadyBackoffMs: 250,
  retryBackoffMaxMs: 8_000,
  maxPrefetchPerPause: 3,
  maxPrefetchBytesPerPause: 8_000_000,
  prefetchLayers: 1,
  budget: 50_000
};

/** The options {@link retryDelayMs} reads. */
export type RetryOptions = Required<Pick<DriverOptions, 'maxRetries' | 'notReadyBackoffMs' | 'retryBackoffMaxMs'>>;

/** The retry settings every request the store sends again uses, unless `StoreOptions.driver` sets them. */
export const RETRY_DEFAULTS: RetryOptions = {maxRetries: DEFAULTS.maxRetries, notReadyBackoffMs: DEFAULTS.notReadyBackoffMs, retryBackoffMaxMs: DEFAULTS.retryBackoffMaxMs};

/**
 * How long to wait before sending a refused request again, or `null` where it is not sent again. A
 * `429` backpressure is the server shedding load and a `503` not-ready a server still starting;
 * each is retried up to `maxRetries` times, after a backoff doubled per attempt from 1 s or from
 * `notReadyBackoffMs`, or after the server's `Retry-After` where that is longer, and never after
 * more than `retryBackoffMaxMs`.
 *
 * @param attempt - Retries already made, from 0.
 */
export function retryDelayMs(error: unknown, attempt: number, o: RetryOptions): number | null {
  const status = error instanceof TesseraError ? error.status : 0;
  if ((status !== 429 && status !== 503) || attempt >= o.maxRetries) return null;
  const base = status === 503 ? o.notReadyBackoffMs : 1000;
  const asked = (error as TesseraError).retryAfterS ?? 0;
  return Math.min(Math.max(base * 2 ** attempt, asked * 1000), o.retryBackoffMaxMs);
}

/**
 * The count field's cells from tiles' counts. A tile nothing matches in is left out, as a response
 * leaves it out, so it does not read as occupied ground.
 */
function countCells(tiles: Iterable<{x: number; y: number; matched: bigint}>): CountCell[] {
  const cells: CountCell[] = [];
  for (const t of tiles) if (t.matched > 0n) cells.push({x: t.x, y: t.y, count: Number(t.matched)});
  return cells;
}

/** @internal */
export class Driver {
  private readonly o: Required<DriverOptions>;

  private mTarget: number;
  private lastVisibleInView: number | undefined;
  /**
   * Per depth, the per-cell masked counts the latest response at that depth left, and the rectangle
   * they are complete for. The depth choice is arithmetic over them wherever they cover the view,
   * and the average `mTarget` model answers elsewhere (`budget.ts`). Kept per depth so a zoom back
   * out still finds the coarser field.
   *
   * Read from the response's bands rather than from the store per plan, which would scale with
   * what is held. Adopted after the response's own reconcile: adopted earlier, the derive that
   * draws the arrival could choose a depth where nothing is held.
   */
  private counts = new Map<number, CountField>();
  /**
   * Per depth, the `θ` an unfiltered response showed: served over matched, summed over the tiles in
   * view that served more than the floor and fewer than the cap and their matches.
   */
  private theta = new Map<number, number>();
  /** The presented-frame handle: all the driver knows of what is on screen. */
  private presented: {want: TileRect; depth: number; version: number; standInStale: boolean} | null =
    null;
  private heldBbox: {bbox: [number, number, number, number]; depth: number} | null = null;

  // Foreground lifecycle.
  private inFlight: AbortController | null = null;
  /** The last status reported, so a camera answered from held tiles reports only a change. */
  private lastStatus: 'loading' | 'shown' | 'empty' | 'refused' | 'retrying' | null = null;
  private inFlightAt: {rect: TileRect; depth: number; since: number} | null = null;
  private queued: ViewState | null = null;
  private generation = 0;
  /** A pending retry, held so `cancel()` clears it. */
  private retryHandle: unknown = null;

  // Motion.
  /** Negative infinity so the first schedule reads as arriving from stillness. */
  private lastScheduleAt = Number.NEGATIVE_INFINITY;
  private lastRequestAt = Number.NEGATIVE_INFINITY;
  private movedAt = 0;
  private lastTarget: [number, number] | null = null;
  private velocity: [number, number] | undefined;
  private debounceHandle: unknown = null;
  private lastView: ViewState | null = null;
  private width = 0;
  private height = 0;

  // Settle cadence.
  private settleHandle: unknown = null;
  private settleDeadline = 0;
  private lastFullDeriveAt = 0;

  /**
   * Releases the depth hold after a settle. A settle sets `bankReady`; the next `schedule`
   * suspends the hold so a recalibrated depth can win, and the derive that adopts a depth re-arms
   * the hold. Without this every plan would keep the presented depth. At rest the hold is not
   * released, so marks do not change with no user action.
   */
  private bankReady = false;
  private holdSuspended = false;

  /** The count-only refresh's own slot, separate from the foreground's. */
  private revalidating: AbortController | null = null;

  // Anticipation.
  private idleHandle: unknown = null;
  private background: AbortController | null = null;
  private bitesSincePause = 0;
  private bytesSincePause = 0;
  /** Set while a foreground request defers anticipation; it runs when that request clears. */
  private anticipationEligible = false;

  /**
   * The `depth:rect` a settle has already asked for because the derived frame was not held there,
   * so a shed or refused request does not become a retry at the settle rate. Cleared by the next
   * camera move.
   */
  private askedUncovered: string | null = null;

  /** Whether the cold view's count-only seed has been made or has failed; see {@link seedCounts}. */
  private seeded = false;

  constructor(
    private readonly replica: Replica,
    private readonly meta: DriverMeta,
    private readonly clock: Clock,
    private readonly events: DriverEvents,
    options: DriverOptions = {},
    private readonly prefetch = true
  ) {
    this.o = {...DEFAULTS, ...options};
    this.mTarget = meta.thetaTargetMarks;
  }

  get calibration(): {mTarget: number; visibleInView: number | undefined} {
    return {mTarget: this.mTarget, visibleInView: this.lastVisibleInView};
  }

  /**
   * Adopts a new marks-on-screen budget. The depth hold is suspended so a change of depth is taken
   * on the next plan: the hold absorbs calibration wobble, and a budget the user just set is not
   * wobble.
   */
  setBudget(budget: number): void {
    if (!Number.isFinite(budget) || budget <= 0 || budget === this.o.budget) return;
    this.o.budget = budget;
    this.holdSuspended = true;
  }

  /** The presented-frame handle, for a consumer deciding whether its own drawn state matches. */
  get presentedFrame() {
    return this.presented;
  }

  private trace(kind: string, fields: Record<string, number | string>): void {
    this.events.onTrace?.(kind, fields);
  }

  private planFor(view: ViewState, velocity?: [number, number]): Plan {
    const viewport = this.viewportOf(view);
    // The planner computes this box again; the count field is looked up here because the planner
    // does not consult the replica.
    const inputs: PlannerInputs = {
      viewport,
      budget: this.o.budget,
      mTarget: this.mTarget,
      counts: this.countsFor(worldBbox(viewport, 1)),
      k: this.meta.kMaxMarks,
      thinning: this.meta.filtered?.() ? undefined : {target: this.meta.thetaTargetMarks, kMin: this.meta.kMin ?? 0, seen: this.theta},
      maxTiles: this.meta.maxTilesPerRequest,
      visibleInView: this.lastVisibleInView,
      velocity,
      heldBytes: this.replica.bytes,
      budgetBytes: this.replica.budgetBytes,
      depthLayers: this.o.prefetchLayers,
      holdDepth: this.holdSuspended ? undefined : this.presented?.depth
    };
    return plan(inputs);
  }

  private viewportOf(view: ViewState): Viewport {
    return {target: [view.target[0], view.target[1]], zoom: view.zoom, width: this.width, height: this.height};
  }

  /**
   * The finest count field that covers the view's tiles at its depth. Past its edge the plan falls
   * back to the average model until a response re-anchors a field.
   */
  private countsFor(bbox: [number, number, number, number]): CountField | undefined {
    let best: CountField | undefined;
    for (const field of this.counts.values()) {
      if (!rectContains(field.covers, tileRectOfBbox(bbox, field.depth))) continue;
      if (!best || field.depth > best.depth) best = field;
    }
    return best;
  }

  /**
   * Adopts the counts a response left, over the widest rectangle they are complete for.
   *
   * The cells are read after the absorb, so every non-empty tile of the fetched region has a band
   * and a tile absent from them is empty. The wider rectangle the frame spans is claimed only where
   * the replica holds every tile of it; claimed otherwise, unfetched tiles would read as empty and
   * the choice would go too deep. An anticipatory fetch's responses are not adopted: one covers
   * part of its region, so its rectangle is incomplete when it lands.
   */
  private adopt(depth: number, cells: CountCell[], fetched: TileRect, spans: TileRect): void {
    const whole = this.replica.novelIn(spans, depth, this.meta.kMaxMarks) === 0;
    this.counts.set(depth, {depth, cells, covers: whole ? spans : fetched});
  }

  /** Every view-state change enters here. */
  schedule(view: ViewState, width: number, height: number): void {
    const now = this.clock.now();
    if (this.bankReady) {
      this.holdSuspended = true;
      this.bankReady = false;
    }
    this.lastView = view;
    this.width = width;
    this.height = height;
    this.movedAt = now;
    const wasStill = now - this.lastScheduleAt > this.o.debounceMs;
    const elapsed = now - this.lastScheduleAt;
    const target: [number, number] = [view.target[0], view.target[1]];
    this.velocity =
      this.lastTarget && elapsed > 0 && elapsed < 200
        ? [(target[0] - this.lastTarget[0]) / elapsed, (target[1] - this.lastTarget[1]) / elapsed]
        : undefined;
    this.lastTarget = target;
    this.lastScheduleAt = now;

    // Movement resets the pause: pending eligibility is cancelled, an anticipatory fetch in flight
    // survives, and the per-pause allowances reset.
    this.bitesSincePause = 0;
    this.bytesSincePause = 0;
    this.anticipationEligible = false;
    this.askedUncovered = null;
    if (this.idleHandle) this.clock.cancel(this.idleHandle);
    if (this.prefetch) {
      this.idleHandle = this.clock.after(this.o.idleMs, () => {
        this.anticipationEligible = true;
        void this.anticipate();
      });
    }

    const planned = this.planFor(view);
    if (this.covers(planned)) {
      // The plan agreed with the presented depth, so the suspension has done its job.
      this.holdSuspended = false;
      this.trace('covered', {depth: this.heldBbox?.depth ?? -1});
      this.movedAt = 0;
      this.reportHeld(planned);
      // A warm client lives on this path, so the staleness bound must be reachable here.
      this.revalidateIfDue(view);
      return;
    }

    this.reconcile('schedule', view);

    if (this.debounceHandle) this.clock.cancel(this.debounceHandle);
    const notRecent = now - this.lastRequestAt > this.o.leadingEdgeMinGapMs;
    if (wasStill && notRecent && !this.inFlight) {
      void this.request(view);
      return;
    }
    this.debounceHandle = this.clock.after(
      this.storeCanAnswer(view) ? this.o.debounceMs : this.o.uncachedDebounceMs,
      () => void this.request(view)
    );
  }

  /**
   * Draws this view from what is held, asking for nothing. A view switched back to already holds
   * the bands its last visit fetched, so it is drawn at once rather than after the settle's
   * request. It derives at full fidelity, as a settle does: the camera did not move.
   */
  redraw(view: ViewState, width: number, height: number): void {
    this.lastView = view;
    this.width = width;
    this.height = height;
    this.reconcile('settle', view, false);
  }

  /** An absorb landed mid-fetch, so pieces paint as they arrive. The consumer coalesces frames. */
  absorbed(): void {
    if (this.lastView) this.reconcile('absorb', this.lastView);
  }

  /**
   * Decides what a paint costs; every trigger passes through here. Reuse is exact, by version
   * counter. A fold marks the handle's stand-ins stale for the settle to repair. The full
   * derivation, which clears `standInStale`, is paid at most once per
   * {@link DriverOptions.deriveMinGapMs} while the view moves, and always at the settle.
   */
  private reconcile(trigger: 'schedule' | 'absorb' | 'response' | 'settle', view: ViewState, ask = true): void {
    const planned = this.planFor(view);
    const handle = this.presented;
    const covered =
      handle !== null &&
      handle.depth === planned.choice.depth &&
      rectContains(handle.want, planned.render);

    if (
      covered &&
      handle.version === this.replica.version &&
      (trigger !== 'settle' || !handle.standInStale)
    ) {
      // A settle readies the bank even when it finds nothing to do.
      if (trigger === 'settle') this.bankReady = true;
      this.holdSuspended = false;
      this.trace('reuse', {depth: handle.depth});
      // Reuse says the frame is the one this plan wants, not that the replica holds anything at its
      // depth.
      if (trigger === 'settle' && ask) this.askUncovered(view, planned);
      return;
    }

    if (covered && trigger !== 'settle') {
      // The depths agree, so the suspension has done its job.
      this.holdSuspended = false;
      this.presented = {...handle, version: this.replica.version, standInStale: true};
      this.events.onFrame({tier: 'fold', plan: planned});
      this.scheduleSettle();
      return;
    }

    const now = this.clock.now();
    if (trigger !== 'settle' && now - this.lastFullDeriveAt < this.o.deriveMinGapMs) {
      this.scheduleSettle();
      return;
    }
    this.lastFullDeriveAt = now;
    const frame = this.replica.frameFromCache(planned.render, planned.choice.depth, this.meta.kMaxMarks);
    this.presented = {
      want: planned.render,
      depth: planned.choice.depth,
      version: frame.version,
      standInStale: false
    };
    // The hold re-arms around the adopted depth, and a settle readies the bank for the next motion.
    this.holdSuspended = false;
    if (trigger === 'settle') this.bankReady = true;
    this.events.onFrame({tier: 'derive', plan: planned, frame});
    if (trigger === 'settle' && ask) this.askUncovered(view, planned);
  }

  /**
   * Asks for the depth the settle just derived at, where the replica holds nothing there.
   *
   * The first request of a session is planned by the average model; every plan after it uses the
   * response's per-tile counts. Where the average overshot, the count-driven depth is shallower,
   * the settle derives there, and nothing has been fetched there: the frame is all stand-ins and
   * has no counts. Requests are otherwise issued only on a camera move, and a filter or highlight
   * change reaches the same state. So the settle asks, once per uncovered frame. The response
   * covers the rectangle, so the next settle asks for nothing.
   */
  private askUncovered(view: ViewState, planned: Plan): void {
    if (this.inFlight || this.queued || this.revalidating) return;
    const depth = planned.choice.depth;
    if (this.replica.novelIn(planned.visible.rect, depth, this.meta.kMaxMarks) === 0) return;
    const key = `${depth}:${planned.visible.rect.x0},${planned.visible.rect.y0},${planned.visible.rect.x1},${planned.visible.rect.y1}`;
    if (this.askedUncovered === key) return;
    this.askedUncovered = key;
    this.trace('uncovered', {depth, n: planned.choice.tiles});
    void this.request(view);
  }

  /** Whether `bands` serve anything inside `rect`: what a view's status reports. */
  private heldIn(bands: Iterable<Band>, rect: TileRect): 'shown' | 'empty' {
    for (const band of bands) if (band.served > 0 && rectContainsTile(rect, band.x, band.y)) return 'shown';
    return 'empty';
  }

  private report(status: 'loading' | 'shown' | 'empty' | 'refused' | 'retrying', detail?: unknown): void {
    this.lastStatus = status;
    this.events.onStatus?.(status, detail);
  }

  /**
   * Reports what the held tiles say of a camera they cover, where no request is out to answer it and
   * the status differs. A retry pending for an earlier camera is dropped: this one is answered.
   */
  private reportHeld(planned: Plan): void {
    if (this.inFlight || !this.covers(planned)) return;
    const status = this.heldIn(this.replica.exactIn(planned.visible.rect, planned.choice.depth), planned.visible.rect);
    if (this.retryHandle) this.clock.cancel(this.retryHandle);
    this.retryHandle = null;
    if (status !== this.lastStatus) this.report(status);
  }

  private storeCanAnswer(view: ViewState): boolean {
    return this.covers(this.planFor(view));
  }

  private covers(planned: Plan): boolean {
    if (!this.heldBbox || !this.presented) return false;
    if (planned.choice.depth !== this.heldBbox.depth) return false;
    // Novelty over the visible box, containment over the render rect: the margin is fetched
    // opportunistically, and its absence does not fail a pan that stays inside the drawn area.
    return (
      rectContains(this.presented.want, planned.visible.rect) &&
      this.replica.novelIn(planned.visible.rect, planned.choice.depth, this.meta.kMaxMarks) === 0
    );
  }

  /**
   * Starts the count-only refresh in its own slot when everything is idle; any real fetch aborts
   * it. In the foreground slot it would queue a user's pan behind it.
   */
  private revalidateIfDue(view: ViewState): void {
    if (this.inFlight || this.queued || this.revalidating) return;
    if (!this.replica.dueForRevalidation()) return;
    const planned = this.planFor(view);
    const controller = new AbortController();
    this.revalidating = controller;
    void this.replica
      .fetchRegion(planned.visible.rect, planned.choice.depth, this.meta.kMaxMarks, controller.signal, planned.render, undefined, false)
      .then(() => this.trace('revalidate', {depth: planned.choice.depth}))
      .catch(() => {})
      .finally(() => {
        if (this.revalidating === controller) this.revalidating = null;
      });
  }

  private inFlightUseful(render: TileRect, depth: number): boolean {
    return (
      this.inFlightAt !== null &&
      this.clock.now() - this.inFlightAt.since < this.o.inFlightMaxMs &&
      this.inFlightAt.depth === depth &&
      rectIntersection(this.inFlightAt.rect, render) !== null
    );
  }

  private dispatchQueued(): void {
    this.inFlightAt = null;
    const next = this.queued;
    if (next) {
      this.queued = null;
      void this.request(next);
      return;
    }
    // A cleared slot re-arms anticipation, but only with no foreground in flight and no view
    // queued, so an anticipatory fetch does not go ahead of the user.
    if (this.anticipationEligible) void this.anticipate();
  }

  private scheduleSettle(): void {
    const now = this.clock.now();
    if (this.settleHandle) this.clock.cancel(this.settleHandle);
    else this.settleDeadline = now + this.o.settleMaxMs;
    const wait = Math.min(this.o.settleMs, Math.max(0, this.settleDeadline - now));
    this.settleHandle = this.clock.after(wait, () => {
      this.settleHandle = null;
      if (this.lastView) this.reconcile('settle', this.lastView);
    });
  }

  cancel(): void {
    if (this.debounceHandle) this.clock.cancel(this.debounceHandle);
    if (this.settleHandle) this.clock.cancel(this.settleHandle);
    this.settleHandle = null;
    if (this.idleHandle) this.clock.cancel(this.idleHandle);
    this.idleHandle = null;
    if (this.retryHandle) this.clock.cancel(this.retryHandle);
    this.retryHandle = null;
    this.background?.abort();
    this.background = null;
    this.revalidating?.abort();
    this.revalidating = null;
    this.inFlight?.abort();
    this.inFlight = null;
    this.inFlightAt = null;
    this.queued = null;
    this.heldBbox = null;
    this.presented = null;
    this.lastStatus = null;
    this.movedAt = 0;
    this.velocity = undefined;
    this.lastTarget = null;
    this.anticipationEligible = false;
    this.askedUncovered = null;
    this.seeded = false;
    this.counts.clear();
    this.theta.clear();
  }

  private async anticipate(): Promise<void> {
    if (!this.prefetch || !this.lastView) return;
    if (this.inFlight) return this.trace('ringskip', {why: 1});
    if (this.queued) return this.trace('ringskip', {why: 1});
    if (this.background) return this.trace('ringskip', {why: 2});
    if (this.bitesSincePause >= this.o.maxPrefetchPerPause) return this.trace('ringskip', {why: 3});
    if (this.bytesSincePause >= this.o.maxPrefetchBytesPerPause) return this.trace('ringskip', {why: 4});

    const planned = this.planFor(this.lastView, this.velocity);
    let ring: (typeof planned.background)[number] | null = null;
    for (const band of planned.background) {
      if (this.replica.novelIn(band.rect, band.depth, this.meta.kMaxMarks) > 0) {
        ring = band;
        break;
      }
    }
    if (!ring) return this.trace('ringskip', {why: 5});

    const controller = new AbortController();
    this.background = controller;
    const started = this.clock.now();
    try {
      this.bitesSincePause += 1;
      const frame = await this.replica.fetchRegion(
        ring.rect,
        ring.depth,
        this.meta.kMaxMarks,
        controller.signal,
        undefined,
        1,
        false,
        true
      );
      this.bytesSincePause += frame.plan.bytes;
      this.trace('ring', {depth: ring.depth, ms: this.clock.now() - started, n: frame.plan.novel, bytes: frame.plan.bytes});
    } catch {
      // A shed anticipatory fetch answers nothing; only the foreground retries.
    } finally {
      if (this.background === controller) this.background = null;
      // Within one pause, allowances permitting, the next fetch follows without the idle delay.
      if (this.anticipationEligible && !this.inFlight && !this.queued) void this.anticipate();
    }
  }

  /**
   * Fetches counts before marks for the one view no counts describe.
   *
   * The first request of a session is planned by the average model, which can overshoot by an
   * order of magnitude: one cold view asked for a million points and drew fewer than a tenth of
   * them. A count-only request (`k = 0`, the tile list alone) is small and answers what the average
   * model guesses at. The cold view adopts those counts and plans again, and the marks request
   * that follows is count-driven.
   *
   * Once per session, where no counts exist at all. A pan onto uncovered ground also falls back to
   * the average model, and seeding it would put a round trip in front of every such pan.
   */
  private async seedCounts(view: ViewState, planned: Plan, signal: AbortSignal): Promise<boolean> {
    const depth = planned.choice.depth;
    const started = this.clock.now();
    let tiles;
    try {
      tiles = await this.replica.counts(planned.visible.rect, depth, signal);
    } catch (error) {
      // A refused seed does not refuse the view: the marks request the average model planned is
      // still made. An abort is rethrown, and the caller's generation check drops the request.
      if (signal.aborted) throw error;
      this.trace('seedfail', {depth, ms: this.clock.now() - started});
      this.seeded = true;
      return false;
    }
    const cells = countCells(tiles.map((t) => ({...tileXY(t.tile, depth), matched: t.matched})));
    let visible = 0;
    for (const t of tiles) visible += Number(t.visible);
    // Complete for the rectangle asked over: a response omits only cells whose masked count is zero.
    this.counts.set(depth, {depth, cells, covers: planned.visible.rect});
    this.lastVisibleInView = visible;
    this.seeded = true;
    this.trace('seed', {depth, ms: this.clock.now() - started, n: cells.length, visible});
    return true;
  }

  private async request(view: ViewState, attempt = 0): Promise<void> {
    let planned = this.planFor(view);
    let choice: DepthChoice = planned.choice;

    // A real fetch displaces a running revalidation; the user does not wait behind a refresh.
    this.revalidating?.abort();
    this.revalidating = null;
    if (this.inFlight && this.inFlightUseful(planned.render, choice.depth)) {
      this.queued = view;
      return;
    }
    this.inFlight?.abort();
    const controller = new AbortController();
    this.inFlight = controller;
    this.inFlightAt = {rect: planned.render, depth: choice.depth, since: this.clock.now()};
    const generation = ++this.generation;
    const movedAt = this.movedAt || this.clock.now();
    this.report('loading');

    try {
      // The cold view fetches counts before marks (see {@link seedCounts}), inside the foreground
      // slot it already holds.
      if (!this.seeded && this.counts.size === 0 && choice.source === 'average') {
        const seeded = await this.seedCounts(view, planned, controller.signal);
        if (generation !== this.generation) return;
        if (seeded) {
          // The depth drawn while the seed was out came from the average model, so it is no
          // reason to hold.
          this.holdSuspended = true;
          planned = this.planFor(view);
          choice = planned.choice;
          this.inFlightAt = {rect: planned.render, depth: choice.depth, since: this.clock.now()};
        }
      }
      const startedAt = this.clock.now();
      this.lastRequestAt = startedAt;
      this.trace('request', {
        depth: choice.depth,
        n: choice.tiles,
        waited: startedAt - movedAt,
        predicted: Math.round(choice.predictedMarks),
        from: choice.source
      });
      // The fetch absorbs and reports; what is derived is `reconcile`'s decision.
      const frame = await this.replica.fetchRegion(
        planned.visible.rect,
        choice.depth,
        this.meta.kMaxMarks,
        controller.signal,
        planned.render,
        undefined,
        false
      );
      if (generation !== this.generation) return;
      const arrivedAt = this.clock.now();

      // Visible and served figures are summed over the visible box the prediction was for, not the
      // wider render rect; summing over the render rect inflates `actual` and stalls calibration.
      // Cells are read over the whole render rect, since the next plan may ask about any of it.
      const cells = countCells(frame.exact);
      let visible = 0;
      let actual = 0;
      let thinnedServed = 0;
      let thinnedMatched = 0;
      const floor = Math.min(this.meta.kMin ?? 0, this.meta.kMaxMarks);
      for (const b of frame.exact) {
        const matched = Number(b.matched);
        if (!rectContainsTile(planned.visible.rect, b.x, b.y)) continue;
        visible += Number(b.visible);
        actual += b.served;
        if (b.served > floor && b.served < Math.min(this.meta.kMaxMarks, matched)) {
          thinnedServed += b.served;
          thinnedMatched += matched;
        }
      }
      if (thinnedMatched > 0 && !this.meta.filtered?.()) this.theta.set(choice.depth, thinnedServed / thinnedMatched);

      this.trace('arrived', {
        ms: arrivedAt - startedAt,
        n: frame.plan.bytes,
        server: Math.round((frame.response?.timings.serverUs ?? 0) / 1000),
        depth: choice.depth,
        novel: frame.plan.novel,
        wanted: frame.plan.wanted,
        predicted: Math.round(choice.predictedMarks),
        actual,
        from: choice.source
      });

      this.heldBbox = {bbox: [0, 0, 0, 0], depth: choice.depth};
      this.reconcile('response', view);

      this.adopt(choice.depth, cells, planned.visible.rect, planned.render);
      this.lastVisibleInView = visible;
      // Corrected against the average model's own prediction, not the count-driven one: the loop
      // models the average, which stays the fallback for views no counts describe.
      this.mTarget = calibrate(
        {predictedMarks: choice.averageMarks, actualMarks: actual, visibleInView: visible},
        this.mTarget,
        this.meta.thetaTargetMarks
      );
      this.report(this.heldIn(frame.exact, planned.visible.rect));
      this.movedAt = 0;
      if (this.queued) {
        this.inFlight = null;
        this.dispatchQueued();
        return;
      }

      // The margin fetch keeps the foreground slot, so a new request aborts it as it would the
      // primary, and its failure does not touch the new request's state.
      if (planned.foreground.rect !== planned.visible.rect) {
        try {
          const margin = await this.replica.fetchRegion(
            planned.foreground.rect,
            choice.depth,
            this.meta.kMaxMarks,
            controller.signal,
            planned.render,
            undefined,
            false
          );
          if (generation !== this.generation) return;
          this.reconcile('response', view);
          // The margin is ground now held at the same depth, so it widens the count field.
          this.adopt(
            choice.depth,
            countCells(margin.exact),
            planned.foreground.rect,
            planned.render
          );
        } catch {
          // The view is already drawn; the margin is for the next gesture.
        }
      }
      if (generation !== this.generation) return;
      this.inFlight = null;
      // A camera that moved within the held tiles while the margin was out was not reported.
      if (this.lastView) this.reportHeld(this.planFor(this.lastView));
      this.dispatchQueued();
    } catch (error) {
      if (controller.signal.aborted || generation !== this.generation) return;
      this.inFlightAt = null;
      this.inFlight = null;

      const backoff = retryDelayMs(error, attempt, this.o);
      if (backoff !== null) {
        this.report('retrying');
        this.retryHandle = this.clock.after(backoff, () => {
          this.retryHandle = null;
          // Anything newer supersedes the retry.
          if (generation === this.generation) void this.request(view, attempt + 1);
        });
        return;
      }
      this.movedAt = 0;
      this.heldBbox = null;
      this.presented = null;
      this.report('refused', error);
    }
  }
}
