import {compose, fold, type Composition} from './compose.js';
import {Driver, type Clock, type DriverMeta, type DriverOptions, type ViewState} from './driver.js';
import type {Plan} from './prefetch.js';
import type {Replica, ReplicaFrame} from './replica.js';

/**
 * The presented frame: the {@link Composition} on screen, and the bookkeeping that puts it there.
 *
 * The driver decides when anything happens and hands over a verdict per paint, fold or derive.
 * This object carries out the verdict and holds the result; `compose.ts` decides which bands
 * contribute. Paints are coalesced to one per animation frame through an injected
 * {@link FrameScheduler}, so the path runs in node under a fake. The consumer gets a `Composition`
 * by reference, with exact bands and stand-in pieces kept separate; building buffers from them is
 * the renderer's work.
 */

/** Schedules work for the next paint. @internal */
export type FrameScheduler = {
  request(fire: () => void): unknown;
  cancel(handle: unknown): void;
};

/** `requestAnimationFrame` in a browser; a 16 ms timeout, one 60 Hz frame, elsewhere. @internal */
export function defaultFrameScheduler(): FrameScheduler {
  if (typeof requestAnimationFrame === 'function' && typeof cancelAnimationFrame === 'function') {
    return {
      request: (fire) => requestAnimationFrame(() => fire()),
      cancel: (handle) => cancelAnimationFrame(handle as number)
    };
  }
  return {
    request: (fire) => setTimeout(fire, 16),
    cancel: (handle) => clearTimeout(handle as ReturnType<typeof setTimeout>)
  };
}

/**
 * What the map is showing, as the store's `status` projection reports it:
 *
 * - `idle`: no view has been asked for yet.
 * - `loading`: a request for the view is in flight.
 * - `retrying`: the server answered `429` or `503`, and the request will be sent again.
 * - `shown`: a frame is on screen. Counts may be displayed in this state only.
 * - `empty`: the answer holds no point this principal may see in view.
 * - `refused`: the request failed, so what is here is not known.
 *
 * A host shows `empty` and `refused` differently, since only `empty` is an answer.
 *
 * @category Projections
 */
export type PresentedStatus = 'idle' | 'loading' | 'retrying' | 'shown' | 'empty' | 'refused';

/**
 * Why a request failed, as the store's projections report it.
 *
 * @category Projections
 */
export type Refusal = {
  /** A {@link TesseraError}'s `code`, or `fetch-failed` for any other error. */
  code: string;
  /** A {@link TesseraError}'s `detail`, or the other error's message. */
  detail: string;
};

/**
 * The {@link Refusal} a thrown error stands for: the `code` and `detail` of a {@link TesseraError},
 * or `fetch-failed` and the message for any other error, such as one from a request that reached
 * no server.
 *
 * @category Projections
 */
export function refusalOf(error: unknown): Refusal {
  const e = (error ?? {}) as {code?: string; detail?: string; message?: string};
  return {code: e.code ?? 'fetch-failed', detail: e.detail ?? e.message ?? String(error)};
}

/** The driver's verdict; see `DriverEvents.onFrame`. */
type Verdict = {tier: 'fold'; plan: Plan} | {tier: 'derive'; plan: Plan; frame: ReplicaFrame};

/** What accompanied a presented frame: the plan that chose it and the fetch that fed it. @internal */
export type Presented = {
  frame: Composition;
  tier: 'fold' | 'derive';
  plan: Plan;
  /** The replica frame a derive composed from; null for a fold, which composes from held state. */
  fetched: ReplicaFrame | null;
  calibration: {mTarget: number; visibleInView: number | undefined};
};

/** @internal */
export type PresenterEvents = {
  /** A frame reached the presented slot. Fires at most once per scheduler tick. */
  onPresented(presented: Presented): void;
  onStatus(status: PresentedStatus, refusal: Refusal | null): void;
  onTrace?(kind: string, fields: Record<string, number | string>): void;
  /** Named phases, for a trace that wants to time the fold and the derive. */
  onPhase?<T>(kind: string, fn: () => T, fields?: Record<string, number>): T;
};

/**
 * Throws where the picture differs from what was served: where exact tiles draw a different number
 * of marks than were served, or a tile that is not exact carries counts, since a superset read as
 * density overstates. A dropped mark discloses nothing, but it is the sign of an assembly bug.
 *
 * @internal
 */
export function assertCompositionMatchesServed(c: Composition): void {
  if (c.exactDrawn !== c.exactServed) {
    throw new Error(
      `composition: drawing ${c.exactDrawn} marks across exact tiles but the server served ` +
        `${c.exactServed}. A mark was lost between the replica and the frame.`
    );
  }
  for (const tile of c.tiles) {
    if (!tile.exact && tile.counts !== null) {
      throw new Error(
        `tile ${tile.prefix} draws a superset of its served set but carries counts. ` +
          `A superset of marks must never be read as density.`
      );
    }
  }
}

