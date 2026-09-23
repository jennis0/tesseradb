use super::*;

// =================================================================================================
// Posture and counters
// =================================================================================================

/// What the write executor is able to do; `readyz` reads it. Composed from the thread's own
/// state, which latches, and the WAL's, which is mirrored and can recover
/// ([`ExecutorHealth::posture`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum ExecutorPosture {
    /// `Engine::start_write_executor` was never called. Writes are refused; reads are unaffected.
    NotStarted = 0,
    /// Executing normally.
    Running = 1,
    /// The WAL refuses every further operation. The executor is alive and still applies denies.
    /// A sync failure is repaired in process ([`Executor::recover_wal`]); a torn append is not, and
    /// the WAL then stays poisoned for the life of the process.
    WalPoisoned = 2,
    /// The thread is gone: it panicked, or every handle was dropped and it shut down. Both mean
    /// the same thing to a caller — there is nothing left to apply a write to.
    Dead = 3,
}

impl ExecutorPosture {
    /// The wire spelling for `/control/status`. Not `Debug`: renaming a variant must not change
    /// what operators read.
    pub fn as_str(self) -> &'static str {
        match self {
            ExecutorPosture::NotStarted => "not-started",
            ExecutorPosture::Running => "running",
            ExecutorPosture::WalPoisoned => "wal-poisoned",
            ExecutorPosture::Dead => "dead",
        }
    }

    pub(in crate::write) fn from_u8(v: u8) -> Self {
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
    /// The thread's own state, `NotStarted` to `Running` to `Dead`. Advanced with `fetch_max`, so a
    /// thread that panics as it is spawned is not overwritten by the parent's `Running`.
    pub(in crate::write) lifecycle: AtomicU8,
    /// Whether the WAL refuses operations now. Mirrored from the WAL in both directions, because a
    /// discard of the undurable region ends the condition. Written only by the executor thread.
    pub(in crate::write) wal_poisoned: AtomicBool,
    /// Times the executor discarded an undurable WAL region and returned to service. The posture
    /// recovers, so this is the figure to alarm on: repeated recoveries are a failing disk.
    pub(in crate::write) wal_recoveries: AtomicU64,
    pub(in crate::write) work_submitted: AtomicU64,
    pub(in crate::write) deny_submitted: AtomicU64,
    /// Flush ticks fired since the executor started.
    pub(crate) ticks: AtomicU64,
    /// A `POST /control/flush` awaiting the tick it pulls forward. A flag: one tick answers every
    /// request made before it. Written under [`Self::publication`] so a request and the publication
    /// number it is answered with cannot straddle a tick.
    pub(crate) flush_requested: AtomicBool,
    /// The publication counter a client waits on. `completed` moves when a cycle (a tick and the
    /// flush it dispatched) has published, including a tick with nothing to publish. A cycle whose
    /// gates were shut or whose flush failed stays open, so the number is reached only by the retry
    /// that succeeds.
    pub(crate) publication: Mutex<PublicationCycle>,
    /// A tick dispatched one view's plan and left another view's rows buffered. The cycle stays open
    /// until a tick dispatches with nothing left over.
    pub(crate) deferred_plans: AtomicBool,
    /// When the last cycle failed to publish, as a marker (see [`Self::set_marker`]); `0` if none has
    /// since the last success. Floors the retry at [`FAILED_CYCLE_RETRY`].
    pub(in crate::write) failed_cycle_nanos: AtomicU64,
    /// When a publication refusal was last logged, so a held-open cycle logs once a period.
    pub(in crate::write) refusal_logged_nanos: AtomicU64,
    /// Whether a flush is executing on the pool. A tick that finds it set is skipped, never queued:
    /// two concurrent flushes would consume the same buffer range.
    pub(crate) flush_in_flight: Arc<AtomicBool>,
    /// A completed fold, or merge, has been handed back and not yet published. Shared with the
    /// executor's [`Background`](super::executor::Background) so a test can wait on a held
    /// publication.
    pub(crate) fold_completed_pending: Arc<AtomicBool>,
    pub(crate) merge_completed_pending: Arc<AtomicBool>,
    /// A merge, coalesce or fold is running, or a flush or a coalesce has been handed back and
    /// not yet published. Shared as above, so a test can wait for maintenance to go idle.
    pub(crate) fold_in_flight: Arc<AtomicBool>,
    pub(crate) merge_in_flight: Arc<AtomicBool>,
    pub(crate) coalesce_in_flight: Arc<AtomicBool>,
    pub(crate) coalesce_completed_pending: Arc<AtomicBool>,
    pub(crate) flush_completed_pending: Arc<AtomicBool>,
    /// The overlay holds dispositions the durable WAL does not, after [`Executor::recover_wal`]
    /// discarded an undurable region. Latches until restart: the node publishes no manifest and
    /// rotates no WAL, because either would make a never-acked deny permanent.
    pub(crate) overlay_diverged: AtomicBool,
    /// `CURRENT` names a prefix this process is not serving: a fold flipped it and could not swap.
    /// Latches until restart. Publishing from here would write acked rows under a prefix no restart
    /// reads, and the WAL rotation after it would reclaim the only other copy.
    pub(crate) prefix_diverged: AtomicBool,
    /// Buffer occupancy as of the last apply. What `/control/ingest`'s occupancy bound is checked
    /// against.
    pub(crate) buffered_items: AtomicUsize,
    /// Rows that would acquire geometry at the last tick, joins included. Zero on a gated node;
    /// growing without bound on one whose flush keeps failing.
    pub(crate) flushable_items: AtomicUsize,
    /// Flushes published since the executor started.
    pub(crate) flushes: AtomicU64,
    /// When the flush now on the pool was dispatched, as a marker; `0` when none is.
    pub(in crate::write) flush_started_nanos: AtomicU64,
    /// Wall nanoseconds per row drained, an EWMA over published flushes from dispatch to
    /// publication. The buffer-occupancy 429 derives `Retry-After` from it. `0` before the first.
    pub(in crate::write) flush_nanos_per_row_ewma: AtomicU64,
    /// The last tick, as a marker; `0` before the first.
    pub(in crate::write) last_tick_nanos: AtomicU64,
    /// The tick period, so a snapshot can say how long until the next tick.
    pub(in crate::write) flush_period_nanos: AtomicU64,
    /// Side-manifests written for deny state alone.
    pub(crate) overlay_publications: AtomicU64,
    /// Ticks that found a flush in flight and skipped. A rising count means the publication period
    /// is longer than the one configured.
    pub(crate) flush_skips: AtomicU64,
    /// Flushes that failed and left the buffer intact for the next tick.
    pub(crate) flush_failures: AtomicU64,
    /// Allocations that found a `SEGMENTS-<n>.json` this executor did not write: evidence of a
    /// second writer over the bundle root.
    pub(crate) foreign_side_manifests: AtomicU64,
    /// Entity-space coalesce publications since the executor started.
    pub(crate) coalesces: AtomicU64,
    /// Coalesces that failed or no longer rebased.
    pub(crate) coalesce_failures: AtomicU64,
    /// Row-space merge publications.
    pub(crate) merges: AtomicU64,
    pub(crate) merge_failures: AtomicU64,
    /// Compaction folds published.
    pub(crate) folds: AtomicU64,
    /// Folds that failed or were discarded. Each leaves orphan files under a prefix `CURRENT` never
    /// named.
    pub(crate) fold_failures: AtomicU64,
    /// A `POST /control/compact` awaiting the tick that dispatches it.
    pub(crate) fold_requested: AtomicBool,
    /// Folds that were asked for and `plan_fold` refused, one counter per gate
    /// ([`crate::compact::NoFold::index`]). A gate that stands is counted at every tick, so the
    /// largest entry is the condition the deployment is in.
    pub(crate) fold_refusals: [AtomicU64; crate::compact::NoFold::GATES.len()],
    /// The last refusal, with the two byte figures of an `insufficient_disc` one.
    pub(crate) last_fold_refusal: Mutex<Option<FoldRefusal>>,
    /// The WAL as the last sample found it. Sampled on the executor thread at a tick, at most once a
    /// period, so a status poll costs the node nothing.
    pub(crate) wal_gauge: Mutex<WalGauge>,
    /// When the last fold ended, however it ended, as a unix second. Written by the fold's thread
    /// on both exits, because the executor never sees a failure inside `execute`.
    pub(crate) fold_ended_unix: AtomicU64,
    /// The last fold's wall clock in seconds.
    pub(crate) last_fold_secs: AtomicU64,
    /// The highest resident set the last fold's pass staircase saw. Sampled at pass boundaries, so
    /// a spike inside a pass is not in it.
    pub(crate) last_fold_rss: AtomicU64,
    /// The last fold's attribute pass IO, in bytes.
    pub(crate) last_fold_attr_read: AtomicU64,
    pub(crate) last_fold_attr_written: AtomicU64,
    /// The last fold's staircase, pass by pass.
    pub(crate) last_fold_passes: Mutex<Vec<crate::compact::PassCost>>,
    /// The last fold's degradation report. The durable copy is the file in `reports/`.
    pub(crate) last_fold_report: Mutex<Vec<tessera_lifecycle::membership::Degradation>>,
    /// A flush has finished on the pool and is holding at the test hook. Always `false` outside
    /// tests.
    pub(crate) flush_holding: AtomicBool,
    /// A fold has finished its passes and is holding at the test hook. Always `false` outside tests.
    pub(crate) fold_holding: AtomicBool,
    /// Nanoseconds spent in the whole apply step (clone, inserts, generation, swap), summed over
    /// both lanes. An ingest apply clones the buffer and dominates it; a deny apply clones only the
    /// overlay. It estimates how long a deny waits behind the work item in flight.
    pub(in crate::write) apply_nanos_total: AtomicU64,
    /// The window close, partitioned by [`WriteStage`]. Written only under `bench-timing`; zeros
    /// mean not measured.
    pub(in crate::write) stage_nanos: [AtomicU64; WriteStage::COUNT],
    /// The flush, partitioned by [`crate::flush::FlushStage`]. Written only under `bench-timing`.
    pub(in crate::write) flush_stage_nanos: [AtomicU64; crate::flush::FlushStage::COUNT],
    /// `execute_flush` returns on the pool, `Ok` or `Err`.
    pub(in crate::write) flush_executions: AtomicU64,
    /// Rows in every `execute_flush` that returned `Ok`.
    pub(in crate::write) flush_rows_executed: AtomicU64,
    /// Rows a publication removed from the buffer, summed over every flush that swapped.
    pub(in crate::write) flush_rows_published: AtomicU64,
    pub(in crate::write) apply_nanos_max: AtomicU64,
    /// Work-lane jobs answered, counted just before each answer is sent. `work_submitted -
    /// work_completed` is the queue depth the 429's `Retry-After` is derived from. Deny-lane jobs
    /// are not counted: their queue is unbounded.
    pub(in crate::write) work_completed: AtomicU64,
    /// An EWMA (weight 1/8) of one work-lane job's whole service time. A cumulative mean would keep
    /// reporting an old fast regime after the buffer has grown. Written when a job finishes, so see
    /// [`Self::work_started_nanos`].
    pub(in crate::write) work_service_nanos_ewma: AtomicU64,
    /// When the work item now executing started, as a marker; `0` when idle. The EWMA moves only
    /// when a job finishes, so the estimate takes the larger of the two: a caller behind a long job
    /// waits at least as long as it has already run.
    pub(in crate::write) work_started_nanos: AtomicU64,
    /// The origin every marker is measured from.
    pub(in crate::write) base: std::time::Instant,
    /// The overlay depth at which [`Executor::apply_changes`] alarms. `usize::MAX` means no limit.
    pub(in crate::write) overlay_soft_limit: AtomicUsize,
    /// Times the overlay crossed to at or above the soft limit. Crossings, not applies above it:
    /// the depth falls only at a fold, so a level trigger would log on every deny.
    pub(in crate::write) overlay_soft_limit_alarms: AtomicU64,
    /// Whether the overlay is known to be at or above the soft limit.
    pub(in crate::write) overlay_soft_limit_latched: AtomicBool,
    /// The row count at which a commit window closes. Defaults to a real bound
    /// ([`DEFAULT_COMMIT_WINDOW_MAX_ROWS`]): under sustained load the queue never empties, so closing
    /// on an empty queue alone would hold the whole load in memory.
    pub(in crate::write) commit_window_max_rows: AtomicUsize,
    /// What every closed commit window's allocation collected, in entity space. One mutex so a
    /// reader never sees one window's `runs` against another's `baseline`. The deny lane allocates
    /// no ids and contributes nothing.
    pub(in crate::write) fragmentation: Mutex<FragmentationTally>,
    /// Delta tiers as encoded, cumulative over published flushes. Fed at publication, so a
    /// discarded flush's tier does not count.
    pub(in crate::write) tier_fragmentation: Mutex<FragmentationTally>,
    /// Published tiers behind [`Self::tier_fragmentation`].
    pub(in crate::write) fragmentation_tiers: AtomicU64,
    /// Commit windows behind [`Self::fragmentation`].
    pub(in crate::write) fragmentation_windows: AtomicU64,
    /// The executor's WAL counters: a clone of the meter the [`ExecutorWal`] holds.
    pub(in crate::write) wal: Arc<WalMeter>,
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
    /// The per-entry WAL append loop: serialise and write. Three records go in it, and the
    /// `Wal*` stages below say which.
    WalAppend,
    /// `mint_window_codes`: padding every row to the declared schema and resolving every category
    /// value in it to its code, minting the keys the vocabulary does not hold.
    VocabularyMint,
    /// `mint_records`, `derive_records` and `growth_records`: the artifacts the window's rows
    /// named that no artifact holds, the edges that come with them, and the memberships its joins
    /// declared.
    DeriveRecords,
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
    pub const COUNT: usize = 15;
    pub const ALL: [WriteStage; Self::COUNT] = [
        WriteStage::Allocate,
        WriteStage::WalAppend,
        WriteStage::VocabularyMint,
        WriteStage::DeriveRecords,
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
            WriteStage::VocabularyMint => "vocab_mint",
            WriteStage::DeriveRecords => "derive_records",
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

    /// Charges the time since this mark to `slot` and returns a fresh mark. Reads no clock
    /// without `bench-timing`.
    #[inline(always)]
    #[allow(unused_variables)]
    pub(in crate::write) fn lap(self, slot: &AtomicU64) -> StageMark {
        #[cfg(feature = "bench-timing")]
        {
            let now = std::time::Instant::now();
            slot.fetch_add(now.duration_since(self.0).as_nanos() as u64, Ordering::Relaxed);
            StageMark(now)
        }
        #[cfg(not(feature = "bench-timing"))]
        {
            self
        }
    }
}

/// The gates `plan_fold` can refuse on, in the order [`ExecutorStats::fold_refusals_by_gate`]
/// counts them. Literals from a closed list, so no corpus-derived text reaches a status response.
pub const FOLD_GATES: [&str; crate::compact::NoFold::GATES.len()] = crate::compact::NoFold::GATES;

/// A fold that was asked for and `plan_fold` would not plan. `gate` is the
/// [`crate::compact::NoFold`] variant in snake case. For `insufficient_disc`, `need_bytes` is 150%
/// of the live bytes and `had_bytes` is what `statvfs` answered; nothing but a fold reclaims, so
/// that gap does not close on its own.
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
/// `members` is the direct signal: steady state is two, and a growing count is a rotation that
/// is not reclaiming. `pin` separates the two ways a log gets large: `None` with a large `bytes`
/// is fast ingest that the next publication rotates; `Some` with a large `pin_span_bytes` is a
/// log that cannot rotate below that position, and the name says what would release it.
/// `position` counts record bytes across every member ever held, so it is not a size.
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
    /// See [`ExecutorHealth::apply_nanos_total`].
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
    /// Rows that would acquire geometry at the last tick (§3.5), joins included — zero on a gated
    /// node, growing without bound on one whose flush keeps failing.
    pub flushable_items: usize,
    /// Flushes published since the executor started.
    pub flushes: u64,
    /// Side-manifests written for deny state alone.
    pub overlay_publications: u64,
    /// Ticks skipped because a flush was already in flight.
    pub flush_skips: u64,
    /// Flushes that failed and left the buffer intact for the next tick (§10).
    pub flush_failures: u64,
    /// See [`ExecutorHealth::foreign_side_manifests`].
    pub foreign_side_manifests: u64,
    /// Entity-space coalesce publications, and the ones that produced nothing.
    pub coalesces: u64,
    pub coalesce_failures: u64,
    /// Row-space merge publications, and the ones that produced nothing.
    pub merges: u64,
    pub merge_failures: u64,
    /// Compaction folds published, and the ones discarded.
    pub folds: u64,
    pub fold_failures: u64,
    /// Whether a `POST /control/compact` is awaiting the next tick.
    pub fold_requested: bool,
    /// The sum of [`Self::fold_refusals_by_gate`].
    pub fold_refusals: u64,
    /// Refused folds by gate, indexed as [`FOLD_GATES`] names them.
    pub fold_refusals_by_gate: [u64; FOLD_GATES.len()],
    pub last_fold_refusal: Option<FoldRefusal>,
    /// The WAL as the last sample found it — see [`WalGauge`].
    pub wal: WalGauge,
    /// See [`ExecutorHealth::last_fold_secs`] and [`ExecutorHealth::last_fold_rss`].
    pub last_fold_secs: u64,
    pub last_fold_rss: u64,
    pub last_fold_attr_read: u64,
    pub last_fold_attr_written: u64,
    /// Whether a `POST /control/flush` is awaiting the next tick.
    pub flush_requested: bool,
    /// Whether a flush unit is executing on the pool — see [`ExecutorHealth::flush_in_flight`].
    pub flush_in_flight: bool,
    /// Whether a merge is running on the pool or finished and not yet published.
    pub merge_in_flight: bool,
    /// Whether a coalesce is running on the pool or finished and not yet published.
    pub coalesce_in_flight: bool,
    /// See [`ExecutorHealth::overlay_diverged`].
    pub overlay_diverged: bool,
    /// See [`ExecutorHealth::prefix_diverged`].
    pub prefix_diverged: bool,
    /// Successful WAL appends since the executor started.
    pub wal_appends: u64,
    /// Successful WAL fsyncs. `wal_appends / wal_fsyncs` is the mean entries per commit window,
    /// over the ingest and deny windows together; near 1.0 under concurrent load means group commit
    /// is not amortising anything.
    pub wal_fsyncs: u64,
    /// See [`ExecutorHealth::wal_recoveries`].
    pub wal_recoveries: u64,
    /// Work-lane jobs answered. A caller that has its answer is already counted here.
    pub work_completed: u64,
    /// `work_submitted - work_completed`, saturating: the two are read separately, so completed
    /// may briefly exceed submitted.
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
    /// See [`ExecutorHealth::overlay_soft_limit_alarms`].
    pub overlay_soft_limit_alarms: u64,
    /// See [`ExecutorHealth::fragmentation`].
    pub fragmentation: FragmentationTally,
    /// Commit windows behind [`Self::fragmentation`].
    pub fragmentation_windows: u64,
    /// See [`ExecutorHealth::tier_fragmentation`].
    pub tier_fragmentation: FragmentationTally,
    /// Published tiers behind [`Self::tier_fragmentation`].
    pub fragmentation_tiers: u64,
}

