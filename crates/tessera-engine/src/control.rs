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
    /// Resolve raw term descriptors to `TermId`s: a dictionary hit returns the durable
    /// bundle-relative id; a miss is interned in this process's extension state and resumed
    /// across calls. `/control/ingest` resolves before the batch is durably appended. An
    /// extension id is never satisfiable by any session, so a live/replay mismatch only
    /// renumbers bookkeeping.
    pub fn resolve_terms(&self, descriptors: &[Descriptor]) -> Vec<TermId> {
        self.write
            .live()
            .resolve_terms(&self.generation.load().dict, descriptors)
    }

    /// Resolve an external id to its `EntityId`, checking every item established live before
    /// falling back to the bundle's own sidecar extent. A sidecar failure (digest mismatch,
    /// out-of-order extent, corruption) returns `Err` rather than panicking.
    pub fn resolve_external_id(
        &self,
        external_id: &[u8],
    ) -> std::result::Result<Option<EntityId>, StoreError> {
        if let Some(entity) = self.write.live().established_entity(external_id) {
            return Ok(Some(entity));
        }
        self.generation.load().external_index.resolve(external_id)
    }

    /// Invert `tessera_id`s to entity ids for the admin plane, all-or-nothing. The idset is
    /// checked against the same generation the inversions use, so a swap mid-call
    /// cannot invert under a different snapshot than it validated against; a mismatch is
    /// [`EngineError::StaleIdSet`]. `None` per position for an identifier that names nothing.
    /// Points sit below the high-water mark; row-less entities sit at or above the low-water
    /// mark — testing only `entity < high_water` would refuse every layer identifier ever issued.
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

    /// Does `view` hold a row for `entity`? "In the view" is the view's permutation and the
    /// commit window's buffer: a row accepted but
    /// not yet flushed is in no permutation, and missing it would let two batches hand one flush
    /// two rows for one entity in one view.
    pub fn view_holds(&self, entity: EntityId, view: &str) -> bool {
        let generation = self.generation();
        generation.bundle.partitions.values().any(|partition| {
            partition
                .views
                .get(view)
                .is_some_and(|data| data.row_space.row_of(entity).is_some())
        }) || generation.buffer.contains_in_view(entity, view)
    }

    /// An already-flushed entity's full term set, ascending, once the entity's own row has left
    /// the buffer. The full set, never a subset: comparing only the terms the writer named would
    /// accept a batch that dropped one.
    ///
    /// `None` where no layer holds a list — unknown, not empty. `Some(vec![])` means the item
    /// legitimately carries no label. A malformed layer is `None` and is logged, naming the
    /// artefact and not the entity.
    pub fn flushed_terms(&self, entity: EntityId) -> Option<Vec<TermId>> {
        flushed_terms_of(&self.generation(), entity)
    }

    /// Batch form of [`Self::resolve_external_id`] for `/control/ingest`'s duplicate check: the
    /// live map first for the whole batch, then one batched, sorted sidecar call for whatever
    /// residual keys it didn't resolve — each bundle extent opened at most once regardless of
    /// batch size. Returns one `Option<EntityId>` per input, in the caller's given order.
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

    /// `entity -> external_id` for drill-down. The live map is consulted first: post-build
    /// ingest has no locator slot and no extent entry. `Ok(None)` means no external id; it must
    /// never mean "could not find out". An entity below
    /// the live high-water and unknown to both sources fails closed as
    /// `Err(StoreError::InvalidSidecar)`.
    pub fn external_id_of(
        &self,
        entity: EntityId,
    ) -> std::result::Result<Option<Vec<u8>>, StoreError> {
        self.external_id_of_in(&self.generation.load(), entity)
    }

    /// [`Self::external_id_of`] against a generation the caller already loaded. `Engine::item`
    /// must not take a second `load()`: the sidecar is per-generation, so
    /// resolving a row against one generation and its external id against another would mix
    /// generations within one request.
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

    /// The body hash and per-row entity ids a batch id was previously accepted with — the
    /// idempotency check for `/control/ingest`: equal hash is a 200 no-op; different hash is 409.
    /// An accelerant, not the authority: the same check runs again on the executor, race-free.
    pub fn accepted_batch(&self, batch_id: &str) -> Option<([u8; 32], Vec<EntityId>)> {
        self.write.live().accepted_batch(batch_id)
    }

    /// Compute the wire `tessera_id` for `entity`, returned instead of the raw `EntityId`, which
    /// never crosses the trust boundary. `IdentityKey::forward` refuses an entity at or above
    /// `u32::MAX`: unreachable in practice, but a typed error rather than a panic.
    pub fn tessera_id_of(&self, entity: EntityId) -> std::result::Result<TesseraId, IdentityError> {
        let generation = self.generation.load_full();
        self.identity_key
            .forward(generation.bundle.manifest.identity.shard_id, entity)
    }

    /// Request a flush. Accepted at any time and executed promptly: the flag pulls the tick's
    /// deadline forward and the doorbell wakes an idle executor. The 202 means "accepted, not yet
    /// done"; the response never waits on the segment write. An operator trigger, rate-decoupled
    /// from ingest. [`Engine::request_flush_publication`] is the same request with its
    /// publication number; this drops the number.
    pub fn request_flush(&self) {
        let _ = self.request_flush_publication();
    }

    /// Request a compaction fold — the trigger `POST /control/compact` will take (reserved,
    /// unbuilt). The same shape as [`Engine::request_flush`], but a fold gets its own thread
    /// rather than the shared pool, and a 202 means minutes to hours of IO still to come. At most
    /// one fold is in flight: a second request while one runs is satisfied by neither.
    ///
    /// Not built yet: the trigger's own gauges and minimum interval.
    pub fn request_fold(&self) {
        self.write
            .health()
            .fold_requested
            .store(true, Ordering::SeqCst);
        self.write.wake();
    }

    /// Submit an ingest batch whose rows name no artifacts — the plain form, and every batch that
    /// carries no membership column.
    pub fn accept_ingest(
        &self,
        rows: Vec<UnallocatedRow>,
        batch_id: String,
        body_hash: [u8; 32],
    ) -> std::result::Result<Vec<EntityId>, crate::write::AcceptError> {
        self.accept_ingest_joining(rows, batch_id, body_hash, Default::default())
            .map(|(entity_ids, _)| entity_ids)
    }

    /// Submit an ingest batch and wait for its receipt. Rows arrive **unallocated**: entity ids
    /// are assigned on the executor, at the close of the commit window this submission lands in.
    /// `artifacts` says which artifacts these rows join, resolved and grown in the same commit so
    /// a batch is never half-applied: a closed layer refuses the whole batch if a key names no
    /// artifact; an open layer creates it.
    ///
    /// Blocking — a tokio handler must call this inside `spawn_blocking`.
    pub fn accept_ingest_joining(
        &self,
        rows: Vec<UnallocatedRow>,
        batch_id: String,
        body_hash: [u8; 32],
        artifacts: tessera_lifecycle::BatchArtifacts,
    ) -> std::result::Result<(Vec<EntityId>, u64), crate::write::AcceptError> {
        // Checked before the submit, so an out-of-extent row is refused with nothing acked,
        // nothing WAL-durable and no entity id burned. `plan_flush` assumes this holds.
        //
        // A stepped-down node refuses ingest here, before the per-row checks: this keeps it from
        // accepting rows a flush would bury under a manifest assembled from older served state.
        // Denies are not gated; see `AcceptError::SteppedDown`.
        if self.any_partition_stepped_down() {
            return Err(crate::write::AcceptError::SteppedDown);
        }
        // A row longer than the schema would pair values with columns that do not exist. A
        // shorter row is lawful: a column declared while the service runs appends at the tail, so
        // a row decoded before the declaration holds nothing for it, padded at the window's close.
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
        // Checked against each row's own view, not the bundle as a whole: a bundle-wide check
        // would pass a row with no cell in the view it targets.
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

    /// Submit one `/control/changes` entry and wait for its receipt. An `Err` does not mean
    /// nothing happened: for `Delete`/`Suppress` a WAL failure still
    /// applies the change. See `ExecError::Wal`. A caller with several wants
    /// [`Engine::submit_change`] instead.
    pub fn accept_change(
        &self,
        entity: EntityId,
        op: ChangeOp,
    ) -> std::result::Result<(), crate::write::AcceptError> {
        self.write.accept_change(entity, op)
    }

    /// Enqueue one `/control/changes` entry without waiting, so several from one request reach
    /// the executor's queue together — the precondition for the deny lane's group commit. Read
    /// `PendingChange::wait` before treating either half's `Err` as "nothing happened".
    pub fn submit_change(
        &self,
        entity: EntityId,
        op: ChangeOp,
    ) -> std::result::Result<crate::write::PendingChange, crate::write::AcceptError> {
        self.write.submit_change(entity, op)
    }

    /// One registered layer's declaration, by name, with no gate. Answers what a declaration
    /// says, never what is served: [`Engine::visible_layers`] resolves reachability.
    pub fn registered_layer(&self, name: &str) -> Option<tessera_types::layer::RegisteredLayer> {
        self.write.live().registered_layer(name)
    }

    /// Register an annotation layer, returning its `tessera_id`, the only address it can later
    /// be suppressed by — entity ids never cross the boundary.
    pub fn register_layer(
        &self,
        declaration: tessera_types::layer::LayerDeclaration,
    ) -> std::result::Result<TesseraId, crate::write::AcceptError> {
        let entity = self.write.register_layer(declaration)?;
        // The blinding is total over the space the allocator will issue, so a row-less entity is
        // always inside it and this conversion cannot fail in practice.
        let generation = self.generation();
        self.issued_id(generation.bundle.manifest.identity.shard_id, entity)
    }

    /// The `tessera_id` of an entity a layer write just allocated, or the write's refusal.
    fn issued_id(
        &self,
        shard: u32,
        entity: EntityId,
    ) -> std::result::Result<TesseraId, crate::write::AcceptError> {
        self.identity_key.forward(shard, entity).map_err(|_| {
            crate::write::AcceptError::Exec(tessera_lifecycle::ExecError::LayerRefused {
                detail: "an allocated entity id lies outside the identity space".to_string(),
            })
        })
    }

    /// Refuses memberships naming an entity that is not a point.
    fn refuse_rowless<'a>(
        &self,
        memberships: impl Iterator<Item = &'a croaring::Bitmap>,
    ) -> std::result::Result<(), crate::write::AcceptError> {
        // Entity space is `u32`, and both marks sit inside it, so the narrowing is total.
        let high_water = self.allocator_high_water() as u32;
        let rowless: u64 = memberships
            .map(|m| m.cardinality() - m.range_cardinality(0..high_water))
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
        Ok(())
    }

    /// Drop a layer. Its name is tombstoned and refused on recreation for ever.
    pub fn drop_layer(&self, name: String) -> std::result::Result<(), crate::write::AcceptError> {
        self.write.drop_layer(name)
    }

    /// Declare an attribute column while the service runs. `true` where a column of that name
    /// already carried exactly this identity and nothing was appended.
    /// Blocking — a tokio handler must call this inside `spawn_blocking`.
    pub fn declare_attribute(
        &self,
        request: tessera_lifecycle::AttributeRequest,
    ) -> std::result::Result<bool, crate::write::AcceptError> {
        self.write.declare_attribute(request)
    }

    /// Fill attribute values on entities that already exist. Nothing is allocated and no row is
    /// created: every cell that is absent takes the supplied value, one that already holds it is
    /// a no-op, and one that holds a different value refuses the whole batch.
    /// Blocking — a tokio handler must call this inside `spawn_blocking`.
    pub fn fill_values(
        &self,
        request: tessera_lifecycle::ValuesRequest,
    ) -> std::result::Result<crate::write::ValuesReceipt, crate::write::AcceptError> {
        self.write.fill_values(request)
    }

    /// Declare a vocabulary while the service runs. Answers `(existing, added, titles)`.
    /// Blocking — a tokio handler must call this inside `spawn_blocking`.
    pub fn declare_vocabulary(
        &self,
        request: tessera_lifecycle::VocabularyRequest,
    ) -> std::result::Result<(bool, u64, u64), crate::write::AcceptError> {
        self.write.declare_vocabulary(request)
    }

    /// A page of values for a vocabulary that exists. Answers `(added, existing, titles)`.
    /// Blocking — a tokio handler must call this inside `spawn_blocking`.
    pub fn mint_vocabulary_values(
        &self,
        vocabulary: String,
        values: Vec<tessera_lifecycle::DeclaredValue>,
    ) -> std::result::Result<(u64, u64, u64), crate::write::AcceptError> {
        self.write.mint_vocabulary_values(vocabulary, values)
    }

    /// Declare a view group while the service runs. The gate's labels are checked here, since
    /// only the engine holds the plugin; every other rule is the write executor's.
    /// Blocking — a tokio handler must call this inside `spawn_blocking`.
    pub fn create_view_group(
        &self,
        mut declaration: tessera_lifecycle::wal::ViewGroupDeclaration,
    ) -> std::result::Result<bool, crate::write::AcceptError> {
        declaration.visibility = self.check_visibility(declaration.visibility.as_deref())?;
        declaration.point_default = self.check_point_default(declaration.point_default.as_deref())?;
        self.write.create_view_group(declaration)
    }

    /// Create a plain view while the service runs, on [`Engine::create_view_group`]'s rule.
    /// Blocking — a tokio handler must call this inside `spawn_blocking`.
    pub fn create_plain_view(
        &self,
        mut declaration: tessera_lifecycle::wal::PlainViewDeclaration,
    ) -> std::result::Result<bool, crate::write::AcceptError> {
        declaration.visibility = self.check_visibility(declaration.visibility.as_deref())?;
        declaration.point_default = self.check_point_default(declaration.point_default.as_deref())?;
        self.write.create_plain_view(declaration)
    }

    /// [`tessera_plugin::check_point_default`] with this engine's plugin: the default as stored,
    /// or a view refusal.
    fn check_point_default(
        &self,
        default: Option<&str>,
    ) -> std::result::Result<Option<String>, crate::write::AcceptError> {
        default
            .map(|default| tessera_plugin::check_point_default(self.plugin.as_ref(), default))
            .transpose()
            .map_err(|detail| {
                crate::write::AcceptError::Exec(tessera_lifecycle::ExecError::ViewRefused { detail })
            })
    }

    /// [`tessera_plugin::check_visibility`] with this engine's plugin: the gate as stored, or a
    /// view refusal.
    fn check_visibility(
        &self,
        visibility: Option<&[String]>,
    ) -> std::result::Result<Option<Vec<String>>, crate::write::AcceptError> {
        tessera_plugin::check_visibility(self.plugin.as_ref(), visibility).map_err(|detail| {
            crate::write::AcceptError::Exec(tessera_lifecycle::ExecError::ViewRefused { detail })
        })
    }

    /// Create a view of a view group. Almost nothing is validated here; the gate's labels are the
    /// exception, checked at [`Engine::check_visibility`].
    pub fn create_view(
        &self,
        group: String,
        key: String,
        visibility: Option<Vec<String>>,
        metadata: std::collections::BTreeMap<String, tessera_types::view::ViewMetadataValue>,
    ) -> std::result::Result<(), crate::write::AcceptError> {
        let visibility = self.check_visibility(visibility.as_deref())?;
        self.write.create_view(group, key, visibility, metadata)
    }

    /// Drop a view. Its key may be created again, as a new incarnation with none of the dropped
    /// view's rows or artifacts.
    pub fn drop_view(
        &self,
        group: String,
        key: String,
        delete_dangling: bool,
    ) -> std::result::Result<crate::write::ViewDropped, crate::write::AcceptError> {
        self.write.drop_view(group, key, delete_dangling)
    }

    /// Put a batch of artifacts into one level of a layer, returning a `tessera_id` per artifact
    /// and the batch's counts. A held key is filled; an unheld key is published. Members must be
    /// points: a row-less entity counts towards the declared size while visible to nobody.
    pub fn put_artifacts(
        &self,
        layer: String,
        level: u32,
        artifacts: Vec<tessera_lifecycle::IncomingArtifact>,
    ) -> std::result::Result<PublishedArtifacts, crate::write::AcceptError> {
        self.refuse_rowless(artifacts.iter().map(|a| &a.members))?;

        // A declared member that is deleted refuses the batch; a suppressed one is accepted, being
        // a live member temporarily outside every mask. The refusal reports a count and a
        // position, never an entity id. A membership spelled by exclusion carries no members here:
        // the complement is taken on the executor, so this and the row-less check pass over the
        // empty set the record carries.
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
            .map(|entity| self.issued_id(shard, entity))
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

    /// Add points to the memberships of artifacts that already exist, each named by its published
    /// key. A suppressed artifact grows like any other and stays suppressed: the key resolves
    /// against the store, not what is served. The member checks are `publish_artifacts`'s. A
    /// point may also name its artifacts via `/control/ingest`'s column; this entry point is for
    /// correcting points that already exist, and an unknown key is refused here regardless.
    ///
    /// One [`GrownMembership`] per join. Neither an ordinal nor a membership size crosses the
    /// boundary.
    pub fn grow_memberships(
        &self,
        layer: String,
        level: u32,
        joins: Vec<tessera_lifecycle::IncomingGrowth>,
    ) -> std::result::Result<Vec<GrownMembership>, crate::write::AcceptError> {
        self.refuse_rowless(joins.iter().map(|j| &j.joining))?;

        // A count and the key of the first join naming one, never an entity id: the detail is the
        // caller's 422 body.
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
                Ok(GrownMembership {
                    tessera_id: self.issued_id(shard, receipt.entity)?,
                    joined: receipt.joined,
                    filled: receipt.filled,
                    left: receipt.left,
                    withdrawn: receipt.withdrawn,
                })
            })
            .collect()
    }

    /// Where an artifact's entity sits, and what was published there. Addressing only: this says
    /// an entity is artifact *n* of a level; it says nothing about
    /// whether the asker may know that, and every route acting on the answer puts it through the
    /// visibility predicate first. The membership itself is not returned: a caller with a raw
    /// member set could count it, and an unmasked count over items a principal may not see leaks.
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

