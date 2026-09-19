//! The control-plane calls: resolving, writing, declaring and publishing.

use std::sync::atomic::Ordering;

use tessera_lifecycle::command::UnallocatedRow;
use tessera_lifecycle::wal::ChangeOp;
use tessera_plugin::Descriptor;
use tessera_store::StoreError;
use tessera_types::{EntityId, IdentityError, TermId, TesseraId};

use crate::engine::Engine;
use crate::error::{EngineError, Result};
use crate::write::joined::flushed_terms_of;
use crate::Generation;

impl Engine {
    /// Resolve raw term descriptors to `TermId`s (dictionary hit → durable bundle-relative id;
    /// miss → an id interned in this process's extension state, resumed across calls).
    ///
    /// **Caller obligation — the durability-ordering exemption.** Every other resolution site
    /// resolves *after* the record carrying the descriptors is durably appended and fsynced, so a
    /// batch whose append fails cannot leave the live resolver a step ahead of what a replay would
    /// reconstruct. `/control/ingest` is the one structural exception: signature-sorted assignment
    /// (I9/§11.1) needs each item's terms to compute its sort key before its `WalRow` can be
    /// framed at all. Judged safe because an extension id is by construction unsatisfiable by any
    /// session, so a live/replay mismatch renumbers bookkeeping and never a visibility outcome —
    /// the full argument, and why it is not merely convenient, is at `WritePath::resolve_terms`.
    ///
    /// *(Restated here rather than only cross-referenced: `WritePath` is `pub(crate)`, so rustdoc
    /// renders none of its docs for a reader of this public API, and a bare pointer to an invisible
    /// page is not an obligation a caller can honour.)*
    pub fn resolve_terms(&self, descriptors: &[Descriptor]) -> Vec<TermId> {
        self.write
            .live()
            .resolve_terms(&self.generation.load().dict, descriptors)
    }

    /// Resolve an external id to its `EntityId`, checking every item established live (bundle
    /// replay's own `IngestBatch` rows, plus every `/control/ingest` batch accepted since) before
    /// falling back to the bundle's own `entities/external-ids-0.arrow` extent.
    ///
    /// **Fallible**, and that is the point: a real sidecar failure — digest
    /// mismatch, out-of-order extent, corrupt locator — propagates as `Err` rather than panicking
    /// inside `ExternalIdIndex::resolve`. A `/control/changes` request naming
    /// an external id backed by a corrupt sidecar gets a `500`, never a silent "unknown" *or* a
    /// panicked worker.
    pub fn resolve_external_id(
        &self,
        external_id: &[u8],
    ) -> std::result::Result<Option<EntityId>, StoreError> {
        if let Some(entity) = self.write.live().established_entity(external_id) {
            return Ok(Some(entity));
        }
        self.generation.load().external_index.resolve(external_id)
    }

    /// Invert `tessera_id`s to entity ids for the admin plane, all-or-nothing.
    ///
    /// **The idset is checked first, against the same generation the inversions use** — one
    /// `load_full`, exactly as [`crate::viewport::Engine::item`] does it, so a swap landing
    /// mid-call cannot validate the idset against one snapshot and invert under another. A
    /// mismatch is [`EngineError::StaleIdSet`] and **decides before any inversion happens**: a
    /// caller holding a list gathered before a key rotation is refused wholesale rather than
    /// having its identifiers reinterpreted under the new key, which would name different live
    /// items (decision 0025).
    ///
    /// **`None` for an identifier that names nothing**, per position, so a caller learns which.
    /// The permutation is total — every `u64` inverts to *something* — so the range check is the
    /// whole of the misdirection guard: a shard that is not this one, or an entity in a range the
    /// allocator has never issued from, cannot name an item this deployment ever issued. Both are
    /// facts about the identifier space rather than about any item's visibility, and this is the
    /// admin plane (R5), so refusing precisely discloses nothing a caller could not compute.
    ///
    /// **The issued range is two ranges, and reading it as one is how layer suppression breaks.**
    /// Points are below the high-water mark; row-less entities — a layer's own, so that a
    /// suppression against it is an ordinary `/control/changes` entry — are at or above the
    /// row-less mark. A single `entity < high_water` test refuses every layer identifier this
    /// deployment has ever handed out, and the symptom is not an error anyone would connect to
    /// this line: it is that suppressing a layer answers *no such thing*. What names nothing is the
    /// **gap between the marks**, which is exactly the unissued space.
    ///
    /// Ordered as the caller supplied, like [`Self::resolve_external_ids`], so a refusal can name
    /// the offending position.
    pub fn resolve_tessera_ids(
        &self,
        ids: &[TesseraId],
        idset: u32,
    ) -> Result<Vec<Option<EntityId>>> {
        let generation = self.generation.load_full();
        if idset != generation.bundle.manifest.identity.idset {
            return Err(EngineError::StaleIdSet);
        }
        let shard = generation.bundle.manifest.identity.shard_id;
        let high_water = self.allocator_high_water();
        let low_water = self.allocator_low_water();
        Ok(ids
            .iter()
            .map(|id| {
                let (id_shard, entity) = self.identity_key.invert(*id);
                let issued = entity.raw() < high_water || entity.raw() >= low_water;
                (id_shard == shard && issued).then_some(entity)
            })
            .collect())
    }

