use super::*;

/// The refusal a roster error is answered with: the three the wire tells apart, and this module's
/// `ExecError` doc for why the caller's remedy decides.
pub(super) fn roster_error(e: mosaica_lifecycle::RosterError) -> ExecError {
    use mosaica_lifecycle::RosterError;
    let detail = e.to_string();
    match e {
        // A conflict is a live key and nothing else now: a dropped key is created again at a
        // fresh incarnation, so the tombstone arm this match once had has no refusal left to
        // carry.
        RosterError::Exists { .. } => ExecError::ViewConflict { detail },
        RosterError::Unknown { .. } => ExecError::ViewUnknown { detail },
        RosterError::Refused(_) => ExecError::ViewRefused { detail },
    }
}

/// The entities of `views` that hold a row in no other view, the commit-window buffer included:
/// the items a drop of `views` deletes.
///
/// `views` is every id the dropped key resolves to (`Manifest::view_ids_for_key`), not the one
/// the request happened to name: a key is a view of the group that owns it and one of every group
/// sharing its views, so a probe over a single spelling reads the wrong row space when the drop
/// was addressed to the other, and would delete an entity that holds a row under the key's own
/// second name.
///
/// A row accepted but not yet flushed counts as a row in its view. An item whose only row is
/// buffered in `views` is deleted, since the drop discards that row, and one with a buffered row in
/// another view is kept. The cost is [`view_entities`]'s walk, then a `row_of` per other view and
/// a buffer lookup per entity found.
pub(super) fn dangling_entities(generation: &Generation, views: &[String]) -> Vec<EntityId> {
    let in_views = |view: &String| views.contains(view);
    view_entities(generation, views)
        .iter()
        .map(|raw| EntityId::new(u64::from(raw)))
        .filter(|entity| {
            let in_another_view = generation.bundle.partitions.values().any(|partition| {
                partition.views.iter().any(|(id, data)| {
                    !in_views(id) && data.row_space.row_of(*entity).is_some()
                })
            });
            let buffered_elsewhere = generation
                .buffer
                .rows_of(*entity)
                .any(|item| !in_views(&item.view));
            !in_another_view && !buffered_elsewhere
        })
        .collect()
}

/// Every entity holding a row in one of these views, or buffered for one, minus the deleted: the
/// set an exclusion is complemented against, and the one a drop's deletions are found in.
///
/// A deleted entity is excluded and a suppressed one is not. A deletion is irreversible and its
/// entity can contribute to no count again, where a suppression is a live member temporarily
/// outside every mask, which the inclusion spelling would have named and this one keeps.
///
/// The cost is a walk of the views' own slots, on the executor loop: the base permutation's
/// present pages ([`mosaica_store::permutation::Permutation::try_for_each_slot`]) and each
/// extent's rows, then the buffer once.
pub(super) fn view_entities(generation: &Generation, views: &[String]) -> croaring::Bitmap {
    let mut entities = croaring::Bitmap::new();
    for view in views {
        for partition in generation.bundle.partitions.values() {
            let Some(view_data) = partition.views.get(view) else {
                continue;
            };
            let row_space = &view_data.row_space;
            let _ = row_space
                .base()
                .try_for_each_slot(|entity, _| -> Result<(), std::convert::Infallible> {
                    entities.add(entity as u32);
                    Ok(())
                });
            for extent in row_space.extents() {
                entities.extend(extent.pairs().map(|(entity, _)| entity as u32));
            }
        }
    }
    // Buffered rows are in the view (`rows()`, joins included): a point acknowledged and not
    // yet flushed is an entity of this view, and an exclusion taken without it would leave every
    // such point out of the membership for ever: the one asymmetry between the two spellings
    // that would not be stale but wrong.
    for (entity, item) in generation.buffer.rows() {
        if views.iter().any(|view| view == &item.view) {
            entities.add(entity.raw() as u32);
        }
    }
    entities.andnot_inplace(generation.overlay.deleted_set());
    entities
}

/// What `commit_growth` answers per join: the artifact's entity and how many of the joining
/// members it did not already hold.
///
/// Every key here has resolved in `prepare_grow` under the same lock, so the second lookup cannot
/// fail; a failure is a bug in that ordering and is treated as one. The difference is taken
/// against the membership as it stands before the record is applied, which is the only time
/// it exists.
pub(super) fn growth_receipt(
    registry: &LayerRegistry,
    store: &ArtifactStore,
    layer: &str,
    level: u32,
    joins: &[mosaica_lifecycle::IncomingGrowth],
    prepared: &mosaica_lifecycle::PreparedGrow,
) -> Vec<mosaica_lifecycle::MembershipGrown> {
    joins
        .iter()
        .zip(&prepared.filled)
        .enumerate()
        .map(|(index, (join, filled))| {
            let ordinal = registry
                .resolve_growth_key(layer, level, join.view.as_deref(), &join.key, store)
                .expect("prepare_grow resolved every key before the receipt was read");
            let record = store
                .get(layer, level, ordinal)
                .expect("a resolved ordinal names a record");
            // The set this row moves, which is the membership on a row with no rank and the
            // content's generating set on a row with one. A rank naming no content refused the
            // batch in `prepare_grow` above, so the `None` arm here is unreachable and answers
            // nothing rather than panicking on a thread that owes an acknowledgement.
            //
            // `left` is counted against the set the joins have already entered, which is the
            // order the page is applied in: an entity this page both joins and leaves is one this
            // page took out. Both counts are taken from how much of a bitmap the set holds, so
            // neither the set nor the set after the joins is built.
            let counted = |held: &dyn Fn(&croaring::Bitmap) -> u64| {
                let joined = join.joining.cardinality() - held(&join.joining);
                let left = if join.leaving.is_empty() {
                    0
                } else {
                    let both = join.leaving.and(&join.joining);
                    held(&join.leaving) + both.cardinality() - held(&both)
                };
                (joined, left)
            };
            let (joined, left) = match join.rank {
                None => counted(&|other| record.members.and_cardinality(other)),
                Some(rank) => record
                    .contents
                    .get(rank as usize)
                    .map_or((0, 0), |content| {
                        counted(&|other| content.generated_from.and_cardinality(other))
                    }),
            };
            mosaica_lifecycle::MembershipGrown {
                entity: record.entity,
                joined,
                filled: *filled,
                left,
                withdrawn: prepared
                    .withdrawn
                    .iter()
                    .find(|(row, _)| *row == index)
                    .map(|(_, rank)| *rank),
            }
        })
        .collect()
}

