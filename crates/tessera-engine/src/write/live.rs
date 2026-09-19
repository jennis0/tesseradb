use super::*;

// =================================================================================================
// Live state
// =================================================================================================

/// One accepted batch, as the idempotency index holds it.
///
/// The ids ride along so a byte-identical replay answers with the same `tessera_id`s without
/// re-deriving them from `external_id`. A null-external-id row has none of those to derive them
/// from. An accepted values batch was allocated nothing and carries an empty vector.
#[derive(Clone)]
pub(in crate::write) struct AcceptedBatch {
    pub(in crate::write) body_hash: [u8; 32],
    pub(in crate::write) entity_ids: Vec<EntityId>,
    /// Where the record carrying this batch lies in the log. The index is a cache of the WAL
    /// ([`tessera_lifecycle::batch_identity`]), so an entry outlives its record only until the
    /// rotation that deletes the member holding it. `Executor::rotate_wal` forgets everything
    /// below [`ExecutorWal::retained_from`], which is the horizon a restart would rebuild.
    pub(in crate::write) wal_pos: u64,
}

/// The `/control/ingest` and `/control/values` idempotency index: batch id -> what was accepted
/// under it.
pub(in crate::write) type AcceptedBatches = FxHashMap<String, AcceptedBatch>;

/// The descriptor resolver's detached extension state: interned novel descriptors, plus the next
/// extension id to hand out.
pub(in crate::write) type ResolverState = (FxHashMap<Vec<u8>, TermId>, u32);

/// State the handler side reads and the executor thread writes.
///
/// Each field carries its own lock because it is read concurrently by a request path that is not
/// the executor (`Engine::resolve_external_id`, `Engine::external_id_of`, `/control/ingest`'s
/// replay check, `/control/status`'s high-water). The generation pointer is not held here: it is
/// swapped by `Executor::publish_arc`, the crate's one non-atomic store.
pub(crate) struct LiveState {
    /// The entity id allocator. Written only by the executor, never by a handler: entity ids are
    /// assigned at a window's close, on the one thread that also advances the high-water mark.
    pub(in crate::write) allocator: Mutex<Allocator>,
    pub(in crate::write) established: Mutex<std::collections::HashMap<Vec<u8>, EntityId>>,
    pub(in crate::write) established_inverse: Mutex<FxHashMap<EntityId, Vec<u8>>>,
    /// The descriptor resolver's extension state. Resolved in the handler before submitting, not
    /// on the executor: signature-sorted assignment needs the term set to compute a sort key
    /// before any id exists, the structural exception argued at [`WritePath::resolve_terms`].
    pub(in crate::write) resolver_state: Mutex<ResolverState>,
    pub(in crate::write) accepted_batches: Mutex<AcceptedBatches>,
    /// The annotation layer registry. Written only by the executor: a registration is a WAL
    /// append followed by an apply, on the one thread that also holds the allocator. Read by the
    /// request path, which resolves a session's reachable set from it.
    pub(in crate::write) registry: Mutex<LayerRegistry>,
    /// Every artifact's entity-space membership, on the registry's contract: written only by the
    /// executor, read by the request path.
    pub(in crate::write) artifacts: Mutex<ArtifactStore>,
    /// The view roster, on the registry's contract: written only by the executor, a create is a
    /// WAL append followed by an apply, on the one thread that also holds the allocator. Read by
    /// the request path, which resolves a view id against the manifest the roster made.
    pub(in crate::write) roster: Mutex<tessera_lifecycle::ViewRoster>,
    /// The attribute columns declared while the service runs and not yet folded into a
    /// `MANIFEST.json`, on the roster's contract: written only by the executor (a declaration
    /// is a WAL append followed by an apply) and read at every side-manifest publication, which is
    /// the declaration's durable home.
    pub(in crate::write) attributes: Mutex<crate::attributes::RuntimeAttributes>,
    /// The vocabularies declared while the service runs and not yet folded into a
    /// `MANIFEST.json`, on the attribute list's contract: written only by the executor, read at
    /// every side-manifest publication.
    pub(in crate::write) vocabularies: Mutex<crate::vocabularies::RuntimeVocabularies>,
    /// The view groups and plain views declared while the service runs and not yet folded, on
    /// the vocabulary list's contract.
    pub(in crate::write) view_declarations: Mutex<crate::view_declarations::RuntimeViewDeclarations>,
}

