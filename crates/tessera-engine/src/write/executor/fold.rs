use super::*;

/// Memory this process could take without reclaiming anything it needs, in bytes. `None` where
/// unknowable.
///
/// Reads two sources and takes the smaller, because either can be the real bound. `MemAvailable`
/// is the kernel's own estimate of what an allocation could get without swapping, already net of
/// the page cache it would evict. A cgroup v2 `memory.max` is the ceiling a container is killed
/// at, and it charges page cache against itself, so a node with 400 GiB of host RAM and a 16 GiB
/// cgroup is bounded by the cgroup. Reading only the first lets a fold pass its pre-flight and
/// then be OOM-killed by the container that always owned the answer.
pub(super) fn available_memory() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let available = meminfo
        .lines()
        .find(|line| line.starts_with("MemAvailable:"))
        .and_then(|line| line.split_whitespace().nth(1)?.parse::<u64>().ok())
        .map(|kib| kib * 1024)?;

    // `memory.max` is "max" when unlimited, which parses to `None` and leaves `MemAvailable` as
    // the answer, the same result as no cgroup at all.
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

/// Delete every `v#####` tree under the bundle root that `CURRENT` does not name, once, before
/// the executor thread is spawned.
///
/// Two things leave an orphaned prefix behind, and nothing else reclaims either: a discarded fold
/// (its output sits under a name `CURRENT` never took), and a process that exits mid-fold, between
/// the `CURRENT` flip and the swap, or while a superseded prefix still has a reader. Left alone,
/// each occurrence costs a bundle's worth of disc until an operator notices.
///
/// This runs only at the moment "not named by `CURRENT`" and "not in use" coincide. Mid-life, a
/// prefix `CURRENT` does not name may still be one this process is serving, since the live
/// generation holds mappings into it until the last request finishes ([`Executor::pending_reclaim`]
/// waits on that). At the instant the write executor starts there is no such generation: nothing
/// has been served yet, and the only prefix any mapping can name is the one `Engine::open` read
/// from `CURRENT`.
///
/// Called synchronously from `start_executor`, before the thread is spawned. A directory that
/// exists but is not yet committed is indistinguishable from an orphan, which is correct for a
/// fold's own output but wrong for a prefix a caller is staging through
/// `Engine::publish_rotated_prefix_for_test`; running this on the spawned thread would race that
/// staging. Running it here also lets a caller observe that the sweep has finished, by
/// `start_write_executor` having returned.
///
/// Not called from `Engine::open`: a node that has not started a write executor has not declared
/// itself the bundle's writer, and deleting another process's superseded prefix from a read-only
/// replica is not this crate's judgement to make. It would be safe to do so (a POSIX mapping
/// outlives its directory entry, and a fresh open resolves through `CURRENT`, which this sweep
/// never touches), but safe is not the same as this crate's to do.
///
/// `next_prefix_name` counts from the directory listing, so once an orphan `v00003` is gone the
/// next fold may reuse the name `v00003`. That is safe because what identifies a bundle is its
/// `MANIFEST.json` digest, which `CURRENT` carries beside the prefix name, not the prefix name
/// itself.
///
/// Every failure to reclaim a tree is a warning and nothing else: `reclaim_prefix` refuses the
/// live prefix as a second guard behind this function's own filter, and a tree that cannot be
/// deleted simply stays, the same residual as before this sweep existed.
pub(in crate::write) fn sweep_orphan_prefixes(bundle_root: &Path, live: &str) {
    let Ok(entries) = std::fs::read_dir(bundle_root) else {
        return;
    };
    let (mut swept, mut refused) = (0usize, 0usize);
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        // A prefix is `v` + five-or-more digits, matching `next_prefix_name`'s own shape.
        // Anything else under the root, such as `CURRENT`, the WAL, or a directory an operator
        // put there, is not this sweep's business.
        let is_prefix = name
            .strip_prefix('v')
            .is_some_and(|digits| digits.len() >= 5 && digits.bytes().all(|b| b.is_ascii_digit()));
        if !is_prefix || name == live || !entry.path().is_dir() {
            continue;
        }
        match tessera_store::reclaim_prefix(&entry.path()) {
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
/// Sums two manifests: the build's artefacts, digested in the bundle-level `MANIFEST.json`, and
/// everything the write path produced, in the partition's `SEGMENTS-<n>.json`. Summing only the
/// side-manifest understates the named bytes badly, since it leaves out the build's own artefacts.
///
/// The gap between on-disc and named bytes is every merged-away segment, every consumed tier and
/// every superseded side-manifest. They stay on disc because a step-down can still serve one, and
/// a fold is the only thing that reclaims them, which is what makes this gauge a fold trigger
/// rather than an alarm.
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

/// Free bytes on the filesystem holding `path`, or `None` where unknowable, in which case the
/// pre-flight disc check is skipped rather than refusing on a guess.
///
/// Reads `f_bavail`, not `f_bfree`: the reserved blocks a filesystem keeps for root are not space
/// a fold may plan to use.
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

/// The largest live segment count across this generation's views.
///
/// The maximum rather than the sum, because a tile resolves to one contiguous range per live
/// segment of the view it is in, so what a viewport in one view pays is that view's own segment
/// count, whatever the others hold.
///
/// No build today emits a second view, so max and sum agree on every bundle that exists; this is
/// written for when that changes.
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

/// Is this extent's `(view, incarnation)` pair one the live manifest still declares?
///
/// An entity-scoped extent belongs to no view and is always carried: its `view` is `None`, and so
/// is its incarnation. A group-scoped one is carried only where the pair is live. A dropped key's
/// columns sit in the list until a fold reclaims them, and a key created again writes its own
/// column under the same family name.
///
/// Fail-closed by construction: a half-stamped entry, a view with no incarnation or the reverse,
/// matches nothing and is omitted. That loses a derived artefact and never serves one from the
/// wrong incarnation.
pub(super) fn carries_live_view(
    live: &FxHashMap<&str, tessera_types::view::ViewIncarnation>,
    view: Option<&str>,
    incarnation: Option<tessera_types::view::ViewIncarnation>,
) -> bool {
    match (view, incarnation) {
        (None, None) => true,
        (Some(view), Some(incarnation)) => live.get(view) == Some(&incarnation),
        _ => false,
    }
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
        && plan
            .runs
            .iter()
            .all(|r| live_manifest.external_id_runs.contains(r))
        && plan.locator_extents.iter().all(|path| {
            live_manifest
                .locator_extents
                .iter()
                .any(|extent| &extent.path == path)
        })
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
}

/// What [`Executor::fold_still_applies`] found: what the fold carries forward from the live
/// manifest, the deletions it retires, and how many entities the carried extents hold.
struct FoldApplies<'a> {
    forward: CarriedExtents<'a>,
    executed: croaring::Bitmap,
    carried_entities: u64,
}

/// What every derived writer of one fold shares: where the files go, the row spaces and segments
/// the fold has just written, the levels its retirement is about to move, and the file counter.
struct DerivedPass<'a> {
    prefix_dir: &'a std::path::Path,
    partition: &'a str,
    n: u64,
    incarnations: FxHashMap<String, tessera_types::view::ViewIncarnation>,
    spaces: Vec<(String, tessera_store::RowSpace)>,
    segments: Vec<(String, tessera_store::read::SegmentData)>,
    pending: &'a PendingRetirement,
    index: tessera_store::derived::DerivedIndex,
}

/// What a fold carries forward: everything the live manifest still lists that the fold did not
/// consume, i.e. what published during its flight.
///
/// Listed order is preserved in every one of these. For segments it is entity order, which
/// `RowSpace::with_extent` requires; for runs it is recency, which newest-first resolution reads;
/// elsewhere it keeps a manifest's bytes from depending on a set's iteration order.
///
/// Nothing of a dead incarnation is carried. A dropped view has no base in the fold's plan and
/// never consumed its segments, so carrying its segments forward would name an incarnation the new
/// manifest does not declare; they are left behind with the superseded prefix's files instead. The
/// key is `(view, incarnation)` and not the view alone because a dropped view id may be declared
/// again, and a segment stamped with the dead incarnation would then serve the new view the old
/// one's points. Without this filter, every fold after such a drop is discarded by the base check,
/// so compaction stops for the life of the bundle.
pub(super) struct CarriedExtents<'a> {
    segments: Vec<&'a tessera_store::manifest::SegmentDescriptor>,
    tiers: Vec<String>,
    runs: Vec<String>,
    locators: Vec<tessera_store::manifest::LocatorExtent>,
    attrs: Vec<tessera_store::manifest::AttrExtent>,
    records: Vec<tessera_store::manifest::RecordExtent>,
    texts: Vec<tessera_store::manifest::TextExtent>,
    entity_terms: Vec<tessera_store::manifest::EntityTermsExtent>,
    /// The dropped views whose segments were left behind, and how many segments that was.
    omitted_views: Vec<String>,
    omitted_segments: usize,
}

