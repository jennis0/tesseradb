use mosaica_lifecycle::membership::ArtifactRecord;

use super::*;

/// The manifest state a reconstruction starts from, before WAL replay unions what was written
/// since: the high water and the registry's complete current view.
///
/// A struct rather than four arguments: the fields travel together and are all read from the same
/// manifests, and a fifth stays a compile error here rather than a defaulted argument.
pub(crate) struct ManifestSeed<'a> {
    /// Every point shard's allocator seed, ascending by shard; the first is the write shard
    /// ([`LiveState::shards`]).
    pub shards: Vec<ShardSeed>,
    /// The served side-manifest's `shards.size`.
    pub size: u64,
    pub layers: &'a [mosaica_types::layer::RegisteredLayer],
    /// The highest `next_layer_space` any partition's manifest saved.
    pub next_layer_space: u32,
    /// The highest layer registry version counter any partition's manifest saved.
    pub registry_version: u64,
    /// Every view created since the build, across every partition's manifest, and every
    /// incarnation that has died. The build's own roster is not here: it is in `MANIFEST.json`
    /// and is seeded separately.
    pub created_views: &'a [mosaica_types::view::CreatedView],
    pub dead_view_incarnations: &'a [mosaica_types::view::DeadIncarnation],
    /// The views a build declared, as `(group, key)`: the keys a create must not reissue.
    pub declared_views: Vec<(String, String)>,
    /// `Manifest::view_ids_for_key`: every view id a dropped key resolves to, the owner's and
    /// every sharing group's. Replay's `ViewDrop` arm prunes the buffer with it, and it is passed
    /// rather than derived because `mosaica-lifecycle` holds no manifest.
    pub view_ids_of_key: &'a dyn Fn(&str, &str) -> Vec<String>,
    /// Every published member pack, across every partition's manifest, beside the shard holding
    /// it, with the prefix directory their paths are relative to.
    pub membership_extents: &'a [(ShardId, mosaica_store::manifest::MembershipExtent)],
    /// Every layer space's record, across every partition's manifest: its record packs, which
    /// pair with the member packs over the same ordinals, and its levels' artifact-write counters.
    ///
    /// The counters are seeded after the records and before the replay, or a level comes back at
    /// its record count rather than the number the publication recorded. See
    /// `ArtifactStore::seed_level_version`.
    pub layer_spaces: &'a [mosaica_store::manifest::LayerSpaceRecord],
    pub prefix_dir: std::path::PathBuf,
    /// The served schema as the manifests make it: the build's columns with every side manifest's
    /// runtime declarations appended (`Manifest::with_attributes`). Replay compares each
    /// `AttributeDeclare` record against it, so a record restating a folded column is applied as
    /// nothing and one contradicting the manifests refuses the open.
    pub manifest: &'a mosaica_store::manifest::Manifest,
    /// The side manifests' `attributes` and `scoped_attributes`, the runtime declarations no fold
    /// has written into a `MANIFEST.json`; replay appends to these.
    pub attributes: crate::attributes::RuntimeAttributes,
    /// The side manifests' `vocabularies`, on [`Self::attributes`]' rule; replay appends to it.
    pub vocabularies: crate::vocabularies::RuntimeVocabularies,
    /// The side manifests' `groups` and `plain_views`, on [`Self::attributes`]' rule; replay
    /// appends to them.
    pub view_declarations: crate::view_declarations::RuntimeViewDeclarations,
}

/// One point shard's allocator, as its record leaves it.
pub(crate) struct ShardSeed {
    pub id: ShardId,
    /// The shard record's `high_water`.
    pub high_water: u64,
    /// The newest served side-manifest's freed ids of this shard, and the sets it holds back with
    /// their positions, by tenancy; empty where no manifest can be trusted to be the newest.
    pub free: mosaica_lifecycle::ByTenancy,
    pub held: Vec<(u64, mosaica_lifecycle::ByTenancy)>,
}

