//! The write path: one writer thread that owns the WAL, the two queues that feed it, and the live
//! state a handler reads before submitting.
//!
//! The `Wal` is moved by value onto one [`Executor`] thread, so append, fsync, apply and swap
//! happen in that order because only that thread can do any of them, and two acceptances cannot
//! lose each other's update. The generation swap in [`Executor::publish_arc`] is the only
//! non-atomic `store` in the crate; `scripts/check-layers.sh` holds that. The thread is a plain
//! `std::thread`, so `Engine::accept_ingest` blocks and a tokio handler wraps it in
//! `spawn_blocking`.
//!
//! - [`LiveState`]: the maps a handler reads and the executor writes, each behind its own lock.
//! - [`WritePath`]: the handler side, held by `Engine`. Owns the `Wal` until the executor starts.
//! - [`Executor`]: the thread. Owns the [`ExecutorWal`] and the publishing capability.
//!
//! Work is bounded (a full queue answers 429). Deny is unbounded and is never refused for load, and
//! the loop drains it to empty before taking work: a deny waits for at most the work item in
//! flight, a sustained deny flood starves ingest, and the deny queue is unbounded in memory. The
//! lane is chosen by the command ([`LifecycleHandle::enqueue`]), so a suppression can never be
//! put on the bounded queue.

mod executor;
mod health;
mod live;
mod replay;
mod schema;

pub use executor::*;
pub use health::*;
pub(crate) use live::*;
pub(crate) use replay::*;
pub(crate) use schema::*;

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
// The ack channel
// =================================================================================================

/// The receipt half of a submitted command. A handler acks only after the effect is in force:
/// durable, applied and swapped in. `ack_follows_fsync_then_swap` holds that.
pub(crate) struct Responder(SyncSender<Receipt>);

impl Responder {
    fn new(tx: SyncSender<Receipt>) -> Self {
        Responder(tx)
    }

    /// A caller that has gone away is not an error: the effect stands either way.
    fn ack(&self, ack: Ack) {
        let _ = self.0.send(Receipt::ok(ack));
    }

    fn fail(&self, error: ExecError) {
        let _ = self.0.send(Receipt::failed(error));
    }
}

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
    /// The bundle root's write lock, held for as long as the executor is. Dropped after the join
    /// in [`WritePath::drop`] — field drops follow the `drop` body — so the lock outlives every
    /// write the executor makes.
    bundle_lock: Option<crate::bundle_lock::BundleWriteLock>,
    health: Arc<ExecutorHealth>,
    #[cfg(feature = "fault-injection")]
    faults: Option<Arc<tessera_lifecycle::faults::FaultSwitchboard>>,
}

/// Why an executor could not be started.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// Another executor holds this bundle root's write lock (`crate::bundle_lock`).
    ///
    /// One executor owns a bundle root. Every name a publication allocates — the side-manifest
    /// number, an entity id, a `seg_id` — comes from state one executor holds, and a second writer
    /// takes the same names from the same seed. The refusal is what keeps the second one from
    /// starting; the side-manifest floor keeps a node that met one anyway from livelocking.
    BundleLocked(crate::bundle_lock::BundleLockError),
    /// The bundle root could not be listed for the side-manifest numbers already on disc
    /// (`tessera_store::highest_side_manifest_n`).
    ///
    /// A node that cannot see which `n` are taken cannot allocate one, and every publication it
    /// made would be a guess at a free name. Refusing to start is recoverable — the bundle is
    /// untouched — where starting is not.
    SideManifestScan(String),
}

