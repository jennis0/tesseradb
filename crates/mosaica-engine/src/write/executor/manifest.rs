use super::*;

/// Derived files, by the shard holding them; a layer space's held shapes are filed under the first
/// shard.
pub(in crate::write) type DerivedByShard =
    std::collections::BTreeMap<ShardId, Vec<mosaica_store::manifest::DerivedExtent>>;

/// What a publication stamps on its layer spaces and shards: each level's records version by
/// space, each shard's slice versions, and the derived files still adoptable at them.
pub(super) struct ArtifactCoordinates {
    pub(super) records:
        std::collections::BTreeMap<LayerSpace, Vec<mosaica_store::manifest::LevelVersion>>,
    pub(super) slices:
        std::collections::BTreeMap<ShardId, Vec<mosaica_store::manifest::SliceVersion>>,
    pub(super) derived: Vec<(ShardId, mosaica_store::manifest::DerivedExtent)>,
}

/// Every level's records version, by space, each shard's slice versions, and the derived files of
/// `held` still adoptable at them: a file stays while the level's records version and its shard's
/// slice version are both the ones it was written at. A file whose level has moved is dropped, so
/// a manifest never names one nothing could adopt, except the row column of a layer in
/// `enumerated` and the member file beside it: an enumerated membership only grows between folds,
/// and an open completes a column the level has moved past
/// ([`crate::artifacts::ArtifactProjections::adopt_columns`]). Nothing completes a level's label
/// column or its band-order copy, so those are dropped with every other file, and a level holds
/// a copy only while the copy is current. A spatial layer's column is read only at its level's
/// version, so it is dropped like any other file. Only the layers in `spaces`, the registered
/// ones by name, are named, each at its space.
pub(super) fn artifact_coordinates(
    store: &ArtifactStore,
    held: &DerivedByShard,
    pending: Option<&PendingRetirement>,
    spaces: &std::collections::BTreeMap<String, LayerSpace>,
    enumerated: &std::collections::BTreeSet<String>,
) -> ArtifactCoordinates {
    let records_at = |layer: &str, level: u32| match pending {
        Some(pending) => pending.records_version_after(store, layer, level),
        None => store.records_version(layer, level),
    };
    let slice_at = |shard: ShardId, layer: &str, level: u32| match pending {
        Some(pending) => pending.version_after(store, shard, layer, level),
        None => Some(store.slice(shard)?.level_version(layer, level)),
    };
    let mut records = std::collections::BTreeMap::<_, Vec<_>>::new();
    for (layer, level, _) in store.records_versions() {
        if let Some(space) = spaces.get(layer) {
            records
                .entry(*space)
                .or_default()
                .push(mosaica_store::manifest::LevelVersion {
                    level,
                    version: records_at(layer, level),
                });
        }
    }
    let mut slices = std::collections::BTreeMap::<_, Vec<_>>::new();
    for shard in store.shards() {
        let Some(slice) = store.slice(shard) else {
            continue;
        };
        let versions = slices.entry(shard).or_default();
        for (layer, level, _) in slice.slice_versions() {
            if let (Some(space), Some(version)) = (spaces.get(layer), slice_at(shard, layer, level))
            {
                versions.push(mosaica_store::manifest::SliceVersion {
                    space: *space,
                    level,
                    version,
                });
            }
        }
    }
    let derived = held
        .iter()
        .flat_map(|(shard, extents)| extents.iter().map(move |extent| (*shard, extent)))
        .filter(|(shard, entry)| {
            let Some((layer, _)) = spaces.iter().find(|(_, space)| **space == entry.space) else {
                return false;
            };
            let records_now = records_at(layer, entry.level);
            let Some(slice_now) = slice_at(*shard, layer, entry.level) else {
                return false;
            };
            match entry.form {
                mosaica_store::manifest::DerivedForm::RowColumn { .. }
                | mosaica_store::manifest::DerivedForm::RowMembers
                    if enumerated.contains(layer) =>
                {
                    entry.level_version <= records_now && entry.slice_version <= slice_now
                }
                _ => entry.level_version == records_now && entry.slice_version == slice_now,
            }
        })
        .map(|(shard, entry)| (shard, entry.clone()))
        .collect();
    ArtifactCoordinates {
        records,
        slices,
        derived,
    }
}

