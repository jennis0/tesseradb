import {
  Driver,
  type Plan,
  type Replica,
  type ReplicaFrame,
  type DriverViewState as ViewState
} from '@tessera/client';

/** The driver's tier verdict — see `Driver`'s `onFrame` doc for what each costs. */
type Verdict = {tier: 'fold'; plan: Plan} | {tier: 'derive'; plan: Plan; frame: ReplicaFrame};
import {
  assemble,
  assembledMarks,
  assertAssemblyMatchesServed,
  foldBandColumn,
  refreshExact,
  type Assembled
} from './assemble.js';
import {widenDomain} from './colour.js';
import {readConfig} from './config.js';
import {trace} from './trace.js';
import type {Store} from './state.js';

/**
 * The deck-side binding of the core driver — what remains of `ViewportController` after the
 * scheduler moved to `tessera-client` (client-architecture §3).
 *
 * The driver decides *when* anything happens: requests, retries, revalidation, anticipation,
 * settle timing. This binding decides only how a frame the driver hands over becomes the store's
 * `assembled` — the fold-vs-derive choice, the rAF coalescing that keeps redraws to one per
 * animation frame, and the panel bookkeeping. It holds no timers except `requestAnimationFrame`
 * and issues no requests: the two properties that define the vis side of the boundary.
 */
export class DriverBinding {
  private readonly driver: Driver;
  private lastView: ViewState | null = null;
  private width = 0;
  private height = 0;
  /** rAF coalescing — the one clock the vis side is allowed. */
  private pending: Verdict | null = null;
  private raf: number | null = null;

  constructor(
    private readonly store: Store,
    private readonly replica: Replica,
    prefetch = true
  ) {
    const meta = store.state.meta!;
    this.driver = new Driver(
      replica,
      {
        kMaxMarks: meta.selection.kMaxMarks,
        maxTilesPerRequest: meta.maxTilesPerRequest,
        thetaTargetMarks: meta.selection.thetaTargetMarks
      },
      {
        now: () => performance.now(),
        after: (ms, fire) => setTimeout(fire, ms),
        cancel: (handle) => clearTimeout(handle as ReturnType<typeof setTimeout>)
      },
      {
        onFrame: (verdict) => this.apply(verdict),
        onStatus: (status, detail) => this.status(status, detail),
        onTrace: (kind, fields) => trace.event(kind, fields)
      },
      {budget: store.state.budget, prefetchLayers: readConfig().prefetchLayers},
      prefetch
    );
  }

  /** Forward a changed marks-on-screen budget to the driver; the caller schedules after. */
  setBudget(budget: number): void {
    this.driver.setBudget(budget);
  }

  schedule(view: ViewState, width: number, height: number): void {
    this.lastView = view;
    this.width = width;
    this.height = height;
    this.driver.schedule(view, width, height);
  }

  absorbed(): void {
    this.driver.absorbed();
  }

  cancel(): void {
    this.driver.cancel();
    if (this.raf !== null) cancelAnimationFrame(this.raf);
    this.raf = null;
    this.pending = null;
  }

  /** Verdicts coalesce to one application per animation frame; the newest wins — except that a
   * fold never replaces a pending derive: the fold's contract is that its information may ride
   * one step stale, while a dropped derive would leave the driver's presented handle describing
   * a frame the screen never showed. */
  private apply(verdict: Verdict): void {
    if (this.pending?.tier === 'derive' && verdict.tier === 'fold') return;
    this.pending = verdict;
    if (this.raf !== null) return;
    this.raf = requestAnimationFrame(() => {
      this.raf = null;
      const next = this.pending;
      this.pending = null;
      if (next) this.applyNow(next);
    });
  }

  /**
   * Execute the driver's tier — no second-guessing here. The reconciliation rule lives in the
   * driver as one statement; this side only knows how to do what it was told: a fold refreshes
   * exact bands from the depth index and keeps the stand-ins by reference, a derive composes the
   * frame the driver already paid the walk for.
   */
  private applyNow(verdict: Verdict): void {
    const held = this.store.state.assembled;
    let assembled: Assembled;
    if (verdict.tier === 'fold') {
      // A fold against nothing drawn can only follow a refusal that cleared the store while the
      // driver's handle survived; the settle's derive repairs it, so dropping this one is safe.
      if (held === null) return;
      assembled = trace.phase('refresh', () =>
        refreshExact(held, this.replica.exactIn(held.want, held.depth), this.replica.version)
      );
    } else {
      const frame = verdict.frame;
      assembled = trace.phase(
        'derive',
        () => assemble(frame, this.store.state.colourBy ? [this.store.state.colourBy] : []),
        {depth: frame.depth, n: frame.exact.length, standIn: frame.fallback.length}
      );
    }
    if (assembledMarks(assembled) === 0 && this.store.state.assembled === null) return;
    assertAssemblyMatchesServed(assembled);
    const plan = verdict.plan;
    const frame = verdict.tier === 'derive' ? verdict.frame : null;

    const meta = this.store.state.meta;
    const {mTarget, visibleInView} = this.driver.calibration;
    this.store.update((s) => {
      s.assembled = assembled;
      s.sessionWarm = true;
      s.status = 'shown';
      s.mTarget = mTarget;
      if (visibleInView !== undefined) s.lastVisibleInView = visibleInView;
      if (s.colourBy && !meta?.declaredScalars.find((c) => c.name === s.colourBy)?.category) {
        const widened = foldBandColumn(assembled, s.colourBy, s.domains[s.colourBy] ?? null, widenDomain);
        if (widened) s.domains[s.colourBy] = widened;
      }
      if (frame?.response) {
        s.lastTimings = frame.response.timings;
        s.lastBytes = frame.plan.bytes;
      }
      if (frame) {
        s.lastPlan = {omitted: frame.plan.wanted - frame.plan.novel, fetched: frame.plan.novel};
      }
      s.replicaBytes = this.replica.bytes;
      s.replicaPoints = this.replica.points;
      s.replicaBands = this.replica.bandCount;
      s.inFlight = 0;
    });
    void plan;
  }

  private status(status: string, detail?: unknown): void {
    this.store.update((s) => {
      if (status === 'loading') {
        s.status = 'loading';
        s.inFlight = 1;
        return;
      }
      s.inFlight = 0;
      if (status === 'retrying') s.status = 'retrying';
      else if (status === 'empty') s.status = 'empty';
      else if (status === 'refused') {
        s.status = 'refused';
        s.assembled = null;
        const e = detail as {code?: string; detail?: string; message?: string};
        s.lastError = {code: e?.code ?? 'fetch-failed', detail: e?.detail ?? e?.message ?? String(detail)};
      } else s.status = 'shown';
    });
  }
}
