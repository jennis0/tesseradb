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
use std::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard};

use rustc_hash::FxHashMap;

use tessera_authz::Dict;
use tessera_lifecycle::alloc::{high_water_from, AllocError, Allocator, PendingItem};
use tessera_lifecycle::buffer::DescriptorResolver;
use tessera_lifecycle::command::{Ack, Command, ExecError, Receipt, SubmitError, UnallocatedRow};
use tessera_lifecycle::faults::WalMeter;
use tessera_lifecycle::overlay::replay;
use tessera_lifecycle::wal::{ChangeOp, ExecutorWal, Wal, WalRecord, WalRow};
use tessera_lifecycle::{assign_sorted, IngestBuffer, Overlay};
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
    /// measurement** (`docs/design-memos/2026-08-01-deny-ack-baseline.md`). A deny's wait is
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
    work_service_nanos_ewma: AtomicU64,
    /// Task 6 (D5): overlay depth at which [`Executor::apply_change`] raises an alarm.
    /// [`usize::MAX`] means **no limit configured**, which is what every embedder and every test
    /// that never calls `Engine::set_overlay_soft_limit` gets.
    ///
    /// Deliberately not `0` for "unset": `tessera-server`'s config refuses `0` as degenerate for
    /// this key, so one value would have to mean "off" on one side of the crate boundary and
    /// "alarm on everything" on the other. That is how a knob comes to be silently inert.
    overlay_soft_limit: AtomicUsize,
    /// Times [`Executor::apply_change`] has published an overlay at or above
    /// [`Self::overlay_soft_limit`]. **It alarms; it does not act** — there is no fold until stage
    /// 2.3, so this counter and its log line are the whole of the mechanism.
    overlay_soft_limit_alarms: AtomicU64,
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
    /// Times an `apply_change` published an overlay at or above the configured soft limit (Task 6,
    /// D5). **It alarms; it does not act.**
    pub overlay_soft_limit_alarms: u64,
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
            overlay_soft_limit: AtomicUsize::new(usize::MAX),
            overlay_soft_limit_alarms: AtomicU64::new(0),
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
            overlay_soft_limit_alarms: self.overlay_soft_limit_alarms.load(Ordering::Relaxed),
        }
    }

    fn record_apply(&self, nanos: u64) {
        self.apply_nanos_total.fetch_add(nanos, Ordering::Relaxed);
        self.apply_nanos_max.fetch_max(nanos, Ordering::Relaxed);
    }

    /// One work-lane job finished. Called from [`Executor::run`], on the executor thread and
    /// nowhere else, which is what lets the EWMA be a plain load/store rather than a CAS loop.
    ///
    /// `sample` is the **whole** `execute` call — append, fsync, apply, swap and ack — because that
    /// is what a queued job waits for. `apply_nanos_total` is the wrong operand for a drain
    /// estimate and its own doc says why: it sums both lanes and excludes the fsync, and the fsync
    /// is the term the drain is paced by.
    fn record_work_service(&self, sample_nanos: u64) {
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
    pub fn set_overlay_soft_limit(&self, limit: usize) {
        self.overlay_soft_limit.store(limit, Ordering::Relaxed);
    }

    pub fn overlay_soft_limit(&self) -> usize {
        self.overlay_soft_limit.load(Ordering::Relaxed)
    }

    /// Record one crossing of the soft limit. Separate from the check so the **startup** evaluation
    /// (`Engine::set_overlay_soft_limit`, for an overlay that was already over the limit when the
    /// WAL was replayed) and the runtime one share a counter.
    pub fn record_overlay_soft_limit_alarm(&self) {
        self.overlay_soft_limit_alarms
            .fetch_add(1, Ordering::Relaxed);
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
///    buffer only grows. Measured (`docs/design-memos/2026-08-01-deny-ack-baseline.md`): ~3.0–3.5 ms
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
/// every `Published::` construction to this file, and there are exactly two (`publish`, and
/// `execute_ingest`'s replay arm); and `ack_follows_fsync_then_swap`'s `BeforeAck` leg fails on
/// engine state — the effect is not in force at the moment the ack is being sent — with no
/// reference to the step log. Type, rule, test: the claim is that no *one* of them is the
/// guarantee.
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
        pub(super) fn ack(&self, ack: Ack, _proof: Published) {
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

    fn allocate_sorted(&self, items: &mut [PendingItem]) -> Result<(), AllocError> {
        let mut alloc = lock_recover(&self.allocator);
        assign_sorted(items, &mut alloc)
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
                            stats.work_service_nanos_ewma,
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
/// before `work` is touched, which is what makes the starvation bound "the work item currently
/// executing" rather than "the work queue's depth".
pub(crate) struct LifecycleQueues {
    work: Receiver<Job>,
    deny: Receiver<Job>,
    /// The wake signal. Capacity one — see [`LifecycleHandle::bell`] and [`Executor::run`].
    bell: Receiver<()>,
}

// =================================================================================================
// The executor
// =================================================================================================

/// The single writer. One per partition, on its own thread, owning the WAL by value.
struct Executor {
    wal: ExecutorWal,
    live: Arc<LiveState>,
    /// **The only publishing capability in the write path.** Not in [`LiveState`], which the
    /// handler side shares.
    generation: Arc<GenerationHandle>,
    queues: LifecycleQueues,
    health: Arc<ExecutorHealth>,
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
    /// and a work item runs at most once per drain, so a deny's wait is bounded by the work item
    /// currently executing rather than by queue depth. The consequences are chosen: a sustained
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
    /// *At-most-one-work-item is Task 3a's shape, not the executor's permanent one*: Task 7a drains
    /// work into a commit window. Leftover tokens stay harmless under that change.
    fn run(&mut self) {
        loop {
            while let Ok(job) = self.queues.deny.try_recv() {
                self.execute(job);
            }
            if let Ok(job) = self.queues.work.try_recv() {
                // Timed **around the whole `execute`**, not around the apply: a queued job waits
                // for append, fsync, apply, swap and ack, and Task 6 derives `retry_after_s` from
                // this. The deny lane above is deliberately not timed — it has no bounded queue,
                // so it has no depth to drain and no 429 to derive.
                let started = std::time::Instant::now();
                self.execute(job);
                self.health
                    .record_work_service(started.elapsed().as_nanos() as u64);
                continue;
            }
            if self.queues.bell.recv().is_err() {
                break;
            }
        }
    }

    fn execute(&mut self, job: Job) {
        let Job { command, respond } = job;
        match command {
            Command::Ingest {
                rows,
                batch_id,
                body_hash,
            } => self.execute_ingest(&respond, rows, batch_id, body_hash),
            Command::Change {
                external_id,
                entity,
                op,
                descriptors,
            } => self.execute_change(&respond, external_id, entity, op, descriptors),
        }
    }

    /// `allocate → append → fsync → apply → swap → ack`, with today's per-command semantics.
    ///
    /// **An append failure applies nothing here**, in deliberate contrast to the deny path below.
    /// Applying un-fsynced ingest would make items appear and vanish across a crash, and lifecycle
    /// §4's apply-anyway rule is written for `Delete`/`Suppress` only. Task 7b's rule 2 restates
    /// this per window; getting it wrong here would pre-break it.
    fn execute_ingest(
        &mut self,
        respond: &Responder,
        rows: Vec<UnallocatedRow>,
        batch_id: String,
        body_hash: [u8; 32],
    ) {
        // Idempotency, evaluated **on the executor**: between a handler check and the enqueue an
        // open window can close (Task 8), and a retry that saw "unknown" and then enqueued into a
        // fresh window has double-allocated. Checked before `assign_sorted`, so a conflicting retry
        // burns no entity ids.
        if let Some((prev_hash, prev_ids)) = self.live.accepted_batch(&batch_id) {
            return if prev_hash == body_hash {
                let proof = Published::already_in_force(&prev_ids);
                self.ack(
                    respond,
                    Ack::Ingested {
                        entity_ids: prev_ids,
                    },
                    proof,
                )
            } else {
                self.ack_failed(respond, ExecError::BatchConflict { batch_id })
            };
        }

        // The fail-closed backstop for the widened check-to-apply race — see
        // `LiveState::established_collisions`.
        let collisions = self.live.established_collisions(&rows);
        if collisions > 0 {
            return self.ack_failed(
                respond,
                ExecError::DuplicateExternalId { count: collisions },
            );
        }

        // I9/§11.1: signature-sorted assignment, on the executor, from day one and permanent.
        let mut pending: Vec<PendingItem> = rows.iter().map(UnallocatedRow::to_pending).collect();
        if let Err(e) = self.live.allocate_sorted(&mut pending) {
            return self.ack_failed(respond, ExecError::Alloc(e));
        }

        // `terms` is **moved** out of each row, not cloned. `WalRow` has no `terms` field — the WAL
        // stores raw descriptors — so the resolved set has to be carried separately to the buffer
        // apply, and the obvious `rows.iter().map(|r| r.terms.clone())` costs one heap allocation
        // per row. That showed up as +14% on the 10,000-row bench arm, the only batch size at which
        // real work overtakes the fsync floor at all, and it was a regression this change
        // introduced rather than a cost the previous shape paid.
        let mut terms: Vec<Vec<TermId>> = Vec::with_capacity(rows.len());
        let mut wal_rows: Vec<WalRow> = Vec::with_capacity(rows.len());
        for (mut row, p) in rows.into_iter().zip(&pending) {
            terms.push(std::mem::take(&mut row.terms));
            wal_rows.push(row.into_wal_row(p.entity_id.expect("assign_sorted assigns every item")));
        }
        let entity_ids: Vec<EntityId> = wal_rows.iter().map(|r| r.entity_id).collect();

        // **A failed batch burns entity ids.** Assignment happens before the append, so ids given
        // to a batch whose append then fails are never issued again. That is I9-safe — ids stay
        // strictly monotone and each is issued once — and it is Phase 1's behaviour too, since the
        // handler allocated before appending. It is stated here because it is now visible in one
        // place instead of split across two files.
        let record = WalRecord::IngestBatch {
            batch_id: batch_id.clone(),
            body_hash,
            rows: wal_rows.clone(),
        };
        if let Err(e) = self.wal.append(&record).and_then(|()| self.wal.fsync()) {
            self.observe_wal();
            return self.ack_failed(respond, ExecError::Wal(e));
        }

        // Durable, not yet in force. See `pause_point`.
        self.pause_point(PauseSiteArg::AfterFsync);
        let published = self.apply_ingest(&wal_rows, terms);
        // Recorded after the swap, so a concurrent replay of the same batch id can never observe a
        // window where the generation has swapped but the idempotency index has not caught up.
        self.live
            .record_accepted_batch(batch_id, body_hash, entity_ids.clone());
        self.observe_wal();
        self.ack(respond, Ack::Ingested { entity_ids }, published);
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
                self.ack(respond, Ack::Changed, published);
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

    /// Clone the buffer, insert the batch, publish.
    fn apply_ingest(&self, rows: &[WalRow], terms: Vec<Vec<TermId>>) -> Published {
        let started = std::time::Instant::now();
        let generation = self.generation.load_full();
        let mut buffer = (*generation.buffer).clone();

        let mut established = lock_recover(&self.live.established);
        // Updated together in one critical section, so a `/control/changes` lookup and a
        // `/v1/items` drill-down can never disagree about the same item.
        let mut established_inverse = lock_recover(&self.live.established_inverse);
        // `terms` is consumed, not borrowed: each row's resolved set is *moved* into the buffer.
        // Borrowing would force `row_terms.clone()` here, one heap allocation per row on the one
        // thread every write is serialised through.
        for (row, row_terms) in rows.iter().zip(terms) {
            // Contracts §3.4: no external id means nothing to establish. `None` must never collide
            // with `None`, so this skips rather than inserting under a shared empty key.
            if let Some(external_id) = &row.external_id {
                established.insert(external_id.clone(), row.entity_id);
                established_inverse.insert(row.entity_id, external_id.clone());
            }
            buffer.insert_row_with_terms(row, row_terms);
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
        let depth = overlay.len();
        let limit = self.health.overlay_soft_limit();
        if depth >= limit {
            self.health.record_overlay_soft_limit_alarm();
            tracing::warn!(
                overlay_depth = depth,
                overlay_soft_limit = limit,
                "ALARM: the overlay is at or above its configured soft limit. Nothing acts on \
                 this: there is no fold until stage 2.3, so the depth will not come down on its \
                 own. Overlay depth is a term in I1's composition cost and in every deny's ack \
                 latency (each acceptance clones the overlay)"
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
    fn ack(&self, respond: &Responder, ack: Ack, proof: Published) {
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
}