/// Why [`Executor::commit_side_manifest`] did not commit: the manifest would regress durable
/// state, or the store could not write it.
pub(super) enum ManifestCommitRefused {
    Regresses(crate::geometry::ManifestRegression),
    Store(mosaica_store::StoreError),
}

impl std::fmt::Display for ManifestCommitRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ManifestCommitRefused::Regresses(r) => r.fmt(f),
            ManifestCommitRefused::Store(e) => e.fmt(f),
        }
    }
}

/// Every shard's overlay in `generation`, as [`write_deny_state`] takes them.
pub(super) fn shard_overlays(generation: &Generation) -> impl Iterator<Item = (ShardId, &Overlay)> {
    generation
        .shards()
        .map(|shard| (shard.id(), &*shard.overlay))
}

/// Replace a manifest's deny fields, each named shard's and each layer space's, with the overlays'
/// live state, restated fresh at every write as two separate sets and never their union:
/// publishing the union would make every deletion look retirable by an unsuppress. Each space's
/// `owed` list is what `owed` answers for the space's deletions.
pub(super) fn write_deny_state<'a>(
    manifest: &mut SegmentsManifest,
    overlays: impl IntoIterator<Item = (ShardId, &'a Overlay)>,
    spaces: &crate::LayerSpaces,
    owed: impl Fn(LayerSpace, &croaring::Bitmap) -> Vec<mosaica_store::manifest::OwedDeletions>,
) {
    for (shard, overlay) in overlays {
        let Some(record) = manifest.shard_mut(shard) else {
            continue;
        };
        record.deny = EntitySet::of(overlay.suppressed_set());
        record.tombstones = EntitySet::of(overlay.deleted_set());
    }
    for space in spaces.keys() {
        mosaica_store::manifest::layer_space_mut(&mut manifest.layer_spaces, *space);
    }
    let shards: std::collections::BTreeSet<ShardId> =
        manifest.shards.records.iter().map(|r| r.id).collect();
    for record in &mut manifest.layer_spaces {
        let overlay = match spaces.get(&record.space) {
            Some(parts) => &*parts.overlay,
            None => &Overlay::new(),
        };
        record.deny = EntitySet::of(overlay.suppressed_set());
        let deleted = overlay.deleted_set();
        record.tombstones = EntitySet::of(deleted);
        record.owed = match deleted.is_empty() {
            true => Vec::new(),
            false => owed(record.space, deleted),
        };
        record.owed.retain(|owed| shards.contains(&owed.shard));
    }
}

/// Carry the live vocabulary bindings into a manifest's `vocabulary_extensions`. Unioned, never
/// restated: a binding must never shrink. The fold does not call this; it folds every served
/// extension into `MANIFEST.vocabularies` directly.
pub(super) fn write_vocabulary_extensions(
    manifest: &mut SegmentsManifest,
    vocabularies: &Vocabularies,
    bundle_vocabularies: &[mosaica_store::manifest::ManifestVocabulary],
) {
    for extension in vocabularies.extensions_beyond(bundle_vocabularies) {
        match manifest
            .partition
            .vocabulary_extensions
            .iter_mut()
            .find(|held| held.name == extension.name)
        {
            Some(held) => {
                for value in extension.values {
                    if !held.values.iter().any(|v| v.key == value.key) {
                        held.values.push(value);
                    }
                }
            }
            None => manifest.partition.vocabulary_extensions.push(extension),
        }
    }
}

