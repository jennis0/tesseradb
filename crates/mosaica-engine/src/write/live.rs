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

/// One layer space's deletions whose owing shards are known, and each such shard's share of them:
/// those a compaction settled, or those [`LiveState::owing`] read from the slices.
#[derive(Clone, Default)]
pub(in crate::write) struct SettledOwing {
    pub(in crate::write) deletions: croaring::Bitmap,
    pub(in crate::write) by_shard: BTreeMap<ShardId, croaring::Bitmap>,
}

impl SettledOwing {
    /// Each settled shard's share of `deleted`, leaving out a shard whose share is empty.
    fn share(&self, deleted: &croaring::Bitmap) -> BTreeMap<ShardId, croaring::Bitmap> {
        self.by_shard
            .iter()
            .map(|(shard, artifacts)| (*shard, artifacts.and(deleted)))
            .filter(|(_, share)| !share.is_empty())
            .collect()
    }

    /// [`Self::share`], as a side-manifest records it.
    pub(in crate::write) fn owed(
        &self,
        deleted: &croaring::Bitmap,
    ) -> Vec<mosaica_store::manifest::OwedDeletions> {
        owed_records(self.share(deleted))
    }
}

/// Each shard's artifact deletions, as a side-manifest records them.
fn owed_records(
    by_shard: BTreeMap<ShardId, croaring::Bitmap>,
) -> Vec<mosaica_store::manifest::OwedDeletions> {
    by_shard
        .into_iter()
        .map(
            |(shard, artifacts)| mosaica_store::manifest::OwedDeletions {
                shard,
                artifacts: mosaica_store::manifest::EntitySet::of(&artifacts),
            },
        )
        .collect()
}

/// Add each shard's share in `shares` to `into`.
fn add_shares(
    into: &mut BTreeMap<ShardId, croaring::Bitmap>,
    shares: BTreeMap<ShardId, croaring::Bitmap>,
) {
    for (shard, share) in shares {
        into.entry(shard).or_default().or_inplace(&share);
    }
}

/// `shard`'s slice of every membership in `written`. Panics on a shard the store holds no slice
/// of: answering from another shard's slice would project its entities through this shard's row
/// space.
pub(crate) fn slice_of(written: &ArtifactStore, shard: ShardId) -> &ArtifactStore {
    written
        .slice(shard)
        .expect("every served shard has a slice")
}

/// Every layer space's [`SettledOwing`], by space.
pub(in crate::write) type SettledSpaces = BTreeMap<LayerSpace, SettledOwing>;

/// What the side-manifest's layer spaces record for each shard whose compaction must remove a
/// deletion, read as settled: each space's deletions and each shard's share of them.
pub(in crate::write) fn settled_of(
    layer_spaces: &[mosaica_store::manifest::LayerSpaceRecord],
) -> SettledSpaces {
    layer_spaces
        .iter()
        .filter_map(|record| {
            let deletions = record.tombstones.entities()?.clone();
            let by_shard = record
                .owed
                .iter()
                .filter_map(|owed| Some((owed.shard, owed.artifacts.entities()?.clone())))
                .collect();
            (!deletions.is_empty()).then_some((
                record.space,
                SettledOwing {
                    deletions,
                    by_shard,
                },
            ))
        })
        .collect()
}

/// One point shard's write state, on [`LiveState`]'s contract.
pub(crate) struct ShardWrite {
    pub(in crate::write) id: ShardId,
    /// The entity id allocator, which holds whether the shard is open. Written only by the
    /// executor, never by a handler: entity ids are assigned at a window's close, on the one
    /// thread that also advances the high-water mark.
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

    /// Whether the shard takes new items at its high water, as its record, the log and the
    /// windows since leave it.
    pub(crate) fn state(&self) -> mosaica_types::ShardState {
        lock_recover(&self.allocator).state()
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

    /// Run `f` with the entity id allocator held.
    pub(in crate::write) fn with_allocator<R>(&self, f: impl FnOnce(&mut Allocator) -> R) -> R {
        let mut alloc = lock_recover(&self.allocator);
        f(&mut alloc)
    }
}

/// Every allocator of `shards`, ascending, locked in that order: a window's whole allocation runs
/// under these, so the sorted run and every high-water advance cannot be separated.
pub(in crate::write) fn lock_allocators(
    shards: &[Arc<ShardWrite>],
) -> Vec<MutexGuard<'_, Allocator>> {
    shards
        .iter()
        .map(|shard| lock_recover(&shard.allocator))
        .collect()
}