/// The executor's refusal for a registry error: a differing fixed part is the caller's `409`
/// (`ExecError::PartConflict`), everything else the `422` a refused layer operation has always
/// been.
pub(super) fn refusal_of(e: mosaica_lifecycle::RegistryError) -> ExecError {
    match e {
        // A second `excluding` on a held key is the same `409` a differing fixed part is: the
        // complement it asks for is a different set from the one the artifact holds.
        mosaica_lifecycle::RegistryError::PartConflict { .. }
        | mosaica_lifecycle::RegistryError::ExclusionOnHeldKey { .. } => ExecError::PartConflict {
            detail: e.to_string(),
        },
        other => ExecError::LayerRefused {
            detail: other.to_string(),
        },
    }
}

/// What `WritePath::publish_artifacts` answers: the entities in the caller's order and the
/// batch's counts.
///
/// A key the level held is accepted under the fill rule and is not created, so `created` is how
/// many artifacts the batch minted, `without_content` how many of those carry no content on a
/// layer declaring some, `filled` how many fixed parts were filled on held artifacts, and `joined`
/// how many memberships the batch added, to the artifacts it created and to held ones alike. None
/// names an artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PublishedBatch {
    pub(crate) entities: Vec<EntityId>,
    pub(crate) created: u64,
    pub(crate) without_content: u64,
    pub(crate) filled: u64,
    pub(crate) joined: u64,
}

impl Executor {
    pub(super) fn execute(&mut self, command: Command) {
        match command {
            // Unreachable on the deny lane while the lane follows the command; handled so the
            // executor stays total over `Command`. This goes through `admit_ingest` rather than
            // around it, so if this arm ever becomes reachable it still has idempotency rather
            // than becoming a second, unchecked ingest path. The window it is given is empty, so
            // `BatchState::Held` is unconstructible here and the answers are exactly `admit`'s.
            Command::Ingest { submission, reply } => {
                let window = CommitWindow::new(self.next_window_seq());
                let (window, _) = self.admit_ingest(
                    window,
                    submission,
                    reply,
                );
                if !window.is_empty() {
                    self.close_window(window);
                }
            }
            // A window of one entry is exactly the per-command semantics, which is why there is
            // no second deny implementation to keep in step with the first.
            Command::Changes {
                mut changes,
                stamp,
                reply,
            } => {
                if !self.follow_since(stamp, |generation| {
                    crate::edited::follow_changes(generation, &mut changes)
                }) {
                    reply.fail(ExecError::Stale);
                    return;
                }
                let mut entries = vec![DenyEntry {
                    changes,
                    reply: Some(reply),
                }];
                // The cascade rides this path too: a deletion admitted here that skipped it would
                // strand every artifact depending on the one deleted.
                self.cascade_dependents(&mut entries);
                self.commit_denies(entries)
            }
            Command::RegisterLayer { declaration, reply } => self.commit_registry(
                |registry, alloc| registry.prepare_create(*declaration, alloc),
                |record| match record {
                    WalRecord::LayerCreate { layer_entity, .. } => *layer_entity,
                    _ => unreachable!("prepare_create returns a LayerCreate"),
                },
                reply,
            ),
            Command::DropLayer { name, reply } => {
                self.commit_registry(|registry, _| registry.prepare_drop(&name), |_| (), reply)
            }
            Command::CreateView {
                group,
                key,
                visibility,
                metadata,
                reply,
            } => self.commit_view_create(group, key, visibility, metadata, reply),
            Command::DropView { group, key, reply } => self.commit_view_drop(group, key, reply),
            Command::DeclareAttribute { request, reply } => {
                self.commit_attribute_declare(*request, reply)
            }
            Command::DeclareVocabulary { request, reply } => {
                self.commit_vocabulary_declare(*request, reply)
            }
            Command::MintVocabularyValues {
                vocabulary,
                values,
                reply,
            } => self.commit_vocabulary_values(vocabulary, values, reply),
            Command::CreateViewGroup { declaration, reply } => {
                self.commit_view_group_create(*declaration, reply)
            }
            Command::CreatePlainView { declaration, reply } => {
                self.commit_plain_view_create(*declaration, reply)
            }
            Command::PublishArtifacts {
                layer,
                level,
                mut artifacts,
                stamp,
                reply,
            } => {
                if !self.follow_since(stamp, |generation| {
                    crate::edited::follow_artifacts(generation, &mut artifacts)
                }) {
                    reply.fail(ExecError::Stale);
                    return;
                }
                self.commit_artifacts(layer, level, artifacts, reply)
            }
            Command::GrowMemberships {
                layer,
                level,
                mut joins,
                stamp,
                reply,
            } => {
                if !self.follow_since(stamp, |generation| {
                    crate::edited::follow_growth(generation, &mut joins)
                }) {
                    reply.fail(ExecError::Stale);
                    return;
                }
                self.commit_growth(layer, level, joins, reply)
            }
        }
    }

