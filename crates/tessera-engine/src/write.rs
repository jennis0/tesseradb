//! The write path: the WAL handle, the I9 allocator, the live external-id maps, the descriptor
//! resolver's extension state and the `/control/ingest` idempotency index — everything the
//! acceptance paths mutate, behind one seam.
//!
//! Carved out of `session.rs` (Phase 2 stage 2.1, Task 0a) so the write-path work and the
//! read-path work can proceed in separate files. The carve is deliberately **not** a wholesale
//! move of every field: four of them (`allocator`, `established`, `established_inverse`,
//! `resolver_state`) are read by `Engine`'s own read paths as well as written here, so
//! [`WritePath`] owns the mutable state and exposes read accessors, and `Engine` composes over
//! them (`Engine::resolve_external_id`, `Engine::resolve_external_ids`, `Engine::external_id_of`
//! stay in `session.rs`, where the immutable, bundle-derived `external_index` they fall back to
//! also lives).
//!
//! `generation` and `dict` are **shared** with the `Engine`, not owned by this type: the WAL
//! critical section below swaps the generation pointer, and `resolve_terms` resolves against the
//! same bundle dictionary `Engine::authorise` looks descriptors up in.

use std::sync::{Arc, Mutex};

use rustc_hash::FxHashMap;

use tessera_authz::Dict;
use tessera_lifecycle::alloc::{Allocator, PendingItem};
use tessera_lifecycle::buffer::DescriptorResolver;
use tessera_lifecycle::wal::{ChangeOp, Wal, WalError, WalRecord, WalRow};
use tessera_lifecycle::{assign_sorted, Overlay};
use tessera_plugin::Descriptor;
use tessera_types::{EntityId, TermId};

use crate::{Generation, GenerationHandle};

/// The mutable write-side state of a running engine. See this module's doc for why the split
/// between this type and `Engine` falls where it does.
pub(crate) struct WritePath {
    /// The write-ahead log handle, kept open for future ingest/change acceptance (Task 13); not
    /// exercised by this task's authorise/viewport paths.
    wal: Mutex<Wal>,
    /// The I9 allocator, seeded at open (`max(manifest high-water, WAL high-water)`); not
    /// exercised by this task's authorise/viewport paths, but seeding it here — rather than
    /// leaving it to whichever task first needs it — is what the brief asks `Engine::open` to do.
    allocator: Mutex<Allocator>,
    /// External ids established live (bundle replay's `IngestBatch` rows, plus every
    /// subsequently-accepted `/control/ingest` batch) — consulted before falling back to
    /// `external_index`, so a `/control/changes` naming an item ingested only seconds ago (not
    /// yet in any bundle) still resolves (Task 13).
    established: Mutex<FxHashMap<Vec<u8>, EntityId>>,
    /// The inverse of `established` — `entity -> external_id` — for the drill-down direction
    /// (Important I-9). Written by the same two writers as `established` (`Engine::open`'s
    /// replay and `WritePath::accept_ingest`), in the same critical section each time, so the two
    /// maps can never disagree about the same item (task-9 brief).
    established_inverse: Mutex<FxHashMap<EntityId, Vec<u8>>>,
    /// The descriptor resolver's extension state (dictionary-miss descriptors interned in
    /// replay/accept order), detached from replay's borrow of `dict` and resumed on every live
    /// resolution — see `DescriptorResolver::resume`'s doc (Task 13).
    resolver_state: Mutex<(FxHashMap<Vec<u8>, TermId>, u32)>,
    /// `/control/ingest` idempotency index: accepted batch id -> `(body hash, entity ids)` it was
    /// accepted with (Task 13). The entity ids ride along so a byte-identical replay can answer
    /// with the same `tessera_id`s per row (contracts §3.4 r6) without needing to re-resolve them
    /// from `external_id` — which a null-external-id row has none of.
    #[allow(clippy::type_complexity)]
    accepted_batches: Mutex<FxHashMap<String, ([u8; 32], Vec<EntityId>)>>,
    /// Shared with the `Engine`, not owned: the acceptance paths below publish their new
    /// generation through this exact pointer, which is the same one every read path loads from.
    generation: Arc<GenerationHandle>,
    /// Shared with the `Engine`, not owned: the bundle dictionary [`Self::resolve_terms`] resolves
    /// against is the same one `Engine::authorise` looks granted descriptors up in.
    dict: Arc<Dict>,
}

