//! The write path: the single writer thread that owns the WAL, the two queues that feed it, and
//! the live state a handler reads before submitting.
//!
//! Carved out of `session.rs` by the stage-2.1 seam (Task 0a); given its executor by Task 3a.
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
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
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
    /// Total nanoseconds spent cloning the `IngestBuffer`/`Overlay` inside the apply step.
    ///
    /// **This is the deny-ack latency floor, and in stage 2.1 nothing caps it.** A deny's wait is
    /// bounded by "the work item currently executing", and that item includes a clone that is
    /// O(total buffered items) — plan 7b sizes it at 100–300 ms per clone at 1 M buffered items
    /// and 1–3 s at 10 M — while `flush_max_items` is inert until stage 2.2, so the buffer only
    /// grows. Counted from Task 3a rather than 7b precisely because 3a is where lifecycle §1.3's
    /// "never queued behind work of unbounded duration" first becomes a claim made in code.
    clone_nanos_total: AtomicU64,
    clone_nanos_max: AtomicU64,
}

/// A snapshot of [`ExecutorHealth`], for `/control/status` and for tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecutorStats {
    pub posture: ExecutorPosture,
    pub work_submitted: u64,
    pub deny_submitted: u64,
    pub clone_nanos_total: u64,
    pub clone_nanos_max: u64,
}

impl ExecutorHealth {
    fn new() -> Self {
        ExecutorHealth {
            posture: AtomicU8::new(ExecutorPosture::NotStarted as u8),
            work_submitted: AtomicU64::new(0),
            deny_submitted: AtomicU64::new(0),
            clone_nanos_total: AtomicU64::new(0),
            clone_nanos_max: AtomicU64::new(0),
        }
    }

    fn advance(&self, to: ExecutorPosture) {
        self.posture.fetch_max(to as u8, Ordering::SeqCst);
    }

    pub fn posture(&self) -> ExecutorPosture {
        ExecutorPosture::from_u8(self.posture.load(Ordering::SeqCst))
    }

    pub fn stats(&self) -> ExecutorStats {
        ExecutorStats {
            posture: self.posture(),
            work_submitted: self.work_submitted.load(Ordering::Relaxed),
            deny_submitted: self.deny_submitted.load(Ordering::Relaxed),
            clone_nanos_total: self.clone_nanos_total.load(Ordering::Relaxed),
            clone_nanos_max: self.clone_nanos_max.load(Ordering::Relaxed),
        }
    }

    fn record_clone(&self, nanos: u64) {
        self.clone_nanos_total.fetch_add(nanos, Ordering::Relaxed);
        self.clone_nanos_max.fetch_max(nanos, Ordering::Relaxed);
    }
}

/// Proof that a generation carrying a command's effect is live.
///
/// **The ack-ordering rule, in the type system.** [`Executor::ack`] cannot send a *successful*
/// receipt without one of these, so "ack before swap" — a client observing 200 for a suppression
/// not yet in force — does not compile. Statement order inside one function would not survive the
/// three rewrites this loop is scheduled for (7a's window, 7b's close policy, 9's coupled ack);
/// a required argument does.
///
/// It has exactly two producers, both named and both auditable in this file. If a third appears,
/// the guarantee is gone.
#[must_use = "a Published token exists to be handed to `ack`; dropping it discards the proof"]
pub(crate) struct Published(());

impl Published {
    /// Produced by the generation swap, and by nothing else on the success path.
    fn by_swap() -> Self {
        Published(())
    }

    /// The one case where a success ack is honest without this command having swapped: an
    /// idempotent replay of a `batch_id` whose **original** acceptance already swapped (contracts
    /// §3.4's replay rule). The effect is in force; it was simply put there by an earlier command.
    fn already_in_force() -> Self {
        Published(())
    }
}

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
}

impl std::fmt::Display for ExecutorStartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecutorStartError::AlreadyStarted => {
                write!(f, "this engine's write executor is already running")
            }
        }
    }
}

impl std::error::Error for ExecutorStartError {}

