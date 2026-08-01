//! The write path: the single writer thread that owns the WAL, the two queues that feed it, and
//! the live state a handler reads before submitting.
//!
//! Carved out of `session.rs` by the stage-2.1 seam (Task 0a); given its executor by Task 3a.
//!
//! ## "Executor" and "the lifecycle thread" are the same thing
//!
//! Lifecycle §1.3, §4 and §7 call this **the lifecycle thread**; the stage-2.1 plan's brief
//! introduced "executor" and the types below took the name. They denote one object — the OS thread
//! is literally named `"tessera-lifecycle"` at [`WritePath::start_executor`]. In particular §7's
//! "the engine's public API is sync and owns no executor" is about **async runtimes**: it forbids
//! `tessera-engine` acquiring tokio and running futures (policed by `scripts/check-layers.sh`'s
//! `deny tessera-engine tokio`), not owning a plain `std::thread`. A synchronous engine that owns
//! one writer thread is what §1.3 asks for; `Engine::accept_ingest` blocking its caller is the
//! visible consequence, and is why a tokio handler must wrap it in `spawn_blocking`.
//!
//! ## What changed at Task 3a, and why it is the substance rather than a refactor
//!
//! Phase 1 served `/control/ingest` and `/control/changes` **inline**, on whichever thread the
//! request landed on, and kept `append → fsync → apply → swap` atomic by holding one `Mutex<Wal>`
//! across all four steps — a discipline defended by a fourteen-line comment, because the mutex is
//! not obviously about ordering at all. Task 3a deletes that mutex. The `Wal` is **moved by value**
//! onto one [`Executor`] thread per partition, and the ordering stops being a discipline: there is
//! one thread that can reach the WAL, one thread that can publish a generation, and it does the
//! four steps in that order because there is nowhere else for them to happen.
//!
//! The lost-update race the comment was defending against (Critical 1 — two acceptances both
//! `load_full`, both clone, and whichever `store`s last silently discards the other's already-acked
//! change) is gone for the same reason, and so is **Track C's S2**: the engine has exactly one
//! non-atomic `.store(` — the swap below — and it runs on the executor thread.
//! `scripts/check-layers.sh` polices that, because the property survives only while it stays true
//! and stage 2.2's flush is precisely a second publisher.
//!
//! *Honest limit, so a reader does not over-read the claim:* `Engine::generation` is `pub(crate)`
//! and `ArcSwap::store` is a public inherent method, so any module in this crate **could** publish.
//! What is structural is that no part of the *write path* holds that capability any more — this
//! type does not own the pointer, only the executor does. Narrowing `Engine::generation` itself
//! needs `session.rs` and `viewport.rs` together and is a controller decision.
//!
//! ## Three types, split by who writes
//!
//! - [`LiveState`] — the maps and indices a handler **reads** and the executor **writes**.
//!   `Arc`-shared; each field keeps the lock it had, because each is still read concurrently by a
//!   request path that is not the executor.
//! - [`WritePath`] — the handler side, held by `Engine`. Owns the not-yet-started `Wal`, the
//!   [`LifecycleHandle`], and the thread's `JoinHandle`.
//! - [`Executor`] — the thread. Owns the [`ExecutorWal`] and the only publishing capability.
//!
//! ## The lane asymmetry
//!
//! Work is bounded (`ingest_queue_bound`, full → 429); deny is unbounded and **can never be
//! refused for load**. The loop drains deny to empty before touching work, so a deny's wait is
//! bounded by the work item currently executing rather than by queue depth. Two consequences,
//! chosen rather than discovered: **a sustained deny flood starves ingest completely**, and **the
//! deny queue is unbounded in memory**.
//!
//! The lane is chosen by the **command**, not by which method a handler called — see
//! [`LifecycleHandle::submit`]. That is not tidiness: `submit(Command::Change { .. })` putting a
//! suppression on the bounded queue would 429 a security operation, which contracts §3.1 forbids.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard};

use rustc_hash::FxHashMap;

use tessera_authz::Dict;
use tessera_lifecycle::alloc::{high_water_from, AllocError, Allocator};
use tessera_lifecycle::buffer::DescriptorResolver;
use tessera_lifecycle::command::{Ack, Command, ExecError, Receipt, SubmitError, UnallocatedRow};
use tessera_lifecycle::faults::WalMeter;
use tessera_lifecycle::overlay::replay;
use tessera_lifecycle::wal::{ChangeOp, ExecutorWal, Wal, WalError, WalRecord};
use tessera_lifecycle::window::{ClosedEntry, CommitWindow, WindowEntry};
use tessera_lifecycle::{IngestBuffer, Overlay};
use tessera_plugin::Descriptor;
use tessera_store::StoreError;
use tessera_types::{EntityId, TermId};

use crate::session::EngineError;
use crate::{Generation, GenerationHandle};

// =================================================================================================
// Posture and counters
// =================================================================================================

/// What the write executor is currently able to do — **the signal Task 3b's `readyz` reads**.
///
/// Four states rather than a bool, because the operator response differs and a bool would collapse
/// "nobody started a writer" into "the writer died", which are different bugs.
///
/// **Monotone.** Published with `fetch_max` over the discriminant, never `store`, so a thread that
/// panics the instant it is spawned cannot have its `Dead` clobbered by the parent's `Running`.
/// Readiness that can go back up is not readiness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum ExecutorPosture {
    /// `Engine::start_write_executor` was never called. Writes are refused; reads are unaffected.
    NotStarted = 0,
    /// Executing normally.
    Running = 1,
    /// The WAL refuses every further operation (lifecycle §4's fail-closed rule). **The executor
    /// is still alive and still applying denies** — see [`Executor::run`] for why exiting here
    /// would be the worse of two fail-closed answers.
    WalPoisoned = 2,
    /// The thread is gone: it panicked, or every handle was dropped and it shut down. Both mean
    /// the same thing to a caller — there is nothing left to apply a write to.
    Dead = 3,
}

impl ExecutorPosture {
    /// The stable wire spelling for `/control/status`.
    ///
    /// **Not `Debug`.** These strings are read by operators and by whatever scrapes the admin
    /// plane, so a rename of a Rust variant must not silently change an operator-facing field;
    /// `posture_spellings_are_stable` is what makes that a test rather than an intention. Kebab
    /// case to match the error `code` vocabulary (`not-ready`, `fail-closed`, `bad-credential`)
    /// that shares the same surfaces.
    pub fn as_str(self) -> &'static str {
        match self {
            ExecutorPosture::NotStarted => "not-started",
            ExecutorPosture::Running => "running",
            ExecutorPosture::WalPoisoned => "wal-poisoned",
            ExecutorPosture::Dead => "dead",
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            1 => ExecutorPosture::Running,
            2 => ExecutorPosture::WalPoisoned,
            3 => ExecutorPosture::Dead,
            _ => ExecutorPosture::NotStarted,
        }
    }
}

/// The executor's liveness and its counters, shared between the thread and every reader.
///
/// Lives in `tessera-engine` rather than in `tessera-lifecycle` because it describes **the
/// thread**, and the thread is this crate's by the plan's Decision 1 — a crate that deliberately
/// owns no executor should not own the executor's liveness vocabulary.
#[derive(Debug)]
pub struct ExecutorHealth {
    posture: AtomicU8,
    work_submitted: AtomicU64,
    deny_submitted: AtomicU64,
    /// Total nanoseconds spent in **the whole apply step** — the `IngestBuffer`/`Overlay` clone,
    /// the per-row inserts, the `Generation` construction and the swap.
    ///
    /// **Named for what it measures.** It was `clone_nanos_*` while the timer already started at
    /// the top of `apply_ingest`/`apply_change`, so it was never the clone alone, and Task 7b is
    /// told to size the deny-ack floor from it — a figure that must not quietly be something else.
    /// The clone dominates it (it is O(total buffered items) while the inserts are O(batch)), which
    /// is why it is still the right operand for that sizing; but the name now says what was timed.
    ///
    /// **This bounds the deny-ack *wait*, not the deny's own cost — corrected 2026-08-01 against
    /// measurement** (`docs/evidence/memos/2026-08-01-deny-ack-baseline.md`). A deny's wait is
    /// bounded by "the work item currently executing", and *that* item's apply includes a clone
    /// that is O(total buffered items) — plan 7b sizes it at 100–300 ms per clone at 1 M buffered
    /// items and 1–3 s at 10 M — while `flush_max_items` is inert until stage 2.2, so the buffer
    /// only grows. Measured at 1 M buffered items: a deny under sustained ingest acks in 165 ms
    /// p50 / 346 ms max, against a 3.2 ms quiescent floor. That much is confirmed.
    ///
    /// **What this counter is NOT is the deny's own floor**, which an earlier revision of this doc
    /// claimed and Task 7b was told to size from. `apply_change` clones the **overlay**
    /// (`Executor::apply_change`); only `apply_ingest` clones the buffer. Measured, a deny's own
    /// apply is **1.3 µs at 1,000,000 buffered items** and is flat in buffer depth — it is
    /// O(overlay), rising to ~4.3 µs at overlay depth 2,000. This counter sums **both lanes**, so
    /// its value is dominated by ingest applies and attributes none of itself to either.
    ///
    /// **And it is an estimator of the wait, not a bound on it**: measured, the worst deny ack
    /// exceeds `apply_nanos_max` over the same phase by up to 1.53×, because a deny waits for the
    /// whole in-flight item (append, fsync, apply, ack) and then pays its own append and fsync.
    ///
    /// Counted from Task 3a rather than 7b precisely because 3a is where lifecycle §1.3's
    /// "never queued behind work of unbounded duration" first becomes a claim made in code.
    apply_nanos_total: AtomicU64,
    apply_nanos_max: AtomicU64,
    /// Work-lane jobs whose `execute` has returned. **The other half of the queue-depth gauge**:
    /// `work_submitted - work_completed` is what [`ExecutorStats::work_depth`] reports and what
    /// Task 6's `retry_after_s` is derived from. Deny-lane jobs are deliberately not counted here
    /// — they ride an unbounded queue that has no depth to report and no 429 to derive.
    work_completed: AtomicU64,
    /// An **exponentially-weighted** mean of one work-lane job's whole service time (append +
    /// fsync + apply + swap + ack), in nanoseconds. Written only by the executor thread.
    ///
    /// **Why not a cumulative mean over `total / completed`.** That was this field's first shape and
    /// it is wrong in the one case the estimate exists for. Service time is dominated by an
    /// `IngestBuffer` clone that is O(total buffered items) and grows monotonically while flush is
    /// inert (`apply_nanos_total`'s doc has the measurements), so a server that ingested 10⁶ fast
    /// batches and has since risen to seconds per batch still reports the fast mean — the old
    /// samples swamp the recent ones — and a caller is told to come back in one second against a
    /// queue that needs three minutes. An estimate that is wrong in the unsafe direction *and*
    /// carries the authority of a derivation is worse than the hard-coded `1` it replaces.
    ///
    /// `x += (sample - x) / 8` — integer, one atomic, no allocation, O(1) on the 10⁹ write path.
    ///
    /// **It is written only when a job *finishes*, which makes it blind in exactly the state that
    /// produces the 429** — see [`Self::work_started_nanos`], which is the correction.
    work_service_nanos_ewma: AtomicU64,
    /// When the work item currently executing started, as nanoseconds since [`Self::base`], **plus
    /// one**; `0` means no work item is in flight. Written only by the executor thread.
    ///
    /// # Why this exists (fix round 1, F7)
    ///
    /// [`Self::record_work_service`] runs *after* `execute` returns, so while one long job is in
    /// flight the EWMA still reports the previous, faster regime. That is the 10⁹ shape: an
    /// `IngestBuffer` clone is O(total buffered items) and flush is inert until stage 2.2, so the
    /// first job at a new buffer depth is the slow one, and it is precisely while it runs that the
    /// queue fills and callers are shed. Every one of them was told to come back in 1 s against a
    /// drain measured in minutes, and an obedient caller then re-establishes a connection and
    /// re-uploads up to `ingest_max_batch_bytes` per second, per client. **The estimator's error was
    /// on the load-amplifying side**, which none of its three stated caveats covered.
    ///
    /// [`ExecutorStats::service_nanos_for_estimate`] takes `max(ewma, elapsed-of-current-job)`,
    /// which is the cheapest correction that cannot under-report: whatever the recent regime was,
    /// the job running *now* has already taken this long, and a caller behind it waits at least that.
    ///
    /// The `+1` is what distinguishes "started at zero nanoseconds" from "idle" without a second
    /// atomic.
    work_started_nanos: AtomicU64,
    /// The origin [`Self::work_started_nanos`] is measured from. An `Instant` is not storable in an
    /// atomic; a fixed origin plus an atomic offset is, and the executor's `Instant::now()` is
    /// already taken for the service sample, so this costs no extra clock read on the write path.
    base: std::time::Instant,
    /// Task 6 (D5): overlay depth at which [`Executor::apply_change`] raises an alarm.
    /// [`usize::MAX`] means **no limit configured**, which is what every embedder and every test
    /// that never calls `Engine::set_overlay_soft_limit` gets.
    ///
    /// Deliberately not `0` for "unset": `tessera-server`'s config refuses `0` as degenerate for
    /// this key, so one value would have to mean "off" on one side of the crate boundary and
    /// "alarm on everything" on the other. That is how a knob comes to be silently inert.
    overlay_soft_limit: AtomicUsize,
    /// Times the overlay has **crossed** into being at or above [`Self::overlay_soft_limit`]. **It
    /// alarms; it does not act** — there is no fold until stage 2.3, so this counter and its log
    /// line are the whole of the mechanism.
    ///
    /// **Crossings, not publications** (fix round 1, F5). This counted every `apply_change` at or
    /// above the limit, i.e. it was level-triggered on a quantity that never decreases: `Overlay`
    /// entries survive `suppress → unsuppress`, and nothing shrinks the overlay until stage 2.3. A
    /// node that crossed 500 000 therefore emitted one four-line WARN **per deny, forever**, with no
    /// path back — flooding the log precisely while the node was under deny pressure. `control.rs`
    /// states that exact standard itself ("an ERROR per occurrence is an alarm flood rather than a
    /// signal") one file over. [`Self::overlay_soft_limit_latched`] is the edge.
    overlay_soft_limit_alarms: AtomicU64,
    /// Whether the overlay is currently *known* to be at or above the soft limit — the edge
    /// trigger's memory. Set when [`Self::note_overlay_depth`] observes a crossing, cleared when it
    /// observes a depth below the limit or when the limit itself is re-set.
    overlay_soft_limit_latched: AtomicBool,
    /// Task 7a: the row count at which a commit window closes — `ingest.commit_window_max_items`,
    /// which counts **rows** (see that key's doc: its default is sized from `window rows ×
    /// term_density`, and both the heap and the latency a window costs scale in rows).
    ///
    /// Reaches the executor by [`Engine::set_commit_window_max_rows`] on `set_overlay_soft_limit`'s
    /// precedent, because widening `start_write_executor`'s argument list would touch
    /// `engine/tests/pins.rs` and four `tessera-bench` sites, outside Track B's allowlist.
    ///
    /// **Defaulted to a real number, not `usize::MAX`.** An embedder that sets nothing must still
    /// get a bounded window: the drain that fills a window frees a queue slot per entry, which a
    /// concurrent submitter immediately refills, so "close when the queue is empty" is not a bound
    /// under sustained load — it is an invitation to hold the entire load in memory. See
    /// [`DEFAULT_COMMIT_WINDOW_MAX_ROWS`].
    commit_window_max_rows: AtomicUsize,
    /// The executor's WAL counters. A **clone** of the meter the [`ExecutorWal`] holds, kept here
    /// so the numbers have a reader: `/control/status` (Task 3b) and Task 7a's
    /// `one_fsync_per_window`, whose whole subject is `wal_fsyncs` not rising with the number of
    /// submissions in a window. Constructed here and cloned into the handle at
    /// [`WritePath::start_executor`], never moved into it.
    wal: Arc<WalMeter>,
}

