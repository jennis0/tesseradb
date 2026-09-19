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

mod command;
mod executor;
mod health;
pub(crate) mod joined;
mod live;
mod reconstruct;
mod schema;

pub(crate) use command::*;
pub use command::ViewDropped;
pub use executor::*;
pub use health::*;
pub(crate) use live::*;
pub(crate) use reconstruct::*;
pub(crate) use schema::*;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard};

use rustc_hash::{FxHashMap, FxHashSet};

use tessera_authz::{DeltaTier, Dict, FragmentCache};
use tessera_lifecycle::alloc::{high_water_from, low_water_from, AllocError, Allocator};
use tessera_lifecycle::buffer::DescriptorResolver;
use tessera_lifecycle::command::{ExecError, SubmitError, UnallocatedRow};
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

use crate::error::EngineError;
use crate::{Generation, GenerationHandle};

/// Take a lock, recovering rather than panicking if a previous holder panicked.
///
/// The executor thread is the only writer of these maps. Recovering keeps an unrelated read from
/// panicking on a poisoned lock; the executor's death is already reported fail-closed by
/// [`ExecutorPosture::Dead`], each map insert is individually complete so the recovered state is a
/// prefix of a batch, and buffered items have no row geometry to contribute to a viewport, count
/// or density. The WAL, not these maps, is the durable record either way.
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
    /// A plain `Option`: starting the executor takes `&mut self`, so there is no shared-access
    /// problem, and after the take this field stays `None`.
    wal: Option<Wal>,
    /// The sole owner of the two queue senders. [`LifecycleHandle`] is not `Clone`:
    /// [`WritePath::drop`] disconnects the channels and joins the thread, and an outstanding clone
    /// would make that join hang.
    handle: Option<LifecycleHandle>,
    join: Option<std::thread::JoinHandle<()>>,
    /// The bundle root's write lock, held for as long as the executor is. Dropped after the join
    /// in [`WritePath::drop`], so the lock outlives every write the executor makes.
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
    /// The OS refused the thread (`EAGAIN`: thread or memory limits). The WAL has already moved
    /// into the closure that failed to spawn and was dropped with it, so the engine is permanently
    /// writer-less and a retry answers `AlreadyStarted`. Restart the process.
    Spawn(std::io::ErrorKind),
    /// Another executor holds this bundle root's write lock (`crate::bundle_lock`). One executor
    /// owns a bundle root: every name a publication allocates (a side-manifest number, an entity
    /// id, a `seg_id`) comes from state that executor holds, and a second writer would allocate
    /// from the same seed.
    BundleLocked(crate::bundle_lock::BundleLockError),
    /// The bundle root could not be listed for the side-manifest numbers already on disc
    /// (`tessera_store::highest_side_manifest_n`). Refusing to start leaves the bundle untouched.
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

