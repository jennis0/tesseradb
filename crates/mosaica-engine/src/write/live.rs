use super::*;

// =================================================================================================
// Live state
// =================================================================================================

/// One accepted batch, as the idempotency index holds it: what each row of the request became,
/// so a byte-identical replay answers the `mosaica_id`s the first acceptance did.
#[derive(Clone)]
pub(in crate::write) struct AcceptedBatch {
    pub(in crate::write) body_hash: [u8; 32],
    pub(in crate::write) receipt: Vec<mosaica_lifecycle::RowReceipt>,
    /// Where the record carrying this batch lies in the log. The index is a cache of the WAL
    /// ([`mosaica_lifecycle::batch_identity`]), so an entry outlives its record only until the
    /// rotation that deletes the member holding it. `Executor::rotate_wal` forgets everything
    /// below [`ExecutorWal::retained_from`], which is the horizon a restart would rebuild.
    pub(in crate::write) wal_pos: u64,
}

/// The `/control/ingest` idempotency index: batch id -> what was accepted
/// under it.
pub(in crate::write) type AcceptedBatches = FxHashMap<String, AcceptedBatch>;

/// The descriptor resolver's detached extension state: interned novel descriptors, plus the next
/// extension id to hand out.
pub(in crate::write) type ResolverState = (FxHashMap<Vec<u8>, TermId>, u32);

/// The layer registry as a side-manifest carries it.
pub(in crate::write) struct RegistryForPublication {
    pub layers: Vec<mosaica_types::layer::RegisteredLayer>,
    pub next_layer_space: u32,
    /// The registry's version counter.
    pub version: u64,
}

/// One point shard's write state, on [`LiveState`]'s contract.
pub(crate) struct ShardWrite {
    pub(in crate::write) id: ShardId,
    /// The entity id allocator. Written only by the executor, never by a handler: entity ids are
    /// assigned at a window's close, on the one thread that also advances the high-water mark.
    allocator: Mutex<Allocator>,
}

impl ShardWrite {
    pub(in crate::write) fn new(id: ShardId, allocator: Allocator) -> ShardWrite {
        ShardWrite {
            id,
            allocator: Mutex::new(allocator),
        }
    }

    pub(crate) fn high_water(&self) -> u64 {
        lock_recover(&self.allocator).high_water()
    }

    pub(crate) fn size(&self) -> u64 {
        lock_recover(&self.allocator).size()
    }

    /// [`Allocator::room`], under the lock.
    pub(in crate::write) fn room(
        &self,
        edits: u64,
        items: u64,
    ) -> Result<(), mosaica_types::ShardFull> {
        lock_recover(&self.allocator).room(edits, items)
    }

    /// The allocator's freed ids and the sets it holds back, as a side-manifest records them.
    pub(crate) fn freed(
        &self,
    ) -> (
        mosaica_store::manifest::EntitySet,
        Vec<mosaica_store::manifest::HeldEntities>,
    ) {
        use mosaica_lifecycle::union_of;
        let alloc = lock_recover(&self.allocator);
        let held = alloc
            .held()
            .iter()
            .map(|(position, ids)| mosaica_store::manifest::HeldEntities {
                position: *position,
                entities: mosaica_store::manifest::EntitySet::of(&union_of(ids)),
            })
            .collect();
        let free = mosaica_store::manifest::EntitySet::of(&union_of(alloc.free()));
        (free, held)
    }

    /// Run `f` with the entity id allocator held. The window's whole allocation is one call to
    /// this, so the sorted run and the high-water advance cannot be separated.
    pub(in crate::write) fn with_allocator<R>(&self, f: impl FnOnce(&mut Allocator) -> R) -> R {
        let mut alloc = lock_recover(&self.allocator);
        f(&mut alloc)
    }
}

/// State the handler side reads and the executor thread writes.
///
/// Each field carries its own lock because it is read concurrently by a request path that is not
/// the executor (`/control/ingest`'s replay check, `/control/status`'s high-water). The generation pointer is not held here: it is
/// swapped by `Executor::publish_arc`, the crate's one non-atomic store.
pub(crate) struct LiveState {
    /// Each point shard's write state, ascending by id.
    pub(in crate::write) shards: Vec<ShardWrite>,
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
    /// executor, read by the request path. Of a bundle of several point shards it holds the write
    /// shard's slice of each membership and generating set, and [`Self::slices`] the others'.
    pub(in crate::write) artifacts: Mutex<ArtifactStore>,
    /// Every point shard's slice of every artifact's membership and generating sets but the one
    /// [`Self::artifacts`] holds, read at open, each beside what a row form reads of the record:
    /// its view, attachment and each set's cardinality. A record's key, labels and parents are
    /// read from [`Self::artifacts`]. A bundle of several shards takes no write, so nothing writes
    /// these.
    pub(in crate::write) slices: Vec<(ShardId, Arc<ArtifactStore>)>,
    /// The view roster, on the registry's contract: written only by the executor, a create is a
    /// WAL append followed by an apply, on the one thread that also holds the allocator. Read by
    /// the request path, which resolves a view id against the manifest the roster made.
    pub(in crate::write) roster: Mutex<mosaica_lifecycle::ViewRoster>,
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
    pub(in crate::write) view_declarations:
        Mutex<crate::view_declarations::RuntimeViewDeclarations>,
}