/// A snapshot of [`ExecutorHealth`], for `/control/status` and for tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecutorStats {
    pub posture: ExecutorPosture,
    pub work_submitted: u64,
    pub deny_submitted: u64,
    /// See [`ExecutorHealth::apply_nanos_total`] — the whole apply step, not the clone alone.
    pub apply_nanos_total: u64,
    pub apply_nanos_max: u64,
    /// Successful WAL appends since the executor started.
    pub wal_appends: u64,
    /// Successful WAL fsyncs since the executor started — the unit Task 7a's group commit is
    /// defined in ("one fsync per window") and the one the ingest baseline memo's ~3.2 ms floor is
    /// a cost per.
    ///
    /// **`wal_appends / wal_fsyncs` is the production measurement of Task 7a's amortisation**, and
    /// the reason no window-size gauge was added: one append per entry and one fsync per window make
    /// that ratio the mean entries per window, over the whole life of the executor. Both are already
    /// on `/control/status`. It is the *ingest* mean only where ingest dominates: every `Change`
    /// contributes one of each and pulls the ratio towards 1, since a deny is appended and fsynced
    /// alone (`execute_change`) and stays out of the window until Task 9. Read it when diagnosing
    /// ingest throughput — a ratio pinned at ~1.0 under
    /// concurrent load means every window is closing with one entry in it, which is what a workload
    /// that re-ingests the same `external_id`s does (`CommitWindow::holds_external_id_of` closes the window
    /// on nearly every entry), and it is the difference between group commit working and group commit
    /// running.
    pub wal_fsyncs: u64,
    /// Work-lane jobs whose `execute` has returned (Task 6).
    pub work_completed: u64,
    /// `work_submitted - work_completed`, saturating.
    ///
    /// **A snapshot of two independently-advancing counters, not an instantaneous truth.** They are
    /// read separately and `work_submitted` is bumped *after* the `try_send`
    /// ([`LifecycleHandle::submit`]), so the executor can complete a job before its submitter has
    /// recorded it and `completed > submitted` is transiently legal. `saturating_sub` is why that
    /// is harmless rather than an underflow.
    pub work_depth: u64,
    /// The EWMA of one work-lane job's whole service time — see
    /// [`ExecutorHealth::work_service_nanos_ewma`]. `0` means nothing has completed yet.
    pub work_service_nanos_ewma: u64,
    /// How long the work item currently executing has been running, in nanoseconds; `0` when the
    /// executor is idle. See [`ExecutorHealth::work_started_nanos`].
    pub work_in_flight_nanos: u64,
    /// Times the overlay crossed to at or above the configured soft limit (Task 6, D5). **It alarms;
    /// it does not act**, and it counts **crossings**, not publications above the limit — see
    /// [`ExecutorHealth::overlay_soft_limit_alarms`].
    pub overlay_soft_limit_alarms: u64,
}

impl ExecutorStats {
    /// The service figure a `retry_after_s` derivation must use: `max(ewma, in-flight elapsed)`.
    ///
    /// **Never the raw EWMA.** [`ExecutorHealth::work_started_nanos`] has the argument in full: the
    /// EWMA is written only when a job finishes, so during the one long job that is filling the
    /// queue it still reports the previous fast regime — and telling every shed caller to come back
    /// in a second against a drain measured in minutes amplifies exactly the load the 429 exists to
    /// shed.
    ///
    /// It remains an **estimator**, and this correction does not change that; it removes one
    /// specific error whose direction was known and unsafe. `estimate_retry_after_s`'s own doc has
    /// the three reasons that remain.
    pub fn service_nanos_for_estimate(&self) -> u64 {
        self.work_service_nanos_ewma.max(self.work_in_flight_nanos)
    }
}

impl ExecutorHealth {
    fn new() -> Self {
        ExecutorHealth {
            posture: AtomicU8::new(ExecutorPosture::NotStarted as u8),
            work_submitted: AtomicU64::new(0),
            deny_submitted: AtomicU64::new(0),
            apply_nanos_total: AtomicU64::new(0),
            apply_nanos_max: AtomicU64::new(0),
            work_completed: AtomicU64::new(0),
            work_service_nanos_ewma: AtomicU64::new(0),
            work_started_nanos: AtomicU64::new(0),
            base: std::time::Instant::now(),
            overlay_soft_limit: AtomicUsize::new(usize::MAX),
            overlay_soft_limit_alarms: AtomicU64::new(0),
            overlay_soft_limit_latched: AtomicBool::new(false),
            commit_window_max_rows: AtomicUsize::new(DEFAULT_COMMIT_WINDOW_MAX_ROWS),
            wal: Arc::new(WalMeter::new()),
        }
    }

    fn advance(&self, to: ExecutorPosture) {
        self.posture.fetch_max(to as u8, Ordering::SeqCst);
    }

    pub fn posture(&self) -> ExecutorPosture {
        ExecutorPosture::from_u8(self.posture.load(Ordering::SeqCst))
    }

    pub fn stats(&self) -> ExecutorStats {
        let work_submitted = self.work_submitted.load(Ordering::Relaxed);
        let work_completed = self.work_completed.load(Ordering::Relaxed);
        ExecutorStats {
            posture: self.posture(),
            work_submitted,
            deny_submitted: self.deny_submitted.load(Ordering::Relaxed),
            apply_nanos_total: self.apply_nanos_total.load(Ordering::Relaxed),
            apply_nanos_max: self.apply_nanos_max.load(Ordering::Relaxed),
            wal_appends: self.wal.appends(),
            wal_fsyncs: self.wal.fsyncs(),
            work_completed,
            work_depth: work_submitted.saturating_sub(work_completed),
            work_service_nanos_ewma: self.work_service_nanos_ewma.load(Ordering::Relaxed),
            work_in_flight_nanos: self.work_in_flight_nanos(),
            overlay_soft_limit_alarms: self.overlay_soft_limit_alarms.load(Ordering::Relaxed),
        }
    }

    /// How long the work item currently executing has been running; `0` when idle.
    ///
    /// One clock read, taken only when a snapshot is asked for — the 429 paths and
    /// `/control/status`, never per row. `saturating_sub` because the two reads are not atomic
    /// together: the executor can finish and clear the marker between the load and the elapsed, and
    /// a job that started "in the future" relative to a stale `base.elapsed()` must report zero
    /// rather than wrap to an enormous drain estimate.
    fn work_in_flight_nanos(&self) -> u64 {
        match self.work_started_nanos.load(Ordering::Relaxed) {
            0 => 0,
            started_plus_one => (self.base.elapsed().as_nanos() as u64)
                .saturating_sub(started_plus_one.saturating_sub(1)),
        }
    }

    /// An otherwise-fresh health block whose clock origin is in the past, so a test can construct a
    /// job that has been in flight for a stated duration without waiting for one. Test-only, and it
    /// touches nothing but [`Self::base`] — every counter starts where `new` puts it.
    #[cfg(test)]
    fn with_base(base: std::time::Instant) -> Self {
        let mut health = Self::new();
        health.base = base;
        health
    }

    /// Mark the work item that is about to run. Executor thread only.
    fn mark_work_started(&self, at: std::time::Instant) {
        let offset = at.saturating_duration_since(self.base).as_nanos() as u64;
        self.work_started_nanos
            .store(offset.saturating_add(1), Ordering::Relaxed);
    }

    fn record_apply(&self, nanos: u64) {
        self.apply_nanos_total.fetch_add(nanos, Ordering::Relaxed);
        self.apply_nanos_max.fetch_max(nanos, Ordering::Relaxed);
    }

    /// One commit window finished: `entries` work-lane jobs completed, in `elapsed_nanos` between
    /// them.
    ///
    /// **The EWMA sample is `elapsed / entries`, not the whole window**, because the estimator it
    /// feeds multiplies it by [`ExecutorStats::work_depth`], which counts *commands*. A whole-window
    /// sample would tell every shed caller to wait the window factor longer than the drain takes —
    /// wrong in the same direction the cumulative-mean form was, just not as far.
    ///
    /// `entries` is never zero: an empty window is never opened (see [`Executor::run_work_pass`]).
    ///
    /// `elapsed_nanos` must be measured from the window's `opened_at`, which is stamped when the
    /// window is *constructed* — so a replacement window is constructed only after the previous
    /// one's `close_window` returns, or every window after the first in a pass charges its
    /// predecessor's append, fsync, apply and swap to itself.
    fn record_window_service(&self, entries: u64, elapsed_nanos: u64) {
        debug_assert!(entries > 0, "an empty window is never closed");
        let entries = entries.max(1);
        // `entries - 1` here and one more inside `record_work_service`: one completion per entry.
        self.work_completed
            .fetch_add(entries - 1, Ordering::Relaxed);
        self.record_work_service(elapsed_nanos / entries);
    }

    /// A work-lane job that was answered without being executed — an idempotent replay, a 409. It
    /// occupied a queue slot and was counted at submission, so it must be counted here too or
    /// [`ExecutorStats::work_depth`] drifts upward forever and every 429 inherits the drift.
    fn note_work_refused(&self) {
        self.work_completed.fetch_add(1, Ordering::Relaxed);
    }

    /// One work-lane job finished, and the EWMA observation for it. Called on the executor thread
    /// and nowhere else, which is what lets the EWMA be a plain load/store rather than a CAS loop.
    ///
    /// `sample_nanos` is the **per-job** service — the whole of what a queued job waits for: append,
    /// fsync, apply, swap and ack. A window divides its elapsed by its entry count before calling
    /// here (see [`ExecutorHealth::record_window_service`]), because the estimator this feeds
    /// multiplies the EWMA by a depth counted in *commands*. `apply_nanos_total` is the wrong
    /// operand for a drain estimate and its own doc says why: it sums both lanes and excludes the
    /// fsync, and the fsync is the term the drain is paced by.
    fn record_work_service(&self, sample_nanos: u64) {
        // Cleared **first**: between this and the EWMA store, a concurrent `stats()` should see the
        // stale (smaller) EWMA rather than an in-flight elapsed for a job that has finished. Both
        // orderings are honest; this one cannot over-report a drain that is already over.
        self.work_started_nanos.store(0, Ordering::Relaxed);
        self.work_completed.fetch_add(1, Ordering::Relaxed);
        let prev = self.work_service_nanos_ewma.load(Ordering::Relaxed);
        let next = if prev == 0 {
            // First observation: seed rather than decay towards a fictitious zero, which would
            // otherwise take eight jobs to reach the truth and under-report for all of them.
            sample_nanos
        } else {
            let p = prev as i128;
            let s = sample_nanos as i128;
            (p + (s - p) / 8).max(1) as u64
        };
        self.work_service_nanos_ewma.store(next, Ordering::Relaxed);
    }

    /// Task 6 (D5): the overlay depth at which `apply_change` alarms. `usize::MAX` disables it.
    ///
    /// **Re-arms the edge trigger.** Setting the limit is a configuration act, so the next
    /// [`Self::note_overlay_depth`] must evaluate it afresh — otherwise lowering the limit under an
    /// already-latched overlay would be silent, which is the one moment an operator most wants the
    /// alarm.
    pub fn set_overlay_soft_limit(&self, limit: usize) {
        self.overlay_soft_limit.store(limit, Ordering::Relaxed);
        self.overlay_soft_limit_latched
            .store(false, Ordering::Relaxed);
    }

    pub fn overlay_soft_limit(&self) -> usize {
        self.overlay_soft_limit.load(Ordering::Relaxed)
    }

    /// Task 7a: set the row count at which a commit window closes. See
    /// [`Self::commit_window_max_rows`] for the field, and [`Engine::set_commit_window_max_rows`]
    /// for why it arrives this way rather than through `start_write_executor`.
    pub fn set_commit_window_max_rows(&self, rows: usize) {
        self.commit_window_max_rows
            .store(rows.max(1), Ordering::Relaxed);
    }

    pub fn commit_window_max_rows(&self) -> usize {
        self.commit_window_max_rows.load(Ordering::Relaxed)
    }

    /// Evaluate an overlay depth against the soft limit, **edge-triggered**, and report whether this
    /// observation is a crossing worth alarming on.
    ///
    /// One function rather than a check plus a counter bump, because the edge is the whole content:
    /// a caller that could ask "am I over?" and then bump would reintroduce the level-triggered
    /// flood one call site at a time. Both evaluation sites — `Executor::apply_change` at runtime,
    /// `Engine::set_overlay_soft_limit` for the overlay a WAL replay produced before any executor
    /// existed — go through here, so they share the counter *and* the edge.
    ///
    /// Returns `true` at most once per crossing. Depth falling back below the limit re-arms it, as
    /// does re-setting the limit; neither happens in this build (overlay entries survive
    /// `unsuppress` and nothing folds until stage 2.3), and the trigger is written for the mechanism
    /// rather than for the current absence of one.
    pub fn note_overlay_depth(&self, depth: usize) -> bool {
        if depth >= self.overlay_soft_limit.load(Ordering::Relaxed) {
            if self
                .overlay_soft_limit_latched
                .swap(true, Ordering::Relaxed)
            {
                return false;
            }
            self.overlay_soft_limit_alarms
                .fetch_add(1, Ordering::Relaxed);
            true
        } else {
            self.overlay_soft_limit_latched
                .store(false, Ordering::Relaxed);
            false
        }
    }
}

/// Contracts §3.1's 429 row and §0.3 **deviation 11** (r10): every 429 carries `Retry-After` and an
/// agreeing body `retry_after_s`, and **the value is per-subject** — the fixed `1` belongs to the
/// compute-admission gate alone. This is the ingest queue's own figure.
///
/// `depth` jobs ahead of the caller, each taking about `service_nanos` to drain.
///
/// # This is an estimator, and here is exactly what makes it one
///
/// 1. **Service time is not stationary.** One work-lane job costs one fsync plus an `IngestBuffer`
///    clone that is O(total buffered items), and `flush_max_items` is inert until stage 2.2, so the
///    buffer only grows. Measured (`docs/evidence/memos/2026-08-01-deny-ack-baseline.md`): ~3.0–3.5 ms
///    quiescent even at 1 M buffered, but 67–167 ms p50 and up to 666 ms under six concurrent
///    submitters. The EWMA tracks the recent regime; it does not predict the next one.
/// 2. **The deny lane is in the real drain and not in this figure.** `Executor::run` drains the
///    deny queue **to empty** before taking a single work item, so a burst of suppressions delays
///    every queued ingest by time this estimate cannot see. Denies are never shed for load, so this
///    is by design, and it means the answer is a floor on a busy node rather than a bound.
/// 3. **`depth` is a snapshot of two independently-advancing counters** — see
///    [`ExecutorStats::work_depth`].
///
/// No caller may treat the result as a bound. What it is good for is the thing a fixed `1` gets
/// wrong: a caller retrying every second against a queue draining in thirty manufactures exactly the
/// load the 429 exists to shed, which is deviation 11's own argument.
///
/// `service_nanos == 0` means **nothing has completed yet**, so there is no observation at all;
/// the answer is [`RETRY_AFTER_MIN_SECS`], and that is a floor chosen for lack of evidence, never a
/// measurement.
pub fn estimate_retry_after_s(depth: u64, service_nanos: u64) -> u64 {
    if service_nanos == 0 {
        return RETRY_AFTER_MIN_SECS;
    }
    // `u128` throughout: `depth` is bounded by the queue bound but `service_nanos` is not bounded
    // by anything, and a wrap here would produce a *small* retry hint — i.e. it would silently
    // answer "come back immediately" for the deepest queue, which is the one case the whole
    // mechanism exists for.
    let nanos = (depth as u128).saturating_mul(service_nanos as u128);
    let secs = nanos.div_ceil(1_000_000_000);
    (secs.max(RETRY_AFTER_MIN_SECS as u128) as u64).min(RETRY_AFTER_MAX_SECS)
}