/// Why an accepted write did not succeed: it never reached the executor ([`SubmitError`]), or it
/// failed while executing ([`ExecError`]).
///
/// A failed send proves the command was never enqueued. Anything after a successful send may have
/// taken effect: `Submit(ReceiptLost)` and `Exec(Wal)` on a `Delete`/`Suppress` report 500, never
/// 503, because the write may already be durable and applied; `Submit(QueueFull)` is 429 and
/// `Submit(ExecutorDead)` is 503, since non-enqueue there is proven. `tessera-server`'s
/// `map_accept_error` owns this mapping. Neither this enum nor [`SubmitError`]/[`ExecError`] is
/// `#[non_exhaustive]`, so a new variant is a compile error at every match, including that one,
/// rather than a silent 500.
#[derive(Debug)]
pub enum AcceptError {
    Submit(SubmitError),
    Exec(ExecError),
    /// A row's coordinates fall outside the view's declared quantisation extent, so the point has
    /// no cell to occupy. Refused before anything is acked or WAL-durable, rather than clamped
    /// (see [`Quantisation::contains`]). Checked at the engine's ingest boundary because the
    /// buffer has more than one writer, not only the HTTP handler.
    OutsideExtent {
        index: usize,
        x: f64,
        y: f64,
        quantisation: tessera_store::manifest::Quantisation,
    },
    /// A row names a view this bundle does not declare, so there is no frame to quantise it
    /// against. Checked at the engine's boundary for the same reason as [`Self::OutsideExtent`];
    /// the HTTP handler refuses an unknown `x-tessera-view` with its own 404 and is only one of
    /// the buffer's writers.
    UnknownView {
        index: usize,
        view: String,
    },
    /// A row carries more scalars than the schema declares columns. The commit window indexes a
    /// row's scalars positionally against the declared columns, so a longer row would pair values
    /// with columns that do not exist. A row shorter than the schema is lawful: a column declared
    /// at a running service appends at the tail, and the window's close pads a short row with each
    /// missing column's absence (`crate::attributes::pad_to_schema`) before anything indexes it.
    /// Checked at the engine's boundary for the same reason as [`Self::OutsideExtent`]. The
    /// declared list includes filterable-only columns; only the segment narrows to the render
    /// ones.
    ScalarArity {
        index: usize,
        expected: usize,
        got: usize,
    },
    /// A partition is serving a stepped-down side-manifest. Ingest is refused at the engine's
    /// boundary: a stepped-down node that accepted and flushed would assemble its manifest from
    /// older served partition state at a higher `n`, permanently shadowing the stepped-past
    /// segment, and rotation moving the reclaim bound would make its acked rows unrecoverable.
    /// Denies are not gated: a deny is entity-space state carried by WAL and manifest deny fields,
    /// threatens no segment, and must never be refused.
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
    /// The view roster: which views of which groups exist, and which keys are burnt. Rebuilt
    /// exactly as the layer registry beside it is: seeded from the manifests, then the log
    /// replayed on top.
    pub(crate) roster: tessera_lifecycle::ViewRoster,
    /// The attribute columns declared at a running service and not yet folded, rebuilt as the
    /// roster is: seeded from the manifests, then the log replayed on top.
    pub(crate) attributes: crate::attributes::RuntimeAttributes,
    /// The vocabularies declared at a running service and not yet folded, rebuilt as the
    /// attribute columns beside them are.
    pub(crate) vocabularies: crate::vocabularies::RuntimeVocabularies,
    /// The view groups and plain views declared at a running service and not yet folded, rebuilt
    /// as the vocabularies beside them are.
    pub(crate) view_declarations: crate::view_declarations::RuntimeViewDeclarations,
}

impl WritePath {
    /// Assemble the write path.
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
    /// `&mut self` rather than a lock: every caller holds the `Engine` by value before sharing it,
    /// so single ownership of the WAL is enforced by the borrow checker instead of a runtime
    /// `take`.
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
        // Before the WAL is taken, so a refused start leaves this engine as it was: the WAL moves
        // into the executor's closure and cannot be handed back, and a caller that meets a locked
        // bundle can retry and get the same answer.
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

        // The startup sweep runs before anything else and before the thread. `AlreadyStarted` is
        // checked first, so a second `start_executor` on the same path cannot sweep twice.
        sweep_orphan_prefixes(&flush.bundle_root, &generation.load().prefix);

        // Above every `SEGMENTS-<n>.json` on disc, not above what a manifest names: see
        // [`Executor::next_manifest_n`]. The sweep above has already removed the unpublished
        // prefixes, so what is left is what a reader could resolve.
        let next_manifest_n = tessera_store::highest_side_manifest_n(&flush.bundle_root)
            .map_err(|e| ExecutorStartError::SideManifestScan(e.to_string()))?
            .map_or(1, |highest| highest + 1);

