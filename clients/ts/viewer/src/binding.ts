import {
  Driver,
  RENDER_MARGIN,
  rectContains,
  worldBbox,
  type Plan,
  type Replica,
  type ReplicaFrame,
  type DriverViewState as ViewState
} from '@tessera/client';
import {
  assemble,
  assembledMarks,
  assertAssemblyMatchesServed,
  foldBandColumn,
  refreshExact,
  type Assembled
} from './assemble.js';
import {widenDomain} from './colour.js';
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
  private pending: {frame: ReplicaFrame; plan: Plan; reason: string} | null = null;
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
        onFrame: (frame, plan, reason) => this.apply(frame, plan, reason),
        onStatus: (status, detail) => this.status(status, detail),
        onTrace: (kind, fields) => trace.event(kind, fields)
      },
      {budget: store.state.budget},
      prefetch
    );
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

  /** Frames coalesce to one application per animation frame; the newest wins. */
  private apply(frame: ReplicaFrame, plan: Plan, reason: string): void {
    this.pending = {frame, plan, reason};
    if (this.raf !== null) return;
    this.raf = requestAnimationFrame(() => {
      this.raf = null;
      const next = this.pending;
      this.pending = null;
      if (next) this.applyNow(next.frame, next.plan, next.reason);
    });
  }

  private applyNow(frame: ReplicaFrame, plan: Plan, reason: string): void {
    const held = this.store.state.assembled;
    // Fold when the drawn frame still covers this one; derive in full otherwise. A `settle` is
    // always the full derivation — that is what the driver's settle cadence exists to pay for.
    const foldable =
      reason !== 'settle' &&
      held !== null &&
      held.depth === frame.depth &&
      rectContains(held.want, frame.want);
    const assembled: Assembled = foldable
      ? trace.phase('refresh', () =>
          refreshExact(held, this.replica.exactIn(held.want, held.depth), this.replica.version)
        )
      : trace.phase(
          'derive',
          () => assemble(frame, this.store.state.colourBy ? [this.store.state.colourBy] : []),
          {depth: frame.depth, n: frame.exact.length, standIn: frame.fallback.length}
        );
    if (assembledMarks(assembled) === 0 && this.store.state.assembled === null) return;
    assertAssemblyMatchesServed(assembled);

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
      if (frame.response) {
        s.lastTimings = frame.response.timings;
        s.lastBytes = frame.plan.bytes;
      }
      s.replicaBytes = this.replica.bytes;
      s.replicaPoints = this.replica.points;
      s.replicaBands = this.replica.bandCount;
      s.lastPlan = {omitted: frame.plan.wanted - frame.plan.novel, fetched: frame.plan.novel};
      s.inFlight = 0;
    });
    // The world bbox the drawn buffer answers — kept for panels; the covered test itself is the
    // driver's, over its presented-frame handle.
    void worldBbox({target: [0, 0], zoom: 0, width: this.width, height: this.height}, RENDER_MARGIN);
    void plan;
    void this.lastView;
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
