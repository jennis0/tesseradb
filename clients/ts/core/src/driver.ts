import {calibrate, type DepthChoice} from './budget.js';
import {plan, type Plan, type PlannerInputs, type Viewport} from './prefetch.js';
import {rectContains, rectContainsTile, rectIntersection, type TileRect} from './rects.js';
import type {Replica, ReplicaFrame} from './replica.js';
import {TesseraError} from './client.js';

/**
 * The driver: the scheduler that decides when the client asks, retries, revalidates and
 * anticipates — the machinery `client-architecture.md` §3 specifies, extracted from the viewer
 * where it had accreted timer by timer.
 *
 * **Four orthogonal regions, one clock.** Motion (still|gesture), the foreground request
 * lifecycle (idle|primary|margin, with the queued-view slot and every retry inside the same
 * generation discipline), the settle cadence, and anticipation (idle|eligible|bite-in-flight).
 * They run concurrently — a pipelined pan is motion *and* a request in flight; an in-flight ring
 * survives movement because its server cost is already paid.
 *
 * **Headless by construction.** The clock is injected; there is no DOM, no deck, no ambient
 * timer. Every behaviour here is testable with a fake clock, which is the property the 2026-08-10
 * review found the previous shape had made impossible — and the three defects that shipped
 * because of it (a staleness bound starved by the covered path, anticipation budgets that never
 * bound, a retry that survived `cancel()`) are pinned by `driver.test.ts` against this object.
 *
 * The driver owns scheduler-facing state — `mTarget`, the last visible count, the presented-frame
 * handle — and reports through callbacks. It never touches presentation: what to draw arrives as
 * a {@link ReplicaFrame} for the consumer to compose and render.
 */

/** The injected clock — `setTimeout`'s shape, so the browser passes its own straight through. */
export type Clock = {
  now(): number;
  after(ms: number, fire: () => void): unknown;
  cancel(handle: unknown): void;
};

export type ViewState = {target: [number, number, number]; zoom: number};

/** What the driver needs to know from `/v1/meta`, injected once at session start. */
export type DriverMeta = {
  kMaxMarks: number;
  maxTilesPerRequest: number;
  thetaTargetMarks: number;
};

export type DriverEvents = {
  /**
   * The reconciler's verdict: what this paint may cost, decided in one place (the tier rule
   * client-architecture §3 assigns to the driver, and review finding F8 demanded be one
   * statement).
   *
   * - `fold` — the presented frame still covers the view and only exact bands moved: the
   *   consumer refreshes those from the depth index and lets stand-ins ride one step stale.
   *   No frame is carried, because none is needed.
   * - `derive` — depth or region changed, or the settle is finalising: the full derivation,
   *   carried as `frame`. The driver has already paid the stand-in walk exactly once and
   *   rate-limited it during gestures; the consumer just composes.
   *
   * The third tier — `reuse`, nothing changed — never reaches the consumer at all: the driver
   * traces it and stops. That the walk cannot run more often than the tier rule allows is the
   * whole fix for the 18–20 fps zoom the eager per-schedule derivation caused.
   */
  onFrame(verdict: {tier: 'fold'; plan: Plan} | {tier: 'derive'; plan: Plan; frame: ReplicaFrame}): void;
  onStatus?(status: 'loading' | 'shown' | 'empty' | 'refused' | 'retrying', detail?: unknown): void;
  /** The Phase-0 instrumentation stream: request/arrived/covered/ring/ringskip/revalidate. */
  onTrace?(kind: string, fields: Record<string, number>): void;
};