impl WritePath {
    /// Assemble the write path from the state `Engine::open` reconstructed at open: the WAL
    /// handle, the seeded allocator, WAL replay's `established` map and its inverse, the detached
    /// resolver extension state, and the idempotency index rebuilt from the replayed records.
    ///
    /// **One call, and deliberately so.** Two later tracks both edit `Engine::open`; a one-line
    /// construction site conflicts trivially where a twenty-line one does not.
    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    pub(crate) fn new(
        wal: Wal,
        allocator: Allocator,
        established: FxHashMap<Vec<u8>, EntityId>,
        established_inverse: FxHashMap<EntityId, Vec<u8>>,
        resolver_state: (FxHashMap<Vec<u8>, TermId>, u32),
        accepted_batches: FxHashMap<String, ([u8; 32], Vec<EntityId>)>,
        generation: Arc<GenerationHandle>,
        dict: Arc<Dict>,
    ) -> Self {
        WritePath {
            wal: Mutex::new(wal),
            allocator: Mutex::new(allocator),
            established: Mutex::new(established),
            established_inverse: Mutex::new(established_inverse),
            resolver_state: Mutex::new(resolver_state),
            accepted_batches: Mutex::new(accepted_batches),
            generation,
            dict,
        }
    }

    /// The I9 allocator's current high-water mark — exposed for tests/diagnostics confirming
    /// `Engine::open`'s seeding rule (`max(manifest high-water, WAL high-water)`); not otherwise
    /// used by this task's request paths.
    pub(crate) fn allocator_high_water(&self) -> u64 {
        self.allocator.lock().unwrap().high_water()
    }

    /// The `EntityId` an external id was established live under (bundle replay's own
    /// `IngestBatch` rows, plus every `/control/ingest` batch accepted since), or `None` if the
    /// live map has never seen it. The bundle's own extent is `Engine`'s to consult — see
    /// `Engine::resolve_external_id`, which composes this accessor with that fallback.
    pub(crate) fn established_entity(&self, external_id: &[u8]) -> Option<EntityId> {
        self.established.lock().unwrap().get(external_id).copied()
    }

    /// Batch form of [`Self::established_entity`], taking the live map's lock **once** for the
    /// whole batch. That single critical section is the behaviour `Engine::resolve_external_ids`
    /// has always had, and it is not merely an optimisation: per-key locking would let an
    /// acceptance land between two keys of one duplicate check, so the batch would be answered
    /// from two different snapshots of the live map.
    ///
    /// Returns one `Option<EntityId>` per input, in the caller's given order.
    pub(crate) fn established_entities(&self, external_ids: &[Vec<u8>]) -> Vec<Option<EntityId>> {
        let established = self.established.lock().unwrap();
        external_ids
            .iter()
            .map(|id| established.get(id.as_slice()).copied())
            .collect()
    }

    /// The external id an entity was established live under, or `None` if the live inverse map has
    /// never seen it — the drill-down direction (Important I-9). `None` here is not an answer to
    /// `/v1/items`; it means "ask the bundle", which `Engine::external_id_of` does.
    pub(crate) fn established_external_id(&self, entity: EntityId) -> Option<Vec<u8>> {
        self.established_inverse
            .lock()
            .unwrap()
            .get(&entity)
            .cloned()
    }

