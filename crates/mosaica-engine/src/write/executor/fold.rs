use super::*;

/// Memory this process could take without reclaiming anything it needs, in bytes. `None` where
/// unknowable.
///
/// The smaller of `MemAvailable` and a cgroup v2 `memory.max`: either can be the real bound.
pub(super) fn available_memory() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let available = meminfo
        .lines()
        .find(|line| line.starts_with("MemAvailable:"))
        .and_then(|line| line.split_whitespace().nth(1)?.parse::<u64>().ok())
        .map(|kib| kib * 1024)?;

    // "max" (unlimited) parses to `None`, leaving `MemAvailable` as the answer.
    let cgroup = std::fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|cgroup| {
            let path = cgroup
                .lines()
                .next()?
                .split(':')
                .nth(2)?
                .trim_start_matches('/');
            std::fs::read_to_string(format!("/sys/fs/cgroup/{path}/memory.max")).ok()
        })
        .and_then(|max| max.trim().parse::<u64>().ok());

    Some(cgroup.map_or(available, |limit| limit.min(available)))
}

/// The levels a fold's retirement is about to move, and the set it retires.
///
/// A fold writes its manifest before it retires, since the retirement is not reversible: a
/// manifest that would not commit must leave it undone. [`Self::records`] composes a level's
/// records without the retired artifacts, and [`Self::version_after`] stamps them with the version
/// the level will carry once the retirement has run. A containment partition and a spatial level's
/// row forms are omitted for a pending level and recompose on first use.
pub(super) struct PendingRetirement {
    levels: Vec<(String, u32)>,
    retired: croaring::Bitmap,
}

impl PendingRetirement {
    fn is_pending(&self, layer: &str, level: u32) -> bool {
        self.levels.iter().any(|(l, v)| l == layer && *v == level)
    }

    /// The version `layer`'s `level` will have once this fold's retirement has run.
    pub(super) fn version_after(&self, store: &ArtifactStore, layer: &str, level: u32) -> u64 {
        store.level_version(layer, level) + u64::from(self.is_pending(layer, level))
    }

    /// The level's records as the retirement will leave them: every artifact but those whose own
    /// entity is retired.
    fn records<'s>(
        &'s self,
        store: &'s ArtifactStore,
        layer: &str,
        level: u32,
    ) -> impl Iterator<Item = (u32, &'s mosaica_lifecycle::membership::ArtifactRecord)> + 's {
        let retired = &self.retired;
        store
            .level(layer, level)
            .filter(move |(_, record)| !retired.contains(record.entity.raw() as u32))
    }
}

/// How many ordinals a level's derived structures cover: one past the highest live ordinal,
/// which is the length the reader sizes the level at (`ArtifactRows::build_over`). A hole below
/// it is covered and a hole at the top is not.
fn level_length<'a>(
    records: impl Iterator<Item = (u32, &'a mosaica_lifecycle::membership::ArtifactRecord)>,
) -> u32 {
    records.map(|(ordinal, _)| ordinal + 1).max().unwrap_or(0)
}

/// What a fold hands the manifest commit in place of the held list: the derived files it has just
/// written, and the retirement its levels are stamped for.
pub(super) struct FoldDerived<'a> {
    pub(super) written: &'a [mosaica_store::manifest::DerivedExtent],
    pub(super) pending_retirement: &'a PendingRetirement,
    /// The ids this fold frees, held back from the WAL position beside them.
    pub(super) freed: &'a mosaica_store::manifest::HeldEntities,
}

/// The entities of `executed` the allocator may issue again: each one whose number, the entity
/// its item was first given, is another. An item's number is never freed, so a `mosaica_id` a
/// client holds never comes to name another item.
fn freed_by(
    live: &Generation,
    executed: &croaring::Bitmap,
) -> Result<croaring::Bitmap, mosaica_store::StoreError> {
    const CHUNK: usize = 1 << 16;
    let mut freed = croaring::Bitmap::new();
    let mut entities: Vec<EntityId> = Vec::with_capacity(CHUNK);
    let mut iter = executed.iter().peekable();
    while iter.peek().is_some() {
        entities.clear();
        entities.extend(
            iter.by_ref()
                .take(CHUNK)
                .map(|entity| EntityId::new(u64::from(entity))),
        );
        let numbers = crate::edited::numbers_of(live, &entities)?;
        for (entity, number) in entities.iter().zip(numbers) {
            if number != *entity {
                freed.add(entity.raw() as u32);
            }
        }
    }
    Ok(freed)
}

/// The fold-written files whose stamped version is the level's now, after the fold's retirement
/// has run; every other one is dropped and named. See [`PendingRetirement`].
fn held_at_current_version(
    store: &ArtifactStore,
    entries: &[mosaica_store::manifest::DerivedExtent],
) -> Vec<mosaica_store::manifest::DerivedExtent> {
    entries
        .iter()
        .filter(|entry| {
            let now = store.level_version(&entry.layer, entry.level);
            if now == entry.level_version {
                return true;
            }
            tracing::error!(
                layer = %entry.layer,
                level = entry.level,
                form = entry.form.dir(),
                stamped = entry.level_version,
                now,
                "ALARM: a fold-written derived file is stamped with a version the level does not \
                 carry after the retirement; it is dropped and the level recomposes on first use"
            );
            false
        })
        .cloned()
        .collect()
}

/// Delete every `v#####` tree under the bundle root that `CURRENT` does not name, once, before
/// the executor thread is spawned.
///
/// Must run before the thread starts: once serving begins, a live generation may still hold a
/// prefix `CURRENT` does not name, and [`Executor::pending_reclaim`] reclaims that instead.
pub(in crate::write) fn sweep_orphan_prefixes(bundle_root: &Path, live: &str) {
    let Ok(entries) = std::fs::read_dir(bundle_root) else {
        return;
    };
    let (mut swept, mut refused) = (0usize, 0usize);
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        // A prefix is `v` + five-or-more digits (`next_prefix_name`'s own shape).
        let is_prefix = name
            .strip_prefix('v')
            .is_some_and(|digits| digits.len() >= 5 && digits.bytes().all(|b| b.is_ascii_digit()));
        if !is_prefix || name == live || !entry.path().is_dir() {
            continue;
        }
        match mosaica_store::reclaim_prefix(&entry.path()) {
            Ok(()) => {
                swept += 1;
                tracing::info!(
                    prefix = %name,
                    "startup sweep: reclaimed an orphaned prefix left by a discarded fold or an \
                     exit mid-publication"
                );
            }
            Err(e) => {
                refused += 1;
                tracing::warn!(
                    prefix = %name,
                    error = %e,
                    "startup sweep: could not reclaim an orphaned prefix; it stands, and its disc \
                     with it"
                );
            }
        }
    }
    if swept > 0 || refused > 0 {
        tracing::info!(live = %live, swept, refused, "startup sweep complete");
    }
}

/// What is on disc under `prefix_dir` against what `generation`'s manifests still name. `None`
/// where the tree cannot be walked.
///
/// `named` sums both the bundle-level `MANIFEST.json` and the partition's `SEGMENTS-<n>.json`;
/// the build's own artefacts are in the first and would be missed by reading only the second.
pub(super) fn dead_bytes_of(prefix_dir: &Path, generation: &Generation) -> Option<crate::compact::DeadBytes> {
    pub(super) fn walk(dir: &Path, total: &mut u64) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                walk(&entry.path(), total);
            } else {
                *total += meta.len();
            }
        }
    }
    if !prefix_dir.is_dir() {
        return None;
    }
    let mut on_disc = 0u64;
    walk(prefix_dir, &mut on_disc);
    let named: u64 = generation
        .bundle
        .manifest
        .files
        .values()
        .map(|digest| digest.size)
        .chain(
            generation
                .bundle
                .partitions
                .values()
                .flat_map(|partition| partition.manifest.files.values())
                .map(|digest| digest.size),
        )
        .sum();
    Some(crate::compact::DeadBytes { on_disc, named })
}

/// Free bytes on the filesystem holding `path`, or `None` where unknowable.
///
/// Reads `f_bavail`, not `f_bfree`: reserved-for-root blocks are not space a fold may plan to use.
#[cfg(unix)]
pub(super) fn free_disc(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stats) } != 0 {
        return None;
    }
    Some(stats.f_bavail as u64 * stats.f_frsize as u64)
}

#[cfg(not(unix))]
pub(super) fn free_disc(_path: &Path) -> Option<u64> {
    None
}

/// The largest live segment count across this generation's views: the maximum, not the sum, since
/// a viewport in one view pays only that view's own segment count.
pub(super) fn live_segments_of(generation: &Generation) -> usize {
    generation
        .bundle
        .partitions
        .values()
        .flat_map(|partition| partition.views.values())
        .map(|view| view.segments.len())
        .max()
        .unwrap_or(0)
}

/// Whether every artefact a completed fold consumed is still listed in the live manifest.
pub(super) fn fold_rebases(
    plan: &crate::compact::FoldPlan,
    live_manifest: &SegmentsManifest,
    consumed_segments: &FxHashSet<(&str, &str)>,
) -> bool {
    let listed_segments: FxHashSet<(&str, &str)> = live_manifest
        .segments
        .iter()
        .map(|d| (d.view.as_str(), d.seg_id.as_str()))
        .collect();
    consumed_segments.is_subset(&listed_segments)
        && plan.tiers.iter().all(|t| live_manifest.deltas.contains(t))
        && plan.attr_extents.iter().all(|consumed| {
            live_manifest
                .attr_extents
                .iter()
                .any(|extent| extent.values == consumed.values)
        })
        && plan.record_extents.iter().all(|consumed| {
            live_manifest
                .record_extents
                .iter()
                .any(|extent| extent.blocks == consumed.blocks)
        })
        && plan.text_extents.iter().all(|consumed| {
            live_manifest
                .text_extents
                .iter()
                .any(|extent| extent.dict == consumed.dict)
        })
        && plan.entity_terms_extents.iter().all(|consumed| {
            live_manifest
                .entity_terms_extents
                .iter()
                .any(|extent| extent.terms == consumed.terms)
        })
        // An index removed during the flight takes nothing; one still held must still list every
        // run the fold consumed.
        && {
            let listed: FxHashSet<&str> = live_manifest.edited_items.files().collect();
            plan.edited.files().all(|rel| listed.contains(rel))
        }
        && plan.unique.iter().all(|consumed| {
            live_manifest
                .unique_indexes
                .iter()
                .find(|index| index.attribute == consumed.attribute)
                .is_none_or(|index| {
                    let listed: FxHashSet<&str> = index.files().collect();
                    consumed.files().all(|rel| listed.contains(rel))
                })
        })
}

/// What [`Executor::fold_still_applies`] found: what the fold carries forward from the live
/// manifest, the deletions it retires, and how many entities the carried extents hold.
struct FoldApplies<'a> {
    forward: CarriedExtents<'a>,
    executed: croaring::Bitmap,
    carried_entities: u64,
}

/// The `(layer, level)`s whose observed memberships overlap, which have no label column.
type Overlapping = std::collections::BTreeSet<(String, u32)>;

/// What every derived writer of one fold shares: where the files go, the row spaces and segments
/// the fold has just written, the levels its retirement is about to move, and the file counter.
struct DerivedPass<'a> {
    prefix_dir: &'a std::path::Path,
    partition: &'a str,
    n: u64,
    incarnations: FxHashMap<String, mosaica_types::view::ViewIncarnation>,
    spaces: Vec<(String, mosaica_store::RowSpace)>,
    segments: Vec<(String, mosaica_store::read::SegmentData)>,
    pending: &'a PendingRetirement,
    index: mosaica_store::derived::DerivedIndex,
}