export type DriverOptions = {
  /** Trailing debounce while the store can answer the view. */
  debounceMs?: number;
  /** Debounce when it cannot — the request is not redundant, so the wait is nearly nothing. */
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
  /** Anticipation pacing — D5: shipped at design budgets, judged by measurement. */
  maxPrefetchPerPause?: number;
  maxPrefetchBytesPerPause?: number;
  /** Zoom layers kept resident-but-undrawn (0 = none, 2+ for strong machines). */
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

export class Driver {
  private readonly o: Required<DriverOptions>;

  // Scheduler-facing state (client-architecture §3): what is asked of the server is set here.
  private mTarget: number;
  private lastVisibleInView: number | undefined;
  /** The presented-frame handle — all the driver knows of what is on screen. */
  private presented: {want: TileRect; depth: number; version: number; standInStale: boolean} | null =
    null;
  private heldBbox: {bbox: [number, number, number, number]; depth: number} | null = null;

  // Foreground lifecycle.
  private inFlight: AbortController | null = null;
  private inFlightAt: {rect: TileRect; depth: number; since: number} | null = null;
  private queued: ViewState | null = null;
  private generation = 0;
  /** The retry is a timed re-entry *inside* the same discipline — tracked, so cancel() clears it. */
  private retryHandle: unknown = null;

  // Motion.
  /** Negative infinity so the very first schedule reads as arriving from stillness. */
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
   * The banked-calibration release. A settle marks the bank ready; the next `schedule` — motion,
   * by construction — suspends the depth-hold so the calibrated depth can win, and the derive
   * that adopts a depth re-arms the hold by becoming the new `presented`. Without this the hold
   * fed itself forever and the bidirectional calibration was inert (review finding 1): every
   * plan held to `presented.depth`, and the derive the hold forced wrote that same depth back.
   * At rest the hold never releases, which is what keeps marks from popping with no user action.
   */
  private bankReady = false;
  private holdSuspended = false;

  /** The counts-only refresh's own slot — never the foreground's (D2; review finding 3). */
  private revalidating: AbortController | null = null;

  // Anticipation.
  private idleHandle: unknown = null;
  private background: AbortController | null = null;
  private bitesSincePause = 0;
  private bytesSincePause = 0;
  /** Deferred by a foreground in flight — re-evaluated when it clears, not discarded. */
  private anticipationEligible = false;

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
   * Adopt a new marks-on-screen budget mid-session. Construction-time options are otherwise
   * final, and the budget was frozen with them — which left the viewer's budget control changing
   * a store field no plan ever read again.
   *
   * The depth hold is suspended so a one-step depth change is taken on the very next plan. The
   * hold exists to absorb calibration wobble the user never asked for; a budget they just set is
   * the opposite case, and holding it until the next settle banks reads as the control being dead.
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

  private trace(kind: string, fields: Record<string, number>): void {
    this.events.onTrace?.(kind, fields);
  }

  private planFor(view: ViewState, velocity?: [number, number]): Plan {
    const inputs: PlannerInputs = {
      viewport: this.viewportOf(view),
      budget: this.o.budget,
      mTarget: this.mTarget,
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

    // Movement resets the pause: pending eligibility is cancelled (chosen for a view that no
    // longer exists), an in-flight bite survives (its cost is paid), budgets re-arm.
    this.bitesSincePause = 0;
    this.bytesSincePause = 0;
    this.anticipationEligible = false;
    if (this.idleHandle) this.clock.cancel(this.idleHandle);
    if (this.prefetch) {
      this.idleHandle = this.clock.after(this.o.idleMs, () => {
        this.anticipationEligible = true;
        void this.anticipate();
      });
    }

    if (this.covers(view)) {
      // The unheld plan agreed with the presented depth: nothing banked mattered, and a latched
      // suspension would leave later at-rest plans unheld — the pop risk the hold prevents.
      this.holdSuspended = false;
      this.trace('covered', {depth: this.heldBbox?.depth ?? -1});
      this.movedAt = 0;
      // D2 (latency-neutral staleness): the covered path is where a warm client lives, so it is
      // where the bound must be reachable — through the foreground slot, never beside a fetch.
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

  /** An absorb landed mid-fetch: pieces paint as they arrive. The consumer coalesces to frames. */
  absorbed(): void {
    if (this.lastView) this.reconcile('absorb', this.lastView);
  }

  /**
   * The tier rule — the one statement of what a paint may cost, every trigger passing through.
   *
   * Reuse is exact (a version counter, not a heuristic); a fold marks the handle's stand-ins
   * stale and lets the settle repair them; the full derivation is paid at most once per
   * {@link DriverOptions.deriveMinGapMs} while the view is moving, and unconditionally at the
   * settle — which is also the only trigger allowed to clear `standInStale`.
   */
  private reconcile(trigger: 'schedule' | 'absorb' | 'response' | 'settle', view: ViewState): void {
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
      // A settle that found nothing to do still readies the bank: calibration corrections from
      // the arrivals before it apply on the next motion.
      if (trigger === 'settle') this.bankReady = true;
      // Reuse under an unheld plan is agreement — the suspension has done its job.
      this.holdSuspended = false;
      this.trace('reuse', {depth: handle.depth});
      return;
    }

    if (covered && trigger !== 'settle') {
      // Covered under an unheld plan means the depths agree — the suspension has done its job.
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
    // A derive adopted a depth: the hold re-arms around it, and a completed settle readies the
    // bank so the next motion can adopt a recalibrated depth.
    this.holdSuspended = false;
    if (trigger === 'settle') this.bankReady = true;
    this.events.onFrame({tier: 'derive', plan: planned, frame});
  }

  private storeCanAnswer(view: ViewState): boolean {
    return this.covers(view);
  }

  private covers(view: ViewState): boolean {
    if (!this.heldBbox || !this.presented) return false;
    const planned = this.planFor(view);
    if (planned.choice.depth !== this.heldBbox.depth) return false;
    // Novelty over the VISIBLE box, containment over the render rect: the margin beyond the
    // screen is bought opportunistically and its absence must not fail a pan that never left
    // the drawn buffer.
    return (
      rectContains(this.presented.want, planned.visible.rect) &&
      this.replica.novelIn(planned.visible.rect, planned.choice.depth, this.meta.kMaxMarks) === 0
    );
  }

  /**
   * D2: the counts-only refresh runs in its own slot, started only when everything is idle and
   * aborted the moment a real fetch wants the wire — it may never occupy the foreground slot,
   * because `inFlightUseful` would then queue a user's pan behind it (review finding 3).
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
    // The arrival that cleared the slot is the re-arm signal anticipation waits on — guarded on
    // "no foreground in flight AND no queued view", so a chained dispatch keeps deferring it and
    // a bite never queues ahead of the user at the admission gate.
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
    // The retry dies with everything else — the defect the review found was precisely that it
    // did not.
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
    this.movedAt = 0;
    this.velocity = undefined;
    this.lastTarget = null;
    this.anticipationEligible = false;
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
      // A shed ring is nobody's answer; the foreground's retry budget is the one that matters.
    } finally {
      if (this.background === controller) this.background = null;
      // **The re-arm the design exists for**: within one pause, budgets permitting, the next
      // bite follows without re-paying the idle delay. Movement resets the pause and cancels
      // eligibility; a foreground in flight defers via dispatchQueued's re-entry.
      if (this.anticipationEligible && !this.inFlight && !this.queued) void this.anticipate();
    }
  }

  private async request(view: ViewState, attempt = 0): Promise<void> {
    const planned = this.planFor(view);
    const choice: DepthChoice = planned.choice;

    // A real fetch displaces a running revalidation unconditionally — the refresh is the one
    // request the user must never wait behind.
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
    const startedAt = this.clock.now();
    this.lastRequestAt = startedAt;
    this.trace('request', {depth: choice.depth, n: choice.tiles, waited: startedAt - movedAt});
    this.events.onStatus?.('loading');

    try {
      // `standIns: false` — the fetch absorbs and reports; what gets DERIVED is the
      // reconciler's decision, paid once under its own rate rule rather than per fetch.
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
      this.trace('arrived', {
        ms: arrivedAt - startedAt,
        n: frame.plan.bytes,
        server: Math.round((frame.response?.timings.serverUs ?? 0) / 1000),
        depth: choice.depth,
        novel: frame.plan.novel,
        wanted: frame.plan.wanted
      });

      this.heldBbox = {bbox: [0, 0, 0, 0], depth: choice.depth};
      this.reconcile('response', view);

      // Calibration over the rect the prediction was for — the VISIBLE box, not the wider render
      // rect the frame spans. Review F6: summing over 1.69x the predicted area inflated `actual`
      // and silenced the loop in the one direction that mattered.
      let visible = 0;
      let actual = 0;
      for (const b of frame.exact) {
        if (!rectContainsTile(planned.visible.rect, b.x, b.y)) continue;
        visible += Number(b.visible);
        actual += b.served;
      }
      this.lastVisibleInView = visible;
      this.mTarget = calibrate(
        {predictedMarks: choice.predictedMarks, actualMarks: actual, visibleInView: visible},
        this.mTarget,
        this.meta.thetaTargetMarks
      );
      this.events.onStatus?.(actual === 0 && visible === 0 ? 'empty' : 'shown');
      this.movedAt = 0;
      if (this.queued) {
        this.inFlight = null;
        this.dispatchQueued();
        return;
      }

      // The margin leg — an explicit phase of the lifecycle, still owning the foreground slot so
      // a new request supersedes it through the same abort it would use on the primary; its
      // failure or supersession must not touch the *new* request's bookkeeping (review finding 7).
      if (planned.foreground.rect !== planned.visible.rect) {
        try {
          await this.replica.fetchRegion(
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
        } catch {
          // Already drawn; the margin buys the next gesture, not this one.
        }
      }
      if (generation !== this.generation) return;
      this.inFlight = null;
      this.dispatchQueued();
    } catch (error) {
      if (controller.signal.aborted || generation !== this.generation) return;
      this.inFlightAt = null;
      this.inFlight = null;

      // Two retryable refusals, and they are not the same wait. A 429 `backpressure` is the
      // server shedding load, retried on the exponential the shed path always used; a 503
      // `not-ready` is an unverified bundle or an unready worker (contracts §3.1), which the
      // built driver did not retry at all — a client that gave up on it turned a starting server
      // into a refusal. Both surface as `retrying`; both re-enter the same generation discipline.
      const status = error instanceof TesseraError ? error.status : 0;
      const retryable = status === 429 || status === 503;
      if (retryable && attempt < this.o.maxRetries) {
        this.events.onStatus?.('retrying');
        const base = status === 503 ? this.o.notReadyBackoffMs : 1000;
        const backoff = Math.min(base * 2 ** attempt, this.o.retryBackoffMaxMs);
        this.retryHandle = this.clock.after(backoff, () => {
          this.retryHandle = null;
          // Superseded by anything newer: the retry re-enters the same discipline.
          if (generation === this.generation) void this.request(view, attempt + 1);
        });
        return;
      }
      this.movedAt = 0;
      this.heldBbox = null;
      this.presented = null;
      this.events.onStatus?.('refused', error);
    }
  }
}
