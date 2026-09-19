use super::*;

pub(super) fn views_of(generation: &Generation) -> Vec<String> {
    let mut views: Vec<String> = generation
        .bundle
        .partitions
        .values()
        .flat_map(|p| p.views.keys().cloned())
        .collect();
    views.sort_unstable();
    views.dedup();
    views
}

/// One view's row space in `bundle`, or `None` where the partition or the view is not there.
pub(super) fn view_row_space<'a>(
    bundle: &'a tessera_store::read::Bundle,
    partition: &str,
    view: &str,
) -> Option<&'a tessera_store::permutation::RowSpace> {
    bundle
        .partitions
        .get(partition)
        .and_then(|p| p.views.get(view))
        .map(|v| &v.row_space)
}

/// Write one membership extent per packed level into the prefix and fsync it, returning the
/// manifest entries. The file half of [`Executor::write_membership_extents`] and
/// [`Executor::rewrite_membership_extents`], which differ only in where `ready` comes from.
///
/// The layer name never reaches the filename, since a name-derived path could escape the
/// directory or collide; the manifest entry carries the name instead. The directory listing is
/// fsynced too, so a crash cannot leave a manifest naming a file that was never written.
pub(super) fn pack_membership_extents(
    prefix_dir: &std::path::Path,
    partition: &str,
    n: u64,
    ready: Vec<tessera_lifecycle::membership::PendingExtent>,
) -> tessera_store::Result<Vec<tessera_store::manifest::MembershipExtent>> {
    if ready.is_empty() {
        return Ok(Vec::new());
    }
    let dir = prefix_dir
        .join("partitions")
        .join(partition)
        .join("members");
    std::fs::create_dir_all(&dir).map_err(|source| tessera_store::StoreError::Io {
        path: dir.clone(),
        source,
    })?;
    let mut entries = Vec::with_capacity(ready.len());
    for (index, (layer, level, ordinal_lo, blobs)) in ready.into_iter().enumerate() {
        let name = format!("members-{n:06}-{index:03}.tsmb");
        let count = blobs.len() as u32;
        let bytes = tessera_store::membership::pack(ordinal_lo, &blobs);
        tessera_store::write_and_fsync(&dir.join(&name), &bytes)?;
        entries.push(tessera_store::manifest::MembershipExtent {
            path: format!("partitions/{partition}/members/{name}"),
            layer,
            level,
            ordinal_lo,
            count,
        });
    }
    tessera_store::fsync_dir(&dir)?;
    Ok(entries)
}

/// A record extent's files, resolved against the prefix directory that holds them. All three, and
/// any one missing is a refusal to open rather than "those entities have no record".
pub(super) fn record_extent_paths(
    prefix_dir: &std::path::Path,
    extent: &tessera_store::manifest::RecordExtent,
) -> tessera_filter::RecordExtentPaths {
    tessera_filter::RecordExtentPaths {
        blocks: prefix_dir.join(&extent.blocks),
        hasrow: prefix_dir.join(&extent.hasrow),
        directory: prefix_dir.join(&extent.directory),
    }
}

/// An entity→term extent's four files, resolved against the prefix directory that holds them.
pub(super) fn entity_terms_extent_paths(
    prefix_dir: &std::path::Path,
    extent: &tessera_store::manifest::EntityTermsExtent,
) -> tessera_store::EntityTermsExtentPaths {
    tessera_store::EntityTermsExtentPaths {
        hasrow: prefix_dir.join(&extent.hasrow),
        offsets: prefix_dir.join(&extent.offsets),
        terms: prefix_dir.join(&extent.terms),
        bases: prefix_dir.join(&extent.bases),
    }
}

/// A text extent's three files, resolved against the prefix directory that holds them, under the
/// column name a leaf resolves to.
pub(super) fn text_extent_paths(
    prefix_dir: &std::path::Path,
    extent: &tessera_store::manifest::TextExtent,
) -> crate::filter::TextExtentPaths {
    crate::filter::TextExtentPaths {
        column: crate::filter::extent_column_name(&extent.column, extent.view.as_deref()),
        dict_rel: extent.dict.clone(),
        dict: prefix_dir.join(&extent.dict),
        postings: prefix_dir.join(&extent.postings),
        presence: prefix_dir.join(&extent.presence),
    }
}

/// The reader this process holds open for the tier `rel` names, or `None` if it holds none.
///
/// The two lists are positional against each other: `tiers` was opened from `rels`, which is the
/// `deltas` list of the manifest they came from.
pub(super) fn held_tier(tiers: &[Arc<DeltaTier>], rels: &[String], rel: &str) -> Option<Arc<DeltaTier>> {
    tiers
        .iter()
        .zip(rels)
        .find(|(_, live_rel)| live_rel.as_str() == rel)
        .map(|(tier, _)| Arc::clone(tier))
}

/// A background job is outstanding while it runs, and until its completed unit has been drained
/// from its channel and published; see [`Executor::fold_outstanding`].
pub(super) fn outstanding(in_flight: &AtomicBool, completed_pending: &AtomicBool) -> bool {
    in_flight.load(Ordering::SeqCst) || completed_pending.load(Ordering::SeqCst)
}

/// Whether this layer's memberships are a **stored** set, the kind a record's delta describes.
///
/// A `spatial` layer's membership is the rows inside its shapes and an `attribute` layer's is the
/// rows carrying a value. Both are evaluated against the geometry, so neither takes anything from
/// a publication's or a growth's record.
pub(super) fn stored_membership(declaration: &tessera_types::layer::LayerDeclaration) -> bool {
    matches!(
        declaration.membership,
        tessera_types::layer::MembershipSource::Enumerated
    ) && declaration.shape.is_none()
}

/// A layer whose memberships are resolved from shapes: spatial, with a shape declared.
pub(in crate::write) fn spatial_membership(declaration: &tessera_types::layer::LayerDeclaration) -> bool {
    matches!(
        declaration.membership,
        tessera_types::layer::MembershipSource::Spatial
    ) && declaration.shape.is_some()
}

impl Executor {
    /// Whether a fold is **outstanding**: running, or completed and not yet published.
    ///
    /// A fold plans against a snapshot of the live manifest, so it excludes a merge or a coalesce
    /// on this boundary rather than on whether either is currently executing: a job completed but
    /// undrained is about to change the manifest, so a pass dispatched beside it is discarded at
    /// its rebase check.
    ///
    /// A suspension does not stall: [`Executor::run`] drains every completed job before any
    /// dispatcher runs, and [`Executor::wait_for_work`] treats a pending flag as a reason for the
    /// fast completion poll.
    pub(super) fn fold_outstanding(&self) -> bool {
        outstanding(&self.fold_in_flight, &self.health.fold_completed_pending)
    }

    /// Whether a merge is outstanding, on the boundary [`Executor::fold_outstanding`] uses,
    /// applied to the row-space merge.
    pub(super) fn merge_outstanding(&self) -> bool {
        outstanding(&self.merge_in_flight, &self.health.merge_completed_pending)
    }

    /// Whether a coalesce is outstanding, on the boundary [`Executor::fold_outstanding`] uses,
    /// applied to the entity-space coalesce.
    pub(super) fn coalesce_outstanding(&self) -> bool {
        outstanding(
            &self.coalesce_in_flight,
            &self.health.coalesce_completed_pending,
        )
    }

    /// Select and dispatch an entity-space coalesce, if one qualifies and none is outstanding.
    ///
    /// Checks that at most one is outstanding before building the plan, for the same reason a
    /// flush does: two passes would select overlapping windows, and the loser's manifest edit
    /// would no longer rebase after doing all of its IO.
    pub(super) fn dispatch_coalesce(&mut self, generation: &Arc<Generation>) {
        // Suspended until a fold is published, not merely until it stops running: see
        // [`Executor::fold_outstanding`] for why that later boundary is the one that matters. A
        // coalesce publishing under a fold would be orphaned by the flip and would be discarded at
        // its rebase check, so running it here is wasted work, not a hazard.
        if self.coalesce_policy.width < 2
            || !self.coalesce_enabled.load(Ordering::SeqCst)
            || self.coalesce_outstanding()
            || self.fold_outstanding()
            || !self.may_publish()
        {
            return;
        }
        // A poisoned or diverged node publishes no manifest, the same gate `publish_overlay_state`
        // uses: a coalesce's manifest carries the live deny state, and writing that from an
        // overlay with no durable record behind it would make a deny that returned 500 and was
        // never acknowledged permanent on every restore.
        if self.wal.is_poisoned() || self.health.overlay_diverged.load(Ordering::SeqCst) {
            return;
        }
        let Some((partition, partition_data)) = generation.bundle.partitions.iter().next() else {
            return;
        };
        if partition_data.stepped_down() {
            return;
        }
        let Some(plan) = crate::coalesce::plan_coalesce(
            partition,
            &partition_data.manifest,
            &generation.bundle.manifest.files,
            self.coalesce_policy,
            // The roster as this generation has it: a scoped column of an incarnation that is no
            // longer live is for the fold to reclaim, not for this pass to merge.
            &|view, incarnation| {
                generation
                    .bundle
                    .manifest
                    .is_live_incarnation(view, incarnation)
            },
        ) else {
            return;
        };

        self.coalesce_attempt += 1;
        let ctx = crate::coalesce::CoalesceContext {
            prefix_dir: self.prefix_dir(generation),
            prefix: generation.prefix.clone(),
            // The same never-reused shape a `seg_id` has, and for the same reason: two passes at
            // one `n` would otherwise write one path, and the second `File::create` truncates
            // files the first has memory-mapped.
            out_rel: format!(
                "partitions/{partition}/coalesced/coalesce-{}-{}",
                partition_data.segments_n, self.coalesce_attempt
            ),
        };

        self.coalesce_in_flight.store(true, Ordering::SeqCst);
        let in_flight = Arc::clone(&self.coalesce_in_flight);
        let health = Arc::clone(&self.health);
        let submit = self.coalesce_submit.clone();
        self.pool.spawn(move || {
            match crate::coalesce::execute_coalesce(plan, ctx) {
                Ok(completed) => {
                    // Set before the send, exactly as a flush's is; see
                    // `ExecutorHealth::flush_completed_pending` for the handshake's ordering.
                    health
                        .coalesce_completed_pending
                        .store(true, Ordering::SeqCst);
                    let _ = submit.send(completed);
                }
                Err(e) => {
                    // Nothing happened, retry next tick: the manifest is the only commit point,
                    // so a failure before it leaves orphan files nothing references and every
                    // consumed entry still stands.
                    health.coalesce_failures.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(
                        error = %e,
                        "an entity-space coalesce failed; the axes it would have bounded keep \
                         growing and it is retried at the next tick"
                    );
                }
            }
            in_flight.store(false, Ordering::SeqCst);
        });
    }