        let (work_tx, work_rx) = std::sync::mpsc::sync_channel(queue_bound);
        let (deny_tx, deny_rx) = std::sync::mpsc::channel();
        // Capacity one, and `try_send` that discards `Full`: a token means something may be
        // waiting, and a second token while one is pending adds nothing. The executor only blocks
        // on this after observing both queues empty: see [`Executor::run`].
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
        // Read before the pointer moves into the thread. What the bundle's manifests already carry
        // is this list's starting point: see the field for why it is held rather than re-cloned
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
                    next_manifest_n,
                    deny_dirty: false,
                    windows_since_publication: 0,
                    bundle_root: flush.bundle_root,
                    identity_key: flush.identity_key,
                    pool: flush.pool,
                    max_distinct_terms: flush.max_distinct_terms,
                    coalesce_policy: flush.coalesce,
                    coalesce: executor::Background::new(),
                    refresh: flush.refresh,
                    merge_policy: flush.merge,
                    switches: flush.switches,
                    merge: executor::Background::sharing(
                        Default::default(),
                        Arc::clone(&health.merge_completed_pending),
                    ),
                    fold: executor::Background::sharing(
                        Default::default(),
                        Arc::clone(&health.fold_completed_pending),
                    ),
                    suggest_dir: flush.suggest_dir,
                    suggest: executor::Background::new(),
                    flush: executor::Background::sharing(
                        Arc::clone(&health.flush_in_flight),
                        Default::default(),
                    ),
                    configured_merge_bytes: flush.configured_merge_bytes,
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
                // Declared last so it drops first during unwind: the posture reaches `Dead` before
                // the receivers disconnect, so a subsequent submitter cannot see `ExecutorDead`
                // while `readyz` still reports ready.
                //
                // It does not order the posture against the in-flight submitter. The in-flight
                // command is destructured into `Executor::execute`'s frame, so its `Reply` drops
                // earlier in the unwind than this guard, and that caller's `rx.recv()` can return
                // before the posture moves. Its error is then `SubmitError::ReceiptLost`, mapped to
                // 500 rather than 503, which is correct regardless of the posture because the
                // command may be fully applied. `tests/write.rs`'s
                // `an_executor_panic_is_reported_dead` asserts both halves.
                //
                // The lifecycle axis is published with `fetch_max` and answered before the WAL
                // flag, so `Dead` is absorbing and a bounded poll of `/readyz` converges.
                let _guard = DeathGuard(health);
                executor.run();
            })
            .map_err(|e| ExecutorStartError::Spawn(e.kind()))?;

        // Advanced after a successful spawn, not before it: a failed spawn must leave the
        // posture at `NotStarted` (an operator configuration fault: writes refused, reads
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

    /// The maps a handler reads before submitting.
    pub(crate) fn live(&self) -> &LiveState {
        &self.live
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

    /// Submit a geometry publication to the executor and block until it has been performed.
    ///
    /// Rides the work lane and is never shed: see [`LifecycleHandle::publish_geometry`].
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
    /// `false` where there is no executor to publish through: the hook's callers all start one,
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

    // --- submission -----------------------------------------------------------------------------

    /// Open a reply channel, enqueue the command built around it, and wait for the answer that
    /// command's reply is typed to carry.
    fn submit<T>(&self, command: impl FnOnce(Reply<T>) -> Command) -> Result<T, AcceptError> {
        let (reply, pending) = Reply::channel(
            #[cfg(feature = "fault-injection")]
            self.faults.clone(),
        );
        self.handle()?.enqueue(command(reply))?;
        pending.accept()
    }

    /// Submit an ingest batch and wait for its receipt.
    ///
    /// Blocking: a tokio handler must call this inside `spawn_blocking`, because `tessera-engine`
    /// has no tokio dependency. Rows arrive unallocated: entity ids are assigned on the executor,
    /// at the close of the commit window this submission lands in. Returns the assigned ids and
    /// how many artifacts this batch's membership column created ([`Ingested::minted`]).
    pub(crate) fn accept_ingest(
        &self,
        rows: Vec<UnallocatedRow>,
        batch_id: String,
        body_hash: [u8; 32],
        artifacts: tessera_lifecycle::BatchArtifacts,
    ) -> Result<(Vec<EntityId>, u64), AcceptError> {
        let mark = StageMark::now();
        let answered = self.submit(|reply| Command::Ingest {
            rows,
            batch_id,
            body_hash,
            artifacts,
            reply,
        });
        self.health().lap(WriteStage::SubmitToReceipt, mark);
        answered.map(|ingested| (ingested.entity_ids, ingested.minted))
    }

    /// Register an annotation layer and wait for its receipt.
    ///
    /// Returns the layer's own entity, which the caller turns into a `tessera_id`, the only
    /// address by which the layer can later be suppressed, since an entity id never crosses the
    /// boundary. A failure means the layer does not exist, the opposite of a deny's posture: see
    /// `Executor::commit_registry`.
    pub(crate) fn register_layer(
        &self,
        declaration: tessera_types::layer::LayerDeclaration,
    ) -> Result<EntityId, AcceptError> {
        self.submit(|reply| Command::RegisterLayer {
            declaration: Box::new(declaration),
            reply,
        })
    }

    /// Drop a layer, tombstoning its name for ever.
    pub(crate) fn drop_layer(&self, name: String) -> Result<(), AcceptError> {
        self.submit(|reply| Command::DropLayer { name, reply })
    }

    /// Create a view of a view group while the service runs.
    pub(crate) fn create_view(
        &self,
        group: String,
        key: String,
        visibility: Option<Vec<String>>,
        metadata: std::collections::BTreeMap<String, tessera_types::view::ViewMetadataValue>,
    ) -> Result<(), AcceptError> {
        self.submit(|reply| Command::CreateView {
            group,
            key,
            visibility,
            metadata,
            reply,
        })
    }

    /// Declare an attribute column while the service runs. Answers whether the name already
    /// carried this identity, in which case nothing was appended.
    pub(crate) fn declare_attribute(
        &self,
        request: tessera_lifecycle::AttributeRequest,
    ) -> Result<bool, AcceptError> {
        self.submit(|reply| Command::DeclareAttribute {
            request: Box::new(request),
            reply,
        })
    }

    /// Fill attribute values on entities that already exist (`POST /control/values`), and answer
    /// what the batch did.
    pub(crate) fn fill_values(
        &self,
        request: tessera_lifecycle::ValuesRequest,
    ) -> Result<ValuesReceipt, AcceptError> {
        self.submit(|reply| Command::Values {
            request: Box::new(request),
            reply,
        })
    }

    /// Declare a vocabulary. Answers `(existing, added, titles)`: whether a vocabulary of that
    /// name already carried this identity, how many of the request's values were novel, and how
    /// many held values it gave a new title.
    pub(crate) fn declare_vocabulary(
        &self,
        request: tessera_lifecycle::VocabularyRequest,
    ) -> Result<(bool, u64, u64), AcceptError> {
        let declared = self.submit(|reply| Command::DeclareVocabulary {
            request: Box::new(request),
            reply,
        })?;
        Ok((declared.existing, declared.added, declared.titles))
    }

    /// A page of values for a vocabulary that exists. Answers `(added, existing, titles)`.
    pub(crate) fn mint_vocabulary_values(
        &self,
        vocabulary: String,
        values: Vec<tessera_lifecycle::DeclaredValue>,
    ) -> Result<(u64, u64, u64), AcceptError> {
        let minted = self.submit(|reply| Command::MintVocabularyValues {
            vocabulary,
            values,
            reply,
        })?;
        Ok((minted.added, minted.existing, minted.titles))
    }

    /// Declare a view group. Answers whether a group of that name already carried this identity.
    pub(crate) fn create_view_group(
        &self,
        declaration: tessera_lifecycle::wal::ViewGroupDeclaration,
    ) -> Result<bool, AcceptError> {
        self.submit(|reply| Command::CreateViewGroup {
            declaration: Box::new(declaration),
            reply,
        })
    }

    /// Create a plain view. Answers whether a view of that name already carried this identity.
    pub(crate) fn create_plain_view(
        &self,
        declaration: tessera_lifecycle::wal::PlainViewDeclaration,
    ) -> Result<bool, AcceptError> {
        self.submit(|reply| Command::CreatePlainView {
            declaration: Box::new(declaration),
            reply,
        })
    }

    /// Drop a view, freeing its key and killing its incarnation.
    pub(crate) fn drop_view(
        &self,
        group: String,
        key: String,
        delete_dangling: bool,
    ) -> Result<ViewDropped, AcceptError> {
        self.submit(|reply| Command::DropView {
            group,
            key,
            delete_dangling,
            reply,
        })
    }

    /// Publish a batch of artifacts, returning their entities in the caller's submitted order and
    /// the batch's counts ([`PublishedBatch`]).
    pub(crate) fn publish_artifacts(
        &self,
        layer: String,
        level: u32,
        artifacts: Vec<IncomingArtifact>,
    ) -> Result<PublishedBatch, AcceptError> {
        self.submit(|reply| Command::PublishArtifacts {
            layer,
            level,
            artifacts,
            reply,
        })
    }

    /// Grow the memberships of artifacts that already exist, answering one receipt per join in
    /// the caller's order. See `Executor::commit_growth`.
    pub(crate) fn grow_memberships(
        &self,
        layer: String,
        level: u32,
        joins: Vec<tessera_lifecycle::IncomingGrowth>,
    ) -> Result<Vec<tessera_lifecycle::MembershipGrown>, AcceptError> {
        self.submit(|reply| Command::GrowMemberships {
            layer,
            level,
            joins,
            reply,
        })
    }

    /// Submit one `/control/changes` entry and wait for its receipt.
    ///
    /// If the append/fsync fails and `op` is `Delete`/`Suppress`, the change is still applied, the
    /// item hidden, before this returns `Err`: never a refusal that leaves a deny unapplied. So an
    /// `Err` here does not mean nothing happened; see [`ExecError::Wal`]. This is the one-item
    /// shape; a caller with a whole request's worth of changes wants [`WritePath::submit_change`],
    /// because waiting here between items reduces the deny lane's group commit to one entry per
    /// window.
    pub(crate) fn accept_change(&self, entity: EntityId, op: ChangeOp) -> Result<(), AcceptError> {
        self.submit_change(entity, op)?.wait()
    }

    /// Enqueue one `/control/changes` entry without waiting for its receipt.
    ///
    /// See [`LifecycleHandle::enqueue`]: a caller that enqueues a whole request and only then
    /// collects gives the executor the queue depth its deny window needs, so one request of N
    /// denies costs one fsync instead of N. Read that doc before treating either half's `Err` as
    /// "nothing happened".
    pub(crate) fn submit_change(
        &self,
        entity: EntityId,
        op: ChangeOp,
    ) -> Result<PendingChange, AcceptError> {
        let (reply, pending) = Reply::channel(
            #[cfg(feature = "fault-injection")]
            self.faults.clone(),
        );
        self.handle()?.enqueue(Command::Change { entity, op, reply })?;
        Ok(PendingChange(pending))
    }

}
/// An enqueued `/control/changes` entry, awaiting its answer.
///
/// The public face of `Pending<()>`: a change's answer carries no ids, so the only thing a caller
/// can do with it is learn whether the change took hold, and this type says exactly that in its
/// `wait` signature.
pub struct PendingChange(Pending<()>);