/// Why an accepted write did not succeed: it was never handed to the executor
/// ([`SubmitError`]), or it failed while executing ([`ExecError`]).
///
/// The two must stay distinguishable all the way to the HTTP boundary: `QueueFull` is a 429 the
/// caller should retry, `ExecutorDead` is a 503 and a not-ready node, and an `ExecError::Wal` on a
/// deny is a **500 for an effect that is nonetheless in force**. Task 3b owns the mapping table.
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

        let meter = Arc::new(WalMeter::new());
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

        health.advance(ExecutorPosture::Running);
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
                // Declared LAST so it drops FIRST during unwind: the posture must reach `Dead`
                // before the receivers disconnect, or there is a window in which a submitter
                // correctly sees `ExecutorDead` while `readyz` still reports ready.
                let _guard = DeathGuard(health);
                executor.run();
            })
            .expect("spawning the lifecycle thread");

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

/// Where one submitted [`Command`]'s [`Receipt`] is delivered.
///
/// A **synchronous** channel sender, and that is forced rather than chosen: `tessera-engine` has no
/// tokio dependency and must not acquire one, so the plan's two options for "receipt awaiting must
/// not block the reactor" collapse to one — the handler wraps its submit in `spawn_blocking`, and
/// this stays a plain `std::sync::mpsc` sender. `sync_channel(1)`, not `channel()`, so the
/// executor's ack send never blocks on a caller that has gone away.
pub type Responder = SyncSender<Receipt>;

/// One queued unit of work: what to do, and where to say it was done.
///
/// The responder travels **with** the command rather than being looked up afterwards, because Task
/// 8's join case needs several of them against one entry.
pub struct Job {
    pub command: Command,
    pub respond: Responder,
}

/// The handler-side end of the write executor: two queues, and the asymmetry between them.
///
/// **Not `Clone`, and that is load-bearing** — [`WritePath::drop`] joins the executor thread, which
/// terminates only when every sender has disconnected. One owner means the join always completes.
pub struct LifecycleHandle {
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
    /// Submit an ingest command and wait for its receipt. **May 429** (queue full).
    pub fn submit(&self, command: Command) -> std::result::Result<Receipt, SubmitError> {
        self.enqueue(command)
    }

    /// Submit a `/control/changes` command and wait for its receipt. **Never 429**, but it can
    /// still report [`SubmitError::ExecutorDead`]: a deny is never refused for *load*, which is not
    /// the same as never refused.
    pub fn submit_deny(&self, command: Command) -> std::result::Result<Receipt, SubmitError> {
        self.enqueue(command)
    }

    /// The lane is chosen by the **command**, not by which method was called.
    ///
    /// Both public methods funnel here, so `submit(Command::Change { .. })` cannot put a
    /// suppression on the bounded queue and 429 it — contracts §3.1 forbids `/control/changes`
    /// answering 429, and a one-line slip at a handler is exactly how that would happen.
    /// [`Command::is_never_shed`] exists to make this structural and this is the only place it is
    /// consulted.
    fn enqueue(&self, command: Command) -> std::result::Result<Receipt, SubmitError> {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let job = Job {
            command,
            respond: tx,
        };

        if job.command.is_never_shed() {
            self.deny.send(job).map_err(|_| SubmitError::ExecutorDead)?;
            // Bumped **after** the enqueue and **before** the blocking wait, so a test can observe
            // "the deny is queued" as a condition rather than betting on a sleep. Also the operand
            // Task 6's `retry_after_s` is derived from.
            self.health.deny_submitted.fetch_add(1, Ordering::SeqCst);
        } else {
            self.work.try_send(job).map_err(|e| match e {
                // `retry_after_s` is a placeholder until Task 6 derives it from window age and
                // observed drain rate; the variant, which is what 3b maps to 429, is already right.
                TrySendError::Full(_) => SubmitError::QueueFull { retry_after_s: 1 },
                TrySendError::Disconnected(_) => SubmitError::ExecutorDead,
            })?;
            self.health.work_submitted.fetch_add(1, Ordering::SeqCst);
        }

        // Ring **after** the enqueue: a token may be spurious, but it can never be missing.
        // A full bell means one is already pending, which says everything this one would.
        if let Err(TrySendError::Disconnected(())) = self.bell.try_send(()) {
            return Err(SubmitError::ExecutorDead);
        }

        // A dropped responder means the executor died holding this job — never `Ok`. Answering
        // anything else here is the false-202 `SubmitError`'s own doc calls the worst available
        // outcome.
        rx.recv().map_err(|_| SubmitError::ExecutorDead)
    }
}