    /// Select and dispatch a row-space merge, if one qualifies and none is running.
    ///
    /// Gated exactly as a coalesce is, with the extra reason that this one moves geometry: a
    /// poisoned or diverged node writes no manifest, and a stepped-down partition publishes
    /// nothing. `plan_merge` checks the last condition itself, since it is bundle state rather
    /// than executor health.
    pub(super) fn dispatch_merge(&mut self, generation: &Arc<Generation>) {
        // Suspended until a fold is *published*, for the reason `dispatch_coalesce` states and on
        // the boundary `fold_outstanding` states.
        if !self.merge_enabled.load(Ordering::SeqCst)
            || self.merge_outstanding()
            || self.fold_outstanding()
            || self.wal.is_poisoned()
            || !self.may_publish()
        {
            return;
        }
        let Some(plan) = crate::merge::plan_merge(generation, self.merge_policy) else {
            return;
        };
        let manifest = &generation.bundle.manifest;
        // This view's schema, not the bundle's: the merged segment must carry the scoped render
        // lanes its inputs carry, or the rewrite serves them as absence.
        let scalar_schema = view_scalar_schema_of(manifest, &plan.view);
        let runtime: Vec<String> = self
            .live
            .attributes_for_publication()
            .0
            .into_iter()
            .map(|d| d.name)
            .collect();
        let absent_ok = lawful_absences(&scalar_schema, scalar_schema_of(manifest).len(), &runtime);
        let Some(partition_data) = generation.bundle.partitions.get(&plan.partition) else {
            return;
        };

        self.merge_attempt += 1;
        let ctx = crate::merge::MergeContext {
            prefix_dir: self.prefix_dir(generation),
            prefix: generation.prefix.clone(),
            // The same never-reused shape a flush's `seg_id` has, and for the same reason: two
            // attempts at one `n` would otherwise write one path, and the second `File::create`
            // truncates files the first has memory-mapped.
            seg_id: format!("merge-{}-{}", partition_data.segments_n, self.merge_attempt),
            identity_key: self.identity_key,
            shard_id: manifest.identity.shard_id,
            scalar_schema,
            absent_ok,
            watermark: generation.watermark,
            entity_id_high_water: partition_data.manifest.entity_id_high_water,
        };

        self.merge_in_flight.store(true, Ordering::SeqCst);
        let in_flight = Arc::clone(&self.merge_in_flight);
        let health = Arc::clone(&self.health);
        let submit = self.merge_submit.clone();
        self.pool.spawn(move || {
            match crate::merge::execute(plan, ctx) {
                Ok(completed) => {
                    health.merge_completed_pending.store(true, Ordering::SeqCst);
                    let _ = submit.send(completed);
                }
                Err(e) => {
                    health.merge_failures.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(
                        error = %e,
                        "a merge failed; the segment count keeps growing and it is retried at the \
                         next tick"
                    );
                }
            }
            in_flight.store(false, Ordering::SeqCst);
        });
    }

    /// Dispatch a suggestion-index rebuild where one vocabulary's side map has run far enough
    /// ahead of its base (`crate::suggest::SuggestIndexes::most_owed_rebuild`).
    ///
    /// Runs on the pool, not this thread, because this thread must reach a queued deny promptly.
    /// Nothing waits on the rebuild: a value in the side map is suggested exactly as one in the
    /// base is, so a rebuild that never finishes costs residency, not a missing answer.
    pub(super) fn dispatch_suggest_rebuild(&mut self) {
        if self.suggest_in_flight.load(Ordering::SeqCst) {
            return;
        }
        let generation = self.generation.load_full();
        let Some((vocabulary, covered_through)) = generation.suggest.most_owed_rebuild() else {
            return;
        };
        let vocabulary = vocabulary.to_string();
        let Some(minter) = generation.vocabularies.get(&vocabulary) else {
            return;
        };
        // Snapshotted here rather than read on the pool: the minter lives on the generation and the
        // next window publishes a new one, so the build must own its input.
        let values = crate::suggest::values_of(minter);
        self.suggest_build += 1;
        let build = self.suggest_build;
        let dir = self.suggest_dir.join(&vocabulary);

        self.suggest_in_flight.store(true, Ordering::SeqCst);
        let in_flight = Arc::clone(&self.suggest_in_flight);
        let submit = self.suggest_submit.clone();
        let pool = Arc::clone(&self.pool);
        self.pool.spawn(move || {
            match crate::suggest::SuggestIndex::build(&dir, build, &values, &pool) {
                Ok(index) => {
                    let _ = submit.send(crate::suggest::CompletedSuggest {
                        vocabulary,
                        index: Arc::new(index),
                        covered_through,
                    });
                }
                Err(source) => {
                    // The live index is still complete, since the side map holds everything the
                    // base does not, so this costs residency and is retried at the next tick.
                    tracing::warn!(
                        %vocabulary,
                        %source,
                        "a suggestion index rebuild failed; the side map keeps the live index \
                         complete and the rebuild is retried at the next tick"
                    );
                }
            }
            in_flight.store(false, Ordering::SeqCst);
        });
    }

    /// Drop one vocabulary's suggestion index and publish. The executor half of
    /// `Engine::forget_suggestion_index_for_test`.
    ///
    /// Runs on this thread, the sole publisher, so a swap performed elsewhere could be lost to one
    /// already in flight here. Carries everything else forward and moves neither version counter,
    /// as [`Self::publish_completed_suggests`] does.
    #[cfg(feature = "fault-injection")]
    pub(super) fn forget_suggestion_index(&mut self, vocabulary: &str) {
        let live = self.generation.load_full();
        let next = live.with(|g| {
            g.suggest = Arc::new(live.suggest.without(vocabulary));
        });
        // Nothing acknowledged anything: the hook's own channel is what the caller waits on, so
        // the token is dropped here as the rebuild's is.
        self.publish(next, std::time::Instant::now());
    }

    /// Build one vocabulary's suggestion index from the live minter and publish it, inline. Backs
    /// `ExecutorWork::RebuildSuggestionIndex`.
    ///
    /// Runs the dispatch's own two steps without the threshold or the pool, submitted to the same
    /// channel and published by the same [`Self::publish_completed_suggests`], so a test observes
    /// the production path's result. Inline because the caller is blocked on it.
    ///
    /// A build that fails publishes nothing and is not an error here: the caller's next request
    /// sees the index it already had.
    #[cfg(feature = "fault-injection")]
    pub(super) fn rebuild_suggestion_index_now(&mut self, vocabulary: &str) {
        let generation = self.generation.load_full();
        let Some(covered_through) = generation
            .suggest
            .get(vocabulary)
            .map(|live| live.next_seq())
        else {
            return;
        };
        let Some(minter) = generation.vocabularies.get(vocabulary) else {
            return;
        };
        let values = crate::suggest::values_of(minter);
        self.suggest_build += 1;
        let dir = self.suggest_dir.join(vocabulary);
        let Ok(index) =
            crate::suggest::SuggestIndex::build(&dir, self.suggest_build, &values, &self.pool)
        else {
            return;
        };
        let _ = self.suggest_submit.send(crate::suggest::CompletedSuggest {
            vocabulary: vocabulary.to_string(),
            index: Arc::new(index),
            covered_through,
        });
        self.publish_completed_suggests();
    }

    /// Publish every finished rebuild, and report whether any did.
    ///
    /// Its own swap, carrying everything else forward: no geometry moved and both structures
    /// answer identically. `segments_version` and `overlay_version` are left unchanged, since
    /// bumping either would invalidate every row projection for a change no request can observe.
    pub(super) fn publish_completed_suggests(&mut self) -> bool {
        let mut any = false;
        while let Ok(completed) = self.suggest_done.try_recv() {
            let generation = self.generation.load_full();
            let superseded = generation
                .suggest
                .get(&completed.vocabulary)
                .map(|live| live.base().dir().to_path_buf());
            let suggest = Arc::new(generation.suggest.with_rebuilt(
                &completed.vocabulary,
                completed.index,
                completed.covered_through,
            ));
            let next = generation.with(|g| {
                g.suggest = suggest;
            });
            // Nothing acknowledged anything: a rebuild answers no caller, so the token is
            // dropped here as the coalesce's is.
            self.publish(next, std::time::Instant::now());
            // After the swap: a request still holding the superseded generation keeps its pages,
            // since the mapping outlives the directory entry, so deleting before the swap would
            // race a walk against a file whose name had just been removed.
            if let Some(dir) = superseded {
                let _ = std::fs::remove_dir_all(dir);
            }
            any = true;
        }
        any
    }

    /// Apply every completed merge waiting from the pool, and report whether any did.
    pub(super) fn publish_completed_merges(&mut self) -> bool {
        // Left in the channel rather than dropped; see
        // `MaintenanceDeps::merge_publication_paused`. Always false in a shipped build.
        if self.merge_publication_paused.load(Ordering::SeqCst) {
            return false;
        }
        let mut any = false;
        while let Ok(completed) = self.merge_done.try_recv() {
            self.publish_merge(completed);
            any = true;
        }
        if any {
            self.health
                .merge_completed_pending
                .store(false, Ordering::SeqCst);
        }
        any
    }

    /// Publish a row-space merge: its own swap, a `segments_version` bump, and a refresh armed
    /// before it.
    ///
    /// The one publication that permutes row space rather than extending it: a row id inside the
    /// merged span names a different entity afterwards, so every cached projection covering that
    /// span is wrong and refuses to serve or extend. The refresh is armed before the swap, so a
    /// same-key racer in that window is shed with a 429 instead of paying for the rebuild.
    ///
    /// Gets its own swap rather than riding the next flush, or the flush's zero-cost path would
    /// carry the merge's refresh.
    pub(super) fn publish_merge(&mut self, completed: crate::merge::CompletedMerge) {
        // The seam between the merge's execution on the pool and its publication here: the merged
        // segment exists, its inputs stand, and this thread has committed to nothing. It has not
        // yet read the overlay it will re-derive the deny mask from. This is the first statement,
        // so a parked executor holds no lock and has taken no decision a kill would tear.
        self.pause_point(PauseSiteArg::BeforeMergePublish);
        let started = std::time::Instant::now();
        // A node whose durable state disagrees with what it is serving publishes nothing
        // (`may_publish`). The unit's files are orphans and its inputs still stand, which is the
        // same posture every other publication failure takes.
        if !self.may_publish() {
            return;
        }
        // Every counted discard below is the same posture, so it is one closure rather than the
        // shape repeated. It owns what it reports, so it borrows nothing the sequence below needs.
        let discard = {
            let health = Arc::clone(&self.health);
            move |reason: &str| {
                health.merge_failures.fetch_add(1, Ordering::Relaxed);
                tracing::error!(
                    "ALARM: discarding a completed merge: {reason}. Its files are orphans, every \
                     consumed segment still stands, and the next tick re-plans"
                );
            }
        };
        let live = self.generation.load_full();
        if live.prefix != completed.prefix {
            tracing::warn!("discarding a completed merge planned against a superseded prefix");
            return;
        }
        let Some(partition_data) = live.bundle.partitions.get(&completed.plan.partition) else {
            return;
        };

        let mut manifest = partition_data.manifest.clone();
        if !crate::merge::rebase_into(&mut manifest, &completed) {
            // Its inputs are gone, or their runs are no longer contiguous. Expected rather than
            // exceptional, and the files are orphans nothing references.
            tracing::warn!("discarding a completed merge that no longer rebases");
            return;
        }
        write_deny_state(&mut manifest, &live.overlay);
        write_vocabulary_extensions(
            &mut manifest,
            &live.vocabularies,
            &live.bundle.manifest.vocabularies,
        );

        let manifest_n = match self.allocate_manifest_n() {
            Ok(n) => n,
            Err(e) => {
                discard(&format!(
                    "its side-manifest number could not be allocated ({e})"
                ));
                return;
            }
        };
        // The publication seam: the merged segment's files are on disc and nothing durable names
        // them until this write returns.
        self.pause_point(PauseSiteArg::BeforeManifestPublish);
        if let Err(e) = self.commit_side_manifest(
            &partition_data.manifest,
            &self.prefix_dir(&live),
            &completed.plan.partition,
            manifest_n,
            &mut manifest,
            None,
        ) {
            discard(&format!("its side-manifest could not be committed ({e})"));
            return;
        }

        let consumed: Vec<String> = completed
            .plan
            .inputs
            .iter()
            .map(|i| i.seg_id.clone())
            .collect();
        let merged_seg_id = completed.segment.seg_id.clone();
        let next_bundle = match live.bundle.with_merged(
            &completed.plan.partition,
            &completed.plan.view,
            &consumed,
            completed.segment,
            completed.output.extent,
            tessera_store::read::PublishedManifest {
                manifest,
                n: manifest_n,
            },
        ) {
            Ok(bundle) => bundle,
            Err(e) => {
                // ABA-safe by `seg_id`: an absent input is proof the inputs are gone, never a
                // pointer comparison. Discarded rather than forced, since forcing would collapse
                // a run that is no longer the one the merged extent's rows were computed against.
                tracing::warn!(error = %e, "discarding a completed merge that no longer rebases");
                return;
            }
        };

        let segments_version = live.segments_version + 1;
        // Every held row form of the view is rebased over the merged extent before the swap, the
        // twin of the flush's extension: the rows inside the merged span name other entities now,
        // so a form that kept its bits there would count one segment's rows as another's. A
        // stored level's rebase costs the members inside the merged extent's range per artifact; a
        // spatial level's is the merged segment resolved whole against its shapes, in
        // `ArtifactProjections::rebase_merged`.
        if let (Some(previous), Some(space)) = (
            view_row_space(
                &live.bundle,
                &completed.plan.partition,
                &completed.plan.view,
            ),
            view_row_space(
                &next_bundle,
                &completed.plan.partition,
                &completed.plan.view,
            ),
        ) {
            let merged_segment = next_bundle
                .partitions
                .get(&completed.plan.partition)
                .and_then(|p| p.views.get(&completed.plan.view))
                .and_then(|v| v.segments.iter().find(|s| s.seg_id == merged_seg_id))
                .map(|s| s.as_ref());
            self.live.with_artifacts(|store| {
                let rows_of = |layer: &str, level: u32| {
                    self.segment_rows_of(
                        &completed.plan.view,
                        layer,
                        level,
                        merged_segment,
                        &[],
                        store,
                    )
                };
                self.artifact_projections.rebase_merged(
                    &live.prefix,
                    &completed.plan.view,
                    store,
                    previous,
                    space,
                    &merged_seg_id,
                    segments_version,
                    &rows_of,
                )
            });
        }

        let next = Arc::new(live.with(|g| {
            g.segments_version = segments_version;
            g.bundle = next_bundle;
            // The consumed segments' delta tiers carry over: their entities still have rows in the
            // merged segment.
        }));
        // The claim names the generation it is for, so a pass that is superseded mid-flight
        // releases nothing when it ends; see `refresh::clear_if_current`.
        self.refresh
            .in_flight
            .store(segments_version, Ordering::SeqCst);
        self.publish_arc(Arc::clone(&next), started);
        self.refresh.spawn(next);

        self.row_projection_cache
            .prune_generations_below(segments_version.saturating_sub(KEEP_SUPERSEDED_GENERATIONS));
        self.prune_region_cache(segments_version);
        self.health.merges.fetch_add(1, Ordering::Relaxed);
    }