impl PendingChange {
    /// Block until the executor answers this change.
    ///
    /// `Err` does not mean "nothing happened": for `Delete`/`Suppress` see [`ExecError::Wal`],
    /// and for [`SubmitError::ReceiptLost`] see `Pending::wait`.
    pub fn wait(self) -> Result<(), AcceptError> {
        self.0.accept()
    }
}

impl Drop for WritePath {
    /// Disconnect the queues, then join.
    ///
    /// Without this the executor outlives its `Engine` and keeps appending and fsyncing while the
    /// caller's next statement is typically `TempDir::drop`, producing intermittent `ENOENT` from
    /// the sidecar rename in tests spread across many files.
    ///
    /// The join is unconditional and cannot hang: [`LifecycleHandle`] is not `Clone` and this type
    /// is its only owner, so dropping it below disconnects every sender.
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
        // After the join, never before: the lock's promise is that no other executor writes while
        // this one might, and this one might until its thread has ended.
        drop(self.bundle_lock.take());
    }
}

/// Flips the posture to [`ExecutorPosture::Dead`] however the executor thread ends: a clean
/// shutdown or a panic anywhere in the loop body. The two are not distinguished: to a caller they
/// mean the same thing, that there is nothing left to apply a write to. `NotStarted` is its own
/// variant because that is an operator configuration fault rather than a runtime one.
struct DeathGuard(Arc<ExecutorHealth>);

