//! The **entity-space** coalesce: what a maintenance pass may bound without touching row space.
//!
//! Flush appends one delta tier, one external-id run, one locator extent and (when it promotes) one
//! dictionary extent per tick. Three of those four are terms in a *steady-state* cost:
//!
//! - a fragment build probes **every live tier** per satisfied term (`build_fragment_with_deltas`),
//! - the ingest duplicate check and the drill-down scan **every run** whose bounds admit the key
//!   (`ExternalIdSidecar::resolve`, `resolve_many`),
//! - `Engine::open` reads **every dictionary extent**.
//!
//! At a 90 s tick that is ~960 of each per day of sustained ingest. Coalescing them is a
//! **content-preserving re-encode** — the same postings, the same bindings, the same descriptors in
//! the same order — so it is decision 0044's D2 half of merge: it bumps no `segments_version`,
//! rotates no cache key, invalidates no projection and no fragment, and is 0043-conforming by
//! construction rather than by a refresh mechanism. The row-space half (`tessera_store::merge`) is
//! the one that permutes row ids, and it is gated on 0044's D1 mechanism; this is not.
//!
//! ## What makes each axis safe to rewrite
//!
//! Each is a different argument, and none of them is "it is obviously fine":
//!
//! - **Tiers** are unioned into a fragment, so their order and their division into files are both
//!   immaterial; what must not change is the set of `(term, entity)` pairs, which
//!   [`tessera_authz::coalesce_delta_tiers`] preserves exactly.
//! - **Runs** are searched newest-first and a key may appear in several of them (decision 0047's
//!   re-binding), so a coalesced run must keep the **newest** binding — and the window must be
//!   contiguous in the list, or the coalesced run would sit at a recency position it did not earn.
//! - **Dictionary extents** are positional: an ordinal is an index into the concatenation in listed
//!   order, and a session's granted terms are resolved once at authorise and never re-resolved. The
//!   window must be contiguous and land in place, or every ordinal after it shifts and a session
//!   evaluates a term it was not granted.
//!
//! ## What it must never take
//!
//! **The build's own artefacts**, which are the entries digest-named in `MANIFEST.json` rather than
//! in the side-manifest. Two reasons, and the second is the sharp one: rewriting a file the bundle
//! manifest names means writing a new prefix, which is compaction under another name; and the base
//! `ext-locator.u32`'s ordinals are positions in the concatenation of the build's runs in listed
//! order, so a coalesce that consumed or reordered them would renumber the whole base direction.
//! `ExternalIdSidecar::deferred_from_manifest` also derives the base locator's path from
//! `external_id_runs[0]`, which stops resolving the moment that entry is not the build's.
//!
//! **A merge retires nothing.** No tombstone is applied and no posting is dropped for a deleted
//! entity. A pass here that did either has left this module and entered compaction's.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use tessera_authz::{coalesce_dict_extents, coalesce_delta_tiers, DeltaTier};
use tessera_store::coalesce_external_id_runs;
use tessera_store::manifest::{DictExtent, FileDigest, LocatorExtent, SegmentsManifest};
use tessera_store::merge::size_tier;

/// The tag rule a coalesced tier's postings use. Same constant, same reason, as
/// `crate::flush::SMALL_TERM_THRESHOLD`: the threshold decides an encoding, never a content.
const SMALL_TERM_THRESHOLD: u32 = 32;

/// What a coalesce is allowed to take, per axis.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CoalescePolicy {
    /// How many same-tier entries select a coalesce. Below this, nothing is coalesced; `< 2`
    /// disables the pass entirely.
    pub(crate) width: usize,
    /// Sizes at or below this compare **equal** — see [`size_tier`]. A flush's tier and run are
    /// kilobytes at a modest ingest rate, so without a floor every tick produces its own size class
    /// and the width is never reached: the failure is silent and looks like a policy that simply
    /// never triggers.
    pub(crate) floor_bytes: u64,
    /// The most input bytes one axis may take in one pass. Bounds the pool transient: tier
    /// coalescence holds every input tier's pairs at once (~2–3× their bytes), and the run
    /// coalesce holds every input run's keys.
    pub(crate) max_input_bytes: u64,
}

