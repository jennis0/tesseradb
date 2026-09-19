use super::*;

/// The manifest state a reconstruction starts from, before WAL replay unions what was written
/// since: both entity-space marks and the registry's complete current view.
///
/// A struct rather than four arguments: the fields travel together and are all read from the same
/// manifests, and a fifth stays a compile error here rather than a defaulted argument.
pub(crate) struct ManifestSeed<'a> {
    /// `max(build MANIFEST, side manifests)`: the point region's floor.
    pub high_water: u64,
    /// `min(ceiling, side manifests)`: the row-less region's ceiling. A build sets this whenever
    /// its declaration carried layers, or the first online registration reissues their ids.
    pub low_water: u64,
    pub layers: &'a [tessera_types::layer::RegisteredLayer],
    pub tombstones: &'a [String],
    /// Every view created since the build, across every partition's manifest, and every
    /// incarnation that has died. The build's own roster is not here: it is in `MANIFEST.json`
    /// and is seeded separately.
    pub created_views: &'a [tessera_types::view::CreatedView],
    pub dead_view_incarnations: &'a [tessera_types::view::DeadIncarnation],
    /// The views a build declared, as `(group, key)`: the keys a create must not reissue.
    pub declared_views: Vec<(String, String)>,
    /// `Manifest::view_ids_for_key`: every view id a dropped key resolves to, the owner's and
    /// every sharing group's. Replay's `ViewDrop` arm prunes the buffer with it, and it is passed
    /// rather than derived because `tessera-lifecycle` holds no manifest.
    pub view_ids_of_key: &'a dyn Fn(&str, &str) -> Vec<String>,
    /// Every published membership extent, across every partition's manifest, with the prefix
    /// directory their paths are relative to.
    pub membership_extents: &'a [tessera_store::manifest::MembershipExtent],
    /// Every `(layer, level)`'s artifact-write counter as of the publication, across every
    /// partition's manifest.
    ///
    /// Seeded after the records and before the replay, or a level comes back at its record count
    /// rather than the number the publication recorded. See `ArtifactStore::seed_level_version`.
    pub level_versions: &'a [tessera_store::manifest::LevelVersion],
    pub prefix_dir: std::path::PathBuf,
    /// The served schema as the manifests make it: the build's columns with every side manifest's
    /// runtime declarations appended (`Manifest::with_attributes`). Replay compares each
    /// `AttributeDeclare` record against it, so a record restating a folded column is applied as
    /// nothing and one contradicting the manifests refuses the open.
    pub manifest: &'a tessera_store::manifest::Manifest,
    /// The side manifests' `attributes` and `scoped_attributes`, the runtime declarations no fold
    /// has written into a `MANIFEST.json`; replay appends to these.
    pub attributes: crate::attributes::RuntimeAttributes,
    /// The side manifests' `vocabularies`, on [`Self::attributes`]' rule; replay appends to it.
    pub vocabularies: crate::vocabularies::RuntimeVocabularies,
    /// The side manifests' `groups` and `plain_views`, on [`Self::attributes`]' rule; replay
    /// appends to them.
    pub view_declarations: crate::view_declarations::RuntimeViewDeclarations,
}