    /// Resolve raw term descriptors to `TermId`s: a dictionary hit resolves to its durable,
    /// bundle-relative id; a miss is interned into the process-lifetime extension state, resumed
    /// from wherever WAL replay (or the previous call to this method) left off — see
    /// `DescriptorResolver::resume`'s doc for why restarting that sequence per call would be
    /// fail-open.
    ///
    /// **Durability-ordering exemption (review finding, Important 3):** ideally every call to
    /// this method happens only after the record that will carry its descriptors is durably WAL
    ///-appended and fsynced — otherwise an extension id can be minted in-process for a batch
    /// whose append then fails, leaving the live resolver's state one step ahead of what a
    /// restart-replay would ever reconstruct from the WAL alone. `WritePath::accept_change` honours
    /// that ordering (it resolves only after its `Change` record's append/fsync succeeds).
    /// `/control/ingest` is a deliberate, structural exception: signature-sorted entity-id
    /// assignment (I9/§11.1, `allocate_sorted`) needs each item's resolved terms to compute its
    /// sort key *before* the item's `WalRow` (which carries the assigned id) can even be framed
    /// for append — so this call cannot be deferred past the durability boundary for ingest
    /// without abandoning signature-sorted assignment itself. This is judged safe in practice
    /// (not merely convenient) because an extension id is, by construction, unsatisfiable by any
    /// session's `satisfied` set (`tessera_lifecycle::buffer`'s module doc) — a live/replay
    /// mismatch in exactly *which* extension id a novel descriptor got renumbers internal
    /// bookkeeping only, never a visibility outcome.
    pub(crate) fn resolve_terms(&self, descriptors: &[Descriptor]) -> Vec<TermId> {
        let mut state = self.resolver_state.lock().unwrap();
        let (extension, next_extension_id) = std::mem::take(&mut *state);
        let mut resolver = DescriptorResolver::resume(&self.dict, extension, next_extension_id);
        let ids = descriptors.iter().map(|d| resolver.resolve(d)).collect();
        *state = resolver.into_state();
        ids
    }

    /// Allocate entity ids for a freshly-parsed ingest batch, in signature-sorted order (I9/§11.1)
    /// — must be called after every item's `terms` field is populated (via
    /// [`WritePath::resolve_terms`]) and before the batch's `WalRow`s are framed for WAL append.
    ///
    /// **Propagates `AllocError`** rather than silently discarding it (a pre-existing
    /// `unused_must_use` gap this task closes incidentally, to keep `cargo clippy -D warnings`
    /// green): the allocator's ceiling is a real, reachable failure (I9's u32 cap), and an
    /// ingest batch left with unassigned or partially-assigned ids would frame `WalRow`s the WAL
    /// must never see.
    pub(crate) fn allocate_sorted(
        &self,
        items: &mut [PendingItem],
    ) -> std::result::Result<(), tessera_lifecycle::alloc::AllocError> {
        let mut alloc = self.allocator.lock().unwrap();
        assign_sorted(items, &mut alloc)
    }

    /// The body hash and per-row entity ids a batch id was previously accepted with, if any — the
    /// idempotency check for `/control/ingest`'s replay rule (R5): equal hash -> 200 no-op
    /// (returning the same `tessera_id`s, via the entity ids here); different hash -> 409.
    pub(crate) fn accepted_batch(&self, batch_id: &str) -> Option<([u8; 32], Vec<EntityId>)> {
        self.accepted_batches.lock().unwrap().get(batch_id).cloned()
    }

    /// Record a batch id as accepted. Must only be called after the batch's `IngestBatch` record
    /// has been WAL-appended and fsynced (the ack contract) — this index is purely an in-memory
    /// accelerant for the idempotency check above, not itself a durability boundary.
    pub(crate) fn record_accepted_batch(
        &self,
        batch_id: String,
        body_hash: [u8; 32],
        entity_ids: Vec<EntityId>,
    ) {
        self.accepted_batches
            .lock()
            .unwrap()
            .insert(batch_id, (body_hash, entity_ids));
    }