    /// Does `view` hold a row for `entity` — **the "already in the view" arm of the ingest join
    /// rule** (`views.md` §4)?
    ///
    /// "In the view" is the view's permutation **and** the commit window's buffer: a row accepted
    /// but not yet flushed is in no permutation, and a check that missed it would let two batches
    /// hand one flush two rows for one entity in one view, which the single-valued permutation
    /// cannot hold. The window's own open entries are covered upstream, by the early close a held
    /// external id already forces.
    ///
    /// One permutation read and one hash lookup; nothing walks.
    pub fn view_holds(&self, entity: EntityId, view: &str) -> bool {
        let generation = self.generation();
        generation.bundle.partitions.values().any(|partition| {
            partition
                .views
                .get(view)
                .is_some_and(|data| data.row_space.row_of(entity).is_some())
        }) || generation.buffer.contains_in_view(entity, view)
    }

    /// An already-flushed entity's **full** term set, ascending, from the entity→term transpose
    /// (contracts §2.4) — the join rule's label arm, once the entity's own row has left the buffer
    /// (`views.md` §4).
    ///
    /// **The full set, and it never leaves the server.** This is the opposite surface from the
    /// drill-down's `labels`, which serves the intersection with the asking session: this compares
    /// a *writer's* batch against what the deployment already holds, and equality is the whole
    /// question — an arm that compared only the terms the writer named would accept a batch that
    /// dropped one. Nothing derived from it is returned; the refusal names the row, never a term.
    ///
    /// `None` where no layer holds a list for the entity, which is *unknown* rather than *empty*
    /// and leaves the comparison unavailable exactly as an empty buffer does. `Some(vec![])` is a
    /// real answer: an item may legitimately carry no label.
    ///
    /// **A malformed layer is `None`, not a wrong answer — and it is logged, not swallowed.** The
    /// transpose refuses a bad offset pair rather than truncating
    /// (`tessera_store::entity_terms`), and this is a *report*, not an authorisation: the join it
    /// guards is inert either way (a joining row carries no descriptors), so a corrupt artefact
    /// loses the refusal rather than turning a caller's batch into a server error. That is the
    /// recoverable-and-discloses-nothing side of the line, where the posture is *report loudly and
    /// let the operator decide* — so the warning below fires, and the same corruption is a hard
    /// error on the drill-down path, which propagates it.
    ///
    /// **The warning names the artefact and not the entity** (**I10**, contracts §4). The
    /// byte-scanner sweeps payloads *and logs* for entity ids, and `crate`'s store follows the
    /// external-ID sidecar's rule at the same standard: the error carries the file and the shape
    /// of the inconsistency, which is what an operator chasing a systematic build or flush defect
    /// needs, and naming the slot buys nothing an entity-independent message does not.
    pub fn flushed_terms(&self, entity: EntityId) -> Option<Vec<TermId>> {
        flushed_terms_of(&self.generation(), entity)
    }

    /// Batch form of [`Self::resolve_external_id`] for `/control/ingest`'s duplicate check
    /// (contracts §3.1 r6): live map first for the *whole* batch (Important I-8 — `established`
    /// holds every id ingested since the build, which the sidecar cannot see at all, and is
    /// exactly where a retried client batch's duplicate lives), then one batched, sorted sidecar
    /// call for whatever residual keys the live map didn't resolve — each bundle extent is opened
    /// at most once regardless of batch size, never once per row.
    ///
    /// Returns one `Option<EntityId>` per input, in the caller's given order.
    pub fn resolve_external_ids(
        &self,
        external_ids: &[Vec<u8>],
    ) -> std::result::Result<Vec<Option<EntityId>>, StoreError> {
        let mut results: Vec<Option<EntityId>> = self.write.live().established_entities(external_ids);

        let residual_positions: Vec<usize> = results
            .iter()
            .enumerate()
            .filter_map(|(i, r)| if r.is_none() { Some(i) } else { None })
            .collect();
        if residual_positions.is_empty() {
            return Ok(results);
        }
        let residual_keys: Vec<Vec<u8>> = residual_positions
            .iter()
            .map(|&i| external_ids[i].clone())
            .collect();
        let residual_results = self
            .generation
            .load()
            .external_index
            .resolve_many(&residual_keys)?;
        for (pos, resolved) in residual_positions.into_iter().zip(residual_results) {
            results[pos] = resolved;
        }
        Ok(results)
    }

    /// `entity -> external_id` for drill-down (`/v1/items`). Ordering mirrors
    /// `resolve_external_id`'s live-map-first rule, running the other way: post-build ingest has
    /// no locator slot and no extent entry, so the live map (`established_inverse`) is consulted
    /// first — Important I-9. `Ok(None)` means "this item genuinely has no caller external id", a
    /// legitimate state since `external_id` is optional on ingest; it must never mean "I could
    /// not find out". A `/v1/items` entity that is below the live high-water, past this bundle's
    /// locator, and unknown to the live map is an inconsistency, not an absent external id, and
    /// fails closed as `Err(StoreError::InvalidSidecar)` — see
    /// `ExternalIdSidecar::external_id_of_checked`'s doc.
    pub fn external_id_of(
        &self,
        entity: EntityId,
    ) -> std::result::Result<Option<Vec<u8>>, StoreError> {
        self.external_id_of_in(&self.generation.load(), entity)
    }

    /// [`Self::external_id_of`] against a generation the caller already loaded.
    ///
    /// `Engine::item` is the caller, and it must not take a second `load()`: the sidecar is now
    /// per-generation (a fold rewrites it — see [`Generation::external_index`]), so a drill-down
    /// that resolved its row against one generation and its external id against another would be
    /// exactly the cross-generation mix I11's within-request rule forbids, reached through the one
    /// field that used to be process-wide.
    pub(crate) fn external_id_of_in(
        &self,
        generation: &Generation,
        entity: EntityId,
    ) -> std::result::Result<Option<Vec<u8>>, StoreError> {
        if let Some(external_id) = self.write.live().established_external_id(entity) {
            return Ok(Some(external_id));
        }
        generation
            .external_index
            .external_id_of_checked(entity, self.allocator_high_water())
    }