/** @internal */
export class Presenter {
  private readonly driver: Driver;
  private held: Composition | null = null;
  private pending: Verdict | null = null;
  private tick: unknown = null;
  private lastView: ViewState | null = null;
  private width = 0;
  private height = 0;
  private status: PresentedStatus = 'idle';

  constructor(
    private readonly replica: Replica,
    meta: DriverMeta,
    clock: Clock,
    private readonly scheduler: FrameScheduler,
    private readonly events: PresenterEvents,
    options: DriverOptions = {},
    prefetch = true
  ) {
    this.driver = new Driver(
      replica,
      meta,
      clock,
      {
        onFrame: (verdict) => this.apply(verdict),
        onStatus: (status, detail) => this.transition(status, detail),
        onTrace: (kind, fields) => events.onTrace?.(kind, fields)
      },
      options,
      prefetch
    );
  }

  /** The composition on screen, or null when nothing is. */
  get frame(): Composition | null {
    return this.held;
  }

  get currentStatus(): PresentedStatus {
    return this.status;
  }

  get calibration(): {mTarget: number; visibleInView: number | undefined} {
    return this.driver.calibration;
  }

  get view(): {view: ViewState; width: number; height: number} | null {
    return this.lastView ? {view: this.lastView, width: this.width, height: this.height} : null;
  }

  /** Every view-state change enters here; the driver debounces. */
  schedule(view: ViewState, width: number, height: number): void {
    this.lastView = view;
    this.width = width;
    this.height = height;
    this.driver.schedule(view, width, height);
  }

  /**
   * Presents this view from the replica's held bands, asking for nothing; see
   * {@link Driver.redraw}. A view holding nothing for the camera presents nothing.
   */
  redraw(view: ViewState, width: number, height: number): void {
    this.lastView = view;
    this.width = width;
    this.height = height;
    this.driver.redraw(view, width, height);
  }

  /** Asks again for the last view, after a budget change or a dropped replica. */
  reschedule(): void {
    if (this.lastView) this.driver.schedule(this.lastView, this.width, this.height);
  }

  /** A piece of a split response was absorbed, and its bands are drawable now. */
  absorbed(): void {
    this.driver.absorbed();
  }

  /** Forwards a changed marks-on-screen budget; the caller reschedules after. */
  setBudget(budget: number): void {
    this.driver.setBudget(budget);
  }

  /**
   * Abandons everything in flight and every timer, and drops the presented frame. Every caller (a
   * change of principal, filter or dataset) leaves the frame answering a question no longer asked.
   */
  cancel(): void {
    this.driver.cancel();
    if (this.tick !== null) this.scheduler.cancel(this.tick);
    this.tick = null;
    this.pending = null;
    this.held = null;
  }

  /**
   * Verdicts coalesce to one per scheduler tick and the newest wins, except that a fold does not
   * replace a pending derive: a fold may be one step stale, while a dropped derive would leave the
   * driver's handle describing a frame the screen never showed.
   */
  private apply(verdict: Verdict): void {
    if (this.pending?.tier === 'derive' && verdict.tier === 'fold') return;
    this.pending = verdict;
    if (this.tick !== null) return;
    this.tick = this.scheduler.request(() => {
      this.tick = null;
      const next = this.pending;
      this.pending = null;
      if (next) this.applyNow(next);
    });
  }

  private phase<T>(kind: string, fn: () => T, fields?: Record<string, number>): T {
    return this.events.onPhase ? this.events.onPhase(kind, fn, fields) : fn();
  }

  /**
   * Carries out the driver's verdict. A fold refreshes exact bands from the depth index and keeps the
   * stand-ins by reference; a derive composes the frame the driver derived.
   */
  private applyNow(verdict: Verdict): void {
    const held = this.held;
    let frame: Composition;
    if (verdict.tier === 'fold') {
      // A fold with nothing drawn follows a refusal that cleared the frame; the settle's derive
      // repairs it.
      if (held === null) return;
      frame = this.phase('refresh', () =>
        fold(held, this.replica.exactIn(held.want, held.depth), this.replica.version)
      );
    } else {
      const fetched = verdict.frame;
      frame = this.phase('derive', () => compose(fetched), {
        depth: fetched.depth,
        n: fetched.exact.length,
        standIn: fetched.fallback.length
      });
    }
    // An empty frame over nothing drawn is not a paint; the status reports `empty`.
    if (frame.exactDrawn + frame.provisional === 0 && this.held === null) return;
    assertCompositionMatchesServed(frame);
    this.held = frame;
    this.status = 'shown';
    this.events.onPresented({
      frame,
      tier: verdict.tier,
      plan: verdict.plan,
      fetched: verdict.tier === 'derive' ? verdict.frame : null,
      calibration: this.driver.calibration
    });
  }

  private transition(status: 'loading' | 'shown' | 'empty' | 'refused' | 'retrying', detail?: unknown): void {
    this.status = status;
    if (status !== 'refused') {
      this.events.onStatus(status, null);
      return;
    }
    // A refusal drops the frame. The replica keeps its bands, which still answer the last view that
    // succeeded.
    this.held = null;
    this.events.onStatus('refused', refusalOf(detail));
  }
}