/// The floor under every derived `retry_after_s`. Zero would tell a client to retry immediately,
/// which is a busy-wait dressed as backpressure.
pub const RETRY_AFTER_MIN_SECS: u64 = 1;

/// The ceiling under every derived `retry_after_s`.
///
/// Argued rather than picked: past a few minutes every client library treats `Retry-After` as "go
/// away", so a larger number buys no additional client behaviour while making the header look
/// broken. The operator's signal for a queue that deep is `work_depth` on `/control/status`, which
/// is not clamped.
pub const RETRY_AFTER_MAX_SECS: u64 = 300;

/// Task 7a: the commit window's row bound for an engine whose embedder sets none.
///
/// The same figure `tessera-server`'s `ingest.commit_window_max_items` defaults to, restated here
/// because this crate cannot see that crate's config and **must not** default to "unbounded". The
/// drain that fills a window frees a bounded-queue slot per entry, and a concurrent submitter
/// refills it immediately, so under sustained load `work.try_recv()` never returns `Err` and a
/// window bounded only by the drain is bounded by nothing: the executor would hold the entire load
/// in memory, through allocation, framing and apply, having appended nothing. `ingest_queue_bound`
/// is deliberately small for exactly that reason (`config.rs`: "backpressure that arrives at the
/// OOM killer is not [a working queue]") and a window must not undo it.
pub const DEFAULT_COMMIT_WINDOW_MAX_ROWS: usize = 10_000;

// =================================================================================================
// The ack channel, and the proof it demands
// =================================================================================================

/// The receipt half of a submitted command: the sender, the proof token, and **nothing else**.
///
/// ## Why this is a module and not two types beside the executor
///
/// The first draft put `Published` next to [`Executor`] and claimed "ack before swap does not
/// compile". It was false, and demonstrably so. The proof was demanded only by the *helper*
/// [`Responder::ack`]; `Receipt::ok` is a public constructor with no proof parameter and
/// `Job.respond` was a public raw `SyncSender<Receipt>`, so `respond.send(Receipt::ok(ack))`
/// compiled anywhere — including inside this file, which is the only place that matters, since the
/// rewrites the token exists to survive (7a's window, 7b's close policy, 9's coupled ack) are all
/// rewrites *of this file*. Rust's privacy is per **module**, so a guard that lives in the same
/// module as the code it guards guards nothing.
///
/// So the sender moves in here and the field is private to this module. Outside it — which is all
/// of the executor — a `Responder` offers exactly two operations, [`Responder::ack`] (needs a
/// [`Published`]) and [`Responder::fail`] (cannot carry an `Ack`). There is no third route to a
/// successful receipt, because there is no way to reach the channel.
///
/// ## What this still does not buy, stated because the last version of this comment overclaimed
///
/// [`Published::by_swap`] and [`Published::already_in_force`] are callable from anywhere in
/// `write.rs`. A worker who *wants* to ack early can still mint a token — the replay path's
/// `already_in_force()` is the obvious thing to reach for, and is exactly what the reviewer's
/// mutation used. Two things catch that rather than the type system: `check-layers.sh` rule 3 pins
/// every `Published::` construction to this file, and there are exactly two ([`Executor::publish`],
/// and the replay arm of [`Executor::admit`]); and `ack_follows_fsync_then_swap`'s `BeforeAck` leg
/// fails on engine state — the effect is not in force at the moment the ack is being sent — with no
/// reference to the step log. Type, rule, test: the claim is that no *one* of them is the
/// guarantee.
///
/// **Task 7a weakened this, and the weakening is deliberate.** [`Responder::ack`] takes `&Published`
/// rather than a `Published` by value. Before 7a, N acks needed N tokens, so an ack *loop* had to
/// mint inside itself — at a site with no swap adjacent, which is grep-visible and which a reader of
/// this file would query. Now one token acks unboundedly many waiters, so acking window *k+1*'s
/// waiters with window *k*'s token type-checks. That is the right trade — by-value would have forced
/// exactly the mint-in-a-loop this comment warns about, and `check-layers.sh`'s rule is a *location*
/// rule that would not have seen it — but it is a real reduction in what the type carries, and it is
/// written down here rather than left to be rediscovered. What still holds it: one window swaps
/// once, and [`Executor::close_window`] is the only place a window's waiters are reached.
mod ack {
    use std::sync::mpsc::SyncSender;

    use tessera_lifecycle::command::{Ack, ExecError, Receipt};

    /// Proof that a generation carrying a command's effect is live.
    ///
    /// [`super::Responder::ack`] cannot send a *successful* receipt without one, and the only
    /// producers are the three named constructors below.
    #[must_use = "a Published token exists to be handed to `ack`; dropping it discards the proof"]
    pub(super) struct Published(());

    impl Published {
        /// Produced by the generation swap, and by nothing else on the success path.
        pub(super) fn by_swap() -> Self {
            Published(())
        }

        /// The one case where a success ack is honest without *this* command having swapped: an
        /// idempotent replay of a `batch_id` whose **original** acceptance already swapped
        /// (contracts §3.4's replay rule). The effect is in force; it was simply put there by an
        /// earlier command.
        ///
        /// Takes the recorded ids it is replaying so it cannot be conjured out of nothing at a
        /// site that has looked nothing up — the argument is the evidence, and the borrow makes
        /// "I found this batch already accepted" a precondition of the call rather than a comment
        /// above it.
        pub(super) fn already_in_force(_replay_of: &[tessera_types::EntityId]) -> Self {
            Published(())
        }
    }

    /// Where one submitted `Command`'s [`Receipt`] is delivered.
    ///
    /// A **synchronous** channel sender, and that is forced rather than chosen: `tessera-engine`
    /// has no tokio dependency and must not acquire one, so the plan's two options for "receipt
    /// awaiting must not block the reactor" collapse to one — the handler wraps its submit in
    /// `spawn_blocking`, and this stays a plain `std::sync::mpsc` sender. `sync_channel(1)`, not
    /// `channel()`, so the executor's ack send never blocks on a caller that has gone away.
    pub(crate) struct Responder(SyncSender<Receipt>);

    impl Responder {
        pub(super) fn new(tx: SyncSender<Receipt>) -> Self {
            Responder(tx)
        }

        /// Send a **successful** receipt. Requires proof that the effect is live.
        ///
        /// A dropped receiver is not an error: the caller's connection went away, and by then the
        /// effect is already in force.
        ///
        /// **By reference, since Task 7a** — one generation swap now acknowledges N waiters, and
        /// `Published` is deliberately neither `Clone` nor constructible outside `write.rs`. Taking
        /// it by value would have forced the ack loop to mint a token per waiter, which is precisely
        /// the residual hole this module's doc names: a `Published::by_swap()` at a site with no
        /// swap adjacent, which `check-layers.sh` rule 3 (a *location* rule) would not see. A borrow
        /// keeps "you must hold proof" as a precondition of the call while letting one swap answer
        /// the window it published.
        pub(super) fn ack(&self, ack: Ack, _proof: &Published) {
            let _ = self.0.send(Receipt::ok(ack));
        }

        /// Send a failure receipt. No proof, because there is no effect to prove — and no way to
        /// smuggle an [`Ack`] through it.
        pub(super) fn fail(&self, error: ExecError) {
            let _ = self.0.send(Receipt::failed(error));
        }
    }
}

use ack::{Published, Responder};

// =================================================================================================
// Live state
// =================================================================================================

/// Take a lock, recovering rather than panicking if a previous holder panicked.
///
/// **Why not `.unwrap()`.** The only writer of these maps is the executor thread. If it panics
/// mid-apply, `unwrap()` would poison every one of them, and the *next* `/v1/items` drill-down —
/// an unrelated read on an unrelated request — would panic inside `spawn_blocking` and become a
/// 500. One writer fault would silently become a total read-plane outage.
///
/// Recovering is safe here and is not a shrug: the executor's death is **already** reported
/// fail-closed by [`ExecutorPosture::Dead`], so the node stops being routed traffic through the
/// front door rather than through a panic storm; each map insert is individually complete, so the
/// recovered state is a prefix of a batch rather than a torn value; and in stage 2.1 buffered items
/// have no row geometry at all (no flush until 2.2), so a partial prefix contributes to no
/// viewport, count or density. The WAL, not these maps, is the durable record either way.
fn lock_recover<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The `/control/ingest` idempotency index: batch id -> `(body hash, the ids its rows were given)`.
/// The ids ride along so a byte-identical replay answers with the same `tessera_id`s without
/// re-deriving them from `external_id` — which a null-external-id row has none of.
type AcceptedBatches = FxHashMap<String, ([u8; 32], Vec<EntityId>)>;

/// The descriptor resolver's detached extension state: interned novel descriptors, plus the next
/// extension id to hand out.
type ResolverState = (FxHashMap<Vec<u8>, TermId>, u32);

/// State the handler side reads and the executor thread writes.
///
/// The seam is drawn by **who writes**, which is why every field still carries a lock: each is read
/// concurrently by a request path that is not the executor (`Engine::resolve_external_id`,
/// `Engine::external_id_of`, `/control/ingest`'s replay check, `/control/status`'s high-water).
/// The generation pointer is deliberately **not** here — see this module's doc.
pub(crate) struct LiveState {
    /// The I9 allocator. Written only by the executor now: allocation moved off the handler at
    /// Task 3a (per command) and widens to per window at 7a, without this type changing.
    allocator: Mutex<Allocator>,
    established: Mutex<FxHashMap<Vec<u8>, EntityId>>,
    established_inverse: Mutex<FxHashMap<EntityId, Vec<u8>>>,
    /// The descriptor resolver's extension state. Written from **both** sides, which is correct and
    /// is the one asymmetry in this type: ingest resolves in the handler *before* submitting
    /// (signature-sorted assignment needs the term set to compute a sort key before any ID exists —
    /// the structural exception argued at [`WritePath::resolve_terms`]), while a change resolves on
    /// the executor *after* its own append has been fsynced.
    resolver_state: Mutex<ResolverState>,
    accepted_batches: Mutex<AcceptedBatches>,
    dict: Arc<Dict>,
}

impl LiveState {
    fn established_entity(&self, external_id: &[u8]) -> Option<EntityId> {
        lock_recover(&self.established).get(external_id).copied()
    }

    fn established_entities(&self, external_ids: &[Vec<u8>]) -> Vec<Option<EntityId>> {
        let established = lock_recover(&self.established);
        external_ids
            .iter()
            .map(|id| established.get(id.as_slice()).copied())
            .collect()
    }

    fn established_external_id(&self, entity: EntityId) -> Option<Vec<u8>> {
        lock_recover(&self.established_inverse)
            .get(&entity)
            .cloned()
    }

    fn allocator_high_water(&self) -> u64 {
        lock_recover(&self.allocator).high_water()
    }

    fn accepted_batch(&self, batch_id: &str) -> Option<([u8; 32], Vec<EntityId>)> {
        lock_recover(&self.accepted_batches).get(batch_id).cloned()
    }

    fn resolve_terms(&self, descriptors: &[Descriptor]) -> Vec<TermId> {
        let mut state = lock_recover(&self.resolver_state);
        let (extension, next_extension_id) = std::mem::take(&mut *state);
        let mut resolver = DescriptorResolver::resume(&self.dict, extension, next_extension_id);
        let ids = descriptors.iter().map(|d| resolver.resolve(d)).collect();
        *state = resolver.into_state();
        ids
    }

    /// How many of `rows` name an external id the live map already holds.
    ///
    /// **The backstop for a race the executor created** (Task 3a security review, C1). The
    /// handler's own duplicate check (`control.rs`) reads this same map, but `established` is
    /// written at *apply* time — which used to be microseconds later, under the WAL mutex, and is
    /// now a whole queue drain later. A client retry under a **fresh** `batch_id` (the case
    /// `control.rs` itself calls most likely) therefore passes the handler check twice, gets two
    /// entity ids for one external id, and the second `insert` overwrites the first. A later
    /// `suppress` resolves to the second only: the first stays visible, is a byte-identical copy of
    /// a suppressed document, and is nameable by **no external id at all** — so no deny can ever
    /// reach it.
    ///
    /// Checked here, on the one thread that also performs the insert, so check and apply cannot be
    /// separated. Only the live map needs re-checking: the bundle's sidecar is immutable, so the
    /// handler's bundle-side check cannot go stale.
    ///
    /// Returns a **count**, never the ids: this value reaches a 409 body, and an external id is
    /// caller data that `error.rs`'s standing rule keeps out of response bodies. The handler's own
    /// check is the one that names them, to a caller who supplied them.
    fn established_collisions(&self, rows: &[UnallocatedRow]) -> usize {
        let established = lock_recover(&self.established);
        rows.iter()
            .filter_map(|r| r.external_id.as_ref())
            .filter(|id| established.contains_key(id.as_slice()))
            .count()
    }

    /// Run `f` with the I9 allocator held. The window's whole allocation is one call to this, so
    /// the sorted run and the high-water advance cannot be separated.
    fn with_allocator<R>(&self, f: impl FnOnce(&mut Allocator) -> R) -> R {
        let mut alloc = lock_recover(&self.allocator);
        f(&mut alloc)
    }

    fn record_accepted_batch(&self, batch_id: String, body_hash: [u8; 32], ids: Vec<EntityId>) {
        lock_recover(&self.accepted_batches).insert(batch_id, (body_hash, ids));
    }
}

// =================================================================================================
// The handler side
// =================================================================================================

/// The write path as a request handler sees it: live state to consult, and a queue to submit to.
pub(crate) struct WritePath {
    live: Arc<LiveState>,
    /// The WAL, from `Engine::open` until [`WritePath::start_executor`] moves it onto the thread.
    /// A plain `Option`, not a `Mutex<Option<..>>`: starting the executor takes `&mut self`, so
    /// there is no shared-access problem to solve, and after the take this field is permanently
    /// `None` — the WAL genuinely leaves the request path rather than merely becoming uncontended.
    wal: Option<Wal>,
    /// The **sole** owner of the two queue senders. Deliberately not handed out and
    /// [`LifecycleHandle`] is deliberately not `Clone`: [`WritePath::drop`] must be able to
    /// disconnect the channels and then join, and an outstanding clone anywhere would make that
    /// join hang forever.
    handle: Option<LifecycleHandle>,
    join: Option<std::thread::JoinHandle<()>>,
    health: Arc<ExecutorHealth>,
    #[cfg(feature = "fault-injection")]
    faults: Option<Arc<tessera_lifecycle::faults::FaultSwitchboard>>,
}

/// Why an executor could not be started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutorStartError {
    /// This engine already has one. The WAL can be owned once.
    AlreadyStarted,
    /// The OS refused the thread (`EAGAIN`: thread or memory limits).
    ///
    /// Its own variant rather than an `expect`, because the panic it replaces would have fired
    /// **after** the WAL was taken out of the request path and could reach the caller as a startup
    /// abort with no posture to read. As a returned error, `tessera-server`'s `prepare` fails
    /// startup deliberately and the posture stays [`ExecutorPosture::NotStarted`]. The engine is
    /// permanently writer-less either way: the WAL moved into the closure that failed to spawn and
    /// was dropped with it, so a retry answers `AlreadyStarted`. Restart the process.
    Spawn(std::io::ErrorKind),
}

