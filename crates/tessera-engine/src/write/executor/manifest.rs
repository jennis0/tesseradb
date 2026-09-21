use super::*;

/// Every level's version, and the derived files of `held` stamped with their level's version.
///
/// A file whose level has moved is dropped, so a manifest never names one nothing could adopt. A
/// `pending` retirement states the version each level will carry once it has run
/// ([`PendingRetirement::version_after`]); without one the store's version is the answer.
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
/// state, or the store could not write it. One type so every publication site's failure arm
/// reports whichever it was through the `error = %e` it already has.
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

/// Replace a manifest's deny fields with the overlay's live state. Serialised fresh at every
/// write, never carried forward: copying an earlier manifest's fields forward would leave an
/// unsuppress never reaching disc. The two fields are taken from the two bitmaps separately, never
/// from `Overlay::denied`'s union, since publishing the union would make every deletion look
/// retirable by an unsuppress.
pub(super) fn write_deny_state(manifest: &mut SegmentsManifest, overlay: &Overlay) {
    manifest.deny = overlay
        .suppressed_entities()
        .into_iter()
        .map(|entity_id| ManifestDenyEntry {
            entity_id,
            cause: "suppress".to_string(),
        })
        .collect();
    manifest.tombstones = overlay.deleted_entities();
}

/// Carry the live vocabulary bindings into a manifest's `vocabulary_extensions`,
/// `write_deny_state`'s sibling, called beside it at every publication site except the fold's.
/// Union, never restate: a binding must never shrink, so this appends only what
/// `extensions_beyond` gives beyond what the manifest already carries. The fold does not call
/// this: it folds every served extension into `MANIFEST.vocabularies` directly.
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

impl Executor {
    /// Take the next side-manifest number: this executor's counter, raised over every
    /// `SEGMENTS-<n>.json` present under the bundle root.
    ///
    /// One publication, one scan: a caller allocating several numbers at once raises the floor
    /// itself and then takes each number from [`Executor::take_manifest_n`].
    pub(super) fn allocate_manifest_n(&mut self) -> tessera_store::Result<u64> {
        self.raise_manifest_floor()?;
        Ok(self.take_manifest_n())
    }

    /// Raise the counter over every `SEGMENTS-<n>.json` on disc, and alarm if it moved.
    ///
    /// A floor above the counter is positive evidence of a second writer: in single-writer
    /// operation the two are equal at every allocation. Raising the floor keeps this node
    /// publishing rather than colliding at every number it re-plans at.
    ///
    /// A bundle root that cannot be listed fails the allocation and so the publication: the caller
    /// discards, its files are orphans, and the next tick re-plans.
    pub(super) fn raise_manifest_floor(&mut self) -> tessera_store::Result<()> {
        let on_disk = tessera_store::highest_side_manifest_n(&self.deps.bundle_root)?;
        let floor = on_disk.map_or(0, |highest| highest + 1);
        if floor > self.next_manifest_n {
            self.health
                .foreign_side_manifests
                .fetch_add(1, Ordering::Relaxed);
            tracing::error!(
                floor,
                counter = self.next_manifest_n,
                root = %self.deps.bundle_root.display(),
                "ALARM: a side-manifest this executor did not write is on disc. One executor owns \
                 a bundle root; publications continue above it, and what the other writer has \
                 published is not reconciled with what this node holds"
            );
            self.next_manifest_n = floor;
        }
        Ok(())
    }

    /// The counter alone, for a caller that has just raised the floor.
    pub(super) fn take_manifest_n(&mut self) -> u64 {
        let n = self.next_manifest_n;
        self.next_manifest_n += 1;
        n
    }

    /// Commits one partition's side-manifest. Every publication writes its manifest through here,
    /// so two things are done once: the manifest's ordered scalars are checked against
    /// `live_manifest` ([`crate::geometry::check_manifest_publishable`]), since a manifest is
    /// assembled by editing a clone that may be stale; and the level versions and derived files are
    /// stamped ([`artifact_coordinates`]). A refusal writes nothing.
    pub(super) fn commit_side_manifest(
        &self,
        live_manifest: &tessera_store::manifest::SegmentsManifest,
        prefix_dir: &std::path::Path,
        partition: &str,
        n: u64,
        next: &mut tessera_store::manifest::SegmentsManifest,
        fold: Option<FoldDerived<'_>>,
    ) -> Result<(), ManifestCommitRefused> {
        // Only the fold brings its own derived files and levels pending retirement; every other
        // publication carries the held list forward.
        let (derived, pending) = match &fold {
            Some(fold) => (fold.written, Some(fold.pending_retirement)),
            None => (self.derived_extents.as_slice(), None),
        };
        let (level_versions, derived_extents) = self
            .live
            .with_artifacts(|store| artifact_coordinates(store, derived, pending));
        next.level_versions = level_versions;
        next.derived_extents = derived_extents;
        crate::geometry::check_manifest_publishable(live_manifest, next)
            .map_err(ManifestCommitRefused::Regresses)?;
        tessera_store::write_segments_manifest(prefix_dir, partition, n, next)
            .map_err(ManifestCommitRefused::Store)
    }

    /// Restate the live row-less state into a side-manifest about to be committed: the registry
    /// and its low-water mark, the roster, the attribute columns, the vocabularies, and the view
    /// groups and plain views.
    ///
    /// Restated from live state, never carried forward from the clone: a manifest a publication
    /// starts from may be several behind, so carrying a stale value forward would drop a
    /// registration. `min`, not `max`, for the mark: the row-less region grows downward.
    pub(super) fn write_live_state(&self, manifest: &mut SegmentsManifest, vocabularies: &Vocabularies) {
        let (layers, layer_tombstones, low_water) = self.live.registry_for_publication();
        manifest.entity_id_low_water = manifest.entity_id_low_water.min(low_water);
        manifest.layers = layers;
        manifest.layer_tombstones = layer_tombstones;
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

    /// A carried binding must survive even when the live view has nothing to say about it: a write
    /// touching an unrelated vocabulary must not erase an extension already held.
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