/// What a fold carries forward: everything the live manifest still lists that the fold did not
/// consume, i.e. what published during its flight.
///
/// Listed order is preserved: for segments it is entity order, required by
/// `RowSpace::with_extent`. Nothing of a
/// dead incarnation is carried; a dropped view's segments are left behind with the superseded
/// prefix's files instead of naming an incarnation the new manifest does not declare.
pub(super) struct CarriedExtents<'a> {
    segments: Vec<&'a mosaica_store::manifest::SegmentDescriptor>,
    tiers: Vec<String>,
    attrs: Vec<mosaica_store::manifest::AttrExtent>,
    records: Vec<mosaica_store::manifest::RecordExtent>,
    texts: Vec<mosaica_store::manifest::TextExtent>,
    entity_terms: Vec<mosaica_store::manifest::EntityTermsExtent>,
    /// Every unique index the live manifest holds, less what the fold consumed: a folded
    /// column's base runs are the fold's own and are left out here.
    unique: Vec<mosaica_store::manifest::UniqueIndexRuns>,
    /// The edited-items live runs written during the fold's flight; the fold's own are its base.
    edited: mosaica_store::manifest::EditedItemsRuns,
    /// The dropped views whose segments were left behind, and how many segments that was.
    omitted_views: Vec<String>,
    omitted_segments: usize,
}

/// The entries of `live` the fold did not consume, in listed order, keyed by file path.
pub(super) fn not_consumed<T: Clone>(
    live: &[T],
    consumed: &FxHashSet<&str>,
    key: impl Fn(&T) -> &str,
) -> Vec<T> {
    live.iter()
        .filter(|entry| !consumed.contains(key(entry)))
        .cloned()
        .collect()
}

pub(super) fn carried_forward<'a>(
    plan: &crate::compact::FoldPlan,
    live_manifest: &'a SegmentsManifest,
    live_incarnations: &FxHashMap<&str, mosaica_types::view::ViewIncarnation>,
    consumed_segments: &FxHashSet<(&str, &str)>,
) -> CarriedExtents<'a> {
    let mut omitted_views: Vec<String> = Vec::new();
    let mut omitted_segments = 0usize;
    let segments: Vec<&mosaica_store::manifest::SegmentDescriptor> = live_manifest
        .segments
        .iter()
        .filter(|d| !consumed_segments.contains(&(d.view.as_str(), d.seg_id.as_str())))
        .filter(|d| {
            let live = live_incarnations.get(d.view.as_str()) == Some(&d.incarnation);
            if !live {
                omitted_segments += 1;
                if !omitted_views.iter().any(|v| v == &d.view) {
                    omitted_views.push(d.view.clone());
                }
            }
            live
        })
        .collect();
    let mut attrs = not_consumed(
        &live_manifest.attr_extents,
        &plan
            .attr_extents
            .iter()
            .map(|extent| extent.values.as_str())
            .collect(),
        |extent| extent.values.as_str(),
    );
    attrs.retain(|extent| {
        crate::filter::carries_live_view(
            &|view| live_incarnations.get(view).copied(),
            extent.view.as_deref(),
            extent.incarnation,
        )
    });
    let mut texts = not_consumed(
        &live_manifest.text_extents,
        &plan
            .text_extents
            .iter()
            .map(|extent| extent.dict.as_str())
            .collect(),
        |extent| extent.dict.as_str(),
    );
    texts.retain(|extent| {
        crate::filter::carries_live_view(
            &|view| live_incarnations.get(view).copied(),
            extent.view.as_deref(),
            extent.incarnation,
        )
    });
    CarriedExtents {
        segments,
        tiers: live_manifest
            .deltas
            .iter()
            .filter(|t| !plan.tiers.contains(t))
            .cloned()
            .collect(),
        attrs,
        records: not_consumed(
            &live_manifest.record_extents,
            &plan
                .record_extents
                .iter()
                .map(|extent| extent.blocks.as_str())
                .collect(),
            |extent| extent.blocks.as_str(),
        ),
        texts,
        entity_terms: not_consumed(
            &live_manifest.entity_terms_extents,
            &plan
                .entity_terms_extents
                .iter()
                .map(|extent| extent.terms.as_str())
                .collect(),
            |extent| extent.terms.as_str(),
        ),
        unique: live_manifest
            .unique_indexes
            .iter()
            .map(|index| {
                match plan.unique.iter().find(|folded| folded.attribute == index.attribute) {
                    None => index.clone(),
                    Some(folded) => {
                        let consumed: FxHashSet<&str> = folded.files().collect();
                        mosaica_store::manifest::UniqueIndexRuns {
                            attribute: index.attribute.clone(),
                            base: Vec::new(),
                            live: index
                                .live
                                .iter()
                                .filter(|rel| !consumed.contains(rel.as_str()))
                                .cloned()
                                .collect(),
                        }
                    }
                }
            })
            .collect(),
        edited: {
            let consumed: FxHashSet<&str> = plan.edited.files().collect();
            let unconsumed =
                |runs: &mosaica_store::manifest::KeyRuns| mosaica_store::manifest::KeyRuns {
                    base: Vec::new(),
                    live: runs
                        .live
                        .iter()
                        .filter(|rel| !consumed.contains(rel.as_str()))
                        .cloned()
                        .collect(),
                };
            mosaica_store::manifest::EditedItemsRuns {
                by_number: unconsumed(&live_manifest.edited_items.by_number),
                by_entity: unconsumed(&live_manifest.edited_items.by_entity),
            }
        },
        omitted_views,
        omitted_segments,
    }
}

/// Exactly the files the new manifest names that the fold did not write, deduplicated: linking one
/// path twice is what `hard_link_forward` refuses.
pub(super) fn carried_files(
    partition: &str,
    live_manifest: &SegmentsManifest,
    bundle_files: &std::collections::BTreeMap<String, mosaica_store::manifest::FileDigest>,
    forward: &CarriedExtents,
) -> std::collections::BTreeSet<String> {
    // A segment an earlier fold carried has its files listed in the bundle's manifest.
    let listed =
        |rel: &str| live_manifest.files.contains_key(rel) || bundle_files.contains_key(rel);
    let mut rels: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for descriptor in &forward.segments {
        let segment_prefix = format!(
            "partitions/{}/{}/segments/{}",
            partition,
            mosaica_store::view_rel(&descriptor.view),
            descriptor.seg_id
        );
        for name in mosaica_store::SEGMENT_FILES {
            rels.insert(format!("{segment_prefix}/{name}"));
        }
        let presence_prefix = format!("{segment_prefix}/{}/", RENDER_PRESENCE_DIR);
        rels.extend(
            live_manifest
                .files
                .keys()
                .chain(bundle_files.keys())
                .filter(|rel| rel.starts_with(&presence_prefix))
                .cloned(),
        );
        let edited_rows = format!(
            "{segment_prefix}/{}",
            mosaica_store::edited::EDITED_ROWS_FILE
        );
        if listed(&edited_rows) {
            rels.insert(edited_rows);
        }
    }
    rels.extend(forward.tiers.iter().cloned());
    for extent in &forward.attrs {
        rels.extend(extent.files().map(String::from));
    }
    for extent in &forward.records {
        rels.extend(extent.files().map(String::from));
    }
    for extent in &forward.texts {
        rels.extend(extent.files().map(String::from));
    }
    for extent in &forward.entity_terms {
        rels.extend(extent.files().map(String::from));
    }
    for index in &forward.unique {
        rels.extend(index.files().map(String::from));
    }
    rels.extend(forward.edited.files().map(String::from));
    rels.extend(live_manifest.dict_extents.iter().map(|e| e.path.clone()));
    rels
}

/// Every `(layer, level)` the artifact store holds.
pub(super) fn levels_of(store: &ArtifactStore) -> impl Iterator<Item = (String, u32)> + '_ {
    store
        .levels_and_extents()
        .map(|(layer, level, _)| (layer.to_string(), level))
}

impl Executor {
    /// Whether [`crate::compact::CompactionSchedule`] calls for a fold now.
    ///
    /// The dead-bytes gauge is a directory walk and is passed as a closure: `due` calls it only
    /// after every cheaper gauge has declined. It walks the live prefix, not the bundle root; an
    /// orphaned prefix is the startup sweep's to reclaim, not a fold's.
    pub(super) fn scheduled_fold(&self, generation: &Arc<Generation>) -> Option<crate::compact::FoldTrigger> {
        let now = unix_now()?;
        let live_rows: u64 = generation
            .bundle
            .partitions
            .values()
            .flat_map(|partition| partition.manifest.segments.iter())
            .map(|descriptor| u64::from(descriptor.row_count))
            .sum();
        crate::compact::due(
            &self.deps.compaction,
            now,
            self.fold_floor_from(),
            crate::compact::Gauges {
                live_segments: live_segments_of(generation),
                retirable_deletions: generation.overlay.deleted_len(),
                live_rows,
            },
            || dead_bytes_of(&self.prefix_dir(generation), generation),
        )
    }

    /// The instant `compaction_min_interval_secs` is measured from: a fold may start no sooner
    /// than the interval after the previous one started, and never before the previous one has
    /// ended. Measuring from the end alone would drift the schedule by the fold's own duration;
    /// measuring from the start alone would let a fold that discards re-dispatch immediately.
    pub(super) fn fold_floor_from(&self) -> Option<u64> {
        let started = self.last_fold_start_unix?;
        let ended = self.health.fold_ended_unix.load(Ordering::SeqCst);
        Some(started.max(ended.saturating_sub(self.deps.compaction.min_interval_secs)))
    }

