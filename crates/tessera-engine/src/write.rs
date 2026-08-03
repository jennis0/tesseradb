//! The write path: the single writer thread that owns the WAL, the two queues that feed it, and
//! the live state a handler reads before submitting.
//!
//! ## "Executor" and "the lifecycle thread" are the same thing
//!
//! Lifecycle §1.3, §4 and §7 call this **the lifecycle thread**; the types below call it the
//! executor. They denote one object — the OS thread
//! is literally named `"tessera-lifecycle"` at [`WritePath::start_executor`]. In particular §7's
//! "the engine's public API is sync and owns no executor" is about **async runtimes**: it forbids
//! `tessera-engine` acquiring tokio and running futures (policed by `scripts/check-layers.sh`'s
//! `deny tessera-engine tokio`), not owning a plain `std::thread`. A synchronous engine that owns
//! one writer thread is what §1.3 asks for; `Engine::accept_ingest` blocking its caller is the
//! visible consequence, and is why a tokio handler must wrap it in `spawn_blocking`.
//!
//! ## Why one thread owns the WAL, rather than a mutex guarding it
//!
//! The construction to argue against is serving `/control/ingest` and `/control/changes`
//! **inline**, on whichever thread the request landed on, keeping `append → fsync → apply → swap`
//! atomic by holding one `Mutex<Wal>` across all four steps. It works, and it is what a reader
//! expects; what it costs is that the mutex is not obviously about ordering at all, so the
//! discipline has to be explained rather than read.
//!
//! Here the `Wal` is **moved by value** onto one [`Executor`] thread per partition, and the
//! ordering stops being a discipline: there is one thread that can reach the WAL, one thread that
//! can publish a generation, and it does the four steps in that order because there is nowhere else
//! for them to happen.
//!
//! Two lost-update races go with it. Two acceptances that both `load_full`, both clone, and whose
//! later `store` silently discards the other's already-acked change cannot occur when only one
//! thread stores. Nor can the worse variant, a lost *geometry* publication, which leaves the
//! **live** generation on the pin drain list, where the cache's prune evicts projections still in
//! use. The engine has exactly one non-atomic `.store(` — the swap below — and it runs on the
//! executor thread. `scripts/check-layers.sh` polices that, because the property survives only
//! while it stays true, and a flush would be precisely a second publisher.
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

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard};

use rustc_hash::{FxHashMap, FxHashSet};

use tessera_authz::{DeltaTier, Dict};
use tessera_lifecycle::alloc::{high_water_from, AllocError, Allocator};
use tessera_lifecycle::buffer::DescriptorResolver;
use tessera_lifecycle::command::{Ack, Command, ExecError, Receipt, SubmitError, UnallocatedRow};
use tessera_lifecycle::faults::WalMeter;
use tessera_lifecycle::overlay::{replay, PredicateChange};
use tessera_lifecycle::wal::{ChangeOp, ExecutorWal, Wal, WalError, WalRecord};
use tessera_lifecycle::window::{ClosedEntry, CommitWindow, FragmentationTally, WindowEntry};
use tessera_lifecycle::{IngestBuffer, Overlay};

use crate::cache::RowProjectionCache;
use crate::cache::KEEP_SUPERSEDED_GENERATIONS;
use crate::geometry::{check_publishable, GeometryRefused};
use tessera_plugin::Descriptor;
use tessera_spatial::tiler::ScalarType;
use tessera_store::manifest::{DenyEntry as ManifestDenyEntry, SegmentsManifest};
use tessera_store::{Bundle, StoreError};
use tessera_types::{EntityId, IdentityKey, TermId};

use crate::session::EngineError;
use crate::{Generation, GenerationHandle};

// =================================================================================================
// Posture and counters
// =================================================================================================

/// What the write executor is able to do — **the signal `readyz` reads**.
///
/// Four states rather than a bool, because the operator response differs and a bool would collapse
/// "nobody started a writer" into "the writer died", which are different bugs.
///
/// **Composed from two components, not latched as one value.** The thread's own state
/// (`NotStarted` → `Running` → `Dead`) is monotone and latched: published with `fetch_max`, never
/// `store`, so a thread that panics the instant it is spawned cannot have its `Dead` clobbered by
/// the parent's `Running`, and `Dead` is absorbing. The WAL's state is **not** latched — it is
/// mirrored live from the WAL in both directions, because a WAL that has discarded its undurable
/// region is genuinely healthy again and a posture that could not say so would be reporting a
/// condition that no longer exists. See [`ExecutorHealth::posture`] for how the two compose and why
/// `Dead` still wins over everything.
///
/// A latched value was the original shape and it made this enum's own doc false: `fetch_max` over
/// all four meant `WalPoisoned` could never be left, so the function whose stated purpose was to
/// mirror the WAL rather than remember an error was the one that remembered.
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
    ///
    /// **Leavable, and only in one direction that matters.** A sync failure is repairable: the
    /// executor discards the undurable region and the posture returns to `Running` without a
    /// restart ([`Executor::recover_wal`]). A torn append is not repairable, so a handle that
    /// reaches it stays here for the life of the process — not by a latch here, but because the
    /// WAL itself never leaves that state. Terminality lives in the one place that knows whether it
    /// is true.
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
    /// The **thread's** own state: `NotStarted` → `Running` → `Dead`, advanced with `fetch_max` and
    /// never lowered. `Dead` is absorbing and is written by a drop guard on the executor's own
    /// stack during unwind, so a panicked executor can never read as running again.
    ///
    /// The WAL's state is deliberately not folded in here — see [`Self::wal_poisoned`].
    lifecycle: AtomicU8,
    /// Whether the WAL currently refuses operations, mirrored from the WAL on every observation and
    /// **in both directions**.
    ///
    /// Separate from [`Self::lifecycle`] because the two have opposite temporal shapes and one
    /// atomic cannot carry both. Thread death is permanent and must latch; WAL poisoning is a
    /// condition that a discard can end. Folding them into one monotone value — which is what this
    /// was — meant a node that recovered went on reporting a fault it no longer had, and left
    /// `Executor::observe_wal`'s stated purpose ("mirror the WAL rather than remember an error")
    /// describing something the code did not do.
    ///
    /// Written only by the executor thread, so a plain store is enough; read by `/readyz` and
    /// `/control/status` from any thread.
    wal_poisoned: AtomicBool,
    /// Times the executor discarded an undurable WAL region and returned to service.
    ///
    /// **The incident survives the recovery.** Readiness coming back is the right operator-facing
    /// answer — a node latched unready over a condition that cleared seconds ago is an outage the
    /// storage never caused — but it would otherwise erase every trace that durability was once
    /// lost. This counter is that trace, and it is the number to alarm on: a node that recovers
    /// repeatedly is a node whose disk is failing slowly, which no single posture reading shows.
    wal_recoveries: AtomicU64,
    work_submitted: AtomicU64,
    deny_submitted: AtomicU64,
    /// Flush ticks fired since the executor started — the observable that makes "the tick runs"
    /// a condition a test can wait on rather than a sleep it has to guess at.
    pub(crate) ticks: AtomicU64,
    /// A `POST /control/flush` awaiting the next tick (contracts §3.4). A flag, not a count: the
    /// endpoint's 202 means "accepted, not yet done", and two requests before one tick are
    /// satisfied by that tick together.
    pub(crate) flush_requested: AtomicBool,
    /// **The in-memory overlay holds dispositions the durable WAL does not** (§7.2).
    ///
    /// Set when [`Executor::recover_wal`] discards an undurable region, and **cleared only by a
    /// restart**. `Wal::discard_undurable` deliberately does not un-apply — "a restart will not
    /// carry them" — so the node returns to `Running` holding denies no record backs, and the
    /// poisoned posture no longer covers it. Publishing a manifest or rotating the WAL from that
    /// overlay would make a 500'd, never-acked deny permanent.
    ///
    /// Distinct from [`Self::wal_poisoned`], which is mirrored from the WAL in both directions:
    /// this one latches, because what diverged stays diverged until the process is replaced.
    pub(crate) overlay_diverged: AtomicBool,
    /// Buffer occupancy as of the last apply — what `/control/ingest`'s occupancy bound is checked
    /// against, and what `flush_max_items` marks ready.
    ///
    /// Published by the executor and read by handlers, so it lags by at most one apply. That is
    /// the right shape for a backpressure signal: an exact figure would need the handler to hold
    /// the generation, and the bound it feeds is a ceiling with an order of magnitude of headroom
    /// (see `DEFAULT_INGEST_BUFFER_MAX_ITEMS`), not a precise quota.
    pub(crate) buffered_items: AtomicUsize,
    /// Items that would acquire geometry at the last tick — the buffer minus what the three
    /// dispositions exclude (§3.5).
    ///
    /// **The gauge for a stalled flush.** It stays at zero on a node whose gates are closed and
    /// grows without bound on one whose flush keeps failing, which are the two states
    /// `buffered_items` alone cannot tell apart from healthy backlog.
    pub(crate) flushable_items: AtomicUsize,
    /// Durable buffered rows excluded from every flush because their coordinates fall outside the
    /// bundle's declared extent (§6). They never leave this state on their own.
    /// Flushes published since the executor started — what "an acked ingest became visible" is
    /// observed on, rather than on a sleep.
    pub(crate) flushes: AtomicU64,
    /// Ticks that found a flush already in flight and skipped rather than queued (§1.1).
    ///
    /// **Alarmed, because `flush_max_age_secs` would otherwise miss it**: a flush persistently
    /// slower than the tick is a visibility-latency breach, and the period an operator configured
    /// is not the period they are getting.
    pub(crate) flush_skips: AtomicU64,
    /// Flushes that failed and left the buffer intact for the next tick (§10).
    pub(crate) flush_failures: AtomicU64,
    /// Total nanoseconds spent in **the whole apply step** — the `IngestBuffer`/`Overlay` clone,
    /// the per-row inserts, the `Generation` construction and the swap.
    ///
    /// **Named for what it measures.** The timer starts at the top of the apply, so it is not the
    /// clone alone. The clone dominates it — O(total buffered items) against O(batch) for the
    /// inserts — which is why it is still the right operand for sizing a deny-ack floor; but a
    /// figure something is sized from must not quietly be something else, so the name says what was
    /// timed.
    ///
    /// **This bounds the deny-ack *wait*, not the deny's own cost**, and the distinction is
    /// measured rather than argued (`docs/evidence/memos/2026-08-01-deny-ack-baseline.md`). A deny's
    /// wait is bounded by "the work item currently executing", and *that* item's apply includes a
    /// clone that is O(total buffered items) — modelled at 100–300 ms per clone at 1 M buffered
    /// items and 1–3 s at 10 M, bounded now by what the flush leaves buffered rather than by the
    /// whole corpus. Measured at 1 M buffered items: a deny under sustained ingest acks in
    /// 165 ms p50 / 346 ms max, against a 3.2 ms quiescent floor. That much is confirmed.
    ///
    /// **What this counter is NOT is the deny's own floor**, and reading it as one is the available
    /// mistake. [`Executor::apply_changes`] clones the **overlay**; only the ingest apply clones the
    /// buffer. Measured, a deny's own
    /// apply is **1.3 µs at 1,000,000 buffered items** and is flat in buffer depth — it is
    /// O(overlay), rising to ~4.3 µs at overlay depth 2,000. This counter sums **both lanes**, so
    /// its value is dominated by ingest applies and attributes none of itself to either.
    ///
    /// **And it is an estimator of the wait, not a bound on it**: measured, the worst deny ack
    /// exceeds `apply_nanos_max` over the same phase by up to 1.53×, because a deny waits for the
    /// whole in-flight item (append, fsync, apply, ack) and then pays its own append and fsync.
    ///
    /// Counted at all because lifecycle §1.3's "never queued behind work of unbounded duration" is
    /// a claim this file makes in code, and a claim of that shape needs a measurement beside it.
    apply_nanos_total: AtomicU64,
    apply_nanos_max: AtomicU64,
    /// Work-lane jobs whose `execute` has returned. **The other half of the queue-depth gauge**:
    /// `work_submitted - work_completed` is what [`ExecutorStats::work_depth`] reports and what
    /// the 429's `retry_after_s` is derived from. Deny-lane jobs are deliberately not counted here
    /// — they ride an unbounded queue that has no depth to report and no 429 to derive.
    work_completed: AtomicU64,
    /// An **exponentially-weighted** mean of one work-lane job's whole service time (append +
    /// fsync + apply + swap + ack), in nanoseconds. Written only by the executor thread.
    ///
    /// **Why not a cumulative mean over `total / completed`.** It is wrong in the one case the
    /// estimate exists for. Service time is dominated by an
    /// `IngestBuffer` clone that is O(total buffered items) and grows monotonically while there is
    /// no flush (`apply_nanos_total`'s doc has the measurements), so a server that ingested 10⁶ fast
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
    /// # Why this exists
    ///
    /// [`Self::record_work_service`] runs *after* `execute` returns, so while one long job is in
    /// flight the EWMA still reports the previous, faster regime. That is the 10⁹ shape: an
    /// `IngestBuffer` clone is O(total buffered items), so the first job at a new buffer depth is
    /// the slow one, and it is precisely while it runs that the
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
    /// The overlay depth at which [`Executor::apply_changes`] raises an alarm.
    /// [`usize::MAX`] means **no limit configured**, which is what every embedder and every test
    /// that never calls `Engine::set_overlay_soft_limit` gets.
    ///
    /// Deliberately not `0` for "unset": `tessera-server`'s config refuses `0` as degenerate for
    /// this key, so one value would have to mean "off" on one side of the crate boundary and
    /// "alarm on everything" on the other. That is how a knob comes to be silently inert.
    overlay_soft_limit: AtomicUsize,
    /// Times the overlay has **crossed** into being at or above [`Self::overlay_soft_limit`]. **It
    /// alarms; it does not act** — there is no compaction fold (⊘), so this counter and its log
    /// line are the whole of the mechanism.
    ///
    /// **Crossings, not publications.** Counting every apply at or above the limit is
    /// level-triggering on a quantity that never decreases: `Overlay` entries survive
    /// `suppress → unsuppress`, and nothing shrinks the overlay. A node that crossed 500 000 would
    /// emit one four-line WARN **per deny, forever**, with no path back — flooding the log precisely
    /// while the node is under deny pressure. `control.rs` states that exact standard itself ("an
    /// ERROR per occurrence is an alarm flood rather than a signal") one file over.
    /// [`Self::overlay_soft_limit_latched`] is the edge.
    overlay_soft_limit_alarms: AtomicU64,
    /// Whether the overlay is currently *known* to be at or above the soft limit — the edge
    /// trigger's memory. Set when [`Self::note_overlay_depth`] observes a crossing, cleared when it
    /// observes a depth below the limit or when the limit itself is re-set.
    overlay_soft_limit_latched: AtomicBool,
    /// The row count at which a commit window closes — `ingest.commit_window_max_items`,
    /// which counts **rows** (see that key's doc: its default is sized from `window rows ×
    /// term_density`, and both the heap and the latency a window costs scale in rows).
    ///
    /// Reaches the executor by [`Engine::set_commit_window_max_rows`] rather than through
    /// `start_write_executor`'s argument list, on `set_overlay_soft_limit`'s precedent: a knob every
    /// embedder and every test would otherwise have to pass explicitly is a knob that gets passed
    /// wrong.
    ///
    /// **Defaulted to a real number, not `usize::MAX`.** An embedder that sets nothing must still
    /// get a bounded window: the drain that fills a window frees a queue slot per entry, which a
    /// concurrent submitter immediately refills, so "close when the queue is empty" is not a bound
    /// under sustained load — it is an invitation to hold the entire load in memory. See
    /// [`DEFAULT_COMMIT_WINDOW_MAX_ROWS`].
    commit_window_max_rows: AtomicUsize,
    /// What every closed commit window's allocation collected, in entity space — the counters
    /// behind `/control/status`'s `fragmentation` (contracts §3.4).
    ///
    /// Folded here rather than measured here, because the measurement needs the window's ids and
    /// term lists together and that pairing exists only inside `CommitWindow::allocate`. See
    /// [`tessera_lifecycle::window::FragmentationTally`] for what the numbers mean, what they
    /// deliberately do not, and why the per-term state it needs is bounded by one window rather
    /// than by the corpus.
    ///
    /// **The deny lane contributes nothing**, and not because deny windows are empty: the deny lane
    /// never constructs a `CommitWindow` at all. `Executor::commit_denies` is a separate path over
    /// `DenyEntry`, and denies assign no entity ids.
    ///
    /// Five counters under one mutex rather than five atomics, because they are only ever written
    /// together (once per window close, by the executor thread) and only ever read together (one
    /// `/control/status` snapshot). Five independent atomics would let a reader see a `runs` from
    /// one window against a `baseline` from the next, and publish a ratio that never existed.
    /// Read through [`lock_recover`] on the same argument every other lock in this module makes:
    /// an operator gauge must not turn one writer fault into a panicking admin plane.
    fragmentation: Mutex<FragmentationTally>,
    /// Commit windows whose allocation has been tallied. The denominator an operator needs to read
    /// the rest: the ratios are means over windows, and a mean over three windows is not a trend.
    fragmentation_windows: AtomicU64,
    /// The executor's WAL counters. A **clone** of the meter the [`ExecutorWal`] holds, kept here
    /// so the numbers have a reader: `/control/status`, and `one_fsync_per_window`, whose whole
    /// subject is `wal_fsyncs` not rising with the number of
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
    /// Flush ticks fired since the executor started (§1.3).
    pub ticks: u64,
    /// Items in the ingest buffer as of the last apply — the figure `/control/ingest`'s occupancy
    /// bound is checked against.
    pub buffered_items: usize,
    /// Items that would acquire geometry at the last tick (§3.5) — zero on a gated node, growing
    /// without bound on one whose flush keeps failing.
    pub flushable_items: usize,
    /// Flushes published since the executor started.
    pub flushes: u64,
    /// Ticks skipped because a flush was already in flight (§1.1) — a rising count is a flush
    /// persistently slower than the tick, i.e. a visibility-latency breach.
    pub flush_skips: u64,
    /// Flushes that failed and left the buffer intact for the next tick (§10).
    pub flush_failures: u64,
    /// Whether a `POST /control/flush` is awaiting the next tick.
    pub flush_requested: bool,
    /// Whether this node's overlay has diverged from its durable WAL (§7.2). **Latching**: it
    /// publishes no flush and rotates no WAL until restarted.
    pub overlay_diverged: bool,
    /// Successful WAL appends since the executor started.
    pub wal_appends: u64,
    /// Successful WAL fsyncs since the executor started — the unit group commit is
    /// defined in ("one fsync per window") and the one the ingest baseline memo's ~3.2 ms floor is
    /// a cost per.
    ///
    /// **`wal_appends / wal_fsyncs` is the production measurement of group commit's amortisation**,
    /// and the reason no window-size gauge was added: one append per entry and one fsync per window
    /// make that ratio the mean entries per window, over the whole life of the executor. Both are
    /// already on `/control/status`. It is a mean over **both** windows — the ingest one
    /// ([`Executor::run_work_pass`]) and the deny one ([`Executor::commit_denies`]), which are
    /// separate windows with separate close policies (`tessera_lifecycle::window` argues why they
    /// are not one), so a ratio that mixes a deny-heavy and an ingest-heavy period says nothing
    /// about either. Read it when diagnosing
    /// ingest throughput — a ratio pinned at ~1.0 under
    /// concurrent load means every window is closing with one entry in it, which is what a workload
    /// that re-ingests the same `external_id`s does (`CommitWindow::holds_external_id_of` closes the window
    /// on nearly every entry), and it is the difference between group commit working and group commit
    /// running.
    pub wal_fsyncs: u64,
    /// Times the executor discarded an undurable WAL region and returned to service — see
    /// [`ExecutorHealth::wal_recoveries`].
    ///
    /// **The durability incident's only surviving trace.** The posture returns to `running` once the
    /// condition clears, which is the right answer for routing and the wrong one for diagnosis; this
    /// is what an operator alarms on. A node recovering repeatedly has a disk that is failing
    /// slowly, and every deny answered 500 in between is one whose caller owes a retry
    /// (contracts §3.1).
    pub wal_recoveries: u64,
    /// Work-lane jobs whose `execute` has returned.
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
    /// Times the overlay crossed to at or above the configured soft limit. **It alarms;
    /// it does not act**, and it counts **crossings**, not publications above the limit — see
    /// [`ExecutorHealth::overlay_soft_limit_alarms`].
    pub overlay_soft_limit_alarms: u64,
    /// What every closed commit window's allocation collected (contracts §3.4's `fragmentation`).
    /// See [`ExecutorHealth::fragmentation`] and, for the meaning of the numbers,
    /// [`tessera_lifecycle::window::FragmentationTally`].
    pub fragmentation: FragmentationTally,
    /// Commit windows behind [`Self::fragmentation`].
    pub fragmentation_windows: u64,
}