    /// The body hash and per-row entity ids a batch id was previously accepted with, if any — the
    /// idempotency check for `/control/ingest`'s replay rule: equal hash -> 200 no-op (returning
    /// the same `tessera_id`s, via the entity ids here); different hash -> 409.
    ///
    /// **An accelerant, never the authority.** The same check runs again on the executor, which is
    /// the only place it can be race-free (see `WritePath`'s executor). A handler consulting this
    /// is saving a queue round-trip on the common case, not deciding anything.
    pub fn accepted_batch(&self, batch_id: &str) -> Option<([u8; 32], Vec<EntityId>)> {
        self.write.live().accepted_batch(batch_id)
    }

    /// Compute the wire `tessera_id` for `entity` under this deployment's current shard id and
    /// identity key (contracts §2.6/§3.4 r6). `/control/ingest`'s 200 response returns each
    /// accepted row's `tessera_id` this way rather than its raw `EntityId` (I10: entity ids never
    /// cross the trust boundary).
    ///
    /// **Fallible, not `.unwrap()`-able**: `IdentityKey::forward` refuses an entity at or above
    /// `u32::MAX` (Important I-1). The I9 allocator's ceiling makes that unreachable in practice
    /// for any entity this method is ever called with, but this stays a typed error rather than a
    /// panic — an internal invariant violation must fail closed (500), never crash the request
    /// thread or silently truncate.
    pub fn tessera_id_of(&self, entity: EntityId) -> std::result::Result<TesseraId, IdentityError> {
        let generation = self.generation.load_full();
        self.identity_key
            .forward(generation.bundle.manifest.identity.shard_id, entity)
    }

    /// Request a flush. **Accepted at any time, executed promptly** (contracts §3.4): the flag
    /// pulls the tick's deadline forward and the doorbell wakes an idle executor, so the flush
    /// runs at the next loop iteration — through the one tick path, with everything a tick
    /// guarantees. The 202 still means "accepted, not yet done": the segment write is pool work
    /// of real duration, and the response never waits on it.
    ///
    /// This is an *operator* trigger and deliberately the only thing that may pull the tick: it
    /// is rate-decoupled from ingest, so it cannot recreate the publish-on-trip hazard that got
    /// `flush_max_items` deleted (decision 0045) — a publication period proportional to load,
    /// rotating every session's projection key at that rate.
    /// [`Engine::request_flush_publication`] is the same request with its publication number,
    /// which is the form the control plane takes; this one drops the number for a caller that
    /// only wants the tick pulled forward. One request path, so the flag is set under the
    /// publication lock whichever is called.
    pub fn request_flush(&self) {
        let _ = self.request_flush_publication();
    }

    /// Request a **compaction fold** — the trigger `POST /control/compact` will take (contracts
    /// §3.4, reserved and unbuilt).
    ///
    /// The same shape as [`Engine::request_flush`] and for the same reason: the flag is read at the
    /// tick, on the one thread that publishes, so everything a tick guarantees holds for a
    /// requested fold. What differs is where the work runs — a fold gets its own thread rather than
    /// the shared pool (compaction §3) — and how long it takes: a 202 here means "accepted", and
    /// the fold is minutes to hours of IO afterwards.
    ///
    /// **At most one fold is in flight and a second request is not queued**: a fold requested while
    /// one runs is satisfied by neither, because the flag is a flag. That is the same answer
    /// compaction §9 gives the automatic trigger, which is refused outright while one runs.
    ///
    /// ⊘ **The trigger's three gauges and its minimum interval are not built** (compaction §9). A
    /// fold happens when something calls this, which for now is an operator or a test.
    pub fn request_fold(&self) {
        self.write
            .health()
            .fold_requested
            .store(true, Ordering::SeqCst);
        self.write.wake();
    }

    /// Submit an ingest batch whose rows name no artifacts — the plain form, and every batch that
    /// carries no membership column.
    ///
    /// One line of delegation rather than a second implementation: what a batch says about
    /// artifacts is a *field* of the command, and defaulting it here keeps the ordinary caller from
    /// having to spell an empty one.
    pub fn accept_ingest(
        &self,
        rows: Vec<UnallocatedRow>,
        batch_id: String,
        body_hash: [u8; 32],
    ) -> std::result::Result<Vec<EntityId>, crate::write::AcceptError> {
        self.accept_ingest_joining(rows, batch_id, body_hash, Default::default())
            .map(|(entity_ids, _)| entity_ids)
    }