/// What this executor has published into side-manifests, and whether live state is ahead of the
/// newest one. The extent lists are complete lists, not diffs: a publication clones a manifest
/// that may be stale, and extending that clone would drop entries.
pub(in crate::write) struct SideManifests {
    /// The next `SEGMENTS-<n>.json` number, raised over disc before every allocation.
    next_n: u64,
    /// Whether live state holds deny state, declarations, or operator-applied artifact records no
    /// side-manifest carries yet. Cleared by the publication that writes them out.
    pub(super) behind_live: bool,
    /// Whether a data door's batch grew memberships or content no side-manifest carries yet.
    /// Published at the tick, not behind the batch, since those are already WAL-durable.
    pub(super) growth_unpublished: bool,
    /// Deny windows applied since the last publication, the counter
    /// [`OVERLAY_PUBLICATION_MAX_WINDOWS`] floors.
    pub(super) windows_since_publication: u64,
    /// Every member pack this node has published, by shard.
    pub(super) membership_extents: super::publications::PackedMembers,
    /// Every derived file the current prefix holds, by shard, the layer spaces' held shapes under
    /// the first, filtered to what a manifest reaches ([`artifact_coordinates`]); a fold replaces
    /// it wholesale.
    pub(super) derived_extents: DerivedByShard,
    /// Each layer space's record packs and content extents, held and written with
    /// `membership_extents`, which they travel with: a membership without its record or its
    /// content withholds the artifact.
    pub(super) spaces: std::collections::BTreeMap<LayerSpace, HeldSpace>,
    /// The digest of every member pack, record pack and content extent file a publication into
    /// the current prefix wrote, restated into every side-manifest's `files` as the extent lists
    /// are; a fold replaces it wholesale.
    pub(super) artifact_files:
        std::collections::BTreeMap<String, mosaica_store::manifest::FileDigest>,
    /// The shard and space files each partition's newest side-manifest names, which the next
    /// publication keeps where its own would be the same bytes.
    pub(super) refs: std::collections::BTreeMap<String, mosaica_store::manifest::PublishedRefs>,
}

/// One layer space's files as [`SideManifests`] holds them.
#[derive(Clone, Debug, Default)]
pub(in crate::write) struct HeldSpace {
    pub(in crate::write) record_packs: Vec<mosaica_store::manifest::RecordPackExtent>,
    pub(in crate::write) content_extents: Vec<mosaica_store::manifest::RecordExtent>,
}

impl HeldSpace {
    /// What a side-manifest's record of the space lists.
    pub(in crate::write) fn of(record: &mosaica_store::manifest::LayerSpaceRecord) -> HeldSpace {
        HeldSpace {
            record_packs: record.record_packs.clone(),
            content_extents: record.content_extents.clone(),
        }
    }

    /// The files the space's record packs and content extents name.
    pub(in crate::write) fn files(&self) -> impl Iterator<Item = &str> {
        let records = self.record_packs.iter().map(|extent| extent.path.as_str());
        records.chain(
            self.content_extents
                .iter()
                .flat_map(|extent| extent.files()),
        )
    }
}

impl SideManifests {
    /// Open over what the bundle's manifests already carry.
    pub(in crate::write) fn seeded(
        next_n: u64,
        membership_extents: super::publications::PackedMembers,
        derived_extents: DerivedByShard,
        spaces: std::collections::BTreeMap<LayerSpace, HeldSpace>,
        artifact_files: std::collections::BTreeMap<String, mosaica_store::manifest::FileDigest>,
        refs: std::collections::BTreeMap<String, mosaica_store::manifest::PublishedRefs>,
        growth_unpublished: bool,
    ) -> Self {
        SideManifests {
            next_n,
            behind_live: false,
            growth_unpublished,
            windows_since_publication: 0,
            membership_extents,
            derived_extents,
            spaces,
            artifact_files,
            refs,
        }
    }

    /// Keep in [`Self::artifact_files`] only what the held lists still name.
    pub(super) fn retain_named_artifact_files(&mut self) {
        let named: std::collections::BTreeSet<&str> = self
            .membership_extents
            .values()
            .flatten()
            .map(|extent| extent.path.as_str())
            .chain(self.spaces.values().flat_map(HeldSpace::files))
            .collect();
        self.artifact_files
            .retain(|rel, _| named.contains(rel.as_str()));
    }