impl std::fmt::Display for ExecutorStartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecutorStartError::AlreadyStarted => {
                write!(f, "this engine's write executor is already running")
            }
            ExecutorStartError::Spawn(kind) => write!(
                f,
                "the write thread could not be spawned ({kind:?}), so this engine accepts no writes"
            ),
            ExecutorStartError::BundleLocked(e) => write!(
                f,
                "this bundle root is already being written ({e}); stop the other writer first"
            ),
            ExecutorStartError::SideManifestScan(detail) => write!(
                f,
                "the side-manifest numbers on disc could not be read ({detail})"
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
                "row {index} carries {got} scalars and the schema declares {expected}; send at \
                 most one value per declared column, in declaration order"
            ),
            AcceptError::OutsideExtent {
                index,
                x,
                y,
                quantisation: q,
            } => write!(
                f,
                "row {index} at ({x}, {y}) is outside the view's declared extent (x {}..{}, y \
                 {}..{}); rebuild the view with a wider extent to hold it",
                q.x_min, q.x_max, q.y_min, q.y_max
            ),
            AcceptError::UnknownView { index, view } => write!(
                f,
                "row {index} names view '{view}', which this bundle does not declare"
            ),
            AcceptError::SteppedDown => write!(
                f,
                "ingest is refused while a partition serves a stepped-down side-manifest; repair \
                 or restore the damaged newest manifest's files, then retry"
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

impl WritePath {
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
            bundle_lock: None,
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
        if self.wal.is_none() {
            return Err(ExecutorStartError::AlreadyStarted);
        }
        // **Before the WAL is taken**, so a refused start leaves this engine exactly as it was: the
        // WAL moves into the executor's closure and cannot be handed back, and a caller that meets
        // a locked bundle must be able to answer the same `AlreadyStarted`/`BundleLocked` question
        // again rather than a stale one.
        let bundle_lock = crate::bundle_lock::BundleWriteLock::acquire(&flush.bundle_root)
            .map_err(|e| {
                tracing::error!(
                    error = %e,
                    "ALARM: refusing to start a write executor over a bundle root another writer \
                     holds"
                );
                ExecutorStartError::BundleLocked(e)
            })?;
        tracing::debug!(root = %bundle_lock.path().display(), "bundle write lock taken");
        let wal = self.wal.take().expect("checked immediately above");
        let wal_position_at_start = wal.position();

        // **Compaction §7's startup sweep, before anything else and before the thread** — see
        // `sweep_orphan_prefixes` for why both halves of that matter. `AlreadyStarted` is checked
        // first, so a second `start_executor` on the same path cannot sweep a second time.
        sweep_orphan_prefixes(&flush.bundle_root, &generation.load().prefix);

        // **Above every `SEGMENTS-<n>.json` on disc, not above what a manifest names** — see
        // [`Executor::next_manifest_n`]. The sweep above has already removed the unpublished
        // prefixes, so what is left is what a reader could resolve.
        let next_manifest_n = tessera_store::highest_side_manifest_n(&flush.bundle_root)
            .map_err(|e| ExecutorStartError::SideManifestScan(e.to_string()))?
            .map_or(1, |highest| highest + 1);

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
        // The derived files the last fold or the build wrote, held for the same reason.
        let seeded_derived_extents: Vec<tessera_store::manifest::DerivedExtent> = generation
            .load()
            .bundle
            .partitions
            .values()
            .flat_map(|p| p.manifest.derived_extents.iter().cloned())
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
                    next_manifest_n,
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
                    derived_extents: seeded_derived_extents,
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
        self.bundle_lock = Some(bundle_lock);
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

    /// Submits a command, waits for its receipt, and takes the ack that command answers with.
    fn run<T>(
        &self,
        command: Command,
        take: impl FnOnce(Ack) -> Option<T>,
    ) -> Result<T, AcceptError> {
        match self.handle()?.submit(command)?.outcome {
            Ok(ack) => Ok(take(ack).expect("a command answers with its own ack")),
            Err(e) => Err(AcceptError::Exec(e)),
        }
    }

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
        let answered = self.run(
            Command::Ingest {
                rows,
                batch_id,
                body_hash,
                artifacts,
            },
            |ack| match ack {
                Ack::Ingested { entity_ids, minted } => Some((entity_ids, minted)),
                _ => None,
            },
        );
        self.health().lap(WriteStage::SubmitToReceipt, mark);
        answered
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
        self.run(
            Command::RegisterLayer {
                declaration: Box::new(declaration),
            },
            |ack| match ack {
                Ack::LayerRegistered { entity } => Some(entity),
                _ => None,
            },
        )
    }

    /// Drop a layer, tombstoning its name for ever.
    pub(crate) fn drop_layer(&self, name: String) -> Result<(), AcceptError> {
        self.run(
            Command::DropLayer { name },
            |ack| match ack {
                Ack::LayerDropped => Some(()),
                _ => None,
            },
        )
    }

    /// Create a view of a view group while the service runs (`views.md` §3.2).
    pub(crate) fn create_view(
        &self,
        group: String,
        key: String,
        visibility: Option<Vec<String>>,
        metadata: std::collections::BTreeMap<String, tessera_types::view::ViewMetadataValue>,
    ) -> Result<(), AcceptError> {
        self.run(
            Command::CreateView {
                group,
                key,
                visibility,
                metadata,
            },
            |ack| match ack {
                Ack::ViewCreated => Some(()),
                _ => None,
            },
        )
    }

    /// Declare an attribute column while the service runs (`ingest.md` §1.3, §6.3). Answers
    /// whether the name already carried this identity, in which case nothing was appended.
    pub(crate) fn declare_attribute(
        &self,
        request: tessera_lifecycle::AttributeRequest,
    ) -> Result<bool, AcceptError> {
        self.run(
            Command::DeclareAttribute {
                request: Box::new(request),
            },
            |ack| match ack {
                Ack::AttributeDeclared { existing } => Some(existing),
                _ => None,
            },
        )
    }

    /// Fill attribute values on entities that already exist (`POST /control/values`,
    /// `ingest.md` §1.4), and answer what the batch did.
    pub(crate) fn fill_values(
        &self,
        request: tessera_lifecycle::ValuesRequest,
    ) -> Result<ValuesReceipt, AcceptError> {
        self.run(
            Command::Values {
                request: Box::new(request),
            },
            |ack| match ack {
                Ack::ValuesFilled {
                    filled,
                    held,
                    joined,
                    minted,
                } => Some(ValuesReceipt {
                    filled,
                    held,
                    joined,
                    minted,
                }),
                _ => None,
            },
        )
    }

    /// Declare a vocabulary. Answers `(existing, added, titles)`: whether a vocabulary of that
    /// name already carried this identity, how many of the request's values were novel, and how
    /// many held values it gave a new title.
    pub(crate) fn declare_vocabulary(
        &self,
        request: tessera_lifecycle::VocabularyRequest,
    ) -> Result<(bool, u64, u64), AcceptError> {
        self.run(
            Command::DeclareVocabulary {
                request: Box::new(request),
            },
            |ack| match ack {
                Ack::VocabularyDeclared {
                    existing,
                    added,
                    titles,
                } => Some((existing, added, titles)),
                _ => None,
            },
        )
    }

    /// A page of values for a vocabulary that exists. Answers `(added, existing, titles)`.
    pub(crate) fn mint_vocabulary_values(
        &self,
        vocabulary: String,
        values: Vec<tessera_lifecycle::DeclaredValue>,
    ) -> Result<(u64, u64, u64), AcceptError> {
        self.run(
            Command::MintVocabularyValues { vocabulary, values },
            |ack| match ack {
                Ack::VocabularyValuesMinted {
                    added,
                    existing,
                    titles,
                } => Some((added, existing, titles)),
                _ => None,
            },
        )
    }

    /// Declare a view group. Answers whether a group of that name already carried this identity.
    pub(crate) fn create_view_group(
        &self,
        declaration: tessera_lifecycle::wal::ViewGroupDeclaration,
    ) -> Result<bool, AcceptError> {
        self.run(
            Command::CreateViewGroup {
                declaration: Box::new(declaration),
            },
            |ack| match ack {
                Ack::ViewGroupCreated { existing } => Some(existing),
                _ => None,
            },
        )
    }

    /// Create a plain view. Answers whether a view of that name already carried this identity.
    pub(crate) fn create_plain_view(
        &self,
        declaration: tessera_lifecycle::wal::PlainViewDeclaration,
    ) -> Result<bool, AcceptError> {
        self.run(
            Command::CreatePlainView {
                declaration: Box::new(declaration),
            },
            |ack| match ack {
                Ack::PlainViewCreated { existing } => Some(existing),
                _ => None,
            },
        )
    }

    /// Drop a view — freeing its key and killing its incarnation (decision 0115) — and answer how
    /// many entities `delete_dangling` submitted for deletion (`views.md` §3.4).
    pub(crate) fn drop_view(
        &self,
        group: String,
        key: String,
        delete_dangling: bool,
    ) -> Result<u64, AcceptError> {
        self.run(
            Command::DropView {
                group,
                key,
                delete_dangling,
            },
            |ack| match ack {
                Ack::ViewDropped { deleted } => Some(deleted),
                _ => None,
            },
        )
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
        self.run(
            Command::PublishArtifacts {
                layer,
                level,
                artifacts,
            },
            |ack| match ack {
                Ack::ArtifactsPublished {
                    entities,
                    created,
                    without_content,
                    filled,
                    joined,
                } => Some(PublishedBatch {
                    entities,
                    created,
                    without_content,
                    filled,
                    joined,
                }),
                _ => None,
            },
        )
    }

    /// Grow the memberships of artifacts that already exist, answering one receipt per join in
    /// the caller's order. See `Executor::commit_growth`.
    pub(crate) fn grow_memberships(
        &self,
        layer: String,
        level: u32,
        joins: Vec<tessera_lifecycle::IncomingGrowth>,
    ) -> Result<Vec<tessera_lifecycle::MembershipGrown>, AcceptError> {
        self.run(
            Command::GrowMemberships {
                layer,
                level,
                joins,
            },
            |ack| match ack {
                Ack::MembershipsGrown { grown } => Some(grown),
                _ => None,
            },
        )
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
        // **After the join, never before.** The lock's promise is that no other executor writes
        // while this one might, and this one might until its thread has ended.
        drop(self.bundle_lock.take());
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
                "this engine has no write executor, so it cannot publish geometry",
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