    /// Apply every completed coalesce waiting from the pool, and report whether any did.
    pub(super) fn publish_completed_coalesces(&mut self) -> bool {
        let mut any = false;
        while let Ok(completed) = self.coalesce_done.try_recv() {
            self.publish_coalesce(completed);
            any = true;
        }
        if any {
            // Cleared only after something was drained, never on an empty pass. This is the other
            // half of the handshake; see `ExecutorHealth::flush_completed_pending`.
            self.health
                .coalesce_completed_pending
                .store(false, Ordering::SeqCst);
        }
        any
    }

    /// Publish an entity-space coalesce: a manifest edit, a sidecar swap and a tier-list swap, and
    /// no `segments_version` bump.
    ///
    /// The half of merge that touches no row space: a delta tier is `(term, entity)` pairs, a run
    /// and its locator are `external_id ↔ entity`, a dictionary extent is descriptors, so no
    /// projection is invalidated and no session pays anything. It swaps the generation's tier list
    /// and the external-id sidecar, both content-preserving, so a request holding either version
    /// agrees on every answer.
    ///
    /// The consumed files are not deleted: every side-manifest below this `n` still names them, and
    /// reclaiming them is compaction's.
    pub(super) fn publish_coalesce(&mut self, completed: crate::coalesce::CompletedCoalesce) {
        let started = std::time::Instant::now();
        // Same `may_publish` guard as `publish_merge`.
        if !self.may_publish() {
            return;
        }
        // Every counted discard below is the same posture, so it is one closure rather than the
        // shape repeated. It owns what it reports, so it borrows nothing the sequence below needs.
        let discard = {
            let health = Arc::clone(&self.health);
            move |reason: &str| {
                health.coalesce_failures.fetch_add(1, Ordering::Relaxed);
                tracing::error!(
                    "ALARM: discarding a completed coalesce: {reason}. Its files are orphans, \
                     every consumed entry still stands, and the next tick re-plans"
                );
            }
        };
        let live = self.generation.load_full();
        if live.prefix != completed.prefix {
            tracing::warn!(
                planned = %completed.prefix,
                live = %live.prefix,
                "discarding a completed coalesce planned against a superseded prefix"
            );
            return;
        }
        let Some(partition_data) = live.bundle.partitions.get(&completed.plan.partition) else {
            return;
        };

        let mut manifest = partition_data.manifest.clone();
        if !crate::coalesce::rebase_into(&mut manifest, &completed) {
            // The window it planned against is gone. Expected rather than exceptional, as
            // `rebase_into` shows, and the files are orphans nothing references.
            tracing::warn!("discarding a completed coalesce that no longer rebases");
            return;
        }
        // Composed before the manifest is written, in the flush's order and for its reason: a
        // composition that refuses must not leave a published manifest naming layers this process
        // cannot serve, and the reverse order commits a manifest whose own writer then refuses it.
        // The unit carries opened columns, so this cannot fail on IO.
        let windows: Vec<crate::filter::CoalescedWindow> = completed
            .attrs
            .iter()
            .zip(&completed.plan.attrs)
            .map(|(attr, window)| crate::filter::CoalescedWindow {
                column: crate::filter::extent_column_name(
                    &attr.extent.column,
                    attr.extent.view.as_deref(),
                ),
                consumed: window.extents.iter().map(|e| e.values.clone()).collect(),
                values_rel: attr.extent.values.clone(),
                values: Arc::clone(&attr.values),
                // A keyword window's merged dictionary, beside the ordinals it numbers; the
                // composition installs the pair as one layer or refuses.
                dict: attr.dict.clone(),
            })
            .collect();
        // The text axis's windows, named by dictionary path on both sides. The paths are resolved
        // against the live prefix here, on the executor, and opened inside the composition. This
        // is the flush's arrangement for a text extent, and for its reason: a text layer is three
        // files that must be installed together.
        let prefix_dir = self.prefix_dir(&live);
        let text_windows: Vec<crate::filter::CoalescedTextWindow> = completed
            .texts
            .iter()
            .zip(&completed.plan.texts)
            .map(|(extent, window)| crate::filter::CoalescedTextWindow {
                consumed: window.extents.iter().map(|e| e.dict.clone()).collect(),
                paths: text_extent_paths(&prefix_dir, extent),
            })
            .collect();
        // The transpose's stack is re-derived from the rebased manifest, not patched. This is the
        // form `delta_postings` below takes, and for its reason: re-deriving is the one shape that
        // cannot drift from what a restart would open. It is affordable here where it would not be
        // per flush: the base's `hasrow` is a run-container bitmap and its other two files are
        // mapped rather than read, and a coalesce fires once per `width` ticks. `None` where the
        // axis did not run, in which case the live stack rides through untouched.
        let entity_terms = if completed.terms.is_none() {
            None
        } else {
            let partition_dir = prefix_dir
                .join("partitions")
                .join(&completed.plan.partition);
            let extents: Vec<tessera_store::EntityTermsExtentPaths> = manifest
                .entity_terms_extents
                .iter()
                .map(|e| entity_terms_extent_paths(&prefix_dir, e))
                .collect();
            match tessera_store::EntityTermsStack::open(
                Some(&partition_dir.join(tessera_store::ENTITY_TERMS_DIR)),
                &extents,
            ) {
                Ok(stack) => Some(Arc::new(stack)),
                Err(e) => {
                    discard(&format!(
                        "its entity→term extent would not compose into a stack ({e}), and a \
                         manifest must not name a layer this process cannot serve"
                    ));
                    return;
                }
            }
        };
        // The record axis's stack is re-derived from the rebased manifest too, on exactly the
        // transpose's rule above: a coalesce that folds a window of record extents into one must
        // leave the live reader probing the extent it wrote and not the ones it consumed, or the
        // process serves from layers its own manifest no longer names until a restart. Affordable
        // for the same reason: the layers are memory-mapped, and a coalesce fires once per
        // `width` ticks. `None` where the axis did not run, and the live stack rides through.
        let records =
            if completed.record.is_none() {
                None
            } else {
                let partition_dir = prefix_dir
                    .join("partitions")
                    .join(&completed.plan.partition);
                // The schema decides whether there is a base, exactly as it does at open: a build
                // writes `attrs/record` only where a column has no other home. Derived rather than
                // probed for, so a missing base refuses instead of reading as "those entities have no
                // record".
                let blob_resident =
                    live.bundle.manifest.declared_scalars.iter().any(|d| {
                        crate::filter::blob_resident(d, &live.bundle.manifest.vocabularies)
                    });
                let record_dir = partition_dir.join("attrs").join("record");
                // Both lists, one stack, as the open composes them: an artifact's content extents
                // hold the same format and the same reader, and the two never share an entity.
                let extents: Vec<tessera_filter::RecordExtentPaths> = manifest
                    .record_extents
                    .iter()
                    .chain(manifest.artifact_record_extents.iter())
                    .map(|e| record_extent_paths(&prefix_dir, e))
                    .collect();
                match tessera_filter::RecordStack::open(
                    blob_resident.then_some(record_dir.as_path()),
                    &extents,
                    live.filter_columns.access(),
                ) {
                    Ok(stack) => Some(Arc::new(stack)),
                    Err(e) => {
                        discard(&format!(
                            "its record extent would not compose into a stack ({e}), and a \
                             manifest must not name a layer this process cannot serve"
                        ));
                        return;
                    }
                }
            };
        let filter_columns =
            match live
                .filter_columns
                .with_coalesced(&windows, &text_windows, entity_terms, records)
            {
                Ok(columns) => Arc::new(columns),
                Err(e) => {
                    discard(&format!(
                        "its attribute extents would not replace the layers they consumed ({e}), \
                         and a manifest must not name a column this process cannot serve"
                    ));
                    return;
                }
            };
        // Complete current state, serialised fresh from the overlay this publication carries, the
        // same rule every other manifest write follows.
        write_deny_state(&mut manifest, &live.overlay);
        write_vocabulary_extensions(
            &mut manifest,
            &live.vocabularies,
            &live.bundle.manifest.vocabularies,
        );

        let manifest_n = match self.allocate_manifest_n() {
            Ok(n) => n,
            Err(e) => {
                discard(&format!(
                    "its side-manifest number could not be allocated ({e})"
                ));
                return;
            }
        };
        // The publication seam: the coalesced extents are on disc and nothing durable names them
        // until this write returns.
        self.pause_point(PauseSiteArg::BeforeManifestPublish);
        if let Err(e) = self.commit_side_manifest(
            &partition_data.manifest,
            &self.prefix_dir(&live),
            &completed.plan.partition,
            manifest_n,
            &mut manifest,
            None,
        ) {
            discard(&format!("its side-manifest could not be committed ({e})"));
            return;
        }

        // The sidecar reads the *new* manifest, so it must be built after the edit and before the
        // swap. It is built here, on the executor, because a failure must abandon the publication
        // rather than leave the generation naming runs no sidecar can resolve.
        let next_index = match crate::session::ExternalIdIndex::open(
            &live.bundle.manifest,
            &manifest,
            &self.prefix_dir(&live),
        ) {
            Ok(index) => index,
            Err(e) => {
                // Not the discard above: this manifest is already committed. The process keeps
                // serving the pre-coalesce sidecar, which answers identically.
                self.health
                    .coalesce_failures
                    .fetch_add(1, Ordering::Relaxed);
                tracing::error!(
                    error = %e,
                    "ALARM: a coalesce's manifest committed but its external-id sidecar would not \
                     open; a restart opens the committed manifest and no operator action is owed"
                );
                return;
            }
        };

        let next_bundle = match live.bundle.with_manifest(
            &completed.plan.partition,
            tessera_store::read::PublishedManifest {
                manifest,
                n: manifest_n,
            },
        ) {
            Ok(bundle) => bundle,
            Err(e) => {
                tracing::warn!(error = %e, "discarding a completed coalesce that no longer rebases");
                return;
            }
        };

        // The tier list, with the consumed tiers replaced by the one that carries their pairs.
        // Rebuilt from the new manifest rather than patched positionally: `deltas` is now the
        // authority on which tiers are live, and re-deriving from it is the one form that cannot
        // drift from what a restart would open.
        let mut delta_postings: Vec<Arc<DeltaTier>> = Vec::new();
        let coalesced = completed.tier.as_ref();
        for rel in &next_bundle
            .partitions
            .get(&completed.plan.partition)
            .expect("the partition this publication just rebased")
            .manifest
            .deltas
        {
            let held = match coalesced.filter(|(path, _)| path == rel) {
                Some((_, tier)) => Some(Arc::clone(tier)),
                None => held_tier(&live.delta_postings, &partition_data.manifest.deltas, rel),
            };
            let Some(tier) = held else {
                // The manifest is already committed, as at the sidecar exit above.
                self.health
                    .coalesce_failures
                    .fetch_add(1, Ordering::Relaxed);
                tracing::error!(
                    tier = %rel,
                    "ALARM: a coalesce's manifest committed and names a delta tier this process \
                     does not hold open, so the swap is abandoned; a restart opens the committed \
                     manifest"
                );
                return;
            };
            delta_postings.push(tier);
        }

        let next = live.with(|g| {
            // The live columns with each consumed window replaced by the layer that carries its
            // values, the same set of `(entity, value)` pairs in fewer files, so a request holding
            // the old and one holding the new agree on every answer.
            g.filter_columns = filter_columns;
            g.bundle = next_bundle;
            // The sidecar rides the swap, rather than being stored beside it: a coalesce is
            // content-preserving, but a fold is not, since it drops the retired entities' keys and
            // writes into a new prefix. One pointer carries both, so no request can ever hold a
            // generation and a sidecar from two publications.
            g.external_index = Arc::new(next_index);
            g.delta_postings = delta_postings;
        });
        // The outgoing sidecar is remembered before it stops being live. A coalesce is the one
        // publication that builds a new one over the same prefix, so from here a generation
        // holding the old one is invisible to the sidecar count reclamation takes; see
        // `superseded_sidecars`. Held weakly and pruned as it goes, so a prefix that coalesces all
        // day accumulates pointers rather than mappings.
        self.superseded_sidecars
            .retain(|held| held.strong_count() > 0);
        self.superseded_sidecars
            .push(Arc::downgrade(&live.external_index));
        self.publish(next, started);
        self.health.coalesces.fetch_add(1, Ordering::Relaxed);
    }