impl std::fmt::Display for ExecutorStartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecutorStartError::AlreadyStarted => {
                write!(f, "this engine's write executor is already running")
            }
            ExecutorStartError::Spawn(kind) => write!(
                f,
                "the lifecycle thread could not be spawned ({kind:?}); this engine can no longer \
                 accept writes"
            ),
        }
    }
}

impl std::error::Error for ExecutorStartError {}

/// Why an accepted write did not succeed: it was never handed to the executor
/// ([`SubmitError`]), or it failed while executing ([`ExecError`]).
///
/// The four outcomes must stay distinguishable all the way to the HTTP boundary, and this is the
/// type `accept_ingest`/`accept_change` return, so it is the first place a caller meets them:
///
/// - `Submit(QueueFull)` — 429, the caller should retry;
/// - `Submit(ExecutorDead)` — 503 `not-ready`; non-enqueue is **proven**, so "nothing happened" is
///   true;
/// - `Submit(ReceiptLost)` — **500, never 503.** The executor died *holding* the command, which may
///   be fully applied and swapped in. Reporting it as not-ready is the fail-open the Task 3b design
///   gate's unanimous CRITICAL was about, and it is missing from this list no longer;
/// - `Exec(Wal)` on a `Delete`/`Suppress` — a **500 for an effect that is nonetheless in force**.
///
/// `tessera-server`'s `map_accept_error` owns the table; `map_change_batch_error` folds it for a
/// batch.
///
/// **Deliberately not `#[non_exhaustive]`, and that absence is load-bearing.** Neither this enum
/// nor [`SubmitError`]/[`ExecError`] carries the attribute, which is the only reason adding a
/// variant to any of them is an `E0004` at every cross-crate match — including the mapping table
/// above, which has no `_` arm precisely so a new outcome cannot become a silent 500. Adding
/// `#[non_exhaustive]` later looks like ordinary API hygiene for a `pub` enum and would convert
/// every one of those compile errors into a permitted wildcard.
#[derive(Debug)]
pub enum AcceptError {
    Submit(SubmitError),
    Exec(ExecError),
}