    /// Validate, allocate, append, sync, apply: `commit_registry`'s sequence, for the same reason
    /// and with one addition: the record lands in two structures, the registry (for a level
    /// that grew) and the store (for the memberships themselves), and both are applied under the
    /// one lock the preparation was made under.
    ///
    /// Nothing is applied before the record is durable. A membership applied and then lost is an
    /// artifact whose `mosaica_id` a caller already holds and whose members come back empty,
    /// served as absent, indistinguishable from one that failed its criterion. So the append comes
    /// first, and a failure means the batch does not exist.
    pub(super) fn commit_artifacts(
        &mut self,
        layer: String,
        level: u32,
        mut incoming: Vec<IncomingArtifact>,
        reply: Reply<PublishedBatch>,
    ) {
        // Read before the record is applied, because it is what says a held row form is the form
        // this publication follows: see [`Self::bring_artifacts_forward`].
        // Partitioned before any ordinal is claimed: a key the level holds is compared under the
        // fill rule and resolves to its existing ordinal, and only the keys it does not hold are
        // published. The answer is up to three kinds of record, in the order they are appended
        // and applied.
        //
        // The complement is taken here and nowhere else: a membership spelled by exclusion is
        // materialised on the executor, against the view's entity set as it stands at this step,
        // before the record is written, so the log, the store and every read path carry the
        // inclusion the other spelling would have produced. No serving path evaluates a complement
        // against a viewer's mask, which would disclose the existence of items outside it.
        //
        // The held-key refusal is taken first: an exclusion on a key the level holds is a `409`
        // ([`RegistryError::ExclusionOnHeldKey`], which `prepare_put` makes below over the same
        // store), and the walk of the view's entities is the most expensive thing this route does,
        // so the refusal spends nothing, as every other refusal on this path does not.
        if let Err(e) = self.materialise_exclusions(&layer, level, &mut incoming) {
            reply.fail(e);
            return;
        }
        let served = self.generation.load_full();
        let views = crate::write::ServedViews(&served.bundle.manifest);
        let prepared = self.live.with_publication_state(|registry, store, alloc| {
            registry.prepare_put(&layer, level, &incoming, store, alloc, &views)
        });
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(e) => {
                reply.fail(refusal_of(e));
                return;
            }
        };
        let records: Vec<&WalRecord> = prepared
            .publish
            .iter()
            .chain(prepared.fills.iter())
            .chain(prepared.growth.iter())
            .collect();
        if records.is_empty() {
            // Every key was held and every part identical: nothing to append, on
            // `commit_growth`'s no-op rule, and the acknowledgement is the held artifacts' own.
            reply.ack(PublishedBatch {
                entities: prepared.entities,
                created: 0,
                without_content: 0,
                filled: 0,
                joined: 0,
            });
            return;
        }