impl LiveState {
    pub(in crate::write) fn established_entity(&self, external_id: &[u8]) -> Option<EntityId> {
        lock_recover(&self.established).get(external_id).copied()
    }

    pub(in crate::write) fn established_entities(&self, external_ids: &[Vec<u8>]) -> Vec<Option<EntityId>> {
        let established = lock_recover(&self.established);
        external_ids
            .iter()
            .map(|id| established.get(id.as_slice()).copied())
            .collect()
    }

    pub(in crate::write) fn established_external_id(&self, entity: EntityId) -> Option<Vec<u8>> {
        lock_recover(&self.established_inverse)
            .get(&entity)
            .cloned()
    }

    pub(in crate::write) fn allocator_high_water(&self) -> u64 {
        lock_recover(&self.allocator).high_water()
    }

    pub(in crate::write) fn allocator_low_water(&self) -> u64 {
        lock_recover(&self.allocator).low_water()
    }

    /// Runs `f` with both the registry and the allocator held, in that lock order.
    ///
    /// One critical section: a registration reads one and writes both, and taking them separately
    /// would let a second registration allocate between the name check and the run allocation.
    /// This order is the only one taken anywhere, so it cannot deadlock against
    /// [`LiveState::registry_for_publication`].
    pub(in crate::write) fn with_registry_and_allocator<R>(
        &self,
        f: impl FnOnce(&mut LayerRegistry, &mut Allocator) -> R,
    ) -> R {
        let mut registry = lock_recover(&self.registry);
        let mut alloc = lock_recover(&self.allocator);
        f(&mut registry, &mut alloc)
    }

    pub(in crate::write) fn apply_registry_record(&self, record: &WalRecord) {
        lock_recover(&self.registry).apply(record);
    }

    /// Runs `f` with the registry, the artifact store and the allocator held, in that lock order.
    ///
    /// Extends `with_registry_and_allocator`'s order: a publication reads the level's cursor,
    /// allocates against the registry's runs and writes both, so two batches taking the locks
    /// separately would be handed the same ordinals. This order is taken everywhere the three are
    /// held together, so it cannot deadlock.
    pub(in crate::write) fn with_publication_state<R>(
        &self,
        f: impl FnOnce(&mut LayerRegistry, &mut ArtifactStore, &mut Allocator) -> R,
    ) -> R {
        let mut registry = lock_recover(&self.registry);
        let mut artifacts = lock_recover(&self.artifacts);
        let mut alloc = lock_recover(&self.allocator);
        f(&mut registry, &mut artifacts, &mut alloc)
    }

    /// The bound rotation may not reclaim past, or `None` if no membership is at risk. See
    /// [`ArtifactStore::oldest_wal_pos`]. This pins the log until membership has a home outside
    /// it.
    pub(in crate::write) fn artifacts_oldest_wal_pos(&self) -> Option<u64> {
        lock_recover(&self.artifacts).oldest_wal_pos()
    }

    /// Read the artifact store: the request path's route to a membership.
    pub(crate) fn with_artifacts<R>(&self, f: impl FnOnce(&ArtifactStore) -> R) -> R {
        f(&lock_recover(&self.artifacts))
    }

    /// Everything not yet in a manifest, packed and ready. See [`ArtifactStore::unpublished`].
    pub(in crate::write) fn unpublished_memberships(
        &self,
    ) -> (
        Vec<tessera_lifecycle::membership::PendingExtent>,
        Vec<(String, u32)>,
    ) {
        lock_recover(&self.artifacts).unpublished()
    }

    /// The supplied content of every artifact not yet in a manifest. See
    /// [`tessera_lifecycle::membership::ArtifactStore::unpublished_content`].
    pub(in crate::write) fn unpublished_content(&self) -> Vec<(tessera_types::EntityId, Vec<(u16, String)>)> {
        lock_recover(&self.artifacts).unpublished_content()
    }