    /// Accept an ingest batch atomically: WAL append -> fsync -> apply (buffer clone + insert) ->
    /// generation swap, all while holding `self.wal`'s lock (**review finding, Critical 1**: the
    /// previous split — append/fsync under the caller's own WAL lock, then a *separate*,
    /// unlocked `apply_ingest`/`apply_change` call — let two concurrent acceptances race on
    /// `ArcSwap::load_full`/`store`: both load the same pre-swap generation, both clone it, and
    /// whichever `store`s last silently discards the other's already-fsynced, already-acked
    /// change with no error. Holding the WAL mutex across the *entire* append-through-swap
    /// sequence, for both this method and [`WritePath::accept_change`], serialises every generation
    /// swap through one lock: the second of two concurrent acceptances cannot even begin its
    /// `load_full()` until the first has finished its `store()`, so it always builds its new
    /// generation on top of the first's effect rather than racing it.
    ///
    /// `rows` carry raw descriptor bytes (never `TermId`s — see `WalRow`'s doc); `terms` is each
    /// row's already-resolved term set, in the same order (resolved by the caller via
    /// [`WritePath::resolve_terms`] before this call — see that method's doc for why ingest,
    /// specifically, cannot defer resolution past this call the way [`WritePath::accept_change`]
    /// does).
    ///
    /// On success, also records `batch_id`/`body_hash` as accepted (the idempotency index) before
    /// releasing the lock, so a concurrent replay of the same batch id can never observe a window
    /// where the generation has swapped but the idempotency index hasn't caught up yet.
    ///
    /// Returns each accepted row's `EntityId`, in the same order as `rows` — never the caller's
    /// raw entity ids to keep (I10 stays server-side), but the caller (`/control/ingest`) needs
    /// them for exactly as long as it takes to turn each into a `tessera_id` (via
    /// [`crate::session::Engine::tessera_id_of`]) for the 200 response (contracts §3.4 r6). Also
    /// recorded, keyed by `batch_id`, so a byte-identical replay of an already-acked batch can
    /// answer with the same `tessera_id`s without re-deriving them from `external_id` — which
    /// would not work at all for a row that has none.
    pub(crate) fn accept_ingest(
        &self,
        rows: Vec<WalRow>,
        terms: Vec<Vec<TermId>>,
        batch_id: String,
        body_hash: [u8; 32],
    ) -> std::result::Result<Vec<EntityId>, WalError> {
        debug_assert_eq!(rows.len(), terms.len());
        let record = WalRecord::IngestBatch {
            batch_id: batch_id.clone(),
            body_hash,
            rows: rows.clone(),
        };

        let mut wal = self.wal.lock().unwrap();
        wal.append(&record)?;
        wal.fsync()?;

        let generation = self.generation.load_full();
        let mut buffer = (*generation.buffer).clone();
        let mut established = self.established.lock().unwrap();
        // `established` and `established_inverse` are updated together, in this one critical
        // section, so a `/control/changes` lookup and a `/v1/items` drill-down can never
        // disagree about the same item (task-9 brief, Important I-9).
        let mut established_inverse = self.established_inverse.lock().unwrap();
        for (row, row_terms) in rows.iter().zip(&terms) {
            // Contracts §3.4 r6: no external id means no sidecar entry and nothing to establish
            // here either -- the item is addressable only by its `tessera_id`. `None` must never
            // collide with `None`, so this simply skips the insert rather than inserting under a
            // shared "empty" key.
            if let Some(external_id) = &row.external_id {
                established.insert(external_id.clone(), row.entity_id);
                established_inverse.insert(row.entity_id, external_id.clone());
            }
            buffer.insert_row_with_terms(row, row_terms.clone());
        }
        drop(established);
        drop(established_inverse);

        let next = Generation {
            prefix: generation.prefix.clone(),
            segments_version: generation.segments_version,
            watermark: generation.watermark,
            bundle: Arc::clone(&generation.bundle),
            overlay_version: generation.overlay_version + 1,
            overlay: Arc::clone(&generation.overlay),
            buffer: Arc::new(buffer),
        };
        self.generation.store(Arc::new(next));

        let entity_ids: Vec<EntityId> = rows.iter().map(|row| row.entity_id).collect();
        self.accepted_batches
            .lock()
            .unwrap()
            .insert(batch_id, (body_hash, entity_ids.clone()));

        drop(wal);
        Ok(entity_ids)
    }