impl Default for CoalescePolicy {
    /// **Eight, 1 MiB, 256 MiB.** The width is the same shape as the segment merge's `tier_width`
    /// and a little wider, because an entity-space pass costs no projection rebuild and can afford
    /// to run less often per byte moved. The floor is a size at which per-file overheads stop
    /// dominating; the input cap is `max_merged_segment_bytes`' order, which models to a
    /// ~0.5–0.8 GB pool transient on the tier axis.
    fn default() -> Self {
        CoalescePolicy {
            width: 8,
            floor_bytes: 1 << 20,
            max_input_bytes: 256 << 20,
        }
    }
}

/// One coalesce's immutable plan: which entries of which axes it consumes.
///
/// **Everything is named by path**, never by index. `seg_id`s and the paths derived from them are
/// never reused (contracts §2.1), so a path that is still in the live manifest at publication is
/// still the same bytes — which is what makes the rebase ABA-safe against the flushes that
/// published while this ran.
#[derive(Debug, Default)]
pub(crate) struct CoalescePlan {
    pub(crate) partition: String,
    /// Consumed `deltas` entries, in list order. Empty if the tier axis did not qualify.
    pub(crate) tiers: Vec<String>,
    /// Consumed `external_id_runs` entries, oldest first — the order the keep-newest rule reads.
    pub(crate) runs: Vec<String>,
    /// The `locator_extents` entries indexing those runs, in list order.
    pub(crate) locators: Vec<LocatorExtent>,
    /// Consumed `dict_extents` entries, in list order.
    pub(crate) dicts: Vec<DictExtent>,
}

impl CoalescePlan {
    pub(crate) fn is_empty(&self) -> bool {
        self.tiers.is_empty() && self.runs.is_empty() && self.dicts.is_empty()
    }
}

/// Plan a coalesce over `manifest`, taking `build_files` to be the build's own artefacts.
///
/// Pure, and takes the two manifests rather than a generation, so every selection rule is testable
/// without an engine. Returns `None` when no axis qualifies, which is the ordinary answer for all
/// but one tick in `width`.
pub(crate) fn plan_coalesce(
    partition: &str,
    manifest: &SegmentsManifest,
    build_files: &BTreeMap<String, FileDigest>,
    policy: CoalescePolicy,
) -> Option<CoalescePlan> {
    if policy.width < 2 {
        return None;
    }
    let size_of = |rel: &str| -> u64 {
        manifest
            .files
            .get(rel)
            .or_else(|| build_files.get(rel))
            .map_or(0, |d| d.size)
    };
    let is_build = |rel: &str| build_files.contains_key(rel);

    let mut plan = CoalescePlan {
        partition: partition.to_string(),
        ..Default::default()
    };

    // ---- tiers: any contiguous same-tier window, because a union has no order ----------------
    if let Some(window) = select_window(&manifest.deltas, policy, |rel| {
        (!is_build(rel)).then(|| size_of(rel))
    }) {
        plan.tiers = manifest.deltas[window].to_vec();
    }

    // ---- runs: driven from the locator extents, which name their run --------------------------
    //
    // **Entity-adjacent, exactly as `MergePolicy::select` requires of segments.** The coalesced
    // locator extent covers one span `[lo, hi]`, and `external_id_of_checked` finds an extent by
    // the first span that contains the entity — so a span overlapping another extent's would
    // answer one entity's ordinal against another's run. Ascending and non-overlapping is what
    // makes the union a single well-formed span; it is satisfied trivially at one slice per
    // partition, and it is what keeps two slices' interleaved flushes from being coalesced
    // together.
    let locator_size = |extent: &LocatorExtent| -> Option<u64> {
        (!is_build(&extent.path) && !is_build(&extent.external_id_run))
            .then(|| size_of(&extent.path) + size_of(&extent.external_id_run))
    };
    if let Some(window) = select_window(&manifest.locator_extents, policy, locator_size) {
        let extents = &manifest.locator_extents[window];
        let adjacent = extents
            .windows(2)
            .all(|pair| pair[0].entity_hi < pair[1].entity_lo);
        // The runs must be a contiguous block of `external_id_runs`, in the same order: the
        // coalesced run takes the block's position, and recency is list position.
        let runs: Vec<String> = extents.iter().map(|e| e.external_id_run.clone()).collect();
        let contiguous = manifest
            .external_id_runs
            .windows(runs.len().max(1))
            .any(|w| w == runs.as_slice());
        if adjacent && contiguous {
            plan.locators = extents.to_vec();
            plan.runs = runs;
        }
    }

    // ---- dictionary extents: contiguous and in place, because ordinals are positions ----------
    if let Some(window) = select_window(&manifest.dict_extents, policy, |extent: &DictExtent| {
        (!is_build(&extent.path)).then(|| size_of(&extent.path))
    }) {
        plan.dicts = manifest.dict_extents[window].to_vec();
    }

    (!plan.is_empty()).then_some(plan)
}