    /// Digest each of `rels` under `prefix_dir` onto [`Self::artifact_files`].
    pub(super) fn digest_artifact_files<'a>(
        &mut self,
        prefix_dir: &std::path::Path,
        rels: impl IntoIterator<Item = &'a str>,
    ) -> mosaica_store::Result<()> {
        for rel in rels {
            let digest = mosaica_store::digest_of(&prefix_dir.join(rel))?;
            self.artifact_files.insert(rel.to_string(), digest);
        }
        Ok(())
    }

    /// Take the next side-manifest number, having raised the floor over disc.
    pub(super) fn allocate_manifest_n(
        &mut self,
        bundle_root: &std::path::Path,
        health: &ExecutorHealth,
    ) -> mosaica_store::Result<u64> {
        self.raise_manifest_floor(bundle_root, health)?;
        Ok(self.take_manifest_n())
    }

    /// Raise the counter over every `SEGMENTS-<n>.json` on disc, a partition's, a shard's or a
    /// space's, and alarm if it moved: a floor above the counter is evidence of a second writer,
    /// since in single-writer operation the two are equal at every allocation.
    pub(super) fn raise_manifest_floor(
        &mut self,
        bundle_root: &std::path::Path,
        health: &ExecutorHealth,
    ) -> mosaica_store::Result<()> {
        let on_disk = mosaica_store::highest_published_n(bundle_root)?;
        let floor = on_disk.map_or(0, |highest| highest + 1);
        if floor > self.next_n {
            health.foreign_side_manifests.fetch_add(1, Ordering::Relaxed);
            tracing::error!(
                floor,
                counter = self.next_n,
                root = %bundle_root.display(),
                "ALARM: a side-manifest this executor did not write is on disc. One executor owns \
                 a bundle root; publications continue above it, and what the other writer has \
                 published is not reconciled with what this node holds"
            );
            self.next_n = floor;
        }
        Ok(())
    }

    /// The counter alone, for a caller that has just raised the floor.
    pub(super) fn take_manifest_n(&mut self) -> u64 {
        let n = self.next_n;
        self.next_n += 1;
        n
    }
}