/// State the handler side reads and the executor thread writes.
///
/// Each field carries its own lock because it is read concurrently by a request path that is not
/// the executor (`/control/ingest`'s replay check, `/control/status`'s high-water). The generation pointer is not held here: it is
/// swapped by `Executor::publish_arc`, the crate's one non-atomic store.
pub(crate) struct LiveState {
    /// Each point shard's write state, ascending by id, the open one taking new items. Grown only
    /// by the executor, when a window that opens a shard commits.
    pub(in crate::write) shards: RwLock<Vec<Arc<ShardWrite>>>,
    /// One past the highest shard id ever opened, as the side-manifest's `shards.next_id`, the log
    /// and the windows since leave it. Written only by the executor.
    pub(in crate::write) next_shard_id: std::sync::atomic::AtomicU32,
    /// The high water at which the open shard seals, as the side-manifest's `shards.size` and the
    /// log leave it. Written only by the executor.
    pub(in crate::write) size: std::sync::atomic::AtomicU64,
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
    /// executor, read by the request path. Holds every record with the first shard's slice of each
    /// membership, and every other shard's slice ([`ArtifactStore::slice`]).
    pub(in crate::write) artifacts: Mutex<ArtifactStore>,
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
    /// The deletions for which the open or a compaction settled the shards whose compaction must
    /// remove them. Written only by the executor, at a compaction's publication.
    pub(in crate::write) settled_owing: Mutex<SettledSpaces>,
    /// The deletions outside `settled_owing` whose owing shards [`Self::owing`] has read from the
    /// slices since the artifact store was last written. Written only by the executor, and emptied
    /// at every write of the store, since a write can change which slices hold a deleted artifact.
    pub(in crate::write) probed_owing: Mutex<SettledSpaces>,
}

impl LiveState {
    /// Every shard's write state, ascending by id.
    pub(crate) fn shards(&self) -> Vec<Arc<ShardWrite>> {
        self.read_shards().clone()
    }