/// The first window of `policy.width` consecutive entries that are all eligible, share one size
/// tier, and total within `policy.max_input_bytes`.
///
/// `size_of` returns `None` for an entry this axis may not take — the build's own artefacts —
/// which both excludes it and breaks the window, so a selection can never straddle one.
///
/// **The first qualifying window wins, not the best one**, for the reason `MergePolicy::select`
/// gives: this is idempotent work on a cadence, and a policy nobody can predict from the manifest
/// costs more than a marginally better choice buys.
fn select_window<T>(
    entries: &[T],
    policy: CoalescePolicy,
    size_of: impl Fn(&T) -> Option<u64>,
) -> Option<std::ops::Range<usize>> {
    if entries.len() < policy.width {
        return None;
    }
    for start in 0..=entries.len() - policy.width {
        let window = &entries[start..start + policy.width];
        let Some(sizes) = window.iter().map(&size_of).collect::<Option<Vec<u64>>>() else {
            continue;
        };
        let tier = size_tier(sizes[0], policy.floor_bytes);
        if !sizes
            .iter()
            .all(|s| size_tier(*s, policy.floor_bytes) == tier)
        {
            continue;
        }
        if sizes.iter().sum::<u64>() > policy.max_input_bytes {
            continue;
        }
        return Some(start..start + policy.width);
    }
    None
}

/// Everything [`execute_coalesce`] needs beyond its plan — taken from the generation on the
/// executor thread and then immutable, exactly as [`crate::flush::FlushContext`] is.
pub(crate) struct CoalesceContext {
    pub(crate) prefix_dir: PathBuf,
    pub(crate) prefix: String,
    /// The directory every output of this pass is written into, prefix-relative. A **never-reused**
    /// id in `seg_id`'s namespace (contracts §2.1): two passes at one `n` would otherwise write the
    /// same paths, and the second `File::create` would truncate files the first has memory-mapped.
    pub(crate) out_rel: String,
}

/// A coalesce whose files are durable, awaiting the manifest edit and the swap on the executor.
pub(crate) struct CompletedCoalesce {
    pub(crate) plan: CoalescePlan,
    pub(crate) prefix: String,
    /// The coalesced tier's path and its reopened reader, or `None` if the tier axis did not run.
    pub(crate) tier: Option<(String, Arc<DeltaTier>)>,
    /// The coalesced run's path, and the locator extent covering the consumed extents' union span.
    pub(crate) run: Option<(String, LocatorExtent)>,
    pub(crate) dict: Option<DictExtent>,
    /// Every file this pass wrote, prefix-relative, with its digest — computed on the pool.
    pub(crate) files: BTreeMap<String, FileDigest>,
}

/// Why a coalesce produced nothing. **Every failure is "nothing happened, retry next tick"**: the
/// manifest is the only commit point, so a failure before it leaves orphan files nothing
/// references and the consumed entries stand.
#[derive(Debug)]
pub(crate) struct CoalesceFailed(pub(crate) String);