        let positions = match self.make_durable(&records, "an artifact publication") {
            Ok(positions) => positions,
            Err(e) => {
                reply.fail(e);
                return;
            }
        };
        // Promptly: an operator's publication route, one request at a time, and a restore that
        // needs no log for what it just published.
        self.apply_artifact_records(&records, &positions, Publish::Promptly);
        reply.ack(PublishedBatch {
            entities: prepared.entities,
            created: prepared.created,
            without_content: prepared.without_content,
            filled: prepared.fills.len() as u64,
            joined: prepared.joined,
        });
    }

    /// Materialise every membership this batch spelled by exclusion, and answer the refusal where
    /// the layer cannot be read.
    ///
    /// The view's entity set is every entity holding a row in it or buffered for it, deleted
    /// entities excluded, and the membership is one `andnot` of the caller's list over it. On a
    /// group-scoped layer the view is the artifact's own; on an entity-scoped one it is the union
    /// of the views the layer is drawn on, which is the layer's whole corpus and the set the
    /// build complements against (`layers.rs::resolve_artifact`, `0..high_water`).
    ///
    /// Two divergences from the build's byte-identity are structural and stated rather than
    /// closed: an entity ingested after this step is in the inclusion spelling's membership and
    /// not in the exclusion's, and a suppressed entity is in both, suppression not being deletion.
    ///
    /// The complement's size is logged, and the acknowledgement's `joined` counts it among the
    /// memberships the batch added.
    pub(super) fn materialise_exclusions(
        &self,
        layer: &str,
        level: u32,
        incoming: &mut [IncomingArtifact],
    ) -> Result<(), ExecError> {
        if !incoming.iter().any(|a| a.excluding.is_some()) {
            return Ok(());
        }
        let Some(registered) = self.live.registered_layer(layer) else {
            // The registry refuses the unknown layer a statement later, in its own words.
            return Ok(());
        };
        // The `409` before the walk. The rule is the registry's and `prepare_put` states it over
        // the same store index a moment later; what is here is the order, so that a repeat of a
        // publication the level already holds costs a key lookup rather than a view's rows.
        let held = self.live.with_artifacts(|store| {
            incoming
                .iter()
                .filter(|artifact| artifact.excluding.is_some())
                .filter_map(|artifact| Some((artifact, artifact.key.as_deref()?)))
                .find(|(artifact, key)| {
                    store
                        .ordinal_of_key(layer, level, artifact.view.as_deref(), key)
                        .is_some()
                })
                .map(|(_, key)| key.to_string())
        });
        if let Some(key) = held {
            return Err(refusal_of(
                mosaica_lifecycle::RegistryError::ExclusionOnHeldKey {
                    layer: layer.to_string(),
                    level,
                    key,
                },
            ));
        };
        let generation = self.generation.load_full();
        let mut sets: std::collections::HashMap<Option<String>, croaring::Bitmap> =
            std::collections::HashMap::new();
        for artifact in incoming.iter_mut() {
            let Some(excluded) = artifact.excluding.as_ref().map(|e| e.cardinality()) else {
                continue;
            };
            let view = artifact.view.clone();
            let entities = sets.entry(view.clone()).or_insert_with(|| {
                // The artifact names a view's key and the generation holds view ids (`quarter:q1`),
                // so the key is resolved against the layer's own declared views rather than used
                // as an id directly, which would match nothing and make every group-scoped
                // complement empty.
                let views: Vec<String> = match &view {
                    Some(key) => registered
                        .declaration
                        .views
                        .iter()
                        .filter(|id| crate::artifacts::view_key(id) == key)
                        .cloned()
                        .collect(),
                    None => registered.declaration.views.clone(),
                };
                view_entities(&generation, &views)
            });
            let started = std::time::Instant::now();
            let members = artifact
                .complement_against(entities)
                .expect("the artifact carries an exclusion");
            tracing::info!(
                layer = %layer,
                view = ?view,
                excluded,
                in_view = entities.cardinality(),
                members,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "a membership spelled by exclusion was complemented against the view's entities"
            );
        }
        Ok(())
    }

    /// Grow the memberships of artifacts that already exist: `commit_artifacts`'s sequence
    /// (validate, append, sync, apply) with nothing allocated, because a join takes no ordinal and
    /// no entity.
    ///
    /// This is the second way state enters the artifact store, and the first that is not a whole
    /// record. It is a growth path, which is why it is admissible at all: a membership's removal
    /// rules govern how a bit leaves it, and this adds bits that are then retired by exactly the
    /// routes every other member is retired by; `ArtifactStore::grow` carries the argument in
    /// full, and there is one such method rather than one per caller.
    ///
    /// Nothing is applied before the record is durable, on `commit_artifacts`'s reason, one step
    /// sharper: a join applied and then lost is an artifact that comes back from a restart without
    /// the point, which nothing distinguishes from an artifact that failed its existence
    /// criterion. The same failure is what the pin `ArtifactStore::mark_growth_packed` releases
    /// exists against, on the packing side.
    ///
    /// This is the control plane's route into growth, and it is not the only one. An ingest batch
    /// carrying a column named for a layer grows the same memberships through
    /// `Executor::close_window` instead: resolved at admission, appended inside the window's own
    /// fsync, and applied through the same `ArtifactStore::grow` this command reaches. The two
    /// share the record and the store method rather than the command, because a batch's entities
    /// do not exist until its window allocates and a command cannot wait inside one.
    ///
    /// Minting is that close's and not this command's. An unknown key here is refused whatever the
    /// layer's value set says: this route names an artifact to add members to, where a membership
    /// column names the artifact a point belongs to and may therefore create it. Where the two do
    /// agree is the thread: a mint claims ordinals serially on this executor, exactly as
    /// `commit_artifacts` does, which is why neither claim is made in a handler.
    pub(super) fn commit_growth(
        &mut self,
        layer: String,
        level: u32,
        joins: Vec<mosaica_lifecycle::IncomingGrowth>,
        reply: Reply<Vec<mosaica_lifecycle::MembershipGrown>>,
    ) {
        // `commit_artifacts`' reason: the version a held row form must be at for this delta to be
        // the one it is missing.
        // The receipt is read beside the preparation, under the same lock and before the
        // record is applied: afterwards every joining member is a member, and how many were new
        // is gone.
        let prepared = self.live.with_publication_state(|registry, store, _| {
            let prepared = registry.prepare_grow(&layer, level, &joins, store)?;
            let grown = growth_receipt(registry, store, &layer, level, &joins, &prepared);
            Ok::<_, mosaica_lifecycle::RegistryError>((prepared, grown))
        });
        let (prepared, grown) = match prepared {
            Ok(prepared) => prepared,
            Err(e) => {
                reply.fail(refusal_of(e));
                return;
            }
        };
        // The fills first, then the growth: the order the records are applied in, and the order a
        // held form takes them in below.
        let records: Vec<&WalRecord> = prepared
            .fills
            .iter()
            .chain(prepared.growth.iter())
            .collect();
        if records.is_empty() {
            // Every key resolved, nothing was joining and every part was held identically. No
            // record is owed for a no-op, and appending an empty one would pin the log at a
            // growth that changed nothing.
            reply.ack(grown);
            return;
        }

        let positions = match self.make_durable(&records, "a membership growth") {
            Ok(positions) => positions,
            Err(e) => {
                reply.fail(e);
                return;
            }
        };
        // Promptly, on `commit_artifacts`' rule: this is the operator's own growth route.
        self.apply_artifact_records(&records, &positions, Publish::Promptly);
        reply.ack(grown);
    }

    /// Validate, allocate, append, sync, apply: in that order, which is the whole of the
    /// registry's durability contract.
    ///
    /// Nothing is applied before the record is durable, and this is the opposite posture from a
    /// deny. A suppression is applied to the live overlay even when its append fails, because
    /// leaving an accepted deny unapplied is a fail-open and "in force but not durable" is the
    /// safer of two bad states. A registration has no such asymmetry: a layer that exists in memory
    /// and not in the log comes back from a restart as a name that is free again, having already
    /// handed a caller a `mosaica_id` for its entity. So the append comes first and a failure means
    /// the layer does not exist: which is what the caller is told.
    pub(super) fn commit_registry<T>(
        &mut self,
        prepare: impl FnOnce(
            &mut LayerRegistry,
            &mut Allocator,
        )
            -> std::result::Result<WalRecord, mosaica_lifecycle::RegistryError>,
        ack_of: impl FnOnce(&WalRecord) -> T,
        reply: Reply<T>,
    ) {
        let prepared = self.live.with_registry_and_allocator(prepare);
        let record = match prepared {
            Ok(record) => record,
            Err(e) => {
                reply.fail(ExecError::LayerRefused {
                    detail: e.to_string(),
                });
                return;
            }
        };

        if let Err(e) = self.make_durable(&[&record], "a layer registration") {
            reply.fail(e);
            return;
        }

        let ack = ack_of(&record);
        self.live.apply_registry_record(&record);
        // A dropped layer's derived structures go with it. Retention only, never correctness: a
        // tombstoned name never resolves through the registry again, so nothing held here is
        // reachable to be served.
        //
        // The store's own copy of a dropped layer's memberships is not released here, because
        // `ArtifactStore::remove_layer` is reached from nowhere: a drop touches the registry and
        // stops there. Releasing it changes what the next fold repacks and how far back the
        // rotation pin holds the log.
        if let WalRecord::LayerDrop { name, .. } = &record {
            // The deltas held for the tick describe forms that are going with the layer.
            self.pending_forms.retain(|(layer, _), _| layer != name);
            self.deps.artifact_projections.forget(name);
            self.deps.lineages.forget(name);
            self.deps.level_contents.forget(name);
        }
        // The registry is durable in the log but not yet in a manifest, and a rotation reclaims the
        // log. Marking the manifest dirty is what gets it published at the next flush, on the same
        // mechanism a deny uses to reach `SEGMENTS-<n>.json`.
        self.side_manifests.behind_live = true;
        reply.ack(ack);
    }

    /// `PUT /control/views/{group}/{key}`: create a view of a group while the service runs.
    ///
    /// The shape is `commit_registry`'s, because the obligation is: prepare against state only
    /// this thread may write, append, fsync, apply, publish, ack. What differs is that a view has
    /// a row space, an empty one, so the apply reaches the bundle rather than stopping at a
    /// live-state map, and the ack therefore rides a generation swap rather than a registry token.
    ///
    /// The ordinal is spent whatever happens next. A create whose append fails is refused with
    /// its ordinal unreturned, exactly as a failed registration keeps its ids: an ordinal reissued
    /// after a torn append that replay might still apply is two views under one alias, which is
    /// worse than a gap in a sequence nothing counts.
    pub(super) fn commit_view_create(
        &mut self,
        group: String,
        key: String,
        visibility: Option<Vec<String>>,
        metadata: std::collections::BTreeMap<String, mosaica_types::view::ViewMetadataValue>,
        reply: Reply<()>,
    ) {
        let started = std::time::Instant::now();
        let generation = self.generation.load_full();
        let Some(descriptor) = generation
            .bundle
            .manifest
            .groups
            .iter()
            .find(|g| g.name == group)
        else {
            // The same 404 an unknown view id is, and for the same reason: a group nobody declared
            // and a key no view holds must be one answer, or the difference between them is an
            // existence oracle over the roster.
            reply.fail(ExecError::ViewUnknown {
                detail: format!(
                    "unknown view group '{group}': a group is declared at a build and its views \
                     grow at a running service; there is no create that mints a group"
                ),
            });
            return;
        };
        let facts = mosaica_lifecycle::GroupFacts {
            name: &descriptor.name,
            members_of: descriptor.members_of.as_deref(),
            metadata: &descriptor.metadata,
        };
        let prepared = self
            .live
            .with_roster(|roster| roster.prepare_create(facts, &key, visibility, metadata));
        let record = match prepared {
            Ok(record) => record,
            Err(e) => {
                reply.fail(roster_error(e));
                return;
            }
        };
        if let Err(e) = self.make_durable(&[&record], "a view creation") {
            reply.fail(e);
            return;
        }
        self.live.with_roster(|roster| roster.apply(&record));
        self.publish_roster(&generation, started, &[]);
        // Durable in the log and not yet in a manifest, and a rotation reclaims the log: so the
        // roster reaches `SEGMENTS-<n>.json` on the mechanism a deny already uses.
        self.side_manifests.behind_live = true;
        reply.ack(());
    }

    /// `PUT /control/attributes`: declare an attribute column while the service runs.
    ///
    /// The shape is [`Self::commit_view_create`]'s: resolve against state only this thread may
    /// write, append, fsync, apply, publish, ack. The apply reaches the bundle, because the served
    /// schema is the manifest's `declared_scalars` and every reader takes it from there: the
    /// successor generation carries the column at the tail of that list, the filter columns hold
    /// an empty stack for it so the next flush's extent composes onto something, and a vocabulary
    /// no column named before is narrowed to the width this column stores.
    ///
    /// Ingestable at the ack. A batch decoded against the successor's schema carries the column;
    /// one decoded against the predecessor's is shorter by one and is padded with the column's
    /// absence at its window's close (`crate::attributes::pad_to_schema`). The column is listed on
    /// `/v1/meta` from the swap and absent for every entity until a row fills it.
    ///
    /// An identical redeclaration answers the existing identity with nothing appended; a
    /// differing one is a conflict. A failed append means the column does not exist, on the layer
    /// registration's rule.
    pub(super) fn commit_attribute_declare(
        &mut self,
        request: mosaica_lifecycle::AttributeRequest,
        reply: Reply<bool>,
    ) {
        let started = std::time::Instant::now();
        let generation = self.generation.load_full();
        let resolved = crate::attributes::resolve(&request, &generation.bundle.manifest, |name| {
            self.live.registered_layer(name).is_some()
        });
        let compiled = match resolved {
            Ok(crate::attributes::Resolution::Existing) => {
                reply.ack(true);
                return;
            }
            Ok(crate::attributes::Resolution::New(compiled)) => compiled,
            Ok(crate::attributes::Resolution::Unique { name, unique }) => {
                self.begin_unique_declare(name, unique, reply);
                return;
            }
            Err(e) => {
                reply.fail(e);
                return;
            }
        };
        let (entity, scoped) = match &compiled {
            crate::attributes::CompiledAttribute::Entity(d) => (vec![d.clone()], Vec::new()),
            crate::attributes::CompiledAttribute::Scoped(f) => (Vec::new(), vec![f.clone()]),
        };
        let manifest = generation.bundle.manifest.with_attributes(&entity, &scoped);
        // An entity-scoped column takes an empty stack in the filter columns, at its position in
        // the served list; a scoped family's per-view columns are opened by the first flush that
        // writes one, as a family declared at the build is for a view created since. Built before
        // the append, so a column this process cannot hold is refused with nothing written.
        let filter_columns = match &compiled {
            crate::attributes::CompiledAttribute::Entity(d) => {
                let declared_index = manifest.declared_scalars.len() - 1;
                match generation.filter_columns.with_runtime_column(
                    d,
                    declared_index,
                    &manifest.vocabularies,
                ) {
                    Ok(columns) => Arc::new(columns),
                    Err(e) => {
                        reply.fail(ExecError::AttributeRefused {
                            detail: format!("attribute '{}': {e}", request.name),
                        });
                        return;
                    }
                }
            }
            crate::attributes::CompiledAttribute::Scoped(_) => {
                Arc::clone(&generation.filter_columns)
            }
        };
        let record = WalRecord::AttributeDeclare {
            declaration: Box::new(compiled.declaration(request.title.clone())),
        };
        if let Err(e) = self.make_durable(&[&record], "an attribute declaration") {
            reply.fail(e);
            return;
        }

        // The apply: the live list first, then the successor generation built from it.
        self.live
            .with_attributes(|attributes| attributes.push(compiled.clone()));
        let vocabularies = Arc::clone(&generation.vocabularies);
        // A category over a vocabulary no column named before has no suggestion index, the open
        // building one only for the vocabularies a column names; built here, on this thread, as
        // the open builds it, so the suggest verb answers the column from the acknowledgement
        // rather than from the next restart. A build that fails is omitted and warned, on
        // `SuggestIndexes::build`'s rule: the suggest verb refuses the column and nothing else is
        // affected.
        let suggest = match compiled.category() {
            Some((vocabulary, _)) if generation.suggest.get(vocabulary).is_none() => {
                let built = crate::suggest::SuggestIndexes::build(
                    &self.deps.suggest_dir,
                    &vocabularies,
                    [vocabulary.to_string()],
                    &self.deps.pool,
                );
                Arc::new(generation.suggest.with_built(built))
            }
            _ => Arc::clone(&generation.suggest),
        };
        let bundle = generation.bundle.with_views(manifest);
        let next = generation.with(|g| {
            g.bundle = bundle;
            g.vocabularies = vocabularies;
            g.filter_columns = filter_columns;
            g.suggest = suggest;
        });
        self.publish(next, started);
        // Durable in the log and not yet in a manifest, and a rotation reclaims the log: the
        // declaration reaches `SEGMENTS-<n>.json` on the mechanism a deny already uses.
        self.side_manifests.behind_live = true;
        // A new unique column holds no value, so its index starts empty.
        if let crate::attributes::CompiledAttribute::Entity(d) = &compiled {
            if d.unique {
                self.publish_new_unique_index(&d.name, started);
            }
        }
        reply.ack(false);
    }

    /// `PUT /control/view_groups/{name}`: declare a view group while the service runs.
    ///
    /// The shape is [`Self::commit_attribute_declare`]'s: resolve against state only this
    /// thread may write, append, fsync, apply, publish, ack. The apply reaches the bundle,
    /// because the served roster is the manifest's `groups` and every reader takes it from there:
    /// the successor generation carries the group with an empty roster, and a view of it may be
    /// created in the next request.
    ///
    /// An identical redeclaration answers the group that exists; a differing one is a conflict. A
    /// failed append means the group does not exist.
    pub(super) fn commit_view_group_create(
        &mut self,
        declaration: mosaica_lifecycle::wal::ViewGroupDeclaration,
        reply: Reply<bool>,
    ) {
        let started = std::time::Instant::now();
        let generation = self.generation.load_full();
        let compiled = match crate::view_declarations::resolve_group(
            &declaration,
            &generation.bundle.manifest,
        ) {
            Ok(crate::view_declarations::Resolution::Existing) => {
                reply.ack(true);
                return;
            }
            Ok(crate::view_declarations::Resolution::New(compiled)) => *compiled,
            Err(e) => {
                reply.fail(e);
                return;
            }
        };
        let record = WalRecord::ViewGroupCreate {
            declaration: Box::new(declaration),
        };
        if let Err(e) = self.make_durable(&[&record], "a view group declaration") {
            reply.fail(e);
            return;
        }
        self.live
            .with_view_declarations(|declarations| declarations.push_group(compiled.clone()));
        let manifest = generation
            .bundle
            .manifest
            .with_groups(std::slice::from_ref(&compiled));
        self.publish_view_manifest(&generation, manifest, started);
        // Durable in the log and not yet in a manifest, and a rotation reclaims the log: the
        // declaration reaches `SEGMENTS-<n>.json` on the mechanism a deny already uses.
        self.side_manifests.behind_live = true;
        reply.ack(false);
    }

    /// `PUT /control/views/{name}`: create a plain view while the service runs.
    ///
    /// The shape is [`Self::commit_view_group_create`]'s, and what differs is that a plain view
    /// has a row space, an empty one until its first flush, so the apply goes through
    /// `Bundle::with_views`, which is what gives a view created at a running service its place in
    /// the per-view map. A view absent from that map is read as an unknown view by the viewport
    /// and as a disagreement between the mask and the bundle by the deny mask.
    pub(super) fn commit_plain_view_create(
        &mut self,
        declaration: mosaica_lifecycle::wal::PlainViewDeclaration,
        reply: Reply<bool>,
    ) {
        let started = std::time::Instant::now();
        let generation = self.generation.load_full();
        let compiled = match crate::view_declarations::resolve_plain(
            &declaration,
            &generation.bundle.manifest,
        ) {
            Ok(crate::view_declarations::Resolution::Existing) => {
                reply.ack(true);
                return;
            }
            Ok(crate::view_declarations::Resolution::New(compiled)) => *compiled,
            Err(e) => {
                reply.fail(e);
                return;
            }
        };
        let record = WalRecord::PlainViewCreate {
            declaration: Box::new(declaration),
        };
        if let Err(e) = self.make_durable(&[&record], "a plain view creation") {
            reply.fail(e);
            return;
        }
        self.live
            .with_view_declarations(|declarations| declarations.push_plain(compiled.clone()));
        let manifest = generation
            .bundle
            .manifest
            .with_plain_views(std::slice::from_ref(&compiled));
        self.publish_view_manifest(&generation, manifest, started);
        self.side_manifests.behind_live = true;
        reply.ack(false);
    }

    /// Publish a generation carrying `manifest` and nothing else moved: the swap a group
    /// declaration and a plain view creation both make.
    ///
    /// `Bundle::with_views`, which brings the per-view map into step with the manifest: a view
    /// the manifest declares and the map does not is an unknown view to the viewport, and a
    /// created view gains an empty row space. `segments_version` and the watermark are unmoved,
    /// on `publish_roster`'s argument: no row moved.
    pub(super) fn publish_view_manifest(
        &mut self,
        generation: &Arc<Generation>,
        manifest: mosaica_store::manifest::Manifest,
        started: std::time::Instant,
    ) {
        let bundle = generation.bundle.with_views(manifest);
        let next = generation.with(|g| {
            g.bundle = bundle;
        });
        self.publish(next, started)
    }

    /// `PUT /control/vocabularies/{name}`: declare a vocabulary while the service runs.
    ///
    /// The shape is [`Self::commit_attribute_declare`]'s: resolve against state only this
    /// thread may write, draw the codes, append, fsync, apply, publish, ack. The apply reaches
    /// the bundle, because the served vocabulary table is the manifest's `vocabularies` and every
    /// reader takes it from there: the successor generation carries the vocabulary, and its
    /// minter holds the values the declaration named.
    ///
    /// Usable at the ack. A `declared` category column may name the vocabulary in the next
    /// request, and a row may carry a value it holds; a key it does not hold is refused under the
    /// declare-then-use rule, unchanged.
    ///
    /// An identical redeclaration answers the vocabulary that exists and applies the request's
    /// values as a page; a differing one is a conflict. A failed append means the vocabulary does
    /// not exist and no code was spent.
    pub(super) fn commit_vocabulary_declare(
        &mut self,
        request: mosaica_lifecycle::VocabularyRequest,
        reply: Reply<VocabularyDeclared>,
    ) {
        let started = std::time::Instant::now();
        let generation = self.generation.load_full();
        let compiled = match crate::vocabularies::resolve(&request, &generation.bundle.manifest) {
            Ok(crate::vocabularies::Resolution::Existing) => {
                // A redeclaration is the same vocabulary, and its values are a page against it.
                self.commit_vocabulary_page(request.name, request.values, reply, |page| {
                    VocabularyDeclared {
                        existing: true,
                        added: page.added,
                        titles: page.titles,
                    }
                });
                return;
            }
            Ok(crate::vocabularies::Resolution::New(compiled)) => *compiled,
            Err(e) => {
                reply.fail(e);
                return;
            }
        };
        // The codes, drawn into a minter this thread owns and nothing has published. A draw that
        // exhausts the width refuses with nothing appended and no binding anywhere.
        let mut minter = match mosaica_store::vocabulary::VocabularyMinter::declared(
            compiled.name.clone(),
            compiled.kind,
            compiled.visibility,
            compiled.width,
            &compiled.reserved,
            [],
            request.values.iter().map(|v| v.key.as_str()),
        ) {
            Ok(minter) => minter,
            Err(e) => {
                reply.fail(ExecError::VocabularyRefused {
                    detail: e.to_string(),
                });
                return;
            }
        };
        let mut codes = Vec::with_capacity(request.values.len());
        for value in &request.values {
            if let Some(code) = minter.code_of(&value.key) {
                codes.push((value.key.clone(), code));
            }
            if let Some(title) = &value.title {
                minter.set_title(&value.key, title.clone());
            }
        }
        let record = WalRecord::VocabularyDeclare {
            declaration: Box::new(crate::vocabularies::declaration_record(
                &request, &compiled, &codes,
            )),
        };
        if let Err(e) = self.make_durable(&[&record], "a vocabulary declaration") {
            reply.fail(e);
            return;
        }

        // The apply: the live list first, then the successor generation built from it.
        let added = codes.len() as u64;
        self.live
            .with_vocabularies(|vocabularies| vocabularies.push(compiled.clone()));
        let mut vocabularies: Vocabularies = (*generation.vocabularies).clone();
        vocabularies.insert(minter);
        let manifest = generation
            .bundle
            .manifest
            .with_vocabularies(std::slice::from_ref(&compiled));
        let bundle = generation.bundle.with_views(manifest);
        let next = generation.with(|g| {
            g.bundle = bundle;
            g.vocabularies = Arc::new(vocabularies);
        });
        self.publish(next, started);
        // Durable in the log and not yet in a manifest, and a rotation reclaims the log: the
        // declaration reaches `SEGMENTS-<n>.json` on the mechanism a deny already uses.
        self.side_manifests.behind_live = true;
        reply.ack(VocabularyDeclared {
            existing: false,
            added,
            // A new vocabulary holds no value whose title could be replaced: every title it
            // carries arrived with the value that drew its code.
            titles: 0,
        });
    }

    /// `PATCH /control/vocabularies/{name}/values`: a page of values for a vocabulary that
    /// exists.
    pub(super) fn commit_vocabulary_values(
        &mut self,
        vocabulary: String,
        values: Vec<mosaica_lifecycle::DeclaredValue>,
        reply: Reply<VocabularyValues>,
    ) {
        self.commit_vocabulary_page(vocabulary, values, reply, std::convert::identity);
    }

    /// One page of values, whether it arrived on the values route or as the inline values of a
    /// redeclaration.
    ///
    /// Every value of the page is checked before any code is drawn, so a refused page binds
    /// nothing and a caller's corrected retry means what they think it means. The page is one
    /// append and one fsync: a `VocabularyDeclare` record carrying the page's values with the
    /// codes drawn for them, because a value's title is part of what the page acknowledges and a
    /// `VocabularyMint` record carries none.
    ///
    /// `answer` turns what the page did into the answer the command it arrived on is owed: a
    /// values page's is that unchanged, and a redeclaration's is a [`VocabularyDeclared`] carrying
    /// the same two counts.
    ///
    /// A title supplied for a held key replaces the held title and is counted into the
    /// acknowledgement. The key-to-code binding does not move, so a row already carrying the code
    /// means what it meant; what changes is the name a client draws.
    pub(super) fn commit_vocabulary_page<T>(
        &mut self,
        vocabulary: String,
        values: Vec<mosaica_lifecycle::DeclaredValue>,
        reply: Reply<T>,
        answer: impl Fn(VocabularyValues) -> T,
    ) {
        let started = std::time::Instant::now();
        let generation = self.generation.load_full();
        let Some(held) = generation.vocabularies.get(&vocabulary) else {
            // The same 404 an unknown view is, and for the same reason: a vocabulary nobody
            // declared and one this deployment does not carry are one answer.
            reply.fail(ExecError::ViewUnknown {
                detail: format!(
                    "unknown vocabulary '{vocabulary}': a vocabulary is declared at a build or by \
                     `PUT /control/vocabularies/{{name}}`; a page of values does not create one, \
                     because the value set's width and visibility are the declaration's to state"
                ),
            });
            return;
        };
        let titles = match crate::vocabularies::check_page(held, &vocabulary, &values) {
            Ok(titles) => titles,
            Err(e) => {
                reply.fail(e);
                return;
            }
        };
        let mut minter = held.clone();
        let mut codes = Vec::with_capacity(values.len());
        let mut added = 0u64;
        let mut existing = 0u64;
        for value in &values {
            match minter.mint(&value.key) {
                Ok(mosaica_store::vocabulary::Minted::Fresh(code)) => {
                    added += 1;
                    codes.push((value.key.clone(), code));
                }
                Ok(mosaica_store::vocabulary::Minted::Existing(code)) => {
                    existing += 1;
                    codes.push((value.key.clone(), code));
                }
                Err(e) => {
                    reply.fail(ExecError::VocabularyRefused {
                        detail: e.to_string(),
                    });
                    return;
                }
            }
            if let Some(title) = &value.title {
                minter.set_title(&value.key, title.clone());
            }
        }
        // Nothing to append where the page bound nothing and changed no title. A repeat of a page
        // already applied is a no-op, and an fsync for it would be a durable record of a decision
        // nothing made. `titles` counts the held keys whose title this page changes, so a page
        // restating the titles a deployment holds appends nothing.
        if added == 0 && titles == 0 {
            reply.ack(answer(VocabularyValues {
                added,
                existing,
                titles: 0,
            }));
            return;
        }
        let declaration = mosaica_lifecycle::wal::VocabularyDeclaration {
            name: vocabulary.clone(),
            title: None,
            kind: minter.kind(),
            visibility: minter.visibility(),
            width: minter.width().arrow_type_name().to_string(),
            values: codes
                .iter()
                .map(
                    |(key, code)| mosaica_lifecycle::wal::DeclaredVocabularyValue {
                        key: key.clone(),
                        code: Some(*code),
                        title: minter.title_of(key).map(str::to_string),
                    },
                )
                .collect(),
            reserved: Vec::new(),
        };
        let record = WalRecord::VocabularyDeclare {
            declaration: Box::new(declaration),
        };
        if let Err(e) = self.make_durable(&[&record], "a page of vocabulary values") {
            reply.fail(e);
            return;
        }
        let mut vocabularies: Vocabularies = (*generation.vocabularies).clone();
        vocabularies.insert(minter);
        let next = generation.with(|g| {
            g.vocabularies = Arc::new(vocabularies);
        });
        self.publish(next, started);
        // A binding of a *built* vocabulary reaches the manifest as a `vocabulary_extensions`
        // entry and one of a runtime-declared vocabulary as a value of its own runtime entry;
        // both are written at the next side-manifest publication, which this marks due.
        self.side_manifests.behind_live = true;
        reply.ack(answer(VocabularyValues {
            added,
            existing,
            titles,
        }));
    }

    /// `DELETE /control/views/{group}/{key}`: drop a view, freeing its key and killing its
    /// incarnation, and delete every item the drop leaves with a row in no view.
    ///
    /// The deletions are ordinary ones: they enter the overlay and retire at the fold that
    /// executes them. They travel in the drop's own record, so no log holds the drop without them,
    /// and the probe that finds them runs on this thread with no yield before the append: a batch
    /// acked between them could give an item a row elsewhere, and the deletion would then destroy
    /// a row the caller was told had landed.
    pub(super) fn commit_view_drop(
        &mut self,
        group: String,
        key: String,
        reply: Reply<ViewDropped>,
    ) {
        let started = std::time::Instant::now();
        let generation = self.generation.load_full();
        // The owner's key, whatever group the request named: a key belongs to the group that owns
        // the views, and dropping the key takes the view out of every group sharing them.
        let owner = generation.bundle.manifest.owner_of_group(&group);
        // Every id the key resolves to, which is what a drop takes away: the owner's view and
        // every sharing group's. Built from the owner rather than from the group the request
        // named, and used by the probe and the buffer prune alike. A prune over the requested
        // spelling alone would leave the other's buffered rows to be flushed into whatever takes
        // the key next.
        let ids = generation.bundle.manifest.view_ids_for_key(&owner, &key);
        // Computed before the drop applies, because the probe reads the row space the drop is
        // about to take away.
        let dangling = dangling_entities(&generation, &ids);
        let prepared = self
            .live
            .with_roster(|roster| roster.prepare_drop(&owner, &key, dangling.clone()));
        let record = match prepared {
            Ok(record) => record,
            Err(e) => {
                reply.fail(roster_error(e));
                return;
            }
        };
        if let Err(e) = self.make_durable(&[&record], "a view drop") {
            reply.fail(e);
            return;
        }
        self.live.with_roster(|roster| roster.apply(&record));
        self.publish_roster(&generation, started, &ids);
        let served = self.generation.load_full();
        let retired = self.live.with_publication_state(|registry, store, _| {
            crate::write::retire_dead_view_artifacts(registry, store, &served.bundle.manifest)
        });
        for (layer, _) in &retired {
            self.pending_forms.retain(|(held, _), _| held != layer);
            self.deps.artifact_projections.forget(layer);
            self.deps.lineages.forget(layer);
            self.deps.level_contents.forget(layer);
        }
        let deleted = dangling.len() as u64;
        if !dangling.is_empty() {
            self.apply_changes(
                dangling
                    .into_iter()
                    .map(|entity| (entity, mosaica_lifecycle::ChangeOp::Delete))
                    .collect(),
            );
        }
        self.side_manifests.behind_live = true;
        reply.ack(ViewDropped { deleted });
    }

    /// Publish the generation a create or a drop makes: the bundle as the live roster describes
    /// it, the deny mask re-derived over the views it now has, and every buffered row of a view
    /// that has gone.
    ///
    /// The buffered rows of a dropped view are discarded. They name a coordinate system that no
    /// longer exists, so nothing will ever give them geometry, and a row left in the buffer for a
    /// view no flush will plan pins `oldest_wal_pos`, and with it every WAL member after it, for
    /// the life of the process. An item left with no row anywhere is deleted by the drop itself
    /// ([`Self::commit_view_drop`]).
    ///
    /// The dropped views' group-scoped columns go with them, so a key created again opens its own
    /// base at its first flush rather than answering from its predecessor's.
    pub(super) fn publish_roster(
        &self,
        generation: &Arc<Generation>,
        started: std::time::Instant,
        dropped: &[String],
    ) {
        let (created, tombstones) = self.live.roster_for_publication();
        let manifest = generation
            .bundle
            .manifest
            .with_roster(&created, &tombstones);
        let bundle = generation.bundle.with_views(manifest);
        let buffer = if dropped.is_empty() {
            Arc::clone(&generation.buffer)
        } else {
            let mut buffer = (*generation.buffer).clone();
            buffer.remove_views(dropped);
            self.health
                .buffered_items
                .store(buffer.len(), Ordering::SeqCst);
            Arc::new(buffer)
        };
        let next = generation.with(|g| {
            g.bundle = bundle;
            g.buffer = buffer;
            if !dropped.is_empty() {
                g.filter_columns =
                    Arc::new(generation.filter_columns.without_scoped_columns(dropped));
            }
        });
        self.publish(next, started);
    }

}
