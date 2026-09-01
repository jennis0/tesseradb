import {compose, fold, type Composition} from './compose.js';
import {Driver, type Clock, type DriverMeta, type DriverOptions, type ViewState} from './driver.js';
import type {Plan} from './prefetch.js';
import type {Replica, ReplicaFrame} from './replica.js';

/**
 * The presented frame: the {@link Composition} on screen, and the bookkeeping that puts it there.
 *
 * The driver decides *when* anything happens — requests, retries, revalidation, anticipation,
 * settle timing — and hands over a verdict per paint: fold or derive. This object executes the
 * verdict and holds the result. It used to be the deck binding's business (client-architecture
 * §6 step 3, the half that had not moved); it is in the client because which bands contribute
 * to the frame, and on what authority, is a rule every client must obey, not a rendering detail.
 * `compose.ts` decides; this holds and sequences.
 *
 * **Headless.** The one clock the vis side may own — coalescing paints to one per animation
 * frame — is injected here as a {@link FrameScheduler}, mirroring the driver's injected
 * {@link Clock}, so the whole path from verdict to presented frame runs under a fake scheduler in
 * node. The default is `requestAnimationFrame` where it exists and a 16 ms timeout elsewhere.
 *
 * What the consumer gets is a `Composition` by reference — exact bands and stand-in pieces, never
 * concatenated. Turning pieces into buffers is the vis side's copy to pay (`viewer/src/assemble.ts`).
 */

/** The one vis-side clock, injected: coalesce work to the next paint. `setTimeout`'s shape. */
export type FrameScheduler = {
  request(fire: () => void): unknown;
  cancel(handle: unknown): void;
};

/** `requestAnimationFrame` in a browser; a 16 ms timeout — one 60 Hz frame — anywhere else. */
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
 * The display states, kept distinct because collapsing them is how a fail-closed server becomes a
 * fail-misleading picture (client-interaction §9). `empty` (the principal sees nothing here) and
 * `refused` (we do not know) are semantic opposites; `loading` and `retrying` are neither; `shown`
 * is the only one that may display counts.
 */
export type PresentedStatus = 'idle' | 'loading' | 'retrying' | 'shown' | 'empty' | 'refused';

export type Refusal = {code: string; detail: string};

/** The driver's tier verdict — see `Driver`'s `onFrame` doc for what each costs. */
type Verdict = {tier: 'fold'; plan: Plan} | {tier: 'derive'; plan: Plan; frame: ReplicaFrame};

/** What accompanied a presented frame: the plan that chose it and the fetch that fed it. */
export type Presented = {
  frame: Composition;
  tier: 'fold' | 'derive';
  plan: Plan;
  /** The replica frame a derive composed from; null for a fold, which composes from held state. */
  fetched: ReplicaFrame | null;
  calibration: {mTarget: number; visibleInView: number | undefined};
};

export type PresenterEvents = {
  /** A frame reached the presented slot. Fires at most once per scheduler tick. */
  onPresented(presented: Presented): void;
  onStatus(status: PresentedStatus, refusal: Refusal | null): void;
  onTrace?(kind: string, fields: Record<string, number | string>): void;
  /** Named phases, for a trace that wants to time the fold and the derive. */
  onPhase?<T>(kind: string, fn: () => T, fields?: Record<string, number>): T;
};

/**
 * Fidelity between the picture and what was served — a check, **not an invariant**. Dropping a
 * mark fails in the safe direction (a thinner picture discloses nothing), but a lost mark has
 * been the signature of every assembly bug so far, so it throws. Exact tiles only; no non-exact
 * tile may carry a count, because a superset read as density overstates (`delta-serving.md` §7).
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
   * Present this view from the replica's held bands, asking for nothing — see
   * {@link Driver.redraw}. The switch's immediate publish (`view-switching.md` §3); a view holding
   * nothing for the camera presents nothing, and the caller's empty frame stands.
   */
  redraw(view: ViewState, width: number, height: number): void {
    this.lastView = view;
    this.width = width;
    this.height = height;
    this.driver.redraw(view, width, height);
  }

  /** Re-ask for the last view — what a budget change or a dropped replica needs. */
  reschedule(): void {
    if (this.lastView) this.driver.schedule(this.lastView, this.width, this.height);
  }

  /** A piece of a split response was absorbed: its bands are drawable now, not at the settle. */
  absorbed(): void {
    this.driver.absorbed();
  }

  /** Forward a changed marks-on-screen budget; the caller reschedules after. */
  setBudget(budget: number): void {
    this.driver.setBudget(budget);
  }

  /**
   * Abandon everything in flight and every timer, and drop the presented frame.
   *
   * A frame is dropped rather than kept because the callers — a principal switch, a filter change,
   * a dataset switch — are all cases where what is on screen answers a question no longer being
   * asked, and the next derive is the repair.
   */
  cancel(): void {
    this.driver.cancel();
    if (this.tick !== null) this.scheduler.cancel(this.tick);
    this.tick = null;
    this.pending = null;
    this.held = null;
  }

  /**
   * Verdicts coalesce to one application per scheduler tick; the newest wins — except that a fold
   * never replaces a pending derive. The fold's contract is that its information may ride one step
   * stale, while a dropped derive would leave the driver's presented handle describing a frame the
   * screen never showed.
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
   * Execute the driver's tier — no second-guessing here. The reconciliation rule lives in the
   * driver as one statement; this side only knows how to do what it was told: a fold refreshes
   * exact bands from the depth index and keeps the stand-ins by reference, a derive composes the
   * frame the driver already paid the walk for.
   */
  private applyNow(verdict: Verdict): void {
    const held = this.held;
    let frame: Composition;
    if (verdict.tier === 'fold') {
      // A fold against nothing drawn can only follow a refusal that cleared the frame while the
      // driver's handle survived; the settle's derive repairs it, so dropping this one is safe.
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
    // An empty frame over nothing drawn is not a paint: the status callback says `empty`, and a
    // composition with no marks would make every subscriber rebuild for a picture that has not
    // changed.
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
    // A refusal draws no marks: the frame goes, the replica keeps its bands — they are still the
    // answer to the last view that succeeded, and discarding them would make recovery pay for a
    // full rewrite.
    this.held = null;
    const e = detail as {code?: string; detail?: string; message?: string} | undefined;
    this.events.onStatus('refused', {
      code: e?.code ?? 'fetch-failed',
      detail: e?.detail ?? e?.message ?? String(detail)
    });
  }
}