impl Executor {
    /// Restates the live registry, roster, shard size and declarations, checks the manifest does
    /// not regress durable state, stamps its level versions, restates the extent lists unless a
    /// fold brings its own, and writes it: the shard and space files that changed, then the
    /// partition file that commits them. A refusal writes nothing. Also prunes the partition's
    /// superseded side-manifests, unless it is stepped down. Returns the files the partition file
    /// names.
    pub(super) fn commit_side_manifest(
        &mut self,
        partition_data: &mosaica_store::read::PartitionData,
        prefix_dir: &std::path::Path,
        partition: &str,
        n: u64,
        next: &mut mosaica_store::manifest::SegmentsManifest,
        fold: Option<FoldDerived<'_>>,
    ) -> Result<mosaica_store::manifest::PublishedRefs, ManifestCommitRefused> {
        self.write_live_state(next, fold.is_some());
        if let Some(fold) = &fold {
            next.shard_mut(fold.shard)
                .expect("the fold's shard is in the manifest it publishes")
                .held_entities
                .push(fold.freed.clone());
        }
        let fold_written: DerivedByShard;
        let (derived, pending) = match &fold {
            Some(fold) => {
                // A held shape is its layer space's, filed beside the first shard as
                // `derived_by_shard` files it, and checked against that shard's slice version.
                let first = next.shards.records.first().map_or(fold.shard, |record| record.id);
                let mut held = DerivedByShard::new();
                for extent in fold.written {
                    let shard = match extent.form {
                        mosaica_store::manifest::DerivedForm::ShapeHeld => first,
                        _ => fold.shard,
                    };
                    held.entry(shard).or_default().push(extent.clone());
                }
                for (shard, extent) in &fold.kept {
                    held.entry(*shard).or_default().push(extent.clone());
                }
                fold_written = held;
                (&fold_written, Some(fold.pending_retirement))
            }
            None => (&self.side_manifests.derived_extents, None),
        };
        let registered = self.live.registered_layers();
        let spaces = mosaica_types::layer::spaces_by_name(&registered);
        let enumerated = registered
            .iter()
            .filter(|layer| {
                layer.declaration.membership == mosaica_types::layer::MembershipSource::Enumerated
            })
            .map(|layer| layer.declaration.name.clone())
            .collect();
        let ArtifactCoordinates {
            records: mut level_versions,
            slices: mut slice_versions,
            derived: derived_extents,
        } = self.live.with_artifacts(|store| {
            artifact_coordinates(store, derived, pending, &spaces, &enumerated)
        });
        // The clone may predate these extents, so they are restated from the held lists, and
        // their files' digests with them.
        if fold.is_none() {
            for record in &mut next.shards.records {
                record.membership_extents = self
                    .side_manifests
                    .membership_extents
                    .get(&record.id)
                    .cloned()
                    .unwrap_or_default();
            }
            next.partition
                .files
                .retain(|rel, _| !mosaica_store::layout::is_artifact_file(partition, rel));
            next.partition
                .files
                .extend(self.side_manifests.artifact_files.clone());
        }
        // One record per registered layer, at its space; a dropped layer's leaves the manifest.
        next.layer_spaces
            .retain(|record| registered.iter().any(|layer| layer.space == record.space));
        for layer in &registered {
            mosaica_store::manifest::layer_space_mut(&mut next.layer_spaces, layer.space);
        }
        for record in &mut next.layer_spaces {
            let (records, contents) = match &fold {
                Some(fold) => (fold.records.get(&record.space), fold.contents.get(&record.space)),
                None => self
                    .side_manifests
                    .spaces
                    .get(&record.space)
                    .map(|held| (&held.record_packs, &held.content_extents))
                    .unzip(),
            };
            record.record_packs = records.cloned().unwrap_or_default();
            record.content_extents = contents.cloned().unwrap_or_default();
            record.level_versions = level_versions.remove(&record.space).unwrap_or_default();
        }
        for record in &mut next.shards.records {
            if let Some(versions) = slice_versions.remove(&record.id) {
                record.slice_versions = versions;
            }
        }
        next.file_derived_by_shard(derived_extents);
        crate::geometry::check_manifest_publishable(partition_data, next)
            .map_err(ManifestCommitRefused::Regresses)?;
        // A fold's new prefix holds none of the live prefix's files.
        let previous = match fold {
            Some(_) => None,
            None => self.side_manifests.refs.get(partition),
        };
        let staged = mosaica_store::stage_publication(prefix_dir, partition, n, next, previous)
            .map_err(ManifestCommitRefused::Store)?;
        // The publication seam: the publication's files, its shard and space files among them,
        // are on disc, and nothing durable names them until the partition file is written. A
        // fold's seam is the flip of `CURRENT`.
        if fold.is_none() {
            self.pause_point(PauseSiteArg::BeforeManifestPublish);
        }
        let refs = staged.commit().map_err(ManifestCommitRefused::Store)?;
        // A fold's are the live prefix's only once `CURRENT` names its prefix.
        if fold.is_none() {
            self.side_manifests
                .refs
                .insert(partition.to_string(), refs.clone());
        }
        // Nothing is pruned on a stepped-down partition: the manifests between its older served
        // `n` and the newest are what a reopen walks back through.
        if !partition_data.stepped_down() {
            match mosaica_store::prune_superseded_segments_manifests(prefix_dir, partition) {
                Ok(0) => {}
                Ok(removed) => tracing::debug!(
                    removed,
                    partition = %partition,
                    "superseded side-manifests were pruned"
                ),
                Err(e) => tracing::warn!(
                    error = %e,
                    partition = %partition,
                    "the partition's superseded side-manifests could not be pruned; they stay on \
                     disc and the next publication prunes again"
                ),
            }
        }
        Ok(refs)
    }

    /// [`write_deny_state`] over `generation`'s overlays and spaces, with each space's deletions
    /// listed against the shards whose compaction has to remove them.
    pub(super) fn write_deny_state_of(
        &self,
        manifest: &mut SegmentsManifest,
        generation: &Generation,
    ) {
        write_deny_state(
            manifest,
            shard_overlays(generation),
            &generation.layer_spaces,
            |space, deleted| self.live.owed_deletions(space, deleted),
        );
    }