impl WritePath {
    /// Rebuilds the write-side state from durable storage: opens and replays the WAL, seeds the
    /// allocator, and rebuilds the external-id maps, the descriptor resolver's extension, the
    /// registries and the idempotency index. Returns the first generation's overlay and buffer.
    ///
    /// `initial_deny` is the side-manifest's deny state; it seeds the overlay before replay, and
    /// the WAL is applied on top since dispositions are idempotent.
    pub(crate) fn reconstruct(
        wal_path: &Path,
        seed: ManifestSeed<'_>,
        dict: &Dict,
        initial_deny: &[(EntityId, ChangeOp)],
        vocabularies: &mut Vocabularies,
        has_row: impl Fn(EntityId, &str) -> bool,
    ) -> Result<(Overlay, IngestBuffer, WritePathState), EngineError> {
        let (wal, records) = Wal::open(wal_path).map_err(EngineError::Wal)?;

        // A record whose meaning is not built refuses the open, before anything is applied: a log
        // carrying one was written by a binary this one is not, and replaying past it would serve
        // state that omits what the record said. Naming the track tells the operator which binary.
        for record in &records {
            if let Some((kind, track)) = tessera_lifecycle::wal::unbuilt_track(record) {
                return Err(EngineError::Malformed(format!(
                    "the WAL carries a {kind} record, which this build cannot apply (track {track})"
                )));
            }
        }

        // Vocabularies declared while the service ran, then the mints that name them, over what the
        // caller seeded from the manifests. A record restating a vocabulary the manifests carry
        // applies only its values; one disagreeing about kind, visibility, width or binding refuses
        // the open, since either reading would recolour acked rows.
        let mut runtime_vocabularies = seed.vocabularies;
        for record in &records {
            let WalRecord::VocabularyDeclare { declaration } = record else {
                continue;
            };
            let compiled = crate::vocabularies::compile_record(declaration).ok_or_else(|| {
                EngineError::Malformed(format!(
                    "the WAL declares vocabulary '{}' at width '{}', which is not a code width",
                    declaration.name, declaration.width
                ))
            })?;
            match vocabularies.get_mut(&compiled.name) {
                Some(minter) => {
                    // Kind, visibility and width: the same fields `crate::vocabularies::resolve`
                    // compares at the door.
                    if minter.kind() != compiled.kind
                        || minter.visibility() != compiled.visibility
                        || minter.width() != compiled.width
                    {
                        return Err(EngineError::Malformed(format!(
                            "the WAL declares vocabulary '{}' with a kind, visibility or width the \
                             manifests do not carry for that name",
                            compiled.name
                        )));
                    }
                    for value in &compiled.values {
                        minter
                            .seed_value(&value.key, value.code)
                            .map_err(|e| EngineError::Malformed(e.to_string()))?;
                        // Replayed in log order, so the last title a page supplied is the one
                        // the minter ends holding.
                        if let Some(title) = &value.title {
                            minter.set_title(&value.key, title.clone());
                        }
                    }
                    for &code in &compiled.reserved {
                        minter.seed_reserved(code);
                    }
                }
                None => {
                    let mut minter = tessera_store::vocabulary::VocabularyMinter::new(
                        compiled.name.clone(),
                        compiled.kind,
                        compiled.visibility,
                        compiled.width,
                    );
                    minter
                        .seed_manifest(&compiled)
                        .map_err(|e| EngineError::Malformed(e.to_string()))?;
                    vocabularies.insert(minter);
                }
            }
            // The runtime list is what the next publication writes. A name `MANIFEST.json`
            // already carries is one a fold has folded in, and belongs to one list, not two.
            if !runtime_vocabularies.holds(&compiled.name)
                && !seed
                    .manifest
                    .vocabularies
                    .iter()
                    .any(|v| v.name == compiled.name)
            {
                runtime_vocabularies.push(ManifestVocabulary {
                    values: Vec::new(),
                    ..compiled
                });
            }
        }

        for record in &records {
            if let WalRecord::VocabularyMint {
                vocabulary,
                key,
                code,
            } = record
            {
                let minter = vocabularies.get_mut(vocabulary).ok_or_else(|| {
                    EngineError::Malformed(format!(
                        "the WAL mints into vocabulary '{vocabulary}', which this bundle does not \
                         declare"
                    ))
                })?;
                minter
                    .seed_value(key, *code)
                    .map_err(|e| EngineError::Malformed(e.to_string()))?;
            }
        }

        let high_water = seed.high_water.max(high_water_from(&records));
        // The row-less mark takes the minimum where the point mark takes the maximum: the two
        // regions grow towards each other, so "furthest along" is downward here.
        let low_water = seed.low_water.min(low_water_from(&records));
        // `try_with_marks`, not `with_marks`: the seeds come from durable state this run did not
        // write, so a corrupt or hand-edited pair must be refused here, not as a later opaque
        // exhaustion error.
        let allocator = Allocator::try_with_marks(high_water, low_water).map_err(|e| {
            EngineError::Malformed(format!(
                "entity-ID allocator seed from durable state (MANIFEST high-water {}, WAL \
                 high-water {}; MANIFEST low-water {}, WAL low-water {}): {e}",
                seed.high_water,
                high_water_from(&records),
                seed.low_water,
                low_water_from(&records),
            ))
        })?;

        // The manifest's registry is the starting point; replay runs over it. Every WAL record
        // postdates manifest state, so seeding afterwards would resurrect a layer dropped since the
        // last publication, gate and all.
        let mut registry = LayerRegistry::new();
        registry.seed(seed.layers, seed.tombstones);
        for record in &records {
            registry.apply(record);
        }
        // The layer-entity cursor cannot be inferred from the records: a `LayerCreate` says which
        // entity a layer took, not how much of its block was left, and resuming from
        // `max(entity) + 1` would be wrong once a drop retired the highest-numbered layer.
        registry.reseed_entity_cursor();

        // Same ordering rule as the registry above. The build's declared views are seeded first
        // because their keys are taken; forgetting them would let a create reissue one.
        let mut roster = tessera_lifecycle::ViewRoster::new();
        roster.seed_declared(seed.declared_views.iter().cloned());
        roster.seed(seed.created_views, seed.dead_view_incarnations);
        for record in &records {
            roster.apply(record);
        }

        // View groups and plain views declared while the service ran; one the served manifest
        // already carries applies as nothing. Groups must reach the manifest before the roster's
        // creates are merged, or `Manifest::with_roster` drops a create whose group is unknown.
        let mut view_declarations = seed.view_declarations;
        for record in &records {
            match record {
                WalRecord::ViewGroupCreate { declaration } => {
                    match crate::view_declarations::resolve_group(declaration, seed.manifest) {
                        Ok(crate::view_declarations::Resolution::New(group)) => {
                            if !view_declarations.holds_group(&group.name) {
                                view_declarations.push_group(*group);
                            }
                        }
                        Ok(crate::view_declarations::Resolution::Existing) => {}
                        Err(e) => {
                            return Err(EngineError::Malformed(format!(
                                "the WAL declares view group '{}', which this bundle refuses ({e})",
                                declaration.name
                            )));
                        }
                    }
                }
                WalRecord::PlainViewCreate { declaration } => {
                    match crate::view_declarations::resolve_plain(declaration, seed.manifest) {
                        Ok(crate::view_declarations::Resolution::New(view)) => {
                            if !view_declarations.holds_plain(&view.id) {
                                view_declarations.push_plain(*view);
                            }
                        }
                        Ok(crate::view_declarations::Resolution::Existing) => {}
                        Err(e) => {
                            return Err(EngineError::Malformed(format!(
                                "the WAL declares view '{}', which this bundle refuses ({e})",
                                declaration.name
                            )));
                        }
                    }
                }
                _ => {}
            }
        }

        // Same ordering rule again. A record naming a column the served schema already holds
        // identically applies as nothing; one holding a different identity under a held name
        // disagrees with the manifests about what every row stores, and refuses the open.
        let mut attributes = seed.attributes;
        let mut served = seed.manifest.clone();
        for record in &records {
            let WalRecord::AttributeDeclare { declaration } = record else {
                continue;
            };
            let Some(compiled) = crate::attributes::compile_record(declaration) else {
                return Err(EngineError::Malformed(format!(
                    "the WAL declares attribute '{}' with type '{}', which this build cannot \
                     store",
                    declaration.name, declaration.ty
                )));
            };
            match crate::attributes::held_by_name(&served, &declaration.name) {
                Some(held) if held == compiled => continue,
                Some(_) => {
                    return Err(EngineError::Malformed(format!(
                        "the WAL declares attribute '{}' with a type or scope the manifests do not \
                         carry for that name",
                        declaration.name
                    )));
                }
                None => {}
            }
            match &compiled {
                crate::attributes::CompiledAttribute::Entity(d) => {
                    served = served.with_attributes(std::slice::from_ref(d), &[]);
                }
                crate::attributes::CompiledAttribute::Scoped(f) => {
                    served = served.with_attributes(&[], std::slice::from_ref(f));
                }
            }
            attributes.push(compiled);
        }

        // Same ordering rule again: replay unions onto the manifests' membership extents. An
        // extent is addressed by absolute ordinal from the layer's reserved runs, so this must run
        // after the registry seeding above.
        let mut artifacts = ArtifactStore::new();
        let mut undecodable = 0usize;
        // How many memberships the seed holds on the heap because the mapping would not take.
        // Reached only on parser drift or damage; see the record arm below.
        let mut on_heap = 0usize;
        for extent in seed.membership_extents {
            let path = seed.prefix_dir.join(&extent.path);
            // The pack is held for as long as the memberships read through it: one `Arc` per
            // extent is cloned into each `Members`.
            let pack = Arc::new(
                tessera_store::membership::MembershipPack::open(&path)
                    .map_err(|e| EngineError::Malformed(e.to_string()))?,
            );
            let owner: Arc<dyn std::any::Any + Send + Sync> = pack.clone();
            // The manifest and the file must agree about which artifacts this range names, or a
            // disagreement would serve one cluster's members under another's identity.
            if pack.ordinal_lo() != extent.ordinal_lo || pack.count() != extent.count {
                return Err(EngineError::Malformed(format!(
                    "membership extent {} covers [{}, +{}) but the manifest names [{}, +{})",
                    extent.path,
                    pack.ordinal_lo(),
                    pack.count(),
                    extent.ordinal_lo,
                    extent.count
                )));
            }
            let Some(runs) = registry
                .get(&extent.layer)
                .and_then(|layer| layer.runs.get(extent.level as usize))
            else {
                // A dropped layer's extents outlive it until the next fold rewrites the prefix, so
                // skipping them is correct: a tombstoned name is not a fault.
                continue;
            };
            for (ordinal, blob) in pack.iter() {
                // An empty blob is a hole, not a fault: the ordinal exists with no artifact, which
                // is what a fold leaves behind where Rule F retired one. Treating it as undecodable
                // would alarm on every level a deletion has touched.
                if blob.is_empty() {
                    continue;
                }
                let Some(entity) = runs.entity_of(ordinal as u64).map(EntityId::new) else {
                    undecodable += 1;
                    continue;
                };
                match tessera_lifecycle::membership::decode_record(entity, blob) {
                    Some((mut record, shape)) => {
                        // The membership is read through the pack's mapping and the decoded
                        // bitmap is dropped, to avoid holding a heap bitmap per artifact.
                        //
                        // SAFETY: `blob` is a slice of `pack`'s read-only mapping, and `Members`
                        // holds `owner` for as long as it holds the view. No extent file is
                        // written twice (names come from a counter that only rises, one executor
                        // per bundle root), so a mapped file is never truncated under a reader.
                        let mapped = unsafe {
                            tessera_lifecycle::membership::mapped_members(blob, owner.clone())
                        };
                        // The same cardinality check `ArtifactStore::rehouse_members` makes: a
                        // disagreement means the two readers drifted apart or the bytes are
                        // damaged. Where it fails, the record keeps the bitmap it decoded.
                        match mapped {
                            Some(members)
                                if members.cardinality() == record.members.cardinality() =>
                            {
                                record.members = members;
                            }
                            _ => on_heap += 1,
                        }
                        artifacts.seed(&extent.layer, extent.level, ordinal, record, shape)
                    }
                    None => undecodable += 1,
                }
            }
            // How far the level reaches comes from the extent, not the records in it: a hole at
            // the top is implied by nothing, so a level seeded from records alone would come back
            // short.
            artifacts.seed_extent_bound(
                &extent.layer,
                extent.level,
                extent.ordinal_lo.saturating_add(extent.count),
            );
        }
        // The published version, before replay moves it. Every level the manifest names gets the
        // counter the publication recorded, which the replay below then advances for every record
        // past that publication. See `ArtifactStore::seed_level_version`.
        for version in seed.level_versions {
            artifacts.seed_level_version(&version.layer, version.level, version.version);
        }
        for (record, position) in records.iter().zip(wal.replayed_positions()) {
            undecodable += artifacts.apply(record, *position);
        }
        if on_heap > 0 {
            tracing::warn!(
                count = on_heap,
                "ALARM: the membership located inside the blob disagreed with the one decoded from \
                 it, so those artifacts are served from the decoded bitmap on the heap. This is \
                 parser drift or damage in the extent; check the extent files it came from"
            );
        }
        if undecodable > 0 {
            tracing::error!(
                count = undecodable,
                "ALARM: artifact memberships in the durable prefix did not decode, so those \
                 artifacts are served as absent. Check the extent files for damage"
            );
        }

        // Taken off the manifest seed before it is shadowed by the overlay seed below.
        let view_ids_of_key = seed.view_ids_of_key;
        // Same ordering rule again, with one exception: `Unsuppress` needs the later record to
        // win, so seeding first and replaying on top reverts an acked unsuppress otherwise.
        let mut seed = Overlay::new();
        for (entity, op) in initial_deny {
            seed.apply(*entity, *op);
        }

        let (overlay, mut buffer, established, resolver) =
            replay(&records, dict, seed, view_ids_of_key);
        // Re-hashed once at open: `replay` builds this with `FxHashMap`, the live index does not
        // (see `WritePath::established`'s doc).
        let established: std::collections::HashMap<Vec<u8>, EntityId> =
            established.into_iter().collect();

        // Replay walks every retained record, including batches whose rows a flush has already
        // given geometry; those rows are dropped here or the next flush would write them twice.
        // The test is per (entity, view), not a watermark, since an entity may hold rows in
        // several views.
        let already_flushed: Vec<(EntityId, String)> = buffer
            .rows()
            .map(|(entity, item)| (*entity, item.view.clone()))
            .filter(|(entity, view)| has_row(*entity, view))
            .collect();
        if !already_flushed.is_empty() {
            tracing::debug!(
                count = already_flushed.len(),
                "WAL rows that already have geometry were not re-buffered"
            );
        }
        for (entity, view) in already_flushed {
            buffer.remove_in_view(entity, &view);
        }

        // Where each surviving row sits in the log, so a rotation knows what it may reclaim below.
        for (record, position) in records.iter().zip(wal.replayed_positions()) {
            if let WalRecord::IngestBatch { rows, .. } = record {
                for row in rows {
                    buffer.set_wal_pos(row.entity_id, &row.view, *position);
                }
            }
        }

        // The values batches, into the buffer's fill map: resolving a column name needs the served
        // schema, so this runs here. A batch whose cells a flush already wrote is re-buffered;
        // `plan_flush` consumes the fill and stops pinning the log.
        for (record, position) in records.iter().zip(wal.replayed_positions()) {
            // In log order, as a drop treats buffered rows: the group-scoped fills addressed to
            // the dropped key go with it, and one accepted under a later view of that key stays.
            if let WalRecord::ViewDrop { view } = record {
                let owner_view =
                    format!("{}{}{}", view.group, tessera_store::GROUP_SEPARATOR, view.key);
                let orphaned: Vec<EntityId> = buffer
                    .scoped_fills()
                    .filter(|((_, held), _)| *held == owner_view)
                    .map(|((entity, _), _)| *entity)
                    .collect();
                for entity in orphaned {
                    buffer.remove_scoped_fill(entity, &owner_view);
                }
                continue;
            }
            let WalRecord::ValuesBatch {
                view,
                columns,
                rows,
                ..
            } = record
            else {
                continue;
            };
            // A record with no view names no flush pass to write its cells; reading it as some
            // view's would put a group-scoped cell in the wrong column.
            let Some(view) = view else {
                return Err(EngineError::Malformed(
                    "the WAL carries a values batch naming no view, which no writer produces"
                        .to_string(),
                ));
            };
            let families = scoped_families_of_view(&served, view);
            let owner_view = scoped_owner_view_of(&served, view);
            // Empty unless the served manifest no longer carries the view's key: the group's
            // families, whose cells went with the view.
            let dropped_families: &[tessera_store::manifest::ScopedScalar] = owner_view
                .split_once(tessera_store::GROUP_SEPARATOR)
                .and_then(|(group, _)| served.groups.iter().find(|g| g.name == group))
                .filter(|_| families.is_empty())
                .map_or(&[], |group| &group.scoped_scalars);
            for row in rows {
                // A deleted entity's fill is never flushed, and would hold the log where it sits.
                if overlay.is_deleted(row.entity_id) {
                    continue;
                }
                let mut scalars: Vec<WalScalar> =
                    vec![WalScalar::Null; served.declared_scalars.len()];
                let mut scoped: Vec<WalScalar> = vec![WalScalar::Null; families.len()];
                let mut any_entity = false;
                let mut any_scoped = false;
                for (at, name) in columns.iter().enumerate() {
                    let Some(value) = row.values.get(at) else {
                        continue;
                    };
                    if matches!(value, WalScalar::Null) {
                        continue;
                    }
                    if let Some(position) =
                        served.declared_scalars.iter().position(|d| &d.name == name)
                    {
                        scalars[position] = value.clone();
                        any_entity = true;
                        continue;
                    }
                    if let Some(position) = families.iter().position(|f| &f.name == name) {
                        scoped[position] = value.clone();
                        any_scoped = true;
                        continue;
                    }
                    if dropped_families.iter().any(|f| &f.name == name) {
                        continue;
                    }
                    // A column the served schema no longer carries. The values are unreadable
                    // rather than wrong, since nothing can say which column they belong to, so
                    // the open is refused rather than the cells dropped.
                    return Err(EngineError::Malformed(format!(
                        "the WAL carries a values batch naming column '{name}', which this \
                         deployment's schema does not declare"
                    )));
                }
                if any_entity {
                    buffer.fill(
                        row.entity_id,
                        tessera_lifecycle::Fill {
                            view: view.clone(),
                            scalars,
                            wal_pos: Some(*position),
                        },
                        |value| matches!(value, WalScalar::Null),
                    );
                    buffer.set_fill_wal_pos(row.entity_id, *position);
                }
                if any_scoped {
                    buffer.fill_scoped(
                        row.entity_id,
                        owner_view.clone(),
                        tessera_lifecycle::ScopedFill {
                            view: view.clone(),
                            scoped,
                            wal_pos: Some(*position),
                        },
                        |value| matches!(value, WalScalar::Null),
                    );
                    buffer.set_scoped_fill_wal_pos(row.entity_id, &owner_view, *position);
                }
            }
        }

        let established_inverse: FxHashMap<EntityId, Vec<u8>> = established
            .iter()
            .map(|(ext, ent)| (*ent, ext.clone()))
            .collect();
        let resolver_state = resolver.into_state();

        // Every batch-carrying record, by the rule both accept sites use
        // ([`tessera_lifecycle::batch_identity`]): a values batch is held under an id too, so
        // matching only `IngestBatch` would leave a values id unknown after a restart.
        let mut accepted_batches: AcceptedBatches = FxHashMap::default();
        for (record, position) in records.iter().zip(wal.replayed_positions()) {
            if let Some(identity) = tessera_lifecycle::batch_identity(record) {
                accepted_batches.insert(
                    identity.batch_id.to_string(),
                    AcceptedBatch {
                        body_hash: identity.body_hash,
                        entity_ids: identity.allocation,
                        wal_pos: *position,
                    },
                );
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
                registry,
                artifacts,
                roster,
                attributes,
                vocabularies: runtime_vocabularies,
                view_declarations,
            },
        ))
    }

}