impl std::fmt::Display for CoalesceFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Turn a plan into durable files. **Runs on the background pool, over immutable inputs.**
pub(crate) fn execute_coalesce(
    plan: CoalescePlan,
    ctx: CoalesceContext,
) -> Result<CompletedCoalesce, CoalesceFailed> {
    let out_dir = ctx.prefix_dir.join(&ctx.out_rel);
    std::fs::create_dir_all(&out_dir).map_err(|e| CoalesceFailed(format!("coalesce dir: {e}")))?;
    let rel = |name: &str| format!("{}/{name}", ctx.out_rel);
    let mut files: BTreeMap<String, FileDigest> = BTreeMap::new();

    let tier = if plan.tiers.is_empty() {
        None
    } else {
        let inputs: Vec<PathBuf> = plan.tiers.iter().map(|p| ctx.prefix_dir.join(p)).collect();
        let path = out_dir.join("delta.arrow");
        coalesce_delta_tiers(&inputs, &path, SMALL_TERM_THRESHOLD)
            .map_err(|e| CoalesceFailed(format!("delta tiers: {e}")))?;
        files.insert(rel("delta.arrow"), digest_of(&path)?);
        let reader =
            DeltaTier::open(&path).map_err(|e| CoalesceFailed(format!("coalesced tier: {e}")))?;
        Some((rel("delta.arrow"), Arc::new(reader)))
    };

    let run = if plan.runs.is_empty() {
        None
    } else {
        let inputs: Vec<PathBuf> = plan.runs.iter().map(|p| ctx.prefix_dir.join(p)).collect();
        // The union of the consumed extents' spans, which the planner has already checked is one
        // ascending, non-overlapping sequence — so this is a single span with the same coverage.
        let entity_lo = plan.locators[0].entity_lo;
        let entity_hi = plan.locators[plan.locators.len() - 1].entity_hi;
        coalesce_external_id_runs(&inputs, entity_lo, entity_hi, &out_dir)
            .map_err(|e| CoalesceFailed(format!("external-id runs: {e}")))?;
        files.insert(
            rel("external-ids.arrow"),
            digest_of(&out_dir.join("external-ids.arrow"))?,
        );
        files.insert(
            rel("ext-locator.u32"),
            digest_of(&out_dir.join("ext-locator.u32"))?,
        );
        Some((
            rel("external-ids.arrow"),
            LocatorExtent {
                path: rel("ext-locator.u32"),
                entity_lo,
                entity_hi,
                external_id_run: rel("external-ids.arrow"),
            },
        ))
    };

    let dict = if plan.dicts.is_empty() {
        None
    } else {
        let inputs: Vec<PathBuf> = plan
            .dicts
            .iter()
            .map(|e| ctx.prefix_dir.join(&e.path))
            .collect();
        let path = out_dir.join("terms-0.dict");
        let records = coalesce_dict_extents(&inputs, &path)
            .map_err(|e| CoalesceFailed(format!("dictionary extents: {e}")))?;
        // **The record count is checked, not trusted.** `Dict::load` counts *distinct*
        // descriptors while a `records` field counts records, and the two differ only in a case
        // the writer is forbidden to produce (decision 0042). A disagreement here means an input
        // extent repeated a descriptor, which is the silent cross-compartment renumbering 0042
        // exists to prevent — so it fails the pass rather than republishing it under one name.
        let declared: u64 = plan.dicts.iter().map(|e| e.records).sum();
        if records != declared {
            return Err(CoalesceFailed(format!(
                "the coalesced dictionary extent holds {records} records where its inputs declare \
                 {declared}; an input repeated a descriptor (decision 0042) and coalescing it \
                 would renumber every ordinal after the repeat"
            )));
        }
        files.insert(rel("terms-0.dict"), digest_of(&path)?);
        Some(DictExtent {
            path: rel("terms-0.dict"),
            records,
        })
    };

    Ok(CompletedCoalesce {
        plan,
        prefix: ctx.prefix,
        tier,
        run,
        dict,
        files,
    })
}