impl Drop for DeathGuard {
    fn drop(&mut self) {
        self.0.advance(ExecutorPosture::Dead);
    }
}

// =================================================================================================
// The queues
// =================================================================================================

/// What the executor's work lane carries: a lifecycle command, or a geometry publication.
///
/// The store-shaped half cannot live in `tessera-lifecycle`: that crate has no `tessera-store`
/// dependency and must not acquire one, so a `Command` variant carrying an `Arc<Bundle>` is not
/// expressible there. This enum is `tessera-engine`'s own executor vocabulary, and it exists so
/// the executor thread is the only publisher of a generation; a second publisher's compare-and-swap
/// could not stop the executor's own `store` from clobbering it.
///
/// A publication carries a bare sender rather than a [`Reply`]: its answer is engine-local, not a
/// client request, and there is no [`ExecError`] it can fail with.
pub(crate) enum ExecutorWork {
    Lifecycle(Command),
    PublishGeometry {
        publication: GeometryPublication,
        respond: SyncSender<std::result::Result<(), GeometryRefused>>,
    },
    /// Drop one vocabulary's suggestion index and publish: `Engine::forget_suggestion_index_for_test`.
    ///
    /// A test hook that is nonetheless a publication, so it comes through this queue like every
    /// other: the executor thread reads the live generation, builds a successor and stores it, so
    /// a store from anywhere else can be overwritten by a swap already in flight.
    #[cfg(feature = "fault-injection")]
    ForgetSuggestionIndex {
        vocabulary: String,
        respond: SyncSender<()>,
    },
    /// Rebuild one vocabulary's suggestion index from the live minter and publish it :
    /// `Engine::rebuild_suggestion_index_for_test`.
    ///
    /// A test hook for a cadence a test cannot otherwise reach: a rebuild is dispatched when a
    /// vocabulary's side map has run `SUGGEST_REBUILD_SIDE_VALUES` (4,096) values ahead of its
    /// base, hundreds of ingest batches past what a fixture builds, and it is the one publication
    /// that moves neither `segments_version` nor `overlay_version`
    /// (`Executor::publish_completed_suggests`). It comes through this queue for the same reason as
    /// [`ExecutorWork::ForgetSuggestionIndex`]: it is a publication, and the executor thread is the
    /// sole publisher.
    #[cfg(feature = "fault-injection")]
    RebuildSuggestionIndex {
        vocabulary: String,
        respond: SyncSender<()>,
    },
}