    /// Plan a fold and start it on its own thread, if one is requested and nothing blocks it. Not
    /// on the shared rayon pool: a fold's input is the corpus, and occupying request-serving
    /// workers for its duration would let maintenance compete with the product it serves.
    ///
    /// A refusal (a latched gate) consumes the request flag; a suspension (a merge or coalesce
    /// outstanding) leaves it armed for the next tick (see [`Executor::fold_outstanding`]).
    pub(super) fn dispatch_fold(&mut self, generation: &Arc<Generation>) {
        // A fold this node could not publish is hours of IO spent to produce an orphan. The
        // planner's own gates cover the recoverable postures; this one covers the two that latch.
        if !self.may_publish() {
            return;
        }
        // A request arriving while a fold runs is refused, not held. Consuming the flag makes
        // that true: left armed, it is also a wake reason (`tick_if_due`), so every completion
        // poll for the running fold's remaining hours would take a full tick and plan a flush off
        // it, collapsing the publication cadence to the poll interval.
        if self.fold.in_flight() {
            if self.health.fold_requested.swap(false, Ordering::SeqCst) {
                tracing::warn!(
                    "a compaction fold was requested while one is already running; refused rather \
                     than queued. At most one fold is in flight, and the running one will \
                     re-evaluate the gauges when it lands"
                );
            }
            return;
        }
        // A completed fold not yet drained excludes a second one, but unlike the refusal above
        // this consumes nothing: publication is one pass away, so a request left armed is
        // answered by the next tick rather than dropped.
        if self.fold_outstanding() {
            return;
        }
        // A request dispatches on its own terms, whatever hour it is and whatever the gauges
        // read, so the schedule is not consulted for one. Evaluating both would dispatch exactly
        // the same fold, and the only difference would be logging a requested fold under whichever
        // gauge happened to agree, in the line an operator reads to find out why the corpus was
        // rewritten.
        let requested = self.health.fold_requested.load(Ordering::SeqCst);
        let scheduled = if requested {
            None
        } else {
            self.scheduled_fold(generation)
        };
        if !requested && scheduled.is_none() {
            return;
        }
        // At most one fold, and none while a merge or a coalesce is outstanding (running, or
        // completed and not yet published, see [`Executor::fold_outstanding`]). Their outputs
        // would be orphaned by the flip and their inputs are the fold's, so starting now would
        // mean re-reading the corpus to discard it at the rebase check.
        if self.merge_outstanding() || self.coalesce_outstanding() {
            return;
        }
        // A unique declaration's rounds read the layers and runs a fold rewrites.
        if self.unique_declarations.busy() {
            return;
        }

        let plan = match crate::compact::plan_fold(
            generation,
            self.log.wal.is_poisoned(),
            self.health.overlay_diverged.load(Ordering::SeqCst),
            crate::compact::FoldResources {
                available_memory: available_memory(),
                free_disc: free_disc(&self.deps.bundle_root),
                membership_containers: self
                    .live
                    .with_artifacts(|store| store.membership_containers()),
            },
        ) {
            Ok(plan) => plan,
            Err(reason) => {
                self.health.fold_requested.store(false, Ordering::SeqCst);
                self.health.record_fold_refusal(reason);
                tracing::warn!(
                    gate = ?reason,
                    "a compaction fold was requested and refused: this node folds nothing in \
                     this state, and the request is answered rather than retried"
                );
                return;
            }
        };

        let manifest = &generation.bundle.manifest;
        // One schema per view: the bundle-wide render tail plus that view's group-scoped lanes.
        let scalar_schema: std::collections::BTreeMap<String, Vec<(String, ScalarType)>> = plan
            .views
            .iter()
            .map(|view| {
                (
                    view.view.clone(),
                    view_scalar_schema_of(manifest, &view.view),
                )
            })
            .collect();
        let Some(partition_data) = generation.bundle.partitions.get(&plan.partition) else {
            return;
        };
        // The columns declared at a running service and not yet folded, taken at the plan so a
        // declaration made while the fold runs is not among them.
        let (runtime_attributes, runtime_scoped_attributes) = {
            let (entity, scoped) = self.live.attributes_for_publication();
            (
                entity.into_iter().map(|d| d.name).collect::<Vec<_>>(),
                scoped.into_iter().map(|f| f.name).collect::<Vec<_>>(),
            )
        };
        // Per view, the columns an input segment may lawfully lack: the runtime columns above and
        // the view's group-scoped lanes. Read from `runtime_attributes`, not a second snapshot of
        // the live list, so the two cannot disagree about a declaration landing between them.
        let absent_ok: std::collections::BTreeMap<String, Vec<String>> = {
            let entity_scoped = scalar_schema_of(manifest).len();
            scalar_schema
                .iter()
                .map(|(view, schema)| {
                    (
                        view.clone(),
                        lawful_absences(schema, entity_scoped, &runtime_attributes),
                    )
                })
                .collect()
        };
        let to_prefix = match crate::compact::next_prefix_name(&self.deps.bundle_root) {
            Ok(prefix) => prefix,
            Err(e) => {
                self.health.fold_requested.store(false, Ordering::SeqCst);
                self.health.fold_failures.fetch_add(1, Ordering::Relaxed);
                tracing::error!(error = %e, "could not name a new prefix for the fold");
                return;
            }
        };

        let attempt = self.fold.next_attempt();
        let ctx = crate::compact::FoldContext {
            from_prefix_dir: self.prefix_dir(generation),
            to_prefix_dir: self.deps.bundle_root.join(&to_prefix),
            to_prefix: to_prefix.clone(),
            identity_key: self.deps.identity_key,
            shard_id: manifest.identity.shard_id,
            scalar_schema,
            band_schema: band_schema_of(manifest),
            absent_ok,
            runtime_attributes,
            runtime_scoped_attributes,
            // A `seg_id` must never be reused across the bundle's life, or a rebase's ABA check
            // would be meaningless.
            seg_id: format!("fold-{}-{attempt}", partition_data.segments_n),
            base_postings: Arc::clone(&generation.postings),
            tiers: generation.delta_postings.clone(),
            declared_scalars: manifest.declared_scalars.clone(),
            scoped_scalars: manifest.scoped_scalars(),
            view_incarnations: manifest
                .views
                .iter()
                .map(|v| (v.id.clone(), v.incarnation))
                .collect(),
            vocabularies: manifest.vocabularies.clone(),
        };

        if let Some(trigger) = scheduled {
            // Every gauge, not only the one that fired: a fold is minutes to hours, and an
            // operator reading why one started needs to see the state that produced it, not just
            // the single number that crossed first. The dead-bytes pair especially, since it is
            // the one figure `/control/status` does not carry.
            let dead = dead_bytes_of(&self.prefix_dir(generation), generation);
            tracing::info!(
                trigger = ?trigger,
                live_segments = live_segments_of(generation),
                retirable_deletions = generation.overlay.deleted_len(),
                on_disc_bytes = dead.map(|d| d.on_disc),
                named_bytes = dead.map(|d| d.named),
                "dispatching a scheduled compaction fold"
            );
        }
        self.health.fold_requested.store(false, Ordering::SeqCst);
        let unit = self.fold.start();
        let health = Arc::clone(&self.health);
        let switches = Arc::clone(&self.deps.switches);
        let spawned = std::thread::Builder::new()
            .name("mosaica-fold".to_string())
            .spawn(move || {
                match crate::compact::execute(plan, ctx) {
                    Ok(mut completed) => {
                        // Test hook; always false otherwise. See `Engine::set_fold_paused_for_test`.
                        health.fold_holding.store(true, Ordering::SeqCst);
                        while switches.fold_paused.load(Ordering::SeqCst) {
                            std::thread::sleep(std::time::Duration::from_millis(5));
                        }
                        health.fold_holding.store(false, Ordering::SeqCst);
                        completed.finished = std::time::Instant::now();
                        // Before the in-flight flag clears, so a tick that observes the fold
                        // finished also observes when. See `fold_floor_from`.
                        health
                            .fold_ended_unix
                            .store(unix_now().unwrap_or(0), Ordering::SeqCst);
                        unit.complete(completed);
                    }
                    Err(e) => {
                        health
                            .fold_ended_unix
                            .store(unix_now().unwrap_or(0), Ordering::SeqCst);
                        // Counted after the flag clears, so a request made on seeing the count
                        // is not refused as arriving while this fold runs.
                        drop(unit);
                        health.fold_failures.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!(
                            error = %e,
                            "a compaction fold failed; its files are orphans under a prefix \
                             CURRENT does not name, nothing retired, and the next trigger \
                             re-plans from scratch"
                        );
                    }
                }
            });
        if spawned.is_ok() {
            self.last_fold_start_unix = unix_now();
        }
        if let Err(e) = spawned {
            // The closure was dropped without running, which cleared the in-flight flag.
            self.health.fold_failures.fetch_add(1, Ordering::Relaxed);
            tracing::error!(error = %e, "the OS refused a thread for the compaction fold");
        }
    }

    /// Apply every completed fold waiting from its thread, and report whether any did.
    pub(super) fn publish_completed_folds(&mut self) -> bool {
        // Test hook; always false in a shipped build. See `MaintenanceDeps::switches`.
        if self.deps.switches.fold_publication_paused.load(Ordering::SeqCst) {
            return false;
        }
        let mut any = false;
        while let Some(completed) = self.fold.next_completed() {
            self.publish_fold(completed);
            any = true;
        }
        if any {
            self.fold.drained();
        }
        any
    }

    /// Writes every derived artifact file into the prefix the fold is publishing. All of them are
    /// derived, so a file that will not compose is left out and built on first use.
    fn write_fold_derived(
        &mut self,
        to_prefix_dir: &std::path::Path,
        completed: &crate::compact::CompletedFold,
        manifest_n: u64,
        pending: &PendingRetirement,
    ) -> Vec<mosaica_store::manifest::DerivedExtent> {
        let plan = &completed.plan;
        let views: Vec<(String, u32)> = completed
            .segments
            .iter()
            .map(|segment| (segment.view.clone(), segment.row_count))
            .collect();
        let mut pass = DerivedPass {
            prefix_dir: to_prefix_dir,
            partition: &plan.partition,
            n: manifest_n,
            incarnations: plan
                .views
                .iter()
                .map(|view| (view.view.clone(), view.incarnation))
                .collect(),
            spaces: self.fold_row_spaces(to_prefix_dir, &plan.partition, &views),
            segments: fold_segments(to_prefix_dir, &plan.partition, &completed.segments),
            pending,
            index: Default::default(),
        };
        let mut derived = self.write_containment_partitions(&mut pass);
        // Layouts must be chosen before the files and the registry snapshot, or the fold would
        // publish a level in its old layout under a record claiming the new one.
        let (layouts, overlapping) = self.choose_layouts(&pass.spaces, pending, &pass.segments);
        for (layer, level, chosen) in &layouts {
            if self.live.record_layout(layer, *level, *chosen) {
                self.deps.artifact_projections.forget_level(layer, *level);
            }
        }
        self.pending_forms.clear();
        derived.extend(self.write_tile_indexes(&mut pass, &layouts));
        derived.extend(self.write_row_columns(&mut pass, &layouts, &overlapping));
        let shape_rows = self.write_shape_rows(&mut pass, &derived);
        derived.extend(shape_rows);
        derived.extend(self.write_shape_held(&mut pass));
        derived
    }