/// The entries of `live` the fold did not consume, in listed order.
///
/// The five extent lists are identified by the file the fold consumed them by: the values,
/// blocks, dictionary or terms path, each `seg_id`-derived and never reused. Dropping an entry the
/// fold did not carry forward is never a refusal to open; it answers with an empty result instead.
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
    live_incarnations: &FxHashMap<&str, tessera_types::view::ViewIncarnation>,
    consumed_segments: &FxHashSet<(&str, &str)>,
) -> CarriedExtents<'a> {
    // Owned, because the log line that reports them outlives the generation this borrows from.
    let mut omitted_views: Vec<String> = Vec::new();
    let mut omitted_segments = 0usize;
    let segments: Vec<&tessera_store::manifest::SegmentDescriptor> = live_manifest
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
    // Nothing of a dead incarnation here either: a group-scoped column's extents outlive the drop
    // that orphaned them, and a key created again writes its own column under the same family
    // name.
    attrs.retain(|extent| {
        carries_live_view(
            live_incarnations,
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
        carries_live_view(
            live_incarnations,
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
        runs: live_manifest
            .external_id_runs
            .iter()
            .filter(|r| !plan.runs.contains(r))
            .cloned()
            .collect(),
        locators: live_manifest
            .locator_extents
            .iter()
            .filter(|extent| !plan.locator_extents.contains(&extent.path))
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
        omitted_views,
        omitted_segments,
    }
}

/// Exactly the files the new manifest names that the fold did not write, deduplicated: a carried
/// segment's run and locator are already in the run and locator lists, and linking one path twice
/// is what `hard_link_forward` refuses.
///
/// Every file an entry names is included, never a subset. A segment carries its morton, cut index
/// and columns, plus a `presence/<column>.roaring` per rendered column that has an absence (a
/// missing presence file reads as every row present). An attribute extent carries its values and
/// presence always, and its dictionary, postings and offsets wherever the entry names them; a
/// record extent carries all three of its files, a text extent all three of its, a transpose
/// extent all four.
///
/// Taken from the manifest and not from a directory scan, because a scan finds what is there and
/// the manifest says what must be.
pub(super) fn carried_files(
    partition: &str,
    live_manifest: &SegmentsManifest,
    forward: &CarriedExtents,
) -> std::collections::BTreeSet<String> {
    let mut rels: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for descriptor in &forward.segments {
        let segment_prefix = format!(
            "partitions/{}/{}/segments/{}",
            partition,
            tessera_store::view_rel(&descriptor.view),
            descriptor.seg_id
        );
        for name in [
            "morton.u32",
            tessera_store::read::CutIndex::FILE,
            "columns.arrow",
        ] {
            rels.insert(format!("{segment_prefix}/{name}"));
        }
        let presence_prefix = format!("{segment_prefix}/{}/", RENDER_PRESENCE_DIR);
        rels.extend(
            live_manifest
                .files
                .keys()
                .filter(|rel| rel.starts_with(&presence_prefix))
                .cloned(),
        );
    }
    rels.extend(forward.runs.iter().cloned());
    rels.extend(forward.locators.iter().map(|e| e.path.clone()));
    rels.extend(forward.tiers.iter().cloned());
    for extent in &forward.attrs {
        rels.insert(extent.values.clone());
        rels.insert(extent.presence.clone());
        rels.extend(extent.dict.iter().cloned());
        rels.extend(extent.postings.iter().cloned());
        rels.extend(extent.offsets.iter().cloned());
    }
    for extent in &forward.records {
        rels.insert(extent.blocks.clone());
        rels.insert(extent.hasrow.clone());
        rels.insert(extent.directory.clone());
    }
    for extent in &forward.texts {
        rels.insert(extent.dict.clone());
        rels.insert(extent.postings.clone());
        rels.insert(extent.presence.clone());
    }
    for extent in &forward.entity_terms {
        rels.insert(extent.hasrow.clone());
        rels.insert(extent.offsets.clone());
        rels.insert(extent.terms.clone());
        rels.insert(extent.bases.clone());
    }
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
    /// Every gauge is read off the generation this tick loaded, so they agree with each other and
    /// with the plan the dispatch is about to take. Three of the four are field reads (a `len` per
    /// view, a bitmap cardinality, a sum of row counts), which is what lets this run at every tick
    /// rather than on a cadence of its own.
    ///
    /// The fourth is a directory walk, so it is passed as a closure: `due` calls it only after
    /// every cheaper route has declined, so a deployment whose segments or deletions have already
    /// dispatched a fold never pays for it. What it walks is the live prefix, not the bundle root:
    /// an orphaned prefix from a discarded fold is dead bytes too, but it is the startup sweep's to
    /// reclaim, not a fold's, so counting it here would dispatch folds that cannot reduce it.
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
            &self.compaction,
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

    /// The instant `compaction_min_interval_secs` is measured from.
    ///
    /// The rule: a fold may start no sooner than the interval after the previous one started, and
    /// never before the previous one has ended. Both halves matter.
    ///
    /// Measuring from the end alone makes the floor a function of the fold's own duration, which
    /// drifts the window off its schedule: a fold that starts at 00:10 and takes three hours ends
    /// at 03:10, so a floor measured from 03:10 pushes tomorrow's 00:00 window later, folding on
    /// alternate nights instead of every night.
    ///
    /// Measuring from the start alone is fail-open the other way: a fold that runs longer than the
    /// interval and then discards clears the floor the instant it ends, leaving the gauge that
    /// dispatched it unchanged and re-dispatching the same fold repeatedly (see
    /// `last_fold_start_unix`). Taking `end − interval` when that is the later origin costs a long
    /// fold one further interval of quiet and costs a short one nothing, since `end − interval` is
    /// then behind its own start.
    ///
    /// `fold_ended_unix` is the end of the fold's passes, written by its own thread. The
    /// publication that follows is executor work of seconds and is not counted.
    pub(super) fn fold_floor_from(&self) -> Option<u64> {
        let started = self.last_fold_start_unix?;
        let ended = self.health.fold_ended_unix.load(Ordering::SeqCst);
        Some(started.max(ended.saturating_sub(self.compaction.min_interval_secs)))
    }

    /// Plan a fold and start it on its own thread, if one is requested and nothing blocks it.
    ///
    /// Not on the shared pool: flush, merge and coalesce all execute on the rayon pool a
    /// viewport's tile loop installs onto, which is right for them because
    /// `max_merged_segment_bytes` bounds what they do. A fold's input is the corpus, so occupying
    /// request-serving workers for its duration would let maintenance work compete with the
    /// product it serves.
    ///
    /// The request flag is consumed by a refusal but not by a suspension. A gate that is poisoned,
    /// diverged or stepped down is a state an operator must act on, so a request against it is
    /// refused: answered with one warning and dropped, rather than re-planned every tick. A merge
    /// or coalesce that is outstanding (running, or completed and not yet published) is neither,
    /// so the fold is suspended instead: the flag stays armed and the next tick tries again once
    /// that pass lands (see [`Executor::fold_outstanding`]).
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
        if self.fold_in_flight.load(Ordering::SeqCst) {
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

        let plan = match crate::compact::plan_fold(
            generation,
            self.wal.is_poisoned(),
            self.health.overlay_diverged.load(Ordering::SeqCst),
            crate::compact::FoldResources {
                available_memory: available_memory(),
                free_disc: free_disc(&self.bundle_root),
                // Read here rather than in the planner, which is pure over the generation and has
                // no route to the resident artifact store: this is the one term of the fold's
                // budget that cannot be derived from a manifest. A deployment with no artifacts
                // pays one empty iteration for it.
                membership_containers: self
                    .live
                    .with_artifacts(|store| store.membership_containers()),
            },
        ) {
            Ok(plan) => plan,
            Err(reason) => {
                self.health.fold_requested.store(false, Ordering::SeqCst);
                // Recorded on `/control/status` and not only in the log line: a refusal moves
                // neither `folds` nor `fold_failures`, so the status endpoint would otherwise be
                // silent about a condition a deployment cannot leave on its own (see
                // `ExecutorHealth::fold_refusals`).
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
        // One schema per view the fold will rewrite: the bundle-wide render tail plus that view's
        // group-scoped render lanes. A single bundle-wide list would drop a family's lane from
        // every rewritten segment of a group's view.
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
        // The columns declared at a running service and not yet folded, by name: the fold writes
        // each a base from its extents alone, and the publication takes them off the runtime
        // list. Taken from the live list at the plan, so a declaration made while the fold runs
        // is not among them.
        let (runtime_attributes, runtime_scoped_attributes) = {
            let (entity, scoped) = self.live.attributes_for_publication();
            (
                entity.into_iter().map(|d| d.name).collect::<Vec<_>>(),
                scoped.into_iter().map(|f| f.name).collect::<Vec<_>>(),
            )
        };
        // Per view, the columns an input segment may lawfully lack: the runtime columns above and
        // the view's group-scoped lanes. Any other missing column fails the fold as a torn
        // segment. Read from `runtime_attributes` rather than from a second snapshot of the live
        // list, because a declaration landing between two reads would put a column on one list and
        // not the other, and the fold would then either refuse a lawful absence or accept a torn
        // one.
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
        let to_prefix = match crate::compact::next_prefix_name(&self.bundle_root) {
            Ok(prefix) => prefix,
            Err(e) => {
                self.health.fold_requested.store(false, Ordering::SeqCst);
                self.health.fold_failures.fetch_add(1, Ordering::Relaxed);
                tracing::error!(error = %e, "could not name a new prefix for the fold");
                return;
            }
        };

        self.fold_attempt += 1;
        let ctx = crate::compact::FoldContext {
            from_prefix_dir: self.prefix_dir(generation),
            to_prefix_dir: self.bundle_root.join(&to_prefix),
            to_prefix: to_prefix.clone(),
            identity_key: self.identity_key,
            shard_id: manifest.identity.shard_id,
            scalar_schema,
            absent_ok,
            runtime_attributes,
            runtime_scoped_attributes,
            // The same never-reused shape a flush's and a merge's `seg_id` have. A fold writes
            // into a fresh prefix, so nothing can collide today; the id still must be unique
            // because a `seg_id` naming two different segments across a bundle's life would make a
            // rebase's ABA check meaningless.
            seg_id: format!("fold-{}-{}", partition_data.segments_n, self.fold_attempt),
            base_postings: Arc::clone(&generation.postings),
            tiers: generation.delta_postings.clone(),
            declared_scalars: manifest.declared_scalars.clone(),
            // The scoped families, flattened: the attribute pass folds one column per view of
            // each, and a fold that omitted them would write a prefix their directories are
            // absent from.
            scoped_scalars: manifest.scoped_scalars(),
            // The roster those families' view ids are placed by: a scoped column's directory
            // carries the incarnation above the build's, so the fold has to write it where the
            // opener will look.
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
        self.fold_in_flight.store(true, Ordering::SeqCst);
        let in_flight = Arc::clone(&self.fold_in_flight);
        let health = Arc::clone(&self.health);
        let submit = self.fold_submit.clone();
        let paused = Arc::clone(&self.fold_paused);
        let spawned = std::thread::Builder::new()
            .name("tessera-fold".to_string())
            .spawn(move || {
                match crate::compact::execute(plan, ctx) {
                    Ok(mut completed) => {
                        // A test holding the fold here models the flight a real corpus gives for
                        // free; see `Engine::set_fold_paused_for_test`. Always false otherwise.
                        health.fold_holding.store(true, Ordering::SeqCst);
                        while paused.load(Ordering::SeqCst) {
                            std::thread::sleep(std::time::Duration::from_millis(5));
                        }
                        health.fold_holding.store(false, Ordering::SeqCst);
                        // After the hold, so the staircase's hand-off row is the hand-off.
                        completed.finished = std::time::Instant::now();
                        // Set before the send, exactly as a flush's is; see
                        // `ExecutorHealth::flush_completed_pending` for the handshake's ordering.
                        health.fold_completed_pending.store(true, Ordering::SeqCst);
                        let _ = submit.send(completed);
                    }
                    Err(e) => {
                        health.fold_failures.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!(
                            error = %e,
                            "a compaction fold failed; its files are orphans under a prefix \
                             CURRENT does not name, nothing retired, and the next trigger \
                             re-plans from scratch"
                        );
                    }
                }
                // The attempt's end, recorded on the thread because one of its two exits never
                // reaches the executor; see `fold_floor_from`. Written before the in-flight flag
                // clears, so a tick that observes the fold finished also observes when.
                health
                    .fold_ended_unix
                    .store(unix_now().unwrap_or(0), Ordering::SeqCst);
                in_flight.store(false, Ordering::SeqCst);
            });
        if spawned.is_ok() {
            // This attempt's start; see `fold_floor_from`.
            self.last_fold_start_unix = unix_now();
        }
        if let Err(e) = spawned {
            // The closure, and with it the in-flight clone, was dropped, so the flag is cleared
            // through the field rather than through the copy that never ran.
            self.fold_in_flight.store(false, Ordering::SeqCst);
            self.health.fold_failures.fetch_add(1, Ordering::Relaxed);
            tracing::error!(error = %e, "the OS refused a thread for the compaction fold");
        }
    }

    /// Apply every completed fold waiting from its thread, and report whether any did.
    pub(super) fn publish_completed_folds(&mut self) -> bool {
        // Left in the channel rather than dropped; see `MaintenanceDeps::fold_publication_paused`.
        // Always false in a shipped build.
        if self.fold_publication_paused.load(Ordering::SeqCst) {
            return false;
        }
        let mut any = false;
        while let Ok(completed) = self.fold_done.try_recv() {
            self.publish_fold(completed);
            any = true;
        }
        if any {
            self.health
                .fold_completed_pending
                .store(false, Ordering::SeqCst);
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
        data_plugin_hash: &str,
        pending: &PendingRetirement,
    ) -> Vec<tessera_store::manifest::DerivedExtent> {
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
            // Stamped from the plan, which is what the row spaces came from: a view created
            // since the plan has no space here to describe.
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
        let mut derived = self.write_containment_partitions(&mut pass, data_plugin_hash);
        // Layouts are chosen before the files and the registry snapshot, or the fold would
        // publish a level in its old layout under a record claiming the new one. A level whose
        // layout flipped has its held forms dropped: nothing would ever claim them again.
        let layouts = self.choose_layouts(&pass.spaces, pending, &pass.segments);
        for (layer, level, chosen) in &layouts {
            if self.live.record_layout(layer, *level, *chosen) {
                self.artifact_projections.forget_level(layer, *level);
            }
        }
        // The fold rewrites every level, so the deltas held for the tick describe forms that are
        // going.
        self.pending_forms.clear();
        derived.extend(self.write_tile_indexes(&mut pass, &layouts));
        derived.extend(self.write_row_columns(&mut pass, &layouts));
        // The row forms of the spatial levels the columns do not cover.
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
        // The durability gates are asked again here, hours after the plan asked them. A fold's
        // flight is the widest window in the write path, and what it publishes at the end of it is
        // a side-manifest carrying `tombstones` and `deny` serialised from the live overlay. If
        // the WAL poisoned or the overlay diverged meanwhile, that overlay holds dispositions no
        // durable record backs, and writing them into a manifest would make them permanent on
        // every restore. `plan_fold` refuses for exactly this reason; every other manifest writer
        // re-asks at its own dispatch, and only this one has a window long enough for the answer
        // to change.
        //
        // The divergence half is already asked by `may_publish` above; this is the WAL's own.
        if self.wal.is_poisoned() {
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
        // ABA-safe because ids are never reused, so an artefact still listed is the same artefact
        // the fold consumed. [`Executor::fold_outstanding`] makes a merge or a coalesce publishing
        // under a fold unreachable; this stays as defence in depth, being one set comparison
        // against a manifest already in hand on a path that has just spent hours of IO.
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
        // The `MANIFEST.json` roster decides which incarnations are live, not the partition's view
        // map. The map is a cache of open row spaces; the roster read here is the same snapshot the
        // new manifest is written from, so what is carried and what is declared cannot disagree.
        let live_incarnations: FxHashMap<&str, tessera_types::view::ViewIncarnation> = live
            .bundle
            .manifest
            .views
            .iter()
            .map(|v| (v.id.as_str(), v.incarnation))
            .collect();
        let forward = carried_forward(plan, live_manifest, &live_incarnations, &consumed_segments);

        // Every carried-forward extent must begin at or above the fold's own base, per view.
        // `RowSpace::with_extent` refuses an extent below the base permutation's bound, and
        // `ExternalIdSidecar` gives the base locator absolute priority below its own length, so a
        // violation here is a prefix that either will not open or answers "this item has no
        // external id" for an item that has one. Both would be discovered only after `CURRENT` had
        // flipped, so they are checked before anything is written.
        for descriptor in &forward.segments {
            let Some(view) = plan.views.iter().find(|s| s.view == descriptor.view) else {
                // A view created and flushed since the plan was taken has a segment and no base
                // in it. That is transient and self-healing: the next fold plans over a bundle
                // that holds the view, and nothing is lost meanwhile but this fold's work. A
                // dropped view reaches this point too, but the carry-forward already omits a
                // dropped view's segments, so this arm is reached only by a view that arrived
                // during the flight.
                return Err("a carried-forward segment names a view created since the plan was taken, so \
                     the fold has no base for it; the next fold plans over a bundle that has it"
                    .to_string());
            };
            if descriptor.entity_lo < view.permutation_bound {
                return Err("a carried-forward segment begins below the fold's own base permutation".to_string());
            }
        }
        // Partition-wide here where the segment loop above is per-view: `ext-locator.u32` is one
        // array per partition, so there is no per-view bound to compare against. This cannot
        // falsely fire because entity ids are issued monotonically from one bundle-wide
        // high-water, so a locator extent published after the fold's snapshot begins above every
        // entity that had a row at it, in every view. `plan.entity_bound` is the maximum of the
        // per-view bounds and is therefore at or below that high-water, so no view's extent can
        // fall beneath it.
        if forward
            .locators
            .iter()
            .any(|extent| extent.entity_lo < plan.entity_bound)
        {
            return Err("a carried-forward locator extent begins below the fold's own base locator".to_string());
        }
        // The fold emits no run 0 for a deployment that held no external ids at its snapshot: no
        // runs, no locator. If one arrived during the flight, a carried-forward flush run would
        // become `external_id_runs[0]`, and the sidecar derives the base locator's path from that
        // entry's directory, so it would take a flush's entity-range extent for the full-length
        // base locator. Discarded rather than published; the next fold's snapshot holds the run
        // and emits a proper base for it.
        if completed.external_id_run.is_none() && !forward.runs.is_empty() {
            return Err("the deployment gained its first external-id run during the fold's flight".to_string());
        }

        // ---- retirement, evaluated here and nowhere earlier ---------------------------------------
        let mut carried = crate::compact::CarriedForward::new();
        for descriptor in &forward.segments {
            carried.add_segment(descriptor);
        }
        for extent in &forward.locators {
            carried.add_locator_extent(extent);
        }
        let executed = crate::compact::executed(&plan.tombstones, &carried);
        // ---- the merge-size relation, against the fold's own output -------------------------------
        //
        // `max_merged_segment_bytes` must stay strictly below the base segment's bytes or the next
        // startup refuses the configuration. A fold normally satisfies it more comfortably, since
        // it folds every extent into the base, and the case to catch is the small corpus where it
        // does not. Refused here, loudly, rather than at the restart that discovers it.
        if let Some(configured) = self.configured_merge_bytes {
            if completed.base_segment_bytes > 0 && configured >= completed.base_segment_bytes {
                return Err(format!(
                    "merge.max_merged_segment_bytes ({configured}) is not strictly below the \
                     folded base segment's {} bytes, so the next startup would refuse the \
                     deployment; lower it before the next fold",
                    completed.base_segment_bytes
                ));
            }
        }
        Ok(FoldApplies {
            carried_entities: carried.len(),
            forward,
            executed,
        })
    }

    /// Publish a fold: rebase or discard, assemble `SEGMENTS-<n>` from the live partition
    /// manifest, check the merge-size relation against the fold's own output, hard-link the
    /// carry-forwards, write `MANIFEST.json`, write `SEGMENTS-<n>.json`, flip `CURRENT`, open the
    /// new prefix in-process, swap onto it, rotate the WAL, and reclaim the old prefix.
    ///
    /// Everything about live state is decided here, not at the plan. The plan named files and
    /// cloned `D₀`. What the fold carries forward, post-snapshot segments, tiers, runs, locator
    /// extents, is whatever the live manifest still holds that the plan did not consume, read here
    /// hours later. `tombstones` is the one arithmetic that must be a set difference against live
    /// state: a delete accepted during the fold's flight names an entity whose row the fold did
    /// not drop, so publishing the executed set, or the plan's, would retire that deletion while
    /// its row survives in the rebuilt base.
    ///
    /// `CURRENT` is the commit point. A failure at any step up to the flip discards the fold: its
    /// files are orphans under a prefix nothing names, every consumed artefact still stands, and
    /// the next trigger re-plans. A failure after the flip is different and is alarmed as one: the
    /// bundle on disc is the new one and a restart opens it, while this process goes on serving the
    /// old geometry it still holds mapped.
    pub(super) fn publish_fold(&mut self, completed: crate::compact::CompletedFold) {
        let started = std::time::Instant::now();
        // The fold thread's staircase, continued here for the publication's phases so the gauges
        // on `/control/status` cover the whole fold (`compact::Staircase`). A discard below drops
        // it with the fold.
        let mut stairs =
            crate::compact::Staircase::resume(completed.cost.clone(), completed.finished);
        let live = self.generation.load_full();
        let plan = &completed.plan;
        // A node whose durable state disagrees with what it is serving publishes nothing (see
        // `may_publish`). The unit's files are orphans and its inputs still stand, the same
        // posture every other publication failure takes.
        if !self.may_publish() {
            return;
        }
        // Every discard below is the same posture: nothing happened, the files are orphans, the
        // next trigger re-plans. So it is one closure rather than the same shape repeated many
        // times. It owns what it reports so that it borrows nothing from `self` or from
        // `completed`, both of which the sequence below still needs.
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
        let live_manifest = &live.bundle.partitions[&plan.partition].manifest;
        let retired_count = executed.cardinality();
        // Allocated here rather than beside the manifest write, so the artifact pass below can
        // name its files after the publication that introduces them, in one sequence.
        let manifest_n = match self.allocate_manifest_n() {
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
        // A fold publishes a new prefix, and membership extent paths are prefix-relative. Carrying
        // the paths forward would name files the new prefix does not contain, and the bundle would
        // refuse at the next open; dropping them would lose every membership, with the artifacts
        // coming back registered, still addressable, and served as absent. Instead they are
        // written again, into the prefix being published, from the resident entity-space store.
        // Entity space is what makes that a rewrite rather than a translation: entity ids do not
        // move at a fold, only rows do, so the durable form needs no remapping, and the derived
        // row form is rebuilt from it afterwards.
        //
        // It is still not a copy. The fold retires entities, and a membership carried forward
        // unchanged would go on counting members that no longer exist, in the size the
        // proportional existence criterion divides by. `repack_all` drops exactly the executed
        // deletions and nothing else: a suppressed member keeps its bit (Rule S), and no generating
        // set is touched at all.
        let to_prefix_dir = self.bundle_root.join(&completed.prefix);
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

        // The containment partitions, in the same pass and against the prefix just written. They
        // are derived, so an empty list is a cost and not a fault; see
        // `write_containment_partitions`.
        //
        // The levels this fold is about to change and has not yet ([`PendingRetirement`]).
        // `repack_all` above wrote the post-retirement records into the prefix while the store
        // still holds the pre-retirement ones. A partition composed from the store would describe
        // a level the prefix does not contain: under `WithdrawContent` a content is dropped
        // whole, which shifts the ranks, and a partition read at the wrong rank is a containment
        // answer for another content's generating set. Those levels get no partition and
        // recompose on first use. Their row columns and tile indexes are written from the records
        // the retirement will leave, and at the version it will leave the level at.
        let pending = PendingRetirement {
            levels: self
                .live
                .with_artifacts(|store| store.levels_moved_by(&executed)),
            retired: executed.clone(),
        };
        // One counter for the whole publication. Every derived file the executor writes below is
        // named from it, so no two of these calls can name the same file; see
        // `tessera_store::derived::DerivedIndex`. The term images are named from a second counter,
        // on the fold thread; the two counters cover disjoint kinds, and
        // `compact::TERM_IMAGE_MANIFEST_N` carries why that pass cannot use this one.
        let derived = self.write_fold_derived(
            &to_prefix_dir,
            &completed,
            manifest_n,
            &live.bundle.manifest.data_plugin_hash,
            &pending,
        );
        stairs.record("8 derived");

        // ---- the report, before anything retires ---------------------------------------------------
        //
        // A deletion is not retired before the caller has been told what it degraded. This fold is
        // about to retire the overlay entries for `executed`, so the report of what those
        // deletions took away from the artifacts that held them is written first, and a report
        // that cannot be written discards the fold. Nothing is lost by discarding: the deletions
        // are already in force, and the next fold reports them.
        //
        // The sweep is one `and_cardinality` per artifact against the set the fold already holds.
        // It runs here, where the store still holds what this fold is about to retire; the write
        // is deferred to the last reversible step before the flip, so an abandoned fold leaves no
        // notice claiming an obligation it did not discharge.
        let degraded = self
            .live
            .with_artifacts(|store| store.degradations(&executed));
        stairs.record("9 report");

        // ---- assemble `SEGMENTS-<n>` from the live partition manifest ------------------------------
        //
        // Each view's fold base first and its carried extents after it, because the reader takes
        // the first segment listed for a view as the one `permutation.bin` addresses and every
        // later one as an extent above it.
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
        let mut external_id_runs = Vec::with_capacity(1 + forward.runs.len());
        external_id_runs.extend(completed.external_id_run.clone());
        external_id_runs.extend(forward.runs.iter().cloned());

        // The executed entries leave `tombstones` here as well as the overlay, and the two must be
        // one decision: the manifest is the overlay's other durable home, so a manifest carrying
        // what the swap is about to retire would re-seed it at the next restart.
        let mut published_overlay = (*live.overlay).clone();
        published_overlay.retire(&executed);

        // The registry comes from the registry, not from the manifest beside it. A layer
        // registration and an artifact publication write a side manifest and do not swap the
        // generation, so the manifest this fold is holding can be several publications behind, and
        // a fold that copied its (empty) layer list would publish a prefix whose membership
        // extents name layers it does not declare. Every one of them is then skipped at open as a
        // dropped layer's leftovers, and every artifact comes back absent with no error. The
        // online publication path takes the same posture for the same reason.
        let (registered_layers, registered_tombstones, registry_low_water) =
            self.live.registry_for_publication();
        // The roster, from the live roster rather than from the fold's own inputs, on the same
        // argument as above: the manifest a fold planned against may be several publications
        // behind, and a view created since must not be dropped by the publication that lands.
        let (created_views, dead_view_incarnations) = self.live.roster_for_publication();
        let (runtime_attributes, runtime_scoped_attributes) =
            self.live.attributes_for_publication();
        // The vocabularies this fold is about to write into `MANIFEST.json`, named here so the
        // live list can be emptied of exactly them once the publication has landed.
        let folded_vocabularies = self
            .live
            .with_vocabularies(|vocabularies| vocabularies.names());
        // The view groups and plain views likewise.
        let (folded_groups, folded_plain_views) = self
            .live
            .with_view_declarations(|declarations| declarations.names());

        // The fold has written the scoped columns, vocabularies, view declarations, bindings and
        // file digests into `MANIFEST.json`, so the side-manifest starts without them.
        let mut segments_manifest = SegmentsManifest {
            // The flight's text extents; the pass merged every other one into the new base index.
            // A flush publishing during the fold indexed entities the new base does not hold, and
            // dropping its entry would answer every `match` over that batch's prose with an empty
            // result, since the words are not in the base the fold wrote.
            text_extents: forward.texts.clone(),
            // Live, and untouched. Deriving either from the fold's inputs would move the watermark
            // backwards past every post-snapshot entity, and composition treats an entity at or
            // above the watermark as buffered rather than rowed, so the gap would go invisible.
            // `check_publishable` refuses a regression; this is what keeps it from having to.
            watermark: live_manifest.watermark,
            entity_id_high_water: live_manifest.entity_id_high_water,
            // Live, for a sharper reason than the watermark's: the fold rewrites the point region
            // and touches the row-less one not at all, since a layer has no rows to renumber.
            // Deriving this from the fold's inputs would raise the mark back towards the ceiling
            // and hand the next registration ids a live layer already holds. The registry has to
            // travel with it too: rotation reclaims the WAL records the mark is otherwise
            // recovered from, so a fold that published an empty list would lose every gate at the
            // next restart while the layers themselves kept being referenced.
            entity_id_low_water: live_manifest.entity_id_low_water.min(registry_low_water),
            layers: registered_layers,
            layer_tombstones: registered_tombstones,
            views: created_views,
            // The declarations made since the fold planned, and only those. The fold's
            // `MANIFEST.json` is the served schema as it stood at the plan, runtime columns
            // included, with a base written for each (`compact::FoldContext::runtime_attributes`).
            // Restating those here would duplicate a fact the prefix's own manifest now states. A
            // declaration made while the fold ran is in neither and must survive the publication
            // that lands, so the live list is taken and the folded names removed from it.
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
            dead_view_incarnations,
            // The pass's own output, not the live list. The paths are prefix-relative and the fold
            // publishes a new prefix, so what the artifact pass wrote is the only list that names
            // files this prefix contains. The content extents beside it are carried by link, their
            // bytes being the same inodes under a second name.
            membership_extents: repacked.clone(),
            // The term-image pass's own output, not the live list, for the same reason. The images
            // cover the new base alone, which is what a projection of the base is: a flush landing
            // during the flight is carried forward as an extent, and an extent's rows are walked.
            term_image_extents: completed
                .term_images
                .iter()
                .map(|images| images.extent.clone())
                .collect(),
            artifact_record_extents: self.artifact_record_extents.clone(),
            segments,
            deltas: forward.tiers.clone(),
            // Verbatim, and the live list rather than the plan's: a flush that promoted during the
            // fold's flight appended an extent whose ordinals the live dictionary already holds,
            // and dropping it would shift every ordinal after it.
            dict_extents: live_manifest.dict_extents.clone(),
            // Publishing an attribute artefact is two obligations: the files and this list. The
            // fold's own base columns are named by convention and digested in `MANIFEST.json`; a
            // flight extent is reachable only through this entry, so linking its bytes while
            // leaving the list empty would produce a bundle that opens cleanly and answers filters
            // without every post-snapshot entity's value, with no symptom. The two halves are
            // written here, in one manifest write.
            attr_extents: forward.attrs.clone(),
            record_extents: forward.records.clone(),
            entity_terms_extents: forward.entity_terms.clone(),
            external_id_runs,
            locator_extents: forward.locators.clone(),
            ..SegmentsManifest::empty()
        };
        write_deny_state(&mut segments_manifest, &published_overlay);

        // ---- the new `MANIFEST.json` ----------------------------------------------------------
        let mut bundle_manifest = self.fold_bundle_manifest(&live, &completed, plan);

        let carried_rels = carried_files(&plan.partition, live_manifest, &forward);
        for rel in &carried_rels {
            // A hard link changes nothing about a file's content, so the digest it earned under
            // the old prefix's path is still correct under the new one; nothing is re-hashed. A
            // file the live manifests name but do not digest is a bundle this fold must not
            // propagate.
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
        // The artifacts' content extents are linked and not digested, which is the one place this
        // set is not uniform: they carry no entry in either manifest's `files`, since nothing
        // digests them at their own publication, so putting them through the loop above would
        // discard every fold on a node that has ever published supplied content. They are linked
        // here instead. An artifact whose content the new prefix does not carry is not served
        // without its description: it is withheld, so losing these is losing the artifacts.
        //
        // From the held list, not from the manifest beside it, the same stale-generation trap the
        // registry above falls into: a side-manifest write does not swap the generation, so the
        // manifest this fold holds names only the content extents that existed at the last one.
        for extent in &self.artifact_record_extents {
            carried_rels.push(extent.blocks.clone());
            carried_rels.push(extent.hasrow.clone());
            carried_rels.push(extent.directory.clone());
        }
        if let Err(e) =
            tessera_store::hard_link_forward(&from_prefix_dir, &to_prefix_dir, &carried_rels)
        {
            discard(&format!("its carry-forwards would not link ({e})"));
            return;
        }
        // Both the bytes and the names need syncing, and the bytes are the half that is not
        // obvious: a hard link copies no bytes, so the directory entry is plainly new, but that
        // does not make the bytes durable. No producer fsyncs a data file: neither
        // `write_single_batch` nor the morton, tier or run writers sync, and
        // `write_segments_manifest` syncs the manifest and its directory and nothing the manifest
        // names. Elsewhere in the write path that is safe, because a torn file is detectable
        // through its digest and its rows are still in the WAL, so a flush can re-run. The fold is
        // the one operation that removes every part of that recovery path: it flips `CURRENT` onto
        // these links, deletes the prefix holding the only other names for the same inodes, and
        // rotates the WAL out from under the records. So a data file the kernel had not written
        // back becomes unrecoverable, not merely undetected, for whatever it had not flushed in
        // the last publications before the flip, which is exactly the window a fold's
        // carry-forward set is drawn from.
        //
        // Cheap, because the set is only what published during the flight: `plan_fold` consumes
        // everything the manifest named at its snapshot, so nothing older than the fold is here.
        // The fold's own passes synced on its own thread.
        let carried_paths: Vec<PathBuf> = carried_rels
            .iter()
            .map(|rel| to_prefix_dir.join(rel))
            .collect();
        if let Err(e) = tessera_store::fsync_written(&carried_paths) {
            discard(&format!("its carry-forwards would not sync ({e})"));
            return;
        }
        let manifest_digest =
            match tessera_store::write_manifest_json(&to_prefix_dir, &bundle_manifest) {
                Ok(digest) => digest,
                Err(e) => {
                    discard(&format!("its MANIFEST.json would not commit ({e})"));
                    return;
                }
            };
        if let Err(e) = self.commit_side_manifest(
            live_manifest,
            &to_prefix_dir,
            &plan.partition,
            manifest_n,
            &mut segments_manifest,
            Some(FoldDerived {
                written: &derived,
                pending_retirement: &pending.levels,
            }),
        ) {
            discard(&format!(
                "its SEGMENTS-{manifest_n}.json would not commit ({e})"
            ));
            return;
        }
        // The commit point. Everything above is reversible; nothing below is.
        //
        // A kill parked anywhere before this line leaves a complete, synced `v#####` tree
        // `CURRENT` never named, and the startup sweep reclaims it whole, with no per-file
        // bookkeeping needed. The fold's manifest writes above carry no pause site of their own;
        // their crash story is this one's.
        //
        // The report is the last reversible step, which is what gives it evidential value: it
        // says an obligation was discharged, so a notice left behind by a fold that was then
        // abandoned at its link, its manifest or its flip would be a false positive, with an
        // operator polling `reports/` reading that a deletion was reported when it is still owed
        // one. Everything that can still discard is above this line.
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
            tessera_store::write_current(&self.bundle_root, &completed.prefix, &manifest_digest)
        {
            discard(&format!("CURRENT would not flip ({e})"));
            return;
        }
        stairs.record("11 flip");

        // The resident store retires here, before the new generation is installed, not after the
        // warm below. The generation this fold is about to publish carries an overlay with the
        // executed deletions already retired, so between installing it and retiring the store
        // there would be a window in which a deleted artifact's own entity reads not denied while
        // its record is still there, and would be served with its content to whoever asked.
        //
        // Ahead of the swap is safe in the other direction: the generation still being served
        // carries the deletion in its own overlay, so an artifact this removes was already absent
        // for every request reaching it.
        let mut moved = self.live.retire_artifacts(&pending.retired);
        self.live.mark_memberships_published();
        // And the growths, which only a whole rewrite reaches. `rewrite_membership_extents` wrote
        // every level entire, from the resident store, so a membership that grew since the last
        // fold is in the prefix `CURRENT` now names, the one publication that carries a record
        // sitting below its level's high-water. Until this point the log was holding those records
        // as the only copy.
        self.live.mark_growth_packed();
        // The resident memberships move onto the extents this fold wrote; see
        // `LiveState::rehouse_memberships`. Until they do, every membership seeded at open still
        // reads through the previous prefix's packs, and reclamation would unlink files whose
        // blocks the mapping goes on holding.
        let (rehoused, kept) = self.live.rehouse_memberships(&to_prefix_dir, &repacked);
        if kept > 0 {
            // Unreachable, on the seed's rule: the extents were written from this store a moment
            // ago and a hole is an empty blob the rehousing skips, so a record that does not take
            // is one the store no longer holds at that ordinal, or one whose cardinality disagrees
            // with the bytes this fold wrote for it. The first is a level the prefix and the store
            // describe differently; the second is an artifact served from the bitmap it already
            // holds, against an extent a restart will read instead.
            tracing::error!(
                rehoused,
                kept,
                "ALARM: artifact memberships this fold wrote back do not match the records they \
                 were written from; the level the prefix carries and the level being served \
                 disagree for those ordinals"
            );
        }
        self.membership_extents = repacked;
        // The retirement moved the levels the artifact pass said it would, checked rather than
        // assumed. A pending level's structures were stamped with the version the level would
        // have after this retirement ([`PendingRetirement`]). `levels_moved_by` and `retire` read
        // one predicate, so any other outcome is unreachable today; the check guards against a
        // structure being carried into a later manifest at a version it does not describe if that
        // predicate ever diverges. It protects the live lists and the manifests later flushes
        // write from them; the fold's own manifest is already durable with the version and the
        // structures it states.
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
        // The structures this fold wrote replace whatever the previous prefix held: their paths
        // are prefix-relative and the fold publishes a new prefix, so the old entries name files
        // this prefix does not contain. Held only at the version the store now carries.
        self.derived_extents = self.live.with_artifacts(|store| {
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
        stairs.record("13 open");

        self.live.with_artifacts(|store| {
            self.artifact_projections.adopt_derived(
                &to_prefix_dir,
                &completed.prefix,
                &self.derived_extents,
                store,
            )
        });
        stairs.record("14 adopt");

        // The row forms, rebuilt here rather than by whoever arrives first. Row space renumbers
        // globally at a fold, so every projection built over the old one is invalid at the flip,
        // and a level is a deployment-wide artefact rather than a per-session value, so a
        // first-toucher rebuild would be a stall on whichever request arrived next. It is the same
        // construction the request path runs, on the permutation this fold has just written.
        //
        // Run after the swap and after the retire above, and both orderings matter: built before
        // the generation is live, every projection would be keyed to one no reader can ask for;
        // built before the store retires, every projection would be keyed to a store version the
        // retire is about to bump, discarding the whole warm on the first request.
        self.warm_artifact_caches();
        // The pieces the artifact pass staged were taken by the builds above; what is left is
        // staged for a level nothing built a form for, and would otherwise be held indefinitely.
        self.shapes.clear_staged();
        stairs.record("15 warm");

        // ---- rotate the WAL ---------------------------------------------------------------------
        //
        // Immediately, because this is the whole mitigation for retirement's durability window:
        // the manifest seed no longer carries the executed entries, but the WAL still holds the
        // original `ChangeByEntity{Delete}` records, so until a rotation whose head snapshot
        // postdates this fold has reclaimed them, a restart resurrects them harmlessly (they name
        // entities with no row and no postings) and permanently, since a rotation snapshot applies
        // entries and never assigns.
        self.rotate_wal();
        stairs.record("16 wal");
        // The columns this fold wrote into `MANIFEST.json` leave the runtime list: from here they
        // are the build's, with a base every reader opens.
        self.live.with_attributes(|attributes| {
            attributes.retire_folded(
                &completed.runtime_attributes,
                &completed.runtime_scoped_attributes,
            )
        });
        // The vocabularies beside them. Every one the list held when the manifest was assembled is
        // in the `MANIFEST.json` this fold wrote, that manifest being the live one, so the list
        // empties by name rather than by a since-plan subtraction; a declaration made after the
        // assembly is not among them and stays.
        self.live
            .with_vocabularies(|vocabularies| vocabularies.retire_folded(&folded_vocabularies));
        // The view groups and plain views beside them, on the same rule.
        self.live.with_view_declarations(|declarations| {
            declarations.retire_folded(&folded_groups, &folded_plain_views)
        });

        // ---- reclaim the superseded prefix -----------------------------------------------------
        //
        // Owned first: the rest of the carry-forward set borrows the generation being moved here,
        // and the log below still has to say what was left behind.
        let CarriedExtents {
            omitted_views,
            omitted_segments,
            ..
        } = forward;
        self.pending_reclaim.push(PendingReclaim {
            generation: live,
            prefix_dir: from_prefix_dir,
            // Taken, not cloned: these belong to the prefix being superseded, and the prefix this
            // fold just published starts with none.
            superseded_sidecars: std::mem::take(&mut self.superseded_sidecars),
        });
        self.reclaim_superseded_prefixes();
        stairs.record("17 reclaim");
        let cost = stairs.into_cost();

        self.health.folds.fetch_add(1, Ordering::Relaxed);
        let (passes, fold_secs, staircase_rss) = self.health.record_fold_cost(
            cost,
            completed.attr_bytes_read,
            completed.attr_bytes_written,
        );
        // One line per view, beside the summary rather than inside it: a group's keys are
        // separate views over one dictionary, each paying its own table and its own payload, and a
        // total says nothing about which of them is expensive. The build reports the same four
        // figures per view, so the two routes' reports read alike.
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
            // What the fold reclaimed by leaving it behind. A dropped view's row space goes with
            // the superseded prefix and nothing else records that it did, so an operator who
            // cannot see it here cannot see it at all. Zero on every fold of a bundle nothing was
            // dropped from, which is nearly all of them.
            dropped_views = %if omitted_views.is_empty() {
                "none".to_string()
            } else {
                omitted_views.join(",")
            },
            dropped_view_segments = omitted_segments,
            "a compaction fold published: the bundle is one base segment per partition-view, one \
             base postings tier, one external-id run and one locator, plus whatever landed during \
             its flight"
        );
    }

    /// The `MANIFEST.json` a fold's new prefix carries: the live one, with the schema wound back to
    /// what it was at the plan, every live vocabulary binding folded in, and the fold's own files.
    ///
    /// `entity_id_high_water` here is the snapshot's entity space, not the live one, and the two
    /// fields of that name mean different things. `SEGMENTS-<n>.json`'s seeds the entity id
    /// allocator and is the live value; this one is what
    /// `ExternalIdSidecar::deferred_from_manifest` takes as the base locator's declared length,
    /// sized to the snapshot so post-snapshot locator extents stay reachable past it. A live value
    /// here would make the base locator claim every post-snapshot entity and answer "this item has
    /// no external id" for items that have one. Writing the lower value is safe for the allocator
    /// only because `Engine::open` seeds its floor from the max of this and the side-manifest's,
    /// which carries the live one.
    ///
    /// The schema is as it stood at the plan. A column declared while the fold ran has no base in
    /// the new prefix, so it stays off this manifest and on the side manifest's runtime list, from
    /// which the reopen appends it again at the same tail position.
    ///
    /// The vocabulary bindings fold in verbatim. Keys and codes are byte-identical, from the live
    /// minters and from the served side-manifests' extensions alike, and the merge is a union, so
    /// neither path can drop one. Re-deriving, re-sorting or re-numbering here would recolour the
    /// whole corpus with no error and no digest mismatch, because `columns.arrow` stores the code
    /// and nothing else records what it meant.
    pub(super) fn fold_bundle_manifest(
        &self,
        live: &Generation,
        completed: &crate::compact::CompletedFold,
        plan: &crate::compact::FoldPlan,
    ) -> tessera_store::manifest::Manifest {
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
        bundle_manifest.entity_id_high_water = plan.entity_bound;
        bundle_manifest.files = completed.files.clone();
        let carried_bindings: Vec<_> = live
            .bundle
            .partitions
            .values()
            .flat_map(|partition| partition.manifest.vocabulary_extensions.iter().cloned())
            .collect();
        tessera_store::vocabulary::fold_extensions_into(
            &mut bundle_manifest.vocabularies,
            &carried_bindings,
        );
        bundle_manifest
    }

    /// Latch [`ExecutorHealth::prefix_diverged`]: `CURRENT` names a prefix this process could not
    /// swap onto, so it must stop writing durable state.
    ///
    /// Every publication after this point would land in a tree no restart reads. The live
    /// generation still names the superseded prefix, and every manifest path derives its
    /// directory from that generation (correctly, on the success path), so a flush would write its
    /// segment and side-manifest under the old prefix, ack the rows, and then rotate the WAL and
    /// reclaim their records: acked ingest, lost at the next restart, with nothing logged at the
    /// loss. A crash cannot produce this, because a crashed process stops writing; only a live one
    /// that flipped `CURRENT` and carried on can.
    ///
    /// The superseded prefix is deliberately not reclaimed on this path: it is what this process
    /// is still serving from, and `reclaim_prefix` would refuse it anyway now that `CURRENT` names
    /// the other one.
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

    /// Opens the prefix `CURRENT` now names and swaps onto it. The tier list is taken from the
    /// committed manifest, which is what a restart would open; the readers are the live ones,
    /// whose files the carry-forward gave a second name, so they outlive the old prefix.
    fn swap_onto_folded_prefix(
        &mut self,
        prefix: &str,
        live: &Arc<Generation>,
        live_tiers: &[String],
        folded_tiers: &[String],
        retired: croaring::Bitmap,
    ) -> Result<(), String> {
        let (bundle, rotation) =
            crate::session::open_rotation(&self.bundle_root, prefix, &live.fragments, retired)
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

    /// Delete every superseded prefix nothing is reading any more: the reclamation event that is
    /// the fold's reason for existing, since nothing else in the system can delete a file.
    ///
    /// The `Arc` is the wait, not a hope. Every file the new prefix still needs has a directory
    /// entry there, so this unlinks directory entries and never live data, but the external-id
    /// sidecar opens its runs lazily, so a request still holding the superseded generation could
    /// be about to open a path under that tree. The generation pointer has already moved, so no
    /// new holder can appear; holding the `Arc` here and reclaiming only at a count of one turns
    /// "wait for the readers" into a condition rather than a delay.
    ///
    /// Three counts, because a strong count of the generation and its sidecar together answer for
    /// every generation a flush produced over the prefix, but not for one holding a sidecar a
    /// coalesce replaced. `superseded_sidecars` covers that case with weak references, so the
    /// three counts between them name every sidecar that was ever live over the prefix and
    /// therefore every generation that could still resolve a path inside it.
    ///
    /// A failure alarms once and drops the entry: `remove_dir_all` failing is a permissions or
    /// device fault rather than a transient one, and retrying it every tick would flood the log
    /// around a condition an operator has to act on. The tree then stands as an orphan, the same
    /// residual a process exiting mid-wait leaves.
    pub(super) fn reclaim_superseded_prefixes(&mut self) {
        if self.pending_reclaim.is_empty() {
            return;
        }
        let mut still_read = Vec::new();
        for pending in std::mem::take(&mut self.pending_reclaim) {
            // Three counts, because they reach three different sets; see `pending_reclaim` and
            // `superseded_sidecars`. The generation's own count answers for itself; its sidecar's
            // answers for every other generation a flush produced over the same prefix, because a
            // flush publishes by cloning the live sidecar `Arc` rather than building one; and the
            // weak list answers for the generations that hold a sidecar a coalesce replaced, the
            // one publication that builds a new sidecar over an unchanged prefix and so the one
            // case the second count cannot see.
            //
            // Read off the held generation, so at rest the two strong counts are 1 (this entry is
            // the only holder of the generation, and the generation is the only holder of the
            // sidecar) and every weak count is 0. The post-fold generation has a sidecar of its
            // own and appears in none of the three.
            if Arc::strong_count(&pending.generation) > 1
                || Arc::strong_count(&pending.generation.external_index) > 1
                || pending
                    .superseded_sidecars
                    .iter()
                    .any(|held| held.strong_count() > 0)
            {
                still_read.push(pending);
                continue;
            }
            let PendingReclaim {
                generation,
                prefix_dir,
                superseded_sidecars,
            } = pending;
            drop(superseded_sidecars);
            drop(generation);
            match tessera_store::reclaim_prefix(&prefix_dir) {
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
    /// Written outside the prefix. A fold reclaims the prefix it superseded, so a report written
    /// into the new prefix would be deleted by the next fold before an operator read it.
    /// `reports/` sits in the bundle root beside the prefixes, which nothing reclaims, and the
    /// startup sweep only knows about `v#####` directories.
    ///
    /// A report with nothing in it is still written: an operator polling the directory must be
    /// able to tell "this fold degraded nothing" from "this fold never reported", and an absent
    /// file says the second.
    pub(super) fn write_fold_report(
        &self,
        prefix: &str,
        degraded: &[tessera_lifecycle::membership::Degradation],
    ) -> std::io::Result<()> {
        let dir = self.bundle_root.join("reports");
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
        tessera_store::write_and_fsync(&path, &bytes)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        tessera_store::fsync_dir(&dir).map_err(|e| std::io::Error::other(e.to_string()))?;
        // The in-memory copy is set by the caller, after the flip, and not here. This file is
        // durable evidence and the accessor is a convenience; publishing the convenience while the
        // fold can still be discarded would let an operator read a discharge that did not happen.
        Ok(())
    }

    /// The fold's artifact pass: write every level whole into the prefix being published, with the
    /// fold's executed deletions dropped from each membership.
    ///
    /// The returned list replaces the manifest's, rather than extending it: one extent per level,
    /// covering `[0, len)`, so the accumulated extents of every earlier publication collapse into
    /// one file each and the prefix names nothing it does not contain.
    ///
    /// Holes are written, not packed around, which is the asymmetry with the append-only path:
    /// that one skips a level whose unpublished range has a gap, because a gap there means a
    /// publication landed out of order. Here a gap is expected, left behind when this fold retires
    /// an artifact (Rule F), and closing it would hand every later artifact in the level the
    /// identity of its neighbour, since an ordinal is the identity a caller's `tessera_id`
    /// resolves to.
    pub(super) fn rewrite_membership_extents(
        &self,
        prefix_dir: &std::path::Path,
        partition: &str,
        n: u64,
        retired: &croaring::Bitmap,
    ) -> tessera_store::Result<Vec<tessera_store::manifest::MembershipExtent>> {
        let ready = self.live.with_artifacts(|store| store.repack_all(retired));
        pack_membership_extents(prefix_dir, partition, n, ready)
    }

    /// Compose and write this prefix's containment partitions, one file per `(layer, level)`.
    ///
    /// Against the prefix being published, not the one being left: a fold rewrites the term index,
    /// so a partition composed from the old postings would name a table the new prefix's entities
    /// are not in. The new file is on disk by the time this runs, so the reader is opened over the
    /// prefix this is writing into.
    ///
    /// Run inside the artifact pass, before the registry snapshot the manifest is written from:
    /// the coordinate each entry carries is the level version at the moment it was composed, and
    /// the version list beside it comes from the same borrow, so the two cannot disagree about a
    /// publication landing between them.
    ///
    /// Every failure is an empty list, not a discarded fold: a partition is derived, so the level
    /// recomposes it on first use.
    fn write_containment_partitions(
        &self,
        pass: &mut DerivedPass<'_>,
        data_plugin_hash: &str,
    ) -> Vec<tessera_store::manifest::DerivedExtent> {
        let prefix_dir = pass.prefix_dir;
        let partition = pass.partition;
        let n = pass.n;
        let pending = pass.pending;
        // The gate: under any plugin but the builtin the partition is not sound at all, so nothing
        // is composed and nothing is written (`crate::containment`).
        if !crate::containment::signature_shaped(data_plugin_hash) {
            return Vec::new();
        }
        let postings_path = prefix_dir
            .join("partitions")
            .join(partition)
            .join("terms")
            .join("postings.arrow");
        let postings = match tessera_authz::PostingsReader::open(&postings_path, true) {
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

        // Composed under one borrow with the versions they are composed at, and written outside it:
        // composing is the dear part and needs the store, writing a file does not.
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

        // Filed by the shared writer, the same one `tessera build`'s artifact pass calls: one
        // naming rule, one durability sequence, one manifest-entry shape.
        tessera_store::derived::file_derived(
            prefix_dir,
            partition,
            n,
            &mut pass.index,
            composed
                .into_iter()
                .map(
                    |(layer, level, level_version, bytes)| tessera_store::derived::Filed {
                        view: None,
                        incarnation: None,
                        layer,
                        level,
                        level_version,
                        form: tessera_store::manifest::DerivedForm::Containment,
                        bytes: tessera_store::derived::FiledBytes::InHand(bytes),
                    },
                )
                .collect(),
        )
    }

    /// Project and write this prefix's tile-index extent columns, one file per
    /// `(view, layer, level)`.
    ///
    /// Against the prefix being published, and against its base permutation. A fold renumbers row
    /// space wholesale, so an extent projected through the old one would name other people's
    /// documents. The new `permutation.bin` for each view is already on disk by this point, so the
    /// row space is opened over what this fold is publishing: base only, with no extents, which is
    /// exactly what the row form holds ([`tessera_store::RowSpace::project_base`]) and therefore
    /// what its extents are computed over. A flush landing during the flight appends rows above the
    /// base and moves none of these.
    ///
    /// Run inside the artifact pass, before the registry snapshot, and omitting the levels a
    /// retirement is about to move, for the same reason as
    /// [`Executor::write_containment_partitions`]: the coordinate an entry carries and the version
    /// list beside it come from the same borrow, and a level whose records the prefix already
    /// holds in post-retirement form is not the level the store would project.
    ///
    /// This is a second pass of the same projection the row form is built from: an extent is
    /// `minimum` and `maximum` over the same `project_base`. It is paid here so that
    /// `warm_artifact_caches` below claims the column instead of deriving one, and so a restart
    /// maps it rather than deriving it. The same function is used rather than a cheaper min/max
    /// walk deliberately, so the column written here and the column derived from the row form are
    /// equal by construction; `tests/artifact_tile_index.rs` asserts them byte for byte.
    ///
    /// It adds no residency, though: [`crate::tile_index::TileIndex::project`] holds one membership
    /// at a time, so this pass is a few bytes an artifact where the row form it is deriving the
    /// same extents from would be gigabytes, and the fold runs before the flip, with the outgoing
    /// generation's forms still resident.
    ///
    /// Every failure is an empty list, not a discarded fold: the index is derived.
    ///
    /// The base row space of every view this fold just wrote, opened once for the whole artifact
    /// pass.
    ///
    /// Against the prefix being published, and base only, with no extents, which is exactly what a
    /// row form holds ([`tessera_store::RowSpace::project_base`]) and therefore what every
    /// structure derived from one is computed over. A flush landing during the flight appends rows
    /// above the base and moves none of these.
    ///
    /// A view whose permutation will not load is simply absent: its structures are derived on
    /// first use, which is what every request did before the fold wrote anything.
    pub(super) fn fold_row_spaces(
        &self,
        prefix_dir: &std::path::Path,
        partition: &str,
        views: &[(String, u32)],
    ) -> Vec<(String, tessera_store::RowSpace)> {
        let mut spaces: Vec<(String, tessera_store::RowSpace)> = Vec::new();
        for (view, row_count) in views {
            let partition_dir = prefix_dir.join("partitions").join(partition);
            let path = tessera_store::view_path(&partition_dir, view).join("permutation.bin");
            match tessera_store::Permutation::load(&path) {
                Ok(permutation) => spaces.push((
                    view.clone(),
                    tessera_store::RowSpace::new(std::sync::Arc::new(permutation), *row_count),
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
    /// retirement does not move, and an enumerated level it does ([`PendingRetirement`]). A spatial
    /// level the retirement moves is omitted: its rows are resolved from shapes, and the shape of
    /// an artifact the retirement removes is still held.
    pub(super) fn composes_row_structures(
        &self,
        pending: &PendingRetirement,
        layer: &str,
        level: u32,
    ) -> bool {
        !pending.is_pending(layer, level)
            || self.live.registered_layer(layer).is_some_and(|registered| {
                registered.declaration.membership
                    == tessera_types::layer::MembershipSource::Enumerated
            })
    }

    /// The fold's layout re-evaluation, taken inside the artifact pass and before a derived byte
    /// is written.
    ///
    /// The observations it reads are post-retirement: `repack_all` above has already written the
    /// surviving memberships into the prefix, and this reads the store, so a level the fold
    /// retired most of is observed as the level it is about to become.
    ///
    /// The record is per `(layer, level)` and the observation is per view, a mismatch the levels
    /// themselves create: a layer drawn in two views has two row spaces and so two localities. The
    /// shape is taken in the first view the fold wrote, deterministically, because the record has
    /// one slot and a level is one level however many views draw it. Where two views disagree
    /// sharply the pick follows the first, and the other view's column is written in that layout,
    /// which is correct and may be slower than that view would have chosen on its own.
    ///
    /// A pin is read, never re-derived, and reaches here as `declaration.layout`; see
    /// [`crate::layout::choose`].
    ///
    /// This adds a whole extra projection of one view to the fold's artifact pass:
    /// `RowSpace::project_base` per record, the same call the row form and the tile index each
    /// already make.
    pub(super) fn choose_layouts(
        &self,
        spaces: &[(String, tessera_store::RowSpace)],
        pending: &PendingRetirement,
        fold_segments: &[(String, tessera_store::read::SegmentData)],
    ) -> Vec<(String, u32, tessera_types::layer::ServingLayout)> {
        let Some((first_view, space)) = spaces.first() else {
            return Vec::new();
        };
        let levels: Vec<(String, u32)> = self.live.with_artifacts(|store| {
            levels_of(store)
                .filter(|(layer, level)| self.composes_row_structures(pending, layer, *level))
                .collect()
        });
        let mut out = Vec::with_capacity(levels.len());
        // Level counts gathered per layer for the roll-up below, the per-level lines being one
        // level's own shape and the sum being what a response pays.
        let mut per_layer: std::collections::BTreeMap<String, Vec<(u32, u64)>> =
            std::collections::BTreeMap::new();
        for (layer, level) in levels {
            let Some(registered) = self.live.registered_layer(&layer) else {
                continue;
            };
            // One membership at a time, never the level's row form. The observation is the same
            // `project_base` per artifact either way; what differs is what is held while it runs,
            // and a row form held here would sit beside the outgoing generation's own forms,
            // because the fold runs before the flip.
            //
            // A spatial level is observed over its resolved rows: every segment the fold wrote is
            // resolved against the level's shapes here, inside the artifact pass and before
            // anything is written, and the pieces are staged under the new segment ids for the
            // derived files this pass writes and for the row forms the flip's warm builds. That is
            // the fold's re-resolution of everything, because the fold renumbered every row.
            let spatial = spatial_membership(&registered.declaration);
            let shape = self.live.with_artifacts(|store| {
                if spatial {
                    let mut observed = None;
                    for (view, segment) in fold_segments {
                        let held = self.shapes.level(
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
                            observed = Some(tessera_store::derived::observe_shape(
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
                    observed.unwrap_or_else(tessera_store::derived::LevelShape::empty)
                } else {
                    // A borrowing artifact is observed over the membership it is served on, which
                    // `members_of` reads from its target where the record declares none. The tile
                    // index and the column this fold then files for such a level are true when
                    // written and are not read afterwards: the borrowed set moves with the
                    // target's level version and the file is keyed on this level's, so the engine
                    // serves a borrowing level artifact-major and derives its index from the form
                    // it resolved (`crate::artifacts::ArtifactRows::inherit`).
                    tessera_store::derived::observe_shape(space.base_rows(), &|visit| {
                        for (ordinal, record) in pending.records(store, &layer, level) {
                            visit(ordinal, &space.project_base(store.members_of(record)));
                        }
                    })
                }
            });
            let chosen = crate::layout::choose(&registered.declaration, shape);
            tracing::info!(
                layer = %layer,
                level,
                artifacts = shape.artifacts,
                // The trigger for the layout choice, reported alongside it rather than only used
                // internally.
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
            out.push((layer, level, chosen));
        }
        // What a whole-layer response costs, reported and never refused. The per-level lines above
        // each carry their own count; this is the sum, and the sum predicts response volume,
        // because a response carries one row per served artifact and the levels a request does not
        // exclude are all of them.
        //
        // Reported rather than bounded: a large response is slow, not wrong. It discloses nothing
        // the mask did not already allow and a rerun costs nothing, so it is the operator's call
        // and not the service's. The bound that does exist is the request's `levels` parameter,
        // whose absent case follows this layer's own declared zoom ranges, so an operator who sees
        // a number here they do not like has a declaration to change.
        //
        // No byte estimate: bytes per artifact depend on what the layer declares. A count-only
        // level is tens of bytes and one declaring a hull is unbounded, the rings being a function
        // of the membership. A constant here would be a guess; the artifact count is what is
        // actually known.
        for (layer, mut levels) in per_layer {
            levels.sort_unstable();
            let total: u64 = levels.iter().map(|(_, n)| *n).sum();
            // The levels this fold evaluated, which is not always the layer's whole set: a level
            // being retired is filtered out above, and so is one whose layer is no longer
            // registered. The build's report (`tessera_build::artifact_pass::report`) is where an
            // operator reads a layer's response cost across every level; this is the fold's own
            // view of what it just re-evaluated.
            tracing::info!(
                layer = %layer,
                evaluated_artifacts = total,
                per_level = ?levels,
                "the fold re-evaluated these levels of a layer; the sum is what a response naming \
                 them carries, one row per served artifact"
            );
        }
        out
    }

    /// Write this prefix's row-major columns, one file per `(view, layer, level)` whose chosen
    /// layout has one.
    ///
    /// A level whose label column will not compose gets no file, and the manifest then names none
    /// for it, so the level is served artifact-major on the reader's side instead (see
    /// `ArtifactProjections::get_or_build`). Whether an attribute is single-valued is a property of
    /// the data, so this is checked at fold time rather than at declaration.
    ///
    /// Every failure is an empty entry, not a discarded fold: a column is derived, and the
    /// artifact-major route answers every question it would have.
    fn write_row_columns(
        &self,
        pass: &mut DerivedPass<'_>,
        layouts: &[(String, u32, tessera_types::layer::ServingLayout)],
    ) -> Vec<tessera_store::manifest::DerivedExtent> {
        let prefix_dir = pass.prefix_dir;
        let partition = pass.partition;
        let n = pass.n;
        let incarnations = &pass.incarnations;
        let spaces = &pass.spaces;
        let pending = pass.pending;
        let fold_segments = &pass.segments;
        let wanted: Vec<(String, u32, tessera_types::layer::ServingLayout)> = layouts
            .iter()
            .filter(|(layer, level, layout)| {
                layout.is_row_major()
                    && self.composes_row_structures(pending, layer, *level)
                    // An attribute level's column is not this fold's to write. Its labels come
                    // from the value column the predicate names, and this pass composes from the
                    // level's stored memberships, which such a level has none of. Composing
                    // anyway would write a file of nothing but holes, name it in the manifest, and
                    // leave the reader adopting a column no request will ever claim. A spatial
                    // level's column is written, from the rows the pass just resolved.
                    && self
                        .live
                        .registered_layer(layer)
                        .is_some_and(|registered| {
                            matches!(
                                registered.declaration.membership,
                                tessera_types::layer::MembershipSource::Enumerated
                                    | tessera_types::layer::MembershipSource::Spatial
                            )
                        })
            })
            .cloned()
            .collect();
        if wanted.is_empty() || spaces.is_empty() {
            return Vec::new();
        }

        // Composed under one borrow with the versions they are written at, and filed outside it,
        // exactly as the tile indexes are.
        //
        // Straight to the file, not through a `RowColumn`: the fold wants the column's bytes and
        // nothing else, and `project_row_column` writes them front to back into a file under the
        // engine's scratch. Building a form here would hold the packed column and then copy it.
        let scratch = self.artifact_projections.scratch();
        let written: Vec<(
            String,
            String,
            u32,
            u64,
            tessera_types::layer::ServingLayout,
            std::path::PathBuf,
        )> = self.live.with_artifacts(|store| {
            let mut out = Vec::with_capacity(wanted.len() * spaces.len());
            for (layer, level, layout) in &wanted {
                let version = pending.version_after(store, layer, *level);
                let ordinals = level_length(pending.records(store, layer, *level));
                let spatial = self.live.registered_layer(layer).is_some_and(|registered| {
                    registered.declaration.membership
                        == tessera_types::layer::MembershipSource::Spatial
                });
                for (view, space) in spaces {
                    let composed = if spatial {
                        // The fold's segment is the whole base at row base 0, so the piece staged
                        // in `choose_layouts` is the level's membership in this view.
                        let piece = self.shapes.get(view, layer, *level).and_then(|held| {
                            fold_segments
                                .iter()
                                .find(|(v, _)| v == view)
                                .and_then(|(_, segment)| held.staged(&segment.seg_id))
                        });
                        match piece {
                            Some(piece) => {
                                let rows = piece.as_ref();
                                tessera_store::derived::project_row_column(
                                    rows.len() as u32,
                                    space.base_rows(),
                                    *layout,
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
                        tessera_store::derived::project_row_column(
                            ordinals,
                            space.base_rows(),
                            *layout,
                            scratch,
                            &|visit| {
                                for (ordinal, record) in pending.records(store, layer, *level) {
                                    visit(ordinal, &space.project_base(store.members_of(record)));
                                }
                            },
                        )
                    };
                    match composed {
                        Ok(Some(path)) => {
                            out.push((view.clone(), layer.clone(), *level, version, *layout, path))
                        }
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
                            "a row-major column would not be composed at the fold; that level \
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

        // Filed by the shared writer; see `write_containment_partitions` above.
        tessera_store::derived::file_derived(
            prefix_dir,
            partition,
            n,
            &mut pass.index,
            written
                .into_iter()
                .filter_map(|(view, layer, level, level_version, layout, path)| {
                    // No incarnation, no file: a structure addressed by row and stamped with a
                    // guessed incarnation would label another view's rows.
                    let Some(incarnation) = incarnations.get(&view).copied() else {
                        let _ = std::fs::remove_file(&path);
                        return None;
                    };
                    Some(tessera_store::derived::Filed {
                        view: Some(view),
                        incarnation: Some(incarnation),
                        layer,
                        level,
                        level_version,
                        form: tessera_store::manifest::DerivedForm::RowColumn { layout },
                        bytes: tessera_store::derived::FiledBytes::Staged(path),
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
        columns: &[tessera_store::manifest::DerivedExtent],
    ) -> Vec<tessera_store::manifest::DerivedExtent> {
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
        let mut filed: Vec<tessera_store::derived::Filed> = Vec::new();
        for (layer, level) in &levels {
            let version = self
                .live
                .with_artifacts(|store| store.level_version(layer, *level));
            for (view, segment) in fold_segments {
                let covered = columns.iter().any(|c| {
                    matches!(
                        c.form,
                        tessera_store::manifest::DerivedForm::RowColumn { .. }
                    ) && &c.layer == layer
                        && c.level == *level
                        && c.view.as_ref() == Some(view)
                });
                if covered {
                    continue;
                }
                let Some(piece) = self
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
                filed.push(tessera_store::derived::Filed {
                    view: Some(view.clone()),
                    incarnation: Some(incarnation),
                    layer: layer.clone(),
                    level: *level,
                    level_version: version,
                    form: tessera_store::manifest::DerivedForm::ShapeRows {
                        seg_id: segment.seg_id.clone(),
                        row_count: segment.row_count,
                    },
                    bytes: tessera_store::derived::FiledBytes::InHand(
                        tessera_store::derived::shape_rows_bytes(
                            version,
                            &segment.seg_id,
                            segment.row_count,
                            &piece,
                        ),
                    ),
                });
            }
        }
        tessera_store::derived::file_derived(prefix_dir, partition, n, &mut pass.index, filed)
    }

    /// Write this prefix's persisted decompositions: every spatial level's held shapes for each
    /// fold view, as the level holds them at its current version.
    fn write_shape_held(
        &self,
        pass: &mut DerivedPass<'_>,
    ) -> Vec<tessera_store::manifest::DerivedExtent> {
        let prefix_dir = pass.prefix_dir;
        let partition = pass.partition;
        let n = pass.n;
        let incarnations = &pass.incarnations;
        let pending = pass.pending;
        let fold_segments = &pass.segments;
        let filed: Vec<tessera_store::derived::Filed> = self.live.with_artifacts(|store| {
            let mut out = Vec::new();
            let levels: Vec<(String, u32)> = levels_of(store)
                .filter(|(layer, level)| !pending.is_pending(layer, *level))
                .filter(|(layer, _)| self.live.spatial_layer(layer))
                .collect();
            for (layer, level) in &levels {
                let version = store.level_version(layer, *level);
                for (view, _) in fold_segments {
                    let Some(held) = self.shapes.get(view, layer, *level) else {
                        continue;
                    };
                    if held.level_version != version {
                        continue;
                    }
                    let shapes: Vec<(Option<&[u8]>, Option<&tessera_store::derived::HeldShape>)> =
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
                    out.push(tessera_store::derived::Filed {
                        view: Some(view.clone()),
                        incarnation: Some(incarnation),
                        layer: layer.clone(),
                        level: *level,
                        level_version: version,
                        form: tessera_store::manifest::DerivedForm::ShapeHeld,
                        bytes: tessera_store::derived::FiledBytes::InHand(
                            tessera_store::derived::shape_held_bytes(version, &shapes),
                        ),
                    });
                }
            }
            out
        });
        tessera_store::derived::file_derived(prefix_dir, partition, n, &mut pass.index, filed)
    }

    fn write_tile_indexes(
        &self,
        pass: &mut DerivedPass<'_>,
        layouts: &[(String, u32, tessera_types::layer::ServingLayout)],
    ) -> Vec<tessera_store::manifest::DerivedExtent> {
        let prefix_dir = pass.prefix_dir;
        let partition = pass.partition;
        let n = pass.n;
        let incarnations = &pass.incarnations;
        let spaces = &pass.spaces;
        let pending = pass.pending;
        if spaces.is_empty() {
            return Vec::new();
        }

        // Projected under one borrow with the versions they are projected at, and written outside
        // it: projecting is the dear part and needs the store, writing a file does not.
        let projected: Vec<(String, String, u32, u64, Vec<u8>)> =
            self.live.with_artifacts(|store| {
                let levels: Vec<(String, u32)> = store
                    .levels_and_extents()
                    .map(|(layer, level, _)| (layer.to_string(), level))
                    .filter(|(layer, level)| self.composes_row_structures(pending, layer, *level))
                    // A row-major level has nothing to index: its candidacy is a scan of
                    // `viewport ∩ M_auth`, which the viewport already bounds. Writing one would be
                    // writing a file no reader on that route opens, and a level that falls back
                    // derives its index on first use anyway.
                    .filter(|(layer, level)| {
                        !layouts
                            .iter()
                            .any(|(l, v, layout)| l == layer && v == level && layout.is_row_major())
                    })
                    // A spatial level's index is not projected from its records, which carry no
                    // membership; an index of empties would be adopted at open and settle nothing.
                    // Its index is built at open over the held pieces instead, one pass per level
                    // over row extents.
                    .filter(|(layer, _)| {
                        !self.live.registered_layer(layer).is_some_and(|registered| {
                            registered.declaration.membership
                                == tessera_types::layer::MembershipSource::Spatial
                        })
                    })
                    .collect();
                let mut out = Vec::with_capacity(levels.len() * spaces.len());
                for (layer, level) in &levels {
                    let version = pending.version_after(store, layer, *level);
                    // The level's own length, holes included: a column sized by the last live
                    // ordinal is short, and a short one is dropped at open rather than adopted.
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

        // Filed by the shared writer; see `write_containment_partitions` above.
        tessera_store::derived::file_derived(
            prefix_dir,
            partition,
            n,
            &mut pass.index,
            projected
                .into_iter()
                .filter_map(|(view, layer, level, level_version, bytes)| {
                    // No incarnation, no file; see `write_row_columns`.
                    let incarnation = *incarnations.get(&view)?;
                    Some(tessera_store::derived::Filed {
                        view: Some(view),
                        incarnation: Some(incarnation),
                        layer,
                        level,
                        level_version,
                        form: tessera_store::manifest::DerivedForm::TileIndex,
                        bytes: tessera_store::derived::FiledBytes::InHand(bytes),
                    })
                })
                .collect(),
        )
    }

    /// Rebuild every level's row-space membership, and every lineage this fold moved, against the
    /// live generation.
    ///
    /// Called at the fold's own publication, on this thread: the alternative is not a cache miss
    /// but a stall, and it would land on a request rather than on maintenance. Cheap everywhere
    /// else, since a deployment with no artifacts iterates nothing.
    ///
    /// The lineages are warmed here for the same reason but not the same extent. A row form is
    /// invalid at every level because the prefix renumbered row space; a lineage is invalid only
    /// where this fold retired an artifact, because it holds ordinals. So this asks for all of
    /// both and pays for one of each per level that moved.
    ///
    /// Errors are impossible here, but absences are not: a view the generation does not carry is
    /// simply not warmed, and its first request builds what it needs.
    ///
    /// What each level's build reads is what the fold wrote and the publication adopted: an
    /// artifact-major level claims its tile index, a row-major one transposes its column, and a
    /// level with neither projects its memberships (`ArtifactProjections::get_or_build`).
    pub(super) fn warm_artifact_caches(&self) {
        let generation = self.generation.load_full();
        let levels: Vec<(String, u32)> =
            self.live.with_artifacts(|store| levels_of(store).collect());
        if levels.is_empty() {
            return;
        }
        let started = std::time::Instant::now();
        let before_projections = self.artifact_projections.builds();
        let before_lineages = self.lineages.builds();
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
                    // A predicate level is not warmed here: its membership is evaluated against
                    // the geometry, so a form built now would be filed under this fold's
                    // `segments_version` and the first flush after it rebuilds anyway. The pieces
                    // a rule needs (the column's value layers, the view's quantisation, the
                    // segment list) are the request path's, and reaching for them here would be a
                    // second place the membership is assembled.
                    //
                    // A spatial level is warmed, because its membership is held rather than
                    // evaluated: the fold's artifact pass resolved the new segments against the
                    // level's shapes and staged the pieces, so the build here is the assembly of
                    // them, and the form it produces is maintained from then on.
                    let registered = self.live.registered_layer(layer);
                    let membership = registered
                        .as_ref()
                        .map(|r| r.declaration.membership.clone());
                    let spatial = registered
                        .as_ref()
                        .is_some_and(|r| spatial_membership(&r.declaration));
                    if matches!(
                        membership,
                        Some(tessera_types::layer::MembershipSource::Attribute(_))
                    ) || (!spatial
                        && !matches!(
                            membership,
                            Some(tessera_types::layer::MembershipSource::Enumerated)
                        ))
                    {
                        continue;
                    }
                    let segments = if spatial {
                        crate::viewport::segments_with_row_bases(view, view_data).ok()
                    } else {
                        None
                    };
                    self.live.with_artifacts(|store| {
                        let predicate = segments.as_ref().map(|segments| {
                            crate::artifacts::PredicateSource::Spatial(
                                crate::artifacts::SpatialSource {
                                    level: self.shapes.level(
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
                        self.artifact_projections.get_or_build(
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
                            self.live.registered_layer(layer).is_some_and(|r| {
                                crate::artifacts::serves_column_only(&r.declaration)
                            }),
                        )
                    });
                }
            }
        }
        for (layer, level) in &levels {
            self.live.with_artifacts(|store| {
                self.lineages.get_or_build(
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
            projections = self.artifact_projections.builds() - before_projections,
            lineages = self.lineages.builds() - before_lineages,
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
    segments: &[tessera_store::manifest::SegmentDescriptor],
) -> Vec<(String, tessera_store::read::SegmentData)> {
    let mut out = Vec::with_capacity(segments.len());
    for descriptor in segments {
        let partition_dir = prefix_dir.join("partitions").join(partition);
        let dir = tessera_store::view_path(&partition_dir, &descriptor.view)
            .join("segments")
            .join(&descriptor.seg_id);
        let morton = tessera_store::read::MortonSlice::load(&dir.join("morton.u32"));
        let cuts = tessera_store::read::CutIndex::load(
            &dir.join(tessera_store::read::CutIndex::FILE),
            descriptor.row_count,
        );
        let columns = tessera_store::read::ColumnsRef::load(&dir.join("columns.arrow"));
        match (morton, cuts, columns) {
            (Ok(morton), Ok(cuts), Ok(columns)) => out.push((
                descriptor.view.clone(),
                tessera_store::read::SegmentData {
                    seg_id: descriptor.seg_id.clone(),
                    row_count: descriptor.row_count,
                    morton,
                    cuts,
                    columns,
                },
            )),
            (Err(error), _, _) | (_, Err(error), _) | (_, _, Err(error)) => tracing::warn!(
                view = %descriptor.view,
                seg_id = %descriptor.seg_id,
                %error,
                "the fold could not reopen a segment it just wrote; its spatial memberships are \
                 resolved at the flip instead"
            ),
        }
    }
    out
}
