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

use tessera_authz::{DeltaTier, Dict, FragmentCache};
use tessera_lifecycle::alloc::{high_water_from, low_water_from, AllocError, Allocator};
use tessera_lifecycle::buffer::DescriptorResolver;
use tessera_lifecycle::command::{Ack, Command, ExecError, Receipt, SubmitError, UnallocatedRow};
use tessera_lifecycle::faults::WalMeter;
use tessera_lifecycle::membership::{ArtifactStore, IncomingArtifact};
use tessera_lifecycle::overlay::replay;
use tessera_lifecycle::registry::LayerRegistry;
use tessera_lifecycle::wal::{ChangeOp, ExecutorWal, Wal, WalError, WalRecord, WalScalar};
use tessera_lifecycle::window::{ClosedEntry, CommitWindow, FragmentationTally, WindowEntry};
use tessera_lifecycle::{IngestBuffer, Overlay};

use crate::cache::RowProjectionCache;
use crate::cache::KEEP_SUPERSEDED_GENERATIONS;
use crate::geometry::{check_publishable, GeometryPublication, GeometryRefused};
use tessera_plugin::Descriptor;
use tessera_spatial::tiler::ScalarType;
use tessera_store::manifest::{
    DenyEntry as ManifestDenyEntry, ManifestVocabulary, SegmentsManifest,
};
use tessera_store::merge::MergePolicy;
use tessera_store::render_presence::RENDER_PRESENCE_DIR;
use tessera_store::vocabulary::{MintError, Minted, Vocabularies};
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
    /// A `POST /control/flush` awaiting the tick it pulls forward (contracts §3.4). A flag, not a
    /// count: the endpoint's 202 means "accepted, not yet done", and two requests before one tick
    /// are satisfied by that tick together.
    pub(crate) flush_requested: AtomicBool,
    /// Whether a flush is executing on the pool. A tick arriving while it is set is skipped, never
    /// queued: two concurrent flushes would double-consume the buffer range (§1.1). Set by the
    /// executor before the spawn, cleared by the pool after its sends, and read by
    /// `/control/status` so a reader can tell "no flush has landed yet" from "one is running".
    pub(crate) flush_in_flight: AtomicBool,
    /// A completed flush has been sent to `flush_done` and not yet drained.
    ///
    /// **The pool's half of the completion handshake** ([`FLUSH_COMPLETION_POLL`]): set
    /// immediately *before* the send, cleared by `publish_completed_flushes` only after it drained
    /// something. The ordering closes the race a bare `flush_in_flight` check leaves open — the
    /// pool clearing `in_flight` after its send, between the executor's empty `try_recv` and its
    /// wait computation, would put the executor to sleep for a full tick with a completed unit in
    /// the channel. With set-before-send, at wait time either the send has not happened
    /// (`in_flight` still true) or this flag is already visible; there is no gap.
    pub(crate) flush_completed_pending: AtomicBool,
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
    /// **`CURRENT` names a prefix this process is not serving** — the fold flipped the commit point
    /// and then could not complete its swap.
    ///
    /// Latching, like [`Self::overlay_diverged`] and for a sharper version of its reason. After the
    /// flip the durable bundle is the new prefix and the live generation is still the old one, so
    /// every publication this executor performs writes into a tree no restart reads: a flush's
    /// side-manifest and segment land under the superseded prefix, the rows are acked, and the WAL
    /// rotation that follows reclaims the only other copy of them. That is **acked ingest lost at
    /// the next restart**, behind no error at all — the failure a crash cannot cause, because a
    /// crashed process stops writing.
    ///
    /// So the node keeps serving what it has and publishes nothing until it is restarted, at which
    /// point it opens the prefix `CURRENT` names and is correct again. Cleared only by that
    /// restart.
    pub(crate) prefix_diverged: AtomicBool,
    /// Buffer occupancy as of the last apply — what `/control/ingest`'s occupancy bound is checked
    /// against.
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
    /// When the flush now on the pool was dispatched, as an offset from [`Self::base`] plus one;
    /// `0` when none is. Read by [`Self::record_flush_published`] for the drain sample.
    flush_started_nanos: AtomicU64,
    /// Wall nanoseconds per row drained, an EWMA over published flushes measured from dispatch to
    /// publication. Always on, where `flush_stage_nanos` is written only under `bench-timing`: it
    /// is the observed drain rate the buffer-occupancy 429 derives `Retry-After` from (ingest
    /// §4.2; [`estimate_buffer_retry_after_s`]). `0` before the first publication.
    flush_nanos_per_row_ewma: AtomicU64,
    /// The last tick, as an offset from [`Self::base`] plus one; `0` before the first.
    last_tick_nanos: AtomicU64,
    /// The tick period, `flush_max_age_secs` in nanoseconds, so a snapshot can say how long until
    /// the next tick without the executor's own clock.
    flush_period_nanos: AtomicU64,
    /// Side-manifests written for deny state alone — the gauge that makes the restore path's
    /// freshness observable, and what a test asserts a publication happened at all against.
    pub(crate) overlay_publications: AtomicU64,
    /// Ticks that found a flush already in flight and skipped rather than queued (§1.1).
    ///
    /// **Alarmed, because `flush_max_age_secs` would otherwise miss it**: a flush persistently
    /// slower than the tick is a visibility-latency breach, and the period an operator configured
    /// is not the period they are getting.
    pub(crate) flush_skips: AtomicU64,
    /// Flushes that failed and left the buffer intact for the next tick (§10).
    pub(crate) flush_failures: AtomicU64,
    /// Entity-space coalesce publications since the executor started (decision 0044's D2). A
    /// separate counter from `flushes` because the two publish different things: a flush moves
    /// geometry, a coalesce bounds the tier, run and dictionary-extent counts and moves none.
    pub(crate) coalesces: AtomicU64,
    /// Coalesces that failed or no longer rebased, leaving every consumed entry standing.
    pub(crate) coalesce_failures: AtomicU64,
    /// Row-space merge publications, and the ones that produced nothing. Separate from
    /// `coalesces` because the two publish different things: a merge bumps `segments_version` and
    /// costs a refresh round, a coalesce does neither.
    pub(crate) merges: AtomicU64,
    pub(crate) merge_failures: AtomicU64,
    /// Compaction folds published since the executor started, and the ones that produced nothing.
    ///
    /// A fold's own counter rather than a share of `merges`, because the two are different events:
    /// a merge bounds an axis inside the live prefix, a fold replaces the prefix, retires
    /// deletions and reclaims the superseded tree. The failure counter is what an operator watches
    /// for a fold that keeps re-reading the corpus and discarding — every failure leaves orphans
    /// under a prefix `CURRENT` never named, so the disc cost is visible before the cause is.
    pub(crate) folds: AtomicU64,
    pub(crate) fold_failures: AtomicU64,
    /// A `POST /control/compact` awaiting the tick that dispatches it. A flag rather than a count,
    /// for `flush_requested`'s reason: at most one fold is in flight, so two requests before one
    /// tick are satisfied by that tick together.
    pub(crate) fold_requested: AtomicBool,
    /// Folds the scheduler planned and `plan_fold` refused, and the last refusal in full.
    ///
    /// **A third counter beside `folds` and `fold_failures`, because a refusal is neither of
    /// those.** Nothing was folded, so `folds` does not move; nothing was written to discard, so
    /// `fold_failures` does not either. Without a counter of its own a refusal leaves no figure at
    /// all, and the pair an operator watches sits still while the corpus stops shrinking. That
    /// matters most for the refusal a deployment cannot leave: the fold demands 150% of the live
    /// bytes free on a device already holding 1.3–2.6× live, and it is the only operation that
    /// reclaims (compaction §8).
    ///
    /// `plan_fold` is called only for a fold the schedule or an operator asked for, so every
    /// increment here answers a request rather than an idle tick. `nothing_to_fold` is among the
    /// reasons and is not an alarm: it is what a `POST /control/compact` against an empty corpus
    /// answers.
    ///
    /// **One counter per gate**, indexed by `NoFold::index`, because a single total answers "a
    /// fold was refused" and not "which refusal is standing". The schedule re-evaluates at every
    /// tick once the interval floor has passed, and a refusal stamps no `last_fold_start_unix`, so
    /// a gate that stands increments on every tick from then on. `last_refusal` alone would be
    /// whichever refusal happened last, which on a node refusing on disc every tick is whatever
    /// else refused in between — the alarm fires and does not say why. Six counters cost six words
    /// and make the standing gate the one with the large number.
    pub(crate) fold_refusals: [AtomicU64; crate::compact::NoFold::GATES.len()],
    /// The last refusal, or `None` before the first — see [`ExecutorHealth::fold_refusals`] and
    /// [`FoldRefusal`]. A `Mutex` for [`ExecutorHealth::last_fold_passes`]' reason: written once
    /// per refused fold, read only by `/control/status`. Kept beside the per-gate counters because
    /// it is the only place the two figures of an `insufficient_disc` refusal appear.
    pub(crate) last_fold_refusal: Mutex<Option<FoldRefusal>>,
    /// The WAL as the last sample found it — see [`WalGauge`], which says what each figure means
    /// and what the pin's span distinguishes.
    ///
    /// **Sampled on the executor thread rather than at the poll**, because the log lives on that
    /// thread and a status request has no route to it, so a dashboard adds nothing to the node's
    /// cost. The sample is taken at a tick, before that tick's own publication rotates anything,
    /// and at most once per `flush_max_age_secs` — the walk is O(members) and the member count is
    /// unbounded under a pin ([`Executor::sample_wal_gauge`]). The reading is therefore up to one
    /// period old. Once more at [`Executor::run`]'s entry, so a node's first period does not
    /// report an empty log.
    pub(crate) wal_gauge: Mutex<WalGauge>,
    /// A completed fold has been sent to `fold_done` and not yet drained — the dedicated thread's
    /// half of the same completion handshake a flush has, and set before the send for the same
    /// reason.
    pub(crate) fold_completed_pending: AtomicBool,
    /// When the last fold **ended**, however it ended, as a unix second — 0 before the first one.
    ///
    /// **Written by the fold's own thread on both of its exits**, because the executor never sees
    /// one of them: a failure inside `execute` reaches no `publish_fold` and would otherwise leave
    /// the schedule believing the attempt was still the one that started. It exists for the
    /// interval floor's second half — see [`Executor::fold_floor_from`], where the rule it serves
    /// is stated.
    pub(crate) fold_ended_unix: AtomicU64,
    /// The last fold's wall clock in seconds, and the highest resident set its own staircase saw
    /// (`compact::PassCost`) — the two gauges `/control/status` publishes for the most expensive
    /// operation in the system. Both are 0 before the first fold. Both cover the whole fold, from
    /// the fold thread's entry to the superseded prefix's reclaim: the publication's phases are
    /// rows of the same staircase (`compact::Staircase`).
    ///
    /// **The RSS figure is a staircase maximum, not a peak**, and the difference is not pedantry:
    /// it is sampled at pass boundaries, so a spike inside a pass is invisible to it. It is
    /// what a deployment has, and probe P1 is what says how far under the true peak it sits.
    pub(crate) last_fold_secs: AtomicU64,
    pub(crate) last_fold_rss: AtomicU64,
    /// The last fold's attribute pass IO (`filter-index.md` §6.2's reported-never-triggered-on
    /// ruling). The staircase says what pass 4a cost in time and residency; these say what it cost
    /// in bytes, which is the axis its non-disruption argument is made on.
    pub(crate) last_fold_attr_read: AtomicU64,
    pub(crate) last_fold_attr_written: AtomicU64,
    /// The last fold's staircase, pass by pass — what the two gauges above are a reduction of.
    ///
    /// **The gauges alarm and this diagnoses**, which is why both exist: `last_fold_rss` says the
    /// fold reached 9 GiB and this says which pass it reached it on, and only the second is
    /// actionable. A `Mutex` rather than a fifth atomic because it is written once per fold, hours
    /// apart, and read only by `/control/status`.
    pub(crate) last_fold_passes: Mutex<Vec<crate::compact::PassCost>>,
    /// The last fold's degradation report — which artifacts its deletions took members from, and
    /// which supplied content lost a source (write cycle §4.2).
    ///
    /// **The durable copy is the file in `reports/`**; this is the same content held for the
    /// operator route, so a caller polling an endpoint does not have to read the bundle root. Empty
    /// before the first fold and after one that degraded nothing — which the file distinguishes and
    /// this does not, deliberately: an operator asking *what did the last fold degrade* wants the
    /// list, and an operator asking *did it report* wants the directory.
    pub(crate) last_fold_report: Mutex<Vec<tessera_lifecycle::membership::Degradation>>,
    /// A fold has finished its passes and is **holding** at the test hook
    /// (`Engine::set_fold_paused_for_test`). Always `false` in a shipped build, where nothing ever
    /// sets the flag it waits on; it exists so a test can wait on the hold as a condition rather
    /// than guess at it with a sleep, which is what makes the mid-flight-flush cases deterministic.
    pub(crate) fold_holding: AtomicBool,
    /// See [`Self::flush_completed_pending`], whose handshake this shares.
    pub(crate) merge_completed_pending: AtomicBool,
    /// Whether a completed coalesce is waiting to be published — see
    /// [`Self::flush_completed_pending`], whose handshake and ordering this shares exactly.
    pub(crate) coalesce_completed_pending: AtomicBool,
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
    /// **The window close, partitioned** — the write-path equivalent of [`crate::timing`]'s
    /// viewport breakdown, and the instrumentation `arms::ingest` said would be needed to attribute
    /// ingest cost ("StageTimings covers the viewport path only; there is no write-path equivalent
    /// yet"). Six stages that together partition `close_window`; `apply_nanos_total` above stays as
    /// the coarse figure `/control/status` already publishes, and stages 4–6 sum to it.
    ///
    /// **The clock reads are gated, the call sites are not.** `stage_nanos` is written by
    /// [`ExecutorHealth::lap`], which is a no-op without `bench-timing` — so the instrumented and
    /// uninstrumented builds take the same path, exactly as `timing.rs` argues for the read side.
    /// Zeros in a release build mean "not measured", never "free".
    stage_nanos: [AtomicU64; WriteStage::COUNT],
    /// **The flush, partitioned** ([`crate::flush::FlushStage`]), beside `stage_nanos` and never
    /// added to it: the pool's stages are wall clock on another thread, and the ingest
    /// attribution's partition of `stage_nanos` against `submit→receipt` holds only while those
    /// laps stay this thread's own. The executor's stages are written as they happen
    /// ([`Self::flush_lap`]); the pool's arrive once per `execute_flush` return
    /// ([`Self::record_flush_execution`]), so a flush still running on the pool is in no total.
    /// Zero without `bench-timing`, as `stage_nanos` is.
    flush_stage_nanos: [AtomicU64; crate::flush::FlushStage::COUNT],
    /// `execute_flush` returns on the pool, whichever way. `flushes` counts publications; the two
    /// differ by the executions the rebase discarded and by any completed unit still in the
    /// channel.
    flush_executions: AtomicU64,
    /// Rows in every `execute_flush` that returned `Ok`.
    flush_rows_executed: AtomicU64,
    /// Rows a publication removed from the buffer, summed over every flush that swapped.
    flush_rows_published: AtomicU64,
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
    /// alarms, and the schedule acts on a different number** — this counter and its log
    /// line are the whole of the mechanism.
    ///
    /// **Crossings, not publications.** Counting every apply at or above the limit is
    /// level-triggering on a quantity that falls only at a fold, and only for its deletion half:
    /// `Overlay` entries survive `suppress → unsuppress`, and a suppression never retires at all.
    /// A node that crossed 500 000 would
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
    /// Delta tiers as encoded, cumulative over every published flush — the tier-scope half of
    /// contracts §3.4's `fragmentation`, and the one at which between-window scatter is visible
    /// ([`FragmentationTally::of_tier`]). Fed at publication, never at plan or execute: a
    /// discarded flush's tier is an orphan and must not count.
    tier_fragmentation: Mutex<FragmentationTally>,
    /// Published tiers behind [`Self::tier_fragmentation`].
    fragmentation_tiers: AtomicU64,
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

/// The six stages a commit window's close partitions into, in the order `close_window` runs them.
///
/// **They partition wall clock on the executor thread**, so the sum plus whatever is unattributed
/// is the close's whole duration. `Apply*` are the three inside `apply_window`, and together they
/// are the coarse `apply_nanos_total` that `/control/status` already publishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteStage {
    /// `CommitWindow::allocate` — entity-id assignment and the signature sort.
    Allocate,
    /// The per-entry WAL append loop: serialise and write.
    WalAppend,
    /// One `fsync` for the whole window — group commit's amortisation half.
    WalFsync,
    /// `apply_window`'s deep copy of the ingest buffer (F3's operand).
    ApplyBufferClone,
    /// `apply_window`'s per-row loop: `established`, `established_inverse`, the buffer insert.
    ApplyRows,
    /// Within [`WriteStage::ApplyRows`]: the forward `established` insert (`Vec<u8>`-keyed).
    RowEstablished,
    /// Within [`WriteStage::ApplyRows`]: the `established_inverse` insert (`EntityId`-keyed).
    RowEstablishedInv,
    /// Within [`WriteStage::ApplyRows`]: `IngestBuffer::insert_row_with_terms`.
    RowBufferInsert,
    /// Within [`WriteStage::ApplyRows`]: `IngestBuffer::set_wal_pos`.
    RowWalPos,
    /// `admit_ingest`: taking a submitted job into the open commit window — the WAL record and the
    /// window entry are built here, before anything is durable.
    AdmitWindow,
    /// `record_accepted_batch`: the idempotency index insert, which clones the batch's whole
    /// `Vec<EntityId>` (one per batch, not per row).
    RecordBatch,
    /// **Outside `close_window` entirely**: the caller's `accept_ingest`, from entry to receipt.
    ///
    /// Overlaps every other stage rather than partitioning beside them — it is the whole of what a
    /// `/control/ingest` handler waits on, and the executor's stages happen inside it. The
    /// difference between this and the stages is queueing, the channel round-trip, and the
    /// caller-side blocking wait, which is the ~2 µs/row `WriteStage` could not otherwise see.
    SubmitToReceipt,
    /// The generation swap.
    ApplySwap,
}

impl WriteStage {
    pub const COUNT: usize = 13;
    pub const ALL: [WriteStage; Self::COUNT] = [
        WriteStage::Allocate,
        WriteStage::WalAppend,
        WriteStage::WalFsync,
        WriteStage::ApplyBufferClone,
        WriteStage::ApplyRows,
        WriteStage::ApplySwap,
        WriteStage::RowEstablished,
        WriteStage::RowEstablishedInv,
        WriteStage::RowBufferInsert,
        WriteStage::RowWalPos,
        WriteStage::AdmitWindow,
        WriteStage::RecordBatch,
        WriteStage::SubmitToReceipt,
    ];
    pub fn name(self) -> &'static str {
        match self {
            WriteStage::Allocate => "allocate",
            WriteStage::WalAppend => "wal_append",
            WriteStage::WalFsync => "wal_fsync",
            WriteStage::ApplyBufferClone => "buffer_clone",
            WriteStage::ApplyRows => "apply_rows",
            WriteStage::ApplySwap => "swap",
            WriteStage::RowEstablished => "  .est_fwd",
            WriteStage::RowEstablishedInv => "  .est_inv",
            WriteStage::RowBufferInsert => "  .buf_insert",
            WriteStage::RowWalPos => "  .wal_pos",
            WriteStage::AdmitWindow => "admit",
            WriteStage::RecordBatch => "record_batch",
            WriteStage::SubmitToReceipt => "submit→receipt",
        }
    }
}

/// A lap mark. Carries an `Instant` only under `bench-timing`; a zero-sized token otherwise, so
/// the uninstrumented build allocates no clock and the call sites need no `#[cfg]`.
#[derive(Clone, Copy)]
pub(crate) struct StageMark(#[cfg(feature = "bench-timing")] pub(crate) std::time::Instant);

impl StageMark {
    #[inline(always)]
    pub(crate) fn now() -> Self {
        #[cfg(feature = "bench-timing")]
        {
            StageMark(std::time::Instant::now())
        }
        #[cfg(not(feature = "bench-timing"))]
        {
            StageMark()
        }
    }
}

/// The gates `plan_fold` can refuse on, in the order [`ExecutorStats::fold_refusals_by_gate`]
/// counts them and [`FoldRefusal::gate`] names them.
///
/// **Literals from a closed list, never a formatted variant.** `crate::compact::NoFold::GATES`
/// carries the argument: a `format!("{:?}", reason)` would put whatever a future arm holds onto a
/// status response, and an arm naming a layer or a view would then publish corpus-derived text
/// with nothing in the type system objecting.
pub const FOLD_GATES: [&str; crate::compact::NoFold::GATES.len()] = crate::compact::NoFold::GATES;

/// A fold the scheduler asked for and `plan_fold` would not plan.
///
/// **What a reader should conclude from it**: a recent `at_unix` with `folds` not moving is a
/// deployment that is asking for compaction and not getting it, and `gate` says which of the six
/// conditions is holding. For `insufficient_disc` the two figures are the whole diagnosis —
/// `need_bytes` is 150% of the live bytes the input manifests name and `had_bytes` is what
/// `statvfs` answered, and the gap between them is what the device has to gain before a fold will
/// start. Nothing else in the system reclaims, so that gap does not close on its own.
///
/// `gate` is [`crate::compact::NoFold`]'s variant name in snake case; the figures are `None` for
/// the four conditions that carry none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FoldRefusal {
    pub gate: &'static str,
    pub need_bytes: Option<u64>,
    pub had_bytes: Option<u64>,
    /// When the refusal happened, as a unix second.
    pub at_unix: u64,
}

/// The write-ahead log's size and its rotation bound, sampled at the executor's tick.
///
/// **What a reader should conclude from it.** `members` is the direct signal: steady-state
/// retention is two, and a sequence that keeps growing is a rotation that is not reclaiming
/// (`Wal::members`). `pin` is what separates the two ways a log gets large. `None` and a large
/// `bytes` is a log that is large because ingest is fast, and the next publication rotates it.
/// `Some` and a large `pin_span_bytes` is a log that cannot rotate below that position, and the
/// name says what would release it: `publication` and `content` go at a tick, `growth` and `fill`
/// only at the compaction fold's whole rewrite (`ArtifactStore::wal_pin`). Without the pin those
/// two states read identically.
///
/// `position` counts record bytes across every member the sequence has ever held, so it rises
/// through reclamation and is not a size; `bytes` is what the surviving members occupy now. The
/// span is `position - pin`, the part of the log the pin is holding down.
///
/// Sampled at the executor's first loop iteration and at most once per `flush_max_age_secs`
/// thereafter, so the figures are up to one period old and are never unsampled on a running node.
/// The walk costs two `stat`s per member and the member count is unbounded under a pin, so the
/// rate limit is what stops the gauge getting dearer as the condition it reports gets worse
/// ([`Executor::sample_wal_gauge`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WalGauge {
    pub members: u64,
    pub bytes: u64,
    pub position: u64,
    /// The rotation bound and the name of the pin holding it, or `None` when nothing pins the log.
    pub pin: Option<(&'static str, u64)>,
    /// `position - pin`, and `0` when nothing pins the log.
    pub pin_span_bytes: u64,
    /// Walks taken since the executor started, `1` for the reading its first loop iteration took.
    /// Two polls returning the same value read the same sample, not two samples that agreed.
    pub samples: u64,
}

/// A snapshot of [`ExecutorHealth`], for `/control/status` and for tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecutorStats {
    pub posture: ExecutorPosture,
    pub work_submitted: u64,
    pub deny_submitted: u64,
    /// See [`ExecutorHealth::apply_nanos_total`] — the whole apply step, not the clone alone.
    pub apply_nanos_total: u64,
    /// Per-stage nanoseconds for the window close, indexed by [`WriteStage`]. **All zero without
    /// `bench-timing`** — see [`ExecutorHealth::stage_nanos`].
    pub stage_nanos: [u64; WriteStage::COUNT],
    /// Per-stage nanoseconds for the flush, indexed by [`crate::flush::FlushStage`]. **All zero
    /// without `bench-timing`** — see [`ExecutorHealth::flush_stage_nanos`].
    pub flush_stage_nanos: [u64; crate::flush::FlushStage::COUNT],
    /// `execute_flush` returns on the pool, `Ok` or `Err` — see
    /// [`ExecutorHealth::flush_executions`].
    pub flush_executions: u64,
    /// Rows in every `execute_flush` that returned `Ok`.
    pub flush_rows_executed: u64,
    /// Rows removed from the buffer by every publication that swapped.
    pub flush_rows_published: u64,
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
    /// Side-manifests written for deny state alone (contracts §2.3's immediate-publication rule).
    /// Advances without `flushes`, and without moving any geometry.
    pub overlay_publications: u64,
    /// Ticks skipped because a flush was already in flight (§1.1) — a rising count is a flush
    /// persistently slower than the tick, i.e. a visibility-latency breach.
    pub flush_skips: u64,
    /// Flushes that failed and left the buffer intact for the next tick (§10).
    pub flush_failures: u64,
    /// Entity-space coalesce publications, and the ones that produced nothing — the observable
    /// behind "the tier, run and dictionary-extent counts are bounded".
    pub coalesces: u64,
    pub coalesce_failures: u64,
    /// Row-space merge publications, and the ones that produced nothing — the observable behind
    /// "the segment count is bounded".
    pub merges: u64,
    pub merge_failures: u64,
    /// Compaction folds published, and the ones discarded — the observable behind "deletions
    /// retire, orphans are reclaimed, and the bundle returns to one segment per partition-view".
    pub folds: u64,
    pub fold_failures: u64,
    /// Whether a `POST /control/compact` is awaiting the next tick.
    pub fold_requested: bool,
    /// Requested folds `plan_fold` would not plan, and the last of them — see
    /// [`ExecutorHealth::fold_refusals`] for why a refusal advances neither counter above, and
    /// [`FoldRefusal`] for what a reader concludes from the pair.
    ///
    /// The total is the sum of [`ExecutorStats::fold_refusals_by_gate`] rather than a counter of
    /// its own, so the two cannot disagree.
    pub fold_refusals: u64,
    /// The same refusals split by gate, indexed as [`FOLD_GATES`] names them. A gate that stands
    /// is counted at every tick the schedule re-evaluates on, so the largest entry is the
    /// condition the deployment is actually in.
    pub fold_refusals_by_gate: [u64; FOLD_GATES.len()],
    pub last_fold_refusal: Option<FoldRefusal>,
    /// The WAL as the last sample found it — see [`WalGauge`].
    pub wal: WalGauge,
    /// The last fold's wall clock in seconds and the highest resident set its pass staircase saw,
    /// in bytes — see [`ExecutorHealth::last_fold_secs`] for what the second number is and is not.
    /// Both 0 before the first fold.
    pub last_fold_secs: u64,
    pub last_fold_rss: u64,
    pub last_fold_attr_read: u64,
    pub last_fold_attr_written: u64,
    /// Whether a `POST /control/flush` is awaiting the next tick.
    pub flush_requested: bool,
    /// Whether a flush unit is executing on the pool — see [`ExecutorHealth::flush_in_flight`].
    pub flush_in_flight: bool,
    /// Whether this node's overlay has diverged from its durable WAL (§7.2). **Latching**: it
    /// publishes no flush and rotates no WAL until restarted.
    pub overlay_diverged: bool,
    /// Whether `CURRENT` names a prefix this process is not serving (see
    /// [`ExecutorHealth::prefix_diverged`]). **Latching**: it publishes nothing and rotates no WAL
    /// until restarted.
    pub prefix_diverged: bool,
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
    /// The observed drain cost: wall nanoseconds per row, an EWMA over published flushes from
    /// dispatch to publication. `0` before the first publication.
    pub flush_nanos_per_row_ewma: u64,
    /// Time until the next scheduled tick; `0` when one is due or overdue.
    pub next_tick_in_nanos: u64,
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
    /// Delta tiers as encoded, cumulative over published flushes — the scope at which
    /// between-window scatter is visible (contracts §3.4; [`FragmentationTally::of_tier`]).
    pub tier_fragmentation: FragmentationTally,
    /// Published tiers behind [`Self::tier_fragmentation`].
    pub fragmentation_tiers: u64,
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

    /// Tier-scope postings over containers touched (contracts §3.4): the encoded tiers, where a
    /// tier spans every window the buffer accumulated between ticks — so this figure, unlike the
    /// window-scope one, moves with between-window scatter. `None` before any flush has published.
    pub fn tier_postings_per_container(&self) -> Option<f64> {
        let f = self.tier_fragmentation;
        (f.containers > 0).then(|| f.postings as f64 / f.containers as f64)
    }

    /// Tier-scope run ratio, same construction as [`Self::run_ratio`] (`1.0` fully scattered,
    /// larger is better), over tiers as encoded. `None` before any flush has published.
    pub fn tier_run_ratio(&self) -> Option<f64> {
        let f = self.tier_fragmentation;
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
            flush_in_flight: AtomicBool::new(false),
            flush_completed_pending: AtomicBool::new(false),
            overlay_diverged: AtomicBool::new(false),
            prefix_diverged: AtomicBool::new(false),
            overlay_publications: AtomicU64::new(0),
            buffered_items: AtomicUsize::new(0),
            flushable_items: AtomicUsize::new(0),
            flushes: AtomicU64::new(0),
            flush_started_nanos: AtomicU64::new(0),
            flush_nanos_per_row_ewma: AtomicU64::new(0),
            last_tick_nanos: AtomicU64::new(0),
            flush_period_nanos: AtomicU64::new(0),
            flush_skips: AtomicU64::new(0),
            flush_failures: AtomicU64::new(0),
            coalesces: AtomicU64::new(0),
            coalesce_failures: AtomicU64::new(0),
            coalesce_completed_pending: AtomicBool::new(false),
            merges: AtomicU64::new(0),
            merge_failures: AtomicU64::new(0),
            merge_completed_pending: AtomicBool::new(false),
            folds: AtomicU64::new(0),
            fold_failures: AtomicU64::new(0),
            fold_requested: AtomicBool::new(false),
            fold_refusals: std::array::from_fn(|_| AtomicU64::new(0)),
            last_fold_refusal: Mutex::new(None),
            wal_gauge: Mutex::new(WalGauge::default()),
            fold_completed_pending: AtomicBool::new(false),
            fold_ended_unix: AtomicU64::new(0),
            last_fold_secs: AtomicU64::new(0),
            last_fold_rss: AtomicU64::new(0),
            last_fold_attr_read: AtomicU64::new(0),
            last_fold_attr_written: AtomicU64::new(0),
            last_fold_passes: Mutex::new(Vec::new()),
            last_fold_report: Mutex::new(Vec::new()),
            fold_holding: AtomicBool::new(false),
            apply_nanos_total: AtomicU64::new(0),
            stage_nanos: Default::default(),
            flush_stage_nanos: std::array::from_fn(|_| AtomicU64::new(0)),
            flush_executions: AtomicU64::new(0),
            flush_rows_executed: AtomicU64::new(0),
            flush_rows_published: AtomicU64::new(0),
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
            tier_fragmentation: Mutex::new(FragmentationTally::default()),
            fragmentation_windows: AtomicU64::new(0),
            fragmentation_tiers: AtomicU64::new(0),
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

    /// The last fold's staircase, pass by pass — empty before the first fold.
    ///
    /// **Separate from [`ExecutorStats`] rather than a field on it.** That struct is `Copy` and is
    /// read on paths that take it per request; a `Vec` in it would make every one of those an
    /// allocation for a figure only the operator plane wants, hours apart.
    pub fn last_fold_passes(&self) -> Vec<crate::compact::PassCost> {
        lock_recover(&self.last_fold_passes).clone()
    }

    /// The last fold's degradation report — see [`Self::last_fold_report`].
    pub fn last_fold_report(&self) -> Vec<tessera_lifecycle::membership::Degradation> {
        lock_recover(&self.last_fold_report).clone()
    }

    pub fn stats(&self) -> ExecutorStats {
        let work_submitted = self.work_submitted.load(Ordering::Relaxed);
        let work_completed = self.work_completed.load(Ordering::Relaxed);
        // Read once; the total below is their sum, so a poll cannot publish a total the breakdown
        // does not add up to.
        let fold_refusals_by_gate: [u64; FOLD_GATES.len()] =
            std::array::from_fn(|gate| self.fold_refusals[gate].load(Ordering::Relaxed));
        ExecutorStats {
            posture: self.posture(),
            work_submitted,
            deny_submitted: self.deny_submitted.load(Ordering::Relaxed),
            apply_nanos_total: self.apply_nanos_total.load(Ordering::Relaxed),
            stage_nanos: std::array::from_fn(|i| self.stage_nanos[i].load(Ordering::Relaxed)),
            flush_stage_nanos: std::array::from_fn(|i| {
                self.flush_stage_nanos[i].load(Ordering::Relaxed)
            }),
            flush_executions: self.flush_executions.load(Ordering::Relaxed),
            flush_rows_executed: self.flush_rows_executed.load(Ordering::Relaxed),
            flush_rows_published: self.flush_rows_published.load(Ordering::Relaxed),
            apply_nanos_max: self.apply_nanos_max.load(Ordering::Relaxed),
            wal_appends: self.wal.appends(),
            wal_fsyncs: self.wal.fsyncs(),
            wal_recoveries: self.wal_recoveries.load(Ordering::Relaxed),
            work_completed,
            work_depth: work_submitted.saturating_sub(work_completed),
            work_service_nanos_ewma: self.work_service_nanos_ewma.load(Ordering::Relaxed),
            work_in_flight_nanos: self.work_in_flight_nanos(),
            flush_nanos_per_row_ewma: self.flush_nanos_per_row_ewma.load(Ordering::Relaxed),
            next_tick_in_nanos: self
                .flush_period_nanos
                .load(Ordering::Relaxed)
                .saturating_sub(self.elapsed_since_marker(&self.last_tick_nanos)),
            overlay_soft_limit_alarms: self.overlay_soft_limit_alarms.load(Ordering::Relaxed),
            fragmentation: *lock_recover(&self.fragmentation),
            tier_fragmentation: *lock_recover(&self.tier_fragmentation),
            fragmentation_tiers: self.fragmentation_tiers.load(Ordering::Relaxed),
            fragmentation_windows: self.fragmentation_windows.load(Ordering::Relaxed),
            ticks: self.ticks.load(Ordering::Relaxed),
            overlay_diverged: self.overlay_diverged.load(Ordering::SeqCst),
            prefix_diverged: self.prefix_diverged.load(Ordering::SeqCst),
            flushable_items: self.flushable_items.load(Ordering::SeqCst),
            flushes: self.flushes.load(Ordering::Relaxed),
            overlay_publications: self.overlay_publications.load(Ordering::Relaxed),
            flush_skips: self.flush_skips.load(Ordering::Relaxed),
            flush_failures: self.flush_failures.load(Ordering::Relaxed),
            coalesces: self.coalesces.load(Ordering::Relaxed),
            coalesce_failures: self.coalesce_failures.load(Ordering::Relaxed),
            merges: self.merges.load(Ordering::Relaxed),
            merge_failures: self.merge_failures.load(Ordering::Relaxed),
            folds: self.folds.load(Ordering::Relaxed),
            fold_failures: self.fold_failures.load(Ordering::Relaxed),
            fold_requested: self.fold_requested.load(Ordering::SeqCst),
            fold_refusals_by_gate,
            fold_refusals: fold_refusals_by_gate.iter().sum(),
            last_fold_refusal: *lock_recover(&self.last_fold_refusal),
            wal: *lock_recover(&self.wal_gauge),
            last_fold_secs: self.last_fold_secs.load(Ordering::Relaxed),
            last_fold_rss: self.last_fold_rss.load(Ordering::Relaxed),
            last_fold_attr_read: self.last_fold_attr_read.load(Ordering::Relaxed),
            last_fold_attr_written: self.last_fold_attr_written.load(Ordering::Relaxed),
            buffered_items: self.buffered_items.load(Ordering::Relaxed),
            flush_requested: self.flush_requested.load(Ordering::SeqCst),
            flush_in_flight: self.flush_in_flight.load(Ordering::SeqCst),
        }
    }

    /// Record a fold the planner would not plan. Executor thread only, once per refusal.
    fn record_fold_refusal(&self, reason: crate::compact::NoFold) {
        let (gate, need_bytes, had_bytes) = reason.gauge();
        *lock_recover(&self.last_fold_refusal) = Some(FoldRefusal {
            gate,
            need_bytes,
            had_bytes,
            at_unix: unix_now().unwrap_or(0),
        });
        self.fold_refusals[reason.index()].fetch_add(1, Ordering::Relaxed);
    }

    /// Record the WAL's size and rotation bound. Executor thread only, once per tick.
    fn record_wal_gauge(&self, gauge: WalGauge) {
        *lock_recover(&self.wal_gauge) = gauge;
    }

    /// Fold one closed window's tally in. Executor thread only, once per window close.
    fn record_fragmentation(&self, tally: FragmentationTally) {
        lock_recover(&self.fragmentation).merge(tally);
        self.fragmentation_windows.fetch_add(1, Ordering::Relaxed);
    }

    fn record_tier_fragmentation(&self, tally: FragmentationTally) {
        lock_recover(&self.tier_fragmentation).merge(tally);
        self.fragmentation_tiers.fetch_add(1, Ordering::Relaxed);
    }

    /// How long the work item currently executing has been running; `0` when idle.
    ///
    /// One clock read, taken only when a snapshot is asked for — the 429 paths and
    /// `/control/status`, never per row. `saturating_sub` because the two reads are not atomic
    /// together: the executor can finish and clear the marker between the load and the elapsed, and
    /// a job that started "in the future" relative to a stale `base.elapsed()` must report zero
    /// rather than wrap to an enormous drain estimate.
    fn work_in_flight_nanos(&self) -> u64 {
        self.elapsed_since_marker(&self.work_started_nanos)
    }

    /// Nanoseconds since a marker was set, or `0` where it is unset. A marker is an offset from
    /// [`Self::base`] plus one, so that `0` can mean unset; the `saturating_sub` is
    /// [`Self::work_in_flight_nanos`]'s argument.
    fn elapsed_since_marker(&self, marker: &AtomicU64) -> u64 {
        match marker.load(Ordering::Relaxed) {
            0 => 0,
            set_plus_one => (self.base.elapsed().as_nanos() as u64)
                .saturating_sub(set_plus_one.saturating_sub(1)),
        }
    }

    /// Set a marker to `at`. `0` is reserved for unset, hence the `+ 1`.
    fn set_marker(&self, marker: &AtomicU64, at: std::time::Instant) {
        let offset = at.saturating_duration_since(self.base).as_nanos() as u64;
        marker.store(offset.saturating_add(1), Ordering::Relaxed);
    }

    /// The flush unit is about to go to the pool. Executor thread only.
    fn mark_flush_started(&self, at: std::time::Instant) {
        self.set_marker(&self.flush_started_nanos, at);
    }

    /// A flush published `rows`: fold its wall time per row into the drain EWMA and clear the
    /// marker. Executor thread only. A flush of no rows moves nothing, having drained nothing.
    fn record_flush_published(&self, rows: usize) {
        let elapsed = self.elapsed_since_marker(&self.flush_started_nanos);
        self.flush_started_nanos.store(0, Ordering::Relaxed);
        if rows == 0 {
            return;
        }
        let sample = elapsed / rows as u64;
        let prev = self.flush_nanos_per_row_ewma.load(Ordering::Relaxed);
        // Seeded by the first observation, then the same eighth-weight decay as the work EWMA.
        let next = if prev == 0 {
            sample.max(1)
        } else {
            let p = prev as i128;
            let s = sample as i128;
            (p + (s - p) / 8).max(1) as u64
        };
        self.flush_nanos_per_row_ewma.store(next, Ordering::Relaxed);
    }

    /// A tick happened at `at`. Executor thread only.
    fn mark_tick(&self, at: std::time::Instant) {
        self.set_marker(&self.last_tick_nanos, at);
    }

    /// The tick period, from the executor's flush configuration.
    fn set_flush_period_secs(&self, secs: u64) {
        self.flush_period_nanos
            .store(secs.saturating_mul(1_000_000_000), Ordering::Relaxed);
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

    /// Charge the time since `mark` to `stage`, and return a fresh mark. A no-op without
    /// `bench-timing`, where it returns `mark` unchanged and reads no clock.
    #[inline(always)]
    #[allow(unused_variables)]
    fn lap(&self, stage: WriteStage, mark: StageMark) -> StageMark {
        #[cfg(feature = "bench-timing")]
        {
            let now = std::time::Instant::now();
            self.stage_nanos[stage as usize].fetch_add(
                now.duration_since(mark.0).as_nanos() as u64,
                Ordering::Relaxed,
            );
            StageMark(now)
        }
        #[cfg(not(feature = "bench-timing"))]
        {
            mark
        }
    }

    /// Charge the time since `mark` to a flush stage run on this thread, and return a fresh
    /// mark. A no-op without `bench-timing`, as [`Self::lap`] is.
    #[inline(always)]
    #[allow(unused_variables)]
    pub(crate) fn flush_lap(&self, stage: crate::flush::FlushStage, mark: StageMark) -> StageMark {
        #[cfg(feature = "bench-timing")]
        {
            let now = std::time::Instant::now();
            self.flush_stage_nanos[stage as usize].fetch_add(
                now.duration_since(mark.0).as_nanos() as u64,
                Ordering::Relaxed,
            );
            StageMark(now)
        }
        #[cfg(not(feature = "bench-timing"))]
        {
            mark
        }
    }

    /// One `execute_flush` returned on the pool: count it, count its rows if it succeeded, and
    /// add its laps. Called from the pool thread, after the return and before the completed unit
    /// is sent. The adds are separate relaxed stores, so a status read during this call can see
    /// the count without some of the laps; a flush still on the pool is in none of them.
    #[allow(unused_variables)]
    pub(crate) fn record_flush_execution(
        &self,
        laps: &crate::flush::FlushLaps,
        rows: Option<usize>,
    ) {
        self.flush_executions.fetch_add(1, Ordering::Relaxed);
        if let Some(rows) = rows {
            self.flush_rows_executed
                .fetch_add(rows as u64, Ordering::Relaxed);
        }
        #[cfg(feature = "bench-timing")]
        for (slot, nanos) in self.flush_stage_nanos.iter().zip(laps.nanos) {
            if nanos != 0 {
                slot.fetch_add(nanos, Ordering::Relaxed);
            }
        }
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
    /// does re-setting the limit — and the first of those is now a live path rather than a
    /// hypothetical: a fold's retirement withdraws the executed deletions, so an alarmed node that
    /// folds drops back under the limit and alarms again if it climbs back.
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

/// `Retry-After` for the buffer-occupancy 429, derived from the observed drain rate (ingest
/// §4.2): the buffer drains at a flush, which runs at the tick, so the wait is the time until the
/// next tick plus what draining `buffered` rows costs at the observed per-row figure, clamped to
/// the same floor and ceiling as [`estimate_retry_after_s`].
///
/// An estimator on the same terms as the queue's: the per-row figure is an EWMA over published
/// flushes and the next flush may be slower, and a flush already on the pool is not counted, so
/// the answer is a floor on a busy node. Before any flush has published the per-row figure is `0`
/// and the answer is the time to the next tick alone, which is the floor an operator would choose
/// for lack of evidence.
pub fn estimate_buffer_retry_after_s(stats: &ExecutorStats, buffered: u64) -> u64 {
    let drain = (buffered as u128).saturating_mul(stats.flush_nanos_per_row_ewma as u128);
    let nanos = (stats.next_tick_in_nanos as u128).saturating_add(drain);
    let secs = nanos.div_ceil(1_000_000_000);
    (secs.max(RETRY_AFTER_MIN_SECS as u128) as u64).min(RETRY_AFTER_MAX_SECS)
}

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
    /// producers are the named constructors below — the ones `scripts/check-layers.sh` pins to this
    /// file.
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

        /// A command that resolved to **no change at all**: every key it named exists and nothing
        /// was joining, so no record was appended and no structure moved.
        ///
        /// This is honest without a swap for the one reason none of the others can claim — there is
        /// no effect whose being in force could lag the ack. It is the narrowest of the four and
        /// the easiest to misuse: *nothing to do* and *not done yet* are the same shape from the
        /// caller's side and opposite from the service's, so a site reaching for this must have
        /// established the first. Takes the resolved batch, on the constructors above's rule: the
        /// argument is the evidence that something was looked at.
        pub(super) fn nothing_to_apply(_resolved: &[tessera_lifecycle::IncomingGrowth]) -> Self {
            Published(())
        }

        /// The same, for a `PUT` whose every key the level held with every part identical
        /// (`ingest.md` §1.5): the preparation carries no record, so nothing moved. Takes the
        /// prepared batch, on `nothing_to_apply`'s rule.
        pub(super) fn nothing_prepared(_prepared: &tessera_lifecycle::PreparedPut) -> Self {
            Published(())
        }

        /// An attribute declaration that met a column already carrying its identity
        /// (`ingest.md` §1.1: a part present and identical): nothing was appended and nothing
        /// moved, the effect being in force from the build or the earlier declaration. Takes the
        /// request that was looked up, on the constructors above's rule.
        pub(super) fn already_declared(_held: &tessera_lifecycle::AttributeRequest) -> Self {
            Published(())
        }

        /// A page of vocabulary values that bound nothing and filled nothing
        /// (`ingest.md` §1.1: every part present and identical). Nothing was appended and nothing
        /// moved. Takes the page, on the constructors above's rule.
        pub(super) fn nothing_bound(_page: &[tessera_lifecycle::DeclaredValue]) -> Self {
            Published(())
        }

        /// A view group or plain view declaration that met the object already carrying its
        /// identity (`ingest.md` §1.1): nothing was appended and nothing moved. Takes the name
        /// that was looked up, on the constructors above's rule.
        pub(super) fn already_declared_view(_name: &str) -> Self {
            Published(())
        }

        /// A registry or artifact-store record applied. **Neither structure is carried by a
        /// generation**, which is why this is honest without a swap: `/v1/meta`, every reachability
        /// check and every membership read them from `LiveState` behind its own lock, so the effect
        /// is in force the instant `LayerRegistry::apply` or `ArtifactStore::apply` returns. A
        /// generation swap would prove something about row space, which a layer has none of and an
        /// artifact holds only through its members.
        ///
        /// Takes the applied record for the same reason the replay constructor takes its ids: the
        /// argument is the evidence, so the token cannot be minted at a site that has applied
        /// nothing.
        pub(super) fn registry_applied(_applied: &tessera_lifecycle::WalRecord) -> Self {
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
    established: Mutex<std::collections::HashMap<Vec<u8>, EntityId>>,
    established_inverse: Mutex<FxHashMap<EntityId, Vec<u8>>>,
    /// The descriptor resolver's extension state. Written from **one** side only: ingest resolves
    /// in the handler *before* submitting, because signature-sorted assignment needs the term set to
    /// compute a sort key before any ID exists — the structural exception argued at
    /// [`WritePath::resolve_terms`]. A change used to resolve on the executor *after* its own append
    /// was fsynced, which made this the one asymmetry in the type; that deferred pass had no
    /// consumer once the evaluate store went and is deleted (decision 0048).
    resolver_state: Mutex<ResolverState>,
    accepted_batches: Mutex<AcceptedBatches>,
    /// The annotation layer registry. **Written only by the executor** — a registration is a WAL
    /// append followed by an apply, on the one thread that also holds the allocator — and read by
    /// the request path, which resolves a session's reachable set from it.
    registry: Mutex<LayerRegistry>,
    /// Every artifact's entity-space membership, on the registry's contract: written only by the
    /// executor, read by the request path.
    ///
    /// **Beside the registry rather than inside it**, because their lifetimes differ: a layer's
    /// declaration is small, published into every manifest and rebuilt from one; a membership is
    /// large and — until the packaging question is settled — lives only in the log. Folding them
    /// into one structure would put the second's durability problem onto the first.
    artifacts: Mutex<ArtifactStore>,
    /// The view roster, on the registry's contract: **written only by the executor** — a create is
    /// a WAL append followed by an apply, on the one thread that also holds the allocator — and
    /// read by the request path, which resolves a view id against the manifest the roster made.
    roster: Mutex<tessera_lifecycle::ViewRoster>,
    /// The attribute columns declared while the service runs and not yet folded into a
    /// `MANIFEST.json`, on the roster's contract: **written only by the executor** (a declaration
    /// is a WAL append followed by an apply) and read at every side-manifest publication, which is
    /// the declaration's durable home (`ingest.md` §6.3).
    attributes: Mutex<crate::attributes::RuntimeAttributes>,
    /// The vocabularies declared while the service runs and not yet folded into a
    /// `MANIFEST.json`, on the attribute list's contract: written only by the executor, read at
    /// every side-manifest publication (`ingest.md` §1.3).
    vocabularies: Mutex<crate::vocabularies::RuntimeVocabularies>,
    /// The view groups and plain views declared while the service runs and not yet folded, on
    /// the vocabulary list's contract (`ingest.md` §1.3, §10 R9).
    view_declarations: Mutex<crate::view_declarations::RuntimeViewDeclarations>,
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

    fn allocator_low_water(&self) -> u64 {
        lock_recover(&self.allocator).low_water()
    }

    /// The registry as a manifest carries it, plus the mark that must be published beside it.
    ///
    /// **The three travel together and that is the point of returning them from one lock.** A
    /// manifest carrying a layer whose reserved run sits above the published mark would, at the
    /// next restart, hand that run out again — so reading them separately, with a registration in
    /// between, is a way to publish exactly that inconsistency.
    /// Runs `f` with both the registry and the allocator held, in that lock order.
    ///
    /// **One critical section, because a registration reads one and writes both.** Taking them
    /// separately would let a second registration allocate between the name check and the run
    /// allocation, and the pair would then disagree about which ids a layer holds. The order —
    /// registry then allocator — is the only order taken anywhere, which is what keeps it from
    /// deadlocking against [`LiveState::registry_for_publication`].
    fn with_registry_and_allocator<R>(
        &self,
        f: impl FnOnce(&mut LayerRegistry, &mut Allocator) -> R,
    ) -> R {
        let mut registry = lock_recover(&self.registry);
        let mut alloc = lock_recover(&self.allocator);
        f(&mut registry, &mut alloc)
    }

    fn apply_registry_record(&self, record: &WalRecord) {
        lock_recover(&self.registry).apply(record);
    }

    /// Runs `f` with the registry, the artifact store and the allocator held, in that lock order.
    ///
    /// **One critical section over all three, on `with_registry_and_allocator`'s argument.** A
    /// publication reads the level's cursor from the store, checks its keys, allocates against the
    /// registry's runs and writes both — so two batches taking the locks separately would be handed
    /// the same ordinals, and the second would overwrite the first's artifacts in place.
    ///
    /// The order — registry, store, allocator — extends the existing one rather than interleaving
    /// with it, which is what keeps the two from deadlocking against each other.
    fn with_publication_state<R>(
        &self,
        f: impl FnOnce(&mut LayerRegistry, &mut ArtifactStore, &mut Allocator) -> R,
    ) -> R {
        let mut registry = lock_recover(&self.registry);
        let mut artifacts = lock_recover(&self.artifacts);
        let mut alloc = lock_recover(&self.allocator);
        f(&mut registry, &mut artifacts, &mut alloc)
    }

    /// The bound rotation may not reclaim past, or `None` if no membership is at risk. See
    /// [`ArtifactStore::oldest_wal_pos`] — this pins the log, deliberately and visibly, until
    /// membership has a home outside it.
    fn artifacts_oldest_wal_pos(&self) -> Option<u64> {
        lock_recover(&self.artifacts).oldest_wal_pos()
    }

    /// Read the artifact store — the request path's route to a membership.
    pub(crate) fn with_artifacts<R>(&self, f: impl FnOnce(&ArtifactStore) -> R) -> R {
        f(&lock_recover(&self.artifacts))
    }

    /// Everything not yet in a manifest, packed and ready — see [`ArtifactStore::unpublished`].
    fn unpublished_memberships(
        &self,
    ) -> (
        Vec<tessera_lifecycle::membership::PendingExtent>,
        Vec<(String, u32)>,
    ) {
        lock_recover(&self.artifacts).unpublished()
    }

    /// The supplied content of every artifact not yet in a manifest — see
    /// [`tessera_lifecycle::membership::ArtifactStore::unpublished_content`].
    fn unpublished_content(&self) -> Vec<(tessera_types::EntityId, Vec<(u16, String)>)> {
        lock_recover(&self.artifacts).unpublished_content()
    }

    /// Record every level as published to its current extent, and with that release the log.
    ///
    /// **Called only once the manifest naming the extents is durable.** The recomputation is over
    /// what the store holds *now* rather than over what was packed: the executor is the only writer,
    /// so nothing has been added since the pack, and recomputing is one fewer thing to keep in step
    /// than threading the packed ranges back through.
    fn mark_memberships_published(&self) {
        let mut artifacts = lock_recover(&self.artifacts);
        let levels: Vec<(String, u32, u32)> = artifacts
            .levels_and_extents()
            .map(|(layer, level, len)| (layer.to_string(), level, len))
            .collect();
        for (layer, level, len) in levels {
            artifacts.mark_published(&layer, level, len);
        }
    }

    /// Record that the content extent carrying every pending content fill is named by a durable
    /// manifest — see [`tessera_lifecycle::membership::ArtifactStore::mark_content_published`].
    ///
    /// **Called from the overlay publication and not from the fold.** The publication writes the
    /// content extent (`write_content_extent`) before its manifest; the fold carries content
    /// extents forward unchanged, so a fill pending at a fold is still pending after it.
    fn mark_content_published(&self) {
        lock_recover(&self.artifacts).mark_content_published();
    }

    /// Release the log from every growth the fold's whole rewrite has just made durable — see
    /// [`tessera_lifecycle::membership::ArtifactStore::mark_growth_packed`].
    ///
    /// **Called from the fold and from nowhere else.** `mark_memberships_published` is the
    /// append-only packer's mark and covers only the tail above each level's high-water; a growth
    /// lands below it. Releasing the pin there would leave a join durable nowhere the next restart
    /// reads.
    fn mark_growth_packed(&self) {
        lock_recover(&self.artifacts).mark_growth_packed();
    }

    /// Read the resident memberships back through the extents a publication has just written, so
    /// each one is a view over the live prefix rather than a heap bitmap or a view into a prefix
    /// that is about to be unlinked.
    ///
    /// **The fold's, and the seed's own rule applied to a rewrite** (`Engine::open`). A membership
    /// seeded from the previous prefix holds that prefix's pack alive: reclamation unlinks the
    /// directory, the mapping survives the directory entry, and the file's blocks stay allocated
    /// with no name to see them under. Rehousing onto the extents this fold wrote drops those
    /// packs at the moment the fold makes them redundant, and re-maps every membership the
    /// retirement copied to the heap through `Members::to_mut`.
    ///
    /// **After the resident retirement, never before.** [`ArtifactStore::rehouse_members`] refuses
    /// a membership whose cardinality differs from the one it replaces, and what the extent holds
    /// is the post-retirement set; running it first would refuse every artifact this fold took a
    /// member from and leave those on the heap.
    ///
    /// Returns how many memberships took and how many did not. A pack that will not open leaves
    /// its level on the heap and alarms; a blob the store cannot match is the caller's alarm to
    /// raise, and what it means is in the call sites.
    ///
    /// **The cost is one file mapping per extent and one checked decode per record, under the
    /// artifacts mutex.** The decode is [`Members::mapped`]'s own validation, which walks a
    /// bitmap's header region rather than its values, and the whole pass is `O(artifacts in the
    /// extents)` — every artifact the node holds, at a fold. Nothing reads the store while it runs.
    /// ⊘ The stall is unmeasured above 2.5×10⁵ artifacts (25,846,007 GBIF occurrences, 2026-09-13,
    /// where it was not separable from the fold around it); at 10⁶ and beyond it is a request-path
    /// pause nobody has put a number on.
    fn rehouse_memberships(
        &self,
        prefix_dir: &std::path::Path,
        extents: &[tessera_store::manifest::MembershipExtent],
    ) -> (u64, u64) {
        let mut artifacts = lock_recover(&self.artifacts);
        let (mut rehoused, mut kept) = (0u64, 0u64);
        for extent in extents {
            let path = prefix_dir.join(&extent.path);
            let pack = match tessera_store::membership::MembershipPack::open(&path) {
                Ok(pack) => Arc::new(pack),
                Err(error) => {
                    tracing::error!(
                        path = %path.display(),
                        %error,
                        "ALARM: an extent this node wrote and fsynced a moment ago would not \
                         open; its memberships stay on the heap, where they answer exactly as \
                         before, and the file a restart reads is the one that would not open"
                    );
                    continue;
                }
            };
            let owner: Arc<dyn std::any::Any + Send + Sync> = pack.clone();
            for (ordinal, blob) in pack.iter() {
                // An empty blob is a hole: an ordinal a retirement emptied, or one no artifact was
                // ever published at. There is nothing to rehouse and nothing is wrong.
                if blob.is_empty() {
                    continue;
                }
                // SAFETY: the seed's contract, over a file this process has just written
                // (`Engine::open`). `blob` is a slice of `pack`'s read-only mapping and `owner` is
                // that same pack, held by every `Members` the mapping produces.
                let mapped =
                    unsafe { tessera_lifecycle::membership::mapped_members(blob, owner.clone()) };
                let took = match mapped {
                    // `MembershipPack::iter` answers the absolute ordinal, which is what the
                    // store addresses by.
                    Some(members) => {
                        artifacts.rehouse_members(&extent.layer, extent.level, ordinal, members)
                    }
                    None => false,
                };
                match took {
                    true => rehoused += 1,
                    false => kept += 1,
                }
            }
        }
        (rehoused, kept)
    }

    /// Apply the fold's executed deletions to the resident artifact store — the second half of the
    /// artifact pass, run once the prefix carrying the rewritten extents is live. Retired artifacts
    /// leave their levels; retired members leave the memberships that survive; and every content
    /// whose generating set lost a source is withdrawn (decision 0135; the fold's report already
    /// named it). Returns the levels the retirement moved, which the fold compares with what it
    /// stamped.
    fn retire_artifacts(&self, retired: &croaring::Bitmap) -> Vec<(String, u32)> {
        lock_recover(&self.artifacts).retire(retired)
    }

    /// Where an entity sits: `(layer, level, ordinal)`. Addressing only — see
    /// [`LayerRegistry::locate`].
    pub(crate) fn locate_artifact(&self, entity: EntityId) -> Option<(String, u32, u32)> {
        lock_recover(&self.registry)
            .locate(entity)
            .map(|(name, level, ordinal)| (name.to_string(), level, ordinal))
    }

    /// Resolve which layers a principal may know exist. See `LayerRegistry::resolve_for` — one set
    /// probe answers a gate-failed name and a never-registered one alike.
    pub(crate) fn resolve_layers(
        &self,
        is_satisfied: impl Fn(tessera_types::TermId) -> bool,
        resolve_label: impl Fn(&str) -> Option<tessera_types::TermId>,
    ) -> tessera_lifecycle::ResolvedLayers {
        lock_recover(&self.registry).resolve_for(is_satisfied, resolve_label)
    }

    /// Every registered layer, as the registry holds it. **The declarations, never a decision** —
    /// `registered_layer`'s rule over the whole set, and the caller applies the gate.
    ///
    /// Its one caller is `Engine::warm_artifact_projections`, which has no principal to resolve
    /// against: it builds a level's row form, which is the same structure for every principal, and
    /// the gate is applied to what is *served* from it on the request that asks.
    pub(crate) fn registered_layers(&self) -> Vec<tessera_types::layer::RegisteredLayer> {
        lock_recover(&self.registry).snapshot().0
    }

    /// One registered layer, by name. The caller has already established the name is reachable —
    /// this returns the declaration, never the decision.
    pub(crate) fn registered_layer(
        &self,
        name: &str,
    ) -> Option<tessera_types::layer::RegisteredLayer> {
        lock_recover(&self.registry).get(name).cloned()
    }

    /// Every `membership = { attribute = f }` layer, with the declared-scalar index of `f` and the
    /// vocabulary that column's values are named by — `(layer, index, vocabulary)`.
    ///
    /// **A layer whose column this bundle does not declare is skipped**, which mints nothing: such
    /// a declaration is refused at registration, so an absence here is one that never validated and
    /// the fail-closed reading is that the layer holds no values.
    ///
    /// `column_of` is the caller's — the manifest's `declared_scalars` is a generation's, and this
    /// holds the registry rather than a generation.
    fn predicate_columns(
        &self,
        column_of: impl Fn(&str) -> Option<(usize, Option<String>)>,
    ) -> Vec<(String, usize, Option<String>)> {
        let registry = lock_recover(&self.registry);
        registry
            .iter()
            .filter_map(|(name, registered)| {
                let tessera_types::layer::MembershipSource::Attribute(field) =
                    &registered.declaration.membership
                else {
                    return None;
                };
                column_of(field).map(|(index, vocabulary)| (name.to_string(), index, vocabulary))
            })
            .collect()
    }

    /// Record one level's re-evaluated serving layout, returning whether it **moved**.
    ///
    /// **Under the registry's own lock and before the snapshot**, which is the whole of the
    /// ordering the selection memo §5 requires: the manifest is written from
    /// [`LiveState::registry_for_publication`], so a layout recorded after that snapshot would
    /// reach neither the files nor the record.
    fn record_layout(
        &self,
        layer: &str,
        level: u32,
        layout: tessera_types::layer::ServingLayout,
    ) -> bool {
        lock_recover(&self.registry).set_layout(layer, level, layout)
    }

    /// Run `f` with the roster held — the create and drop preparations, and nothing else.
    fn with_roster<R>(&self, f: impl FnOnce(&mut tessera_lifecycle::ViewRoster) -> R) -> R {
        let mut roster = lock_recover(&self.roster);
        f(&mut roster)
    }

    /// Run `f` with the runtime attribute list held: the declaration's apply and the fold's
    /// retirement, and nothing else.
    fn with_attributes<R>(
        &self,
        f: impl FnOnce(&mut crate::attributes::RuntimeAttributes) -> R,
    ) -> R {
        let mut attributes = lock_recover(&self.attributes);
        f(&mut attributes)
    }

    /// What a publication carries forward: the declarations no fold has written into a
    /// `MANIFEST.json`, complete current state, on [`Self::roster_for_publication`]'s contract.
    fn attributes_for_publication(
        &self,
    ) -> (
        Vec<tessera_store::manifest::DeclaredScalar>,
        Vec<tessera_store::manifest::ScopedScalar>,
    ) {
        lock_recover(&self.attributes).snapshot()
    }

    /// Run `f` with the runtime vocabulary list held: the declaration's apply, a page's apply and
    /// the fold's retirement, and nothing else.
    fn with_vocabularies<R>(
        &self,
        f: impl FnOnce(&mut crate::vocabularies::RuntimeVocabularies) -> R,
    ) -> R {
        let mut vocabularies = lock_recover(&self.vocabularies);
        f(&mut vocabularies)
    }

    /// What a publication carries forward: the vocabularies no fold has written into a
    /// `MANIFEST.json`, with their values as the live minters hold them, on
    /// [`Self::attributes_for_publication`]'s contract.
    fn vocabularies_for_publication(
        &self,
        vocabularies: &Vocabularies,
    ) -> Vec<tessera_store::manifest::ManifestVocabulary> {
        lock_recover(&self.vocabularies).snapshot(vocabularies)
    }

    /// Run `f` with the runtime view declarations held: a declaration's apply and the fold's
    /// retirement, and nothing else.
    fn with_view_declarations<R>(
        &self,
        f: impl FnOnce(&mut crate::view_declarations::RuntimeViewDeclarations) -> R,
    ) -> R {
        let mut declarations = lock_recover(&self.view_declarations);
        f(&mut declarations)
    }

    /// What a publication carries forward: the view groups and plain views no fold has written
    /// into a `MANIFEST.json`, complete current state.
    fn view_declarations_for_publication(
        &self,
    ) -> (
        Vec<tessera_store::manifest::GroupDescriptor>,
        Vec<tessera_store::manifest::ViewDescriptor>,
    ) {
        lock_recover(&self.view_declarations).snapshot()
    }

    /// What a publication carries forward: the creations and the dead incarnations, complete
    /// current state — [`Self::registry_for_publication`]'s contract, for the roster.
    fn roster_for_publication(
        &self,
    ) -> (
        Vec<tessera_types::view::CreatedView>,
        Vec<tessera_types::view::DeadIncarnation>,
    ) {
        lock_recover(&self.roster).snapshot()
    }

    fn registry_for_publication(
        &self,
    ) -> (Vec<tessera_types::layer::RegisteredLayer>, Vec<String>, u64) {
        let registry = lock_recover(&self.registry);
        let low_water = lock_recover(&self.allocator).low_water();
        let (layers, tombstones) = registry.snapshot();
        (layers, tombstones, low_water)
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
    /// `is_deleted` is the overlay's verdict on the current holder: **a deleted holder does not
    /// collide** (decision 0047 — edit is delete + re-ingest, and our retention of a dead binding
    /// must never refuse a user's write). A *suppressed* holder still collides: suppression is
    /// temporary hiding, and re-ingesting past one is the byte-identical-copy hole this check
    /// exists to close.
    /// The apply-adjacent backstop for the check-to-apply race, **rewritten as the join rule**
    /// (`views.md` §4): a known external id is a collision only where the entity it names already
    /// has a row in the view this batch names. Anywhere else it is a join, and the row is stamped
    /// with the entity it joins — here rather than in the handler's answer, because this map and
    /// this generation are the ones the apply will clone from.
    ///
    /// **A deleted holder is neither** (decision 0047): the binding is dead bookkeeping and the
    /// row allocates fresh, which is what makes a re-ingest under the same external id land.
    fn established_collisions(
        &self,
        rows: &mut [UnallocatedRow],
        is_deleted: impl Fn(EntityId) -> bool,
        holds: impl Fn(EntityId, &str) -> bool,
    ) -> usize {
        let established = lock_recover(&self.established);
        let mut collisions = 0;
        for row in rows.iter_mut() {
            let Some(id) = row.external_id.as_ref() else {
                continue;
            };
            let Some(entity) = established.get(id.as_slice()).copied() else {
                continue;
            };
            if is_deleted(entity) {
                row.join = None;
                continue;
            }
            if holds(entity, &row.view) {
                collisions += 1;
                continue;
            }
            row.join = Some(entity);
        }
        collisions
    }

    /// Drop every retired entity's external-id binding from the live map — **the other half of
    /// Rule F's retirement, and without it retirement 409s a lawful re-ingest.**
    ///
    /// Compaction §3 pass 3 drops the retired entities' keys from the folded run 0, and says in
    /// the same breath that the fold *also* removes them from the live external-id map, because
    /// "either alone leaves the other path answering". This is that other path. Both duplicate
    /// checks — the handler's in `control.rs` and [`Self::established_collisions`] here — exempt a
    /// holder only while `overlay.is_deleted(holder)` is true, and retirement is precisely what
    /// makes it false. A binding left standing therefore turns a re-ingest of that external id
    /// into a **409** the moment the fold publishes: a user's write refused, contradicting decision
    /// 0047 directly, and permanently, since nothing else ever removes a key.
    ///
    /// **Called before the swap, not after**, at the one site that also retires
    /// ([`Executor::publish_geometry`]). Between a prune and a retirement the key is simply absent
    /// from the live map and the bundle's own sidecar still answers for it, whose holder is still
    /// deleted — so the check still exempts and the write is still allowed. The other order has a
    /// window in which the key resolves to an entity that is no longer deleted, which is the 409
    /// this exists to close, narrowed but not removed.
    ///
    /// **A rebind is not disturbed.** Decision 0047 makes edit a delete plus a re-ingest, so an
    /// external id whose deleted holder has already been re-ingested maps to the *new* entity here.
    /// The forward entry is removed only when it still names the retired entity, so retiring the
    /// forgotten holder cannot unbind the live one.
    fn forget_established(&self, retired: &croaring::Bitmap) -> usize {
        if retired.is_empty() {
            return 0;
        }
        let mut inverse = lock_recover(&self.established_inverse);
        let mut established = lock_recover(&self.established);
        let mut forgotten = 0usize;
        for entity in retired.iter() {
            let entity = EntityId::new(u64::from(entity));
            let Some(key) = inverse.remove(&entity) else {
                continue;
            };
            if established.get(&key) == Some(&entity) {
                established.remove(&key);
            }
            forgotten += 1;
        }
        forgotten
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
    /// A row's coordinates fall outside the view's declared quantisation extent, so the point has
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
        x: f64,
        y: f64,
        quantisation: tessera_store::manifest::Quantisation,
    },
    /// A row names a view this bundle does not declare, so there is no frame to quantise it
    /// against and no row space for it to land in.
    ///
    /// Checked here, beside [`Self::OutsideExtent`] and for the same more-than-one-caller reason:
    /// since the extent became the view's (decision 0040), resolving a row's frame *is* resolving
    /// its view, and a row whose view cannot be resolved has nothing to be checked against. The
    /// HTTP handler refuses an unknown `x-tessera-view` with its own 404 and is only one of the
    /// buffer's writers.
    UnknownView {
        index: usize,
        view: String,
    },
    /// A row carries more scalars than the schema declares columns.
    ///
    /// **The commit window indexes `row.scalars` positionally against `MANIFEST.declared_scalars`**
    /// — that is how a category's key finds its vocabulary — so a row longer than the schema would
    /// pair values with columns that do not exist. A row **shorter** than the schema is lawful
    /// (`ingest.md` §7.1): a column declared at a running service appends at the tail, so a row
    /// decoded against the schema before the declaration, or a batch omitting a column, holds
    /// nothing for the positions it lacks, and the window's close pads it with each one's absence
    /// (`crate::attributes::pad_to_schema`) before anything indexes it.
    ///
    /// Checked at the engine's boundary for the same more-than-one-caller reason as
    /// [`Self::OutsideExtent`]: the invariant is about the buffer, and the HTTP handler is only one
    /// of the buffer's writers. The declared list is the **full** one, filterable-only columns
    /// included: a row's scalars cover every declared column, and only the *segment* narrows to
    /// the render ones.
    ScalarArity {
        index: usize,
        expected: usize,
        got: usize,
    },
    /// A partition is serving a stepped-down side-manifest (owner-ruled gate, 2026-08-04;
    /// write-path §5.6). Ingest is refused **at the engine's boundary**, for the same
    /// more-than-one-caller reason as [`Self::OutsideExtent`]: a stepped-down node that accepted
    /// and flushed would assemble its manifest from the *older served* partition state at a
    /// higher `n`, permanently shadowing the stepped-past segment — and once rotation moves the
    /// reclaim bound, its acked rows are unrecoverable. Denies are deliberately **not** gated:
    /// a deny is entity-space state carried by WAL and manifest deny fields, threatens no
    /// segment, and must never be refused.
    SteppedDown,
}

impl std::fmt::Display for AcceptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AcceptError::Submit(e) => write!(f, "{e}"),
            AcceptError::Exec(e) => write!(f, "{e}"),
            AcceptError::ScalarArity {
                index,
                expected,
                got,
            } => write!(
                f,
                "row {index} carries {got} scalars, but the schema declares {expected}. A row \
                 carries at most one value per declared column, in declaration order; a column it \
                 omits at the tail is absent"
            ),
            AcceptError::OutsideExtent {
                index,
                x,
                y,
                quantisation: q,
            } => write!(
                f,
                "ingest row {index} at ({x}, {y}) is outside this view's declared extent (x {}..{}, \
                 y {}..{}). Coordinates are quantised against that extent, which is fixed for the \
                 view's life (decision 0040), so an out-of-extent point has no cell to occupy; it \
                 is refused here rather than clamped, because a clamped point at the boundary \
                 cannot be told from one that belongs there. The remedy is to rebuild the view \
                 under a corrected extent, which is a migration",
                q.x_min, q.x_max, q.y_min, q.y_max
            ),
            AcceptError::UnknownView { index, view } => write!(
                f,
                "ingest row {index} names view '{view}', which this bundle does not declare. A \
                 view carries its own frame and its own row space (decision 0040), so a row \
                 naming none of them has no cell to occupy and no order to be placed in"
            ),
            AcceptError::SteppedDown => write!(
                f,
                "a partition is serving a stepped-down side-manifest, so ingest is refused: a \
                 flush from this state would assemble its manifest from the older served state at \
                 a higher n, permanently shadowing the stepped-past segment and its acked rows. \
                 Repair or restore the damaged newest manifest's files, then retry"
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
    established: std::collections::HashMap<Vec<u8>, EntityId>,
    established_inverse: FxHashMap<EntityId, Vec<u8>>,
    resolver_state: ResolverState,
    accepted_batches: AcceptedBatches,
    pub(crate) registry: LayerRegistry,
    pub(crate) artifacts: ArtifactStore,
    /// The view roster — which views of which groups exist, and which keys are burnt
    /// (`views.md` §3.2). Rebuilt exactly as the layer registry beside it is: seeded from the
    /// manifests, then the log replayed on top.
    pub(crate) roster: tessera_lifecycle::ViewRoster,
    /// The attribute columns declared at a running service and not yet folded (`ingest.md`
    /// §6.3), rebuilt as the roster is: seeded from the manifests, then the log replayed on top.
    pub(crate) attributes: crate::attributes::RuntimeAttributes,
    /// The vocabularies declared at a running service and not yet folded (`ingest.md` §1.3),
    /// rebuilt as the attribute columns beside them are.
    pub(crate) vocabularies: crate::vocabularies::RuntimeVocabularies,
    /// The view groups and plain views declared at a running service and not yet folded
    /// (`ingest.md` §1.3), rebuilt as the vocabularies beside them are.
    pub(crate) view_declarations: crate::view_declarations::RuntimeViewDeclarations,
}

/// The manifest state a reconstruction starts from, before WAL replay unions what was written
/// since: both entity-space marks and the registry's complete current view.
///
/// **A struct rather than four arguments, and the four travel together for a reason.** A layer in
/// `layers` whose reserved run sits above `low_water` is an inconsistency that reissues ids, so
/// assembling them at one site — where they are all read from the same manifests — is what keeps
/// them agreeing. Adding a fifth stays a compile error there rather than a defaulted argument here.
pub(crate) struct ManifestSeed<'a> {
    /// `max(build MANIFEST, side manifests)` — the point region's floor.
    pub high_water: u64,
    /// `min(ceiling, side manifests)` — the row-less region's ceiling. A build carries a term here
    /// whenever its declaration carried layers: the layers it registered spent row-less ids, and the mark
    /// recording that has to survive into what this seeds from, or the first online registration
    /// reissues them.
    pub low_water: u64,
    pub layers: &'a [tessera_types::layer::RegisteredLayer],
    pub tombstones: &'a [String],
    /// Every view created since the build, across every partition's manifest (`views.md` §3.2),
    /// and every incarnation that has died. The build's own roster is not here: it is in
    /// `MANIFEST.json` and is seeded separately.
    pub created_views: &'a [tessera_types::view::CreatedView],
    pub dead_view_incarnations: &'a [tessera_types::view::DeadIncarnation],
    /// The views a build declared, as `(group, key)` — the keys a create must not reissue.
    pub declared_views: Vec<(String, String)>,
    /// `Manifest::view_ids_for_key` — every view id a dropped key resolves to, the owner's and
    /// every sharing group's (`views.md` §3.3). Replay's `ViewDrop` arm prunes the buffer with it,
    /// and it is passed rather than derived because `tessera-lifecycle` holds no manifest.
    pub view_ids_of_key: &'a dyn Fn(&str, &str) -> Vec<String>,
    /// Every published membership extent, across every partition's manifest, with the prefix
    /// directory their paths are relative to.
    pub membership_extents: &'a [tessera_store::manifest::MembershipExtent],
    /// Every `(layer, level)`'s artifact-write counter as of the publication, across every
    /// partition's manifest.
    ///
    /// **Seeded after the records and before the replay**, which is what makes a coordinate
    /// written before a restart comparable with one after it: seeding a level's records bumps its
    /// version once per record, so without this a level comes back at its record count rather than
    /// at the number the publication recorded — and every derived structure keyed on the version
    /// would be rejected on every restart. See `ArtifactStore::seed_level_version`.
    pub level_versions: &'a [tessera_store::manifest::LevelVersion],
    pub prefix_dir: std::path::PathBuf,
    /// The served schema as the manifests make it: the build's columns with every side manifest's
    /// runtime declarations appended (`Manifest::with_attributes`). Replay compares each
    /// `AttributeDeclare` record against it, so a record restating a folded column is applied as
    /// nothing and one contradicting the manifests refuses the open.
    pub manifest: &'a tessera_store::manifest::Manifest,
    /// The side manifests' `attributes` and `scoped_attributes`, the runtime declarations no fold
    /// has written into a `MANIFEST.json`; replay appends to these.
    pub attributes: crate::attributes::RuntimeAttributes,
    /// The side manifests' `vocabularies`, on [`Self::attributes`]' rule; replay appends to it.
    pub vocabularies: crate::vocabularies::RuntimeVocabularies,
    /// The side manifests' `groups` and `plain_views`, on [`Self::attributes`]' rule; replay
    /// appends to them.
    pub view_declarations: crate::view_declarations::RuntimeViewDeclarations,
}

/// The levels a fold's retirement is about to move, and the set it retires.
///
/// A fold writes its manifest before it retires, because the retirement is not reversible and a
/// manifest that would not commit must leave it undone. So at step 3a the store holds the
/// pre-retirement records while the prefix being written holds the post-retirement ones. The
/// retirement changes a level in three ways: an artifact whose own entity is retired leaves (its
/// slot becomes a hole), a surviving artifact loses the retired members from its membership, and a
/// content whose generating set lost a member is dropped or shrunk.
///
/// For a row column and a tile index only the first matters. Both are the memberships projected
/// through the row space this fold wrote, and a retired entity has no row in it (pass 1 dropped
/// them), so a surviving artifact projects to the same rows before and after its membership
/// shrinks; a generating set is read from the store's records and from neither structure. So the
/// fold composes both from the store's records without the artifacts the retirement removes
/// ([`Self::records`]) and stamps them with the version the level will have once it has run
/// ([`Self::version_after`]). A containment partition is rank-sensitive, since a dropped content
/// shifts the ranks after it, and a spatial level's row forms are resolved from shapes the
/// retirement removes, so those stay omitted for a pending level and recompose on first use.
///
/// The version after the retirement is the store's plus one for a level here and the store's own
/// otherwise: `ArtifactStore::retire` moves a level it changes by one, and it reads the same
/// predicate `levels_moved_by` read to fill this. `publish_fold` checks the two agree after the
/// retirement and drops any structure whose stamp the store does not then carry.
///
/// What happens when the retirement does not follow the manifest. A fold discarded between the
/// manifest and the flip leaves a prefix `CURRENT` never names, which nothing opens. A process
/// that dies after the flip restarts from the manifest: its records are the post-retirement ones
/// and its stated version is the stamped one, so the structures describe what was seeded, and any
/// record the log replays over them moves the version and they are refused at the version check.
/// In every case the level recomposes on first use, at a cost and never with another level's
/// answer.
struct PendingRetirement {
    levels: Vec<(String, u32)>,
    retired: croaring::Bitmap,
}

impl PendingRetirement {
    fn is_pending(&self, layer: &str, level: u32) -> bool {
        self.levels.iter().any(|(l, v)| l == layer && *v == level)
    }

    /// The version `layer`'s `level` will have once this fold's retirement has run.
    fn version_after(&self, store: &ArtifactStore, layer: &str, level: u32) -> u64 {
        store.level_version(layer, level) + u64::from(self.is_pending(layer, level))
    }

    /// The level's records as the retirement will leave them: every artifact but those whose own
    /// entity is retired.
    fn records<'s>(
        &'s self,
        store: &'s ArtifactStore,
        layer: &str,
        level: u32,
    ) -> impl Iterator<Item = (u32, &'s tessera_lifecycle::membership::ArtifactRecord)> + 's {
        let retired = &self.retired;
        store
            .level(layer, level)
            .filter(move |(_, record)| !retired.contains(record.entity.raw() as u32))
    }
}

/// How many ordinals a level's derived structures cover: one past the highest live ordinal,
/// which is the length the reader sizes the level at (`ArtifactRows::build_over`). A hole below
/// it is covered and a hole at the top is not.
fn level_length<'a>(
    records: impl Iterator<Item = (u32, &'a tessera_lifecycle::membership::ArtifactRecord)>,
) -> u32 {
    records.map(|(ordinal, _)| ordinal + 1).max().unwrap_or(0)
}

/// The two artifact coordinates a manifest carries: every level's version, and the derived
/// structures whose stamped version is that version.
///
/// **Filtered, so the invariant is true by construction rather than checked at open**: every entry
/// a manifest names is one whose coordinate equals the version list beside it, so a manifest never
/// names a structure that has already been invalidated. An entry whose level has moved is dropped
/// here rather than carried and rejected later — carrying it would leave the prefix naming a file
/// nothing could ever adopt, which reads as a partition that exists.
///
/// `pending_retirement` is the levels this publication is about to change and has not yet, which
/// is the fold's own case ([`PendingRetirement`]). For those levels the version stated, and the
/// version an entry must carry to be named, is the store's plus one: the version the level will
/// have when the retirement has run, and the version the fold stamped the structures it composed
/// for such a level with. An entry a previous prefix held for the level carries the store's own
/// version or an older one and is dropped. Every other publication passes an empty slice, having
/// nothing pending.
fn artifact_coordinates(
    store: &ArtifactStore,
    held: &[tessera_store::manifest::ContainmentExtent],
    held_indexes: &[tessera_store::manifest::TileIndexExtent],
    held_columns: &[tessera_store::manifest::RowColumnExtent],
    held_shape_rows: &[tessera_store::manifest::ShapeRowsExtent],
    held_shape_held: &[tessera_store::manifest::ShapeHeldExtent],
    pending_retirement: &[(String, u32)],
) -> ArtifactCoordinates {
    let expected = |layer: &str, level: u32| {
        let pending = pending_retirement
            .iter()
            .any(|(l, v)| l == layer && *v == level);
        store.level_version(layer, level) + u64::from(pending)
    };
    let versions: Vec<tessera_store::manifest::LevelVersion> = store
        .level_versions()
        .map(|(layer, level, _)| tessera_store::manifest::LevelVersion {
            layer: layer.to_string(),
            level,
            version: expected(layer, level),
        })
        .collect();
    let still_true = held
        .iter()
        .filter(|entry| expected(&entry.layer, entry.level) == entry.level_version)
        .cloned()
        .collect();
    // The tile indexes take the same filter and for the same reason. The view an entry carries is
    // not part of it: a view is an address, not a validity term — a level's version is what says
    // whether the extents projected through *any* row space still describe it.
    let indexes_still_true = held_indexes
        .iter()
        .filter(|entry| expected(&entry.layer, entry.level) == entry.level_version)
        .cloned()
        .collect();
    // The row-major columns take the same filter, and the layout tag each carries is not part of
    // it: a tag says which *form* the file is in, and what decides whether it still describes the
    // level is the version, exactly as it is for an extent column.
    let columns_still_true = held_columns
        .iter()
        .filter(|entry| expected(&entry.layer, entry.level) == entry.level_version)
        .cloned()
        .collect();
    // The shape row forms take the same filter. The segment half of their key is not part of it:
    // a segment is immutable and its id never reused, so an entry naming one that no generation
    // serves any more is a file nothing will claim, and is dropped when the prefix is.
    let shape_rows_still_true = held_shape_rows
        .iter()
        .filter(|entry| expected(&entry.layer, entry.level) == entry.level_version)
        .cloned()
        .collect();
    let shape_held_still_true = held_shape_held
        .iter()
        .filter(|entry| expected(&entry.layer, entry.level) == entry.level_version)
        .cloned()
        .collect();
    ArtifactCoordinates {
        level_versions: versions,
        containment: still_true,
        tile_indexes: indexes_still_true,
        row_columns: columns_still_true,
        shape_rows: shape_rows_still_true,
        shape_held: shape_held_still_true,
    }
}

/// The entries of one fold-written list whose stamped version is the level's now, after the
/// fold's retirement has run; every other entry is dropped and named. See [`PendingRetirement`].
fn held_at_current_version<E: Clone>(
    store: &ArtifactStore,
    what: &str,
    entries: &[E],
    coordinate: impl Fn(&E) -> (&str, u32, u64),
) -> Vec<E> {
    entries
        .iter()
        .filter(|entry| {
            let (layer, level, stamped) = coordinate(entry);
            let now = store.level_version(layer, level);
            if now == stamped {
                return true;
            }
            tracing::error!(
                layer,
                level,
                stamped,
                now,
                "ALARM: a fold-written {what} is stamped with a version the level does not carry \
                 after the retirement; it is dropped and the level recomposes on first use"
            );
            false
        })
        .cloned()
        .collect()
}

/// What [`artifact_coordinates`] stamps into a side-manifest: the level versions and every
/// derived-structure list filtered to the entries still true at them.
struct ArtifactCoordinates {
    level_versions: Vec<tessera_store::manifest::LevelVersion>,
    containment: Vec<tessera_store::manifest::ContainmentExtent>,
    tile_indexes: Vec<tessera_store::manifest::TileIndexExtent>,
    row_columns: Vec<tessera_store::manifest::RowColumnExtent>,
    shape_rows: Vec<tessera_store::manifest::ShapeRowsExtent>,
    shape_held: Vec<tessera_store::manifest::ShapeHeldExtent>,
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
        seed: ManifestSeed<'_>,
        dict: &Dict,
        initial_deny: &[(EntityId, ChangeOp)],
        vocabularies: &mut Vocabularies,
        has_row: impl Fn(EntityId, &str) -> bool,
    ) -> Result<(Overlay, IngestBuffer, WritePathState), EngineError> {
        let (wal, records) = Wal::open(wal_path).map_err(EngineError::Wal)?;

        // **A record whose meaning is not built refuses the open, before anything is applied.**
        // The ingest design's records and fields are in the format ahead of their tracks
        // (`ingest.md` §7.1, §8) so that every track lands against one log; a log carrying one
        // was written by a binary this one is not, and replaying past it would serve state that
        // omits what the record said. Naming the track is what tells the operator which binary.
        for record in &records {
            if let Some((kind, track)) = tessera_lifecycle::wal::unbuilt_track(record) {
                return Err(EngineError::Malformed(format!(
                    "the WAL carries a {kind} record, whose apply path is track {track}'s and is \
                     not built (ingest.md §8); this node does not open"
                )));
            }
        }

        // **Mints apply over the manifest seed, in log order** — the same seed-before-replay rule
        // the deny state follows below, and for the same reason: every WAL record postdates the
        // manifests. The caller has already seeded from `MANIFEST.vocabularies` and the served
        // side-manifests' `vocabulary_extensions`, so what remains is what was minted after the
        // last manifest write.
        //
        // Bindings are append-only and never rebound, so this is order-insensitive except for
        // conflicts — and a conflict inside the durable prefix is corruption of acked state, never
        // a race: every row written under either binding is of unknowable colour. Refusing to open
        // is the only answer that does not silently recolour one of them.
        // **The vocabularies declared while the service ran, before the mints that name them**
        // (`ingest.md` §1.3): the manifests' runtime list is the starting point and every record
        // postdates it, on the registry's ordering rule. A record restating a vocabulary the
        // manifests already carry identically is applied as the values it names and nothing else
        // — what a fold that moved the declaration into `MANIFEST.json` before the log rotated
        // leaves behind — and one carrying a different identity under a held name is a log that
        // disagrees with the manifests about what every code of that vocabulary stands for, and
        // refuses the open.
        //
        // **A page of values is one of these records too**, not a run of `VocabularyMint`s: a
        // value's title is part of what the page acknowledged and a mint record carries none, so
        // a page recorded as mints would come back from a restart with its bindings and without
        // the names a client draws.
        let mut runtime_vocabularies = seed.vocabularies;
        for record in &records {
            let WalRecord::VocabularyDeclare { declaration } = record else {
                continue;
            };
            let compiled = crate::vocabularies::compile_record(declaration);
            match vocabularies.get_mut(&compiled.name) {
                Some(minter) => {
                    // Kind, visibility and the code space's width: the whole of what
                    // `crate::vocabularies::resolve` compares at the door, less the reserved list,
                    // which a minter holds as spent codes rather than as a list.
                    if minter.kind() != compiled.kind
                        || minter.visibility() != compiled.visibility
                        || minter.width().arrow_type_name() != compiled.width
                    {
                        return Err(EngineError::Malformed(format!(
                            "the WAL declares vocabulary '{}' with an identity the manifests do \
                             not carry for that name; every row holding one of its codes is of \
                             unknowable colour, so this node does not open",
                            compiled.name
                        )));
                    }
                    for value in &compiled.values {
                        minter
                            .seed_value(&value.key, value.code)
                            .map_err(|e| EngineError::Malformed(e.to_string()))?;
                        // Replayed in log order, so the last title a page supplied is the one
                        // the minter ends holding (decision 0136's amendment).
                        if let Some(title) = &value.title {
                            minter.set_title(&value.key, title.clone());
                        }
                    }
                    for &code in &compiled.reserved {
                        minter.seed_reserved(code);
                    }
                }
                None => {
                    // The width the declaration named, so a code drawn after this restart lands
                    // in the space the columns over it store (`ManifestVocabulary::width`).
                    let width = tessera_spatial::tiler::ScalarType::parse(&compiled.width)
                        .unwrap_or(tessera_spatial::tiler::ScalarType::U32);
                    let mut minter = tessera_store::vocabulary::VocabularyMinter::new(
                        compiled.name.clone(),
                        compiled.kind,
                        compiled.visibility,
                        width,
                    );
                    minter
                        .seed_manifest(&compiled)
                        .map_err(|e| EngineError::Malformed(e.to_string()))?;
                    vocabularies.insert(minter);
                }
            }
            // The runtime list is what the next publication writes. A name `MANIFEST.json`
            // already carries is one a fold has folded in, and belongs to one list, not two.
            if !runtime_vocabularies.holds(&compiled.name)
                && !seed
                    .manifest
                    .vocabularies
                    .iter()
                    .any(|v| v.name == compiled.name)
            {
                runtime_vocabularies.push(ManifestVocabulary {
                    values: Vec::new(),
                    ..compiled
                });
            }
        }

        for record in &records {
            if let WalRecord::VocabularyMint {
                vocabulary,
                key,
                code,
            } = record
            {
                let minter = vocabularies.get_mut(vocabulary).ok_or_else(|| {
                    EngineError::Malformed(format!(
                        "the WAL mints into vocabulary '{vocabulary}', which this bundle does not \
                         declare. Every row that carries one of its codes would be of unknowable \
                         colour, so this node does not open"
                    ))
                })?;
                minter
                    .seed_value(key, *code)
                    .map_err(|e| EngineError::Malformed(e.to_string()))?;
            }
        }

        let high_water = seed.high_water.max(high_water_from(&records));
        // **The row-less mark takes the minimum where the point mark takes the maximum**, because
        // the two regions grow towards each other and "furthest along" is downward here. Both homes
        // are consulted for the same reason: rotation reclaims the WAL records `low_water_from`
        // derives from, and a manifest is only as current as its last publication.
        let low_water = seed.low_water.min(low_water_from(&records));
        // `try_with_marks`, not `with_marks`: the seeds come from durable state this process did
        // not write in this run, so a corrupt or hand-edited pair that already meets must be
        // refused **here**, before any allocation, rather than surfacing later as an opaque
        // exhaustion error.
        let allocator = Allocator::try_with_marks(high_water, low_water).map_err(|e| {
            EngineError::Malformed(format!(
                "entity-ID allocator seed from durable state (MANIFEST high-water {}, WAL \
                 high-water {}; MANIFEST low-water {}, WAL low-water {}): {e}",
                seed.high_water,
                high_water_from(&records),
                seed.low_water,
                low_water_from(&records),
            ))
        })?;

        // **The manifests' registry is the starting point and replay runs over it**, on the same
        // ordering rule the deny state below follows: every WAL record postdates any state a
        // manifest carries. Seeding afterwards would resurrect a layer that was dropped since the
        // last publication, gate and all.
        let mut registry = LayerRegistry::new();
        registry.seed(seed.layers, seed.tombstones);
        for record in &records {
            registry.apply(record);
        }
        // **The layer-entity cursor cannot be inferred from the records and is not durable.** A
        // `LayerCreate` says which entity a layer took, not which block it came from nor how much
        // of that block was left; resuming from `max(entity) + 1` would be wrong the moment a drop
        // retired the highest-numbered layer. Reseeding costs at most one block per restart, out
        // of 65 536, and a durable cursor would buy back an id space nothing is short of.
        registry.reseed_entity_cursor();

        // **The roster, on the registry's ordering rule and for the same reason**: the manifests
        // are the starting point and every WAL record postdates them, so seeding afterwards would
        // resurrect a view that was dropped since the last publication. The build's declared views
        // are seeded first because their keys are taken — a roster that forgot them would let a
        // create reissue a key a declared view already holds.
        let mut roster = tessera_lifecycle::ViewRoster::new();
        roster.seed_declared(seed.declared_views.iter().cloned());
        roster.seed(seed.created_views, seed.dead_view_incarnations);
        for record in &records {
            roster.apply(record);
        }

        // **The view groups and plain views declared while the service ran, on the registry's
        // ordering rule** (`ingest.md` §1.3): the manifests' runtime lists are the starting point
        // and every record postdates them. A record naming an object the served manifest already
        // carries is applied as nothing, which is what a fold that moved the declaration into
        // `MANIFEST.json` before the log rotated leaves behind; one this build cannot compile is
        // a log written by a binary this one is not, and refuses the open.
        //
        // **Before the roster is seeded is not required and before the manifest merge is**: a
        // create names a group, and `Manifest::with_roster` drops a record whose group the
        // manifest does not declare, so the group has to reach the manifest first
        // (`Engine::open`).
        let mut view_declarations = seed.view_declarations;
        for record in &records {
            match record {
                WalRecord::ViewGroupCreate { declaration } => {
                    match crate::view_declarations::resolve_group(declaration, seed.manifest) {
                        Ok(crate::view_declarations::Resolution::New(group)) => {
                            if !view_declarations.holds_group(&group.name) {
                                view_declarations.push_group(*group);
                            }
                        }
                        Ok(crate::view_declarations::Resolution::Existing) => {}
                        Err(e) => {
                            return Err(EngineError::Malformed(format!(
                                "the WAL declares view group '{}', which this bundle refuses \
                                 ({e}); this node does not open",
                                declaration.name
                            )));
                        }
                    }
                }
                WalRecord::PlainViewCreate { declaration } => {
                    match crate::view_declarations::resolve_plain(declaration, seed.manifest) {
                        Ok(crate::view_declarations::Resolution::New(view)) => {
                            if !view_declarations.holds_plain(&view.id) {
                                view_declarations.push_plain(*view);
                            }
                        }
                        Ok(crate::view_declarations::Resolution::Existing) => {}
                        Err(e) => {
                            return Err(EngineError::Malformed(format!(
                                "the WAL declares view '{}', which this bundle refuses ({e}); \
                                 this node does not open",
                                declaration.name
                            )));
                        }
                    }
                }
                _ => {}
            }
        }

        // **The runtime attribute columns, on the same ordering rule** (`ingest.md` §6.3): the
        // manifests' lists are the starting point and every record postdates them. A record
        // naming a column the served schema already holds identically is applied as nothing,
        // which is what a fold that moved the column into `MANIFEST.json` before the log rotated
        // leaves behind; one holding a different identity under a held name is a log that
        // disagrees with the manifests about what every row stores, and refuses the open.
        let mut attributes = seed.attributes;
        let mut served = seed.manifest.clone();
        for record in &records {
            let WalRecord::AttributeDeclare { declaration } = record else {
                continue;
            };
            let Some(compiled) = crate::attributes::compile_record(declaration) else {
                return Err(EngineError::Malformed(format!(
                    "the WAL declares attribute '{}' with type '{}', which this build cannot \
                     store; this node does not open",
                    declaration.name, declaration.ty
                )));
            };
            match crate::attributes::held_by_name(&served, &declaration.name) {
                Some(held) if held == compiled => continue,
                Some(_) => {
                    return Err(EngineError::Malformed(format!(
                        "the WAL declares attribute '{}' with an identity the manifests do not \
                         carry for that name; every row stored under it is of unknowable shape, \
                         so this node does not open",
                        declaration.name
                    )));
                }
                None => {}
            }
            match &compiled {
                crate::attributes::CompiledAttribute::Entity(d) => {
                    served = served.with_attributes(std::slice::from_ref(d), &[]);
                }
                crate::attributes::CompiledAttribute::Scoped(f) => {
                    served = served.with_attributes(&[], std::slice::from_ref(f));
                }
            }
            attributes.push(compiled);
        }

        // **The manifests' membership extents are the starting point, and replay unions what came
        // after** — the registry's ordering rule above, for the same reason: every WAL record
        // postdates any state a manifest carries, so seeding afterwards would overwrite a later
        // publication with an earlier one.
        //
        // An extent is addressed by absolute ordinal and an artifact's entity comes from its
        // layer's reserved runs, which the registry has just finished seeding — so this must run
        // after it and does.
        let mut artifacts = ArtifactStore::new();
        let mut undecodable = 0usize;
        // How many memberships the seed holds on the heap because the mapping would not take.
        // Unreachable but for parser drift or damage — see the record arm below.
        let mut on_heap = 0usize;
        for extent in seed.membership_extents {
            let path = seed.prefix_dir.join(&extent.path);
            // **The pack is held for as long as the memberships read through it.** One `Arc` per
            // extent is cloned into each `Members`, which is what makes the view below valid for
            // the store's whole life.
            let pack = Arc::new(
                tessera_store::membership::MembershipPack::open(&path)
                    .map_err(|e| EngineError::Malformed(e.to_string()))?,
            );
            let owner: Arc<dyn std::any::Any + Send + Sync> = pack.clone();
            // The manifest and the file must agree about which artifacts this range names. A
            // disagreement would serve one cluster's members under another's identity, so it
            // refuses rather than trusting either.
            if pack.ordinal_lo() != extent.ordinal_lo || pack.count() != extent.count {
                return Err(EngineError::Malformed(format!(
                    "membership extent {} covers [{}, +{}) but the manifest names [{}, +{})",
                    extent.path,
                    pack.ordinal_lo(),
                    pack.count(),
                    extent.ordinal_lo,
                    extent.count
                )));
            }
            let Some(runs) = registry
                .get(&extent.layer)
                .and_then(|layer| layer.runs.get(extent.level as usize))
            else {
                // A dropped layer's extents outlive it until the next fold rewrites the prefix.
                // Skipping them is correct — the layer is gone — and silent, because a tombstoned
                // name is not a fault.
                continue;
            };
            for (ordinal, blob) in pack.iter() {
                // **An empty blob is a hole and not a fault** — the ordinal exists and holds no
                // artifact, which is what a fold leaves behind where Rule F's arm retired one. It
                // is written rather than packed around because an ordinal is identity; reading it
                // as undecodable would alarm on every level a deletion has ever touched.
                if blob.is_empty() {
                    continue;
                }
                let Some(entity) = runs.entity_of(ordinal as u64).map(EntityId::new) else {
                    undecodable += 1;
                    continue;
                };
                match tessera_lifecycle::membership::decode_record(entity, blob) {
                    Some((mut record, shape)) => {
                        // **The membership is read through the pack rather than copied out of
                        // it**, which is the route the build's own publication takes over the
                        // extent it has just written (`tessera_lifecycle::Members`). The bitmap
                        // `decode_record` built is dropped here. Keeping it costs a serving node
                        // one Roaring bitmap per artifact over the whole corpus for as long as it
                        // runs: 31 GB of anonymous memory at open over the 1.6×10⁶ artifacts and
                        // 3.4×10⁹ member entries of the GBIF corpus, for bytes already mapped.
                        //
                        // SAFETY: `blob` is a slice of `pack`'s read-only mapping, `owner` is that
                        // same pack, and the `Members` this produces holds `owner` for as long as
                        // it holds the view. **No extent file is ever written twice**, which is
                        // what keeps a mapped file from being truncated under a reader: an extent
                        // is named `members-{n:06}-{index:03}` from the publication counter
                        // (`Executor::allocate_manifest_n`), which only rises within an executor
                        // and is seeded above every candidate any partition carries when that
                        // executor is built (`Engine`'s executor seed). One executor owns a bundle
                        // root, so that is the whole set of writers. The writer itself is
                        // `tessera_store::write_and_fsync`, whose `File::create` would truncate a
                        // name it was handed twice.
                        let mapped = unsafe {
                            tessera_lifecycle::membership::mapped_members(blob, owner.clone())
                        };
                        // **The same cardinality check `ArtifactStore::rehouse_members` makes.**
                        // Both readers walk the same blob, so a disagreement means `members_bytes`
                        // and `decode_record` have drifted apart or the bytes are damaged, and
                        // what a short membership produces is a low masked count for every viewer
                        // — which the existence criterion renders as absent with nothing to
                        // notice. Where the check does not hold, the record keeps the bitmap it
                        // decoded.
                        match mapped {
                            Some(members)
                                if members.cardinality() == record.members.cardinality() =>
                            {
                                record.members = members;
                            }
                            _ => on_heap += 1,
                        }
                        artifacts.seed(&extent.layer, extent.level, ordinal, record, shape)
                    }
                    None => undecodable += 1,
                }
            }
            // **How far the level reaches comes from the extent, not from the records in it.** A
            // hole in the middle is implied by the ordinals either side of it; a hole at the *top*
            // is implied by nothing, so a level seeded from records alone comes back short and the
            // next publication is handed the ordinal — and the entity derived from it — that the
            // artifact this fold deleted was published under.
            artifacts.seed_extent_bound(
                &extent.layer,
                extent.level,
                extent.ordinal_lo.saturating_add(extent.count),
            );
        }
        // **The published version, before replay puts anything over it.** Every level the manifest
        // names gets the counter the publication recorded, replacing whatever the seeding above
        // bumped it to; the replay below then moves it for every record the log carries past that
        // publication, which is exactly the signal a reader deciding whether to adopt a derived
        // structure needs (`ArtifactStore::seed_level_version`).
        for version in seed.level_versions {
            artifacts.seed_level_version(&version.layer, version.level, version.version);
        }
        for (record, position) in records.iter().zip(wal.replayed_positions()) {
            undecodable += artifacts.apply(record, *position);
        }
        if on_heap > 0 {
            tracing::warn!(
                count = on_heap,
                "ALARM: the membership located inside the blob disagreed with the one decoded from \
                 it, so those artifacts are served from the decoded bitmap on the heap; the two \
                 readers walk the same bytes, so this is parser drift or damage in the extent"
            );
        }
        if undecodable > 0 {
            tracing::error!(
                count = undecodable,
                "ALARM: artifact memberships in the durable prefix did not decode; those artifacts \
                 are absent rather than empty, which a viewer cannot tell from a criterion they \
                 failed to clear"
            );
        }

        // Taken off the manifest seed before it is shadowed by the overlay seed below.
        let view_ids_of_key = seed.view_ids_of_key;
        // **The manifests' deny state is the starting point, and replay runs over it.** Ordering,
        // not aesthetics — see `replay`'s own doc: every WAL record postdates any state an
        // honourable manifest carries, and the one op that needs the later record to win is
        // `Unsuppress`. Seeding afterwards silently reverts an acked unsuppress on any restart in
        // the publication gap.
        let mut seed = Overlay::new();
        for (entity, op) in initial_deny {
            seed.apply(*entity, *op);
        }

        let (overlay, mut buffer, established, resolver) =
            replay(&records, dict, seed, view_ids_of_key);
        // **Re-hashed at the boundary, once, at startup.** `replay` builds this with `FxHashMap`;
        // the live index deliberately does not — see `WritePath::established`'s doc. Converting
        // here costs one pass over the replayed set at open and keeps the hasher choice in one
        // place rather than propagating it into `tessera-lifecycle`.
        let established: std::collections::HashMap<Vec<u8>, EntityId> =
            established.into_iter().collect();

        // **The buffer holds exactly the rows that have no geometry, and this is where that becomes
        // true.** Replay walks every retained WAL record, including the `IngestBatch` rows of every
        // flush whose member has not yet been reclaimed — so without this the buffer comes back
        // holding rows that already have segments, and the next flush writes each of them a second
        // time under a second entity's worth of geometry.
        //
        // **The test is `row_of`, not a watermark.** A watermark is a cheap scalar proxy for "has a
        // row", exact only while entity-allocation order and flush order coincide — that is, while
        // there is one view per partition, which write-path §4.3 records as load-bearing and unenforced.
        // The predicate below is what the watermark approximates, so it stays exact at any number of
        // views and needs no per-view bookkeeping anywhere.
        //
        // It is also what `compose::verdict` now relies on. That function used to gate rule 4 on
        // `entity < watermark` to stop a stale buffer answering for an entity the fragment already
        // covers; the gate is gone, and this invariant is what replaces it.
        // **Per (entity, view), because an entity may hold a row in several views** (`views.md`
        // §4). The question is not "does this entity have geometry" — a joined entity has some,
        // in the view it was first ingested into — but "does this *row* have geometry", and a
        // predicate over the entity alone would drop a second view's pending row from the buffer
        // while no segment held it. Every row is in one view, so the two questions coincide
        // exactly while a corpus has one view, which is why the narrower one costs nothing.
        let already_flushed: Vec<(EntityId, String)> = buffer
            .rows()
            .map(|(entity, item)| (*entity, item.view.clone()))
            .filter(|(entity, view)| has_row(*entity, view))
            .collect();
        if !already_flushed.is_empty() {
            tracing::debug!(
                count = already_flushed.len(),
                "WAL rows that already have geometry were not re-buffered"
            );
        }
        for (entity, view) in already_flushed {
            buffer.remove_in_view(entity, &view);
        }

        // **Where each surviving row sits in the log**, so a rotation knows what it may reclaim
        // below (write-path §4.5). Stamped after the filter rather than before it, because a row that
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
                    buffer.set_wal_pos(row.entity_id, &row.view, *position);
                }
            }
        }

        // **The values batches, into the buffer's fill map** (`ingest.md` §1.4). Replayed here
        // rather than in `tessera-lifecycle`'s pass because a values record names its columns and
        // resolving a name to a position needs the served schema — the build's declarations plus
        // every runtime one above, which is what `served` now is.
        //
        // **A batch whose cells a flush already wrote is re-buffered and writes nothing.** The
        // WAL member holding it is reclaimed on its own schedule, so a replay meets records the
        // flush has consumed; `plan_flush` applies the fill rule to every fill against the
        // flushed homes, drops the cells already held, and names the fill consumed anyway — so
        // the next tick removes it and it stops pinning the log.
        for (record, position) in records.iter().zip(wal.replayed_positions()) {
            let WalRecord::ValuesBatch {
                view,
                columns,
                rows,
                ..
            } = record
            else {
                continue;
            };
            // A record with no view names no flush pass to write its cells; no writer produces
            // one, and reading it as some view's would put a group-scoped cell in the wrong
            // column.
            let Some(view) = view else {
                return Err(EngineError::Malformed(
                    "the WAL carries a values batch naming no view, which no writer produces; \
                     this node does not open"
                        .to_string(),
                ));
            };
            let families = scoped_families_of_view(&served, view);
            let owner_view = scoped_owner_view_of(&served, view);
            for row in rows {
                let mut scalars: Vec<WalScalar> =
                    vec![WalScalar::Null; served.declared_scalars.len()];
                let mut scoped: Vec<WalScalar> = vec![WalScalar::Null; families.len()];
                let mut any_entity = false;
                let mut any_scoped = false;
                for (at, name) in columns.iter().enumerate() {
                    let Some(value) = row.values.get(at) else {
                        continue;
                    };
                    if matches!(value, WalScalar::Null) {
                        continue;
                    }
                    if let Some(position) =
                        served.declared_scalars.iter().position(|d| &d.name == name)
                    {
                        scalars[position] = value.clone();
                        any_entity = true;
                        continue;
                    }
                    if let Some(position) = families.iter().position(|f| &f.name == name) {
                        scoped[position] = value.clone();
                        any_scoped = true;
                        continue;
                    }
                    // A column the served schema no longer carries. The values are unreadable
                    // rather than wrong — nothing can say which column they belong to — so the
                    // open is refused rather than the cells dropped.
                    return Err(EngineError::Malformed(format!(
                        "the WAL carries a values batch naming column '{name}', which this \
                         deployment's schema does not declare; this node does not open"
                    )));
                }
                if any_entity {
                    buffer.fill(
                        row.entity_id,
                        tessera_lifecycle::Fill {
                            view: view.clone(),
                            scalars,
                            wal_pos: Some(*position),
                        },
                        |value| matches!(value, WalScalar::Null),
                    );
                    buffer.set_fill_wal_pos(row.entity_id, *position);
                }
                if any_scoped {
                    buffer.fill_scoped(
                        row.entity_id,
                        owner_view.clone(),
                        tessera_lifecycle::ScopedFill {
                            view: view.clone(),
                            scoped,
                            wal_pos: Some(*position),
                        },
                        |value| matches!(value, WalScalar::Null),
                    );
                    buffer.set_scoped_fill_wal_pos(row.entity_id, &owner_view, *position);
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
                registry,
                artifacts,
                roster,
                attributes,
                vocabularies: runtime_vocabularies,
                view_declarations,
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
                registry: Mutex::new(state.registry),
                artifacts: Mutex::new(state.artifacts),
                roster: Mutex::new(state.roster),
                attributes: Mutex::new(state.attributes),
                vocabularies: Mutex::new(state.vocabularies),
                view_declarations: Mutex::new(state.view_declarations),
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
        flush: MaintenanceDeps,
        #[cfg(feature = "fault-injection")] faults: Option<
            Arc<tessera_lifecycle::faults::FaultSwitchboard>,
        >,
    ) -> Result<(), ExecutorStartError> {
        let wal = self.wal.take().ok_or(ExecutorStartError::AlreadyStarted)?;
        let wal_position_at_start = wal.position();

        // **Compaction §7's startup sweep, before anything else and before the thread** — see
        // `sweep_orphan_prefixes` for why both halves of that matter. `AlreadyStarted` is checked
        // first, so a second `start_executor` on the same path cannot sweep a second time.
        sweep_orphan_prefixes(&flush.bundle_root, &generation.load().prefix);

        let (work_tx, work_rx) = std::sync::mpsc::sync_channel(queue_bound);
        let (deny_tx, deny_rx) = std::sync::mpsc::channel();
        // Capacity one, and `try_send` that discards `Full`: a token means "something may be
        // waiting", and a second token while one is pending says nothing new. The executor only
        // ever blocks on this having **observed both queues empty**, which is what makes discarding
        // safe — see [`Executor::run`].
        let (bell_tx, bell_rx) = std::sync::mpsc::sync_channel(1);
        // Completed flushes have their own, unbounded channel — see `Executor::flush_done`. A
        // coalesce gets its own for the same reasons: it may not be shed, and it must not queue
        // behind the deny lane or a commit window.
        let (flush_tx, flush_rx) = std::sync::mpsc::channel();
        let (coalesce_tx, coalesce_rx) = std::sync::mpsc::channel();
        let (merge_tx, merge_rx) = std::sync::mpsc::channel();
        let (fold_tx, fold_rx) = std::sync::mpsc::channel();
        let (suggest_tx, suggest_rx) = std::sync::mpsc::channel();

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
        // Read before the pointer moves into the thread. What the bundle's manifests already carry
        // is this list's starting point — see the field for why it is held rather than re-cloned
        // from a (stale) live manifest at each publication.
        let seeded_membership_extents: Vec<tessera_store::manifest::MembershipExtent> = generation
            .load()
            .bundle
            .partitions
            .values()
            .flat_map(|p| p.manifest.membership_extents.iter().cloned())
            .collect();
        // The containment partitions the last fold wrote, seeded identically: a publication clones
        // a stale manifest, so the list has to be held here rather than re-read from it.
        let seeded_containment_extents: Vec<tessera_store::manifest::ContainmentExtent> =
            generation
                .load()
                .bundle
                .partitions
                .values()
                .flat_map(|p| p.manifest.containment_extents.iter().cloned())
                .collect();
        // The tile indexes the last fold wrote, seeded identically and for the identical reason.
        let seeded_tile_index_extents: Vec<tessera_store::manifest::TileIndexExtent> = generation
            .load()
            .bundle
            .partitions
            .values()
            .flat_map(|p| p.manifest.tile_index_extents.iter().cloned())
            .collect();
        // The row-major columns the last fold wrote, seeded identically and for the identical
        // reason.
        let seeded_row_column_extents: Vec<tessera_store::manifest::RowColumnExtent> = generation
            .load()
            .bundle
            .partitions
            .values()
            .flat_map(|p| p.manifest.row_column_extents.iter().cloned())
            .collect();
        // The shape row forms the build or the last fold wrote, seeded identically and for the
        // identical reason.
        let seeded_shape_rows_extents: Vec<tessera_store::manifest::ShapeRowsExtent> = generation
            .load()
            .bundle
            .partitions
            .values()
            .flat_map(|p| p.manifest.shape_rows_extents.iter().cloned())
            .collect();
        let seeded_shape_held_extents: Vec<tessera_store::manifest::ShapeHeldExtent> = generation
            .load()
            .bundle
            .partitions
            .values()
            .flat_map(|p| p.manifest.shape_held_extents.iter().cloned())
            .collect();
        // The content half, seeded identically and for the identical reason.
        let seeded_content_extents: Vec<tessera_store::manifest::RecordExtent> = generation
            .load()
            .bundle
            .partitions
            .values()
            .flat_map(|p| p.manifest.artifact_record_extents.iter().cloned())
            .collect();
        #[cfg(feature = "fault-injection")]
        let thread_faults = faults.clone();
        // The tick clock a snapshot reads, seeded so `next_tick_in_nanos` is one period until
        // the executor's first tick rather than zero.
        health.set_flush_period_secs(flush.max_age_secs);
        health.mark_tick(std::time::Instant::now());

        let join = std::thread::Builder::new()
            .name("tessera-lifecycle".to_string())
            .spawn(move || {
                let mut executor = Executor {
                    wal: exec_wal,
                    live,
                    generation,
                    row_projection_cache,
                    region_cache: flush.region_cache,
                    artifact_projections: flush.artifact_projections,
                    shapes: flush.shapes,
                    lineages: flush.lineages,
                    level_contents: flush.level_contents,
                    queues: LifecycleQueues {
                        work: work_rx,
                        deny: deny_rx,
                        bell: bell_rx,
                    },
                    health: Arc::clone(&health),
                    window_seq: 0,
                    flush_max_age_secs: flush.max_age_secs,
                    flush_max_items: flush.max_items,
                    flush_attempt: 0,
                    // Above every candidate present at open, per partition — see the field's doc.
                    next_manifest_n: flush.next_manifest_n,
                    deny_dirty: false,
                    windows_since_publication: 0,
                    bundle_root: flush.bundle_root,
                    identity_key: flush.identity_key,
                    pool: flush.pool,
                    max_distinct_terms: flush.max_distinct_terms,
                    flush_done: flush_rx,
                    flush_submit: flush_tx,
                    coalesce_policy: flush.coalesce,
                    coalesce_in_flight: Arc::new(AtomicBool::new(false)),
                    coalesce_attempt: 0,
                    coalesce_done: coalesce_rx,
                    coalesce_submit: coalesce_tx,
                    refresh: flush.refresh,
                    merge_policy: flush.merge,
                    coalesce_enabled: flush.coalesce_enabled,
                    merge_enabled: flush.merge_enabled,
                    merge_in_flight: Arc::new(AtomicBool::new(false)),
                    merge_attempt: 0,
                    merge_done: merge_rx,
                    merge_submit: merge_tx,
                    fold_in_flight: Arc::new(AtomicBool::new(false)),
                    fold_attempt: 0,
                    fold_done: fold_rx,
                    fold_submit: fold_tx,
                    suggest_dir: flush.suggest_dir,
                    suggest_in_flight: Arc::new(AtomicBool::new(false)),
                    suggest_build: 0,
                    suggest_done: suggest_rx,
                    suggest_submit: suggest_tx,
                    configured_merge_bytes: flush.configured_merge_bytes,
                    fold_paused: flush.fold_paused,
                    fold_publication_paused: flush.fold_publication_paused,
                    merge_publication_paused: flush.merge_publication_paused,
                    compaction: flush.compaction,
                    last_fold_start_unix: None,
                    superseded_sidecars: Vec::new(),
                    membership_extents: seeded_membership_extents,
                    containment_extents: seeded_containment_extents,
                    tile_index_extents: seeded_tile_index_extents,
                    row_column_extents: seeded_row_column_extents,
                    shape_rows_extents: seeded_shape_rows_extents,
                    shape_held_extents: seeded_shape_held_extents,
                    artifact_record_extents: seeded_content_extents,
                    pending_reclaim: Vec::new(),
                    last_tick: std::time::Instant::now(),
                    pending_forms: std::collections::BTreeMap::new(),
                    // Seeded from the opened WAL's position so a freshly started node does not
                    // rotate until something is appended in this run.
                    wal_position_at_last_rotation: wal_position_at_start,
                    last_wal_sample: None,
                    wal_samples: 0,
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

    /// Ring the executor's doorbell. No executor yet started is a no-op: the flag a caller set is
    /// read at the executor's first loop iteration anyway.
    pub(crate) fn wake(&self) {
        if let Ok(handle) = self.handle() {
            handle.wake();
        }
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
        publication: GeometryPublication,
    ) -> std::result::Result<(), PublishGeometryError> {
        self.handle
            .as_ref()
            .ok_or(PublishGeometryError::NoExecutor)?
            .publish_geometry(publication)
    }

    /// Submit a suggestion-index drop to the executor and block until it has published.
    ///
    /// `false` where there is no executor to publish through — the hook's callers all start one,
    /// and a test that did not would otherwise assert against an unchanged generation.
    #[cfg(feature = "fault-injection")]
    pub(crate) fn forget_suggestion_index(&self, vocabulary: String) -> bool {
        self.handle
            .as_ref()
            .is_some_and(|handle| handle.forget_suggestion_index(vocabulary))
    }

    #[cfg(feature = "fault-injection")]
    pub(crate) fn rebuild_suggestion_index(&self, vocabulary: String) -> bool {
        self.handle
            .as_ref()
            .is_some_and(|handle| handle.rebuild_suggestion_index(vocabulary))
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
    ///
    /// Returns the assigned ids and **how many artifacts this batch's membership column created**
    /// (`Ack::Ingested::minted`).
    pub(crate) fn accept_ingest(
        &self,
        rows: Vec<UnallocatedRow>,
        batch_id: String,
        body_hash: [u8; 32],
        artifacts: tessera_lifecycle::BatchArtifacts,
    ) -> Result<(Vec<EntityId>, u64), AcceptError> {
        let mark = StageMark::now();
        let receipt = self.handle()?.submit(Command::Ingest {
            rows,
            batch_id,
            body_hash,
            artifacts,
        })?;
        self.health().lap(WriteStage::SubmitToReceipt, mark);
        match receipt.outcome {
            Ok(Ack::Ingested { entity_ids, minted }) => Ok((entity_ids, minted)),
            Ok(other) => {
                unreachable!("an Ingest command answers with Ack::Ingested, not {other:?}")
            }
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
    /// Register an annotation layer and wait for its receipt.
    ///
    /// Returns the layer's own entity, which the caller turns into a `tessera_id` — the only
    /// address by which the layer can later be suppressed, since an entity id never crosses the
    /// boundary (**I10**).
    ///
    /// **A failure means the layer does not exist**, which is the opposite of a deny's posture and
    /// deliberately so: see `Executor::commit_registry`.
    pub(crate) fn register_layer(
        &self,
        declaration: tessera_types::layer::LayerDeclaration,
    ) -> Result<EntityId, AcceptError> {
        let receipt = self.handle()?.submit(Command::RegisterLayer {
            declaration: Box::new(declaration),
        })?;
        match receipt.outcome {
            Ok(Ack::LayerRegistered { entity }) => Ok(entity),
            Ok(other) => {
                unreachable!("a RegisterLayer command answers LayerRegistered, not {other:?}")
            }
            Err(e) => Err(AcceptError::Exec(e)),
        }
    }

    /// Drop a layer, tombstoning its name for ever.
    pub(crate) fn drop_layer(&self, name: String) -> Result<(), AcceptError> {
        let receipt = self.handle()?.submit(Command::DropLayer { name })?;
        match receipt.outcome {
            Ok(Ack::LayerDropped) => Ok(()),
            Ok(other) => unreachable!("a DropLayer command answers LayerDropped, not {other:?}"),
            Err(e) => Err(AcceptError::Exec(e)),
        }
    }

    /// Create a view of a view group while the service runs (`views.md` §3.2).
    pub(crate) fn create_view(
        &self,
        group: String,
        key: String,
        visibility: Option<Vec<String>>,
        metadata: std::collections::BTreeMap<String, tessera_types::view::ViewMetadataValue>,
    ) -> Result<(), AcceptError> {
        let receipt = self.handle()?.submit(Command::CreateView {
            group,
            key,
            visibility,
            metadata,
        })?;
        match receipt.outcome {
            Ok(Ack::ViewCreated) => Ok(()),
            Ok(other) => unreachable!("a CreateView command answers ViewCreated, not {other:?}"),
            Err(e) => Err(AcceptError::Exec(e)),
        }
    }

    /// Declare an attribute column while the service runs (`ingest.md` §1.3, §6.3). Answers
    /// whether the name already carried this identity, in which case nothing was appended.
    pub(crate) fn declare_attribute(
        &self,
        request: tessera_lifecycle::AttributeRequest,
    ) -> Result<bool, AcceptError> {
        let receipt = self.handle()?.submit(Command::DeclareAttribute {
            request: Box::new(request),
        })?;
        match receipt.outcome {
            Ok(Ack::AttributeDeclared { existing }) => Ok(existing),
            Ok(other) => {
                unreachable!("a DeclareAttribute command answers AttributeDeclared, not {other:?}")
            }
            Err(e) => Err(AcceptError::Exec(e)),
        }
    }

    /// Fill attribute values on entities that already exist (`POST /control/values`,
    /// `ingest.md` §1.4), and answer what the batch did.
    pub(crate) fn fill_values(
        &self,
        request: tessera_lifecycle::ValuesRequest,
    ) -> Result<ValuesReceipt, AcceptError> {
        let receipt = self.handle()?.submit(Command::Values {
            request: Box::new(request),
        })?;
        match receipt.outcome {
            Ok(Ack::ValuesFilled {
                filled,
                held,
                joined,
            }) => Ok(ValuesReceipt {
                filled,
                held,
                joined,
            }),
            Ok(other) => unreachable!("a Values command answers ValuesFilled, not {other:?}"),
            Err(e) => Err(AcceptError::Exec(e)),
        }
    }

    /// Declare a vocabulary. Answers `(existing, added, titles)`: whether a vocabulary of that
    /// name already carried this identity, how many of the request's values were novel, and how
    /// many held values it gave a new title.
    pub(crate) fn declare_vocabulary(
        &self,
        request: tessera_lifecycle::VocabularyRequest,
    ) -> Result<(bool, u64, u64), AcceptError> {
        let receipt = self.handle()?.submit(Command::DeclareVocabulary {
            request: Box::new(request),
        })?;
        match receipt.outcome {
            Ok(Ack::VocabularyDeclared {
                existing,
                added,
                titles,
            }) => Ok((existing, added, titles)),
            Ok(other) => {
                unreachable!(
                    "a DeclareVocabulary command answers VocabularyDeclared, not {other:?}"
                )
            }
            Err(e) => Err(AcceptError::Exec(e)),
        }
    }

    /// A page of values for a vocabulary that exists. Answers `(added, existing, titles)`.
    pub(crate) fn mint_vocabulary_values(
        &self,
        vocabulary: String,
        values: Vec<tessera_lifecycle::DeclaredValue>,
    ) -> Result<(u64, u64, u64), AcceptError> {
        let receipt = self
            .handle()?
            .submit(Command::MintVocabularyValues { vocabulary, values })?;
        match receipt.outcome {
            Ok(Ack::VocabularyValuesMinted {
                added,
                existing,
                titles,
            }) => Ok((added, existing, titles)),
            Ok(other) => unreachable!(
                "a MintVocabularyValues command answers VocabularyValuesMinted, not {other:?}"
            ),
            Err(e) => Err(AcceptError::Exec(e)),
        }
    }

    /// Declare a view group. Answers whether a group of that name already carried this identity.
    pub(crate) fn create_view_group(
        &self,
        declaration: tessera_lifecycle::wal::ViewGroupDeclaration,
    ) -> Result<bool, AcceptError> {
        let receipt = self.handle()?.submit(Command::CreateViewGroup {
            declaration: Box::new(declaration),
        })?;
        match receipt.outcome {
            Ok(Ack::ViewGroupCreated { existing }) => Ok(existing),
            Ok(other) => {
                unreachable!("a CreateViewGroup command answers ViewGroupCreated, not {other:?}")
            }
            Err(e) => Err(AcceptError::Exec(e)),
        }
    }

    /// Create a plain view. Answers whether a view of that name already carried this identity.
    pub(crate) fn create_plain_view(
        &self,
        declaration: tessera_lifecycle::wal::PlainViewDeclaration,
    ) -> Result<bool, AcceptError> {
        let receipt = self.handle()?.submit(Command::CreatePlainView {
            declaration: Box::new(declaration),
        })?;
        match receipt.outcome {
            Ok(Ack::PlainViewCreated { existing }) => Ok(existing),
            Ok(other) => {
                unreachable!("a CreatePlainView command answers PlainViewCreated, not {other:?}")
            }
            Err(e) => Err(AcceptError::Exec(e)),
        }
    }

    /// Drop a view — freeing its key and killing its incarnation (decision 0115) — and answer how
    /// many entities `delete_dangling` submitted for deletion (`views.md` §3.4).
    pub(crate) fn drop_view(
        &self,
        group: String,
        key: String,
        delete_dangling: bool,
    ) -> Result<u64, AcceptError> {
        let receipt = self.handle()?.submit(Command::DropView {
            group,
            key,
            delete_dangling,
        })?;
        match receipt.outcome {
            Ok(Ack::ViewDropped { deleted }) => Ok(deleted),
            Ok(other) => unreachable!("a DropView command answers ViewDropped, not {other:?}"),
            Err(e) => Err(AcceptError::Exec(e)),
        }
    }

    /// Which layers a principal may know exist, resolved once per session.
    pub(crate) fn resolve_layers(
        &self,
        is_satisfied: impl Fn(tessera_types::TermId) -> bool,
        resolve_label: impl Fn(&str) -> Option<tessera_types::TermId>,
    ) -> tessera_lifecycle::ResolvedLayers {
        self.live.resolve_layers(is_satisfied, resolve_label)
    }

    pub(crate) fn registered_layer(
        &self,
        name: &str,
    ) -> Option<tessera_types::layer::RegisteredLayer> {
        self.live.registered_layer(name)
    }

    /// See `WriteState::registered_layers`.
    pub(crate) fn registered_layers(&self) -> Vec<tessera_types::layer::RegisteredLayer> {
        self.live.registered_layers()
    }

    /// Publish a batch of artifacts, returning their entities in the caller's submitted order and
    /// the batch's counts (`Ack::ArtifactsPublished`).
    pub(crate) fn publish_artifacts(
        &self,
        layer: String,
        level: u32,
        artifacts: Vec<IncomingArtifact>,
    ) -> Result<PublishedBatch, AcceptError> {
        let receipt = self.handle()?.submit(Command::PublishArtifacts {
            layer,
            level,
            artifacts,
        })?;
        match receipt.outcome {
            Ok(Ack::ArtifactsPublished {
                entities,
                created,
                without_content,
                filled,
                joined,
            }) => Ok(PublishedBatch {
                entities,
                created,
                without_content,
                filled,
                joined,
            }),
            Ok(other) => {
                unreachable!("a PublishArtifacts command answers ArtifactsPublished, not {other:?}")
            }
            Err(e) => Err(AcceptError::Exec(e)),
        }
    }

    /// Grow the memberships of artifacts that already exist, answering one receipt per join in
    /// the caller's order. See `Executor::commit_growth`.
    pub(crate) fn grow_memberships(
        &self,
        layer: String,
        level: u32,
        joins: Vec<tessera_lifecycle::IncomingGrowth>,
    ) -> Result<Vec<tessera_lifecycle::MembershipGrown>, AcceptError> {
        let receipt = self.handle()?.submit(Command::GrowMemberships {
            layer,
            level,
            joins,
        })?;
        match receipt.outcome {
            Ok(Ack::MembershipsGrown { grown }) => Ok(grown),
            Ok(other) => {
                unreachable!("a GrowMemberships command answers MembershipsGrown, not {other:?}")
            }
            Err(e) => Err(AcceptError::Exec(e)),
        }
    }

    pub(crate) fn with_artifacts<R>(&self, f: impl FnOnce(&ArtifactStore) -> R) -> R {
        self.live.with_artifacts(f)
    }

    pub(crate) fn locate_artifact(&self, entity: EntityId) -> Option<(String, u32, u32)> {
        self.live.locate_artifact(entity)
    }

    pub(crate) fn allocator_low_water(&self) -> u64 {
        self.live.allocator_low_water()
    }

    pub(crate) fn accept_change(&self, entity: EntityId, op: ChangeOp) -> Result<(), AcceptError> {
        self.submit_change(entity, op)?.wait()
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
        entity: EntityId,
        op: ChangeOp,
    ) -> Result<PendingChange, AcceptError> {
        let pending = self.handle()?.enqueue(Command::Change { entity, op })?;
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
        publication: GeometryPublication,
        respond: SyncSender<std::result::Result<(), GeometryRefused>>,
    },
    /// Drop one vocabulary's suggestion index and publish — `Engine::forget_suggestion_index_for_test`.
    ///
    /// **A test hook that is nonetheless a publication**, so it comes through this queue like every
    /// other. It swapped the generation directly at first, which is the second publisher
    /// `check-layers.sh` forbids (lifecycle §1.3, #59): the executor thread reads the live
    /// generation, builds a successor and stores it, so a store from anywhere else can be
    /// overwritten by a swap already in flight — and a test that lost its swap would pass or fail
    /// on timing rather than on the behaviour under test.
    #[cfg(feature = "fault-injection")]
    ForgetSuggestionIndex {
        vocabulary: String,
        respond: SyncSender<()>,
    },
    /// Rebuild one vocabulary's suggestion index from the live minter and publish it —
    /// `Engine::rebuild_suggestion_index_for_test`.
    ///
    /// **A test hook for a cadence a test cannot otherwise reach.** A rebuild is dispatched when a
    /// vocabulary's side map has run `SUGGEST_REBUILD_SIDE_VALUES` (4,096) values ahead of its
    /// base, which is hundreds of ingest batches — far past what a fixture builds — and it is the
    /// one publication that deliberately moves neither `segments_version` nor `overlay_version`
    /// (`Executor::publish_completed_suggests`). So it is exactly the state a per-session set's key
    /// cannot see, and the only way to put a test in it is to ask for the rebuild directly. It
    /// comes through this queue for [`ExecutorWork::ForgetSuggestionIndex`]'s reason: it is a
    /// publication, and the executor thread is the sole publisher.
    #[cfg(feature = "fault-injection")]
    RebuildSuggestionIndex {
        vocabulary: String,
        respond: SyncSender<()>,
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
    /// [`crate::Engine::publish_rotated_prefix_for_test`] was offered a prefix `CURRENT` does not name.
    ///
    /// **Refused rather than published**, because `CURRENT` is the commit point and the bundle
    /// identity *is* the digest it names (contracts §2.1). Publishing an uncommitted prefix would
    /// leave the process serving geometry a restart could not find, and nothing would detect the
    /// disagreement until that restart.
    PrefixNotCommitted { offered: String, current: String },
    /// [`crate::Engine::publish_rotated_prefix_for_test`] could not open the prefix it was handed, or one of
    /// the artefacts inside it. Its files stand as orphans under a prefix nothing serves, and
    /// nothing was swapped.
    PrefixNotOpenable(String),
}

impl std::fmt::Display for PublishGeometryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PublishGeometryError::Refused(refused) => write!(f, "{refused}"),
            PublishGeometryError::NoExecutor => f.write_str(
                "this engine has no write executor, and a geometry publication is a swap on that \
                 thread (lifecycle §1.3)",
            ),
            PublishGeometryError::PrefixNotCommitted { offered, current } => write!(
                f,
                "refusing to publish prefix '{offered}': CURRENT names '{current}', so the \
                 publication is not committed and a restart would not find it"
            ),
            PublishGeometryError::PrefixNotOpenable(detail) => {
                write!(f, "the written prefix would not open: {detail}")
            }
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
        publication: GeometryPublication,
    ) -> std::result::Result<(), PublishGeometryError> {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.work
            .send(ExecutorWork::PublishGeometry {
                publication,
                respond: tx,
            })
            .map_err(|_| PublishGeometryError::NoExecutor)?;
        let _ = self.bell.try_send(());
        rx.recv()
            .map_err(|_| PublishGeometryError::NoExecutor)?
            .map_err(PublishGeometryError::Refused)
    }

    /// Submit a suggestion-index drop and block until the executor has published it.
    ///
    /// A blocking `send` on the work lane, exactly as [`Self::publish_geometry`] is and for the
    /// same reason: it is a publication, not a client request, and shedding it would leave the
    /// caller's next assertion racing a swap that never happened.
    #[cfg(feature = "fault-injection")]
    pub(crate) fn forget_suggestion_index(&self, vocabulary: String) -> bool {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        if self
            .work
            .send(ExecutorWork::ForgetSuggestionIndex {
                vocabulary,
                respond: tx,
            })
            .is_err()
        {
            return false;
        }
        let _ = self.bell.try_send(());
        rx.recv().is_ok()
    }

    /// Submit a suggestion-index rebuild and block until the executor has published it — the same
    /// shape as [`Self::forget_suggestion_index`] and for the same reason.
    #[cfg(feature = "fault-injection")]
    pub(crate) fn rebuild_suggestion_index(&self, vocabulary: String) -> bool {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        if self
            .work
            .send(ExecutorWork::RebuildSuggestionIndex {
                vocabulary,
                respond: tx,
            })
            .is_err()
        {
            return false;
        }
        let _ = self.bell.try_send(());
        rx.recv().is_ok()
    }

    /// Ring the executor's doorbell without submitting anything.
    ///
    /// A spurious token is harmless (capacity one; `Full` means a wake-up is already pending); a
    /// missing one costs nothing but latency, because `Executor::wait_for_work` times out at the
    /// tick regardless. The one caller is `Engine::request_flush`: the flag it sets is consumed by
    /// `tick_if_due`, and without this ring an idle executor would not look at it until the next
    /// timeout — turning "executes promptly" back into "executes within one tick".
    pub(crate) fn wake(&self) {
        let _ = self.bell.try_send(());
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

/// How many deny windows may pass before the overlay publishes regardless of whether the drain has
/// closed.
///
/// **A liveness floor, not a latency bound.** The drain loops while the deny lane is non-empty, so
/// under arrival faster than application it need never close, and without a floor the newest
/// manifest would trail live state indefinitely. Latency is not what this bounds — architecture §3
/// (r23) budgets the whole write path at seconds to minutes, denies included, and grants latitude
/// in *when* work is batched under one condition this design keeps: a deny's ack stays coupled to
/// its application, which happens at the window's own fsync and swap, upstream of any publication.
///
/// What the batching *does* buy is bytes. A side-manifest is complete state (contracts §2.3), so
/// publishing per window through a bulk revocation of `N` rewrites a growing set once per window —
/// Θ(N²/window) on disc. Collapsing a burst into one write removes that; this floor bounds the
/// exposure the collapsing admits, at ≤ 64,000 dispositions, all durable in the WAL, all enforced
/// live, and recovered by any WAL-bearing restart. Only a no-WAL restore sees the gap.
const OVERLAY_PUBLICATION_MAX_WINDOWS: u64 = 64;

/// How often the executor re-checks for a completed flush while one is in flight or completed
/// but not yet drained.
///
/// **This is what bounds publication latency on an idle node, and it exists because the pool must
/// not ring the doorbell.** `flush_submit`'s doc records why: an executor holding a clone of its
/// own bell sender would keep the channel alive for ever and `WritePath::drop`'s join would hang —
/// and a pool task holding one re-creates the same hang for the duration of a flush at shutdown.
/// So the wake-up is a poll, armed only while [`ExecutorHealth`]'s `flush_in_flight` /
/// `flush_completed_pending` pair says there is something to wait for: a quiescent executor still
/// sleeps the full tick, and a flush's publication lands within this interval of its files being
/// durable rather than at the next tick — which is what keeps `POST /control/flush` "prompt" on
/// an idle node, and the ack→visibility bound at one tick rather than two.
const FLUSH_COMPLETION_POLL: std::time::Duration = std::time::Duration::from_millis(20);

/// How often the executor looks for a **completed fold** while one is running.
///
/// Coarser than [`FLUSH_COMPLETION_POLL`] purely because a fold's duration is minutes to hours
/// rather than seconds (compaction §14: modelled, IO-bound, never measured), so the 20 ms interval
/// would spin the loop hundreds of thousands of times waiting for one publication. There is no
/// latency argument on the other side: compaction §6.1's standing ruling is that a fold's wall
/// clock is a property nobody observes, so a fifth of a second at the end of it is free. Once the
/// fold *has* sent, `fold_completed_pending` puts the wait back on the fast poll.
const FOLD_COMPLETION_POLL: std::time::Duration = std::time::Duration::from_millis(200);

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
pub(crate) struct MaintenanceDeps {
    pub(crate) max_age_secs: u64,
    /// §4.1's `flush_max_items` — the tick's row trigger.
    pub(crate) max_items: usize,
    /// The entity-space coalesce's policy — see [`crate::coalesce::CoalescePolicy`].
    pub(crate) coalesce: crate::coalesce::CoalescePolicy,
    /// What a geometry publication needs to start the background refresh decision 0044's D1
    /// rules — see [`crate::refresh`].
    pub(crate) refresh: crate::refresh::RefreshDeps,
    /// The row-space merge's policy — see [`crate::merge`].
    pub(crate) merge: MergePolicy,
    /// The artifact row forms, shared for the one thing this thread does with them: rebuilding
    /// every level's projection **inside** the fold that invalidated it
    /// (`annotation-representation.md` §5.0.3). A level is a deployment-wide artefact rather than a
    /// per-session value, so leaving it to the first request after the flip is a stall of tens of
    /// seconds for whoever arrives first.
    pub(crate) artifact_projections: Arc<crate::artifacts::ArtifactProjections>,
    /// The region decompositions (`crate::region`), pruned of superseded generations at every
    /// geometry swap exactly as the row-projection cache is — a row-space artefact keyed on a
    /// generation is unusable after it (I11), and only retention is left to do.
    pub(crate) region_cache: Arc<
        crate::single_flight::SingleFlightCache<
            crate::region::RegionKey,
            crate::region::RegionDecomposition,
        >,
    >,
    /// The spatial levels' held shapes and per-segment pieces (`crate::shapes`) — filled by the
    /// flush before its publication, rebuilt at a publication into a shape layer, re-resolved at
    /// the fold and the merge.
    pub(crate) shapes: Arc<crate::shapes::ShapeStore>,
    /// The lineages, shared for the half of the same warm that is theirs — see
    /// [`Executor::warm_artifact_caches`].
    pub(crate) lineages: Arc<crate::cut::Lineages>,
    /// The supplied-content tables, shared for the one thing this thread does with them: dropping
    /// a layer's when the layer is dropped, beside the two caches above.
    pub(crate) level_contents: Arc<crate::artifact_content::LevelContents>,
    /// Whether the coalesce and the merge run at all — see `Engine::merge_enabled`.
    pub(crate) coalesce_enabled: Arc<AtomicBool>,
    pub(crate) merge_enabled: Arc<AtomicBool>,
    /// The bundle **root**, from which the live prefix directory is derived per use — see
    /// [`Executor::prefix_dir`] and `Engine::bundle_root`.
    pub(crate) bundle_root: PathBuf,
    /// Where a rebuilt suggestion index is written — the engine's own cache directory, never the
    /// bundle (`crate::suggest`'s header).
    pub(crate) suggest_dir: PathBuf,
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
    /// `EngineConfig::max_merged_segment_bytes` **as configured**, `None` where the deployment set
    /// nothing — not the resolved policy value, which always has one.
    ///
    /// Compaction §4 step 3 re-checks write-path §7's base-segment relation against the fold's own
    /// output, because a fold that shrank the base below an operator's configured merge cap would
    /// publish a deployment the *next startup* refuses to open. `tessera-server`'s loader checks
    /// only an explicitly set value (an unset one is derived from the base and cannot violate the
    /// relation), so the fold must be able to tell the two apart — which the policy alone cannot.
    pub(crate) configured_merge_bytes: Option<u64>,
    /// Whether a fold **holds** between its last pass and its submission —
    /// `Engine::set_fold_paused_for_test`, which is what lets a test land a flush inside a fold's
    /// flight. Always `false` in a shipped build.
    pub(crate) fold_paused: Arc<AtomicBool>,
    /// Whether a **completed** fold is left undrained in its channel —
    /// `Engine::set_fold_publication_paused_for_test`. Always `false` in a shipped build.
    ///
    /// **The other half of [`Self::fold_paused`], and it opens a different window.** That one holds
    /// the fold thread *before* it clears `fold_in_flight`, so merge and coalesce are still
    /// suspended and nothing can publish under it. This one lets the thread finish — the flag
    /// clears, the suspension lifts — and stops the executor draining the result, which is the one
    /// state in which a merge or coalesce can dispatch, publish, and leave the fold planned against
    /// artefacts the live manifest no longer lists.
    ///
    /// That state was reachable in production, and **not rarely**: the fold thread clears
    /// `fold_in_flight` after its send, and an executor already inside `tick_if_due` read the
    /// cleared flag and dispatched. Measured at **3 of 93 whole-binary runs and 12 of 480 runs of
    /// the single test (~3%)** under 3–4 concurrent lanes, each occurrence costing a discarded
    /// corpus rewrite and an orphan prefix nothing sweeps. The instruction window is microseconds
    /// wide; the *observed* rate is not that, because the executor's loop and the job's completion
    /// are both driven by the tick cadence and align far more often than independence predicts.
    /// The dispatchers now suspend on publication rather than on completion
    /// ([`Executor::fold_outstanding`]), so the state is no longer reachable through the executor
    /// at all. This hook is what holds a fold in it, which is how the suspension itself is tested:
    /// a merge offered ten ticks under a held fold takes none of them.
    pub(crate) fold_publication_paused: Arc<AtomicBool>,
    /// Whether a **completed** merge is left undrained in its channel —
    /// `Engine::set_merge_publication_paused_for_test`. Always `false` in a shipped build.
    ///
    /// [`Self::fold_publication_paused`]'s shape, opening the merge's own window: a flush
    /// publishing between a merge's plan and its publication, which is the interleaving under
    /// which the merge's rebase must keep the live manifest's watermark rather than its
    /// plan-time snapshot (`crate::merge::rebase_into`). Reachable in production on any tick a
    /// merge and a flush share, microseconds wide unassisted; this makes it deterministic.
    pub(crate) merge_publication_paused: Arc<AtomicBool>,
    /// When a fold is dispatched with nobody asking for one — see
    /// [`crate::compact::CompactionSchedule`].
    pub(crate) compaction: crate::compact::CompactionSchedule,
}

/// The bundle's declared scalar tail, as the segment writer wants it.
///
/// **Infallible, because an unwritable declaration cannot reach an open bundle.**
/// `DeclaredScalar::arrow_type` is a `ScalarType`, so a manifest naming a type this build cannot
/// store fails to deserialise and the bundle never opens (`manifest::scalar_type_name`). This
/// returned `None` while the field was a string, and a flush, an ingest and a fold each carried an
/// arm for that case — three guards against a state no loaded generation can be in.
pub(crate) fn scalar_schema_of(
    manifest: &tessera_store::manifest::Manifest,
) -> Vec<(String, ScalarType)> {
    // **Render columns only.** A `filter`-only column is entity-space; giving it a slot in every
    // row is the cost §10.3's routing exists to avoid, and — since `gather_scalars` refuses a
    // segment missing a declared column — a schema built from the full list would make a merge
    // refuse the build's own segment for correctly omitting one.
    manifest
        .render_scalars()
        .map(|d| (d.name.clone(), d.arrow_type))
        .collect()
}

/// Every view's **group-scoped** attribute families, keyed by view id (`views.md` §5).
///
/// **The families whose owning group's key set holds this view's key** — the owner's own views,
/// and the same keys under every group declaring `members` of it (§3.3, decision 0116). A family
/// belongs to the group that owns the keys (a `members` group's list is always empty), and a batch
/// into any view addressing one of those keys carries its values under their plain names, because
/// the address of a scoped value is `(attribute → its group, key)` and never the view.
///
/// The sharing door was refused until 2026-09-01 on the argument that two views of one key would
/// put two extents over one entity. That is withdrawn: the cell is written once — `admit`'s cell
/// arm dedupes an identical second value and refuses a differing one — and the extent it is written
/// into is addressed by [`scoped_owner_view_of`], which is the owner's view id whichever door the
/// row came through.
///
/// **This is the one derivation, and three callers take it**: the ingest boundary parses a batch's
/// schema against it, the commit window mints a scoped category's keys against it, and the flush
/// writes a row's values into the columns it names. A second copy would let a row's positional
/// tail be built against one list and read against another.
pub(crate) fn scoped_families_by_view(
    manifest: &tessera_store::manifest::Manifest,
) -> FxHashMap<String, Vec<tessera_store::manifest::ScopedScalar>> {
    let mut out: FxHashMap<String, Vec<tessera_store::manifest::ScopedScalar>> =
        FxHashMap::default();
    for group in &manifest.groups {
        for view in &group.views {
            let id = format!(
                "{}{}{}",
                group.name,
                tessera_store::GROUP_SEPARATOR,
                view.key
            );
            let families = scoped_families_of_view(manifest, &id);
            if families.is_empty() {
                continue;
            }
            out.insert(id, families.to_vec());
        }
    }
    out
}

/// One view's **group-scoped** families — [`scoped_families_by_view`]'s rule, asked of one view.
///
/// **The rule itself lives here and the map is built from it**, so a caller that wants one view's
/// answer does not walk every group to get it and cannot derive a second, differing list. The
/// families are the **owning** group's, in that group's manifest order, which is the order a
/// buffered row's `scoped` list is positional against; empty for a plain view, for a view whose key
/// the owning group does not carry, and for a group that owns no family.
pub(crate) fn scoped_families_of_view<'a>(
    manifest: &'a tessera_store::manifest::Manifest,
    view: &str,
) -> &'a [tessera_store::manifest::ScopedScalar] {
    const NONE: &[tessera_store::manifest::ScopedScalar] = &[];
    let owner = scoped_owner_view_of(manifest, view);
    let Some((owner_group, key)) = owner.split_once(tessera_store::GROUP_SEPARATOR) else {
        return NONE;
    };
    let Some(group) = manifest.groups.iter().find(|g| g.name == owner_group) else {
        return NONE;
    };
    // A sharing group's roster carries the owner's keys by construction, so the key is matched
    // rather than assumed: a key the owning group does not carry addresses no cell.
    if !group.views.iter().any(|v| v.key == key) {
        return NONE;
    }
    &group.scoped_scalars
}

/// The view id a scoped value written through `view` is **addressed by** — the owning group's view
/// of the same key (`views.md` §5, decision 0116).
///
/// Equal to `view` itself for every view of the owning group, and for every view in no scope at
/// all; a sharing group's view resolves to the owner's. This is the one place a door becomes an
/// address, so a row that arrived through the sharing spelling writes the byte-identical extent,
/// under the byte-identical column name, that the owner's door would have written.
pub(crate) fn scoped_owner_view_of(
    manifest: &tessera_store::manifest::Manifest,
    view: &str,
) -> String {
    let Some((group, key)) = view.split_once(tessera_store::GROUP_SEPARATOR) else {
        return view.to_string();
    };
    let owner = manifest
        .groups
        .iter()
        .find(|g| g.name == group)
        .and_then(|g| g.members_of.as_deref())
        .unwrap_or(group);
    format!("{owner}{}{key}", tessera_store::GROUP_SEPARATOR)
}

/// One view's writer schema: the bundle-wide render tail, then the **group-scoped** render lanes
/// that view's rows carry (`views.md` §5).
///
/// **Every producer of a segment takes this, and taking the bundle-wide list alone was the
/// defect.** A build writes a scoped family's lane into the row tail of every view of its group;
/// a merge or a fold that rewrote such a segment from `scalar_schema_of` alone wrote the
/// entity-scoped tail and nothing per family, so values served correctly before the rewrite came
/// back as the type's zero afterwards — indistinguishable from absence, with no error anywhere.
/// The lanes are appended after the declared ones, which is the order the build writes them in.
///
/// **No gate here, deliberately.** A writer has no principal; which lanes a row space holds is a
/// property of the bundle, and narrowing it by a session's sight would drop a lane the build
/// wrote. `viewport::scoped_render_scalars` is the read half, and it narrows the same list.
pub(crate) fn view_scalar_schema_of(
    manifest: &tessera_store::manifest::Manifest,
    view: &str,
) -> Vec<(String, ScalarType)> {
    let mut schema = scalar_schema_of(manifest);
    schema.extend(
        crate::viewport::scoped_render_families(manifest, view)
            .into_iter()
            .map(|f| (f.name.clone(), f.arrow_type)),
    );
    schema
}

/// The columns of one view's writer schema an input segment may lawfully lack, for a merge or a
/// fold of segments written before them (`tessera_store::segment_cursor::gather_scalars`): the
/// group-scoped render lanes, which begin at `entity_scoped` in the schema (`views.md` §5), and
/// the entity-scoped columns declared at a running service and not yet folded (`ingest.md` §6.3).
/// Every other column of the schema is one every input holds, and one missing is a torn segment.
pub(crate) fn lawful_absences(
    schema: &[(String, ScalarType)],
    entity_scoped: usize,
    runtime: &[String],
) -> Vec<String> {
    schema
        .iter()
        .enumerate()
        .filter(|(position, (name, _))| {
            *position >= entity_scoped || runtime.iter().any(|held| held == name)
        })
        .map(|(_, (name, _))| name.clone())
        .collect()
}

/// The filterable columns, with the position each occupies in a buffered row's scalar list.
///
/// **Positional against the full `declared_scalars`, not against the render tail.** A row's scalars
/// are indexed positionally against the whole declaration (that is how the commit window finds a
/// category key's vocabulary), and `scalar_schema_of` deliberately narrows to render columns — so
/// building this from that list would read a filter column's value out of a neighbouring column's
/// slot wherever the two differ, which is most schemas.
///
/// **A `visibility = "derived"` category is here whether or not it is declared filterable**, and
/// that is the same rule the build applies when it decides which columns owe a value column and
/// derived postings (`filter-index.md` §2.3). Its member sets are what `/v1/categories` derives
/// value visibility from, and the postings cover the build alone — so without an extent, a value
/// carried only by entities ingested since the build would never be offered to a principal who can
/// see one of them. The set of columns a flush writes extents for and the set the reader composes
/// are the same predicate, `filter::owes_value_column`, or the reader refuses an extent for a
/// column it does not hold.
pub(crate) fn filter_schema_of(
    manifest: &tessera_store::manifest::Manifest,
) -> Vec<crate::flush::FilterColumnSpec> {
    manifest
        .declared_scalars
        .iter()
        .enumerate()
        // **Text is not here, and `owes_value_column` is where that is decided** — it owes no value
        // column, so there is no attribute extent for this pass to write. Its flush track is
        // `text_schema_of`, whose extent is a dictionary and postings instead.
        .filter(|(_, d)| crate::filter::owes_value_column(d, &manifest.vocabularies))
        .map(|(index, d)| crate::flush::FilterColumnSpec {
            index,
            name: d.name.clone(),
            ty: d.arrow_type,
            category: d.vocabulary.is_some(),
        })
        .collect()
}

/// The indexed `text` columns, each with the analyser its declaration named — resolved once per
/// dispatch rather than per row, because constructing one deserialises the segmenter's dictionaries.
///
/// **Refuses rather than defaults when the binary does not carry the recorded analyser.** A flush
/// that indexed a batch with a different pipeline than the base build used would leave one column
/// whose two layers disagree about what a word is, and a `match` would answer from whichever layer
/// happened to hold the entity — a wrong answer with no error anywhere.
pub(crate) fn text_schema_of(
    manifest: &tessera_store::manifest::Manifest,
) -> Result<Vec<crate::flush::TextColumnSpec>, crate::flush::FlushFailed> {
    let mut out = Vec::new();
    for (index, d) in manifest.declared_scalars.iter().enumerate() {
        if d.arrow_type != tessera_spatial::tiler::ScalarType::Text || !d.index {
            continue;
        }
        let identity = d.analyser.as_deref().ok_or_else(|| {
            crate::flush::FlushFailed(format!(
                "column '{}' is text but the manifest records no analyser identity",
                d.name
            ))
        })?;
        let analyser = tessera_analyse::analyser(identity.split('/').next().unwrap_or_default())
            .filter(|a| a.identity() == identity)
            .ok_or_else(|| {
                crate::flush::FlushFailed(format!(
                    "column '{}' was indexed by analyser '{identity}', which this binary does not \
                     carry — a flush cannot extend an index whose terms it cannot reproduce",
                    d.name
                ))
            })?;
        out.push(crate::flush::TextColumnSpec {
            index,
            name: d.name.clone(),
            analyser: std::sync::Arc::new(analyser),
        });
    }
    Ok(out)
}

/// The analyser a group-scoped `text` family's terms were produced by
/// ([`text_schema_of`]'s resolution, over a family's declaration).
fn analyser_of(
    family: &tessera_store::manifest::ScopedScalar,
) -> Result<tessera_analyse::Analyser, crate::flush::FlushFailed> {
    let identity = family.analyser.as_deref().ok_or_else(|| {
        crate::flush::FlushFailed(format!(
            "the scoped column family '{}' is text but the manifest records no analyser identity",
            family.name
        ))
    })?;
    tessera_analyse::analyser(identity.split('/').next().unwrap_or_default())
        .filter(|a| a.identity() == identity)
        .ok_or_else(|| {
            crate::flush::FlushFailed(format!(
                "the scoped column family '{}' was indexed by analyser '{identity}', which this \
                 binary does not carry — a flush cannot extend an index whose terms it cannot \
                 reproduce",
                family.name
            ))
        })
}

/// The blob-resident columns, with each one's position in a buffered row's scalar list — which is
/// also its field tag (records §3).
///
/// **The predicate must be the build's**, [`crate::filter::blob_resident`], because this is the
/// third placement pass and the three have to partition the same schema the same way. A flush that
/// placed a field differently from the build would drop an ingested value the build stores: the
/// buffered scalar is read by exactly three consumers — the render indices, the filter schema and
/// this one — and a column no consumer claims is acknowledged and then lost.
pub(crate) fn record_schema_of(
    manifest: &tessera_store::manifest::Manifest,
) -> Vec<crate::flush::RecordColumnSpec> {
    manifest
        .declared_scalars
        .iter()
        .enumerate()
        .filter(|(_, d)| crate::filter::blob_resident(d, &manifest.vocabularies))
        .map(|(index, d)| crate::flush::RecordColumnSpec {
            index,
            name: d.name.clone(),
            ty: d.arrow_type,
        })
        .collect()
}

/// A vocabulary code, at its column's declared width. Mirrors `tessera-server`'s own `category_code`
/// helper of the same shape, which cannot be reused here: that one lives on the other side of the
/// ingest boundary and returns an `ApiError`, where a mint failure here is `Executor`-internal and
/// has already been dispositioned by the time a code is being written.
///
/// `is_category_width` (checked at schema parse) admits `u8`/`u16`/`u32` only, so the fallthrough is
/// `u32` — the widest, which cannot truncate a code the other two could hold.
/// The category-width code a row's scalar carries, or `None` where it carries none.
///
/// **The three category widths and nothing else** — `tessera_spatial::ScalarType::is_category_width`
/// is what the declaration is checked against, so a wider or non-integer column never names a
/// predicate layer and a value of one reaching here is a schema that never validated.
fn scalar_code(scalar: &WalScalar) -> Option<u32> {
    match scalar {
        WalScalar::U8(v) => Some(u32::from(*v)),
        WalScalar::U16(v) => Some(u32::from(*v)),
        WalScalar::U32(v) => Some(*v),
        _ => None,
    }
}

fn code_at_declared_width(width: ScalarType, code: u32) -> WalScalar {
    match width {
        ScalarType::U8 => WalScalar::U8(code as u8),
        ScalarType::U16 => WalScalar::U16(code as u16),
        _ => WalScalar::U32(code),
    }
}

/// Why [`Executor::commit_side_manifest`] did not commit: the manifest would regress durable
/// state, or the store could not write it. One type so every publication site's failure arm
/// reports whichever it was through the `error = %e` it already has.
enum ManifestCommitRefused {
    Regresses(crate::geometry::ManifestRegression),
    Store(tessera_store::StoreError),
}

impl std::fmt::Display for ManifestCommitRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ManifestCommitRefused::Regresses(r) => r.fmt(f),
            ManifestCommitRefused::Store(e) => e.fmt(f),
        }
    }
}

/// Every view the bundle holds, across partitions. A flush plans per view, because a segment's
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

/// Carry the live vocabulary bindings into a manifest's `vocabulary_extensions` — `write_deny_state`'s
/// sibling, called beside it at every publication site except the fold's.
///
/// **Union, never restate.** `manifest` here is always `partition_data.manifest.clone()`, so it
/// already carries every extension a previous publication wrote; `extensions_beyond` gives only
/// what `MANIFEST.vocabularies` (the *build*, not this side-manifest) does not already carry, and
/// this appends that into what is already held rather than replacing it. That asymmetry with
/// [`write_deny_state`] is deliberate and is [`tessera_store::manifest::VocabularyExtension`]'s own
/// documented rule: `deny` must be able to shrink on an unsuppress, so it is restated fresh every
/// time; a binding must never shrink, so restating it fresh is exactly the shape that could
/// silently drop one. On the executor as it stands today `vocabularies` is always the same live
/// generation the manifest was cloned from, so `extensions_beyond` happens to recompute a superset
/// of whatever is already held — but that is a fact about today's single-threaded caller, not a
/// property of this function, and this function must hold even if that caller ever changes. The
/// unit test beside it proves the union rather than trusting the coincidence.
///
/// The fold does not call this: it folds every served `vocabulary_extensions` directly into the new
/// prefix's `MANIFEST.vocabularies` and writes an empty extension set on purpose (see
/// `publish_fold`) — restating the same bindings here as well would bind each key twice, once in
/// each home.
fn write_vocabulary_extensions(
    manifest: &mut SegmentsManifest,
    vocabularies: &Vocabularies,
    bundle_vocabularies: &[tessera_store::manifest::ManifestVocabulary],
) {
    for extension in vocabularies.extensions_beyond(bundle_vocabularies) {
        match manifest
            .vocabulary_extensions
            .iter_mut()
            .find(|held| held.name == extension.name)
        {
            Some(held) => {
                for value in extension.values {
                    if !held.values.iter().any(|v| v.key == value.key) {
                        held.values.push(value);
                    }
                }
            }
            None => manifest.vocabulary_extensions.push(extension),
        }
    }
}

#[cfg(test)]
mod segment_schema_tests {
    use super::*;
    use tessera_plugin::Plugin;
    use tessera_store::manifest::DeclaredScalar;

    /// **A segment's writer schema is the render columns, and this guards the one line that makes
    /// it so.**
    ///
    /// `scalar_schema_of` feeds flush, merge, fold and compact. `gather_scalars` refuses a segment
    /// missing a declared column, so a schema built from the *full* `declared_scalars` — which
    /// includes `filter`-only columns, deliberately absent from `columns.arrow` — makes a merge
    /// refuse the build's own segment. Nothing is wrong at either end, and an ordinary merge
    /// reaches it.
    ///
    /// This calls the production function rather than re-deriving its filter, because a test that
    /// re-implements the predicate passes with the fix reverted.
    #[test]
    fn a_segments_writer_schema_omits_filter_only_columns() {
        let manifest = tessera_store::manifest::Manifest {
            bundle_format: 3,
            created_at: String::new(),
            data_plugin_hash: tessera_plugin::Passthrough::new().data_plugin_hash(),
            declared_bounds: serde_json::json!({}),
            vocabularies: vec![],
            small_term_threshold: 32,
            entity_id_high_water: 0,
            identity: tessera_store::manifest::IdentityDescriptor {
                construction: "siphash-2-4".to_string(),
                rounds: 1,
                key: "0123456789abcdef0123456789abcdef".to_string(),
                shard_id: 0,
                idset: 1,
            },
            groups: Vec::new(),
            views: vec![],
            partitions: vec![],
            provenance: serde_json::json!({}),
            files: std::collections::BTreeMap::new(),
            declared_scalars: vec![
                DeclaredScalar {
                    name: "department".to_string(),
                    arrow_type: ScalarType::U16,
                    vocabulary: Some("departments".to_string()),
                    analyser: None,
                    index: true,
                    render: true,
                },
                DeclaredScalar {
                    name: "title".to_string(),
                    arrow_type: ScalarType::Utf8,
                    vocabulary: None,
                    analyser: None,
                    index: true,
                    render: false,
                },
            ],
        };

        let schema = scalar_schema_of(&manifest);
        assert_eq!(
            schema,
            vec![("department".to_string(), ScalarType::U16)],
            "a filter-only column must not reach the segment writer"
        );

        // And the full list is untouched — the ingest plane supplies values for every declared
        // column, filterable ones included.
        assert_eq!(manifest.declared_scalars.len(), 2);
    }
}

#[cfg(test)]
mod vocabulary_extensions_tests {
    use super::*;
    use std::collections::BTreeMap;
    use tessera_store::manifest::{
        ManifestVocabulary, ManifestVocabularyValue, VocabularyExtension, VocabularyKind,
    };

    fn empty_manifest() -> SegmentsManifest {
        SegmentsManifest {
            watermark: 0,
            entity_id_high_water: 0,
            entity_id_low_water: tessera_types::layer::ROWLESS_CEILING,
            layers: Vec::new(),
            layer_tombstones: Vec::new(),
            views: Vec::new(),
            scoped_columns: Vec::new(),
            attributes: Vec::new(),
            scoped_attributes: Vec::new(),
            vocabularies: Vec::new(),
            groups: Vec::new(),
            plain_views: Vec::new(),
            dead_view_incarnations: Vec::new(),
            membership_extents: Vec::new(),
            level_versions: Vec::new(),
            containment_extents: Vec::new(),
            tile_index_extents: Vec::new(),
            row_column_extents: Vec::new(),
            shape_rows_extents: Vec::new(),
            shape_held_extents: Vec::new(),
            artifact_record_extents: Vec::new(),
            segments: Vec::new(),
            deltas: Vec::new(),
            dict_extents: Vec::new(),
            attr_extents: Vec::new(),
            record_extents: Vec::new(),
            entity_terms_extents: Vec::new(),
            text_extents: Vec::new(),
            external_id_runs: Vec::new(),
            locator_extents: Vec::new(),
            tombstones: Vec::new(),
            deny: Vec::new(),
            vocabulary_extensions: Vec::new(),
            files: BTreeMap::new(),
        }
    }

    fn empty_vocabulary(name: &str) -> ManifestVocabulary {
        ManifestVocabulary {
            name: name.to_string(),
            kind: VocabularyKind::Discovered,
            visibility: crate::Visibility::Derived,
            width: "u32".to_string(),
            values: Vec::new(),
            reserved: Vec::new(),
        }
    }

    /// **A carried binding must survive even when the live view has nothing to say about it.**
    /// `extensions_beyond` only emits an entry for a vocabulary its own `by_name` tracks
    /// (`vocabulary.rs`'s doc on the type), so a manifest that already carries an extension for one
    /// the live view does not — here, `vocabularies` tracks only `"department"`, with no bindings of
    /// its own, so `extensions_beyond` returns nothing at all — must not have that carried entry
    /// erased by a write that touches an unrelated vocabulary.
    ///
    /// **Mutation:** replace the union body with
    /// `manifest.vocabulary_extensions = vocabularies.extensions_beyond(bundle_vocabularies);` and
    /// this fails — the carried `"legacy"` binding is wiped by a write that had nothing new to say.
    #[test]
    fn a_carried_extension_survives_a_write_the_live_view_recomputes_nothing_for() {
        let mut manifest = empty_manifest();
        manifest.vocabulary_extensions.push(VocabularyExtension {
            name: "legacy".to_string(),
            values: vec![ManifestVocabularyValue {
                key: "held".to_string(),
                code: 7,
                title: None,
            }],
        });

        let vocabularies = Vocabularies::seed(&[empty_vocabulary("department")], &[], &[]).unwrap();

        write_vocabulary_extensions(&mut manifest, &vocabularies, &[]);

        let legacy = manifest
            .vocabulary_extensions
            .iter()
            .find(|e| e.name == "legacy")
            .expect("a binding this manifest already carried must not be dropped");
        assert_eq!(legacy.values.len(), 1);
        assert_eq!(legacy.values[0].key, "held");
        assert_eq!(legacy.values[0].code, 7);
    }

    /// The ordinary case beside it: a fresh mint is appended beside what is already carried, and a
    /// binding restated identically is not duplicated.
    #[test]
    fn a_fresh_binding_is_appended_beside_what_is_already_carried_and_not_duplicated() {
        let mut manifest = empty_manifest();
        manifest.vocabulary_extensions.push(VocabularyExtension {
            name: "department".to_string(),
            values: vec![ManifestVocabularyValue {
                key: "eng".to_string(),
                code: 4,
                title: None,
            }],
        });

        let mut vocabularies =
            Vocabularies::seed(&[empty_vocabulary("department")], &[], &[]).unwrap();
        // Restates the binding the manifest already holds, plus one genuinely new one.
        vocabularies
            .get_mut("department")
            .unwrap()
            .seed_value("eng", 4)
            .unwrap();
        vocabularies
            .get_mut("department")
            .unwrap()
            .mint("finance")
            .unwrap();

        write_vocabulary_extensions(&mut manifest, &vocabularies, &[]);

        let department = manifest
            .vocabulary_extensions
            .iter()
            .find(|e| e.name == "department")
            .unwrap();
        let mut keys: Vec<&str> = department.values.iter().map(|v| v.key.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["eng", "finance"],
            "the carried key and the fresh one both survive, each exactly once"
        );
    }
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
        // view's oldest waiting row. An empty plan cannot occur (`plan_flush` returns
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

/// One superseded prefix awaiting reclamation, and the two `Arc`s whose release says no thread can
/// still resolve a path inside it — see [`Executor::pending_reclaim`].
struct PendingReclaim {
    generation: Arc<Generation>,
    prefix_dir: PathBuf,
    /// Every sidecar that was live over this prefix **before** the one the held generation carries
    /// — see [`Executor::superseded_sidecars`].
    superseded_sidecars: Vec<std::sync::Weak<crate::session::ExternalIdIndex>>,
}

/// Seconds since the Unix epoch, or `None` if the clock is before it.
///
/// `None` reads as "no fold has ended yet", which switches the interval floor off rather than
/// jamming it on — the safe direction for a clock this absurd, and the same answer a fresh process
/// gives.
fn unix_now() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|elapsed| elapsed.as_secs())
}

/// Memory this process could take without reclaiming anything it needs, in bytes — the figure
/// compaction §3's pre-flight compares its estimate against. `None` where unknowable.
///
/// **Two sources and the smaller wins**, because either can be the real bound: `MemAvailable` is
/// the kernel's own estimate of what an allocation could get without swapping, already net of the
/// page cache it would evict; a cgroup v2 `memory.max` is the ceiling a container is killed at, and
/// it charges page cache against itself, so a node with 400 GiB of host RAM and a 16 GiB cgroup is
/// bounded by the cgroup. Reading only the first is how a fold passes its pre-flight and is then
/// OOM-killed by the container that always owned the answer.
fn available_memory() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let available = meminfo
        .lines()
        .find(|line| line.starts_with("MemAvailable:"))
        .and_then(|line| line.split_whitespace().nth(1)?.parse::<u64>().ok())
        .map(|kib| kib * 1024)?;

    // `memory.max` is "max" when unlimited, which parses to `None` and leaves `MemAvailable` as the
    // answer — the same result as no cgroup at all.
    let cgroup = std::fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|cgroup| {
            let path = cgroup
                .lines()
                .next()?
                .split(':')
                .nth(2)?
                .trim_start_matches('/');
            std::fs::read_to_string(format!("/sys/fs/cgroup/{path}/memory.max")).ok()
        })
        .and_then(|max| max.trim().parse::<u64>().ok());

    Some(cgroup.map_or(available, |limit| limit.min(available)))
}

/// **Compaction §7's startup sweep**: delete every `v#####` tree under the bundle root that
/// `CURRENT` does not name, once, before the executor thread is spawned.
///
/// # What it is cleaning up, and why nothing else could
///
/// Two residues, and neither has any other route out. A **discarded fold** leaves a complete
/// prefix — five passes' worth of output, up to a bundle in size — under a name `CURRENT` never
/// took; `next_prefix_name` steps *past* it by construction and the reclamation at a fold's tail
/// takes only the prefix that fold itself superseded, so no later fold ever comes back for it. A
/// **process that exits mid-fold, or between the `CURRENT` flip and the swap, or while a
/// superseded prefix is still waiting on its last reader**, leaves the same thing. Without this
/// each occurrence costs a bundle of disc until an operator notices, which is what made the
/// interval floor on a *discarded* fold the only thing between one bad configuration value and a
/// full device (compaction §8, §9).
///
/// # Why startup is the only safe time, and the executor the only safe caller
///
/// Mid-life, a prefix `CURRENT` does not name may still be one this process is serving: the live
/// generation holds mappings into it until the last request finishes, which is exactly what
/// [`Executor::pending_reclaim`] waits on. At the moment the write executor starts there is no such
/// generation — nothing has been served, and the only prefix any mapping can name is the one
/// `Engine::open` read from `CURRENT`. So "not live" and "not in use" coincide here and nowhere
/// else.
///
/// **Called synchronously from `start_executor`, before the thread is spawned, and that is not a
/// detail.** A directory that exists but is not yet committed is indistinguishable from an orphan —
/// which is correct for a fold's output, since a fold cannot run before this does, and wrong for a
/// prefix a *caller* is staging through `Engine::publish_rotated_prefix_for_test`. Running on the spawned
/// thread leaves exactly that race: `start_write_executor` returns, the caller begins staging, and
/// the sweep reads the directory between its creation and the `CURRENT` flip. Running here makes
/// "the sweep has finished" something the caller can observe, by `start_write_executor` having
/// returned.
///
/// **A writer's act, which is why it is not in `Engine::open`.** A node that has not started a write
/// executor has not declared itself the bundle's writer, and deleting another process's superseded
/// prefix from a read-only replica is not this crate's judgement to make. (It would in fact be safe
/// — a POSIX mapping outlives its directory entry, the same argument compaction §8 makes for the
/// fragment sweep, and a fresh open resolves through `CURRENT`, which is never swept — but "safe"
/// is not "ours to do".)
///
/// # A swept name can be issued again, and that is not contracts §2.1's id reuse
///
/// `next_prefix_name` counts from the directory listing, so once an orphan `v00003` is gone the
/// next fold may be `v00003`. A `seg_id` may never be reused because a rebase check compares them
/// to decide whether a mid-flight unit still applies; a prefix name is a directory name nothing
/// holds across the sweep. What identifies a bundle is its `MANIFEST.json` digest, which `CURRENT`
/// carries beside the prefix and which the fragment cache keys on — two prefixes sharing a name
/// across a proven-complete deletion of the first are still distinguishable by every mechanism that
/// has to tell them apart.
///
/// **Every failure is a warning and nothing else.** `reclaim_prefix` refuses the live prefix itself
/// (a second guard behind this one's own filter), and a tree that cannot be deleted is a tree that
/// stays — the same residual as before this existed, and never a reason to refuse to start.
fn sweep_orphan_prefixes(bundle_root: &Path, live: &str) {
    let Ok(entries) = std::fs::read_dir(bundle_root) else {
        return;
    };
    let (mut swept, mut refused) = (0usize, 0usize);
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        // A prefix is `v` + five-or-more digits (`next_prefix_name`'s own shape). Anything else
        // under the root — `CURRENT`, the WAL, a directory an operator put there — is not this
        // sweep's business and must not be guessed at.
        let is_prefix = name
            .strip_prefix('v')
            .is_some_and(|digits| digits.len() >= 5 && digits.bytes().all(|b| b.is_ascii_digit()));
        if !is_prefix || name == live || !entry.path().is_dir() {
            continue;
        }
        match tessera_store::reclaim_prefix(&entry.path()) {
            Ok(()) => {
                swept += 1;
                tracing::info!(
                    prefix = %name,
                    "startup sweep: reclaimed an orphaned prefix left by a discarded fold or an \
                     exit mid-publication"
                );
            }
            Err(e) => {
                refused += 1;
                tracing::warn!(
                    prefix = %name,
                    error = %e,
                    "startup sweep: could not reclaim an orphaned prefix; it stands, and its disc \
                     with it"
                );
            }
        }
    }
    if swept > 0 || refused > 0 {
        tracing::info!(live = %live, swept, refused, "startup sweep complete");
    }
}

/// What is on disc under `prefix_dir` against what `generation`'s manifests still name — the
/// operands of compaction §9's dead-bytes gauge. `None` where the tree cannot be walked.
///
/// **Two manifests, and summing one of them alone reads as a catastrophe.** The build's artefacts
/// are digested in the bundle-level `MANIFEST.json`; everything the write path produced is in the
/// partition's `SEGMENTS-<n>.json`. Taking only the side-manifest reported 9.1 MiB named against a
/// 9.4 GiB tree in one measured run — an orphan ratio of 1065×, which was a missing addend and not
/// a leak.
///
/// **What the gap actually is**: every merged-away segment, every consumed tier, every superseded
/// side-manifest. They stay because a step-down serves one of them (contracts §2.3), and a fold is
/// the only thing that reclaims them — which is what makes this a fold trigger rather than an
/// alarm. The measured no-compaction steady state is 2.0–2.6×.
fn dead_bytes_of(prefix_dir: &Path, generation: &Generation) -> Option<crate::compact::DeadBytes> {
    fn walk(dir: &Path, total: &mut u64) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                walk(&entry.path(), total);
            } else {
                *total += meta.len();
            }
        }
    }
    if !prefix_dir.is_dir() {
        return None;
    }
    let mut on_disc = 0u64;
    walk(prefix_dir, &mut on_disc);
    let named: u64 = generation
        .bundle
        .manifest
        .files
        .values()
        .map(|digest| digest.size)
        .chain(
            generation
                .bundle
                .partitions
                .values()
                .flat_map(|partition| partition.manifest.files.values())
                .map(|digest| digest.size),
        )
        .sum();
    Some(crate::compact::DeadBytes { on_disc, named })
}

/// Free bytes on the filesystem holding `path`, or `None` where unknowable — compaction §8's
/// pre-flight then does not run, on `tessera-build`'s precedent rather than refusing on a guess.
///
/// `f_bavail`, not `f_bfree`: the reserved blocks a filesystem keeps for root are not space a fold
/// may plan to use.
#[cfg(unix)]
fn free_disc(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stats) } != 0 {
        return None;
    }
    Some(stats.f_bavail as u64 * stats.f_frsize as u64)
}

#[cfg(not(unix))]
fn free_disc(_path: &Path) -> Option<u64> {
    None
}

/// The largest live segment count across this generation's views — compaction §9's segment gauge.
///
/// The **max** rather than the sum, because the gauge is per (partition, view): a tile resolves to
/// one contiguous range per live segment *of the view it is in*, so one view at 64 segments is
/// what a viewport there pays, whatever the others hold.
///
/// ⊘ **Untestable today, and stated rather than claimed**: no build emits a second view
/// (compaction §6.3), so max and sum agree on every bundle that exists and no case here
/// distinguishes them. It is written this way because the views fold-in is what makes the
/// difference real, not because a test caught it.
fn live_segments_of(generation: &Generation) -> usize {
    generation
        .bundle
        .partitions
        .values()
        .flat_map(|partition| partition.views.values())
        .map(|view| view.segments.len())
        .max()
        .unwrap_or(0)
}

/// Is this extent's `(view, incarnation)` pair one the live manifest still declares?
///
/// **An entity-scoped extent belongs to no view and is always carried** — its `view` is `None`,
/// and so is its incarnation. A group-scoped one is carried only where the pair is live: a dropped
/// key's columns sit in the list until a fold reclaims them, and a key created again writes its
/// own column under the same family name (decision 0115).
///
/// **Fail-closed by construction**: a half-stamped entry — a view with no incarnation, or the
/// reverse — matches nothing and is omitted, which loses a derived artefact and never serves one.
fn carries_live_view(
    live: &FxHashMap<&str, tessera_types::view::ViewIncarnation>,
    view: Option<&str>,
    incarnation: Option<tessera_types::view::ViewIncarnation>,
) -> bool {
    match (view, incarnation) {
        (None, None) => true,
        (Some(view), Some(incarnation)) => live.get(view) == Some(&incarnation),
        _ => false,
    }
}

/// The refusal a roster error is answered with — the three the wire tells apart
/// (`views.md` §3.2, and this module's `ExecError` doc for why the caller's remedy decides).
fn roster_error(e: tessera_lifecycle::RosterError) -> ExecError {
    use tessera_lifecycle::RosterError;
    let detail = e.to_string();
    match e {
        // **A conflict is a *live* key and nothing else now** (decision 0115): a dropped key is
        // created again at a fresh incarnation, so the tombstone arm this match once had has no
        // refusal left to carry.
        RosterError::Exists { .. } => ExecError::ViewConflict { detail },
        RosterError::Unknown { .. } => ExecError::ViewUnknown { detail },
        RosterError::Refused(_) => ExecError::ViewRefused { detail },
    }
}

/// The entities of `views` that hold a row in **no other view** — the commit-window buffer
/// included (`views.md` §3.4's `delete_dangling`).
///
/// **`views` is every id the dropped key resolves to** (`Manifest::view_ids_for_key`), not the one
/// the request happened to name: a key is a view of the group that owns it *and* one of every
/// group sharing its views (`views.md` §3.3), so a probe over a single spelling reads the wrong
/// row space when the drop was addressed to the other, and counts an entity dangling that holds a
/// row under the key's own second name.
///
/// **The buffer counts as a view's rows.** A row accepted but not yet flushed is in no
/// permutation, so a probe that read the permutations alone would call an entity dangling that a
/// caller was told had landed elsewhere — and then delete it.
///
/// **Row space is walked, entity space only where it cannot be.** A view's rows invert to their
/// entities directly wherever the row space can be inverted, which is every view a flush created
/// and every built view that published a `row-entity.u32`; where it cannot, the fallback asks
/// each entity below the high-water whether this view holds it, which is `O(entity space)` and is
/// reported rather than hidden, because a silent one would look like an idle service.
fn dangling_entities(generation: &Generation, views: &[String]) -> Vec<EntityId> {
    let mut candidates: Vec<EntityId> = Vec::new();
    for view in views {
        for partition in generation.bundle.partitions.values() {
            let Some(view_data) = partition.views.get(view) else {
                continue;
            };
            let rows = view_data.row_space.total_rows();
            if view_data.row_space.can_invert() {
                for row in 0..rows {
                    if let Some(entity) = view_data
                        .row_space
                        .entity_of(tessera_types::RowId::new(row as u32))
                    {
                        candidates.push(entity);
                    }
                }
            } else {
                let bound = view_data.row_space.base().bound();
                tracing::warn!(
                    view = %view,
                    entities = bound,
                    "this view publishes no row→entity table, so delete_dangling walks entity \
                     space to enumerate its rows"
                );
                for raw in 0..bound {
                    let entity = EntityId::new(raw);
                    if view_data.row_space.row_of(entity).is_some() {
                        candidates.push(entity);
                    }
                }
            }
        }
    }
    // **Every buffered row, joins included** (`rows()`, not `iter()`): the question here is which
    // entities have a row *in one of these views*, which is geometry, and a join is a row.
    for (entity, item) in generation.buffer.rows() {
        if views.iter().any(|view| view == &item.view) {
            candidates.push(*entity);
        }
    }
    candidates.sort_unstable_by_key(|e| e.raw());
    candidates.dedup();
    candidates.retain(|entity| {
        // Already deleted is already gone: a second deletion of the same entity is a no-op the
        // overlay would absorb, and counting it would report work the drop did not do.
        if generation.overlay.is_deleted(*entity) {
            return false;
        }
        let in_another_view = generation.bundle.partitions.values().any(|partition| {
            partition
                .views
                .iter()
                .any(|(id, data)| !views.contains(id) && data.row_space.row_of(*entity).is_some())
        });
        let buffered_elsewhere = generation.buffer.rows().any(|(buffered, item)| {
            buffered == entity && !views.iter().any(|view| view == &item.view)
        });
        !in_another_view && !buffered_elsewhere
    });
    candidates
}

/// Every entity holding a row in one of these views, or buffered for one, **minus the deleted**
/// — the set an exclusion is complemented against (`ingest.md` §2.3).
///
/// [`dangling_entities`]'s walk without its second question: that one asks which entities would
/// be left with no row if these views went away, and this asks which have a row in them now. A
/// view that publishes no row→entity table is walked over entity space, and the warning there is
/// that walk's, not repeated here.
///
/// **A deleted entity is excluded and a suppressed one is not.** A deletion is irreversible and
/// its entity can contribute to no count again — publishing a membership that named one would be
/// refused a statement later — where a suppression is a live member temporarily outside every
/// mask, which the inclusion spelling would have named and this one keeps.
/// **The cost is a walk of the view's rows**, on the executor loop, once per publication that
/// carries an exclusion: a row→entity inversion per row, or a `row_of` per entity where the view
/// publishes no inversion table. It is accepted — the bound on the *list* is what makes the
/// operation admissible at all, and the complement cannot be taken before the whole list is in
/// (`ingest.md` §2.3) — and it is stated here so it is not rediscovered as a surprise.
fn view_entities(generation: &Generation, views: &[String]) -> croaring::Bitmap {
    let mut entities = croaring::Bitmap::new();
    for view in views {
        for partition in generation.bundle.partitions.values() {
            let Some(view_data) = partition.views.get(view) else {
                continue;
            };
            let rows = view_data.row_space.total_rows();
            if view_data.row_space.can_invert() {
                for row in 0..rows {
                    if let Some(entity) = view_data
                        .row_space
                        .entity_of(tessera_types::RowId::new(row as u32))
                    {
                        entities.add(entity.raw() as u32);
                    }
                }
            } else {
                let bound = view_data.row_space.base().bound();
                tracing::warn!(
                    view = %view,
                    entities = bound,
                    "this view publishes no row→entity table, so an exclusion's complement walks \
                     entity space to enumerate its rows"
                );
                for raw in 0..bound {
                    let entity = EntityId::new(raw);
                    if view_data.row_space.row_of(entity).is_some() {
                        entities.add(raw as u32);
                    }
                }
            }
        }
    }
    // **Buffered rows are in the view** (`rows()`, joins included): a point acknowledged and not
    // yet flushed is an entity of this view, and an exclusion taken without it would leave every
    // such point out of the membership for ever — the one asymmetry between the two spellings
    // that would not be stale but wrong.
    for (entity, item) in generation.buffer.rows() {
        if views.iter().any(|view| view == &item.view) {
            entities.add(entity.raw() as u32);
        }
    }
    entities.remove_run_compression();
    let deleted: Vec<u32> = entities
        .iter()
        .filter(|raw| generation.overlay.is_deleted(EntityId::new(*raw as u64)))
        .collect();
    for raw in deleted {
        entities.remove(raw);
    }
    entities
}

fn views_of(generation: &Generation) -> Vec<String> {
    let mut views: Vec<String> = generation
        .bundle
        .partitions
        .values()
        .flat_map(|p| p.views.keys().cloned())
        .collect();
    views.sort_unstable();
    views.dedup();
    views
}

/// Whether this layer's memberships are a **stored** set — the one kind a record's delta describes.
///
/// A `spatial` layer's membership is the rows inside its shapes and an `attribute` layer's is the
/// rows carrying a value; both are evaluated against the geometry, so neither has anything to take
/// from a publication's or a growth's record.
/// One view's row space in `bundle`, or `None` where the partition or the view is not there.
fn view_row_space<'a>(
    bundle: &'a tessera_store::read::Bundle,
    partition: &str,
    view: &str,
) -> Option<&'a tessera_store::permutation::RowSpace> {
    bundle
        .partitions
        .get(partition)
        .and_then(|p| p.views.get(view))
        .map(|v| &v.row_space)
}

fn stored_membership(declaration: &tessera_types::layer::LayerDeclaration) -> bool {
    matches!(
        declaration.membership,
        tessera_types::layer::MembershipSource::Enumerated
    ) && declaration.shape.is_none()
}

/// The `(layer, level)` a record changes the artifacts of, and `None` for every other record —
/// what a caller needs to read that level's version before the record moves it.
fn artifact_level_of(record: &WalRecord) -> Option<(&str, u32)> {
    match record {
        WalRecord::ArtifactPublish { layer, level, .. }
        | WalRecord::ArtifactGrow { layer, level, .. } => Some((layer.as_str(), *level)),
        _ => None,
    }
}

/// **The growth records one closed window owes**, with the index of the entry to blame if an append
/// fails, in the order they are to be appended.
///
/// **One record per `(layer, level)` for the whole window, not one per entry.** A record is a list
/// of `(ordinal, joining)`, entries in a window are already committed together under one fsync, and
/// several batches naming one cluster are the ordinary shape of a client ingesting in parallel — so
/// merging costs one union and saves a record and a pin per batch. What may not merge is the
/// address: a join carries its own `(layer, level, ordinal)` and nothing infers one from another's.
///
/// The entities are `entity_ids[row]` — the assignment this window just made, in the caller's own
/// row order (`tessera_lifecycle::ClosedEntry`) — which is what puts a point's membership in the
/// same commit as the point.
fn growth_records<W>(closed: &[tessera_lifecycle::ClosedEntry<W>]) -> Vec<(WalRecord, usize)> {
    use std::collections::BTreeMap;
    /// One `(layer, level)`'s joins: the entry to blame for the append, and a bitmap per ordinal.
    type Level = (usize, BTreeMap<u32, croaring::Bitmap>);
    // Ordered, so the records a window appends do not depend on hash iteration order: two nodes
    // replaying one log must read the same sequence, and a test comparing two runs is entitled to
    // the same one.
    let mut by_level: BTreeMap<(&str, u32), Level> = BTreeMap::new();
    for (index, entry) in closed.iter().enumerate() {
        for join in &entry.memberships {
            // A key with no ordinal was minted at the close, and a minted artifact was published
            // *carrying* these rows — one record instead of a publication and a growth against it.
            let Some(ordinal) = join.ordinal else {
                continue;
            };
            let (_, ordinals) = by_level
                .entry((join.layer.as_str(), join.level))
                .or_insert_with(|| (index, BTreeMap::new()));
            let joining = ordinals.entry(ordinal).or_default();
            for row in &join.rows {
                let entity = entry.entity_ids[*row as usize];
                // Entity space is `u32` by I9, so the narrowing is total.
                joining.add(entity.raw() as u32);
            }
        }
    }
    by_level
        .into_iter()
        .filter_map(|((layer, level), (index, ordinals))| {
            let joins = ordinals
                .iter()
                .map(|(ordinal, joining)| (*ordinal, joining));
            tessera_lifecycle::membership::growth_record(layer, level, joins)
                .map(|record| (record, index))
        })
        .collect()
}

/// **The artifacts a closed window's rows named and no artifact holds** — the records that create
/// them, in the order they must be appended, and how many each entry is to be told it created.
///
/// **Minting is a publication, and it happens here rather than at admission.** An ordinal is claimed
/// from the level's own cursor and is durable only in the record that claims it, so a claim made at
/// admission would be held, unappended, across everything the executor does before the window
/// closes — including a `PublishArtifacts` command, which reads the same cursor and would take the
/// same ordinal. Here there is nothing to interleave with: the window is closed, the allocation is
/// made, and the record is appended a few statements later inside the window's own fsync.
///
/// **One artifact per key per level, for the whole window** (`artifacts-from-points.md` §5's second
/// ruling). The keys are gathered into one map before anything is prepared, so two points in one
/// batch — or two batches in one window — naming the same unknown key mint once and join the one
/// artifact. A key a *live* artifact already holds is not minted at all: it is re-resolved here
/// against `ArtifactStore::ordinal_of_key`, because a publication may have landed between the
/// batch's admission and this close, and it grows instead.
///
/// **A minted artifact is published carrying its members**, not published empty and then grown. The
/// entities exist by this point — the allocation is the statement above the caller — so the one
/// record says the whole of what happened, and the join needs no second record and no log pin of
/// its own. That is why `growth_records` skips a membership whose ordinal is `None`.
///
/// **The edges are created, and this is the one route by which the wire creates one.** A growth adds
/// members and never lineage, so a lineage naming an artifact that already exists can only be
/// checked; a lineage naming one that does not yet exist is settled where every edge is settled, at
/// the publication that creates the artifact. The chain arrives **parent before child** — the
/// ordering constraint `annotation-representation.md` §5.0.4 puts on edges, applied to a batch — in
/// the only two shapes a column can spell: a nested lineage is one level and one record, where
/// `prepare_publish` resolves a sibling's ordinal within its own batch whatever order the artifacts
/// sit in; and a tiered chain is a record per level, coarse first, where the parent's ordinal was
/// fixed by the record before and is answered by `pending`.
fn mint_plan<W>(
    closed: &[tessera_lifecycle::ClosedEntry<W>],
) -> Option<(MintPlan, Vec<tessera_lifecycle::BatchEdge>)> {
    use std::collections::BTreeMap;
    let mut wanted: MintPlan = BTreeMap::new();
    for (index, entry) in closed.iter().enumerate() {
        for join in &entry.memberships {
            if join.ordinal.is_some() {
                continue;
            }
            let (_, members) = wanted
                .entry((join.layer.clone(), join.level, join.key.clone()))
                // **The first entry that named the key owns the mint**, which is what makes the
                // per-batch count sum to the window's: `growth_records` blames an append the same
                // way, and one convention for both keeps a report from double-counting.
                .or_insert_with(|| (index, croaring::Bitmap::new()));
            for row in &join.rows {
                // Entity space is `u32` by I9, so the narrowing is total.
                members.add(entry.entity_ids[*row as usize].raw() as u32);
            }
        }
    }
    if wanted.is_empty() {
        return None;
    }
    let edges = closed
        .iter()
        .flat_map(|e| e.edges.iter().cloned())
        .collect();
    Some((wanted, edges))
}

/// What one window is about to mint: `(layer, level, key)` → the entry that first named it, and the
/// entities joining it.
type MintPlan = std::collections::BTreeMap<(String, u32, String), (usize, croaring::Bitmap)>;

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
    /// The waiter, or `None` for an entry nobody asked for.
    ///
    /// **`None` is the cascade** (`Executor::cascade_dependents`): deleting an artifact deletes
    /// the artifacts depending on it, and those deletions have no caller to answer. They are
    /// entries in every other respect — their own WAL record, applied in the same window, retired
    /// at the same fold — so the ack is the only thing that distinguishes them.
    respond: Option<Responder>,
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
    /// See [`MaintenanceDeps::region_cache`].
    region_cache: Arc<
        crate::single_flight::SingleFlightCache<
            crate::region::RegionKey,
            crate::region::RegionDecomposition,
        >,
    >,
    /// The artifact row forms — rebuilt here at the fold, and read by every viewport. See
    /// [`MaintenanceDeps::artifact_projections`].
    artifact_projections: Arc<crate::artifacts::ArtifactProjections>,
    /// See [`MaintenanceDeps::shapes`].
    shapes: Arc<crate::shapes::ShapeStore>,
    /// The lineages, rebuilt beside them and for the same reason.
    lineages: Arc<crate::cut::Lineages>,
    /// The supplied-content tables, held for the layer drop below. Not warmed at the fold: a
    /// table is read from the blob the fold has just rewritten, and reading every level's is a
    /// pass over the whole of it — where a row form is rebuilt there because row space renumbered
    /// under it, this one is merely stale and the first request that wants a level pays for that
    /// level alone.
    level_contents: Arc<crate::artifact_content::LevelContents>,
    queues: LifecycleQueues,
    health: Arc<ExecutorHealth>,
    /// The last window's sequence number. [`BatchState::Held`] is what it is for; all it has to be
    /// is distinct per window.
    window_seq: u64,
    /// §4's `flush_max_age_secs` — the tick's period.
    flush_max_age_secs: u64,
    /// §4.1's `flush_max_items` — buffered rows at which the tick comes due ahead of its period.
    flush_max_items: usize,
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
    /// Whether the overlay holds deny state no side-manifest carries yet.
    ///
    /// Set by any window of changes — every remaining op is a `Delete`, `Suppress` or
    /// `Unsuppress`, and each of the three moves state a manifest carries; cleared only by a
    /// successful publication. It persists across a refused publication, which is what makes a node
    /// that was poisoned or diverged publish once on its own after recovery rather than waiting for
    /// its next deny.
    deny_dirty: bool,
    /// Deny windows applied since the last publication — the counter
    /// [`OVERLAY_PUBLICATION_MAX_WINDOWS`] floors.
    windows_since_publication: u64,
    /// The bundle root. **Not the prefix directory, and that is the fourth gap of compaction §4.**
    ///
    /// A flush publishes *inside* the live prefix — never `MANIFEST.json`, never `CURRENT` — which
    /// is what separates it from a compaction, and for as long as nothing could publish a new
    /// prefix a directory captured once was the same value. A fold breaks that: the first deny
    /// published after a flip would write its side-manifest into the prefix reclamation is about
    /// to delete, which is acked deny state gone from the restore path with no error anywhere.
    /// Storing a second copy and rotating it is not the fix — it is one more thing to miss at one
    /// of eight call sites. [`Executor::prefix_dir`] derives it from the live generation instead,
    /// and a derived value cannot go stale.
    bundle_root: PathBuf,
    identity_key: IdentityKey,
    /// The shared compute pool a flush executes on (§1.1), and the handle it submits its completed
    /// unit back through.
    pool: Arc<rayon::ThreadPool>,
    /// See [`MaintenanceDeps::max_distinct_terms`].
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
    /// The entity-space coalesce's policy, its in-flight flag, its attempt counter and its own
    /// completion channel — the same three-part shape a flush has, and separate from a flush's for
    /// the reason decision 0044's D2 gives: the two halves of merge are independent work, and
    /// coupling them would make the cheap one wait on the expensive one.
    coalesce_policy: crate::coalesce::CoalescePolicy,
    coalesce_in_flight: Arc<AtomicBool>,
    coalesce_attempt: u64,
    coalesce_done: Receiver<crate::coalesce::CompletedCoalesce>,
    coalesce_submit: Sender<crate::coalesce::CompletedCoalesce>,
    /// The background refresh's dependencies — see [`crate::refresh`].
    refresh: crate::refresh::RefreshDeps,
    /// The row-space merge's policy, its in-flight flag, its attempt counter and its own
    /// completion channel. Separate from both the flush's and the coalesce's: a merge publishes as
    /// **its own swap** (decision 0044's D3 — the one-cadence rule lost its justification when pin
    /// retention was deleted, and under 0043 the coupling is harmful, since it makes a flush's
    /// zero-cost path carry the merge's refresh).
    coalesce_enabled: Arc<AtomicBool>,
    merge_policy: MergePolicy,
    merge_enabled: Arc<AtomicBool>,
    merge_in_flight: Arc<AtomicBool>,
    merge_attempt: u64,
    merge_done: Receiver<crate::merge::CompletedMerge>,
    merge_submit: Sender<crate::merge::CompletedMerge>,
    /// **The compaction fold** — the same in-flight flag, attempt counter and completion channel
    /// the other three maintenance passes have, and one thing none of them has: its own thread.
    ///
    /// A fold's input is the corpus, and `flush`, `merge` and `coalesce` all execute on the shared
    /// rayon pool a viewport's tile loop installs onto. Occupying request-serving workers for the
    /// minutes-to-hours a fold takes is the maintenance schedule leaking into the product that
    /// decision 0043 forbids, so [`Executor::dispatch_fold`] spawns a plain thread and the fold
    /// stays sequential on it (compaction §3, which also takes the memory bound sequential
    /// execution gives: there are no per-worker buffers to multiply).
    fold_in_flight: Arc<AtomicBool>,
    fold_attempt: u64,
    fold_done: Receiver<crate::compact::CompletedFold>,
    fold_submit: Sender<crate::compact::CompletedFold>,
    /// **The suggestion index's rebuild**, on the same in-flight / channel shape as the three
    /// passes above, and deliberately the *smallest* of them: it reads a vocabulary out of the
    /// generation and writes files the manifest does not name, so it has no plan, no gate and
    /// nothing to refuse. One at a time across every vocabulary, because the cost it exists to
    /// bound is the sort's memory and not its latency (`crate::suggest`).
    suggest_dir: PathBuf,
    suggest_in_flight: Arc<AtomicBool>,
    suggest_build: u64,
    suggest_done: Receiver<crate::suggest::CompletedSuggest>,
    suggest_submit: Sender<crate::suggest::CompletedSuggest>,
    /// See [`MaintenanceDeps::configured_merge_bytes`].
    configured_merge_bytes: Option<u64>,
    /// See [`MaintenanceDeps::fold_paused`].
    fold_paused: Arc<AtomicBool>,
    /// See [`MaintenanceDeps::fold_publication_paused`].
    fold_publication_paused: Arc<AtomicBool>,
    /// See [`MaintenanceDeps::merge_publication_paused`].
    merge_publication_paused: Arc<AtomicBool>,
    /// See [`crate::compact::CompactionSchedule`]. Consulted at the tick, beside the flush's own.
    compaction: crate::compact::CompactionSchedule,
    /// When the last fold attempt **started**, as a Unix timestamp — half of the operand
    /// `compaction_min_interval_secs` is measured from. See [`Executor::fold_floor_from`] for the
    /// rule and [`ExecutorHealth::fold_ended_unix`] for the other half.
    ///
    /// **Stamped by every dispatch, whatever the attempt then does**, and that is what makes the
    /// interval a rate limit rather than a success-rate limit. Several discard causes are
    /// *persistent* — the merge-size relation against a small corpus, a carried file with no digest
    /// — and a discard leaves the gauge that dispatched the fold exactly where it was. Stamped only
    /// on success, the next tick would redispatch, rewrite the whole corpus, discard again, and
    /// repeat for ever, each iteration leaving a complete prefix `CURRENT` never named and which
    /// no sweep reclaims. One bad configuration value would fill the device and take the write
    /// path down with it.
    ///
    /// Process-local, and `crate::compact::due` argues why that is harmless for the *success*
    /// case: both gauges are read against the bundle a fold itself produced.
    last_fold_start_unix: Option<u64>,
    /// Every external-id sidecar that has been **replaced** over the live prefix, weakly.
    ///
    /// **A `Weak`, and that is the whole trick.** The question reclamation has to answer is "can any
    /// live generation still resolve a path under this prefix", and a generation resolves external
    /// ids through its sidecar. A flush publishes by *cloning* the live sidecar `Arc`, so one
    /// pointer answers for every generation a flush produced — which is what
    /// [`Executor::reclaim_superseded_prefixes`] counts. **A coalesce does not**: it builds a new
    /// sidecar over the same prefix, so a generation still holding the pre-coalesce one is invisible
    /// to that count, and unlinking the tree under it turns its next external-id lookup into a typed
    /// IO error. Holding the old sidecars *strongly* would answer the question and keep their
    /// mappings alive for the prefix's whole life; a `Weak` answers it and costs a pointer.
    ///
    /// Pruned at each push, so a long-lived prefix does not accumulate dead entries, and moved into
    /// the [`PendingReclaim`] at a fold's publication — the new prefix starts with none.
    superseded_sidecars: Vec<std::sync::Weak<crate::session::ExternalIdIndex>>,
    /// Superseded prefixes awaiting reclamation, each held by the generation that named it.
    ///
    /// **The `Arc` is the wait.** Compaction §8 reclaims the old prefix whole, and lifecycle §2
    /// adds that prefix deletion waits on the requests still finishing against it. Unlinking a
    /// *mapped* file is safe on POSIX — `reclaim.rs` rests on exactly that — but the external-id
    /// sidecar opens its runs lazily, so a request holding the superseded generation could still
    /// be about to `File::open` a path under that tree. Holding the generation and reclaiming only
    /// once nothing else holds it turns that window into a wait: the pointer has already moved, so
    /// no new holder can appear and the count falls monotonically to one.
    ///
    /// **The sidecar's own count is asked too, and it is the one that reaches furthest.** The
    /// hazard is a *lazy* open: `ExternalIdSidecar` maps each run and locator extent at first touch,
    /// so a request that loaded a generation over the old prefix and has not yet resolved an
    /// external id will `File::open` a path under the deleted tree. A flush publishes by cloning
    /// the live sidecar `Arc`, so every generation a flush produced over this prefix shares one —
    /// and waiting on that `Arc` sees them all, where waiting on the fold's own superseded
    /// generation sees only itself.
    ///
    /// ⊘ **It is a narrowing, not a proof.** A coalesce publishes a *new* sidecar over the same
    /// prefix, so a generation still holding the pre-coalesce one is invisible to both counts. The
    /// residual is a request that fails with a typed IO error — never a wrong answer, since the
    /// paths simply cease to exist — and closing it properly means tracking every live generation
    /// per prefix, which nothing does today.
    ///
    /// A `Vec` rather than an `Option` because several folds may run in one process and a busy
    /// generation may outlive the next fold's snapshot. What it does **not** cover is a process
    /// that exits first: the tree then stands as an orphan until something sweeps it, which is
    /// compaction §7's startup sweep and is not built.
    /// Every membership extent this node has published, **complete current state** rather than a
    /// diff.
    ///
    /// **Held here because the manifest a publication starts from is stale.** Both publication paths
    /// clone the *live generation's* manifest, and a side-manifest write does not swap the
    /// generation — so a second publication that merely extended its clone would drop the first
    /// publication's entries, and every artifact in them would come back absent at the next open.
    /// The deny list solves the identical problem by writing complete state from the live overlay;
    /// this is that posture for a list the overlay does not hold.
    membership_extents: Vec<tessera_store::manifest::MembershipExtent>,
    /// Every containment partition the current prefix holds — one per `(layer, level)` the last
    /// fold wrote one for. **Held rather than read from the manifest**, for
    /// [`Executor::membership_extents`]' reason: a publication clones a stale manifest.
    ///
    /// **What reaches a manifest is this list filtered**, at every commit, to the entries the
    /// store's level versions still make adoptable ([`artifact_coordinates`]) — so a manifest never
    /// names a partition that has already been invalidated, whatever this list happens to hold. The
    /// fold replaces it wholesale, its paths being relative to the prefix the fold publishes.
    containment_extents: Vec<tessera_store::manifest::ContainmentExtent>,
    /// Every tile-index extent column the current prefix holds — one per `(view, layer, level)` the
    /// last fold wrote one for. Held, filtered and replaced exactly as
    /// [`Executor::containment_extents`] is, and by the same code
    /// ([`artifact_coordinates`]); the only difference is that a view is part of the address,
    /// because an extent is a pair of rows and a row space is per view.
    tile_index_extents: Vec<tessera_store::manifest::TileIndexExtent>,
    /// Every row-major column the current prefix holds — one per `(view, layer, level)` the last
    /// fold wrote one for. Held, filtered and replaced exactly as
    /// [`Executor::tile_index_extents`] is, and by the same code ([`artifact_coordinates`]); the
    /// only difference is the layout tag each entry carries, which says which form the file is in
    /// and is checked against the file's own magic at open.
    row_column_extents: Vec<tessera_store::manifest::RowColumnExtent>,
    /// Every persisted shape row form the current prefix holds — one per `(view, layer, level,
    /// segment)` the build or the last fold wrote one for. Held, filtered and replaced exactly as
    /// [`Executor::row_column_extents`] is, and by the same code ([`artifact_coordinates`]).
    shape_rows_extents: Vec<tessera_store::manifest::ShapeRowsExtent>,
    /// Every persisted decomposition file the current prefix holds, held and filtered as
    /// [`Executor::shape_rows_extents`] is.
    shape_held_extents: Vec<tessera_store::manifest::ShapeHeldExtent>,
    /// Every artifact **content** extent this node has published, complete current state, held for
    /// the reason above and written the same way. The two lists travel together: a membership
    /// without its content leaves an artifact whose description cannot be read, which withholds it.
    artifact_record_extents: Vec<tessera_store::manifest::RecordExtent>,
    pending_reclaim: Vec<PendingReclaim>,
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
    /// **What every accepted write since the last tick did to each level's row forms**, keyed by
    /// `(layer, level)` and applied at the next tick (`ingest.md` §1.3, §10 ruling 6).
    ///
    /// The deltas of one level carry consecutive level versions, which is what lets a form be
    /// brought from wherever it stands to the store's present. A level the fold rewrites has its
    /// entry dropped with its forms: those deltas describe records that no longer exist.
    pending_forms: std::collections::BTreeMap<(String, u32), Vec<crate::artifacts::LevelDelta>>,
    /// The WAL's sequence position after the last rotation (or at start), so a tick can tell
    /// whether the log has grown since — the deny-only regime's rotation trigger (owner-ruled
    /// 2026-08-04; write-path §4.5). An idle node whose position has not moved rotates nothing.
    wal_position_at_last_rotation: u64,
    /// When [`Executor::sample_wal_gauge`] last began a walk, or `None` before the first one.
    /// The rate limit on that walk is stated there; this is the clock it reads.
    last_wal_sample: Option<std::time::Instant>,
    /// Walks [`Executor::sample_wal_gauge`] has taken, published as [`WalGauge::samples`] so a
    /// reader can tell a reading that was refreshed from one the rate limit held back.
    wal_samples: u64,
    #[cfg(feature = "fault-injection")]
    faults: Option<Arc<tessera_lifecycle::faults::FaultSwitchboard>>,
}

impl Executor {
    /// Drop the region decompositions of generations older than the retention depth — the same
    /// pass, at the same swap, as `RowProjectionCache::prune_generations_below`.
    fn prune_region_cache(&self, segments_version: u64) {
        let floor = segments_version.saturating_sub(KEEP_SUPERSEDED_GENERATIONS);
        self.region_cache
            .retain_keys(|key| key.segments_version >= floor);
    }

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
        // The WAL gauge is sampled at the tick, and the first tick is a whole period away. Taken
        // once here so a node that has just restarted onto a log it replayed does not publish
        // "no members, no bytes" for that period, which reads as an empty log rather than an
        // unsampled one. This is the sample that arms the rate limit.
        self.sample_wal_gauge();
        loop {
            self.recover_wal();
            // **Completed flushes are applied before the tick plans another**, and the order is
            // load-bearing: until a flush is published its items are still in the buffer, so a tick
            // that planned first would re-plan the very rows the completed unit already wrote.
            let published = self.publish_completed_flushes()
                | self.publish_completed_coalesces()
                | self.publish_completed_merges()
                | self.publish_completed_folds()
                | self.publish_completed_suggests();
            self.tick_if_due();
            while self.run_deny_pass() {}
            // **At drain close**: one write covers a burst of consecutive windows rather than one
            // per window, which is what keeps a bulk revocation from rewriting a growing complete
            // state once per 1,000 entries. Runs on every iteration, so a node whose publication
            // was refused while poisoned publishes as soon as it recovers — the wait below is
            // tick-bounded, so that is within one tick even on an idle node.
            self.publish_overlay_state();
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
    /// **Three triggers reach this cadence and none publishes off it.** The period, the
    /// buffered-row count, and `POST /control/flush` — accepted at any time and executed here, its
    /// 202 already meaning "accepted, not yet done".
    ///
    /// **The row trigger is what bounds the commit window's cost** (write-path §4.1). Every close
    /// deep-copies the buffer, so with `B` rows buffered between publications and a close every `W`
    /// a flush interval pays `B²/2W` — and under the age tick alone `B` is the arrival rate times
    /// the period, unbounded in the rate. `flush_max_items` bounds `B` directly, which is the axis
    /// `docs/evidence/memos/2026-08-05-ingest-rate.md` measures an interior optimum on. (This is
    /// decision 0045's deleted key, restored 2026-09-04 with a consumer: the earlier one marked
    /// the buffer "flush-ready" and nothing read the mark, because the tick never skipped a
    /// non-empty buffer either.)
    ///
    /// It also drives `reclaim` — lifecycle §2.1 assigns that gap to "whichever stage introduces
    /// a periodic publisher", and this is that publisher.
    fn tick_if_due(&mut self) {
        let period = std::time::Duration::from_secs(self.flush_max_age_secs);
        // **A requested flush pulls the deadline forward; it does not publish off the cadence.**
        // `POST /control/flush` sets the flag and rings the doorbell, and the tick fires here, on
        // this one path, at the next loop iteration — so everything a tick guarantees (one flush
        // in flight, plan gates, rebase, retention) holds for an operator-triggered flush exactly
        // as for a scheduled one. The publish-on-trip hazard that killed `flush_max_items`
        // (a publication period proportional to ingest rate) does not apply: this trigger is an
        // operator action, rate-decoupled from ingest by construction.
        // **The occupancy the executor itself maintains**, not a count derived from a generation
        // this thread would have to load: `apply_window` and every flush publication store it, so
        // the trigger reads the same figure `/control/ingest`'s 429 is checked against.
        let rows_due = self.health.buffered_items.load(Ordering::SeqCst) >= self.flush_max_items;
        // **A row trip is a tick**, with the period restarted under it — not a second cadence
        // beside the period. That is what stops the two compounding: a loader fast enough to trip
        // the rows publishes on the rows and the age clock never comes due, and a loader slow
        // enough never to trip them publishes on the age exactly as before.
        let period_due = self.last_tick.elapsed() >= period;
        let due = period_due || rows_due;
        let requested = self.health.flush_requested.load(Ordering::SeqCst);
        let fold_requested = self.health.fold_requested.load(Ordering::SeqCst);
        if !due && !requested && !fold_requested {
            return;
        }
        // **The WAL's size and its rotation bound, sampled here because nothing off this thread
        // can read them.** The log is owned by the executor and a status request has no route to
        // it, so the gauge is taken at the tick and published as of that tick. Before this tick's
        // own publication, so the reading is what the tick found rather than what it left; on both
        // the flushing and the flush-skipped path, so a node whose flush is stalled still reports
        // the log growing under it. The tick is not a period — a row trip fires one every
        // `FLUSH_COMPLETION_POLL` while the buffer is full — so the walk rate-limits itself to one
        // per period and returns without doing anything on the ticks in between.
        self.sample_wal_gauge();
        // **Reclamation is checked at every tick, ahead of the flush's in-flight gate**, because it
        // is the one maintenance step whose readiness depends on nothing this executor does: it is
        // waiting on request threads to finish against a superseded generation. Skipping it on a
        // tick that found a flush running would leave a whole prefix on disc for another period
        // for no reason.
        self.reclaim_superseded_prefixes();
        // On the same argument, and ahead of the flush's in-flight gate for the same reason: it is
        // owed to residency rather than to any request, and it waits on nothing this thread does.
        self.dispatch_suggest_rebuild();
        // **The row forms of every level a write touched, published here and nowhere else**
        // (`ingest.md` §1.3, §10 ruling 6). Ahead of the flush's in-flight gate on reclamation's
        // argument: it waits on nothing this thread does, and a tick that found a flush running
        // still owes the interval's writes their publication. A request builds no form, so a level
        // whose deltas are not yet published is served as last published — up to a tick stale,
        // with counts understating and never the reverse.
        self.publish_row_forms();

        let generation = self.generation.load_full();

        // **At most one flush in flight, checked before any plan is built.** A period tick
        // arriving while one runs is *skipped, not queued* — two concurrent flushes would
        // double-consume the buffer range — and a skipped tick must not pay the plan either: a
        // plan deep-clones every buffered item, which at the buffer bound is an O(buffer-bytes)
        // allocate-and-free for a gauge (memory review, 2026-08-04). The gauge is fed from a
        // clone-free count instead, so a stalled flush still shows its backlog growing. Skips are
        // counted and alarmed, because a flush persistently slower than the tick is a
        // visibility-latency breach that `flush_max_age_secs` would otherwise silently miss.
        //
        // **A *requested* flush is not consumed by a skip.** The flag stays armed and a
        // requested-only wake returns without counting a tick, so the request executes at the
        // first iteration after the in-flight flush lands — which `FLUSH_COMPLETION_POLL` bounds
        // to within ~20 ms of its publication. Consuming it here would silently drop an
        // operator's "drain now" whenever it raced a scheduled flush.
        if self.health.flush_in_flight.load(Ordering::SeqCst) {
            if due {
                self.last_tick = std::time::Instant::now();
                self.health.mark_tick(self.last_tick);
                self.health.ticks.fetch_add(1, Ordering::Relaxed);
                let flushable = generation
                    .buffer
                    .iter()
                    .filter(|(entity, _)| !generation.overlay.is_deleted(**entity))
                    .count();
                self.health
                    .flushable_items
                    .store(flushable, Ordering::SeqCst);
                // **A skipped row trip is not the alarm the skipped period is.** The row
                // trigger asks for a publication as soon as `flush_max_items` have buffered, and
                // a loader fast enough will ask again while the last one is still writing — that
                // is the trigger doing its job under backpressure, and the buffer is bounded by
                // `ingest_buffer_max_items`'s 429 whatever happens. A missed *period* is the
                // visibility-latency breach, because it is the guarantee `flush_max_age_secs`
                // makes. Counting both here would bury the one alarm under the other's noise:
                // a 36M-row cell logs a few hundred row trips against a flush and none of them
                // is a breach.
                if flushable > 0 && period_due {
                    self.health.flush_skips.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(
                        "ALARM: a flush was still running when the next tick came due, so this \
                         tick published nothing. The effective publication period is longer \
                         than flush_max_age_secs, which is a visibility-latency breach"
                    );
                }
            }
            return;
        }

        self.last_tick = std::time::Instant::now();
        self.health.mark_tick(self.last_tick);
        self.health.ticks.fetch_add(1, Ordering::Relaxed);
        // Requested flushes are consumed by the tick whether or not there is anything to flush: a
        // `POST /control/flush` against an empty buffer is satisfied by the tick it triggered, not
        // held until something arrives.
        self.health.flush_requested.store(false, Ordering::SeqCst);

        // **Planned on this thread, executed on the pool.** The plan — which buffered items
        // acquire geometry and what the three dispositions do to them (§3.5) — is the
        // invariant-bearing half and is taken against the live generation here; the segment write
        // and the publication follow through `dispatch_flushes`. The count it produces on the way
        // is what an operator needs to see a stalled flush: items that *would* acquire geometry at
        // this tick, which stays at zero on a gated node and grows on one whose flush is failing.
        let mark = StageMark::now();
        let mut flushable = 0usize;
        let mut plans: Vec<(String, crate::flush::FlushPlan)> = Vec::new();
        for view in views_of(&generation) {
            match crate::flush::plan_flush(
                &generation,
                &view,
                self.wal.is_poisoned(),
                self.health.overlay_diverged.load(Ordering::SeqCst),
            ) {
                Ok(plan) => {
                    flushable += plan.items.len();
                    plans.push((view, plan));
                }
                Err(crate::flush::NoFlush::NothingToFlush) => {}
                Err(gate) => {
                    // Per tick, and deliberately: a gated node is gated until an operator acts, and
                    // the tick is the interval at which that is worth repeating.
                    tracing::warn!(
                        view = %view,
                        gate = ?gate,
                        "flush skipped: this node publishes no geometry in this state"
                    );
                }
            }
        }
        self.health.flush_lap(crate::flush::FlushStage::Plan, mark);
        self.health
            .flushable_items
            .store(flushable, Ordering::SeqCst);

        if plans.is_empty() {
            // Nothing to flush, so no publication is coming to rotate the log — the deny-only
            // regime. See `rotate_if_grown`.
            self.rotate_if_grown();
        } else {
            self.dispatch_flushes(&generation, plans);
        }
        // **The fold is dispatched before the two it suspends**, so a tick that starts one does not
        // also start a merge that the flip would orphan (compaction §1).
        self.dispatch_fold(&generation);
        // **The entity-space coalesce shares the tick and nothing else** (decision 0044 D2). It
        // is independent of the flush: it consumes what earlier ticks published, so a tick that
        // dispatched a flush may dispatch one too, and a gated node — which publishes no
        // geometry — still bounds the axes a coalesce owns.
        self.dispatch_coalesce(&generation);
        self.dispatch_merge(&generation);
        drop(generation);
    }

    /// Whether a fold is **outstanding**: running, or completed and not yet published.
    ///
    /// # Publication is the boundary the mutual exclusion has to use, and running was not
    ///
    /// A fold plans against a snapshot of which artefacts the live manifest lists; a merge and a
    /// coalesce change exactly that. So the three exclude one another — and the state each must
    /// exclude is not "the other is executing" but "the other's effect is not visible yet". A
    /// background job passes through three phases: running, completed and sitting undrained in its
    /// channel, and published. The `*_in_flight` flag covers only the first, and the executor's own
    /// loop straddles the second: `publish_completed_merges` runs at the top of an iteration and
    /// drains nothing because the merge is still working, and `tick_if_due` later in the *same*
    /// iteration reads a by-then-cleared `merge_in_flight` and dispatches a fold against a
    /// generation that is about to change. The next iteration publishes the merge, and the fold is
    /// left naming artefacts the live manifest no longer lists — discarded whole at its rebase
    /// check.
    ///
    /// **Every outcome of that was fail-closed, and it was still worth closing**: the cost is a
    /// discarded corpus rewrite — minutes to hours at scale — plus an orphan prefix tree nothing
    /// sweeps, compaction §7's startup sweep covering only what is present when an executor
    /// starts. It was also not rare. Measured on the merge-lands-during-fold direction: **3 of 93
    /// runs of the whole `--test fold` binary and 12 of 480 runs of the single test, ~3%**, under
    /// 3–4 concurrent lanes, with the mechanism confirmed each time (one dispatch, one orphan
    /// prefix holding only `partitions/`, the discard line, then a second dispatch). "Microseconds
    /// wide" describes the instruction window and is not the rate, because the loop and the job's
    /// completion are both driven by the tick cadence rather than being independent.
    ///
    /// The `*_completed_pending` flags this reads are the ones the completion handshake already
    /// maintains — set before the send, cleared by the drain
    /// ([`ExecutorHealth::flush_completed_pending`] states the ordering) — so the boundary needed
    /// no new state, only the right flag.
    ///
    /// # Suspended, not refused
    ///
    /// A pass that does not start here has had nothing rejected and has lost no intent, which is
    /// why every site says *suspended*. `dispatch_merge` calls `plan_merge` fresh on every tick, so
    /// a merge that does not start is simply re-decided at the next one against whatever the corpus
    /// is then; the plan does not survive the tick, and it is not meant to. That is the same
    /// principle this boundary rests on — a merge plan names specific segments, so one made before
    /// a fold and held until after would be a plan against a generation the fold is about to
    /// replace. Re-planning is what keeps a plan and the generation it executes on together.
    ///
    /// # Why this cannot wedge
    ///
    /// A suspension here lasts at most one pass. The pending flags are set only by a completing job
    /// and cleared only by `publish_completed_*`, which [`Executor::run`] calls at the top of every
    /// iteration, unconditionally and *before* `tick_if_due` — no dispatcher's suspension can
    /// suppress the drain that clears the flag it suspended on, because no dispatcher runs before
    /// it. Nothing on this path sets a pending flag, so a dispatcher cannot starve itself, and the
    /// mutual case resolves for the same reason: whichever flags are set, the next iteration's drain
    /// clears them all before any dispatch is attempted. The executor also cannot sleep through it —
    /// [`Executor::wait_for_work`] treats every pending flag as a reason for the fast completion
    /// poll rather than the full tick. The one state in which a pending flag never clears is a
    /// test's publication pause, which is `false` in a shipped build and wakes the executor when it
    /// is lifted.
    ///
    /// A suspended *requested* fold is not consumed either: the request flag stays armed and the
    /// next tick tries again, which is the treatment `dispatch_fold` already gives a fold suspended
    /// for a running merge.
    fn fold_outstanding(&self) -> bool {
        self.fold_in_flight.load(Ordering::SeqCst)
            || self.health.fold_completed_pending.load(Ordering::SeqCst)
    }

    /// Whether a merge is running, or completed and not yet published — the boundary
    /// [`Executor::fold_outstanding`] states, applied to the row-space merge.
    fn merge_outstanding(&self) -> bool {
        self.merge_in_flight.load(Ordering::SeqCst)
            || self.health.merge_completed_pending.load(Ordering::SeqCst)
    }

    /// Whether a coalesce is running, or completed and not yet published — the boundary
    /// [`Executor::fold_outstanding`] states, applied to the entity-space coalesce.
    fn coalesce_outstanding(&self) -> bool {
        self.coalesce_in_flight.load(Ordering::SeqCst)
            || self
                .health
                .coalesce_completed_pending
                .load(Ordering::SeqCst)
    }

    /// Select and dispatch an entity-space coalesce, if one qualifies and none is outstanding.
    ///
    /// **At most one outstanding, checked before the plan is built**, for the same reason a flush
    /// is: two passes would select overlapping windows and the loser's manifest edit would no
    /// longer rebase, having done all of its IO first.
    fn dispatch_coalesce(&mut self, generation: &Arc<Generation>) {
        // **Suspended until a fold is published, not merely until it stops running** (compaction
        // §1, and [`Executor::fold_outstanding`] for why the later boundary is the load-bearing
        // one): a coalesce publishing under a fold would be orphaned by the flip and would discard
        // the fold at its rebase check, so running it is waste rather than hazard. The safety argument rests on that rebase check, not on
        // this line; what this buys is that the fold is not routinely discarded by the maintenance
        // running beside it.
        if self.coalesce_policy.width < 2
            || !self.coalesce_enabled.load(Ordering::SeqCst)
            || self.coalesce_outstanding()
            || self.fold_outstanding()
            || !self.may_publish()
        {
            return;
        }
        // A poisoned or diverged node publishes no manifest at all (`publish_overlay_state`'s
        // gate, for the same reason): a coalesce's manifest carries the live deny state, and
        // writing that from an overlay no durable record backs would make a 500'd, never-acked
        // deny permanent on every restore.
        if self.wal.is_poisoned() || self.health.overlay_diverged.load(Ordering::SeqCst) {
            return;
        }
        let Some((partition, partition_data)) = generation.bundle.partitions.iter().next() else {
            return;
        };
        if partition_data.stepped_down() {
            return;
        }
        let Some(plan) = crate::coalesce::plan_coalesce(
            partition,
            &partition_data.manifest,
            &generation.bundle.manifest.files,
            self.coalesce_policy,
            // The roster as this generation has it (decision 0115): a scoped column of an
            // incarnation that is no longer live is the fold's to reclaim, not this pass's to
            // merge.
            &|view, incarnation| {
                generation
                    .bundle
                    .manifest
                    .is_live_incarnation(view, incarnation)
            },
        ) else {
            return;
        };

        self.coalesce_attempt += 1;
        let ctx = crate::coalesce::CoalesceContext {
            prefix_dir: self.prefix_dir(generation),
            prefix: generation.prefix.clone(),
            // The same never-reused shape a `seg_id` has, and for the same reason: two passes at
            // one `n` would otherwise write one path, and the second `File::create` truncates
            // files the first has memory-mapped.
            out_rel: format!(
                "partitions/{partition}/coalesced/coalesce-{}-{}",
                partition_data.segments_n, self.coalesce_attempt
            ),
        };

        self.coalesce_in_flight.store(true, Ordering::SeqCst);
        let in_flight = Arc::clone(&self.coalesce_in_flight);
        let health = Arc::clone(&self.health);
        let submit = self.coalesce_submit.clone();
        self.pool.spawn(move || {
            match crate::coalesce::execute_coalesce(plan, ctx) {
                Ok(completed) => {
                    // Set before the send, exactly as a flush's is — see
                    // `ExecutorHealth::flush_completed_pending` for the handshake's ordering.
                    health
                        .coalesce_completed_pending
                        .store(true, Ordering::SeqCst);
                    let _ = submit.send(completed);
                }
                Err(e) => {
                    // Nothing happened, retry next tick: the manifest is the only commit point,
                    // so a failure before it leaves orphan files nothing references and every
                    // consumed entry still stands.
                    health.coalesce_failures.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(
                        error = %e,
                        "an entity-space coalesce failed; the axes it would have bounded keep \
                         growing and it is retried at the next tick"
                    );
                }
            }
            in_flight.store(false, Ordering::SeqCst);
        });
    }

    /// Select and dispatch a row-space merge, if one qualifies and none is running.
    ///
    /// **Gated exactly as a coalesce is, and for the extra reason that this one moves geometry**:
    /// a poisoned or diverged node writes no manifest, and a stepped-down partition publishes
    /// nothing (`plan_merge` checks the last itself, it being bundle state rather than executor
    /// health).
    fn dispatch_merge(&mut self, generation: &Arc<Generation>) {
        // Suspended until a fold is *published*, for the reason `dispatch_coalesce` states and on
        // the boundary `fold_outstanding` states.
        if !self.merge_enabled.load(Ordering::SeqCst)
            || self.merge_outstanding()
            || self.fold_outstanding()
            || self.wal.is_poisoned()
            || !self.may_publish()
        {
            return;
        }
        let Some(plan) = crate::merge::plan_merge(generation, self.merge_policy) else {
            return;
        };
        let manifest = &generation.bundle.manifest;
        // **This view's schema, not the bundle's** — the merged segment must carry the scoped
        // render lanes its inputs carry, or the rewrite serves them as absence (`views.md` §5).
        let scalar_schema = view_scalar_schema_of(manifest, &plan.view);
        let runtime: Vec<String> = self
            .live
            .attributes_for_publication()
            .0
            .into_iter()
            .map(|d| d.name)
            .collect();
        let absent_ok = lawful_absences(&scalar_schema, scalar_schema_of(manifest).len(), &runtime);
        let Some(partition_data) = generation.bundle.partitions.get(&plan.partition) else {
            return;
        };

        self.merge_attempt += 1;
        let ctx = crate::merge::MergeContext {
            prefix_dir: self.prefix_dir(generation),
            prefix: generation.prefix.clone(),
            // The same never-reused shape a flush's `seg_id` has, and for the same reason: two
            // attempts at one `n` would otherwise write one path, and the second `File::create`
            // truncates files the first has memory-mapped.
            seg_id: format!("merge-{}-{}", partition_data.segments_n, self.merge_attempt),
            identity_key: self.identity_key,
            shard_id: manifest.identity.shard_id,
            scalar_schema,
            absent_ok,
            watermark: generation.watermark,
            entity_id_high_water: partition_data.manifest.entity_id_high_water,
        };

        self.merge_in_flight.store(true, Ordering::SeqCst);
        let in_flight = Arc::clone(&self.merge_in_flight);
        let health = Arc::clone(&self.health);
        let submit = self.merge_submit.clone();
        self.pool.spawn(move || {
            match crate::merge::execute(plan, ctx) {
                Ok(completed) => {
                    health.merge_completed_pending.store(true, Ordering::SeqCst);
                    let _ = submit.send(completed);
                }
                Err(e) => {
                    health.merge_failures.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(
                        error = %e,
                        "a merge failed; the segment count keeps growing and it is retried at the \
                         next tick"
                    );
                }
            }
            in_flight.store(false, Ordering::SeqCst);
        });
    }

    /// **Dispatch a suggestion-index rebuild** where one vocabulary's side map has run far enough
    /// ahead of its base (`crate::suggest::SuggestIndexes::most_owed_rebuild`).
    ///
    /// Off this thread and onto the pool, on the flush's own argument: the sort is measured in
    /// seconds to tens of seconds at 10⁷ values, and this thread is the one that must reach a
    /// queued deny promptly (§1.1). Nothing waits on it — a value in the side map is suggested
    /// exactly as one in the base is, so a rebuild that never finishes costs residency and no
    /// answer.
    fn dispatch_suggest_rebuild(&mut self) {
        if self.suggest_in_flight.load(Ordering::SeqCst) {
            return;
        }
        let generation = self.generation.load_full();
        let Some((vocabulary, covered_through)) = generation.suggest.most_owed_rebuild() else {
            return;
        };
        let vocabulary = vocabulary.to_string();
        let Some(minter) = generation.vocabularies.get(&vocabulary) else {
            return;
        };
        // Snapshotted here rather than read on the pool: the minter lives on the generation and the
        // next window publishes a new one, so the build must own its input.
        let values = crate::suggest::values_of(minter);
        self.suggest_build += 1;
        let build = self.suggest_build;
        let dir = self.suggest_dir.join(&vocabulary);

        self.suggest_in_flight.store(true, Ordering::SeqCst);
        let in_flight = Arc::clone(&self.suggest_in_flight);
        let submit = self.suggest_submit.clone();
        let pool = Arc::clone(&self.pool);
        self.pool.spawn(move || {
            match crate::suggest::SuggestIndex::build(&dir, build, &values, &pool) {
                Ok(index) => {
                    let _ = submit.send(crate::suggest::CompletedSuggest {
                        vocabulary,
                        index: Arc::new(index),
                        covered_through,
                    });
                }
                Err(source) => {
                    // The live index is still complete — the side map holds everything the base
                    // does not — so this costs residency and is retried at the next tick.
                    tracing::warn!(
                        %vocabulary,
                        %source,
                        "a suggestion index rebuild failed; the side map keeps the live index \
                         complete and the rebuild is retried at the next tick"
                    );
                }
            }
            in_flight.store(false, Ordering::SeqCst);
        });
    }

    /// Drop one vocabulary's suggestion index and publish — the executor half of
    /// `Engine::forget_suggestion_index_for_test`.
    ///
    /// **On this thread, which is the whole point of the hook going through the queue.** The
    /// executor is the sole publisher (lifecycle §1.3, #59), so a swap performed anywhere else can
    /// be lost to one already in flight here. It carries everything else forward and moves neither
    /// version counter, exactly as [`Self::publish_completed_suggests`] does and for the same
    /// reason: what changed is which structure a value's entries are read out of.
    #[cfg(feature = "fault-injection")]
    fn forget_suggestion_index(&mut self, vocabulary: &str) {
        let live = self.generation.load_full();
        let next = Generation {
            suggest: Arc::new(live.suggest.without(vocabulary)),
            prefix: live.prefix.clone(),
            vocabularies: Arc::clone(&live.vocabularies),
            filter_columns: Arc::clone(&live.filter_columns),
            segments_version: live.segments_version,
            watermark: live.watermark,
            bundle: Arc::clone(&live.bundle),
            dict: Arc::clone(&live.dict),
            postings: Arc::clone(&live.postings),
            fragments: Arc::clone(&live.fragments),
            external_index: Arc::clone(&live.external_index),
            delta_postings: live.delta_postings.clone(),
            overlay_version: live.overlay_version,
            overlay: Arc::clone(&live.overlay),
            buffer: Arc::clone(&live.buffer),
            denied: Arc::clone(&live.denied),
        };
        // Nothing acknowledged anything — the hook's own channel is what the caller waits on — so
        // the token is dropped here as the rebuild's is.
        let _published = self.publish(next, std::time::Instant::now());
    }

    /// Build one vocabulary's suggestion index from the live minter and publish it, inline —
    /// `ExecutorWork::RebuildSuggestionIndex`.
    ///
    /// **The dispatch's own two steps, without the threshold and without the pool**: the same
    /// `SuggestIndex::build` over the same `values_of` snapshot, submitted to the same channel and
    /// published by the same [`Self::publish_completed_suggests`], so what a test observes is the
    /// production path's result rather than a second one. Inline because the caller is blocked on
    /// it and a test that returned before the swap would race the assertion it exists to make.
    ///
    /// A build that fails publishes nothing and is not an error here: the caller's next request
    /// sees the index it already had, which is the same outcome the dispatch has.
    #[cfg(feature = "fault-injection")]
    fn rebuild_suggestion_index_now(&mut self, vocabulary: &str) {
        let generation = self.generation.load_full();
        let Some(covered_through) = generation
            .suggest
            .get(vocabulary)
            .map(|live| live.next_seq())
        else {
            return;
        };
        let Some(minter) = generation.vocabularies.get(vocabulary) else {
            return;
        };
        let values = crate::suggest::values_of(minter);
        self.suggest_build += 1;
        let dir = self.suggest_dir.join(vocabulary);
        let Ok(index) =
            crate::suggest::SuggestIndex::build(&dir, self.suggest_build, &values, &self.pool)
        else {
            return;
        };
        let _ = self.suggest_submit.send(crate::suggest::CompletedSuggest {
            vocabulary: vocabulary.to_string(),
            index: Arc::new(index),
            covered_through,
        });
        self.publish_completed_suggests();
    }

    /// Publish every finished rebuild, and report whether any did.
    ///
    /// **Its own swap, carrying everything else forward.** No geometry moved, no row is stale and
    /// no cache key rotates: what changed is which of two structures a value's entries are read out
    /// of, and both answer identically. `segments_version` and `overlay_version` therefore stand —
    /// a rebuild that bumped either would invalidate every row projection in the process for a
    /// change no request can observe.
    fn publish_completed_suggests(&mut self) -> bool {
        let mut any = false;
        while let Ok(completed) = self.suggest_done.try_recv() {
            let generation = self.generation.load_full();
            let superseded = generation
                .suggest
                .get(&completed.vocabulary)
                .map(|live| live.base().dir().to_path_buf());
            let suggest = Arc::new(generation.suggest.with_rebuilt(
                &completed.vocabulary,
                completed.index,
                completed.covered_through,
            ));
            let next = Generation {
                suggest,
                prefix: generation.prefix.clone(),
                vocabularies: Arc::clone(&generation.vocabularies),
                filter_columns: Arc::clone(&generation.filter_columns),
                segments_version: generation.segments_version,
                watermark: generation.watermark,
                bundle: Arc::clone(&generation.bundle),
                dict: Arc::clone(&generation.dict),
                postings: Arc::clone(&generation.postings),
                fragments: Arc::clone(&generation.fragments),
                external_index: Arc::clone(&generation.external_index),
                delta_postings: generation.delta_postings.clone(),
                overlay_version: generation.overlay_version,
                overlay: Arc::clone(&generation.overlay),
                buffer: Arc::clone(&generation.buffer),
                denied: Arc::clone(&generation.denied),
            };
            // Nothing acknowledged anything: a rebuild answers no caller, so the token is
            // dropped here as the coalesce's is.
            let _published = self.publish(next, std::time::Instant::now());
            // **After the swap, and unlinking a mapped file is the point.** A request still holding
            // the superseded generation keeps its pages — the mapping outlives the directory entry
            // — and a rebuild that deleted before the swap would race a walk against a file whose
            // name it had just removed.
            if let Some(dir) = superseded {
                let _ = std::fs::remove_dir_all(dir);
            }
            any = true;
        }
        any
    }

    /// Apply every completed merge waiting from the pool, and report whether any did.
    fn publish_completed_merges(&mut self) -> bool {
        // Left in the channel rather than dropped — see
        // `MaintenanceDeps::merge_publication_paused`. Always false in a shipped build.
        if self.merge_publication_paused.load(Ordering::SeqCst) {
            return false;
        }
        let mut any = false;
        while let Ok(completed) = self.merge_done.try_recv() {
            self.publish_merge(completed);
            any = true;
        }
        if any {
            self.health
                .merge_completed_pending
                .store(false, Ordering::SeqCst);
        }
        any
    }

    /// **Publish a row-space merge: its own swap, a `segments_version` bump, and a refresh
    /// armed before it** (decision 0044's D2/D3).
    ///
    /// The one publication in the write path that **permutes** row space rather than extending it.
    /// A row id inside the merged span names a different entity afterwards, so every cached
    /// projection covering that span is wrong and `RowProjection::extends_to` refuses to serve or
    /// extend it — which is why the refresh is armed before the swap and why a same-key racer in
    /// that window is shed 429 instead of paying the *measured* 1 277 ms rebuild.
    ///
    /// **Its own swap, not a rider on the next flush.** The one-cadence rule lost its stated
    /// justification when pin retention was deleted (decision 0041), and under 0043 the coupling
    /// is actively harmful: it would make the flush's zero-cost path carry the merge's refresh.
    /// The cost of the split is one extra `segments_version` bump per merge — one more refresh
    /// round, nothing a viewer observes.
    fn publish_merge(&mut self, completed: crate::merge::CompletedMerge) {
        // The seam between the merge's execution on the pool and its publication here: the merged
        // segment exists, its inputs stand, and this thread has committed to nothing — it has not
        // yet read the overlay it will re-derive the deny mask from. First statement, so a parked
        // executor holds no lock and has taken no decision a kill would tear.
        self.pause_point(PauseSiteArg::BeforeMergePublish);
        let started = std::time::Instant::now();
        // **A node whose durable state disagrees with what it is serving publishes nothing**
        // (`may_publish`). The unit's files are orphans and its inputs still stand, which is the
        // same posture every other publication failure takes.
        if !self.may_publish() {
            return;
        }
        let live = self.generation.load_full();
        if live.prefix != completed.prefix {
            tracing::warn!("discarding a completed merge planned against a superseded prefix");
            return;
        }
        let Some(partition_data) = live.bundle.partitions.get(&completed.plan.partition) else {
            return;
        };

        let mut manifest = partition_data.manifest.clone();
        if !crate::merge::rebase_into(&mut manifest, &completed) {
            // Its inputs are gone, or their runs are no longer contiguous. Expected rather than
            // exceptional, and the files are orphans nothing references.
            tracing::warn!("discarding a completed merge that no longer rebases");
            return;
        }
        write_deny_state(&mut manifest, &live.overlay);
        write_vocabulary_extensions(
            &mut manifest,
            &live.vocabularies,
            &live.bundle.manifest.vocabularies,
        );

        let manifest_n = self.allocate_manifest_n();
        // The publication seam: the merged segment's files are on disc and nothing durable names
        // them until this write returns (correctness-suite §12.3).
        self.pause_point(PauseSiteArg::BeforeManifestPublish);
        if let Err(e) = self.commit_side_manifest(
            &partition_data.manifest,
            &self.prefix_dir(&live),
            &completed.plan.partition,
            manifest_n,
            &mut manifest,
            &self.containment_extents,
            &self.tile_index_extents,
            &self.row_column_extents,
            &self.shape_rows_extents,
            &self.shape_held_extents,
            &[],
        ) {
            self.health.merge_failures.fetch_add(1, Ordering::Relaxed);
            tracing::error!(
                error = %e,
                "ALARM: a completed merge's side-manifest could not be committed; its files are \
                 orphans, every consumed segment still stands, and the next tick re-plans"
            );
            return;
        }

        let consumed: Vec<String> = completed
            .plan
            .inputs
            .iter()
            .map(|i| i.seg_id.clone())
            .collect();
        let merged_seg_id = completed.segment.seg_id.clone();
        let next_bundle = match live.bundle.with_merged(
            &completed.plan.partition,
            &completed.plan.view,
            &consumed,
            completed.segment,
            completed.output.extent,
            tessera_store::read::PublishedManifest {
                manifest,
                n: manifest_n,
            },
        ) {
            Ok(bundle) => bundle,
            Err(e) => {
                // ABA-safe by `seg_id`: an absent input is proof the inputs are gone, never a
                // pointer comparison. Discarded rather than forced — forcing would collapse a run
                // that is no longer the one the merged extent's rows were computed against.
                tracing::warn!(error = %e, "discarding a completed merge that no longer rebases");
                return;
            }
        };

        let segments_version = live.segments_version + 1;
        // **And every held row form of the view is rebased over the merged extent, before the
        // swap** — the twin of the flush's extension. The rows inside the merged span name other
        // entities now, so a form that kept its bits there would count one segment's rows as
        // another's; a form dropped instead would be projected whole by the next request naming
        // the level, which at rung 3 was 108 s and a shed request, once per merge
        // (`probes/2026-09-05-merge-arm/`). A stored level's rebase costs the members inside the
        // merged extent's entity range per artifact; a spatial level's is the merged segment
        // resolved whole against its shapes, its rows being the consumed segments' rows renumbered
        // (`polygon-membership.md` §6.3) — `ArtifactProjections::rebase_merged`.
        if let (Some(previous), Some(space)) = (
            view_row_space(
                &live.bundle,
                &completed.plan.partition,
                &completed.plan.view,
            ),
            view_row_space(
                &next_bundle,
                &completed.plan.partition,
                &completed.plan.view,
            ),
        ) {
            let merged_segment = next_bundle
                .partitions
                .get(&completed.plan.partition)
                .and_then(|p| p.views.get(&completed.plan.view))
                .and_then(|v| v.segments.iter().find(|s| s.seg_id == merged_seg_id))
                .map(|s| s.as_ref());
            self.live.with_artifacts(|store| {
                let rows_of = |layer: &str, level: u32| {
                    self.segment_rows_of(
                        &completed.plan.view,
                        layer,
                        level,
                        merged_segment,
                        &[],
                        store,
                    )
                };
                self.artifact_projections.rebase_merged(
                    &live.prefix,
                    &completed.plan.view,
                    store,
                    previous,
                    space,
                    &merged_seg_id,
                    segments_version,
                    &rows_of,
                )
            });
        }
        // **Re-derived over the new row space, because row ids changed meaning inside the span.**
        // Carrying the mask forward would leave denied rows pointing at whichever entities now
        // occupy those ids — the one way this mask can silently re-expose a deleted item.
        let denied = Arc::new(crate::compose::derive_denied(&live.overlay, &next_bundle));

        let next = Arc::new(Generation {
            prefix: live.prefix.clone(),
            vocabularies: Arc::clone(&live.vocabularies),
            // A **merge** rewrites geometry, never the filter artefact, so the columns are carried
            // forward here. The fold is the one that rebuilds them, in its own pass 4a
            // (`filter-index.md` §6.2), and it publishes through its own seam rather than through
            // this path.
            filter_columns: Arc::clone(&live.filter_columns),
            // A merge changes no value and no title, so the index it holds is still the right one.
            suggest: Arc::clone(&live.suggest),
            segments_version,
            // A merge moves neither, and both are the live values — see `MergeSpec::watermark`.
            watermark: live.watermark,
            bundle: next_bundle,
            dict: Arc::clone(&live.dict),
            postings: Arc::clone(&live.postings),
            fragments: Arc::clone(&live.fragments),
            external_index: Arc::clone(&live.external_index),
            // **The consumed segments' delta tiers stay listed**, and the entities they carry
            // still have rows — in the merged segment. Dropping one would make every item it
            // carries invisible to every session. See `crate::merge::rebase_into`.
            delta_postings: live.delta_postings.clone(),
            overlay_version: live.overlay_version,
            overlay: Arc::clone(&live.overlay),
            buffer: Arc::clone(&live.buffer),
            denied,
        });
        // The claim names the generation it is for, so a pass that is superseded mid-flight
        // releases nothing when it ends — see `refresh::clear_if_current`.
        self.refresh
            .in_flight
            .store(segments_version, Ordering::SeqCst);
        let _published = self.publish_arc(Arc::clone(&next), started);
        self.refresh.spawn(next);

        self.row_projection_cache
            .prune_generations_below(segments_version.saturating_sub(KEEP_SUPERSEDED_GENERATIONS));
        self.prune_region_cache(segments_version);
        self.health.merges.fetch_add(1, Ordering::Relaxed);
    }

    /// Whether [`crate::compact::CompactionSchedule`] calls for a fold now.
    ///
    /// Every gauge is read off the generation this tick loaded, so they agree with each other and
    /// with the plan the dispatch is about to take. **Three of the four are field reads** — a `len`
    /// per view, a bitmap cardinality, a sum of row counts — which is what lets this run at every
    /// tick rather than on a cadence of its own.
    ///
    /// **The fourth is a directory walk, and it is a closure for that reason.** `due` calls it only
    /// after every cheaper route has declined, so a deployment whose segments or deletions have
    /// already dispatched a fold never pays for it, and one with the route switched off never calls
    /// it at all. What it walks is the **live prefix**, not the bundle root: an orphaned prefix from
    /// a discarded fold is dead bytes too, but it is the startup sweep's to reclaim and not a
    /// fold's, so counting it here would dispatch folds that cannot reduce it.
    fn scheduled_fold(&self, generation: &Arc<Generation>) -> Option<crate::compact::FoldTrigger> {
        let now = unix_now()?;
        let live_rows: u64 = generation
            .bundle
            .partitions
            .values()
            .flat_map(|partition| partition.manifest.segments.iter())
            .map(|descriptor| u64::from(descriptor.row_count))
            .sum();
        crate::compact::due(
            &self.compaction,
            now,
            self.fold_floor_from(),
            crate::compact::Gauges {
                live_segments: live_segments_of(generation),
                retirable_deletions: generation.overlay.deleted_len(),
                live_rows,
            },
            || dead_bytes_of(&self.prefix_dir(generation), generation),
        )
    }

    /// The instant `compaction_min_interval_secs` is measured from, and it is neither the last
    /// fold's start nor its end alone.
    ///
    /// **The rule: a fold may start no sooner than the interval after the previous one *started*,
    /// and never before the previous one has *ended*.** Both halves are load-bearing and each
    /// breaks a different way on its own.
    ///
    /// Measuring from the **end** — which is what a naive "stamp on completion" gives — makes the
    /// floor a function of the fold's own duration, and that drifts the window off its schedule. A
    /// fold that starts at 00:10 and takes three hours ends at 03:10; tomorrow's window opens at
    /// 00:00, which is inside a 24 h floor measured from 03:10, so it folds on alternate nights and
    /// the deployment's segment count sawtooths at twice the amplitude the operator configured.
    /// That is the whole knob's purpose defeated by an accident of duration.
    ///
    /// Measuring from the **start** alone is fail-open in the other direction. A fold that runs
    /// *longer* than the interval and then discards clears the floor the instant it ends, leaving
    /// the gauge that dispatched it exactly where it was — the redispatch-for-ever loop
    /// [`Executor::last_fold_start_unix`] describes, reached by the one case a start stamp cannot
    /// see. Taking `end − interval` when that is the later origin costs a long fold one further
    /// interval of quiet and costs a short one nothing, since `end − interval` is then behind its
    /// own start.
    ///
    /// `fold_ended_unix` is the end of the fold's *passes*, written by its own thread; the
    /// publication that follows is executor work of seconds and is deliberately not counted.
    fn fold_floor_from(&self) -> Option<u64> {
        let started = self.last_fold_start_unix?;
        let ended = self.health.fold_ended_unix.load(Ordering::SeqCst);
        Some(started.max(ended.saturating_sub(self.compaction.min_interval_secs)))
    }

    /// Plan a fold and start it **on its own thread**, if one is requested and nothing blocks it.
    ///
    /// **Not on the shared pool** (compaction §3). Flush, merge and coalesce all execute on the
    /// rayon pool a viewport's tile loop installs onto, and that is right for them because
    /// `max_merged_segment_bytes` bounds what they do. A fold's input is the corpus, so occupying
    /// request-serving workers for its duration is exactly the maintenance schedule leaking into
    /// the product that decision 0043 forbids.
    ///
    /// **The request flag is consumed by a refusal but not by a suspension**, and the two words
    /// are kept apart deliberately. A gate — poisoned, diverged, stepped down — is a state an
    /// operator must act on, and re-planning into it every tick is the log flood compaction §9
    /// refuses; the request is *refused*, answered with one warning and dropped. A merge or
    /// coalesce that is *outstanding* — running, or completed and not yet published — is neither,
    /// so the fold is *suspended*: the flag stays armed and the next tick tries again once that
    /// pass lands ([`Executor::fold_outstanding`] states why nothing is lost by re-deciding).
    fn dispatch_fold(&mut self, generation: &Arc<Generation>) {
        // A fold this node could not publish is hours of IO spent to produce an orphan. The
        // planner's own gates cover the recoverable postures; this one covers the two that latch.
        if !self.may_publish() {
            return;
        }
        // **A request arriving while a fold runs is refused, not held** (compaction §9: "the
        // trigger is refused while one runs"). Consuming the flag is what makes that true — left
        // armed, it is also a *wake* reason (`tick_if_due`), so every completion poll for the
        // running fold's remaining hours would take a full tick and plan a flush off it, collapsing
        // the publication cadence to the poll interval and then dispatching a second corpus rewrite
        // the moment the first landed.
        if self.fold_in_flight.load(Ordering::SeqCst) {
            if self.health.fold_requested.swap(false, Ordering::SeqCst) {
                tracing::warn!(
                    "a compaction fold was requested while one is already running; refused rather \
                     than queued — at most one fold is in flight, and the running one will \
                     re-evaluate the gauges when it lands"
                );
            }
            return;
        }
        // **A completed fold not yet drained excludes a second one, quietly.** Unlike the refusal
        // above this consumes nothing: publication is one pass away, so a request left armed is
        // answered by the next tick rather than dropped — the treatment a fold suspended for a
        // running merge already gets.
        if self.fold_outstanding() {
            return;
        }
        // A request dispatches on its own terms — whatever hour it is and whatever the gauges read
        // — so the schedule is not consulted for one. **That is about attribution rather than
        // about whether the fold happens**: evaluating both would dispatch exactly the same fold,
        // and what it would additionally do is log a requested fold under whichever gauge happened
        // to agree, in the line an operator reads to find out why the corpus was rewritten.
        let requested = self.health.fold_requested.load(Ordering::SeqCst);
        let scheduled = if requested {
            None
        } else {
            self.scheduled_fold(generation)
        };
        if !requested && scheduled.is_none() {
            return;
        }
        // **At most one fold, and none while a merge or a coalesce is outstanding** — running, or
        // completed and not yet published ([`Executor::fold_outstanding`]). Their outputs would be
        // orphaned by the flip and their inputs are the fold's, so starting now would mean
        // re-reading the corpus to discard it at the rebase check.
        if self.merge_outstanding() || self.coalesce_outstanding() {
            return;
        }

        let plan = match crate::compact::plan_fold(
            generation,
            self.wal.is_poisoned(),
            self.health.overlay_diverged.load(Ordering::SeqCst),
            crate::compact::FoldResources {
                available_memory: available_memory(),
                free_disc: free_disc(&self.bundle_root),
                // **Read here rather than in the planner**, which is pure over the generation and
                // has no route to the resident artifact store — and this is the one term of the
                // fold's budget that cannot be derived from a manifest at all. A deployment with no
                // artifacts pays one empty iteration for it.
                membership_containers: self
                    .live
                    .with_artifacts(|store| store.membership_containers()),
            },
        ) {
            Ok(plan) => plan,
            Err(reason) => {
                self.health.fold_requested.store(false, Ordering::SeqCst);
                // Beside the log line, and on the operator plane rather than only in it: a
                // refusal moves neither `folds` nor `fold_failures`, so `/control/status` is
                // otherwise silent about the one condition a deployment cannot leave on its own
                // (see `ExecutorHealth::fold_refusals`).
                self.health.record_fold_refusal(reason);
                tracing::warn!(
                    gate = ?reason,
                    "a compaction fold was requested and refused: this node folds nothing in \
                     this state, and the request is answered rather than retried"
                );
                return;
            }
        };

        let manifest = &generation.bundle.manifest;
        // **One schema per view the fold will rewrite** — the bundle-wide render tail plus that
        // view's group-scoped render lanes. A single bundle-wide list dropped a family's lane from
        // every rewritten segment of a group's view (`views.md` §5).
        let scalar_schema: std::collections::BTreeMap<String, Vec<(String, ScalarType)>> = plan
            .views
            .iter()
            .map(|view| {
                (
                    view.view.clone(),
                    view_scalar_schema_of(manifest, &view.view),
                )
            })
            .collect();
        let Some(partition_data) = generation.bundle.partitions.get(&plan.partition) else {
            return;
        };
        // The columns declared at a running service and not yet folded, by name: the fold writes
        // each a base from its extents alone, and the publication takes them off the runtime list
        // (`ingest.md` §6.3). Taken from the live list at the plan, so a declaration made while
        // the fold runs is not among them.
        let (runtime_attributes, runtime_scoped_attributes) = {
            let (entity, scoped) = self.live.attributes_for_publication();
            (
                entity.into_iter().map(|d| d.name).collect::<Vec<_>>(),
                scoped.into_iter().map(|f| f.name).collect::<Vec<_>>(),
            )
        };
        // Per view, the columns an input segment may lawfully lack: the runtime columns above and
        // the view's group-scoped lanes. Any other missing column fails the fold as a torn segment.
        // Read from `runtime_attributes` rather than from a second snapshot of the live list: a
        // declaration landing between two reads would put a column on one list and not the other,
        // and the fold would then either refuse a lawful absence or accept a torn one.
        let absent_ok: std::collections::BTreeMap<String, Vec<String>> = {
            let entity_scoped = scalar_schema_of(manifest).len();
            scalar_schema
                .iter()
                .map(|(view, schema)| {
                    (
                        view.clone(),
                        lawful_absences(schema, entity_scoped, &runtime_attributes),
                    )
                })
                .collect()
        };
        let to_prefix = match crate::compact::next_prefix_name(&self.bundle_root) {
            Ok(prefix) => prefix,
            Err(e) => {
                self.health.fold_requested.store(false, Ordering::SeqCst);
                self.health.fold_failures.fetch_add(1, Ordering::Relaxed);
                tracing::error!(error = %e, "could not name a new prefix for the fold");
                return;
            }
        };

        self.fold_attempt += 1;
        let ctx = crate::compact::FoldContext {
            from_prefix_dir: self.prefix_dir(generation),
            to_prefix_dir: self.bundle_root.join(&to_prefix),
            to_prefix: to_prefix.clone(),
            identity_key: self.identity_key,
            shard_id: manifest.identity.shard_id,
            scalar_schema,
            absent_ok,
            runtime_attributes,
            runtime_scoped_attributes,
            // The same never-reused shape a flush's and a merge's `seg_id` have (contracts §2.1).
            // A fold writes into a fresh prefix, so nothing can collide today; the id is still
            // unique because a `seg_id` naming two different segments across a bundle's life is
            // what makes a rebase's ABA check meaningless.
            seg_id: format!("fold-{}-{}", partition_data.segments_n, self.fold_attempt),
            base_postings: Arc::clone(&generation.postings),
            tiers: generation.delta_postings.clone(),
            declared_scalars: manifest.declared_scalars.clone(),
            // The scoped families, flattened: the attribute pass folds one column per view of
            // each, and a fold that omitted them wrote a prefix their directories are absent
            // from (`views.md` §5).
            scoped_scalars: manifest.scoped_scalars(),
            // The roster those families' view ids are placed by (decision 0115): a scoped column's
            // directory carries the incarnation above the build's, so the fold has to write it
            // where the opener will look.
            view_incarnations: manifest
                .views
                .iter()
                .map(|v| (v.id.clone(), v.incarnation))
                .collect(),
            vocabularies: manifest.vocabularies.clone(),
        };

        if let Some(trigger) = scheduled {
            // **Every gauge, not only the one that fired.** A fold is minutes to hours, and an
            // operator reading why one started needs to see the state that produced it rather than
            // the single number that crossed first — the dead-bytes pair especially, since it is
            // the one figure `/control/status` does not carry.
            let dead = dead_bytes_of(&self.prefix_dir(generation), generation);
            tracing::info!(
                trigger = ?trigger,
                live_segments = live_segments_of(generation),
                retirable_deletions = generation.overlay.deleted_len(),
                on_disc_bytes = dead.map(|d| d.on_disc),
                named_bytes = dead.map(|d| d.named),
                "dispatching a scheduled compaction fold"
            );
        }
        self.health.fold_requested.store(false, Ordering::SeqCst);
        self.fold_in_flight.store(true, Ordering::SeqCst);
        let in_flight = Arc::clone(&self.fold_in_flight);
        let health = Arc::clone(&self.health);
        let submit = self.fold_submit.clone();
        let paused = Arc::clone(&self.fold_paused);
        let spawned = std::thread::Builder::new()
            .name("tessera-fold".to_string())
            .spawn(move || {
                match crate::compact::execute(plan, ctx) {
                    Ok(mut completed) => {
                        // A test holding the fold here models the flight a real corpus gives for
                        // free — see `Engine::set_fold_paused_for_test`. Always false otherwise.
                        health.fold_holding.store(true, Ordering::SeqCst);
                        while paused.load(Ordering::SeqCst) {
                            std::thread::sleep(std::time::Duration::from_millis(5));
                        }
                        health.fold_holding.store(false, Ordering::SeqCst);
                        // After the hold, so the staircase's hand-off row is the hand-off.
                        completed.finished = std::time::Instant::now();
                        // Set before the send, exactly as a flush's is — see
                        // `ExecutorHealth::flush_completed_pending` for the handshake's ordering.
                        health.fold_completed_pending.store(true, Ordering::SeqCst);
                        let _ = submit.send(completed);
                    }
                    Err(e) => {
                        health.fold_failures.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!(
                            error = %e,
                            "a compaction fold failed; its files are orphans under a prefix \
                             CURRENT does not name, nothing retired, and the next trigger \
                             re-plans from scratch"
                        );
                    }
                }
                // **The attempt's end, recorded on the thread because one of its two exits never
                // reaches the executor.** See `fold_floor_from`. Written before the in-flight flag
                // clears, so a tick that observes the fold finished also observes when.
                health
                    .fold_ended_unix
                    .store(unix_now().unwrap_or(0), Ordering::SeqCst);
                in_flight.store(false, Ordering::SeqCst);
            });
        if spawned.is_ok() {
            // This attempt's start — see `fold_floor_from`.
            self.last_fold_start_unix = unix_now();
        }
        if let Err(e) = spawned {
            // The closure — and with it the in-flight clone — was dropped, so the flag is cleared
            // through the field rather than through the copy that never ran.
            self.fold_in_flight.store(false, Ordering::SeqCst);
            self.health.fold_failures.fetch_add(1, Ordering::Relaxed);
            tracing::error!(error = %e, "the OS refused a thread for the compaction fold");
        }
    }

    /// Apply every completed fold waiting from its thread, and report whether any did.
    fn publish_completed_folds(&mut self) -> bool {
        // Left in the channel rather than dropped — see `MaintenanceDeps::fold_publication_paused`.
        // Always false in a shipped build.
        if self.fold_publication_paused.load(Ordering::SeqCst) {
            return false;
        }
        let mut any = false;
        while let Ok(completed) = self.fold_done.try_recv() {
            self.publish_fold(completed);
            any = true;
        }
        if any {
            self.health
                .fold_completed_pending
                .store(false, Ordering::SeqCst);
        }
        any
    }

    /// **Publish a fold: compaction §4's seven steps, in order, and the reclamation §8 owes.**
    ///
    /// Rebase or discard → assemble `SEGMENTS-<n>` from the live partition manifest → check the
    /// merge-size relation against the fold's own output → hard-link the carry-forwards, write
    /// `MANIFEST.json`, write `SEGMENTS-<n>.json`, flip `CURRENT` → open the new prefix in-process
    /// → one swap → rotate the WAL → reclaim the old prefix.
    ///
    /// # Everything about live state is decided here, and nothing about it was decided at the plan
    ///
    /// The plan named files and cloned `D₀`. What the fold *carries forward* — post-snapshot
    /// segments, tiers, runs, locator extents — is whatever the live manifest still holds that the
    /// plan did not consume, and it is read here, hours later. `tombstones` is the one arithmetic
    /// that must be a set difference against live state: a delete accepted during the fold's flight
    /// names an entity whose row the fold did *not* drop, so publishing the executed set (or the
    /// plan's) would retire that deletion while its row survives in the rebuilt base.
    ///
    /// # `CURRENT` is the commit point, and everything before it is reversible
    ///
    /// A failure at any step up to the flip discards the fold: its files are orphans under a prefix
    /// nothing names, every consumed artefact still stands, and the next trigger re-plans. A
    /// failure *after* the flip is a different thing and is alarmed as one — the bundle on disc is
    /// the new one and a restart opens it, while this process goes on serving the old geometry it
    /// still holds mapped. That is compaction §7's "crash between `CURRENT` and the swap", reached
    /// without a crash.
    fn publish_fold(&mut self, completed: crate::compact::CompletedFold) {
        use std::collections::{BTreeMap, BTreeSet};

        let started = std::time::Instant::now();
        // The fold thread's staircase, continued here for the publication's phases so the gauges
        // on `/control/status` cover the whole fold (`compact::Staircase`). A discard below drops
        // it with the fold.
        let mut stairs =
            crate::compact::Staircase::resume(completed.cost.clone(), completed.finished);
        let live = self.generation.load_full();
        let plan = &completed.plan;
        // **A node whose durable state disagrees with what it is serving publishes nothing**
        // (`may_publish`). The unit's files are orphans and its inputs still stand, which is the
        // same posture every other publication failure takes.
        if !self.may_publish() {
            return;
        }
        // Every discard below is the same posture — nothing happened, the files are orphans, the
        // next trigger re-plans — so it is one closure rather than a shape repeated eleven times.
        // It owns what it reports so that it borrows nothing from `self` or from `completed`, both
        // of which the sequence below still needs.
        let discard = {
            let health = Arc::clone(&self.health);
            let prefix = completed.prefix.clone();
            move |reason: &str| {
                health.fold_failures.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    prefix = %prefix,
                    "discarding a completed fold: {reason}. Its files are orphans under a prefix \
                     CURRENT does not name, every consumed artefact still stands, and nothing \
                     retired"
                );
            }
        };

        // **The durability gates are asked again here, hours after the plan asked them.** A fold's
        // flight is the widest window in the write path, and what it publishes at the end of it is
        // a side-manifest carrying `tombstones` and `deny` serialised from the live overlay. If the
        // WAL poisoned or the overlay diverged meanwhile, that overlay holds dispositions no
        // durable record backs — the apply-anyway entries of write-path §5.5, in force behind a
        // 500 and never acked — and writing them into a manifest makes them permanent on every
        // restore, which is the outcome the gate exists to prevent. `plan_fold` refuses for exactly
        // this reason; every other manifest writer re-asks at its own dispatch, and only this one
        // had a window long enough for the answer to change.
        //
        // The divergence half is already asked by `may_publish` above; this is the WAL's own.
        if self.wal.is_poisoned() {
            discard(
                "the WAL poisoned during its flight, so its manifest would publish deny state \
                     no durable record backs",
            );
            return;
        }

        if live.prefix != plan.prefix {
            discard("it was planned against a superseded prefix");
            return;
        }
        let Some(partition_data) = live.bundle.partitions.get(&plan.partition) else {
            discard("the live bundle no longer carries the partition it folded");
            return;
        };
        let live_manifest = &partition_data.manifest;

        // ---- step 1: rebase or discard --------------------------------------------------------
        //
        // ABA-safe because ids are never reused (contracts §2.1), so an artefact still listed is
        // the same artefact the fold consumed. A merge or a coalesce that published under the fold
        // fails this.
        //
        // **Unreachable by construction while the suspension holds, and kept anyway.** No merge or
        // coalesce can publish under a fold at all: `dispatch_merge` and `dispatch_coalesce` are
        // the only routes to either, both run from the tick, and both consult
        // `Executor::fold_outstanding`, which covers the fold from dispatch through publication.
        // What would make this reachable again is a dispatcher that stopped consulting those
        // predicates, or a second route to a merge — a control-plane trigger, a second writer. The
        // check costs one set comparison against a manifest already in hand, on a path that has
        // just spent hours of IO, and its failure mode is fail-closed where forcing would drop
        // every row the merge wrote; so it stays as defence in depth rather than as a path with a
        // known rate.
        let consumed_segments: FxHashSet<(&str, &str)> = plan
            .views
            .iter()
            .flat_map(|view| {
                view.segments
                    .iter()
                    .map(move |segment| (view.view.as_str(), segment.seg_id.as_str()))
            })
            .collect();
        let listed_segments: FxHashSet<(&str, &str)> = live_manifest
            .segments
            .iter()
            .map(|d| (d.view.as_str(), d.seg_id.as_str()))
            .collect();
        if !consumed_segments.is_subset(&listed_segments)
            || !plan.tiers.iter().all(|t| live_manifest.deltas.contains(t))
            || !plan
                .runs
                .iter()
                .all(|r| live_manifest.external_id_runs.contains(r))
            || !plan.locator_extents.iter().all(|path| {
                live_manifest
                    .locator_extents
                    .iter()
                    .any(|extent| &extent.path == path)
            })
            || !plan.attr_extents.iter().all(|consumed| {
                live_manifest
                    .attr_extents
                    .iter()
                    .any(|extent| extent.values == consumed.values)
            })
            || !plan.record_extents.iter().all(|consumed| {
                live_manifest
                    .record_extents
                    .iter()
                    .any(|extent| extent.blocks == consumed.blocks)
            })
            || !plan.text_extents.iter().all(|consumed| {
                live_manifest
                    .text_extents
                    .iter()
                    .any(|extent| extent.dict == consumed.dict)
            })
            || !plan.entity_terms_extents.iter().all(|consumed| {
                live_manifest
                    .entity_terms_extents
                    .iter()
                    .any(|extent| extent.terms == consumed.terms)
            })
        {
            discard("an artefact it consumed is no longer listed in the live manifest");
            return;
        }

        // ---- the carry-forward set ------------------------------------------------------------
        //
        // Listed order is preserved in every one of these: for segments it is entity order, which
        // `RowSpace::with_extent` requires; for runs it is recency, which decision 0047's
        // newest-first resolution reads.
        //
        // **A dead incarnation's segments are carried by nothing** (`views.md` §3.4, decision
        // 0115): the drop retains the view out of the bundle, so the plan has no base for it and
        // never consumed its segments — and carrying them would name an incarnation the new
        // manifest does not declare. This filter is the whole of "reclamation by omission": the
        // descriptors are left behind with the superseded prefix's files, which the reclaim then
        // deletes. Without it every fold after a drop of a view that held rows is *discarded* by
        // the base check below, so compaction stops for the life of the bundle and nothing ever
        // retires.
        //
        // **`(view, incarnation)`, not the view alone.** A dropped key may be created again
        // (decision 0115), and the recreated view is declared under the same id — so "is this
        // view still declared" stopped being the question the moment the burn was withdrawn. A
        // segment stamped with the dead incarnation would otherwise be carried into the fold's
        // output and the new view would serve the predecessor's points.
        //
        // **The `MANIFEST.json` roster decides, not the partition's view map.** Both answer the
        // same question — the map is retained against the same roster at `Bundle::with_views` —
        // but only one of them is that question: the map is a cache of open row spaces, and a
        // later change to how it is built would move this predicate without anyone reading this
        // line. The roster read here is the same snapshot the new manifest is written from, so
        // what is carried and what is declared cannot disagree.
        let live_incarnations: FxHashMap<&str, tessera_types::view::ViewIncarnation> = live
            .bundle
            .manifest
            .views
            .iter()
            .map(|v| (v.id.as_str(), v.incarnation))
            .collect();
        // **Owned, because the log that reports them outlives this borrow**: the generation is
        // moved into `pending_reclaim` at step 8, a few lines before the publication is logged.
        let mut omitted_views: Vec<String> = Vec::new();
        let mut omitted_segments = 0usize;
        let carried_segments: Vec<&tessera_store::manifest::SegmentDescriptor> = live_manifest
            .segments
            .iter()
            .filter(|d| !consumed_segments.contains(&(d.view.as_str(), d.seg_id.as_str())))
            .filter(|d| {
                let live = live_incarnations.get(d.view.as_str()) == Some(&d.incarnation);
                if !live {
                    omitted_segments += 1;
                    if !omitted_views.iter().any(|v| v == &d.view) {
                        omitted_views.push(d.view.clone());
                    }
                }
                live
            })
            .collect();
        let carried_tiers: Vec<String> = live_manifest
            .deltas
            .iter()
            .filter(|t| !plan.tiers.contains(t))
            .cloned()
            .collect();
        let carried_runs: Vec<String> = live_manifest
            .external_id_runs
            .iter()
            .filter(|r| !plan.runs.contains(r))
            .cloned()
            .collect();
        let carried_locators: Vec<tessera_store::manifest::LocatorExtent> = live_manifest
            .locator_extents
            .iter()
            .filter(|extent| !plan.locator_extents.contains(&extent.path))
            .cloned()
            .collect();
        // **The flight's attribute extents, and the pass consumed every other one** (filter-index
        // §6.2). Selected by the values path because that is what the plan consumed and what the
        // fold read; the column name is not an identity here, since a column has many extents.
        //
        // Listed order is preserved for the reason it is everywhere else in this set: composition
        // unions the layers, so their order is immaterial to the answer — but a manifest whose
        // bytes depend on a set iteration order is a bundle identity that depends on one.
        let consumed_attrs: FxHashSet<&str> = plan
            .attr_extents
            .iter()
            .map(|extent| extent.values.as_str())
            .collect();
        let carried_attrs: Vec<tessera_store::manifest::AttrExtent> = live_manifest
            .attr_extents
            .iter()
            .filter(|extent| !consumed_attrs.contains(extent.values.as_str()))
            // **And nothing of a dead incarnation** (decision 0115), on the segment filter's
            // argument: a group-scoped column's extents outlive the drop that orphaned them, and
            // a key created again writes its own column under the same family name.
            .filter(|extent| {
                carries_live_view(
                    &live_incarnations,
                    extent.view.as_deref(),
                    extent.incarnation,
                )
            })
            .cloned()
            .collect();
        // The record-blob extents take the attribute extents' shape exactly: the fold consumed
        // every one its snapshot named and folded the rows into the new base blob; what is carried
        // is the flight's — a flush publishing during the fold appended entities the new base does
        // not hold, and dropping its entry would answer their drill-downs "no record" with no
        // symptom. Identified by the blocks path, `seg_id`-derived and never reused.
        let consumed_records: FxHashSet<&str> = plan
            .record_extents
            .iter()
            .map(|extent| extent.blocks.as_str())
            .collect();
        let carried_records: Vec<tessera_store::manifest::RecordExtent> = live_manifest
            .record_extents
            .iter()
            .filter(|extent| !consumed_records.contains(extent.blocks.as_str()))
            .cloned()
            .collect();
        // The text extents, same shape again: the fold merged every one its snapshot named into
        // the new base index, and what is carried is the flight's. Identified by the dictionary
        // path, which is `seg_id`-derived and never reused — and which is also the half a reader
        // cannot substitute, an extent's postings being positions in *its own* dictionary.
        let consumed_texts: FxHashSet<&str> = plan
            .text_extents
            .iter()
            .map(|extent| extent.dict.as_str())
            .collect();
        // The transpose's extents, the same shape a third time: pass 4c folded every one its
        // snapshot named into the new base, and what is carried is the flight's. Identified by the
        // terms path, `seg_id`-derived and never reused. Dropping a flight entry would leave the
        // entities that flush minted with *unknown* labels — a drill-down without them and, on the
        // write path, a join rule with nothing to compare against.
        let consumed_entity_terms: FxHashSet<&str> = plan
            .entity_terms_extents
            .iter()
            .map(|extent| extent.terms.as_str())
            .collect();
        let carried_entity_terms: Vec<tessera_store::manifest::EntityTermsExtent> = live_manifest
            .entity_terms_extents
            .iter()
            .filter(|extent| !consumed_entity_terms.contains(extent.terms.as_str()))
            .cloned()
            .collect();
        let carried_texts: Vec<tessera_store::manifest::TextExtent> = live_manifest
            .text_extents
            .iter()
            .filter(|extent| !consumed_texts.contains(extent.dict.as_str()))
            .filter(|extent| {
                carries_live_view(
                    &live_incarnations,
                    extent.view.as_deref(),
                    extent.incarnation,
                )
            })
            .cloned()
            .collect();

        // **Every carried-forward extent must begin at or above the fold's own base**, per view.
        // `RowSpace::with_extent` refuses an extent below the base permutation's bound and
        // `ExternalIdSidecar` gives the base locator absolute priority below its own length — so a
        // violation here is a prefix that either will not open or answers "this item has no
        // external id" for items that have one (contracts §2.4's wrong-answer-wearing-a-legitimate-
        // state's-clothes). Both would be discovered *after* `CURRENT` had flipped, so they are
        // checked before anything is written. The relation holds for every publication that
        // cleared the live row space's own floor; this refuses to be the place it is assumed.
        for descriptor in &carried_segments {
            let Some(view) = plan.views.iter().find(|s| s.view == descriptor.view) else {
                // **A view that arrived during the flight**, and the only way to reach this now:
                // a view created and flushed since the plan was taken has a segment and no base
                // in it. That is transient and self-healing — the next fold plans over a bundle
                // that holds the view, and nothing is lost meanwhile but this fold's work — which
                // is why it is said here rather than left under the sentence below. The other
                // reader of this line was a **dropped** view, whose segments the carry-forward
                // now omits (`views.md` §3.4), and that one did not self-heal: it discarded every
                // fold of the bundle for ever.
                discard(
                    "a carried-forward segment names a view created since the plan was taken, so \
                     the fold has no base for it; the next fold plans over a bundle that has it",
                );
                return;
            };
            if descriptor.entity_lo < view.permutation_bound {
                discard("a carried-forward segment begins below the fold's own base permutation");
                return;
            }
        }
        // **Partition-wide here where the segment loop above is per-view, and that is correct
        // rather than a coarsening.** `ext-locator.u32` is one array per partition (§3, pass 3), so
        // there is no per-view bound to compare against — but the reason it cannot falsely fire is
        // the allocator, not the file: entity ids are issued monotonically from **one** bundle-wide
        // high-water (I9), so a locator extent published after the fold's snapshot begins above
        // every entity that had a row at it, in every view. `plan.entity_bound` is the maximum of
        // those per-view bounds and is therefore at or below that high-water. A view whose own
        // bound is lower cannot produce an extent beneath the maximum, because it does not get to
        // choose its ids.
        if carried_locators
            .iter()
            .any(|extent| extent.entity_lo < plan.entity_bound)
        {
            discard("a carried-forward locator extent begins below the fold's own base locator");
            return;
        }
        // The fold emits no run 0 for a deployment that held no external ids at its snapshot
        // (contracts §2.4 r6: no runs, no locator). If one arrived during the flight, a
        // carried-forward flush run would become `external_id_runs[0]` — and the sidecar derives
        // the *base locator's* path from that entry's directory, so it would take a flush's
        // entity-range extent for the full-length base locator. Discarded rather than published;
        // the next fold's snapshot holds the run and emits a proper base for it.
        if completed.external_id_run.is_none() && !carried_runs.is_empty() {
            discard("the deployment gained its first external-id run during the fold's flight");
            return;
        }

        // ---- retirement: compaction §5, evaluated here and nowhere earlier ---------------------
        let mut carried = crate::compact::CarriedForward::new();
        for descriptor in &carried_segments {
            carried.add_segment(descriptor);
        }
        for extent in &carried_locators {
            carried.add_locator_extent(extent);
        }
        let executed = crate::compact::executed(&plan.tombstones, &carried);
        let retired_count = executed.cardinality();
        // Allocated here rather than beside the manifest write, so the artifact pass below can name
        // its files after the publication that introduces them — one sequence, not two.
        let manifest_n = self.allocate_manifest_n();

        // ---- step 3: the merge-size relation, against the fold's own output --------------------
        //
        // `max_merged_segment_bytes` must stay strictly below the base segment's bytes or the
        // *next startup* refuses the configuration (write-path §7). A fold normally satisfies it
        // more comfortably — it folds every extent into the base — and the case to catch is the
        // small corpus where it does not. Refused here, loudly, rather than at the restart that
        // discovers it.
        if let Some(configured) = self.configured_merge_bytes {
            if completed.base_segment_bytes > 0 && configured >= completed.base_segment_bytes {
                self.health.fold_failures.fetch_add(1, Ordering::Relaxed);
                tracing::error!(
                    max_merged_segment_bytes = configured,
                    base_segment_bytes = completed.base_segment_bytes,
                    "ALARM: publishing this fold would leave a deployment the next startup \
                     refuses to open — merge.max_merged_segment_bytes is not strictly below the \
                     folded base segment's size. The fold is discarded and the configuration \
                     needs lowering before the next one"
                );
                return;
            }
        }

        // ---- step 3a: the artifact pass --------------------------------------------------------
        //
        // **A fold publishes a new prefix, and membership extent paths are prefix-relative.** So
        // there are three things this could do with them and two are wrong: carrying the paths
        // forward names files the new prefix does not contain, and the bundle refuses at the next
        // open; dropping them loses every membership silently, and the artifacts come back
        // registered, still addressable, and served as absent.
        //
        // The third — and this is `annotation-representation.md` §5.0.3's artifact pass — is to
        // write them again, into the prefix being published, from the resident entity-space store.
        // **Entity space is what makes that a rewrite rather than a translation**: entity ids do not
        // move at a fold, only rows do, so the durable form needs no remapping and the derived row
        // form is rebuilt from it afterwards.
        //
        // It is still not a copy. The fold retires entities, and a membership carried forward
        // unchanged goes on counting members that no longer exist — in the size the proportional
        // existence criterion divides by. `repack_all` drops exactly the executed deletions and
        // nothing else: a suppressed member keeps its bit (Rule S), and no generating set is
        // touched at all.
        let to_prefix_dir = self.bundle_root.join(&completed.prefix);
        stairs.record("6 hand-off");
        let repacked = match self.rewrite_membership_extents(
            &to_prefix_dir,
            &plan.partition,
            manifest_n,
            &executed,
        ) {
            Ok(repacked) => repacked,
            Err(e) => {
                discard(&format!(
                    "its artifact memberships would not be rewritten ({e})"
                ));
                return;
            }
        };

        stairs.record("7 memberships");

        // **The containment partitions, in the same pass and against the prefix just written.**
        // They are derived, so an empty list is a cost and not a fault — see
        // `write_containment_partitions`.
        // **The levels this fold is about to change and has not yet** ([`PendingRetirement`]).
        // `repack_all` above wrote the post-retirement records into the prefix while the store
        // still holds the pre-retirement ones. A partition composed from the store would describe
        // a level the prefix does not contain: under `WithdrawContent` a content is dropped whole,
        // which shifts the ranks, and a partition read at the wrong rank is a containment answer
        // for another content's generating set. Those levels get no partition and recompose on
        // first use. Their row columns and tile indexes are written, from the records the
        // retirement will leave and at the version it will leave the level at: on rung 3 a fold
        // that retired members of `mesh/descriptors` and wrote neither cost the warm 135 s and
        // 3.7 GB of resident memory projecting the level's 1.66×10⁹ memberships, against 16 s to
        // transpose the column (`probes/2026-09-04-epoch-shard-fold-decomposition/`).
        let pending = PendingRetirement {
            levels: self
                .live
                .with_artifacts(|store| store.levels_moved_by(&executed)),
            retired: executed.clone(),
        };
        // **One counter for the whole publication.** Every derived file this fold writes is named
        // from it, so no two of these calls can name the same file — see
        // `tessera_store::derived::DerivedIndex`.
        let mut derived_index = tessera_store::derived::DerivedIndex::default();
        let containment = self.write_containment_partitions(
            &to_prefix_dir,
            &plan.partition,
            manifest_n,
            &live.bundle.manifest.data_plugin_hash,
            &pending,
            &mut derived_index,
        );
        // **The row spaces every derived structure below is computed over**, opened once: base
        // only, against the permutations this fold just wrote.
        let index_views: Vec<(String, u32)> = completed
            .segments
            .iter()
            .map(|segment| (segment.view.clone(), segment.row_count))
            .collect();
        let spaces = self.fold_row_spaces(&to_prefix_dir, &plan.partition, &index_views);
        // **The layout re-evaluation, here and not later** (selection memo §5). The fold writes the
        // membership files first and snapshots the registry after, so a choice taken after the
        // snapshot would reach neither the files nor the manifest — and the fold would publish a
        // level in the old layout with a record claiming the new one. Taken before either.
        //
        // **A flip drops the level's held forms explicitly.** The cached row form is
        // replace-on-mismatch and this fold moves the prefix, so it would go anyway; the *held*
        // tile index and column would not, because a level that flipped is never asked for its old
        // form again and nothing would ever claim the entry.
        let fold_segments = fold_segments(&to_prefix_dir, &plan.partition, &completed.segments);
        // **Which incarnation each planned view is** (decision 0115), so every derived structure
        // this pass writes is stamped with the one whose row space it was written over. Taken
        // from the plan rather than from the live manifest: the plan is what the row spaces above
        // came from, and a view created since it was taken has no space here to describe.
        let fold_incarnations: FxHashMap<String, tessera_types::view::ViewIncarnation> = plan
            .views
            .iter()
            .map(|view| (view.view.clone(), view.incarnation))
            .collect();
        let layouts = self.choose_layouts(&spaces, &pending, &fold_segments);
        for (layer, level, chosen) in &layouts {
            if self.live.record_layout(layer, *level, *chosen) {
                self.artifact_projections.forget_level(layer, *level);
            }
        }
        // **The fold rewrites every level, so every delta held for the tick describes a form that
        // is going.** The forms themselves go on the prefix move; the store holds what the deltas
        // said, and the new prefix's forms are built from it.
        self.pending_forms.clear();
        // **The tile indexes, in the same pass and omitting the same levels** — and omitting the
        // levels now recorded row-major, which have nothing to index. Their extents are rows, so
        // they are per view and are projected against the base permutation this fold just wrote.
        let tile_indexes = self.write_tile_indexes(
            &to_prefix_dir,
            &plan.partition,
            manifest_n,
            &fold_incarnations,
            &spaces,
            &layouts,
            &pending,
            &mut derived_index,
        );
        // **And the columns for the levels that do**, in the same pass and under the same
        // omissions. A level whose column will not compose gets no entry, and is served
        // artifact-major.
        let row_columns = self.write_row_columns(
            &to_prefix_dir,
            &plan.partition,
            manifest_n,
            &fold_incarnations,
            &spaces,
            &layouts,
            &pending,
            &fold_segments,
            &mut derived_index,
        );
        // **And the row forms of the spatial levels the columns do not cover**, so the next open
        // claims what this fold just resolved instead of resolving it again
        // (`polygon-membership.md` §6.3; owner ruling 2026-08-29).
        let shape_rows = self.write_shape_rows(
            &to_prefix_dir,
            &plan.partition,
            manifest_n,
            &fold_incarnations,
            &row_columns,
            &pending,
            &fold_segments,
            &mut derived_index,
        );
        let shape_held = self.write_shape_held(
            &to_prefix_dir,
            &plan.partition,
            manifest_n,
            &fold_incarnations,
            &pending,
            &fold_segments,
            &mut derived_index,
        );
        stairs.record("8 derived");

        // ---- step 3b: the report, before anything retires ---------------------------------------
        //
        // **A deletion is not retired before the caller has been told what it degraded**
        // (write-path §5.8). This fold is about to retire the overlay entries for `executed`, so the
        // report of what those deletions took away from the artifacts that held them is written
        // first — and a report that cannot be written **discards the fold**, which is the whole
        // content of "retirement and report in one publication" (write cycle §7). Nothing is lost by
        // discarding: the deletions are already in force, and the next fold reports them.
        //
        // The sweep is one `and_cardinality` per artifact against the set the fold already holds.
        // The sweep runs here, where the store still holds what this fold is about to retire; the
        // *write* is deferred to the last reversible step before the flip, so an abandoned fold
        // leaves no notice claiming an obligation it did not discharge.
        let degraded = self
            .live
            .with_artifacts(|store| store.degradations(&executed));
        stairs.record("9 report");

        // ---- step 2: assemble `SEGMENTS-<n>` from the live partition manifest ------------------
        //
        // Each view's fold base first and its carried extents after it, because the reader takes
        // the first segment listed for a view as the one `permutation.bin` addresses and every
        // later one as an extent above it.
        let mut segments = Vec::with_capacity(completed.segments.len() + carried_segments.len());
        for base in &completed.segments {
            segments.push(base.clone());
            segments.extend(
                carried_segments
                    .iter()
                    .filter(|d| d.view == base.view)
                    .map(|d| (*d).clone()),
            );
        }
        let mut external_id_runs = Vec::with_capacity(1 + carried_runs.len());
        external_id_runs.extend(completed.external_id_run.clone());
        external_id_runs.extend(carried_runs.iter().cloned());

        // **The executed entries leave `tombstones` here as well as the overlay**, and the two must
        // be one decision: the manifest is the overlay's other durable home (write-path §4.5), so a
        // manifest carrying what the swap is about to retire would re-seed it at the next restart.
        let mut published_overlay = (*live.overlay).clone();
        published_overlay.retire(&executed);

        // **The registry comes from the registry, not from the manifest beside it.** A layer
        // registration and an artifact publication write a *side* manifest and do not swap the
        // generation, so the manifest this fold is holding can be several publications behind —
        // and a fold that copied its (empty) layer list would publish a prefix whose membership
        // extents name layers it does not declare. Every one of them is then skipped at open as a
        // dropped layer's leftovers, and every artifact comes back absent with no error anywhere.
        // The online publication path takes the same posture for the same reason.
        let (registered_layers, registered_tombstones, registry_low_water) =
            self.live.registry_for_publication();
        // The roster, from the live roster rather than from the fold's own inputs, on exactly the
        // argument above it: the manifest a fold planned against may be several publications
        // behind, and a view created since must not be dropped by the publication that lands.
        let (created_views, dead_view_incarnations) = self.live.roster_for_publication();
        let (runtime_attributes, runtime_scoped_attributes) =
            self.live.attributes_for_publication();
        // The vocabularies this fold is about to write into `MANIFEST.json`, named here so the
        // live list can be emptied of exactly them once the publication has landed.
        let folded_vocabularies = self
            .live
            .with_vocabularies(|vocabularies| vocabularies.names());
        // The view groups and plain views likewise (`ingest.md` §1.3).
        let (folded_groups, folded_plain_views) = self
            .live
            .with_view_declarations(|declarations| declarations.names());

        let mut segments_manifest = SegmentsManifest {
            // The flight's text extents, and the pass merged every other one into the new base
            // index. A flush publishing during the fold indexed entities the new base does not
            // hold, and dropping its entry would answer every `match` over that batch's prose with
            // silence — the words are simply not in the base the fold wrote.
            text_extents: carried_texts.clone(),
            // **Live, and untouched.** Deriving either from the fold's inputs moves the watermark
            // backwards past every post-snapshot entity, and composition treats an entity at or
            // above it as buffered rather than rowed — so the gap goes invisible to every principal
            // with no error. `check_publishable` refuses a regression; this is what keeps it from
            // having to.
            watermark: live_manifest.watermark,
            entity_id_high_water: live_manifest.entity_id_high_water,
            // **Live, and for a sharper reason than the watermark's.** The fold rewrites the point
            // region and touches the row-less one not at all — no layer is folded, because a layer
            // has no rows to renumber. Deriving this from the fold's inputs would raise the mark
            // back towards the ceiling and hand the next registration ids a live layer already
            // holds. And the registry has to travel with it: rotation reclaims the WAL records the
            // mark is otherwise recovered from, so a fold that published an empty list would lose
            // every gate at the next restart while the layers themselves kept being referenced.
            entity_id_low_water: live_manifest.entity_id_low_water.min(registry_low_water),
            layers: registered_layers,
            layer_tombstones: registered_tombstones,
            views: created_views,
            // **Emptied, because the fold has just written the list into `MANIFEST.json`.** The
            // new bundle manifest carries every `(family, view)` the live one had folded into
            // `scoped_scalars[..].views`, and the fold wrote a column for each — so restating them
            // here would be a second copy of a fact the prefix's own manifest now states
            // (`views.md` §5).
            scoped_columns: Vec::new(),
            // **The declarations made since the fold planned, and only those.** The fold's
            // `MANIFEST.json` is the served schema as it stood at the plan, runtime columns
            // included, with a base written for each (`compact::FoldContext::runtime_attributes`);
            // restating those here would be a second copy of a fact the prefix's own manifest now
            // states, on the scoped columns' argument above. A declaration made while the fold ran
            // is in neither and must survive the publication that lands, so the live list is taken
            // and the folded names removed from it (`ingest.md` §6.3).
            attributes: runtime_attributes
                .iter()
                .filter(|d| !completed.runtime_attributes.contains(&d.name))
                .cloned()
                .collect(),
            scoped_attributes: runtime_scoped_attributes
                .iter()
                .filter(|f| !completed.runtime_scoped_attributes.contains(&f.name))
                .cloned()
                .collect(),
            // **Emptied, because the fold has just written them into `MANIFEST.json`**, on the
            // scoped columns' argument above: `bundle_manifest` below is the live manifest, which
            // carries every runtime vocabulary the merge appended, so restating them here would be
            // a second copy of a fact the new prefix's own manifest states. A vocabulary declared
            // *while the fold ran* is in the live manifest too — a declaration writes no artefact
            // for the fold to have missed, unlike an attribute column's base — so it folds in with
            // the rest and needs no since-plan half.
            vocabularies: Vec::new(),
            // **Emptied, because the fold has just written them into `MANIFEST.json`**, on the
            // vocabularies' argument above: a group and a plain view are manifest state and write
            // no artefact for the fold to have missed, so one declared while the fold ran folds in
            // with the rest.
            groups: Vec::new(),
            plain_views: Vec::new(),
            dead_view_incarnations,
            // **The pass's own output, not the live list.** The paths are prefix-relative and the
            // fold publishes a *new* prefix, so what step 3a wrote is the only list that names
            // files this prefix contains. The content extents beside it are carried by link, their
            // bytes being the same inodes under a second name.
            membership_extents: repacked.clone(),
            // **Stamped by `commit_side_manifest`, from the store and the list above.** Placed
            // here as the empty pair the assembly needs and replaced at the commit, so the version
            // list and the partitions beside it come from one borrow rather than from two points
            // in the fold's flight.
            level_versions: Vec::new(),
            containment_extents: Vec::new(),
            tile_index_extents: Vec::new(),
            row_column_extents: Vec::new(),
            shape_rows_extents: Vec::new(),
            shape_held_extents: Vec::new(),
            artifact_record_extents: self.artifact_record_extents.clone(),
            segments,
            deltas: carried_tiers.clone(),
            // **Verbatim, and the live list rather than the plan's**: a flush that promoted during
            // the fold's flight appended an extent whose ordinals the live dictionary already
            // holds, and dropping it would shift every ordinal after it.
            dict_extents: live_manifest.dict_extents.clone(),
            // **Publishing an attribute artefact is two obligations: the files and this list**
            // (filter-index §6.2). The fold's own base columns are named by convention and
            // digested in `MANIFEST.json`; a *flight* extent is reachable only through this entry,
            // so linking its bytes while leaving the list empty produces a bundle that opens
            // cleanly and silently answers filters without every post-snapshot entity's value — a
            // wrong answer with no symptom, and strictly worse than a refusal to open. The two
            // halves are written here, in one manifest write.
            attr_extents: carried_attrs.clone(),
            record_extents: carried_records.clone(),
            entity_terms_extents: carried_entity_terms.clone(),
            external_id_runs,
            locator_extents: carried_locators.clone(),
            tombstones: Vec::new(),
            deny: Vec::new(),
            // **Empty, because the fold has just folded them in.** Every binding these carried is
            // now a value of the new prefix's `MANIFEST.vocabularies`, so restating them here
            // would bind each key twice — once in each home — and a later reader would have to
            // decide which won.
            vocabulary_extensions: Vec::new(),
            // Every digest goes in `MANIFEST.json` instead — see below.
            files: BTreeMap::new(),
        };
        write_deny_state(&mut segments_manifest, &published_overlay);

        // ---- the new `MANIFEST.json` ----------------------------------------------------------
        //
        // **`entity_id_high_water` here is the *snapshot's* entity space, not the live one**, and
        // the two fields of that name mean different things. `SEGMENTS-<n>.json`'s seeds the I9
        // allocator and is the live value, above. `MANIFEST.json`'s is what
        // `ExternalIdSidecar::deferred_from_manifest` takes as the base locator's declared length —
        // the reader that makes this field's value load-bearing here — and
        // pass 3 sized that locator to the snapshot so post-snapshot locator extents stay reachable
        // past it (compaction §3, pass 3). A live value here would make the base locator claim
        // every post-snapshot entity and answer "this item has no external id" for items that have
        // one.
        //
        // **It is not the only reader, and the second one is I9's allocator.** `Engine::open` seeds
        // the allocator's floor from `bundle.manifest.entity_id_high_water.max(side_manifest)`
        // (`session.rs`), so writing a *lower* value here is safe only because the side-manifest
        // carries the live one and the `max` picks it up. That is the whole of why lowering this
        // field does not re-issue entity ids after a restart — and it is a property of the other
        // reader, not of this one, so a change on either side has to re-check it.
        let mut bundle_manifest = live.bundle.manifest.clone();
        // **The schema as it stood at the plan.** A column declared while the fold ran has no
        // base in the new prefix, so it stays off this manifest and on the side manifest's runtime
        // list, from which the reopen appends it again at the same tail position
        // (`ingest.md` §6.3; `compact::FoldContext::runtime_attributes`).
        {
            let (runtime_attributes, runtime_scoped_attributes) =
                self.live.attributes_for_publication();
            let since_plan: Vec<&str> = runtime_attributes
                .iter()
                .map(|d| d.name.as_str())
                .filter(|name| !completed.runtime_attributes.iter().any(|n| n == name))
                .collect();
            bundle_manifest
                .declared_scalars
                .retain(|d| !since_plan.contains(&d.name.as_str()));
            let scoped_since_plan: Vec<&str> = runtime_scoped_attributes
                .iter()
                .map(|f| f.name.as_str())
                .filter(|name| {
                    !completed
                        .runtime_scoped_attributes
                        .iter()
                        .any(|n| n == name)
                })
                .collect();
            for group in &mut bundle_manifest.groups {
                group
                    .scoped_scalars
                    .retain(|f| !scoped_since_plan.contains(&f.name.as_str()));
            }
        }
        // **Every live binding, with its title, into the table this fold writes**
        // (`ingest.md` §1.3). A vocabulary declared at a running service carries its declaration
        // in the served manifest and its values in its minter, and the extensions folded in below
        // are a second path to the same bindings; taking them from the minters here makes the
        // written table complete whichever path fed it, and the merge is a union so neither can
        // drop one.
        crate::vocabularies::merge_live_values(&mut bundle_manifest, &live.vocabularies);
        bundle_manifest.entity_id_high_water = plan.entity_bound;
        bundle_manifest.files = completed.files.clone();
        // **The extensions fold in verbatim, and verbatim is the whole rule** (§3.3). Every binding
        // the served side-manifests carried becomes a value of the new prefix's
        // `MANIFEST.vocabularies`, keys and codes byte-identical, and the new prefix's first
        // `SEGMENTS-<n>.json` restates an empty extension set — which is what the `Vec::new()`
        // above is.
        //
        // A fold that re-derived, re-sorted or re-numbered here would recolour the whole corpus
        // with no error and no digest mismatch, because `columns.arrow` stores the code and nothing
        // else records what it meant. Appending the carried values is therefore the entire
        // operation: no compilation, no normalisation, no pass through the schema compiler.
        //
        // Decision 0050 touches none of this. Codes are not ordinals, index nothing positional, and
        // no cached artefact is keyed by them, so the fold's postings rewrite and fragment
        // invalidation pass over the vocabulary table without reading it.
        let carried_bindings: Vec<_> = live
            .bundle
            .partitions
            .values()
            .flat_map(|partition| partition.manifest.vocabulary_extensions.iter().cloned())
            .collect();
        tessera_store::vocabulary::fold_extensions_into(
            &mut bundle_manifest.vocabularies,
            &carried_bindings,
        );

        // Exactly the files the new manifest names, deduplicated: a carried segment's run and
        // locator are already in the run and locator lists, and linking one path twice is what
        // `hard_link_forward` refuses.
        let mut carried_rels: BTreeSet<String> = BTreeSet::new();
        for descriptor in &carried_segments {
            let segment_prefix = format!(
                "partitions/{}/{}/segments/{}",
                plan.partition,
                tessera_store::view_rel(&descriptor.view),
                descriptor.seg_id
            );
            for name in ["morton.u32", "columns.arrow"] {
                carried_rels.insert(format!("{segment_prefix}/{name}"));
            }
            // **And every render column's presence bitmap the live manifest names for it**
            // (decision 0064). A fixed list of two files was right while a segment held exactly
            // two; a segment now holds a `presence/<column>.roaring` per rendered column that has
            // an absence, and a carried segment that arrived without one would read as
            // every-row-present — an item with no number matching a range containing zero, which
            // is the 2026-08-11 defect reached by the fold's carry-forward rather than by the
            // scan. Taken from the manifest, not from a directory scan, for the reason
            // `AttrExtent` gives: a scan finds what is there, and the manifest says what must be.
            let presence_prefix = format!("{segment_prefix}/{}/", RENDER_PRESENCE_DIR);
            carried_rels.extend(
                live_manifest
                    .files
                    .keys()
                    .filter(|rel| rel.starts_with(&presence_prefix))
                    .cloned(),
            );
        }
        carried_rels.extend(carried_runs.iter().cloned());
        carried_rels.extend(carried_locators.iter().map(|e| e.path.clone()));
        carried_rels.extend(carried_tiers.iter().cloned());
        // **Every file a carried attribute extent's entry names**, not the two a numeric one has.
        // The values and the presence bitmap always; the sorted dictionary whenever the entry names
        // one, which is exactly when the column is a keyword — its values are ordinals into *that
        // layer's* dictionary and nothing else numbers them, so a carried extent without it is an
        // entry pointing at a file that is not there. The whole prefix then refuses to open, which
        // is how this was found. `postings` and `offsets` ride along for the same reason: an entry
        // naming a file the link set omits is a bundle that will not open, whatever the file is
        // for (`filter-index.md` §2.5; records §4.3, §7).
        for extent in &carried_attrs {
            carried_rels.insert(extent.values.clone());
            carried_rels.insert(extent.presence.clone());
            carried_rels.extend(extent.dict.iter().cloned());
            carried_rels.extend(extent.postings.iter().cloned());
            carried_rels.extend(extent.offsets.iter().cloned());
        }
        // All three files of every carried record extent: the blocks and both addressing files,
        // any of whose absence is a refusal to open rather than "those entities have no record"
        // (records §7).
        for extent in &carried_records {
            carried_rels.insert(extent.blocks.clone());
            carried_rels.insert(extent.hasrow.clone());
            carried_rels.insert(extent.directory.clone());
        }
        // All three files of every carried text extent, under the same rule: the dictionary and
        // the postings are one record — an ordinal names a position in *this* dictionary — and the
        // presence half is what stops an entity whose prose analysed to no terms reading as absent.
        for extent in &carried_texts {
            carried_rels.insert(extent.dict.clone());
            carried_rels.insert(extent.postings.clone());
            carried_rels.insert(extent.presence.clone());
        }
        // All three files of every carried transpose extent, under the same rule: the offsets
        // address the terms and the has-row bitmap ranks them, so any one missing is a refusal at
        // open rather than a shorter label set (`tessera_store::entity_terms`).
        for extent in &carried_entity_terms {
            carried_rels.insert(extent.hasrow.clone());
            carried_rels.insert(extent.offsets.clone());
            carried_rels.insert(extent.terms.clone());
        }
        carried_rels.extend(live_manifest.dict_extents.iter().map(|e| e.path.clone()));
        for rel in &carried_rels {
            // A hard link changes nothing about a file's content, so the digest it earned under the
            // old prefix's path is still correct under the new one — nothing is re-hashed. A file
            // the live manifests name but do not digest is a bundle this fold must not propagate.
            let Some(digest) = live_manifest
                .files
                .get(rel)
                .or_else(|| live.bundle.manifest.files.get(rel))
            else {
                discard("a carried-forward file has no digest in either live manifest");
                return;
            };
            bundle_manifest.files.insert(rel.clone(), digest.clone());
        }

        // ---- step 4: link, write, write, flip --------------------------------------------------
        let from_prefix_dir = self.prefix_dir(&live);
        let mut carried_rels: Vec<String> = carried_rels.into_iter().collect();
        // **The artifacts' content extents are linked and not digested**, which is the one place
        // this set is not uniform. They carry no entry in either manifest's `files` — nothing
        // digests them at their own publication — so putting them through the loop above would
        // discard every fold on a node that has ever published supplied content. They are linked
        // here, after it, and the digest question is theirs to answer wherever it is answered for
        // the online route. An artifact whose content the new prefix does not carry is not served
        // without its description: it is **withheld** (decision 0076), so losing these is losing the
        // artifacts.
        // **From the held list, not from the manifest beside it** — the same stale-generation trap
        // the registry above falls into: a side-manifest write does not swap the generation, so the
        // manifest this fold holds names only the content extents that existed at the last one.
        for extent in &self.artifact_record_extents {
            carried_rels.push(extent.blocks.clone());
            carried_rels.push(extent.hasrow.clone());
            carried_rels.push(extent.directory.clone());
        }
        if let Err(e) =
            tessera_store::hard_link_forward(&from_prefix_dir, &to_prefix_dir, &carried_rels)
        {
            discard(&format!("its carry-forwards would not link ({e})"));
            return;
        }
        // **Both halves — the bytes and the names — and the bytes are the half that is not
        // obvious.** A hard link copies no bytes, so the directory entry is plainly the new thing;
        // the trap is concluding from that that the bytes were already durable. **No producer
        // fsyncs a data file.** Neither `write_single_batch` nor the morton, tier or run writers
        // sync, and `write_segments_manifest` syncs the manifest and its directory and nothing the
        // manifest names. That is a *reasoned* position everywhere else in the write path — a torn
        // file is detectable through its digest, its rows are still in the WAL, and the flush
        // re-runs — and the fold is the one operation that destroys every part of it: it flips
        // `CURRENT` onto these links, deletes the prefix holding the only other names for the same
        // inodes, and rotates the WAL out from under the records. "Detectable" becomes "detectably
        // gone", for whatever the kernel had not written back — roughly the last 30 s of
        // publications before the flip, which is exactly the window a fold's carry-forward set is
        // drawn from.
        //
        // Cheap, because the set is only what published *during* the flight: `plan_fold` consumes
        // everything the manifest named at its snapshot, so nothing older than the fold is here.
        // The fold's own five passes synced on its own thread (`compact::execute`, pass 5).
        let carried_paths: Vec<PathBuf> = carried_rels
            .iter()
            .map(|rel| to_prefix_dir.join(rel))
            .collect();
        if let Err(e) = tessera_store::fsync_written(&carried_paths) {
            discard(&format!("its carry-forwards would not sync ({e})"));
            return;
        }
        let manifest_digest =
            match tessera_store::write_manifest_json(&to_prefix_dir, &bundle_manifest) {
                Ok(digest) => digest,
                Err(e) => {
                    discard(&format!("its MANIFEST.json would not commit ({e})"));
                    return;
                }
            };
        if let Err(e) = self.commit_side_manifest(
            live_manifest,
            &to_prefix_dir,
            &plan.partition,
            manifest_n,
            &mut segments_manifest,
            &containment,
            &tile_indexes,
            &row_columns,
            &shape_rows,
            &shape_held,
            &pending.levels,
        ) {
            discard(&format!(
                "its SEGMENTS-{manifest_n}.json would not commit ({e})"
            ));
            return;
        }
        // **The commit point.** Everything above is reversible; nothing below is.
        //
        // Which makes this the simplest publication seam in the system: a kill parked here leaves
        // a complete, synced `v#####` tree `CURRENT` never named, and the startup sweep reclaims
        // it whole — no per-file bookkeeping (correctness-suite §12.3, compaction §7). The fold's
        // manifest writes above deliberately carry no pause site of their own: their crash story
        // is this one's.
        // **The report is the last reversible step, and that placement is the whole of its
        // evidential value.** It says an obligation was discharged, so a notice left behind by a
        // fold that was then abandoned at its link, its manifest or its flip is a false positive:
        // an operator polling `reports/` reads that a deletion was reported when it is still owed
        // one. Everything that can still discard is above this line.
        if let Err(e) = self.write_fold_report(&completed.prefix, &degraded) {
            discard(&format!(
                "its degradation report would not be written ({e}), and a deletion may not retire \
                 before the caller has been told what it degraded"
            ));
            return;
        }
        stairs.record("10 manifest");

        self.pause_point(PauseSiteArg::BeforeCurrentFlip);
        if let Err(e) =
            tessera_store::write_current(&self.bundle_root, &completed.prefix, &manifest_digest)
        {
            discard(&format!("CURRENT would not flip ({e})"));
            return;
        }
        stairs.record("11 flip");

        // **The resident store retires here, before the new generation is installed** — not after
        // the warm below. The generation this fold is about to publish carries an overlay with the
        // executed deletions already retired, so between installing it and retiring the store there
        // is a window in which a deleted artifact's own entity reads *not denied* while its record
        // is still there: it would be served, with its content, to whoever asked. The warm makes
        // that window tens of seconds wide at 10⁷ artifacts.
        //
        // Ahead of the swap is safe in the other direction: the generation still being served
        // carries the deletion in its own overlay, so an artifact this removes was already absent
        // for every request reaching it.
        let mut moved = self.live.retire_artifacts(&pending.retired);
        self.live.mark_memberships_published();
        // **And the growths, which only a whole rewrite reaches.** `rewrite_membership_extents`
        // wrote every level entire, from the resident store, so a membership that grew since the
        // last fold is in the prefix `CURRENT` now names — the one publication that carries a
        // record sitting below its level's high-water. Until this point the log was holding those
        // records as the only copy.
        self.live.mark_growth_packed();
        // **The resident memberships move onto the extents this fold wrote** — see
        // `LiveState::rehouse_memberships`. Until they do, every membership seeded at open still
        // reads through the previous prefix's packs, and reclamation would unlink files whose
        // blocks the mapping goes on holding.
        let (rehoused, kept) = self.live.rehouse_memberships(&to_prefix_dir, &repacked);
        if kept > 0 {
            // **Unreachable, on the seed's rule.** The extents were written from this store a
            // moment ago and a hole is an empty blob the rehousing skips, so a record that does
            // not take is one the store no longer holds at that ordinal, or one whose cardinality
            // disagrees with the bytes this fold wrote for it. The first is a level the prefix and
            // the store describe differently; the second is an artifact served from the bitmap it
            // already holds, against an extent a restart will read instead.
            tracing::error!(
                rehoused,
                kept,
                "ALARM: artifact memberships this fold wrote back do not match the records they \
                 were written from; the level the prefix carries and the level being served \
                 disagree for those ordinals"
            );
        }
        self.membership_extents = repacked;
        // **The retirement moved the levels step 3a said it would, checked rather than assumed.**
        // A pending level's structures were stamped with the version the level would have after
        // this retirement (`PendingRetirement`). `levels_moved_by` and `retire` read one predicate,
        // so any other outcome is unreachable; the check is what keeps a structure from being
        // carried into a later manifest at a version it does not describe if that ever changes.
        // It protects the live lists and the manifests later flushes write from them; the fold's
        // own manifest is already durable with the version and the structures it states, and for
        // that the shared predicate (`membership.rs`'s `record_moved_by`) is the whole guarantee.
        moved.sort();
        let mut expected = pending.levels.clone();
        expected.sort();
        if moved != expected {
            tracing::error!(
                stamped_for = ?expected,
                moved = ?moved,
                "ALARM: the fold's retirement moved levels other than the ones its derived \
                 structures were stamped for; every structure whose stamp the store does not \
                 carry is dropped and its level recomposes on first use"
            );
        }
        // The structures this fold wrote replace whatever the previous prefix held: their paths
        // are prefix-relative and the fold publishes a new prefix, so the old entries name files
        // this prefix does not contain. Held only at the version the store now carries.
        let (containment, tile_indexes, row_columns, shape_rows, shape_held) =
            self.live.with_artifacts(|store| {
                (
                    held_at_current_version(
                        store,
                        "containment partition",
                        &segments_manifest.containment_extents,
                        |e| (e.layer.as_str(), e.level, e.level_version),
                    ),
                    held_at_current_version(
                        store,
                        "tile index",
                        &segments_manifest.tile_index_extents,
                        |e| (e.layer.as_str(), e.level, e.level_version),
                    ),
                    held_at_current_version(
                        store,
                        "row column",
                        &segments_manifest.row_column_extents,
                        |e| (e.layer.as_str(), e.level, e.level_version),
                    ),
                    held_at_current_version(
                        store,
                        "shape row form",
                        &segments_manifest.shape_rows_extents,
                        |e| (e.layer.as_str(), e.level, e.level_version),
                    ),
                    held_at_current_version(
                        store,
                        "held shape",
                        &segments_manifest.shape_held_extents,
                        |e| (e.layer.as_str(), e.level, e.level_version),
                    ),
                )
            });
        self.containment_extents = containment;
        self.tile_index_extents = tile_indexes;
        self.row_column_extents = row_columns;
        self.shape_rows_extents = shape_rows;
        self.shape_held_extents = shape_held;
        *lock_recover(&self.health.last_fold_report) = degraded;
        stairs.record("12 retire");

        // ---- steps 5 and 6: open the new prefix, then one swap ---------------------------------
        let rotation = crate::session::open_rotation(
            &self.bundle_root,
            &completed.prefix,
            &live.fragments,
            executed,
        );
        let (bundle, rotation) = match rotation {
            Ok(pair) => pair,
            Err(e) => {
                self.diverge_from_current(&completed.prefix);
                tracing::error!(
                    error = %e,
                    prefix = %completed.prefix,
                    "ALARM: CURRENT names the folded prefix and this process could not open it. \
                     The bundle on disc is complete and a restart serves it; until then this node \
                     serves the superseded prefix and publishes nothing. Nothing retired"
                );
                return;
            }
        };

        // **The tier list, re-derived from the committed manifest** rather than filtered
        // positionally: `deltas` is the authority on which tiers are live (contracts §2.3 r18), and
        // re-deriving from it is the one form that cannot drift from what a restart would open. The
        // readers themselves are the live `Arc`s — their mappings are of the same inodes the
        // carry-forward just gave a second name, so they survive the old prefix's deletion exactly
        // as `reclaim_prefix` argues.
        let mut delta_postings: Vec<Arc<DeltaTier>> = Vec::with_capacity(carried_tiers.len());
        for rel in &segments_manifest.deltas {
            match live
                .delta_postings
                .iter()
                .zip(&live_manifest.deltas)
                .find(|(_, live_rel)| *live_rel == rel)
            {
                Some((tier, _)) => delta_postings.push(Arc::clone(tier)),
                None => {
                    self.diverge_from_current(&completed.prefix);
                    tracing::error!(
                        tier = %rel,
                        "ALARM: the folded manifest names a delta tier this process does not hold \
                         open; abandoning the swap rather than serving a fragment built from fewer \
                         tiers than the manifest declares. CURRENT names the new prefix and a \
                         restart serves it"
                    );
                    return;
                }
            }
        }

        let segments_version = live.segments_version + 1;
        if let Err(e) = self.publish_geometry(
            GeometryPublication::within_prefix(
                completed.prefix.clone(),
                segments_version,
                // Live, and untouched — see the manifest's own note above.
                live.watermark,
                bundle,
                // Carried forward, never renumbered and never shrunk (compaction §3, pass 4).
                Arc::clone(&live.dict),
                delta_postings,
            )
            .rotating(rotation),
        ) {
            self.diverge_from_current(&completed.prefix);
            tracing::error!(
                error = %e,
                "ALARM: CURRENT names the folded prefix and the swap was refused. A restart \
                 serves the new bundle; until then this node serves the superseded one and \
                 publishes nothing"
            );
            return;
        }

        stairs.record("13 open");

        // **The structures this fold wrote, adopted by the process that wrote them.**
        // `Engine::open` adopts a prefix's containment partitions, tile indexes and row columns
        // against the store it seeded; this is the same prefix and the same store, retired above.
        // Without it the warm below projects every level from its memberships and only a restart
        // reads what the artifact pass wrote. After the swap, because a claim is keyed by prefix
        // and a request on the outgoing generation would drop an entry the new one is about to
        // ask for.
        self.live.with_artifacts(|store| {
            self.artifact_projections.adopt_all(
                &to_prefix_dir,
                &completed.prefix,
                &self.containment_extents,
                store,
            );
            self.artifact_projections.adopt_indexes(
                &to_prefix_dir,
                &completed.prefix,
                &self.tile_index_extents,
                store,
            );
            self.artifact_projections.adopt_columns(
                &to_prefix_dir,
                &completed.prefix,
                &self.row_column_extents,
                store,
            );
        });
        stairs.record("14 adopt");

        // **The row forms, rebuilt here rather than by whoever arrives first.** Row space renumbers
        // globally at a fold, so every projection built over the old one is invalid at the flip —
        // and a level is a deployment-wide artefact rather than a per-session value, so the
        // first-toucher rebuild a session mask can absorb would be a stall of tens of seconds on
        // whichever request arrived next (`annotation-representation.md` §5.0.3). It is the same
        // construction the request path runs, on the permutation this fold has just written:
        // measured at 32.8 s threaded for 10⁷ artifacts over 10⁹ rows
        // (`probes/2026-08-16-fold-artifact-pass/`).
        //
        // **After the swap, and after the retire above.** Two orderings, both load-bearing: built
        // before the generation is live, every projection would be keyed to one no reader can ask
        // for; built before the store retires, every projection would be keyed to a store version
        // the retire is about to bump, and the whole warm would be discarded on the first request —
        // paying the stall it exists to prevent, having already paid for the warm.
        self.warm_artifact_caches();
        // The pieces the artifact pass staged were taken by the builds above; what is left is
        // staged for a level nothing built a form for, and would otherwise be held for ever.
        self.shapes.clear_staged();
        stairs.record("15 warm");

        // ---- step 7: rotate the WAL ------------------------------------------------------------
        //
        // Immediately, and that is compaction §5's whole mitigation for retirement's durability
        // window: the manifest seed no longer carries the executed entries, but the WAL still holds
        // the original `ChangeByEntity{Delete}` records, so until a rotation whose head snapshot
        // postdates this fold has reclaimed them a restart resurrects them — harmlessly (they name
        // entities with no row and no postings) and **permanently**, since a rotation snapshot
        // applies entries and never assigns.
        self.rotate_wal();
        stairs.record("16 wal");
        // The columns this fold wrote into `MANIFEST.json` leave the runtime list: from here they
        // are the build's, with a base every reader opens (`ingest.md` §6.3).
        self.live.with_attributes(|attributes| {
            attributes.retire_folded(
                &completed.runtime_attributes,
                &completed.runtime_scoped_attributes,
            )
        });
        // The vocabularies beside them. Every one the list held when the manifest was assembled is
        // in the `MANIFEST.json` this fold wrote, that manifest being the live one, so the list
        // empties by name rather than by a since-plan subtraction; a declaration made after the
        // assembly is not among them and stays.
        self.live
            .with_vocabularies(|vocabularies| vocabularies.retire_folded(&folded_vocabularies));
        // The view groups and plain views beside them, on the same rule.
        self.live.with_view_declarations(|declarations| {
            declarations.retire_folded(&folded_groups, &folded_plain_views)
        });

        // ---- step 8: reclaim the superseded prefix (compaction §8) ------------------------------
        self.pending_reclaim.push(PendingReclaim {
            generation: live,
            prefix_dir: from_prefix_dir,
            // Taken, not cloned: these belong to the prefix being superseded, and the prefix this
            // fold just published starts with none.
            superseded_sidecars: std::mem::take(&mut self.superseded_sidecars),
        });
        self.reclaim_superseded_prefixes();
        stairs.record("17 reclaim");
        let cost = stairs.into_cost();

        self.health.folds.fetch_add(1, Ordering::Relaxed);
        // **The fold's own account of what it spent, at the one severity an operator reads.** The
        // most expensive operation in the system had no cost record at all until it had one here:
        // its counters said a fold happened, and nothing said what it took. The staircase is the
        // diagnostic half — a resident set that climbs on one pass names that pass — and the two
        // gauges below are the alarming half, on `/control/status`.
        let passes = cost
            .iter()
            .map(|c| {
                // Total and anonymous, because §3's budget is a claim about the split: a fold
                // whose total climbs because its mapped inputs became resident is behaving as
                // designed, and one whose *anonymous* half climbs with the corpus has a term
                // nobody budgeted. One number cannot distinguish them.
                format!(
                    "{}={:?}/{:.2}GiB({:.2} anon)",
                    c.pass,
                    c.elapsed,
                    c.rss as f64 / (1u64 << 30) as f64,
                    c.anon as f64 / (1u64 << 30) as f64,
                )
            })
            .collect::<Vec<_>>()
            .join(" ");
        // Summed before it is truncated to seconds: a fold of many sub-second rows is not a
        // zero-second fold.
        let fold_secs = cost
            .iter()
            .map(|c| c.elapsed)
            .sum::<std::time::Duration>()
            .as_secs();
        let staircase_rss = cost.iter().map(|c| c.rss).max().unwrap_or(0);
        self.health
            .last_fold_secs
            .store(fold_secs, Ordering::Relaxed);
        self.health
            .last_fold_rss
            .store(staircase_rss, Ordering::Relaxed);
        self.health
            .last_fold_attr_read
            .store(completed.attr_bytes_read, Ordering::Relaxed);
        self.health
            .last_fold_attr_written
            .store(completed.attr_bytes_written, Ordering::Relaxed);
        *lock_recover(&self.health.last_fold_passes) = cost;
        tracing::info!(
            prefix = %completed.prefix,
            segments_version,
            retired = retired_count,
            carried_entities = carried.len(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            passes = %passes,
            fold_secs,
            staircase_rss,
            attr_bytes_read = completed.attr_bytes_read,
            attr_bytes_written = completed.attr_bytes_written,
            // **What the fold reclaimed by leaving it behind** (`views.md` §3.4). A dropped view's
            // row space goes with the superseded prefix and nothing else records that it did: the
            // mechanism is a deliberate omission, so an operator who cannot see it here cannot see
            // it at all. Zero on every fold of a bundle nothing was dropped from, which is nearly
            // all of them.
            dropped_views = %if omitted_views.is_empty() {
                "none".to_string()
            } else {
                omitted_views.join(",")
            },
            dropped_view_segments = omitted_segments,
            "a compaction fold published: the bundle is one base segment per partition-view, one \
             base postings tier, one external-id run and one locator, plus whatever landed during \
             its flight"
        );
    }

    /// Latch [`ExecutorHealth::prefix_diverged`]: `CURRENT` names a prefix this process could not
    /// swap onto, so it must stop writing durable state.
    ///
    /// **Every publication after this point would land in a tree no restart reads.** The live
    /// generation still names the superseded prefix and every manifest path derives its directory
    /// from that generation — correctly, on the success path — so a flush would write its segment
    /// and side-manifest under the old prefix, ack the rows, and then rotate the WAL and reclaim
    /// their records. Acked ingest, lost at the next restart, with nothing logged at the loss. A
    /// crash cannot produce this because a crashed process stops writing; only a live one that
    /// flipped `CURRENT` and carried on can.
    ///
    /// The superseded prefix is deliberately **not** reclaimed on this path — it is what this
    /// process is still serving from, and `reclaim_prefix` would refuse it anyway now that
    /// `CURRENT` names the other one.
    fn diverge_from_current(&self, committed: &str) {
        self.health.fold_failures.fetch_add(1, Ordering::Relaxed);
        self.health.prefix_diverged.store(true, Ordering::SeqCst);
        tracing::error!(
            committed_prefix = %committed,
            "ALARM: this node's live generation and its durable CURRENT disagree. It keeps serving \
             what it has and publishes nothing — no geometry, no deny state, no WAL rotation — \
             until it is restarted, at which point it opens the committed prefix and is correct \
             again. Publishing from here would write acked state into a prefix no restart reads"
        );
    }

    /// Whether this executor may still write durable state — the two latching postures, asked in
    /// one place so a new publication kind cannot miss one.
    ///
    /// Deliberately **not** including the WAL's poison flag: that one is recoverable and is asked
    /// separately by the callers that care (`rotate_wal` cannot append at all; `plan_flush` refuses
    /// for the apply-anyway reason). These two are terminal until a restart.
    fn may_publish(&self) -> bool {
        !self.health.overlay_diverged.load(Ordering::SeqCst)
            && !self.health.prefix_diverged.load(Ordering::SeqCst)
    }

    /// Delete every superseded prefix nothing is reading any more — **the reclamation event**
    /// compaction §8 makes the fold's reason for existing (on-disc bytes are a measured 2.0–2.6×
    /// the bytes the manifest names, and nothing else in the system can delete a file).
    ///
    /// **The `Arc` is the wait, and it is a wait rather than a hope.** Every file the new prefix
    /// still needs has a directory entry there, so this unlinks directory entries and never live
    /// data — but the external-id sidecar opens its runs lazily, so a request still holding the
    /// superseded generation could be about to open a path under that tree. The generation pointer
    /// has already moved, so no new holder can appear; holding the `Arc` here and reclaiming only
    /// at a count of one turns "wait for the readers" into a condition rather than a delay.
    ///
    /// **And it is now exhaustive rather than a narrowing.** It once counted the held generation
    /// and its sidecar, which together answer for every generation a *flush* produced over the
    /// prefix and for none of the ones holding a sidecar a **coalesce** replaced — a set the counts
    /// could not see at all. `superseded_sidecars` closes that, weakly, so the three counts between
    /// them name every sidecar that was ever live over the prefix and therefore every generation
    /// that could still resolve a path inside it.
    ///
    /// A failure alarms once and drops the entry: `remove_dir_all` failing is a permissions or
    /// device fault rather than a transient one, and retrying it every tick is a log flood around a
    /// condition an operator has to act on. The tree then stands as an orphan, which is the same
    /// residual a process exiting mid-wait leaves.
    fn reclaim_superseded_prefixes(&mut self) {
        if self.pending_reclaim.is_empty() {
            return;
        }
        let mut still_read = Vec::new();
        for pending in std::mem::take(&mut self.pending_reclaim) {
            // **Three counts, because they reach three different sets** — see `pending_reclaim`
            // and `superseded_sidecars`. The generation's own count answers for itself; its
            // sidecar's answers for every *other* generation a flush produced over the same prefix,
            // because a flush publishes by cloning the live sidecar `Arc` rather than building one;
            // and the weak list answers for the generations that hold a sidecar a **coalesce**
            // replaced, which is the one publication that builds a new one over an unchanged
            // prefix and so the one case the second count cannot see.
            //
            // Read off the held generation, so at rest the two strong counts are 1 — this entry is
            // the only holder of the generation, and the generation is the only holder of the
            // sidecar — and every weak count is 0. The post-fold generation has a sidecar of its
            // own and appears in none of the three.
            if Arc::strong_count(&pending.generation) > 1
                || Arc::strong_count(&pending.generation.external_index) > 1
                || pending
                    .superseded_sidecars
                    .iter()
                    .any(|held| held.strong_count() > 0)
            {
                still_read.push(pending);
                continue;
            }
            let PendingReclaim {
                generation,
                prefix_dir,
                superseded_sidecars,
            } = pending;
            drop(superseded_sidecars);
            drop(generation);
            match tessera_store::reclaim_prefix(&prefix_dir) {
                Ok(()) => tracing::info!(
                    prefix = %prefix_dir.display(),
                    "the superseded prefix is reclaimed: the build's base, every merged-away \
                     segment, every consumed tier and every superseded side-manifest"
                ),
                Err(e) => tracing::error!(
                    error = %e,
                    prefix = %prefix_dir.display(),
                    "ALARM: the superseded prefix could not be reclaimed and stands as an orphan. \
                     Nothing live was unlinked; what is lost is the disc the fold exists to free"
                ),
            }
        }
        self.pending_reclaim = still_read;
    }

    /// Apply every completed coalesce waiting from the pool, and report whether any did.
    fn publish_completed_coalesces(&mut self) -> bool {
        let mut any = false;
        while let Ok(completed) = self.coalesce_done.try_recv() {
            self.publish_coalesce(completed);
            any = true;
        }
        if any {
            // Cleared only after something was drained, never on an empty pass — the other half of
            // the handshake; see `ExecutorHealth::flush_completed_pending`.
            self.health
                .coalesce_completed_pending
                .store(false, Ordering::SeqCst);
        }
        any
    }

    /// **Publish an entity-space coalesce: a manifest edit, a sidecar swap and a tier-list swap —
    /// and no `segments_version` bump** (decision 0044 D2).
    ///
    /// This is the half of merge that touches no row space. Nothing it rewrites addresses a row:
    /// a delta tier is `(term, entity)` pairs, a run and its locator are `external_id ↔ entity`,
    /// a dictionary extent is descriptors. So no projection is invalidated, no fragment is stale,
    /// no cache key rotates and no session pays anything — which is what makes it 0043-conforming
    /// by construction rather than by 0044's refresh mechanism, and why it lands ahead of the
    /// row-space merge that is gated on it.
    ///
    /// **What it does swap is the two pieces of live state the manifest names**: the generation's
    /// tier list, so a fragment built after this reads one file where it read `width`; and the
    /// external-id sidecar, so the duplicate check scans one run where it scanned `width`. Both
    /// are content-preserving, so a request holding the old and a request holding the new agree
    /// on every answer — the swap buys the bound, never a correctness property.
    ///
    /// **The consumed files are not deleted.** Every side-manifest below this `n` still names
    /// them, and a step-down serves one of those (contracts §2.3); reclaiming them is
    /// compaction's, along with every other orphan.
    fn publish_coalesce(&mut self, completed: crate::coalesce::CompletedCoalesce) {
        let started = std::time::Instant::now();
        // **A node whose durable state disagrees with what it is serving publishes nothing**
        // (`may_publish`). The unit's files are orphans and its inputs still stand, which is the
        // same posture every other publication failure takes.
        if !self.may_publish() {
            return;
        }
        let live = self.generation.load_full();
        if live.prefix != completed.prefix {
            tracing::warn!(
                planned = %completed.prefix,
                live = %live.prefix,
                "discarding a completed coalesce planned against a superseded prefix"
            );
            return;
        }
        let Some(partition_data) = live.bundle.partitions.get(&completed.plan.partition) else {
            return;
        };

        let mut manifest = partition_data.manifest.clone();
        if !crate::coalesce::rebase_into(&mut manifest, &completed) {
            // The window it planned against is gone. Expected rather than exceptional — see
            // `rebase_into` — and the files are orphans nothing references.
            tracing::warn!("discarding a completed coalesce that no longer rebases");
            return;
        }
        // **Composed before the manifest is written, in the flush's order and for its reason**
        // (filter-index §5.2): a composition that refuses must not leave a published manifest
        // naming layers this process cannot serve, and the reverse order commits a manifest whose
        // own writer then refuses it. The unit carries opened columns, so this cannot fail on IO.
        let windows: Vec<crate::filter::CoalescedWindow> = completed
            .attrs
            .iter()
            .zip(&completed.plan.attrs)
            .map(|(attr, window)| crate::filter::CoalescedWindow {
                column: crate::filter::extent_column_name(
                    &attr.extent.column,
                    attr.extent.view.as_deref(),
                ),
                consumed: window.extents.iter().map(|e| e.values.clone()).collect(),
                values_rel: attr.extent.values.clone(),
                values: Arc::clone(&attr.values),
                // A keyword window's merged dictionary, beside the ordinals it numbers; the
                // composition installs the pair as one layer or refuses.
                dict: attr.dict.clone(),
            })
            .collect();
        // The text axis's windows, named by dictionary path on both sides. The paths are resolved
        // against the live prefix here, on the executor, and opened inside the composition — the
        // flush's arrangement for a text extent, and for its reason: a text layer is three files
        // that must be installed together.
        let prefix_dir = self.prefix_dir(&live);
        let text_windows: Vec<crate::filter::CoalescedTextWindow> = completed
            .texts
            .iter()
            .zip(&completed.plan.texts)
            .map(|(extent, window)| crate::filter::CoalescedTextWindow {
                consumed: window.extents.iter().map(|e| e.dict.clone()).collect(),
                paths: crate::filter::TextExtentPaths {
                    column: crate::filter::extent_column_name(
                        &extent.column,
                        extent.view.as_deref(),
                    ),
                    dict_rel: extent.dict.clone(),
                    dict: prefix_dir.join(&extent.dict),
                    postings: prefix_dir.join(&extent.postings),
                    presence: prefix_dir.join(&extent.presence),
                },
            })
            .collect();
        // **The transpose's stack is re-derived from the rebased manifest**, not patched — the
        // form `delta_postings` below takes, and for its reason: re-deriving is the one shape that
        // cannot drift from what a restart would open. It is affordable here where it would not be
        // per flush: the base's `hasrow` is a run-container bitmap and its other two files are
        // mapped rather than read, and a coalesce fires once per `width` ticks. `None` where the
        // axis did not run, in which case the live stack rides through untouched.
        let entity_terms = if completed.terms.is_none() {
            None
        } else {
            let partition_dir = prefix_dir
                .join("partitions")
                .join(&completed.plan.partition);
            let extents: Vec<tessera_store::EntityTermsExtentPaths> = manifest
                .entity_terms_extents
                .iter()
                .map(|e| tessera_store::EntityTermsExtentPaths {
                    hasrow: prefix_dir.join(&e.hasrow),
                    offsets: prefix_dir.join(&e.offsets),
                    terms: prefix_dir.join(&e.terms),
                })
                .collect();
            match tessera_store::EntityTermsStack::open(
                Some(&partition_dir.join(tessera_store::ENTITY_TERMS_DIR)),
                &extents,
            ) {
                Ok(stack) => Some(Arc::new(stack)),
                Err(e) => {
                    self.health
                        .coalesce_failures
                        .fetch_add(1, Ordering::Relaxed);
                    tracing::error!(
                        error = %e,
                        "ALARM: a completed coalesce's entity→term extent would not compose into \
                         a stack; discarding it rather than publishing a manifest naming a layer \
                         this process cannot serve. Its files are orphans and every consumed \
                         entry still stands"
                    );
                    return;
                }
            }
        };
        // **The record axis's stack is re-derived from the rebased manifest too**, on exactly the
        // transpose's rule above: a coalesce that folds a window of record extents into one must
        // leave the live reader probing the extent it wrote and not the ones it consumed, or the
        // process serves from layers its own manifest no longer names until a restart. Affordable
        // for the same reason: the layers are memory-mapped, and a coalesce fires once per
        // `width` ticks. `None` where the axis did not run, and the live stack rides through.
        let records =
            if completed.record.is_none() {
                None
            } else {
                let partition_dir = prefix_dir
                    .join("partitions")
                    .join(&completed.plan.partition);
                // The schema decides whether there is a base, exactly as it does at open: a build
                // writes `attrs/record` only where a column has no other home. Derived rather than
                // probed for, so a missing base refuses instead of reading as "those entities have no
                // record".
                let blob_resident =
                    live.bundle.manifest.declared_scalars.iter().any(|d| {
                        crate::filter::blob_resident(d, &live.bundle.manifest.vocabularies)
                    });
                let record_dir = partition_dir.join("attrs").join("record");
                // **Both lists, one stack**, as the open composes them: an artifact's content extents
                // hold the same format and the same reader, and the two never share an entity.
                let extents: Vec<tessera_filter::RecordExtentPaths> = manifest
                    .record_extents
                    .iter()
                    .chain(manifest.artifact_record_extents.iter())
                    .map(|e| tessera_filter::RecordExtentPaths {
                        blocks: prefix_dir.join(&e.blocks),
                        hasrow: prefix_dir.join(&e.hasrow),
                        directory: prefix_dir.join(&e.directory),
                    })
                    .collect();
                match tessera_filter::RecordStack::open(
                    blob_resident.then_some(record_dir.as_path()),
                    &extents,
                    live.filter_columns.access(),
                ) {
                    Ok(stack) => Some(Arc::new(stack)),
                    Err(e) => {
                        self.health
                            .coalesce_failures
                            .fetch_add(1, Ordering::Relaxed);
                        tracing::error!(
                            error = %e,
                            "ALARM: a completed coalesce's record extent would not compose into a \
                             stack; discarding it rather than publishing a manifest naming a \
                             layer this process cannot serve. Its files are orphans and every \
                             consumed entry still stands"
                        );
                        return;
                    }
                }
            };
        let filter_columns = match live.filter_columns.with_coalesced(
            &windows,
            &text_windows,
            entity_terms,
            records,
        ) {
            Ok(columns) => Arc::new(columns),
            Err(e) => {
                self.health
                    .coalesce_failures
                    .fetch_add(1, Ordering::Relaxed);
                tracing::error!(
                    error = %e,
                    "ALARM: a completed coalesce's attribute extents would not replace the layers \
                     they consumed; discarding it rather than publishing a manifest naming a \
                     column this process cannot serve. Its files are orphans and every consumed \
                     entry still stands"
                );
                return;
            }
        };
        // Complete current state, serialised fresh from the overlay this publication carries —
        // the same rule every other manifest write follows (contracts §2.3).
        write_deny_state(&mut manifest, &live.overlay);
        write_vocabulary_extensions(
            &mut manifest,
            &live.vocabularies,
            &live.bundle.manifest.vocabularies,
        );

        let manifest_n = self.allocate_manifest_n();
        // The publication seam: the coalesced extents are on disc and nothing durable names them
        // until this write returns (correctness-suite §12.3).
        self.pause_point(PauseSiteArg::BeforeManifestPublish);
        if let Err(e) = self.commit_side_manifest(
            &partition_data.manifest,
            &self.prefix_dir(&live),
            &completed.plan.partition,
            manifest_n,
            &mut manifest,
            &self.containment_extents,
            &self.tile_index_extents,
            &self.row_column_extents,
            &self.shape_rows_extents,
            &self.shape_held_extents,
            &[],
        ) {
            self.health
                .coalesce_failures
                .fetch_add(1, Ordering::Relaxed);
            tracing::error!(
                error = %e,
                "ALARM: a completed coalesce's side-manifest could not be committed; its files \
                 are orphans, every consumed entry still stands, and the next tick re-plans"
            );
            return;
        }

        // The sidecar reads the *new* manifest, so it must be built after the edit and before the
        // swap — and it is built here, on the executor, because a failure must abandon the
        // publication rather than leave the generation naming runs no sidecar can resolve.
        let next_index = match crate::session::ExternalIdIndex::open(
            &live.bundle.manifest,
            &manifest,
            &self.prefix_dir(&live),
        ) {
            Ok(index) => index,
            Err(e) => {
                self.health
                    .coalesce_failures
                    .fetch_add(1, Ordering::Relaxed);
                tracing::error!(
                    error = %e,
                    "ALARM: a coalesce's manifest committed but its external-id sidecar would not \
                     open; the process keeps serving the pre-coalesce sidecar, which answers \
                     identically, and a restart opens the committed manifest"
                );
                return;
            }
        };

        let next_bundle = match live.bundle.with_manifest(
            &completed.plan.partition,
            tessera_store::read::PublishedManifest {
                manifest,
                n: manifest_n,
            },
        ) {
            Ok(bundle) => bundle,
            Err(e) => {
                tracing::warn!(error = %e, "discarding a completed coalesce that no longer rebases");
                return;
            }
        };

        // **The tier list, with the consumed tiers replaced by the one that carries their pairs.**
        // Rebuilt from the new manifest rather than patched positionally: `deltas` is now the
        // authority on which tiers are live (contracts §2.3 r18), and re-deriving from it is the
        // one form that cannot drift from what a restart would open.
        let mut delta_postings: Vec<Arc<DeltaTier>> = Vec::new();
        let coalesced = completed.tier.as_ref();
        for rel in &next_bundle
            .partitions
            .get(&completed.plan.partition)
            .expect("the partition this publication just rebased")
            .manifest
            .deltas
        {
            match coalesced.filter(|(path, _)| path == rel) {
                Some((_, tier)) => delta_postings.push(Arc::clone(tier)),
                None => match live
                    .delta_postings
                    .iter()
                    .zip(&partition_data.manifest.deltas)
                    .find(|(_, live_rel)| *live_rel == rel)
                {
                    Some((tier, _)) => delta_postings.push(Arc::clone(tier)),
                    None => {
                        tracing::error!(
                            tier = %rel,
                            "ALARM: a coalesce's manifest names a delta tier this process does \
                             not hold open; abandoning the swap rather than serving a fragment \
                             built from fewer tiers than the manifest declares"
                        );
                        return;
                    }
                },
            }
        }

        let next = Generation {
            prefix: live.prefix.clone(),
            vocabularies: Arc::clone(&live.vocabularies),
            // The live columns with each consumed window replaced by the layer that carries its
            // values — the same set of `(entity, value)` pairs in fewer files, so a request holding
            // the old and one holding the new agree on every answer.
            filter_columns,
            // A coalesce is content-preserving in value space too.
            suggest: Arc::clone(&live.suggest),
            // **Unchanged, and this is the whole of D2.** Row space did not move, so no
            // projection is stale and no cache key may rotate.
            segments_version: live.segments_version,
            watermark: live.watermark,
            bundle: next_bundle,
            dict: Arc::clone(&live.dict),
            postings: Arc::clone(&live.postings),
            fragments: Arc::clone(&live.fragments),
            // **The sidecar rides the swap, rather than being stored beside it.** It used to be an
            // `ArcSwap` on the `Engine`, stored one statement after this publication; that was
            // sound here because a coalesce is content-preserving, and it is not sound for a fold,
            // which drops the retired entities' keys and writes into a new prefix. One pointer
            // now carries both, so no request can ever hold a generation and a sidecar from two
            // publications.
            external_index: Arc::new(next_index),
            delta_postings,
            overlay_version: live.overlay_version,
            overlay: Arc::clone(&live.overlay),
            buffer: Arc::clone(&live.buffer),
            denied: Arc::clone(&live.denied),
        };
        // **The outgoing sidecar is remembered before it stops being live.** A coalesce is the one
        // publication that builds a *new* one over the same prefix, so from here a generation
        // holding the old one is invisible to the sidecar count reclamation takes — see
        // `superseded_sidecars`. Weakly, and pruned as it goes, so a prefix that coalesces all day
        // accumulates pointers rather than mappings.
        self.superseded_sidecars
            .retain(|held| held.strong_count() > 0);
        self.superseded_sidecars
            .push(Arc::downgrade(&live.external_index));
        let _published = self.publish(next, started);
        self.health.coalesces.fetch_add(1, Ordering::Relaxed);
    }

    /// Apply every completed flush waiting from the pool, and report whether any did.
    ///
    /// Drained after the deny lane and before work, so a publication never delays a suppression
    /// and never waits behind a commit window.
    fn publish_completed_flushes(&mut self) -> bool {
        let mut any = false;
        while let Ok(completed) = self.flush_done.try_recv() {
            let mark = StageMark::now();
            self.publish_flush(completed);
            self.health
                .flush_lap(crate::flush::FlushStage::PublishWall, mark);
            any = true;
        }
        if any {
            // Cleared only after something was drained (never on an empty pass), so a set-and-send
            // landing between this loop's empty `try_recv` and a clear could not be erased — the
            // handshake's other half; see `ExecutorHealth::flush_completed_pending`.
            self.health
                .flush_completed_pending
                .store(false, Ordering::SeqCst);
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
        let mark = StageMark::now();
        let submit = self.flush_submit.clone();
        let Some((partition, partition_data)) = generation.bundle.partitions.iter().next() else {
            return;
        };
        let manifest = &generation.bundle.manifest;
        let scalar_schema = scalar_schema_of(manifest);
        let filter_schema = filter_schema_of(manifest);
        let record_schema = record_schema_of(manifest);
        // **An unusable analyser stops the dispatch rather than flushing an unindexed batch.** A
        // flush that skipped the column would leave the buffer's text out of the index with no
        // error, and the next fold would rebuild from values that are in the blob — so the gap
        // would close silently and look like nothing had happened.
        let text_schema = match text_schema_of(manifest) {
            Ok(schema) => schema,
            Err(e) => {
                self.health.flush_failures.fetch_add(1, Ordering::Relaxed);
                tracing::error!(
                    error = %e.0,
                    "ALARM: a text column's analyser is not one this binary carries; no flush is \
                     dispatched, and the buffer is retained"
                );
                return;
            }
        };
        let render_indices: Vec<usize> = manifest.render_indices().collect();
        // **The group-scoped families, by view** (`views.md` §5), taken once for the dispatch: the
        // schema below is per view, because a family's lanes and columns are its group's views'
        // and no others'.
        let scoped_by_view = scoped_families_by_view(manifest);
        // **One plan per dispatch.** Every context a dispatch builds takes `next_n` from the same
        // unchanging `partition_data`, so they would all write `SEGMENTS-<next_n>.json` at one
        // path and only one could commit. Dispatching one makes that structurally unreachable and
        // saves the losers' segment writes; `write_segments_manifest`'s refuse-to-replace stands
        // behind it at the format boundary. The rest re-plan at the next tick, against a
        // `segments_version` the winner has advanced.
        //
        // **Chosen by oldest unflushed row, not by view name.** `views_of` sorts
        // lexicographically, so taking the first would let a continuously-fed `s0` deny `s1` a
        // flush for ever. `items` is ascending by entity id and I9 issues ids monotonically, so
        // `items.first()` is an age key needing no cursor state — which turns starvation into a
        // bound: with `s` views, ack→visibility is at most `s × flush_max_age_secs`.
        //
        // **Unreachable today**: `tessera build` emits one view, and a plan naming a view this
        // bundle does not carry is dropped just below.
        let deferred = plans.len().saturating_sub(1);
        let Some((view, plan)) = plan_to_dispatch(plans) else {
            return;
        };
        if deferred > 0 {
            tracing::warn!(
                deferred,
                dispatched = %view,
                "a flush unit is per view and every plan in a dispatch shares one side-manifest \
                 name, so one view publishes per tick; the rest re-plan at the next one"
            );
        }

        let mut contexts = Vec::with_capacity(1);
        {
            let Some(view_data) = partition_data.views.get(&view) else {
                return;
            };
            // **This view's frame** (decision 0040): the flush quantises against the extent the
            // view's own positions were placed in, and a bundle-wide one would put a second
            // view's rows on the first's grid. The manifest is the authority for both — a plan
            // naming a view the manifest does not declare is dropped here rather than flushed
            // against a guessed frame, which is the same refusal `accept_ingest` makes upstream.
            // **And this view's incarnation** (decision 0115), resolved from the same manifest
            // and on the same rule: a plan naming a view the manifest does not declare is
            // dropped, never flushed under a guess. The stamp goes on the segment, on every
            // scoped column this flush writes, and on every extent — which is what stops a key
            // created again from adopting them.
            let Some(incarnation) = manifest.incarnation_of(&view) else {
                tracing::error!(
                    view = %view,
                    "a flush plan names a view this bundle's manifest does not declare, so its \
                     incarnation cannot be resolved; the plan is dropped and the buffer is \
                     retained"
                );
                return;
            };
            let Some(quantisation) = manifest.quantisation_of(&view) else {
                tracing::error!(
                    view = %view,
                    "a flush plan names a view this bundle's manifest does not declare, so \
                     there is no frame to quantise its rows against; the plan is dropped \
                     and the buffer is retained"
                );
                return;
            };
            let Ok(row_base) = u32::try_from(view_data.row_space.total_rows()) else {
                // Row ids are `u32` (bundle_format 1). A view that has crossed 2^32 rows cannot
                // take another segment, and saying so is better than wrapping into row 0.
                tracing::error!(
                    view = %view,
                    "ALARM: this view's row space has reached the u32 ceiling; no further flush \
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
            // **This view's scoped families, and where each one's value sits in a buffered row's
            // `scoped` list** (`views.md` §5). Positional against the group's own manifest order,
            // which is the order `/control/ingest` parsed the batch against — one derivation,
            // `scoped_families_by_view`, so the two cannot come to disagree about which value
            // belongs to which family.
            let families = scoped_by_view.get(&view).cloned().unwrap_or_default();
            // **Where those families' columns live** — the owner's view of the same key, which is
            // `view` itself under the owning group's own views (decision 0116).
            let scoped_view = scoped_owner_view_of(manifest, &view);
            let Some(scoped_incarnation) = manifest.incarnation_of(&scoped_view) else {
                // Fail closed (decision 0115): an owner view the manifest cannot place is a
                // bundle whose halves disagree, and flushing under a guessed incarnation is how
                // a dropped view's predecessor adopts rows.
                tracing::error!(
                    view = %scoped_view,
                    "ALARM: no incarnation for the owner view; no flush is planned this tick"
                );
                return;
            };
            let scoped_schema: Vec<crate::flush::ScopedColumnSpec> = match families
                .iter()
                .enumerate()
                .map(|(index, family)| {
                    let text = family.arrow_type == ScalarType::Text;
                    let analyser = if text {
                        Some(std::sync::Arc::new(analyser_of(family)?))
                    } else {
                        None
                    };
                    Ok(crate::flush::ScopedColumnSpec {
                        index,
                        name: family.name.clone(),
                        ty: family.arrow_type,
                        category: family.vocabulary.is_some(),
                        filterable: crate::filter::scoped_is_filterable(family),
                        has_value_column: crate::filter::scoped_has_value_column(family),
                        render: family.render,
                        has_base: family.views.contains(&scoped_view),
                        analyser,
                    })
                })
                .collect::<Result<Vec<_>, crate::flush::FlushFailed>>()
            {
                Ok(schema) => schema,
                Err(e) => {
                    // The same refusal `text_schema_of` makes, for its reason: a flush that
                    // indexed prose with a pipeline the base was not built by leaves one column
                    // whose two layers disagree about what a word is.
                    self.health.flush_failures.fetch_add(1, Ordering::Relaxed);
                    tracing::error!(
                        error = %e,
                        "ALARM: a group-scoped text family's analyser is not one this binary \
                         carries; no flush is dispatched, and the buffer is retained"
                    );
                    return;
                }
            };
            // **The lanes this view's rows carry.** Two cases, and the split is which side of the
            // family's `views` list this flush is on.
            //
            // Under a view of the family's **own** group, every rendered family of the group gets
            // a lane whether or not the manifest already lists the view: this flush is what gives
            // the view its column, and a lane withheld until the manifest agreed would drop the
            // very batch that acquired it. Publication adds the pair, so every later flush, merge
            // and fold of this view derives the same list from `view_scalar_schema_of`.
            //
            // A sharing group's view is on the same side of that split since decision 0116 — it
            // writes the family through the key it shares — so it takes the same branch and the
            // same argument. Under any other view the read side's list is exactly right: a view in
            // no scope at all owes a lane of absences rather than no lane, because a segment
            // missing one is a segment its own view's rewriters would have to guess about.
            let scoped_render: Vec<tessera_store::manifest::ScopedScalar> = if families.is_empty() {
                crate::viewport::scoped_render_families(manifest, &view)
                    .into_iter()
                    .cloned()
                    .collect()
            } else {
                families.iter().filter(|f| f.render).cloned().collect()
            };

            // Where each lane's value sits in a buffered row's `scoped` list, `None` where this
            // view writes none of them — since decision 0116 that is a view whose key is in no
            // scope at all, a sharing group's writing the family through the key it shares, and
            // any family the batch could not have named.
            let scoped_render_indices: Vec<Option<usize>> = scoped_render
                .iter()
                .map(|lane| families.iter().position(|f| f.name == lane.name))
                .collect();
            contexts.push((
                plan,
                crate::flush::FlushContext {
                    prefix_dir: self.prefix_dir(generation),
                    partition: partition.clone(),
                    view: view.clone(),
                    scoped_view,
                    scoped_incarnation,
                    incarnation,

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
                    quantisation,
                    // **This view's schema, entity-scoped tail then scoped render lanes** — the
                    // same list a merge and a fold of this view take (`view_scalar_schema_of`),
                    // so a segment written by any of the three carries the same columns.
                    //
                    // **The two derivations agree only because a `members` group can never own a
                    // family** (`Manifest::validate_groups` refuses one, `views.md` §3.3): under a
                    // view whose key is in a scope — the owner's own, or a sharing group's of the
                    // same key — `scoped_render` is the owning group's rendered families in
                    // manifest order, which is exactly what `scoped_render_families` yields there
                    // once publication has put the owner view id on each family's list; under any
                    // other view the branch above *is* that function. Change either site — or that
                    // refusal — and the third has to move with it, or a flush writes a tail its own
                    // view's rewriters cannot read.
                    scalar_schema: {
                        let mut schema = scalar_schema.clone();
                        schema.extend(scoped_render.iter().map(|f| (f.name.clone(), f.arrow_type)));
                        schema
                    },
                    render_indices: render_indices.clone(),
                    scoped_schema,
                    scoped_render: scoped_render_indices,
                    filter_schema: filter_schema.clone(),
                    record_schema: record_schema.clone(),
                    text_schema: text_schema.clone(),
                    dict: Arc::clone(&generation.dict),
                    novel_descriptors,
                    max_distinct_terms: self.max_distinct_terms,
                    prefix: generation.prefix.clone(),
                    shapes: self.shapes.levels_of_view(&view),
                },
            ));
        }
        if contexts.is_empty() {
            return;
        }
        self.health
            .flush_lap(crate::flush::FlushStage::Dispatch, mark);

        self.health.flush_in_flight.store(true, Ordering::SeqCst);
        self.health.mark_flush_started(std::time::Instant::now());
        let health = Arc::clone(&self.health);
        self.pool.spawn(move || {
            for (plan, ctx) in contexts {
                let mut laps = crate::flush::FlushLaps::default();
                match crate::flush::execute_flush(plan, ctx, &mut laps) {
                    Ok(completed) => {
                        health.record_flush_execution(&laps, Some(completed.consumed.len()));
                        // **Pending is set before the send** — the completion handshake's whole
                        // ordering; see `ExecutorHealth::flush_completed_pending`.
                        health.flush_completed_pending.store(true, Ordering::SeqCst);
                        // A send failure means the executor is gone, which is a shutdown and not a
                        // fault: the files are orphans nothing references, and replay re-flushes.
                        let _ = submit.send(completed);
                    }
                    Err(e) => {
                        health.record_flush_execution(&laps, None);
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
            health.flush_in_flight.store(false, Ordering::SeqCst);
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
        } else if self.health.flush_in_flight.load(Ordering::SeqCst)
            || self.health.flush_completed_pending.load(Ordering::SeqCst)
            || self.coalesce_in_flight.load(Ordering::SeqCst)
            || self
                .health
                .coalesce_completed_pending
                .load(Ordering::SeqCst)
            || self.merge_in_flight.load(Ordering::SeqCst)
            || self.health.merge_completed_pending.load(Ordering::SeqCst)
            || self.health.fold_completed_pending.load(Ordering::SeqCst)
        {
            // A flush or a coalesce is executing on the pool, or its completed unit is waiting in
            // the corresponding channel. The pool cannot ring the doorbell (see `flush_submit`),
            // so this poll is what bounds publication latency on an idle node — see
            // `FLUSH_COMPLETION_POLL`. Without the coalesce arm an idle node's completed coalesce
            // waits for the next *tick*, which at a 90 s period is 90 s of a pass that has already
            // done all of its IO sitting unpublished.
            until_tick.min(FLUSH_COMPLETION_POLL)
        } else if self.fold_in_flight.load(Ordering::SeqCst) {
            // A fold is running on its own thread, which — like the pool — cannot ring the
            // doorbell. Its completion has to be noticed by polling, and the *only* reason this is
            // a separate, coarser interval from the arm above is duration: a fold runs for minutes
            // to hours where a flush runs for seconds, so the 20 ms poll would spin the loop
            // hundreds of thousands of times for one publication whose latency nobody observes.
            until_tick.min(FOLD_COMPLETION_POLL)
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
    /// shrinks only at a fold — so an N-item revocation
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
            let Command::Change { entity, op } = command else {
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
                record: WalRecord::ChangeByEntity {
                    entity_id: entity,
                    op,
                },
                entity,
                op,
                respond: Some(respond),
            });
        }

        if entries.is_empty() {
            return false;
        }
        self.cascade_dependents(&mut entries);
        self.commit_denies(entries);
        true
    }

    /// Add a deletion for every artifact that depends on one this window deletes
    /// ([decision 0089](../../../docs/decisions/0089-a-dependency-edge-carries-deletion-and-visibility.md),
    /// rule 1).
    ///
    /// **Extra entries in the window, and nothing else.** A cascaded deletion is a deletion: it
    /// gets its own `ChangeByEntity` record in the same append, is applied to the same overlay
    /// clone, hides its artifact at the same ack, and retires at the compaction fold that executes
    /// it — Rule F (write-path §5.4), by the same route as the deletion that caused it. There is no
    /// second removal rule here and there must never be one; a cascade that retired anywhere else
    /// is the fail-open two removal rules have been conflated into twice already.
    ///
    /// **Before the append, so a restart agrees with the live node.** The records are in the log,
    /// so replay rebuilds the same overlay rather than re-deriving the cascade from a store whose
    /// edges a later publication may have changed.
    ///
    /// Only `Delete` cascades. A suppression is reversible and retires only on unsuppress (Rule S),
    /// so cascading one would need an inverse nothing carries — and the dependent is withheld while
    /// its target is suppressed anyway, by the serving predicate's dependency term rather than by
    /// any state.
    fn cascade_dependents(&mut self, entries: &mut Vec<DenyEntry>) {
        let deleted: Vec<EntityId> = entries
            .iter()
            .filter(|e| matches!(e.op, ChangeOp::Delete))
            .map(|e| e.entity)
            .collect();
        if deleted.is_empty() {
            return;
        }
        let cascade = self
            .live
            .with_artifacts(|store| store.cascade_from(&deleted));
        for entity in cascade {
            entries.push(DenyEntry {
                record: WalRecord::ChangeByEntity {
                    entity_id: entity,
                    op: ChangeOp::Delete,
                },
                entity,
                op: ChangeOp::Delete,
                respond: None,
            });
        }
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
    /// - every [`ChangeOp::Unsuppress`] applies **nothing** — the whole non-deny class, since
    ///   decision 0048 deleted `Predicate`.
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
            let applied: Vec<(EntityId, ChangeOp)> = entries
                .iter()
                .filter(|e| matches!(e.op, ChangeOp::Delete | ChangeOp::Suppress))
                .map(|e| (e.entity, e.op))
                .collect();
            if !applied.is_empty() {
                // **Deliberately does not mark the overlay dirty.** These entries were applied
                // under the apply-anyway rule and then answered 500 — they are in force in memory
                // with no durable record behind them, and contracts §3.1's residual is that a
                // restart drops them. Publishing them would make a never-acked deny permanent on
                // every restore, which is the fail-open `overlay_diverged`'s gate also guards. The
                // gate would refuse this window anyway; not setting the flag is the primary
                // reason it never arises.
                let _published = self.apply_changes(applied);
            }
            let mut real = Some(error);
            for (i, entry) in entries.into_iter().enumerate() {
                let e = if i == index {
                    real.take().unwrap_or(WalError::Poisoned)
                } else {
                    WalError::Poisoned
                };
                if let Some(respond) = &entry.respond {
                    self.ack_failed(respond, ExecError::Wal(e));
                }
            }
            return;
        }

        // Durable, not yet in force. See `pause_point`.
        self.pause_point(PauseSiteArg::AfterFsync);

        let applied: Vec<(EntityId, ChangeOp)> = entries.iter().map(|e| (e.entity, e.op)).collect();

        // One overlay clone, one generation, **one swap** for every entry in the window.
        //
        // **Every window owes the disc a publication.** There used to be a test here for whether
        // any entry touched deny state, because a `Predicate` change's durable home was the WAL
        // alone and no manifest field carried one; with that op deleted (decision 0048) each of the
        // three remaining ops moves state a `SEGMENTS-<n>.json` carries, and a window is never
        // empty — `commit_denies` is only ever called with entries.
        let published = self.apply_changes(applied);
        self.deny_dirty = true;
        self.windows_since_publication += 1;
        // The liveness floor: a drain that never closes still publishes. See
        // `OVERLAY_PUBLICATION_MAX_WINDOWS`.
        if self.windows_since_publication >= OVERLAY_PUBLICATION_MAX_WINDOWS {
            self.publish_overlay_state();
        }

        // **k waiters, one proof.** A death partway through this loop leaves some waiters acked and
        // some not; every un-acked one gets `SubmitError::ReceiptLost` → 500, never `ExecutorDead`
        // → 503, because its change is durably in force.
        for entry in entries {
            if let Some(respond) = &entry.respond {
                self.ack(respond, Ack::Changed, &published);
            }
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
    /// ## Why there is no age bound, and why the config key is deleted
    ///
    /// The specified third trigger is `opened_at.elapsed() >= commit_window_max_age_ms`, whose
    /// stated purpose is to stop a lone ingest on an idle server waiting the full window age
    /// "for company that is not coming". **It is declined, and `ingest.commit_window_max_age_ms`
    /// is deleted** (docs/decisions/0034-the-window-does-not-linger.md carries the no-linger
    /// argument; docs/decisions/0045-inert-config-keys-are-deleted.md the key's removal).
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
    /// `IngestBuffer` clone that is O(total buffered items) — the dominant term, bounded by
    /// `ingest_buffer_max_items` now that flush drains it (see [`Executor::apply_window`]). That is why
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
                    publication,
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
                    let _ = respond.send(self.publish_geometry(publication));
                    self.health.note_work_refused();
                    did_work = true;
                    continue;
                }
                #[cfg(feature = "fault-injection")]
                ExecutorWork::ForgetSuggestionIndex {
                    vocabulary,
                    respond,
                } => {
                    // The open window closes first, on the arm above's reasoning exactly: this
                    // swaps the whole generation, and doing it under a window that has not applied
                    // its ingest would have the window's own swap carry the pre-drop indexes
                    // forward — losing the drop, and leaving the test asserting against a state it
                    // asked to leave.
                    if !window.is_empty() {
                        window = self.close_and_reopen(window);
                    }
                    self.forget_suggestion_index(&vocabulary);
                    let _ = respond.send(());
                    did_work = true;
                    continue;
                }
                #[cfg(feature = "fault-injection")]
                ExecutorWork::RebuildSuggestionIndex {
                    vocabulary,
                    respond,
                } => {
                    // The open window closes first, on the arm above's reasoning: the rebuild reads
                    // the live minter, and a window holding an ingest that mints has not published
                    // its value yet — so a rebuild taken under it would omit exactly the value the
                    // caller asked for the rebuild to pick up.
                    if !window.is_empty() {
                        window = self.close_and_reopen(window);
                    }
                    self.rebuild_suggestion_index_now(&vocabulary);
                    let _ = respond.send(());
                    did_work = true;
                    continue;
                }
            };
            let Job { command, respond } = job;
            let Command::Ingest {
                rows,
                batch_id,
                body_hash,
                artifacts,
            } = command
            else {
                // **Every command but `Ingest` and `Change` arrives here**, which is the layer
                // registrations, the publications and the growths: the lane follows the command
                // (`Command::is_never_shed`) and only a `Change` takes the deny queue. A `Change`
                // itself is therefore unreachable, and is executed rather than dropped so that a
                // future variant is answered instead of silently losing its waiter.
                //
                // **This arm applies immediately, while a window holding earlier-arriving ingest is
                // still open**, so WAL append order stops equalling submission order. That is
                // tolerable for the four variants that reach it — each appends, fsyncs and applies
                // its own record, none touches the buffer or swaps the generation, and neither
                // registry nor artifact state depends on ingest that has not been allocated. It is
                // **not** tolerable for a deny-shaped variant, whose out-of-order apply is what
                // lifecycle §4 is written against: such a variant must close the window first.
                //
                // One consequence is load-bearing elsewhere: a publication executing here **claims
                // ordinals from a level's cursor while a window is open**, which is why an ingest
                // batch's minted key claims its own at the *close* and not at admission
                // (`Executor::mint_records`).
                self.execute(Job { command, respond });
                // This job was counted at submission on the work lane and `execute` counts nothing,
                // so it is counted here or `work_depth` drifts up one per occurrence forever — the
                // drift `note_work_refused` exists to prevent.
                self.health.note_work_refused();
                did_work = true;
                continue;
            };

            let admitted;
            let m = StageMark::now();
            (window, admitted) =
                self.admit_ingest(window, rows, batch_id, body_hash, artifacts, respond);
            self.health.lap(WriteStage::AdmitWindow, m);
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

    /// The prefix directory to write into, **derived from the generation the caller is publishing
    /// against** rather than remembered.
    ///
    /// This is compaction §4's fourth gap, closed by construction. Every write inside a bundle
    /// belongs to one prefix, and which prefix that is changes when a fold flips `CURRENT`. The
    /// alternative — a stored `PathBuf` rotated at the flip — has to be got right at all eight
    /// sites that use it, and the one that would be missed is not the flush path anybody would
    /// think to check: it is [`Executor::publish_deny_state`], where the first deny published
    /// after a flip writes its side-manifest into the prefix reclamation is about to delete. Acked
    /// deny state, absent from the restore path, no error anywhere. A derived value cannot be
    /// missed.
    ///
    /// Every caller already holds the generation it is acting on — publications load it to rebase
    /// against, and the maintenance planners load it to plan from — so this costs one `join` and
    /// no lookup.
    fn prefix_dir(&self, generation: &Generation) -> PathBuf {
        self.bundle_root.join(&generation.prefix)
    }

    /// Take the next side-manifest number. See [`Executor::next_manifest_n`].
    fn allocate_manifest_n(&mut self) -> u64 {
        let n = self.next_manifest_n;
        self.next_manifest_n += 1;
        n
    }

    /// Commit one partition's side-manifest — **the only route to
    /// `tessera_store::write_segments_manifest` in this crate**, and the durable half of the
    /// publication guard.
    ///
    /// `crate::geometry::check_publishable` refuses a *generation* that regresses the watermark,
    /// but every publication writes its manifest before it swaps, and assembles it by editing a
    /// clone of the live one — a seam the swap guard cannot see, and the one through which the
    /// merge's rebase regressed the durable watermark by a batch whenever a flush shared its
    /// flight. So the manifest's own ordered scalars are checked against `live_manifest` — the
    /// live partition manifest at this publication — here, where every publication converges
    /// (see [`crate::geometry::check_manifest_publishable`] for what is compared and why). A
    /// refusal leaves each caller its usual failure posture: nothing written, files orphaned,
    /// the next tick re-plans.
    ///
    /// **The artifact coordinates are stamped here rather than by each caller**, which is the same
    /// argument the watermark check above rests on: every publication converges on this function,
    /// and a level's version list assembled at five call sites is one that goes stale at whichever
    /// of them nobody thought about. `containment` and `tile_indexes` are the derived-structure
    /// lists the caller wants named — the fold's freshly written ones, everyone else's held ones —
    /// and what lands in the manifest is each list filtered to the entries the store's versions
    /// still make adoptable ([`artifact_coordinates`]). Read `next.containment_extents` and
    /// `next.tile_index_extents` back after a success to keep the held lists in step.
    // Eight, plus the manifest being written. Every one of them is a thing this publication *is* —
    // where it goes, what it replaces, and the three artifact coordinates it has to stamp.
    // Bundling them would name the same nine things one call earlier.
    #[allow(clippy::too_many_arguments)]
    fn commit_side_manifest(
        &self,
        live_manifest: &tessera_store::manifest::SegmentsManifest,
        prefix_dir: &std::path::Path,
        partition: &str,
        n: u64,
        next: &mut tessera_store::manifest::SegmentsManifest,
        containment: &[tessera_store::manifest::ContainmentExtent],
        tile_indexes: &[tessera_store::manifest::TileIndexExtent],
        row_columns: &[tessera_store::manifest::RowColumnExtent],
        shape_rows: &[tessera_store::manifest::ShapeRowsExtent],
        shape_held: &[tessera_store::manifest::ShapeHeldExtent],
        pending_retirement: &[(String, u32)],
    ) -> Result<(), ManifestCommitRefused> {
        let coordinates = self.live.with_artifacts(|store| {
            artifact_coordinates(
                store,
                containment,
                tile_indexes,
                row_columns,
                shape_rows,
                shape_held,
                pending_retirement,
            )
        });
        next.level_versions = coordinates.level_versions;
        next.containment_extents = coordinates.containment;
        next.tile_index_extents = coordinates.tile_indexes;
        next.row_column_extents = coordinates.row_columns;
        next.shape_rows_extents = coordinates.shape_rows;
        next.shape_held_extents = coordinates.shape_held;
        crate::geometry::check_manifest_publishable(live_manifest, next)
            .map_err(ManifestCommitRefused::Regresses)?;
        tessera_store::write_segments_manifest(prefix_dir, partition, n, next)
            .map_err(ManifestCommitRefused::Store)
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
        artifacts: tessera_lifecycle::BatchArtifacts,
        respond: Responder,
    ) -> (CommitWindow<Responder>, Admission) {
        match BatchState::of(&self.live, &window, &batch_id) {
            BatchState::Accepted {
                body_hash: prev_hash,
                entity_ids,
            } => {
                if prev_hash == body_hash {
                    let proof = Published::already_in_force(&entity_ids);
                    // **A replay mints nothing, and the zero says so**: the artifacts this batch's
                    // keys created were created when it was first accepted, and this submission
                    // created none.
                    self.ack(
                        &respond,
                        Ack::Ingested {
                            entity_ids,
                            minted: 0,
                        },
                        &proof,
                    );
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
                if let Some(entry) = self.admit(rows, batch_id, body_hash, artifacts, respond) {
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
    ///
    /// ## The membership column's keys resolve here too, and a bad one refuses this batch alone
    ///
    /// A batch naming an artifact that does not exist **on a closed layer** is refused naming the
    /// key, and nothing it carried is admitted — the standard `/control/ingest` refusals are held
    /// to, and the standard one for a growth (`artifacts-from-points.md` §6.1: the whole batch or
    /// none of it). It happens **here** rather than at the close because a window holds several
    /// callers' batches: one caller's typo may not refuse another caller's rows, and after the
    /// allocation there is no per-entry refusal left to make.
    ///
    /// **On an open layer the same key mints**, and the ordinal it will hold is *not* claimed here:
    /// see `Executor::mint_records` for why the claim belongs at the close. What is decided here is
    /// everything about that key which can still refuse one batch on its own — whether the layer's
    /// declaration admits an artifact carrying nothing but a name, and whether the batch's own
    /// column named one child under two parents.
    ///
    /// An ordinal a key already resolves to is carried from here rather than re-derived at the
    /// close — see [`tessera_lifecycle::ResolvedMembership`] for why that is safe and what it means
    /// when the artifact has gone by then.
    fn admit(
        &mut self,
        rows: Vec<UnallocatedRow>,
        batch_id: String,
        body_hash: [u8; 32],
        artifacts: tessera_lifecycle::BatchArtifacts,
        respond: Responder,
    ) -> Option<WindowEntry<Responder>> {
        // The fail-closed backstop for the widened check-to-apply race — see
        // `LiveState::established_collisions`. The overlay read here is the same generation the
        // apply below will clone from, on the same thread, so the deleted-holder exemption cannot
        // race its own delete.
        let generation = self.generation.load();
        let mut rows = rows;
        let collisions = self.live.established_collisions(
            &mut rows,
            |e| generation.overlay.is_deleted(e),
            |entity, view| {
                // The same predicate the handler answered with, read from the generation this
                // apply will clone from: the view's permutation, and the buffer beside it for the
                // rows an earlier window accepted and no flush has taken yet.
                generation.bundle.partitions.values().any(|partition| {
                    partition
                        .views
                        .get(view)
                        .is_some_and(|data| data.row_space.row_of(entity).is_some())
                }) || generation.buffer.contains_in_view(entity, view)
            },
        );
        // **The join rule's arms, on the one thread that settles join-ness** (`views.md` §4, §5;
        // decision 0116). They used to run in `/control/ingest`'s handler, a whole queue drain
        // before `established_collisions` above decided which rows are joins — so a row whose
        // holder was established in between was admitted as a join having passed no arm at all.
        // One authoritative site, and the refusal text is the handler's own so the bodies are
        // byte-identical to what the earlier site answered. `settle_joins` also completes an
        // accepted join — dropping its descriptors and terms, backfilling its omitted `render`
        // values — because that is the same per-row pass over the same sources.
        if collisions == 0 {
            if let Err(detail) = settle_joins(&generation, &mut rows) {
                drop(generation);
                self.ack_failed(&respond, ExecError::JoinRefused { detail });
                self.health.note_work_refused();
                return None;
            }
        }
        drop(generation);
        if collisions > 0 {
            self.ack_failed(
                &respond,
                ExecError::DuplicateExternalId { count: collisions },
            );
            self.health.note_work_refused();
            return None;
        }

        let (memberships, edges) = match self.resolve_memberships(&artifacts) {
            Ok(resolved) => resolved,
            Err(detail) => {
                self.ack_failed(&respond, ExecError::LayerRefused { detail });
                self.health.note_work_refused();
                return None;
            }
        };

        Some(WindowEntry {
            rows,
            batch_id,
            body_hash,
            memberships,
            edges,
            waiters: vec![respond],
        })
    }
}

/// What one accepted `POST /control/values` batch did (`ingest.md` §1.4). Every count is bounded
/// by the caller's own request and names no entity and no value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValuesReceipt {
    pub filled: u64,
    pub held: u64,
    pub joined: u64,
}

/// What one values batch's fill rule produced: the cells to hold until the flush writes them, and
/// the counts the acknowledgement carries.
struct PlannedFills {
    /// The entity-scoped cells, one entry per entity.
    fills: Vec<(EntityId, tessera_lifecycle::Fill)>,
    /// The group-scoped cells, one entry per `(entity, owner view)` — the address a scoped value
    /// has, and never the entity alone (`views.md` §5).
    scoped_fills: Vec<(EntityId, String, tessera_lifecycle::ScopedFill)>,
    filled: u64,
    held: u64,
}

/// Where one of a values batch's named columns lands in a row's two positional spaces.
enum ValuesColumn {
    /// A position in `MANIFEST.declared_scalars`, the space a buffered row's `scalars` is
    /// positional against.
    Entity(usize),
    /// A position in the view's group-scoped families, the space its `scoped` list is positional
    /// against (`views.md` §5).
    Scoped(usize),
}

/// Apply the fill rule to one values batch (`ingest.md` §1.1, §1.4), producing the cells nothing
/// holds and refusing on the first cell that is held differently.
///
/// **Three sources, in the order a cell is claimed.** The entity's own buffered row, the cells an
/// earlier values batch filled and no flush has written yet, and the flushed homes — the same
/// three the join arm reads, plus the unflushed fills, which exist only on this route. A cell
/// this pass leaves absent has no claimant in any of them, which is what makes the extents
/// disjoint per column when the flush writes them.
///
/// **A row index, a column name and a key reach the caller; nothing else does.** No entity id, no
/// external id and no value on either side (**I10**, `ingest.md` §1.4).
fn plan_fills(
    generation: &Generation,
    request: &tessera_lifecycle::ValuesRequest,
) -> Result<PlannedFills, ExecError> {
    let manifest = &generation.bundle.manifest;
    let declared = &manifest.declared_scalars;
    let families = scoped_families_of_view(manifest, &request.view);
    let owner_view = scoped_owner_view_of(manifest, &request.view);
    // The cell's key, for a refusal — the half of the owner view id a caller spelled, never the
    // owning group, which a sharing group's caller has no business learning from a refusal
    // (`settle_joins`' rule).
    let key = owner_view
        .split_once(tessera_store::GROUP_SEPARATOR)
        .map(|(_, key)| key)
        .unwrap_or(owner_view.as_str())
        .to_string();

    // One resolution per batch, not per row. A name in neither space is refused here as well as
    // at the door: the door reads the served schema of a generation this pass may have moved past.
    // **A `render` column cannot be filled** (`ingest.md` §6.3). A fill acquires no row, so it
    // never reaches the hot column, and the hot column is the only home a tile and the drill-down
    // read a rendered value from. Two shapes, refused for two reasons:
    //
    // - **`render` alone** — not `index`, and not a `derived` category — has *no home at all*
    //   for a fill: `owes_value_column` is false, so there is no entity-space column, and
    //   `blob_resident` is false because the column renders, so there is no blob row either. The
    //   value would be acknowledged and stored nowhere.
    // - **`render` with `index`**, and a rendered `derived` category, *do* have an entity-space
    //   column, so a fill would be stored and would answer a filter that took the entity route.
    //   It would still draw absent on every tile and at the drill-down, and answer nothing to a
    //   filter whose request made the row route cheaper — which is a per-request cost choice
    //   (decision 0068, `FilterColumns::leaf_space`'s `prefer_row`), not a property of the
    //   column. One column answering two ways depending on the shape of the request is the
    //   reason this is refused rather than half-served.
    //
    // **The guard stands although `PUT /control/attributes` now refuses `render`** (decision
    // 0136's amendment, 2026-09-08). That door closes the runtime half: no column declared at a
    // running service carries the flag. A column the *build* declared `render` still reaches
    // here, which is every rendered column a deployment has, so this is the arm that fires.
    //
    // R10, which read a back-filled `render` value as filterable where `index` was declared, is
    // withdrawn. It was true of the entity route and said nothing about the value being drawn
    // nowhere, which is what this refusal keeps the two surfaces from disagreeing over.
    let row_tail_only = |d: &tessera_store::manifest::DeclaredScalar| d.render;
    let mut columns = Vec::with_capacity(request.columns.len());
    for name in &request.columns {
        if let Some(position) = declared.iter().position(|d| &d.name == name) {
            if row_tail_only(&declared[position]) {
                return Err(ExecError::ValuesRefused {
                    detail: format!(
                        "column '{name}' is declared `render`, and a rendered value is drawn \
                         from the hot column of the row that carries it. A values row acquires no \
                         row, so the value would be drawn on no tile and at no drill-down — and \
                         where the column is not also `index` it would be stored nowhere at all. \
                         Re-ingest the point, or declare the column without `render` \
                         (`ingest.md` §6.3)"
                    ),
                });
            }
            columns.push(ValuesColumn::Entity(position));
            continue;
        }
        if let Some(position) = families.iter().position(|f| &f.name == name) {
            if row_tail_only(&crate::session::declared_of_scoped(&families[position])) {
                return Err(ExecError::ValuesRefused {
                    detail: format!(
                        "group-scoped column '{name}' is declared `render`, and a rendered value \
                         is drawn from the hot column of the row that carries it, which a values \
                         row does not acquire (`ingest.md` §6.3)"
                    ),
                });
            }
            columns.push(ValuesColumn::Scoped(position));
            continue;
        }
        return Err(ExecError::ValuesRefused {
            detail: format!(
                "column '{name}' is neither in MANIFEST.declared_scalars nor a group-scoped \
                 family whose key set holds view '{}' (`views.md` §5). Declare the column, or \
                 name the view whose key addresses the cell",
                request.view
            ),
        });
    }

    let mut fills = Vec::with_capacity(request.rows.len());
    let mut scoped_fills = Vec::new();
    let mut filled = 0u64;
    let mut held_count = 0u64;
    for (index, row) in request.rows.iter().enumerate() {
        let entity = row.entity;
        if generation.overlay.is_deleted(entity) {
            return Err(ExecError::ValuesRefused {
                detail: format!(
                    "row {index} names an entity this deployment has deleted. A deletion is not \
                     undone by a fill (decision 0047): the item is re-ingested, which allocates a \
                     fresh entity"
                ),
            });
        }
        let buffered = generation.buffer.get(entity);
        // The cells an earlier batch filled and no flush has written. Read as a claimant beside
        // the other two: a cell filled at the last tick is held, not absent. **A lookup, not a
        // scan** — this is asked once per row and a batch runs to `max_batch_rows`.
        let pending = generation.buffer.fill_of(entity);
        let pending_scoped = generation.buffer.scoped_fill_of(entity, &owner_view);
        // Read at most once for this row, and only if a blob-resident column asks.
        let mut blob = crate::session::BlobRow::default();
        // **Absence in a fill's tails is `WalScalar::Null` for every family, a category
        // included.** A category's own spelling of absence is the reserved code (decision 0064),
        // which the flush's gather maps `Null` onto — and using it here would make a merge of two
        // fills unable to tell an unfilled category cell from a filled one, the reserved code
        // being an ordinary `u8` to any predicate over the value alone.
        let mut scalars: Vec<WalScalar> = vec![WalScalar::Null; declared.len()];
        let mut scoped: Vec<WalScalar> = vec![WalScalar::Null; families.len()];
        let mut any_entity = false;
        let mut any_scoped = false;

        for (position, column) in columns.iter().enumerate() {
            let Some(supplied) = row.values.get(position) else {
                continue;
            };
            match column {
                ValuesColumn::Entity(at) => {
                    let d = &declared[*at];
                    if crate::session::scalar_is_absent(supplied, d) {
                        continue;
                    }
                    // **Each source is asked for a *held* value, not for a slot** — the shape
                    // `settle_joins` reads its arms in. A buffered row that carries the position
                    // and holds the column's absence answers `Some(absence)`, so a chain that
                    // short-circuited on `Some` would stop there and never reach the pending fill
                    // or the flushed home: a cell already filled would read as absent, be filled
                    // a second time, and the second value would be dropped at the merge after
                    // being acknowledged.
                    let held = |value: WalScalar| {
                        (!crate::session::scalar_is_absent(&value, d)).then_some(value)
                    };
                    let stored = buffered
                        .and_then(|item| item.scalars.get(*at).cloned())
                        .and_then(held)
                        .or_else(|| {
                            pending
                                .and_then(|fill| fill.scalars.get(*at).cloned())
                                .and_then(held)
                        })
                        .or_else(|| {
                            crate::session::flushed_scalar_of(generation, entity, *at, &mut blob)
                                .and_then(held)
                        });
                    match stored {
                        None => {
                            scalars[*at] = supplied.clone();
                            filled += 1;
                            any_entity = true;
                        }
                        Some(stored) if stored == *supplied => held_count += 1,
                        Some(_) => {
                            return Err(ExecError::ValueConflict {
                                detail: format!(
                                    "row {index} supplies a value for column '{}' that this \
                                     deployment already holds a different one for. An \
                                     entity-scoped attribute is one value per entity, so a values \
                                     row fills a cell that is absent, restates the value held, or \
                                     is refused; changing it is a delete plus a re-ingest \
                                     (decision 0047, `ingest.md` §1.1)",
                                    d.name
                                ),
                            })
                        }
                    }
                }
                ValuesColumn::Scoped(at) => {
                    let family = &families[*at];
                    let d = crate::session::declared_of_scoped(family);
                    if crate::session::scalar_is_absent(supplied, &d) {
                        continue;
                    }
                    // Every buffered row of the entity whose view addresses this same key — the
                    // cell's own rows, not the entity's own row, which is a different question
                    // (`settle_joins`' scoped arm). Each source answers a *held* value on the
                    // entity-scoped arm's rule: `find_map` over the slot alone would stop at the
                    // first row that carries the position, absence included, and an entity with
                    // two buffered rows under one key would then hide a value the second holds.
                    let held = |value: WalScalar| {
                        (!crate::session::scalar_is_absent(&value, &d)).then_some(value)
                    };
                    let stored = generation
                        .buffer
                        .rows_of(entity)
                        .filter(|item| scoped_owner_view_of(manifest, &item.view) == owner_view)
                        .find_map(|item| item.scoped.get(*at).cloned().and_then(held))
                        .or_else(|| {
                            pending_scoped
                                .and_then(|fill| fill.scoped.get(*at).cloned())
                                .and_then(held)
                        })
                        .or_else(|| {
                            crate::session::flushed_scoped_of(
                                generation,
                                entity,
                                family,
                                &owner_view,
                            )
                            .and_then(held)
                        });
                    // **A `text` family past a flush is refused rather than compared**
                    // (`views.md` §5): the column stores a dictionary, postings and a presence
                    // bitmap and no value per entity, so there is nothing to compare a supplied
                    // string against, and admitting it would write a second text layer stamped
                    // with the same view that `match` unions across. Occupancy is asked instead.
                    if stored.is_none()
                        && family.arrow_type == ScalarType::Text
                        && crate::session::flushed_scoped_text_present(
                            generation,
                            entity,
                            family,
                            &owner_view,
                        )
                    {
                        return Err(ExecError::ValueConflict {
                            detail: format!(
                                "row {index} supplies a value for group-scoped column '{}', and \
                                 this deployment already holds prose for key '{key}'. A `text` \
                                 family's stored value cannot be compared once it has flushed, so \
                                 a cell that holds prose takes no second one, equal or not \
                                 (views §5)",
                                family.name
                            ),
                        });
                    }
                    match stored {
                        None => {
                            scoped[*at] = supplied.clone();
                            filled += 1;
                            any_scoped = true;
                        }
                        Some(stored) if stored == *supplied => held_count += 1,
                        Some(_) => {
                            return Err(ExecError::ValueConflict {
                                detail: format!(
                                    "row {index} supplies a value for group-scoped column '{}' \
                                     that this deployment already holds a different one for under \
                                     key '{key}'. A scoped value is addressed by (attribute, key) \
                                     and is one value per cell, so a values row fills a cell that \
                                     is absent, restates the value held, or is refused \
                                     (views §5, `ingest.md` §1.1)",
                                    family.name
                                ),
                            })
                        }
                    }
                }
            }
        }
        if any_entity {
            fills.push((
                entity,
                tessera_lifecycle::Fill {
                    view: request.view.clone(),
                    scalars,
                    wal_pos: None,
                },
            ));
        }
        if any_scoped {
            scoped_fills.push((
                entity,
                owner_view.clone(),
                tessera_lifecycle::ScopedFill {
                    view: request.view.clone(),
                    scoped,
                    wal_pos: None,
                },
            ));
        }
    }
    Ok(PlannedFills {
        fills,
        scoped_fills,
        filled,
        held: held_count,
    })
}

/// The growth records one values batch's layer columns produce (`ingest.md` §1.4).
///
/// **A key no artifact holds refuses the batch.** A values batch allocates nothing and creates
/// nothing, so there is no minting arm here: the caller publishes the artifact and then names it.
fn values_growth_records(
    memberships: &[tessera_lifecycle::ResolvedMembership],
    rows: &[tessera_lifecycle::IncomingValues],
) -> Result<Vec<WalRecord>, String> {
    use std::collections::BTreeMap;
    // Ordered, so the records a batch appends do not depend on hash iteration order: two nodes
    // replaying one log must read the same sequence.
    let mut by_level: BTreeMap<(&str, u32), BTreeMap<u32, croaring::Bitmap>> = BTreeMap::new();
    for join in memberships {
        let Some(ordinal) = join.ordinal else {
            return Err(format!(
                "column '{}' names the key '{}', which no artifact of level {} holds. A values \
                 batch creates nothing (`ingest.md` §1.4): publish the artifact, then name it",
                join.layer, join.key, join.level
            ));
        };
        let joining = by_level
            .entry((join.layer.as_str(), join.level))
            .or_default()
            .entry(ordinal)
            .or_default();
        for row in &join.rows {
            let Some(entity) = rows.get(*row as usize) else {
                return Err(format!(
                    "column '{}' names row {row}, which this batch does not carry",
                    join.layer
                ));
            };
            // Entity space is `u32` by I9, so the narrowing is total.
            joining.add(entity.entity.raw() as u32);
        }
    }
    Ok(by_level
        .into_iter()
        .filter_map(|((layer, level), ordinals)| {
            tessera_lifecycle::membership::growth_record(
                layer,
                level,
                ordinals
                    .iter()
                    .map(|(ordinal, joining)| (*ordinal, joining)),
            )
        })
        .collect())
}

/// How many of one growth record's joining members the artifacts do not already hold — read
/// **before** the record is applied, which is the only time the difference exists.
fn new_members_of(record: &WalRecord, store: &tessera_lifecycle::ArtifactStore) -> u64 {
    let WalRecord::ArtifactGrow {
        layer,
        level,
        growth,
    } = record
    else {
        return 0;
    };
    growth
        .iter()
        .map(|delta| {
            let Some(joining) = tessera_lifecycle::membership::deserialise_members(&delta.joining)
            else {
                return 0;
            };
            match store.get(layer, *level, delta.ordinal) {
                Some(record) => joining.andnot_cardinality(&record.members),
                None => 0,
            }
        })
        .sum()
}

/// Settle every joining row of one batch, whose join-ness `established_collisions` has just decided
/// (`views.md` §4, §5; decision 0116) — the **join rule**'s three arms, and then the completion an
/// accepted join owes.
///
/// `Err` is the refusal the caller is answered with — a `409`, whole batch without effect, taken
/// before the WAL append so a refused batch leaves no record. The text is what
/// `/control/ingest`'s handler answered with until 2026-09-01, byte for byte: the site moved and
/// the body did not, so a caller cannot tell one from the other and the byte-identity tests hold.
///
/// **Why all three are here and none in the handler.** A joining row carries geometry, and — for a
/// scoped family — the cell its key addresses. Everything else it might name is already decided:
/// the entity's label, and its entity-scoped attributes. What each arm checks is that the caller is
/// not trying to change one of those through a second view's row. The handler could ask the same
/// questions, and did, but it asked them of an answer a queue drain old: a row promoted to a join
/// between the handler's pass and this one passed no arm at all, which is the race this collapse
/// closes.
///
/// **A row index and a column name reach the caller; nothing else does.** No entity id, no external
/// id and no value on either side (**I10**, and `error.rs`'s standing rule about caller data in
/// bodies).
///
/// **One pass, because the arms and the completion read the same sources.** A joining row's
/// buffered row is fetched once, its record-blob row is decompressed at most once
/// ([`crate::session::BlobRow`]) however many blob-resident columns ask for it, and the descriptor
/// drop and the render backfill happen in the same visit. Splitting them cost a second lookup per
/// row and a decompression per blob column per site (review finding F4).
fn settle_joins(generation: &Generation, rows: &mut [UnallocatedRow]) -> Result<(), String> {
    if rows.iter().all(|row| row.join.is_none()) {
        return Ok(());
    }
    let manifest = &generation.bundle.manifest;
    let declared = &manifest.declared_scalars;
    // One derivation per batch, not per row: every row of a batch names one view, and this is the
    // list its `scoped` tail was parsed positionally against at the boundary.
    let view = rows.first().map(|row| row.view.as_str()).unwrap_or("");
    let scoped_families = scoped_families_of_view(manifest, view);
    let owner_view = scoped_owner_view_of(manifest, view);
    // The cell's key, for the refusal — the half of the owner view id a caller spelled, and never
    // the owning group, which a sharing group's caller has no business learning from a refusal.
    let key = owner_view
        .split_once(tessera_store::GROUP_SEPARATOR)
        .map(|(_, key)| key)
        .unwrap_or(owner_view.as_str())
        .to_string();

    for (index, row) in rows.iter_mut().enumerate() {
        let Some(entity) = row.join else {
            continue;
        };
        let buffered = generation.buffer.get(entity);
        // Read at most once for this row, and only if a blob-resident column asks.
        let mut blob = crate::session::BlobRow::default();
        // **The label arm reads the buffer first and the transpose after it, and both are exact.**
        // The buffer holds the entity's own row until its flush; past that, `entities/terms/`
        // holds the same set in promoted ordinals (contracts §2.4).
        //
        // A novel descriptor resolves to a process-local extension id, which no stored ordinal can
        // equal, so a batch naming a label the deployment has never interned is a mismatch — which
        // is right: the flushed entity cannot be carrying it.
        let held_terms: Option<Vec<u32>> = match &buffered {
            Some(buffered) => Some(buffered.terms.iter().map(|t| t.raw()).collect()),
            None => crate::session::flushed_terms_of(generation, entity)
                .map(|terms| terms.iter().map(|t| t.raw()).collect()),
        };
        if let Some(mut held_terms) = held_terms {
            let mut supplied_terms: Vec<u32> = row.terms.iter().map(|t| t.raw()).collect();
            supplied_terms.sort_unstable();
            supplied_terms.dedup();
            held_terms.sort_unstable();
            held_terms.dedup();
            if supplied_terms != held_terms {
                return Err(format!(
                    "row {index} joins an entity this deployment already holds, under a different \
                     access label. A re-label is a delete plus a re-ingest (decision 0047), never \
                     a field carried in on a second view's row: the alternative is a widening with \
                     no overlay entry, or a narrowing that bypasses the deny lanes (views §4)"
                ));
            }
        }
        // **The attribute arm reads the buffer first and the stored value after it, and both are
        // exact** (2026-08-31, closing `views.md` §4's last ⊘). An entity-scoped attribute is one
        // value per entity, so a joining row must carry the stored value or leave it absent. A
        // differing one is refused naming the column — silently keeping either value would make
        // the answer depend on which view a filter was asked under, which is exactly what a
        // *scoped* attribute is for and this is not one.
        //
        // The two sources are compared by the *same* equality, on values normalised to the shape a
        // batch carries (`stored_as_wal`), so the buffered and the flushed arm produce
        // byte-identical refusals and cannot come to disagree about what "the same value" means.
        for (position, d) in declared.iter().enumerate() {
            let Some(supplied) = row.scalars.get(position) else {
                continue;
            };
            let held = match &buffered {
                Some(buffered) => buffered.scalars.get(position).cloned(),
                // `None` here is *no value held* and *could not find out* alike; see
                // `session::flushed_scalar_of` for why one answer serves both.
                None => crate::session::flushed_scalar_of(generation, entity, position, &mut blob),
            };
            let Some(held) = held else {
                continue;
            };
            if crate::session::scalar_is_absent(&held, d) {
                continue;
            }
            // **An omitted value is not a disagreement**, and is not written through as an absence
            // either: the backfill below fills a `render` column's omitted slot from the entity's
            // stored value, once join-ness is settled.
            if crate::session::scalar_is_absent(supplied, d) || held == *supplied {
                continue;
            }
            return Err(format!(
                "row {index} joins an entity this deployment already holds, with a different \
                 value for column '{}'. An entity-scoped attribute is one value per entity, so a \
                 joining row byte-matches the stored value or omits it (views §4, §5)",
                d.name
            ));
        }
        // **The scoped cell arm: one value per `(entity, attribute, key)`, whichever door wrote it**
        // (`views.md` §5, decision 0116). A scoped value is not the entity's, so the two arms above
        // do not reach it; it is the *cell's*, and the cell a joining row addresses may already hold
        // a value — put there through the owning group's view, or through any group sharing those
        // views, in this window or a previous one.
        //
        // Three answers, and the middle one is what makes the two doors safe:
        //
        // - the cell is empty, or this row names no value for it → the row writes it;
        // - the cell holds the **same** value → the row's copy is dropped. One claimant per cell, so
        //   the extents stay disjoint in entity space and the composition has nothing to refuse;
        //   this is what replaces the old two-extents jam argument for the one-door rule;
        // - the cell holds a **different** value → `409` naming the column and the key.
        //
        // **The same-window case needs no separate check.** Two batches writing one cell means two
        // rows for one entity, so the second names an external id the open window already holds and
        // `admit_ingest` closes the window before reaching here — after which the first batch's row
        // is in the buffer and the buffered source below is the one that answers.
        //
        // **A `text` family past a flush is refused rather than compared, and that is the whole
        // rule for it** (2026-09-01, review finding F1). Nothing here can compare prose across a
        // flush boundary: a text column stores a dictionary, postings and a presence bitmap, and no
        // per-entity value for `flushed_scoped_of` to read back. The blob-resident analogy the
        // entity-scoped arm makes does not carry — *there* a lost comparison costs only the report,
        // because a joining row writes no record field, but here the row's value **is** written, as
        // a second text layer stamped with the same view. Text layers have no coverage check (their
        // disjointness rests on I9, which no longer holds for a scoped column, two views of one key
        // now reaching one cell) and `match` unions across them, so an admitted disagreement is two
        // sets of words answering under one column with no symptom anywhere. So occupancy is asked
        // instead of equality, and an occupied cell refuses a supplied string — equal or not, the
        // equality being exactly what cannot be established. Omitting the column still passes, and
        // the buffered source above still compares text exactly.
        for (position, family) in scoped_families.iter().enumerate() {
            let Some(supplied) = row.scoped.get(position) else {
                continue;
            };
            let d = crate::session::declared_of_scoped(family);
            if crate::session::scalar_is_absent(supplied, &d) {
                continue;
            }
            // Every buffered row of the entity whose view addresses this same key — the cell's own
            // rows, not the entity's own row, which is a different question and `buffer.get`'s.
            let held = generation
                .buffer
                .rows_of(entity)
                .filter(|item| scoped_owner_view_of(manifest, &item.view) == owner_view)
                .find_map(|item| {
                    let value = item.scoped.get(position)?;
                    (!crate::session::scalar_is_absent(value, &d)).then(|| value.clone())
                })
                .or_else(|| {
                    crate::session::flushed_scoped_of(generation, entity, family, &owner_view)
                });
            if held.is_none()
                && family.arrow_type == ScalarType::Text
                && crate::session::flushed_scoped_text_present(
                    generation,
                    entity,
                    family,
                    &owner_view,
                )
            {
                return Err(format!(
                    "row {index} names a value for group-scoped column '{}', and this deployment \
                     already holds prose for key '{}'. A `text` family's stored value cannot be \
                     compared once it has flushed — the column stores a dictionary and postings \
                     and no value per entity — so a cell that holds prose takes no second one, \
                     equal or not: changing it is a delete plus a re-ingest (decision 0047), and \
                     omitting the column leaves the cell as it stands (views §5)",
                    family.name, key
                ));
            }
            let Some(held) = held else {
                continue;
            };
            if crate::session::scalar_is_absent(&held, &d) {
                continue;
            }
            if held == row.scoped[position] {
                // The dedupe. Absence in this row's tail, and the cell keeps the one claimant it
                // already had.
                row.scoped[position] = tessera_lifecycle::WalScalar::Null;
                continue;
            }
            return Err(format!(
                "row {index} names a different value for group-scoped column '{}' than this \
                 deployment already holds for key '{}'. A scoped value is addressed by \
                 (attribute, key) and is one value per cell, so a row naming that cell — through \
                 the owning group's view or through any group sharing it — byte-matches the stored \
                 value or omits it (views §5)",
                family.name, key
            ));
        }

        // ---- past this point the row is admitted, and what follows completes it ----

        // **A joining row carries no descriptors and no terms, and this is where they go**
        // (`views.md` §4). The entity's label is the one it already has: its terms are already in
        // the postings, put there by the flush that gave it its first row, and re-writing them from
        // this row is how a second view would come to re-label an entity with no overlay entry.
        //
        // Here rather than in the handler for `established_collisions`'s reason: the handler's
        // answer is a queue drain old. A row it called new and this pass calls a join would arrive
        // with its descriptors intact and re-label the entity; a row it called a join and this pass
        // calls new — its holder deleted in between — would arrive with them already dropped and
        // allocate a fresh entity carrying no label at all, which is invisible to every principal.
        row.descriptors = Vec::new();
        row.terms = Vec::new();

        // **An accepted join's omitted `render` values are backfilled here** (`views.md` §4, owner
        // ruling 2026-08-31), and here rather than in the handler because this is where join-ness
        // is *settled*: `established_collisions` is what finally decides which rows join and which
        // allocate fresh, and a row that stops being a join must not carry a value it took from an
        // entity it turned out not to be joining.
        //
        // A joining row is geometry-only in entity space — no descriptors, no postings, no
        // attribute column, no record field — but its scalars still travel in its own row tail, so
        // an omitted `render` value would put an **absence** in the joined view's hot column while
        // every other view of the same entity rendered a value. An entity-scoped attribute is one
        // value per entity (`views.md` §5); one that renders under one view and not another is not.
        //
        // Before the WAL append, so the log carries the value the flush will write and replay
        // reproduces it rather than re-deriving it against whatever the bundle holds by then.
        for (position, d) in declared.iter().enumerate() {
            if !d.render {
                continue;
            }
            let Some(supplied) = row.scalars.get(position) else {
                continue;
            };
            if !crate::session::scalar_is_absent(supplied, d) {
                continue;
            }
            // The entity's own row where it is still buffered, the stored homes after it. A column
            // the entity genuinely holds nothing for is `None` here and its absence stays an
            // absence in every view.
            let held = match &buffered {
                Some(item) => item.scalars.get(position).cloned(),
                None => crate::session::flushed_scalar_of(generation, entity, position, &mut blob),
            };
            let Some(held) = held else {
                continue;
            };
            if crate::session::scalar_is_absent(&held, d) {
                continue;
            }
            row.scalars[position] = held;
        }
    }
    Ok(())
}

/// What `commit_growth` answers per join: the artifact's entity and how many of the joining
/// members it did not already hold.
///
/// Every key here has resolved in `prepare_grow` under the same lock, so the second lookup cannot
/// fail; a failure is a bug in that ordering and is treated as one. The difference is taken
/// against the membership as it stands **before** the record is applied, which is the only time
/// it exists.
fn growth_receipt(
    registry: &LayerRegistry,
    store: &ArtifactStore,
    layer: &str,
    level: u32,
    joins: &[tessera_lifecycle::IncomingGrowth],
    prepared: &tessera_lifecycle::PreparedGrow,
) -> Vec<tessera_lifecycle::MembershipGrown> {
    joins
        .iter()
        .zip(&prepared.filled)
        .enumerate()
        .map(|(index, (join, filled))| {
            let ordinal = registry
                .resolve_growth_key(layer, level, &join.key, store)
                .expect("prepare_grow resolved every key before the receipt was read");
            let record = store
                .get(layer, level, ordinal)
                .expect("a resolved ordinal names a record");
            // **The set this row moves**, which is the membership on a row with no rank and the
            // content's generating set on a row with one. A rank naming no content refused the
            // batch in `prepare_grow` above, so the `None` arm here is unreachable and answers
            // nothing rather than panicking on a thread that owes an acknowledgement.
            //
            // **`left` is counted against the set the joins have already entered**, which is the
            // order the page is applied in (`ingest.md` §1.1): an entity this page both joins and
            // leaves is one this page took out. The copy that takes is skipped where nothing
            // leaves, which is every membership row.
            let counted = |set: &croaring::Bitmap| {
                let joined = join.joining.andnot_cardinality(set);
                let left = if join.leaving.is_empty() {
                    0
                } else {
                    let mut after_joins = set.clone();
                    after_joins.or_inplace(&join.joining);
                    join.leaving.and_cardinality(&after_joins)
                };
                (joined, left)
            };
            let (joined, left) = match join.rank {
                None => counted(&record.members),
                Some(rank) => record
                    .contents
                    .get(rank as usize)
                    .map_or((0, 0), |content| counted(&content.generated_from)),
            };
            tessera_lifecycle::MembershipGrown {
                entity: record.entity,
                joined,
                filled: *filled,
                left,
                withdrawn: prepared
                    .withdrawn
                    .iter()
                    .find(|(row, _)| *row == index)
                    .map(|(_, rank)| *rank),
            }
        })
        .collect()
}

/// The executor's refusal for a registry error: a differing fixed part is the caller's `409`
/// (`ExecError::PartConflict`), everything else the `422` a refused layer operation has always
/// been.
fn refusal_of(e: tessera_lifecycle::RegistryError) -> ExecError {
    match e {
        // A second `excluding` on a held key is the same `409` a differing fixed part is: the
        // complement it asks for is a different set from the one the artifact holds
        // (`ingest.md` §1.3).
        tessera_lifecycle::RegistryError::PartConflict { .. }
        | tessera_lifecycle::RegistryError::ExclusionOnHeldKey { .. } => ExecError::PartConflict {
            detail: e.to_string(),
        },
        other => ExecError::LayerRefused {
            detail: other.to_string(),
        },
    }
}

/// What `WritePath::publish_artifacts` answers: the entities in the caller's order and the
/// batch's counts, as `Ack::ArtifactsPublished` carries them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PublishedBatch {
    pub(crate) entities: Vec<EntityId>,
    pub(crate) created: u64,
    pub(crate) without_content: u64,
    pub(crate) filled: u64,
    pub(crate) joined: u64,
}

impl Executor {
    /// Resolve one batch's membership keys, and check the edges its adjacency declared.
    ///
    /// Returns the memberships — each carrying the ordinal it resolved to, or `None` where an open
    /// layer will mint it at the close — and the edges whose **child** is one of those mints, which
    /// are the only edges this route creates rather than checks.
    ///
    /// `Err` is the refusal text the caller is answered with, whole batch without effect.
    ///
    /// **The memberships resolve first, and that order is what the edge checks rest on**: a key is
    /// created only by being a membership (every entry of a list column names an artifact the point
    /// belongs to — `artifacts-from-points.md` §4), so the set of keys this batch is about to mint
    /// is known once they are done, and neither a child nor a parent can be minted without
    /// appearing there.
    fn resolve_memberships(
        &self,
        artifacts: &tessera_lifecycle::BatchArtifacts,
    ) -> Result<
        (
            Vec<tessera_lifecycle::ResolvedMembership>,
            Vec<tessera_lifecycle::BatchEdge>,
        ),
        String,
    > {
        if artifacts.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }
        // **One line per batch, not one per edge.** A lineage over a 10⁵-cluster tree whose roster
        // was published without parents would otherwise emit 10⁵ formatted writes on the write
        // path, which is the shape that makes a log a second bottleneck. The count is the signal
        // and the examples are what an operator acts on.
        let mut unrecorded: Vec<String> = Vec::new();
        let mut unrecorded_total = 0usize;
        let resolved = self.live.with_publication_state(|registry, store, _| {
            let memberships: Vec<tessera_lifecycle::ResolvedMembership> = artifacts
                .memberships
                .iter()
                .map(|join| {
                    registry
                        .resolve_or_mint(&join.layer, join.level, &join.key, store)
                        .map(|ordinal| tessera_lifecycle::ResolvedMembership {
                            layer: join.layer.clone(),
                            level: join.level,
                            key: join.key.clone(),
                            ordinal,
                            rows: join.rows.clone(),
                        })
                        .map_err(|e| e.to_string())
                })
                .collect::<Result<_, String>>()?;
            // **Two indexes of one set, because an edge asks two different questions of it.** A
            // *child*'s level is the edge's own, so it is asked precisely; a *parent*'s is whatever
            // the layer's shape says to look at, so it is asked of the layer. A levelled taxonomy
            // legitimately carries one key at two levels, and one index would treat a key minting
            // at one of them as minting at both.
            let minting: std::collections::BTreeSet<(&str, u32, &str)> = memberships
                .iter()
                .filter(|m| m.ordinal.is_none())
                .map(|m| (m.layer.as_str(), m.level, m.key.as_str()))
                .collect();
            let anywhere: std::collections::BTreeSet<(&str, &str)> = minting
                .iter()
                .map(|(layer, _, key)| (*layer, *key))
                .collect();

            // **A child named under two parents refuses the batch**, which is the build's own
            // refusal at the other entry point (`artifacts-from-points.md` §4): two rows naming
            // different parents for one artifact are two hierarchies, and which of them was
            // published would be the batch's row order rather than anything the caller wrote. It is
            // made here, over the batch's own column, because that is where the two rows are — and
            // it is the whole check for a minted child, whose parent nothing else has an opinion
            // about yet. It holds at every kind: only a `nested` or `tiered` list column declares
            // edges, and a `dag` layer's several parents arrive on its artifact rows' `parent`
            // list by the publish route, never here (`ListMeaning`, decision 0125).
            let mut claimed: std::collections::BTreeMap<(&str, u32, &str), &str> =
                Default::default();
            for edge in &artifacts.edges {
                let at = (edge.layer.as_str(), edge.level, edge.child.as_str());
                if let Some(first) = claimed.insert(at, edge.parent.as_str()) {
                    if first != edge.parent {
                        return Err(format!(
                            "{} in level {} of {} is named as a child of both {first} and {}. A \
                             list column declares the edges, so two rows naming different parents \
                             for one artifact are two hierarchies — and which of them was \
                             published would be the batch's row order rather than anything the \
                             caller wrote",
                            edge.child, edge.level, edge.layer, edge.parent
                        ));
                    }
                }
            }

            let mut mints = Vec::new();
            for edge in &artifacts.edges {
                let layer = edge.layer.as_str();
                match registry.check_edge(
                    edge,
                    store,
                    minting.contains(&(layer, edge.level, edge.child.as_str())),
                    &|key| anywhere.contains(&(layer, key)),
                ) {
                    Ok(tessera_lifecycle::EdgeCheck::Agrees) => {}
                    // The child does not exist yet, so this edge is its parent rather than a claim
                    // about a stored one — carried to the close, where the artifact is created and
                    // where lineage has always been settled.
                    Ok(tessera_lifecycle::EdgeCheck::Mints) => mints.push(edge.clone()),
                    // **Reported, not refused** — see `LayerRegistry::check_edge`. The membership
                    // half of the same entry is unambiguous and lands; what is lost is an edge this
                    // route cannot create, and an operator who published a roster without its
                    // parents needs to be told rather than blocked.
                    Ok(tessera_lifecycle::EdgeCheck::Unrecorded) => {
                        unrecorded_total += 1;
                        if unrecorded.len() < 5 {
                            unrecorded.push(format!(
                                "{} of {} under {}",
                                edge.child, edge.layer, edge.parent
                            ));
                        }
                    }
                    Err(e) => return Err(e.to_string()),
                }
            }
            Ok((memberships, mints))
        });
        if unrecorded_total > 0 {
            tracing::warn!(
                count = unrecorded_total,
                examples = ?unrecorded,
                "an ingest batch's list column names parent edges these layers do not hold; the \
                 memberships are applied and the edges are not — a growth adds members, and \
                 lineage is declared where the artifact is published"
            );
        }
        resolved
    }

    /// **The artifacts this window's *values* named and nothing holds** — one per
    /// `membership = { attribute = f }` layer whose column carried a value the level has no
    /// artifact for.
    ///
    /// **A value exists because a point carries it**, at both entry points: a build mints from the
    /// column it has just read, and an ingest mints from the rows that have just arrived. The two
    /// use the same rule for the key (`tessera_types::layer::attribute_value_key`), which is what
    /// makes them agree about which artifact a value names — a key that could be written two ways
    /// would let one route mint a second artifact for a value the other already named.
    ///
    /// **After the vocabulary mint, and that ordering is load-bearing.** A novel category key is a
    /// string in the row until the pass above draws it a code; reading the row before that would
    /// name the artifact after a code nobody had assigned yet.
    ///
    /// **Suppression-blindness carries over unchanged.** The lookup is
    /// [`ArtifactStore::ordinal_of_key`] — the store's key index, which loses a key at exactly one
    /// event, the fold retiring the artifact's own entity. A *suppressed* value's key therefore
    /// still resolves and mints nothing, so a suppression cannot be defeated by ingesting a point
    /// carrying the value; a *deleted* one does mint again, and the new artifact is a new object
    /// with a new entity, which is what a deletion means.
    ///
    /// **Publication into such a layer stays refused** — this is not that route. What is created
    /// here is an identity the rule produces, carrying its key and nothing else, on
    /// `LayerRegistry::prepare_derive`'s own contract.
    fn derive_records(
        &mut self,
        closed: &[tessera_lifecycle::ClosedEntry<Responder>],
        vocabularies: &Vocabularies,
    ) -> Result<Vec<WalRecord>, String> {
        use tessera_types::layer::attribute_value_key;

        // Which declared scalar each predicate layer reads, resolved once. A layer naming a column
        // this bundle does not declare is refused at registration, so an absence here is a
        // declaration that never validated — skipped rather than guessed at, which mints nothing.
        let generation = self.generation.load();
        let declared = &generation.bundle.manifest.declared_scalars;
        let predicates: Vec<(String, usize, Option<String>)> =
            self.live.predicate_columns(|field| {
                let index = declared.iter().position(|scalar| scalar.name == field)?;
                Some((index, declared[index].vocabulary.clone()))
            });
        if predicates.is_empty() {
            return Ok(Vec::new());
        }

        let mut records = Vec::new();
        for (layer, index, vocabulary) in predicates {
            // `code → key`, for the values this window actually carried. Walked from the live
            // bindings rather than inverted per row: a vocabulary is a map from key to code, so a
            // per-row reverse lookup would rebuild this per point.
            let mut key_of_code: std::collections::BTreeMap<u32, String> = Default::default();
            if let Some(name) = &vocabulary {
                if let Some(minter) = vocabularies.get(name) {
                    for (key, code) in minter.bindings() {
                        key_of_code.insert(code, key.to_string());
                    }
                }
            }
            let mut wanted: std::collections::BTreeSet<String> = Default::default();
            for entry in closed {
                for row in entry.rows() {
                    let Some(code) = row.scalars.get(index).and_then(scalar_code) else {
                        continue;
                    };
                    // Code 0 is a category code space's reserved *absent* sentinel and names no
                    // value; a plain integer column has no such reservation.
                    if vocabulary.is_some() && code == tessera_store::vocabulary::ABSENT_CODE {
                        continue;
                    }
                    wanted.insert(attribute_value_key(
                        code,
                        key_of_code.get(&code).map(String::as_str),
                    ));
                }
            }
            if wanted.is_empty() {
                continue;
            }
            let prepared = self.live.with_publication_state(|registry, store, alloc| {
                let fresh: Vec<String> = wanted
                    .iter()
                    // A predicate layer is entity-scoped: `LayerRegistry::prepare_derive`
                    // refuses a group-scoped one, so the key sits in the one set.
                    .filter(|key| store.ordinal_of_key(&layer, 0, None, key).is_none())
                    .cloned()
                    .collect();
                if fresh.is_empty() {
                    return Ok(None);
                }
                registry
                    .prepare_derive(&layer, 0, &fresh, store, alloc)
                    .map(Some)
                    .map_err(|e| e.to_string())
            })?;
            if let Some(record) = prepared {
                records.push(record);
            }
        }
        Ok(records)
    }

    /// Prepare the publications that create every artifact this window's rows named and nothing
    /// holds — see [`mint_plan`] for what is minted and why it is minted here.
    ///
    /// Patches the memberships whose key resolved *since* admission to the ordinal it resolved to,
    /// so they grow rather than mint; leaves a minted key's ordinal `None`, which is what tells
    /// [`growth_records`] the publication carried the join.
    ///
    /// `Err` is the refusal text every waiter in the window is answered with. It costs the window,
    /// which is the price of a decision that can only be made once the batches are together — and
    /// the two shapes that reach it are a lineage two *batches* disagree about, and an allocator
    /// that could not supply the reserved run. Everything a single batch can be refused for on its
    /// own was refused at its admission.
    fn mint_records(
        &mut self,
        closed: &mut [tessera_lifecycle::ClosedEntry<Responder>],
    ) -> Result<(Vec<WalRecord>, Vec<u64>), String> {
        use std::collections::BTreeMap;
        let mut minted_per_entry = vec![0u64; closed.len()];
        let Some((wanted, edges)) = mint_plan(closed) else {
            return Ok((Vec::new(), minted_per_entry));
        };

        type Prepared = Result<
            (
                Vec<WalRecord>,
                std::collections::BTreeMap<(String, u32, String), u32>,
                std::collections::BTreeSet<(String, u32, String)>,
            ),
            String,
        >;
        let prepared: Prepared = self.live.with_publication_state(|registry, store, alloc| {
            // **A child named under two parents refuses**, across the window as it does within a
            // batch: two entries naming different parents for one artifact are two hierarchies,
            // and there is no correct output. Checked before anything is prepared, so a refusal
            // spends nothing. Every kind whose list declares edges is a tree here — a `dag`
            // layer's list is memberships and its parents travel on the artifact row (decision
            // 0125) — so a child's parents are at most one key, held as a list because that is
            // the record's shape. The cycle those edges could close is refused where the
            // artifacts are created, in `prepare_publish`, which walks the batch's own edges — a
            // growth never adds lineage, so the window's minted edges are every edge a cycle
            // could run through.
            let mut parents: BTreeMap<(String, u32, String), Vec<String>> = BTreeMap::new();
            for edge in &edges {
                let at = (edge.layer.clone(), edge.level, edge.child.clone());
                let named = parents.entry(at).or_default();
                if named.contains(&edge.parent) {
                    continue;
                }
                if !named.is_empty() {
                    return Err(format!(
                        "{} in level {} of {} is named as a child of both {} and {}. A list \
                         column declares the edges, so two rows naming different parents for one \
                         artifact are two hierarchies — and which of them was published would be \
                         the order the batches arrived in rather than anything the caller wrote",
                        edge.child, edge.level, edge.layer, named[0], edge.parent
                    ));
                }
                named.push(edge.parent.clone());
            }
            // Re-resolved here and not trusted from admission: a publication executes between an
            // admission and this close (it takes the work lane, and the window is open across it),
            // so a key that named nothing then may name an artifact now — and §5's second ruling is
            // that a key a live artifact holds is never minted again.
            let mut resolved: BTreeMap<(String, u32, String), u32> = BTreeMap::new();
            let mut to_mint: BTreeMap<(&str, u32), Vec<(&str, &croaring::Bitmap)>> =
                BTreeMap::new();
            for ((layer, level, key), (_, members)) in &wanted {
                // The ingest route carries no artifact view; `resolve_or_mint` refuses a
                // group-scoped layer there (`ingest.md` §1.5), so the key sits in the one set.
                match store.ordinal_of_key(layer, *level, None, key) {
                    Some(ordinal) => {
                        resolved.insert((layer.clone(), *level, key.clone()), ordinal);
                    }
                    None => to_mint
                        .entry((layer.as_str(), *level))
                        .or_default()
                        .push((key.as_str(), members)),
                }
            }

            // **Ascending level, one record each, coarse first.** A tiered chain's parent sits one
            // level up and is fixed by the record before this one; a nested lineage is level 0
            // alone, one record, and `prepare_publish` resolves a parent that is a sibling of its
            // own batch.
            let mut assigned: BTreeMap<(&str, u32, &str), u32> = BTreeMap::new();
            let mut records = Vec::new();
            for ((layer, level), keys) in &to_mint {
                let incoming: Vec<tessera_lifecycle::IncomingArtifact> = keys
                    .iter()
                    .map(|(key, members)| tessera_lifecycle::IncomingArtifact {
                        key: Some((*key).to_string()),
                        // Entity-scoped: a group-scoped layer is refused at admission, a point's
                        // layer column carrying no artifact view (`ingest.md` §1.5).
                        view: None,
                        members: (*members).clone(),
                        excluding: None,
                        // **Nothing but its name.** A layer declaring supplied content or a
                        // dependency refuses the key at admission rather than minting an artifact
                        // that could not be served — `LayerRegistry::resolve_or_mint` makes both
                        // refusals, in the words `prepare_publish` would have made them in.
                        contents: Vec::new(),
                        attached_to: None,
                        parent_keys: parents
                            .get(&((*layer).to_string(), *level, (*key).to_string()))
                            .cloned()
                            .unwrap_or_default(),
                        // A layer declaring a `shape` publishes boxes an author wrote, so a point
                        // naming a key on such a layer has nothing to mint one from — the layer is
                        // a predicate and `resolve_or_mint` refuses the key at admission.
                        shape: None,
                    })
                    .collect();
                // **One level up and no further.** Entry *k* of a list is the parent of entry
                // *k+1*, so a chain minted from one names its parent exactly one level coarser;
                // searching the levels above that would invent an edge across a gap the reader
                // deliberately does not read past (`tessera_types::layer::parent_edges`).
                let pending = |key: &str| {
                    let coarser = level.checked_sub(1)?;
                    assigned.get(&(*layer, coarser, key)).map(|ordinal| {
                        tessera_lifecycle::wal::ParentRef {
                            level: coarser,
                            ordinal: *ordinal,
                        }
                    })
                };
                let record = registry
                    .prepare_publish(layer, *level, &incoming, store, alloc, &pending)
                    .map_err(|e| e.to_string())?;
                let WalRecord::ArtifactPublish { artifacts, .. } = &record else {
                    unreachable!("prepare_publish returns an ArtifactPublish");
                };
                // Read back off the record rather than recomputed from the level's cursor: what a
                // finer level's parent resolves to is what this record actually claimed. The order
                // is `incoming`'s, which is `keys`', which is why the two zip.
                for ((key, _), artifact) in keys.iter().zip(artifacts) {
                    debug_assert_eq!(artifact.key.as_deref(), Some(*key));
                    assigned.insert((*layer, *level, key), artifact.ordinal);
                }
                records.push(record);
            }
            let minted = assigned
                .keys()
                .map(|(layer, level, key)| ((*layer).to_string(), *level, (*key).to_string()))
                .collect();
            Ok((records, resolved, minted))
        });
        let (records, resolved, minted) = prepared?;

        // A key that acquired an artifact between its batch's admission and this close is an
        // ordinary growth, and `growth_records` takes it from there. Usually none did — that needs
        // a publication to have executed inside the window — so the pass is skipped rather than
        // walked.
        if !resolved.is_empty() {
            for entry in closed.iter_mut() {
                for join in entry.memberships.iter_mut() {
                    if join.ordinal.is_some() {
                        continue;
                    }
                    let at = (join.layer.clone(), join.level, join.key.clone());
                    join.ordinal = resolved.get(&at).copied();
                }
            }
        }
        for ((layer, level, key), (index, _)) in &wanted {
            if minted.contains(&(layer.clone(), *level, key.clone())) {
                minted_per_entry[*index] += 1;
            }
        }
        Ok((records, minted_per_entry))
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
        let mut mark = StageMark::now();

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

        mark = self.health.lap(WriteStage::Allocate, mark);

        // **Mint every novel discovered-vocabulary key this window's rows carry, in place, before
        // anything is appended.** A discovered vocabulary's key travels as `WalScalar::Utf8` from
        // the ingest boundary (`tessera-server`'s `category_code`, which must not mint itself: two
        // requests racing one novel key would each draw and split the key across two codes). The
        // commit-window close is where minting *may* happen — the live view is authoritative and
        // serial here, exactly as `VocabularyMinter::mint`'s own doc requires — so it happens once,
        // against a mutable copy of the published bindings that becomes the next generation's if
        // the window survives, and is discarded untouched if it does not.
        //
        // One `Vocabularies` copy for the whole window, not one per row: `mint` is view-first, so a
        // second row naming an already-minted-this-window key sees the first row's binding and
        // returns `Existing` rather than drawing again — which is what keeps two rows sharing one
        // novel key inside a window down to one `VocabularyMint` record.
        let generation = self.generation.load_full();
        let mut vocabularies: Vocabularies = (*generation.vocabularies).clone();
        let declared_scalars = generation.bundle.manifest.declared_scalars.clone();
        // **The arity is this generation's, and a row admitted under an earlier one is padded
        // here** (`ingest.md` §7.1). A column declared between a batch's admission and this close
        // appended at the tail of `declared_scalars`, so the row's own positions keep their
        // meaning and the positions it lacks are columns it holds nothing for. Padded before the
        // mint pass below indexes `row.scalars` by declared position, and before the append, so
        // the log carries every row at the schema its flush will write.
        let mut closed = closed;
        for entry in closed.iter_mut() {
            for row in entry.rows_mut() {
                crate::attributes::pad_to_schema(&mut row.scalars, &declared_scalars);
            }
        }
        // **The group-scoped families, by the view a row names** (`views.md` §5). A row's scoped
        // tail is positional against the families of the group that owns its view, so the mint
        // pass below needs the same list the boundary parsed against — derived once for the
        // window rather than per row, and from the live manifest, which is what the boundary read
        // too.
        let scoped_by_view: FxHashMap<String, Vec<tessera_store::manifest::ScopedScalar>> =
            scoped_families_by_view(&generation.bundle.manifest);
        let mut fresh_bindings: Vec<(String, String, u32)> = Vec::new();
        let mut mint_failed: Option<MintError> = None;
        'minting: for entry in closed.iter_mut() {
            for row in entry.rows_mut() {
                for (index, declared) in declared_scalars.iter().enumerate() {
                    let Some(vocabulary) = declared.vocabulary.as_deref() else {
                        // A plain scalar, or a category column already at its bound width — either
                        // way, nothing for this site to resolve.
                        continue;
                    };
                    let WalScalar::Utf8(key) = &row.scalars[index] else {
                        // Already a code: either a declared vocabulary (the handler resolved it) or
                        // a discovered one this row's earlier pass through this same loop resolved.
                        continue;
                    };
                    let key = key.clone();
                    let minter = vocabularies.get_mut(vocabulary).unwrap_or_else(|| {
                        // `Vocabularies::seed` refuses to open a bundle whose `declared_scalars`
                        // names a vocabulary `MANIFEST.vocabularies` does not carry, so a live
                        // generation cannot disagree with its own declaration. Reaching this is a
                        // defect in that invariant, not reachable input.
                        panic!(
                            "column '{}' names vocabulary '{vocabulary}', which the live bindings \
                             do not carry",
                            declared.name
                        )
                    });
                    match minter.mint(&key) {
                        Ok(Minted::Fresh(code)) => {
                            fresh_bindings.push((vocabulary.to_string(), key, code));
                            row.scalars[index] = code_at_declared_width(declared.arrow_type, code);
                        }
                        Ok(Minted::Existing(code)) => {
                            row.scalars[index] = code_at_declared_width(declared.arrow_type, code);
                        }
                        Err(e) => {
                            mint_failed = Some(e);
                            break 'minting;
                        }
                    }
                }
                // **The same mint, over the row's scoped tail** (`views.md` §5). A scoped category
                // is a category: its key travels from the boundary exactly as an entity-scoped
                // one's does, and this is the one place a novel key becomes a code. A row whose
                // view is in no scope has an empty list here and the loop does nothing.
                let Some(families) = scoped_by_view.get(row.view.as_str()) else {
                    continue;
                };
                for (index, family) in families.iter().enumerate() {
                    let Some(vocabulary) = family.vocabulary.as_deref() else {
                        continue;
                    };
                    let Some(WalScalar::Utf8(key)) = row.scoped.get(index) else {
                        continue;
                    };
                    let key = key.clone();
                    let minter = vocabularies.get_mut(vocabulary).unwrap_or_else(|| {
                        panic!(
                            "scoped column family '{}' names vocabulary '{vocabulary}', which \\
                             the live bindings do not carry",
                            family.name
                        )
                    });
                    match minter.mint(&key) {
                        Ok(Minted::Fresh(code)) => {
                            fresh_bindings.push((vocabulary.to_string(), key, code));
                            row.scoped[index] = code_at_declared_width(family.arrow_type, code);
                        }
                        Ok(Minted::Existing(code)) => {
                            row.scoped[index] = code_at_declared_width(family.arrow_type, code);
                        }
                        Err(e) => {
                            mint_failed = Some(e);
                            break 'minting;
                        }
                    }
                }
            }
        }
        if let Some(e) = mint_failed {
            // Nothing has been appended yet, so — exactly as a failed allocation — the window has
            // no effect: the mutated `vocabularies` copy is dropped with it, and every waiter gets
            // the same refusal.
            self.fail_window_mint(closed, e, entries, started);
            return;
        }

        // **The artifacts this window's rows named and nothing holds, created here** — see
        // `Executor::mint_records`. Prepared before anything is appended, on `prepare_publish`'s own
        // rule that every check runs before the first allocation, so a refusal spends nothing. A
        // failure past that point has spent reserved ids, exactly as a failed window's rows have.
        let (mut mint_records, minted_per_entry) = match self.mint_records(&mut closed) {
            Ok(minted) => minted,
            Err(detail) => {
                self.fail_window_layer(closed, detail, entries, started);
                return;
            }
        };
        // **The artifacts this window's *values* named**, one per attribute-predicate layer whose
        // column carried a value nothing holds — see `Executor::derive_records`. It runs after the
        // vocabulary mint above, because a novel category key is a code only once that pass has
        // drawn it, and the key an artifact is named by is the value's key.
        match self.derive_records(&closed, &vocabularies) {
            Ok(records) => mint_records.extend(records),
            Err(detail) => {
                self.fail_window_layer(closed, detail, entries, started);
                return;
            }
        }

        // One record per entry — batch identity is preserved through the window, which is what a
        // joined retry is answered off — appended in entries order, which is also apply order.
        let mut failed_at: Option<(usize, WalError)> = None;

        // **Mint records land first, ahead of every batch record, inside the one fsync below** —
        // `WalRecord::VocabularyMint`'s own doc states this ordering is why a mint is durable in
        // the same commit as the rows it colours. Not tracked in `positions`: that vector is
        // rotation's per-*batch-entry* index, and a mint record belongs to no entry.
        for (vocabulary, key, code) in &fresh_bindings {
            if let Err(e) = self.wal.append(&WalRecord::VocabularyMint {
                vocabulary: vocabulary.clone(),
                key: key.clone(),
                code: *code,
            }) {
                // No entry has been attempted yet, so there is no "the entry whose append failed"
                // to single out — the same arbitrary choice the fsync failure below makes.
                failed_at = Some((0, e));
                break;
            }
        }

        // The position **before** each append is where that record lands, and it is the only moment
        // it can be read: afterwards the log has moved on, and after the window it is one number for
        // several records. A row's position is what a rotation reclaims below, so an entry whose
        // append failed contributes none — the loop breaks before pushing.
        let mut positions: Vec<u64> = Vec::with_capacity(closed.len());
        if failed_at.is_none() {
            for (i, entry) in closed.iter().enumerate() {
                let at = self.wal.position();
                if let Err(e) = self.wal.append(&entry.record) {
                    failed_at = Some((i, e));
                    break;
                }
                positions.push(at);
            }
        }

        // **The joins this window's rows declared, in the same commit as the rows** — one record
        // per `(layer, level)` over every entry, appended behind the batch records and inside the
        // one fsync below (`artifacts-from-points.md` §6.2). Built after the allocation because
        // that is the first moment a row has an entity to join with, and the ordinals were resolved
        // at admission.
        //
        // Each record's position is read before its append and carried to the apply: a growth below
        // its level's published high-water is held in the log by that position until a fold rewrites
        // the level whole, and releasing it early is the silent loss `ArtifactStore::grow`'s own doc
        // is written against.
        //
        // **The publications that minted come first**, because a growth of the same window may name
        // an ordinal one of them claimed — not today, a minted artifact being published with its
        // members, but replay applies this sequence in order and an artifact must exist before
        // anything addresses it.
        let mut minted: Vec<(WalRecord, u64)> = Vec::new();
        if failed_at.is_none() {
            for record in mint_records {
                let at = self.wal.position();
                if let Err(e) = self.wal.append(&record) {
                    // No entry is more to blame than another for a record the whole window's keys
                    // produced; the first waiter gets the real error, as the fsync arm does.
                    failed_at = Some((0, e));
                    break;
                }
                minted.push((record, at));
            }
        }
        let mut growth: Vec<(WalRecord, u64)> = Vec::new();
        if failed_at.is_none() {
            for (record, i) in growth_records(&closed) {
                let at = self.wal.position();
                if let Err(e) = self.wal.append(&record) {
                    failed_at = Some((i, e));
                    break;
                }
                growth.push((record, at));
            }
        }
        mark = self.health.lap(WriteStage::WalAppend, mark);
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
        self.health.lap(WriteStage::WalFsync, mark);
        self.observe_wal();
        if let Some((index, error)) = failed_at {
            self.fail_window_wal(closed, index, error, entries, started);
            return;
        }

        // Durable, not yet in force. See `pause_point`.
        self.pause_point(PauseSiteArg::AfterFsync);
        // One buffer clone, one generation, **one swap** for every entry in the window — carrying
        // the mutated `vocabularies`, so the next generation publishes this window's mints and not
        // merely its rows.
        let published = self.apply_window(&mut closed, &positions, vocabularies, &fresh_bindings);

        // **After the rows are in force, never before.** A membership is projected through rows, so
        // a store that held the join while the generation still lacked the row would describe an
        // artifact by a point nothing could yet see. The reverse order costs nothing: both are
        // durable by this line, and the log is what a restart reads.
        if !minted.is_empty() || !growth.is_empty() {
            // **The level versions these records are about to move, read in the order they move
            // them** — what each record's held row form must be at for that record's delta to be
            // the one it is missing (`Executor::bring_artifacts_forward`). Two records of one
            // window may name the same level, so the version is carried forward across the
            // sequence rather than read once: every publication and every growth bumps exactly one
            // level exactly once.
            let mut befores: Vec<u64> = Vec::with_capacity(minted.len() + growth.len());
            self.live.with_artifacts(|store| {
                let mut seen: std::collections::BTreeMap<(&str, u32), u64> =
                    std::collections::BTreeMap::new();
                for (record, _) in minted.iter().chain(growth.iter()) {
                    let Some((layer, level)) = artifact_level_of(record) else {
                        befores.push(0);
                        continue;
                    };
                    let at = seen
                        .entry((layer, level))
                        .or_insert_with(|| store.level_version(layer, level));
                    befores.push(*at);
                    *at += 1;
                }
            });
            // **What the store refused, per record**, so the tick's row forms take only what the
            // records took (`ArtifactStore::apply_reporting`).
            let mut refused_per_record: Vec<Vec<usize>> =
                vec![Vec::new(); minted.len() + growth.len()];
            let undecodable = self.live.with_publication_state(|registry, store, _| {
                let mut undecodable = 0;
                // The registry half first, per record: a mint may have extended its level's
                // reserved runs, and the store's own apply resolves ordinals against them.
                for ((record, position), refused) in minted.iter().zip(&mut refused_per_record) {
                    registry.apply(record);
                    undecodable += store.apply_reporting(record, *position, refused);
                }
                for ((record, position), refused) in growth
                    .iter()
                    .zip(refused_per_record.iter_mut().skip(minted.len()))
                {
                    undecodable += store.apply_reporting(record, *position, refused);
                }
                undecodable
            });
            if undecodable > 0 {
                // Unreachable in practice — these bytes were serialised from a live bitmap moments
                // ago — and alarmed rather than asserted, because the alternative to noticing is an
                // artifact that is quietly the size it was before.
                tracing::error!(
                    count = undecodable,
                    "ALARM: a membership growth did not survive its own round trip"
                );
            }
            // **And every delta the records just took is held for the tick**, in the order they
            // took it, rather than being applied to the row forms here — the ingest twin of
            // `commit_growth`'s own accumulation (`ingest.md` §1.3).
            for (((record, _), before), refused) in minted
                .iter()
                .chain(growth.iter())
                .zip(befores)
                .zip(&refused_per_record)
            {
                self.hold_delta(record, before, refused);
            }
            // A growth against an artifact **above** its level's high-water is carried by the next
            // tail pack like any other unpublished record; one below it waits for the fold, held in
            // the log by the pin. Marking the manifest dirty is what gets the first case published.
            self.deny_dirty = true;
        }

        // Recorded after the swap, so a concurrent replay of a batch id can never observe a window
        // where the generation has swapped but the idempotency index has not caught up.
        let m = StageMark::now();
        for entry in &closed {
            let (batch_id, body_hash) = entry.batch_key();
            self.live.record_accepted_batch(
                batch_id.to_string(),
                body_hash,
                entry.entity_ids.clone(),
            );
        }
        self.health.lap(WriteStage::RecordBatch, m);
        self.observe_wal();

        // **What a batch minted is reported to the batch that minted it.** Under
        // `value_set = "open"` a typo creates a permanent object rather than being refused — the
        // trade the declaration makes knowingly — and the mitigation is that it is visible: the
        // caller is told the count in its own 200, and the operator gets this line.
        let created: u64 = minted_per_entry.iter().sum();
        if created > 0 {
            tracing::info!(
                minted = created,
                artifacts = ?minted
                    .iter()
                    .flat_map(|(record, _)| match record {
                        WalRecord::ArtifactPublish { layer, level, artifacts, .. } => artifacts
                            .iter()
                            .filter_map(|a| a.key.as_ref())
                            .map(|key| format!("{key} in level {level} of {layer}"))
                            .take(8)
                            .collect::<Vec<_>>(),
                        _ => Vec::new(),
                    })
                    .collect::<Vec<_>>(),
                "an ingest batch named keys no artifact held, and this layer's value set is open, \
                 so they were created carrying nothing but their names"
            );
        }

        // **N waiters, one proof.** A death partway through this loop leaves some waiters acked and
        // some not; every un-acked one gets `SubmitError::ReceiptLost` → 500, never `ExecutorDead`
        // → 503, because its ingest is durably in force. That is the widening `ReceiptLost`'s own
        // doc predicts for this task.
        for (entry, minted) in closed.into_iter().zip(minted_per_entry) {
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
                self.ack(&waiter, Ack::Ingested { entity_ids, minted }, &published);
            }
            self.ack(&last, Ack::Ingested { entity_ids, minted }, &published);
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

    /// A category key could not acquire a code: **nothing was appended, nothing applied**, exactly
    /// as a failed allocation — the mutated `vocabularies` copy is dropped with `closed`, and the
    /// live bindings are untouched. Unlike a WAL failure, the refusal is uniform: no waiter's own
    /// append was closer to the fault than any other's, so every one gets the same error.
    ///
    /// ## Why the detail is rendered here
    ///
    /// `MintError` lives in `tessera_store::vocabulary` and `ExecError` in `tessera-lifecycle`,
    /// which deliberately carries no `tessera-store` dependency, so the error cannot cross as
    /// itself. It is rendered to text on this thread and travels in
    /// [`ExecError::VocabularyRefused`], which `map_accept_error` answers as the `422` §3.6
    /// requires — naming the vocabulary and its width, both the deployment's own schema. Mapping it
    /// to a WAL or allocator failure instead would answer a bodyless 500 for a refusal the caller
    /// can act on, and would tell an operator the log was at fault when it was not.
    /// A window whose **artifact** mint could not be prepared: nothing was appended, nothing
    /// applied. Every waiter gets the same refusal, which is the cost of a decision that can only be
    /// made once the window's batches are together — see `Executor::mint_records`.
    fn fail_window_layer(
        &self,
        closed: Vec<ClosedEntry<Responder>>,
        detail: String,
        entries: u64,
        started: std::time::Instant,
    ) {
        for entry in closed {
            for waiter in entry.waiters {
                self.ack_failed(
                    &waiter,
                    ExecError::LayerRefused {
                        detail: detail.clone(),
                    },
                );
            }
        }
        self.health
            .record_window_service(entries, started.elapsed().as_nanos() as u64);
    }

    fn fail_window_mint(
        &self,
        closed: Vec<ClosedEntry<Responder>>,
        error: MintError,
        entries: u64,
        started: std::time::Instant,
    ) {
        let detail = error.to_string();
        for entry in closed {
            for waiter in entry.waiters {
                self.ack_failed(
                    &waiter,
                    ExecError::VocabularyRefused {
                        detail: detail.clone(),
                    },
                );
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
                artifacts,
            } => {
                let window = CommitWindow::new(self.next_window_seq());
                let (window, _) =
                    self.admit_ingest(window, rows, batch_id, body_hash, artifacts, respond);
                if !window.is_empty() {
                    self.close_window(window);
                }
            }
            // A window of one entry is exactly the per-command semantics this path used to have,
            // which is why there is no second deny implementation to keep in step with the first.
            Command::Change { entity, op } => {
                let mut entries = vec![DenyEntry {
                    record: WalRecord::ChangeByEntity {
                        entity_id: entity,
                        op,
                    },
                    entity,
                    op,
                    respond: Some(respond),
                }];
                // The cascade rides this path too — a window of one is still a window, and a
                // deletion admitted here that skipped it would strand every artifact depending on
                // the one deleted.
                self.cascade_dependents(&mut entries);
                self.commit_denies(entries)
            }
            Command::RegisterLayer { declaration } => self.commit_registry(
                |registry, alloc| registry.prepare_create(*declaration, alloc),
                |record| match record {
                    WalRecord::LayerCreate { layer_entity, .. } => Ack::LayerRegistered {
                        entity: *layer_entity,
                    },
                    _ => unreachable!("prepare_create returns a LayerCreate"),
                },
                respond,
            ),
            Command::DropLayer { name } => self.commit_registry(
                |registry, _| registry.prepare_drop(&name),
                |_| Ack::LayerDropped,
                respond,
            ),
            Command::CreateView {
                group,
                key,
                visibility,
                metadata,
            } => self.commit_view_create(group, key, visibility, metadata, respond),
            Command::DropView {
                group,
                key,
                delete_dangling,
            } => self.commit_view_drop(group, key, delete_dangling, respond),
            Command::DeclareAttribute { request } => {
                self.commit_attribute_declare(*request, respond)
            }
            Command::Values { request } => self.commit_values(*request, respond),
            Command::DeclareVocabulary { request } => {
                self.commit_vocabulary_declare(*request, respond)
            }
            Command::MintVocabularyValues { vocabulary, values } => {
                self.commit_vocabulary_values(vocabulary, values, respond)
            }
            Command::CreateViewGroup { declaration } => {
                self.commit_view_group_create(*declaration, respond)
            }
            Command::CreatePlainView { declaration } => {
                self.commit_plain_view_create(*declaration, respond)
            }
            Command::PublishArtifacts {
                layer,
                level,
                artifacts,
            } => self.commit_artifacts(layer, level, artifacts, respond),
            Command::GrowMemberships {
                layer,
                level,
                joins,
            } => self.commit_growth(layer, level, joins, respond),
        }
    }

    /// **Hold one accepted write's delta until the tick** (`ingest.md` §1.3, §10 ruling 6).
    ///
    /// Every route that changes a level's records — a publication, a growth, a page of a
    /// generating set, a fill — arrives here with the level version it followed, and the level's
    /// row forms take the run of them at the next tick. A record naming no level's forms is held
    /// all the same: which views hold a form is not this thread's question until it publishes.
    ///
    /// **`refused` names the growth entries the store did not take**, by their position in the
    /// record (`ArtifactStore::apply_reporting`), and they are held for nothing: a form that
    /// unioned an entity the records refused would count a member no artifact has. A record whose
    /// every entry was refused is still held, empty, because it moved the level's version and the
    /// versions of an interval's deltas must stay consecutive.
    fn hold_delta(&mut self, record: &WalRecord, before: u64, refused: &[usize]) {
        let (layer, level, kind) = match record {
            WalRecord::ArtifactPublish {
                layer,
                level,
                artifacts,
                ..
            } => (
                layer,
                *level,
                crate::artifacts::DeltaKind::Published(
                    artifacts.iter().map(|a| a.ordinal).collect(),
                ),
            ),
            WalRecord::ArtifactGrow {
                layer,
                level,
                growth,
            } => {
                // **The log's own bytes decoded, so what reaches the row forms is what reached the
                // records.** A set that does not decode is skipped and nothing else is, which is
                // the disposition `ArtifactStore::apply_growth` makes of it: that delta did not
                // enter the records either, and the alarm it raised has already been said.
                let mut joins = Vec::new();
                let mut pages = Vec::new();
                for (index, grown) in growth.iter().enumerate() {
                    if refused.contains(&index) {
                        continue;
                    }
                    let Some(joining) =
                        tessera_lifecycle::membership::deserialise_members(&grown.joining)
                    else {
                        continue;
                    };
                    match grown.set {
                        tessera_lifecycle::wal::GrownSet::Membership => {
                            joins.push((grown.ordinal, joining))
                        }
                        tessera_lifecycle::wal::GrownSet::GeneratingSet { rank, cardinality } => {
                            // **A leave, or a withdrawal, re-derives the operator whole**
                            // (`ingest.md` §1.1): a union cannot express a leave, and the
                            // withdrawal an emptied set makes moves every rank above it. A page of
                            // joins alone is unioned into the operator that is served.
                            let leaves =
                                tessera_lifecycle::membership::deserialise_leaving(&grown.leaving)
                                    .is_none_or(|leaving| !leaving.is_empty());
                            pages.push(crate::artifacts::SetPage {
                                ordinal: grown.ordinal,
                                rank,
                                joining,
                                whole: leaves || cardinality == 0,
                            });
                        }
                    }
                }
                (
                    layer,
                    *level,
                    crate::artifacts::DeltaKind::Grown { joins, pages },
                )
            }
            WalRecord::ArtifactFill {
                layer,
                level,
                ordinal,
                ..
            } => (
                layer,
                *level,
                crate::artifacts::DeltaKind::Filled(vec![*ordinal]),
            ),
            _ => return,
        };
        self.pending_forms
            .entry((layer.clone(), level))
            .or_default()
            .push(crate::artifacts::LevelDelta { before, kind });
    }

    /// **Publish every level's row forms from the deltas held since the last tick** — the one
    /// moment a served form changes (`ingest.md` §1.3, §10 ruling 6).
    ///
    /// Called from the tick and from nowhere else: a request never brings a form forward, so a
    /// level whose deltas are not yet published is served as last published, up to a tick stale.
    /// A level with no form held takes nothing here; the open's warm and the first request to
    /// reach such a level are what build one.
    fn publish_row_forms(&mut self) {
        let pending = std::mem::take(&mut self.pending_forms);
        for ((layer, level), deltas) in &pending {
            self.publish_level_forms(layer, *level, deltas);
        }
    }

    /// **Apply an interval's deltas to every held row form of one level**, in every view the
    /// generation carries (`docs/evidence/memos/2026-09-03-post-flush-artifact-frames.md`).
    ///
    /// A form at a version these deltas do not follow is dropped rather than amended, which
    /// [`crate::artifacts::ArtifactProjections::publish`] argues in full.
    ///
    /// A view two partitions carry is skipped, exactly as `Engine::warm_artifact_projections`
    /// skips it: it is `EngineError::MultiPartitionView` on the request path, so there is no row
    /// space here that a request would agree with.
    fn publish_level_forms(
        &self,
        layer: &str,
        level: u32,
        deltas: &[crate::artifacts::LevelDelta],
    ) {
        // **Where the delta's rows come from.** A stored membership's are the records' members,
        // projected; a spatial level's are the new shapes, resolved over every live segment of
        // the view — the level's shapes are rebuilt here at the version the record just moved it
        // to, and the resolution touches the new ordinals alone. An attribute predicate's members
        // are the rows carrying a value, which a record's delta says nothing about, so its form
        // takes none — see `ArtifactProjections::bring_forward`. An unregistered layer has
        // nothing to read.
        let Some(registered) = self.live.registered_layer(layer) else {
            return;
        };
        let stored = stored_membership(&registered.declaration);
        let spatial = registered.declaration.membership
            == tessera_types::layer::MembershipSource::Spatial
            && registered.declaration.shape.is_some();
        if !stored && !spatial {
            return;
        }
        let generation = self.generation.load_full();
        let mut views: std::collections::BTreeMap<&str, Option<&tessera_store::read::ViewData>> =
            std::collections::BTreeMap::new();
        for partition in generation.bundle.partitions.values() {
            for (name, data) in &partition.views {
                views
                    .entry(name.as_str())
                    .and_modify(|held| *held = None)
                    .or_insert(Some(data));
            }
        }
        self.live.with_artifacts(|store| {
            for (view, data) in views {
                let Some(data) = data else { continue };
                if stored {
                    self.artifact_projections.publish(
                        &generation.prefix,
                        view,
                        layer,
                        level,
                        store,
                        &data.row_space,
                        deltas,
                        Some(&crate::artifacts::DeltaRows::Projected),
                    );
                    continue;
                }
                // A growth and a page never reach a spatial level: the registry refuses one before
                // a record is written (`RegistryError::NotEnumerated`), so what the interval holds
                // for such a level is publications, whose new shapes are resolved over every
                // segment here.
                // A fill is here beside a publication: on a spatial level the shape *is* the
                // membership, so an ordinal whose shape was filled needs resolving exactly as a
                // new one does.
                let ordinals: Vec<u32> = deltas
                    .iter()
                    .flat_map(|delta| match &delta.kind {
                        crate::artifacts::DeltaKind::Published(ordinals)
                        | crate::artifacts::DeltaKind::Filled(ordinals) => ordinals.clone(),
                        _ => Vec::new(),
                    })
                    .collect();
                if ordinals.is_empty() {
                    continue;
                }
                let ordinals = &ordinals;
                let held = self.shapes.level(
                    view,
                    layer,
                    level,
                    store,
                    &crate::shapes::PersistedPieces::none(),
                );
                let Ok(segments) = crate::viewport::segments_with_row_bases(view, data) else {
                    continue;
                };
                let started = std::time::Instant::now();
                let mut joined: Vec<Option<croaring::Bitmap>> = vec![None; held.shapes.len()];
                let mut rows_tested = 0u64;
                for (segment, row_base) in &segments {
                    let (piece, cost) = held.resolve_ordinals(segment, ordinals);
                    rows_tested += cost.rows_tested;
                    for (ordinal, part) in piece.into_iter().enumerate() {
                        let Some(part) = part else { continue };
                        let slot = joined[ordinal].get_or_insert_with(croaring::Bitmap::new);
                        if !part.is_empty() {
                            slot.or_inplace(&part.add_offset(i64::from(*row_base)));
                        }
                    }
                }
                tracing::info!(
                    layer = %layer,
                    level,
                    view = %view,
                    artifacts = ordinals.len(),
                    segments = segments.len(),
                    rows_tested,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "a publication into a spatial level resolved its new shapes over every segment"
                );
                let rows_of = |ordinal: u32| {
                    joined
                        .get(ordinal as usize)
                        .cloned()
                        .flatten()
                        .unwrap_or_default()
                };
                self.artifact_projections.publish(
                    &generation.prefix,
                    view,
                    layer,
                    level,
                    store,
                    &data.row_space,
                    deltas,
                    Some(&crate::artifacts::DeltaRows::Resolved(&rows_of)),
                );
            }
        });
    }

    /// **Where a geometry publication's rows come from, per level of one view** — what
    /// `ArtifactProjections::extend_flushed` and `rebase_merged` are told
    /// (`crate::artifacts::SegmentRows`). A stored level projects; a spatial level takes
    /// `resolved` where the flush's pool resolved the segment against the shapes now held, and
    /// resolves the segment here otherwise — a level rebuilt by a publication since the flush was
    /// planned, or one no flush plan saw; an attribute predicate takes nothing.
    fn segment_rows_of(
        &self,
        view: &str,
        layer: &str,
        level: u32,
        segment: Option<&tessera_store::read::SegmentData>,
        resolved: &[crate::shapes::ShapePiece],
        store: &tessera_lifecycle::membership::ArtifactStore,
    ) -> Option<crate::artifacts::SegmentRows> {
        let registered = self.live.registered_layer(layer)?;
        if stored_membership(&registered.declaration) {
            return Some(crate::artifacts::SegmentRows::Projected);
        }
        if registered.declaration.membership != tessera_types::layer::MembershipSource::Spatial
            || registered.declaration.shape.is_none()
        {
            return None;
        }
        let held = self.shapes.level(
            view,
            layer,
            level,
            store,
            &crate::shapes::PersistedPieces::none(),
        );
        if let Some(piece) = resolved.iter().find(|piece| {
            piece.level.layer == layer
                && piece.level.level == level
                && Arc::ptr_eq(&piece.level, &held)
        }) {
            return Some(crate::artifacts::SegmentRows::Resolved(Arc::clone(
                &piece.rows,
            )));
        }
        let segment = segment?;
        let (rows, cost) = held.resolve(segment);
        tracing::info!(
            layer = %layer,
            level,
            view = %view,
            seg_id = %segment.seg_id,
            rows_tested = cost.rows_tested,
            rows_interior = cost.rows_interior,
            elapsed_ms = cost.elapsed_ms,
            "a geometry publication resolved its segment against a spatial level's shapes"
        );
        Some(crate::artifacts::SegmentRows::Resolved(Arc::new(rows)))
    }

    /// Validate, allocate, append, sync, apply — `commit_registry`'s sequence, for the same reason
    /// and with one addition: the record lands in **two** structures, the registry (for a level
    /// that grew) and the store (for the memberships themselves), and both are applied under the
    /// one lock the preparation was made under.
    ///
    /// **Nothing is applied before the record is durable.** A membership applied and then lost is
    /// an artifact whose `tessera_id` a caller already holds and whose members come back empty —
    /// served as absent, indistinguishable from one that failed its criterion. So the append comes
    /// first, and a failure means the batch does not exist.
    fn commit_artifacts(
        &mut self,
        layer: String,
        level: u32,
        mut incoming: Vec<IncomingArtifact>,
        respond: Responder,
    ) {
        // Read before the record is applied, because it is what says a held row form is the form
        // this publication follows — see [`Self::bring_artifacts_forward`].
        let before = self
            .live
            .with_artifacts(|store| store.level_version(&layer, level));
        // **Partitioned before any ordinal is claimed** (`ingest.md` §1.5, R3): a key the level
        // holds is compared under the fill rule and resolves to its existing ordinal, and only the
        // keys it does not hold are published. The answer is up to three kinds of record, in the
        // order they are appended and applied.
        // **The complement, taken here and nowhere else** (`ingest.md` §2.3): a membership spelled
        // by exclusion is materialised on the executor, against the view's entity set as it stands
        // at this step, *before* the record is written — so the log, the store and every read path
        // carry the inclusion the other spelling would have produced, and no serving path can
        // evaluate a complement against a viewer's mask, which would disclose the existence of
        // items outside it (`annotation-write-cycle.md` §6.1).
        //
        // **The held-key refusal is taken first**: an exclusion on a key the level holds is a
        // `409` ([`RegistryError::ExclusionOnHeldKey`], which `prepare_put` makes below over the
        // same store), and the walk of the view's entities is the most expensive thing this route
        // does — so the refusal spends nothing, as every other refusal on this path does not.
        if let Err(e) = self.materialise_exclusions(&layer, level, &mut incoming) {
            respond.fail(e);
            return;
        }
        let prepared = self.live.with_publication_state(|registry, store, alloc| {
            registry.prepare_put(&layer, level, &incoming, store, alloc)
        });
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(e) => {
                respond.fail(refusal_of(e));
                return;
            }
        };
        let records: Vec<&WalRecord> = prepared
            .publish
            .iter()
            .chain(prepared.fills.iter())
            .chain(prepared.growth.iter())
            .collect();
        if records.is_empty() {
            // Every key was held and every part identical: nothing to append, on
            // `commit_growth`'s no-op rule, and the acknowledgement is the held artifacts' own.
            let published = Published::nothing_prepared(&prepared);
            let ack = Ack::ArtifactsPublished {
                entities: prepared.entities,
                created: 0,
                without_content: 0,
                filled: 0,
                joined: 0,
            };
            respond.ack(ack, &published);
            return;
        }

        // The position each record will occupy — read **before** its append, because that is the
        // bound rotation must not reclaim past, and after the append it names the next record
        // instead. Several records, one fsync: the batch is the commit unit, as a window's is.
        let mut positions = Vec::with_capacity(records.len());
        let appended = records.iter().try_for_each(|record| {
            positions.push(self.wal.position());
            self.wal.append(record)
        });
        if let Err(e) = appended.and_then(|()| self.wal.fsync()) {
            // The ordinals and any extension block this preparation spent are not returned, on
            // `commit_registry`'s argument: a torn append that replays would otherwise land these
            // artifacts on entities a later batch also holds.
            tracing::error!(
                error = %e,
                "ALARM: an artifact publication could not be made durable; the artifacts do not \
                 exist and their reserved ids are spent"
            );
            respond.fail(ExecError::Wal(e));
            return;
        }

        let mut refused_per_record: Vec<Vec<usize>> = vec![Vec::new(); records.len()];
        let undecodable = self.live.with_publication_state(|registry, store, _| {
            let mut undecodable = 0;
            for ((record, position), refused) in
                records.iter().zip(&positions).zip(&mut refused_per_record)
            {
                registry.apply(record);
                undecodable += store.apply_reporting(record, *position, refused);
            }
            undecodable
        });
        if undecodable > 0 {
            // Unreachable in practice — these bytes were serialised from a live bitmap moments ago
            // — and alarmed rather than asserted because the alternative to noticing is an artifact
            // that is silently absent.
            tracing::error!(
                count = undecodable,
                "ALARM: an artifact record did not survive its own round trip"
            );
        }
        // **Every delta the store just took is held for the tick, in the same order**
        // (`Self::hold_delta`): the publication's ordinals, the growth's joins and pages, and the
        // ordinals a fill changed. Each record moved the level's version by one, so `before` walks
        // with them.
        for ((at, record), refused) in (before..).zip(records.iter()).zip(&refused_per_record) {
            self.hold_delta(record, at, refused);
        }
        let published = Published::registry_applied(records[0]);
        // A shape layer's held shapes are rebuilt at the tick's publication, at the version these
        // records moved the level to, and the new shapes resolved over every segment the
        // generation serves (`polygon-membership.md` §6.3: built at publication and at open, never
        // on a request).
        // Durable in the log, not yet in a manifest. The registry half of this record reaches
        // `SEGMENTS-<n>.json` at the next flush on the deny lane's mechanism; the membership half
        // has nowhere to reach, which is what the rotation pin holds the log for.
        self.deny_dirty = true;
        respond.ack(
            Ack::ArtifactsPublished {
                entities: prepared.entities,
                created: prepared.created,
                without_content: prepared.without_content,
                filled: prepared.fills.len() as u64,
                joined: prepared.joined,
            },
            &published,
        );
    }

    /// Materialise every membership this batch spelled by exclusion, and answer the refusal where
    /// the layer cannot be read (`ingest.md` §2.3).
    ///
    /// **The view's entity set is every entity holding a row in it or buffered for it, deleted
    /// entities excluded**, and the membership is one `andnot` of the caller's list over it. On a
    /// group-scoped layer the view is the artifact's own; on an entity-scoped one it is the union
    /// of the views the layer is drawn on, which is the layer's whole corpus and the set the
    /// build complements against (`layers.rs::resolve_artifact`, `0..high_water`).
    ///
    /// **Two divergences from the build's byte-identity are structural and stated rather than
    /// closed** (`ingest.md` §2.3): an entity ingested after this step is in the inclusion
    /// spelling's membership and not in the exclusion's, and a suppressed entity is in both,
    /// suppression not being deletion.
    ///
    /// The size is logged rather than answered: how large a membership a caller's exclusion came
    /// to is operator-facing, and a count of the corpus is not something a publication's
    /// acknowledgement carries (C8).
    fn materialise_exclusions(
        &self,
        layer: &str,
        level: u32,
        incoming: &mut [IncomingArtifact],
    ) -> Result<(), ExecError> {
        if !incoming.iter().any(|a| a.excluding.is_some()) {
            return Ok(());
        }
        let Some(registered) = self.live.registered_layer(layer) else {
            // The registry refuses the unknown layer a statement later, in its own words.
            return Ok(());
        };
        // The `409` before the walk. The rule is the registry's and `prepare_put` states it over
        // the same store index a moment later; what is here is the order, so that a repeat of a
        // publication the level already holds costs a key lookup rather than a view's rows.
        let held = self.live.with_artifacts(|store| {
            incoming
                .iter()
                .filter(|artifact| artifact.excluding.is_some())
                .filter_map(|artifact| Some((artifact, artifact.key.as_deref()?)))
                .find(|(artifact, key)| {
                    store
                        .ordinal_of_key(layer, level, artifact.view.as_deref(), key)
                        .is_some()
                })
                .map(|(_, key)| key.to_string())
        });
        if let Some(key) = held {
            return Err(refusal_of(
                tessera_lifecycle::RegistryError::ExclusionOnHeldKey {
                    layer: layer.to_string(),
                    level,
                    key,
                },
            ));
        };
        let generation = self.generation.load_full();
        let mut sets: std::collections::HashMap<Option<String>, croaring::Bitmap> =
            std::collections::HashMap::new();
        for artifact in incoming.iter_mut() {
            let Some(excluded) = artifact.excluding.as_ref().map(|e| e.cardinality()) else {
                continue;
            };
            let view = artifact.view.clone();
            let entities = sets.entry(view.clone()).or_insert_with(|| {
                // **The artifact names a view's *key* and the generation holds view *ids***
                // (`quarter:q1`; `views.md` §3.1), so the key is resolved against the layer's own
                // declared views rather than used as an id — which matched nothing and made every
                // group-scoped complement empty.
                let views: Vec<String> = match &view {
                    Some(key) => registered
                        .declaration
                        .views
                        .iter()
                        .filter(|id| crate::artifacts::view_key(id) == key)
                        .cloned()
                        .collect(),
                    None => registered.declaration.views.clone(),
                };
                view_entities(&generation, &views)
            });
            let started = std::time::Instant::now();
            let members = artifact
                .complement_against(entities)
                .expect("the artifact carries an exclusion");
            tracing::info!(
                layer = %layer,
                view = ?view,
                excluded,
                in_view = entities.cardinality(),
                members,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "a membership spelled by exclusion was complemented against the view's entities"
            );
        }
        Ok(())
    }

    /// Grow the memberships of artifacts that already exist — `commit_artifacts`'s sequence
    /// (validate, append, sync, apply) with nothing allocated, because a join takes no ordinal and
    /// no entity.
    ///
    /// **This is the second way state enters the artifact store, and the first that is not a whole
    /// record.** It is a *growth* path, which is why it is admissible at all: write-path §5.4's two
    /// removal rules govern how a bit **leaves** a membership, and this adds bits that are then
    /// retired by exactly the routes every other member is retired by — `ArtifactStore::grow`
    /// carries the argument in full, and there is one such method rather than one per caller.
    ///
    /// **Nothing is applied before the record is durable**, on `commit_artifacts`'s reason, one
    /// step sharper: a join applied and then lost is an artifact that comes back from a restart
    /// *without* the point, which nothing distinguishes from an artifact that failed its existence
    /// criterion. The same failure is what the pin `ArtifactStore::mark_growth_packed` releases
    /// exists against, on the packing side.
    ///
    /// **This is the control plane's route into growth, and it is no longer the only one.** An
    /// ingest batch carrying a column named for a layer grows the same memberships through
    /// `Executor::close_window` instead (`artifacts-from-points.md` §6.2) — resolved at admission,
    /// appended inside the window's own fsync, and applied through the same `ArtifactStore::grow`
    /// this command reaches. The two share the record and the store method rather than the command,
    /// because a batch's entities do not exist until its window allocates and a command cannot wait
    /// inside one.
    ///
    /// **Minting is that close's and not this command's** (§6.3). An unknown key here is refused
    /// whatever the layer's value set says: this route names an artifact to add members to, where a
    /// membership column names the artifact a *point* belongs to and may therefore create it. Where
    /// the two do agree is the thread — a mint claims ordinals serially on this executor, exactly as
    /// `commit_artifacts` does, which is why neither claim is made in a handler.
    fn commit_growth(
        &mut self,
        layer: String,
        level: u32,
        joins: Vec<tessera_lifecycle::IncomingGrowth>,
        respond: Responder,
    ) {
        // `commit_artifacts`' reason: the version a held row form must be at for this delta to be
        // the one it is missing.
        let before = self
            .live
            .with_artifacts(|store| store.level_version(&layer, level));
        // The receipt is read beside the preparation, under the same lock and **before** the
        // record is applied: afterwards every joining member is a member, and how many were new
        // is gone.
        let prepared = self.live.with_publication_state(|registry, store, _| {
            let prepared = registry.prepare_grow(&layer, level, &joins, store)?;
            let grown = growth_receipt(registry, store, &layer, level, &joins, &prepared);
            Ok::<_, tessera_lifecycle::RegistryError>((prepared, grown))
        });
        let (prepared, grown) = match prepared {
            Ok(prepared) => prepared,
            Err(e) => {
                respond.fail(refusal_of(e));
                return;
            }
        };
        // The fills first, then the growth: the order the records are applied in, and the order a
        // held form takes them in below.
        let records: Vec<&WalRecord> = prepared
            .fills
            .iter()
            .chain(prepared.growth.iter())
            .collect();
        if records.is_empty() {
            // Every key resolved, nothing was joining and every part was held identically. No
            // record is owed for a no-op, and appending an empty one would pin the log at a
            // growth that changed nothing.
            respond.ack(
                Ack::MembershipsGrown { grown },
                &Published::nothing_to_apply(&joins),
            );
            return;
        }

        // The position each record will occupy, read **before** its append — `commit_artifacts`'s
        // reason, and here it is the bound that holds the log until the fold rewrites the level
        // whole, since a grown or filled record sits below the high-water the tail pack starts
        // from. Several records, one fsync: the batch is the commit unit.
        let mut positions = Vec::with_capacity(records.len());
        let appended = records.iter().try_for_each(|record| {
            positions.push(self.wal.position());
            self.wal.append(record)
        });
        if let Err(e) = appended.and_then(|()| self.wal.fsync()) {
            tracing::error!(
                error = %e,
                "ALARM: a membership growth could not be made durable; the entities did not join \
                 and no part was filled"
            );
            respond.fail(ExecError::Wal(e));
            return;
        }

        let mut refused_per_record: Vec<Vec<usize>> = vec![Vec::new(); records.len()];
        let undecodable = self.live.with_publication_state(|_, store, _| {
            records
                .iter()
                .zip(&positions)
                .zip(&mut refused_per_record)
                .map(|((record, position), refused)| {
                    store.apply_reporting(record, *position, refused)
                })
                .sum::<usize>()
        });
        if undecodable > 0 {
            // Unreachable in practice — these bytes were serialised from a live bitmap moments ago
            // — and alarmed rather than asserted, because the alternative to noticing is an
            // artifact that is quietly the size it was before.
            tracing::error!(
                count = undecodable,
                "ALARM: a membership growth did not survive its own round trip"
            );
        }
        for ((at, record), refused) in (before..).zip(records.iter()).zip(&refused_per_record) {
            self.hold_delta(record, at, refused);
        }
        let published = Published::registry_applied(records[0]);
        // A growth against an artifact **above** its level's high-water is carried by the next
        // tail pack like any other unpublished record; one below it waits for the fold, held in the
        // log by the pin. Marking the manifest dirty is what gets the first case published.
        self.deny_dirty = true;
        respond.ack(Ack::MembershipsGrown { grown }, &published);
    }

    /// Validate, allocate, append, sync, apply — in that order, which is the whole of the
    /// registry's durability contract.
    ///
    /// **Nothing is applied before the record is durable, and this is the opposite posture from a
    /// deny.** A suppression is applied to the live overlay even when its append fails, because
    /// leaving an accepted deny unapplied is a fail-open and "in force but not durable" is the
    /// safer of two bad states. A registration has no such asymmetry: a layer that exists in memory
    /// and not in the log comes back from a restart as a name that is free again, having already
    /// handed a caller a `tessera_id` for its entity. So the append comes first and a failure means
    /// the layer does not exist — which is what the caller is told.
    fn commit_registry(
        &mut self,
        prepare: impl FnOnce(
            &mut LayerRegistry,
            &mut Allocator,
        )
            -> std::result::Result<WalRecord, tessera_lifecycle::RegistryError>,
        ack_of: impl FnOnce(&WalRecord) -> Ack,
        respond: Responder,
    ) {
        let prepared = self.live.with_registry_and_allocator(prepare);
        let record = match prepared {
            Ok(record) => record,
            Err(e) => {
                respond.fail(ExecError::LayerRefused {
                    detail: e.to_string(),
                });
                return;
            }
        };

        if let Err(e) = self.wal.append(&record).and_then(|()| self.wal.fsync()) {
            // The ids the preparation spent are **not** returned. The allocator is monotone with no
            // free list, and re-issuing a run whose `LayerCreate` may or may not have reached the
            // disk is the one outcome worse than losing 65 536 ids out of four billion: a torn
            // append that replays would land a layer on entities a later registration also holds.
            tracing::error!(
                error = %e,
                "ALARM: a layer registration could not be made durable; the layer does not exist \
                 and its reserved ids are spent"
            );
            respond.fail(ExecError::Wal(e));
            return;
        }

        let ack = ack_of(&record);
        self.live.apply_registry_record(&record);
        // **A dropped layer's derived structures go with it.** Neither cache had a removal path,
        // so each was bounded by the triples a process had ever seen rather than the ones it
        // holds — gigabytes a level at the campaign's target, pinned for the life of the process.
        // Retention only, never correctness: a tombstoned name never resolves through the registry
        // again, so nothing held here was reachable to be served.
        //
        // ⊘ **The store's own copy of a dropped layer's memberships is not released**, because
        // `ArtifactStore::remove_layer` is reached from nowhere — a drop touches the registry and
        // stops there. That is the larger half of the same retention, and it is a write-path
        // question rather than a caching one: releasing it changes what the next fold repacks and
        // how far back the rotation pin holds the log.
        if let WalRecord::LayerDrop { name } = &record {
            // The deltas held for the tick describe forms that are going with the layer.
            self.pending_forms.retain(|(layer, _), _| layer != name);
            self.artifact_projections.forget(name);
            self.lineages.forget(name);
            self.level_contents.forget(name);
        }
        let published = Published::registry_applied(&record);
        // The registry is durable in the log but not yet in a manifest, and a rotation reclaims the
        // log. Marking the manifest dirty is what gets it published at the next flush, on the same
        // mechanism a deny uses to reach `SEGMENTS-<n>.json`.
        self.deny_dirty = true;
        respond.ack(ack, &published);
    }

    /// `PUT /control/views/{group}/{key}` — create a view of a group while the service runs
    /// (`views.md` §3.2, decision 0108).
    ///
    /// **The shape is `commit_registry`'s**, because the obligation is: prepare against state only
    /// this thread may write, append, fsync, apply, publish, ack. What differs is that a view has
    /// a *row space* — an empty one — so the apply reaches the bundle rather than stopping at a
    /// live-state map, and the ack therefore rides a generation swap rather than a registry token.
    ///
    /// **The ordinal is spent whatever happens next.** A create whose append fails is refused with
    /// its ordinal unreturned, exactly as a failed registration keeps its ids: an ordinal reissued
    /// after a torn append that replay might still apply is two views under one alias, which is
    /// worse than a gap in a sequence nothing counts.
    fn commit_view_create(
        &mut self,
        group: String,
        key: String,
        visibility: Option<Vec<String>>,
        metadata: std::collections::BTreeMap<String, tessera_types::view::ViewMetadataValue>,
        respond: Responder,
    ) {
        let started = std::time::Instant::now();
        let generation = self.generation.load_full();
        let Some(descriptor) = generation
            .bundle
            .manifest
            .groups
            .iter()
            .find(|g| g.name == group)
        else {
            // The same 404 an unknown view id is, and for the same reason: a group nobody declared
            // and a key no view holds must be one answer, or the difference between them is an
            // existence oracle over the roster.
            respond.fail(ExecError::ViewUnknown {
                detail: format!(
                    "unknown view group '{group}'. A group is declared at a build and its views \
                     grow at a running service (views §3.1); there is no create that mints a group"
                ),
            });
            self.health.note_work_refused();
            return;
        };
        let facts = tessera_lifecycle::GroupFacts {
            name: &descriptor.name,
            members_of: descriptor.members_of.as_deref(),
            metadata: &descriptor.metadata,
        };
        let prepared = self
            .live
            .with_roster(|roster| roster.prepare_create(facts, &key, visibility, metadata));
        let record = match prepared {
            Ok(record) => record,
            Err(e) => {
                respond.fail(roster_error(e));
                self.health.note_work_refused();
                return;
            }
        };
        if let Err(e) = self.wal.append(&record).and_then(|()| self.wal.fsync()) {
            tracing::error!(
                error = %e,
                view = %format!("{group}:{key}"),
                "ALARM: a view creation could not be made durable; the view does not exist"
            );
            respond.fail(ExecError::Wal(e));
            return;
        }
        self.live.with_roster(|roster| roster.apply(&record));
        let published = self.publish_roster(&generation, started, &[]);
        // Durable in the log and not yet in a manifest, and a rotation reclaims the log — so the
        // roster reaches `SEGMENTS-<n>.json` on the mechanism a deny already uses (`views.md`
        // §3.2: the durable home is the segments manifest).
        self.deny_dirty = true;
        respond.ack(Ack::ViewCreated, &published);
    }

    /// `PUT /control/attributes` — declare an attribute column while the service runs
    /// (`ingest.md` §1.3, §6.3; decision 0136).
    ///
    /// **The shape is [`Self::commit_view_create`]'s**: resolve against state only this thread
    /// may write, append, fsync, apply, publish, ack. The apply reaches the bundle, because the
    /// served schema is the manifest's `declared_scalars` and every reader takes it from there:
    /// the successor generation carries the column at the tail of that list, the filter columns
    /// hold an empty stack for it so the next flush's extent composes onto something, and a
    /// vocabulary no column named before is narrowed to the width this column stores.
    ///
    /// **Ingestable at the ack.** A batch decoded against the successor's schema carries the
    /// column; one decoded against the predecessor's is shorter by one and is padded with the
    /// column's absence at its window's close (`crate::attributes::pad_to_schema`). The column is
    /// listed on `/v1/meta` from the swap and absent for every entity until a row fills it.
    ///
    /// An identical redeclaration answers the existing identity with nothing appended; a
    /// differing one is a conflict (`ingest.md` §1.1). A failed append means the column does not
    /// exist, on the layer registration's rule.
    /// `POST /control/values` — fill attribute values on entities that already exist
    /// (`ingest.md` §1.4).
    ///
    /// **It allocates nothing and creates no row.** Every entity was resolved at the boundary, so
    /// this pass adds cells to entities that have them and members to artifacts that hold them; a
    /// subject that does not exist refused the batch before it was submitted (`ingest.md` §1.6).
    ///
    /// **The fill rule is evaluated here and nowhere else** (`ingest.md` §1.1), beside the join
    /// arm and for its reason (decision 0116): the sources are the commit-window buffer, the
    /// unflushed fills and the flushed homes, and only this thread moves any of them. An absent
    /// cell takes the value, a cell holding the identical value is a no-op, and a cell holding a
    /// different value refuses the whole batch with a `409` naming the column and the key and
    /// never the held value.
    ///
    /// **One append, one fsync, one apply.** The values record and the growth records its layer
    /// columns produced are made durable together, so there is no state in which a cell is filled
    /// and its membership is not (write-path §7.3).
    fn commit_values(&mut self, request: tessera_lifecycle::ValuesRequest, respond: Responder) {
        let started = std::time::Instant::now();
        let generation = self.generation.load_full();

        // **The batch-id replay check, on the executor** ([`BatchState`]'s rule). A values batch
        // allocates nothing, so a replay has no ids to hand back; a byte-identical retry runs the
        // pass below and finds every cell held identically, which is the fill rule's own no-op.
        if let Some((held_hash, _)) = self.live.accepted_batch(&request.batch_id) {
            if held_hash != request.body_hash {
                self.ack_failed(
                    &respond,
                    ExecError::BatchConflict {
                        batch_id: request.batch_id.clone(),
                    },
                );
                self.health.note_work_refused();
                return;
            }
        }

        let planned = match plan_fills(&generation, &request) {
            Ok(planned) => planned,
            Err(e) => {
                self.ack_failed(&respond, e);
                self.health.note_work_refused();
                return;
            }
        };
        // **A layer column on a values row is a membership join** (`ingest.md` §1.4), taking the
        // growth route's own record. A key no artifact holds is refused rather than minted: a
        // values batch creates nothing, and minting from one would make a typo a permanent object
        // on the one route whose rule is that it allocates nothing.
        let (memberships, _) = match self.resolve_memberships(&request.artifacts) {
            Ok(resolved) => resolved,
            Err(detail) => {
                self.ack_failed(&respond, ExecError::LayerRefused { detail });
                self.health.note_work_refused();
                return;
            }
        };
        let growth = match values_growth_records(&memberships, &request.rows) {
            Ok(records) => records,
            Err(detail) => {
                self.ack_failed(&respond, ExecError::ValuesRefused { detail });
                self.health.note_work_refused();
                return;
            }
        };
        // Read beside the preparation and **before** the apply, on `growth_receipt`'s rule:
        // afterwards every joining member is a member and how many were new is gone.
        let joined = self.live.with_artifacts(|store| {
            growth
                .iter()
                .map(|record| new_members_of(record, store))
                .sum::<u64>()
        });

        let values_record = WalRecord::ValuesBatch {
            batch_id: request.batch_id.clone(),
            body_hash: request.body_hash,
            view: Some(request.view.clone()),
            columns: request.columns.clone(),
            rows: request
                .rows
                .iter()
                .map(|row| tessera_lifecycle::wal::ValuesRow {
                    entity_id: row.entity,
                    values: row.values.clone(),
                })
                .collect(),
        };
        // The level version each growth record is the delta against, read before the apply moves
        // it — `commit_growth`'s rule.
        let before: Vec<u64> = growth
            .iter()
            .map(|record| match record {
                WalRecord::ArtifactGrow { layer, level, .. } => self
                    .live
                    .with_artifacts(|store| store.level_version(layer, *level)),
                _ => 0,
            })
            .collect();
        // The position **before** each append is where the record lands, and the values record's
        // is what pins the log until the flush writes its cells (`ingest.md` §1.4).
        let values_position = self.wal.position();
        let mut positions = Vec::with_capacity(growth.len());
        let appended = self.wal.append(&values_record).and_then(|()| {
            growth.iter().try_for_each(|record| {
                positions.push(self.wal.position());
                self.wal.append(record)
            })
        });
        if let Err(e) = appended.and_then(|()| self.wal.fsync()) {
            tracing::error!(
                error = %e,
                "ALARM: a values batch could not be made durable; no cell was filled and no \
                 membership grew"
            );
            respond.fail(ExecError::Wal(e));
            self.health.note_work_refused();
            return;
        }
        self.observe_wal();

        // The growth applies through the artifact store's own path, exactly as a page on the
        // growth route does, and is held in the log at its record until the fold rewrites the
        // level.
        let mut refused_per_record: Vec<Vec<usize>> = vec![Vec::new(); growth.len()];
        let undecodable = self.live.with_publication_state(|_, store, _| {
            growth
                .iter()
                .zip(&positions)
                .zip(refused_per_record.iter_mut())
                .map(|((record, position), refused)| {
                    store.apply_reporting(record, *position, refused)
                })
                .sum::<usize>()
        });
        if undecodable > 0 {
            // Unreachable in practice — these bytes were serialised from a live bitmap moments ago
            // — and alarmed rather than asserted, on `commit_growth`'s rule.
            tracing::error!(
                count = undecodable,
                "ALARM: a values batch's membership growth did not survive its own round trip"
            );
        }
        for ((record, at), refused) in growth.iter().zip(&before).zip(&refused_per_record) {
            self.hold_delta(record, *at, refused);
        }

        // **The cells reach the buffer's fill map**, which is what the next flush writes into the
        // family's entity-space extent and the record blob (`ingest.md` §6.3). The map is cloned
        // with the buffer, on the immutable-snapshot rule every generation is built by.
        let mut buffer = (*generation.buffer).clone();
        for (entity, fill) in planned.fills {
            buffer.fill(entity, fill, |value| matches!(value, WalScalar::Null));
            buffer.set_fill_wal_pos(entity, values_position);
        }
        for (entity, owner_view, fill) in planned.scoped_fills {
            buffer.fill_scoped(entity, owner_view.clone(), fill, |value| {
                matches!(value, WalScalar::Null)
            });
            buffer.set_scoped_fill_wal_pos(entity, &owner_view, values_position);
        }
        self.health
            .buffered_items
            .store(buffer.len(), Ordering::SeqCst);
        let next = Generation {
            prefix: generation.prefix.clone(),
            // **Unmoved**: no row moved and no segment was published. What moved is the buffer's
            // fill map, which no permutation and no mask reads.
            segments_version: generation.segments_version,
            watermark: generation.watermark,
            bundle: Arc::clone(&generation.bundle),
            dict: Arc::clone(&generation.dict),
            postings: Arc::clone(&generation.postings),
            fragments: Arc::clone(&generation.fragments),
            external_index: Arc::clone(&generation.external_index),
            delta_postings: generation.delta_postings.clone(),
            overlay_version: generation.overlay_version,
            overlay: Arc::clone(&generation.overlay),
            buffer: Arc::new(buffer),
            vocabularies: Arc::clone(&generation.vocabularies),
            filter_columns: Arc::clone(&generation.filter_columns),
            suggest: Arc::clone(&generation.suggest),
            denied: Arc::clone(&generation.denied),
        };
        let published = self.publish(next, started);
        // A values batch allocates no entity, so the index records none: the batch id and the
        // body hash are the whole of what a retry is answered off.
        self.live
            .record_accepted_batch(request.batch_id.clone(), request.body_hash, Vec::new());
        // A growth above its level's high-water is published by the next tail pack, on
        // `commit_growth`'s mechanism.
        if !growth.is_empty() {
            self.deny_dirty = true;
        }
        respond.ack(
            Ack::ValuesFilled {
                filled: planned.filled,
                held: planned.held,
                joined,
            },
            &published,
        );
    }

    fn commit_attribute_declare(
        &mut self,
        request: tessera_lifecycle::AttributeRequest,
        respond: Responder,
    ) {
        let started = std::time::Instant::now();
        let generation = self.generation.load_full();
        let resolved = crate::attributes::resolve(
            &request,
            &generation.bundle.manifest,
            &generation.vocabularies,
            |name| self.live.registered_layer(name).is_some(),
        );
        let (compiled, narrow) = match resolved {
            Ok(crate::attributes::Resolution::Existing) => {
                respond.ack(
                    Ack::AttributeDeclared { existing: true },
                    &Published::already_declared(&request),
                );
                return;
            }
            Ok(crate::attributes::Resolution::New { compiled, narrow }) => (compiled, narrow),
            Err(e) => {
                respond.fail(e);
                self.health.note_work_refused();
                return;
            }
        };
        let (entity, scoped) = match &compiled {
            crate::attributes::CompiledAttribute::Entity(d) => (vec![d.clone()], Vec::new()),
            crate::attributes::CompiledAttribute::Scoped(f) => (Vec::new(), vec![f.clone()]),
        };
        let manifest = generation.bundle.manifest.with_attributes(&entity, &scoped);
        // An entity-scoped column takes an empty stack in the filter columns, at its position in
        // the served list; a scoped family's per-view columns are opened by the first flush that
        // writes one, as a family declared at the build is for a view created since. Built before
        // the append, so a column this process cannot hold is refused with nothing written.
        let filter_columns = match &compiled {
            crate::attributes::CompiledAttribute::Entity(d) => {
                let declared_index = manifest.declared_scalars.len() - 1;
                match generation.filter_columns.with_runtime_column(
                    d,
                    declared_index,
                    &manifest.vocabularies,
                ) {
                    Ok(columns) => Arc::new(columns),
                    Err(e) => {
                        respond.fail(ExecError::AttributeRefused {
                            detail: format!("attribute '{}': {e}", request.name),
                        });
                        self.health.note_work_refused();
                        return;
                    }
                }
            }
            crate::attributes::CompiledAttribute::Scoped(_) => {
                Arc::clone(&generation.filter_columns)
            }
        };
        let record = WalRecord::AttributeDeclare {
            declaration: Box::new(compiled.declaration(request.title.clone())),
        };
        if let Err(e) = self.wal.append(&record).and_then(|()| self.wal.fsync()) {
            tracing::error!(
                error = %e,
                attribute = %request.name,
                "ALARM: an attribute declaration could not be made durable; the column does not \
                 exist"
            );
            respond.fail(ExecError::Wal(e));
            return;
        }

        // The apply: the live list first, then the successor generation built from it.
        self.live
            .with_attributes(|attributes| attributes.push(compiled.clone()));
        let vocabularies = match narrow {
            Some((vocabulary, width)) => {
                let mut vocabularies: Vocabularies = (*generation.vocabularies).clone();
                if let Some(minter) = vocabularies.get_mut(&vocabulary) {
                    // Checked by `resolve` against the same bindings; a code bound past the width
                    // between the two reads cannot happen, minting being this thread's alone.
                    if let Err(code) = minter.narrow_to(width) {
                        tracing::error!(
                            vocabulary = %vocabulary,
                            code,
                            "ALARM: a vocabulary bound a code past a width the executor had just \
                             checked it against; the declaration is durable and the width stays \
                             the wider one"
                        );
                    }
                }
                Arc::new(vocabularies)
            }
            None => Arc::clone(&generation.vocabularies),
        };
        // A category over a vocabulary no column named before has no suggestion index, the open
        // building one only for the vocabularies a column names; built here, on this thread, as
        // the open builds it, so the suggest verb answers the column from the acknowledgement
        // rather than from the next restart. A build that fails is omitted and warned, on
        // `SuggestIndexes::build`'s rule: the suggest verb refuses the column and nothing else is
        // affected.
        let suggest = match compiled.category() {
            Some((vocabulary, _)) if generation.suggest.get(vocabulary).is_none() => {
                let built = crate::suggest::SuggestIndexes::build(
                    &self.suggest_dir,
                    &vocabularies,
                    [vocabulary.to_string()],
                    &self.pool,
                );
                Arc::new(generation.suggest.with_built(built))
            }
            _ => Arc::clone(&generation.suggest),
        };
        let bundle = generation.bundle.with_views(manifest);
        let denied = Arc::new(crate::compose::derive_denied(&generation.overlay, &bundle));
        let next = Generation {
            prefix: generation.prefix.clone(),
            // **Unmoved**, on `publish_roster`'s argument: no row moved.
            segments_version: generation.segments_version,
            watermark: generation.watermark,
            bundle,
            dict: Arc::clone(&generation.dict),
            postings: Arc::clone(&generation.postings),
            fragments: Arc::clone(&generation.fragments),
            external_index: Arc::clone(&generation.external_index),
            delta_postings: generation.delta_postings.clone(),
            overlay_version: generation.overlay_version,
            overlay: Arc::clone(&generation.overlay),
            buffer: Arc::clone(&generation.buffer),
            vocabularies,
            filter_columns,
            suggest,
            denied,
        };
        let published = self.publish(next, started);
        // Durable in the log and not yet in a manifest, and a rotation reclaims the log: the
        // declaration reaches `SEGMENTS-<n>.json` on the mechanism a deny already uses.
        self.deny_dirty = true;
        respond.ack(Ack::AttributeDeclared { existing: false }, &published);
    }

    /// `PUT /control/view_groups/{name}` — declare a view group while the service runs
    /// (`ingest.md` §1.3; decision 0136).
    ///
    /// **The shape is [`Self::commit_attribute_declare`]'s**: resolve against state only this
    /// thread may write, append, fsync, apply, publish, ack. The apply reaches the bundle,
    /// because the served roster is the manifest's `groups` and every reader takes it from there:
    /// the successor generation carries the group with an empty roster, and a view of it may be
    /// created in the next request.
    ///
    /// An identical redeclaration answers the group that exists; a differing one is a conflict
    /// (`ingest.md` §1.1). A failed append means the group does not exist.
    fn commit_view_group_create(
        &mut self,
        declaration: tessera_lifecycle::wal::ViewGroupDeclaration,
        respond: Responder,
    ) {
        let started = std::time::Instant::now();
        let generation = self.generation.load_full();
        let compiled = match crate::view_declarations::resolve_group(
            &declaration,
            &generation.bundle.manifest,
        ) {
            Ok(crate::view_declarations::Resolution::Existing) => {
                respond.ack(
                    Ack::ViewGroupCreated { existing: true },
                    &Published::already_declared_view(&declaration.name),
                );
                return;
            }
            Ok(crate::view_declarations::Resolution::New(compiled)) => *compiled,
            Err(e) => {
                respond.fail(e);
                self.health.note_work_refused();
                return;
            }
        };
        let record = WalRecord::ViewGroupCreate {
            declaration: Box::new(declaration),
        };
        if let Err(e) = self.wal.append(&record).and_then(|()| self.wal.fsync()) {
            tracing::error!(
                error = %e,
                group = %compiled.name,
                "ALARM: a view group declaration could not be made durable; the group does not \
                 exist"
            );
            respond.fail(ExecError::Wal(e));
            return;
        }
        self.live
            .with_view_declarations(|declarations| declarations.push_group(compiled.clone()));
        let manifest = generation
            .bundle
            .manifest
            .with_groups(std::slice::from_ref(&compiled));
        let published = self.publish_view_manifest(&generation, manifest, started);
        // Durable in the log and not yet in a manifest, and a rotation reclaims the log: the
        // declaration reaches `SEGMENTS-<n>.json` on the mechanism a deny already uses.
        self.deny_dirty = true;
        respond.ack(Ack::ViewGroupCreated { existing: false }, &published);
    }

    /// `PUT /control/views/{name}` — create a plain view while the service runs (`ingest.md`
    /// §1.3 and §10, R9; decision 0136).
    ///
    /// **The shape is [`Self::commit_view_group_create`]'s**, and what differs is that a plain
    /// view has a *row space* — an empty one, until its first flush — so the apply goes through
    /// `Bundle::with_views`, which is what gives a view created at a running service its place in
    /// the per-view map. A view absent from that map is read as an unknown view by the viewport
    /// and as a disagreement between the mask and the bundle by the deny mask.
    fn commit_plain_view_create(
        &mut self,
        declaration: tessera_lifecycle::wal::PlainViewDeclaration,
        respond: Responder,
    ) {
        let started = std::time::Instant::now();
        let generation = self.generation.load_full();
        let compiled = match crate::view_declarations::resolve_plain(
            &declaration,
            &generation.bundle.manifest,
        ) {
            Ok(crate::view_declarations::Resolution::Existing) => {
                respond.ack(
                    Ack::PlainViewCreated { existing: true },
                    &Published::already_declared_view(&declaration.name),
                );
                return;
            }
            Ok(crate::view_declarations::Resolution::New(compiled)) => *compiled,
            Err(e) => {
                respond.fail(e);
                self.health.note_work_refused();
                return;
            }
        };
        let record = WalRecord::PlainViewCreate {
            declaration: Box::new(declaration),
        };
        if let Err(e) = self.wal.append(&record).and_then(|()| self.wal.fsync()) {
            tracing::error!(
                error = %e,
                view = %compiled.id,
                "ALARM: a plain view creation could not be made durable; the view does not exist"
            );
            respond.fail(ExecError::Wal(e));
            return;
        }
        self.live
            .with_view_declarations(|declarations| declarations.push_plain(compiled.clone()));
        let manifest = generation
            .bundle
            .manifest
            .with_plain_views(std::slice::from_ref(&compiled));
        let published = self.publish_view_manifest(&generation, manifest, started);
        self.deny_dirty = true;
        respond.ack(Ack::PlainViewCreated { existing: false }, &published);
    }

    /// Publish a generation carrying `manifest` and nothing else moved — the swap a group
    /// declaration and a plain view creation both make.
    ///
    /// **`Bundle::with_views`, which brings the per-view map into step with the manifest**: a
    /// view the manifest declares and the map does not is an unknown view to the viewport, and a
    /// created view gains the empty row space `views.md` §3.2 gives it. `segments_version` and
    /// the watermark are unmoved, on `publish_roster`'s argument: no row moved.
    fn publish_view_manifest(
        &mut self,
        generation: &Arc<Generation>,
        manifest: tessera_store::manifest::Manifest,
        started: std::time::Instant,
    ) -> Published {
        let bundle = generation.bundle.with_views(manifest);
        let denied = Arc::new(crate::compose::derive_denied(&generation.overlay, &bundle));
        let next = Generation {
            prefix: generation.prefix.clone(),
            segments_version: generation.segments_version,
            watermark: generation.watermark,
            bundle,
            dict: Arc::clone(&generation.dict),
            postings: Arc::clone(&generation.postings),
            fragments: Arc::clone(&generation.fragments),
            external_index: Arc::clone(&generation.external_index),
            delta_postings: generation.delta_postings.clone(),
            overlay_version: generation.overlay_version,
            overlay: Arc::clone(&generation.overlay),
            buffer: Arc::clone(&generation.buffer),
            vocabularies: Arc::clone(&generation.vocabularies),
            filter_columns: Arc::clone(&generation.filter_columns),
            suggest: Arc::clone(&generation.suggest),
            denied,
        };
        self.publish(next, started)
    }

    /// `PUT /control/vocabularies/{name}` — declare a vocabulary while the service runs
    /// (`ingest.md` §1.3; decision 0136).
    ///
    /// **The shape is [`Self::commit_attribute_declare`]'s**: resolve against state only this
    /// thread may write, draw the codes, append, fsync, apply, publish, ack. The apply reaches
    /// the bundle, because the served vocabulary table is the manifest's `vocabularies` and every
    /// reader takes it from there: the successor generation carries the vocabulary, and its
    /// minter holds the values the declaration named.
    ///
    /// **Usable at the ack.** A `declared` category column may name the vocabulary in the next
    /// request, and a row may carry a value it holds; a key it does not hold is the declare-then-
    /// use refusal, unchanged (per-point-attributes §5).
    ///
    /// An identical redeclaration answers the vocabulary that exists and applies the request's
    /// values as a page; a differing one is a conflict (`ingest.md` §1.1). A failed append means
    /// the vocabulary does not exist and no code was spent.
    fn commit_vocabulary_declare(
        &mut self,
        request: tessera_lifecycle::VocabularyRequest,
        respond: Responder,
    ) {
        let started = std::time::Instant::now();
        let generation = self.generation.load_full();
        let compiled = match crate::vocabularies::resolve(&request, &generation.bundle.manifest) {
            Ok(crate::vocabularies::Resolution::Existing) => {
                // A redeclaration is the same vocabulary, and its values are a page against it.
                self.commit_vocabulary_page(request.name, request.values, true, respond);
                return;
            }
            Ok(crate::vocabularies::Resolution::New(compiled)) => *compiled,
            Err(e) => {
                respond.fail(e);
                self.health.note_work_refused();
                return;
            }
        };
        // The codes, drawn into a minter this thread owns and nothing has published. A draw that
        // exhausts the width refuses with nothing appended and no binding anywhere.
        let width = tessera_spatial::tiler::ScalarType::parse(&compiled.width)
            .unwrap_or(tessera_spatial::tiler::ScalarType::U32);
        let mut minter = tessera_store::vocabulary::VocabularyMinter::new(
            compiled.name.clone(),
            compiled.kind,
            compiled.visibility,
            width,
        );
        for &code in &compiled.reserved {
            minter.seed_reserved(code);
        }
        let mut codes = Vec::with_capacity(request.values.len());
        for value in &request.values {
            match minter.mint(&value.key) {
                Ok(minted) => codes.push((value.key.clone(), minted.code())),
                Err(e) => {
                    respond.fail(ExecError::VocabularyRefused {
                        detail: e.to_string(),
                    });
                    self.health.note_work_refused();
                    return;
                }
            }
            if let Some(title) = &value.title {
                minter.set_title(&value.key, title.clone());
            }
        }
        let record = WalRecord::VocabularyDeclare {
            declaration: Box::new(crate::vocabularies::declaration_record(
                &request, &compiled, &codes,
            )),
        };
        if let Err(e) = self.wal.append(&record).and_then(|()| self.wal.fsync()) {
            tracing::error!(
                error = %e,
                vocabulary = %compiled.name,
                "ALARM: a vocabulary declaration could not be made durable; the vocabulary does \
                 not exist"
            );
            respond.fail(ExecError::Wal(e));
            return;
        }

        // The apply: the live list first, then the successor generation built from it.
        let added = codes.len() as u64;
        self.live
            .with_vocabularies(|vocabularies| vocabularies.push(compiled.clone()));
        let mut vocabularies: Vocabularies = (*generation.vocabularies).clone();
        vocabularies.insert(minter);
        let manifest = generation
            .bundle
            .manifest
            .with_vocabularies(std::slice::from_ref(&compiled));
        let bundle = generation.bundle.with_views(manifest);
        let denied = Arc::new(crate::compose::derive_denied(&generation.overlay, &bundle));
        let next = Generation {
            prefix: generation.prefix.clone(),
            // **Unmoved**, on `publish_roster`'s argument: no row moved.
            segments_version: generation.segments_version,
            watermark: generation.watermark,
            bundle,
            dict: Arc::clone(&generation.dict),
            postings: Arc::clone(&generation.postings),
            fragments: Arc::clone(&generation.fragments),
            external_index: Arc::clone(&generation.external_index),
            delta_postings: generation.delta_postings.clone(),
            overlay_version: generation.overlay_version,
            overlay: Arc::clone(&generation.overlay),
            buffer: Arc::clone(&generation.buffer),
            vocabularies: Arc::new(vocabularies),
            filter_columns: Arc::clone(&generation.filter_columns),
            // **No suggestion index.** One is built for the vocabularies a *column* names, and
            // this vocabulary is named by none until one is declared over it — which is where the
            // index is built (`commit_attribute_declare`).
            suggest: Arc::clone(&generation.suggest),
            denied,
        };
        let published = self.publish(next, started);
        // Durable in the log and not yet in a manifest, and a rotation reclaims the log: the
        // declaration reaches `SEGMENTS-<n>.json` on the mechanism a deny already uses.
        self.deny_dirty = true;
        respond.ack(
            Ack::VocabularyDeclared {
                existing: false,
                added,
                // A new vocabulary holds no value whose title could be replaced: every title it
                // carries arrived with the value that drew its code.
                titles: 0,
            },
            &published,
        );
    }

    /// `PATCH /control/vocabularies/{name}/values` — a page of values for a vocabulary that
    /// exists (`ingest.md` §1.3).
    fn commit_vocabulary_values(
        &mut self,
        vocabulary: String,
        values: Vec<tessera_lifecycle::DeclaredValue>,
        respond: Responder,
    ) {
        self.commit_vocabulary_page(vocabulary, values, false, respond);
    }

    /// One page of values, whether it arrived on the values route or as the inline values of a
    /// redeclaration.
    ///
    /// **Every value of the page is checked before any code is drawn**, so a refused page binds
    /// nothing and a caller's corrected retry means what they think it means. The page is one
    /// append and one fsync — a `VocabularyDeclare` record carrying the page's values with the
    /// codes drawn for them, because a value's title is part of what the page acknowledges and a
    /// `VocabularyMint` record carries none.
    ///
    /// **A title supplied for a held key replaces the held title** and is counted into the
    /// acknowledgement (decision 0136's amendment). The key-to-code binding does not move, so a
    /// row already carrying the code means what it meant; what changes is the name a client draws.
    fn commit_vocabulary_page(
        &mut self,
        vocabulary: String,
        values: Vec<tessera_lifecycle::DeclaredValue>,
        redeclaration: bool,
        respond: Responder,
    ) {
        let started = std::time::Instant::now();
        let generation = self.generation.load_full();
        let Some(held) = generation.vocabularies.get(&vocabulary) else {
            // The same 404 an unknown view is, and for the same reason: a vocabulary nobody
            // declared and one this deployment does not carry are one answer.
            respond.fail(ExecError::ViewUnknown {
                detail: format!(
                    "unknown vocabulary '{vocabulary}'. A vocabulary is declared at a build or by \
                     `PUT /control/vocabularies/{{name}}` (ingest §1.3); a page of values does \
                     not create one, because the value set's width and visibility are the \
                     declaration's to state"
                ),
            });
            self.health.note_work_refused();
            return;
        };
        let titles = match crate::vocabularies::check_page(held, &vocabulary, &values) {
            Ok(titles) => titles,
            Err(e) => {
                respond.fail(e);
                self.health.note_work_refused();
                return;
            }
        };
        let mut minter = held.clone();
        let mut codes = Vec::with_capacity(values.len());
        let mut added = 0u64;
        let mut existing = 0u64;
        for value in &values {
            match minter.mint(&value.key) {
                Ok(tessera_store::vocabulary::Minted::Fresh(code)) => {
                    added += 1;
                    codes.push((value.key.clone(), code));
                }
                Ok(tessera_store::vocabulary::Minted::Existing(code)) => {
                    existing += 1;
                    codes.push((value.key.clone(), code));
                }
                Err(e) => {
                    respond.fail(ExecError::VocabularyRefused {
                        detail: e.to_string(),
                    });
                    self.health.note_work_refused();
                    return;
                }
            }
            if let Some(title) = &value.title {
                minter.set_title(&value.key, title.clone());
            }
        }
        // **Nothing to append where the page bound nothing and changed no title.** A repeat of a
        // page already applied is the no-op `ingest.md` §1.1 asks for, and an fsync for it would
        // be a durable record of a decision nothing made. `titles` counts the held keys whose
        // title this page changes, so a page restating the titles a deployment holds appends
        // nothing.
        if added == 0 && titles == 0 {
            respond.ack(
                if redeclaration {
                    Ack::VocabularyDeclared {
                        existing: true,
                        added: 0,
                        titles: 0,
                    }
                } else {
                    Ack::VocabularyValuesMinted {
                        added,
                        existing,
                        titles: 0,
                    }
                },
                &Published::nothing_bound(&values),
            );
            return;
        }
        let declaration = tessera_lifecycle::wal::VocabularyDeclaration {
            name: vocabulary.clone(),
            title: None,
            kind: minter.kind(),
            visibility: minter.visibility(),
            width: minter.width().arrow_type_name().to_string(),
            values: codes
                .iter()
                .map(
                    |(key, code)| tessera_lifecycle::wal::DeclaredVocabularyValue {
                        key: key.clone(),
                        code: Some(*code),
                        title: minter.title_of(key).map(str::to_string),
                    },
                )
                .collect(),
            reserved: Vec::new(),
        };
        let record = WalRecord::VocabularyDeclare {
            declaration: Box::new(declaration),
        };
        if let Err(e) = self.wal.append(&record).and_then(|()| self.wal.fsync()) {
            tracing::error!(
                error = %e,
                vocabulary = %vocabulary,
                "ALARM: a page of vocabulary values could not be made durable; none of them is \
                 bound"
            );
            respond.fail(ExecError::Wal(e));
            return;
        }
        let mut vocabularies: Vocabularies = (*generation.vocabularies).clone();
        vocabularies.insert(minter);
        let next = Generation {
            prefix: generation.prefix.clone(),
            segments_version: generation.segments_version,
            watermark: generation.watermark,
            bundle: Arc::clone(&generation.bundle),
            dict: Arc::clone(&generation.dict),
            postings: Arc::clone(&generation.postings),
            fragments: Arc::clone(&generation.fragments),
            external_index: Arc::clone(&generation.external_index),
            delta_postings: generation.delta_postings.clone(),
            overlay_version: generation.overlay_version,
            overlay: Arc::clone(&generation.overlay),
            buffer: Arc::clone(&generation.buffer),
            vocabularies: Arc::new(vocabularies),
            filter_columns: Arc::clone(&generation.filter_columns),
            suggest: Arc::clone(&generation.suggest),
            denied: Arc::clone(&generation.denied),
        };
        let published = self.publish(next, started);
        // A binding of a *built* vocabulary reaches the manifest as a `vocabulary_extensions`
        // entry and one of a runtime-declared vocabulary as a value of its own runtime entry;
        // both are written at the next side-manifest publication, which this marks due.
        self.deny_dirty = true;
        respond.ack(
            if redeclaration {
                Ack::VocabularyDeclared {
                    existing: true,
                    added,
                    titles,
                }
            } else {
                Ack::VocabularyValuesMinted {
                    added,
                    existing,
                    titles,
                }
            },
            &published,
        );
    }

    /// `DELETE /control/views/{group}/{key}` — drop a view, freeing its key and killing its
    /// incarnation (`views.md` §3.4, decision 0115).
    ///
    /// **Dropping a view deletes no entity.** An entity whose only view was dropped still exists,
    /// with its label, its attributes and its artifact memberships, in no view — and a later batch
    /// into a new view picks it up by `external_id` under the join rule. `delete_dangling` is the
    /// caller who *did* mean "and the items that were only here", and it is **sugar and nothing
    /// else**: the entities are submitted as ordinary deletions, which enter the overlay and
    /// retire at the fold that executes them (Rule F, write-path §5.4). It is not a second
    /// retirement route, and the two removal rules are untouched by anything here.
    ///
    /// **The probe and the submission are one step on this thread**, which is what the
    /// serialisation is for: a batch acked between them could re-add an entity the probe had
    /// already found dangling, and the deletion would then destroy a row the caller was told had
    /// landed.
    fn commit_view_drop(
        &mut self,
        group: String,
        key: String,
        delete_dangling: bool,
        respond: Responder,
    ) {
        let started = std::time::Instant::now();
        let generation = self.generation.load_full();
        // The owner's key, whatever group the request named: a key belongs to the group that owns
        // the views, and dropping the key takes the view out of every group sharing them
        // (`views.md` §3.3).
        let owner = generation.bundle.manifest.owner_of_group(&group);
        // **Every id the key resolves to**, which is what a drop takes away — the owner's view and
        // every sharing group's. Built from the *owner* rather than from the group the request
        // named, and used by all three things below that act on "the views of this key": the log
        // line, the `delete_dangling` probe and the buffer prune. A prune over the requested
        // spelling alone leaves the other's buffered rows to be flushed into whatever takes the
        // key next ([decision 0115](../../../docs/decisions/0115-a-dropped-view-key-is-reusable.md)).
        let ids = generation.bundle.manifest.view_ids_for_key(&owner, &key);
        let id = format!("{owner}{}{key}", tessera_store::GROUP_SEPARATOR);
        let prepared = self
            .live
            .with_roster(|roster| roster.prepare_drop(&owner, &key));
        let record = match prepared {
            Ok(record) => record,
            Err(e) => {
                respond.fail(roster_error(e));
                self.health.note_work_refused();
                return;
            }
        };
        // **Computed before the drop applies**, because the probe reads the row space the drop is
        // about to take away — and on this thread, with no yield between it and the submission.
        let dangling = if delete_dangling {
            dangling_entities(&generation, &ids)
        } else {
            Vec::new()
        };
        if let Err(e) = self.wal.append(&record).and_then(|()| self.wal.fsync()) {
            tracing::error!(
                error = %e,
                view = %id,
                "ALARM: a view drop could not be made durable; the view still exists"
            );
            respond.fail(ExecError::Wal(e));
            return;
        }
        self.live.with_roster(|roster| roster.apply(&record));
        let published = self.publish_roster(&generation, started, &ids);
        self.deny_dirty = true;
        // **Ordinary deletions, through the ordinary lane.** They are appended, fsynced and
        // applied by the same path a `/control/changes` delete takes, so they retire at the fold
        // under Rule F and nowhere else. A failure here is reported the way that lane reports one
        // — in force, and possibly not durable — and does not un-drop the view, which is already
        // acknowledged as far as the log is concerned.
        let deleted = dangling.len() as u64;
        if !dangling.is_empty() {
            let mut entries: Vec<DenyEntry> = dangling
                .into_iter()
                .map(|entity| DenyEntry {
                    record: WalRecord::ChangeByEntity {
                        entity_id: entity,
                        op: tessera_lifecycle::ChangeOp::Delete,
                    },
                    entity,
                    op: tessera_lifecycle::ChangeOp::Delete,
                    respond: None,
                })
                .collect();
            self.cascade_dependents(&mut entries);
            self.commit_denies(entries);
        }
        respond.ack(Ack::ViewDropped { deleted }, &published);
    }

    /// Publish the generation a create or a drop makes: the bundle as the live roster describes
    /// it, the deny mask re-derived over the views it now has, and every buffered row of a view
    /// that has gone.
    ///
    /// **The buffered rows of a dropped view are discarded, and that is not a deletion.** They
    /// name a coordinate system that no longer exists, so nothing will ever give them geometry —
    /// and a row left in the buffer for a view no flush will plan pins `oldest_wal_pos`, and with
    /// it every WAL member after it, for the life of the process. Their entities are untouched:
    /// an entity left in no view is exactly what `views.md` §3.4 says a drop produces.
    fn publish_roster(
        &self,
        generation: &Arc<Generation>,
        started: std::time::Instant,
        dropped: &[String],
    ) -> Published {
        let (created, tombstones) = self.live.roster_for_publication();
        let manifest = generation
            .bundle
            .manifest
            .with_roster(&created, &tombstones);
        let bundle = generation.bundle.with_views(manifest);
        let buffer = if dropped.is_empty() {
            Arc::clone(&generation.buffer)
        } else {
            let mut buffer = (*generation.buffer).clone();
            // Rows, not entities, and by (entity, view): an entity whose row in the dropped view
            // was a join keeps the row it holds elsewhere, and `rows()` is what sees the join at
            // all.
            let orphaned: Vec<(EntityId, String)> = generation
                .buffer
                .rows()
                .filter(|(_, item)| dropped.contains(&item.view))
                .map(|(entity, item)| (*entity, item.view.clone()))
                .collect();
            for (entity, view) in orphaned {
                buffer.remove_in_view(entity, &view);
            }
            self.health
                .buffered_items
                .store(buffer.len(), Ordering::SeqCst);
            Arc::new(buffer)
        };
        // **Re-derived, never carried**: the mask holds one entry per view of the bundle and its
        // own contract is that a missing one means the mask and the bundle disagree — which is
        // exactly the state carrying it forward across a create would produce.
        let denied = Arc::new(crate::compose::derive_denied(&generation.overlay, &bundle));
        let next = Generation {
            prefix: generation.prefix.clone(),
            // **Unmoved**: no row moved, so every row-projection cache keyed on it stays valid.
            // The coalesce publication is the precedent — a new bundle at the same version.
            segments_version: generation.segments_version,
            watermark: generation.watermark,
            bundle,
            dict: Arc::clone(&generation.dict),
            postings: Arc::clone(&generation.postings),
            fragments: Arc::clone(&generation.fragments),
            external_index: Arc::clone(&generation.external_index),
            delta_postings: generation.delta_postings.clone(),
            overlay_version: generation.overlay_version,
            overlay: Arc::clone(&generation.overlay),
            buffer,
            vocabularies: Arc::clone(&generation.vocabularies),
            filter_columns: Arc::clone(&generation.filter_columns),
            suggest: Arc::clone(&generation.suggest),
            denied,
        };
        self.publish(next, started)
    }

    /// Clone the buffer **once**, insert every entry in the window, publish **once**.
    ///
    /// The amortisation this buys is the one that grows: the clone is O(total buffered items) and
    /// the buffer grows until the next tick drains it, so a window of k entries pays it once
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
    ///    10⁹ — **O(N²/B)** — bounded only by the flush draining the buffer each tick and by
    ///    `ingest_buffer_max_items` when it cannot. Measured pre-flush:
    ///    `apply_nanos_max` 210–437 ms at ~1.34 M buffered items
    ///    (`docs/evidence/memos/2026-08-01-deny-ack-baseline.md`, result 3). It is the *small*
    ///    batches the window collects.
    /// 2. **Flush drains the buffer each tick**, so the clone's operand is bounded by one tick's
    ///    arrivals in the steady state and by `ingest_buffer_max_items` when flush is failing. A
    ///    chunked or persistent buffer remains the remedy if the per-window clone itself ever
    ///    measures as the constraint — do not build a second mechanism around it before that.
    ///
    /// No counter is added for this: `apply_nanos_total` / `apply_nanos_max` already
    /// measure it and are already on `/control/status`.
    ///
    /// `terms` is **taken** out of each entry rather than borrowed: each row's resolved set is
    /// *moved* into the buffer, where borrowing would force one `Vec<TermId>` clone per row on the
    /// one thread every write is serialised through — measured at +14% on the
    /// 10 000-row arm. `&mut` is what buys it; an entry's `terms` is empty after this and nothing
    /// downstream reads it — the ack needs `entity_ids`, not terms.
    ///
    /// `vocabularies` is `close_window`'s locally mutated copy — the live bindings plus this
    /// window's mints — and is published verbatim rather than `Arc::clone(&generation.vocabularies)`
    /// as every other unmoved field is: the whole reason minting happens on the executor is that the
    /// mutation must reach the *next* generation, and cloning the *old* `Arc` here would silently
    /// discard every code this window just drew.
    fn apply_window(
        &self,
        closed: &mut [ClosedEntry<Responder>],
        positions: &[u64],
        vocabularies: Vocabularies,
        mints: &[(String, String, u32)],
    ) -> Published {
        let started = std::time::Instant::now();
        let mut mark = StageMark::now();
        let generation = self.generation.load_full();
        // The suggestion index's side map, grown by exactly the keys this window minted. Nothing
        // is rebuilt: the base index and every other vocabulary's are carried behind their `Arc`s,
        // and the fold's handles are borrows of data baked into the binary rather than a
        // deserialisation.
        let suggest = generation.suggest.with_mints(
            &tessera_analyse::SuggestionFold::new(),
            &vocabularies,
            mints,
        );
        let mut buffer = (*generation.buffer).clone();
        mark = self.health.lap(WriteStage::ApplyBufferClone, mark);

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
                let mut m = StageMark::now();
                if let Some(external_id) = &row.external_id {
                    established.insert(external_id.clone(), row.entity_id);
                    m = self.health.lap(WriteStage::RowEstablished, m);
                    established_inverse.insert(row.entity_id, external_id.clone());
                    m = self.health.lap(WriteStage::RowEstablishedInv, m);
                }
                buffer.insert_row_with_terms(row, row_terms);
                let m = self.health.lap(WriteStage::RowBufferInsert, m);
                buffer.set_wal_pos(row.entity_id, &row.view, *wal_pos);
                self.health.lap(WriteStage::RowWalPos, m);
            }
        }
        drop(established);
        drop(established_inverse);
        mark = self.health.lap(WriteStage::ApplyRows, mark);

        // Published here, and at every other place buffer occupancy changes — the flush's
        // publication and the deny lane's — so `/control/ingest`'s occupancy bound reads a figure
        // the executor maintains rather than one a handler derives from a generation it would
        // have to load.
        self.health
            .buffered_items
            .store(buffer.len(), Ordering::SeqCst);

        let next = Generation {
            filter_columns: Arc::clone(&generation.filter_columns),
            overlay_version: generation.overlay_version + 1,
            buffer: Arc::new(buffer),
            prefix: generation.prefix.clone(),
            vocabularies: Arc::new(vocabularies),
            segments_version: generation.segments_version,
            watermark: generation.watermark,
            bundle: Arc::clone(&generation.bundle),
            dict: Arc::clone(&generation.dict),
            postings: Arc::clone(&generation.postings),
            fragments: Arc::clone(&generation.fragments),
            external_index: Arc::clone(&generation.external_index),
            delta_postings: generation.delta_postings.clone(),
            overlay: Arc::clone(&generation.overlay),
            // **The one publication that changes the suggestion index**, and it changes it by the
            // same mints that changed the bindings above: a novel key gets its code here, and a
            // viewer typing its prefix on the next keystroke must be offered it rather than
            // waiting for the next rebuild (`value-suggestion.md` §6.1). Every other publication
            // carries the index forward.
            suggest,
            // Neither the deny sets nor the row space moved, so the mask is unchanged. An ingest
            // adds a *buffered* row, which has no row id to be denied at.
            denied: Arc::clone(&generation.denied),
        };
        let published = self.publish(next, started);
        self.health.lap(WriteStage::ApplySwap, mark);
        published
    }

    /// Clone the overlay **once**, apply every change in the window, publish **once**.
    ///
    /// The amortisation this buys is the one that grows. [`Overlay`] never shrinks —
    /// entries survive `suppress → unsuppress` and shrink only at a fold — so the clone
    /// is O(overlay depth) and the depth rises by one per new item denied. Applying an N-item
    /// revocation one command at a time therefore copies Θ(N²) entries; a window of k pays the clone
    /// once for the k. The clone is also every deny's ack-latency floor
    /// ([`ExecutorHealth::apply_nanos_total`], already on `/control/status`, so no counter is added
    /// for this).
    ///
    /// Changes are applied in view order, which is the window's entries order, which is the deny
    /// lane's FIFO arrival order — so a `suppress` and a later `unsuppress` of the same item resolve
    /// as they would have as two separate commands. The view is iterated once, forwards.
    ///
    /// Pins are never invalidated by this (I11): a pin fixes `(prefix, segments_version)`, and this
    /// bumps `overlay_version`. That is lifecycle §2.3's rule that a suppression applies to a
    /// pinned request the moment it is accepted, without expiring the pin.
    fn apply_changes(&self, changes: Vec<(EntityId, ChangeOp)>) -> Published {
        let started = std::time::Instant::now();
        let generation = self.generation.load_full();
        let mut overlay: Overlay = (*generation.overlay).clone();
        // What the window did, for the mask below: which entities it denied, and whether any
        // removal happened at all.
        let mut newly_denied: Vec<EntityId> = Vec::new();
        let mut deleted: Vec<EntityId> = Vec::new();
        let mut unsuppressed = false;
        for (entity, op) in changes {
            match op {
                ChangeOp::Delete => {
                    newly_denied.push(entity);
                    deleted.push(entity);
                }
                ChangeOp::Suppress => newly_denied.push(entity),
                ChangeOp::Unsuppress => unsuppressed = true,
            }
            overlay.apply(entity, op);
        }

        // **It alarms on the union; the schedule acts on the deletions.** `overlay_soft_limit`
        // gauges `deleted ∪ suppressed`, which is what an operator should see, while the fold
        // trigger it seeds keys on the retirable part — a suppression never retires, and a fold
        // dispatched on the union would rewrite the corpus to retire nothing (compaction §9).
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
                "ALARM: the overlay has crossed its configured soft limit. A compaction fold is \
                 the lever — it retires the executed deletions (Rule F) — but nothing schedules \
                 one: the automatic trigger and POST /control/compact are unbuilt (compaction \
                 §9), so the depth comes down only when something calls for a fold. This line is \
                 edge-triggered, so it will NOT repeat while the overlay stays over. \
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
                for (view, view_data) in &partition.views {
                    let Some(rows) = denied.get_mut(view) else {
                        continue;
                    };
                    for entity in &newly_denied {
                        if let Some(row) = view_data.row_space.row_of(*entity) {
                            rows.add(row.raw());
                        }
                    }
                }
            }
            Arc::new(denied)
        };

        // **A deleted row leaves the buffer here** — the runtime half of `replay`'s end-of-pass
        // rule, and the reason a `delete` issued before the item's first flush does not pin the
        // WAL for ever (`IngestBuffer::oldest_wal_pos` is the rotation's reclaim bound, and
        // `plan_flush` never consumes a deleted row, so nothing else would ever remove it).
        // Composition-neutral: `compose::verdict` answers from `is_deleted` before it consults the
        // buffer. The argument in full is at `tessera_lifecycle::overlay::drop_deleted`.
        //
        // **The clone is paid only when a buffered row is actually dropped.** It is O(buffered) —
        // the term the deny-ack memo measured at 165 ms p50 with 1 M buffered — and this is the
        // deny lane, so paying it per window would put a flush-sized stall in front of every
        // revocation. Deleting an entity that already has geometry, which is the ordinary case,
        // costs one hash lookup per entry and no clone at all.
        let buffered_deletions: Vec<EntityId> = deleted
            .into_iter()
            .filter(|entity| generation.buffer.contains(*entity))
            .collect();
        let buffer = if buffered_deletions.is_empty() {
            Arc::clone(&generation.buffer)
        } else {
            let mut buffer = (*generation.buffer).clone();
            for entity in buffered_deletions {
                buffer.remove(entity);
            }
            self.health
                .buffered_items
                .store(buffer.len(), Ordering::SeqCst);
            Arc::new(buffer)
        };

        let next = Generation {
            filter_columns: Arc::clone(&generation.filter_columns),
            // A deny changes who may be told a value name and never which value names exist, so
            // the index is carried and the *predicate* answers differently — which is
            // membership-derivation self-retiring, and is the whole reason it is derived per
            // request rather than maintained (per-point-attributes §3.3).
            suggest: Arc::clone(&generation.suggest),
            overlay_version: generation.overlay_version + 1,
            overlay: Arc::new(overlay),
            prefix: generation.prefix.clone(),
            vocabularies: Arc::clone(&generation.vocabularies),
            segments_version: generation.segments_version,
            watermark: generation.watermark,
            bundle: Arc::clone(&generation.bundle),
            dict: Arc::clone(&generation.dict),
            postings: Arc::clone(&generation.postings),
            fragments: Arc::clone(&generation.fragments),
            external_index: Arc::clone(&generation.external_index),
            delta_postings: generation.delta_postings.clone(),
            buffer,
            denied,
        };
        self.publish(next, started)
    }

    /// Write a side-manifest carrying the live deny state, if any window has moved it.
    ///
    /// **A disc event only.** No geometry moves, nothing is superseded, no cache is pruned and the
    /// generation is untouched — this exists so that a restore from bundle and object store, with
    /// no WAL, recovers the deny state as of the last publication (deny lifecycle memo §4). The
    /// live node never reads it back: its own WAL is authoritative, and `reconstruct` replays over
    /// this as a seed.
    ///
    /// **Off the ack path.** Every 200 in the burst was already sent, at its own window's swap, so
    /// nothing here is between a caller and its acknowledgement. Architecture §3 (r23) budgets the
    /// write path at seconds to minutes with the one condition that a deny's ack stay coupled to
    /// its *application* — which is upstream of this, at the window's fsync and swap.
    ///
    /// **Gated on durability.** A poisoned WAL or a diverged overlay publishes nothing: the
    /// overlay then holds dispositions no durable record backs, and writing them would make a
    /// 500'd, never-acked deny permanent on every restore. The dirty flag survives the refusal, so
    /// a repaired node publishes on its own within a tick rather than waiting for its next deny.
    ///
    /// **Failure alarms and retains.** Nothing is un-acked and nothing is unwound — the state is
    /// WAL-durable either way. Only the disaster-path bound degrades while the alarm stands, and
    /// any later write carries complete state, so a single success repairs it.
    fn publish_overlay_state(&mut self) {
        if !self.deny_dirty {
            return;
        }
        if self.wal.is_poisoned() || !self.may_publish() {
            tracing::warn!(
                "ALARM: deny state is unpublished and this node is poisoned or diverged, so it \
                 will not write a side-manifest. The dispositions are in force and WAL-durable; \
                 what is degraded is the restore path, until the node recovers or restarts"
            );
            return;
        }

        let live = self.generation.load_full();
        // What this publication wrote, per partition, so the resident memberships can move onto it
        // once every manifest naming one is durable (`LiveState::rehouse_memberships`).
        let mut written: Vec<(
            std::path::PathBuf,
            Vec<tessera_store::manifest::MembershipExtent>,
        )> = Vec::new();
        for (partition, partition_data) in &live.bundle.partitions {
            let mut manifest = partition_data.manifest.clone();
            write_deny_state(&mut manifest, &live.overlay);
            // **The registry travels with this publication too, and not only with a flush.** Until
            // artifacts existed, a registration could wait for the next flush to reach a manifest —
            // the WAL held it meanwhile and `publish_flush` says so. An extent breaks that: a
            // manifest naming memberships for a layer it does not declare is internally
            // inconsistent, and at open the layer's reserved runs are what turn an ordinal into an
            // entity, so the extents would be skipped whole and every artifact would come back
            // absent. The two are written together or the manifest is wrong.
            //
            // `min`, not `max`, for the mark — the row-less region grows downward.
            let (layers, layer_tombstones, low_water) = self.live.registry_for_publication();
            manifest.entity_id_low_water = manifest.entity_id_low_water.min(low_water);
            manifest.layers = layers;
            manifest.layer_tombstones = layer_tombstones;
            // **The roster's durable home, restated from the live roster and never from the
            // clone** (`views.md` §3.2): the manifest this was cloned from may be several
            // publications behind, and a create that landed since would be dropped by carrying it
            // forward — which a rotation then makes permanent.
            let (created_views, dead_view_incarnations) = self.live.roster_for_publication();
            manifest.views = created_views;
            manifest.dead_view_incarnations = dead_view_incarnations;
            // The runtime attribute columns beside the roster, on its rule (`ingest.md` §6.3).
            let (attributes, scoped_attributes) = self.live.attributes_for_publication();
            manifest.attributes = attributes;
            manifest.scoped_attributes = scoped_attributes;
            // And the runtime vocabularies, each with its values as the live minters hold them
            // (`ingest.md` §1.3). Restated from the live state rather than carried forward, on
            // the roster's rule: a declaration or a page that landed since the manifest was
            // cloned would otherwise be dropped, and a rotation makes that permanent.
            manifest.vocabularies = self.live.vocabularies_for_publication(&live.vocabularies);
            // And the view groups and plain views declared at a running service, on the roster's
            // rule (`ingest.md` §1.3). A group's roster is `manifest.views` above, restated from
            // the live roster; what these carry is the group's own half and the plain views.
            let (groups, plain_views) = self.live.view_declarations_for_publication();
            manifest.groups = groups;
            manifest.plain_views = plain_views;
            // **Membership extents are written before the manifest that names them**, which is the
            // whole of their durability contract: a manifest naming a missing extent refuses at
            // open, so the file has to be durable first. A failure here abandons the publication
            // rather than committing a manifest that omits them — an omission would read as *no
            // artifact was ever published*, and rotation would then be free to reclaim the log
            // records holding the only other copy.
            // Allocated first so the extents can be named after the publication that carries them:
            // one sequence, not two, and a file whose name says which manifest introduced it.
            let n = self.allocate_manifest_n();
            let prefix_dir = self.prefix_dir(&live);
            let published = match self.write_membership_extents(&prefix_dir, partition, n) {
                Ok(published) => published,
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        partition = %partition,
                        "ALARM: could not write the artifact membership extents; the memberships \
                         stay WAL-durable and the log stays pinned, and the write is retried at the \
                         next tick"
                    );
                    return;
                }
            };
            self.membership_extents.extend(published.clone());
            manifest.membership_extents = self.membership_extents.clone();
            if !published.is_empty() {
                written.push((prefix_dir.clone(), published));
            }
            // **Supplied content goes into the record blob**, the store points already use
            // ([decision 0077](../../../docs/decisions/0077-supplied-content-lives-in-the-record-blob.md)),
            // in extents of its own but on the same list and behind the same reader. Artifact and
            // point entities are disjoint by construction — two regions, growing towards each other
            // — so the rows never collide and each side reads its tags against its own declaration.
            match self.write_content_extent(&prefix_dir, partition, live.bundle.partitions.len(), n)
            {
                // **Assigned from the held list, never pushed onto the clone.** The manifest this
                // publication started from is the *stale* generation's, so extending it drops
                // every earlier publication's entry — and an artifact whose content extent is
                // un-named comes back with its description unreadable and is withheld from every
                // viewer, with the log already released. The membership list above takes this
                // posture for the same reason; a `push` here reintroduced the bug it fixes.
                Ok(Some(extent)) => {
                    self.artifact_record_extents.push(extent);
                    manifest.artifact_record_extents = self.artifact_record_extents.clone();
                }
                Ok(None) => {
                    manifest.artifact_record_extents = self.artifact_record_extents.clone();
                }
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        partition = %partition,
                        "ALARM: could not write the artifact content extent; the content stays \
                         WAL-durable and the log stays pinned, and the write is retried at the next \
                         tick"
                    );
                    return;
                }
            }
            write_vocabulary_extensions(
                &mut manifest,
                &live.vocabularies,
                &live.bundle.manifest.vocabularies,
            );
            // The publication seam, per partition: the dispositions are WAL-durable either way,
            // so a kill parked here loses only the restore path's freshness — which is exactly
            // what a crash test at this seam asserts (correctness-suite §12.3).
            self.pause_point(PauseSiteArg::BeforeManifestPublish);
            if let Err(e) = self.commit_side_manifest(
                &partition_data.manifest,
                &self.prefix_dir(&live),
                partition,
                n,
                &mut manifest,
                &self.containment_extents,
                &self.tile_index_extents,
                &self.row_column_extents,
                &self.shape_rows_extents,
                &self.shape_held_extents,
                &[],
            ) {
                tracing::error!(
                    error = %e,
                    partition = %partition,
                    "ALARM: could not publish the overlay's deny state; it stays in force and                      WAL-durable, and the write is retried at the next drain close or tick. A                      restore taken meanwhile recovers the previously published state"
                );
                return;
            }
        }

        // **Only now**, with every partition's manifest durable, is the log free of these
        // memberships. Marking earlier would let rotation reclaim the records behind an extent a
        // crash could still lose. The content fills the same publication packed are released on
        // the same argument.
        self.live.mark_memberships_published();
        self.live.mark_content_published();
        // **And the memberships move onto the extents this publication wrote**, on the fold's
        // rule (`LiveState::rehouse_memberships`): a membership left on the heap is one the node
        // carries in anonymous memory for as long as it runs, for bytes it has just written and
        // holds open. After the manifests, because a publication that failed above leaves files no
        // manifest names, and this is the point where every one of them is named.
        for (prefix_dir, extents) in &written {
            let (rehoused, kept) = self.live.rehouse_memberships(prefix_dir, extents);
            if kept > 0 {
                // Unreachable for the fold's reason exactly: the extent was packed from these
                // records, and an ordinal with no record is written as an empty blob the
                // rehousing skips.
                tracing::error!(
                    rehoused,
                    kept,
                    "ALARM: artifact memberships this publication wrote do not match the records \
                     they were written from; the extent the manifest now names and the level being \
                     served disagree for those ordinals"
                );
            }
        }

        self.deny_dirty = false;
        self.windows_since_publication = 0;
        self.health
            .overlay_publications
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Write every not-yet-published artifact's supplied content as one record-blob extent.
    ///
    /// **The same store, the same format and the same reader as a point's blob-resident fields** —
    /// which is the point of putting it here rather than in a structure of its own: one set of
    /// format invariants, one fail-closed reader, and the filter and search surfaces reach artifact
    /// properties by the route they already reach a document's when those land.
    ///
    /// What does **not** come with the store is the access rule. A document's field is visible to
    /// whoever may see the document; an artifact's content is visible to whoever contains its
    /// generating set entirely. The two never converge, and the reason sharing a store is safe
    /// anyway is that they never share an entity: which rule governs a row is a range check on its
    /// id (`annotations.md` §7's withdrawal is exactly this distinction — the *storage* half of the
    /// reuse claim survived review, the *visibility* half was the fail-open).
    fn write_content_extent(
        &self,
        prefix_dir: &std::path::Path,
        partition: &str,
        partitions: usize,
        n: u64,
    ) -> tessera_store::Result<Option<tessera_store::manifest::RecordExtent>> {
        let rows = self.live.unpublished_content();
        if rows.is_empty() {
            return Ok(None);
        }
        // **An artifact belongs to no partition**, and this loop runs once per partition — so a
        // second partition would receive an extent holding the *same* artifact entities, and two
        // layers of one record stack whose has-row bitmaps overlap is a state the stack refuses
        // outright, breaking every later coalesce of a window containing both.
        //
        // Refused rather than guessed. Which partition should own an artifact's content, or whether
        // the rows should be split across them by some rule, is a layout question a multi-partition
        // bundle has to answer and nothing here can: writing to the first partition alone would
        // leave the content unreadable from a view carried by another, and writing to all of them
        // is the overlap above. No such bundle exists today (nothing splits one), which is why this
        // is a refusal with an alarm rather than a design.
        if partitions > 1 {
            return Err(tessera_store::StoreError::MalformedBundle {
                detail: format!(
                    "this bundle has {partitions} partitions and an artifact belongs to none of \
                     them, so where its supplied content should be written is undecided; the \
                     publication is refused rather than writing the same rows into every partition"
                ),
            });
        }

        let extents_rel = format!("partitions/{partition}/attrs/record/extents");
        let dir = prefix_dir.join(&extents_rel);
        std::fs::create_dir_all(&dir).map_err(|source| tessera_store::StoreError::Io {
            path: dir.clone(),
            source,
        })?;
        let extent = tessera_store::manifest::RecordExtent {
            blocks: format!("{extents_rel}/artifacts-{n:06}.blocks.bin"),
            hasrow: format!("{extents_rel}/artifacts-{n:06}.hasrow.roaring"),
            directory: format!("{extents_rel}/artifacts-{n:06}.directory.arrow"),
        };
        let io = |path: &std::path::Path| {
            let path = path.to_path_buf();
            move |source| tessera_store::StoreError::Io {
                path: path.clone(),
                source,
            }
        };
        let blocks = prefix_dir.join(&extent.blocks);
        let hasrow = prefix_dir.join(&extent.hasrow);
        let directory = prefix_dir.join(&extent.directory);
        let mut writer = tessera_filter_write::RecordBlobWriter::create(
            &blocks,
            &hasrow,
            &directory,
            tessera_filter::RECORD_BLOCK_TARGET,
        )
        .map_err(io(&blocks))?;
        // **Ascending by entity**, which the blob's block directory requires. Artifact ids descend
        // as they are allocated — the row-less region grows downward — so publication order is
        // exactly the wrong order here, and sorting is not an optimisation.
        let mut rows = rows;
        rows.sort_by_key(|(entity, _)| entity.raw());
        for (entity, fields) in rows {
            let entity = u32::try_from(entity.raw()).map_err(|_| {
                tessera_store::StoreError::MalformedBundle {
                    detail: format!(
                        "artifact entity {} does not fit the u32 entity space (I9's ceiling)",
                        entity.raw()
                    ),
                }
            })?;
            let fields: Vec<tessera_filter::RecordFieldRef<'_>> = fields
                .iter()
                .map(|(tag, value)| tessera_filter::RecordFieldRef {
                    tag: *tag,
                    value: tessera_filter::RecordValueRef::Utf8(value),
                })
                .collect();
            writer.push_row(entity, &fields).map_err(io(&blocks))?;
        }
        writer.finish().map_err(io(&blocks))?;
        // **`finish` syncs the blocks and not the two files that address them.** The directory goes
        // out through an Arrow writer and the has-row bitmap through a plain write, so a crash
        // after the manifest is durable can leave either torn — and a torn addressing file refuses
        // the **whole** record stack at open, taking every point's blob-resident field with it.
        // The membership path syncs per file for the same reason; this one has to do it here
        // because the blob writer is shared with the flush, which syncs its extent another way.
        for path in [&hasrow, &directory] {
            let file = std::fs::File::open(path).map_err(io(path))?;
            file.sync_all().map_err(io(path))?;
        }
        tessera_store::fsync_dir(&dir)?;
        Ok(Some(extent))
    }

    /// Write the fold's degradation report, and keep the last one for the operator route.
    ///
    /// **Outside the prefix, and that is the point.** A fold reclaims the prefix it superseded, so a
    /// report written into the *new* prefix would be deleted by the next fold — two nights later,
    /// the notice a caller had not read yet is gone. `reports/` sits in the bundle root beside the
    /// prefixes, which nothing reclaims, and the startup sweep only knows about `v#####` directories.
    ///
    /// **A report with nothing in it is still written.** An operator polling the directory must be
    /// able to tell "this fold degraded nothing" from "this fold never reported", and an absent file
    /// says the second.
    fn write_fold_report(
        &self,
        prefix: &str,
        degraded: &[tessera_lifecycle::membership::Degradation],
    ) -> std::io::Result<()> {
        let dir = self.bundle_root.join("reports");
        std::fs::create_dir_all(&dir)?;
        let rows: Vec<serde_json::Value> = degraded
            .iter()
            .map(|d| {
                serde_json::json!({
                    "layer": d.layer,
                    "level": d.level,
                    "ordinal": d.ordinal,
                    "key": d.key,
                    "members_lost": d.members_lost,
                    "declared_members": d.declared_members,
                    "contents_lost": d
                        .contents_lost
                        .iter()
                        .map(|(index, lost)| serde_json::json!({"rank": index, "lost": lost}))
                        .collect::<Vec<_>>(),
                })
            })
            .collect();
        let body = serde_json::json!({
            "prefix": prefix,
            "degraded": rows,
        });
        let bytes = serde_json::to_vec_pretty(&body)?;
        let path = dir.join(format!("fold-{prefix}.json"));
        tessera_store::write_and_fsync(&path, &bytes)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        tessera_store::fsync_dir(&dir).map_err(|e| std::io::Error::other(e.to_string()))?;
        // **The in-memory copy is set by the caller, after the flip, and not here.** This file is
        // durable evidence and the accessor is a convenience; publishing the convenience while the
        // fold can still be discarded would let an operator read a discharge that did not happen.
        Ok(())
    }

    /// The fold's artifact pass: write **every** level whole into the prefix being published, with
    /// the fold's executed deletions dropped from each membership.
    ///
    /// The returned list replaces the manifest's, rather than extending it: one extent per level,
    /// covering `[0, len)`, so the accumulated extents of every earlier publication collapse into
    /// one file each and the prefix names nothing it does not contain.
    ///
    /// **Holes are written, not packed around**, which is the asymmetry with the append-only path:
    /// that one skips a level whose unpublished range has a gap, because a gap there means a
    /// publication landed out of order. Here a gap is the *expected* state — it is what Rule F's
    /// arm leaves behind when this fold retires an artifact — and closing it would hand every later
    /// artifact in the level the identity of its neighbour, since an ordinal *is* the identity a
    /// caller's `tessera_id` resolves to.
    fn rewrite_membership_extents(
        &self,
        prefix_dir: &std::path::Path,
        partition: &str,
        n: u64,
        retired: &croaring::Bitmap,
    ) -> tessera_store::Result<Vec<tessera_store::manifest::MembershipExtent>> {
        let ready = self.live.with_artifacts(|store| store.repack_all(retired));
        if ready.is_empty() {
            return Ok(Vec::new());
        }

        let dir = prefix_dir
            .join("partitions")
            .join(partition)
            .join("members");
        std::fs::create_dir_all(&dir).map_err(|source| tessera_store::StoreError::Io {
            path: dir.clone(),
            source,
        })?;
        let mut entries = Vec::with_capacity(ready.len());
        for (index, (layer, level, ordinal_lo, blobs)) in ready.into_iter().enumerate() {
            // The same naming rule the online route follows: the layer name is path-shaped and
            // never reaches a filename; the publication that introduced the file does.
            let name = format!("members-{n:06}-{index:03}.tsmb");
            let count = blobs.len() as u32;
            let bytes = tessera_store::membership::pack(ordinal_lo, &blobs);
            tessera_store::write_and_fsync(&dir.join(&name), &bytes)?;
            entries.push(tessera_store::manifest::MembershipExtent {
                path: format!("partitions/{partition}/members/{name}"),
                layer,
                level,
                ordinal_lo,
                count,
            });
        }
        tessera_store::fsync_dir(&dir)?;
        Ok(entries)
    }

    /// Compose and write this prefix's containment partitions, one file per `(layer, level)`.
    ///
    /// **Against the prefix being published, not the one being left.** A fold rewrites the term
    /// index, so a partition composed from the old postings would name a table the new prefix's
    /// entities are not in. The new file is on disk by the time this runs — compaction's pass 2
    /// writes it — so the reader is opened over the prefix this is writing into.
    ///
    /// **Inside the artifact pass, before the registry snapshot the manifest is written from**
    /// (`2026-08-21-artifact-layout-selection.md` §5): the coordinate each entry carries is the
    /// level version at the moment it was composed, and the version list beside it comes from the
    /// same borrow, so the two cannot disagree about a publication landing between them.
    ///
    /// **Every failure is an empty list, not a discarded fold.** A partition is derived — the level
    /// recomposes it on first use — so refusing to publish over one would be a refusal outside the
    /// disclosure surface, and the thing being refused has a correct fallback.
    #[allow(clippy::too_many_arguments)]
    fn write_containment_partitions(
        &self,
        prefix_dir: &std::path::Path,
        partition: &str,
        n: u64,
        data_plugin_hash: &str,
        pending: &PendingRetirement,
        index: &mut tessera_store::derived::DerivedIndex,
    ) -> Vec<tessera_store::manifest::ContainmentExtent> {
        // The gate: under any plugin but the builtin the partition is not sound at all, so nothing
        // is composed and nothing is written (`crate::containment`).
        if !crate::containment::signature_shaped(data_plugin_hash) {
            return Vec::new();
        }
        let postings_path = prefix_dir
            .join("partitions")
            .join(partition)
            .join("terms")
            .join("postings.arrow");
        let postings = match tessera_authz::PostingsReader::open(&postings_path, true) {
            Ok(postings) => postings,
            Err(error) => {
                tracing::warn!(
                    path = %postings_path.display(),
                    %error,
                    "the fold could not read the prefix it just wrote to compose containment                      partitions; every level recomposes on first use"
                );
                return Vec::new();
            }
        };

        // Composed under one borrow with the versions they are composed at, and written outside it:
        // composing is the dear part and needs the store, writing a file does not.
        let composed: Vec<(String, u32, u64, Vec<u8>)> = self.live.with_artifacts(|store| {
            let levels: Vec<(String, u32)> = store
                .levels_and_extents()
                .map(|(layer, level, _)| (layer.to_string(), level))
                .collect();
            levels
                .into_iter()
                .filter(|(layer, level)| !pending.is_pending(layer, *level))
                .filter_map(|(layer, level)| {
                    let version = store.level_version(&layer, level);
                    match crate::containment::ContainmentPartition::compose(
                        store, &layer, level, &postings,
                    ) {
                        Ok(partition) => {
                            Some((layer, level, version, partition.as_bytes().to_vec()))
                        }
                        Err(error) => {
                            tracing::warn!(
                                layer = %layer,
                                level,
                                %error,
                                "a containment partition would not compose at the fold; that level                                  recomposes on first use"
                            );
                            None
                        }
                    }
                })
                .collect()
        });
        if composed.is_empty() {
            return Vec::new();
        }

        // **Filed by the shared writer**, which is the same one `tessera build`'s artifact pass
        // calls: one naming rule, one durability sequence, one manifest-entry shape.
        tessera_store::derived::file_containment(
            prefix_dir,
            partition,
            n,
            index,
            composed
                .into_iter()
                .map(
                    |(layer, level, level_version, bytes)| tessera_store::derived::Filed {
                        // A partition is a function of the level's records and the prefix's
                        // postings, so it is not per view and the entry carries none — and no
                        // incarnation either, there being no view to carry one for.
                        view: String::new(),
                        incarnation: tessera_store::manifest::DECLARED_INCARNATION,
                        layer,
                        level,
                        level_version,
                        layout: tessera_types::layer::ServingLayout::ArtifactMajor,
                        bytes: tessera_store::derived::FiledBytes::InHand(bytes),
                    },
                )
                .collect(),
        )
    }

    /// Project and write this prefix's tile-index extent columns, one file per
    /// `(view, layer, level)`.
    ///
    /// **Against the prefix being published, and against its base permutation.** A fold renumbers
    /// row space wholesale, so an extent projected through the old one names other people's
    /// documents. Pass 3 has already written the new `permutation.bin` for each view, so the row
    /// space is opened over what this fold is publishing — base only, with no extents, which is
    /// exactly what the row form holds ([`tessera_store::RowSpace::project_base`]) and therefore
    /// what its extents are computed over. A flush landing during the flight appends rows above the
    /// base and moves none of these.
    ///
    /// **Inside the artifact pass, before the registry snapshot**, and omitting the levels a
    /// retirement is about to move — [`Executor::write_containment_partitions`] argues both, and
    /// the argument is the same one: the coordinate an entry carries and the version list beside it
    /// come from the same borrow, and a level whose records the prefix already holds in
    /// post-retirement form is not the level the store would project.
    ///
    /// ⊘ **What this adds to the fold's artifact pass is unpriced**
    /// (`2026-08-21-artifact-layout-selection.md` §9's constraint 9), and it is not small: an extent
    /// is `minimum` and `maximum` over the same `project_base` the row form is built from, so this
    /// is a **second** pass of the projection §8.1 measures at 376 s over 10⁷ artifacts — paid here
    /// so that `warm_artifact_caches` below claims the column instead of deriving one, and so that a
    /// restart maps it rather than deriving it. The same function rather than a cheaper min/max walk
    /// deliberately: it is what the row form is built with, so the column written here and the
    /// column derived from that form are equal by construction rather than by an argument, and
    /// `tests/artifact_tile_index.rs` asserts them byte for byte.
    ///
    /// What it does **not** add is residency: [`crate::tile_index::TileIndex::project`] holds one
    /// membership at a time, so this pass is eight bytes an artifact where the row form it is
    /// deriving the same extents from would be gigabytes — and the fold runs before the flip, with
    /// the outgoing generation's forms still resident.
    ///
    /// **Every failure is an empty list, not a discarded fold** — the index is derived, so refusing
    /// to publish over one would be a refusal outside the disclosure surface.
    /// The base row space of every view this fold just wrote, opened once for the whole artifact
    /// pass.
    ///
    /// **Against the prefix being published, and base only** — with no extents, which is exactly
    /// what a row form holds ([`tessera_store::RowSpace::project_base`]) and therefore what every
    /// structure derived from one is computed over. A flush landing during the flight appends rows
    /// above the base and moves none of these.
    ///
    /// A view whose permutation will not load is simply absent: its structures are derived on first
    /// use, which is what every request did before the fold wrote anything.
    fn fold_row_spaces(
        &self,
        prefix_dir: &std::path::Path,
        partition: &str,
        views: &[(String, u32)],
    ) -> Vec<(String, tessera_store::RowSpace)> {
        let mut spaces: Vec<(String, tessera_store::RowSpace)> = Vec::new();
        for (view, row_count) in views {
            let partition_dir = prefix_dir.join("partitions").join(partition);
            let path = tessera_store::view_path(&partition_dir, view).join("permutation.bin");
            match tessera_store::Permutation::load(&path) {
                Ok(permutation) => spaces.push((
                    view.clone(),
                    tessera_store::RowSpace::new(std::sync::Arc::new(permutation), *row_count),
                )),
                Err(error) => tracing::warn!(
                    view = %view,
                    path = %path.display(),
                    %error,
                    "the fold could not read the permutation it just wrote, so this view's derived \
                     artifact structures are built on first use"
                ),
            }
        }
        spaces
    }

    /// Whether step 3a composes a level's row column and tile index: every level the retirement
    /// does not move, and an enumerated level it does ([`PendingRetirement`]). A spatial level the
    /// retirement moves is omitted: its rows are resolved from shapes, and the shape of an artifact
    /// the retirement removes is still held.
    fn composes_row_structures(
        &self,
        pending: &PendingRetirement,
        layer: &str,
        level: u32,
    ) -> bool {
        !pending.is_pending(layer, level)
            || self.live.registered_layer(layer).is_some_and(|registered| {
                registered.declaration.membership
                    == tessera_types::layer::MembershipSource::Enumerated
            })
    }

    /// **The fold's layout re-evaluation** — decision 0094's step 4, taken inside the artifact pass
    /// and before a derived byte is written.
    ///
    /// The observations it reads are **post-retirement**: `repack_all` above has already written
    /// the surviving memberships into the prefix, and this reads the store, so a level the fold
    /// retired most of is observed as the level it is about to become. That is exactly the case the
    /// re-evaluation exists for.
    ///
    /// **The record is per `(layer, level)` and the observation is per view**, which is a
    /// mismatch the levels themselves create: a layer drawn in two views has two row spaces and so
    /// two localities. The shape is taken in the **first** view the fold wrote, deterministically,
    /// because the record has one slot and a level is one level however many views draw it. Where
    /// two views disagree sharply the pick follows the first and the other view's column is written
    /// in that layout, which is correct and may be slower than that view would have chosen.
    ///
    /// **A pin is read, never re-derived**, and it reaches here as `declaration.layout` — see
    /// [`crate::layout::choose`].
    ///
    /// **What this adds to the fold's artifact pass, coarsely measured** (selection memo §9's
    /// constraint 9): on this crate's largest fixture — 700 000 rows, 1 100 artifacts of a hundred
    /// members each, `tests/artifact_layout_flip.rs` — the pass runs at **2.5 s** with the
    /// re-evaluation and the column write removed and **3.1 s** with them, so the layout machinery
    /// is about **0.6 s, a quarter of the pass**. One debug-build run on one fixture: an order of
    /// magnitude rather than a measurement, and it is what it is because this is a whole extra
    /// projection of one view — `RowSpace::project_base` per record, the same call the row form and
    /// the tile index each already make. ⊘ At the campaign's 10⁷ artifacts it is a third pass over
    /// the 376 s §8.1 prices one at, which is modelled rather than measured.
    fn choose_layouts(
        &self,
        spaces: &[(String, tessera_store::RowSpace)],
        pending: &PendingRetirement,
        fold_segments: &[(String, tessera_store::read::SegmentData)],
    ) -> Vec<(String, u32, tessera_types::layer::ServingLayout)> {
        let Some((first_view, space)) = spaces.first() else {
            return Vec::new();
        };
        let levels: Vec<(String, u32)> = self.live.with_artifacts(|store| {
            store
                .levels_and_extents()
                .map(|(layer, level, _)| (layer.to_string(), level))
                .filter(|(layer, level)| self.composes_row_structures(pending, layer, *level))
                .collect()
        });
        let mut out = Vec::with_capacity(levels.len());
        // Level counts gathered per layer for the roll-up below, the per-level lines being one
        // level's own shape and the sum being what a response pays.
        let mut per_layer: std::collections::BTreeMap<String, Vec<(u32, u64)>> =
            std::collections::BTreeMap::new();
        for (layer, level) in levels {
            let Some(registered) = self.live.registered_layer(&layer) else {
                continue;
            };
            // **One membership at a time, never the level's row form.** The observation is the same
            // `project_base` per artifact either way; what differs is what is held while it runs,
            // and at ten million artifacts a row form here is the gigabytes §7.3 prices — held
            // beside the outgoing generation's own forms, because the fold runs before the flip.
            //
            // **A spatial level is observed over its resolved rows** (`polygon-membership.md`
            // §6.3): every segment the fold wrote is resolved against the level's shapes here,
            // inside the artifact pass and before anything is written, and the pieces are staged
            // under the new segment ids for the derived files this pass writes and for the row
            // forms the flip's warm builds. That is the fold's re-resolution — everything, because
            // the fold renumbered every row.
            let spatial = registered.declaration.membership
                == tessera_types::layer::MembershipSource::Spatial
                && registered.declaration.shape.is_some();
            let shape = self.live.with_artifacts(|store| {
                if spatial {
                    let mut observed = None;
                    for (view, segment) in fold_segments {
                        let held = self.shapes.level(
                            view,
                            &layer,
                            level,
                            store,
                            &crate::shapes::PersistedPieces::none(),
                        );
                        let (piece, cost) = held.resolve(segment);
                        let piece = Arc::new(piece);
                        held.stage(&segment.seg_id, Arc::clone(&piece));
                        tracing::info!(
                            layer = %layer,
                            level,
                            view = %view,
                            seg_id = %segment.seg_id,
                            rows_tested = cost.rows_tested,
                            rows_interior = cost.rows_interior,
                            artifacts_empty = cost.artifacts_empty,
                            elapsed_ms = cost.elapsed_ms,
                            "the fold re-resolved a segment against a spatial level's shapes"
                        );
                        if view == first_view {
                            observed = Some(tessera_store::derived::observe_shape(
                                space.base_rows(),
                                &|visit| {
                                    for (ordinal, rows) in piece.iter().enumerate() {
                                        if let Some(rows) = rows {
                                            visit(ordinal as u32, rows);
                                        }
                                    }
                                },
                            ));
                        }
                    }
                    observed.unwrap_or_else(tessera_store::derived::LevelShape::empty)
                } else {
                    tessera_store::derived::observe_shape(space.base_rows(), &|visit| {
                        for (ordinal, record) in pending.records(store, &layer, level) {
                            visit(ordinal, &space.project_base(&record.members));
                        }
                    })
                }
            });
            let chosen = crate::layout::choose(&registered.declaration, shape);
            tracing::info!(
                layer = %layer,
                level,
                artifacts = shape.artifacts,
                // **The trigger**, and the figure beside it is reported rather than read —
                // decision 0092's (c), and the axis the 2026-08-22 bracket moved the pick onto.
                everywhere_fraction = shape.everywhere_fraction,
                blocks_per_artifact = shape.blocks_per_artifact,
                partitions = shape.partitions,
                pinned = ?registered.declaration.layout,
                was = ?registered.layout_of(level),
                now = ?chosen,
                "the fold re-evaluated a level's serving layout"
            );
            per_layer
                .entry(layer.clone())
                .or_default()
                .push((level, shape.artifacts));
            out.push((layer, level, chosen));
        }
        // **What a whole-layer response costs, reported and never refused** (owner ruling
        // 2026-08-28). The per-level lines above each carry their own count; this is the sum, and
        // the sum is the figure that predicts response volume, because a response carries one row
        // per served artifact and the levels a request does not exclude are all of them.
        //
        // **Reported rather than bounded, and the distinction is the ruling's.** A large response
        // is slow, not wrong: it discloses nothing the mask did not already allow and a rerun costs
        // nothing, so it is the operator's call and not the service's. The bound that does exist is
        // the request's — `levels`, whose absent case follows this layer's own declared zoom ranges
        // — and an operator who sees a number here they do not like has a declaration to change.
        //
        // **No byte estimate.** Bytes per artifact depend on what the layer declares: a count-only
        // level is tens of bytes and one declaring a hull is unbounded, the rings being a function
        // of the membership. A constant here would be a guess wearing a measurement's clothes; the
        // artifact count is what is actually known.
        for (layer, mut levels) in per_layer {
            levels.sort_unstable();
            let total: u64 = levels.iter().map(|(_, n)| *n).sum();
            // **The levels this fold evaluated, which is not always the layer's whole set**: a
            // level being retired is filtered out above, and so is one whose layer is no longer
            // registered. The build's report (`tessera_build::artifact_pass::report`) is the one
            // that sees every level, and is where an operator reads a layer's response cost;
            // this is the fold's own view of what it just re-evaluated.
            tracing::info!(
                layer = %layer,
                evaluated_artifacts = total,
                per_level = ?levels,
                "the fold re-evaluated these levels of a layer; the sum is what a response naming \
                 them carries, one row per served artifact"
            );
        }
        out
    }

    /// Write this prefix's row-major columns, one file per `(view, layer, level)` whose chosen
    /// layout has one.
    ///
    /// **A level whose label column will not compose gets no file**, and the manifest then names
    /// none for it — so the level is served artifact-major on the reader's side, loudly
    /// (`ArtifactProjections::get_or_build`). That is the fold-time half of the refusal the
    /// declaration could not make: whether an attribute is single-valued is a property of the data.
    ///
    /// **Every failure is an empty entry, not a discarded fold** — a column is derived, and the
    /// artifact-major route answers every question it would have.
    #[allow(clippy::too_many_arguments)]
    fn write_row_columns(
        &self,
        prefix_dir: &std::path::Path,
        partition: &str,
        n: u64,
        incarnations: &FxHashMap<String, tessera_types::view::ViewIncarnation>,
        spaces: &[(String, tessera_store::RowSpace)],
        layouts: &[(String, u32, tessera_types::layer::ServingLayout)],
        pending: &PendingRetirement,
        fold_segments: &[(String, tessera_store::read::SegmentData)],
        index: &mut tessera_store::derived::DerivedIndex,
    ) -> Vec<tessera_store::manifest::RowColumnExtent> {
        let wanted: Vec<(String, u32, tessera_types::layer::ServingLayout)> = layouts
            .iter()
            .filter(|(layer, level, layout)| {
                layout.is_row_major()
                    && self.composes_row_structures(pending, layer, *level)
                    // **An attribute level's column is not this fold's to write**, and the reason
                    // is what it is a column *of*: its labels come from the value column the
                    // predicate names, and this pass composes from the level's stored memberships
                    // — which such a level has none of. Composing anyway would write a file of
                    // nothing but holes, name it in the manifest, and leave the reader adopting a
                    // column no request will ever claim. A spatial level's column *is* written,
                    // from the rows the pass just resolved.
                    && self
                        .live
                        .registered_layer(layer)
                        .is_some_and(|registered| {
                            matches!(
                                registered.declaration.membership,
                                tessera_types::layer::MembershipSource::Enumerated
                                    | tessera_types::layer::MembershipSource::Spatial
                            )
                        })
            })
            .cloned()
            .collect();
        if wanted.is_empty() || spaces.is_empty() {
            return Vec::new();
        }

        // Composed under one borrow with the versions they are written at, and filed outside it,
        // exactly as the tile indexes are.
        //
        // **Straight to the file, not through a `RowColumn`.** The fold wants the column's bytes
        // and nothing else, and `project_row_column` writes them front to back into a file under
        // the engine's scratch; building a form here would hold the packed column and then copy it
        // (`docs/evidence/memos/2026-09-12-bounded-assembly-design.md` §4.6).
        let scratch = self.artifact_projections.scratch();
        let written: Vec<(
            String,
            String,
            u32,
            u64,
            tessera_types::layer::ServingLayout,
            std::path::PathBuf,
        )> = self.live.with_artifacts(|store| {
            let mut out = Vec::with_capacity(wanted.len() * spaces.len());
            for (layer, level, layout) in &wanted {
                let version = pending.version_after(store, layer, *level);
                let ordinals = level_length(pending.records(store, layer, *level));
                let spatial = self.live.registered_layer(layer).is_some_and(|registered| {
                    registered.declaration.membership
                        == tessera_types::layer::MembershipSource::Spatial
                });
                for (view, space) in spaces {
                    let composed = if spatial {
                        // The fold's segment is the whole base at row base 0, so the piece
                        // staged in `choose_layouts` is the level's membership in this view.
                        let piece = self.shapes.get(view, layer, *level).and_then(|held| {
                            fold_segments
                                .iter()
                                .find(|(v, _)| v == view)
                                .and_then(|(_, segment)| held.staged(&segment.seg_id))
                        });
                        match piece {
                            Some(piece) => {
                                let rows = piece.as_ref();
                                tessera_store::derived::project_row_column(
                                    rows.len() as u32,
                                    space.base_rows(),
                                    *layout,
                                    scratch,
                                    &|visit| {
                                        for (ordinal, rows) in rows.iter().enumerate() {
                                            if let Some(rows) = rows {
                                                visit(ordinal as u32, rows);
                                            }
                                        }
                                    },
                                )
                            }
                            None => Ok(None),
                        }
                    } else {
                        tessera_store::derived::project_row_column(
                            ordinals,
                            space.base_rows(),
                            *layout,
                            scratch,
                            &|visit| {
                                for (ordinal, record) in pending.records(store, layer, *level) {
                                    visit(ordinal, &space.project_base(&record.members));
                                }
                            },
                        )
                    };
                    match composed {
                        Ok(Some(path)) => out.push((
                            view.clone(),
                            layer.clone(),
                            *level,
                            version,
                            *layout,
                            path,
                        )),
                        Ok(None) => tracing::warn!(
                            layer = %layer,
                            level,
                            view = %view,
                            layout = ?layout,
                            "a row-major column would not compose at the fold — this level's \
                             memberships do not partition — so it is served artifact-major. \
                             Every answer is unchanged; the layout is not"
                        ),
                        Err(error) => tracing::warn!(
                            layer = %layer,
                            level,
                            view = %view,
                            %error,
                            "a row-major column would not be composed at the fold; that level \
                             derives it on first use"
                        ),
                    }
                }
            }
            out
        });
        if written.is_empty() {
            return Vec::new();
        }

        // **Filed by the shared writer** — see `write_containment_partitions` above.
        tessera_store::derived::file_row_columns(
            prefix_dir,
            partition,
            n,
            index,
            written
                .into_iter()
                .filter_map(|(view, layer, level, level_version, layout, path)| {
                    // **No incarnation, no file** (decision 0115): a structure addressed by row
                    // and stamped with a guess would label another key's rows.
                    let Some(incarnation) = incarnations.get(&view).copied() else {
                        let _ = std::fs::remove_file(&path);
                        return None;
                    };
                    Some(tessera_store::derived::Filed {
                        view,
                        incarnation,
                        layer,
                        level,
                        level_version,
                        layout,
                        bytes: tessera_store::derived::FiledBytes::Staged(path),
                    })
                })
                .collect(),
        )
    }

    /// Write this prefix's shape row forms: for every spatial level `columns` does not cover, the
    /// piece the fold resolved for each of its segments in `choose_layouts`, keyed by that segment
    /// and the level's version.
    ///
    /// **Every failure is a dropped entry, not a discarded fold** — the form is derived, and an
    /// open that finds no entry resolves the segment again, loudly.
    #[allow(clippy::too_many_arguments)]
    fn write_shape_rows(
        &self,
        prefix_dir: &std::path::Path,
        partition: &str,
        n: u64,
        incarnations: &FxHashMap<String, tessera_types::view::ViewIncarnation>,
        columns: &[tessera_store::manifest::RowColumnExtent],
        pending: &PendingRetirement,
        fold_segments: &[(String, tessera_store::read::SegmentData)],
        index: &mut tessera_store::derived::DerivedIndex,
    ) -> Vec<tessera_store::manifest::ShapeRowsExtent> {
        let levels: Vec<(String, u32)> = self.live.with_artifacts(|store| {
            store
                .levels_and_extents()
                .map(|(layer, level, _)| (layer.to_string(), level))
                .filter(|(layer, level)| !pending.is_pending(layer, *level))
                .filter(|(layer, _)| {
                    self.live.registered_layer(layer).is_some_and(|registered| {
                        registered.declaration.membership
                            == tessera_types::layer::MembershipSource::Spatial
                            && registered.declaration.shape.is_some()
                    })
                })
                .collect()
        });
        let mut filed: Vec<tessera_store::derived::FiledShapeRows> = Vec::new();
        for (layer, level) in &levels {
            let version = self
                .live
                .with_artifacts(|store| store.level_version(layer, *level));
            for (view, segment) in fold_segments {
                let covered = columns
                    .iter()
                    .any(|c| &c.layer == layer && c.level == *level && &c.view == view);
                if covered {
                    continue;
                }
                let Some(piece) = self
                    .shapes
                    .get(view, layer, *level)
                    .and_then(|held| held.staged(&segment.seg_id))
                else {
                    tracing::warn!(
                        layer = %layer,
                        level,
                        view = %view,
                        seg_id = %segment.seg_id,
                        "the fold holds no resolved piece for this segment, so no row form is \
                         written; the next open resolves it"
                    );
                    continue;
                };
                // **No incarnation, no file** — `write_row_columns`' rule.
                let Some(incarnation) = incarnations.get(view).copied() else {
                    continue;
                };
                filed.push(tessera_store::derived::FiledShapeRows {
                    view: view.clone(),
                    incarnation,
                    layer: layer.clone(),
                    level: *level,
                    level_version: version,
                    seg_id: segment.seg_id.clone(),
                    row_count: segment.row_count,
                    bytes: tessera_store::derived::shape_rows_bytes(
                        version,
                        &segment.seg_id,
                        segment.row_count,
                        &piece,
                    ),
                });
            }
        }
        tessera_store::derived::file_shape_rows(prefix_dir, partition, n, index, filed)
    }

    /// Write this prefix's persisted decompositions: every spatial level's held shapes for each
    /// fold view, as the level holds them at its current version.
    #[allow(clippy::too_many_arguments)]
    fn write_shape_held(
        &self,
        prefix_dir: &std::path::Path,
        partition: &str,
        n: u64,
        incarnations: &FxHashMap<String, tessera_types::view::ViewIncarnation>,
        pending: &PendingRetirement,
        fold_segments: &[(String, tessera_store::read::SegmentData)],
        index: &mut tessera_store::derived::DerivedIndex,
    ) -> Vec<tessera_store::manifest::ShapeHeldExtent> {
        let filed: Vec<tessera_store::derived::Filed> = self.live.with_artifacts(|store| {
            let mut out = Vec::new();
            let levels: Vec<(String, u32)> = store
                .levels_and_extents()
                .map(|(layer, level, _)| (layer.to_string(), level))
                .filter(|(layer, level)| !pending.is_pending(layer, *level))
                .filter(|(layer, _)| {
                    self.live.registered_layer(layer).is_some_and(|registered| {
                        registered.declaration.membership
                            == tessera_types::layer::MembershipSource::Spatial
                            && registered.declaration.shape.is_some()
                    })
                })
                .collect();
            for (layer, level) in &levels {
                let version = store.level_version(layer, *level);
                for (view, _) in fold_segments {
                    let Some(held) = self.shapes.get(view, layer, *level) else {
                        continue;
                    };
                    if held.level_version != version {
                        continue;
                    }
                    let shapes: Vec<(Option<&[u8]>, Option<&tessera_store::derived::HeldShape>)> =
                        (0..held.shapes.len() as u32)
                            .map(|ordinal| {
                                (
                                    store
                                        .shape_of(layer, *level, ordinal)
                                        .and_then(|shapes| shapes.for_view(view)),
                                    held.shapes[ordinal as usize].as_ref(),
                                )
                            })
                            .collect();
                    let Some(incarnation) = incarnations.get(view).copied() else {
                        continue;
                    };
                    out.push(tessera_store::derived::Filed {
                        view: view.clone(),
                        incarnation,
                        layer: layer.clone(),
                        level: *level,
                        level_version: version,
                        layout: tessera_types::layer::ServingLayout::ArtifactMajor,
                        bytes: tessera_store::derived::FiledBytes::InHand(
                            tessera_store::derived::shape_held_bytes(version, &shapes),
                        ),
                    });
                }
            }
            out
        });
        tessera_store::derived::file_shape_held(prefix_dir, partition, n, index, filed)
    }

    #[allow(clippy::too_many_arguments)]
    fn write_tile_indexes(
        &self,
        prefix_dir: &std::path::Path,
        partition: &str,
        n: u64,
        incarnations: &FxHashMap<String, tessera_types::view::ViewIncarnation>,
        spaces: &[(String, tessera_store::RowSpace)],
        layouts: &[(String, u32, tessera_types::layer::ServingLayout)],
        pending: &PendingRetirement,
        index: &mut tessera_store::derived::DerivedIndex,
    ) -> Vec<tessera_store::manifest::TileIndexExtent> {
        if spaces.is_empty() {
            return Vec::new();
        }

        // Projected under one borrow with the versions they are projected at, and written outside
        // it: projecting is the dear part and needs the store, writing a file does not.
        let projected: Vec<(String, String, u32, u64, Vec<u8>)> =
            self.live.with_artifacts(|store| {
                let levels: Vec<(String, u32)> = store
                    .levels_and_extents()
                    .map(|(layer, level, _)| (layer.to_string(), level))
                    .filter(|(layer, level)| self.composes_row_structures(pending, layer, *level))
                    // **A row-major level has nothing to index** (selection memo §1): its candidacy
                    // is a scan of `viewport ∩ M_auth`, which the viewport already bounds. Writing
                    // one would be writing a file no reader on that route opens — and a level that
                    // falls back derives its index on first use, which is the same answer at the
                    // cost this pass was trying to save.
                    .filter(|(layer, level)| {
                        !layouts
                            .iter()
                            .any(|(l, v, layout)| l == layer && v == level && layout.is_row_major())
                    })
                    // **A spatial level's index is not projected from its records**, which carry
                    // no membership — an index of empties would be adopted at open and settle
                    // nothing. Its index is built at open over the held pieces, one pass per
                    // level over row extents, which is cheap where the resolution is not.
                    .filter(|(layer, _)| {
                        !self.live.registered_layer(layer).is_some_and(|registered| {
                            registered.declaration.membership
                                == tessera_types::layer::MembershipSource::Spatial
                        })
                    })
                    .collect();
                let mut out = Vec::with_capacity(levels.len() * spaces.len());
                for (layer, level) in &levels {
                    let version = pending.version_after(store, layer, *level);
                    // **The level's own length, holes included** — a column sized by the last live
                    // ordinal is short, and a short one is dropped at open rather than adopted.
                    let ordinals = level_length(pending.records(store, layer, *level));
                    for (view, space) in spaces {
                        let index = crate::tile_index::TileIndex::project(
                            ordinals,
                            || pending.records(store, layer, *level),
                            space,
                        );
                        out.push((
                            view.clone(),
                            layer.clone(),
                            *level,
                            version,
                            index.as_bytes().to_vec(),
                        ));
                    }
                }
                out
            });
        if projected.is_empty() {
            return Vec::new();
        }

        // **Filed by the shared writer** — see `write_containment_partitions` above.
        tessera_store::derived::file_tile_indexes(
            prefix_dir,
            partition,
            n,
            index,
            projected
                .into_iter()
                .filter_map(|(view, layer, level, level_version, bytes)| {
                    // **No incarnation, no file** — `write_row_columns`' rule.
                    let incarnation = *incarnations.get(&view)?;
                    Some(tessera_store::derived::Filed {
                        view,
                        incarnation,
                        layer,
                        level,
                        level_version,
                        layout: tessera_types::layer::ServingLayout::ArtifactMajor,
                        bytes: tessera_store::derived::FiledBytes::InHand(bytes),
                    })
                })
                .collect(),
        )
    }

    /// Rebuild every level's row-space membership, and every lineage this fold moved, against the
    /// live generation.
    ///
    /// Called at the fold's own publication, on this thread, for the reason §5.0.3 gives: the
    /// alternative is not a cache miss but a stall, and it lands on a request rather than on
    /// maintenance. Cheap everywhere else — a deployment with no artifacts iterates nothing.
    ///
    /// **The lineages are warmed here for the same reason and not the same extent.** A row form is
    /// invalid at every level because the prefix renumbered row space; a lineage is invalid only
    /// where this fold retired an artifact, because it holds ordinals. So this asks for all of
    /// both and pays for one of each per level that moved — and what it is buying is the ~96 ms at
    /// a level of ten million that would otherwise land on whichever request arrived first.
    ///
    /// **Errors are impossible to have here and absences are not**: a view the generation does not
    /// carry is simply not warmed, and its first request builds what it needs, which is the same
    /// outcome this method exists to avoid but not a wrong one.
    ///
    /// What each level's build reads is what the fold wrote and the publication adopted: an
    /// artifact-major level claims its tile index, a row-major one transposes its column, and a
    /// level with neither projects its memberships (`ArtifactProjections::get_or_build`).
    fn warm_artifact_caches(&self) {
        let generation = self.generation.load_full();
        let levels: Vec<(String, u32)> = self.live.with_artifacts(|store| {
            store
                .levels_and_extents()
                .map(|(layer, level, _)| (layer.to_string(), level))
                .collect()
        });
        if levels.is_empty() {
            return;
        }
        let started = std::time::Instant::now();
        let before_projections = self.artifact_projections.builds();
        let before_lineages = self.lineages.builds();
        for partition in generation.bundle.partitions.values() {
            for (view, view_data) in &partition.views {
                for (layer, level) in &levels {
                    // The layout the fold has just recorded, so the warm builds the form the next
                    // request will ask for rather than one it would immediately replace.
                    let layout = self
                        .live
                        .registered_layer(layer)
                        .map(|registered| registered.layout_of(*level))
                        .unwrap_or_default();
                    // ⊘ **A predicate level is not warmed here**, and skipping it is the honest
                    // answer rather than a gap: its membership is evaluated against the geometry,
                    // so a form built now is filed under this fold's `segments_version` and the
                    // first flush after it rebuilds anyway. The pieces a rule needs — the column's
                    // value layers, the view's quantisation, the segment list — are the request
                    // path's, and reaching for them here would be a second place the membership is
                    // assembled. What such a level pays instead is one derivation on the first
                    // request after the fold, which is what every level paid before this warm
                    // existed.
                    //
                    // **A spatial level is warmed**, because its membership is held rather than
                    // evaluated: the fold's artifact pass resolved the new segments against the
                    // level's shapes and staged the pieces, so the build here is the O(containers)
                    // assembly of them, and the form it produces is maintained from then on.
                    let registered = self.live.registered_layer(layer);
                    let membership = registered
                        .as_ref()
                        .map(|r| r.declaration.membership.clone());
                    let spatial = matches!(
                        membership,
                        Some(tessera_types::layer::MembershipSource::Spatial)
                    ) && registered
                        .as_ref()
                        .is_some_and(|r| r.declaration.shape.is_some());
                    if matches!(
                        membership,
                        Some(tessera_types::layer::MembershipSource::Attribute(_))
                    ) || (!spatial
                        && !matches!(
                            membership,
                            Some(tessera_types::layer::MembershipSource::Enumerated)
                        ))
                    {
                        continue;
                    }
                    let segments = if spatial {
                        crate::viewport::segments_with_row_bases(view, view_data).ok()
                    } else {
                        None
                    };
                    self.live.with_artifacts(|store| {
                        let predicate = segments.as_ref().map(|segments| {
                            crate::artifacts::PredicateSource::Spatial(
                                crate::artifacts::SpatialSource {
                                    level: self.shapes.level(
                                        view,
                                        layer,
                                        *level,
                                        store,
                                        &crate::shapes::PersistedPieces::none(),
                                    ),
                                    segments,
                                    total_rows: u32::try_from(view_data.row_space.total_rows())
                                        .unwrap_or(u32::MAX),
                                },
                            )
                        });
                        self.artifact_projections.get_or_build(
                            &generation.prefix,
                            view,
                            layer,
                            *level,
                            store,
                            &view_data.row_space,
                            Some(&generation.partition_source()),
                            layout,
                            predicate.as_ref(),
                            generation.segments_version,
                            self.live.registered_layer(layer).is_some_and(|r| {
                                crate::artifacts::serves_column_only(&r.declaration)
                            }),
                        )
                    });
                }
            }
        }
        for (layer, level) in &levels {
            self.live.with_artifacts(|store| {
                self.lineages.get_or_build(
                    layer,
                    *level,
                    store.lineage_version(layer, *level),
                    || {
                        crate::cut::Lineage::new(store.level(layer, *level).map(
                            |(ordinal, record)| {
                                let within = record
                                    .parents
                                    .iter()
                                    .find(|parent| parent.level == *level)
                                    .map(|parent| parent.ordinal);
                                (ordinal, within)
                            },
                        ))
                    },
                )
            });
        }
        tracing::info!(
            projections = self.artifact_projections.builds() - before_projections,
            lineages = self.lineages.builds() - before_lineages,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "the fold's artifact pass rebuilt every level's row form"
        );
    }

    /// Pack every not-yet-published membership into one extent per level and fsync it, returning
    /// the manifest entries.
    ///
    /// **One file per level per publication.** Publication is append-only, so an extent covers a
    /// contiguous ordinal range and no earlier extent is disturbed — a reader unions a level's
    /// extents and the fold rewrites them into one.
    fn write_membership_extents(
        &self,
        prefix_dir: &std::path::Path,
        partition: &str,
        n: u64,
    ) -> tessera_store::Result<Vec<tessera_store::manifest::MembershipExtent>> {
        let (ready, skipped) = self.live.unpublished_memberships();
        for (layer, level) in skipped {
            // Unreachable while publication is append-only, and alarmed rather than asserted: an
            // extent addresses a dense ordinal range, so packing around a hole would shift every
            // later artifact's identity by one.
            tracing::error!(
                layer = %layer,
                level,
                "ALARM: a level has a hole below its ordinal high-water, so its memberships are \
                 not published; they stay WAL-durable and the log stays pinned"
            );
        }
        if ready.is_empty() {
            return Ok(Vec::new());
        }

        let dir = prefix_dir
            .join("partitions")
            .join(partition)
            .join("members");
        std::fs::create_dir_all(&dir).map_err(|source| tessera_store::StoreError::Io {
            path: dir.clone(),
            source,
        })?;

        let mut entries = Vec::with_capacity(ready.len());
        for (index, (layer, level, ordinal_lo, blobs)) in ready.into_iter().enumerate() {
            // **The layer name never reaches the filename.** It is path-shaped — `clusters/a` — so
            // a name-derived path would escape the directory or collide after escaping. The
            // manifest entry carries the name; the file is addressed by the publication that
            // introduced it and its index within that publication.
            let name = format!("members-{n:06}-{index:03}.tsmb");
            let count = blobs.len() as u32;
            let bytes = tessera_store::membership::pack(ordinal_lo, &blobs);
            tessera_store::write_and_fsync(&dir.join(&name), &bytes)?;
            entries.push(tessera_store::manifest::MembershipExtent {
                path: format!("partitions/{partition}/members/{name}"),
                layer,
                level,
                ordinal_lo,
                count,
            });
        }
        // The directory entry itself has to be durable, or a crash leaves a manifest naming a file
        // whose name was never written — the same rule every other publication here follows.
        tessera_store::fsync_dir(&dir)?;
        Ok(entries)
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
        let mut mark = StageMark::now();
        if !self.publish_flush_stages(completed, &mut mark) {
            // A discarded flush's time since its last lap, so `PublishWall` stays partitioned
            // whichever way the publication ends.
            self.health
                .flush_lap(crate::flush::FlushStage::Discarded, mark);
        }
    }

    /// The publication's stages, each lapped as it ends. Returns whether the flush swapped;
    /// `mark` is left at the last lap so the caller can charge a discard's tail.
    fn publish_flush_stages(
        &mut self,
        completed: crate::flush::CompletedFlush,
        mark: &mut StageMark,
    ) -> bool {
        let started = std::time::Instant::now();
        // **A node whose durable state disagrees with what it is serving publishes nothing**
        // (`may_publish`). The unit's files are orphans and its inputs still stand, which is the
        // same posture every other publication failure takes.
        if !self.may_publish() {
            return false;
        }
        let live = self.generation.load_full();
        if live.prefix != completed.prefix {
            // A compaction moved the prefix under this flush. Nothing to apply it to.
            tracing::warn!(
                planned = %completed.prefix,
                live = %live.prefix,
                "discarding a completed flush planned against a superseded prefix"
            );
            return false;
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
            return false;
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
            return false;
        };
        // **Composed before the manifest is written, because a composition that refuses must not
        // leave a published manifest naming the extents it refused.** The refusal is unreachable —
        // an extent covers entities I9 has just issued, which no earlier layer can hold — so this
        // is the same posture as every other flush failure: the files are orphans, the buffer
        // stands, the next tick re-plans.
        let extents: Vec<crate::filter::PublishedExtent> = completed
            .filter_extents
            .iter()
            .map(|e| {
                (
                    // The resolved name for a group-scoped family's column, the column's own for
                    // an entity-scoped one — one function, so a flush's layer composes under the
                    // key a leaf resolves to (`filter::extent_column_name`).
                    crate::filter::extent_column_name(&e.column, e.view.as_deref()),
                    e.values_rel.clone(),
                    Arc::clone(&e.values),
                    // A keyword extent's dictionary travels with its ordinals or the composition
                    // refuses: the ordinals are positions in *this* dictionary and name nothing
                    // against another (records §4.3).
                    e.dict.clone(),
                )
            })
            .collect();
        // The record extent composes onto the live stack here, not only into the manifest: a
        // published extent that no live stack holds answers no drill-down until the next fold.
        let record_dir = self.bundle_root.join(&completed.prefix);
        let record_paths: Vec<tessera_filter::RecordExtentPaths> = completed
            .record_extent
            .iter()
            .map(|e| tessera_filter::RecordExtentPaths {
                blocks: record_dir.join(&e.blocks),
                hasrow: record_dir.join(&e.hasrow),
                directory: record_dir.join(&e.directory),
            })
            .collect();
        let entity_terms_paths = vec![tessera_store::EntityTermsExtentPaths {
            hasrow: record_dir.join(&completed.entity_terms_extent.hasrow),
            offsets: record_dir.join(&completed.entity_terms_extent.offsets),
            terms: record_dir.join(&completed.entity_terms_extent.terms),
        }];
        let text_paths: Vec<crate::filter::TextExtentPaths> = completed
            .text_extents
            .iter()
            .map(|e| crate::filter::TextExtentPaths {
                column: crate::filter::extent_column_name(&e.column, e.view.as_deref()),
                dict_rel: e.dict.clone(),
                dict: record_dir.join(&e.dict),
                postings: record_dir.join(&e.postings),
                presence: record_dir.join(&e.presence),
            })
            .collect();
        // **The new columns first, then the extents that land on them** (`views.md` §5). A flush
        // of a view a family had no column for wrote its base in the same unit as its extent, and
        // the extent composes *onto* a column — so the column has to exist before the composition
        // below can find it. Empty in every steady-state flush, where the base has been on disc
        // since the build.
        let live_columns = if completed.scoped_columns.is_empty() {
            Arc::clone(&live.filter_columns)
        } else {
            let partition_dir = record_dir.join("partitions").join(&completed.partition);
            // Stamped with the flush's own incarnation, which is what places the base it just
            // wrote (decision 0115).
            let opening: Vec<(String, String, tessera_types::view::ViewIncarnation)> = completed
                .scoped_columns
                .iter()
                .map(|(column, view)| (column.clone(), view.clone(), completed.incarnation))
                .collect();
            match live.filter_columns.with_scoped_columns(
                &partition_dir,
                &opening,
                &live.bundle.manifest.scoped_scalars(),
                &live.bundle.manifest.vocabularies,
                true,
            ) {
                Ok(columns) => Arc::new(columns),
                Err(e) => {
                    self.health.flush_failures.fetch_add(1, Ordering::Relaxed);
                    tracing::error!(
                        error = %e,
                        "ALARM: a completed flush wrote a group-scoped column this process cannot \
                         open; discarding it rather than publishing a manifest naming a column no \
                         request could read. Its files are orphans and the buffer is retained"
                    );
                    return false;
                }
            }
        };
        let filter_columns = match live_columns.with_extents(
            &extents,
            &record_paths,
            &entity_terms_paths,
            &text_paths,
        ) {
            Ok(columns) => Arc::new(columns),
            Err(e) => {
                self.health.flush_failures.fetch_add(1, Ordering::Relaxed);
                tracing::error!(
                    error = %e,
                    "ALARM: a completed flush's filter extents would not compose onto the live \
                     columns; discarding it rather than publishing a bundle whose filter answers \
                     would be wrong. Its files are orphans and the buffer is retained"
                );
                return false;
            }
        };
        *mark = self
            .health
            .flush_lap(crate::flush::FlushStage::Compose, *mark);

        let mut manifest = partition_data.manifest.clone();
        let manifest_n = self.allocate_manifest_n();
        // **The watermark advances at every flush publication, and never regresses** — a
        // publication coordinate, which is the only reading left of it.
        //
        // `entity_hi + 1` of *this view's* flush was the whole definition while a bundle had one
        // view, and under several it is neither monotone nor sufficient. Not monotone: views flush
        // one per tick, so a view holding older entities publishes after one holding newer ones
        // and offers a lower number — which `check_manifest_publishable` refuses, leaving those
        // rows buffered for ever with nothing but a `warn!` to say so. Not sufficient: fragment
        // freshness is `fragment.watermark >= generation.watermark` (`Engine::fragment_for`), so a
        // publication that did not move it would let a session keep a fragment built before this
        // flush's postings tier — its entities in no fragment and no buffer, invisible until the
        // session re-authorised.
        //
        // The entity-threshold reading is already gone: `compose::verdict` dropped its
        // `entity < watermark` gate when the buffer became exactly the rows without geometry, and
        // its own note records that removing it took a silent multi-view hazard with it. What is
        // left reads this as *has anything been published since* — `check_publishable`,
        // `check_manifest_publishable`, and the fragment test above — and all three want a
        // coordinate that strictly advances. Single-view behaviour is unchanged: ids are issued
        // monotonically, so `entity_hi + 1` was already above the live value there.
        // A values-only publication has no segment and so no entity high-water of its own; it
        // still advances the watermark, because it publishes extents a resident fragment must be
        // rebuilt past (`ingest.md` §1.4).
        manifest.watermark = completed
            .segment
            .as_ref()
            .map(|s| s.watermark)
            .unwrap_or(0)
            .max(manifest.watermark + 1);
        manifest.entity_id_high_water = manifest.entity_id_high_water.max(
            completed
                .segment
                .as_ref()
                .map(|s| s.entity_id_high_water)
                .unwrap_or(0),
        );
        // **The row-less half of the same obligation.** A flush is the routine publication, so it
        // is where a registration made since the last one stops depending on the WAL surviving:
        // rotation reclaims `LayerCreate`, and without this the mark and the registry go with it.
        // `min`, not `max` — this region grows downward — and taken from the live allocator rather
        // than from the flush, which knows only about points.
        let (layers, layer_tombstones, low_water) = self.live.registry_for_publication();
        manifest.entity_id_low_water = manifest.entity_id_low_water.min(low_water);
        manifest.layers = layers;
        manifest.layer_tombstones = layer_tombstones;
        // The roster beside them, on the same rule and for the same reason (`views.md` §3.2).
        let (created_views, dead_view_incarnations) = self.live.roster_for_publication();
        manifest.views = created_views;
        manifest.dead_view_incarnations = dead_view_incarnations;
        // The runtime attribute columns beside the roster, on its rule (`ingest.md` §6.3).
        let (attributes, scoped_attributes) = self.live.attributes_for_publication();
        manifest.attributes = attributes;
        manifest.scoped_attributes = scoped_attributes;
        // And the runtime vocabularies with their values, on the same rule (`ingest.md` §1.3).
        manifest.vocabularies = self.live.vocabularies_for_publication(&live.vocabularies);
        // And the view groups and plain views, on the same rule (`ingest.md` §1.3). A group's
        // roster is `manifest.views` above; what these carry is the group's own half.
        let (groups, plain_views) = self.live.view_declarations_for_publication();
        manifest.groups = groups;
        manifest.plain_views = plain_views;
        // **And the group-scoped columns this flush gave a view its first of** (`views.md` §5).
        // Carried forward and appended to, never restated: the list is what a *restart* recovers
        // `scoped_scalars[..].views` from, and a render-only family writes no extent for the
        // derivation to find. `manifest` is the live side-manifest cloned, so the earlier pairs
        // are already here.
        for (column, view) in &completed.scoped_columns {
            let entry = tessera_store::manifest::ScopedColumn {
                column: column.clone(),
                view: view.clone(),
                // **The incarnation this flush wrote under** (decision 0115). The list is carried
                // forward for ever, so an entry outlives the drop that orphaned its column; the
                // stamp is what keeps a key created again from publishing it as its own.
                incarnation: completed.incarnation,
            };
            if !manifest.scoped_columns.contains(&entry) {
                manifest.scoped_columns.push(entry);
            }
        }
        // The segment's own four manifest lists, taken together or not at all: a values-only
        // publication wrote none of the files they name (`ingest.md` §1.4).
        if let Some(segment) = &completed.segment {
            manifest.segments.push(segment.descriptor.clone());
            manifest.deltas.push(segment.tier_path.clone());
            manifest
                .external_id_runs
                .push(segment.external_id_run.clone());
            manifest
                .locator_extents
                .push(segment.locator_extent.clone());
        }
        manifest.files.extend(completed.files);
        if let Some(extent) = completed.dict_extent {
            manifest.dict_extents.push(extent);
        }
        // Named in the manifest as well as digested in `files`: the reader composes exactly what
        // this list names, so an extent on disk that no manifest names is not read and one named
        // but absent is a refusal to open (`FilterColumns::open`).
        manifest
            .attr_extents
            .extend(
                completed
                    .filter_extents
                    .iter()
                    .map(|e| tessera_store::manifest::AttrExtent {
                        column: e.column.clone(),
                        // `None` for an entity-scoped column, which belongs to no view — the
                        // incarnation follows the view exactly (decision 0115).
                        incarnation: e.view.as_ref().map(|_| completed.incarnation),
                        view: e.view.clone(),
                        values: e.values_rel.clone(),
                        presence: e.presence_rel.clone(),
                        // One record, so the layer's files swap as one: an extent's ordinals are
                        // positions in *that* extent's dictionary, and a reader that saw a new
                        // dictionary beside old ordinals would recolour the window (records §7).
                        dict: e.dict_rel.clone(),
                        postings: None,
                        offsets: None,
                    }),
            );
        // The record-blob extent, under the same two-obligation rule: the three files are already
        // in `files`, and this entry is what makes them reachable — a record stack opens exactly
        // what `record_extents` names, so bytes this list omits answer no drill-down and bytes it
        // names but that are absent refuse the open (records §7's fail-closed rule).
        manifest.record_extents.extend(completed.record_extent);
        // The entity→term transpose's extent, under the same two-obligation rule and for the
        // sharper of the two reasons: a list this manifest omits leaves the flushed entities'
        // labels unknown, which serves a drill-down without them (harmless) *and* leaves the join
        // rule's label arm with nothing to compare against (a re-label accepted through a second
        // view's row). The live generation composes it below.
        manifest
            .entity_terms_extents
            .push(completed.entity_terms_extent.clone());
        // The text layers, under the same two-obligation rule: the files are already digested in
        // `files`, and this entry is what makes them reachable to a reopen. The live generation
        // composes them below — a published layer no live reader holds answers no `match` until the
        // next fold, which is the defect the record blob's own composition was missing.
        manifest
            .text_extents
            .extend(completed.text_extents.iter().cloned());
        write_deny_state(&mut manifest, &live.overlay);
        write_vocabulary_extensions(
            &mut manifest,
            &live.vocabularies,
            &live.bundle.manifest.vocabularies,
        );
        *mark = self
            .health
            .flush_lap(crate::flush::FlushStage::Manifest, *mark);

        // **The commit point, and it is still the manifest** — only the thread moved. A failure
        // here discards the flush: its files become orphans nothing references, the buffer is
        // retained, the next tick re-plans. The same posture as every other flush failure, and
        // the reason the write precedes the swap.
        //
        // And therefore the publication seam: the segment's files are on disc, the WAL still holds
        // every row they carry, and nothing durable names them until this write returns
        // (correctness-suite §12.3).
        self.pause_point(PauseSiteArg::BeforeManifestPublish);
        if let Err(e) = self.commit_side_manifest(
            &partition_data.manifest,
            &self.prefix_dir(&live),
            &completed.partition,
            manifest_n,
            &mut manifest,
            &self.containment_extents,
            &self.tile_index_extents,
            &self.row_column_extents,
            &self.shape_rows_extents,
            &self.shape_held_extents,
            &[],
        ) {
            self.health.flush_failures.fetch_add(1, Ordering::Relaxed);
            tracing::error!(
                error = %e,
                "ALARM: a completed flush's side-manifest could not be committed; its files are \
                 orphans, the buffer is retained, and the next tick will re-plan"
            );
            return false;
        }
        *mark = self
            .health
            .flush_lap(crate::flush::FlushStage::Commit, *mark);

        // Stamped with the flush's own incarnation for `Manifest::with_scoped_columns`, which
        // publishes a pair only where it is the live one (decision 0115).
        let scoped_columns: Vec<(String, String, tessera_types::view::ViewIncarnation)> = completed
            .scoped_columns
            .iter()
            .map(|(column, view)| (column.clone(), view.clone(), completed.incarnation))
            .collect();
        let published = tessera_store::read::PublishedManifest {
            manifest,
            n: manifest_n,
        };
        // **A values-only publication substitutes the manifest and leaves the row space alone**
        // (`ingest.md` §1.4). It wrote no segment, so there is nothing to rebase and nothing that
        // could fail to; what it publishes is the value extents its manifest now names.
        let (seg_id, shape_pieces, tier, tier_tally, next_bundle) = match completed.segment {
            None => {
                let bundle = match live.bundle.with_manifest(&completed.partition, published) {
                    Ok(bundle) => bundle,
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "discarding a completed values-only flush whose partition this bundle \
                             no longer carries"
                        );
                        return false;
                    }
                };
                (None, Vec::new(), None, None, bundle)
            }
            Some(segment) => {
                let seg_id = segment.segment.seg_id.clone();
                let bundle = match live.bundle.with_segment(
                    &completed.partition,
                    &completed.view,
                    segment.segment,
                    segment.extent,
                    published,
                ) {
                    Ok(bundle) => bundle,
                    Err(e) => {
                        // The row space moved under this flush — another publication landed
                        // between the plan and here. Discarded, not forced: forcing would put the
                        // segment at a `row_base` that is no longer the end of row space, aliasing
                        // rows.
                        tracing::warn!(
                            error = %e,
                            "discarding a completed flush that no longer rebases"
                        );
                        return false;
                    }
                };
                (
                    Some(seg_id),
                    segment.shape_pieces,
                    Some(segment.tier),
                    Some(segment.tier_tally),
                    bundle,
                )
            }
        };
        // **And the family's own list gains the view this flush wrote a base for**
        // (`views.md` §5). `scoped_scalars[..].views` names the views that *have* a column, so a
        // view that has just acquired one has to enter it — a client reading the list would
        // otherwise conclude the column it is being served does not exist, and the next restart's
        // opener would not open it at all.
        let next_bundle = if scoped_columns.is_empty() {
            next_bundle
        } else {
            let manifest = next_bundle.manifest.with_scoped_columns(&scoped_columns);
            next_bundle.with_views(manifest)
        };
        *mark = self
            .health
            .flush_lap(crate::flush::FlushStage::WithSegment, *mark);

        // **The segment's shape memberships go into the held forms below, with the stored
        // levels' rows** (`polygon-membership.md` §6.3). The pool resolved them against the levels
        // as held when the flush was planned; a publication into a shape layer since then rebuilt
        // that level, and rows resolved over the old shapes would extend a form that no longer
        // describes them. So a piece is taken where its level is the one now held, and the
        // segment is resolved again against the current level where the two differ — one segment,
        // on this thread, in the window a shape publication and a flush overlap.
        *mark = self
            .health
            .flush_lap(crate::flush::FlushStage::ShapesInstall, *mark);

        // **Exactly what was consumed, from the then-current buffer.** O(buffered) on this thread,
        // which is the term the deny-ack memo measured as dominant at 1 M buffered (165 ms p50);
        // one such stall lands ahead of the deny lane per tick, and §10 records it as a cost this
        // design adds rather than one it avoids.
        let mut buffer = (*live.buffer).clone();
        for entity in &completed.consumed {
            // **By (entity, view), never by entity.** A flush consumes one view's rows; an entity
            // that also holds a row awaiting flush in another view keeps it, and removing the
            // entity outright would lose a row that is in no segment and no buffer (`views.md`
            // §4).
            buffer.remove_in_view(*entity, &completed.view);
        }
        // The fills this flush's plan consumed, on the same rule. **Every fill it looked at**,
        // not only the ones it wrote: a fill a restart re-buffered after its own flush writes
        // nothing and must still leave the buffer, or it pins the log at its `ValuesBatch` record
        // for ever (`ingest.md` §1.4).
        for entity in &completed.filled {
            buffer.remove_fill(*entity);
        }
        for (entity, owner_view) in &completed.filled_scoped {
            buffer.remove_scoped_fill(*entity, owner_view);
        }
        // **The gauge follows the buffer here too.** A flush is the other place occupancy changes,
        // and until it was stated here the figure only ever came down at the next apply — so a
        // node that flushed and then took no ingest reported a backlog it had already written, and
        // `/control/ingest`'s occupancy bound was measured against it.
        self.health
            .buffered_items
            .store(buffer.len(), Ordering::SeqCst);
        *mark = self
            .health
            .flush_lap(crate::flush::FlushStage::BufferRebase, *mark);

        let watermark = next_bundle
            .partitions
            .get(&completed.partition)
            .map(|p| p.manifest.watermark)
            .unwrap_or(live.watermark);
        let segments_version = live.segments_version + 1;
        let mut delta_postings = live.delta_postings.clone();
        delta_postings.extend(tier);

        // **Rebuilt against the segment this flush just added**, which is what gives a suppressed
        // or deleted item its place in the mask the moment it acquires a row: until now it was
        // buffered, had no row, and so appeared in no mask at all.
        let denied = Arc::new(crate::compose::derive_denied(&live.overlay, &next_bundle));
        *mark = self
            .health
            .flush_lap(crate::flush::FlushStage::Denied, *mark);

        // **And every held row form of the view gains this segment's rows, before the swap.** A
        // form covers the whole row space, so a segment nothing added to it would leave every
        // artifact one segment short — a member ingested into an artifact counting for nobody
        // until the next fold, which under the nightly gate is hours
        // (`docs/evidence/memos/2026-09-03-post-flush-artifact-frames.md`). A stored level takes
        // one `project_extents_from` per artifact over the entities inside this extent's own
        // range; a spatial level takes the segment's resolution the pool produced above; both on
        // this thread, against the whole-level projection the alternative puts on the next
        // request.
        //
        // A values-only publication added no rows, so it extends no form: `seg_id` is `None` and
        // the row space it publishes is the one it found (`ingest.md` §1.4).
        if let (Some(previous), Some(space)) = (
            view_row_space(&live.bundle, &completed.partition, &completed.view),
            view_row_space(&next_bundle, &completed.partition, &completed.view),
        ) {
            let segment = seg_id.as_ref().and_then(|seg_id| {
                next_bundle
                    .partitions
                    .get(&completed.partition)
                    .and_then(|p| p.views.get(&completed.view))
                    .and_then(|v| v.segments.iter().find(|s| &s.seg_id == seg_id))
                    .map(|s| s.as_ref())
            });
            self.live.with_artifacts(|store| {
                let rows_of = |layer: &str, level: u32| {
                    self.segment_rows_of(
                        &completed.view,
                        layer,
                        level,
                        segment,
                        &shape_pieces,
                        store,
                    )
                };
                self.artifact_projections.extend_flushed(
                    &live.prefix,
                    &completed.view,
                    store,
                    previous,
                    space,
                    segments_version,
                    &rows_of,
                )
            });
        }
        *mark = self
            .health
            .flush_lap(crate::flush::FlushStage::Artifacts, *mark);

        let next = Arc::new(Generation {
            prefix: live.prefix.clone(),
            vocabularies: Arc::clone(&live.vocabularies),
            // The live columns with this flush's extents composed on — the whole of what makes an
            // entity ingested since the build answer a filter on its own value.
            filter_columns,
            // **A flush changes which entities carry a value, not which values exist**, so this is
            // carried rather than rebuilt — the sort a rebuild pays is measured in tens of seconds
            // at 10⁷ values (§6.1). The values a flush's *rows* minted are already in the side map:
            // they were put there at the commit window that minted them, not here.
            suggest: Arc::clone(&live.suggest),
            segments_version,
            watermark,
            bundle: next_bundle,
            dict: completed.dict,
            postings: Arc::clone(&live.postings),
            fragments: Arc::clone(&live.fragments),
            external_index: Arc::clone(&live.external_index),
            delta_postings,
            overlay_version: live.overlay_version,
            overlay: Arc::clone(&live.overlay),
            buffer: Arc::new(buffer),
            denied,
        });
        // **Armed before the swap, and that ordering is the mechanism** (decision 0044 D1; review
        // finding F5). A request landing between the swap and the pool task's first insert must
        // find the flag set, or it takes rung 3 of the ladder as a *build* — the measured 1 277 ms
        // rebuild after a merge — where the whole design is that it be shed with a 429 for the
        // bounded duration of the refresh instead.
        // The claim names the generation it is for, so a pass that is superseded mid-flight
        // releases nothing when it ends — see `refresh::clear_if_current`.
        self.refresh
            .in_flight
            .store(segments_version, Ordering::SeqCst);
        let _published = self.publish_arc(Arc::clone(&next), started);
        self.refresh.spawn(next);

        // A flush supersedes geometry, so it prunes exactly as any other geometry publication
        // does: one swap, one `segments_version` bump, one retention pass. The superseded
        // generation itself is held by nothing but the requests already in flight against it.
        self.row_projection_cache
            .prune_generations_below(segments_version.saturating_sub(KEEP_SUPERSEDED_GENERATIONS));
        self.prune_region_cache(segments_version);
        self.health.flushes.fetch_add(1, Ordering::Relaxed);
        self.health
            .flush_rows_published
            .fetch_add(completed.consumed.len() as u64, Ordering::Relaxed);
        // The drain sample the buffer-occupancy 429 is derived from (ingest §4.2).
        self.health.record_flush_published(completed.consumed.len());
        if let Some(tally) = tier_tally {
            self.health.record_tier_fragmentation(tally);
        }
        *mark = self.health.flush_lap(crate::flush::FlushStage::Swap, *mark);

        self.rotate_wal();
        *mark = self
            .health
            .flush_lap(crate::flush::FlushStage::Rotate, *mark);
        // **The superseded generation is freed here, under a stage, rather than at the return.**
        // `live` is its last reference once the swap has happened (a request in flight holds its
        // own, and then the free lands on that thread instead), and its buffer holds every row
        // that was buffered before the swap: an O(buffered) free on this thread, beside the
        // O(buffered) clone `BufferRebase` measures. Per row published on medcpt-1m the drop read
        // 0.9 µs against the clone's 0.24; per item the two are close, the clone copying the
        // buffer minus the consumed ids and the drop freeing the whole of it
        // (`probes/2026-09-05-flush-attribution/`).
        drop(live);
        drop(completed.consumed);
        self.health
            .flush_lap(crate::flush::FlushStage::DropSuperseded, *mark);
        true
    }

    /// Reclaim what the publication just made redundant — **after** the generation swap and never
    /// before it.
    ///
    /// ```text
    /// generation swap                             ← the publication event, already done above
    /// rotation: snapshot written, then reclaim    ← §7.2, snapshot before any deletion
    /// ```
    ///
    /// **The reclaim bound is the buffer's oldest surviving row.** Rows acked *during* the flush
    /// were appended after its snapshot point, were never consumed, and carry entity ids at or
    /// above the new watermark; reclaiming past them would delete them and recovery would then
    /// reconstruct them from nothing — acked ingest, silently lost at the next restart.
    /// `IngestBuffer::oldest_wal_pos` answers it from the post-publication buffer — the rows that
    /// still have no geometry — and refuses (`None`) if any of them does not know its own
    /// position, which reclaims nothing rather than guessing. With an empty buffer the whole
    /// durable prefix is reclaimable.
    ///
    /// (A `Flush{n, wal_pos}` WAL record used to be appended here first. It was write-only —
    /// recovery reconstructs the buffer by the has-a-row predicate and this function computes its
    /// own bound — and was deleted with `WAL_VERSION` 4 rather than carried as archaeology.)
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
    fn rotate_wal(&mut self) {
        if self.wal.is_poisoned() {
            return;
        }
        if !self.may_publish() {
            tracing::warn!(
                "this node's overlay has diverged from its durable WAL, so it rotates nothing; \
                 the log grows until an operator restarts it"
            );
            return;
        }

        let generation = self.generation.load();
        // A stepped-down node reclaims nothing (owner-ruled 2026-08-04, with the ingest and
        // plan gates): its WAL members are the only recovery material for whatever the
        // step-down shadowed, and freezing reclamation is the fail-closed direction while an
        // operator repairs the damaged newest manifest.
        if generation
            .bundle
            .partitions
            .values()
            .any(|p| p.stepped_down())
        {
            tracing::warn!(
                "a partition is stepped down, so this node rotates nothing; the log grows until \
                 the damaged newest manifest is repaired"
            );
            return;
        }
        let reclaim_below = match generation.buffer.oldest_wal_pos() {
            // Nothing buffered: every ingest row has geometry, so everything below the current
            // position — the whole durable prefix — is reclaimable.
            None => self.wal.position(),
            Some(Some(oldest)) => oldest,
            // A buffered row of unknown position pins the log. Fail-safe and loud by construction:
            // the sequence grows, which is visible, rather than a record vanishing, which is not.
            Some(None) => 0,
        };
        // **The oldest artifact publication pins the log too, and today that means from the first
        // publication onwards.** A membership has no home outside the WAL — segments carry rows and
        // postings, manifests carry the registry, and neither carries a Roaring bitmap of who
        // belongs to a cluster — so reclaiming a member holding one destroys the only copy, leaving
        // the artifact registered, still addressable by a `tessera_id` a caller holds, and served
        // as absent. ⊘ Where membership lives on disk is the owner's open decision; until it lands
        // this is the fail-closed direction, and a log that grows is noticed where a membership
        // that vanishes is not.
        let reclaim_below = match self.live.artifacts_oldest_wal_pos() {
            Some(oldest) => reclaim_below.min(oldest),
            None => reclaim_below,
        };

        let snapshot = generation.overlay.snapshot();
        match self.wal.rotate(&snapshot, reclaim_below) {
            Ok(deleted) => {
                // Post-rotation position, so the next growth check counts only appends made
                // after the snapshot this rotation just wrote.
                self.wal_position_at_last_rotation = self.wal.position();
                if !deleted.is_empty() {
                    tracing::info!(
                        members = ?self.wal.members(),
                        reclaimed = ?deleted,
                        "WAL members reclaimed below the oldest unconsumed row"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "the WAL did not rotate; the log grows until it does");
            }
        }
    }

    /// Rotate at the tick when the log has grown and no flush publication is coming to do it —
    /// **the deny-only regime's rotation** (owner-ruled 2026-08-04; write-path §4.5).
    ///
    /// Rotation used to run only inside `publish_flush`, so a node that took denies without ever
    /// flushing — a loaded bundle with no live ingest, the natural state after a bulk load —
    /// sealed nothing, snapshotted nothing and reclaimed nothing: an unbounded log on the one
    /// lane that structurally cannot be shed, replayed in full at every restart. The tick
    /// already fires every `flush_max_age_secs` regardless of buffer contents, so it is the
    /// site.
    ///
    /// **Gated on growth**, so an idle node rotates nothing: a rotation writes an O(overlay)
    /// snapshot and a new member, and doing that per tick on a quiet deployment would be churn
    /// for no reclaim. The position check is exact — append order is sequence order — and
    /// `rotate_wal` re-checks every safety gate (poisoned, diverged, stepped-down) itself.
    /// Safety is the flush-publication rotation's own argument, unchanged: the snapshot
    /// re-states the whole overlay before anything is deleted, and the reclaim bound is the
    /// oldest surviving buffered row, so nothing acked is lost at any crash point.
    fn rotate_if_grown(&mut self) {
        if self.wal.position() == self.wal_position_at_last_rotation {
            return;
        }
        self.rotate_wal();
    }

    /// Read the WAL's size and its rotation bound into [`ExecutorHealth::wal_gauge`], at most once
    /// per tick period.
    ///
    /// **A read of state this thread already owns, and nothing else.** It rotates nothing,
    /// compares nothing against a limit and returns no decision: `wal_hard_limit_bytes` is a
    /// startup relation and what a node should do at a runtime ceiling is undecided
    /// (`Wal::disc_bytes`). Nothing in the process reads the gauge it writes; `/control/status`
    /// and the tests are its only consumers.
    ///
    /// **The cost is O(members): two `stat`s per surviving member**, the log file and its `.sync`
    /// sidecar, plus one artifact-store lock and two O(1) reads. Steady-state retention is two
    /// members, and the count is published beside the bytes so a reader can see when the walk
    /// stopped being cheap.
    ///
    /// **The rate limit is what keeps that cost bounded, because the member count is not.** Under
    /// a `growth` or `fill` pin nothing below the pin is reclaimed, so a member accumulates per
    /// rotation for as long as the fold that would release it is refused — the condition this
    /// gauge exists to make visible. The walk therefore gets dearer as the problem gets worse.
    /// The tick it sits on is not a period either: `rows_due` holds continuously while the buffer
    /// is at `flush_max_items`, so a loader the flush cannot keep up with ticks at
    /// `FLUSH_COMPLETION_POLL`, 50 times a second. The clock below bounds the walk to one per
    /// `flush_max_age_secs` whatever the tick does, which is the freshness [`WalGauge`] already
    /// promises.
    ///
    /// Called from both the flushing and the flush-skipped path, so a node whose flush is stalled
    /// still reports the log growing under it, and once at [`Executor::run`]'s entry so a restarted
    /// node does not report an unsampled zero for its first period.
    fn sample_wal_gauge(&mut self) {
        let period = std::time::Duration::from_secs(self.flush_max_age_secs);
        if let Some(last) = self.last_wal_sample {
            if last.elapsed() < period {
                return;
            }
        }
        // Before the walk, so a slow walk shortens the next interval rather than pushing it out.
        self.last_wal_sample = Some(std::time::Instant::now());
        self.wal_samples += 1;
        let position = self.wal.position();
        let pin = self.live.with_artifacts(|store| store.wal_pin());
        self.health.record_wal_gauge(WalGauge {
            members: self.wal.member_count(),
            bytes: self.wal.disc_bytes(),
            position,
            pin,
            pin_span_bytes: pin.map_or(0, |(_, pos)| position.saturating_sub(pos)),
            samples: self.wal_samples,
        });
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
    ///
    /// # The one swap, and everything that rides it
    ///
    /// Compaction §4 step 6: prefix, `segments_version`, watermark, bundle, dictionary and tier
    /// list always; and, when the publication carries a `PrefixRotation`, the base postings, the
    /// fragment cache and the identity it keys, the external-id sidecar, and the retirement of the
    /// executed deletions — all through the single `store` below. Not a sequence of stores that a
    /// request could land between: a request loads one pointer and gets a geometry, a term index,
    /// a fragment identity and a sidecar that agree.
    fn publish_geometry(
        &mut self,
        publication: GeometryPublication,
    ) -> std::result::Result<(), GeometryRefused> {
        let GeometryPublication {
            prefix,
            segments_version,
            watermark,
            bundle,
            dict,
            delta_postings,
            rotation,
        } = publication;
        let started = std::time::Instant::now();
        let previous = self.generation.load_full();
        check_publishable(&previous, &prefix, segments_version, watermark)?;

        // **Rule F, in the fold's own swap and nowhere else** (write-path §5.4). An entry
        // withdrawn while the old geometry is still live re-exposes the item for the width of that
        // window, so the overlay is cloned, retired against, and published — never mutated in
        // place on a shared `Arc`, which the read path is holding.
        //
        // `overlay_version` moves **only** when something actually retired. A geometry-only swap
        // that bumped it would falsely signal a change on lifecycle §1.2's *security-state* axis,
        // which §8.5's cache keys read; a retirement that did not bump it would be a real change
        // to that state, invisible to the same keys.
        let (overlay, overlay_version) = match rotation.as_ref().map(|r| &r.retired) {
            Some(retired) if !retired.is_empty() => {
                // **The live external-id map loses the retired bindings first** — see
                // `LiveState::forget_established` for why before the retirement rather than after,
                // and for what a binding left standing costs (a lawful re-ingest, refused 409,
                // permanently).
                let forgotten = self.live.forget_established(retired);
                let mut overlay = (*previous.overlay).clone();
                let count = overlay.retire(retired);
                tracing::info!(
                    retired = count,
                    forgotten_external_ids = forgotten,
                    prefix = %prefix,
                    "Rule F: executed deletions retired in the fold's own publication"
                );
                (Arc::new(overlay), previous.overlay_version + 1)
            }
            _ => (Arc::clone(&previous.overlay), previous.overlay_version),
        };

        // Rebuilt against the new row space: row ids mean something only within one
        // `segments_version`, so a geometry publication invalidates every row in the old mask
        // (`derive_denied`). Derived from the **retired** overlay, not the previous one, or the
        // retired entities would keep their rows in the mask over a row space that no longer holds
        // them. Taken before `bundle` moves into the generation.
        let denied = Arc::new(crate::compose::derive_denied(&overlay, &bundle));

        let next = Generation {
            prefix,
            vocabularies: Arc::clone(&previous.vocabularies),
            // **A rotation carries the new prefix's own columns**, opened over it by
            // `open_rotation`; every other publication stays within the live prefix and carries
            // the live ones. Cloning the previous generation's across a rotation would serve the
            // superseded prefix's mappings — pre-fold values, the blanking missing, out of files
            // the reclamation is about to unlink (`filter-index.md` §6.2).
            filter_columns: rotation.as_ref().map_or_else(
                || Arc::clone(&previous.filter_columns),
                |r| Arc::clone(&r.filter_columns),
            ),
            segments_version,
            watermark,
            bundle,
            dict,
            postings: rotation.as_ref().map_or_else(
                || Arc::clone(&previous.postings),
                |r| Arc::clone(&r.postings),
            ),
            fragments: rotation.as_ref().map_or_else(
                || Arc::clone(&previous.fragments),
                |r| Arc::clone(&r.fragments),
            ),
            external_index: rotation.as_ref().map_or_else(
                || Arc::clone(&previous.external_index),
                |r| Arc::clone(&r.external_index),
            ),
            // **Carried across a rotation too**, and this is not the oversight it looks like: a
            // fold retires entities, never values — a code is pinned forever (§3.4) and no
            // publication removes one from a vocabulary — so the value set the index is over is the
            // set the new prefix carries. What a fold *can* change is a title, and it does so
            // through a new bundle, which is a new `Engine::open` and therefore a fresh build.
            suggest: Arc::clone(&previous.suggest),
            delta_postings,
            overlay_version,
            overlay,
            buffer: Arc::clone(&previous.buffer),
            denied,
        };
        // **Listed before the swap, deleted after it** (compaction §8). At this instant every
        // persisted fragment is under the identity about to be superseded, so the listing *is* the
        // set §8 names — which is not selectable by name, since a cache entry is a SHA-256 over the
        // identity and a hash does not invert. Taking it here and deleting below closes both
        // hazards at once: a listing taken before the swap can never name an entry a request wrote
        // after it, and nothing is deleted at all if the swap does not happen.
        let superseded = rotation
            .as_ref()
            .map(|_| previous.fragments.superseded_entries())
            .unwrap_or_default();

        let next = Arc::new(next);
        let _published = self.publish_arc(Arc::clone(&next), started);

        // **A rotation refreshes after the swap and does not arm the shed** (decision 0053). The
        // pass still runs, most-recently-used first, so a resident session's projection is rebuilt
        // proactively rather than on its next request — but it is no longer load-bearing, and
        // `refresh.in_flight` is deliberately not set: shed only while the refresh pass is shorter
        // than the rebuild it would save, and a fold inverts that by two orders. After a fold a
        // missing projection is an ordinary cache miss. The rule is stated at
        // `RefreshDeps::in_flight`, which is where a future publication kind will look for it.
        if rotation.is_some() {
            self.refresh.spawn(next);
        }

        if !superseded.is_empty() {
            let swept = FragmentCache::sweep(&superseded);
            tracing::info!(
                swept,
                named = superseded.len(),
                "the fold's identity rotated; the persisted fragments under the superseded one are \
                 unreachable and have been reclaimed"
            );
        }

        // The retention pass, at the swap rather than at a reclaim — see
        // `RowProjectionCache::prune_generations_below` for why depth 1 rather than depth 0, which
        // would delete the input to the very patch it exists to enable.
        self.row_projection_cache
            .prune_generations_below(segments_version.saturating_sub(KEEP_SUPERSEDED_GENERATIONS));
        self.prune_region_cache(segments_version);
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
        self.publish_arc(Arc::new(next), started)
    }

    /// [`Self::publish`] over a generation the caller already holds by `Arc` — a geometry
    /// publication needs the same value afterwards, to hand the background refresh.
    fn publish_arc(&self, next: Arc<Generation>, started: std::time::Instant) -> Published {
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
        self.generation.store(next);
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

    /// Reach an armed pause site, if any. Fault-injection builds only; a no-op otherwise.
    ///
    /// The sites are `faults::PauseSite`'s, which is where each is argued: two discriminate the
    /// ack contract's ordering, and three park this thread at the write path's publication seams
    /// for the correctness suite's crash modifier. Every call site holds no lock — a pause inside
    /// one would wedge this thread against its own waiters.
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

/// The pause-site argument, so the executor's call sites read the same in both builds.
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
    BeforeManifestPublish,
    BeforeCurrentFlip,
    BeforeMergePublish,
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

    /// **The buffer-occupancy figure is the time to the next tick plus the observed drain**
    /// (ingest §4.2). Before any flush has published, the drain is unobserved and the answer is
    /// the time to the tick alone; after one, a million rows at the observed 15 µs a row adds
    /// 15 s to it; and the ceiling holds where the arithmetic would exceed it.
    ///
    /// **Mutation:** a fixed constant here fails every arm; dropping the tick term fails the
    /// first; dropping the drain term fails the second.
    #[test]
    fn buffer_retry_after_is_the_tick_plus_the_observed_drain() {
        let now = std::time::Instant::now();
        let health = ExecutorHealth::with_base(now - std::time::Duration::from_secs(3_600));
        health.set_flush_period_secs(90);

        // Thirty seconds into a ninety-second period, nothing published yet.
        health.mark_tick(now - std::time::Duration::from_secs(30));
        let stats = health.stats();
        assert_eq!(stats.flush_nanos_per_row_ewma, 0);
        let secs = estimate_buffer_retry_after_s(&stats, 1_000_000);
        assert!((59..=60).contains(&secs), "the tick alone: got {secs}");

        // A flush of a million rows that took fifteen seconds: 15 µs a row, observed.
        health.mark_flush_started(now - std::time::Duration::from_secs(15));
        health.record_flush_published(1_000_000);
        health.mark_tick(now);
        let stats = health.stats();
        assert!(
            (14_000..=16_000).contains(&stats.flush_nanos_per_row_ewma),
            "per-row drain observed: got {} ns",
            stats.flush_nanos_per_row_ewma
        );
        let secs = estimate_buffer_retry_after_s(&stats, 1_000_000);
        assert!((104..=106).contains(&secs), "tick plus drain: got {secs}");

        // A hundred million rows at that rate is twenty-five minutes; the ceiling holds.
        assert_eq!(
            estimate_buffer_retry_after_s(&stats, 100_000_000),
            RETRY_AFTER_MAX_SECS
        );
        // Nothing buffered and the tick due: the floor, never zero.
        health.mark_tick(now - std::time::Duration::from_secs(90));
        assert_eq!(
            estimate_buffer_retry_after_s(&health.stats(), 0),
            RETRY_AFTER_MIN_SECS
        );
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
    /// `suppress → unsuppress` and shrink only at a fold — so a level-triggered check emits a
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
/// §2), tested where they are decided rather than through a second view no build produces.
#[cfg(test)]
mod dispatch_rules_tests {
    use super::*;
    use tessera_lifecycle::BufferedItem;

    fn plan_from(oldest: u64) -> crate::flush::FlushPlan {
        let item = BufferedItem {
            terms: Vec::new(),
            view: "s".to_string(),
            join: false,
            x: 0.5,
            y: 0.5,
            scalars: Vec::new(),
            scoped: Vec::new(),
            external_id: None,
            wal_pos: None,
        };
        crate::flush::FlushPlan {
            items: vec![(EntityId::new(oldest), item)],
            fills: Vec::new(),
            consumed_fills: Vec::new(),
            consumed_scoped_fills: Vec::new(),
        }
    }

    /// **Obligation 9.** Every context a dispatch builds shares `next_n`, so only one can commit
    /// its side-manifest. The one sent is the view whose oldest waiting row is oldest — not the
    /// first by name, which is what `views_of`'s lexicographic sort would give and which would
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
        let (view, plan) = plan_to_dispatch(plans).expect("one of three");
        assert_eq!(view, "s1", "oldest row wins, not lowest view id");
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

/// The segments a fold wrote, opened from the prefix it wrote them into — what its artifact pass
/// resolves every spatial level against (`polygon-membership.md` §6.3). A segment that will not
/// open is skipped and said so; the level then resolves it at the flip's warm, on the executor,
/// which is the same answer later.
fn fold_segments(
    prefix_dir: &std::path::Path,
    partition: &str,
    segments: &[tessera_store::manifest::SegmentDescriptor],
) -> Vec<(String, tessera_store::read::SegmentData)> {
    let mut out = Vec::with_capacity(segments.len());
    for descriptor in segments {
        let partition_dir = prefix_dir.join("partitions").join(partition);
        let dir = tessera_store::view_path(&partition_dir, &descriptor.view)
            .join("segments")
            .join(&descriptor.seg_id);
        let morton = tessera_store::read::MortonSlice::load(&dir.join("morton.u32"));
        let columns = tessera_store::read::ColumnsRef::load(&dir.join("columns.arrow"));
        match (morton, columns) {
            (Ok(morton), Ok(columns)) => out.push((
                descriptor.view.clone(),
                tessera_store::read::SegmentData {
                    seg_id: descriptor.seg_id.clone(),
                    row_count: descriptor.row_count,
                    morton,
                    columns,
                },
            )),
            (Err(error), _) | (_, Err(error)) => tracing::warn!(
                view = %descriptor.view,
                seg_id = %descriptor.seg_id,
                %error,
                "the fold could not reopen a segment it just wrote; its spatial memberships are \
                 resolved at the flip instead"
            ),
        }
    }
    out
}