    /// Accept one `/control/changes` disposition change atomically: WAL append -> fsync -> apply
    /// (overlay clone + `Overlay::apply`) -> generation swap, all while holding `self.wal`'s lock
    /// — see [`WritePath::accept_ingest`]'s doc for why (Critical 1) and this crate's `ChangeOp`
    /// doc for the three retirement rules this composes with.
    ///
    /// **Deny-op append failure** (lifecycle §4): if the append/fsync genuinely fails and `op` is
    /// `Delete`/`Suppress`, the change is still applied (the item hidden immediately) before this
    /// returns `Err` — never a refusal that leaves a deny unapplied. For any other op, a failed
    /// append/fsync applies nothing.
    ///
    /// **Durability-ordering fix (review finding, Important 3):** `raw_descriptors` (present only
    /// for `Predicate`) are resolved to `TermId`s via [`WritePath::resolve_terms`] *inside* this
    /// method, only after the append/fsync has already succeeded — never before. Unlike ingest
    /// (see `resolve_terms`'s doc for why that path is a structural exception), a change's
    /// resolved terms are needed only for the subsequent `Overlay::apply` call, not for anything
    /// that must be decided before the record can be framed, so there is no reason to mint an
    /// extension id for a record that might never become durable. `Delete`/`Suppress`/
    /// `Unsuppress` never carry descriptors, so the deny-op append-failure path never resolves
    /// anything either.
    pub(crate) fn accept_change(
        &self,
        external_id: Vec<u8>,
        entity: EntityId,
        op: ChangeOp,
        raw_descriptors: Option<Vec<Vec<u8>>>,
    ) -> std::result::Result<(), WalError> {
        let record = WalRecord::Change {
            external_id,
            op,
            descriptors: raw_descriptors.clone(),
        };

        let mut wal = self.wal.lock().unwrap();
        let append_result = wal.append(&record).and_then(|()| wal.fsync());

        let result = match append_result {
            Ok(_) => {
                let terms = raw_descriptors.as_ref().map(|ds| self.resolve_terms(ds));
                self.apply_change_locked(entity, op, terms);
                Ok(())
            }
            Err(e) => {
                if matches!(op, ChangeOp::Delete | ChangeOp::Suppress) {
                    self.apply_change_locked(entity, op, None);
                }
                Err(e)
            }
        };

        drop(wal);
        result
    }

    /// The overlay-clone-and-swap step shared by both of [`WritePath::accept_change`]'s outcomes.
    /// Private: called only while `self.wal`'s lock is held (see [`WritePath::accept_ingest`]'s doc
    /// for why every generation swap must be serialised through that one lock). Pins are never
    /// invalidated by this (I11: a pin fixes `(prefix, segments_version)` only, and this bumps
    /// `overlay_version`, not `segments_version`) — lifecycle §2.3's rule that a suppression
    /// applies to a pinned request the moment it is accepted, without expiring the pin.
    fn apply_change_locked(&self, entity: EntityId, op: ChangeOp, terms: Option<Vec<TermId>>) {
        let generation = self.generation.load_full();
        let mut overlay: Overlay = (*generation.overlay).clone();
        overlay.apply(entity, op, terms);

        let next = Generation {
            prefix: generation.prefix.clone(),
            segments_version: generation.segments_version,
            watermark: generation.watermark,
            bundle: Arc::clone(&generation.bundle),
            overlay_version: generation.overlay_version + 1,
            overlay: Arc::new(overlay),
            buffer: Arc::clone(&generation.buffer),
        };
        self.generation.store(Arc::new(next));
    }
}