    /// Submit an ingest batch and wait for its receipt. Rows arrive **unallocated**: entity ids are
    /// assigned on the executor, at the close of the commit window this submission lands in.
    ///
    /// `artifacts` is what a column named for a layer said — which artifacts these rows join, and
    /// which edges the adjacency of a list column declared (`artifacts-from-points.md` §6.2). It is
    /// resolved and grown **in the same commit as the rows**, so a batch is never half-applied: on
    /// a **closed** layer a key naming no artifact refuses the whole batch before an id is spent,
    /// on an **open** one it creates the artifact it names, and rows that were accepted carry their
    /// memberships from the moment they exist.
    ///
    /// Returns the assigned ids and **how many artifacts this batch created** — zero for every
    /// batch whose keys all existed, and the number a caller is owed because minting is not
    /// undoable (`artifacts-from-points.md` §3).
    ///
    /// Blocking — a tokio handler must call this inside `spawn_blocking`.
    pub fn accept_ingest_joining(
        &self,
        rows: Vec<UnallocatedRow>,
        batch_id: String,
        body_hash: [u8; 32],
        artifacts: tessera_lifecycle::BatchArtifacts,
    ) -> std::result::Result<(Vec<EntityId>, u64), crate::write::AcceptError> {
        // **Every buffered row has a cell**, established here because this is the boundary rows
        // enter the buffer through — and it has more than one caller. A check in the HTTP handler
        // guarded one of them and left the bench arms, the tests and any future ingest route
        // writing points the quantiser would silently clamp onto the edge of the grid.
        //
        // Before the submit, so an out-of-extent row is refused with nothing acked, nothing
        // WAL-durable and no entity id burned (I9). `plan_flush` is entitled to assume this and
        // does; a second copy of the predicate there is how the two would come to disagree.
        // **Step-down gates ingest** (owner-ruled 2026-08-04; write-path §5.6). Before the
        // per-row checks: this is node state, not row state, and refusing here — the boundary
        // with more than one caller — is what keeps a stepped-down node from accepting rows a
        // flush would then bury under a manifest assembled from the older served state. Denies
        // are deliberately not gated; see `AcceptError::SteppedDown`.
        if self.any_partition_stepped_down() {
            return Err(crate::write::AcceptError::SteppedDown);
        }
        // **Arity before the submit.** The commit window indexes `row.scalars` positionally against
        // `declared_scalars`, so a row longer than the schema would pair values with columns that
        // do not exist. **A shorter row is lawful** (`ingest.md` §7.1): a column declared at a
        // running service appends at the tail, so a row decoded against the schema before the
        // declaration, or a batch that omits the column, holds nothing for it and is padded with
        // its absence at the window's close (`attributes::pad_to_schema`). Checked here for the
        // same reason the extent check is: the invariant is about the buffer, and the buffer has
        // more than one writer.
        let declared = self.meta().declared_scalars.len();
        if let Some((index, row)) = rows
            .iter()
            .enumerate()
            .find(|(_, row)| row.scalars.len() > declared)
        {
            return Err(crate::write::AcceptError::ScalarArity {
                index,
                expected: declared,
                got: row.scalars.len(),
            });
        }
        // **Each row against its own view's frame** (decision 0040). The extent is the view's,
        // so one bundle-wide check would pass a row that has no cell in the view it is destined
        // for — silently clamped onto that view's grid edge at the flush. A row naming a view
        // this bundle does not declare is refused here for the same reason: there is no frame to
        // check it against, and the handler's own 404 guards only one of the buffer's writers.
        let meta = self.meta();
        for (index, row) in rows.iter().enumerate() {
            let Some(quantisation) = meta.quantisation_of(&row.view) else {
                return Err(crate::write::AcceptError::UnknownView {
                    index,
                    view: row.view.clone(),
                });
            };
            if !quantisation.contains(row.x, row.y) {
                return Err(crate::write::AcceptError::OutsideExtent {
                    index,
                    x: row.x,
                    y: row.y,
                    quantisation,
                });
            }
        }
        self.write
            .accept_ingest(rows, batch_id, body_hash, artifacts)
    }

    /// Submit one `/control/changes` entry and wait for its receipt.
    ///
    /// **An `Err` does not mean nothing happened**: for `Delete`/`Suppress` a WAL failure still
    /// applies the change before returning (lifecycle §4). See `ExecError::Wal`.
    ///
    /// A caller with a whole request's worth of changes wants [`Engine::submit_change`] instead —
    /// waiting between items is what reduces the deny lane's group commit to one entry per window.
    pub fn accept_change(
        &self,
        entity: EntityId,
        op: ChangeOp,
    ) -> std::result::Result<(), crate::write::AcceptError> {
        self.write.accept_change(entity, op)
    }

    /// Enqueue one `/control/changes` entry **without waiting for its receipt**, so that a caller
    /// with several can have them all in the executor's queue at once.
    ///
    /// That queue depth is the whole precondition for the deny lane's group commit: a caller that
    /// waits between items leaves the executor one entry to gather, and one request of N denies
    /// costs N fsyncs. Read `PendingChange::wait` before treating either half's `Err` as "nothing
    /// happened".
    pub fn submit_change(
        &self,
        entity: EntityId,
        op: ChangeOp,
    ) -> std::result::Result<crate::write::PendingChange, crate::write::AcceptError> {
        self.write.submit_change(entity, op)
    }

    /// One registered layer's declaration, by name — **the control plane's lookup, with no gate**.
    ///
    /// It answers what a *declaration* says, never what is served: the viewer plane's question is
    /// [`Engine::visible_layers`], which resolves reachability per principal and asks the overlay
    /// live. This one exists for `/control/ingest`, which must decide whether a column names a
    /// layer, and for a caller already holding the operator credential that registered it.
    pub fn registered_layer(&self, name: &str) -> Option<tessera_types::layer::RegisteredLayer> {
        self.write.live().registered_layer(name)
    }

    /// Register an annotation layer, returning its `tessera_id`.
    ///
    /// That identifier is the only address by which the layer can later be suppressed — entity ids
    /// never cross the boundary (**I10**) — which is the whole reason a layer takes an entity.
    pub fn register_layer(
        &self,
        declaration: tessera_types::layer::LayerDeclaration,
    ) -> std::result::Result<TesseraId, crate::write::AcceptError> {
        let entity = self.write.register_layer(declaration)?;
        // The blinding is total over the space the allocator will issue — `try_with_marks` refuses
        // a seed at or above the ceiling, so a row-less entity is always inside it — which is why
        // this conversion cannot fail in practice and is unwrapped to a fail-closed refusal rather
        // than a new error shape.
        let generation = self.generation();
        self.identity_key
            .forward(generation.bundle.manifest.identity.shard_id, entity)
            .map_err(|_| {
                crate::write::AcceptError::Exec(tessera_lifecycle::ExecError::LayerRefused {
                    detail: "the layer's entity id lies outside the identity space".to_string(),
                })
            })
    }

