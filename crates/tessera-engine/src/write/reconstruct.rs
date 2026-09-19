use super::*;

/// The manifest state a reconstruction starts from, before WAL replay unions what was written
/// since: both entity-space marks and the registry's complete current view.
///
/// A struct rather than four arguments, because the four travel together. A layer in `layers`
/// whose reserved run sits above `low_water` is an inconsistency that reissues ids, so assembling
/// them at one site, where they are all read from the same manifests, is what keeps them
/// agreeing. Adding a fifth stays a compile error there rather than a defaulted argument here.
pub(crate) struct ManifestSeed<'a> {
    /// `max(build MANIFEST, side manifests)`: the point region's floor.
    pub high_water: u64,
    /// `min(ceiling, side manifests)`: the row-less region's ceiling. A build carries a term here
    /// whenever its declaration carried layers: the layers it registered spent row-less ids, and the mark
    /// recording that has to survive into what this seeds from, or the first online registration
    /// reissues them.
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
    /// Seeded after the records and before the replay. Seeding a level's records bumps its
    /// version once per record, so without this a level comes back at its record count rather
    /// than at the number the publication recorded, and every derived structure keyed on the
    /// version would be rejected on every restart. See `ArtifactStore::seed_level_version`.
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
    /// allocator at the higher of the manifest's and the WAL's marks, and rebuilds the external-id
    /// maps, the descriptor resolver's extension, the registries and the idempotency index. Returns
    /// the first generation's overlay and buffer beside it.
    ///
    /// `initial_deny` is the side-manifest's deny state, which is complete for the partition. It
    /// seeds the overlay before replay and the WAL is applied on top; dispositions are idempotent.
    pub(crate) fn reconstruct(
        wal_path: &Path,
        seed: ManifestSeed<'_>,
        dict: &Dict,
        initial_deny: &[(EntityId, ChangeOp)],
        vocabularies: &mut Vocabularies,
        has_row: impl Fn(EntityId, &str) -> bool,
    ) -> Result<(Overlay, IngestBuffer, WritePathState), EngineError> {
        let (wal, records) = Wal::open(wal_path).map_err(EngineError::Wal)?;

        // A record whose meaning is not built refuses the open, before anything is applied.
        // Records and fields land in the format ahead of the binary tracks that use them, so
        // that every track lands against one log; a log carrying one was written by a binary this
        // one is not, and replaying past it would serve state that omits what the record said.
        // Naming the track is what tells the operator which binary.
        for record in &records {
            if let Some((kind, track)) = tessera_lifecycle::wal::unbuilt_track(record) {
                return Err(EngineError::Malformed(format!(
                    "the WAL carries a {kind} record, which this build cannot apply (track {track})"
                )));
            }
        }

        // Vocabularies declared while the service ran, then the mints that name them, over what the
        // caller seeded from the manifests. Every record postdates the manifests. A record restating
        // a vocabulary the manifests carry applies only its values (a page of values is this record
        // too, because a mint carries no title); one that disagrees about kind, visibility, width or
        // a binding refuses the open, since either reading would recolour acked rows.
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
                    // Kind, visibility and the code space's width: the same fields
                    // `crate::vocabularies::resolve` compares at the door, less the reserved list,
                    // which a minter holds as spent codes rather than as a list.
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
        // The row-less mark takes the minimum where the point mark takes the maximum, because the
        // two regions grow towards each other and "furthest along" is downward here. Both homes
        // are consulted for the same reason: rotation reclaims the WAL records `low_water_from`
        // derives from, and a manifest is only as current as its last publication.
        let low_water = seed.low_water.min(low_water_from(&records));
        // `try_with_marks`, not `with_marks`: the seeds come from durable state this process did
        // not write in this run, so a corrupt or hand-edited pair that already meets must be
        // refused here, before any allocation, rather than surfacing later as an opaque
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

        // The manifests' registry is the starting point and replay runs over it, on the same
        // ordering rule the deny state below follows: every WAL record postdates any state a
        // manifest carries. Seeding afterwards would resurrect a layer that was dropped since the
        // last publication, gate and all.
        let mut registry = LayerRegistry::new();
        registry.seed(seed.layers, seed.tombstones);
        for record in &records {
            registry.apply(record);
        }
        // The layer-entity cursor cannot be inferred from the records and is not durable. A
        // `LayerCreate` says which entity a layer took, not which block it came from nor how much
        // of that block was left; resuming from `max(entity) + 1` would be wrong the moment a drop
        // retired the highest-numbered layer. Reseeding costs at most one block per restart, out
        // of 65 536, and a durable cursor would buy back an id space nothing is short of.
        registry.reseed_entity_cursor();

        // The roster follows the registry's ordering rule, for the same reason: the manifests are
        // the starting point and every WAL record postdates them, so seeding afterwards would
        // resurrect a view that was dropped since the last publication. The build's declared views
        // are seeded first because their keys are taken; a roster that forgot them would let a
        // create reissue a key a declared view already holds.
        let mut roster = tessera_lifecycle::ViewRoster::new();
        roster.seed_declared(seed.declared_views.iter().cloned());
        roster.seed(seed.created_views, seed.dead_view_incarnations);
        for record in &records {
            roster.apply(record);
        }

        // View groups and plain views declared while the service ran. One the served manifest
        // already carries applies as nothing. Groups must reach the manifest before the roster's
        // creates are merged, because `Manifest::with_roster` drops a create whose group is unknown.
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

        // The runtime attribute columns follow the same ordering rule: the manifests' lists are
        // the starting point and every record postdates them. A record naming a column the served
        // schema already holds identically is applied as nothing, which is what a fold that moved
        // the column into `MANIFEST.json` before the log rotated leaves behind; one holding a
        // different identity under a held name is a log that disagrees with the manifests about
        // what every row stores, and refuses the open.
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

        // The manifests' membership extents are the starting point, and replay unions what came
        // after, on the registry's ordering rule above: every WAL record postdates any state a
        // manifest carries, so seeding afterwards would overwrite a later publication with an
        // earlier one.
        //
        // An extent is addressed by absolute ordinal and an artifact's entity comes from its
        // layer's reserved runs, which the registry has just finished seeding, so this must run
        // after it and does.
        let mut artifacts = ArtifactStore::new();
        let mut undecodable = 0usize;
        // How many memberships the seed holds on the heap because the mapping would not take.
        // Reached only on parser drift or damage; see the record arm below.
        let mut on_heap = 0usize;
        for extent in seed.membership_extents {
            let path = seed.prefix_dir.join(&extent.path);
            // The pack is held for as long as the memberships read through it. One `Arc` per
            // extent is cloned into each `Members`, which is what makes the view below valid for
            // the store's whole life.
            let pack = Arc::new(
                tessera_store::membership::MembershipPack::open(&path)
                    .map_err(|e| EngineError::Malformed(e.to_string()))?,
            );
            let owner: Arc<dyn std::any::Any + Send + Sync> = pack.clone();
            // The manifest and the file must agree about which artifacts this range names. A
            // disagreement would serve one cluster's members under another's identity, so it
            // refuses rather than trusting either.
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
                // A dropped layer's extents outlive it until the next fold rewrites the prefix.
                // The layer is gone, so skipping them is correct, and a tombstoned name is not a
                // fault.
                continue;
            };
            for (ordinal, blob) in pack.iter() {
                // An empty blob is a hole and not a fault: the ordinal exists and holds no
                // artifact, which is what a fold leaves behind where Rule F retired one. It is
                // written rather than packed around because an ordinal is identity; reading it as
                // undecodable would alarm on every level a deletion has ever touched.
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
                        // SAFETY: `blob` is a slice of `pack`'s read-only mapping and the
                        // `Members` holds `owner` for as long as it holds the view. No extent file
                        // is written twice (names come from the publication counter, which only
                        // rises, and one executor owns a bundle root), so a mapped file is never
                        // truncated under a reader.
                        let mapped = unsafe {
                            tessera_lifecycle::membership::mapped_members(blob, owner.clone())
                        };
                        // The same cardinality check `ArtifactStore::rehouse_members` makes. Both
                        // readers walk the same blob, so a disagreement means `members_bytes` and
                        // `decode_record` have drifted apart or the bytes are damaged, and a short
                        // membership produces a low masked count for every viewer, which the
                        // existence criterion then renders as absent with nothing to distinguish
                        // it. Where the check does not hold, the record keeps the bitmap it
                        // decoded.
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
            // How far the level reaches comes from the extent, not from the records in it. A hole
            // in the middle is implied by the ordinals either side of it; a hole at the top is
            // implied by nothing, so a level seeded from records alone comes back short and the
            // next publication is handed the ordinal, and the entity derived from it, that the
            // artifact this fold deleted was published under.
            artifacts.seed_extent_bound(
                &extent.layer,
                extent.level,
                extent.ordinal_lo.saturating_add(extent.count),
            );
        }
        // The published version, before replay puts anything over it. Every level the manifest
        // names gets the counter the publication recorded, replacing whatever the seeding above
        // bumped it to; the replay below then moves it for every record the log carries past that
        // publication. That is the signal a reader deciding whether to adopt a derived structure
        // needs (`ArtifactStore::seed_level_version`).
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
        // The manifests' deny state is the starting point, and replay runs over it, on the same
        // ordering rule as `replay`'s own doc: every WAL record postdates any state a manifest
        // carries, and the one op that needs the later record to win is `Unsuppress`. Seeding
        // afterwards reverts an acked unsuppress on any restart in the publication gap.
        let mut seed = Overlay::new();
        for (entity, op) in initial_deny {
            seed.apply(*entity, *op);
        }

        let (overlay, mut buffer, established, resolver) =
            replay(&records, dict, seed, view_ids_of_key);
        // Re-hashed at the boundary, once, at startup. `replay` builds this with `FxHashMap`; the
        // live index does not, see `WritePath::established`'s doc. Converting here costs one pass
        // over the replayed set at open and keeps the hasher choice in one place rather than
        // propagating it into `tessera-lifecycle`.
        let established: std::collections::HashMap<Vec<u8>, EntityId> =
            established.into_iter().collect();

        // Replay walks every retained record, including batches whose rows a flush has already
        // given geometry, so those rows are dropped here or the next flush would write them twice.
        // The test is per (entity, view) and asks the row space, not a watermark: an entity may hold
        // rows in several views, and a watermark is exact only with one view per partition.
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

        // The values batches, into the buffer's fill map. Replayed here because resolving a column
        // name needs the served schema. A batch whose cells a flush already wrote is re-buffered;
        // `plan_flush` drops the held cells and consumes the fill, so it stops pinning the log.
        for (record, position) in records.iter().zip(wal.replayed_positions()) {
            let WalRecord::ValuesBatch {
                view,
                columns,
                rows,
                ..
            } = record
            else {
                continue;
            };
            // A record with no view names no flush pass to write its cells; no writer produces
            // one, and reading it as some view's would put a group-scoped cell in the wrong
            // column.
            let Some(view) = view else {
                return Err(EngineError::Malformed(
                    "the WAL carries a values batch naming no view, which no writer produces"
                        .to_string(),
                ));
            };
            let families = scoped_families_of_view(&served, view);
            let owner_view = scoped_owner_view_of(&served, view);
            for row in rows {
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

        // Every batch-carrying record, by the one rule both accept sites use
        // ([`tessera_lifecycle::batch_identity`]). Matching only `IngestBatch` here would miss
        // that a values batch is held under an id too, so a values id inside the retention window
        // would come back unknown after a restart. A batch id is an opaque client-chosen id and
        // the newest record naming one wins, replay order being append order.
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