    fn read_shards(&self) -> std::sync::RwLockReadGuard<'_, Vec<Arc<ShardWrite>>> {
        self.shards.read().unwrap_or_else(|e| e.into_inner())
    }

    /// The highest-numbered shard: the open one, which takes new items at its high water, where
    /// one is open. Where every shard is sealed, the next window to place a new item opens one.
    pub(crate) fn last_shard(&self) -> Arc<ShardWrite> {
        let shards = self.read_shards();
        Arc::clone(shards.last().expect("a bundle holds a shard"))
    }

    /// Shard `id`'s write state, or `None` for a shard the bundle does not hold.
    pub(crate) fn shard(&self, id: ShardId) -> Option<Arc<ShardWrite>> {
        self.read_shards()
            .iter()
            .find(|shard| shard.id == id)
            .map(Arc::clone)
    }

    /// Every shard's high water, summed.
    pub(crate) fn high_water(&self) -> u64 {
        self.read_shards()
            .iter()
            .map(|shard| shard.high_water())
            .sum()
    }

    /// One past the highest shard id ever opened.
    pub(crate) fn next_shard_id(&self) -> u32 {
        self.next_shard_id
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Take the write state of the shards a committed window opened.
    pub(in crate::write) fn add_shards(&self, opened: Vec<ShardWrite>) {
        let mut shards = self.shards.write().unwrap_or_else(|e| e.into_inner());
        for shard in opened {
            self.next_shard_id
                .fetch_max(shard.id.raw() + 1, std::sync::atomic::Ordering::Relaxed);
            shards.push(Arc::new(shard));
        }
        shards.sort_by_key(|shard| shard.id);
    }

    /// Whether a window whose edits take `edits` ids in each shard and whose `items` new items are
    /// placed fits beside every shard's allocator and the shards it may open
    /// ([`mosaica_lifecycle::alloc::check_window`]).
    pub(crate) fn window_room(
        &self,
        edits: &BTreeMap<ShardId, u64>,
        items: u64,
    ) -> Result<(), mosaica_lifecycle::AllocError> {
        let shards = self.shards();
        let allocators = lock_allocators(&shards);
        let held: Vec<(ShardId, &Allocator)> = shards
            .iter()
            .zip(&allocators)
            .map(|(shard, alloc)| (shard.id, &**alloc))
            .collect();
        mosaica_lifecycle::alloc::check_window(
            &held,
            edits,
            items,
            self.shard_size(),
            self.next_shard_id(),
        )
    }

    /// The size every shard is allocated to.
    pub(crate) fn shard_size(&self) -> u64 {
        self.size.load(std::sync::atomic::Ordering::Relaxed)
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
            drop(registry);
            self.forget_probed_owing();
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
        let out = {
            let mut registry = lock_recover(&self.registry);
            let mut artifacts = lock_recover(&self.artifacts);
            f(&mut registry, &mut artifacts)
        };
        self.forget_probed_owing();
        out
    }

    /// The bound rotation may not reclaim past, or `None` if no membership is at risk. See
    /// [`ArtifactStore::oldest_wal_pos`]. This pins the log until membership has a home outside
    /// it.
    pub(in crate::write) fn artifacts_oldest_wal_pos(&self) -> Option<u64> {
        lock_recover(&self.artifacts).oldest_wal_pos()
    }

    /// Give the artifact store a slice for each of `shards`, opened holding no item.
    pub(in crate::write) fn open_slices(&self, shards: impl IntoIterator<Item = ShardId>) {
        let mut artifacts = lock_recover(&self.artifacts);
        for shard in shards {
            artifacts.open_slice(shard);
        }
        drop(artifacts);
        self.forget_probed_owing();
    }

    /// Read the artifact store: the request path's route to a membership.
    pub(crate) fn with_artifacts<R>(&self, f: impl FnOnce(&ArtifactStore) -> R) -> R {
        f(&lock_recover(&self.artifacts))
    }

    /// Everything not yet in a manifest, packed and ready. See [`ArtifactStore::unpublished`].
    pub(in crate::write) fn unpublished_memberships(
        &self,
    ) -> Vec<mosaica_lifecycle::membership::PendingLevel> {
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
        extents: &executor::PackedMembers,
    ) -> (u64, u64) {
        let mut artifacts = lock_recover(&self.artifacts);
        let (mut rehoused, mut kept) = (0u64, 0u64);
        for (shard, extent) in extents
            .iter()
            .flat_map(|(shard, extents)| extents.iter().map(move |extent| (*shard, extent)))
        {
            let Some(layer) = artifacts.layer_in(extent.space).map(str::to_string) else {
                continue;
            };
            let Some(slice) = artifacts.slice_mut(shard) else {
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
                // An empty blob is an artifact the shard holds nothing of, or a hole: an ordinal a
                // retirement emptied, or one no artifact was ever published at. There is nothing
                // to rehouse and nothing is wrong.
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
                    Some(members) => slice.rehouse_members(&layer, extent.level, ordinal, members),
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
    ) -> Vec<mosaica_lifecycle::membership::MovedLevel> {
        let moved = lock_recover(&self.artifacts).retire(retired);
        self.forget_probed_owing();
        moved
    }

    /// Free the key of each artifact `deleted` names, where the key still names it: a
    /// publication under the key then creates a new artifact, and a growth or fill by it is
    /// refused. The records stay until a compaction removes them.
    pub(in crate::write) fn release_artifact_keys(&self, deleted: &[(LayerSpace, EntityId)]) {
        if deleted.is_empty() {
            return;
        }
        self.with_publication_state(|registry, store| {
            for (space, entity) in deleted {
                if let Some((name, level, ordinal)) = registry.locate_in(*space, *entity) {
                    store.release_key(name, level, ordinal);
                }
            }
        });
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

    /// For each shard, the artifacts of `deleted` in `space` its compaction has to remove from its
    /// slice. A settled deletion is removed by the compactions of the shards settled for it. Any
    /// other is removed by the compactions of the shards whose slice holds a member or a
    /// generating entity of it, or borrows a membership for it, and of every shard where its
    /// layer's memberships are not stored, since each shard derives them. A shard with none to
    /// remove is left out, and so is every shard where no live layer holds `space` and the
    /// deletion is not settled.
    ///
    /// The slices are read once for a deletion until the artifact store is next written.
    pub(in crate::write) fn owing(
        &self,
        space: LayerSpace,
        deleted: &croaring::Bitmap,
    ) -> BTreeMap<ShardId, croaring::Bitmap> {
        let (mut owing, unsettled) = match lock_recover(&self.settled_owing).get(&space) {
            Some(settled) => (settled.share(deleted), deleted.andnot(&settled.deletions)),
            None => (BTreeMap::new(), deleted.clone()),
        };
        let unprobed = match lock_recover(&self.probed_owing).get(&space) {
            Some(probed) => {
                add_shares(&mut owing, probed.share(&unsettled));
                unsettled.andnot(&probed.deletions)
            }
            None => unsettled,
        };
        if unprobed.is_empty() {
            return owing;
        }
        let Some(probed) = self.probe_owing(space, &unprobed) else {
            return owing;
        };
        let mut memo = lock_recover(&self.probed_owing);
        let held = memo.entry(space).or_default();
        held.deletions.or_inplace(&unprobed);
        add_shares(&mut held.by_shard, probed.clone());
        drop(memo);
        add_shares(&mut owing, probed);
        owing
    }

    /// [`Self::owing`] for deletions neither settled nor probed, read from every shard's slice, or
    /// `None` where no live layer holds `space`.
    fn probe_owing(
        &self,
        space: LayerSpace,
        deleted: &croaring::Bitmap,
    ) -> Option<BTreeMap<ShardId, croaring::Bitmap>> {
        let registry = lock_recover(&self.registry);
        let (name, layer) = registry.in_space(space)?;
        let every = !stored_membership(&layer.declaration);
        let written = lock_recover(&self.artifacts);
        let places: Vec<(u32, u32, u32)> = deleted
            .iter()
            .filter_map(|entity| {
                let (_, level, ordinal) =
                    registry.locate_in(space, EntityId::new(u64::from(entity)))?;
                Some((entity, level, ordinal))
            })
            .collect();
        let mut owing = BTreeMap::new();
        for shard in self.shards() {
            let slice = slice_of(&written, shard.id);
            let owed: croaring::Bitmap = match every {
                true => deleted.clone(),
                false => places
                    .iter()
                    .filter(|(_, level, ordinal)| {
                        slice.get(name, *level, *ordinal).is_some_and(|record| {
                            mosaica_lifecycle::membership::borrows_membership(record)
                                || !record.members.is_empty()
                                || record
                                    .contents
                                    .iter()
                                    .any(|content| !content.generated_from.is_empty())
                        })
                    })
                    .map(|(entity, ..)| *entity)
                    .collect(),
            };
            if !owed.is_empty() {
                owing.insert(shard.id, owed);
            }
        }
        Some(owing)
    }

    /// Empty [`Self::probed_owing`]: the artifact store was written.
    fn forget_probed_owing(&self) {
        lock_recover(&self.probed_owing).clear();
    }

    /// [`Self::owing`], as a side-manifest records it.
    pub(in crate::write) fn owed_deletions(
        &self,
        space: LayerSpace,
        deleted: &croaring::Bitmap,
    ) -> Vec<mosaica_store::manifest::OwedDeletions> {
        owed_records(self.owing(space, deleted))
    }

    /// Replace the settled deletions with `settled`, what a compaction's publication left for
    /// later compactions to remove.
    pub(in crate::write) fn settle_owing(&self, settled: SettledSpaces) {
        *lock_recover(&self.settled_owing) = settled;
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