    /// Drop a layer. Its name is tombstoned and refused on recreation for ever.
    pub fn drop_layer(&self, name: String) -> std::result::Result<(), crate::write::AcceptError> {
        self.write.drop_layer(name)
    }

    /// Declare an attribute column while the service runs (`PUT /control/attributes`;
    /// `ingest.md` §1.3, §6.3). Answers `true` where a column of that name already carried
    /// exactly this identity and nothing was appended.
    ///
    /// Blocking — a tokio handler must call this inside `spawn_blocking`.
    pub fn declare_attribute(
        &self,
        request: tessera_lifecycle::AttributeRequest,
    ) -> std::result::Result<bool, crate::write::AcceptError> {
        self.write.declare_attribute(request)
    }

    /// Fill attribute values on entities that already exist (`POST /control/values`;
    /// `ingest.md` §1.4). Nothing is allocated and no row is created: every cell that is absent
    /// takes the supplied value, one that already holds it is a no-op, and one that holds a
    /// different value refuses the whole batch.
    ///
    /// Blocking — a tokio handler must call this inside `spawn_blocking`.
    pub fn fill_values(
        &self,
        request: tessera_lifecycle::ValuesRequest,
    ) -> std::result::Result<crate::write::ValuesReceipt, crate::write::AcceptError> {
        self.write.fill_values(request)
    }

    /// Declare a vocabulary while the service runs (`PUT /control/vocabularies/{name}`;
    /// `ingest.md` §1.3). Answers `(existing, added, titles)`: whether a vocabulary of that name
    /// already carried this identity, how many of the request's values were novel, and how many
    /// held values it gave a new title.
    ///
    /// **Nothing is validated here**, on `register_layer`'s rule: whether the name is free, and
    /// what a held vocabulary's identity is, are state only the write executor may read.
    ///
    /// Blocking — a tokio handler must call this inside `spawn_blocking`.
    pub fn declare_vocabulary(
        &self,
        request: tessera_lifecycle::VocabularyRequest,
    ) -> std::result::Result<(bool, u64, u64), crate::write::AcceptError> {
        self.write.declare_vocabulary(request)
    }

    /// A page of values for a vocabulary that exists (`PATCH /control/vocabularies/{name}/values`;
    /// `ingest.md` §1.3). Answers `(added, existing, titles)`.
    ///
    /// Blocking — a tokio handler must call this inside `spawn_blocking`.
    pub fn mint_vocabulary_values(
        &self,
        vocabulary: String,
        values: Vec<tessera_lifecycle::DeclaredValue>,
    ) -> std::result::Result<(u64, u64, u64), crate::write::AcceptError> {
        self.write.mint_vocabulary_values(vocabulary, values)
    }

    /// Declare a view group while the service runs (`PUT /control/view_groups/{name}`;
    /// `ingest.md` §1.3). Answers `true` where a group of that name already carried exactly this
    /// identity and nothing was appended.
    ///
    /// **The gate's labels are checked here**, on [`Engine::create_view`]'s rule: whether the
    /// plugin can read a label is a question only the engine can ask. Every other rule is the
    /// write executor's.
    ///
    /// Blocking — a tokio handler must call this inside `spawn_blocking`.
    pub fn create_view_group(
        &self,
        declaration: tessera_lifecycle::wal::ViewGroupDeclaration,
    ) -> std::result::Result<bool, crate::write::AcceptError> {
        self.check_gate_labels(declaration.visibility.as_deref())?;
        self.check_point_default(declaration.point_default.as_deref())?;
        self.write.create_view_group(declaration)
    }

    /// Create a plain view while the service runs (`PUT /control/views/{name}`; `ingest.md` §1.3
    /// and §10, R9), on [`Engine::create_view_group`]'s rule.
    ///
    /// Blocking — a tokio handler must call this inside `spawn_blocking`.
    pub fn create_plain_view(
        &self,
        declaration: tessera_lifecycle::wal::PlainViewDeclaration,
    ) -> std::result::Result<bool, crate::write::AcceptError> {
        self.check_gate_labels(declaration.visibility.as_deref())?;
        self.check_point_default(declaration.point_default.as_deref())?;
        self.write.create_plain_view(declaration)
    }

    /// `point_visibility.default` — what a point carrying no label of its own is given
    /// (decision 0133) — measured against the plugin, on [`Engine::check_gate_labels`]' rule.
    ///
    /// **The consequence of getting this wrong lands on every unlabelled row, not on the
    /// declaration.** A default the plugin cannot turn into a term is one no principal holds, so
    /// every point that took it is in nobody's mask and the view fills with rows no viewer can
    /// see; refused here, the operator reads the message once. `inherited` and the empty string
    /// are the build's own two refusals (`tessera_build::config::check_label`), transcribed: the
    /// first is the reserved word for *the container's gate is the whole of it*, which for a
    /// point could only widen, and the second is no label at all.
    fn check_point_default(
        &self,
        default: Option<&str>,
    ) -> std::result::Result<(), crate::write::AcceptError> {
        let Some(default) = default else {
            return Ok(());
        };
        let refused = |detail: String| {
            crate::write::AcceptError::Exec(tessera_lifecycle::ExecError::ViewRefused { detail })
        };
        if default.trim().is_empty() {
            return Err(refused(
                "point_visibility.default is empty. An access label is a term a principal either \
                 holds or does not; write `public` for the one every principal holds"
                    .to_string(),
            ));
        }
        if default == "inherited" {
            return Err(refused(
                "point_visibility.default = \"inherited\" is refused. A container's gate narrows \
                 rather than widens, and a point carrying no terms is already in no principal's \
                 mask — so inheriting would have to *add* a term to the point, which can only \
                 widen it (configuration.md §4). Name the label such a point should carry, or \
                 `public`"
                    .to_string(),
            ));
        }
        let public = std::str::from_utf8(tessera_authz::PUBLIC_LABEL).expect("the label is ASCII");
        if default == public {
            return Ok(());
        }
        let descriptors = self
            .plugin
            .terms_of_labels(&[default.as_bytes().to_vec()])
            .map_err(|e| {
                refused(format!(
                    "point_visibility.default = {default:?} is not a label the plugin can read \
                     ({e}). It is given to every point that carries none of its own \
                     (decision 0133), so a label the plugin cannot turn into a term would put \
                     those points in no principal's mask"
                ))
            })?;
        if descriptors.is_empty() {
            return Err(refused(format!(
                "point_visibility.default = {default:?} names no terms, so every point given it \
                 would be in no principal's mask. Write `public`, or a label naming a term"
            )));
        }
        Ok(())
    }