    /// Restate the live registry, roster, shard size and runtime declarations into a side-manifest
    /// about to be committed, never carried forward from the partition's manifest, which is as of
    /// the last geometry swap. A fold writes the declarations into its `MANIFEST.json` and sets its
    /// own list of those made since it planned, so `fold` leaves them alone.
    fn write_live_state(&self, manifest: &mut SegmentsManifest, fold: bool) {
        let registry = self.live.registry_for_publication();
        for shard in self.live.shards() {
            let Some(record) = manifest
                .shards
                .records
                .iter_mut()
                .find(|record| record.id == shard.id)
            else {
                continue;
            };
            record.high_water = record.high_water.max(shard.high_water());
            (record.free_entities, record.held_entities) = shard.freed();
            record.state = record.state.max(shard.state());
        }
        manifest.shards.next_id = manifest.shards.next_id.max(self.live.next_shard_id());
        manifest.shards.size = self.live.shard_size();
        let partition = &mut manifest.partition;
        partition.layers = registry.layers;
        partition.next_layer_space = partition.next_layer_space.max(registry.next_layer_space);
        partition.layer_registry_version = registry.version;
        let (created_views, dead_view_incarnations) = self.live.roster_for_publication();
        partition.views = created_views;
        partition.dead_view_incarnations = dead_view_incarnations;
        if fold {
            return;
        }
        let vocabularies = &self.generation.load().vocabularies;
        let (attributes, scoped_attributes) = self.live.attributes_for_publication();
        partition.attributes = attributes;
        partition.scoped_attributes = scoped_attributes;
        partition.vocabularies = self.live.vocabularies_for_publication(vocabularies);
        let (groups, plain_views) = self.live.view_declarations_for_publication();
        partition.groups = groups;
        partition.plain_views = plain_views;
    }
}

#[cfg(test)]
mod vocabulary_extensions_tests {
    use super::*;
    use mosaica_store::manifest::{
        ManifestVocabulary, ManifestVocabularyValue, VocabularyExtension, VocabularyKind,
    };

    fn empty_vocabulary(name: &str) -> ManifestVocabulary {
        ManifestVocabulary {
            name: name.to_string(),
            kind: VocabularyKind::Discovered,
            visibility: crate::Visibility::Derived,
            width: mosaica_spatial::tiler::ScalarType::U32,
            values: Vec::new(),
            reserved: Vec::new(),
        }
    }

    /// A write touching an unrelated vocabulary must not erase an extension already held.
    #[test]
    fn a_carried_extension_survives_a_write_the_live_view_recomputes_nothing_for() {
        let mut manifest = SegmentsManifest::empty();
        manifest.partition.vocabulary_extensions.push(VocabularyExtension {
            name: "legacy".to_string(),
            values: vec![ManifestVocabularyValue {
                key: "held".to_string(),
                code: 7,
                title: None,
            }],
        });

        let vocabularies = Vocabularies::seed(&[empty_vocabulary("department")], &[], &[]).unwrap();

        write_vocabulary_extensions(&mut manifest, &vocabularies, &[]);

        let legacy = manifest
            .partition
            .vocabulary_extensions
            .iter()
            .find(|e| e.name == "legacy")
            .expect("a binding this manifest already carried must not be dropped");
        assert_eq!(legacy.values.len(), 1);
        assert_eq!(legacy.values[0].key, "held");
        assert_eq!(legacy.values[0].code, 7);
    }

    /// A fresh mint is appended beside what is already carried, and a restated binding is not duplicated.
    #[test]
    fn a_fresh_binding_is_appended_beside_what_is_already_carried_and_not_duplicated() {
        let mut manifest = SegmentsManifest::empty();
        manifest.partition.vocabulary_extensions.push(VocabularyExtension {
            name: "department".to_string(),
            values: vec![ManifestVocabularyValue {
                key: "eng".to_string(),
                code: 4,
                title: None,
            }],
        });

        let mut vocabularies =
            Vocabularies::seed(&[empty_vocabulary("department")], &[], &[]).unwrap();
        vocabularies
            .get_mut("department")
            .unwrap()
            .seed_value("eng", 4)
            .unwrap();
        vocabularies
            .get_mut("department")
            .unwrap()
            .mint("finance")
            .unwrap();

        write_vocabulary_extensions(&mut manifest, &vocabularies, &[]);

        let department = manifest
            .partition
            .vocabulary_extensions
            .iter()
            .find(|e| e.name == "department")
            .unwrap();
        let mut keys: Vec<&str> = department.values.iter().map(|v| v.key.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["eng", "finance"],
            "the carried key and the fresh one both survive, each exactly once"
        );
    }
}