    /// Apply every completed flush waiting from the pool, and report whether any did.
    ///
    /// Drained after the deny lane and before work, so a publication never delays a suppression
    /// and never waits behind a commit window.
    pub(super) fn publish_completed_flushes(&mut self) -> bool {
        let mut any = false;
        while let Ok(completed) = self.flush_done.try_recv() {
            let mark = StageMark::now();
            self.publish_flush(completed);
            self.health
                .flush_lap(crate::flush::FlushStage::PublishWall, mark);
            any = true;
        }
        if any {
            // Cleared only after something was drained (never on an empty pass), so a set-and-send
            // landing between this loop's empty `try_recv` and a clear could not be erased. This is
            // the handshake's other half; see `ExecutorHealth::flush_completed_pending`.
            self.health
                .flush_completed_pending
                .store(false, Ordering::SeqCst);
        }
        any
    }

    /// Hand each plan to the background pool, and mark a flush in flight until all of them land.
    ///
    /// Execution is off this thread: the segment write is file IO of unbounded duration, and this
    /// thread must drain the deny lane before it touches work, or a flush would put a suppression
    /// behind it. Every input is taken here against the live generation and then moved, so the
    /// pool holds no reference to live state.
    ///
    /// Answers whether a unit reached the pool; every other path drops the plan and leaves the
    /// buffer standing, an unpublished cycle the caller holds open.
    pub(super) fn dispatch_flushes(
        &mut self,
        generation: &Arc<Generation>,
        plans: Vec<(String, crate::flush::FlushPlan)>,
    ) -> bool {
        let mark = StageMark::now();
        let submit = self.flush_submit.clone();
        let Some((partition, partition_data)) = generation.bundle.partitions.iter().next() else {
            return false;
        };
        let manifest = &generation.bundle.manifest;
        let scalar_schema = scalar_schema_of(manifest);
        let filter_schema = filter_schema_of(manifest);
        let record_schema = record_schema_of(manifest);
        // An unusable analyser stops the dispatch rather than flushing an unindexed batch. A
        // flush that skipped the column would leave the buffer's text out of the index with no
        // error, and the next fold would rebuild from values that are in the blob, so the gap
        // would close with nothing to show it had happened.
        let text_schema = match text_schema_of(manifest) {
            Ok(schema) => schema,
            Err(e) => {
                self.health.flush_failures.fetch_add(1, Ordering::Relaxed);
                // Once per period: the condition stands until the binary changes, and a failed
                // cycle retries at `FAILED_CYCLE_RETRY` (`ExecutorHealth::refusal_log_due`).
                if self.health.refusal_log_due() {
                    tracing::error!(
                        error = %e.0,
                        "ALARM: a text column's analyser is not one this binary carries; no flush \
                         is dispatched, and the buffer is retained"
                    );
                }
                return false;
            }
        };
        let render_indices: Vec<usize> = manifest.render_indices().collect();
        // The group-scoped families, by view, taken once for the dispatch: the schema below is
        // per view, because a family's lanes and columns are its group's views' and no others'.
        let scoped_by_view = scoped_families_by_view(manifest);
        // One plan per dispatch: a publication rebases on the generation the one before it
        // swapped ([`Bundle::with_segment`]), so a second unit in flight over the same view's row
        // space would be discarded at its rebase with its files already written. Dispatching one
        // avoids the losers' wasted segment writes; the rest re-plan at the next tick.
        //
        // Chosen by oldest unflushed row, not by view name: `items` is ascending by entity id and
        // entity ids are issued monotonically, so `items.first()` is an age key needing no cursor
        // state, bounding starvation at `s × flush_max_age_secs` for `s` views.
        //
        // A deferred plan holds the publication cycle open (`ExecutorHealth::deferred_plans`): a
        // caller waiting on a publication number is not answered until a tick dispatches with
        // nothing left over.
        self.health.deferred_plans.store(false, Ordering::SeqCst);
        let deferred = plans.len().saturating_sub(1);
        let Some((view, plan)) = plan_to_dispatch(plans) else {
            return false;
        };

        let context = {
            let Some(view_data) = partition_data.views.get(&view) else {
                return false;
            };
            // This view's frame: the flush quantises against the extent the view's own positions
            // were placed in, and a bundle-wide frame would put a second view's rows on the
            // first's grid. A plan naming a view the manifest does not declare is dropped here
            // rather than flushed against a guessed frame, the same refusal `accept_ingest` makes.
            //
            // This view's incarnation is resolved on the same rule. The stamp goes on the segment,
            // every scoped column this flush writes, and every extent, so a key created again
            // cannot adopt them.
            let Some(incarnation) = manifest.incarnation_of(&view) else {
                if self.health.refusal_log_due() {
                    tracing::error!(
                        view = %view,
                        "a flush plan names a view this bundle's manifest does not declare, so \
                         its incarnation cannot be resolved; the plan is dropped and the buffer \
                         is retained"
                    );
                }
                return false;
            };
            let Some(quantisation) = manifest.quantisation_of(&view) else {
                if self.health.refusal_log_due() {
                    tracing::error!(
                        view = %view,
                        "a flush plan names a view this bundle's manifest does not declare, so \
                         there is no frame to quantise its rows against; the plan is dropped \
                         and the buffer is retained"
                    );
                }
                return false;
            };
            let Ok(row_base) = u32::try_from(view_data.row_space.total_rows()) else {
                // Row ids are `u32` (bundle_format 1). A view that has crossed 2^32 rows cannot
                // take another segment, and saying so is better than wrapping into row 0.
                if self.health.refusal_log_due() {
                    tracing::error!(
                        view = %view,
                        "ALARM: this view's row space has reached the u32 ceiling; no further \
                         flush can address it. The deployment must be compacted or re-sharded"
                    );
                }
                return false;
            };

            // The descriptor bytes behind this plan's extension term ids, and the whole of what
            // promotion needed that the buffer does not hold. Computed lazily: in the steady state
            // every descriptor is already interned, `novel` is empty, and this takes no lock and
            // allocates nothing. One comparison per term is the entire cost.
            let dict_len = generation.dict.len();
            let novel: FxHashSet<TermId> = plan
                .items
                .iter()
                .flat_map(|(_, item)| item.terms.iter().copied())
                .filter(|term| term.raw() >= dict_len)
                .collect();
            let novel_descriptors = if novel.is_empty() {
                FxHashMap::default()
            } else {
                self.live.descriptors_of(&novel)
            };

            // A label, not an allocation. `n` is allocated by the executor at publication
            // (`next_manifest_n`), because a deny publication may take one while this flush is in
            // flight. What the plan needs is a component that makes `seg_id` unique, and the
            // sequence it was planned against is exactly that: the never-reused property rests on
            // this plus the attempt counter.
            let planned_at_n = partition_data.segments_n;
            // This view's scoped families, and where each one's value sits in a buffered row's
            // `scoped` list. Positional against the group's own manifest order, which is the order
            // `/control/ingest` parsed the batch against. One derivation, `scoped_families_by_view`,
            // so the two cannot come to disagree about which value belongs to which family.
            let families = scoped_by_view.get(&view).cloned().unwrap_or_default();
            // Where those families' columns live: the owner's view of the same key, which is
            // `view` itself under the owning group's own views.
            let scoped_view = scoped_owner_view_of(manifest, &view);
            let Some(scoped_incarnation) = manifest.incarnation_of(&scoped_view) else {
                // Fail closed: an owner view the manifest cannot place is a bundle whose halves
                // disagree, and flushing under a guessed incarnation is how a dropped view's
                // predecessor adopts rows.
                if self.health.refusal_log_due() {
                    tracing::error!(
                        view = %scoped_view,
                        "ALARM: no incarnation for the owner view; no flush is planned this tick"
                    );
                }
                return false;
            };
            let scoped_schema: Vec<crate::flush::ScopedColumnSpec> = match families
                .iter()
                .enumerate()
                .map(|(index, family)| {
                    let text = family.arrow_type == ScalarType::Text;
                    let analyser = if text {
                        Some(std::sync::Arc::new(analyser_of(family)?))
                    } else {
                        None
                    };
                    Ok(crate::flush::ScopedColumnSpec {
                        index,
                        name: family.name.clone(),
                        ty: family.arrow_type,
                        category: family.vocabulary.is_some(),
                        filterable: crate::filter::scoped_is_filterable(family),
                        has_value_column: crate::filter::scoped_has_value_column(family),
                        render: family.render,
                        has_base: family.views.contains(&scoped_view),
                        analyser,
                    })
                })
                .collect::<Result<Vec<_>, crate::flush::FlushFailed>>()
            {
                Ok(schema) => schema,
                Err(e) => {
                    // The same refusal `text_schema_of` makes, for its reason: a flush that
                    // indexed prose with a pipeline the base was not built by leaves one column
                    // whose two layers disagree about what a word is.
                    self.health.flush_failures.fetch_add(1, Ordering::Relaxed);
                    if self.health.refusal_log_due() {
                        tracing::error!(
                            error = %e,
                            "ALARM: a group-scoped text family's analyser is not one this binary \
                             carries; no flush is dispatched, and the buffer is retained"
                        );
                    }
                    return false;
                }
            };
            // The lanes this view's rows carry: which side of the family's `views` list this
            // flush is on.
            //
            // Under a view of the family's own group, every rendered family of the group gets a
            // lane whether or not the manifest already lists the view: this flush is what gives
            // the view its column. A sharing group's view is on the same side, writing the family
            // through the key it shares. Under any other view a lane of absences is owed rather
            // than no lane, so a segment missing one is not left for its own view's rewriters to
            // guess about.
            let scoped_render: Vec<tessera_store::manifest::ScopedScalar> = if families.is_empty() {
                crate::viewport::scoped_render_families(manifest, &view)
                    .into_iter()
                    .cloned()
                    .collect()
            } else {
                families.iter().filter(|f| f.render).cloned().collect()
            };

            // Where each lane's value sits in a buffered row's `scoped` list, `None` where this
            // view writes none of them: a view whose key is in no scope at all, a sharing group
            // writing the family through the key it shares, or any family the batch could not
            // have named.
            let scoped_render_indices: Vec<Option<usize>> = scoped_render
                .iter()
                .map(|lane| families.iter().position(|f| f.name == lane.name))
                .collect();
            crate::flush::FlushContext {
                prefix_dir: self.prefix_dir(generation),
                partition: partition.clone(),
                view: view.clone(),
                scoped_view,
                scoped_incarnation,
                incarnation,

                // `seg_id`s are never reused, which is what makes the merge rebase ABA-safe. The
                // attempt counter matters: `next_n` alone repeats when a flush is planned twice
                // before it publishes, and a second attempt would then `File::create` over a
                // memory-mapped file, truncating the mapping and SIGBUS on the next read.
                seg_id: format!("flush-{planned_at_n}-{}", self.next_flush_attempt()),
                row_base,
                identity_key: self.identity_key,
                shard_id: manifest.identity.shard_id,
                quantisation,
                // This view's schema, entity-scoped tail then scoped render lanes: the same list a
                // merge and a fold of this view take (`view_scalar_schema_of`), so a segment
                // written by any of the three carries the same columns.
                //
                // The two derivations agree only because a `members` group can never own a family
                // (`Manifest::validate_groups` refuses one): `scoped_render` and
                // `scoped_render_families` yield the same owning group's rendered families under
                // any view in scope. Change either site, or that refusal, and the third has to
                // move with it.
                scalar_schema: {
                    let mut schema = scalar_schema.clone();
                    schema.extend(scoped_render.iter().map(|f| (f.name.clone(), f.arrow_type)));
                    schema
                },
                render_indices: render_indices.clone(),
                scoped_schema,
                scoped_render: scoped_render_indices,
                filter_schema: filter_schema.clone(),
                record_schema: record_schema.clone(),
                text_schema: text_schema.clone(),
                dict: Arc::clone(&generation.dict),
                novel_descriptors,
                max_distinct_terms: self.max_distinct_terms,
                prefix: generation.prefix.clone(),
                shapes: self.shapes.levels_of_view(&view),
            }
        };
        self.health
            .flush_lap(crate::flush::FlushStage::Dispatch, mark);

        if deferred > 0 {
            // Re-armed, so `wait_for_work` polls at `FLUSH_COMPLETION_POLL` and the tick that
            // takes the next view comes at the completion of this flush rather than at the next
            // period. Set before the flag below, since a reader that saw the dispatch first and
            // this second could close the cycle in between.
            self.health.deferred_plans.store(true, Ordering::SeqCst);
            self.health.flush_requested.store(true, Ordering::SeqCst);
            tracing::warn!(
                deferred,
                dispatched = %view,
                "a flush unit is per view and a second unit in flight would be discarded at its \
                 rebase, so one view publishes per tick; the rest re-plan at the next one"
            );
        }
        self.health.flush_in_flight.store(true, Ordering::SeqCst);
        self.health.mark_flush_started(std::time::Instant::now());
        let health = Arc::clone(&self.health);
        self.pool.spawn(move || {
            let mut laps = crate::flush::FlushLaps::default();
            match crate::flush::execute_flush(plan, context, &mut laps) {
                Ok(completed) => {
                    health.record_flush_execution(&laps, Some(completed.consumed.len()));
                    // Pending is set before the send. This is the completion handshake's whole
                    // ordering; see `ExecutorHealth::flush_completed_pending`.
                    health.flush_completed_pending.store(true, Ordering::SeqCst);
                    // A send failure means the executor is gone, which is a shutdown and not a
                    // fault: the files are orphans nothing references, and replay re-flushes.
                    let _ = submit.send(completed);
                }
                Err(e) => {
                    health.record_flush_execution(&laps, None);
                    // Nothing happened, retry next tick. The side-manifest is the only commit
                    // point, so a failure before it leaves orphan files nothing references and the
                    // buffer intact. The cycle stays open and its request is re-armed: a caller
                    // waiting on the number waits for the retry that succeeds.
                    health.flush_failures.fetch_add(1, Ordering::Relaxed);
                    health.fail_publication_cycle();
                    tracing::error!(
                        error = %e,
                        "ALARM: a flush failed; the buffer is retained and it will be retried at \
                         the next tick. Sustained failure grows the buffer until \
                         ingest_buffer_max_items sheds ingest, which is the intended backpressure"
                    );
                }
            }
            health.flush_in_flight.store(false, Ordering::SeqCst);
        });
        true
    }