    /// The half of a gate's validation that needs the plugin (`views.md` §6, decision 0132).
    ///
    /// A view's gate is satisfied by exactly the item-visibility predicate, so the labels go
    /// through the same `Plugin::terms_of_labels` call an item's `access` list takes at
    /// `/control/ingest`, each element one label taken verbatim. A list the plugin cannot read —
    /// an empty element, or no element at all — is refused rather than stored: stored, it would
    /// be a gate no principal could ever satisfy, including the operator who wrote it. `public`
    /// is not asked about — it is the label every principal holds inside the trust boundary
    /// (decision 0088) — and is recognised only as the whole of the list.
    fn check_gate_labels(
        &self,
        visibility: Option<&[String]>,
    ) -> std::result::Result<(), crate::write::AcceptError> {
        tessera_plugin::check_gate(self.plugin.as_ref(), visibility)
            .map(|_| ())
            .map_err(|detail| {
                crate::write::AcceptError::Exec(tessera_lifecycle::ExecError::ViewRefused { detail })
            })
    }

    /// Create a view of a view group while the service runs (`views.md` §3.2, decision 0108).
    ///
    /// **Almost nothing is validated here**, on `register_layer`'s rule: whether the key is free
    /// is state only the write executor may read — a handler that checked first could be
    /// overtaken between its check and the enqueue.
    ///
    /// The **gate's labels** are the exception, and they are here because only the engine holds
    /// the plugin: [`Engine::check_gate_labels`], the one site every gate on this plane is
    /// checked at.
    pub fn create_view(
        &self,
        group: String,
        key: String,
        visibility: Option<Vec<String>>,
        metadata: std::collections::BTreeMap<String, tessera_types::view::ViewMetadataValue>,
    ) -> std::result::Result<(), crate::write::AcceptError> {
        self.check_gate_labels(visibility.as_deref())?;
        self.write.create_view(group, key, visibility, metadata)
    }

    /// Drop a view. Its key is tombstoned and refused on recreation for ever.
    pub fn drop_view(
        &self,
        group: String,
        key: String,
        delete_dangling: bool,
    ) -> std::result::Result<crate::write::ViewDropped, crate::write::AcceptError> {
        self.write.drop_view(group, key, delete_dangling)
    }