/// Apply `completed` to `manifest` in place, or `false` if it no longer rebases.
///
/// **Every consumed entry must still be present, contiguous and in order**, on every axis it
/// touched. That is the rebase: a flush publishing while this ran *appends*, which moves nothing
/// this plan named, so the ordinary answer is that the window is exactly where it was. Anything
/// else means the state the plan was made against is gone, and the coalesce is discarded — its
/// files orphans nothing references, the consumed entries standing, the next tick re-planning.
///
/// **The coalesced entry takes the window's position**, never the end of the list. On the run axis
/// that preserves recency, which decision 0047's newest-binding-first resolution reads off list
/// order; on the dictionary axis it preserves every ordinal, which is a position in the
/// concatenation. Appending instead would be silently wrong on both.
pub(crate) fn rebase_into(manifest: &mut SegmentsManifest, completed: &CompletedCoalesce) -> bool {
    let plan = &completed.plan;

    let tiers = match window_of(&manifest.deltas, &plan.tiers, |rel| rel) {
        Some(at) => at,
        None => return false,
    };
    let runs = match window_of(&manifest.external_id_runs, &plan.runs, |rel| rel) {
        Some(at) => at,
        None => return false,
    };
    let locator_paths: Vec<String> = plan.locators.iter().map(|e| e.path.clone()).collect();
    let locators = match window_of(&manifest.locator_extents, &locator_paths, |e| &e.path) {
        Some(at) => at,
        None => return false,
    };
    let dict_paths: Vec<String> = plan.dicts.iter().map(|e| e.path.clone()).collect();
    let dicts = match window_of(&manifest.dict_extents, &dict_paths, |e| &e.path) {
        Some(at) => at,
        None => return false,
    };

    for rel in plan
        .tiers
        .iter()
        .chain(&plan.runs)
        .chain(&locator_paths)
        .chain(&dict_paths)
    {
        manifest.files.remove(rel);
    }
    manifest.files.extend(
        completed
            .files
            .iter()
            .map(|(rel, digest)| (rel.clone(), digest.clone())),
    );

    if let Some((path, _)) = &completed.tier {
        manifest.deltas.splice(tiers, [path.clone()]);
    }
    if let Some((path, extent)) = &completed.run {
        manifest.external_id_runs.splice(runs, [path.clone()]);
        manifest.locator_extents.splice(locators, [extent.clone()]);
    }
    if let Some(extent) = &completed.dict {
        manifest.dict_extents.splice(dicts, [extent.clone()]);
    }
    true
}