    pub(super) fn next_flush_attempt(&mut self) -> u64 {
        self.flush_attempt += 1;
        self.flush_attempt
    }

    /// Publish every level's row forms from the deltas held since the last tick. This is the one
    /// moment a served form changes.
    ///
    /// Called from the tick and from nowhere else: a request never brings a form forward, so a
    /// level whose deltas are not yet published is served as last published, up to a tick stale.
    /// A level with no form held takes nothing here; the open's warm and the first request to
    /// reach such a level are what build one.
    pub(super) fn publish_row_forms(&mut self) {
        let pending = std::mem::take(&mut self.pending_forms);
        for ((layer, level), deltas) in &pending {
            self.publish_level_forms(layer, *level, deltas);
        }
    }

    /// Apply an interval's deltas to every held row form of one level, in every view the
    /// generation carries.
    ///
    /// A form at a version these deltas do not follow is dropped rather than amended, which
    /// [`crate::artifacts::ArtifactProjections::publish`] argues in full.
    ///
    /// A view two partitions carry is skipped, exactly as `Engine::warm_artifact_projections`
    /// skips it: it is `EngineError::MultiPartitionView` on the request path, so there is no row
    /// space here that a request would agree with.
    pub(super) fn publish_level_forms(
        &self,
        layer: &str,
        level: u32,
        deltas: &[crate::artifacts::LevelDelta],
    ) {
        // Where the delta's rows come from. A stored membership's are the records' members,
        // projected; a spatial level's are the new shapes, resolved over every live segment of
        // the view. The level's shapes are rebuilt here at the version the record just moved it
        // to, and the resolution touches the new ordinals alone. An attribute predicate's members
        // are the rows carrying a value, which a record's delta says nothing about, so its form
        // takes none; see `ArtifactProjections::bring_forward`. An unregistered layer has nothing
        // to read.
        let Some(registered) = self.live.registered_layer(layer) else {
            return;
        };
        let stored = stored_membership(&registered.declaration);
        let spatial = spatial_membership(&registered.declaration);
        if !stored && !spatial {
            return;
        }
        let generation = self.generation.load_full();
        let mut views: std::collections::BTreeMap<&str, Option<&tessera_store::read::ViewData>> =
            std::collections::BTreeMap::new();
        for partition in generation.bundle.partitions.values() {
            for (name, data) in &partition.views {
                views
                    .entry(name.as_str())
                    .and_modify(|held| *held = None)
                    .or_insert(Some(data));
            }
        }
        self.live.with_artifacts(|store| {
            for (view, data) in views {
                let Some(data) = data else { continue };
                if stored {
                    self.artifact_projections.publish(
                        &generation.prefix,
                        view,
                        layer,
                        level,
                        store,
                        &data.row_space,
                        deltas,
                        Some(&crate::artifacts::DeltaRows::Projected),
                    );
                    continue;
                }
                // A growth and a page never reach a spatial level: the registry refuses one before
                // a record is written (`RegistryError::NotEnumerated`), so what the interval holds
                // for such a level is publications, whose new shapes are resolved over every
                // segment here.
                // A fill is here beside a publication: on a spatial level the shape *is* the
                // membership, so an ordinal whose shape was filled needs resolving exactly as a
                // new one does.
                let ordinals: Vec<u32> = deltas
                    .iter()
                    .flat_map(|delta| match &delta.kind {
                        crate::artifacts::DeltaKind::Published(ordinals)
                        | crate::artifacts::DeltaKind::Filled(ordinals) => ordinals.clone(),
                        _ => Vec::new(),
                    })
                    .collect();
                if ordinals.is_empty() {
                    continue;
                }
                let ordinals = &ordinals;
                let held = self.shapes.level(
                    view,
                    layer,
                    level,
                    store,
                    &crate::shapes::PersistedPieces::none(),
                );
                let Ok(segments) = crate::viewport::segments_with_row_bases(view, data) else {
                    continue;
                };
                let started = std::time::Instant::now();
                let mut joined: Vec<Option<croaring::Bitmap>> = vec![None; held.shapes.len()];
                let mut rows_tested = 0u64;
                for (segment, row_base) in &segments {
                    let (piece, cost) = held.resolve_ordinals(segment, ordinals);
                    rows_tested += cost.rows_tested;
                    for (ordinal, part) in piece.into_iter().enumerate() {
                        let Some(part) = part else { continue };
                        let slot = joined[ordinal].get_or_insert_with(croaring::Bitmap::new);
                        if !part.is_empty() {
                            slot.or_inplace(&part.add_offset(i64::from(*row_base)));
                        }
                    }
                }
                tracing::info!(
                    layer = %layer,
                    level,
                    view = %view,
                    artifacts = ordinals.len(),
                    segments = segments.len(),
                    rows_tested,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "a publication into a spatial level resolved its new shapes over every segment"
                );
                let rows_of = |ordinal: u32| {
                    joined
                        .get(ordinal as usize)
                        .cloned()
                        .flatten()
                        .unwrap_or_default()
                };
                self.artifact_projections.publish(
                    &generation.prefix,
                    view,
                    layer,
                    level,
                    store,
                    &data.row_space,
                    deltas,
                    Some(&crate::artifacts::DeltaRows::Resolved(&rows_of)),
                );
            }
        });
    }