impl std::fmt::Display for AcceptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AcceptError::Submit(e) => write!(f, "{e}"),
            AcceptError::Exec(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for AcceptError {}

impl From<SubmitError> for AcceptError {
    fn from(e: SubmitError) -> Self {
        AcceptError::Submit(e)
    }
}

/// Everything [`WritePath::reconstruct`] rebuilds from durable state.
pub(crate) struct WritePathState {
    wal: Wal,
    allocator: Allocator,
    established: FxHashMap<Vec<u8>, EntityId>,
    established_inverse: FxHashMap<EntityId, Vec<u8>>,
    resolver_state: ResolverState,
    accepted_batches: AcceptedBatches,
}

impl WritePath {
    /// Rebuild every piece of write-side state that comes from durable storage: open and replay
    /// the WAL, seed the I9 allocator at `max(manifest high-water, WAL high-water)`, build the
    /// live external-id map and its inverse, detach the descriptor resolver's extension state, and
    /// rebuild the `/control/ingest` idempotency index from the replayed `IngestBatch` records.
    ///
    /// Returns the first generation's `(overlay, buffer)` alongside the write-path state, because
    /// replay produces all four in one pass and the caller needs the first two to build the
    /// `Generation` the executor will then publish through.
    pub(crate) fn reconstruct(
        wal_path: &Path,
        manifest_high_water: u64,
        dict: &Dict,
        resolve_from_bundle: impl Fn(&[u8]) -> std::result::Result<Option<EntityId>, StoreError>,
    ) -> Result<(Overlay, IngestBuffer, WritePathState), EngineError> {
        let (wal, records) = Wal::open(wal_path).map_err(EngineError::Wal)?;

        let high_water = manifest_high_water.max(high_water_from(&records));
        // `try_new`, not `new`: the seed comes from durable state this process did not write in
        // this run, so a corrupt or hand-edited value at or above `u32::MAX` must be refused
        // **here**, before any ingest, rather than surfacing later as an opaque exhaustion error.
        let allocator = Allocator::try_new(high_water).map_err(|e| {
            EngineError::Malformed(format!(
                "entity-ID allocator seed from durable state (MANIFEST high-water {}, WAL \
                 high-water {}): {e}",
                manifest_high_water,
                high_water_from(&records),
            ))
        })?;

        let (overlay, buffer, established, resolver) =
            replay(&records, dict, resolve_from_bundle).map_err(EngineError::Overlay)?;

        let established_inverse: FxHashMap<EntityId, Vec<u8>> = established
            .iter()
            .map(|(ext, ent)| (*ent, ext.clone()))
            .collect();
        let resolver_state = resolver.into_state();

        let mut accepted_batches: AcceptedBatches = FxHashMap::default();
        for record in &records {
            if let WalRecord::IngestBatch {
                batch_id,
                body_hash,
                rows,
            } = record
            {
                let entity_ids = rows.iter().map(|row| row.entity_id).collect();
                accepted_batches.insert(batch_id.clone(), (*body_hash, entity_ids));
            }
        }

        Ok((
            overlay,
            buffer,
            WritePathState {
                wal,
                allocator,
                established,
                established_inverse,
                resolver_state,
                accepted_batches,
            },
        ))
    }

    /// Assemble the write path. **One call**, deliberately: two tracks both edit `Engine::open`,
    /// and a one-line construction site conflicts trivially where a twenty-line one does not.
    ///
    /// No executor is running yet, and no generation pointer is held here. Both arrive at
    /// [`WritePath::start_executor`].
    pub(crate) fn new(state: WritePathState, dict: Arc<Dict>) -> Self {
        WritePath {
            live: Arc::new(LiveState {
                allocator: Mutex::new(state.allocator),
                established: Mutex::new(state.established),
                established_inverse: Mutex::new(state.established_inverse),
                resolver_state: Mutex::new(state.resolver_state),
                accepted_batches: Mutex::new(state.accepted_batches),
                dict,
            }),
            wal: Some(state.wal),
            handle: None,
            join: None,
            health: Arc::new(ExecutorHealth::new()),
            #[cfg(feature = "fault-injection")]
            faults: None,
        }
    }

    /// Move the WAL onto a dedicated thread and open the two queues.
    ///
    /// `&mut self` rather than a lock: every caller holds the `Engine` by value before sharing it
    /// (`tessera-server`'s `prepare`, the bench arms, the tests), so single ownership of the WAL is
    /// enforced by the borrow checker instead of by a runtime `take`.
    pub(crate) fn start_executor(
        &mut self,
        generation: Arc<GenerationHandle>,
        queue_bound: usize,
        #[cfg(feature = "fault-injection")] faults: Option<
            Arc<tessera_lifecycle::faults::FaultSwitchboard>,
        >,
    ) -> Result<(), ExecutorStartError> {
        let wal = self.wal.take().ok_or(ExecutorStartError::AlreadyStarted)?;

        let (work_tx, work_rx) = std::sync::mpsc::sync_channel(queue_bound);
        let (deny_tx, deny_rx) = std::sync::mpsc::channel();
        // Capacity one, and `try_send` that discards `Full`: a token means "something may be
        // waiting", and a second token while one is pending says nothing new. The executor only
        // ever blocks on this having **observed both queues empty**, which is what makes discarding
        // safe — see [`Executor::run`].
        let (bell_tx, bell_rx) = std::sync::mpsc::sync_channel(1);

        // A clone, not a move: `ExecutorHealth` keeps the other end, which is what gives the
        // counters a reader outside the executor thread (`ExecutorStats::wal_fsyncs`).
        let meter = Arc::clone(&self.health.wal);
        #[cfg(not(feature = "fault-injection"))]
        let exec_wal = ExecutorWal::new(wal, meter);
        #[cfg(feature = "fault-injection")]
        let exec_wal = {
            let w = ExecutorWal::new(wal, meter);
            match &faults {
                Some(f) => w.with_faults(Arc::clone(f)),
                None => w,
            }
        };

        let health = Arc::clone(&self.health);
        let live = Arc::clone(&self.live);
        #[cfg(feature = "fault-injection")]
        let thread_faults = faults.clone();

        let join = std::thread::Builder::new()
            .name("tessera-lifecycle".to_string())
            .spawn(move || {
                let mut executor = Executor {
                    wal: exec_wal,
                    live,
                    generation,
                    queues: LifecycleQueues {
                        work: work_rx,
                        deny: deny_rx,
                        bell: bell_rx,
                    },
                    health: Arc::clone(&health),
                    window_seq: 0,
                    #[cfg(feature = "fault-injection")]
                    faults: thread_faults,
                };
                // Declared LAST so it drops FIRST during unwind: the posture reaches `Dead` before
                // the receivers disconnect, so a **subsequent** submitter cannot see
                // `ExecutorDead` while `readyz` still reports ready.
                //
                // **It does not order the posture against the IN-FLIGHT submitter, and an earlier
                // revision of this comment claimed it did** (found at the Task 3b design gate).
                // `Job { command, respond }` is destructured into `Executor::execute`'s frame, so
                // the in-flight `Responder` drops *earlier* in the unwind than this guard: that
                // caller's `rx.recv()` can return before the posture moves. The consequence that
                // matters is that the caller's error must be `SubmitError::ReceiptLost` — mapped to
                // a fail-closed 500 rather than 503 — which is correct *regardless* of the posture,
                // because the command may be fully applied. `tests/write.rs`'s
                // `an_executor_panic_is_reported_dead` asserts both halves and pins that error at
                // its producer.
                //
                // **This ordering does not make a `/readyz` test a race**, which an earlier revision
                // also claimed: `ExecutorPosture` is published with `fetch_max`, so `Dead` is
                // absorbing and a bounded poll converges — the loop in that same test is one. What
                // stops `tessera-server` writing the socket-level version is that inducing the panic
                // needs `fault-injection` as a dev-dependency there; see `health.rs`'s `is_ready`.
                let _guard = DeathGuard(health);
                executor.run();
            })
            .map_err(|e| ExecutorStartError::Spawn(e.kind()))?;

        // Advanced **after** a successful spawn, not before it: a failed spawn must leave the
        // posture at `NotStarted` (an operator configuration fault — writes refused, reads
        // untouched) rather than at a `Running` no thread is behind. Safe against the thread that
        // panics the instant it starts, because `advance` is `fetch_max` and `Dead` outranks
        // `Running` whichever order the two land in.
        self.health.advance(ExecutorPosture::Running);

        self.handle = Some(LifecycleHandle {
            work: work_tx,
            deny: deny_tx,
            bell: bell_tx,
            health: Arc::clone(&self.health),
        });
        self.join = Some(join);
        #[cfg(feature = "fault-injection")]
        {
            self.faults = faults;
        }
        Ok(())
    }

    pub(crate) fn health(&self) -> &Arc<ExecutorHealth> {
        &self.health
    }

    fn handle(&self) -> Result<&LifecycleHandle, SubmitError> {
        // "Never started" and "died" get the same answer, and `SubmitError::ExecutorDead`'s own
        // doc already states the reason: there is no honest 200 to give when there is nothing left
        // to apply it. Both are a 503 and a not-ready node; the *posture* is where an operator
        // learns which (`ExecutorPosture::NotStarted` vs `Dead`).
        self.handle.as_ref().ok_or(SubmitError::ExecutorDead)
    }

    // --- read accessors, unchanged in behaviour from Phase 1 -------------------------------------

    pub(crate) fn allocator_high_water(&self) -> u64 {
        self.live.allocator_high_water()
    }

    pub(crate) fn established_entity(&self, external_id: &[u8]) -> Option<EntityId> {
        self.live.established_entity(external_id)
    }

    /// Batch form, taking the map's lock **once** for the whole batch — not merely an optimisation:
    /// per-key locking would let an acceptance land between two keys of one duplicate check, so the
    /// batch would be answered from two different snapshots of the live map.
    pub(crate) fn established_entities(&self, external_ids: &[Vec<u8>]) -> Vec<Option<EntityId>> {
        self.live.established_entities(external_ids)
    }

    pub(crate) fn established_external_id(&self, entity: EntityId) -> Option<Vec<u8>> {
        self.live.established_external_id(entity)
    }

    pub(crate) fn accepted_batch(&self, batch_id: &str) -> Option<([u8; 32], Vec<EntityId>)> {
        self.live.accepted_batch(batch_id)
    }

    /// Resolve raw term descriptors to `TermId`s.
    ///
    /// **Durability-ordering exemption.** Ideally every call happens only after the record carrying
    /// its descriptors is fsynced. `/control/changes` honours that (the executor resolves after its
    /// append succeeds). `/control/ingest` is a deliberate, structural exception: signature-sorted
    /// assignment (I9/§11.1) needs each item's resolved terms to compute its sort key *before* any
    /// id exists, so this cannot be deferred past the durability boundary without abandoning
    /// signature-sorted assignment itself. Safe in practice, not merely convenient: an extension id
    /// is by construction unsatisfiable by any session's `satisfied` set, so a live/replay mismatch
    /// in *which* extension id a novel descriptor got renumbers internal bookkeeping only, never a
    /// visibility outcome.
    pub(crate) fn resolve_terms(&self, descriptors: &[Descriptor]) -> Vec<TermId> {
        self.live.resolve_terms(descriptors)
    }

    // --- submission -----------------------------------------------------------------------------

    /// Submit an ingest batch and wait for its receipt.
    ///
    /// **Blocking**, so a tokio handler must call this inside `spawn_blocking` — `tessera-engine`
    /// has no tokio dependency and must not acquire one (lifecycle §7's sync-engine rule, policed
    /// by `scripts/check-layers.sh`'s `deny tessera-engine tokio`).
    ///
    /// Rows arrive **unallocated**: entity ids are assigned on the executor, per command here and
    /// per window at Task 7a, and the type does not change between them.
    pub(crate) fn accept_ingest(
        &self,
        rows: Vec<UnallocatedRow>,
        batch_id: String,
        body_hash: [u8; 32],
    ) -> Result<Vec<EntityId>, AcceptError> {
        let receipt = self.handle()?.submit(Command::Ingest {
            rows,
            batch_id,
            body_hash,
        })?;
        match receipt.outcome {
            Ok(Ack::Ingested { entity_ids }) => Ok(entity_ids),
            Ok(Ack::Changed) => unreachable!("an Ingest command answers with Ack::Ingested"),
            Err(e) => Err(AcceptError::Exec(e)),
        }
    }

    /// Submit one `/control/changes` entry and wait for its receipt.
    ///
    /// **Deny-op append failure** (lifecycle §4): if the append/fsync fails and `op` is
    /// `Delete`/`Suppress`, the change is still applied — the item hidden immediately — before this
    /// returns `Err`. Never a refusal that leaves a deny unapplied. So an `Err` here does **not**
    /// mean "nothing happened"; see [`ExecError::Wal`].
    pub(crate) fn accept_change(
        &self,
        external_id: Vec<u8>,
        entity: EntityId,
        op: ChangeOp,
        raw_descriptors: Option<Vec<Vec<u8>>>,
    ) -> Result<(), AcceptError> {
        let receipt = self.handle()?.submit(Command::Change {
            external_id,
            entity,
            op,
            descriptors: raw_descriptors,
        })?;
        match receipt.outcome {
            Ok(_) => Ok(()),
            Err(e) => Err(AcceptError::Exec(e)),
        }
    }
}

impl Drop for WritePath {
    /// Disconnect the queues, then **join**.
    ///
    /// Without this the executor outlives its `Engine` and keeps appending and fsyncing while the
    /// caller's next statement is typically `TempDir::drop` → `remove_dir_all` over the WAL
    /// directory: intermittent `ENOENT` from the sidecar rename, in tests spread across files this
    /// track may not edit. Phase 1's inline path closed the WAL synchronously on drop and nothing
    /// had to be said; moving the WAL onto a thread is what creates the obligation.
    ///
    /// The join is unconditional and cannot hang, because [`LifecycleHandle`] is not `Clone` and
    /// this type is its only owner — dropping it below is guaranteed to disconnect every sender.
    fn drop(&mut self) {
        // A test may have parked the executor at an armed pause point; release it first, or
        // teardown deadlocks on a fault the test forgot to clear.
        #[cfg(feature = "fault-injection")]
        if let Some(faults) = &self.faults {
            faults.release();
        }
        drop(self.handle.take());
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Flips the posture to [`ExecutorPosture::Dead`] however the executor thread ends — a clean
/// shutdown or a panic anywhere in the loop body.
///
/// The two are not distinguished, deliberately: to a caller they mean the same thing, which is that
/// there is nothing left to apply a write to. `NotStarted` earns its own variant because that one
/// is an operator *configuration* fault rather than a runtime one.
struct DeathGuard(Arc<ExecutorHealth>);

impl Drop for DeathGuard {
    fn drop(&mut self) {
        self.0.advance(ExecutorPosture::Dead);
    }
}

// =================================================================================================
// The queues
// =================================================================================================

/// One queued unit of work: what to do, and where to say it was done.
///
/// The responder travels **with** the command rather than being looked up afterwards, because Task
/// 8's join case needs several of them against one entry. It is an [`ack::Responder`], not a raw
/// sender — see that module for why the difference is the whole of the ack-ordering guarantee.
pub(crate) struct Job {
    command: Command,
    respond: Responder,
}

/// The handler-side end of the write executor: two queues, and the asymmetry between them.
///
/// **Not `Clone`, and that is load-bearing** — [`WritePath::drop`] joins the executor thread, which
/// terminates only when every sender has disconnected. One owner means the join always completes.
pub(crate) struct LifecycleHandle {
    /// Bounded by `ingest_queue_bound`; full → [`SubmitError::QueueFull`].
    work: SyncSender<Job>,
    /// Unbounded: a deny is never refused for load.
    deny: Sender<Job>,
    /// Capacity-one wake signal. `std::sync::mpsc` has no select over two receivers, and the two
    /// alternatives were both worse: `crossbeam-channel` is a workspace dependency for one
    /// `select!`, and `recv_timeout` polling would put a latency floor on the one wait the
    /// never-shed lane exists to bound.
    bell: SyncSender<()>,
    health: Arc<ExecutorHealth>,
}

impl LifecycleHandle {
    /// Submit any [`Command`] and block until its receipt arrives.
    ///
    /// **One method, because the lane is chosen by the command and not by the call site.**
    /// `Command::Change` rides the unbounded never-shed queue and can therefore never answer
    /// [`SubmitError::QueueFull`] — contracts §3.1 forbids `/control/changes` answering 429 —
    /// while `Command::Ingest` rides the bounded one and can. There was briefly a `submit_deny`
    /// beside this; its body was byte-identical, so its "Never 429" doc described the *abandoned*
    /// by-call-site rule and was false of itself in both directions. Deleted rather than
    /// documented: a second name for one behaviour is how a stage-2.2 author ends up believing the
    /// lane follows the call.
    ///
    /// Both lanes can still report [`SubmitError::ExecutorDead`]. A deny is never refused for
    /// *load*, which is not the same as never refused; there is no honest 200 to give when there is
    /// nothing left to apply the write to.
    ///
    /// [`Command::is_never_shed`] is the rule, and this is the only place it is consulted.
    pub(crate) fn submit(&self, command: Command) -> std::result::Result<Receipt, SubmitError> {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let job = Job {
            command,
            respond: Responder::new(tx),
        };

        if job.command.is_never_shed() {
            self.deny.send(job).map_err(|_| SubmitError::ExecutorDead)?;
            // Bumped **after** the enqueue and **before** the blocking wait, so a test can observe
            // "the deny is queued" as a condition rather than betting on a sleep. Also the operand
            // Task 6's `retry_after_s` is derived from.
            self.health.deny_submitted.fetch_add(1, Ordering::SeqCst);
        } else {
            self.work.try_send(job).map_err(|e| match e {
                // Task 6: derived, not a placeholder — see [`estimate_retry_after_s`], which also
                // states what makes it an estimator rather than a bound. Both operands are plain
                // atomic loads on a path that must sustain 10⁹-scale ingest.
                TrySendError::Full(_) => {
                    let stats = self.health.stats();
                    SubmitError::QueueFull {
                        retry_after_s: estimate_retry_after_s(
                            stats.work_depth,
                            // Not the raw EWMA — see `ExecutorStats::service_nanos_for_estimate`.
                            // This is the shed path, so the in-flight job is precisely the one the
                            // caller is queued behind.
                            stats.service_nanos_for_estimate(),
                        ),
                    }
                }
                TrySendError::Disconnected(_) => SubmitError::ExecutorDead,
            })?;
            self.health.work_submitted.fetch_add(1, Ordering::SeqCst);
        }

        // Ring **after** the enqueue: a token may be spurious, but it can never be missing.
        // A full bell means one is already pending, which says everything this one would.
        //
        // `ReceiptLost`, not `ExecutorDead`, and the two lines above are why: the job is **already
        // in a queue** by the time the bell is rung, and `Executor::run`'s shutdown pass drains the
        // deny lane and *executes* it before it observes the disconnect. So a dead bell does not
        // prove the command did nothing.
        if let Err(TrySendError::Disconnected(())) = self.bell.try_send(()) {
            return Err(SubmitError::ReceiptLost);
        }

        // A dropped responder means the executor died **holding this job** — never `Ok`. Answering
        // anything else here is the false-202 `SubmitError`'s own doc calls the worst available
        // outcome.
        //
        // `ReceiptLost` because the ack is the **last** step: `append → fsync → apply → swap → ack`
        // (`execute_change`), so a death after the swap leaves a durable, in-force suppression with
        // no receipt. Reporting that as "nothing was submitted" is how an operator comes to believe
        // an item is still visible when it is not — the Task 3b design gate's unanimous CRITICAL.
        rx.recv().map_err(|_| SubmitError::ReceiptLost)
    }
}

/// The executor's end of the queues.
///
/// Named as a pair so the ordering rule is visible from the handle: `deny` is drained to empty
/// before `work` is touched, which is what makes the starvation bound "the work in front of this
/// deny" rather than "the work queue's depth". Since Task 7a that unit is **one commit window**, and
/// since Task 7b that is true of *every* close: [`Executor::run_work_pass`] returns to `run`'s deny
/// drain whenever it closes one — the conflict-forced close was the exception until 7b — which is
/// what keeps the bound finite while ingest keeps arriving. The bound in full, including the one
/// case that costs two closes rather than one, is stated at [`Executor::run_work_pass`].
pub(crate) struct LifecycleQueues {
    work: Receiver<Job>,
    deny: Receiver<Job>,
    /// The wake signal. Capacity one — see [`LifecycleHandle::bell`] and [`Executor::run`].
    bell: Receiver<()>,
}

// =================================================================================================
// The executor
// =================================================================================================

/// What `/control/ingest`'s batch id already means to this executor — **the three states, in the
/// order they are looked up** (Task 8; contracts §3.4, whose Appendix R r8 names the third:
/// *"the batch-id idempotency rule acquires a third state in practice — held but not yet
/// acknowledged — which a retry must join rather than treat as new"*).
///
/// Computed by [`BatchState::of`] on the executor thread, never in a handler. `tessera-lifecycle`
/// deliberately does not know this type: the durable half lives on [`LiveState`], and the window
/// stays a container that knows nothing about idempotency policy.
enum BatchState {
    /// Durably accepted: the WAL record is fsynced, the rows are applied and the ids are recorded.
    /// Same bytes replays these ids; different bytes is a `409`.
    ///
    /// The ids are carried rather than re-derived because **re-deriving is impossible** for a row
    /// that supplied no `external_id`: it is addressable only by its `tessera_id` (contracts §3.4
    /// r6) and appears in no map keyed by anything the retry sends.
    Accepted {
        body_hash: [u8; 32],
        entity_ids: Vec<EntityId>,
    },
    /// **Held but not yet acknowledged**: an entry of the *open* commit window. Its ids exist —
    /// allocation happens at the close — so there is nothing to replay yet; what a byte-identical
    /// retry gets is a place in the queue of waiters that entry will ack.
    ///
    /// `window_seq` identifies the window the entry was found in. In stage 2.1 there is exactly one
    /// open window and it is consulted and joined in the same statement, so this is read by a
    /// `debug_assert!` and nothing else; it is the discriminator a later executor holding more than
    /// one window would need.
    Held {
        window_seq: u64,
        body_hash: [u8; 32],
    },
    /// Never seen. A new entry.
    ///
    /// **This state is reachable for a batch that was in fact accepted, and that is a live
    /// caveat.** `accepted_batches` is rebuilt from WAL replay ([`WritePath::reconstruct`]), so
    /// once a WAL segment is retired an old `batch_id` regresses to `Unknown` and a retry is
    /// re-ingested. Rows carrying an `external_id` are then caught by the duplicate check and the
    /// batch 409s; **rows without one are re-ingested silently as new entities**, leaving a second
    /// copy that no external id names and no deny can reach — Task 3a's C1 shape, arrived at by
    /// retention rather than by a race. Nothing prunes the WAL today (there is **no
    /// `wal_retention` config key** — the absence was recorded as a defect at Task 0b), so the
    /// caveat is latent rather than live; whoever adds retention inherits it, and the bound on
    /// the exposure is the idempotency window an operator's clients actually retry within.
    Unknown,
}

impl BatchState {
    /// Look `batch_id` up: **durable index first, then the open window, then unknown**.
    ///
    /// The two sets are disjoint in this build — a batch id enters `accepted_batches` only at
    /// `close_window`, which consumes the window holding it, and a durably-accepted batch is
    /// refused before it can be pushed — so the order changes no answer today. It is still written
    /// durable-first, because the durable record is the one that survives a restart and an
    /// implementation that preferred the volatile half would answer differently on either side of
    /// one.
    fn of<W>(live: &LiveState, window: &CommitWindow<W>, batch_id: &str) -> BatchState {
        if let Some((body_hash, entity_ids)) = live.accepted_batch(batch_id) {
            return BatchState::Accepted {
                body_hash,
                entity_ids,
            };
        }
        match window.held(batch_id) {
            Some((window_seq, body_hash)) => BatchState::Held {
                window_seq,
                body_hash,
            },
            None => BatchState::Unknown,
        }
    }
}

/// What [`Executor::admit_ingest`] did with a submission, as far as the drain loop needs to know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Admission {
    /// It is in the window (or its waiters).
    Admitted,
    /// It was answered outright — a replay, a join or a 409 — and nothing was added to the window.
    Answered,
    /// A conflicting external id forced the open window to close. The pass must **yield** to
    /// `Executor::run`'s deny drain (lifecycle §1.3; Task 7b's CRITICAL).
    YieldedAfterClose,
}

/// The single writer. One per partition, on its own thread, owning the WAL by value.
struct Executor {
    wal: ExecutorWal,
    live: Arc<LiveState>,
    /// **The only publishing capability in the write path.** Not in [`LiveState`], which the
    /// handler side shares.
    generation: Arc<GenerationHandle>,
    queues: LifecycleQueues,
    health: Arc<ExecutorHealth>,
    /// Task 7a: the last window's sequence number. **Task 8's `BatchState::Held { window_seq, .. }`**
    /// is what it is for; 7a only needs it to be distinct per window.
    window_seq: u64,
    #[cfg(feature = "fault-injection")]
    faults: Option<Arc<tessera_lifecycle::faults::FaultSwitchboard>>,
}

impl Executor {
    /// Drain deny to empty, then execute **at most one** work item, then repeat — blocking only
    /// once both queues have been *observed* empty.
    ///
    /// That last clause is what makes the capacity-one bell safe. If the loop blocked while work
    /// remained, a token discarded as `Full` could be the only wake-up a queued job ever had. As
    /// written, a token is only ever discarded while a job is still visible to the `try_recv`
    /// below, so no job can be left asleep.
    ///
    /// **Deny priority** (lifecycle §1.3): deny is drained to empty at the top of every iteration
    /// and [`Executor::run_work_pass`] returns as soon as it closes a window, so a deny's wait is
    /// bounded by the window (at most two — see that function for the case, and for why the plan's
    /// "≈ 2 × `commit_window_max_age_ms`" describes neither this code nor anything that was built)
    /// in front of it rather than by queue depth. The consequences are chosen: a sustained
    /// deny flood starves ingest completely, and the deny queue is unbounded in memory.
    ///
    /// **Why a deny may safely overtake a queued ingest.** Reordering execution relative to
    /// submission looks like it should break replay equivalence (a live `suppress` replaying before
    /// the ingest that established its target). It cannot: `/control/changes` resolves its
    /// `external_id` against the live map in the *handler* and 404s if the item is not established
    /// yet, and an item is established only at apply. So no deny naming a still-queued ingest's
    /// item can be submitted at all, and WAL append order still equals apply order.
    ///
    /// **Shutdown drains and executes; it does not discard.** The loop leaves only from
    /// `bell.recv()`, which sits *after* both `try_recv`s, so the disconnect iteration has already
    /// drained deny to empty and run one work item. Anything genuinely left behind — work queued
    /// beyond that one item — is dropped with the receivers, and had no waiter left to ack anyway:
    /// a submitter holds `&self` on the handle for the whole call, so `bell.recv()` cannot return
    /// `Err` while any submit is in flight, since the three senders live in one struct and
    /// disconnect together.
    ///
    /// *At-most-one-work-item was Task 3a's shape, not the executor's permanent one*: **Task 7a
    /// drains work into a commit window** ([`Executor::run_work_pass`]). Leftover doorbell tokens
    /// stay harmless under that change, for the reason above — the drain takes every job visible to
    /// its `try_recv`, so a token is still only ever discarded while a job is still visible.
    fn run(&mut self) {
        loop {
            while let Ok(job) = self.queues.deny.try_recv() {
                self.execute(job);
            }
            if self.run_work_pass() {
                continue;
            }
            if self.queues.bell.recv().is_err() {
                break;
            }
        }
    }

    /// **The commit window** (Task 7a; lifecycle §5.1): drain the work queue into one window and
    /// close it. Returns whether anything was done, which is what tells [`Executor::run`] to
    /// re-drain the deny lane rather than block.
    ///
    /// ## The two close triggers, and why only one of them is a policy
    ///
    /// - **The row bound** (`commit_window_max_rows`) — a policy, and the one this task lands.
    ///   Checked **inside** the drain, not after it: every entry pulled frees a bounded-queue slot
    ///   that a concurrent submitter refills at once, so under sustained load the `try_recv` below
    ///   never returns `Err` and "close when the queue is empty" bounds nothing at all. Tripping it
    ///   **returns**, for the same reason read the other way round: a pass that closed and carried
    ///   on draining would not come back here — or to the deny lane — until the load stopped.
    /// - **The work queue observed empty** — *structural, not a policy*. (The **work** queue: the
    ///   deny lane is not consulted here at all.) The alternative is not a different trigger; it is
    ///   a window of un-appended, un-acked ingest surviving `bell.recv()` indefinitely.
    ///
    /// A third close is forced by an entry naming an **external id the window already holds** —
    /// see `CommitWindow::holds_external_id_of`.
    /// That one is a correctness mechanism (it is what keeps Task 3a's security C1 closed across a
    /// window), not a policy; it also yields, for the reason written at the site.
    ///
    /// ## Why there is no age bound, and why the config key is inert (Task 7b)
    ///
    /// The plan gave this task a third trigger, `opened_at.elapsed() >= commit_window_max_age_ms`,
    /// whose stated purpose was to stop a lone ingest on an idle server waiting the full window age
    /// "for company that is not coming". **It was declined, and `ingest.commit_window_max_age_ms`
    /// is inert** (`tessera_server::config::tests::the_commit_window_age_bound_is_inert` fails the
    /// moment anything outside that module reads it).
    ///
    /// An age bound is the safety cap on a **linger** — "having drained the queue empty, wait for
    /// more" — and this executor has no linger. A window is a local of this function and every exit
    /// disposes of it; there is no `CommitWindow` on `Executor` and no path on which one survives
    /// `bell.recv()`. So the interval an age bound would terminate does not exist, and the only
    /// place such a check could fire is *inside* the drain, where it would be a less predictable
    /// spelling of the row bound: the loop's per-entry work is hashing, and the rows it can gather
    /// are bounded by `max_rows` above (worst case `max_rows - 1 + ingest_max_batch_rows`, ≈ 20 000
    /// at the shipped defaults) and, for HTTP submitters, by `ingest_admission` as well — an
    /// admission permit is held to the receipt, and nothing in an open window has been acked.
    ///
    /// **Task 8 qualifies the row bound and the qualification belongs here.** A joined retry
    /// (`admit_ingest`'s `Held` arm) consumes a work-queue slot and adds **zero rows**, so on a
    /// stream of nothing but byte-identical retries the row bound cannot trip and this loop
    /// terminates only on an empty queue. What still bounds it is the structural fact — one entry,
    /// or one joined waiter, per concurrently-blocked submitting thread, since every submitter
    /// blocks on its receipt. That is a bound on *waiters*, not on rows or bytes, and it costs one
    /// `Responder` each; the residency terms Task 6's relation 3 counts are unaffected, because a
    /// join carries no rows into the window. Deny latency is *better* on this path than before,
    /// not worse: one close for N retries where Task 7a took N.
    ///
    /// The interval where the queue momentarily empties while more work is imminent **is** real (a
    /// handler holds its permit across decode, term resolution and sidecar IO before it submits).
    /// But that is a window closing *too early*, and an age bound only ever closes a window
    /// *earlier* — it is the wrong sign. The mechanism that would address it is a linger, which is
    /// declined: it would be paid by every submission, could gather at most the other admitted
    /// handlers, and the sort-scope win it would buy is the one Task 7a's fix round already measured
    /// as order 10¹ runs against the corpus's real signature distribution.
    ///
    /// Lifecycle §5.1 asks for a window "bounded by size **or** age". It is bounded — by size, and
    /// by a drain-empty close that is strictly tighter than any age bound could be.
    ///
    /// ## Deny priority is unchanged
    ///
    /// The deny lane is drained to empty before this is called and again as soon as it returns, and
    /// **stage 2.1's window holds ingest only** (see `tessera_lifecycle::window`): lifecycle §5.1
    /// permits denies to share the window, the plan assigns that to **Task 9**, and it cannot land
    /// before Task 7b's partial-failure split, since a failed mixed window must apply its denies and
    /// drop its ingest.
    ///
    /// So a deny waits at most for the window in front of it — but only because **every close in
    /// this function yields**. The bound is not "the deny lane is drained around this call": this
    /// function is what decides how long "around" is, and while work keeps arriving it decides that
    /// by returning at each close. `a_deny_is_never_queued_behind_work_with_group_commit_disabled`
    /// holds the row-bound path (red the moment that arm loops instead) and
    /// `a_deny_is_never_queued_behind_a_conflict_forced_window_split` holds the conflict path.
    ///
    /// **The honest bound, stated in full** (Task 7b, replacing the plan's "≈ 2 ×
    /// `commit_window_max_age_ms`", whose two premises — an age bound, and denies joining the
    /// window — are both false of this code). A deny waits for the deny entries ahead of it (that
    /// lane is FIFO and unbounded) plus **at most two window closes** — and in stage 2.1 it is
    /// one, because the replacement a conflict opens is closed empty on every path where the first
    /// close succeeded (see the conflict arm). The worst case is two only when the first close
    /// *failed*. What those closes cost is one `assign_sorted` run over the window's
    /// rows, one append per entry, **one fsync** (~3.2 ms measured, ingest baseline memo) and one
    /// `IngestBuffer` clone that is O(total buffered items) — the dominant term, the only one that
    /// grows, and unbounded until stage 2.2's flush (see `Executor::apply_window`). That is why this
    /// is a **starvation** bound and deliberately not a latency target (owner principle 3, Task 7a
    /// brief §0): the window in front may be arbitrarily slow, and nothing here is sized to make it
    /// fast.
    fn run_work_pass(&mut self) -> bool {
        let max_rows = self.health.commit_window_max_rows();
        let mut window: CommitWindow<Responder> = CommitWindow::new(self.next_window_seq());
        let mut did_work = false;

        loop {
            if window.rows() >= max_rows {
                // **Return rather than keep draining.** The drain frees a bounded-queue slot per
                // entry and a concurrent submitter refills it at once, so `try_recv` below never
                // returns `Err` under sustained load: a pass that closed a window and carried on
                // draining would never yield to `Executor::run`'s deny drain for as long as ingest
                // kept arriving, and the deny lane's bound would be the *load*, not the window in
                // front of it (lifecycle §1.3's prohibition is on a deny queued behind work of
                // unbounded duration). Returning costs one `try_recv` per window and restores the
                // bound this function's doc claims.
                // `a_deny_is_never_queued_behind_work_with_group_commit_disabled` is red on
                // `continue` here and green on `return`.
                self.close_window(window);
                return true;
            }
            let Ok(job) = self.queues.work.try_recv() else {
                break;
            };
            let Job { command, respond } = job;
            let Command::Ingest {
                rows,
                batch_id,
                body_hash,
            } = command
            else {
                // Unreachable while the lane follows the command (`Command::is_never_shed`): a
                // `Change` rides the deny queue. Executed rather than dropped, so a future variant
                // that lands here is answered instead of silently losing its waiter.
                //
                // **This arm is order-unsafe as written, and must close the window first if it is
                // ever made reachable.** It applies immediately while a window holding
                // earlier-arriving ingest is still open, so WAL append order stops equalling
                // submission order — and for a deny-shaped variant that is the out-of-order apply
                // lifecycle §4 is written against. Do not make it reachable to save a close.
                self.execute(Job { command, respond });
                // This job was counted at submission on the work lane and `execute` counts nothing,
                // so it is counted here or `work_depth` drifts up one per occurrence forever — the
                // drift `note_work_refused` exists to prevent.
                self.health.note_work_refused();
                did_work = true;
                continue;
            };

            let admitted;
            (window, admitted) = self.admit_ingest(window, rows, batch_id, body_hash, respond);
            did_work = true;
            if admitted == Admission::YieldedAfterClose {
                break;
            }
        }

        if !window.is_empty() {
            self.close_window(window);
            did_work = true;
        }
        did_work
    }

    /// Close `window` and return its replacement.
    ///
    /// **One function so that the ordering is not a statement order two edits apart** (Task 7a gate
    /// F3, made structural by Task 7b). `CommitWindow::new` stamps `opened_at`, and the close it
    /// would otherwise be stamped ahead of is the *previous* window's append, fsync, apply, swap and
    /// acks. Stamped first, a replacement charges its predecessor's whole service to itself —
    /// `record_window_service` doubles, and with it the `retry_after_s` a shed client is told. The
    /// two lines below must stay in this order, and this doc is the only warning a future editor
    /// gets, because **the defect has no observable consequence and Task 8 did not change that** —
    /// it narrowed it further. The enumeration, which is the whole of the argument:
    ///
    /// 1. This is the only construction site of a replacement window, and its only caller is the
    ///    external-id conflict arm of [`Executor::admit_ingest`].
    /// 2. That arm returns [`Admission::YieldedAfterClose`] and the drain loop **breaks in the same
    ///    iteration** (Task 7b's CRITICAL fix), so no *later* entry can ever enter a replacement.
    ///    The only candidate is the conflicting entry itself.
    /// 3. And that entry is refused: `apply_window` inserts its predecessor's external ids into
    ///    `established` before this function returns, so `established_collisions` sees them.
    ///
    /// Task 8 removed the *other* route into this function — a held `batch_id` no longer forces a
    /// close, it joins or 409s in place — so the replacement is reached less often than before, not
    /// more. The two remaining exceptions, both of which leave the ordering unobservable anyway:
    /// a close that **failed** (its `fail_window_wal`/`fail_window_alloc` paths record no accepted
    /// batch and establish nothing, so the entry *is* admitted into the replacement — but both
    /// return before the `AfterFsync` pause point and `ack_failed` carries no pause point, so
    /// nothing can park inside the mis-stamped interval to measure it, and the node's WAL is
    /// poisoned by then); and a 64-bit `digest` collision on an external id, which is not
    /// constructible.
    ///
    /// Task 7b's report handed this test to Task 8 on the expectation that the join would make a
    /// replacement window hold an entry. It does the opposite. Recorded here rather than left as a
    /// promissory note, because a promissory note reads as coverage.
    fn close_and_reopen(&mut self, window: CommitWindow<Responder>) -> CommitWindow<Responder> {
        self.close_window(window);
        CommitWindow::new(self.next_window_seq())
    }

    fn next_window_seq(&mut self) -> u64 {
        self.window_seq += 1;
        self.window_seq
    }

    /// **The batch-id state machine, evaluated on the executor** (Task 8; contracts §3.4 and its
    /// Appendix R r8, lifecycle §5.1's "idempotency across a held window").
    ///
    /// Takes the open window by value and hands it back, possibly replaced. **By value
    /// deliberately**: a `&mut` signature would force a `mem::replace` on the conflict path, which
    /// constructs the replacement *before* the close it replaces — Task 7a gate F3 exactly, see
    /// [`Executor::close_and_reopen`].
    ///
    /// Lookup order is **durable index → open window → unknown**, and the whole reason it runs here
    /// rather than in the handler is that the two are not the same question at two different times:
    /// between a handler check and the enqueue the window can close, so a retry that saw *unknown*
    /// and then enqueued into a fresh window would have double-allocated. `control.rs` keeps its
    /// pre-submit check as the early, well-messaged path; it is advisory and this is the decision.
    ///
    /// | State | What happens |
    /// |---|---|
    /// | [`BatchState::Unknown`] | the ordinary path: external-id conflict check against the window, then `established_collisions`, then a new entry |
    /// | [`BatchState::Accepted`], same bytes | the recorded ids are replayed — re-deriving them is *impossible* for a row that supplied no external id |
    /// | [`BatchState::Accepted`], different bytes | `409`, no effect (Task 3a's behaviour, unmoved) |
    /// | [`BatchState::Held`], same bytes | **join**: the caller's responder is appended to the held entry, and both receive the same ids off one allocation |
    /// | [`BatchState::Held`], different bytes | `409` **to the retry only** — see the arm |
    fn admit_ingest(
        &mut self,
        mut window: CommitWindow<Responder>,
        rows: Vec<UnallocatedRow>,
        batch_id: String,
        body_hash: [u8; 32],
        respond: Responder,
    ) -> (CommitWindow<Responder>, Admission) {
        match BatchState::of(&self.live, &window, &batch_id) {
            BatchState::Accepted {
                body_hash: prev_hash,
                entity_ids,
            } => {
                if prev_hash == body_hash {
                    let proof = Published::already_in_force(&entity_ids);
                    self.ack(&respond, Ack::Ingested { entity_ids }, &proof);
                } else {
                    self.ack_failed(&respond, ExecError::BatchConflict { batch_id });
                }
                self.health.note_work_refused();
                (window, Admission::Answered)
            }
            BatchState::Held {
                window_seq,
                body_hash: prev_hash,
            } => {
                debug_assert_eq!(
                    window_seq,
                    window.seq(),
                    "the entry must be joined to the window it was found in"
                );
                if prev_hash == body_hash {
                    let joined = window.join(&batch_id, respond);
                    debug_assert!(joined, "`held` just answered for this batch id");
                } else {
                    // **The 409 reaches the retry and NOT the held original — an owner-confirmable
                    // default, and this is the one site that decides it** (Task 8 brief §3; the
                    // controller has escalated the question).
                    //
                    // The reading taken. Contracts §3.4's "the batch has no effect" is attached to
                    // the *duplicate-external-id* 409 and means the refused submission, not some
                    // other batch; Appendix R r8 and lifecycle §5.1 then describe the held state
                    // and say only that a retry must **join** rather than treat the batch as new —
                    // neither gives a retry the power to cancel an accepted batch. And the
                    // consequences run one way: the original was accepted, its waiters are blocked
                    // on the acknowledgement it is owed, and discarding it because a *different*
                    // submission arrived with different bytes breaks the durability promise for a
                    // caller who did nothing wrong — while handing any client that can guess a
                    // batch id a cancellation primitive for someone else's in-flight write.
                    //
                    // **If the owner rules the other way**, the change is here and in
                    // `tessera-lifecycle`: mark the held entry discarded (a `bool` on `WindowEntry`,
                    // skipped by `CommitWindow::allocate` and by `held`) and fail its waiters. Not
                    // an entry *removal* — `by_batch` stores indices into `entries` and the
                    // external-id set has no refcounts, so removing one entry means repairing both.
                    self.ack_failed(&respond, ExecError::BatchConflict { batch_id });
                }
                // Either way this job occupied a work-queue slot and was counted at submission,
                // while `record_window_service` counts one completion per *entry* and a join adds
                // no entry. Without this, `work_depth` drifts up by one per retry forever and every
                // 429's `retry_after_s` inherits the drift (Task 7a's mutation 7, in a new place).
                self.health.note_work_refused();
                (window, Admission::Answered)
            }
            BatchState::Unknown => {
                let mut admission = Admission::Admitted;
                // A conflicting entry closes the window **first**, and is then evaluated against
                // the state that close just published — which is what makes the checks below the
                // same checks Task 3a wrote, with the same answers. **Only external ids reach
                // here**: a held batch id was answered above, without closing anything.
                if window.holds_external_id_of(&rows) {
                    window = self.close_and_reopen(window);
                    // **And yield once this entry is handled** (Task 7b). This close is a full
                    // `append → fsync → apply → swap → ack` inside the drain loop, and
                    // `window.rows()` resets with the replacement — so the row bound can never trip
                    // on a conflict-heavy stream and, before this line, a pass could close
                    // unboundedly many windows without ever returning to `Executor::run`'s deny
                    // drain. That is the Task 7a F1 defect exactly (`continue` where the code's own
                    // docs claimed a return), on the one close path F1's fix did not reach, and it
                    // is what lifecycle §1.3 forbids verbatim: a deny queued behind work of
                    // unbounded duration. Reachable at the shipped defaults from a client
                    // re-ingesting an `external_id` that a still-open window already holds.
                    //
                    // The entry is handled first rather than yielding here, because it has already
                    // been taken off the queue and its waiter must be answered.
                    // `a_deny_is_never_queued_behind_a_conflict_forced_window_split` is the leg
                    // that holds it, and it drives this path — **not** the batch-id one, which
                    // Task 8 removed.
                    admission = Admission::YieldedAfterClose;
                }
                if let Some(entry) = self.admit(rows, batch_id, body_hash, respond) {
                    if window.is_empty() {
                        // The in-flight gauge is armed at the **first entry**, never at window
                        // construction: an empty window is never closed, so a gauge armed there
                        // would never be cleared and `service_nanos_for_estimate` would grow
                        // without bound on an idle node (Task 6's F7, inverted).
                        self.health.mark_work_started(window.opened_at());
                    }
                    window.push(entry);
                }
                (window, admission)
            }
        }
    }

    /// The external-id admission check, unchanged from Task 3a's per-command version and still on
    /// the one thread that also performs the inserts. `None` means the caller has already been
    /// answered.
    ///
    /// The batch-id half moved to [`Executor::admit_ingest`] when Task 8 gave it a third state; the
    /// check itself is the same one and answers the same way.
    ///
    /// This check reads state written at **apply**, which is why an entry naming an external id the
    /// *open window* holds must close it before reaching here (see the caller).
    fn admit(
        &mut self,
        rows: Vec<UnallocatedRow>,
        batch_id: String,
        body_hash: [u8; 32],
        respond: Responder,
    ) -> Option<WindowEntry<Responder>> {
        // The fail-closed backstop for the widened check-to-apply race — see
        // `LiveState::established_collisions`.
        let collisions = self.live.established_collisions(&rows);
        if collisions > 0 {
            self.ack_failed(
                &respond,
                ExecError::DuplicateExternalId { count: collisions },
            );
            self.health.note_work_refused();
            return None;
        }

        Some(WindowEntry {
            rows,
            batch_id,
            body_hash,
            waiters: vec![respond],
        })
    }

    /// **Close a commit window**: one signature-sorted allocation run, one WAL record per entry, one
    /// fsync, one generation swap, then every waiter is acked.
    ///
    /// ## I9 is untouched
    ///
    /// "The window allocates" reads like an allocator change and is not one (lifecycle §5.1). IDs
    /// are still issued monotonically from the high-water by one `Allocator::allocate`, still never
    /// reused, still ordered by `(signature, external_id)` through the unchanged `assign_sorted`.
    /// What widens is the **input set**: design §11.1's sort scope becomes the window, at the
    /// server, instead of whatever chunk a client happened to POST.
    ///
    /// **A failed window burns entity ids**, exactly as a failed batch did: assignment precedes the
    /// append, so ids given to a window whose append then fails are never issued again. I9-safe —
    /// ids stay strictly monotone and each is issued once.
    ///
    /// ## The failure rule
    ///
    /// An append or fsync failure **applies nothing**, in deliberate contrast to the deny path:
    /// applying un-fsynced ingest would make items appear and vanish across a crash, and lifecycle
    /// §4's apply-anyway rule is written for `Delete`/`Suppress` only. Task 7b's rule 2 restates it
    /// per window; the window here is ingest-only, so the rule is uniform over it and 7b's
    /// *split* — denies apply, ingest does not — has no subject until Task 9 puts denies in the
    /// window.
    ///
    /// *One pre-existing property this widens and does not fix:* `wal::replay` replays every
    /// well-framed record, including records written **past** the last-fsynced offset — it does not
    /// truncate to the sync point. So an un-fsynced, un-acked ingest record whose bytes reached the
    /// file is reinstated on restart. That is already true per command (append succeeds, fsync
    /// fails, caller gets 500, nothing is applied, replay applies it); a window makes it k records
    /// instead of one.
    ///
    /// **Task 8 owned this and is handing it back with the disposition, not a test.** Task 8's
    /// `crash_between_fsync_and_swap_replays_rather_than_reallocates` covers the *fsynced* half —
    /// a real killed process between fsync and swap — which is lifecycle §8's crash row and the
    /// case a client can retry into. The un-fsynced half is a different property and is **not**
    /// under test, but it is also not the C1 shape it looks like: `Wal::fsync` poisons the handle
    /// on failure and `Wal::append` refuses every later append, so exactly one record for that
    /// batch reaches the file and replay establishes one entity, not two. What a restart actually
    /// produces is an item the caller was told (correctly, at the time) it did not have — visible,
    /// named by its own `external_id`, and therefore reachable by a deny. Fail-closed; recorded so
    /// the next reader does not spend the effort re-deriving it.
    fn close_window(&mut self, window: CommitWindow<Responder>) {
        let entries = window.len() as u64;
        let started = window.opened_at();

        let closed = match self.live.with_allocator(|a| window.allocate(a)) {
            Ok(closed) => closed,
            Err((e, waiters)) => {
                // `allocate` leaves the high-water mark unchanged on this path, so the window has no
                // effect at all — the same statement `ExecError::Alloc` already makes per batch.
                self.fail_window_alloc(e, waiters, entries, started);
                return;
            }
        };

        // One record per entry — batch identity is preserved through the window (Task 8 joins on
        // it) — appended in entries order, which is also apply order (Task 7b rule 1).
        let mut failed_at: Option<(usize, WalError)> = None;
        for (i, entry) in closed.iter().enumerate() {
            if let Err(e) = self.wal.append(&entry.record) {
                failed_at = Some((i, e));
                break;
            }
        }
        // **One fsync for the whole window.** This is the amortisation half of group commit; the
        // allocation scope above is the point of it.
        if failed_at.is_none() {
            if let Err(e) = self.wal.fsync() {
                // Every entry appended cleanly, so there is no "the entry whose append failed" here
                // — the first waiter gets the real error arbitrarily and the rest `Poisoned`. No
                // wire behaviour distinguishes them (`map_accept_error` folds `ExecError::Wal(_)`
                // variant-blind to 500); the distinction is for whoever reads the two messages.
                failed_at = Some((0, e));
            }
        }
        self.observe_wal();
        if let Some((index, error)) = failed_at {
            self.fail_window_wal(closed, index, error, entries, started);
            return;
        }

        // Durable, not yet in force. See `pause_point`.
        self.pause_point(PauseSiteArg::AfterFsync);
        // One buffer clone, one generation, **one swap** for every entry in the window.
        let mut closed = closed;
        let published = self.apply_window(&mut closed);

        // Recorded after the swap, so a concurrent replay of a batch id can never observe a window
        // where the generation has swapped but the idempotency index has not caught up.
        for entry in &closed {
            let (batch_id, body_hash) = entry.batch_key();
            self.live.record_accepted_batch(
                batch_id.to_string(),
                body_hash,
                entry.entity_ids.clone(),
            );
        }
        self.observe_wal();

        // **N waiters, one proof.** A death partway through this loop leaves some waiters acked and
        // some not; every un-acked one gets `SubmitError::ReceiptLost` → 500, never `ExecutorDead`
        // → 503, because its ingest is durably in force. That is the widening `ReceiptLost`'s own
        // doc predicts for this task.
        for entry in closed {
            let ClosedEntry {
                entity_ids,
                mut waiters,
                ..
            } = entry;
            // The last waiter takes the ids; **Task 8's join is what puts a second one here**, and
            // it clones. Popping rather than an `Option` dance: `waiters` is never empty (an entry
            // is built with one), and a `Vec<EntityId>` per entry is up to `ingest_max_batch_rows`
            // long, so cloning it unconditionally would be a real per-row cost for the common case
            // of one waiter. The clone is per *retry*, not per row of the original.
            let last = waiters
                .pop()
                .expect("an entry always has at least one waiter");
            for waiter in waiters {
                let entity_ids = entity_ids.clone();
                self.ack(&waiter, Ack::Ingested { entity_ids }, &published);
            }
            self.ack(&last, Ack::Ingested { entity_ids }, &published);
        }

        self.health
            .record_window_service(entries, started.elapsed().as_nanos() as u64);
    }

    /// The window could not be allocated: nothing was appended, nothing applied, and the high-water
    /// mark did not move. `AllocError` is `Copy`, so every waiter gets the real one.
    fn fail_window_alloc(
        &self,
        error: AllocError,
        waiters: Vec<Vec<Responder>>,
        entries: u64,
        started: std::time::Instant,
    ) {
        for entry in waiters {
            for waiter in entry {
                self.ack_failed(&waiter, ExecError::Alloc(error));
            }
        }
        self.health
            .record_window_service(entries, started.elapsed().as_nanos() as u64);
    }

    /// The window's append or fsync failed: **apply nothing**, and answer every waiter.
    fn fail_window_wal(
        &self,
        closed: Vec<ClosedEntry<Responder>>,
        index: usize,
        error: WalError,
        entries: u64,
        started: std::time::Instant,
    ) {
        let mut real = Some(error);
        for (i, entry) in closed.into_iter().enumerate() {
            for (k, waiter) in entry.waiters.into_iter().enumerate() {
                // The real error goes to the entry the failure belongs to; every other waiter gets
                // `Poisoned`, which is precisely what its own append would have returned had it been
                // attempted after the failure (`Wal::append`'s error arm poisons the handle), and
                // what the WAL will in fact return for every subsequent call.
                let e = if i == index && k == 0 {
                    real.take().unwrap_or(WalError::Poisoned)
                } else {
                    WalError::Poisoned
                };
                self.ack_failed(&waiter, ExecError::Wal(e));
            }
        }
        self.health
            .record_window_service(entries, started.elapsed().as_nanos() as u64);
    }

    fn execute(&mut self, job: Job) {
        let Job { command, respond } = job;
        match command {
            // Unreachable on the deny lane while the lane follows the command; handled so the
            // executor stays total over `Command`. A window of one entry is exactly Task 3a's
            // per-command semantics, which is why there is no second ingest implementation — and
            // why this goes through `admit_ingest` rather than around it: if this arm ever becomes
            // reachable it must not be the one ingest path with no idempotency. The window it is
            // given is empty, so `BatchState::Held` is unconstructible here and the answers are
            // exactly `admit`'s.
            Command::Ingest {
                rows,
                batch_id,
                body_hash,
            } => {
                let window = CommitWindow::new(self.next_window_seq());
                let (window, _) = self.admit_ingest(window, rows, batch_id, body_hash, respond);
                if !window.is_empty() {
                    self.close_window(window);
                }
            }
            Command::Change {
                external_id,
                entity,
                op,
                descriptors,
            } => self.execute_change(&respond, external_id, entity, op, descriptors),
        }
    }

    /// `append → fsync → apply → swap → ack`, with lifecycle §4's deny-op exception.
    fn execute_change(
        &mut self,
        respond: &Responder,
        external_id: Vec<u8>,
        entity: EntityId,
        op: ChangeOp,
        raw_descriptors: Option<Vec<Vec<u8>>>,
    ) {
        let record = WalRecord::Change {
            external_id,
            op,
            descriptors: raw_descriptors.clone(),
        };
        let appended = self.wal.append(&record).and_then(|()| self.wal.fsync());
        self.observe_wal();

        match appended {
            Ok(_) => {
                // Resolution is deferred to **after** the append succeeds: unlike ingest, a
                // change's resolved terms are needed only for the `Overlay::apply` below, so there
                // is no reason to mint an extension id for a record that might never become
                // durable. `Delete`/`Suppress`/`Unsuppress` carry no descriptors, so the deny path
                // below resolves nothing either.
                let terms = raw_descriptors
                    .as_ref()
                    .map(|ds| self.live.resolve_terms(ds));
                // Durable, not yet in force. See `pause_point`.
                self.pause_point(PauseSiteArg::AfterFsync);
                let published = self.apply_change(entity, op, terms);
                self.ack(respond, Ack::Changed, &published);
            }
            Err(e) => {
                // Lifecycle §4, and the rule most easily broken by a plausible tidy-up: a
                // `Delete`/`Suppress` whose append failed is applied **anyway** — the item is
                // hidden immediately — and the caller gets 500 plus an alarm. Never a refusal that
                // leaves a deny unapplied. Deliberately scoped to those two ops: an `Unsuppress`
                // applied without durability would re-expose an item that replay still hides.
                //
                // This keeps working while the WAL is poisoned, which is the whole reason
                // `WalPoisoned` is a posture rather than a shutdown.
                if matches!(op, ChangeOp::Delete | ChangeOp::Suppress) {
                    let _published = self.apply_change(entity, op, None);
                }
                self.ack_failed(respond, ExecError::Wal(e));
            }
        }
    }

    /// Clone the buffer **once**, insert every entry in the window, publish **once**.
    ///
    /// The amortisation this buys is the one that grows: the clone is O(total buffered items) and
    /// the buffer only grows while flush is inert (stage 2.2), so a window of k entries pays it once
    /// instead of k times — and the same clone is the deny-ack latency floor
    /// (`ExecutorHealth::apply_nanos_total`).
    ///
    /// ## What the window did NOT do to this cost, recorded rather than implied (Task 7b)
    ///
    /// The window reduced the clone's **count**, not its **cost**. Two facts a reader sizing
    /// anything from the paragraph above needs, and neither is fixed here:
    ///
    /// 1. **At the shipped defaults `commit_window_max_items == ingest_max_batch_rows == 10 000`,
    ///    so a maximal batch is a one-entry window and gets no amortisation at all.** Ingesting
    ///    10⁹ rows in maximal batches is 10⁵ submissions each cloning a buffer growing towards
    ///    10⁹ — **O(N²/B)** — and *only stage 2.2's flush bounds it*. Measured today:
    ///    `apply_nanos_max` 210–437 ms at ~1.34 M buffered items
    ///    (`docs/evidence/memos/2026-08-01-deny-ack-baseline.md`, result 3). It is the *small*
    ///    batches the window collects. Task 7a did not fix this and did not claim to.
    /// 2. **Stage 2.2 makes the buffer chunked or persistent** when it rewrites buffer handling for
    ///    flush. So 2.1's O(B) clone is a **known temporary**, not an inherited posture — do not
    ///    build a second mechanism around it in the meantime.
    ///
    /// No counter is added for this: `apply_nanos_total` / `apply_nanos_max` (Task 3a) already
    /// measure it and are already on `/control/status`.
    ///
    /// `terms` is **taken** out of each entry rather than borrowed: each row's resolved set is
    /// *moved* into the buffer, where borrowing would force one `Vec<TermId>` clone per row on the
    /// one thread every write is serialised through (Task 3a measured that clone at +14% on the
    /// 10 000-row arm). `&mut` is what buys it; an entry's `terms` is empty after this and nothing
    /// downstream reads it — the ack needs `entity_ids`, not terms.
    fn apply_window(&self, closed: &mut [ClosedEntry<Responder>]) -> Published {
        let started = std::time::Instant::now();
        let generation = self.generation.load_full();
        let mut buffer = (*generation.buffer).clone();

        let mut established = lock_recover(&self.live.established);
        // Updated together in one critical section, so a `/control/changes` lookup and a
        // `/v1/items` drill-down can never disagree about the same item.
        let mut established_inverse = lock_recover(&self.live.established_inverse);
        // Entries in vec order = append order = apply order (Task 7b rule 1): a mixed window must
        // not replay in a different order than it applied, and the single vec is what makes that
        // true by construction rather than by discipline.
        for entry in closed.iter_mut() {
            let terms = std::mem::take(&mut entry.terms);
            for (row, row_terms) in entry.rows().iter().zip(terms) {
                // Contracts §3.4: no external id means nothing to establish. `None` must never
                // collide with `None`, so this skips rather than inserting under a shared empty key.
                if let Some(external_id) = &row.external_id {
                    established.insert(external_id.clone(), row.entity_id);
                    established_inverse.insert(row.entity_id, external_id.clone());
                }
                buffer.insert_row_with_terms(row, row_terms);
            }
        }
        drop(established);
        drop(established_inverse);

        let next = Generation {
            overlay_version: generation.overlay_version + 1,
            buffer: Arc::new(buffer),
            prefix: generation.prefix.clone(),
            segments_version: generation.segments_version,
            watermark: generation.watermark,
            bundle: Arc::clone(&generation.bundle),
            overlay: Arc::clone(&generation.overlay),
        };
        self.publish(next, started)
    }

    /// Clone the overlay, apply the change, publish.
    ///
    /// Pins are never invalidated by this (I11): a pin fixes `(prefix, segments_version)`, and this
    /// bumps `overlay_version`. That is lifecycle §2.3's rule that a suppression applies to a
    /// pinned request the moment it is accepted, without expiring the pin.
    fn apply_change(
        &self,
        entity: EntityId,
        op: ChangeOp,
        terms: Option<Vec<TermId>>,
    ) -> Published {
        let started = std::time::Instant::now();
        let generation = self.generation.load_full();
        let mut overlay: Overlay = (*generation.overlay).clone();
        overlay.apply(entity, op, terms);

        // Task 6 (D5). **It alarms; it does not act** — there is no fold until stage 2.3, so an
        // operator who sets `overlay_soft_limit` today gets a signal that the overlay is deep, not
        // a mechanism that makes it shallower.
        //
        // This is the only place the overlay grows **at runtime**; it is not the only place it
        // grows. `WritePath::reconstruct` builds one from WAL replay before this executor exists,
        // so a node restarting already over the limit is caught by
        // `Engine::set_overlay_soft_limit`'s own one-shot evaluation instead.
        // **Edge-triggered** (fix round 1, F5). The depth never decreases in this build, so a
        // level-triggered check emitted this four-line WARN on every subsequent deny, forever, with
        // no path back — an alarm flood at exactly the moment the node is under deny pressure.
        // `note_overlay_depth` returns `true` only on a crossing.
        let depth = overlay.len();
        let limit = self.health.overlay_soft_limit();
        if self.health.note_overlay_depth(depth) {
            tracing::warn!(
                overlay_depth = depth,
                overlay_soft_limit = limit,
                "ALARM: the overlay has crossed its configured soft limit. Nothing acts on this: \
                 there is no fold until stage 2.3, so the depth will not come down on its own — and \
                 this line is edge-triggered, so it will NOT repeat while the overlay stays over. \
                 Overlay depth is a term in I1's composition cost and in every deny's ack latency \
                 (each acceptance clones the overlay); watch overlay.depth on /control/status"
            );
        }

        let next = Generation {
            overlay_version: generation.overlay_version + 1,
            overlay: Arc::new(overlay),
            prefix: generation.prefix.clone(),
            segments_version: generation.segments_version,
            watermark: generation.watermark,
            bundle: Arc::clone(&generation.bundle),
            buffer: Arc::clone(&generation.buffer),
        };
        self.publish(next, started)
    }

    /// The generation swap. **The only `store` in the write path**, and the only producer of a
    /// [`Published`] token on the success path.
    ///
    /// `load_full` + `store` is safe here for one reason and one only: this is the sole thread that
    /// can publish. Stage 2.2's flush is a second publisher and **must not `store` directly** — it
    /// submits a command and is applied here, as lifecycle §1.3 requires ("submitting a completed,
    /// immutable result back to the lifecycle thread for a swap-only publication step"). A flush
    /// that stored directly would resurrect Track C's S2: a lost geometry publication leaves the
    /// *live* generation on the pin drain list, where Task 5's prune evicts projections still in
    /// use. `scripts/check-layers.sh` refuses any non-atomic `.store(` in this crate's sources
    /// outside this file — and that rule is demonstrated going red, not merely written.
    fn publish(&self, next: Generation, started: std::time::Instant) -> Published {
        self.generation.store(Arc::new(next));
        // The clone above is O(total buffered items) and, with no flush until stage 2.2, the buffer
        // only grows. This counter is what makes the deny-ack floor measurable rather than
        // asserted — see `ExecutorHealth::clone_nanos_total`.
        self.health
            .record_apply(started.elapsed().as_nanos() as u64);
        #[cfg(feature = "fault-injection")]
        if let Some(faults) = &self.faults {
            faults.record(tessera_lifecycle::faults::Step::Swap);
        }
        Published::by_swap()
    }

    /// Mirror the WAL's own poison flag into the posture.
    ///
    /// Asked of the WAL rather than remembered from the last error this loop happened to see: a
    /// posture derived from the executor's bookkeeping can drift from the thing it describes.
    fn observe_wal(&self) {
        if self.wal.is_poisoned() {
            self.health.advance(ExecutorPosture::WalPoisoned);
        }
    }

    /// Send a **successful** receipt. Requires proof that the effect is live — see [`Published`].
    ///
    /// The [`PauseSite::BeforeAck`] point is armed **here**, one statement above the send, rather
    /// than at either call site. That is what makes it a statement about the ack rather than about
    /// a line number: whichever rewrite 7a, 7b or 9 performs, an ack that has moved above the swap
    /// takes this pause point with it, and a test parked here then observes the effect *not* in
    /// force.
    fn ack(&self, respond: &Responder, ack: Ack, proof: &Published) {
        self.pause_point(PauseSiteArg::BeforeAck);
        #[cfg(feature = "fault-injection")]
        if let Some(faults) = &self.faults {
            faults.record(tessera_lifecycle::faults::Step::Ack);
        }
        respond.ack(ack, proof);
    }

    /// Send a failure receipt. **Not** armed with the pause point above: parking there would stall
    /// the WAL-failure tests inside a path that has nothing to say about ack ordering, and there is
    /// no effect for a parked test to look for.
    fn ack_failed(&self, respond: &Responder, error: ExecError) {
        #[cfg(feature = "fault-injection")]
        if let Some(faults) = &self.faults {
            faults.record(tessera_lifecycle::faults::Step::Ack);
        }
        respond.fail(error);
    }

    /// Reach an armed pause site, if any. Test builds only; a no-op otherwise.
    ///
    /// The two sites are `faults::PauseSite`'s, and the reason there are two is argued there: with
    /// only the after-fsync one, parking proves nothing about the relative order of the swap and
    /// the ack, because both are still ahead of the parked executor.
    #[cfg(feature = "fault-injection")]
    fn pause_point(&self, site: PauseSiteArg) {
        use tessera_lifecycle::faults::PauseAction;
        let Some(faults) = &self.faults else { return };
        match faults.pause_point(site) {
            None | Some(PauseAction::Stall) => {}
            Some(PauseAction::Panic) => {
                panic!("fault-injection: executor panicked at the {site:?} pause point")
            }
        }
    }

    #[cfg(not(feature = "fault-injection"))]
    fn pause_point(&self, _site: PauseSiteArg) {}
}

/// The pause-site argument, so the executor's two call sites read the same in both builds.
///
/// In a fault-injection build this **is** `faults::PauseSite`. In a shipped build the module does
/// not exist, so it is a local zero-variant-cost stand-in and `pause_point` is a no-op — the
/// alternative, `#[cfg]` at each call site, is the footgun `faults`'s module doc warns about.
#[cfg(feature = "fault-injection")]
type PauseSiteArg = tessera_lifecycle::faults::PauseSite;

#[cfg(not(feature = "fault-injection"))]
#[derive(Debug, Clone, Copy)]
enum PauseSiteArg {
    AfterFsync,
    BeforeAck,
}

#[cfg(test)]
mod retry_after_tests {
    use super::*;

    /// **Nothing has completed, so there is no observation at all.** The answer is the floor, and
    /// the floor is a floor chosen for lack of evidence — never a measurement dressed as one.
    ///
    /// Reachable in production and not a corner case: a queue bound of `n` can be filled by the
    /// first `n + 1` requests a freshly-started server ever receives.
    ///
    /// **Mutation:** delete the `service_nanos == 0` guard and this returns 0 (`depth × 0`), i.e.
    /// "retry immediately", which is a busy-wait dressed as backpressure.
    #[test]
    fn no_observation_yields_the_floor_not_a_zero() {
        assert_eq!(estimate_retry_after_s(64, 0), RETRY_AFTER_MIN_SECS);
        assert_eq!(estimate_retry_after_s(0, 0), RETRY_AFTER_MIN_SECS);
    }

    /// The derivation itself: a deep queue draining slowly gets a number that is neither `1` nor
    /// the clamp. This is what contracts §0.3 deviation 11 exists for — "a caller that retries at
    /// 1 s against a queue draining in 30 s manufactures exactly the load the 429 exists to shed".
    ///
    /// **Mutation:** return a constant `RETRY_AFTER_MIN_SECS` and this goes red on the first
    /// assertion, which is exactly the pre-Task-6 behaviour (`retry_after_s: 1`, hard-coded).
    #[test]
    fn a_deep_queue_draining_slowly_asks_for_more_than_one_second() {
        // 64 queued jobs at 500 ms each = 32 s.
        assert_eq!(estimate_retry_after_s(64, 500_000_000), 32);
        // Rounds **up**: 3 jobs at 400 ms is 1.2 s, and answering 1 sends the caller back early.
        assert_eq!(estimate_retry_after_s(3, 400_000_000), 2);
        // A fast queue still gets the floor rather than a fractional second.
        assert_eq!(estimate_retry_after_s(64, 3_200_000), RETRY_AFTER_MIN_SECS);
    }

    /// The clamp, both ends. Past a few minutes every client library reads `Retry-After` as "go
    /// away", so a larger number buys no client behaviour; the operator's unclamped signal is
    /// `work_depth` on `/control/status`.
    ///
    /// **Mutation:** drop the `.min(RETRY_AFTER_MAX_SECS)` and the first assertion reports 6400.
    #[test]
    fn the_estimate_is_clamped_at_both_ends() {
        assert_eq!(
            estimate_retry_after_s(64, 100_000_000_000),
            RETRY_AFTER_MAX_SECS
        );
        // And it cannot overflow into a *small* number, which would be the unsafe direction: the
        // deepest, slowest queue must never answer "come back immediately".
        assert_eq!(
            estimate_retry_after_s(u64::MAX, u64::MAX),
            RETRY_AFTER_MAX_SECS
        );
    }

    /// The EWMA tracks the **recent** regime, which is the whole reason it is not a cumulative
    /// mean. A server that ingested a long run of fast batches and has since slowed — the shape a
    /// growing `IngestBuffer` produces, since its clone is O(total buffered items) and flush is
    /// inert until stage 2.2 — must not keep quoting the fast figure.
    ///
    /// **Mutation:** replace `record_work_service`'s EWMA with a cumulative mean
    /// (`total / completed`) and the final assertion fails: 1000 samples at 3 ms swamp 40 at 3 s,
    /// so the estimate stays at the floor while the real drain is minutes.
    #[test]
    fn the_service_estimate_follows_the_recent_regime_not_the_whole_history() {
        let health = ExecutorHealth::new();
        for _ in 0..1000 {
            health.record_work_service(3_000_000); // 3 ms, buffer small
        }
        assert_eq!(
            estimate_retry_after_s(64, health.stats().work_service_nanos_ewma),
            RETRY_AFTER_MIN_SECS,
            "a fast queue is a one-second queue"
        );
        for _ in 0..40 {
            health.record_work_service(3_000_000_000); // 3 s, buffer deep
        }
        let ewma = health.stats().work_service_nanos_ewma;
        assert!(
            ewma > 1_000_000_000,
            "the EWMA must have followed the new regime; got {ewma} ns"
        );
        assert!(
            estimate_retry_after_s(64, ewma) > 60,
            "64 jobs at seconds apiece is minutes, not one second"
        );
    }

    /// `work_depth` is `submitted - completed`, **saturating**, and the saturation is not defensive
    /// programming: `work_submitted` is bumped *after* the `try_send` in
    /// [`LifecycleHandle::submit`], so the executor can complete a job before its submitter has
    /// recorded it and `completed > submitted` is transiently legal.
    #[test]
    fn work_depth_saturates_rather_than_underflowing() {
        let health = ExecutorHealth::new();
        health.record_work_service(1);
        assert_eq!(health.stats().work_completed, 1);
        assert_eq!(health.stats().work_depth, 0);
    }

    /// **The estimator was blind in exactly the state that produces the 429** (fix round 1, F7).
    ///
    /// `record_work_service` runs only when `execute` returns, so while one long job is in flight
    /// the EWMA still reports the previous regime. That is the 10⁹ shape — the first job at a new
    /// `IngestBuffer` depth is the slow one, and it is *while it runs* that the queue fills and
    /// callers are shed — and the resulting error is on the load-amplifying side: every shed caller
    /// is told to come back in a second and re-uploads up to `ingest_max_batch_bytes`.
    ///
    /// The construction is the real one: a thousand fast samples establish a fast EWMA, then a job
    /// is marked started and never completed, which is what "in flight" *is*.
    ///
    /// **Mutation:** make `service_nanos_for_estimate` return `self.work_service_nanos_ewma` and
    /// the second assertion drops to the floor, which is the shipped behaviour this replaces.
    #[test]
    fn a_long_job_in_flight_raises_the_estimate_before_it_completes() {
        // The clock origin is put 90 s in the past rather than waiting 90 s; a job marked as
        // starting *at* the origin has then been in flight for 90 s, which is the state under test.
        let ninety_seconds_ago = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(90))
            .expect("this test needs a host that has been up for at least 90 s");
        let health = ExecutorHealth::with_base(ninety_seconds_ago);
        for _ in 0..1000 {
            health.record_work_service(3_000_000); // 3 ms: a fast, established regime
        }
        assert_eq!(
            estimate_retry_after_s(64, health.stats().service_nanos_for_estimate()),
            RETRY_AFTER_MIN_SECS,
            "with nothing in flight the estimate is the EWMA's"
        );

        // A job that started 90 s ago and has not finished. Nothing has been recorded, so the EWMA
        // is untouched and still says 3 ms.
        health.mark_work_started(ninety_seconds_ago);
        let stats = health.stats();
        assert_eq!(
            stats.work_service_nanos_ewma, 3_000_000,
            "the EWMA is deliberately untouched — this is the blindness, not a fix to it"
        );
        assert!(
            stats.work_in_flight_nanos >= 89_000_000_000,
            "the in-flight job's own elapsed must be visible; got {} ns",
            stats.work_in_flight_nanos
        );
        assert!(
            estimate_retry_after_s(1, stats.service_nanos_for_estimate()) >= 90,
            "a caller queued behind a 90 s job must not be told to come back in one second"
        );

        // And it clears: the marker is dropped when the job completes, so a finished long job does
        // not go on inflating every later estimate.
        health.record_work_service(90_000_000_000);
        assert_eq!(health.stats().work_in_flight_nanos, 0);
    }

    /// **The soft-limit alarm is edge-triggered** (fix round 1, F5). `Overlay::len` never decreases
    /// in this build — entries survive `suppress → unsuppress` and nothing folds until stage 2.3 —
    /// so a level-triggered check emitted a four-line WARN per deny, forever, with no path back,
    /// precisely while the node was under deny pressure.
    ///
    /// **Mutation:** make `note_overlay_depth` return `depth >= limit` unconditionally (dropping the
    /// latch) and the "does not re-fire" assertion goes red.
    #[test]
    fn the_overlay_alarm_fires_once_per_crossing_not_once_per_change() {
        let health = ExecutorHealth::new();
        health.set_overlay_soft_limit(3);

        assert!(!health.note_overlay_depth(1));
        assert!(!health.note_overlay_depth(2));
        assert_eq!(health.stats().overlay_soft_limit_alarms, 0);

        assert!(health.note_overlay_depth(3), "the crossing must alarm");
        assert_eq!(health.stats().overlay_soft_limit_alarms, 1);

        // The state this test exists for: the depth only ever rises from here.
        for depth in 4..1000 {
            assert!(
                !health.note_overlay_depth(depth),
                "depth {depth} is over the limit but is not a CROSSING; alarming here is the flood"
            );
        }
        assert_eq!(health.stats().overlay_soft_limit_alarms, 1);

        // Re-arming, both ways it can happen. Falling back below the limit does not occur in this
        // build, and the trigger is written for the mechanism rather than for its current absence.
        assert!(!health.note_overlay_depth(0));
        assert!(health.note_overlay_depth(3));
        assert_eq!(health.stats().overlay_soft_limit_alarms, 2);

        // Re-setting the limit re-arms too, which is what makes "lower the limit under a live
        // overlay" — the WAL-replay shape `Engine::set_overlay_soft_limit` handles — audible.
        health.set_overlay_soft_limit(2);
        assert!(health.note_overlay_depth(3));
        assert_eq!(health.stats().overlay_soft_limit_alarms, 3);
    }
}