/// The executor's end of the queues.
///
/// Named as a pair so the ordering rule is visible from the handle: `deny` is drained to empty
/// before `work` is touched, which is what makes the starvation bound "the work item currently
/// executing" rather than "the work queue's depth".
pub struct LifecycleQueues {
    pub work: Receiver<Job>,
    pub deny: Receiver<Job>,
    /// The wake signal. Capacity one — see [`LifecycleHandle::bell`] and [`Executor::run`].
    pub bell: Receiver<()>,
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
    /// **Shutdown discards rather than executes.** A submitter holds `&self` on the handle for the
    /// whole call, so `bell.recv()` cannot return `Err` while any submit is in flight — the three
    /// senders live in one struct and disconnect together. Anything still queued at that point had
    /// no waiter left to ack.
    ///
    /// *At-most-one-work-item is Task 3a's shape, not the executor's permanent one*: Task 7a drains
    /// work into a commit window. Leftover tokens stay harmless under that change.
    fn run(&mut self) {
        loop {
            while let Ok(job) = self.queues.deny.try_recv() {
                self.execute(job);
            }
            if let Ok(job) = self.queues.work.try_recv() {
                self.execute(job);
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
                self.ack(
                    respond,
                    Ack::Ingested {
                        entity_ids: prev_ids,
                    },
                    Published::already_in_force(),
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
        self.pause_point();
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
                self.pause_point();
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
            .record_clone(started.elapsed().as_nanos() as u64);
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
    fn ack(&self, respond: &Responder, ack: Ack, _proof: Published) {
        #[cfg(feature = "fault-injection")]
        if let Some(faults) = &self.faults {
            faults.record(tessera_lifecycle::faults::Step::Ack);
        }
        // A dropped responder is not an error: the caller's connection went away, and by then the
        // effect is already in force.
        let _ = respond.send(Receipt::ok(ack));
    }

    fn ack_failed(&self, respond: &Responder, error: ExecError) {
        #[cfg(feature = "fault-injection")]
        if let Some(faults) = &self.faults {
            faults.record(tessera_lifecycle::faults::Step::Ack);
        }
        let _ = respond.send(Receipt::failed(error));
    }

    /// The armed kill point **between fsync and swap** — durable, not yet in force.
    ///
    /// That exact position is needed twice by the plan and is why the hook lands at 3a rather than
    /// at stage 2.4: Task 9's coupled-ack test parks here to prove the receipt is still outstanding
    /// while the effect is not yet visible, and Task 8's crash test needs a process that died here
    /// to replay rather than reallocate (lifecycle §8's "after fsync, before swap" row, whose
    /// "at risk: none" is precisely what it exists to demonstrate). Test builds only; a no-op
    /// otherwise.
    #[cfg(feature = "fault-injection")]
    fn pause_point(&self) {
        use tessera_lifecycle::faults::PauseAction;
        let Some(faults) = &self.faults else { return };
        match faults.pause_point() {
            None | Some(PauseAction::Stall) => {}
            Some(PauseAction::Abort) => {
                // As a killed process would: no ack, the responder drops, the caller sees
                // `ExecutorDead`, and the fsynced record survives for replay (lifecycle §8's
                // "after fsync, before swap" row).
                panic!("fault-injection: executor aborted at the pause point");
            }
            Some(PauseAction::Panic) => {
                panic!("fault-injection: executor panicked at the pause point")
            }
        }
    }

    #[cfg(not(feature = "fault-injection"))]
    fn pause_point(&self) {}
}