    /// Where a geometry publication's rows come from, per level of one view: what
    /// `ArtifactProjections::extend_flushed` and `rebase_merged` are told
    /// (`crate::artifacts::SegmentRows`). A stored level projects; a spatial level takes
    /// `resolved` where the flush's pool resolved the segment against the shapes now held, and
    /// resolves the segment here otherwise, for a level rebuilt by a publication since the flush
    /// was planned, or one no flush plan saw. An attribute predicate takes nothing.
    pub(super) fn segment_rows_of(
        &self,
        view: &str,
        layer: &str,
        level: u32,
        segment: Option<&tessera_store::read::SegmentData>,
        resolved: &[crate::shapes::ShapePiece],
        store: &tessera_lifecycle::membership::ArtifactStore,
    ) -> Option<crate::artifacts::SegmentRows> {
        let registered = self.live.registered_layer(layer)?;
        if stored_membership(&registered.declaration) {
            return Some(crate::artifacts::SegmentRows::Projected);
        }
        if registered.declaration.membership != tessera_types::layer::MembershipSource::Spatial
            || registered.declaration.shape.is_none()
        {
            return None;
        }
        let held = self.shapes.level(
            view,
            layer,
            level,
            store,
            &crate::shapes::PersistedPieces::none(),
        );
        if let Some(piece) = resolved.iter().find(|piece| {
            piece.level.layer == layer
                && piece.level.level == level
                && Arc::ptr_eq(&piece.level, &held)
        }) {
            return Some(crate::artifacts::SegmentRows::Resolved(Arc::clone(
                &piece.rows,
            )));
        }
        let segment = segment?;
        let (rows, cost) = held.resolve(segment);
        tracing::info!(
            layer = %layer,
            level,
            view = %view,
            seg_id = %segment.seg_id,
            rows_tested = cost.rows_tested,
            rows_interior = cost.rows_interior,
            elapsed_ms = cost.elapsed_ms,
            "a geometry publication resolved its segment against a spatial level's shapes"
        );
        Some(crate::artifacts::SegmentRows::Resolved(Arc::new(rows)))
    }