impl WritePath {
    /// Rebuilds the write-side state from durable storage: opens and replays the WAL, seeds the
    /// allocator, and rebuilds the descriptor resolver's extension, the
    /// registries and the idempotency index. Returns the first generation's overlay and buffer.
    ///
    /// `initial_deny` is the side-manifest's deny state; it seeds the overlay before replay, and
    /// the WAL is applied on top since dispositions are idempotent.
    pub(crate) fn reconstruct(
        wal_path: &Path,
        seed: ManifestSeed<'_>,
        dict: &Dict,
        initial_deny: &Overlays,
        vocabularies: &mut Vocabularies,
        has_row: impl Fn(EntityId, &str) -> bool,
    ) -> Result<(Overlays, IngestBuffer, WritePathState), EngineError> {
        let wal = Wal::open(wal_path).map_err(EngineError::Wal)?;
        if seed.shards.len() > 1 && wal.records().next().is_some() {
            return Err(EngineError::SeveralShardsLogged {
                shards: seed.shards.len(),
            });
        }
        let shard_ids: Vec<ShardId> = seed.shards.iter().map(|shard| shard.id).collect();

        // Two walks over the log, each holding one record at a time. The first rebuilds what the
        // second is read against: the declarations, the registry an extent's ordinals resolve
        // through, and the roster. It leaves the ingest batches unread. The second applies the
        // vocabulary mints, the memberships and every record that names an entity.

        // Vocabularies declared while the service ran; the second walk applies the mints that name
        // them. A record restating a vocabulary the manifests carry applies only its values; one
        // disagreeing about kind, visibility, width or binding refuses the open, since either
        // reading would recolour acked rows.
        let mut runtime_vocabularies = seed.vocabularies;
        // The manifest's registry is the starting point; replay runs over it. Every WAL record
        // postdates manifest state, so seeding afterwards would resurrect a layer dropped since the
        // last publication, gate and all.
        let mut registry = LayerRegistry::new();
        registry.seed(seed.layers, seed.next_layer_space, seed.registry_version);
        // Same ordering rule as the registry above. The build's declared views are seeded first
        // because their keys are taken; forgetting them would let a create reissue one.
        let mut roster = mosaica_lifecycle::ViewRoster::new();
        roster.seed_declared(seed.declared_views.iter().cloned());
        roster.seed(seed.created_views, seed.dead_view_incarnations);
        // View groups and plain views declared while the service ran; one the served manifest
        // already carries applies as nothing. Groups must reach the manifest before the roster's
        // creates are merged, or `Manifest::with_roster` drops a create whose group is unknown.
        let mut view_declarations = seed.view_declarations;
        // Same ordering rule again. A record naming a column the served schema already holds
        // identically applies as nothing; one holding a different identity under a held name
        // disagrees with the manifests about what every row stores, and refuses the open.
        // `unique` is not compared: it is the one flag a later declaration may change, and the
        // unique events decide it.
        let mut attributes = seed.attributes;
        let mut served = seed.manifest.clone();
        let mut unique_events: Vec<crate::write::UniqueEvent> = Vec::new();
        // Rotation reclaims a prefix of the log, so its last declaration is the newest.
        let mut size = seed.size;
        // Up to the last view drop every row is buffered, flushed or not, because a drop's
        // promotion of a join row to its item's own row reads the buffer. Past it, a row that
        // already has geometry is not buffered. A drop late in a held log therefore buffers most
        // of the log's rows until the walk ends.
        let mut last_view_drop: Option<u64> = None;

        for item in wal.records_but_ingest() {
            let (position, record) = item.map_err(EngineError::Wal)?;
            // A record whose meaning is not built refuses the open: a log carrying one was written
            // by a binary this one is not, and replaying past it would serve state that omits what
            // the record said. Naming the track tells the operator which binary.
            if let Some((kind, track)) = mosaica_lifecycle::wal::unbuilt_track(&record) {
                return Err(EngineError::Malformed(format!(
                    "the WAL carries a {kind} record, which this build cannot apply (track {track})"
                )));
            }
            registry.apply(&record);
            roster.apply(&record);
            match &record {
                WalRecord::VocabularyDeclare { declaration } => {
                    let compiled =
                        crate::vocabularies::compile_record(declaration).ok_or_else(|| {
                            EngineError::Malformed(format!(
                                "the WAL declares vocabulary '{}' at width '{}', which is not a \
                                 code width",
                                declaration.name, declaration.width
                            ))
                        })?;
                    match vocabularies.get_mut(&compiled.name) {
                        Some(minter) => {
                            // Kind, visibility and width: the same fields
                            // `crate::vocabularies::resolve` compares at the door.
                            if minter.kind() != compiled.kind
                                || minter.visibility() != compiled.visibility
                                || minter.width() != compiled.width
                            {
                                return Err(EngineError::Malformed(format!(
                                    "the WAL declares vocabulary '{}' with a kind, visibility or \
                                     width the manifests do not carry for that name",
                                    compiled.name
                                )));
                            }
                            for value in &compiled.values {
                                minter
                                    .seed_value(&value.key, value.code)
                                    .map_err(|e| EngineError::Malformed(e.to_string()))?;
                                // Replayed in log order, so the last title a page supplied is the
                                // one the minter ends holding.
                                if let Some(title) = &value.title {
                                    minter.set_title(&value.key, title.clone());
                                }
                            }
                            for &code in &compiled.reserved {
                                minter.seed_reserved(code);
                            }
                        }
                        None => {
                            let mut minter = mosaica_store::vocabulary::VocabularyMinter::new(
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
                    // The runtime list is what the next publication writes. A name
                    // `MANIFEST.json` already carries is one a fold has folded in, and belongs to
                    // one list, not two.
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
                WalRecord::UniqueDeclare {
                    attribute,
                    unique,
                    base,
                    live,
                } => {
                    unique_events.push(crate::write::UniqueEvent {
                        attribute: attribute.clone(),
                        unique: *unique,
                        base: base.clone(),
                        live: live.clone(),
                    });
                }
                WalRecord::AttributeDeclare { declaration } => {
                    let Some(compiled) = crate::attributes::compile_record(declaration) else {
                        return Err(EngineError::Malformed(format!(
                            "the WAL declares attribute '{}' with type '{}', which this build \
                             cannot store",
                            declaration.name, declaration.ty
                        )));
                    };
                    match crate::attributes::held_by_name(&served, &declaration.name) {
                        Some(held) if crate::attributes::same_but_unique(&held, &compiled) => {}
                        Some(_) => {
                            return Err(EngineError::Malformed(format!(
                                "the WAL declares attribute '{}' with a type or scope the \
                                 manifests do not carry for that name",
                                declaration.name
                            )));
                        }
                        None => {
                            match &compiled {
                                crate::attributes::CompiledAttribute::Entity(d) => {
                                    served = served.with_attributes(std::slice::from_ref(d), &[]);
                                }
                                crate::attributes::CompiledAttribute::Scoped(f) => {
                                    served = served.with_attributes(&[], std::slice::from_ref(f));
                                }
                            }
                            if declaration.unique {
                                unique_events.push(crate::write::UniqueEvent {
                                    attribute: declaration.name.clone(),
                                    unique: true,
                                    base: Vec::new(),
                                    live: Vec::new(),
                                });
                            }
                            attributes.push(compiled);
                        }
                    }
                }
                WalRecord::ViewDrop { .. } => last_view_drop = Some(position),
                WalRecord::ShardsDeclare { size: declared } => size = *declared,
                _ => {}
            }
        }
        // Same ordering rule again: replay unions onto the manifests' membership extents. An
        // extent is addressed by absolute ordinal from the layer's reserved runs, so this must run
        // after the whole registry is replayed.
        let SeededArtifacts {
            mut artifacts,
            slices,
            mut undecodable,
            on_heap,
        } = seed_artifacts(
            &registry,
            seed.layer_spaces,
            seed.membership_extents,
            &shard_ids,
            &seed.prefix_dir,
        )?;

        // Same ordering rule again: the overlay is seeded and the log replayed on top, because
        // seeding after replay would revert an acked unsuppress the newest manifest predates.
        let mut replay = Replay::new(dict, initial_deny.clone(), seed.view_ids_of_key);
        let mut wal_high_water = 0u64;
        // Every point entity a kept record names.
        let mut named = croaring::Bitmap::new();
        // Each edit's `(number, new entity)` whose first row no flush has written, and so whose
        // pair no run holds; kept where the entity ends buffered or deleted.
        let mut edits: Vec<(EntityId, EntityId)> = Vec::new();
        // Every batch-carrying record, by the rule both accept sites use
        // ([`mosaica_lifecycle::batch_identity`]).
        let mut accepted_batches: AcceptedBatches = FxHashMap::default();

        for item in wal.records() {
            let (position, record) = item.map_err(EngineError::Wal)?;
            if let WalRecord::VocabularyMint {
                vocabulary,
                key,
                code,
            } = &record
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
            if names_held_registration(&registry, &record) {
                // Before the record lands, as on the live path: a publication into a level holding
                // no artifact chooses the level's layout.
                registry.settle_layout(&artifacts, &record);
                undecodable += artifacts.apply(&record, position);
            }
            // A row whose (entity, view) already has geometry was given it by a flush; buffering it
            // would have the next flush write it twice. The test is per (entity, view), not a
            // watermark, since an entity may hold rows in several views.
            let past_drops = last_view_drop.is_none_or(|drop| position > drop);
            replay.apply(&record, position, |entity, view| {
                past_drops && has_row(entity, view)
            });
            wal_high_water = wal_high_water.max(high_water_from([&record]));
            named.or_inplace(&mosaica_lifecycle::alloc::entities_named([&record]));
            if let WalRecord::IngestBatch { edits: batch, .. } = &record {
                edits.extend(batch.iter().filter_map(|edit| {
                    let first = edit.rows.first()?;
                    (!has_row(first.entity_id, &first.view))
                        .then_some((edit.number, first.entity_id))
                }));
            }
            if let Some(identity) = mosaica_lifecycle::batch_identity(&record) {
                accepted_batches.insert(
                    identity.batch_id.to_string(),
                    AcceptedBatch {
                        body_hash: identity.body_hash,
                        receipt: identity.receipt.to_vec(),
                        wal_pos: position,
                    },
                );
            }
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

        let (mut overlay, mut buffer, resolver) = replay.finish();
        // A dropped layer's denies leave with it, though the log and the manifest still hold them.
        overlay.spaces.retain(|space, _| registry.in_space(*space).is_some());

        // Rows buffered up to the last view drop that a flush has already given geometry.
        let already_flushed: Vec<(EntityId, String)> = buffer
            .rows()
            .map(|(entity, item)| (*entity, item.view.clone()))
            .filter(|(entity, view)| has_row(*entity, view))
            .collect();
        for (entity, view) in already_flushed {
            buffer.remove_in_view(entity, &view);
        }

        // An id the manifest records as free and a kept record or the overlay names was issued
        // since the manifest was written.
        named.or_inplace(overlay.items.deleted_set());
        named.or_inplace(overlay.items.suppressed_set());
        let unnamed = croaring::Bitmap::new();
        let mut shards = Vec::with_capacity(seed.shards.len());
        for (at, shard) in seed.shards.into_iter().enumerate() {
            // The log and `named` are the write shard's alone.
            let (wal_high_water, named) = match at {
                0 => (wal_high_water, &named),
                _ => (0, &unnamed),
            };
            // `try_new`: the seed comes from durable state this run did not write, so a corrupt
            // or hand-edited mark is refused here, not as a later opaque refusal.
            let mut allocator = Allocator::try_new(shard.high_water.max(wal_high_water))
                .ok_or_else(|| {
                    EngineError::Malformed(format!(
                        "the high water durable state leaves for shard {} (manifest {}, log \
                         {wal_high_water}) is past {}, where a shard's ids end. Restore the \
                         side-manifest and log this bundle was written with",
                        shard.id,
                        shard.high_water,
                        mosaica_types::ENTITY_END,
                    ))
                })?;
            // A size no declaration could have left is refused at open, not at the first batch.
            mosaica_types::check_shard_size(size, allocator.high_water()).map_err(|e| {
                EngineError::Malformed(format!(
                    "the shard size the manifest and the WAL leave ({size}) does not hold: {e}"
                ))
            })?;
            allocator.set_size(size);
            allocator.seed_freed(shard.free, shard.held, wal.retained_from(), named);
            shards.push(ShardWrite::new(shard.id, allocator));
        }

        let edited_live = crate::edited::EditedLive::derive(&edits, &buffer, &overlay.items);

        let resolver_state = resolver.into_state();

        Ok((
            overlay,
            buffer,
            WritePathState {
                wal,
                shards,
                resolver_state,
                accepted_batches,
                registry,
                artifacts,
                slices: slices
                    .into_iter()
                    .map(|(shard, slice)| (shard, Arc::new(slice)))
                    .collect(),
                roster,
                attributes,
                vocabularies: runtime_vocabularies,
                view_declarations,
                unique_events,
                edited_live,
            },
        ))
    }
}

/// What the manifests' record and member packs seed, before the log is replayed over them.
struct SeededArtifacts {
    /// Every artifact's whole record, with the write shard's slice of its membership and
    /// generating sets.
    artifacts: ArtifactStore,
    /// Every other point shard's slice: see [`LiveState::slices`].
    slices: Vec<(ShardId, ArtifactStore)>,
    /// Artifacts a pack held that did not decode, or whose slices did not hold their sets.
    undecodable: usize,
    /// Memberships held on the heap because the pack's mapping would not take.
    on_heap: usize,
}

/// Every artifact the packs of `layer_spaces` hold, read through `registry`'s runs, each record
/// paired with every one of `shards`' member packs over the same ordinals. `shards` is ascending,
/// so the first is the write shard.
fn seed_artifacts(
    registry: &LayerRegistry,
    layer_spaces: &[mosaica_store::manifest::LayerSpaceRecord],
    membership_extents: &[(ShardId, mosaica_store::manifest::MembershipExtent)],
    shards: &[ShardId],
    prefix_dir: &Path,
) -> Result<SeededArtifacts, EngineError> {
    use mosaica_lifecycle::membership::{decode_slice, decode_slice_members, slices_hold_their_sets};
    let several = shards.len() > 1;
    let mut seeded = SeededArtifacts {
        artifacts: ArtifactStore::new(),
        slices: shards
            .iter()
            .skip(1)
            .map(|shard| (*shard, ArtifactStore::new()))
            .collect(),
        undecodable: 0,
        on_heap: 0,
    };
    let open_pack = |rel: &str, kind, ordinal_lo: u32, count: u32| {
        let pack = mosaica_store::membership::LevelPack::open(&prefix_dir.join(rel), kind)
            .map_err(|e| EngineError::Malformed(e.to_string()))?;
        // The manifest and the file must agree about which artifacts this range names, or a
        // disagreement would serve one cluster's members under another's identity.
        if pack.ordinal_lo() != ordinal_lo || pack.count() != count {
            return Err(EngineError::Malformed(format!(
                "pack {rel} covers [{}, +{}) but the manifest names [{ordinal_lo}, +{count})",
                pack.ordinal_lo(),
                pack.count(),
            )));
        }
        Ok(Arc::new(pack))
    };
    for space in layer_spaces {
        // A dropped layer's space outlives it in a manifest the log's drop postdates.
        let Some((name, layer)) = registry.in_space(space.space) else {
            continue;
        };
        let name = name.to_string();
        // A stored layer's slice holds a record only for an artifact the shard holds something
        // of, or one borrowing its target's membership; the form walks the rest as empty
        // (`crate::artifacts::LevelSlice`). A resolved membership is read from its slice whole.
        let sparse = several && stored_membership(&layer.declaration);
        for extent in &space.record_packs {
            let Some(runs) = layer.runs.get(extent.level as usize) else {
                return Err(EngineError::Malformed(format!(
                    "record pack {} names level {} of layer space {}, which has no such level",
                    extent.path, extent.level, space.space
                )));
            };
            let records = open_pack(
                &extent.path,
                mosaica_store::membership::PackKind::Records,
                extent.ordinal_lo,
                extent.count,
            )?;
            // Every shard's member pack over the same ordinals, in `shards`' order. Each is held
            // for as long as the memberships read through it: one `Arc` per pack is cloned into
            // each `Members`.
            let mut packs = Vec::with_capacity(shards.len());
            for shard in shards {
                let Some((_, members)) = membership_extents
                    .iter()
                    .find(|(held, m)| held == shard && m.pairs(space.space, extent))
                else {
                    return Err(EngineError::Malformed(format!(
                        "record pack {} has no member pack in shard {shard} over the same \
                         ordinals",
                        extent.path
                    )));
                };
                packs.push(open_pack(
                    &members.path,
                    mosaica_store::membership::PackKind::Members,
                    extent.ordinal_lo,
                    extent.count,
                )?);
            }
            let mut member_blobs: Vec<_> = packs.iter().map(|pack| pack.iter()).collect();
            let mut blobs: Vec<&[u8]> = Vec::with_capacity(shards.len());
            for (ordinal, record_blob) in records.iter() {
                blobs.clear();
                blobs.extend(
                    member_blobs
                        .iter_mut()
                        .map(|blobs| blobs.next().map_or(&[][..], |(_, blob)| blob)),
                );
                // An empty record and empty slices are a hole, not a fault: the ordinal exists
                // with no artifact, where a fold retired one.
                if record_blob.is_empty() && blobs.iter().all(|blob| blob.is_empty()) {
                    continue;
                }
                let Some(entity) = runs.entity_of(ordinal as u64).map(EntityId::new) else {
                    seeded.undecodable += 1;
                    continue;
                };
                // The record decodes once, with the write shard's slice; every other slice is its
                // members and sets. Together they hold every generating set whole, or the
                // artifact is damage and refused in every shard.
                let Some((mut record, shape)) = decode_slice(entity, record_blob, blobs[0]) else {
                    seeded.undecodable += 1;
                    continue;
                };
                let mut others = Vec::with_capacity(shards.len() - 1);
                for (at, blob) in blobs.iter().enumerate().skip(1) {
                    match decode_slice_members(blob, record.contents.len()) {
                        Some((members, sets)) if sets.len() == record.contents.len() => {
                            others.push((at, slice_record(&record, members, sets)));
                        }
                        _ => break,
                    }
                }
                let held = std::iter::once(&record)
                    .chain(others.iter().map(|(_, slice)| slice))
                    .collect::<Vec<_>>();
                if others.len() + 1 != shards.len() || !slices_hold_their_sets(&held) {
                    seeded.undecodable += 1;
                    continue;
                }
                if several {
                    let declares = held.iter().any(|slice| !slice.members.is_empty());
                    record.declares_members = Some(declares);
                    for (_, slice) in &mut others {
                        slice.declares_members = Some(declares);
                    }
                }
                let on_heap = &mut seeded.on_heap;
                let mut mapped = |record: &mut ArtifactRecord, at: usize| {
                    // A pack lists only what its shard holds: an empty blob has nothing to map.
                    if blobs[at].is_empty() {
                        return;
                    }
                    // The membership is read through the pack's mapping and the decoded bitmap
                    // is dropped, to avoid holding a heap bitmap per artifact.
                    //
                    // SAFETY: `blobs[at]` is a slice of `packs[at]`'s read-only mapping, and
                    // `Members` holds the pack for as long as it holds the view. No pack is
                    // written twice (names come from a counter that only rises, one executor
                    // per bundle root), so a mapped file is never truncated under a reader.
                    let owner: Arc<dyn std::any::Any + Send + Sync> = packs[at].clone();
                    let members =
                        unsafe { mosaica_lifecycle::membership::mapped_members(blobs[at], owner) };
                    // The same cardinality check `ArtifactStore::rehouse_members` makes: a
                    // disagreement means the two readers drifted apart or the bytes are
                    // damaged. Where it fails, the record keeps the bitmap it decoded.
                    match members {
                        Some(members) if members.cardinality() == record.members.cardinality() => {
                            record.members = members;
                        }
                        _ => *on_heap += 1,
                    }
                };
                mapped(&mut record, 0);
                for (at, mut slice) in others {
                    if sparse && holds_nothing(&slice) {
                        continue;
                    }
                    mapped(&mut slice, at);
                    let (_, store) = &mut seeded.slices[at - 1];
                    store.seed(&name, space.space, extent.level, ordinal, slice, None);
                }
                seeded
                    .artifacts
                    .seed(&name, space.space, extent.level, ordinal, record, shape);
            }
            // How far the level reaches comes from the pack, not the records in it: a hole at the
            // top is implied by nothing, so a level seeded from records alone would come back
            // short.
            let bound = extent.ordinal_lo.saturating_add(extent.count);
            for store in seeded.stores_mut() {
                store.seed_extent_bound(&name, space.space, extent.level, bound);
            }
        }
        // The published version, before replay moves it. Every level the manifest names gets the
        // counter the publication recorded, which the replay then advances for every record past
        // that publication. See `ArtifactStore::seed_level_version`.
        for version in &space.level_versions {
            for store in seeded.stores_mut() {
                store.seed_level_version(&name, version.level, version.version);
            }
        }
    }
    if several {
        seeded.seed_borrowed_targets();
    }
    Ok(seeded)
}

impl SeededArtifacts {
    /// Seed in each slice every artifact a borrowing artifact's chain reaches and the slice left
    /// out, holding nothing: a borrowing artifact resolves its target in its own shard's slice,
    /// and over a slice that held every artifact the chain resolves to an empty membership there.
    fn seed_borrowed_targets(&mut self) {
        use mosaica_lifecycle::membership::borrows_membership;
        let SeededArtifacts {
            artifacts, slices, ..
        } = self;
        let mut reached = rustc_hash::FxHashSet::default();
        for (_, _, _, record) in artifacts.every_artifact() {
            let mut at = record;
            while borrows_membership(at) {
                let Some(target) = at.attached_to.as_ref() else {
                    break;
                };
                let Some(held) = artifacts
                    .get(&target.layer, target.level, target.ordinal)
                    .filter(|held| held.entity == target.entity)
                else {
                    break;
                };
                if !reached.insert((target.layer.clone(), target.space, target.level, target.ordinal)) {
                    break;
                }
                at = held;
            }
        }
        for (layer, space, level, ordinal) in reached {
            let Some(held) = artifacts.get(&layer, level, ordinal) else {
                continue;
            };
            for (_, slice) in slices.iter_mut() {
                if slice.get(&layer, level, ordinal).is_none() {
                    let sets = held.contents.iter().map(|_| croaring::Bitmap::new()).collect();
                    let record = slice_record(held, croaring::Bitmap::new(), sets);
                    slice.seed(&layer, space, level, ordinal, record, None);
                }
            }
        }
    }

    fn stores_mut(&mut self) -> impl Iterator<Item = &mut ArtifactStore> {
        std::iter::once(&mut self.artifacts).chain(self.slices.iter_mut().map(|(_, slice)| slice))
    }
}

/// Whether a shard's `slice` of an artifact holds no member and no generating-set member, and
/// takes no membership from a target.
fn holds_nothing(slice: &ArtifactRecord) -> bool {
    slice.members.is_empty()
        && slice
            .contents
            .iter()
            .all(|content| content.generated_from.is_empty())
        && !mosaica_lifecycle::membership::borrows_membership(slice)
}

/// One point shard's slice of `record`: `members` and `sets` in that shard, and of the record only
/// what a row form reads beside them.
fn slice_record(
    record: &ArtifactRecord,
    members: croaring::Bitmap,
    sets: Vec<croaring::Bitmap>,
) -> ArtifactRecord {
    ArtifactRecord {
        entity: record.entity,
        key: None,
        view: record.view.clone(),
        incarnation: record.incarnation,
        members: members.into(),
        declares_members: record.declares_members,
        contents: record
            .contents
            .iter()
            .zip(sets)
            .map(|(content, generated_from)| mosaica_lifecycle::membership::ContentSet {
                values: None,
                digest: [0; 32],
                generated_from,
                cardinality: content.cardinality,
            })
            .collect(),
        attached_to: record.attached_to.clone(),
        parents: Vec::new(),
        access: Vec::new(),
    }
}

/// Whether `record`, where it writes a layer's artifacts, was written for the registration the
/// replayed registry holds under the layer's name.
fn names_held_registration(registry: &LayerRegistry, record: &WalRecord) -> bool {
    match record {
        WalRecord::ArtifactPublish { layer, space, .. }
        | WalRecord::ArtifactGrow { layer, space, .. }
        | WalRecord::ArtifactFill { layer, space, .. } => registry.names_held(layer, *space),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use croaring::Bitmap;
    use mosaica_lifecycle::membership::{
        content_digest, encode_record, frame_members, ArtifactShapes, ContentSet, Members,
    };
    use mosaica_store::manifest::{
        LayerSpaceRecord, LevelVersion, MembershipExtent, RecordPackExtent,
    };
    use mosaica_store::membership::{pack, PackKind};
    use mosaica_types::layer::{EntityRun, RegisteredLayer, ReservedRuns, RESERVED_BLOCK};

    use super::*;
    use crate::artifacts::test_support::declaration;

    /// `clusters/a`, level 0 reserved four entities from the first block.
    fn registry() -> LayerRegistry {
        let mut registry = LayerRegistry::new();
        registry.seed(
            &[RegisteredLayer {
                declaration: declaration(false, None),
                space: LayerSpace::ZERO,
                runs: vec![ReservedRuns::from_runs(vec![EntityRun {
                    start: RESERVED_BLOCK,
                    end: RESERVED_BLOCK + 4,
                }])],
                version: 1,
                layouts: Vec::new(),
            }],
            1,
            1,
        );
        registry
    }

    /// One artifact's record, with one content of `cardinality` members over every shard.
    fn record(ordinal: u64, cardinality: u64) -> ArtifactRecord {
        ArtifactRecord {
            entity: EntityId::new(RESERVED_BLOCK + ordinal),
            key: Some(format!("k{ordinal}")),
            view: None,
            incarnation: 0,
            members: Members::default(),
            declares_members: None,
            contents: vec![ContentSet {
                values: None,
                digest: content_digest(&["a label".to_string()]),
                generated_from: Bitmap::new(),
                cardinality,
            }],
            attached_to: None,
            parents: Vec::new(),
            access: vec![b"t".to_vec()],
        }
    }

    /// Two shards' packs of one level, written under `dir`: ordinal 0 split over both shards,
    /// 1 a hole, 2 a set whose slices sum short of its cardinality, and 3 members in shard 0
    /// alone. `with_second` leaves shard 1's member pack out of the manifest where false.
    fn two_shards(dir: &Path, with_second: bool) -> Result<SeededArtifacts, EngineError> {
        let shape = ArtifactShapes::new(vec![("s0".into(), vec![1, 2, 3])]);
        let records = [
            encode_record(&record(0, 3), shape.as_ref()),
            Vec::new(),
            encode_record(&record(2, 2), None),
            encode_record(&record(3, 0), None),
        ];
        let slice = |members: &[u32], set: &[u32]| {
            frame_members(&Bitmap::of(members), [Bitmap::of(set)].iter())
        };
        let members = [
            [slice(&[1, 2], &[1, 2]), Vec::new(), slice(&[1], &[1]), slice(&[4], &[])],
            [slice(&[3], &[3]), Vec::new(), slice(&[], &[]), slice(&[], &[])],
        ];
        std::fs::write(dir.join("records.pack"), pack(PackKind::Records, 0, &records)).unwrap();
        let mut extents = Vec::new();
        for (shard, blobs) in members.iter().enumerate() {
            let path = format!("members-{shard}.pack");
            std::fs::write(dir.join(&path), pack(PackKind::Members, 0, blobs)).unwrap();
            if shard == 0 || with_second {
                let shard = ShardId::new(shard as u32).expect("in range");
                let extent = MembershipExtent {
                    path,
                    space: LayerSpace::ZERO,
                    level: 0,
                    ordinal_lo: 0,
                    count: 4,
                };
                extents.push((shard, extent));
            }
        }
        let mut space = LayerSpaceRecord::empty(LayerSpace::ZERO);
        space.record_packs = vec![RecordPackExtent {
            path: "records.pack".into(),
            level: 0,
            ordinal_lo: 0,
            count: 4,
        }];
        space.level_versions = vec![LevelVersion { level: 0, version: 9 }];
        let shards = [ShardId::ZERO, ShardId::new(1).expect("in range")];
        seed_artifacts(&registry(), &[space], &extents, &shards, dir)
    }

    /// The write store holds each whole record with its shard's slice and the shape, the other
    /// shard's store its slice beside the record's attachment and cardinalities alone, and both
    /// reach the level's bound and version. A record whose slices sum short of a set is refused in
    /// every shard, and an artifact whose members are all in one shard declares members in each.
    #[test]
    fn every_shards_slice_of_every_artifact_is_read_at_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        let seeded = two_shards(dir.path(), true).expect("the packs pair");
        let layer = "clusters/a";
        let (written, slice) = (&seeded.artifacts, &seeded.slices[0].1);
        assert_eq!(seeded.slices.len(), 1);
        assert_eq!(seeded.undecodable, 1, "the set that sums short");

        let whole = written.get(layer, 0, 0).expect("held whole");
        let part = slice.get(layer, 0, 0).expect("held as a slice");
        assert_eq!(whole.members, Bitmap::of(&[1, 2]));
        assert_eq!(part.members, Bitmap::of(&[3]));
        assert_eq!(whole.contents[0].generated_from, Bitmap::of(&[1, 2]));
        assert_eq!(part.contents[0].generated_from, Bitmap::of(&[3]));
        assert_eq!(part.contents[0].cardinality, 3);
        assert_eq!((whole.access.len(), part.access.len()), (1, 0));
        assert_eq!(part.key, None);
        assert!(written.shape_of(layer, 0, 0).is_some());
        assert!(slice.shape_of(layer, 0, 0).is_none());

        for store in [written, slice] {
            assert!(store.get(layer, 0, 1).is_none(), "a hole");
            assert!(store.get(layer, 0, 2).is_none(), "refused in every shard");
            assert_eq!(store.slots(layer, 0).len(), 4);
            assert_eq!(store.level_version(layer, 0), 9);
        }
        let own = written.get(layer, 0, 3).expect("members in shard 0 alone");
        assert_eq!(own.declares_members, Some(true));
        assert!(
            slice.get(layer, 0, 3).is_none(),
            "a shard holding nothing of an artifact seeds no slice of it"
        );

        assert!(
            matches!(two_shards(dir.path(), false), Err(EngineError::Malformed(_))),
            "a record pack with no member pack in one shard"
        );
    }
}