impl LiveState {
    /// The one shard written to until several are.
    pub(crate) fn only_shard(&self) -> &ShardWrite {
        mosaica_types::only(&self.shards)
    }

    /// Runs `f` with the registry held: a registration's or a drop's preparation.
    pub(in crate::write) fn with_registry<R>(&self, f: impl FnOnce(&mut LayerRegistry) -> R) -> R {
        f(&mut lock_recover(&self.registry))
    }

    /// Applies a registry record, and a drop to the artifact store too, so a layer registered
    /// later under the name starts from no artifact.
    pub(in crate::write) fn apply_registry_record(&self, record: &WalRecord) {
        let mut registry = lock_recover(&self.registry);
        registry.apply(record);
        if let WalRecord::LayerDrop { name, .. } = record {
            lock_recover(&self.artifacts).remove_layer(name);
        }
    }

    /// Runs `f` with the registry and the artifact store held, in that lock order.
    ///
    /// A publication reads the level's cursor and the space's next block and writes both, so two
    /// batches taking the locks separately would be handed the same ordinals. This order is taken
    /// everywhere the two are held together, so it cannot deadlock.
    pub(in crate::write) fn with_publication_state<R>(
        &self,
        f: impl FnOnce(&mut LayerRegistry, &mut ArtifactStore) -> R,
    ) -> R {
        let mut registry = lock_recover(&self.registry);
        let mut artifacts = lock_recover(&self.artifacts);
        f(&mut registry, &mut artifacts)
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

    /// `shard`'s slice of every membership, given `written`, the store [`Self::with_artifacts`]
    /// lends, which is the write shard's. Panics on a shard the open read no slice of: answering
    /// from another shard's slice would project its entities through this shard's row space.
    pub(crate) fn slice_of<'a>(
        &'a self,
        shard: ShardId,
        written: &'a ArtifactStore,
    ) -> &'a ArtifactStore {
        if shard == self.only_shard().id {
            return written;
        }
        self.slices
            .iter()
            .find(|(held, _)| *held == shard)
            .map(|(_, slice)| slice.as_ref())
            .expect("every shard's slice is read at open")
    }

    /// Everything not yet in a manifest, packed and ready. See [`ArtifactStore::unpublished`].
    pub(in crate::write) fn unpublished_memberships(
        &self,
    ) -> Vec<mosaica_lifecycle::membership::PendingExtent> {
        lock_recover(&self.artifacts).unpublished()
    }

    /// The supplied content of every artifact not yet in a manifest. See
    /// [`mosaica_lifecycle::membership::ArtifactStore::unpublished_content`].
    pub(in crate::write) fn unpublished_content(
        &self,
    ) -> Vec<mosaica_lifecycle::membership::ContentRow> {
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
    /// manifest. See [`mosaica_lifecycle::membership::ArtifactStore::mark_content_published`].
    ///
    /// Called from the overlay publication, not the fold: the fold carries content extents
    /// forward unchanged, so a fill pending at a fold is still pending after it.
    pub(in crate::write) fn mark_content_published(&self) {
        lock_recover(&self.artifacts).mark_content_published();
    }

    /// Release the log from every growth the fold's whole rewrite has just made durable. See
    /// [`mosaica_lifecycle::membership::ArtifactStore::mark_growth_packed`].
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
        extents: &[mosaica_store::manifest::MembershipExtent],
    ) -> (u64, u64) {
        let mut artifacts = lock_recover(&self.artifacts);
        let (mut rehoused, mut kept) = (0u64, 0u64);
        for extent in extents {
            let Some(layer) = artifacts.layer_in(extent.space).map(str::to_string) else {
                continue;
            };
            let path = prefix_dir.join(&extent.path);
            let pack = match mosaica_store::membership::LevelPack::open(
                &path,
                mosaica_store::membership::PackKind::Members,
            ) {
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
                    unsafe { mosaica_lifecycle::membership::mapped_members(blob, owner.clone()) };
                let took = match mapped {
                    // `LevelPack::iter` answers the absolute ordinal, which is what the
                    // store addresses by.
                    Some(members) => {
                        artifacts.rehouse_members(&layer, extent.level, ordinal, members)
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
    pub(in crate::write) fn retire_artifacts(
        &self,
        retired: &mosaica_lifecycle::membership::Retired,
    ) -> Vec<(String, u32)> {
        lock_recover(&self.artifacts).retire(retired)
    }

    /// Where an entity of `space` sits: `(level, ordinal)`. Addressing only. See
    /// [`LayerRegistry::locate_in`].
    pub(crate) fn locate_artifact(
        &self,
        space: LayerSpace,
        entity: EntityId,
    ) -> Option<(u32, u32)> {
        lock_recover(&self.registry)
            .locate_in(space, entity)
            .map(|(_, level, ordinal)| (level, ordinal))
    }

    /// Whether `entity` of `space` is a live layer's own entity or an artifact its levels hold.
    pub(crate) fn artifact_held(&self, space: LayerSpace, entity: EntityId) -> bool {
        let registry = lock_recover(&self.registry);
        if registry
            .in_space(space)
            .is_some_and(|_| entity == mosaica_types::layer::LAYER_ENTITY)
        {
            return true;
        }
        let Some((name, level, ordinal)) = registry.locate_in(space, entity) else {
            return false;
        };
        lock_recover(&self.artifacts)
            .get(name, level, ordinal)
            .is_some()
    }

    /// The name of the live layer whose own entity `entity` of `space` is.
    pub(crate) fn layer_of_own_entity(
        &self,
        space: LayerSpace,
        entity: EntityId,
    ) -> Option<String> {
        lock_recover(&self.registry)
            .in_space(space)
            .filter(|_| entity == mosaica_types::layer::LAYER_ENTITY)
            .map(|(name, _)| name.to_string())
    }

    /// Every artifact that depends, directly or transitively, on an artifact `deleted` names, as
    /// a target in its layer's space; none of `deleted`. See [`ArtifactStore::cascade_from`].
    pub(in crate::write) fn dependents_of(&self, deleted: &[Target]) -> Vec<Target> {
        let roots: Vec<mosaica_lifecycle::membership::ArtifactAt> = deleted
            .iter()
            .filter_map(|target| match target {
                Target::Artifact(space, entity) => Some((*space, *entity)),
                Target::Item(..) => None,
            })
            .collect();
        if roots.is_empty() {
            return Vec::new();
        }
        lock_recover(&self.artifacts)
            .cascade_from(&roots)
            .into_iter()
            .map(|(space, entity)| Target::Artifact(space, entity))
            .collect()
    }

    /// Resolve which layers a principal may know exist. See `LayerRegistry::resolve_for`. One
    /// set probe answers a gate-failed name and a never-registered one alike.
    pub(crate) fn resolve_layers(
        &self,
        admits: impl Fn(&str) -> bool,
    ) -> mosaica_lifecycle::ResolvedLayers {
        lock_recover(&self.registry).resolve_for(admits)
    }

    /// The registry's version, which every registration and drop moves.
    pub(crate) fn registry_version(&self) -> u64 {
        lock_recover(&self.registry).version()
    }

    /// Every registered layer, as the registry holds it: the declarations, never a decision. The
    /// caller applies the gate; this has no principal to resolve against.
    pub(crate) fn registered_layers(&self) -> Vec<mosaica_types::layer::RegisteredLayer> {
        lock_recover(&self.registry).snapshot()
    }

    /// How many layers are registered.
    pub(crate) fn layer_count(&self) -> usize {
        lock_recover(&self.registry).len()
    }

    /// The name of the live layer holding `space`.
    pub(crate) fn layer_in_space(&self, space: LayerSpace) -> Option<String> {
        lock_recover(&self.registry)
            .in_space(space)
            .map(|(name, _)| name.to_string())
    }

    /// The live layer holding `space`, as [`Self::registered_layer`] answers it.
    pub(crate) fn layer_holding(
        &self,
        space: LayerSpace,
    ) -> Option<mosaica_types::layer::RegisteredLayer> {
        lock_recover(&self.registry)
            .in_space(space)
            .map(|(_, layer)| layer.clone())
    }

    /// Whether a live layer holds `space`.
    pub(crate) fn holds_space(&self, space: LayerSpace) -> bool {
        lock_recover(&self.registry).in_space(space).is_some()
    }

    /// The space of the layer registered under `name`.
    pub(crate) fn space_of(&self, name: &str) -> Option<LayerSpace> {
        lock_recover(&self.registry).get(name).map(|layer| layer.space)
    }

    /// One registered layer, by name. The caller has already established the name is reachable.
    /// This returns the declaration, never the decision.
    pub(crate) fn registered_layer(
        &self,
        name: &str,
    ) -> Option<mosaica_types::layer::RegisteredLayer> {
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
                let mosaica_types::layer::MembershipSource::Attribute(field) =
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
        layout: mosaica_types::layer::ServingLayout,
    ) -> bool {
        lock_recover(&self.registry).set_layout(layer, level, layout)
    }

    /// Run `f` with the roster held: the create and drop preparations, and nothing else.
    pub(in crate::write) fn with_roster<R>(
        &self,
        f: impl FnOnce(&mut mosaica_lifecycle::ViewRoster) -> R,
    ) -> R {
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
        Vec<mosaica_store::manifest::DeclaredScalar>,
        Vec<mosaica_store::manifest::ScopedScalar>,
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
    ) -> Vec<mosaica_store::manifest::ManifestVocabulary> {
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
        Vec<mosaica_store::manifest::GroupDescriptor>,
        Vec<mosaica_store::manifest::ViewDescriptor>,
    ) {
        lock_recover(&self.view_declarations).snapshot()
    }

    /// What a publication carries forward: the creations and the dead incarnations, complete
    /// current state. [`Self::registry_for_publication`]'s contract, for the roster.
    pub(in crate::write) fn roster_for_publication(
        &self,
    ) -> (
        Vec<mosaica_types::view::CreatedView>,
        Vec<mosaica_types::view::DeadIncarnation>,
    ) {
        lock_recover(&self.roster).snapshot()
    }

    /// The registry as a manifest carries it, read under one lock so that no registration lands
    /// between its parts: a layer whose space sat at or past the published counter would have that
    /// space given again at the next restart.
    pub(in crate::write) fn registry_for_publication(&self) -> RegistryForPublication {
        let registry = lock_recover(&self.registry);
        RegistryForPublication {
            layers: registry.snapshot(),
            next_layer_space: registry.next_layer_space(),
            version: registry.version(),
        }
    }

    pub(crate) fn accepted_batch(
        &self,
        batch_id: &str,
    ) -> Option<([u8; 32], Vec<mosaica_lifecycle::RowReceipt>)> {
        lock_recover(&self.accepted_batches)
            .get(batch_id)
            .map(|held| (held.body_hash, held.receipt.clone()))
    }

    /// The descriptor bytes behind `terms`, read out of the resolver's extension map.
    ///
    /// `BufferedItem` holds resolved `TermId`s only, so a flush needs this to recover a
    /// descriptor. Total for any id a flush plan can name: replay always re-interns a buffered
    /// item's descriptors before it re-enters the buffer, so `promote` may treat a miss as a
    /// failed flush rather than a dropped term.
    pub(crate) fn descriptors_of(&self, terms: &FxHashSet<TermId>) -> FxHashMap<TermId, Vec<u8>> {
        let state = lock_recover(&self.resolver_state);
        state
            .0
            .iter()
            .filter(|(_, id)| terms.contains(id))
            .map(|(descriptor, &id)| (id, descriptor.clone()))
            .collect()
    }

    pub(crate) fn resolve_terms(&self, dict: &Dict, descriptors: &[Vec<u8>]) -> Vec<TermId> {
        let mut state = lock_recover(&self.resolver_state);
        let (extension, next_extension_id) = std::mem::take(&mut *state);
        let mut resolver = DescriptorResolver::resume(dict, extension, next_extension_id);
        let ids = descriptors.iter().map(|d| resolver.resolve(d)).collect();
        *state = resolver.into_state();
        ids
    }

    /// The terms `descriptors` resolve to, interning none: `None` where one is novel, which no
    /// stored label holds.
    pub(crate) fn lookup_terms(&self, dict: &Dict, descriptors: &[Vec<u8>]) -> Option<Vec<TermId>> {
        let state = lock_recover(&self.resolver_state);
        descriptors
            .iter()
            .map(|d| {
                dict.lookup(d)
                    .or_else(|| state.0.get(d.as_slice()).copied())
            })
            .collect()
    }

    /// Index one accepted batch, at the position of the record that carries it.
    ///
    /// The three arguments are [`mosaica_lifecycle::BatchIdentity`]'s three fields plus that
    /// position.
    pub(in crate::write) fn record_accepted_batch(
        &self,
        batch_id: String,
        body_hash: [u8; 32],
        receipt: Vec<mosaica_lifecycle::RowReceipt>,
        wal_pos: u64,
    ) {
        lock_recover(&self.accepted_batches).insert(
            batch_id,
            AcceptedBatch {
                body_hash,
                receipt,
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
