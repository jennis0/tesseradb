use super::*;

/// Every level's version, and the derived files of `held` still adoptable at it. A file whose
/// level has moved is dropped, so a manifest never names one nothing could adopt.
pub(super) fn artifact_coordinates(
    store: &ArtifactStore,
    held: &[tessera_store::manifest::DerivedExtent],
    pending: Option<&PendingRetirement>,
) -> (
    Vec<tessera_store::manifest::LevelVersion>,
    Vec<tessera_store::manifest::DerivedExtent>,
) {
    let expected = |layer: &str, level: u32| match pending {
        Some(pending) => pending.version_after(store, layer, level),
        None => store.level_version(layer, level),
    };
    let versions = store
        .level_versions()
        .map(|(layer, level, _)| tessera_store::manifest::LevelVersion {
            layer: layer.to_string(),
            level,
            version: expected(layer, level),
        })
        .collect();
    let still_true = held
        .iter()
        .filter(|entry| expected(&entry.layer, entry.level) == entry.level_version)
        .cloned()
        .collect();
    (versions, still_true)
}

/// Why [`Executor::commit_side_manifest`] did not commit: the manifest would regress durable
/// state, or the store could not write it.
pub(super) enum ManifestCommitRefused {
    Regresses(crate::geometry::ManifestRegression),
    Store(tessera_store::StoreError),
}

impl std::fmt::Display for ManifestCommitRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ManifestCommitRefused::Regresses(r) => r.fmt(f),
            ManifestCommitRefused::Store(e) => e.fmt(f),
        }
    }
}

/// Replace a manifest's deny fields with the overlay's live state, restated fresh at every write
/// rather than carried forward, as two separate sets and never their union: publishing the union
/// would make every deletion look retirable by an unsuppress.
pub(super) fn write_deny_state(manifest: &mut SegmentsManifest, overlay: &Overlay) {
    manifest.deny = DenySet::of(overlay.suppressed_set());
    manifest.tombstones = DenySet::of(overlay.deleted_set());
}