/// Why a geometry publication produced no answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishGeometryError {
    /// The live generation refused it: see [`GeometryRefused`].
    Refused(GeometryRefused),
    /// There is no write executor to publish through. Not a refusal of the geometry: a
    /// publication is a swap on the executor thread, so an engine that never started one cannot
    /// publish at all. Reachable only by an embedder that skipped `start_write_executor`;
    /// `tessera-server` starts it unconditionally.
    NoExecutor,
    /// [`crate::Engine::publish_rotated_prefix_for_test`] was offered a prefix `CURRENT` does not name.
    ///
    /// Refused rather than published, because `CURRENT` is the commit point and the bundle
    /// identity is the digest it names. Publishing an uncommitted prefix would leave the process
    /// serving geometry a restart could not find.
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
/// Not `Clone`: [`WritePath::drop`] joins the executor thread, which terminates only when every
/// sender has disconnected. One owner means the join always completes.
pub(crate) struct LifecycleHandle {
    /// Bounded by `ingest_queue_bound`; full means [`SubmitError::QueueFull`] for an ingest, and a
    /// blocking send for a geometry publication, which is not a client request and may not be
    /// shed.
    work: SyncSender<ExecutorWork>,
    /// Unbounded: a deny is never refused for load.
    deny: Sender<Command>,
    /// Capacity-one wake signal. `std::sync::mpsc` has no select over two receivers, and the two
    /// alternatives were both worse: `crossbeam-channel` is a workspace dependency for one
    /// `select!`, and `recv_timeout` polling would put a latency floor on the one wait the
    /// never-shed lane exists to bound.
    bell: SyncSender<()>,
    health: Arc<ExecutorHealth>,
}

impl LifecycleHandle {
    /// Submit a geometry publication and block until the executor has performed it.
    ///
    /// A blocking `send`, not `try_send`: a publication is not a client request and may not be
    /// shed for load, since shedding one would leave a completed flush unpublished with nothing to
    /// retry it. It rides the work lane, never the deny lane; the loop drains deny to empty before
    /// touching work, which keeps a suppression from queueing behind a flush's IO.
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

    /// Submit a suggestion-index rebuild and block until the executor has published it: the same
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
    /// timeout, turning "executes promptly" back into "executes within one tick".
    pub(crate) fn wake(&self) {
        let _ = self.bell.try_send(());
    }