    /// Whether a completed fold still applies to the live bundle, asked before anything is
    /// written: what it carries forward from the live manifest and the deletions it retires, or
    /// why it is discarded.
    fn fold_still_applies<'a>(
        &self,
        completed: &crate::compact::CompletedFold,
        live: &'a Generation,
    ) -> Result<FoldApplies<'a>, String> {
        let plan = &completed.plan;
        // Re-asked here, not just at the plan: a fold's flight is long enough for the WAL to
        // poison after `plan_fold` checked it, and this manifest would then publish deny state no
        // durable record backs. The divergence half is already asked by `may_publish` above.
        if self.log.wal.is_poisoned() {
            return Err("the WAL poisoned during its flight, so its manifest would publish deny state \
                     no durable record backs"
                .to_string());
        }

        if live.prefix != plan.prefix {
            return Err("it was planned against a superseded prefix".to_string());
        }
        let Some(partition_data) = live.bundle.partitions.get(&plan.partition) else {
            return Err("the live bundle no longer carries the partition it folded".to_string());
        };
        let live_manifest = &partition_data.manifest;

        // ---- rebase or discard ------------------------------------------------------------------
        //
        // ABA-safe: ids are never reused, so an artefact still listed is the artefact the fold
        // consumed.
        let consumed_segments: FxHashSet<(&str, &str)> = plan
            .views
            .iter()
            .flat_map(|view| {
                view.segments
                    .iter()
                    .map(move |segment| (view.view.as_str(), segment.seg_id.as_str()))
            })
            .collect();
        if !fold_rebases(plan, live_manifest, &consumed_segments) {
            return Err("an artefact it consumed is no longer listed in the live manifest".to_string());
        }

        // ---- the carry-forward set ----------------------------------------------------------------
        //
        // Which incarnations are live is decided from the `MANIFEST.json` roster, not the
        // partition's view map (a cache of open row spaces), so it cannot disagree with what the
        // new manifest declares.
        let live_incarnations: FxHashMap<&str, mosaica_types::view::ViewIncarnation> = live
            .bundle
            .manifest
            .views
            .iter()
            .map(|v| (v.id.as_str(), v.incarnation))
            .collect();
        // A view dropped during the flight, or dropped and created again, would have its dead
        // incarnation's base published as the live view's rows. The next fold plans the live one.
        if plan
            .views
            .iter()
            .any(|view| live_incarnations.get(view.view.as_str()) != Some(&view.incarnation))
        {
            return Err("a view it folded was dropped during its flight; the next fold plans over \
                     the views as they now stand"
                .to_string());
        }
        let forward = carried_forward(plan, live_manifest, &live_incarnations, &consumed_segments);

        // Every carried-forward segment must begin at or above its view's base, or the prefix will
        // not open. Checked before anything is written, since that would surface only after
        // `CURRENT` flips.
        for descriptor in &forward.segments {
            let Some(view) = plan.views.iter().find(|s| s.view == descriptor.view) else {
                // A view created and flushed since the plan was taken has no base here; that is
                // transient, and the next fold plans over a bundle that holds it.
                return Err("a carried-forward segment names a view created since the plan was taken, so \
                     the fold has no base for it; the next fold plans over a bundle that has it"
                    .to_string());
            };
            if descriptor.entity_lo < view.permutation_bound {
                return Err("a carried-forward segment begins below the fold's own base permutation".to_string());
            }
        }
        // ---- retirement, evaluated here and nowhere earlier ---------------------------------------
        let mut carried = crate::compact::CarriedForward::new();
        for descriptor in &forward.segments {
            let Some(extent) = partition_data
                .views
                .get(&descriptor.view)
                .and_then(|view| {
                    view.row_space
                        .extents()
                        .iter()
                        .find(|extent| extent.seg_id == descriptor.seg_id)
                })
            else {
                return Err(
                    "a carried-forward segment has no extent in the live row space".to_string()
                );
            };
            carried.add_segment(extent);
        }
        let executed = crate::compact::executed(&plan.tombstones, &carried);
        Ok(FoldApplies {
            carried_entities: carried.len(),
            forward,
            executed,
        })
    }

    /// Publish a fold: rebase or discard, assemble `SEGMENTS-<n>` from the live partition
    /// manifest, hard-link the carry-forwards, write `MANIFEST.json`, write `SEGMENTS-<n>.json`, flip `CURRENT`, open the
    /// new prefix in-process, swap onto it, rotate the WAL, and reclaim the old prefix.
    ///
    /// What the fold carries forward is read from live state here, hours after the plan, not from
    /// the plan itself. `tombstones` must be a set difference against live state: a delete
    /// accepted during the flight names an entity the fold did not drop, so publishing the plan's
    /// executed set would retire that deletion while its row survives in the rebuilt base.
    ///
    /// `CURRENT` is the commit point. A failure before the flip discards the fold: its files are
    /// orphans, every consumed artefact still stands, and the next trigger re-plans. A failure
    /// after the flip is alarmed instead: the bundle on disc is the new one, but this process goes
    /// on serving the old geometry it still holds mapped, until restarted.
    pub(super) fn publish_fold(&mut self, completed: crate::compact::CompletedFold) {
        let started = std::time::Instant::now();
        let mut stairs =
            crate::compact::Staircase::resume(completed.cost.clone(), completed.finished);
        let live = self.generation.load_full();
        let plan = &completed.plan;
        if !self.may_publish() {
            return;
        }
        // One closure for every discard below, since each is the same posture: nothing happened,
        // the files are orphans, the next trigger re-plans.
        let discard = {
            let health = Arc::clone(&self.health);
            let prefix = completed.prefix.clone();
            move |reason: &str| {
                health.fold_failures.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    prefix = %prefix,
                    "discarding a completed fold: {reason}. Its files are orphans under a prefix \
                     CURRENT does not name, every consumed artefact still stands, and nothing \
                     retired"
                );
            }
        };

        let FoldApplies {
            forward,
            executed,
            carried_entities,
        } = match self.fold_still_applies(&completed, &live) {
            Ok(applies) => applies,
            Err(reason) => {
                discard(&reason);
                return;
            }
        };
        let partition_data = &live.bundle.partitions[&plan.partition];
        let live_manifest = &partition_data.manifest;
        let retired_count = executed.cardinality();
        let manifest_n = match self
            .side_manifests
            .allocate_manifest_n(&self.deps.bundle_root, &self.health)
        {
            Ok(n) => n,
            Err(e) => {
                discard(&format!(
                    "its side-manifest number could not be allocated ({e})"
                ));
                return;
            }
        };

        // ---- the artifact pass ---------------------------------------------------------------------
        //
        // Membership extent paths are prefix-relative, so they are written again into the new
        // prefix rather than carried forward or dropped. `repack_all` drops exactly the executed
        // deletions: a suppressed member keeps its bit (Rule S).
        let to_prefix_dir = self.deps.bundle_root.join(&completed.prefix);
        stairs.record("6 hand-off");
        let repacked = match self.rewrite_membership_extents(
            &to_prefix_dir,
            &plan.partition,
            manifest_n,
            &executed,
        ) {
            Ok(repacked) => repacked,
            Err(e) => {
                discard(&format!(
                    "its artifact memberships would not be rewritten ({e})"
                ));
                return;
            }
        };

        stairs.record("7 memberships");

        // The levels the retirement below is about to move, and has not yet
        // ([`PendingRetirement`]): the store still holds pre-retirement records, so a level in the
        // retirement's set gets no partition, row column or tile index here, and recomposes on
        // first use at the version the retirement will leave it at.
        let pending = PendingRetirement {
            levels: self
                .live
                .with_artifacts(|store| store.levels_moved_by(&executed)),
            retired: executed.clone(),
        };
        let derived = self.write_fold_derived(
            &to_prefix_dir,
            &completed,
            manifest_n,
            &pending,
        );
        stairs.record("8 derived");

        // ---- the report, before anything retires ---------------------------------------------------
        //
        // A deletion is not retired before the caller has been told what it degraded: computed
        // here, while the store still holds what this fold is about to retire. A report that
        // cannot be written discards the fold; nothing is lost, since the next fold reports it.
        let degraded = self
            .live
            .with_artifacts(|store| store.degradations(&executed));
        stairs.record("9 report");

        // ---- assemble `SEGMENTS-<n>` from the live partition manifest ------------------------------
        //
        // Each view's fold base first, its carried extents after: the reader takes the first
        // segment listed for a view as the one `permutation.bin` addresses.
        let mut segments = Vec::with_capacity(completed.segments.len() + forward.segments.len());
        for base in &completed.segments {
            segments.push(base.clone());
            segments.extend(
                forward
                    .segments
                    .iter()
                    .filter(|d| d.view == base.view)
                    .map(|d| (*d).clone()),
            );
        }
        // Read before the swap, while the edited-items map still holds the retired entities'
        // pairs.
        let freed = match freed_by(&live, &executed) {
            Ok(freed) => freed,
            Err(e) => {
                discard(&format!(
                    "the entities it retires could not be told apart from items' numbers ({e})"
                ));
                return;
            }
        };
        // The suppressions leaving with their entities are logged before the manifest omits
        // them, so a replay of the suppressions the log still holds ends without them. Each entity
        // is deleted until this fold's swap, so the record hides nothing less even if the fold is
        // then discarded.
        let unsuppressed = live.overlay.suppressions_retired_by(&executed);
        if !unsuppressed.is_empty() {
            let record = WalRecord::ChangeBatch {
                changes: unsuppressed
                    .iter()
                    .map(|entity| (EntityId::new(u64::from(entity)), ChangeOp::Unsuppress))
                    .collect(),
            };
            if let Err(failure) = self.append_and_sync(&[&record], None) {
                discard(&format!(
                    "the suppressions leaving with its entities could not be logged ({})",
                    failure.into_error()
                ));
                return;
            }
        }
        // `tombstones` must leave the overlay in the same commit as the manifest, or a manifest
        // carrying what the swap is about to retire would re-seed the overlay at the next restart.
        let mut published_overlay = (*live.overlay).clone();
        published_overlay.retire(&executed);
        // At the log position no later record can name the freed ids at.
        let freed = mosaica_store::manifest::HeldEntities {
            position: self.log.wal.position(),
            entities: mosaica_store::manifest::EntitySet::of(&freed),
        };

        let (runtime_attributes, runtime_scoped_attributes) =
            self.live.attributes_for_publication();
        let folded_vocabularies = self
            .live
            .with_vocabularies(|vocabularies| vocabularies.names());
        let (folded_groups, folded_plain_views) = self
            .live
            .with_view_declarations(|declarations| declarations.names());

        let mut segments_manifest = SegmentsManifest {
            text_extents: forward.texts.clone(),
            // Live and untouched: an entity at or above the watermark is buffered rather than
            // rowed, so deriving this from the fold's inputs would move it backwards past every
            // post-snapshot entity and make the gap invisible.
            watermark: live_manifest.watermark,
            entity_id_high_water: live_manifest.entity_id_high_water,
            // Live: the fold does not renumber the row-less region, so deriving this from the
            // fold's inputs would hand the next registration ids a live layer already holds. The
            // commit lowers it to the allocator's mark and restates the registry and the roster.
            entity_id_low_water: live_manifest.entity_id_low_water,
            // Only the declarations made since the fold planned: the fold's own `MANIFEST.json`
            // already states the rest, with a base written for each.
            attributes: runtime_attributes
                .iter()
                .filter(|d| !completed.runtime_attributes.contains(&d.name))
                .cloned()
                .collect(),
            scoped_attributes: runtime_scoped_attributes
                .iter()
                .filter(|f| !completed.runtime_scoped_attributes.contains(&f.name))
                .cloned()
                .collect(),
            // The artifact pass's own output: paths are prefix-relative, so this is the only list
            // naming files the new prefix contains.
            membership_extents: repacked.clone(),
            term_image_extents: completed
                .term_images
                .iter()
                .map(|images| images.extent.clone())
                .collect(),
            artifact_record_extents: self.side_manifests.artifact_record_extents.clone(),
            segments,
            deltas: forward.tiers.clone(),
            // The live list, not the plan's: a flush that promoted during the flight appended an
            // extent whose ordinals the live dictionary already holds.
            dict_extents: live_manifest.dict_extents.clone(),
            attr_extents: forward.attrs.clone(),
            record_extents: forward.records.clone(),
            entity_terms_extents: forward.entity_terms.clone(),
            unique_indexes: forward
                .unique
                .iter()
                .map(|carried| {
                    let mut index = carried.clone();
                    if let Some((_, base)) = completed
                        .unique
                        .iter()
                        .find(|(attribute, _)| *attribute == carried.attribute)
                    {
                        index.base = base.clone();
                    }
                    index
                })
                .collect(),
            edited_items: mosaica_store::manifest::EditedItemsRuns {
                by_number: mosaica_store::manifest::KeyRuns {
                    base: completed.edited.by_number.base.clone(),
                    live: forward.edited.by_number.live.clone(),
                },
                by_entity: mosaica_store::manifest::KeyRuns {
                    base: completed.edited.by_entity.base.clone(),
                    live: forward.edited.by_entity.live.clone(),
                },
            },
            ..SegmentsManifest::empty()
        };
        write_deny_state(&mut segments_manifest, &published_overlay);

        // ---- the new `MANIFEST.json` ----------------------------------------------------------
        let mut bundle_manifest = self.fold_bundle_manifest(&live, &completed, plan);

        let carried_rels = carried_files(
            &plan.partition,
            live_manifest,
            &live.bundle.manifest.files,
            &forward,
        );
        for rel in &carried_rels {
            let Some(digest) = live_manifest
                .files
                .get(rel)
                .or_else(|| live.bundle.manifest.files.get(rel))
            else {
                discard("a carried-forward file has no digest in either live manifest");
                return;
            };
            bundle_manifest.files.insert(rel.clone(), digest.clone());
        }

        // ---- link, write, write, flip ---------------------------------------------------------
        let from_prefix_dir = self.prefix_dir(&live);
        let mut carried_rels: Vec<String> = carried_rels.into_iter().collect();
        // Content extents carry no entry in either manifest's `files`, so they are linked here
        // rather than through the digest loop above, from the held list rather than the manifest
        // (which can be behind the live generation).
        for extent in &self.side_manifests.artifact_record_extents {
            carried_rels.extend(extent.files().map(String::from));
        }
        if let Err(e) =
            mosaica_store::hard_link_forward(&from_prefix_dir, &to_prefix_dir, &carried_rels)
        {
            discard(&format!("its carry-forwards would not link ({e})"));
            return;
        }
        // A hard link copies no bytes, and no producer fsyncs a data file on write, so the
        // carry-forwards are synced explicitly here: this fold is about to reclaim the old
        // prefix and rotate the WAL, removing both routes a later flush could recover through.
        let carried_paths: Vec<PathBuf> = carried_rels
            .iter()
            .map(|rel| to_prefix_dir.join(rel))
            .collect();
        if let Err(e) = mosaica_store::fsync_written(&carried_paths) {
            discard(&format!("its carry-forwards would not sync ({e})"));
            return;
        }
        let manifest_digest =
            match mosaica_store::write_manifest_json(&to_prefix_dir, &bundle_manifest) {
                Ok(digest) => digest,
                Err(e) => {
                    discard(&format!("its MANIFEST.json would not commit ({e})"));
                    return;
                }
            };
        if let Err(e) = self.commit_side_manifest(
            partition_data,
            &to_prefix_dir,
            &plan.partition,
            manifest_n,
            &mut segments_manifest,
            Some(FoldDerived {
                written: &derived,
                pending_retirement: &pending,
                freed: &freed,
            }),
        ) {
            discard(&format!(
                "its SEGMENTS-{manifest_n}.json would not commit ({e})"
            ));
            return;
        }
        // The commit point. Everything above is reversible; nothing below is. A kill parked before
        // this line leaves a complete, synced `v#####` tree `CURRENT` never named, and the startup
        // sweep reclaims it whole.
        //
        // The report must be written before this line: it says a deletion's obligation was
        // discharged, so writing it after would let a fold abandoned at the flip leave a false one.
        if let Err(e) = self.write_fold_report(&completed.prefix, &degraded) {
            discard(&format!(
                "its degradation report would not be written ({e}), and a deletion may not retire \
                 before the caller has been told what it degraded"
            ));
            return;
        }
        stairs.record("10 manifest");

        self.pause_point(PauseSiteArg::BeforeCurrentFlip);
        if let Err(e) =
            mosaica_store::write_current(&self.deps.bundle_root, &completed.prefix, &manifest_digest)
        {
            discard(&format!("CURRENT would not flip ({e})"));
            return;
        }
        stairs.record("11 flip");

        // Retires the resident store here, before the new generation is installed: the published
        // overlay already carries the executed deletions retired, so retiring after would leave a
        // window where a deleted artifact's entity reads not denied while its record still serves.
        let mut moved = self.live.retire_artifacts(&pending.retired);
        self.live.mark_memberships_published();
        self.live.mark_growth_packed();
        let (rehoused, kept) = self.live.rehouse_memberships(&to_prefix_dir, &repacked);
        if kept > 0 {
            tracing::error!(
                rehoused,
                kept,
                "ALARM: artifact memberships this fold wrote back do not match the records they \
                 were written from; the level the prefix carries and the level being served \
                 disagree for those ordinals"
            );
        }
        self.side_manifests.membership_extents = repacked;
        // Checked, not assumed: a pending level's structures were stamped with the version the
        // level would have after this retirement ([`PendingRetirement`]). If `retire` ever moves a
        // different set, a stamped structure would be carried at a version it does not describe.
        moved.sort();
        let mut expected = pending.levels.clone();
        expected.sort();
        if moved != expected {
            tracing::error!(
                stamped_for = ?expected,
                moved = ?moved,
                "ALARM: the fold's retirement moved levels other than the ones its derived \
                 structures were stamped for; every structure whose stamp the store does not \
                 carry is dropped and its level recomposes on first use"
            );
        }
        self.side_manifests.derived_extents = self.live.with_artifacts(|store| {
            held_at_current_version(store, &segments_manifest.derived_extents)
        });
        *lock_recover(&self.health.last_fold_report) = degraded;
        stairs.record("12 retire");

        let segments_version = live.segments_version + 1;
        if let Err(reason) = self.swap_onto_folded_prefix(
            &completed.prefix,
            &live,
            &live_manifest.deltas,
            &segments_manifest.deltas,
            executed,
        ) {
            self.diverge_from_current(&completed.prefix, &reason);
            return;
        }
        // Taken here, while every generation in it is over the prefix this fold supersedes: one
        // published after the swap is over the new prefix, and the next fold waits for it.
        let mut generations = std::mem::take(
            &mut *self.superseded.lock().unwrap_or_else(|e| e.into_inner()),
        );
        generations.push(Arc::downgrade(&live));
        stairs.record("13 open");

        self.live.with_artifacts(|store| {
            self.deps.artifact_projections.adopt_derived(
                &to_prefix_dir,
                &completed.prefix,
                &self.side_manifests.derived_extents,
                store,
            )
        });
        stairs.record("14 adopt");

        // Must run after both the swap and the retire above: built before the swap, a projection
        // would be keyed to a generation no reader can ask for; built before the retire, it would
        // be keyed to a store version the retire is about to bump, discarding the warm.
        self.warm_artifact_caches();
        self.deps.shapes.clear_staged();
        stairs.record("15 warm");

        // ---- rotate the WAL ---------------------------------------------------------------------
        //
        // Immediately: the manifest seed no longer carries the executed entries, but the WAL still
        // holds the original delete records until a rotation reclaims them. The freed ids are
        // issued once it has.
        if let Some(ids) = freed.entities.entities() {
            let ids = ids.clone();
            self.live
                .with_allocator(|alloc| alloc.release_after(freed.position, ids));
        }
        self.rotate_wal();
        stairs.record("16 wal");
        self.live.with_attributes(|attributes| {
            attributes.retire_folded(
                &completed.runtime_attributes,
                &completed.runtime_scoped_attributes,
            )
        });
        self.live
            .with_vocabularies(|vocabularies| vocabularies.retire_folded(&folded_vocabularies));
        self.live.with_view_declarations(|declarations| {
            declarations.retire_folded(&folded_groups, &folded_plain_views)
        });

        // ---- reclaim the superseded prefix -----------------------------------------------------
        let CarriedExtents {
            omitted_views,
            omitted_segments,
            ..
        } = forward;
        drop(live);
        self.pending_reclaim.push(PendingReclaim {
            generations,
            prefix_dir: from_prefix_dir,
        });
        self.reclaim_superseded_prefixes();
        stairs.record("17 reclaim");
        let cost = stairs.into_cost();

        let (passes, fold_secs, staircase_rss) = self.health.record_fold_cost(
            cost,
            completed.attr_bytes_read,
            completed.attr_bytes_written,
        );
        // After the cost, so a reader that sees the count move reads this fold's passes.
        self.health.folds.fetch_add(1, Ordering::Relaxed);
        // One line per view: a group's keys are separate views over one dictionary, each paying
        // its own table and payload.
        for images in &completed.term_images {
            let summary = &images.summary;
            tracing::info!(
                prefix = %completed.prefix,
                view = %images.extent.view,
                kept = summary.kept,
                terms = summary.terms,
                payload_bytes = summary.payload_bytes,
                table_bytes = summary.table_bytes,
                wall_s = summary.wall.as_secs_f64(),
                "a fold wrote a view's term images"
            );
        }
        tracing::info!(
            prefix = %completed.prefix,
            segments_version,
            retired = retired_count,
            carried_entities,
            elapsed_ms = started.elapsed().as_millis() as u64,
            passes = %passes,
            fold_secs,
            staircase_rss,
            attr_bytes_read = completed.attr_bytes_read,
            attr_bytes_written = completed.attr_bytes_written,
            dropped_views = %if omitted_views.is_empty() {
                "none".to_string()
            } else {
                omitted_views.join(",")
            },
            dropped_view_segments = omitted_segments,
            "a compaction fold published: the bundle is one base segment per partition-view, one \
             base postings tier, plus whatever landed during its flight"
        );
    }

    /// The `MANIFEST.json` a fold's new prefix carries: the live one, with the schema wound back to
    /// what it was at the plan, every live vocabulary binding folded in, and the fold's own files.
    ///
    /// `entity_id_high_water` here is the snapshot's: `Engine::open` seeds the allocator's floor
    /// from the max of this and the side-manifest's live value, so the lower value is safe for the
    /// allocator.
    ///
    /// The vocabulary bindings fold in verbatim, never re-derived, re-sorted or re-numbered: keys
    /// and codes are byte-identical everywhere, and `columns.arrow` stores the code alone, so
    /// renumbering here would recolour the corpus with no error and no digest mismatch.
    pub(super) fn fold_bundle_manifest(
        &self,
        live: &Generation,
        completed: &crate::compact::CompletedFold,
        plan: &crate::compact::FoldPlan,
    ) -> mosaica_store::manifest::Manifest {
        let mut bundle_manifest = live.bundle.manifest.clone();
        let (runtime_attributes, runtime_scoped_attributes) =
            self.live.attributes_for_publication();
        let since_plan: Vec<&str> = runtime_attributes
            .iter()
            .map(|d| d.name.as_str())
            .filter(|name| !completed.runtime_attributes.iter().any(|n| n == name))
            .collect();
        bundle_manifest
            .declared_scalars
            .retain(|d| !since_plan.contains(&d.name.as_str()));
        let scoped_since_plan: Vec<&str> = runtime_scoped_attributes
            .iter()
            .map(|f| f.name.as_str())
            .filter(|name| {
                !completed
                    .runtime_scoped_attributes
                    .iter()
                    .any(|n| n == name)
            })
            .collect();
        for group in &mut bundle_manifest.groups {
            group
                .scoped_scalars
                .retain(|f| !scoped_since_plan.contains(&f.name.as_str()));
        }
        crate::vocabularies::merge_live_values(&mut bundle_manifest, &live.vocabularies);
        bundle_manifest.entity_id_high_water = plan.high_water;
        bundle_manifest.files = completed.files.clone();
        let carried_bindings: Vec<_> = live
            .bundle
            .partitions
            .values()
            .flat_map(|partition| partition.manifest.vocabulary_extensions.iter().cloned())
            .collect();
        mosaica_store::vocabulary::fold_extensions_into(
            &mut bundle_manifest.vocabularies,
            &carried_bindings,
        );
        bundle_manifest
    }

    /// Latch [`ExecutorHealth::prefix_diverged`]: `CURRENT` names a prefix this process could not
    /// swap onto, so it must stop writing durable state.
    ///
    /// A publication after this point would write under the old prefix (which the live generation
    /// still names) and then rotate the WAL, acking ingest that a restart loses. The superseded
    /// prefix is not reclaimed here: this process still serves from it.
    pub(super) fn diverge_from_current(&self, committed: &str, reason: &str) {
        self.health.fold_failures.fetch_add(1, Ordering::Relaxed);
        self.health.prefix_diverged.store(true, Ordering::SeqCst);
        tracing::error!(
            committed_prefix = %committed,
            "ALARM: CURRENT names the folded prefix and this process could not swap onto it: \
             {reason}. It keeps serving the superseded prefix and publishes nothing (no geometry, \
             no deny state, no WAL rotation) until it is restarted, when it opens the committed \
             prefix. Restart this node"
        );
    }

    /// Opens the prefix `CURRENT` now names and swaps onto it.
    fn swap_onto_folded_prefix(
        &mut self,
        prefix: &str,
        live: &Arc<Generation>,
        live_tiers: &[String],
        folded_tiers: &[String],
        retired: croaring::Bitmap,
    ) -> Result<(), String> {
        let (bundle, rotation) =
            crate::engine::open_rotation(&self.deps.bundle_root, prefix, &live.fragments, retired)
                .map_err(|e| format!("the folded prefix would not open ({e})"))?;
        let delta_postings = folded_tiers
            .iter()
            .map(|rel| {
                held_tier(&live.delta_postings, live_tiers, rel).ok_or_else(|| {
                    format!("the folded manifest names delta tier {rel}, which is not held open")
                })
            })
            .collect::<Result<Vec<Arc<DeltaTier>>, String>>()?;
        self.publish_geometry(
            GeometryPublication::within_prefix(
                prefix.to_string(),
                live.segments_version + 1,
                live.watermark,
                bundle,
                Arc::clone(&live.dict),
                delta_postings,
            )
            .rotating(rotation),
        )
        .map_err(|e| format!("the swap was refused ({e})"))
    }

    /// Delete every superseded prefix nothing is reading any more.
    ///
    /// Waits for every generation published over the prefix to be released before unlinking: the
    /// generation pointer has already moved, so no new holder can appear.
    ///
    /// A failure alarms once and drops the entry rather than retrying every tick; the tree stands
    /// as an orphan.
    pub(super) fn reclaim_superseded_prefixes(&mut self) {
        if self.pending_reclaim.is_empty() {
            return;
        }
        let mut still_read = Vec::new();
        for pending in std::mem::take(&mut self.pending_reclaim) {
            if pending.generations.iter().any(|held| held.strong_count() > 0) {
                still_read.push(pending);
                continue;
            }
            let prefix_dir = pending.prefix_dir;
            match mosaica_store::reclaim_prefix(&prefix_dir) {
                Ok(()) => tracing::info!(
                    prefix = %prefix_dir.display(),
                    "the superseded prefix is reclaimed: the build's base, every merged-away \
                     segment, every consumed tier and every superseded side-manifest"
                ),
                Err(e) => tracing::error!(
                    error = %e,
                    prefix = %prefix_dir.display(),
                    "ALARM: the superseded prefix could not be reclaimed and stands as an orphan. \
                     Nothing live was unlinked; what is lost is the disc the fold exists to free"
                ),
            }
        }
        self.pending_reclaim = still_read;
    }

    /// Write the fold's degradation report, and keep the last one for the operator route.
    ///
    /// Written to `reports/` in the bundle root, not into the prefix, which a later fold reclaims.
    /// A report with nothing in it is still written, so an operator can tell "degraded nothing"
    /// from "never reported".
    pub(super) fn write_fold_report(
        &self,
        prefix: &str,
        degraded: &[mosaica_lifecycle::membership::Degradation],
    ) -> std::io::Result<()> {
        let dir = self.deps.bundle_root.join("reports");
        std::fs::create_dir_all(&dir)?;
        let rows: Vec<serde_json::Value> = degraded
            .iter()
            .map(|d| {
                serde_json::json!({
                    "layer": d.layer,
                    "level": d.level,
                    "ordinal": d.ordinal,
                    "key": d.key,
                    "members_lost": d.members_lost,
                    "declared_members": d.declared_members,
                    "contents_lost": d
                        .contents_lost
                        .iter()
                        .map(|(index, lost)| serde_json::json!({"rank": index, "lost": lost}))
                        .collect::<Vec<_>>(),
                })
            })
            .collect();
        let body = serde_json::json!({
            "prefix": prefix,
            "degraded": rows,
        });
        let bytes = serde_json::to_vec_pretty(&body)?;
        let path = dir.join(format!("fold-{prefix}.json"));
        mosaica_store::write_and_fsync(&path, &bytes)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        mosaica_store::fsync_dir(&dir).map_err(|e| std::io::Error::other(e.to_string()))?;
        // The in-memory copy is set by the caller, after the flip, not here: the fold can still
        // be discarded after this file is written.
        Ok(())
    }

    /// The fold's artifact pass: write every level whole into the prefix being published, with the
    /// fold's executed deletions dropped from each membership.
    ///
    /// The returned list replaces the manifest's rather than extending it: one extent per level,
    /// covering `[0, len)`. Holes are written, not packed around: closing one would hand every
    /// later artifact in the level the identity of its neighbour, since an ordinal is the identity
    /// a caller's `mosaica_id` resolves to.
    pub(super) fn rewrite_membership_extents(
        &self,
        prefix_dir: &std::path::Path,
        partition: &str,
        n: u64,
        retired: &croaring::Bitmap,
    ) -> mosaica_store::Result<Vec<mosaica_store::manifest::MembershipExtent>> {
        let ready = self.live.with_artifacts(|store| store.repack_all(retired));
        pack_membership_extents(prefix_dir, partition, n, ready)
    }

    /// Compose and write this prefix's containment partitions, one file per `(layer, level)`.
    ///
    /// Composed against the prefix being published, not the one being left, over the postings that
    /// prefix's own artifact pass just wrote. Every failure is an empty list, not a discarded
    /// fold: a partition is derived, so the level recomposes it on first use.
    fn write_containment_partitions(
        &self,
        pass: &mut DerivedPass<'_>,
    ) -> Vec<mosaica_store::manifest::DerivedExtent> {
        let prefix_dir = pass.prefix_dir;
        let partition = pass.partition;
        let n = pass.n;
        let pending = pass.pending;
        let postings_path = prefix_dir
            .join("partitions")
            .join(partition)
            .join("terms")
            .join("postings.arrow");
        let postings = match mosaica_authz::PostingsReader::open(&postings_path, true) {
            Ok(postings) => postings,
            Err(error) => {
                tracing::warn!(
                    path = %postings_path.display(),
                    %error,
                    "the fold could not read the prefix it just wrote to compose containment \
                     partitions; every level recomposes on first use"
                );
                return Vec::new();
            }
        };

        let composed: Vec<(String, u32, u64, Vec<u8>)> = self.live.with_artifacts(|store| {
            let levels: Vec<(String, u32)> = levels_of(store).collect();
            levels
                .into_iter()
                .filter(|(layer, level)| !pending.is_pending(layer, *level))
                .filter_map(|(layer, level)| {
                    let version = store.level_version(&layer, level);
                    match crate::containment::ContainmentPartition::compose(
                        store, &layer, level, &postings,
                    ) {
                        Ok(partition) => {
                            Some((layer, level, version, partition.as_bytes().to_vec()))
                        }
                        Err(error) => {
                            tracing::warn!(
                                layer = %layer,
                                level,
                                %error,
                                "a containment partition would not compose at the fold; that \
                                 level recomposes on first use"
                            );
                            None
                        }
                    }
                })
                .collect()
        });
        if composed.is_empty() {
            return Vec::new();
        }

        mosaica_store::derived::file_derived(
            prefix_dir,
            partition,
            n,
            &mut pass.index,
            composed
                .into_iter()
                .map(
                    |(layer, level, level_version, bytes)| mosaica_store::derived::Filed {
                        view: None,
                        incarnation: None,
                        layer,
                        level,
                        level_version,
                        form: mosaica_store::manifest::DerivedForm::Containment,
                        bytes: mosaica_store::derived::FiledBytes::InHand(bytes),
                    },
                )
                .collect(),
        )
    }

    /// The base row space of every view this fold just wrote, opened once for the whole artifact
    /// pass: base only, with no extents, which is what every structure derived from one is
    /// computed over.
    ///
    /// A view whose permutation will not load is simply absent: its structures are derived on
    /// first use, the same as before the fold wrote anything.
    pub(super) fn fold_row_spaces(
        &self,
        prefix_dir: &std::path::Path,
        partition: &str,
        views: &[(String, u32)],
    ) -> Vec<(String, mosaica_store::RowSpace)> {
        let mut spaces: Vec<(String, mosaica_store::RowSpace)> = Vec::new();
        for (view, row_count) in views {
            let partition_dir = prefix_dir.join("partitions").join(partition);
            let path = mosaica_store::view_path(&partition_dir, view).join("permutation.bin");
            match mosaica_store::Permutation::load(&path) {
                Ok(permutation) => spaces.push((
                    view.clone(),
                    mosaica_store::RowSpace::new(std::sync::Arc::new(permutation), *row_count),
                )),
                Err(error) => tracing::warn!(
                    view = %view,
                    path = %path.display(),
                    %error,
                    "the fold could not read the permutation it just wrote, so this view's derived \
                     artifact structures are built on first use"
                ),
            }
        }
        spaces
    }

    /// Whether the artifact pass composes a level's row column and tile index: every level the
    /// retirement does not move, and an enumerated level it does. A spatial level the retirement
    /// moves is omitted, since its rows resolve from shapes instead.
    pub(super) fn composes_row_structures(
        &self,
        pending: &PendingRetirement,
        layer: &str,
        level: u32,
    ) -> bool {
        !pending.is_pending(layer, level)
            || self.live.registered_layer(layer).is_some_and(|registered| {
                registered.declaration.membership
                    == mosaica_types::layer::MembershipSource::Enumerated
            })
    }

    /// The fold's layout re-evaluation, run inside the artifact pass, reading the store
    /// post-retirement so a level the fold retired most of is observed as it is about to become.
    ///
    /// The record is per `(layer, level)` but the observation is per view; the shape is taken in
    /// the first view the fold wrote, deterministically, and the other views' columns are written
    /// in that layout even where it is not the one they would have chosen alone. Beside the layouts,
    /// the levels whose observed memberships overlap.
    pub(super) fn choose_layouts(
        &self,
        spaces: &[(String, mosaica_store::RowSpace)],
        pending: &PendingRetirement,
        fold_segments: &[(String, mosaica_store::read::SegmentData)],
    ) -> (
        Vec<(String, u32, mosaica_types::layer::ServingLayout)>,
        Overlapping,
    ) {
        let mut overlapping = std::collections::BTreeSet::new();
        let Some((first_view, space)) = spaces.first() else {
            return (Vec::new(), overlapping);
        };
        let levels: Vec<(String, u32)> = self.live.with_artifacts(|store| {
            levels_of(store)
                .filter(|(layer, level)| self.composes_row_structures(pending, layer, *level))
                .collect()
        });
        let mut out = Vec::with_capacity(levels.len());
        let mut per_layer: std::collections::BTreeMap<String, Vec<(u32, u64)>> =
            std::collections::BTreeMap::new();
        for (layer, level) in levels {
            let Some(registered) = self.live.registered_layer(&layer) else {
                continue;
            };
            // A spatial level is observed over its resolved rows: every segment the fold wrote is
            // resolved against the level's shapes here, and the pieces are staged under the new
            // segment ids for the derived files this pass writes and for the flip's warm.
            let spatial = spatial_membership(&registered.declaration);
            let shape = self.live.with_artifacts(|store| {
                if spatial {
                    let mut observed = None;
                    for (view, segment) in fold_segments {
                        let held = self.deps.shapes.level(
                            view,
                            &layer,
                            level,
                            store,
                            &crate::shapes::PersistedPieces::none(),
                        );
                        let (piece, cost) = held.resolve(segment);
                        let piece = Arc::new(piece);
                        held.stage(&segment.seg_id, Arc::clone(&piece));
                        tracing::info!(
                            layer = %layer,
                            level,
                            view = %view,
                            seg_id = %segment.seg_id,
                            rows_tested = cost.rows_tested,
                            rows_interior = cost.rows_interior,
                            artifacts_empty = cost.artifacts_empty,
                            elapsed_ms = cost.elapsed_ms,
                            "the fold re-resolved a segment against a spatial level's shapes"
                        );
                        if view == first_view {
                            observed = Some(mosaica_store::derived::observe_shape(
                                space.base_rows(),
                                &|visit| {
                                    for (ordinal, rows) in piece.iter().enumerate() {
                                        if let Some(rows) = rows {
                                            visit(ordinal as u32, rows);
                                        }
                                    }
                                },
                            ));
                        }
                    }
                    observed.unwrap_or_else(crate::layout::LevelShape::empty)
                } else {
                    // A borrowing level's filed column is true when written and not read
                    // afterwards: the borrowed set moves with the target's version, so the engine
                    // serves it artifact-major instead (`crate::artifacts::ArtifactRows::inherit`).
                    mosaica_store::derived::observe_shape(space.base_rows(), &|visit| {
                        for (ordinal, record) in pending.records(store, &layer, level) {
                            visit(
                                ordinal,
                                &store
                                    .members_of(record)
                                    .projected(|part| space.project_base(part)),
                            );
                        }
                    })
                }
            });
            let chosen = crate::layout::choose(&registered.declaration, shape);
            tracing::info!(
                layer = %layer,
                level,
                artifacts = shape.artifacts,
                everywhere_fraction = shape.everywhere_fraction,
                blocks_per_artifact = shape.blocks_per_artifact,
                partitions = shape.partitions,
                pinned = ?registered.declaration.layout,
                was = ?registered.layout_of(level),
                now = ?chosen,
                "the fold re-evaluated a level's serving layout"
            );
            per_layer
                .entry(layer.clone())
                .or_default()
                .push((level, shape.artifacts));
            if !shape.partitions {
                overlapping.insert((layer.clone(), level));
            }
            out.push((layer, level, chosen));
        }
        // What a whole-layer response costs: the sum of the per-level counts above, reported and
        // never refused, since a large response is slow, not wrong, and discloses nothing the
        // mask did not already allow.
        for (layer, mut levels) in per_layer {
            levels.sort_unstable();
            let total: u64 = levels.iter().map(|(_, n)| *n).sum();
            tracing::info!(
                layer = %layer,
                evaluated_artifacts = total,
                per_level = ?levels,
                "the fold re-evaluated these levels of a layer; the sum is what a response naming \
                 them carries, one row per served artifact"
            );
        }
        (out, overlapping)
    }

    /// Write this prefix's columns, one file per `(view, layer, level)`: a row-major level's column,
    /// and for a level served artifact-major a label column where its memberships partition the
    /// rows (`mosaica_store::derived::level_column`). Every label column is copied into the order of
    /// the fold's segment's bands beside it. A row-major level whose column will not compose gets no
    /// file, and is served artifact-major instead (see `ArtifactProjections::get_or_build`).
    fn write_row_columns(
        &self,
        pass: &mut DerivedPass<'_>,
        layouts: &[(String, u32, mosaica_types::layer::ServingLayout)],
        overlapping: &Overlapping,
    ) -> Vec<mosaica_store::manifest::DerivedExtent> {
        let prefix_dir = pass.prefix_dir;
        let partition = pass.partition;
        let n = pass.n;
        let incarnations = &pass.incarnations;
        let spaces = &pass.spaces;
        let pending = pass.pending;
        let fold_segments = &pass.segments;
        let wanted: Vec<(String, u32, mosaica_types::layer::ServingLayout)> = layouts
            .iter()
            .filter(|(layer, level, _)| {
                self.composes_row_structures(pending, layer, *level)
                    // An attribute level has no stored memberships to compose a column from.
                    && self
                        .live
                        .registered_layer(layer)
                        .is_some_and(|registered| {
                            matches!(
                                registered.declaration.membership,
                                mosaica_types::layer::MembershipSource::Enumerated
                                    | mosaica_types::layer::MembershipSource::Spatial
                            )
                        })
            })
            .cloned()
            .collect();
        if wanted.is_empty() || spaces.is_empty() {
            return Vec::new();
        }

        let scratch = self.deps.artifact_projections.scratch();
        let written: Vec<(
            String,
            String,
            u32,
            u64,
            mosaica_store::manifest::DerivedForm,
            std::path::PathBuf,
        )> = self.live.with_artifacts(|store| {
            let mut out = Vec::with_capacity(wanted.len() * spaces.len());
            for (layer, level, layout) in &wanted {
                let partitions = !overlapping.contains(&(layer.clone(), *level));
                let Some((composed_as, form)) =
                    mosaica_store::derived::level_column(*layout, partitions)
                else {
                    continue;
                };
                let version = pending.version_after(store, layer, *level);
                let ordinals = level_length(pending.records(store, layer, *level));
                let spatial = self.live.registered_layer(layer).is_some_and(|registered| {
                    registered.declaration.membership
                        == mosaica_types::layer::MembershipSource::Spatial
                });
                for (view, space) in spaces {
                    let segment = fold_segments
                        .iter()
                        .find(|(v, _)| v == view)
                        .map(|(_, segment)| segment);
                    let composed = if spatial {
                        // The fold's segment is the whole base at row base 0, so the piece staged
                        // in `choose_layouts` is the level's membership in this view.
                        let piece = self.deps.shapes.get(view, layer, *level).and_then(|held| {
                            segment.and_then(|segment| held.staged(&segment.seg_id))
                        });
                        match piece {
                            Some(piece) => {
                                let rows = piece.as_ref();
                                mosaica_store::derived::project_row_column(
                                    rows.len() as u32,
                                    space.base_rows(),
                                    composed_as,
                                    scratch,
                                    &|visit| {
                                        for (ordinal, rows) in rows.iter().enumerate() {
                                            if let Some(rows) = rows {
                                                visit(ordinal as u32, rows);
                                            }
                                        }
                                    },
                                )
                            }
                            None => Ok(None),
                        }
                    } else {
                        mosaica_store::derived::project_row_column(
                            ordinals,
                            space.base_rows(),
                            composed_as,
                            scratch,
                            &|visit| {
                                for (ordinal, record) in pending.records(store, layer, *level) {
                                    visit(
                                        ordinal,
                                        &store
                                            .members_of(record)
                                            .projected(|part| space.project_base(part)),
                                    );
                                }
                            },
                        )
                    };
                    match composed {
                        Ok(Some(path)) => {
                            // A served column is filed only with its members beside it.
                            if matches!(
                                form,
                                mosaica_store::manifest::DerivedForm::RowColumn { .. }
                            ) {
                                match mosaica_store::derived::stage_row_members(
                                    &path,
                                    composed_as,
                                    scratch,
                                ) {
                                    Ok(members) => out.push((
                                        view.clone(),
                                        layer.clone(),
                                        *level,
                                        version,
                                        mosaica_store::manifest::DerivedForm::RowMembers,
                                        members,
                                    )),
                                    Err(error) => {
                                        tracing::warn!(
                                            layer = %layer,
                                            level,
                                            view = %view,
                                            %error,
                                            "a row-major column's members would not be written \
                                             at the fold; that level derives both on first use"
                                        );
                                        let _ = std::fs::remove_file(&path);
                                        continue;
                                    }
                                }
                            }
                            let labelled = segment.filter(|_| {
                                composed_as == mosaica_types::layer::ServingLayout::RowMajorLabel
                            });
                            if let Some(segment) = labelled {
                                match mosaica_store::derived::stage_band_labels(
                                    &path,
                                    &segment.bands,
                                    scratch,
                                ) {
                                    Ok(copy) => out.push((
                                        view.clone(),
                                        layer.clone(),
                                        *level,
                                        version,
                                        mosaica_store::manifest::DerivedForm::BandLabels {
                                            seg_id: segment.seg_id.clone(),
                                        },
                                        copy,
                                    )),
                                    Err(error) => tracing::warn!(
                                        layer = %layer,
                                        level,
                                        view = %view,
                                        %error,
                                        "a level's labels would not be copied into the order of \
                                         its segment's bands; the level has no copy"
                                    ),
                                }
                            }
                            out.push((
                                view.clone(),
                                layer.clone(),
                                *level,
                                version,
                                form.clone(),
                                path,
                            ))
                        }
                        // A level served artifact-major whose memberships overlap has no label
                        // column, and that is its shape rather than a fault.
                        Ok(None) if !layout.is_row_major() => {}
                        Ok(None) => tracing::warn!(
                            layer = %layer,
                            level,
                            view = %view,
                            layout = ?layout,
                            "a row-major column would not compose at the fold: this level's \
                             memberships do not partition, so it is served artifact-major. \
                             Every answer is unchanged; the layout is not"
                        ),
                        Err(error) => tracing::warn!(
                            layer = %layer,
                            level,
                            view = %view,
                            %error,
                            form = form.dir(),
                            "a level's column would not be composed at the fold; that level \
                             derives it on first use"
                        ),
                    }
                }
            }
            out
        });
        if written.is_empty() {
            return Vec::new();
        }

        mosaica_store::derived::file_derived(
            prefix_dir,
            partition,
            n,
            &mut pass.index,
            written
                .into_iter()
                .filter_map(|(view, layer, level, level_version, form, path)| {
                    // No incarnation, no file: an unstamped structure would label another
                    // view's rows.
                    let Some(incarnation) = incarnations.get(&view).copied() else {
                        let _ = std::fs::remove_file(&path);
                        return None;
                    };
                    Some(mosaica_store::derived::Filed {
                        view: Some(view),
                        incarnation: Some(incarnation),
                        layer,
                        level,
                        level_version,
                        form,
                        bytes: mosaica_store::derived::FiledBytes::Staged(path),
                    })
                })
                .collect(),
        )
    }

    /// Write this prefix's shape row forms: for every spatial level `columns` does not cover, the
    /// piece the fold resolved for each of its segments in `choose_layouts`, keyed by that segment
    /// and the level's version.
    ///
    /// Every failure is a dropped entry, not a discarded fold: the form is derived, and an open
    /// that finds no entry resolves the segment again.
    fn write_shape_rows(
        &self,
        pass: &mut DerivedPass<'_>,
        columns: &[mosaica_store::manifest::DerivedExtent],
    ) -> Vec<mosaica_store::manifest::DerivedExtent> {
        let prefix_dir = pass.prefix_dir;
        let partition = pass.partition;
        let n = pass.n;
        let incarnations = &pass.incarnations;
        let pending = pass.pending;
        let fold_segments = &pass.segments;
        let levels: Vec<(String, u32)> = self.live.with_artifacts(|store| {
            levels_of(store)
                .filter(|(layer, level)| !pending.is_pending(layer, *level))
                .filter(|(layer, _)| self.live.spatial_layer(layer))
                .collect()
        });
        let mut filed: Vec<mosaica_store::derived::Filed> = Vec::new();
        for (layer, level) in &levels {
            let version = self
                .live
                .with_artifacts(|store| store.level_version(layer, *level));
            for (view, segment) in fold_segments {
                let covered = columns.iter().any(|c| {
                    matches!(
                        c.form,
                        mosaica_store::manifest::DerivedForm::RowColumn { .. }
                    ) && &c.layer == layer
                        && c.level == *level
                        && c.view.as_ref() == Some(view)
                });
                if covered {
                    continue;
                }
                let Some(piece) = self
                    .deps
                    .shapes
                    .get(view, layer, *level)
                    .and_then(|held| held.staged(&segment.seg_id))
                else {
                    tracing::warn!(
                        layer = %layer,
                        level,
                        view = %view,
                        seg_id = %segment.seg_id,
                        "the fold holds no resolved piece for this segment, so no row form is \
                         written; the next open resolves it"
                    );
                    continue;
                };
                // No incarnation, no file; see `write_row_columns`.
                let Some(incarnation) = incarnations.get(view).copied() else {
                    continue;
                };
                filed.push(mosaica_store::derived::Filed {
                    view: Some(view.clone()),
                    incarnation: Some(incarnation),
                    layer: layer.clone(),
                    level: *level,
                    level_version: version,
                    form: mosaica_store::manifest::DerivedForm::ShapeRows {
                        seg_id: segment.seg_id.clone(),
                        row_count: segment.row_count,
                    },
                    bytes: mosaica_store::derived::FiledBytes::InHand(
                        mosaica_store::derived::shape_rows_bytes(
                            version,
                            &segment.seg_id,
                            segment.row_count,
                            &piece,
                        ),
                    ),
                });
            }
        }
        mosaica_store::derived::file_derived(prefix_dir, partition, n, &mut pass.index, filed)
    }

    /// Write this prefix's persisted decompositions: every spatial level's held shapes for each
    /// fold view, as the level holds them at its current version.
    fn write_shape_held(
        &self,
        pass: &mut DerivedPass<'_>,
    ) -> Vec<mosaica_store::manifest::DerivedExtent> {
        let prefix_dir = pass.prefix_dir;
        let partition = pass.partition;
        let n = pass.n;
        let incarnations = &pass.incarnations;
        let pending = pass.pending;
        let fold_segments = &pass.segments;
        let filed: Vec<mosaica_store::derived::Filed> = self.live.with_artifacts(|store| {
            let mut out = Vec::new();
            let levels: Vec<(String, u32)> = levels_of(store)
                .filter(|(layer, level)| !pending.is_pending(layer, *level))
                .filter(|(layer, _)| self.live.spatial_layer(layer))
                .collect();
            for (layer, level) in &levels {
                let version = store.level_version(layer, *level);
                for (view, _) in fold_segments {
                    let Some(held) = self.deps.shapes.get(view, layer, *level) else {
                        continue;
                    };
                    if held.level_version != version {
                        continue;
                    }
                    let shapes: Vec<(Option<&[u8]>, Option<&mosaica_store::derived::HeldShape>)> =
                        (0..held.shapes.len() as u32)
                            .map(|ordinal| {
                                (
                                    store
                                        .shape_of(layer, *level, ordinal)
                                        .and_then(|shapes| shapes.for_view(view)),
                                    held.shapes[ordinal as usize].as_ref(),
                                )
                            })
                            .collect();
                    let Some(incarnation) = incarnations.get(view).copied() else {
                        continue;
                    };
                    out.push(mosaica_store::derived::Filed {
                        view: Some(view.clone()),
                        incarnation: Some(incarnation),
                        layer: layer.clone(),
                        level: *level,
                        level_version: version,
                        form: mosaica_store::manifest::DerivedForm::ShapeHeld,
                        bytes: mosaica_store::derived::FiledBytes::InHand(
                            mosaica_store::derived::shape_held_bytes(version, &shapes),
                        ),
                    });
                }
            }
            out
        });
        mosaica_store::derived::file_derived(prefix_dir, partition, n, &mut pass.index, filed)
    }

    /// Project and write this prefix's tile-index extent columns, one file per
    /// `(view, layer, level)` whose layout is not row-major and whose membership is not spatial.
    fn write_tile_indexes(
        &self,
        pass: &mut DerivedPass<'_>,
        layouts: &[(String, u32, mosaica_types::layer::ServingLayout)],
    ) -> Vec<mosaica_store::manifest::DerivedExtent> {
        let prefix_dir = pass.prefix_dir;
        let partition = pass.partition;
        let n = pass.n;
        let incarnations = &pass.incarnations;
        let spaces = &pass.spaces;
        let pending = pass.pending;
        if spaces.is_empty() {
            return Vec::new();
        }

        let projected: Vec<(String, String, u32, u64, Vec<u8>)> =
            self.live.with_artifacts(|store| {
                let levels: Vec<(String, u32)> = store
                    .levels_and_extents()
                    .map(|(layer, level, _)| (layer.to_string(), level))
                    .filter(|(layer, level)| self.composes_row_structures(pending, layer, *level))
                    .filter(|(layer, level)| {
                        !layouts
                            .iter()
                            .any(|(l, v, layout)| l == layer && v == level && layout.is_row_major())
                    })
                    .filter(|(layer, _)| {
                        !self.live.registered_layer(layer).is_some_and(|registered| {
                            registered.declaration.membership
                                == mosaica_types::layer::MembershipSource::Spatial
                        })
                    })
                    .collect();
                let mut out = Vec::with_capacity(levels.len() * spaces.len());
                for (layer, level) in &levels {
                    let version = pending.version_after(store, layer, *level);
                    let ordinals = level_length(pending.records(store, layer, *level));
                    for (view, space) in spaces {
                        let index = crate::tile_index::TileIndex::project(
                            ordinals,
                            || pending.records(store, layer, *level),
                            space,
                            store,
                        );
                        out.push((
                            view.clone(),
                            layer.clone(),
                            *level,
                            version,
                            index.as_bytes().to_vec(),
                        ));
                    }
                }
                out
            });
        if projected.is_empty() {
            return Vec::new();
        }

        mosaica_store::derived::file_derived(
            prefix_dir,
            partition,
            n,
            &mut pass.index,
            projected
                .into_iter()
                .filter_map(|(view, layer, level, level_version, bytes)| {
                    let incarnation = *incarnations.get(&view)?;
                    Some(mosaica_store::derived::Filed {
                        view: Some(view),
                        incarnation: Some(incarnation),
                        layer,
                        level,
                        level_version,
                        form: mosaica_store::manifest::DerivedForm::TileIndex,
                        bytes: mosaica_store::derived::FiledBytes::InHand(bytes),
                    })
                })
                .collect(),
        )
    }

    /// Rebuild every level's row-space membership, and every lineage this fold moved, against the
    /// live generation.
    ///
    /// Called synchronously at the fold's own publication: the alternative is not a cache miss but
    /// a stall on whichever request arrives first. A view the generation does not carry is simply
    /// not warmed; its first request builds what it needs.
    pub(super) fn warm_artifact_caches(&self) {
        let generation = self.generation.load_full();
        let levels: Vec<(String, u32)> =
            self.live.with_artifacts(|store| levels_of(store).collect());
        if levels.is_empty() {
            return;
        }
        let started = std::time::Instant::now();
        let before_projections = self.deps.artifact_projections.builds();
        let before_lineages = self.deps.lineages.builds();
        for partition in generation.bundle.partitions.values() {
            for (view, view_data) in &partition.views {
                for (layer, level) in &levels {
                    // The layout the fold has just recorded, so the warm builds the form the next
                    // request will ask for rather than one it would immediately replace.
                    let layout = self
                        .live
                        .registered_layer(layer)
                        .map(|registered| registered.layout_of(*level))
                        .unwrap_or_default();
                    // A predicate level is not warmed here: it is evaluated against the geometry
                    // by the request path, not assembled from a stored membership.
                    let registered = self.live.registered_layer(layer);
                    let membership = registered
                        .as_ref()
                        .map(|r| r.declaration.membership.clone());
                    let spatial = registered
                        .as_ref()
                        .is_some_and(|r| spatial_membership(&r.declaration));
                    if matches!(
                        membership,
                        Some(mosaica_types::layer::MembershipSource::Attribute(_))
                    ) || (!spatial
                        && !matches!(
                            membership,
                            Some(mosaica_types::layer::MembershipSource::Enumerated)
                        ))
                    {
                        continue;
                    }
                    let Some(registered) = registered.as_ref() else {
                        continue;
                    };
                    let segments = if spatial {
                        crate::viewport::segments_with_row_bases(view, view_data).ok()
                    } else {
                        None
                    };
                    self.live.with_artifacts(|store| {
                        let predicate = segments.as_ref().map(|segments| {
                            crate::artifacts::PredicateSource::Spatial(
                                crate::artifacts::SpatialSource {
                                    level: self.deps.shapes.level(
                                        view,
                                        layer,
                                        *level,
                                        store,
                                        &crate::shapes::PersistedPieces::none(),
                                    ),
                                    segments,
                                    total_rows: u32::try_from(view_data.row_space.total_rows())
                                        .unwrap_or(u32::MAX),
                                },
                            )
                        });
                        self.deps.artifact_projections.get_or_build(
                            &generation.prefix,
                            view,
                            layer,
                            *level,
                            store,
                            &view_data.row_space,
                            Some(&generation.partition_source()),
                            layout,
                            predicate.as_ref(),
                            generation.segments_version,
                            &registered.declaration,
                        )
                    });
                }
            }
        }
        for (layer, level) in &levels {
            self.live.with_artifacts(|store| {
                self.deps.lineages.get_or_build(
                    layer,
                    *level,
                    store.lineage_version(layer, *level),
                    || {
                        crate::cut::Lineage::new(store.level(layer, *level).map(
                            |(ordinal, record)| {
                                let within = record
                                    .parents
                                    .iter()
                                    .find(|parent| parent.level == *level)
                                    .map(|parent| parent.ordinal);
                                (ordinal, within)
                            },
                        ))
                    },
                )
            });
        }
        tracing::info!(
            projections = self.deps.artifact_projections.builds() - before_projections,
            lineages = self.deps.lineages.builds() - before_lineages,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "the fold's artifact pass rebuilt every level's row form"
        );
    }

}
/// The segments a fold wrote, opened from the prefix it wrote them into: what its artifact pass
/// resolves every spatial level against. A segment that will not open is skipped and logged; the
/// level then resolves it at the flip's warm instead, on the executor.
pub(super) fn fold_segments(
    prefix_dir: &std::path::Path,
    partition: &str,
    segments: &[mosaica_store::manifest::SegmentDescriptor],
) -> Vec<(String, mosaica_store::read::SegmentData)> {
    let mut out = Vec::with_capacity(segments.len());
    for descriptor in segments {
        let partition_dir = prefix_dir.join("partitions").join(partition);
        let dir = mosaica_store::view_path(&partition_dir, &descriptor.view)
            .join("segments")
            .join(&descriptor.seg_id);
        // A fold writes each view's base, whose rows name their entities in the view's table.
        let table = dir
            .parent()
            .and_then(std::path::Path::parent)
            .map(|view| view.join(mosaica_store::ROW_ENTITY_FILE))
            .filter(|path| path.exists())
            .map(|path| mosaica_store::RowToEntity::load(&path));
        let entities = match table {
            Some(Ok(table)) => {
                mosaica_store::edited::RowEntities::Table(std::sync::Arc::new(table))
            }
            Some(Err(e)) => {
                tracing::warn!(
                    view = %descriptor.view,
                    error = %e,
                    "the fold could not reopen a row-to-entity file it just wrote; its spatial \
                     memberships are resolved at the flip instead"
                );
                continue;
            }
            None => mosaica_store::edited::RowEntities::Numbers,
        };
        match mosaica_store::read::SegmentData::load(
            &dir,
            &descriptor.seg_id,
            descriptor.row_count,
            entities,
        ) {
            Ok(segment) => out.push((descriptor.view.clone(), segment)),
            Err(e) => tracing::warn!(
                view = %descriptor.view,
                seg_id = %descriptor.seg_id,
                error = %e.source,
                "the fold could not reopen a segment it just wrote; its spatial memberships are \
                 resolved at the flip instead"
            ),
        }
    }
    out
}

/// One superseded prefix awaiting reclamation, and the generations published over it, whose
/// release says no thread can still resolve a path inside it. See [`Executor::superseded`].
pub(in crate::write) struct PendingReclaim {
    generations: Vec<std::sync::Weak<Generation>>,
    prefix_dir: PathBuf,
}

/// Seconds since the Unix epoch, or `None` if the clock is before it.
///
/// `None` reads as "no fold has ended yet", switching the interval floor off rather than jamming it
/// on: the safe direction, and the same answer a fresh process gives.
pub(in crate::write) fn unix_now() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|elapsed| elapsed.as_secs())
}