impl ExecutorStats {
    /// Total postings over containers touched. `None` before any window has closed.
    pub fn postings_per_container(&self) -> Option<f64> {
        let f = self.fragmentation;
        (f.containers > 0).then(|| f.postings as f64 / f.containers as f64)
    }

    /// Measured mean posting run length over the random baseline at the same density: `1.0` is
    /// fully scattered, larger is better. Both are taken at window scope, so scatter between windows
    /// is invisible to it. `None` before any window has closed.
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
    /// The service figure a `retry_after_s` derivation uses: the larger of the EWMA and the
    /// in-flight job's elapsed time. See [`ExecutorHealth::work_started_nanos`].
    pub fn service_nanos_for_estimate(&self) -> u64 {
        self.work_service_nanos_ewma.max(self.work_in_flight_nanos)
    }
}

impl ExecutorHealth {
    pub(in crate::write) fn new() -> Self {
        ExecutorHealth {
            lifecycle: AtomicU8::new(ExecutorPosture::NotStarted as u8),
            wal_poisoned: AtomicBool::new(false),
            wal_recoveries: AtomicU64::new(0),
            work_submitted: AtomicU64::new(0),
            deny_submitted: AtomicU64::new(0),
            ticks: AtomicU64::new(0),
            flush_requested: AtomicBool::new(false),
            publication: Mutex::new(PublicationCycle::default()),
            deferred_plans: AtomicBool::new(false),
            failed_cycle_nanos: AtomicU64::new(0),
            refusal_logged_nanos: AtomicU64::new(0),
            flush_in_flight: Arc::new(AtomicBool::new(false)),
            fold_completed_pending: Arc::new(AtomicBool::new(false)),
            merge_completed_pending: Arc::new(AtomicBool::new(false)),
            fold_in_flight: Arc::new(AtomicBool::new(false)),
            merge_in_flight: Arc::new(AtomicBool::new(false)),
            coalesce_in_flight: Arc::new(AtomicBool::new(false)),
            coalesce_completed_pending: Arc::new(AtomicBool::new(false)),
            flush_completed_pending: Arc::new(AtomicBool::new(false)),
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
            foreign_side_manifests: AtomicU64::new(0),
            coalesces: AtomicU64::new(0),
            coalesce_failures: AtomicU64::new(0),
            merges: AtomicU64::new(0),
            merge_failures: AtomicU64::new(0),
            folds: AtomicU64::new(0),
            fold_failures: AtomicU64::new(0),
            fold_requested: AtomicBool::new(false),
            fold_refusals: std::array::from_fn(|_| AtomicU64::new(0)),
            last_fold_refusal: Mutex::new(None),
            wal_gauge: Mutex::new(WalGauge::default()),
            fold_ended_unix: AtomicU64::new(0),
            last_fold_secs: AtomicU64::new(0),
            last_fold_rss: AtomicU64::new(0),
            last_fold_attr_read: AtomicU64::new(0),
            last_fold_attr_written: AtomicU64::new(0),
            last_fold_passes: Mutex::new(Vec::new()),
            last_fold_report: Mutex::new(Vec::new()),
            flush_holding: AtomicBool::new(false),
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
    pub(in crate::write) fn advance(&self, to: ExecutorPosture) {
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
    pub(in crate::write) fn mirror_wal(&self, poisoned: bool) -> bool {
        let was = self.wal_poisoned.swap(poisoned, Ordering::SeqCst);
        let recovered = was && !poisoned;
        if recovered {
            self.wal_recoveries.fetch_add(1, Ordering::Relaxed);
        }
        recovered
    }

    /// `Dead` wins over a poisoned WAL: a thread that is gone applies nothing whatever the log
    /// says. `NotStarted` is answered before the WAL is consulted, because nothing has been
    /// attempted on a WAL no executor owns.
    pub fn posture(&self) -> ExecutorPosture {
        match ExecutorPosture::from_u8(self.lifecycle.load(Ordering::SeqCst)) {
            ExecutorPosture::Dead => ExecutorPosture::Dead,
            ExecutorPosture::NotStarted => ExecutorPosture::NotStarted,
            _ if self.wal_poisoned.load(Ordering::SeqCst) => ExecutorPosture::WalPoisoned,
            other => other,
        }
    }

    /// Publication cycles completed since the executor started (contracts §3.4's `publication`).
    pub fn publication(&self) -> u64 {
        lock_recover(&self.publication).completed
    }

    /// Records a `POST /control/flush` and answers the publication number of the cycle that will
    /// honour it.
    ///
    /// A cycle plans from the buffer after it opens, and the flag goes up under the lock that says
    /// whether one is open. With no cycle open, the next cycle plans over this request's work and
    /// is `completed + 1`. With one open, it may have planned already, so the answer is the cycle
    /// after it, `completed + 2`. A tick that finds a flush on the pool opens no cycle and consumes
    /// no flag.
    pub(crate) fn request_flush(&self) -> u64 {
        let cycle = lock_recover(&self.publication);
        self.flush_requested.store(true, Ordering::SeqCst);
        cycle.target()
    }

    /// The number of the cycle work buffered by now becomes visible in, asking for no tick.
    ///
    /// The same two answers [`Self::request_flush`] gives, on the same argument, for a write
    /// acknowledgement that names the number without pulling the cadence forward: the work is
    /// durable and buffered before this is read, so the next cycle to open carries it, and a
    /// cycle already open may have planned first.
    pub(crate) fn publication_target(&self) -> u64 {
        lock_recover(&self.publication).target()
    }

    /// Opens a cycle and consumes the flush request it honours, under one lock, so a request is
    /// never both answered against this cycle and stripped of the flag that brings the next.
    pub(crate) fn open_publication_cycle(&self) {
        let mut cycle = lock_recover(&self.publication);
        self.flush_requested.store(false, Ordering::SeqCst);
        cycle.open = true;
    }

    /// Close the open cycle: a publication swapped and its work is being served.
    ///
    /// Called from the publication itself, where the generation the work is in becomes the live
    /// one, so reaching the number and reading the work are one event. A cycle that deferred a
    /// second view's plan is not closed here: its caller asked for its buffered rows to be
    /// published and one of its views still holds some ([`Self::deferred_plans`]).
    pub(crate) fn close_publication_cycle(&self) {
        if self.deferred_plans.load(Ordering::SeqCst) {
            return;
        }
        let mut cycle = lock_recover(&self.publication);
        if cycle.open {
            cycle.completed += 1;
            cycle.open = false;
        }
        self.failed_cycle_nanos.store(0, Ordering::Relaxed);
        self.refusal_logged_nanos.store(0, Ordering::Relaxed);
    }

    /// The cycle published nothing it was asked to (gates shut, a failed flush, a discarded
    /// publication): hold it open and re-arm the request, so the next tick retries.
    /// [`Executor::tick_if_due`] floors how fast.
    pub(crate) fn fail_publication_cycle(&self) {
        self.set_marker(&self.failed_cycle_nanos, std::time::Instant::now());
        self.flush_requested.store(true, Ordering::SeqCst);
    }

    /// How long a re-armed retry must still wait, or `None` where nothing is owed one.
    pub(crate) fn failed_cycle_backoff(&self) -> Option<std::time::Duration> {
        if self.failed_cycle_nanos.load(Ordering::Relaxed) == 0 {
            return None;
        }
        FAILED_CYCLE_RETRY
            .checked_sub(std::time::Duration::from_nanos(
                self.elapsed_since_marker(&self.failed_cycle_nanos),
            ))
            .filter(|remaining| !remaining.is_zero())
    }

    /// Whether a publication refusal is due to be logged, at most one per tick period.
    ///
    /// Every condition this throttles stands until an operator acts: a poisoned WAL, a diverged
    /// overlay, an analyser this binary does not carry, a view the manifest does not declare, a
    /// row space at the `u32` ceiling. A failed cycle re-arms its request and retries at [`FAILED_CYCLE_RETRY`], so
    /// logging each one where it is found would turn one condition into a line a second. The
    /// counters beside them (`flush_failures`) move every time, which is what an operator alarms
    /// on; the line is what says which condition it is.
    pub(crate) fn refusal_log_due(&self) -> bool {
        let period = self.flush_period_nanos.load(Ordering::Relaxed);
        let marker = self.refusal_logged_nanos.load(Ordering::Relaxed);
        if marker != 0 && self.elapsed_since_marker(&self.refusal_logged_nanos) < period {
            return false;
        }
        self.set_marker(&self.refusal_logged_nanos, std::time::Instant::now());
        true
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
            foreign_side_manifests: self.foreign_side_manifests.load(Ordering::Relaxed),
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
            merge_in_flight: self.merge_in_flight.load(Ordering::SeqCst)
                || self.merge_completed_pending.load(Ordering::SeqCst),
            coalesce_in_flight: self.coalesce_in_flight.load(Ordering::SeqCst)
                || self.coalesce_completed_pending.load(Ordering::SeqCst),
        }
    }

    /// Records what the last fold cost and returns the staircase as one log field, the fold's
    /// seconds and the highest resident set a pass boundary saw. Total and anonymous memory are
    /// both shown: mapped inputs becoming resident is expected, anonymous growth is not.
    pub(in crate::write) fn record_fold_cost(
        &self,
        cost: Vec<crate::compact::PassCost>,
        attr_bytes_read: u64,
        attr_bytes_written: u64,
    ) -> (String, u64, u64) {
        const GIB: f64 = (1u64 << 30) as f64;
        let passes = cost
            .iter()
            .map(|c| {
                format!(
                    "{}={:?}/{:.2}GiB({:.2} anon)",
                    c.pass,
                    c.elapsed,
                    c.rss as f64 / GIB,
                    c.anon as f64 / GIB,
                )
            })
            .collect::<Vec<_>>()
            .join(" ");
        // Summed before truncating to seconds: many sub-second passes are not a zero-second fold.
        let secs = cost
            .iter()
            .map(|c| c.elapsed)
            .sum::<std::time::Duration>()
            .as_secs();
        let rss = cost.iter().map(|c| c.rss).max().unwrap_or(0);
        self.last_fold_secs.store(secs, Ordering::Relaxed);
        self.last_fold_rss.store(rss, Ordering::Relaxed);
        self.last_fold_attr_read
            .store(attr_bytes_read, Ordering::Relaxed);
        self.last_fold_attr_written
            .store(attr_bytes_written, Ordering::Relaxed);
        *lock_recover(&self.last_fold_passes) = cost;
        (passes, secs, rss)
    }

    /// Record a fold the planner would not plan. Executor thread only, once per refusal.
    pub(in crate::write) fn record_fold_refusal(&self, reason: crate::compact::NoFold) {
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
    pub(in crate::write) fn record_wal_gauge(&self, gauge: WalGauge) {
        *lock_recover(&self.wal_gauge) = gauge;
    }

    /// Fold one closed window's tally in. Executor thread only, once per window close.
    pub(in crate::write) fn record_fragmentation(&self, tally: FragmentationTally) {
        lock_recover(&self.fragmentation).merge(tally);
        self.fragmentation_windows.fetch_add(1, Ordering::Relaxed);
    }

    pub(in crate::write) fn record_tier_fragmentation(&self, tally: FragmentationTally) {
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
    pub(in crate::write) fn work_in_flight_nanos(&self) -> u64 {
        self.elapsed_since_marker(&self.work_started_nanos)
    }

    /// Nanoseconds since a marker was set, or `0` where it is unset. A marker is an offset from
    /// [`Self::base`] plus one, so that `0` can mean unset; the `saturating_sub` is
    /// [`Self::work_in_flight_nanos`]'s argument.
    pub(in crate::write) fn elapsed_since_marker(&self, marker: &AtomicU64) -> u64 {
        match marker.load(Ordering::Relaxed) {
            0 => 0,
            set_plus_one => (self.base.elapsed().as_nanos() as u64)
                .saturating_sub(set_plus_one.saturating_sub(1)),
        }
    }

    /// Set a marker to `at`. `0` is reserved for unset, hence the `+ 1`.
    pub(in crate::write) fn set_marker(&self, marker: &AtomicU64, at: std::time::Instant) {
        let offset = at.saturating_duration_since(self.base).as_nanos() as u64;
        marker.store(offset.saturating_add(1), Ordering::Relaxed);
    }

    /// The flush unit is about to go to the pool. Executor thread only.
    pub(in crate::write) fn mark_flush_started(&self, at: std::time::Instant) {
        self.set_marker(&self.flush_started_nanos, at);
    }

    /// A flush published `rows`: fold its wall time per row into the drain EWMA and clear the
    /// marker. Executor thread only. A flush of no rows moves nothing, having drained nothing.
    pub(in crate::write) fn record_flush_published(&self, rows: usize) {
        let elapsed = self.elapsed_since_marker(&self.flush_started_nanos);
        self.flush_started_nanos.store(0, Ordering::Relaxed);
        if rows == 0 {
            return;
        }
        let sample = elapsed / rows as u64;
        let prev = self.flush_nanos_per_row_ewma.load(Ordering::Relaxed);
        let next = ewma_eighth(prev, sample);
        self.flush_nanos_per_row_ewma.store(next, Ordering::Relaxed);
    }

    /// A tick happened at `at`. Executor thread only.
    pub(in crate::write) fn mark_tick(&self, at: std::time::Instant) {
        self.set_marker(&self.last_tick_nanos, at);
    }

    /// The tick period, from the executor's flush configuration.
    pub(in crate::write) fn set_flush_period_secs(&self, secs: u64) {
        self.flush_period_nanos
            .store(secs.saturating_mul(1_000_000_000), Ordering::Relaxed);
    }

    /// An otherwise-fresh health block whose clock origin is in the past, so a test can construct a
    /// job that has been in flight for a stated duration without waiting for one. Test-only, and it
    /// touches nothing but [`Self::base`] — every counter starts where `new` puts it.
    #[cfg(test)]
    pub(in crate::write) fn with_base(base: std::time::Instant) -> Self {
        let mut health = Self::new();
        health.base = base;
        health
    }

    /// Mark the work item that is about to run. Executor thread only.
    pub(in crate::write) fn mark_work_started(&self, at: std::time::Instant) {
        self.set_marker(&self.work_started_nanos, at);
    }

    /// Charge the time since `mark` to `stage`, and return a fresh mark. A no-op without
    /// `bench-timing`, where it returns `mark` unchanged and reads no clock.
    #[inline(always)]
    pub(in crate::write) fn lap(&self, stage: WriteStage, mark: StageMark) -> StageMark {
        mark.lap(&self.stage_nanos[stage as usize])
    }

    /// Charge the time since `mark` to a flush stage run on this thread, and return a fresh
    /// mark. A no-op without `bench-timing`, as [`Self::lap`] is.
    #[inline(always)]
    pub(crate) fn flush_lap(&self, stage: crate::flush::FlushStage, mark: StageMark) -> StageMark {
        mark.lap(&self.flush_stage_nanos[stage as usize])
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

    pub(in crate::write) fn record_apply(&self, nanos: u64) {
        self.apply_nanos_total.fetch_add(nanos, Ordering::Relaxed);
        self.apply_nanos_max.fetch_max(nanos, Ordering::Relaxed);
    }

    /// One commit window finished `entries` work-lane jobs in `elapsed_nanos`. The EWMA sample is
    /// `elapsed / entries`, because the estimate multiplies it by a depth counted in commands.
    /// `elapsed_nanos` is measured from the window's `opened_at`, so a replacement window must be
    /// constructed only after the previous one has closed.
    pub(in crate::write) fn record_window_service(&self, entries: u64, elapsed_nanos: u64) {
        debug_assert!(entries > 0, "an empty window is never closed");
        self.record_work_service(elapsed_nanos / entries.max(1));
    }

    /// One work-lane job answered. Called by its reply just before the answer is sent, and only
    /// for a job counted in `work_submitted`, so [`ExecutorStats::work_depth`] neither drifts nor
    /// lags a caller that has its answer.
    pub(in crate::write) fn note_work_finished(&self) {
        self.work_completed.fetch_add(1, Ordering::Relaxed);
    }

    /// The EWMA observation for one work-lane job. Called on the executor thread and nowhere
    /// else, which is what lets the EWMA be a plain load/store rather than a CAS loop.
    ///
    /// `sample_nanos` is the **per-job** service — the whole of what a queued job waits for: append,
    /// fsync, apply, swap and ack. A window divides its elapsed by its entry count before calling
    /// here (see [`ExecutorHealth::record_window_service`]), because the estimator this feeds
    /// multiplies the EWMA by a depth counted in *commands*. `apply_nanos_total` is the wrong
    /// operand for a drain estimate and its own doc says why: it sums both lanes and excludes the
    /// fsync, and the fsync is the term the drain is paced by.
    pub(in crate::write) fn record_work_service(&self, sample_nanos: u64) {
        // Cleared **first**: between this and the EWMA store, a concurrent `stats()` should see the
        // stale (smaller) EWMA rather than an in-flight elapsed for a job that has finished. Both
        // orderings are honest; this one cannot over-report a drain that is already over.
        self.work_started_nanos.store(0, Ordering::Relaxed);
        let prev = self.work_service_nanos_ewma.load(Ordering::Relaxed);
        let next = ewma_eighth(prev, sample_nanos);
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

/// The ingest queue's `Retry-After`: `depth` jobs ahead, each taking about `service_nanos`.
///
/// An estimate and never a bound. Service time is not stationary, the deny lane drains first
/// and is not in the figure, and `depth` is a snapshot of two counters. `service_nanos == 0`
/// means nothing has completed yet, and answers [`RETRY_AFTER_MIN_SECS`].
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

/// The publication counter's surface on the engine (contracts §3.4).
///
/// Both live here, away from the rest of [`Engine`]'s methods, because both read the write
/// executor's state and the argument for their exactness is this module's.
impl crate::engine::Engine {
    /// Publication cycles completed since this engine's executor started. `GET /control/status`
    /// publishes it as `publication`, and a client compares it against the number its flush
    /// request was answered with.
    pub fn publication(&self) -> u64 {
        self.write.health().publication()
    }

    /// `POST /control/flush`'s form: request the flush **and** answer the publication number the
    /// cycle honouring it will carry.
    ///
    /// A caller reads `/control/status` until its `publication` has reached that number, and is
    /// then promised that a cycle which saw this request's buffered work has published. That
    /// covers a values-only or artifacts-only cycle, which writes no point rows and moves no
    /// `segments_version`. [`ExecutorHealth::request_flush`] argues why the number is exact and
    /// cannot name a cycle that skipped the work.
    ///
    /// [`Engine::request_flush`] calls this and drops the number.
    pub fn request_flush_publication(&self) -> u64 {
        let publication = self.write.health().request_flush();
        self.write.wake();
        publication
    }

    /// The number of the cycle work acknowledged by now becomes visible in, asking for no tick.
    ///
    /// Every write acknowledgement carries this (contracts §3.4). It is read after the route's
    /// own call returned, so the work it names is already buffered, and it is computed under the
    /// lock [`Engine::request_flush_publication`] uses, so the two cannot disagree about which
    /// cycle is open.
    pub fn publication_target(&self) -> u64 {
        self.write.health().publication_target()
    }
}

/// The publication counter and whether a cycle is open ([`ExecutorHealth::publication`]).
///
/// The two travel under one lock because the answer `POST /control/flush` gives is a statement
/// about both: what a request is promised depends on whether a cycle was already under way when
/// it arrived.
#[derive(Debug, Default)]
pub(crate) struct PublicationCycle {
    /// Cycles completed since the executor started. Monotonic, and never reset.
    pub(crate) completed: u64,
    /// A tick is executing, or a flush it dispatched has not been applied.
    pub(crate) open: bool,
}

impl PublicationCycle {
    /// The cycle that carries work buffered as of this read: the next one to open, or the one
    /// after an open one, which may have planned before that work arrived.
    pub(in crate::write) fn target(&self) -> u64 {
        self.completed + if self.open { 2 } else { 1 }
    }
}

/// An exponentially weighted mean with weight 1/8. `0` means no observation, so the first sample
/// seeds it and the result is never `0`.
pub(in crate::write) fn ewma_eighth(prev: u64, sample: u64) -> u64 {
    if prev == 0 {
        return sample.max(1);
    }
    let (p, s) = (prev as i128, sample as i128);
    (p + (s - p) / 8).max(1) as u64
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
    pub(in crate::write) fn no_observation_yields_the_floor_not_a_zero() {
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
    pub(in crate::write) fn buffer_retry_after_is_the_tick_plus_the_observed_drain() {
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
    pub(in crate::write) fn a_deep_queue_draining_slowly_asks_for_more_than_one_second() {
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
    pub(in crate::write) fn the_estimate_is_clamped_at_both_ends() {
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
    pub(in crate::write) fn the_service_estimate_follows_the_recent_regime_not_the_whole_history() {
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
    pub(in crate::write) fn work_depth_saturates_rather_than_underflowing() {
        let health = ExecutorHealth::new();
        health.note_work_finished();
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
    pub(in crate::write) fn a_long_job_in_flight_raises_the_estimate_before_it_completes() {
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
    pub(in crate::write) fn the_overlay_alarm_fires_once_per_crossing_not_once_per_change() {
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