    /// Hand `command` to the executor and return without waiting for its receipt.
    ///
    /// This is what makes group commit reachable for a caller with several commands: a caller that
    /// enqueues N commands and only then waits gives the executor N queued jobs to gather into one
    /// window, where a caller that waits between each gives it one job at a time.
    /// `/control/changes` is that caller.
    ///
    /// The lane is chosen by the command (`Command::is_never_shed`), so a suppression or deletion
    /// can never be put on the bounded queue or answered 429.
    ///
    /// An `Err` does not prove that nothing happened; a caller that treats it that way is fail-open
    /// on the deny lane. [`SubmitError::ExecutorDead`] means the `send` failed, and `send` hands the
    /// value back on failure, so non-enqueue is proven. [`SubmitError::ReceiptLost`] means the
    /// doorbell was disconnected, which happens only after the job is already in a queue:
    /// [`Executor::run`]'s shutdown pass drains the deny lane and executes it before it observes the
    /// disconnect, so the command may be durably in force. No caller may use which half returned an
    /// error as the discriminator; [`SubmitError::may_have_taken_effect`] is, and it is the one
    /// `tessera-server`'s batch fold uses.
    ///
    /// The doorbell rings here rather than in `Pending::wait`, so the executor can commit a small
    /// first window while the caller is still enqueueing, and so a `Pending` dropped without being
    /// waited on still leaves its job able to wake the executor.
    pub(crate) fn enqueue(&self, command: Command) -> std::result::Result<(), SubmitError> {
        if command.is_never_shed() {
            self.deny
                .send(command)
                .map_err(|_| SubmitError::ExecutorDead)?;
            // Bumped after the enqueue and before the blocking wait, so a test can observe "the
            // deny is queued" as a condition rather than betting on a sleep.
            self.health.deny_submitted.fetch_add(1, Ordering::SeqCst);
        } else {
            self.work
                .try_send(ExecutorWork::Lifecycle(command))
                .map_err(|e| match e {
                    // Derived, not a placeholder: see [`estimate_retry_after_s`]. Both operands
                    // are plain atomic loads on a path that must sustain 10⁹-scale ingest.
                    TrySendError::Full(_) => {
                        let stats = self.health.stats();
                        SubmitError::QueueFull {
                            retry_after_s: estimate_retry_after_s(
                                stats.work_depth,
                                // Not the raw EWMA: see `ExecutorStats::service_nanos_for_estimate`.
                                stats.service_nanos_for_estimate(),
                            ),
                        }
                    }
                    TrySendError::Disconnected(_) => SubmitError::ExecutorDead,
                })?;
            self.health.work_submitted.fetch_add(1, Ordering::SeqCst);
        }

        // Ring after the enqueue: a token may be spurious, but it can never be missing. A full
        // bell means one is already pending.
        //
        // `ReceiptLost`, not `ExecutorDead`: the job is already in a queue by the time the bell is
        // rung, and the shutdown pass above executes a queued deny before observing the disconnect,
        // so a dead bell does not prove the command did nothing.
        if let Err(TrySendError::Disconnected(())) = self.bell.try_send(()) {
            return Err(SubmitError::ReceiptLost);
        }

        Ok(())
    }
}

/// The executor's end of the queues.
///
/// Named as a pair so the ordering rule is visible from the handle: `deny` is drained to empty
/// before `work` is touched, which makes the starvation bound "the work in front of this deny"
/// rather than "the work queue's depth". That unit is one commit window, and it holds for every
/// close: [`Executor::run_work_pass`] returns to `run`'s deny drain whenever it closes one, which
/// keeps the bound finite while ingest keeps arriving. The bound in full, including the one case
/// that costs two closes rather than one, is stated at [`Executor::run_work_pass`].
pub(crate) struct LifecycleQueues {
    work: Receiver<ExecutorWork>,
    deny: Receiver<Command>,
    /// The wake signal. Capacity one: see [`LifecycleHandle::bell`] and [`Executor::run`].
    bell: Receiver<()>,
}