/// One join's answer from [`Engine::grow_memberships`]. `joined` is bounded by the members the
/// caller sent, so it says nothing about the members they did not: a membership size never
/// crosses the boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrownMembership {
    /// The artifact's identifier, the same one its publication answered with.
    pub tessera_id: TesseraId,
    /// How many of the joining members were not already in the membership.
    pub joined: u64,
    /// How many of the fixed parts the join carried were absent and are now held.
    pub filled: u64,
    /// How many of the leaving members a generating-set page took out of it.
    pub left: u64,
    /// The rank this page emptied, where it emptied one: the content is withdrawn and the caller
    /// supplies it again.
    pub withdrawn: Option<u16>,
}

/// What [`Engine::put_artifacts`] answers: the identifiers in the caller's order and the batch's
/// counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedArtifacts {
    /// One per artifact in the caller's order: a held artifact's own, or the new one's.
    pub tessera_ids: Vec<TesseraId>,
    /// How many artifacts the batch created; the rest were held.
    pub created: u64,
    /// How many created artifacts carry no content on a layer declaring some.
    pub without_content: u64,
    /// How many fixed parts were filled on held artifacts.
    pub filled: u64,
    /// How many memberships the batch added: every member of a created artifact, and every member
    /// a held artifact did not already hold.
    pub joined: u64,
}

/// Where an artifact sits, as [`Engine::locate_artifact`] answers it. The ordinal is here because
/// the engine's own routes address by it. It does not cross the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedArtifactAddress {
    pub layer: String,
    pub level: u32,
    pub ordinal: u32,
    pub key: Option<String>,
}