/// Carry the live vocabulary bindings into a manifest's `vocabulary_extensions`. Unioned, never
/// restated: a binding must never shrink. The fold does not call this; it folds every served
/// extension into `MANIFEST.vocabularies` directly.
pub(super) fn write_vocabulary_extensions(
    manifest: &mut SegmentsManifest,
    vocabularies: &Vocabularies,
    bundle_vocabularies: &[tessera_store::manifest::ManifestVocabulary],
) {
    for extension in vocabularies.extensions_beyond(bundle_vocabularies) {
        match manifest
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
            None => manifest.vocabulary_extensions.push(extension),
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
    /// Every membership extent this node has published.
    pub(super) membership_extents: Vec<tessera_store::manifest::MembershipExtent>,
    /// Every derived file the current prefix holds, filtered to what a manifest reaches
    /// ([`artifact_coordinates`]); a fold replaces it wholesale.
    pub(super) derived_extents: Vec<tessera_store::manifest::DerivedExtent>,
    /// Every artifact content extent, held and written like `membership_extents`, which it
    /// travels with: a membership without its content withholds the artifact.
    pub(super) artifact_record_extents: Vec<tessera_store::manifest::RecordExtent>,
}

impl SideManifests {
    /// Open over what the bundle's manifests already carry.
    pub(in crate::write) fn seeded(
        next_n: u64,
        membership_extents: Vec<tessera_store::manifest::MembershipExtent>,
        derived_extents: Vec<tessera_store::manifest::DerivedExtent>,
        artifact_record_extents: Vec<tessera_store::manifest::RecordExtent>,
        growth_unpublished: bool,
    ) -> Self {
        SideManifests {
            next_n,
            behind_live: false,
            growth_unpublished,
            windows_since_publication: 0,
            membership_extents,
            derived_extents,
            artifact_record_extents,
        }
    }

    /// Take the next side-manifest number, having raised the floor over disc.
    pub(super) fn allocate_manifest_n(
        &mut self,
        bundle_root: &std::path::Path,
        health: &ExecutorHealth,
    ) -> tessera_store::Result<u64> {
        self.raise_manifest_floor(bundle_root, health)?;
        Ok(self.take_manifest_n())
    }

    /// Raise the counter over every `SEGMENTS-<n>.json` on disc, and alarm if it moved: a floor
    /// above the counter is evidence of a second writer, since in single-writer operation the two
    /// are equal at every allocation.
    pub(super) fn raise_manifest_floor(
        &mut self,
        bundle_root: &std::path::Path,
        health: &ExecutorHealth,
    ) -> tessera_store::Result<()> {
        let on_disk = tessera_store::highest_side_manifest_n(bundle_root)?;
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
    /// Checks the manifest does not regress durable state, stamps its level versions, restates the
    /// extent lists unless a fold brings its own, and writes it; a refusal writes nothing. Also
    /// prunes the partition's superseded side-manifests, unless it is stepped down.
    pub(super) fn commit_side_manifest(
        &self,
        partition_data: &tessera_store::read::PartitionData,
        prefix_dir: &std::path::Path,
        partition: &str,
        n: u64,
        next: &mut tessera_store::manifest::SegmentsManifest,
        fold: Option<FoldDerived<'_>>,
    ) -> Result<(), ManifestCommitRefused> {
        let live_manifest = &partition_data.manifest;
        let (derived, pending) = match &fold {
            Some(fold) => (fold.written, Some(fold.pending_retirement)),
            None => (self.side_manifests.derived_extents.as_slice(), None),
        };
        let (level_versions, derived_extents) = self
            .live
            .with_artifacts(|store| artifact_coordinates(store, derived, pending));
        next.level_versions = level_versions;
        next.derived_extents = derived_extents;
        // The clone may predate these extents, so they are restated from the held lists.
        if fold.is_none() {
            next.membership_extents = self.side_manifests.membership_extents.clone();
            next.artifact_record_extents = self.side_manifests.artifact_record_extents.clone();
        }
        crate::geometry::check_manifest_publishable(live_manifest, next)
            .map_err(ManifestCommitRefused::Regresses)?;
        tessera_store::write_segments_manifest(prefix_dir, partition, n, next)
            .map_err(ManifestCommitRefused::Store)?;
        // Nothing is pruned on a stepped-down partition: the manifests between its older served
        // `n` and the newest are what a reopen walks back through.
        if !partition_data.stepped_down() {
            match tessera_store::prune_superseded_segments_manifests(prefix_dir, partition) {
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
        Ok(())
    }

    /// Restate the live row-less state into a side-manifest about to be committed, never carried
    /// forward from the clone, which may be several behind. `min`, not `max`, for the low-water
    /// mark: the row-less region grows downward.
    pub(super) fn write_live_state(&self, manifest: &mut SegmentsManifest, vocabularies: &Vocabularies) {
        let (layers, layer_tombstones, registry_version, low_water) =
            self.live.registry_for_publication();
        manifest.entity_id_low_water = manifest.entity_id_low_water.min(low_water);
        manifest.layers = layers;
        manifest.layer_tombstones = layer_tombstones;
        manifest.layer_registry_version = registry_version;
        let (created_views, dead_view_incarnations) = self.live.roster_for_publication();
        manifest.views = created_views;
        manifest.dead_view_incarnations = dead_view_incarnations;
        let (attributes, scoped_attributes) = self.live.attributes_for_publication();
        manifest.attributes = attributes;
        manifest.scoped_attributes = scoped_attributes;
        manifest.vocabularies = self.live.vocabularies_for_publication(vocabularies);
        let (groups, plain_views) = self.live.view_declarations_for_publication();
        manifest.groups = groups;
        manifest.plain_views = plain_views;
    }
}

#[cfg(test)]
mod vocabulary_extensions_tests {
    use super::*;
    use tessera_store::manifest::{
        ManifestVocabulary, ManifestVocabularyValue, VocabularyExtension, VocabularyKind,
    };

    fn empty_vocabulary(name: &str) -> ManifestVocabulary {
        ManifestVocabulary {
            name: name.to_string(),
            kind: VocabularyKind::Discovered,
            visibility: crate::Visibility::Derived,
            width: tessera_spatial::tiler::ScalarType::U32,
            values: Vec::new(),
            reserved: Vec::new(),
        }
    }

    /// A write touching an unrelated vocabulary must not erase an extension already held.
    #[test]
    fn a_carried_extension_survives_a_write_the_live_view_recomputes_nothing_for() {
        let mut manifest = SegmentsManifest::empty();
        manifest.vocabulary_extensions.push(VocabularyExtension {
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
        manifest.vocabulary_extensions.push(VocabularyExtension {
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