    pub(super) fn publish_overlay_state(&mut self) {
        if !self.deny_dirty {
            return;
        }
        if self.wal.is_poisoned() || !self.may_publish() {
            tracing::warn!(
                "ALARM: deny state is unpublished and this node is poisoned or diverged, so it \
                 will not write a side-manifest. The dispositions are in force and WAL-durable; \
                 what is degraded is the restore path, until the node recovers or restarts"
            );
            return;
        }

        let live = self.generation.load_full();
        // One scan for the publication, not one per partition. Each partition takes its own `n`,
        // and the floor under all of them is the same disc state
        // ([`Executor::raise_manifest_floor`]); scanning inside the loop would cost a `readdir`
        // per partition instead.
        if let Err(e) = self.raise_manifest_floor() {
            tracing::error!(
                error = %e,
                "ALARM: the side-manifest numbers on disc could not be read; the memberships stay \
                 WAL-durable and the log stays pinned, and the write is retried at the next tick"
            );
            return;
        }
        // What this publication wrote, per partition, so the resident memberships can move onto it
        // once every manifest naming one is durable (`LiveState::rehouse_memberships`).
        let mut written: Vec<(
            std::path::PathBuf,
            Vec<tessera_store::manifest::MembershipExtent>,
        )> = Vec::new();
        for (partition, partition_data) in &live.bundle.partitions {
            let mut manifest = partition_data.manifest.clone();
            write_deny_state(&mut manifest, &live.overlay);
            // The registry travels with this publication too, and not only with a flush. A
            // manifest naming memberships for a layer it does not declare is internally
            // inconsistent, and at open the layer's reserved runs are what turn an ordinal into
            // an entity, so the extents would be skipped whole and every artifact would come back
            // absent. The two are written together or the manifest is wrong.
            self.write_live_state(&mut manifest, &live.vocabularies);
            // Membership extents are written before the manifest that names them, which is the
            // whole of their durability contract: a manifest naming a missing extent refuses at
            // open, so the file has to be durable first. A failure here abandons the publication
            // rather than committing a manifest that omits them. An omission would read as no
            // artifact ever having been published, and rotation would then be free to reclaim the
            // log records holding the only other copy.
            // Allocated first so the extents can be named after the publication that carries them:
            // one sequence, not two, and a file whose name says which manifest introduced it.
            let n = self.take_manifest_n();
            let prefix_dir = self.prefix_dir(&live);
            let published = match self.write_membership_extents(&prefix_dir, partition, n) {
                Ok(published) => published,
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        partition = %partition,
                        "ALARM: could not write the artifact membership extents; the memberships \
                         stay WAL-durable and the log stays pinned, and the write is retried at the \
                         next tick"
                    );
                    return;
                }
            };
            self.membership_extents.extend(published.clone());
            manifest.membership_extents = self.membership_extents.clone();
            if !published.is_empty() {
                written.push((prefix_dir.clone(), published));
            }
            // Supplied content goes into the record blob, the store points already use, in
            // extents of its own but on the same list and behind the same reader. Artifact and
            // point entities are two regions growing towards each other, so they stay disjoint,
            // the rows never collide, and each side reads its tags against its own declaration.
            match self.write_content_extent(&prefix_dir, partition, live.bundle.partitions.len(), n)
            {
                // Assigned from the held list, never pushed onto the clone. The manifest this
                // publication started from is the stale generation's, so extending it would drop
                // every earlier publication's entry. An artifact whose content extent is un-named
                // comes back with its description unreadable and is withheld from every viewer,
                // with the log already released. The membership list above takes this posture for
                // the same reason.
                Ok(Some(extent)) => {
                    self.artifact_record_extents.push(extent);
                    manifest.artifact_record_extents = self.artifact_record_extents.clone();
                }
                Ok(None) => {
                    manifest.artifact_record_extents = self.artifact_record_extents.clone();
                }
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        partition = %partition,
                        "ALARM: could not write the artifact content extent; the content stays \
                         WAL-durable and the log stays pinned, and the write is retried at the next \
                         tick"
                    );
                    return;
                }
            }
            write_vocabulary_extensions(
                &mut manifest,
                &live.vocabularies,
                &live.bundle.manifest.vocabularies,
            );
            // The publication seam, per partition: the dispositions are WAL-durable either way,
            // so a kill parked here loses only the restore path's freshness.
            self.pause_point(PauseSiteArg::BeforeManifestPublish);
            if let Err(e) = self.commit_side_manifest(
                &partition_data.manifest,
                &self.prefix_dir(&live),
                partition,
                n,
                &mut manifest,
                None,
            ) {
                tracing::error!(
                    error = %e,
                    partition = %partition,
                    "ALARM: could not publish the overlay's deny state; it stays in force and \
                     WAL-durable, and the write is retried at the next drain close or tick. A \
                     restore taken meanwhile recovers the previously published state"
                );
                return;
            }
        }

        // Only now, with every partition's manifest durable, is the log free of these
        // memberships. Marking earlier would let rotation reclaim the records behind an extent a
        // crash could still lose. The content fills the same publication packed are released on
        // the same argument.
        self.live.mark_memberships_published();
        self.live.mark_content_published();
        // And the memberships move onto the extents this publication wrote, on the fold's rule
        // (`LiveState::rehouse_memberships`): a membership left on the heap is one the node
        // carries in anonymous memory for as long as it runs, for bytes it has just written and
        // holds open. This runs after the manifests, because a publication that failed above
        // leaves files no manifest names, and this is the point where every one of them is named.
        for (prefix_dir, extents) in &written {
            let (rehoused, kept) = self.live.rehouse_memberships(prefix_dir, extents);
            if kept > 0 {
                // Unreachable for the fold's reason exactly: the extent was packed from these
                // records, and an ordinal with no record is written as an empty blob the
                // rehousing skips.
                tracing::error!(
                    rehoused,
                    kept,
                    "ALARM: artifact memberships this publication wrote do not match the records \
                     they were written from; the extent the manifest now names and the level being \
                     served disagree for those ordinals"
                );
            }
        }

        self.deny_dirty = false;
        self.windows_since_publication = 0;
        self.health
            .overlay_publications
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Write every not-yet-published artifact's supplied content as one record-blob extent.
    ///
    /// The same store, format and reader as a point's blob-resident fields, so filter and search
    /// reach artifact properties the same way they reach a document's.
    ///
    /// The access rule differs, though the store is shared safely: a document's field is visible
    /// to whoever may see the document, an artifact's content to whoever contains its generating
    /// set entirely. The two never share an entity, so which rule governs a row is a range check
    /// on its id.
    pub(super) fn write_content_extent(
        &self,
        prefix_dir: &std::path::Path,
        partition: &str,
        partitions: usize,
        n: u64,
    ) -> tessera_store::Result<Option<tessera_store::manifest::RecordExtent>> {
        let rows = self.live.unpublished_content();
        if rows.is_empty() {
            return Ok(None);
        }
        // An artifact belongs to no partition, and this loop runs once per partition, so a second
        // partition would receive an extent holding the same artifact entities: two layers of one
        // record stack whose has-row bitmaps overlap, which the stack refuses outright.
        //
        // Refused rather than guessed: which partition should own an artifact's content is a
        // layout question a multi-partition bundle has to answer and nothing here can. No such
        // bundle exists today, which is why this is a refusal with an alarm rather than a design.
        if partitions > 1 {
            return Err(tessera_store::StoreError::MalformedBundle {
                detail: format!(
                    "this bundle has {partitions} partitions and an artifact belongs to none of \
                     them, so where its supplied content should be written is undecided; the \
                     publication is refused rather than writing the same rows into every partition"
                ),
            });
        }

        let extents_rel = format!("partitions/{partition}/attrs/record/extents");
        let dir = prefix_dir.join(&extents_rel);
        std::fs::create_dir_all(&dir).map_err(|source| tessera_store::StoreError::Io {
            path: dir.clone(),
            source,
        })?;
        let extent = tessera_store::manifest::RecordExtent {
            blocks: format!("{extents_rel}/artifacts-{n:06}.blocks.bin"),
            hasrow: format!("{extents_rel}/artifacts-{n:06}.hasrow.roaring"),
            directory: format!("{extents_rel}/artifacts-{n:06}.directory.arrow"),
        };
        let io = |path: &std::path::Path| {
            let path = path.to_path_buf();
            move |source| tessera_store::StoreError::Io {
                path: path.clone(),
                source,
            }
        };
        let blocks = prefix_dir.join(&extent.blocks);
        let hasrow = prefix_dir.join(&extent.hasrow);
        let directory = prefix_dir.join(&extent.directory);
        let mut writer = tessera_filter_write::RecordBlobWriter::create(
            &blocks,
            &hasrow,
            &directory,
            tessera_filter::RECORD_BLOCK_TARGET,
        )
        .map_err(io(&blocks))?;
        // Ascending by entity, which the blob's block directory requires. Artifact ids descend as
        // they are allocated, since the row-less region grows downward, so publication order is
        // exactly the wrong order here, and sorting is not an optimisation.
        let mut rows = rows;
        rows.sort_by_key(|(entity, _)| entity.raw());
        for (entity, fields) in rows {
            let entity = u32::try_from(entity.raw()).map_err(|_| {
                tessera_store::StoreError::MalformedBundle {
                    detail: format!(
                        "artifact entity {} does not fit the u32 entity space",
                        entity.raw()
                    ),
                }
            })?;
            let fields: Vec<tessera_filter::RecordFieldRef<'_>> = fields
                .iter()
                .map(|(tag, value)| tessera_filter::RecordFieldRef {
                    tag: *tag,
                    value: tessera_filter::RecordValueRef::Utf8(value),
                })
                .collect();
            writer.push_row(entity, &fields).map_err(io(&blocks))?;
        }
        writer.finish().map_err(io(&blocks))?;
        // `finish` syncs the blocks and not the two files that address them. The directory goes
        // out through an Arrow writer and the has-row bitmap through a plain write, so a crash
        // after the manifest is durable can leave either torn, and a torn addressing file refuses
        // the whole record stack at open, taking every point's blob-resident field with it. The
        // membership path syncs per file for the same reason; this one has to do it here because
        // the blob writer is shared with the flush, which syncs its extent another way.
        for path in [&hasrow, &directory] {
            let file = std::fs::File::open(path).map_err(io(path))?;
            file.sync_all().map_err(io(path))?;
        }
        tessera_store::fsync_dir(&dir)?;
        Ok(Some(extent))
    }

    /// Pack every not-yet-published membership into one extent per level and fsync it, returning
    /// the manifest entries.
    ///
    /// One file per level per publication. Publication is append-only, so an extent covers a
    /// contiguous ordinal range and no earlier extent is disturbed. A reader unions a level's
    /// extents and the fold rewrites them into one.
    pub(super) fn write_membership_extents(
        &self,
        prefix_dir: &std::path::Path,
        partition: &str,
        n: u64,
    ) -> tessera_store::Result<Vec<tessera_store::manifest::MembershipExtent>> {
        let (ready, skipped) = self.live.unpublished_memberships();
        for (layer, level) in skipped {
            // Unreachable while publication is append-only, and alarmed rather than asserted: an
            // extent addresses a dense ordinal range, so packing around a hole would shift every
            // later artifact's identity by one.
            tracing::error!(
                layer = %layer,
                level,
                "ALARM: a level has a hole below its ordinal high-water, so its memberships are \
                 not published; they stay WAL-durable and the log stays pinned"
            );
        }
        pack_membership_extents(prefix_dir, partition, n, ready)
    }

    /// Publication by rebase: apply a completed flush to the then-current generation rather than
    /// to the one it was planned against.
    ///
    /// The flush ran on the pool while this thread went on accepting ingest and denies, so the
    /// generation has moved. The rebase removes exactly the entity ids the flush consumed, never
    /// a range, which would take late arrivals with it, and appends the segment to whatever is
    /// live now.
    ///
    /// A flush planned against a superseded prefix is discarded: its row bases were computed
    /// against a row space that no longer exists.
    pub(super) fn publish_flush(&mut self, completed: crate::flush::CompletedFlush) {
        let mut mark = StageMark::now();
        if self.publish_flush_stages(completed, &mut mark) {
            // The publication cycle closes at the swap (`ExecutorHealth::publication`): the
            // generation carrying this unit's rows, fills and extents is the live one from here,
            // so the number being reached and the work being served are one event.
            self.health.close_publication_cycle();
        } else {
            // A discard leaves the unit's files orphaned and its inputs standing, so the cycle is
            // unpublished: it stays open, its request stays armed, and the next tick re-plans.
            self.health.fail_publication_cycle();
            // A discarded flush's time since its last lap, so `PublishWall` stays partitioned
            // whichever way the publication ends.
            self.health
                .flush_lap(crate::flush::FlushStage::Discarded, mark);
        }
    }

    /// The publication's stages, each lapped as it ends. Returns whether the flush swapped;
    /// `mark` is left at the last lap so the caller can charge a discard's tail.
    pub(super) fn publish_flush_stages(
        &mut self,
        completed: crate::flush::CompletedFlush,
        mark: &mut StageMark,
    ) -> bool {
        let started = std::time::Instant::now();
        // Same `may_publish` guard as `publish_merge`.
        if !self.may_publish() {
            return false;
        }
        // Every counted discard below is the same posture, so it is one closure rather than the
        // shape repeated. It owns what it reports, so it borrows nothing the sequence below needs.
        let discard = {
            let health = Arc::clone(&self.health);
            move |reason: &str| {
                health.flush_failures.fetch_add(1, Ordering::Relaxed);
                tracing::error!(
                    "ALARM: discarding a completed flush: {reason}. Its files are orphans, the \
                     buffer is retained, and the next tick re-plans"
                );
            }
        };
        let live = self.generation.load_full();
        if live.prefix != completed.prefix {
            // A compaction moved the prefix under this flush. Nothing to apply it to.
            tracing::warn!(
                planned = %completed.prefix,
                live = %live.prefix,
                "discarding a completed flush planned against a superseded prefix"
            );
            return false;
        }

        // A promoting flush's ordinals are positions, assigned as `dict.len() + i` against the
        // dictionary it planned against; `Dict::load` reproduces them only if its extent lands
        // where the flush assumed. Scoped to flushes that wrote an extent, since one that
        // promoted nothing carries only ordinals append-only extension preserves.
        //
        // The window is narrow and real: `flush_in_flight` clears only after the pool's sends,
        // and the executor drains completed flushes before it ticks, so a send landing between
        // the drain and the in-flight check leaves a tick planning against a generation whose
        // completed flush is not yet published.
        if dictionary_moved_under(completed.promoted_from_dict_len, live.dict.len()) {
            tracing::warn!(
                planned = completed.promoted_from_dict_len,
                live = live.dict.len(),
                "discarding a completed flush whose dictionary moved under it: the ordinals in \
                 its extent are positions, and they are no longer the positions it assigned"
            );
            return false;
        }

        // Assembled here, from the live partition manifest, and written before the swap. A
        // side-manifest carries complete current state, and current is decided now rather than
        // when the flush was planned: the deny fields come from the overlay this publication
        // carries, so a suppression accepted during the flush's flight is in the manifest the
        // flush publishes.
        let Some(partition_data) = live.bundle.partitions.get(&completed.partition) else {
            tracing::warn!(
                partition = %completed.partition,
                "discarding a completed flush for a partition this bundle no longer carries"
            );
            return false;
        };
        // Composed before the manifest is written, because a composition that refuses must not
        // leave a published manifest naming the extents it refused. The refusal is unreachable: an
        // extent covers entities that were just issued, which no earlier layer can hold. So this
        // is the same posture as every other flush failure: the files are orphans, the buffer
        // stands, the next tick re-plans.
        let extents: Vec<crate::filter::PublishedExtent> = completed
            .filter_extents
            .iter()
            .map(|e| {
                (
                    // The resolved name for a group-scoped family's column, the column's own for
                    // an entity-scoped one. One function, so a flush's layer composes under the
                    // key a leaf resolves to (`filter::extent_column_name`).
                    crate::filter::extent_column_name(&e.column, e.view.as_deref()),
                    e.values_rel.clone(),
                    Arc::clone(&e.values),
                    // A keyword extent's dictionary travels with its ordinals or the composition
                    // refuses: the ordinals are positions in this dictionary and name nothing
                    // against another.
                    e.dict.clone(),
                )
            })
            .collect();
        // The record extent composes onto the live stack here, not only into the manifest: a
        // published extent that no live stack holds answers no drill-down until the next fold.
        let record_dir = self.bundle_root.join(&completed.prefix);
        let record_paths: Vec<tessera_filter::RecordExtentPaths> = completed
            .record_extent
            .iter()
            .map(|e| record_extent_paths(&record_dir, e))
            .collect();
        let entity_terms_paths = vec![entity_terms_extent_paths(
            &record_dir,
            &completed.entity_terms_extent,
        )];
        let text_paths: Vec<crate::filter::TextExtentPaths> = completed
            .text_extents
            .iter()
            .map(|e| text_extent_paths(&record_dir, e))
            .collect();
        // The new columns first, then the extents that land on them. A flush of a view a family
        // had no column for wrote its base in the same unit as its extent, and the extent
        // composes onto a column, so the column has to exist before the composition below can
        // find it. Empty in every steady-state flush, where the base has been on disc since the
        // build.
        let live_columns = if completed.scoped_columns.is_empty() {
            Arc::clone(&live.filter_columns)
        } else {
            let partition_dir = record_dir.join("partitions").join(&completed.partition);
            // Stamped with the flush's own incarnation, which is what places the base it just
            // wrote.
            let opening: Vec<(String, String, tessera_types::view::ViewIncarnation)> = completed
                .scoped_columns
                .iter()
                .map(|(column, view)| (column.clone(), view.clone(), completed.incarnation))
                .collect();
            match live.filter_columns.with_scoped_columns(
                &partition_dir,
                &opening,
                &live.bundle.manifest.scoped_scalars(),
                &live.bundle.manifest.vocabularies,
                true,
            ) {
                Ok(columns) => Arc::new(columns),
                Err(e) => {
                    discard(&format!(
                        "it wrote a group-scoped column this process cannot open ({e}), and a \
                         manifest must not name a column no request could read"
                    ));
                    return false;
                }
            }
        };
        let filter_columns = match live_columns.with_extents(
            &extents,
            &record_paths,
            &entity_terms_paths,
            &text_paths,
        ) {
            Ok(columns) => Arc::new(columns),
            Err(e) => {
                discard(&format!(
                    "its filter extents would not compose onto the live columns ({e}), and a \
                     bundle published over that would answer filters wrongly"
                ));
                return false;
            }
        };
        *mark = self
            .health
            .flush_lap(crate::flush::FlushStage::Compose, *mark);

        let mut manifest = partition_data.manifest.clone();
        let manifest_n = match self.allocate_manifest_n() {
            Ok(n) => n,
            Err(e) => {
                discard(&format!(
                    "its side-manifest number could not be allocated ({e})"
                ));
                return false;
            }
        };
        // The watermark advances at every flush publication and never regresses: a publication
        // coordinate, not an entity count.
        //
        // `entity_hi + 1` of this view's flush is neither monotone nor sufficient once a bundle
        // has several views. Not monotone: views flush one per tick, so a view holding older
        // entities can publish after one holding newer ones and offer a lower number, which
        // `check_manifest_publishable` refuses. Not sufficient: fragment freshness is
        // `fragment.watermark >= generation.watermark` (`Engine::fragment_for`), so a publication
        // that did not move it would leave a session's fragment stale with nothing to say so.
        //
        // A values-only publication has no segment and so no entity high-water of its own; it
        // still advances the watermark, since it publishes extents a resident fragment must be
        // rebuilt past.
        manifest.watermark = completed
            .segment
            .as_ref()
            .map(|s| s.watermark)
            .unwrap_or(0)
            .max(manifest.watermark + 1);
        manifest.entity_id_high_water = manifest.entity_id_high_water.max(
            completed
                .segment
                .as_ref()
                .map(|s| s.entity_id_high_water)
                .unwrap_or(0),
        );
        // The row-less half of the same obligation. A flush is the routine publication, so it is
        // where a registration, a create or a declaration made since the last one stops depending
        // on the WAL surviving: rotation reclaims their records, and without this the mark and
        // everything it covers go with them.
        self.write_live_state(&mut manifest, &live.vocabularies);
        // And the group-scoped columns this flush gave a view its first of. Carried forward and
        // appended to, never restated: the list is what a restart recovers
        // `scoped_scalars[..].views` from, and a render-only family writes no extent for the
        // derivation to find. `manifest` is the live side-manifest cloned, so the earlier pairs
        // are already here.
        for (column, view) in &completed.scoped_columns {
            let entry = tessera_store::manifest::ScopedColumn {
                column: column.clone(),
                view: view.clone(),
                // The incarnation this flush wrote under. The list is carried forward
                // indefinitely, so an entry outlives the drop that orphaned its column; the stamp
                // is what keeps a key created again from publishing it as its own.
                incarnation: completed.incarnation,
            };
            if !manifest.scoped_columns.contains(&entry) {
                manifest.scoped_columns.push(entry);
            }
        }
        // The segment's own four manifest lists, taken together or not at all: a values-only
        // publication wrote none of the files they name.
        if let Some(segment) = &completed.segment {
            manifest.segments.push(segment.descriptor.clone());
            manifest.deltas.push(segment.tier_path.clone());
            manifest
                .external_id_runs
                .push(segment.external_id_run.clone());
            manifest
                .locator_extents
                .push(segment.locator_extent.clone());
        }
        manifest.files.extend(completed.files);
        if let Some(extent) = completed.dict_extent {
            manifest.dict_extents.push(extent);
        }
        // Named in the manifest as well as digested in `files`: the reader composes exactly what
        // this list names, so an extent on disk that no manifest names is not read and one named
        // but absent is a refusal to open (`FilterColumns::open`).
        manifest
            .attr_extents
            .extend(
                completed
                    .filter_extents
                    .iter()
                    .map(|e| tessera_store::manifest::AttrExtent {
                        column: e.column.clone(),
                        // `None` for an entity-scoped column, which belongs to no view: the
                        // incarnation follows the view exactly.
                        incarnation: e.view.as_ref().map(|_| completed.incarnation),
                        view: e.view.clone(),
                        values: e.values_rel.clone(),
                        presence: e.presence_rel.clone(),
                        // One record, so the layer's files swap as one: an extent's ordinals are
                        // positions in that extent's dictionary, and a reader that saw a new
                        // dictionary beside old ordinals would recolour the window.
                        dict: e.dict_rel.clone(),
                        postings: None,
                        offsets: None,
                    }),
            );
        // The record-blob extent, under the same two-obligation rule: the three files are already
        // in `files`, and this entry is what makes them reachable. A record stack opens exactly
        // what `record_extents` names, so bytes this list omits answer no drill-down, and bytes
        // it names but that are absent refuse the open.
        manifest.record_extents.extend(completed.record_extent);
        // The entity→term transpose's extent, under the same two-obligation rule and for the
        // sharper of the two reasons: a list this manifest omits leaves the flushed entities'
        // labels unknown, which serves a drill-down without them (harmless) *and* leaves the join
        // rule's label arm with nothing to compare against (a re-label accepted through a second
        // view's row). The live generation composes it below.
        manifest
            .entity_terms_extents
            .push(completed.entity_terms_extent.clone());
        // The text layers, under the same two-obligation rule: the files are already digested in
        // `files`, and this entry is what makes them reachable to a reopen. The live generation
        // composes them below: a published layer no live reader holds answers no `match` until
        // the next fold, so the record blob's own composition must do the same.
        manifest
            .text_extents
            .extend(completed.text_extents.iter().cloned());
        write_deny_state(&mut manifest, &live.overlay);
        write_vocabulary_extensions(
            &mut manifest,
            &live.vocabularies,
            &live.bundle.manifest.vocabularies,
        );
        *mark = self
            .health
            .flush_lap(crate::flush::FlushStage::Manifest, *mark);

        // The commit point, and it is still the manifest; only the thread moved. A failure here
        // discards the flush: its files become orphans nothing references, the buffer is
        // retained, the next tick re-plans. The same posture as every other flush failure, and
        // the reason the write precedes the swap.
        //
        // And therefore the publication seam: the segment's files are on disc, the WAL still holds
        // every row they carry, and nothing durable names them until this write returns.
        self.pause_point(PauseSiteArg::BeforeManifestPublish);
        if let Err(e) = self.commit_side_manifest(
            &partition_data.manifest,
            &self.prefix_dir(&live),
            &completed.partition,
            manifest_n,
            &mut manifest,
            None,
        ) {
            discard(&format!("its side-manifest could not be committed ({e})"));
            return false;
        }
        *mark = self
            .health
            .flush_lap(crate::flush::FlushStage::Commit, *mark);

        // Stamped with the flush's own incarnation for `Manifest::with_scoped_columns`, which
        // publishes a pair only where it is the live one.
        let scoped_columns: Vec<(String, String, tessera_types::view::ViewIncarnation)> = completed
            .scoped_columns
            .iter()
            .map(|(column, view)| (column.clone(), view.clone(), completed.incarnation))
            .collect();
        let published = tessera_store::read::PublishedManifest {
            manifest,
            n: manifest_n,
        };
        // A values-only publication substitutes the manifest and leaves the row space alone. It
        // wrote no segment, so there is nothing to rebase and nothing that could fail to; what it
        // publishes is the value extents its manifest now names.
        let (seg_id, shape_pieces, tier, tier_tally, next_bundle) = match completed.segment {
            None => {
                let bundle = match live.bundle.with_manifest(&completed.partition, published) {
                    Ok(bundle) => bundle,
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "discarding a completed values-only flush whose partition this bundle \
                             no longer carries"
                        );
                        return false;
                    }
                };
                (None, Vec::new(), None, None, bundle)
            }
            Some(segment) => {
                let seg_id = segment.segment.seg_id.clone();
                let bundle = match live.bundle.with_segment(
                    &completed.partition,
                    &completed.view,
                    segment.segment,
                    segment.extent,
                    published,
                ) {
                    Ok(bundle) => bundle,
                    Err(e) => {
                        // The row space moved under this flush: another publication landed
                        // between the plan and here. Discarded, not forced: forcing would put the
                        // segment at a `row_base` that is no longer the end of row space, aliasing
                        // rows.
                        tracing::warn!(
                            error = %e,
                            "discarding a completed flush that no longer rebases"
                        );
                        return false;
                    }
                };
                (
                    Some(seg_id),
                    segment.shape_pieces,
                    Some(segment.tier),
                    Some(segment.tier_tally),
                    bundle,
                )
            }
        };
        // And the family's own list gains the view this flush wrote a base for.
        // `scoped_scalars[..].views` names the views that have a column, so a view that has just
        // acquired one has to enter it. A client reading the list would otherwise conclude the
        // column it is being served does not exist, and the next restart's opener would not open
        // it at all.
        let next_bundle = if scoped_columns.is_empty() {
            next_bundle
        } else {
            let manifest = next_bundle.manifest.with_scoped_columns(&scoped_columns);
            next_bundle.with_views(manifest)
        };
        *mark = self
            .health
            .flush_lap(crate::flush::FlushStage::WithSegment, *mark);

        // The segment's shape memberships go into the held forms below, with the stored levels'
        // rows. The pool resolved them against the levels as held when the flush was planned; a
        // publication into a shape layer since then rebuilt that level, and rows resolved over
        // the old shapes would extend a form that no longer describes them. So a piece is taken
        // where its level is the one now held, and the segment is resolved again against the
        // current level where the two differ: one segment, on this thread, in the window a shape
        // publication and a flush overlap.
        *mark = self
            .health
            .flush_lap(crate::flush::FlushStage::ShapesInstall, *mark);

        // Exactly what was consumed, from the then-current buffer. This is O(buffered) work on
        // this thread, so one such stall lands ahead of the deny lane per tick: a cost this
        // design adds rather than one it avoids.
        let mut buffer = (*live.buffer).clone();
        for entity in &completed.consumed {
            // By (entity, view), never by entity. A flush consumes one view's rows; an entity
            // that also holds a row awaiting flush in another view keeps it, and removing the
            // entity outright would lose a row that is in no segment and no buffer.
            buffer.remove_in_view(*entity, &completed.view);
        }
        // The fills this flush's plan consumed, on the same rule. Every fill it looked at, not
        // only the ones it wrote: a fill a restart re-buffered after its own flush writes nothing
        // and must still leave the buffer, or it pins the log at its `ValuesBatch` record
        // indefinitely.
        for entity in &completed.filled {
            buffer.remove_fill(*entity);
        }
        for (entity, owner_view) in &completed.filled_scoped {
            buffer.remove_scoped_fill(*entity, owner_view);
        }
        // The gauge follows the buffer here too. A flush is the other place occupancy changes: if
        // it were not stated here, a node that flushed and then took no ingest would report a
        // backlog it had already written, and `/control/ingest`'s occupancy bound is measured
        // against this figure.
        self.health
            .buffered_items
            .store(buffer.len(), Ordering::SeqCst);
        *mark = self
            .health
            .flush_lap(crate::flush::FlushStage::BufferRebase, *mark);

        let watermark = next_bundle
            .partitions
            .get(&completed.partition)
            .map(|p| p.manifest.watermark)
            .unwrap_or(live.watermark);
        let segments_version = live.segments_version + 1;
        let mut delta_postings = live.delta_postings.clone();
        delta_postings.extend(tier);

        *mark = self
            .health
            .flush_lap(crate::flush::FlushStage::Denied, *mark);

        // And every held row form of the view gains this segment's rows, before the swap. A form
        // covers the whole row space, so a segment nothing added to it would leave every artifact
        // one segment short: a member ingested into an artifact counting for nobody until the
        // next fold. A stored level takes one `project_extents_from` per artifact over the
        // entities inside this extent's own range; a spatial level takes the segment's resolution
        // the pool produced above; both on this thread, against the whole-level projection the
        // alternative puts on the next request.
        //
        // A values-only publication added no rows, so it extends no form: `seg_id` is `None` and
        // the row space it publishes is the one it found.
        if let (Some(previous), Some(space)) = (
            view_row_space(&live.bundle, &completed.partition, &completed.view),
            view_row_space(&next_bundle, &completed.partition, &completed.view),
        ) {
            let segment = seg_id.as_ref().and_then(|seg_id| {
                next_bundle
                    .partitions
                    .get(&completed.partition)
                    .and_then(|p| p.views.get(&completed.view))
                    .and_then(|v| v.segments.iter().find(|s| &s.seg_id == seg_id))
                    .map(|s| s.as_ref())
            });
            self.live.with_artifacts(|store| {
                let rows_of = |layer: &str, level: u32| {
                    self.segment_rows_of(
                        &completed.view,
                        layer,
                        level,
                        segment,
                        &shape_pieces,
                        store,
                    )
                };
                self.artifact_projections.extend_flushed(
                    &live.prefix,
                    &completed.view,
                    store,
                    previous,
                    space,
                    segments_version,
                    &rows_of,
                )
            });
        }
        *mark = self
            .health
            .flush_lap(crate::flush::FlushStage::Artifacts, *mark);

        let next = Arc::new(live.with(|g| {
            // The live columns with this flush's extents composed on: the whole of what makes an
            // entity ingested since the build answer a filter on its own value.
            g.filter_columns = filter_columns;
            g.segments_version = segments_version;
            g.watermark = watermark;
            g.bundle = next_bundle;
            g.dict = completed.dict;
            g.delta_postings = delta_postings;
            g.buffer = Arc::new(buffer);
        }));
        // Armed before the swap, and that ordering is the mechanism. A request landing between
        // the swap and the pool task's first insert must find the flag set, or it takes the cost
        // of a full rebuild, where the whole design is that it be shed with a 429 for the bounded
        // duration of the refresh instead.
        // The claim names the generation it is for, so a pass that is superseded mid-flight
        // releases nothing when it ends; see `refresh::clear_if_current`.
        self.refresh
            .in_flight
            .store(segments_version, Ordering::SeqCst);
        self.publish_arc(Arc::clone(&next), started);
        self.refresh.spawn(next);

        // A flush supersedes geometry, so it prunes exactly as any other geometry publication
        // does: one swap, one `segments_version` bump, one retention pass. The superseded
        // generation itself is held by nothing but the requests already in flight against it.
        self.row_projection_cache
            .prune_generations_below(segments_version.saturating_sub(KEEP_SUPERSEDED_GENERATIONS));
        self.prune_region_cache(segments_version);
        self.health.flushes.fetch_add(1, Ordering::Relaxed);
        self.health
            .flush_rows_published
            .fetch_add(completed.consumed.len() as u64, Ordering::Relaxed);
        // The drain sample the buffer-occupancy 429 is derived from.
        self.health.record_flush_published(completed.consumed.len());
        if let Some(tally) = tier_tally {
            self.health.record_tier_fragmentation(tally);
        }
        *mark = self.health.flush_lap(crate::flush::FlushStage::Swap, *mark);

        self.rotate_wal();
        *mark = self
            .health
            .flush_lap(crate::flush::FlushStage::Rotate, *mark);
        // The superseded generation is freed here, under a stage, rather than at the return.
        // `live` is its last reference once the swap has happened (a request in flight holds its
        // own, and then the free lands on that thread instead), and its buffer holds every row
        // that was buffered before the swap: an O(buffered) free on this thread, beside the
        // O(buffered) clone `BufferRebase` measures.
        drop(live);
        drop(completed.consumed);
        self.health
            .flush_lap(crate::flush::FlushStage::DropSuperseded, *mark);
        true
    }

}