/// What the executor needs to run a flush, gathered rather than passed one by one.
///
/// A struct because the alternative is a ten-argument `start_executor`, where the compiler stops
/// distinguishing two `u64`s and a caller can transpose them silently.
pub(crate) struct MaintenanceDeps {
    pub(crate) max_age_secs: u64,
    /// The tick's row trigger.
    pub(crate) max_items: usize,
    /// The entity-space coalesce's policy: see [`crate::coalesce::CoalescePolicy`].
    pub(crate) coalesce: crate::coalesce::CoalescePolicy,
    /// What a geometry publication needs to start the background refresh rules: see
    /// [`crate::refresh`].
    pub(crate) refresh: crate::refresh::RefreshDeps,
    /// The row-space merge's policy: see [`crate::merge`].
    pub(crate) merge: MergePolicy,
    /// The artifact row forms, shared for the one thing this thread does with them: rebuilding
    /// every level's projection inside the fold that invalidated it. A level is a deployment-wide
    /// artefact rather than a per-session value, so leaving it to the first request after the flip
    /// is a stall of tens of seconds for whoever arrives first.
    pub(crate) artifact_projections: Arc<crate::artifacts::ArtifactProjections>,
    /// The region decompositions (`crate::region`), pruned of superseded generations at every
    /// geometry swap exactly as the row-projection cache is: a row-space artefact keyed on a
    /// generation is unusable after it, and only retention is left to do.
    pub(crate) region_cache: Arc<
        crate::single_flight::SingleFlightCache<
            crate::region::RegionKey,
            crate::region::RegionDecomposition,
        >,
    >,
    /// The spatial levels' held shapes and per-segment pieces (`crate::shapes`): filled by the
    /// flush before its publication, rebuilt at a publication into a shape layer, re-resolved at
    /// the fold and the merge.
    pub(crate) shapes: Arc<crate::shapes::ShapeStore>,
    /// The lineages, shared for the half of the same warm that is theirs: see
    /// [`Executor::warm_artifact_caches`].
    pub(crate) lineages: Arc<crate::cut::Lineages>,
    /// The supplied-content tables, shared for the one thing this thread does with them: dropping
    /// a layer's when the layer is dropped, beside the two caches above.
    pub(crate) level_contents: Arc<crate::artifact_content::LevelContents>,
    /// The bundle root, from which the live prefix directory is derived per use: see
    /// [`Executor::prefix_dir`] and `Engine::bundle_root`.
    pub(crate) bundle_root: PathBuf,
    /// Where a rebuilt suggestion index is written: the engine's own cache directory, never the
    /// bundle.
    pub(crate) suggest_dir: PathBuf,
    pub(crate) identity_key: IdentityKey,
    /// The shared compute pool. A flush's segment write runs on it, off this thread, because this
    /// thread is the one that must reach a queued deny promptly.
    pub(crate) pool: Arc<rayon::ThreadPool>,
    /// The plugin's declared `max_distinct_terms`, carried here because promotion is the one path
    /// by which a caller grows the dictionary, and so the one declared bound that is enforced
    /// rather than trusted. See `flush::promote`.
    pub(crate) max_distinct_terms: u64,
    /// `EngineConfig::max_merged_segment_bytes` as configured, `None` where the deployment set
    /// nothing; not the resolved policy value, which always has one.
    ///
    /// The fold re-checks the base-segment relation against its own output, because a fold that
    /// shrank the base below an operator's configured merge cap would publish a deployment the
    /// next startup refuses to open. `tessera-server`'s loader checks only an explicitly set value
    /// (an unset one is derived from the base and cannot violate the relation), so the fold must
    /// tell the two apart, which the policy alone cannot.
    pub(crate) configured_merge_bytes: Option<u64>,
    /// Whether the coalesce and the merge run at all (`coalesce_enabled`, `merge_enabled`);
    /// whether a fold holds between its last pass and its submission (`fold_paused`), which lets a
    /// test land a flush inside a fold's flight; and whether a completed fold or merge is left
    /// undrained in its channel (`fold_publication_paused`, `merge_publication_paused`).
    ///
    /// `fold_paused` holds the fold thread before it clears `fold_in_flight`, so merge and
    /// coalesce stay suspended and nothing can publish under it. `fold_publication_paused` instead
    /// lets the thread finish and the suspension lift, but stops the executor draining the result,
    /// which is the state in which a merge or coalesce can dispatch, publish, and leave the fold
    /// planned against artefacts the live manifest no longer lists. The dispatchers now suspend on
    /// publication rather than on completion ([`Executor::fold_outstanding`]), so that state is no
    /// longer reachable through the executor; this hook is what holds a fold in it for testing the
    /// suspension: a merge offered ten ticks under a held fold takes none of them.
    /// `merge_publication_paused` opens the merge's own window: a flush publishing between a
    /// merge's plan and its publication, the interleaving under which the merge's rebase must keep
    /// the live manifest's watermark rather than its plan-time snapshot
    /// (`crate::merge::rebase_into`).
    pub(crate) switches: Arc<crate::switches::TestSwitches>,
    /// When a fold is dispatched with nobody asking for one: see
    /// [`crate::compact::CompactionSchedule`].
    pub(crate) compaction: crate::compact::CompactionSchedule,
}