    /// Put a batch of artifacts into one level of a layer, returning a `tessera_id` per artifact
    /// in the caller's submitted order and the batch's counts (`ingest.md` §1.5).
    ///
    /// A key the level holds is accepted under the fill rule and answers the held artifact's
    /// identifier; a key it does not hold is published (`LayerRegistry::prepare_put`).
    ///
    /// **Members must be points, and that is checked here.** An enumerated membership is a set of
    /// documents; a row-less entity — another layer, another artifact — has no row, so it would
    /// contribute to no masked count and to no tile, while still counting towards the declared
    /// size the proportional criterion divides by. An artifact could then be pushed below its own
    /// criterion by members that can never be visible to anyone. Relations between artifacts are a
    /// later stage's edges, not a membership.
    ///
    /// The check is against the point region's high-water mark, which only rises, so it cannot go
    /// stale between here and the executor.
    pub fn put_artifacts(
        &self,
        layer: String,
        level: u32,
        artifacts: Vec<tessera_lifecycle::IncomingArtifact>,
    ) -> std::result::Result<PublishedArtifacts, crate::write::AcceptError> {
        // Entity space is `u32` by I9, and both marks sit inside it — the row-less ceiling is
        // derived from `u32::MAX` — so the narrowing is total rather than merely usually safe.
        let high_water = self.allocator_high_water() as u32;
        let rowless: u64 = artifacts
            .iter()
            .map(|a| a.members.cardinality() - a.members.range_cardinality(0..high_water))
            .sum();
        if rowless > 0 {
            return Err(crate::write::AcceptError::Exec(
                tessera_lifecycle::ExecError::LayerRefused {
                    detail: format!(
                        "{rowless} member(s) of this batch name no point; a membership is a set of \
                         documents, and a member with no row would count towards the artifact's \
                         declared size while being visible to nobody"
                    ),
                },
            ));
        }

        // **A declared member that is deleted refuses the batch; a suppressed one is accepted**
        // (`annotation-write-cycle.md` §3.1). The two are not near-neighbours: a deleted entity can
        // never contribute to a count again and, inside a generating set, makes the content
        // unservable from birth — better a loud refusal than a description nobody can read and
        // nobody was told about. A suppressed entity is a live member temporarily outside every
        // mask, and both structures behave fail-closed until the unsuppress; refusing it would make
        // an operator's reversible action refuse a caller's unrelated publication.
        //
        // One `verdict` lookup per declared member, on the control plane, against the live overlay
        // — which cannot go stale in the wrong direction between here and the executor, a deletion
        // being irreversible.
        //
        // **The refusal reports a count and a position, never an entity id** (I10): the detail is
        // forwarded to the caller as the 422 body, and an entity id in it would cross the boundary.
        // **A view the layer's own group has no key for is refused** (`ingest.md` §1.5,
        // `views.md` §3.5), in the words the build refuses the same row in
        // (`tessera-build/src/layers.rs`): an artifact belongs to one view of the group its layer
        // is scoped to, and a key nobody declared is an artifact drawn nowhere — a publication
        // the operator asked for, acked, and then visible to no one. The group's roster is the
        // generation's, which is the build's views plus every view created while the service ran
        // (`views.md` §3.2), so a view created a moment ago passes.
        //
        // The refusal names the group's keys — operator-plane names on the control plane, which
        // decision 0024 puts outside the register's viewer scope.
        {
            let named: std::collections::BTreeSet<&str> = artifacts
                .iter()
                .filter_map(|artifact| artifact.view.as_deref())
                .collect();
            if !named.is_empty() {
                let scope = self
                    .registered_layer(&layer)
                    .and_then(|registered| registered.declaration.scope.group().map(String::from));
                if let Some(group) = scope {
                    let generation = self.generation();
                    let keys: Vec<&str> = generation
                        .bundle
                        .manifest
                        .groups
                        .iter()
                        .filter(|held| held.name == group)
                        .flat_map(|held| held.views.iter().map(|view| view.key.as_str()))
                        .collect();
                    if let Some(unknown) = named.iter().find(|view| !keys.contains(*view)) {
                        return Err(crate::write::AcceptError::Exec(
                            tessera_lifecycle::ExecError::LayerRefused {
                                detail: format!(
                                    "this batch names view '{unknown}', which group '{group}' \
                                     has no such key for. Its keys are: {}. An artifact belongs \
                                     to one view and its keys are unique per (layer, view), so a \
                                     key nobody declared is a refusal rather than an artifact \
                                     drawn nowhere (views §3.5)",
                                    keys.join(", ")
                                ),
                            },
                        ));
                    }
                }
            }
        }

        // **A membership spelled by exclusion carries no members here**: the complement is taken
        // on the executor, against the view's entity set with the deleted already out of it
        // (`ingest.md` §2.3), so both checks pass over the empty set the record carries at this
        // point and neither has anything to say about a list of entities to leave out.
        let generation = self.generation();
        let mut deleted = 0u64;
        let mut first_artifact = None;
        for (index, artifact) in artifacts.iter().enumerate() {
            let in_this = artifact
                .members
                .iter()
                .chain(
                    artifact
                        .contents
                        .iter()
                        .flat_map(|content| content.generated_from.iter()),
                )
                .filter(|entity| generation.overlay.is_deleted(EntityId::new(*entity as u64)))
                .count() as u64;
            if in_this > 0 {
                deleted += in_this;
                first_artifact.get_or_insert(index);
            }
        }
        if let Some(first_artifact) = first_artifact {
            return Err(crate::write::AcceptError::Exec(
                tessera_lifecycle::ExecError::LayerRefused {
                    detail: format!(
                        "{deleted} member(s) or content source(s) of this batch are deleted, the \
                         first in artifact {first_artifact}; a deleted member contributes to no \
                         count and makes supplied content unservable from birth, so the batch is \
                         refused rather than published into silence"
                    ),
                },
            ));
        }

        let batch = self.write.publish_artifacts(layer, level, artifacts)?;
        let shard = generation.bundle.manifest.identity.shard_id;
        let tessera_ids = batch
            .entities
            .into_iter()
            .map(|entity| {
                self.identity_key.forward(shard, entity).map_err(|_| {
                    crate::write::AcceptError::Exec(tessera_lifecycle::ExecError::LayerRefused {
                        detail: "an artifact's entity id lies outside the identity space"
                            .to_string(),
                    })
                })
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(PublishedArtifacts {
            tessera_ids,
            created: batch.created,
            without_content: batch.without_content,
            filled: batch.filled,
            joined: batch.joined,
        })
    }

    /// [`Self::put_artifacts`], answering the identifiers alone — the shape every caller that
    /// publishes new artifacts under new keys wants.
    pub fn publish_artifacts(
        &self,
        layer: String,
        level: u32,
        artifacts: Vec<tessera_lifecycle::IncomingArtifact>,
    ) -> std::result::Result<Vec<TesseraId>, crate::write::AcceptError> {
        self.put_artifacts(layer, level, artifacts)
            .map(|published| published.tessera_ids)
    }

    /// Add points to the memberships of artifacts that already exist, each named by the key it was
    /// published under.
    ///
    /// **A build reading a member table has always done this; this is the same operation at the
    /// other entry point** ([decision 0091](../../../docs/decisions/0091-build-is-ingest-into-an-empty-database.md)).
    /// The artifact then behaves exactly as though the point had been there all along: there is no
    /// state in which a cluster holds some of its points because of how they arrived.
    ///
    /// **A suppressed artifact grows like any other and stays suppressed.** The key resolves
    /// against the *store*, never against what is served — so a suppression cannot be defeated by
    /// growing the artifact it hides, and cannot make the growth refuse either
    /// (`artifacts-from-points.md` §5).
    ///
    /// The two member checks are `publish_artifacts`'s, unchanged and for its reasons: a member
    /// with no row would count towards the declared size the proportional criterion divides by
    /// while being visible to nobody, and a **deleted** member can never contribute to a count
    /// again. A **suppressed** member joins: it is a live member temporarily outside every mask.
    ///
    /// **A point may also name its artifacts on the wire**, which is the same operation arriving
    /// with the rows it is about: `/control/ingest` accepts a column named for a declared layer and
    /// grows these memberships inside the batch's own commit window (`artifacts-from-points.md`
    /// §6.2). This entry point stays what an operator uses for a correction against points that are
    /// already there. An unknown key is refused on **this** route whatever the layer's value set
    /// says — see [`tessera_lifecycle::IncomingGrowth`]; the column at `/control/ingest` is where an
    /// open layer creates the artifact a key names.
    ///
    /// The answer is one [`GrownMembership`] per join, in the caller's order: the artifact's
    /// `tessera_id` and how many of the joining members it did not already hold. Neither an
    /// ordinal nor a membership size (C8).
    pub fn grow_memberships(
        &self,
        layer: String,
        level: u32,
        joins: Vec<tessera_lifecycle::IncomingGrowth>,
    ) -> std::result::Result<Vec<GrownMembership>, crate::write::AcceptError> {
        let high_water = self.allocator_high_water() as u32;
        let rowless: u64 = joins
            .iter()
            .map(|j| j.joining.cardinality() - j.joining.range_cardinality(0..high_water))
            .sum();
        if rowless > 0 {
            return Err(crate::write::AcceptError::Exec(
                tessera_lifecycle::ExecError::LayerRefused {
                    detail: format!(
                        "{rowless} of these joining member(s) name no point; a membership is a set \
                         of documents, and a member with no row would count towards the \
                         artifact's declared size while being visible to nobody"
                    ),
                },
            ));
        }

        // A count and the key of the first join naming one, never an entity id (I10): the detail
        // is the caller's 422 body.
        let generation = self.generation();
        let mut deleted = 0u64;
        let mut first_key = None;
        for join in &joins {
            let in_this = join
                .joining
                .iter()
                .filter(|entity| generation.overlay.is_deleted(EntityId::new(*entity as u64)))
                .count() as u64;
            if in_this > 0 {
                deleted += in_this;
                first_key.get_or_insert(join.key.as_str());
            }
        }
        if let Some(first_key) = first_key {
            return Err(crate::write::AcceptError::Exec(
                tessera_lifecycle::ExecError::LayerRefused {
                    detail: format!(
                        "{deleted} joining member(s) are deleted, the first joining '{first_key}'; a \
                         deleted member contributes to no count, so the batch is refused rather \
                         than applied into silence"
                    ),
                },
            ));
        }

        let grown = self.write.grow_memberships(layer, level, joins)?;
        let shard = generation.bundle.manifest.identity.shard_id;
        grown
            .into_iter()
            .map(|receipt| {
                let tessera_id =
                    self.identity_key
                        .forward(shard, receipt.entity)
                        .map_err(|_| {
                            crate::write::AcceptError::Exec(
                                tessera_lifecycle::ExecError::LayerRefused {
                                    detail:
                                        "an artifact's entity id lies outside the identity space"
                                            .to_string(),
                                },
                            )
                        })?;
                Ok(GrownMembership {
                    tessera_id,
                    joined: receipt.joined,
                    filled: receipt.filled,
                    left: receipt.left,
                    withdrawn: receipt.withdrawn,
                })
            })
            .collect()
    }

    /// Where an artifact's entity sits, and what was published there.
    ///
    /// **Addressing, and the caller gates afterwards.** This says an entity is artifact *n* of a
    /// level; it says nothing about whether the asker may know that, and every route acting on the
    /// answer puts it through the one predicate first. The membership itself is deliberately not
    /// returned — a caller with a raw member set could count it, and an unmasked count over items a
    /// principal may not see is C8's row.
    pub fn locate_artifact(&self, entity: EntityId) -> Option<PublishedArtifactAddress> {
        let (layer, level, ordinal) = self.write.live().locate_artifact(entity)?;
        let key = self.write.live().with_artifacts(|store| {
            store
                .get(&layer, level, ordinal)
                .and_then(|r| r.key.clone())
        });
        Some(PublishedArtifactAddress {
            layer,
            level,
            ordinal,
            key,
        })
    }
}

/// One join's answer from [`Engine::grow_memberships`].
///
/// `joined` is bounded by the members the caller sent, so it says nothing about the members they
/// did not: a membership size never crosses the boundary (C8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrownMembership {
    /// The artifact's identifier, the same one its publication answered with.
    pub tessera_id: TesseraId,
    /// How many of the joining members were not already in the membership.
    pub joined: u64,
    /// How many of the fixed parts the join carried were absent and are now held
    /// (`ingest.md` §1.5).
    pub filled: u64,
    /// How many of the leaving members a generating-set page took out of it.
    pub left: u64,
    /// The rank this page emptied, where it emptied one: the content is withdrawn and the caller
    /// supplies it again (`ingest.md` §1.1).
    pub withdrawn: Option<u16>,
}

/// What [`Engine::put_artifacts`] answers: the identifiers in the caller's order and the batch's
/// counts (`ingest.md` §1.5), each bounded by the caller's own request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedArtifacts {
    /// One per artifact in the caller's order: a held artifact's own, or the new one's.
    pub tessera_ids: Vec<TesseraId>,
    /// How many artifacts the batch created; the rest were held.
    pub created: u64,
    /// How many created artifacts carry no content on a layer declaring some (R5).
    pub without_content: u64,
    /// How many fixed parts were filled on held artifacts.
    pub filled: u64,
    /// How many members joined held artifacts that did not already hold them.
    pub joined: u64,
}

/// Where an artifact sits, as [`Engine::locate_artifact`] answers it.
///
/// The ordinal is here because the engine's own routes address by it. **It does not cross the
/// wire** — see `Command::PublishArtifacts` for why two ordinals are a count of what lies between.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedArtifactAddress {
    pub layer: String,
    pub level: u32,
    pub ordinal: u32,
    pub key: Option<String>,
}