/// Where `needle` sits in `haystack`, as a contiguous run of equal keys — or the empty range at 0
/// when `needle` is empty (an axis this plan did not take), which splices nothing.
fn window_of<'a, T, K: PartialEq + 'a>(
    haystack: &'a [T],
    needle: &[K],
    key: impl Fn(&'a T) -> &'a K,
) -> Option<std::ops::Range<usize>> {
    if needle.is_empty() {
        return Some(0..0);
    }
    if haystack.len() < needle.len() {
        return None;
    }
    (0..=haystack.len() - needle.len())
        .find(|&start| {
            haystack[start..start + needle.len()]
                .iter()
                .map(&key)
                .eq(needle.iter())
        })
        .map(|start| start..start + needle.len())
}

fn digest_of(path: &std::path::Path) -> Result<FileDigest, CoalesceFailed> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path)
        .map_err(|e| CoalesceFailed(format!("digest {}: {e}", path.display())))?;
    let digest = Sha256::digest(&bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    Ok(FileDigest {
        size: bytes.len() as u64,
        sha256: hex,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PARTITION: &str = "p0";

    fn policy() -> CoalescePolicy {
        CoalescePolicy {
            width: 3,
            floor_bytes: 1 << 20,
            max_input_bytes: 1 << 30,
        }
    }

    /// A real, empty coalesced tier — the reader `publish_coalesce` installs on the generation.
    /// Built rather than stubbed because `CompletedCoalesce` carries the opened reader, and a test
    /// double there would be a second definition of what a tier is.
    fn tier_at(dir: &std::path::Path) -> (String, Arc<DeltaTier>) {
        let path = dir.join("delta.arrow");
        tessera_authz::write_delta_tier(&path, &[], SMALL_TERM_THRESHOLD).expect("a tier writes");
        (
            "c/delta.arrow".to_string(),
            Arc::new(DeltaTier::open(&path).expect("it opens")),
        )
    }

    fn digest(size: u64) -> FileDigest {
        FileDigest {
            size,
            sha256: "0".repeat(64),
        }
    }

    /// A manifest with `flushes` flushes' worth of entity-space artefacts on every axis, plus the
    /// build's own run and dictionary extent — which is the arrangement `tessera build` leaves and
    /// every selection rule below is stated against.
    fn manifest_with(flushes: u64) -> (SegmentsManifest, BTreeMap<String, FileDigest>) {
        let build_files: BTreeMap<String, FileDigest> = [
            ("entities/external-ids-0.arrow".to_string(), digest(4096)),
            ("terms/terms-0.dict".to_string(), digest(4096)),
        ]
        .into_iter()
        .collect();

        let mut manifest = SegmentsManifest {
            watermark: 0,
            entity_id_high_water: 0,
            segments: Vec::new(),
            deltas: Vec::new(),
            dict_extents: vec![DictExtent {
                path: "terms/terms-0.dict".to_string(),
                records: 4,
            }],
            external_id_runs: vec!["entities/external-ids-0.arrow".to_string()],
            locator_extents: Vec::new(),
            tombstones: Vec::new(),
            deny: Vec::new(),
            files: BTreeMap::new(),
        };
        for i in 0..flushes {
            let seg = format!("partitions/{PARTITION}/slices/s0/segments/flush-{i}-1");
            for name in ["delta.arrow", "external-ids.arrow", "ext-locator.u32", "terms-0.dict"] {
                manifest.files.insert(format!("{seg}/{name}"), digest(1024));
            }
            manifest.deltas.push(format!("{seg}/delta.arrow"));
            manifest
                .external_id_runs
                .push(format!("{seg}/external-ids.arrow"));
            manifest.locator_extents.push(LocatorExtent {
                path: format!("{seg}/ext-locator.u32"),
                entity_lo: i * 10,
                entity_hi: i * 10 + 9,
                external_id_run: format!("{seg}/external-ids.arrow"),
            });
            manifest.dict_extents.push(DictExtent {
                path: format!("{seg}/terms-0.dict"),
                records: 1,
            });
        }
        (manifest, build_files)
    }

    /// **The build's own artefacts are never taken**, on any axis. Rewriting a file
    /// `MANIFEST.json` digests means writing a new prefix — compaction under another name — and the
    /// base locator's ordinals are positions in the build's runs, so consuming one renumbers the
    /// whole reverse direction for every entity the build knew about.
    ///
    /// **Mutation:** drop the `is_build` guards and the plan takes run 0 and dict extent 0.
    #[test]
    fn the_builds_own_run_and_dictionary_extent_are_never_selected() {
        let (manifest, build_files) = manifest_with(3);
        let plan = plan_coalesce(PARTITION, &manifest, &build_files, policy()).expect("a plan");
        assert!(
            !plan.runs.contains(&"entities/external-ids-0.arrow".to_string()),
            "the build's run: {:?}",
            plan.runs
        );
        assert!(
            !plan.dicts.iter().any(|e| e.path == "terms/terms-0.dict"),
            "the build's dictionary extent: {:?}",
            plan.dicts
        );
    }

    /// Locator extents whose spans overlap are not one span. `external_id_of_checked` finds an
    /// extent by the first span containing the entity, so a coalesced extent overlapping another
    /// would answer one entity's ordinal against another run's keys.
    #[test]
    fn overlapping_locator_spans_are_refused_on_the_run_axis() {
        let (mut manifest, build_files) = manifest_with(3);
        manifest.locator_extents[1].entity_lo = 0; // now overlaps extent 0
        let plan = plan_coalesce(PARTITION, &manifest, &build_files, policy()).expect("a plan");
        assert!(
            plan.runs.is_empty(),
            "the run axis must not select across overlapping spans: {:?}",
            plan.runs
        );
        assert!(
            !plan.tiers.is_empty(),
            "the tier axis is independent and still qualifies"
        );
    }

    /// Size tiering is what bounds write amplification: without it the pass re-reads the artefact
    /// it produced last round, for ever. A window straddling two size classes is not selected.
    #[test]
    fn a_window_spanning_two_size_classes_is_not_selected() {
        let (mut manifest, build_files) = manifest_with(3);
        let big = manifest.deltas[1].clone();
        manifest.files.insert(big, digest(64 << 20));
        let plan = plan_coalesce(PARTITION, &manifest, &build_files, policy());
        assert!(
            plan.as_ref().is_none_or(|p| p.tiers.is_empty()),
            "a 64 MiB tier and two 1 KiB ones are not one class"
        );
    }

    /// Below the width nothing is selected — the ordinary answer at all but one tick in `width`.
    #[test]
    fn nothing_is_selected_below_the_width() {
        let (manifest, build_files) = manifest_with(2);
        assert!(plan_coalesce(PARTITION, &manifest, &build_files, policy()).is_none());
    }

    /// **The coalesced entry lands where the window was, never at the end.** On the run axis that
    /// is recency, which decision 0047's newest-binding-first resolution reads off list order; on
    /// the dictionary axis it is every ordinal after the window.
    ///
    /// **Mutation:** push instead of splice and the coalesced run becomes the newest, so a key it
    /// carries an old binding for outranks the flush that re-bound it.
    #[test]
    fn the_coalesced_entry_takes_the_windows_position() {
        let (mut manifest, build_files) = manifest_with(4);
        let mut policy = policy();
        policy.width = 3;
        let plan = plan_coalesce(PARTITION, &manifest, &build_files, policy).expect("a plan");

        let dir = tempfile::TempDir::new().unwrap();
        let completed = CompletedCoalesce {
            tier: Some(tier_at(dir.path())),
            run: Some((
                "c/external-ids.arrow".to_string(),
                LocatorExtent {
                    path: "c/ext-locator.u32".to_string(),
                    entity_lo: plan.locators[0].entity_lo,
                    entity_hi: plan.locators[plan.locators.len() - 1].entity_hi,
                    external_id_run: "c/external-ids.arrow".to_string(),
                },
            )),
            dict: Some(DictExtent {
                path: "c/terms-0.dict".to_string(),
                records: 3,
            }),
            files: [("c/delta.arrow".to_string(), digest(3072))]
                .into_iter()
                .collect(),
            plan,
            prefix: "v00000".to_string(),
        };
        assert!(rebase_into(&mut manifest, &completed));

        assert_eq!(manifest.deltas.len(), 2, "3 tiers became 1, 1 untouched");
        assert_eq!(manifest.deltas[0], "c/delta.arrow");
        assert_eq!(
            manifest.external_id_runs[0], "entities/external-ids-0.arrow",
            "the build's run stays listed first — the base locator's ordinals resolve inside it"
        );
        assert_eq!(manifest.external_id_runs[1], "c/external-ids.arrow");
        assert_eq!(
            manifest.dict_extents[0].path, "terms/terms-0.dict",
            "the build's dictionary extent keeps ordinal 0"
        );
        assert_eq!(manifest.dict_extents[1].path, "c/terms-0.dict");
        assert!(
            !manifest
                .files
                .keys()
                .any(|k| k.contains("flush-0-1/delta.arrow")),
            "a consumed file leaves the files map"
        );
    }

    /// A plan whose window is gone no longer rebases, and the publication is discarded rather than
    /// forced — its files orphans nothing references, every consumed entry still standing.
    #[test]
    fn a_plan_whose_window_moved_does_not_rebase() {
        let (mut manifest, build_files) = manifest_with(3);
        let plan = plan_coalesce(PARTITION, &manifest, &build_files, policy()).expect("a plan");
        let dir = tempfile::TempDir::new().unwrap();
        let completed = CompletedCoalesce {
            tier: Some(tier_at(dir.path())),
            run: None,
            dict: None,
            files: BTreeMap::new(),
            plan,
            prefix: "v00000".to_string(),
        };
        manifest.deltas.remove(1);
        assert!(!rebase_into(&mut manifest, &completed));
    }
}