impl ExecutorStats {
    /// Total postings over containers touched (contracts §3.4). `None` before any window has closed
    /// — a zero would read as a measurement rather than as an absence.
    ///
    /// **Reduces to mean postings per term per window at every reachable window size**, because a
    /// window spans one 2¹⁶ container or two; [`tessera_lifecycle::window::FragmentationTally`] has
    /// the arithmetic. Emitted because contracts §3.4 specifies it, not because a window collects
    /// the container-count win — it collects none of it.
    pub fn postings_per_container(&self) -> Option<f64> {
        let f = self.fragmentation;
        (f.containers > 0).then(|| f.postings as f64 / f.containers as f64)
    }

    /// Measured mean posting run length over the random baseline at the same density (contracts
    /// §3.4). `1.0` is fully scattered, larger is better; `None` before any window has closed.
    ///
    /// **Within-window sort quality, not stream-scope fragmentation.** Both the measurement and its
    /// baseline are taken at window scope, so this reports what one allocation run collected
    /// relative to a random assignment of that same window — at a one-row window it is identically
    /// `1.0` for every corpus. Fragmentation *between* windows is invisible to it by construction,
    /// which is the part design §11.1 records as permanent. The raw counters are published beside
    /// it so `postings / runs` is available without the window-local normalisation.
    ///
    /// And it is the **entity**-space quantity — posting run length — never the row-space mask run
    /// ratio, which normalises the same way over a different set and is not comparable.
    pub fn run_ratio(&self) -> Option<f64> {
        let f = self.fragmentation;
        (f.runs > 0).then(|| f.baseline_runs_milli as f64 / 1000.0 / f.runs as f64)
    }
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
            lifecycle: AtomicU8::new(ExecutorPosture::NotStarted as u8),
            wal_poisoned: AtomicBool::new(false),
            wal_recoveries: AtomicU64::new(0),
            work_submitted: AtomicU64::new(0),
            deny_submitted: AtomicU64::new(0),
            ticks: AtomicU64::new(0),
            flush_requested: AtomicBool::new(false),
            overlay_diverged: AtomicBool::new(false),
            buffered_items: AtomicUsize::new(0),
            flushable_items: AtomicUsize::new(0),
            flushes: AtomicU64::new(0),
            flush_skips: AtomicU64::new(0),
            flush_failures: AtomicU64::new(0),
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
            fragmentation: Mutex::new(FragmentationTally::default()),
            fragmentation_windows: AtomicU64::new(0),
            wal: Arc::new(WalMeter::new()),
        }
    }

    /// Advance the **thread's** state. Monotone: `NotStarted` → `Running` → `Dead`, never back.
    ///
    /// Takes only those three; the WAL's contribution arrives through [`Self::mirror_wal`], and
    /// keeping them apart is what stops a caller latching a recoverable condition by reaching for
    /// the function next to it.
    fn advance(&self, to: ExecutorPosture) {
        debug_assert!(
            to != ExecutorPosture::WalPoisoned,
            "the WAL's state is mirrored, never advanced — see `mirror_wal`"
        );
        self.lifecycle.fetch_max(to as u8, Ordering::SeqCst);
    }

    /// Publish the WAL's current state, in **either** direction. Executor thread only.
    ///
    /// Returns whether this observation was a recovery, so the counter is bumped exactly once per
    /// transition rather than once per observation — the executor observes on a timer while
    /// degraded, and a level-triggered count would report the poll rate.
    fn mirror_wal(&self, poisoned: bool) -> bool {
        let was = self.wal_poisoned.swap(poisoned, Ordering::SeqCst);
        let recovered = was && !poisoned;
        if recovered {
            self.wal_recoveries.fetch_add(1, Ordering::Relaxed);
        }
        recovered
    }

    /// The two components composed, in the order an operator needs them.
    ///
    /// **`Dead` wins over everything**, including a poisoned WAL: a thread that is gone cannot apply
    /// a write whatever the log says, and the two faults call for different operator actions. It is
    /// also the only arm that must survive a racing recovery — the drop guard runs during unwind,
    /// after which no executor exists to mirror anything, so a `Dead` node stays `Dead` by the
    /// latch rather than by anyone remembering to stop mirroring.
    ///
    /// **`NotStarted` is answered before the WAL is consulted**, because a WAL that no executor owns
    /// has had no operation attempted on it and reporting it poisoned would describe a failure that
    /// could not have happened. Both reduce to not-ready, so neither is the fail-open direction.
    pub fn posture(&self) -> ExecutorPosture {
        match ExecutorPosture::from_u8(self.lifecycle.load(Ordering::SeqCst)) {
            ExecutorPosture::Dead => ExecutorPosture::Dead,
            ExecutorPosture::NotStarted => ExecutorPosture::NotStarted,
            _ if self.wal_poisoned.load(Ordering::SeqCst) => ExecutorPosture::WalPoisoned,
            other => other,
        }
    }

    /// Times an undurable WAL region was discarded and the executor returned to service.
    pub fn wal_recoveries(&self) -> u64 {
        self.wal_recoveries.load(Ordering::Relaxed)
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
            wal_recoveries: self.wal_recoveries.load(Ordering::Relaxed),
            work_completed,
            work_depth: work_submitted.saturating_sub(work_completed),
            work_service_nanos_ewma: self.work_service_nanos_ewma.load(Ordering::Relaxed),
            work_in_flight_nanos: self.work_in_flight_nanos(),
            overlay_soft_limit_alarms: self.overlay_soft_limit_alarms.load(Ordering::Relaxed),
            fragmentation: *lock_recover(&self.fragmentation),
            fragmentation_windows: self.fragmentation_windows.load(Ordering::Relaxed),
            ticks: self.ticks.load(Ordering::Relaxed),
            overlay_diverged: self.overlay_diverged.load(Ordering::SeqCst),
            flushable_items: self.flushable_items.load(Ordering::SeqCst),
            flushes: self.flushes.load(Ordering::Relaxed),
            flush_skips: self.flush_skips.load(Ordering::Relaxed),
            flush_failures: self.flush_failures.load(Ordering::Relaxed),
            buffered_items: self.buffered_items.load(Ordering::Relaxed),
            flush_requested: self.flush_requested.load(Ordering::SeqCst),
        }
    }

    /// Fold one closed window's tally in. Executor thread only, once per window close.
    fn record_fragmentation(&self, tally: FragmentationTally) {
        lock_recover(&self.fragmentation).merge(tally);
        self.fragmentation_windows.fetch_add(1, Ordering::Relaxed);
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

    /// The overlay depth at which the deny apply alarms. `usize::MAX` disables it.
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

    /// Set the row count at which a commit window closes. See
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
    /// flood one call site at a time. Both evaluation sites — [`Executor::apply_changes`] at runtime,
    /// `Engine::set_overlay_soft_limit` for the overlay a WAL replay produced before any executor
    /// existed — go through here, so they share the counter *and* the edge.
    ///
    /// Returns `true` at most once per crossing. Depth falling back below the limit re-arms it, as
    /// does re-setting the limit; neither happens as the code stands (overlay entries survive
    /// `unsuppress` and there is no compaction fold, ⊘), and the trigger is written for the
    /// mechanism rather than for the absence of one.
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
///    clone that is O(total buffered items), which the flush cadence bounds rather than removes.
///    Measured (`docs/evidence/memos/2026-08-01-deny-ack-baseline.md`): ~3.0–3.5 ms
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

/// The commit window's row bound for an engine whose embedder sets none.
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
/// The obvious claim to make for a proof token is "ack before swap does not compile", and putting
/// `Published` beside [`Executor`] does **not** make it true. The proof would then be demanded only
/// by the *helper* [`Responder::ack`], while `Receipt::ok` is a public constructor with no proof
/// parameter; give `Job` a raw `SyncSender<Receipt>` and `respond.send(Receipt::ok(ack))` compiles
/// anywhere — including inside this file, which is the only place that matters, since every rewrite
/// the token exists to survive is a rewrite *of this file*. Rust's privacy is per **module**, so a
/// guard that lives in the same module as the code it guards guards nothing.
///
/// So the sender moves in here and the field is private to this module. Outside it — which is all
/// of the executor — a `Responder` offers exactly two operations, [`Responder::ack`] (needs a
/// [`Published`]) and [`Responder::fail`] (cannot carry an `Ack`). There is no third route to a
/// successful receipt, because there is no way to reach the channel.
///
/// ## What this still does not buy
///
/// [`Published::by_swap`] and [`Published::already_in_force`] are callable from anywhere in
/// `write.rs`. A worker who *wants* to ack early can still mint a token — the replay path's
/// `already_in_force()` is the obvious thing to reach for, and is exactly what a mutation testing
/// this guard reaches for. Two things catch that rather than the type system:
/// `check-layers.sh` rule 3 pins
/// every `Published::` construction to this file, and there are exactly two ([`Executor::publish`],
/// and the replay arm of [`Executor::admit_ingest`]); and `ack_follows_fsync_then_swap`'s
/// `BeforeAck` leg
/// fails on engine state — the effect is not in force at the moment the ack is being sent — with no
/// reference to the step log. Type, rule, test: the claim is that no *one* of them is the
/// guarantee.
///
/// **The token is taken by reference, and that is a deliberate weakening.** [`Responder::ack`]
/// takes `&Published` rather than a `Published` by value, so one token acks unboundedly many
/// waiters — which means acking window *k+1*'s waiters with window *k*'s token type-checks. By
/// value would be stronger per ack and weaker overall: N acks would need N tokens, so a window's
/// ack *loop* would have to mint inside itself, at a site with no swap adjacent, and
/// `check-layers.sh`'s rule is a *location* rule that would not see it. The reduction is real and
/// is written down here rather than left to be rediscovered. What still holds it: one window swaps
/// once, and [`Executor::close_window`] is the only place a window's waiters are reached.
mod ack {
    use std::sync::mpsc::SyncSender;

    use tessera_lifecycle::command::{Ack, ExecError, Receipt};

    /// Proof that a generation carrying a command's effect is live.
    ///
    /// [`super::Responder::ack`] cannot send a *successful* receipt without one, and the only
    /// producers are the **two** named constructors below — the same two `scripts/check-layers.sh`
    /// pins to this file.
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
        /// **By reference** — one generation swap acknowledges N waiters, and
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
/// recovered state is a prefix of a batch rather than a torn value; and buffered items have no row
/// geometry at all — that is what being buffered means — so a partial prefix contributes to no
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
    /// The I9 allocator. **Written only by the executor**, never by a handler: entity ids are
    /// assigned at a window's close, on the one thread that also advances the high-water mark.
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

    /// The descriptor bytes behind `terms`, read out of the resolver's extension map (§3.2).
    ///
    /// **This is the inverse the flush needs, and it is why promotion costs no format change.**
    /// `BufferedItem` holds resolved `TermId`s, so a flush looking at the buffer alone cannot turn
    /// an extension id back into a descriptor; the resolver has held the bytes all along, one
    /// entry per distinct descriptor rather than per item, kept for the process's lifetime so the
    /// assignment stays continuous across replay and live accepts.
    ///
    /// **Total for any id a flush plan can name**, which is what lets `promote` treat a miss as a
    /// failed flush rather than a dropped term: rotation reclaims only WAL members below the
    /// oldest *buffered* row, so a buffered item's record always survives, and replay re-interns
    /// its descriptors before the item goes back into the buffer.
    ///
    /// Walks the map rather than indexing it — it is keyed by descriptor, and the caller wants the
    /// other direction. O(distinct novel descriptors this process has seen), paid only by a
    /// dispatch that actually carries one.
    fn descriptors_of(&self, terms: &FxHashSet<TermId>) -> FxHashMap<TermId, Vec<u8>> {
        let state = lock_recover(&self.resolver_state);
        state
            .0
            .iter()
            .filter(|(_, id)| terms.contains(id))
            .map(|(descriptor, &id)| (id, descriptor.clone()))
            .collect()
    }

    fn resolve_terms(&self, dict: &Dict, descriptors: &[Descriptor]) -> Vec<TermId> {
        let mut state = lock_recover(&self.resolver_state);
        let (extension, next_extension_id) = std::mem::take(&mut *state);
        let mut resolver = DescriptorResolver::resume(dict, extension, next_extension_id);
        let ids = descriptors.iter().map(|d| resolver.resolve(d)).collect();
        *state = resolver.into_state();
        ids
    }

    /// How many of `rows` name an external id the live map already holds.
    ///
    /// **The backstop for a race the executor's own queue creates.** The
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
///   be fully applied and swapped in. Reporting it as not-ready tells an operator nothing happened
///   when a suppression may already be in force, which is the fail-open the deny lane exists to
///   prevent;
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
    /// A row's coordinates fall outside the slice's declared quantisation extent, so the point has
    /// no cell to occupy — refused **before anything is acked or WAL-durable**, and refused rather
    /// than clamped (see [`Quantisation::contains`]).
    ///
    /// Checked here, at the engine's own ingest boundary, rather than in an HTTP handler: the
    /// invariant is *every buffered row has a cell*, which is a fact about the buffer, and the
    /// buffer has more than one writer. A check guarding only the HTTP path leaves the bench arms,
    /// the tests and any future ingest route writing points the quantiser will silently clamp onto
    /// the edge of the grid.
    OutsideExtent {
        index: usize,
        x: f32,
        y: f32,
        quantisation: tessera_store::manifest::Quantisation,
    },
}

impl std::fmt::Display for AcceptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AcceptError::Submit(e) => write!(f, "{e}"),
            AcceptError::Exec(e) => write!(f, "{e}"),
            AcceptError::OutsideExtent {
                index,
                x,
                y,
                quantisation: q,
            } => write!(
                f,
                "ingest row {index} at ({x}, {y}) is outside this slice's declared extent (x {}..{}, \
                 y {}..{}). Coordinates are quantised against that extent, which is fixed for the \
                 slice's life (decision 0040), so an out-of-extent point has no cell to occupy; it \
                 is refused here rather than clamped, because a clamped point at the boundary \
                 cannot be told from one that belongs there. The remedy is to rebuild the slice \
                 under a corrected extent, which is a migration",
                q.x_min, q.x_max, q.y_min, q.y_max
            ),
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
    ///
    /// `manifest_high_water` is `max(build MANIFEST, side-manifest)` — see the caller. The WAL's
    /// own high-water is unioned with it here, and stops being available once rotation reclaims the
    /// records it derives from.
    ///
    /// `initial_deny` is the side-manifest's own deny state — its `deny` (suppressions) and
    /// `tombstones` (deleted entities), which contracts §2.3 makes complete current state for the
    /// partition rather than a diff. It seeds the overlay **before** replay, and WAL replay
    /// unions on top: where the two differ the WAL is the superset and wins, and dispositions are
    /// idempotent, so the union is well-defined. Without this seeding the reader honours `deny` in
    /// name only — the manifest opens and every entity it names is served.
    pub(crate) fn reconstruct(
        wal_path: &Path,
        manifest_high_water: u64,
        dict: &Dict,
        initial_deny: &[(EntityId, ChangeOp)],
        resolve_from_bundle: impl Fn(&[u8]) -> std::result::Result<Option<EntityId>, StoreError>,
        has_row: impl Fn(EntityId) -> bool,
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

        // **The manifests' deny state is the starting point, and replay runs over it.** Ordering,
        // not aesthetics — see `replay`'s own doc: every WAL record postdates any state an
        // honourable manifest carries, and the one op that needs the later record to win is
        // `Unsuppress`. Seeding afterwards silently reverts an acked unsuppress on any restart in
        // the publication gap.
        let mut seed = Overlay::new();
        for (entity, op) in initial_deny {
            seed.apply(*entity, *op, None);
        }

        let (overlay, mut buffer, established, resolver) =
            replay(&records, dict, seed, resolve_from_bundle).map_err(EngineError::Overlay)?;

        // **The buffer holds exactly the rows that have no geometry, and this is where that becomes
        // true.** Replay walks every retained WAL record, including the `IngestBatch` rows of every
        // flush whose member has not yet been reclaimed — so without this the buffer comes back
        // holding rows that already have segments, and the next flush writes each of them a second
        // time under a second entity's worth of geometry.
        //
        // **The test is `row_of`, not a watermark.** A watermark is a cheap scalar proxy for "has a
        // row", exact only while entity-allocation order and flush order coincide — that is, while
        // there is one slice per partition, which flush §2.1 records as load-bearing and unenforced.
        // The predicate below is what the watermark approximates, so it stays exact at any number of
        // slices and needs no per-slice bookkeeping anywhere.
        //
        // It is also what `compose::verdict` now relies on. That function used to gate rule 4 on
        // `entity < watermark` to stop a stale buffer answering for an entity the fragment already
        // covers; the gate is gone, and this invariant is what replaces it.
        let already_flushed: Vec<EntityId> = buffer
            .iter()
            .map(|(entity, _)| *entity)
            .filter(|entity| has_row(*entity))
            .collect();
        if !already_flushed.is_empty() {
            tracing::debug!(
                count = already_flushed.len(),
                "WAL rows that already have geometry were not re-buffered"
            );
        }
        for entity in already_flushed {
            buffer.remove(entity);
        }

        // **Where each surviving row sits in the log**, so a rotation knows what it may reclaim
        // below (flush §7.3). Stamped after the filter rather than before it, because a row that
        // already has geometry is gone from the buffer and stamping it would be a lookup for
        // nothing.
        //
        // The positions are parallel to the records — same order, same length — which is what
        // `Wal::replayed_positions` guarantees. An `IngestBatch` holds a whole window's rows, so
        // every row in one record shares its position; that is exactly right, since reclaiming
        // below the record is what would lose them.
        for (record, position) in records.iter().zip(wal.replayed_positions()) {
            if let WalRecord::IngestBatch { rows, .. } = record {
                for row in rows {
                    buffer.set_wal_pos(row.entity_id, *position);
                }
            }
        }

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
    pub(crate) fn new(state: WritePathState) -> Self {
        WritePath {
            live: Arc::new(LiveState {
                allocator: Mutex::new(state.allocator),
                established: Mutex::new(state.established),
                established_inverse: Mutex::new(state.established_inverse),
                resolver_state: Mutex::new(state.resolver_state),
                accepted_batches: Mutex::new(state.accepted_batches),
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
        row_projection_cache: Arc<RowProjectionCache>,
        queue_bound: usize,
        flush: FlushDeps,
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
        // Completed flushes have their own, unbounded channel — see `Executor::flush_done`.
        let (flush_tx, flush_rx) = std::sync::mpsc::channel();

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
                    row_projection_cache,
                    queues: LifecycleQueues {
                        work: work_rx,
                        deny: deny_rx,
                        bell: bell_rx,
                    },
                    health: Arc::clone(&health),
                    window_seq: 0,
                    flush_max_age_secs: flush.max_age_secs,
                    flush_in_flight: Arc::new(AtomicBool::new(false)),
                    flush_attempt: 0,
                    // Above every candidate present at open, per partition — see the field's doc.
                    next_manifest_n: flush.next_manifest_n,
                    prefix_dir: flush.prefix_dir,
                    identity_key: flush.identity_key,
                    pool: flush.pool,
                    max_distinct_terms: flush.max_distinct_terms,
                    flush_done: flush_rx,
                    flush_submit: flush_tx,
                    last_tick: std::time::Instant::now(),
                    #[cfg(feature = "fault-injection")]
                    faults: thread_faults,
                };
                // Declared LAST so it drops FIRST during unwind: the posture reaches `Dead` before
                // the receivers disconnect, so a **subsequent** submitter cannot see
                // `ExecutorDead` while `readyz` still reports ready.
                //
                // **It does not order the posture against the IN-FLIGHT submitter**, which is the
                // reading to resist.
                // `Job { command, respond }` is destructured into `Executor::execute`'s frame, so
                // the in-flight `Responder` drops *earlier* in the unwind than this guard: that
                // caller's `rx.recv()` can return before the posture moves. The consequence that
                // matters is that the caller's error must be `SubmitError::ReceiptLost` — mapped to
                // a fail-closed 500 rather than 503 — which is correct *regardless* of the posture,
                // because the command may be fully applied. `tests/write.rs`'s
                // `an_executor_panic_is_reported_dead` asserts both halves and pins that error at
                // its producer.
                //
                // **This ordering does not make a `/readyz` test a race** either: the lifecycle axis
                // is published with `fetch_max` and answered before the WAL flag, so `Dead` is
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

    // --- read accessors ---------------------------------------------------------------------------

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
    /// Submit a geometry publication to the executor and block until it has been performed.
    ///
    /// Rides the work lane and is never shed — see [`LifecycleHandle::publish_geometry`].
    pub(crate) fn publish_geometry(
        &self,
        prefix: String,
        segments_version: u64,
        watermark: u64,
        bundle: Arc<Bundle>,
        dict: Arc<Dict>,
        delta_postings: Vec<Arc<DeltaTier>>,
    ) -> std::result::Result<(), PublishGeometryError> {
        self.handle
            .as_ref()
            .ok_or(PublishGeometryError::NoExecutor)?
            .publish_geometry(
                prefix,
                segments_version,
                watermark,
                bundle,
                dict,
                delta_postings,
            )
    }

    pub(crate) fn resolve_terms(&self, dict: &Dict, descriptors: &[Descriptor]) -> Vec<TermId> {
        self.live.resolve_terms(dict, descriptors)
    }

    // --- submission -----------------------------------------------------------------------------

    /// Submit an ingest batch and wait for its receipt.
    ///
    /// **Blocking**, so a tokio handler must call this inside `spawn_blocking` — `tessera-engine`
    /// has no tokio dependency and must not acquire one (lifecycle §7's sync-engine rule, policed
    /// by `scripts/check-layers.sh`'s `deny tessera-engine tokio`).
    ///
    /// Rows arrive **unallocated**: entity ids are assigned on the executor, at the close of the
    /// commit window this submission lands in.
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
    ///
    /// **This is the one-item shape.** A caller with a whole request's worth of changes wants
    /// [`WritePath::submit_change`], because waiting here between items is what reduces the deny
    /// lane's group commit to one entry per window.
    pub(crate) fn accept_change(
        &self,
        external_id: Vec<u8>,
        entity: EntityId,
        op: ChangeOp,
        raw_descriptors: Option<Vec<Vec<u8>>>,
    ) -> Result<(), AcceptError> {
        self.submit_change(external_id, entity, op, raw_descriptors)?
            .wait()
    }

    /// Enqueue one `/control/changes` entry **without waiting for its receipt**.
    ///
    /// The point of the separation is at [`LifecycleHandle::enqueue`]: a caller that enqueues a
    /// whole request and only then collects gives the executor the queue depth its deny window
    /// needs, and one request of N denies costs one fsync instead of N. Read that doc before
    /// treating either half's `Err` as "nothing happened" — the boundary is not the proven
    /// non-enqueue boundary.
    pub(crate) fn submit_change(
        &self,
        external_id: Vec<u8>,
        entity: EntityId,
        op: ChangeOp,
        raw_descriptors: Option<Vec<Vec<u8>>>,
    ) -> Result<PendingChange, AcceptError> {
        let pending = self.handle()?.enqueue(Command::Change {
            external_id,
            entity,
            op,
            descriptors: raw_descriptors,
        })?;
        Ok(PendingChange(pending))
    }
}

/// An enqueued `/control/changes` entry, awaiting its receipt.
///
/// Deliberately **not** a re-export of [`Pending`]: a change's receipt carries no ids, so the only
/// thing a caller can do with it is learn whether the change took hold, and this type says exactly
/// that in its `wait` signature. `Ack::Ingested` reaching a change's caller would be a bug the
/// wider type would not catch.
pub struct PendingChange(Pending);

impl PendingChange {
    /// Block until the executor answers this change.
    ///
    /// `Err` does **not** mean "nothing happened" — for `Delete`/`Suppress` see [`ExecError::Wal`],
    /// and for [`SubmitError::ReceiptLost`] see [`Pending::wait`].
    pub fn wait(self) -> Result<(), AcceptError> {
        match self.0.wait()?.outcome {
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
    /// directory: intermittent `ENOENT` from the sidecar rename, in tests spread across many files.
    /// An inline write path closes the WAL synchronously on drop and needs none of this; moving the
    /// WAL onto a thread is what creates the obligation.
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
/// The responder travels **with** the command rather than being looked up afterwards, because a
/// window entry that has been joined by a retry holds several of them. It is an
/// [`ack::Responder`], not a raw
/// sender — see that module for why the difference is the whole of the ack-ordering guarantee.
pub(crate) struct Job {
    command: Command,
    respond: Responder,
}

/// What the executor's **work** lane carries: a lifecycle command, or a geometry publication.
///
/// **The split is deliberate, and the store-shaped half cannot live in `tessera-lifecycle`.**
/// `command.rs`'s module doc says why: that crate has no `tessera-store` dependency and must not
/// acquire one (a cycle cargo refuses), so a `Command` variant carrying an `Arc<Bundle>` is not
/// expressible there. `Command` stays entity-space and store-free; this enum is
/// `tessera-engine`'s own executor vocabulary, and it exists so that the executor thread is the
/// **only** publisher of a generation (lifecycle §1.3). Before it, `Engine::publish_geometry`
/// swapped the pointer itself from whatever thread called it — a second publisher whose
/// compare-and-swap could not stop the executor's own `store` from clobbering it.
///
/// A publication carries its own response channel rather than a [`Responder`]: its answer is a
/// `Vec<Reclaimed>`, which is engine-local and has no place in [`Ack`].
pub(crate) enum ExecutorWork {
    Lifecycle(Job),
    PublishGeometry {
        prefix: String,
        segments_version: u64,
        watermark: u64,
        bundle: Arc<Bundle>,
        dict: Arc<Dict>,
        delta_postings: Vec<Arc<DeltaTier>>,
        respond: SyncSender<std::result::Result<(), GeometryRefused>>,
    },
}

/// Why a geometry publication produced no answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishGeometryError {
    /// The live generation refused it — see [`GeometryRefused`].
    Refused(GeometryRefused),
    /// There is no write executor to publish through. **Not a refusal of the geometry**: a
    /// publication is a swap on the executor thread, so an engine that never started one cannot
    /// publish at all. Reachable only by an embedder that skipped `start_write_executor`;
    /// `tessera-server` starts it unconditionally.
    NoExecutor,
}

impl std::fmt::Display for PublishGeometryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PublishGeometryError::Refused(refused) => write!(f, "{refused}"),
            PublishGeometryError::NoExecutor => f.write_str(
                "this engine has no write executor, and a geometry publication is a swap on that                  thread (lifecycle §1.3)",
            ),
        }
    }
}

impl std::error::Error for PublishGeometryError {}

/// The handler-side end of the write executor: two queues, and the asymmetry between them.
///
/// **Not `Clone`, and that is load-bearing** — [`WritePath::drop`] joins the executor thread, which
/// terminates only when every sender has disconnected. One owner means the join always completes.
pub(crate) struct LifecycleHandle {
    /// Bounded by `ingest_queue_bound`; full → [`SubmitError::QueueFull`] for an ingest, and a
    /// **blocking** send for a geometry publication, which is not a client request and may not be
    /// shed (lifecycle §1.3: completed units arrive on the work lane, never the deny lane).
    work: SyncSender<ExecutorWork>,
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
    /// while `Command::Ingest` rides the bounded one and can. **There is deliberately no
    /// `submit_deny` beside this.** Its body would be byte-identical, so a "never 429" doc on it
    /// would describe a by-call-site rule that does not exist and be false of itself in both
    /// directions — and a second name for one behaviour is how a later author comes to believe the
    /// lane follows the call.
    ///
    /// Both lanes can still report [`SubmitError::ExecutorDead`]. A deny is never refused for
    /// *load*, which is not the same as never refused; there is no honest 200 to give when there is
    /// nothing left to apply the write to.
    ///
    /// [`Command::is_never_shed`] is the rule, and this is the only place it is consulted.
    ///
    /// **Enqueue and wait are separable, and for denies they must be** — see [`Self::enqueue`].
    /// This is the one-command convenience over the two.
    pub(crate) fn submit(&self, command: Command) -> std::result::Result<Receipt, SubmitError> {
        self.enqueue(command)?.wait()
    }

    /// Hand `command` to the executor and return **without waiting for its receipt**.
    ///
    /// This is what makes group commit reachable for a caller with several commands. A caller that
    /// enqueues N commands and only then waits gives the executor N queued jobs to gather into one
    /// window; a caller that waits between each gives it one, and the window it can build has one
    /// entry in it. `/control/changes` is exactly that caller, and one fsync per item was the whole
    /// of its cost.
    ///
    /// ## What an `Err` from this function does and does not prove
    ///
    /// **It does not prove that nothing happened**, and a caller that treats it that way is
    /// fail-open on the deny lane. Two variants come out of here and they mean opposite things:
    ///
    /// - [`SubmitError::ExecutorDead`] — the `send` failed, and `send` hands the value back on
    ///   failure, so non-enqueue is **proven**.
    /// - [`SubmitError::ReceiptLost`] — the doorbell was disconnected, which happens only *after*
    ///   the job is already in a queue. [`Executor::run`]'s shutdown pass drains the deny lane and
    ///   **executes** it before it observes the disconnect, so the command may be durably in force.
    ///
    /// So the enqueue/wait boundary is not the proven/unproven boundary, and no caller may use
    /// "which half returned this" as the discriminator. [`SubmitError::may_have_taken_effect`] is
    /// the discriminator, and it is the same one `tessera-server`'s batch fold uses.
    ///
    /// The doorbell stays **here** rather than moving into [`Pending::wait`], which would remove
    /// the head-of-request race in which the executor commits a small first window while the caller
    /// is still enqueueing. It would also mean a `Pending` dropped without being waited on leaves
    /// its job queued with nothing to wake it — on an idle node, indefinitely. A deny that is
    /// silently never applied is a worse outcome than an extra fsync, so the ring stays at the
    /// enqueue and the residual race is measured rather than designed away.
    /// Submit a geometry publication and block until the executor has performed it.
    ///
    /// **A blocking `send`, not `try_send`.** A publication is not a client request and may not be
    /// shed for load: shedding one would leave a completed flush unpublished with nothing to retry
    /// it, and there is no 429 for a caller that is not a client. It rides the *work* lane
    /// regardless, never the deny lane — the loop drains deny to empty before touching work, which
    /// is what keeps a suppression from queueing behind a flush's IO (lifecycle §1.3).
    ///
    /// The bell is rung after the enqueue, exactly as [`Self::enqueue`] does and for the same
    /// reason: a token may be spurious, never missing.
    pub(crate) fn publish_geometry(
        &self,
        prefix: String,
        segments_version: u64,
        watermark: u64,
        bundle: Arc<Bundle>,
        dict: Arc<Dict>,
        delta_postings: Vec<Arc<DeltaTier>>,
    ) -> std::result::Result<(), PublishGeometryError> {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.work
            .send(ExecutorWork::PublishGeometry {
                prefix,
                segments_version,
                watermark,
                bundle,
                dict,
                delta_postings,
                respond: tx,
            })
            .map_err(|_| PublishGeometryError::NoExecutor)?;
        let _ = self.bell.try_send(());
        rx.recv()
            .map_err(|_| PublishGeometryError::NoExecutor)?
            .map_err(PublishGeometryError::Refused)
    }

    pub(crate) fn enqueue(&self, command: Command) -> std::result::Result<Pending, SubmitError> {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let job = Job {
            command,
            respond: Responder::new(tx),
        };

        if job.command.is_never_shed() {
            self.deny.send(job).map_err(|_| SubmitError::ExecutorDead)?;
            // Bumped **after** the enqueue and **before** the blocking wait, so a test can observe
            // "the deny is queued" as a condition rather than betting on a sleep.
            self.health.deny_submitted.fetch_add(1, Ordering::SeqCst);
        } else {
            self.work
                .try_send(ExecutorWork::Lifecycle(job))
                .map_err(|e| match e {
                    // Derived, not a placeholder — see [`estimate_retry_after_s`], which also
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

        Ok(Pending(rx))
    }
}

/// An enqueued command whose receipt has not been collected yet.
///
/// Holding one of these is what lets a caller with N commands have all N in the executor's queue at
/// once, which is the only condition under which the deny lane's group commit has anything to
/// gather ([`LifecycleHandle::enqueue`]).
pub(crate) struct Pending(Receiver<Receipt>);

impl Pending {
    /// Block until the executor answers.
    ///
    /// A dropped responder means the executor died **holding this job** — never `Ok`. Answering
    /// anything else here is the false-202 [`SubmitError`]'s own doc calls the worst available
    /// outcome.
    ///
    /// [`SubmitError::ReceiptLost`] because the ack is the **last** step:
    /// `append → fsync → apply → swap → ack` ([`Executor::commit_denies`]), so a death after the
    /// swap leaves a durable, in-force suppression with no receipt. Reporting that as "nothing was
    /// submitted" is how an operator comes to believe an item is still visible when it is not.
    pub(crate) fn wait(self) -> std::result::Result<Receipt, SubmitError> {
        self.0.recv().map_err(|_| SubmitError::ReceiptLost)
    }
}

/// The executor's end of the queues.
///
/// Named as a pair so the ordering rule is visible from the handle: `deny` is drained to empty
/// before `work` is touched, which is what makes the starvation bound "the work in front of this
/// deny" rather than "the work queue's depth". That unit is **one commit window**, and it holds for
/// *every* close: [`Executor::run_work_pass`] returns to `run`'s deny drain whenever it closes one,
/// which is what keeps the bound finite while ingest keeps arriving. A close that carried on
/// draining instead would make the bound the load rather than the window. The bound in full,
/// including the one case that costs two closes rather than one, is stated at
/// [`Executor::run_work_pass`].
pub(crate) struct LifecycleQueues {
    work: Receiver<ExecutorWork>,
    deny: Receiver<Job>,
    /// The wake signal. Capacity one — see [`LifecycleHandle::bell`] and [`Executor::run`].
    bell: Receiver<()>,
}

// =================================================================================================
// The executor
// =================================================================================================

/// What `/control/ingest`'s batch id already means to this executor — **the three states, in the
/// order they are looked up** (contracts §3.4, whose Appendix R r8 names the third:
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
    /// `window_seq` identifies the window the entry was found in. There is exactly one open window
    /// and it is consulted and joined in the same statement, so this is read by a
    /// `debug_assert!` and nothing else; it is the discriminator an executor holding more than
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
    /// copy that no external id names and no deny can reach — the same unreachable duplicate the
    /// window's conflict check exists for, arrived at by retention rather than by a race. Nothing
    /// prunes the WAL today (there is **no `wal_retention` config key**), so the
    /// caveat is latent rather than live; whoever adds retention inherits it, and the bound on
    /// the exposure is the idempotency window an operator's clients actually retry within.
    Unknown,
}

impl BatchState {
    /// Look `batch_id` up: **durable index first, then the open window, then unknown**.
    ///
    /// The two sets are disjoint — a batch id enters `accepted_batches` only at
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
    /// `Executor::run`'s deny drain (lifecycle §1.3).
    YieldedAfterClose,
}

/// The most entries one deny window may hold, and the most changes `/control/changes` enqueues
/// before it collects.
///
/// **It bounds two things and trades nothing.** Below it, a larger value only ever reduces fsyncs
/// and overlay clones; the only things a larger value costs are the size of a failed window's fold
/// and the pending receipts a caller holds. So this is not a knob an operator has a decision to
/// make about, and it is deliberately a constant rather than a configuration key.
///
/// The two bounds, in the order they bind:
///
/// 1. **The drain terminates.** Every entry the deny drain pulls is one a concurrent submitter can
///    replace, so a window with no bound need never close while denies keep arriving — and a window
///    that never closes is not a large window, it is no deny ever being acked. See
///    [`Executor::run_deny_pass`].
/// 2. **Pending receipts stay bounded.** `/control/changes` enqueues in chunks of this size, so the
///    deny runtime's pool (`DENY_MAX_BLOCKING_THREADS` handlers) holds at most that many times this
///    many one-slot channels, rather than that many times whatever fits in a request body.
///
/// One chunk covers the overwhelming majority of revocation requests, so the common case is one
/// window and one fsync. A maximal request body splits into a few tens of windows — against the
/// tens of thousands of fsyncs the per-item path charged for the same request.
pub const DENY_WINDOW_MAX_ENTRIES: usize = 1_000;

/// How long the executor waits before each re-attempt at making a deny window durable, and
/// therefore how many attempts there are: the first sync, plus one per entry here.
///
/// ## What bounds this, and why it is not the write-latency budget
///
/// Design §3's write-latency budget permits a deny to take seconds — up to a minute is acceptable —
/// so there is room. The bound is **not** taken from it, for a reason the budget does not express:
/// the executor is a single thread and the deny lane is FIFO, so this delay is paid by *every* deny
/// queued behind the failing window, not once by the caller who hit the failure. A schedule sized
/// to the budget would let one failing device convert the whole budget into the lane's per-window
/// cost, and the lane's guarantee — never starved beyond one window — is measured in exactly that.
///
/// So the bound is taken from the lane's own observed latency instead. A deny acks in ~3.2 ms
/// quiescent and 165 ms p50 / 346 ms max under sustained ingest
/// (`docs/evidence/memos/2026-08-01-deny-ack-baseline.md`, measured). At 250 ms of added delay a
/// failing window stays inside the range the lane already exhibits under load, so nothing queued
/// behind it waits longer than a busy node already makes it wait.
///
/// **Two re-attempts, not ten**, because of what the repair is: re-dirtying the pages and syncing
/// again (`tessera_lifecycle::wal::Wal::retry_durability`). That converts a transient writeback
/// error; it does nothing about a device that is actually failing. If the third attempt is refused,
/// further attempts are a cost with no mechanism behind them.
///
/// **The delays are not zero**, because the other failure a retry plausibly converts is a
/// short-lived `ENOSPC` — for which an immediate re-attempt is the one schedule guaranteed not to
/// help.
///
/// *Chosen against a measurement, not itself measured: no campaign has established how often a
/// second attempt succeeds, because that is a property of the device rather than of this code.*
const DENY_DURABILITY_BACKOFF: [std::time::Duration; 2] = [
    std::time::Duration::from_millis(50),
    std::time::Duration::from_millis(200),
];

/// How many durability attempts one deny window gets in total — the original sync plus one per
/// [`DENY_DURABILITY_BACKOFF`] entry.
///
/// Public because a test that wants to observe the *exhausted* path has to arm exactly this many
/// failures, and a test that hard-codes the number silently stops testing exhaustion the day the
/// schedule changes — it starts testing recovery instead, and passes either way.
pub const DENY_DURABILITY_ATTEMPTS: usize = DENY_DURABILITY_BACKOFF.len() + 1;

/// How often a degraded executor wakes to attempt recovery when no traffic would wake it
/// ([`Executor::wait_for_work`]).
///
/// **It bounds how long a node stays unready after its storage recovers, and nothing else.** The
/// attempt is two small file operations, so the cost of polling is negligible; the cost of polling
/// *too slowly* is an idle node steering traffic away from itself long after the fault cleared.
/// What the executor needs to run a flush, gathered rather than passed one by one.
///
/// A struct because the alternative is a ten-argument `start_executor`, where the compiler stops
/// distinguishing two `u64`s and a caller can transpose them silently.
pub(crate) struct FlushDeps {
    pub(crate) max_age_secs: u64,
    /// The bundle's current prefix directory. A flush writes inside it, and never touches
    /// `MANIFEST.json` or `CURRENT`.
    pub(crate) prefix_dir: PathBuf,
    pub(crate) identity_key: IdentityKey,
    /// D-D's one shared compute pool — a flush's segment write runs on it, off this thread,
    /// because this thread is the one that must reach a queued deny promptly (§1.1).
    pub(crate) pool: Arc<rayon::ThreadPool>,
    /// The plugin's declared `max_distinct_terms`, carried here because promotion (§3.2) is the
    /// one path by which a *caller* grows the dictionary, and so the one declared bound that is
    /// enforced rather than trusted. See `flush::promote`.
    pub(crate) max_distinct_terms: u64,
    /// One past the highest `SEGMENTS-<n>.json` this bundle carries — the executor's manifest
    /// counter seed. See [`Executor::next_manifest_n`].
    pub(crate) next_manifest_n: u64,
}

/// The bundle's declared scalar tail, as the segment writer wants it.
///
/// `None` if the manifest declares a type this build cannot write. A flush must **not** proceed
/// then: `columns.arrow`'s schema is the fixed columns plus this tail, so a dropped column would
/// produce a segment the reader refuses — and refusing to flush is the fail-closed answer, where
/// writing a short segment is a bundle that no longer opens.
fn scalar_schema_of(
    manifest: &tessera_store::manifest::Manifest,
) -> Option<Vec<(String, ScalarType)>> {
    manifest
        .declared_scalars
        .iter()
        .map(|d| {
            let ty = match d.arrow_type.as_str() {
                "u64" => ScalarType::U64,
                "f32" => ScalarType::F32,
                "utf8" => ScalarType::Utf8,
                _ => return None,
            };
            Some((d.name.clone(), ty))
        })
        .collect()
}

/// Every slice the bundle holds, across partitions. A flush plans per slice, because a segment's
/// entity range is contiguous only within one (§2.1).
/// Replace a manifest's deny fields with the overlay's live state.
///
/// **Serialised fresh at every write, never carried forward from another manifest.** A
/// side-manifest is complete current state (contracts §2.3), and the two fields are the only ones
/// whose truth lives outside the files the manifest names — so copying them from the manifest
/// being extended would publish whatever was true when *that* one was written, indefinitely, and
/// an unsuppress would never reach disc. The rule is one line here and it is the whole of what
/// keeps a manifest a projection of live state rather than an input to the next one.
///
/// The two fields are taken from the two bitmaps separately, never from `Overlay::denied`'s union:
/// they retire under different rules (lifecycle §3), and publishing the union under one field
/// would make every deletion look retirable by an unsuppress.
fn write_deny_state(manifest: &mut SegmentsManifest, overlay: &Overlay) {
    manifest.deny = overlay
        .suppressed_entities()
        .into_iter()
        .map(|entity_id| ManifestDenyEntry {
            entity_id,
            cause: "suppress".to_string(),
        })
        .collect();
    manifest.tombstones = overlay.deleted_entities();
}

/// The one plan a dispatch sends, chosen by **oldest unflushed row**.
///
/// Free and pure so the choice can be tested without an executor — and it is the choice, not the
/// dispatch, that carries the property. See `Executor::dispatch_flushes` for why one plan.
fn plan_to_dispatch(
    plans: Vec<(String, crate::flush::FlushPlan)>,
) -> Option<(String, crate::flush::FlushPlan)> {
    plans.into_iter().min_by_key(|(_, plan)| {
        // `items` is ascending by entity id and I9 issues ids monotonically, so the first is this
        // slice's oldest waiting row. An empty plan cannot occur (`plan_flush` returns
        // `NothingToFlush`), and sorting it last rather than first keeps a hypothetical one from
        // winning every tick.
        plan.items
            .first()
            .map_or(u64::MAX, |(entity, _)| entity.raw())
    })
}

/// Whether a completed flush's dictionary moved under it — see the call site in
/// [`Executor::publish_flush`] for the argument, and the scoping this encodes.
///
/// Pure so the **scoping** is testable: a flush that promoted nothing (`None`) is never discarded
/// however far the dictionary has moved, because its tier names only ordinals below the length it
/// planned against and append-only extension preserves those. Broadening this to every flush would
/// be a liveness hole bought for no safety.
fn dictionary_moved_under(promoted_from_dict_len: Option<u32>, live_len: u32) -> bool {
    promoted_from_dict_len.is_some_and(|planned| planned != live_len)
}

fn slices_of(generation: &Generation) -> Vec<String> {
    let mut slices: Vec<String> = generation
        .bundle
        .partitions
        .values()
        .flat_map(|p| p.slices.keys().cloned())
        .collect();
    slices.sort_unstable();
    slices.dedup();
    slices
}

/// A second is short against the interval an operator or an orchestrator would take to notice, and
/// long enough that a genuinely dead device is retried sixty times a minute rather than continuously.
///
/// It is not a latency bound on anything a caller sees: a degraded node still answers denies
/// immediately, and traffic arriving at any point wakes the loop through the doorbell as usual, so
/// a busy node attempts recovery far more often than this.
const WAL_RECOVERY_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// One deny in an open window: its record, and everything needed to apply it and answer its caller.
///
/// `record` is built at the drain rather than at the append so the window is a list of things that
/// are ready to be written — the append loop does no work that can be got wrong per entry.
struct DenyEntry {
    record: WalRecord,
    entity: EntityId,
    op: ChangeOp,
    raw_descriptors: Option<Vec<Vec<u8>>>,
    respond: Responder,
}

/// The single writer. One per partition, on its own thread, owning the WAL by value.
struct Executor {
    wal: ExecutorWal,
    live: Arc<LiveState>,
    /// **The only publishing capability in the write path.** Not in [`LiveState`], which the
    /// handler side shares.
    generation: Arc<GenerationHandle>,
    /// The row-projection cache, shared for the one thing this thread does with it: dropping the
    /// projections of generations now older than the retention depth. Runs at the swap — see
    /// `RowProjectionCache::prune_generations_below`.
    row_projection_cache: Arc<RowProjectionCache>,
    queues: LifecycleQueues,
    health: Arc<ExecutorHealth>,
    /// The last window's sequence number. [`BatchState::Held`] is what it is for; all it has to be
    /// is distinct per window.
    window_seq: u64,
    /// §4's `flush_max_age_secs` — the tick's period.
    flush_max_age_secs: u64,
    /// Whether a flush is executing on the pool. A tick arriving while it is set is skipped, never
    /// queued — two concurrent flushes would double-consume the buffer range (§1.1).
    flush_in_flight: Arc<AtomicBool>,
    /// Distinguishes two flush attempts at the same `segments_version` — see the `seg_id` this
    /// feeds.
    flush_attempt: u64,
    /// The next `SEGMENTS-<n>.json` number to write, for the single partition this executor
    /// publishes. Seeded at open from `highest_candidate_n + 1`.
    ///
    /// **One allocator, on the one thread that writes manifests.** `n` is per-partition, monotone
    /// and never reused (contracts §2.3), and it must be allocated by whoever writes at it: a
    /// number taken when a flush is *planned* is stale by the time that flush lands, because a
    /// deny publication may have taken one during its flight — and a flush committed beneath the
    /// newest manifest is a segment a restore never reads.
    ///
    /// Seeded from the highest *candidate*, not the served `n`, so a manifest stepped past for
    /// failing verification is never overwritten. `write_segments_manifest` refuses to replace in
    /// any case; seeding above means the refusal cannot arise.
    next_manifest_n: u64,
    /// The bundle prefix directory a flush writes into. A flush publishes **inside the current
    /// prefix** — never `MANIFEST.json`, never `CURRENT` — which is what separates it from a
    /// compaction.
    prefix_dir: PathBuf,
    identity_key: IdentityKey,
    /// The shared compute pool a flush executes on (§1.1), and the handle it submits its completed
    /// unit back through.
    pool: Arc<rayon::ThreadPool>,
    /// See [`FlushDeps::max_distinct_terms`].
    max_distinct_terms: u64,
    /// Completed flushes arriving from the pool (§1.1).
    ///
    /// **Its own channel, not the bounded work queue**, for two reasons. A completed flush may not
    /// be shed — there is no 429 for a unit whose files are already durable, and shedding one
    /// would leave a committed side-manifest with nothing publishing it. And `LifecycleHandle` is
    /// deliberately not `Clone` (`WritePath::drop` joins the thread, which needs one owner), so a
    /// pool task cannot hold one.
    ///
    /// Drained **after** the deny lane, exactly as work is: that ordering is what keeps a
    /// suppression from queueing behind a flush's publication.
    flush_done: Receiver<crate::flush::CompletedFlush>,
    /// The sender pool tasks are given a clone of.
    ///
    /// **Nothing rings the doorbell when a flush completes**, and that is deliberate twice over.
    /// A completed flush is picked up at the next tick, which is what §1.3 requires anyway — every
    /// geometry publication is on one cadence — so ringing would only publish *off* it. And an
    /// executor holding a clone of its own doorbell sender would keep the bell channel alive for
    /// ever, so `wait_for_work` would never observe the disconnect and `WritePath::drop`'s join
    /// would hang: the shutdown path depends on this thread owning no sender of its own.
    flush_submit: Sender<crate::flush::CompletedFlush>,
    /// When the last tick fired. Started at construction, so the first tick is one period after
    /// the executor starts rather than immediately at startup.
    last_tick: std::time::Instant,
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
    /// bounded by the window in front of it — at most two, see that function for the case — rather
    /// than by queue depth. The consequences are chosen: a sustained
    /// deny flood starves ingest completely, and the deny queue is unbounded in memory.
    ///
    /// **Why a deny may safely overtake a queued ingest.** Reordering execution relative to
    /// submission looks like it should break replay equivalence (a live `suppress` replaying before
    /// the ingest that established its target). It cannot: `/control/changes` resolves its
    /// `external_id` against the live map in the *handler* and 404s if the item is not established
    /// yet, and an item is established only at apply. So no deny naming a still-queued ingest's
    /// item can be submitted at all, and WAL append order still equals apply order.
    ///
    /// **The cost of that, stated because it is a real operator-visible gap and nothing closes it.**
    /// An operator issuing `suppress D` while D's ingest is still held gets **404 unknown external
    /// id**; D then becomes visible, unsuppressed, and the operator has to notice and retry. The
    /// commit window widened that interval from one command's fsync to a whole window. Putting deny
    /// dispositions in the window would not close it either — the deny is refused in the handler and
    /// never reaches a lane — so this is not an argument for the mixed window; it is an argument for
    /// an operator who is revoking during a bulk load to verify rather than to trust a 404.
    ///
    /// **Shutdown drains and executes; it does not discard.** The loop leaves only from
    /// `bell.recv()`, which sits *after* both `try_recv`s, so the disconnect iteration has already
    /// drained deny to empty and run one work item. Anything genuinely left behind — work queued
    /// beyond that one item — is dropped with the receivers, and had no waiter left to ack anyway:
    /// a submitter holds `&self` on the handle for the whole call, so `bell.recv()` cannot return
    /// `Err` while any submit is in flight, since the three senders live in one struct and
    /// disconnect together.
    ///
    /// "At most one work item" is one *pass*, and a pass drains work into a commit window
    /// ([`Executor::run_work_pass`]). Leftover doorbell tokens stay harmless under that: the drain
    /// takes every job visible to its `try_recv`, so a token is still only ever discarded while a
    /// job is still visible.
    fn run(&mut self) {
        loop {
            self.recover_wal();
            // **Completed flushes are applied before the tick plans another**, and the order is
            // load-bearing: until a flush is published its items are still in the buffer, so a tick
            // that planned first would re-plan the very rows the completed unit already wrote.
            let published = self.publish_completed_flushes();
            self.tick_if_due();
            while self.run_deny_pass() {}
            if self.run_work_pass() || published {
                continue;
            }
            if !self.wait_for_work() {
                break;
            }
        }
    }

    /// **The flush tick** (§1.3): the one cadence on which geometry is published.
    ///
    /// Runs at the top of the loop, *before* the deny drain, so a tick is never delayed by work
    /// that arrived after it came due — and after it, because a tick that publishes must not
    /// preempt a deny already queued (lifecycle §1.3's priority lane).
    ///
    /// **Three publishers reach this cadence and none publishes off it.** The tick itself;
    /// `flush_max_items`, which marks the buffer flush-*ready* and waits (publishing on trip would
    /// move the real period below the one §4's relation 1 validated, and §2.2's depth trim would
    /// then drop pins before their TTL while the depth alarm saturates); and
    /// `POST /control/flush`, accepted at any time and executed here, its 202 already meaning
    /// "accepted, not yet done".
    ///
    /// It also drives `reclaim` — lifecycle §2.1 assigns that gap to "whichever stage introduces
    /// a periodic publisher", and this is that publisher.
    fn tick_if_due(&mut self) {
        let period = std::time::Duration::from_secs(self.flush_max_age_secs);
        if self.last_tick.elapsed() < period {
            return;
        }
        self.last_tick = std::time::Instant::now();
        self.health.ticks.fetch_add(1, Ordering::Relaxed);
        // Requested flushes are consumed by the tick whether or not there is anything to flush: a
        // `POST /control/flush` against an empty buffer is satisfied by the tick it named, not
        // held until something arrives.
        self.health.flush_requested.store(false, Ordering::SeqCst);

        // **Planned on this thread, executed on the pool.** The plan — which buffered items
        // acquire geometry and what the three dispositions do to them (§3.5) — is the
        // invariant-bearing half and is taken against the live generation here; the segment write
        // and the publication follow through `dispatch_flushes`. The count it produces on the way
        // is what an operator needs to see a stalled flush: items that *would* acquire geometry at
        // this tick, which stays at zero on a gated node and grows without bound on one whose
        // flush is failing.
        let generation = self.generation.load_full();
        let mut flushable = 0usize;
        let mut plans: Vec<(String, crate::flush::FlushPlan)> = Vec::new();
        for slice in slices_of(&generation) {
            match crate::flush::plan_flush(
                &generation,
                &slice,
                self.wal.is_poisoned(),
                self.health.overlay_diverged.load(Ordering::SeqCst),
            ) {
                Ok(plan) => {
                    flushable += plan.items.len();
                    plans.push((slice, plan));
                }
                Err(crate::flush::NoFlush::NothingToFlush) => {}
                Err(gate) => {
                    // Per tick, and deliberately: a gated node is gated until an operator acts, and
                    // the tick is the interval at which that is worth repeating.
                    tracing::warn!(
                        slice = %slice,
                        gate = ?gate,
                        "flush skipped: this node publishes no geometry in this state"
                    );
                }
            }
        }
        self.health
            .flushable_items
            .store(flushable, Ordering::SeqCst);

        // **At most one flush in flight.** A tick arriving while one runs is *skipped, not queued*:
        // two concurrent flushes would double-consume the buffer range. Skips are counted and
        // alarmed, because a flush persistently slower than the tick is a visibility-latency
        // breach that `flush_max_age_secs` would otherwise silently miss.
        if !plans.is_empty() {
            if self.flush_in_flight.load(Ordering::SeqCst) {
                self.health.flush_skips.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    "ALARM: a flush was still running when the next tick came due, so this tick \
                     published nothing. The effective publication period is longer than \
                     flush_max_age_secs, which is a visibility-latency breach"
                );
            } else {
                self.dispatch_flushes(&generation, plans);
            }
        }
        drop(generation);
    }

    /// Apply every completed flush waiting from the pool, and report whether any did.
    ///
    /// Drained after the deny lane and before work, so a publication never delays a suppression
    /// and never waits behind a commit window.
    fn publish_completed_flushes(&mut self) -> bool {
        let mut any = false;
        while let Ok(completed) = self.flush_done.try_recv() {
            self.publish_flush(completed);
            any = true;
        }
        any
    }

    /// Hand each plan to the background pool, and mark a flush in flight until all of them land.
    ///
    /// **Execution is off this thread** (§1.1). The segment write is file IO of unbounded duration,
    /// and this thread is the one that drains the deny lane to empty before it touches work — so a
    /// flush executed inline would put a suppression behind it, which is precisely what lifecycle
    /// §1.3's priority lane exists to prevent.
    ///
    /// Every input is taken here, on this thread, against the live generation and then moved: the
    /// pool holds no reference to live state, which is what makes "over immutable inputs" true.
    fn dispatch_flushes(
        &mut self,
        generation: &Arc<Generation>,
        plans: Vec<(String, crate::flush::FlushPlan)>,
    ) {
        let submit = self.flush_submit.clone();
        let Some((partition, partition_data)) = generation.bundle.partitions.iter().next() else {
            return;
        };
        let manifest = &generation.bundle.manifest;
        let Some(scalar_schema) = scalar_schema_of(manifest) else {
            tracing::error!(
                "ALARM: this bundle declares a scalar type this build cannot write, so no flush \
                 can produce a segment whose columns.arrow matches its schema. Ingest stays \
                 durable and invisible until the binary understands it"
            );
            return;
        };
        // **One plan per dispatch.** Every context a dispatch builds takes `next_n` from the same
        // unchanging `partition_data`, so they would all write `SEGMENTS-<next_n>.json` at one
        // path and only one could commit. Dispatching one makes that structurally unreachable and
        // saves the losers' segment writes; `write_segments_manifest`'s refuse-to-replace stands
        // behind it at the format boundary. The rest re-plan at the next tick, against a
        // `segments_version` the winner has advanced.
        //
        // **Chosen by oldest unflushed row, not by slice name.** `slices_of` sorts
        // lexicographically, so taking the first would let a continuously-fed `s0` deny `s1` a
        // flush for ever. `items` is ascending by entity id and I9 issues ids monotonically, so
        // `items.first()` is an age key needing no cursor state — which turns starvation into a
        // bound: with `s` slices, ack→visibility is at most `s × flush_max_age_secs`.
        //
        // **Unreachable today**: `tessera build` emits one slice, and a plan naming a slice this
        // bundle does not carry is dropped just below.
        let deferred = plans.len().saturating_sub(1);
        let Some((slice, plan)) = plan_to_dispatch(plans) else {
            return;
        };
        if deferred > 0 {
            tracing::warn!(
                deferred,
                dispatched = %slice,
                "a flush unit is per slice and every plan in a dispatch shares one side-manifest \
                 name, so one slice publishes per tick; the rest re-plan at the next one"
            );
        }

        let mut contexts = Vec::with_capacity(1);
        {
            let Some(slice_data) = partition_data.slices.get(&slice) else {
                return;
            };
            let Ok(row_base) = u32::try_from(slice_data.row_space.total_rows()) else {
                // Row ids are `u32` (bundle_format 1). A slice that has crossed 2^32 rows cannot
                // take another segment, and saying so is better than wrapping into row 0.
                tracing::error!(
                    slice = %slice,
                    "ALARM: this slice's row space has reached the u32 ceiling; no further flush \
                     can address it. The deployment must be compacted or re-sharded"
                );
                return;
            };

            // **The descriptor bytes behind this plan's extension term ids** (§3.2), and the whole
            // of what promotion needed that the buffer does not hold. Lazy on purpose: in the
            // steady state every descriptor is already interned, `novel` is empty, and this takes
            // no lock and allocates nothing — one comparison per term is the entire cost.
            let dict_len = generation.dict.len();
            let novel: FxHashSet<TermId> = plan
                .items
                .iter()
                .flat_map(|(_, item)| item.terms.iter().copied())
                .filter(|term| term.raw() >= dict_len)
                .collect();
            let novel_descriptors = if novel.is_empty() {
                FxHashMap::default()
            } else {
                self.live.descriptors_of(&novel)
            };

            // **A label, not an allocation.** `n` is allocated by the executor at publication
            // (`next_manifest_n`), because a deny publication may take one while this flush is in
            // flight. What the plan needs is a component that makes `seg_id` unique, and the
            // sequence it was planned against is exactly that: contracts §2.1's never-reused
            // property rests on this plus the attempt counter, as before.
            let planned_at_n = partition_data.segments_n;
            contexts.push((
                plan,
                crate::flush::FlushContext {
                    prefix_dir: self.prefix_dir.clone(),
                    partition: partition.clone(),
                    slice: slice.clone(),
                    // **`seg_id`s are never reused** (contracts §2.1), which is what makes the
                    // merge rebase ABA-safe — and the attempt counter is not decoration. `next_n`
                    // alone repeats whenever a flush is planned twice before it publishes, and the
                    // second attempt would then `File::create` over files the first has memory
                    // mapped: a truncated mapping, and SIGBUS on the next read of it. The counter
                    // makes every attempt's path distinct, so a re-plan writes beside the earlier
                    // one rather than through it, and the loser's files are orphans nothing
                    // references.
                    seg_id: format!("flush-{planned_at_n}-{}", self.next_flush_attempt()),
                    row_base,
                    identity_key: self.identity_key,
                    shard_id: manifest.identity.shard_id,
                    quantisation: manifest.quantisation,
                    scalar_schema: scalar_schema.clone(),
                    dict: Arc::clone(&generation.dict),
                    novel_descriptors,
                    max_distinct_terms: self.max_distinct_terms,
                    prefix: generation.prefix.clone(),
                },
            ));
        }
        if contexts.is_empty() {
            return;
        }

        self.flush_in_flight.store(true, Ordering::SeqCst);
        let in_flight = Arc::clone(&self.flush_in_flight);
        let health = Arc::clone(&self.health);
        self.pool.spawn(move || {
            for (plan, ctx) in contexts {
                match crate::flush::execute_flush(plan, ctx) {
                    Ok(completed) => {
                        // A send failure means the executor is gone, which is a shutdown and not a
                        // fault: the files are orphans nothing references, and replay re-flushes.
                        let _ = submit.send(completed);
                    }
                    Err(e) => {
                        // **Nothing happened, retry next tick** (§10). The side-manifest is the
                        // only commit point, so a failure before it leaves orphan files nothing
                        // references and the buffer intact.
                        health.flush_failures.fetch_add(1, Ordering::Relaxed);
                        tracing::error!(
                            error = %e,
                            "ALARM: a flush failed; the buffer is retained and it will be retried \
                             at the next tick. Sustained failure grows the buffer until \
                             ingest_buffer_max_items sheds ingest, which is the intended \
                             backpressure"
                        );
                    }
                }
            }
            in_flight.store(false, Ordering::SeqCst);
        });
    }

    /// If the WAL is degraded and the degradation is one a discard can end, end it.
    ///
    /// ## Why a node must be able to leave `WalPoisoned` without a restart
    ///
    /// The causes are transient at least as often as they are terminal — a filesystem that filled
    /// and was relieved, a device that stumbled — and the previous behaviour latched the node
    /// unready for the life of the process over any of them. That is an outage the storage did not
    /// cause. Denies were never blocked by it (they are applied in memory and answered 500 whatever
    /// the posture says, and nothing gates `/control/changes` on readiness), but routing was, and a
    /// node that will not take reads again until someone notices is not fail-closed, it is just
    /// down.
    ///
    /// ## Why the recovery discards rather than repairs
    ///
    /// By the time this runs, every caller of the region above the durable boundary has been told
    /// its write is not durable. Making those bytes durable *afterwards* is fail-open in both lanes:
    /// a refused ingest reappears, and an exhausted deny window's `unsuppress` — appended like every
    /// other entry but deliberately not applied in memory — takes effect at the next replay, undoing
    /// a suppression whose operator was told it still stood. So the region is discarded, which is
    /// precisely what a restart would do with the same file
    /// (`tessera_lifecycle::wal::Wal::discard_undurable`). Nothing is retained to make it possible
    /// and both halves of a sync failure are covered: the bytes go whether or not they reached the
    /// device.
    ///
    /// **What does not recover.** A torn append. There is no repair for it here and the WAL offers
    /// none, so such a node stays `WalPoisoned` until it is restarted — which is the honest answer,
    /// since a partial `write_all` leaves neither the file's contents nor the descriptor's position
    /// known.
    ///
    /// **What it costs a healthy node: one bool read per loop iteration**, and the loop iterates
    /// only when there was work or a wake-up. Everything below the guard is unreachable while the
    /// WAL is fine.
    fn recover_wal(&mut self) {
        if !self.wal.is_poisoned() {
            return;
        }
        if !self.wal.is_recoverable() {
            return;
        }
        // A failure here leaves the handle exactly as it was, so the next pass tries again. It is
        // deliberately silent about failing: this runs on a timer while degraded, and a log line per
        // attempt would turn one storage fault into an unbounded stream of them.
        if self.wal.discard_undurable().is_ok() {
            // **The overlay has now diverged from the durable WAL, and stays diverged.** The
            // discard did not un-apply anything (`Wal::discard_undurable` says why), so every
            // deletion and suppression applied under lifecycle §4's apply-anyway rule is in force
            // in memory with no record behind it. Publishing a flush manifest or rotating the WAL
            // from that overlay would make a 500'd, never-acked deny permanent — contradicting
            // contracts §3.1's residual, which is that a restart does *not* carry it.
            //
            // So the node keeps serving and keeps applying denies, and publishes nothing, until an
            // operator restarts it. That costs ingest visibility and is alarmed for exactly that
            // reason: it is an operator's decision rather than a silent stall. Converging by
            // re-appending was the alternative and is rejected — it produces a state no restart
            // could have produced, which is lifecycle §4's central argument.
            if !self.health.overlay_diverged.swap(true, Ordering::SeqCst) {
                tracing::error!(
                    "ALARM: this node recovered its WAL in process, so its overlay now holds \
                     dispositions no durable record backs. It keeps serving and keeps applying \
                     denies, but publishes NO flush and rotates NO WAL until restarted — ingest \
                     stops becoming visible. Restart this node."
                );
            }
        }
        self.observe_wal();
    }

    /// Block until something may be waiting, and report whether the executor should keep running.
    ///
    /// **While the WAL is degraded this wakes on a timer as well as on the doorbell**, because
    /// otherwise recovery would be reachable only by traffic: a node whose disk recovered during a
    /// quiet period would stay unready until something arrived to wake it, and `/readyz` steers
    /// traffic away from exactly that node. The poll runs only while degraded, so a healthy
    /// executor blocks indefinitely exactly as it did.
    ///
    /// Shutdown is unchanged and still leaves only from here, after both queues have been observed
    /// empty: a timeout resumes the loop, and only a disconnect ends it.
    fn wait_for_work(&self) -> bool {
        // **Bounded by the next tick, always.** An unbounded `recv` here is what an idle node used
        // to do, and with a periodic publisher it is wrong: the tick would fire only when traffic
        // happened to wake the loop, making visibility latency a function of load rather than of
        // `flush_max_age_secs`, and leaving reclaim un-run on exactly the quiescent node
        // lifecycle §2.1 describes.
        //
        // A poisoned WAL wants a shorter wait than the tick, so the two take the smaller.
        let until_tick = std::time::Duration::from_secs(self.flush_max_age_secs)
            .saturating_sub(self.last_tick.elapsed());
        let wait = if self.wal.is_poisoned() {
            until_tick.min(WAL_RECOVERY_POLL_INTERVAL)
        } else {
            until_tick
        };
        // Shutdown is unchanged and still leaves only from here, after both queues have been
        // observed empty: a timeout resumes the loop, and only a disconnect ends it.
        !matches!(
            self.queues.bell.recv_timeout(wait),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
        )
    }

    /// **The deny window**: gather the queued denies into one committable unit and commit it.
    /// Returns whether anything was found, which is what keeps [`Executor::run`] draining before it
    /// blocks.
    ///
    /// ## Why this exists
    ///
    /// One `/control/changes` request of N denies used to cost N `append → fsync → apply → swap`
    /// cycles — measured at one fsync per item and ~300 denies/second, i.e. tens of minutes for a
    /// bulk revocation, with every other deny behind it and ingest starved throughout. The fsync is
    /// only half of it: a per-item path clones the whole [`Overlay`] each time, and the overlay
    /// never shrinks — there is no compaction fold (⊘) — so an N-item revocation
    /// also copies Θ(N²) entries. A window pays both once.
    ///
    /// It takes **two** halves to get that, and neither works alone. This is the executor half; the
    /// other is that `/control/changes` enqueues its whole request before collecting any receipt
    /// ([`LifecycleHandle::enqueue`]). With a caller that waits between items the queue never holds
    /// more than one job per requesting thread, and this function gathers exactly one entry.
    ///
    /// ## The close policy
    ///
    /// **The queue observed empty, or [`DENY_WINDOW_MAX_ENTRIES`] entries, whichever comes first.**
    /// No linger, no age bound, no timer.
    ///
    /// The bound is checked **inside** the drain and is not optional. Every entry pulled is one a
    /// concurrent submitter can replace, so "drain until the queue is empty" terminates only when
    /// the arrival rate drops — under sustained deny load from several requests it need not
    /// terminate at all, and an unbounded window is not "a big window", it is **no deny ever being
    /// acked**. That is the same failure the ingest window's row bound exists for, and the same
    /// remedy.
    ///
    /// **A linger — holding the window open to gather company — is declined.** Its whole benefit is
    /// gathering more denies, and after the enqueue split a request's denies are *already* in the
    /// queue with nothing to wait for; what a linger would additionally gather is denies from a
    /// *different* request that happens to be milliseconds behind. The cost is paid by every
    /// single-deny revocation on an idle node, which is the case the deny lane's latency exists for.
    /// A linger is also the one mechanism here that can make a deny wait for a deny that never
    /// comes.
    ///
    /// **What that leaves, stated because it is measured rather than argued away**: the executor can
    /// wake on the first item's doorbell and commit a window of one or two while the caller is still
    /// enqueueing the rest. The caller enqueues at memory speed and the first window costs an fsync,
    /// so this is a small constant number of extra windows at the head of a request, not N of them.
    /// `a_change_batch_of_n_costs_one_fsync` asserts the bound it produces rather than assuming it
    /// is zero.
    ///
    /// ## What this does not change
    ///
    /// The lane. It is still unbounded, still drained to empty before any work, still never refused
    /// for load, and there is still no route from it to a 429. The bound above closes a window; it
    /// refuses nothing.
    fn run_deny_pass(&mut self) -> bool {
        let mut entries: Vec<DenyEntry> = Vec::new();

        while entries.len() < DENY_WINDOW_MAX_ENTRIES {
            let Ok(job) = self.queues.deny.try_recv() else {
                break;
            };
            let Job { command, respond } = job;
            let Command::Change {
                external_id,
                entity,
                op,
                descriptors,
            } = command
            else {
                // Unreachable while the lane follows the command (`Command::is_never_shed`): only a
                // `Change` rides the deny queue. Executed rather than dropped, so a future variant
                // that lands here is answered instead of silently losing its waiter — and the
                // window gathered so far is committed **first**, because this arm applies
                // immediately and would otherwise be applied ahead of denies that arrived before
                // it. Append order must equal apply order (lifecycle §4).
                if !entries.is_empty() {
                    self.commit_denies(std::mem::take(&mut entries));
                }
                self.execute(Job { command, respond });
                return true;
            };
            entries.push(DenyEntry {
                record: WalRecord::Change {
                    external_id,
                    op,
                    descriptors: descriptors.clone(),
                },
                entity,
                op,
                raw_descriptors: descriptors,
                respond,
            });
        }

        if entries.is_empty() {
            return false;
        }
        self.commit_denies(entries);
        true
    }

    /// `append × k → one fsync → apply → one swap → ack × k`, with lifecycle §4's deny-op
    /// exception folded per entry.
    ///
    /// ## Order
    ///
    /// Append order is entries order is apply order, and entries order is the deny lane's FIFO
    /// arrival order. One vector, built once and iterated forwards, so a `suppress D` and a later
    /// `unsuppress D` in the same window resolve exactly as they would have as two separate
    /// commands. This is why denies need none of the ordering machinery a *mixed* window would
    /// (`tessera_lifecycle::window` argues why the two windows stay separate).
    ///
    /// ## A failed sync is retried before it is a failure
    ///
    /// A sync failure and an append failure are different events, and the window treats them so.
    /// Every append having landed means the window's records are exactly the log's undurable region,
    /// which is the precondition for repairing it: the executor re-writes them and syncs again, a
    /// bounded number of times ([`Executor::retry_deny_durability`]). If a re-attempt succeeds the
    /// window is durable and takes the ordinary path — apply, one swap, **200** to every waiter,
    /// because the dispositions genuinely are durable and any other answer would be a lie in the
    /// direction that costs a caller a retry it does not owe.
    ///
    /// The fold below is therefore what happens when the retries are **exhausted**, or when an
    /// *append* failed and there was never anything to repair.
    ///
    /// ## The failure fold, which is the part to get right
    ///
    /// On an unrepaired append or fsync failure anywhere in the window:
    ///
    /// - every [`ChangeOp::Delete`] and [`ChangeOp::Suppress`] **in the window** is applied anyway —
    ///   the items are hidden immediately — and every waiter still gets an error;
    /// - every [`ChangeOp::Unsuppress`] and [`ChangeOp::Predicate`] applies **nothing**.
    ///
    /// The scope is lifecycle §4's and it is not uniform, which is what distinguishes this from the
    /// ingest window's failure path (`Executor::fail_window_wal` applies nothing at all). Making it
    /// uniform in *either* direction is a defect: applying everything re-exposes an item that replay
    /// still hides, behind a 500 whose body says nothing was applied; applying nothing leaves a
    /// requested suppression unapplied, which is the one thing this lane may never do.
    ///
    /// **Position in the window is not a term.** §4's rule is about the op, not about whether this
    /// particular record happened to be appended before the failure — and making visibility depend
    /// on where in an arbitrary drain order an item landed would be the less fail-closed reading of
    /// the two.
    ///
    /// **What a restart then does with the window, and the one thing it costs.** Replay reads only
    /// the log's durable prefix (`tessera_lifecycle::wal`), so every record this window appended is
    /// discarded: the `Unsuppress` that was correctly refused stays refused, and the `Suppress` that
    /// was applied in memory comes back **unhidden**. That is the honest reading of the 500 the
    /// waiters received — durability was not achieved, it is owed, and the caller must retry
    /// (contracts §3.1, lifecycle §4) — and it is what the append-failure case has always done
    /// anyway, since a `Suppress` whose *append* failed leaves no bytes to replay. The two adjacent
    /// failure points agree, which is what lets an operator reason about the answer at all: a hiding
    /// that survived a restart only when the failure happened to land on the fsync rather than on
    /// the append would be a guarantee nobody could state. The retry above is what makes this the
    /// last resort rather than the first response; it does not change what the resort is.
    ///
    /// **The in-memory rule above is untouched by that**, and must stay so. The item is hidden from
    /// the moment the disposition is accepted until the process ends, which is the whole interval a
    /// live node can be asked about, and the node stops claiming readiness for the rest of it.
    /// Nothing else may come to depend on an under-durable deny: lifecycle §4 gates side-manifest
    /// publication on WAL durability on exactly this reasoning, so that no other node can observe a
    /// suppression a restart here would drop.
    /// **⊘ Specified, not implemented** — there is no replication and no side-manifest publication,
    /// so the obligation is on whoever builds one, not a property to be relied on today.
    fn commit_denies(&mut self, entries: Vec<DenyEntry>) {
        let mut failed_at: Option<(usize, WalError)> = None;
        for (i, entry) in entries.iter().enumerate() {
            if let Err(e) = self.wal.append(&entry.record) {
                failed_at = Some((i, e));
                break;
            }
        }
        // **One fsync for the whole window.** Every entry is durable when it returns, or none is.
        if failed_at.is_none() {
            if let Err(e) = self.wal.fsync() {
                // Every append landed cleanly, so the window's records are exactly the undurable
                // region and the sync can be attempted again — see `retry_deny_durability`. Only if
                // that gives up does this become a failure: the first waiter then gets the real
                // error and the rest `Poisoned`, exactly as the ingest window does, and for the same
                // reason: no wire behaviour distinguishes them.
                if let Err(e) = self.retry_deny_durability(&entries, e) {
                    failed_at = Some((0, e));
                }
            }
        }
        self.observe_wal();

        if let Some((index, error)) = failed_at {
            // Lifecycle §4's exception, per entry — see this function's doc for why the fold is not
            // uniform and why position is not a term in it.
            let applied: Vec<(EntityId, ChangeOp, Option<PredicateChange>)> = entries
                .iter()
                .filter(|e| matches!(e.op, ChangeOp::Delete | ChangeOp::Suppress))
                .map(|e| (e.entity, e.op, None))
                .collect();
            if !applied.is_empty() {
                let _published = self.apply_changes(applied);
            }
            let mut real = Some(error);
            for (i, entry) in entries.into_iter().enumerate() {
                let e = if i == index {
                    real.take().unwrap_or(WalError::Poisoned)
                } else {
                    WalError::Poisoned
                };
                self.ack_failed(&entry.respond, ExecError::Wal(e));
            }
            return;
        }

        // Durable, not yet in force. See `pause_point`.
        self.pause_point(PauseSiteArg::AfterFsync);

        // Resolution is deferred to **after** the append succeeds: a change's resolved terms are
        // needed only for the apply below, so there is no reason to mint an extension id for a
        // record that might never become durable. `Delete`/`Suppress`/`Unsuppress` carry no
        // descriptors, so a window of pure denies resolves nothing at all.
        //
        // Against the **current** generation's dictionary (§3.2): a descriptor a flush has since
        // promoted must resolve to its durable ordinal, or every later change would go on minting
        // a fresh extension id for a term that already has one.
        let dict = Arc::clone(&self.generation.load().dict);
        let applied: Vec<(EntityId, ChangeOp, Option<PredicateChange>)> = entries
            .iter()
            .map(|e| {
                (
                    e.entity,
                    e.op,
                    e.raw_descriptors.as_ref().map(|ds| PredicateChange {
                        terms: self.live.resolve_terms(&dict, ds),
                        descriptors: ds.clone(),
                    }),
                )
            })
            .collect();
        // One overlay clone, one generation, **one swap** for every entry in the window.
        let published = self.apply_changes(applied);

        // **k waiters, one proof.** A death partway through this loop leaves some waiters acked and
        // some not; every un-acked one gets `SubmitError::ReceiptLost` → 500, never `ExecutorDead`
        // → 503, because its change is durably in force.
        for entry in entries {
            self.ack(&entry.respond, Ack::Changed, &published);
        }
    }

    /// A deny window's sync failed. Re-write its records and sync again, up to
    /// [`DENY_DURABILITY_ATTEMPTS`] times in total, and report whether durability was reached.
    ///
    /// ## Why the deny lane retries and the ingest lane does not
    ///
    /// The two lanes' failure paths are not symmetric, and the asymmetry is the whole justification.
    /// An ingest window whose durability fails **applies nothing** — no effect exists anywhere, the
    /// caller is told so, and a restart agrees with the caller. Nothing diverges, so there is
    /// nothing for a retry to rescue. A deny window's failure applies its deletions and
    /// suppressions anyway (lifecycle §4), so the live node hides an item that a restart un-hides:
    /// the *only* case in the write path where reaching durability late changes what the system is,
    /// rather than only what it says. [`tessera_lifecycle::wal::Wal::retry_durability`] is
    /// lane-agnostic and the ingest window could adopt it; it has no reason to.
    ///
    /// ## Why re-writing is the retry, and why it duplicates nothing
    ///
    /// A bare second `fsync` is not a retry on Linux: after a writeback error the kernel may mark
    /// the page clean and report the error exactly once, so the second call returns success with the
    /// data gone. `Wal::retry_durability` therefore rewinds to the last durable offset and writes
    /// the window's records again, re-dirtying exactly the pages that may have been dropped — and
    /// because that region is by construction the region no caller was ever told about, the repair
    /// leaves one copy of each record rather than two. (Two copies would replay correctly as well,
    /// since a disposition is idempotent; that is the fallback argument, not the mechanism.)
    ///
    /// ## What it costs, stated because it is a real regression on one axis
    ///
    /// The apply-anyway rule fires up to ~250 ms later than it did, because the retry runs
    /// **before** the window is applied rather than after. Applying first and retrying second would
    /// keep the hiding immediate, but it would split one window's application in two — the deny ops
    /// now, the rest after the retry — and this window's ordering guarantee is that entries order is
    /// apply order, which a `suppress D` followed by an `unsuppress D` in one window depends on. The
    /// added delay is inside the range the lane already exhibits under sustained ingest (165 ms p50,
    /// 346 ms max, measured); the ordering is not negotiable.
    fn retry_deny_durability(
        &mut self,
        entries: &[DenyEntry],
        first: WalError,
    ) -> std::result::Result<(), WalError> {
        // Cloned only on the failure path, and this is the one place the executor needs the window's
        // records as a slice. A window is at most `DENY_WINDOW_MAX_ENTRIES` small records.
        let records: Vec<WalRecord> = entries.iter().map(|e| e.record.clone()).collect();
        let mut last = first;
        for delay in DENY_DURABILITY_BACKOFF {
            std::thread::sleep(delay);
            match self.wal.retry_durability(&records) {
                Ok(_) => return Ok(()),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// **The commit window** (lifecycle §5.1): drain the work queue into one window and
    /// close it. Returns whether anything was done, which is what tells [`Executor::run`] to
    /// re-drain the deny lane rather than block.
    ///
    /// ## The two close triggers, and why only one of them is a policy
    ///
    /// - **The row bound** (`commit_window_max_rows`) — the only one of the two that is a policy.
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
    /// That one is a correctness mechanism (it is what keeps the unreachable-duplicate hole closed
    /// across a window), not a policy; it also yields, for the reason written at the site.
    ///
    /// ## Why there is no age bound, and why the config key is inert
    ///
    /// The specified third trigger is `opened_at.elapsed() >= commit_window_max_age_ms`, whose
    /// stated purpose is to stop a lone ingest on an idle server waiting the full window age
    /// "for company that is not coming". **It is declined, and `ingest.commit_window_max_age_ms`
    /// is inert** (`docs/decisions/0034-the-window-does-not-linger.md`;
    /// `tessera_server::config::tests::the_commit_window_age_bound_is_inert` fails the
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
    /// **The join qualifies the row bound, and the qualification belongs here.** A joined retry
    /// ([`Executor::admit_ingest`]'s `Held` arm) consumes a work-queue slot and adds **zero rows**,
    /// so on a stream of nothing but byte-identical retries the row bound cannot trip and this loop
    /// terminates only on an empty queue. What still bounds it is the structural fact — one entry,
    /// or one joined waiter, per concurrently-blocked submitting thread, since every submitter
    /// blocks on its receipt. That is a bound on *waiters*, not on rows or bytes, and it costs one
    /// `Responder` each; resident rows are unaffected, because a
    /// join carries no rows into the window. Deny latency is *better* on this path than the
    /// alternative of closing per retry: one close for N retries rather than N.
    ///
    /// The interval where the queue momentarily empties while more work is imminent **is** real (a
    /// handler holds its permit across decode, term resolution and sidecar IO before it submits).
    /// But that is a window closing *too early*, and an age bound only ever closes a window
    /// *earlier* — it is the wrong sign. The mechanism that would address it is a linger, which is
    /// declined: it would be paid by every submission, could gather at most the other admitted
    /// handlers, and the sort-scope win it would buy is of order 10¹ runs against the corpus's real
    /// signature distribution (`tessera_lifecycle::window`'s module doc has the arithmetic).
    ///
    /// Lifecycle §5.1 asks for a window "bounded by size **or** age". It is bounded — by size, and
    /// by a drain-empty close that is strictly tighter than any age bound could be.
    ///
    /// ## Deny priority is unchanged
    ///
    /// The deny lane is drained to empty before this is called and again as soon as it returns, and
    /// **the window holds ingest only**. Lifecycle §5.1 permits deny dispositions to share it; the
    /// permission is declined, and the argument is at `tessera_lifecycle::window`'s module doc,
    /// where a reader considering the mixed window will meet it.
    ///
    /// So a deny waits at most for the window in front of it — but only because **every close in
    /// this function yields**. The bound is not "the deny lane is drained around this call": this
    /// function is what decides how long "around" is, and while work keeps arriving it decides that
    /// by returning at each close. `a_deny_is_never_queued_behind_work_with_group_commit_disabled`
    /// holds the row-bound path (red the moment that arm loops instead) and
    /// `a_deny_is_never_queued_behind_a_conflict_forced_window_split` holds the conflict path.
    ///
    /// **Measured, because it is easy to attribute the bound to the wrong line**: swapping the two
    /// drains in `Executor::run` so the deny lane is visited *after* the work pass rather than
    /// before leaves all three of those tests green, and is not a defect — a deny still waits at
    /// most one window either way. Draining the deny lane only once the work queue has gone *empty*
    /// reds all three. The yield is the mechanism; the drain order is not.
    ///
    /// **The honest bound, stated in full.** A deny waits for the deny entries ahead of it (that
    /// lane is FIFO and unbounded) plus **at most two window closes** — and in practice one,
    /// because the replacement a conflict opens is closed empty on every path where the first
    /// close succeeded (see the conflict arm). The worst case is two only when the first close
    /// *failed*. What those closes cost is one `assign_sorted` run over the window's
    /// rows, one append per entry, **one fsync** (~3.2 ms measured, ingest baseline memo) and one
    /// `IngestBuffer` clone that is O(total buffered items) — the dominant term, the only one that
    /// grows, and unbounded while there is no flush (⊘; see [`Executor::apply_window`]). That is why
    /// this is a **starvation** bound and deliberately not a latency target: the window in front may
    /// be arbitrarily slow, and nothing here is sized to make it fast.
    ///
    /// A bound of "≈ 2 × `commit_window_max_age_ms`" is **not** what this code gives, and its two
    /// premises — an age bound, and denies joining the ingest window — are both false of it.
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
            let Ok(work) = self.queues.work.try_recv() else {
                break;
            };
            let job = match work {
                ExecutorWork::Lifecycle(job) => job,
                ExecutorWork::PublishGeometry {
                    prefix,
                    segments_version,
                    watermark,
                    bundle,
                    dict,
                    delta_postings,
                    respond,
                } => {
                    // **The open window closes first, and that is ordering rather than tidiness.**
                    // A publication swaps the whole generation; performing it while a window holds
                    // ingest that has not been applied would publish geometry against a buffer the
                    // window is about to replace, and the window's own swap would then carry the
                    // pre-publication bundle forward — losing the publication entirely. The same
                    // hazard the `Change`-shaped arm below is warned about, reached by the one
                    // variant that does make it here.
                    if !window.is_empty() {
                        window = self.close_and_reopen(window);
                    }
                    let _ = respond.send(self.publish_geometry(
                        prefix,
                        segments_version,
                        watermark,
                        bundle,
                        dict,
                        delta_postings,
                    ));
                    self.health.note_work_refused();
                    did_work = true;
                    continue;
                }
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
    /// **One function so that the ordering is not a statement order two edits apart.**
    /// `CommitWindow::new` stamps `opened_at`, and the close it
    /// would otherwise be stamped ahead of is the *previous* window's append, fsync, apply, swap and
    /// acks. Stamped first, a replacement charges its predecessor's whole service to itself —
    /// `record_window_service` doubles, and with it the `retry_after_s` a shed client is told. The
    /// two lines below must stay in this order, and this doc is the only warning a future editor
    /// gets, because **getting it wrong has no observable consequence**. The enumeration, which is
    /// the whole of the argument:
    ///
    /// 1. This is the only construction site of a replacement window, and its only caller is the
    ///    external-id conflict arm of [`Executor::admit_ingest`].
    /// 2. That arm returns [`Admission::YieldedAfterClose`] and the drain loop **breaks in the same
    ///    iteration**, so no *later* entry can ever enter a replacement.
    ///    The only candidate is the conflicting entry itself.
    /// 3. And that entry is refused: `apply_window` inserts its predecessor's external ids into
    ///    `established` before this function returns, so `established_collisions` sees them.
    ///
    /// **The second caller is the geometry-publication arm of [`Executor::run_work_pass`]**, which
    /// closes the open window before swapping the generation. It cannot mis-stamp: it does not
    /// admit an entry into the replacement at all, and the very next `try_recv` decides what does.
    ///
    /// A held `batch_id` is **not** a route into this function — it joins or 409s in place — so the
    /// only other ways in are two exceptions, both of which leave the ordering unobservable anyway:
    /// a close that **failed** (its `fail_window_wal`/`fail_window_alloc` paths record no accepted
    /// batch and establish nothing, so the entry *is* admitted into the replacement — but both
    /// return before the `AfterFsync` pause point and `ack_failed` carries no pause point, so
    /// nothing can park inside the mis-stamped interval to measure it, and the node's WAL is
    /// poisoned by then); and a 64-bit `digest` collision on an external id, which is not
    /// constructible.
    ///
    /// **So there is no test here**, and that is recorded rather than left as a gap someone assumes
    /// is covered: the join makes a replacement window hold *fewer* entries, not more, so there is
    /// no construction that observes the mis-stamp.
    fn close_and_reopen(&mut self, window: CommitWindow<Responder>) -> CommitWindow<Responder> {
        self.close_window(window);
        CommitWindow::new(self.next_window_seq())
    }

    fn next_flush_attempt(&mut self) -> u64 {
        self.flush_attempt += 1;
        self.flush_attempt
    }

    /// Take the next side-manifest number. See [`Executor::next_manifest_n`].
    fn allocate_manifest_n(&mut self) -> u64 {
        let n = self.next_manifest_n;
        self.next_manifest_n += 1;
        n
    }

    fn next_window_seq(&mut self) -> u64 {
        self.window_seq += 1;
        self.window_seq
    }

    /// **The batch-id state machine, evaluated on the executor** (contracts §3.4 and its
    /// Appendix R r8, lifecycle §5.1's "idempotency across a held window").
    ///
    /// Takes the open window by value and hands it back, possibly replaced. **By value
    /// deliberately**: a `&mut` signature would force a `mem::replace` on the conflict path, which
    /// constructs the replacement *before* the close it replaces — the exact mis-stamp
    /// [`Executor::close_and_reopen`] exists to prevent.
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
    /// | [`BatchState::Accepted`], different bytes | `409`, no effect |
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
                    // **The 409 reaches the retry and NOT the held original, and this is the one
                    // site that decides it.**
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
                    // **If the opposite reading is ever ruled**, the change is here and in
                    // `tessera-lifecycle`: mark the held entry discarded (a `bool` on `WindowEntry`,
                    // skipped by `CommitWindow::allocate` and by `held`) and fail its waiters. Not
                    // an entry *removal* — `by_batch` stores indices into `entries` and the
                    // external-id set has no refcounts, so removing one entry means repairing both.
                    self.ack_failed(&respond, ExecError::BatchConflict { batch_id });
                }
                // Either way this job occupied a work-queue slot and was counted at submission,
                // while `record_window_service` counts one completion per *entry* and a join adds
                // no entry. Without this, `work_depth` drifts up by one per retry forever and every
                // 429's `retry_after_s` inherits the drift.
                self.health.note_work_refused();
                (window, Admission::Answered)
            }
            BatchState::Unknown => {
                let mut admission = Admission::Admitted;
                // A conflicting entry closes the window **first**, and is then evaluated against
                // the state that close just published — which is what makes the checks below give
                // the per-command answers. **Only external ids reach
                // here**: a held batch id was answered above, without closing anything.
                if window.holds_external_id_of(&rows) {
                    window = self.close_and_reopen(window);
                    // **And yield once this entry is handled.** This close is a full
                    // `append → fsync → apply → swap → ack` inside the drain loop, and
                    // `window.rows()` resets with the replacement — so the row bound can never trip
                    // on a conflict-heavy stream. Without the yield, a pass could close unboundedly
                    // many windows without ever returning to `Executor::run`'s deny drain, which is
                    // what lifecycle §1.3 forbids verbatim: a deny queued behind work of unbounded
                    // duration. Reachable at the shipped defaults from a client re-ingesting an
                    // `external_id` that a still-open window already holds.
                    //
                    // The entry is handled first rather than yielding here, because it has already
                    // been taken off the queue and its waiter must be answered.
                    // `a_deny_is_never_queued_behind_a_conflict_forced_window_split` is the leg
                    // that holds it, and it drives this path — **not** the batch-id one, which
                    // joins rather than closing.
                    admission = Admission::YieldedAfterClose;
                }
                if let Some(entry) = self.admit(rows, batch_id, body_hash, respond) {
                    if window.is_empty() {
                        // The in-flight gauge is armed at the **first entry**, never at window
                        // construction: an empty window is never closed, so a gauge armed there
                        // would never be cleared and `service_nanos_for_estimate` would grow
                        // without bound on an idle node.
                        self.health.mark_work_started(window.opened_at());
                    }
                    window.push(entry);
                }
                (window, admission)
            }
        }
    }

    /// The external-id admission check, on the one thread that also performs the inserts. `None`
    /// means the caller has already been answered.
    ///
    /// The batch-id half is [`Executor::admit_ingest`]'s, because it has three states and this has
    /// one.
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
    /// §4's apply-anyway rule is written for `Delete`/`Suppress` only. The window carries ingest
    /// alone, so this rule is uniform over every entry in it and there is no per-entry split by
    /// disposition to get wrong. That is one of the reasons deny dispositions stay out of the
    /// window — see `tessera_lifecycle::window`'s module doc for the rest.
    /// `an_ingest_append_failure_applies_nothing` holds this rule and
    /// `an_unsuppress_append_failure_applies_nothing` holds the deny lane's op scope beside it.
    ///
    /// **A restart does not undo the refusal.** Replay reads only the log's durable prefix
    /// (`tessera_lifecycle::wal`), so the window's records — which by construction lie past the last
    /// fsync — are discarded rather than replayed. Without that, an ingest refused for want of
    /// durability would exist after the next restart: the caller was told it had nothing, so a
    /// caller doing what the 500 asks and retrying under a fresh batch identifier would end up
    /// holding two. `an_ingest_whose_durability_failed_stays_absent_across_a_reopen` holds this, and
    /// `crash_between_fsync_and_swap_replays_rather_than_reallocates` holds the other half — a real
    /// killed process whose window *did* fsync, which replays rather than reallocating.
    fn close_window(&mut self, window: CommitWindow<Responder>) {
        let entries = window.len() as u64;
        let started = window.opened_at();

        let closed = match self.live.with_allocator(|a| window.allocate(a)) {
            Ok((closed, tally)) => {
                // Recorded here, at the allocation, rather than after the append: the figure
                // describes the ASSIGNMENT, which is made and complete by this point. A window that
                // allocates and then fails its append has still fragmented the entity axis exactly
                // this much, because the ids are issued and `Allocator` never reuses one (I9).
                self.health.record_fragmentation(tally);
                closed
            }
            Err((e, waiters)) => {
                // `allocate` leaves the high-water mark unchanged on this path, so the window has no
                // effect at all — the same statement `ExecError::Alloc` already makes per batch.
                self.fail_window_alloc(e, waiters, entries, started);
                return;
            }
        };

        // One record per entry — batch identity is preserved through the window, which is what a
        // joined retry is answered off — appended in entries order, which is also apply order.
        let mut failed_at: Option<(usize, WalError)> = None;
        // The position **before** each append is where that record lands, and it is the only moment
        // it can be read: afterwards the log has moved on, and after the window it is one number for
        // several records. A row's position is what a rotation reclaims below, so an entry whose
        // append failed contributes none — the loop breaks before pushing.
        let mut positions: Vec<u64> = Vec::with_capacity(closed.len());
        for (i, entry) in closed.iter().enumerate() {
            let at = self.wal.position();
            if let Err(e) = self.wal.append(&entry.record) {
                failed_at = Some((i, e));
                break;
            }
            positions.push(at);
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
        let published = self.apply_window(&mut closed, &positions);

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
            // The last waiter takes the ids; **a joined retry is what puts a second one here**, and
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
            // executor stays total over `Command`. A window of one entry is exactly the
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
            // A window of one entry is exactly the per-command semantics this path used to have,
            // which is why there is no second deny implementation to keep in step with the first.
            Command::Change {
                external_id,
                entity,
                op,
                descriptors,
            } => self.commit_denies(vec![DenyEntry {
                record: WalRecord::Change {
                    external_id,
                    op,
                    descriptors: descriptors.clone(),
                },
                entity,
                op,
                raw_descriptors: descriptors,
                respond,
            }]),
        }
    }

    /// Clone the buffer **once**, insert every entry in the window, publish **once**.
    ///
    /// The amortisation this buys is the one that grows: the clone is O(total buffered items) and
    /// the buffer only grows, there being no flush (⊘), so a window of k entries pays it once
    /// instead of k times — and the same clone is the deny-ack latency floor
    /// ([`ExecutorHealth::apply_nanos_total`]).
    ///
    /// ## What the window does NOT do to this cost
    ///
    /// It reduces the clone's **count**, not its **cost**. Two facts a reader sizing anything from
    /// the paragraph above needs, and neither is addressed here:
    ///
    /// 1. **At the shipped defaults `commit_window_max_items == ingest_max_batch_rows == 10 000`,
    ///    so a maximal batch is a one-entry window and gets no amortisation at all.** Ingesting
    ///    10⁹ rows in maximal batches is 10⁵ submissions each cloning a buffer growing towards
    ///    10⁹ — **O(N²/B)** — and only a flush bounds it. Measured today:
    ///    `apply_nanos_max` 210–437 ms at ~1.34 M buffered items
    ///    (`docs/evidence/memos/2026-08-01-deny-ack-baseline.md`, result 3). It is the *small*
    ///    batches the window collects.
    /// 2. **A flush is specified to make the buffer chunked or persistent** (⊘). So the O(B) clone
    ///    is a known cost with a known remedy, not an inherited posture — do not build a second
    ///    mechanism around it in the meantime.
    ///
    /// No counter is added for this: `apply_nanos_total` / `apply_nanos_max` already
    /// measure it and are already on `/control/status`.
    ///
    /// `terms` is **taken** out of each entry rather than borrowed: each row's resolved set is
    /// *moved* into the buffer, where borrowing would force one `Vec<TermId>` clone per row on the
    /// one thread every write is serialised through — measured at +14% on the
    /// 10 000-row arm. `&mut` is what buys it; an entry's `terms` is empty after this and nothing
    /// downstream reads it — the ack needs `entity_ids`, not terms.
    fn apply_window(&self, closed: &mut [ClosedEntry<Responder>], positions: &[u64]) -> Published {
        let started = std::time::Instant::now();
        let generation = self.generation.load_full();
        let mut buffer = (*generation.buffer).clone();

        let mut established = lock_recover(&self.live.established);
        // Updated together in one critical section, so a `/control/changes` lookup and a
        // `/v1/items` drill-down can never disagree about the same item.
        let mut established_inverse = lock_recover(&self.live.established_inverse);
        // Entries are appended and applied in the same order, and nothing observable depends on
        // which order that is. `CommitWindow::holds_external_id_of` forces a close rather than admit
        // a second entry naming an external id the window already holds, and a row with no external
        // id establishes nothing (the `if let Some` below), so no two entries in one window can
        // write the same key. That is a stronger statement than "one vector, iterated once": it
        // survives a refactor that reorders the vector, where the shape argument does not.
        for (entry, wal_pos) in closed.iter_mut().zip(positions) {
            let terms = std::mem::take(&mut entry.terms);
            for (row, row_terms) in entry.rows().iter().zip(terms) {
                // Contracts §3.4: no external id means nothing to establish. `None` must never
                // collide with `None`, so this skips rather than inserting under a shared empty key.
                if let Some(external_id) = &row.external_id {
                    established.insert(external_id.clone(), row.entity_id);
                    established_inverse.insert(row.entity_id, external_id.clone());
                }
                buffer.insert_row_with_terms(row, row_terms);
                buffer.set_wal_pos(row.entity_id, *wal_pos);
            }
        }
        drop(established);
        drop(established_inverse);

        // Published here, at the one place buffer occupancy changes, so `/control/ingest`'s
        // occupancy bound reads a figure the executor maintains rather than one a handler derives
        // from a generation it would have to load.
        self.health
            .buffered_items
            .store(buffer.len(), Ordering::SeqCst);

        let next = Generation {
            overlay_version: generation.overlay_version + 1,
            buffer: Arc::new(buffer),
            prefix: generation.prefix.clone(),
            segments_version: generation.segments_version,
            watermark: generation.watermark,
            bundle: Arc::clone(&generation.bundle),
            dict: Arc::clone(&generation.dict),
            postings: Arc::clone(&generation.postings),
            delta_postings: generation.delta_postings.clone(),
            overlay: Arc::clone(&generation.overlay),
            // Neither the deny sets nor the row space moved, so the mask is unchanged. An ingest
            // adds a *buffered* row, which has no row id to be denied at.
            denied: Arc::clone(&generation.denied),
        };
        self.publish(next, started)
    }

    /// Clone the overlay **once**, apply every change in the window, publish **once**.
    ///
    /// The amortisation this buys is the one that grows. [`Overlay`] never shrinks —
    /// entries survive `suppress → unsuppress` and there is no compaction fold (⊘) — so the clone
    /// is O(overlay depth) and the depth rises by one per new item denied. Applying an N-item
    /// revocation one command at a time therefore copies Θ(N²) entries; a window of k pays the clone
    /// once for the k. The clone is also every deny's ack-latency floor
    /// ([`ExecutorHealth::apply_nanos_total`], already on `/control/status`, so no counter is added
    /// for this).
    ///
    /// Changes are applied in slice order, which is the window's entries order, which is the deny
    /// lane's FIFO arrival order — so a `suppress` and a later `unsuppress` of the same item resolve
    /// as they would have as two separate commands. The slice is iterated once, forwards.
    ///
    /// Pins are never invalidated by this (I11): a pin fixes `(prefix, segments_version)`, and this
    /// bumps `overlay_version`. That is lifecycle §2.3's rule that a suppression applies to a
    /// pinned request the moment it is accepted, without expiring the pin.
    fn apply_changes(
        &self,
        changes: Vec<(EntityId, ChangeOp, Option<PredicateChange>)>,
    ) -> Published {
        let started = std::time::Instant::now();
        let generation = self.generation.load_full();
        let mut overlay: Overlay = (*generation.overlay).clone();
        // The change is **moved** into the overlay rather than borrowed: it is per entry, and
        // cloning it here would be a per-entry cost on the one thread every write is serialised
        // through.
        // What the window did, for the mask below: which entities it denied, and whether any
        // removal happened at all.
        let mut newly_denied: Vec<EntityId> = Vec::new();
        let mut unsuppressed = false;
        for (entity, op, predicate) in changes {
            match op {
                ChangeOp::Delete | ChangeOp::Suppress => newly_denied.push(entity),
                ChangeOp::Unsuppress => unsuppressed = true,
                ChangeOp::Predicate => {}
            }
            overlay.apply(entity, op, predicate);
        }

        // **It alarms; it does not act** — there is no compaction fold, so an operator who sets
        // `overlay_soft_limit` gets a signal that the overlay is deep, not a mechanism that makes
        // it shallower.
        //
        // This is the only place the overlay grows **at runtime**; it is not the only place it
        // grows. `WritePath::reconstruct` builds one from WAL replay before this executor exists,
        // so a node restarting already over the limit is caught by
        // `Engine::set_overlay_soft_limit`'s own one-shot evaluation instead.
        // **Edge-triggered.** The depth never decreases, so a level-triggered check would emit
        // this four-line WARN on every subsequent deny, forever, with no path back — an alarm flood
        // at exactly the moment the node is under deny pressure.
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

        // **The deny mask, by the cheaper of the two licensed modes** (`derive_denied`). A window
        // of `Delete`/`Suppress` only grows the union, so adding each entity's row is provably
        // equal to re-deriving and costs the window rather than the whole deny set. A window
        // carrying an `Unsuppress` re-derives — subtracting the row would re-expose an entity that
        // `deleted` still holds, which is the one way this mask can fail open.
        let denied = if unsuppressed {
            Arc::new(crate::compose::derive_denied(&overlay, &generation.bundle))
        } else {
            let mut denied = (*generation.denied).clone();
            for partition in generation.bundle.partitions.values() {
                for (slice, slice_data) in &partition.slices {
                    let Some(rows) = denied.get_mut(slice) else {
                        continue;
                    };
                    for entity in &newly_denied {
                        if let Some(row) = slice_data.row_space.row_of(*entity) {
                            rows.add(row.raw());
                        }
                    }
                }
            }
            Arc::new(denied)
        };

        let next = Generation {
            overlay_version: generation.overlay_version + 1,
            overlay: Arc::new(overlay),
            prefix: generation.prefix.clone(),
            segments_version: generation.segments_version,
            watermark: generation.watermark,
            bundle: Arc::clone(&generation.bundle),
            dict: Arc::clone(&generation.dict),
            postings: Arc::clone(&generation.postings),
            delta_postings: generation.delta_postings.clone(),
            buffer: Arc::clone(&generation.buffer),
            denied,
        };
        self.publish(next, started)
    }

    /// **Publication by rebase** (§1.2): apply a completed flush to the **then-current**
    /// generation rather than to the one it was planned against.
    ///
    /// The flush ran on the pool while this thread went on accepting ingest and denies, so the
    /// generation has moved: its buffer holds rows that arrived meanwhile and its overlay holds
    /// dispositions accepted meanwhile. So the rebase removes **exactly the entity ids the flush
    /// consumed** — never a range, which would take the late arrivals with it — and appends the
    /// segment to whatever is live now.
    ///
    /// A flush planned against a superseded *prefix* is discarded: its row bases were computed
    /// against a row space that no longer exists. Its files are orphans nothing references, and the
    /// next tick re-plans.
    fn publish_flush(&mut self, completed: crate::flush::CompletedFlush) {
        let started = std::time::Instant::now();
        let live = self.generation.load_full();
        if live.prefix != completed.prefix {
            // A compaction moved the prefix under this flush. Nothing to apply it to.
            tracing::warn!(
                planned = %completed.prefix,
                live = %live.prefix,
                "discarding a completed flush planned against a superseded prefix"
            );
            return;
        }

        // **A promoting flush's ordinals are positions**, assigned as `dict.len() + i` against the
        // dictionary it planned against, and `Dict::load` will reproduce them only if its extent
        // lands where the flush assumed. If the dictionary moved, they name other descriptors.
        //
        // Scoped to flushes that wrote an extent: one that promoted nothing carries only ordinals
        // below the planned length, which append-only extension preserves, so discarding it would
        // be a liveness hole for no safety.
        //
        // The window is narrow and real: `flush_in_flight` clears only after the pool's sends, and
        // the executor drains completed flushes *before* it ticks, so a send landing between the
        // drain and the in-flight check leaves a tick planning against a generation whose
        // completed flush is not yet published.
        if dictionary_moved_under(completed.promoted_from_dict_len, live.dict.len()) {
            tracing::warn!(
                planned = completed.promoted_from_dict_len,
                live = live.dict.len(),
                "discarding a completed flush whose dictionary moved under it: the ordinals in \
                 its extent are positions, and they are no longer the positions it assigned"
            );
            return;
        }

        // **Assembled here, from the live partition manifest, and written before the swap.**
        // Contracts §2.3 makes a side-manifest complete current state, and *current* is decided
        // now rather than when the flush was planned: the deny fields come from the overlay this
        // publication carries, so a suppression accepted during the flush's flight is in the
        // manifest the flush publishes.
        let Some(partition_data) = live.bundle.partitions.get(&completed.partition) else {
            tracing::warn!(
                partition = %completed.partition,
                "discarding a completed flush for a partition this bundle no longer carries"
            );
            return;
        };
        let mut manifest = partition_data.manifest.clone();
        let manifest_n = self.allocate_manifest_n();
        manifest.watermark = completed.watermark;
        manifest.entity_id_high_water = manifest
            .entity_id_high_water
            .max(completed.entity_id_high_water);
        manifest.segments.push(completed.descriptor);
        manifest.deltas.push(manifest_n);
        manifest.external_id_runs.push(completed.external_id_run);
        manifest.locator_extents.push(completed.locator_extent);
        manifest.files.extend(completed.files);
        if let Some(extent) = completed.dict_extent {
            manifest.dict_extents.push(extent);
        }
        write_deny_state(&mut manifest, &live.overlay);

        // **The commit point, and it is still the manifest** — only the thread moved. A failure
        // here discards the flush: its files become orphans nothing references, the buffer is
        // retained, the next tick re-plans. The same posture as every other flush failure, and
        // the reason the write precedes the swap.
        if let Err(e) = crate::flush::write_segments_manifest(
            &self.prefix_dir,
            &completed.partition,
            manifest_n,
            &manifest,
        ) {
            self.health.flush_failures.fetch_add(1, Ordering::Relaxed);
            tracing::error!(
                error = %e,
                "ALARM: a completed flush's side-manifest could not be committed; its files are                  orphans, the buffer is retained, and the next tick will re-plan"
            );
            return;
        }

        let next_bundle = match live.bundle.with_segment(
            &completed.partition,
            &completed.slice,
            completed.segment,
            completed.extent,
            tessera_store::read::PublishedManifest {
                manifest,
                n: manifest_n,
            },
        ) {
            Ok(bundle) => bundle,
            Err(e) => {
                // The row space moved under this flush — another publication landed between the
                // plan and here. Discarded, not forced: forcing would put the segment at a
                // `row_base` that is no longer the end of row space, aliasing rows.
                tracing::warn!(error = %e, "discarding a completed flush that no longer rebases");
                return;
            }
        };

        // **Exactly what was consumed, from the then-current buffer.** O(buffered) on this thread,
        // which is the term the deny-ack memo measured as dominant at 1 M buffered (165 ms p50);
        // one such stall lands ahead of the deny lane per tick, and §10 records it as a cost this
        // design adds rather than one it avoids.
        let mut buffer = (*live.buffer).clone();
        for entity in &completed.consumed {
            buffer.remove(*entity);
        }

        let watermark = next_bundle
            .partitions
            .get(&completed.partition)
            .map(|p| p.manifest.watermark)
            .unwrap_or(live.watermark);
        let segments_version = live.segments_version + 1;
        let mut delta_postings = live.delta_postings.clone();
        delta_postings.push(completed.tier);

        // **Rebuilt against the segment this flush just added**, which is what gives a suppressed
        // or deleted item its place in the mask the moment it acquires a row: until now it was
        // buffered, had no row, and so appeared in no mask at all.
        let denied = Arc::new(crate::compose::derive_denied(&live.overlay, &next_bundle));

        let next = Generation {
            prefix: live.prefix.clone(),
            segments_version,
            watermark,
            bundle: next_bundle,
            dict: completed.dict,
            postings: Arc::clone(&live.postings),
            delta_postings,
            overlay_version: live.overlay_version,
            overlay: Arc::clone(&live.overlay),
            buffer: Arc::new(buffer),
            denied,
        };
        let _published = self.publish(next, started);

        // A flush supersedes geometry, so it prunes exactly as any other geometry publication
        // does: one swap, one `segments_version` bump, one retention pass. The superseded
        // generation itself is held by nothing but the requests already in flight against it.
        self.row_projection_cache
            .prune_generations_below(segments_version.saturating_sub(KEEP_SUPERSEDED_GENERATIONS));
        self.health.flushes.fetch_add(1, Ordering::Relaxed);

        self.record_and_rotate(segments_version);
    }

    /// Note the publication in the log and reclaim what it made redundant — flush §7.3's last two
    /// steps, **after** the generation swap and never before it.
    ///
    /// ```text
    /// generation swap                             ← the publication event, already done above
    /// Flush{n, wal_pos} appended and fsynced      ← a replay-start optimisation
    /// rotation: snapshot written, then reclaim    ← §7.2, snapshot before any deletion
    /// ```
    ///
    /// **`wal_pos` is the buffer's oldest surviving row, not this record's own offset.** Rows acked
    /// *during* the flush were appended after its snapshot point, were never consumed, and carry
    /// entity ids at or above the new watermark; reclaiming below this record would delete them and
    /// §7.1 would then reconstruct them from nothing. `IngestBuffer::oldest_wal_pos` answers it from
    /// the post-publication buffer — the rows that still have no geometry — and refuses (`None`) if
    /// any of them does not know its own position, which reclaims nothing rather than guessing.
    ///
    /// **Two gates, and neither is the one `plan_flush` applies.** A poisoned WAL cannot be appended
    /// to at all. A node whose overlay has diverged from its durable WAL must rotate nothing (§7.2):
    /// `Wal::discard_undurable` deliberately does not un-apply, so such a node holds dispositions no
    /// record backs, and writing a snapshot from that overlay would make a 500'd, never-acked deny
    /// permanent. `plan_flush` refuses for the same reason, but it is a different site and a flush
    /// already in flight when the divergence happened reaches here regardless.
    ///
    /// Nothing here is fatal. A failure leaves the log longer than it needs to be, which the next
    /// tick retries; the publication itself is already durable and already swapped.
    fn record_and_rotate(&mut self, n: u64) {
        if self.wal.is_poisoned() {
            return;
        }
        if self.health.overlay_diverged.load(Ordering::SeqCst) {
            tracing::warn!(
                "this node's overlay has diverged from its durable WAL, so it rotates nothing;                  the log grows until an operator restarts it"
            );
            return;
        }

        let generation = self.generation.load();
        let oldest = match generation.buffer.oldest_wal_pos() {
            // Nothing buffered: every ingest row has geometry, so the whole durable prefix is
            // reclaimable, and the position is read *after* the `Flush` append below.
            None => None,
            Some(Some(oldest)) => Some(oldest),
            // A buffered row of unknown position pins the log. Fail-safe and loud by construction:
            // the sequence grows, which is visible, rather than a record vanishing, which is not.
            Some(None) => Some(0),
        };

        // The record's own `wal_pos` is read *before* the append and the reclamation's *after*, so
        // with an empty buffer they differ by exactly this record's width. Both are true statements
        // of "below this, every ingest row has been consumed into a segment" — the reclaim value is
        // simply the tighter one, and the record is a replay-start optimisation that nothing reads
        // back (§7.1), so the looser one costs nothing.
        let before_append = self.wal.position();
        if let Err(e) = self
            .wal
            .append(&WalRecord::Flush {
                n,
                wal_pos: oldest.unwrap_or(before_append),
            })
            .and_then(|()| self.wal.fsync().map(|_| ()))
        {
            tracing::warn!(error = %e, "the flush record could not be appended; no rotation this tick");
            return;
        }

        // **Read after the append, not before it.** With an empty buffer the reclaim point is "all
        // of it", and taking the position first would leave the `Flush` record above the line —
        // pinning the very member it was written to announce, so the log would grow by one member
        // per flush and reclaim nothing. The `Flush` record is an optimisation that nothing reads
        // back (§7.1), so there is no reason for it to hold its own member open.
        let reclaim_below = oldest.unwrap_or_else(|| self.wal.position());

        let snapshot = generation.overlay.snapshot();
        match self.wal.rotate(&snapshot, reclaim_below) {
            Ok(deleted) if !deleted.is_empty() => {
                tracing::info!(
                    members = ?self.wal.members(),
                    reclaimed = ?deleted,
                    "WAL members reclaimed below the flush's oldest unconsumed row"
                );
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %e, "the WAL did not rotate; the log grows until it does");
            }
        }
    }

    /// Publish new geometry: check, swap, prune. **The executor's own arm of lifecycle §1.3's
    /// swap-only publication step.**
    ///
    /// This ran in `Engine::publish_geometry` until flush needed a second publisher and made the
    /// arrangement untenable. It was a compare-and-swap in a retry loop there — safe against
    /// *itself*, but not against this thread's unconditional `store`, which could clobber a
    /// publication it had already observed. On this thread there is nothing to race, so there is
    /// no loop: one load, one check, one store.
    ///
    /// `check_publishable` is evaluated against the generation actually being replaced, which is
    /// the one loaded here, because this is the only thread that can replace it.
    fn publish_geometry(
        &mut self,
        prefix: String,
        segments_version: u64,
        watermark: u64,
        bundle: Arc<Bundle>,
        dict: Arc<Dict>,
        delta_postings: Vec<Arc<DeltaTier>>,
    ) -> std::result::Result<(), GeometryRefused> {
        let started = std::time::Instant::now();
        let previous = self.generation.load_full();
        check_publishable(&previous, &prefix, segments_version)?;

        // Rebuilt against the new row space: row ids mean something only within one
        // `segments_version`, so a geometry publication invalidates every row in the old mask
        // (`derive_denied`). Taken before `bundle` moves into the generation.
        let denied = Arc::new(crate::compose::derive_denied(&previous.overlay, &bundle));

        let next = Generation {
            prefix,
            segments_version,
            watermark,
            bundle,
            dict,
            postings: Arc::clone(&previous.postings),
            delta_postings,
            overlay_version: previous.overlay_version,
            overlay: Arc::clone(&previous.overlay),
            buffer: Arc::clone(&previous.buffer),
            denied,
        };
        let _published = self.publish(next, started);

        // The retention pass, at the swap rather than at a reclaim — see
        // `RowProjectionCache::prune_generations_below` for why depth 1 rather than depth 0, which
        // would delete the input to the very patch it exists to enable.
        self.row_projection_cache
            .prune_generations_below(segments_version.saturating_sub(KEEP_SUPERSEDED_GENERATIONS));
        Ok(())
    }

    /// The generation swap. **The only `store` in the write path**, and the only producer of a
    /// [`Published`] token on the success path.
    ///
    /// `load_full` + `store` is safe here for one reason and one only: this is the sole thread that
    /// can publish. A flush would be a second publisher and **must not `store` directly** — it
    /// submits a command and is applied here, as lifecycle §1.3 requires ("submitting a completed,
    /// immutable result back to the lifecycle thread for a swap-only publication step"). A flush
    /// that stored directly would lose geometry publications, and a lost one leaves the
    /// *live* generation on the pin drain list, where the cache's prune evicts projections still in
    /// use. `scripts/check-layers.sh` refuses any non-atomic `.store(` in this crate's sources
    /// outside this file — and that rule is demonstrated going red, not merely written.
    fn publish(&self, next: Generation, started: std::time::Instant) -> Published {
        // **The deny mask's derivation rule, enforced at the one place a generation becomes live.**
        // `crate::compose::derive_denied` states the rule; every build site — the incremental
        // addition on a deny window, the rebuild at each geometry publication, the carry-forward
        // when neither the overlay nor the row space moved — is licensed only if it lands on the
        // same value. Checked here rather than trusted, in debug only because it is O(denies): a
        // site that gets it wrong then fails the suite instead of silently re-exposing a deleted
        // item in a viewer's map, which is the one failure this mask can produce.
        debug_assert!(
            *next.denied == crate::compose::derive_denied(&next.overlay, &next.bundle),
            "the deny mask does not equal a fresh derivation — a build site broke the rule at \
             `derive_denied`; an unsuppress subtracting a row while `deleted` still holds the \
             entity is the classic way"
        );
        self.generation.store(Arc::new(next));
        // The overlay/buffer clone above is O(total buffered items). This counter is what makes
        // the deny-ack floor measurable rather than asserted — see
        // `ExecutorHealth::apply_nanos_total`.
        self.health
            .record_apply(started.elapsed().as_nanos() as u64);
        #[cfg(feature = "fault-injection")]
        if let Some(faults) = &self.faults {
            faults.record(tessera_lifecycle::faults::Step::Swap);
        }
        Published::by_swap()
    }

    /// Mirror the WAL's own poison flag into the posture, **in both directions**.
    ///
    /// Asked of the WAL rather than remembered from the last error this loop happened to see: a
    /// posture derived from the executor's bookkeeping can drift from the thing it describes. That
    /// was the stated intent from the start and it was not what the code did — the flag was raised
    /// through a monotone `fetch_max`, so it could be entered and never left, and the WAL returning
    /// to health was invisible. It is a plain store now, and the WAL is the only thing that decides:
    /// a torn handle never reports healthy because it never *becomes* healthy, not because anything
    /// here refuses to lower the flag.
    fn observe_wal(&self) {
        self.health.mirror_wal(self.wal.is_poisoned());
    }

    /// Send a **successful** receipt. Requires proof that the effect is live — see [`Published`].
    ///
    /// The [`PauseSite::BeforeAck`] point is armed **here**, one statement above the send, rather
    /// than at either call site. That is what makes it a statement about the ack rather than about
    /// a line number: an ack that any later rewrite moves above the swap takes this pause point
    /// with it, and a test parked here then observes the effect *not* in force.
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
/// In a fault-injection build this **is** [`tessera_lifecycle::faults::PauseSite`]. In a shipped
/// build the module does
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
    /// assertion — which is what a hard-coded `retry_after_s: 1` gives.
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
    /// growing `IngestBuffer` produces, since its clone is O(total buffered items) and there is no
    /// flush — must not keep quoting the fast figure.
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

    /// **The EWMA alone is blind in exactly the state that produces the 429.**
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
    /// the second assertion drops to the floor.
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

    /// **The soft-limit alarm is edge-triggered.** `Overlay::len` never decreases — entries survive
    /// `suppress → unsuppress` and there is no compaction fold — so a level-triggered check emits a
    /// four-line WARN per deny, forever, with no path back, precisely while the node is under deny
    /// pressure.
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

        // Re-arming, both ways it can happen. Falling back below the limit does not occur as the
        // code stands, and the trigger is written for the mechanism rather than for its absence.
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

/// The two rules the promotion design added to publication (`2026-08-03-descriptor-promotion-design`
/// §2), tested where they are decided rather than through a second slice no build produces.
#[cfg(test)]
mod dispatch_rules_tests {
    use super::*;
    use tessera_lifecycle::BufferedItem;

    fn plan_from(oldest: u64) -> crate::flush::FlushPlan {
        let item = BufferedItem {
            terms: Vec::new(),
            slice: "s".to_string(),
            x: 0.5,
            y: 0.5,
            scalars: Vec::new(),
            external_id: None,
            wal_pos: None,
        };
        crate::flush::FlushPlan {
            items: vec![(EntityId::new(oldest), item)],
        }
    }

    /// **Obligation 9.** Every context a dispatch builds shares `next_n`, so only one can commit
    /// its side-manifest. The one sent is the slice whose oldest waiting row is oldest — not the
    /// first by name, which is what `slices_of`'s lexicographic sort would give and which would
    /// let a continuously-fed `s0` deny `s1` a flush for ever.
    ///
    /// **Mutation:** replace this with `plans.into_iter().next()` and the assertion below fails —
    /// which is the starvation, made into a test.
    #[test]
    fn a_dispatch_sends_the_plan_holding_the_oldest_unflushed_row() {
        let plans = vec![
            ("s0".to_string(), plan_from(900)),
            ("s1".to_string(), plan_from(100)),
            ("s2".to_string(), plan_from(500)),
        ];
        let (slice, plan) = plan_to_dispatch(plans).expect("one of three");
        assert_eq!(slice, "s1", "oldest row wins, not lowest slice id");
        assert_eq!(plan.items[0].0.raw(), 100);
    }

    #[test]
    fn a_dispatch_with_no_plans_sends_nothing() {
        assert!(plan_to_dispatch(Vec::new()).is_none());
    }

    /// **Obligation 10.** The dictionary guard is scoped to flushes that wrote an extent. A flush
    /// that promoted nothing names only ordinals below the length it planned against, which
    /// append-only extension preserves, so discarding it would cost liveness and buy no safety.
    #[test]
    fn only_a_promoting_flush_is_discarded_when_the_dictionary_moves() {
        // Promoted: its extent's ordinals are positions, and the positions have moved.
        assert!(dictionary_moved_under(Some(7), 9));
        assert!(!dictionary_moved_under(Some(7), 7));
        // Promoted nothing: never discarded, however far the dictionary has gone.
        assert!(!dictionary_moved_under(None, 9));
        assert!(!dictionary_moved_under(None, 0));
    }
}