    /// Record every level as published to its current extent, and with that release the log.
    ///
    /// Called only once the manifest naming the extents is durable. Recomputes from what the
    /// store holds now, rather than threading the packed ranges through, since the executor is
    /// the only writer and nothing has been added since the pack.
    pub(in crate::write) fn mark_memberships_published(&self) {
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
    /// manifest. See [`tessera_lifecycle::membership::ArtifactStore::mark_content_published`].
    ///
    /// Called from the overlay publication, not the fold: the fold carries content extents
    /// forward unchanged, so a fill pending at a fold is still pending after it.
    pub(in crate::write) fn mark_content_published(&self) {
        lock_recover(&self.artifacts).mark_content_published();
    }

    /// Release the log from every growth the fold's whole rewrite has just made durable. See
    /// [`tessera_lifecycle::membership::ArtifactStore::mark_growth_packed`].
    ///
    /// Called from the fold only: `mark_memberships_published` covers only the tail above each
    /// level's high-water, and a growth lands below it.
    pub(in crate::write) fn mark_growth_packed(&self) {
        lock_recover(&self.artifacts).mark_growth_packed();
    }

    /// Read the resident memberships back through the extents a publication has just written, so
    /// each one is a view over the live prefix rather than a heap bitmap or a stale mapping.
    ///
    /// Called after the resident retirement, never before: [`ArtifactStore::rehouse_members`]
    /// refuses a membership whose cardinality differs from the one it replaces, and what the
    /// extent holds is the post-retirement set. Running it first would refuse every artifact this
    /// fold took a member from and leave those on the heap.
    ///
    /// Returns how many memberships took and how many did not; a pack that will not open leaves
    /// its level on the heap and alarms.
    pub(in crate::write) fn rehouse_memberships(
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
                        "ALARM: an extent this node just wrote and fsynced would not open. Its \
                         memberships stay on the heap and keep answering, but a restart reads \
                         the same file and will fail the same way. Investigate the file at path."
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

    /// Apply the fold's executed deletions to the resident artifact store: retired artifacts leave
    /// their levels, retired members leave the memberships that survive, and orphaned content is
    /// withdrawn. Returns the levels the retirement moved.
    pub(in crate::write) fn retire_artifacts(&self, retired: &croaring::Bitmap) -> Vec<(String, u32)> {
        lock_recover(&self.artifacts).retire(retired)
    }

    /// Where an entity sits: `(layer, level, ordinal)`. Addressing only. See
    /// [`LayerRegistry::locate`].
    pub(crate) fn locate_artifact(&self, entity: EntityId) -> Option<(String, u32, u32)> {
        lock_recover(&self.registry)
            .locate(entity)
            .map(|(name, level, ordinal)| (name.to_string(), level, ordinal))
    }

    /// Resolve which layers a principal may know exist. See `LayerRegistry::resolve_for`. One
    /// set probe answers a gate-failed name and a never-registered one alike.
    pub(crate) fn resolve_layers(
        &self,
        is_satisfied: impl Fn(tessera_types::TermId) -> bool,
        resolve_label: impl Fn(&str) -> Option<tessera_types::TermId>,
    ) -> tessera_lifecycle::ResolvedLayers {
        lock_recover(&self.registry).resolve_for(is_satisfied, resolve_label)
    }

    /// Every registered layer, as the registry holds it: the declarations, never a decision. The
    /// caller applies the gate; this has no principal to resolve against.
    pub(crate) fn registered_layers(&self) -> Vec<tessera_types::layer::RegisteredLayer> {
        lock_recover(&self.registry).snapshot().0
    }

    /// One registered layer, by name. The caller has already established the name is reachable.
    /// This returns the declaration, never the decision.
    pub(crate) fn registered_layer(
        &self,
        name: &str,
    ) -> Option<tessera_types::layer::RegisteredLayer> {
        lock_recover(&self.registry).get(name).cloned()
    }

    /// Whether a registered layer resolves its memberships from shapes: [`spatial_membership`],
    /// for a layer held by name.
    pub(crate) fn spatial_layer(&self, name: &str) -> bool {
        self.registered_layer(name)
            .is_some_and(|registered| spatial_membership(&registered.declaration))
    }

    /// Every `membership = { attribute = f }` layer, with the declared-scalar index of `f` and the
    /// vocabulary that column's values are named by: `(layer, index, vocabulary)`.
    ///
    /// A layer whose column this bundle does not declare is skipped: such a declaration is refused
    /// at registration, so an absence here is fail-closed to "the layer holds no values".
    pub(in crate::write) fn predicate_columns(
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

    /// Record one level's re-evaluated serving layout, returning whether it moved.
    ///
    /// Under the registry's own lock and before the snapshot, which is the ordering the manifest
    /// publication requires: the manifest is written from [`LiveState::registry_for_publication`],
    /// so a layout recorded after that snapshot would reach neither the files nor the record.
    pub(in crate::write) fn record_layout(
        &self,
        layer: &str,
        level: u32,
        layout: tessera_types::layer::ServingLayout,
    ) -> bool {
        lock_recover(&self.registry).set_layout(layer, level, layout)
    }

    /// Run `f` with the roster held: the create and drop preparations, and nothing else.
    pub(in crate::write) fn with_roster<R>(&self, f: impl FnOnce(&mut tessera_lifecycle::ViewRoster) -> R) -> R {
        let mut roster = lock_recover(&self.roster);
        f(&mut roster)
    }

    /// Run `f` with the runtime attribute list held: the declaration's apply and the fold's
    /// retirement, and nothing else.
    pub(in crate::write) fn with_attributes<R>(
        &self,
        f: impl FnOnce(&mut crate::attributes::RuntimeAttributes) -> R,
    ) -> R {
        let mut attributes = lock_recover(&self.attributes);
        f(&mut attributes)
    }

    /// What a publication carries forward: the declarations no fold has written into a
    /// `MANIFEST.json`, complete current state, on [`Self::roster_for_publication`]'s contract.
    pub(in crate::write) fn attributes_for_publication(
        &self,
    ) -> (
        Vec<tessera_store::manifest::DeclaredScalar>,
        Vec<tessera_store::manifest::ScopedScalar>,
    ) {
        lock_recover(&self.attributes).snapshot()
    }

    /// Run `f` with the runtime vocabulary list held: the declaration's apply, a page's apply and
    /// the fold's retirement, and nothing else.
    pub(in crate::write) fn with_vocabularies<R>(
        &self,
        f: impl FnOnce(&mut crate::vocabularies::RuntimeVocabularies) -> R,
    ) -> R {
        let mut vocabularies = lock_recover(&self.vocabularies);
        f(&mut vocabularies)
    }

    /// What a publication carries forward: the vocabularies no fold has written into a
    /// `MANIFEST.json`, with their values as the live minters hold them, on
    /// [`Self::attributes_for_publication`]'s contract.
    pub(in crate::write) fn vocabularies_for_publication(
        &self,
        vocabularies: &Vocabularies,
    ) -> Vec<tessera_store::manifest::ManifestVocabulary> {
        lock_recover(&self.vocabularies).snapshot(vocabularies)
    }

    /// Run `f` with the runtime view declarations held: a declaration's apply and the fold's
    /// retirement, and nothing else.
    pub(in crate::write) fn with_view_declarations<R>(
        &self,
        f: impl FnOnce(&mut crate::view_declarations::RuntimeViewDeclarations) -> R,
    ) -> R {
        let mut declarations = lock_recover(&self.view_declarations);
        f(&mut declarations)
    }

    /// What a publication carries forward: the view groups and plain views no fold has written
    /// into a `MANIFEST.json`, complete current state.
    pub(in crate::write) fn view_declarations_for_publication(
        &self,
    ) -> (
        Vec<tessera_store::manifest::GroupDescriptor>,
        Vec<tessera_store::manifest::ViewDescriptor>,
    ) {
        lock_recover(&self.view_declarations).snapshot()
    }

    /// What a publication carries forward: the creations and the dead incarnations, complete
    /// current state. [`Self::registry_for_publication`]'s contract, for the roster.
    pub(in crate::write) fn roster_for_publication(
        &self,
    ) -> (
        Vec<tessera_types::view::CreatedView>,
        Vec<tessera_types::view::DeadIncarnation>,
    ) {
        lock_recover(&self.roster).snapshot()
    }

    /// The registry as a manifest carries it, plus the mark that must be published beside it.
    ///
    /// The three travel together, which is why one lock returns them. A manifest carrying a
    /// layer whose reserved run sits above the published mark would, at the next restart, hand
    /// that run out again. Reading them separately, with a registration in between, is a way to
    /// publish exactly that inconsistency.
    pub(in crate::write) fn registry_for_publication(
        &self,
    ) -> (Vec<tessera_types::layer::RegisteredLayer>, Vec<String>, u64) {
        let registry = lock_recover(&self.registry);
        let low_water = lock_recover(&self.allocator).low_water();
        let (layers, tombstones) = registry.snapshot();
        (layers, tombstones, low_water)
    }

    pub(in crate::write) fn accepted_batch(&self, batch_id: &str) -> Option<([u8; 32], Vec<EntityId>)> {
        lock_recover(&self.accepted_batches)
            .get(batch_id)
            .map(|held| (held.body_hash, held.entity_ids.clone()))
    }

    /// The descriptor bytes behind `terms`, read out of the resolver's extension map.
    ///
    /// `BufferedItem` holds resolved `TermId`s only, so a flush needs this to recover a
    /// descriptor. Total for any id a flush plan can name: replay always re-interns a buffered
    /// item's descriptors before it re-enters the buffer, so `promote` may treat a miss as a
    /// failed flush rather than a dropped term.
    pub(in crate::write) fn descriptors_of(&self, terms: &FxHashSet<TermId>) -> FxHashMap<TermId, Vec<u8>> {
        let state = lock_recover(&self.resolver_state);
        state
            .0
            .iter()
            .filter(|(_, id)| terms.contains(id))
            .map(|(descriptor, &id)| (id, descriptor.clone()))
            .collect()
    }

    pub(in crate::write) fn resolve_terms(&self, dict: &Dict, descriptors: &[Descriptor]) -> Vec<TermId> {
        let mut state = lock_recover(&self.resolver_state);
        let (extension, next_extension_id) = std::mem::take(&mut *state);
        let mut resolver = DescriptorResolver::resume(dict, extension, next_extension_id);
        let ids = descriptors.iter().map(|d| resolver.resolve(d)).collect();
        *state = resolver.into_state();
        ids
    }

    /// How many of `rows` name an external id the live map already holds. Backstops a race the
    /// executor's queue creates against the handler's own check in `control.rs`.
    ///
    /// Returns a count, never the ids: an external id must not reach a response body. A deleted
    /// holder does not collide and allocates fresh; otherwise a known external id is a join, and
    /// the row is stamped with the entity it joins.
    pub(in crate::write) fn established_collisions(
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

    /// Drop every retired entity's external-id binding from the live map: the other half of a
    /// deletion's retirement (Rule F). Without it, retirement 409s a lawful re-ingest, since
    /// compaction drops the same keys from the folded run and either alone leaves the other path
    /// answering.
    ///
    /// Called before the swap, not after, at the one site that also retires
    /// ([`Executor::publish_geometry`]): between a prune and a retirement the sidecar still
    /// answers deleted for the key, so the check still exempts it. The other order has a window
    /// where the key resolves to an entity that is no longer deleted.
    ///
    /// A rebind is not disturbed: the forward entry is removed only when it still names the
    /// retired entity.
    pub(in crate::write) fn forget_established(&self, retired: &croaring::Bitmap) -> usize {
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

    /// Run `f` with the entity id allocator held. The window's whole allocation is one call to
    /// this, so the sorted run and the high-water advance cannot be separated.
    pub(in crate::write) fn with_allocator<R>(&self, f: impl FnOnce(&mut Allocator) -> R) -> R {
        let mut alloc = lock_recover(&self.allocator);
        f(&mut alloc)
    }

    /// Index one accepted batch, at the position of the record that carries it.
    ///
    /// The three arguments are [`tessera_lifecycle::BatchIdentity`]'s three fields plus that
    /// position, and both accept sites check them against what the record they appended says.
    pub(in crate::write) fn record_accepted_batch(
        &self,
        batch_id: String,
        body_hash: [u8; 32],
        ids: Vec<EntityId>,
        wal_pos: u64,
    ) {
        lock_recover(&self.accepted_batches).insert(
            batch_id,
            AcceptedBatch {
                body_hash,
                entity_ids: ids,
                wal_pos,
            },
        );
    }

    /// Forget every batch whose record lay below `retained_from`: the rotation half of the
    /// idempotency horizon.
    ///
    /// A restart rebuilds this index from the members that survive, so dropping entries here
    /// keeps the horizon the same on both sides of a restart. Returns how many entries went.
    pub(in crate::write) fn forget_batches_below(&self, retained_from: u64) -> usize {
        let mut index = lock_recover(&self.accepted_batches);
        let before = index.len();
        index.retain(|_, held| held.wal_pos >= retained_from);
        before - index.len()
    }
}
