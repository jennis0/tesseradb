//! Coalesces entity-space maintenance artefacts without touching row space: delta tiers,
//! external-id runs and their locator extents, dictionary extents, attribute extents, the record
//! blob, text extents and entity-to-term extents.
//!
//! Each axis merges several small extents into one, preserving the set of entries it holds. Some
//! merges renumber ordinals against a merged dictionary; others concatenate. A coalesce changes no
//! row id, bumps no `segments_version`, invalidates no cache or projection, and never touches the
//! build's own artefacts (the files named in `MANIFEST.json` rather than in a side-manifest list).
//! It retires nothing: no posting is dropped and no tombstone is applied.
//!
//! [`plan_coalesce`] runs on the executor and chooses what to take. [`execute_coalesce`] runs on
//! the background pool and writes the merged files. [`rebase_into`] applies the result to the live
//! manifest, or discards it if a flush moved the entries it planned against.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tessera_authz::{coalesce_delta_tiers, coalesce_dict_extents, DeltaTier};
use tessera_store::coalesce_external_id_runs;
use tessera_store::manifest::{
    AttrExtent, DictExtent, EntityTermsExtent, FileDigest, LocatorExtent, RecordExtent,
    SegmentsManifest, TextExtent,
};
use tessera_store::merge::size_tier;

use crate::flush::{digest_of, MaintenanceFailed, SMALL_TERM_THRESHOLD};

/// What a coalesce is allowed to take, per axis.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CoalescePolicy {
    /// How many same-tier entries select a coalesce. Below 2 the pass is disabled.
    pub(crate) width: usize,
    /// Sizes at or below this compare equal (see [`size_tier`]). Without a floor, entries at a
    /// modest ingest rate each fall in their own size class and the width is never reached.
    pub(crate) floor_bytes: u64,
    /// The most input bytes one axis may take in one pass, bounding the memory held while merging.
    pub(crate) max_input_bytes: u64,
}

impl Default for CoalescePolicy {
    /// Eight entries, a 1 MiB floor, a 256 MiB input cap.
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
/// Entries are named by path, not index. Paths are never reused, so a path still in the live
/// manifest at publication is still the same bytes: a flush that published while the pass ran can
/// only append, never move what the plan named.
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
    /// Consumed `attr_extents` entries, one window per column.
    pub(crate) attrs: Vec<ColumnWindow<AttrExtent>>,
    /// Consumed `record_extents` entries: one contiguous window over the record blob's single
    /// pseudo-column. Empty if the axis did not qualify.
    pub(crate) records: Vec<RecordExtent>,
    /// Consumed `text_extents` entries, one window per text column.
    pub(crate) texts: Vec<ColumnWindow<TextExtent>>,
    /// Consumed `entity_terms_extents` entries: one contiguous window. Empty if the axis did not
    /// qualify.
    pub(crate) terms: Vec<EntityTermsExtent>,
}

/// Where a coalesced window's output lives, prefix-relative: `<out>/attrs/<column>/` for an
/// entity-scoped column, and `<out>/attrs/<column>/<group>/<key>/` for one view's column of a
/// group-scoped family.
fn coalesced_column_rel(out_rel: &str, column: &str, view: Option<&str>) -> String {
    let mut rel = format!("{out_rel}/attrs/{column}");
    if let Some(view) = view {
        for component in tessera_store::view_path_components(view) {
            rel.push('/');
            rel.push_str(component);
        }
    }
    rel
}

/// The key a coalesce window is grouped by: the column, the view whose column of a group-scoped
/// family it is, and that view's incarnation. The last two are `None` together for an
/// entity-scoped column, which belongs to no view.
type WindowKey<'a> = (
    &'a str,
    Option<&'a str>,
    Option<tessera_types::view::ViewIncarnation>,
);

/// An extent listed under one column, on an axis whose list interleaves the columns.
trait ColumnExtent {
    fn key(&self) -> WindowKey<'_>;
}

impl ColumnExtent for AttrExtent {
    fn key(&self) -> WindowKey<'_> {
        (self.column.as_str(), self.view.as_deref(), self.incarnation)
    }
}

impl ColumnExtent for TextExtent {
    fn key(&self) -> WindowKey<'_> {
        (self.column.as_str(), self.view.as_deref(), self.incarnation)
    }
}

/// One column's contiguous window of its own subsequence of an axis's list.
#[derive(Debug, Clone)]
pub(crate) struct ColumnWindow<E> {
    pub(crate) column: String,
    /// The view whose column of a group-scoped family this window belongs to, `None` for an
    /// ordinary entity-scoped column. A family's columns share one name, so the key is
    /// `(column, view)` rather than the column alone.
    pub(crate) view: Option<String>,
    /// The incarnation of `view` these extents belong to, `None` exactly when `view` is.
    pub(crate) incarnation: Option<tessera_types::view::ViewIncarnation>,
    pub(crate) extents: Vec<E>,
}

impl CoalescePlan {
    pub(crate) fn is_empty(&self) -> bool {
        self.tiers.is_empty()
            && self.runs.is_empty()
            && self.dicts.is_empty()
            && self.attrs.is_empty()
            && self.records.is_empty()
            && self.texts.is_empty()
            && self.terms.is_empty()
    }
}

/// Plan a coalesce over `manifest`, taking `build_files` to be the build's own artefacts.
///
/// Pure, and takes the two manifests rather than a generation, so every selection rule is testable
/// without an engine. Returns `None` when no axis qualifies.
pub(crate) fn plan_coalesce(
    partition: &str,
    manifest: &SegmentsManifest,
    build_files: &BTreeMap<String, FileDigest>,
    policy: CoalescePolicy,
    is_live: &dyn Fn(&str, tessera_types::view::ViewIncarnation) -> bool,
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

    // Tiers are unioned into a fragment, so any contiguous same-tier window qualifies.
    if let Some(window) = select_window(&manifest.deltas, policy.width, policy, |rel| {
        (!is_build(rel)).then(|| size_of(rel))
    }) {
        plan.tiers = manifest.deltas[window].to_vec();
    }

    // Runs are driven from the locator extents, which name their run. The coalesced extent must
    // cover one ascending, non-overlapping span: `external_id_of_checked` finds an extent by the
    // first span containing the entity, so an overlapping span would answer against the wrong run.
    let locator_size = |extent: &LocatorExtent| -> Option<u64> {
        extent
            .files()
            .all(|rel| !is_build(rel))
            .then(|| extent.files().map(&size_of).sum())
    };
    if let Some(window) = select_window(
        &manifest.locator_extents,
        policy.width,
        policy,
        locator_size,
    ) {
        let extents = &manifest.locator_extents[window];
        let adjacent = extents
            .windows(2)
            .all(|pair| pair[0].entity_hi < pair[1].entity_lo);
        // The runs must be a contiguous block of `external_id_runs` in the same order: the
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

    // Dictionary extents are positional: an ordinal is an index into the concatenation in listed
    // order. Position 0 is always the build's base dictionary and stays untakeable; the window
    // over later entries must be contiguous and land in place, or every ordinal after it shifts.
    if let Some((_base, promoted)) = manifest.dict_extents.split_first() {
        if let Some(window) =
            select_window(promoted, policy.width, policy, |extent: &DictExtent| {
                Some(size_of(&extent.path))
            })
        {
            plan.dicts = manifest.dict_extents[window.start + 1..window.end + 1].to_vec();
        }
    }

    // Attribute extents: one window per column, keyed by `(column, view, incarnation)` so a
    // group-scoped family's views are not merged into each other. A layer's dictionary counts
    // toward the input cap along with its values.
    plan.attrs = column_windows(&manifest.attr_extents, policy, is_live, |extent| {
        Some(extent.files().map(&size_of).sum())
    });

    // Record-blob extents: the attribute axis's selection over the record blob's one
    // pseudo-column. A built bundle's list is empty; the base blob lives in `MANIFEST.files`.
    {
        let size = |extent: &RecordExtent| Some(extent.files().map(&size_of).sum());
        if let Some(window) = widest_window(&manifest.record_extents, policy, size) {
            plan.records = manifest.record_extents[window].to_vec();
        }
    }

    // Text extents: the attribute axis's per-column selection over their own list. A `TextExtent`
    // names its dictionary, postings and presence together, so all three files count toward the
    // input cap: the merge holds every input's postings and streams both dictionaries at once.
    plan.texts = column_windows(&manifest.text_extents, policy, is_live, |extent| {
        Some(extent.files().map(&size_of).sum())
    });

    // Entity-to-term extents: the record axis's selection over `entity_terms_extents`. A built
    // bundle's list is empty; the base layer lives under `entities/terms/` in `MANIFEST.files`.
    {
        let size = |extent: &EntityTermsExtent| Some(extent.files().map(&size_of).sum());
        if let Some(window) = widest_window(&manifest.entity_terms_extents, policy, size) {
            plan.terms = manifest.entity_terms_extents[window].to_vec();
        }
    }

    (!plan.is_empty()).then_some(plan)
}

/// The first window of `width` consecutive entries that are all eligible, share one size tier, and
/// total within `policy.max_input_bytes`. `size_of` returns `None` for an entry this axis may not
/// take, which excludes it and also breaks the window.
///
/// `width` is a parameter rather than `policy.width` because the attribute axis narrows it to fit
/// its per-column input cap; every other axis passes the policy's own.
fn select_window<T>(
    entries: &[T],
    width: usize,
    policy: CoalescePolicy,
    size_of: impl Fn(&T) -> Option<u64>,
) -> Option<std::ops::Range<usize>> {
    if width < 2 || entries.len() < width {
        return None;
    }
    for start in 0..=entries.len() - width {
        let window = &entries[start..start + width];
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
        return Some(start..start + width);
    }
    None
}

/// The widest window that fits the input cap. A run of `policy.width` entries in one size tier
/// must exist first, ignoring the cap. Of the widths from there down to 2, the widest whose
/// bytes fit the cap is taken, so a list with too few entries yields no window.
fn widest_window<T>(
    entries: &[T],
    policy: CoalescePolicy,
    size_of: impl Fn(&T) -> Option<u64>,
) -> Option<std::ops::Range<usize>> {
    let uncapped = CoalescePolicy {
        max_input_bytes: u64::MAX,
        ..policy
    };
    select_window(entries, policy.width, uncapped, &size_of).and_then(|_| {
        (2..=policy.width)
            .rev()
            .find_map(|width| select_window(entries, width, policy, &size_of))
    })
}

/// One window per live `(column, view, incarnation)`, taken over that key's own subsequence of
/// `entries`. Extents of a dropped view's incarnation are skipped: the output path is built
/// from `(column, view)`, so a dead window would write over the live one's files. An entry
/// with a view and no incarnation, or the reverse, is skipped too.
fn column_windows<E: ColumnExtent + Clone>(
    entries: &[E],
    policy: CoalescePolicy,
    is_live: &dyn Fn(&str, tessera_types::view::ViewIncarnation) -> bool,
    size_of: impl Fn(&E) -> Option<u64>,
) -> Vec<ColumnWindow<E>> {
    let mut by_column: BTreeMap<WindowKey<'_>, Vec<&E>> = BTreeMap::new();
    for extent in entries {
        by_column.entry(extent.key()).or_default().push(extent);
    }
    let mut windows = Vec::new();
    for ((column, view, incarnation), extents) in by_column {
        let live = match (view, incarnation) {
            // Entity-scoped: one column bundle-wide, belonging to no view.
            (None, None) => true,
            (Some(view), Some(incarnation)) => is_live(view, incarnation),
            _ => false,
        };
        if !live {
            continue;
        }
        if let Some(window) = widest_window(&extents, policy, |extent: &&E| size_of(extent)) {
            windows.push(ColumnWindow {
                column: column.to_string(),
                view: view.map(str::to_string),
                incarnation,
                extents: extents[window].iter().map(|e| (*e).clone()).collect(),
            });
        }
    }
    windows
}

/// Everything [`execute_coalesce`] needs beyond its plan, taken from the generation on the
/// executor thread and then immutable.
pub(crate) struct CoalesceContext {
    pub(crate) prefix_dir: PathBuf,
    pub(crate) prefix: String,
    /// The directory every output of this pass is written into, prefix-relative. Never reused, so
    /// two passes never write the same paths and truncate each other's mapped files.
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
    /// One coalesced extent per window the attribute axis took, already opened, so publication is
    /// a pointer push on the executor and cannot fail on IO after the manifest edit.
    pub(crate) attrs: Vec<crate::filter::OpenedExtent>,
    /// The record window collapsed into one extent, or `None` if the axis did not run. The
    /// entry only; the live stack is re-derived from the manifest at publication.
    pub(crate) record: Option<RecordExtent>,
    /// One coalesced extent per window the text axis took: the entry only.
    pub(crate) texts: Vec<TextExtent>,
    /// The entity-to-term window collapsed into one extent, or `None` if the axis did not run.
    pub(crate) terms: Option<EntityTermsExtent>,
    /// Every file this pass wrote, prefix-relative, with its digest.
    pub(crate) files: BTreeMap<String, FileDigest>,
}

/// Turn a plan into durable files. Runs on the background pool, over immutable inputs.
pub(crate) fn execute_coalesce(
    plan: CoalescePlan,
    ctx: CoalesceContext,
) -> Result<CompletedCoalesce, MaintenanceFailed> {
    let out_dir = ctx.prefix_dir.join(&ctx.out_rel);
    std::fs::create_dir_all(&out_dir).map_err(|e| MaintenanceFailed(format!("coalesce dir: {e}")))?;
    let mut files: BTreeMap<String, FileDigest> = BTreeMap::new();

    let tier = coalesce_tiers(&plan, &ctx, &out_dir, &mut files)?;
    let run = coalesce_runs(&plan, &ctx, &out_dir, &mut files)?;
    let dict = coalesce_dicts(&plan, &ctx, &out_dir, &mut files)?;

    let mut attrs = Vec::with_capacity(plan.attrs.len());
    for window in &plan.attrs {
        attrs.push(coalesce_attr_window(window, &ctx, &mut files)?);
    }

    let record = coalesce_records(&plan, &ctx, &mut files)?;

    let mut texts = Vec::with_capacity(plan.texts.len());
    for window in &plan.texts {
        texts.push(coalesce_text_window(window, &ctx, &mut files)?);
    }

    let terms = coalesce_entity_terms(&plan, &ctx, &mut files)?;

    Ok(CompletedCoalesce {
        plan,
        prefix: ctx.prefix,
        tier,
        run,
        dict,
        attrs,
        record,
        texts,
        terms,
        files,
    })
}

/// The consumed delta tiers unioned into one fragment, with the reopened reader.
fn coalesce_tiers(
    plan: &CoalescePlan,
    ctx: &CoalesceContext,
    out_dir: &Path,
    files: &mut BTreeMap<String, FileDigest>,
) -> Result<Option<(String, Arc<DeltaTier>)>, MaintenanceFailed> {
    if plan.tiers.is_empty() {
        return Ok(None);
    }
    let rel = |name: &str| format!("{}/{name}", ctx.out_rel);
    let inputs: Vec<PathBuf> = plan.tiers.iter().map(|p| ctx.prefix_dir.join(p)).collect();
    let path = out_dir.join("delta.arrow");
    coalesce_delta_tiers(&inputs, &path, SMALL_TERM_THRESHOLD)
        .map_err(|e| MaintenanceFailed(format!("delta tiers: {e}")))?;
    files.insert(rel("delta.arrow"), digest_of(&path)?);
    let reader =
        DeltaTier::open(&path).map_err(|e| MaintenanceFailed(format!("coalesced tier: {e}")))?;
    Ok(Some((rel("delta.arrow"), Arc::new(reader))))
}

/// The consumed external-id runs merged into one run, with the locator extent indexing it.
fn coalesce_runs(
    plan: &CoalescePlan,
    ctx: &CoalesceContext,
    out_dir: &Path,
    files: &mut BTreeMap<String, FileDigest>,
) -> Result<Option<(String, LocatorExtent)>, MaintenanceFailed> {
    if plan.runs.is_empty() {
        return Ok(None);
    }
    let rel = |name: &str| format!("{}/{name}", ctx.out_rel);
    let inputs: Vec<PathBuf> = plan.runs.iter().map(|p| ctx.prefix_dir.join(p)).collect();
    // The union of the consumed extents' spans: the planner has already checked they are one
    // ascending, non-overlapping sequence, so this is a single span with the same coverage.
    let entity_lo = plan.locators[0].entity_lo;
    let entity_hi = plan.locators[plan.locators.len() - 1].entity_hi;
    coalesce_external_id_runs(&inputs, entity_lo, entity_hi, out_dir)
        .map_err(|e| MaintenanceFailed(format!("external-id runs: {e}")))?;
    files.insert(
        rel("external-ids.arrow"),
        digest_of(&out_dir.join("external-ids.arrow"))?,
    );
    files.insert(
        rel("ext-locator.u32"),
        digest_of(&out_dir.join("ext-locator.u32"))?,
    );
    Ok(Some((
        rel("external-ids.arrow"),
        LocatorExtent {
            path: rel("ext-locator.u32"),
            entity_lo,
            entity_hi,
            external_id_run: rel("external-ids.arrow"),
        },
    )))
}

/// The consumed dictionary extents merged into one, its record count checked against theirs.
fn coalesce_dicts(
    plan: &CoalescePlan,
    ctx: &CoalesceContext,
    out_dir: &Path,
    files: &mut BTreeMap<String, FileDigest>,
) -> Result<Option<DictExtent>, MaintenanceFailed> {
    if plan.dicts.is_empty() {
        return Ok(None);
    }
    let rel = |name: &str| format!("{}/{name}", ctx.out_rel);
    let inputs: Vec<PathBuf> = plan
        .dicts
        .iter()
        .map(|e| ctx.prefix_dir.join(&e.path))
        .collect();
    let path = out_dir.join("terms-0.dict");
    let records = coalesce_dict_extents(&inputs, &path)
        .map_err(|e| MaintenanceFailed(format!("dictionary extents: {e}")))?;
    // The record count is checked against the inputs' declared counts, which differ only if
    // an input extent repeated a descriptor. That would renumber every ordinal after it, so
    // the pass fails here instead of publishing it.
    let declared: u64 = plan.dicts.iter().map(|e| e.records).sum();
    if records != declared {
        return Err(MaintenanceFailed(format!(
            "the coalesced dictionary extent holds {records} records where its inputs declare \
             {declared}; an input repeated a descriptor, and coalescing it \
             would renumber every ordinal after the repeat"
        )));
    }
    files.insert(rel("terms-0.dict"), digest_of(&path)?);
    Ok(Some(DictExtent {
        path: rel("terms-0.dict"),
        records,
    }))
}

/// Attribute extents: one merged extent per window, under `<out>/attrs/<column>/`.
fn coalesce_attr_window(
    window: &ColumnWindow<AttrExtent>,
    ctx: &CoalesceContext,
    files: &mut BTreeMap<String, FileDigest>,
) -> Result<crate::filter::OpenedExtent, MaintenanceFailed> {
    // Which merge runs is decided by whether the window's extents all name a dictionary or
    // all do not: a keyword layer's values are ordinals into its dictionary, and any other
    // family's values are the values themselves, so a window that mixes the two has no single
    // reading.
    let with_dict = window
        .extents
        .iter()
        .filter(|extent| extent.dict.is_some())
        .count();
    let keyword = match with_dict {
        0 => false,
        n if n == window.extents.len() => true,
        _ => {
            return Err(MaintenanceFailed(format!(
                "column '{}' has {with_dict} layers with their own dictionaries and {} \
                 without; a keyword layer's values are ordinals and another family's are \
                 values, so the window has no single reading and neither merge takes it",
                window.column,
                window.extents.len() - with_dict
            )));
        }
    };
    // Per `(column, view)`, not per column: two views of one scoped family share the
    // column's name, so a single directory would have the second window truncate the first's
    // mapped files.
    let column_rel = coalesced_column_rel(&ctx.out_rel, &window.column, window.view.as_deref());
    let column_dir = ctx.prefix_dir.join(&column_rel);
    std::fs::create_dir_all(&column_dir)
        .map_err(|e| MaintenanceFailed(format!("coalesce dir for '{}': {e}", window.column)))?;
    let inputs: Vec<tessera_filter::ValueColumn> = window
        .extents
        .iter()
        .map(|extent| {
            tessera_filter::open_extent(
                &ctx.prefix_dir.join(&extent.values),
                &ctx.prefix_dir.join(&extent.presence),
                tessera_filter::Access::Mapped,
            )
        })
        .collect::<std::io::Result<_>>()
        .map_err(|e| MaintenanceFailed(format!("attr extent for '{}': {e}", window.column)))?;

    let values_rel = format!("{column_rel}/{}", tessera_filter::VALUES_FILE);
    let presence_rel = format!("{column_rel}/{}", tessera_filter::PRESENCE_FILE);
    let values_path = ctx.prefix_dir.join(&values_rel);
    let presence_path = ctx.prefix_dir.join(&presence_rel);
    let mut dict_rel = None;
    if keyword {
        // Each input's dictionary beside its values, in the same order the manifest entry
        // states. Opened sequentially: the merge's cursors walk each file once in ordinal
        // order.
        let dicts: Vec<tessera_filter::SortedDict> = window
            .extents
            .iter()
            .map(|extent| {
                let rel = extent
                    .dict
                    .as_deref()
                    .expect("counted above: every extent of a keyword window names one");
                tessera_filter::SortedDict::open(
                    &ctx.prefix_dir.join(rel),
                    tessera_filter::Access::MappedSequential,
                )
            })
            .collect::<Result<_, _>>()
            .map_err(|e: tessera_filter::DictError| {
                MaintenanceFailed(format!("keyword dictionary for '{}': {e}", window.column))
            })?;
        let layers: Vec<tessera_filter_write::KeywordLayer<'_>> = inputs
            .iter()
            .zip(dicts.iter())
            .map(|(values, dict)| tessera_filter_write::KeywordLayer { values, dict })
            .collect();
        let rel = format!("{column_rel}/{}", tessera_filter::DICT_FILE);
        let dict_path = ctx.prefix_dir.join(&rel);
        // The merge checks its remap against the written dictionary before writing an
        // ordinal, so a wrong remap fails the pass here rather than publishing it.
        tessera_filter_write::coalesce_keyword_extents(
            &layers,
            &values_path,
            &presence_path,
            &dict_path,
        )
        .map_err(|e| MaintenanceFailed(format!("keyword coalesce for '{}': {e}", window.column)))?;
        files.insert(rel.clone(), digest_of(&dict_path)?);
        dict_rel = Some(rel);
    } else {
        let refs: Vec<&tessera_filter::ValueColumn> = inputs.iter().collect();
        tessera_filter_write::coalesce_attr_extents(&refs, &values_path, &presence_path)
            .map_err(|e| MaintenanceFailed(format!("attr coalesce for '{}': {e}", window.column)))?;
    }
    files.insert(values_rel.clone(), digest_of(&values_path)?);
    files.insert(presence_rel.clone(), digest_of(&presence_path)?);
    // Reopened here, on the pool, so publication on the executor is a pointer push. The
    // manifest entry below names the same paths this pair was read from.
    let values =
        tessera_filter::open_extent(&values_path, &presence_path, tessera_filter::Access::Mapped)
            .map_err(|e| MaintenanceFailed(format!("coalesced attr extent: {e}")))?;
    let dict = dict_rel
        .as_ref()
        .map(|rel| {
            tessera_filter::SortedDict::open(
                &ctx.prefix_dir.join(rel),
                tessera_filter::Access::Mapped,
            )
            .map(Arc::new)
        })
        .transpose()
        .map_err(|e| MaintenanceFailed(format!("the coalesced dictionary does not reopen: {e}")))?;
    Ok(crate::filter::OpenedExtent {
        extent: AttrExtent {
            column: window.column.clone(),
            view: window.view.clone(),
            incarnation: window.incarnation,
            values: values_rel,
            presence: presence_rel,
            dict: dict_rel,
            postings: None,
            offsets: None,
        },
        values: Arc::new(values),
        dict,
    })
}

/// Record-blob extents: the window merged by concatenation, re-blocking toward the format's
/// target block size. The merge retires nothing; there is no tombstone parameter to pass.
fn coalesce_records(
    plan: &CoalescePlan,
    ctx: &CoalesceContext,
    files: &mut BTreeMap<String, FileDigest>,
) -> Result<Option<RecordExtent>, MaintenanceFailed> {
    if plan.records.is_empty() {
        return Ok(None);
    }
    let record_rel = format!("{}/attrs/record", ctx.out_rel);
    let record_dir = ctx.prefix_dir.join(&record_rel);
    std::fs::create_dir_all(&record_dir)
        .map_err(|e| MaintenanceFailed(format!("coalesce dir for the record blob: {e}")))?;
    let inputs: Vec<tessera_filter::RecordBlob> = plan
        .records
        .iter()
        .map(|extent| {
            tessera_filter::RecordBlob::open(
                &ctx.prefix_dir.join(&extent.blocks),
                &ctx.prefix_dir.join(&extent.hasrow),
                &ctx.prefix_dir.join(&extent.directory),
                tessera_filter::Access::Mapped,
            )
        })
        .collect::<Result<_, _>>()
        .map_err(|e| MaintenanceFailed(format!("record extent: {e}")))?;
    let refs: Vec<&tessera_filter::RecordBlob> = inputs.iter().collect();

    let extent = RecordExtent {
        blocks: format!("{record_rel}/{}", tessera_filter::RECORD_BLOCKS_FILE),
        hasrow: format!("{record_rel}/{}", tessera_filter::RECORD_HASROW_FILE),
        directory: format!("{record_rel}/{}", tessera_filter::RECORD_DIRECTORY_FILE),
    };
    let blocks_path = ctx.prefix_dir.join(&extent.blocks);
    let hasrow_path = ctx.prefix_dir.join(&extent.hasrow);
    let directory_path = ctx.prefix_dir.join(&extent.directory);
    tessera_filter_write::coalesce_record_extents(
        &refs,
        &blocks_path,
        &hasrow_path,
        &directory_path,
        tessera_filter::RECORD_BLOCK_TARGET,
    )
    .map_err(|e| MaintenanceFailed(format!("record coalesce: {e}")))?;
    for rel in extent.files() {
        files.insert(rel.to_string(), digest_of(&ctx.prefix_dir.join(rel))?);
    }
    // Reopened before the manifest can name it: a merge defect fails the pass here rather
    // than publishing an extent the reader would refuse later.
    tessera_filter::RecordBlob::open(
        &blocks_path,
        &hasrow_path,
        &directory_path,
        tessera_filter::Access::Mapped,
    )
    .map_err(|e| MaintenanceFailed(format!("the coalesced record extent does not reopen: {e}")))?;
    Ok(Some(extent))
}

/// Text extents: the window merged into one layer, dictionary and all. Nothing per entity
/// stores a text ordinal, so nothing outside the three files needs remapping.
fn coalesce_text_window(
    window: &ColumnWindow<TextExtent>,
    ctx: &CoalesceContext,
    files: &mut BTreeMap<String, FileDigest>,
) -> Result<TextExtent, MaintenanceFailed> {
    let column_rel = coalesced_column_rel(&ctx.out_rel, &window.column, window.view.as_deref());
    let column_dir = ctx.prefix_dir.join(&column_rel);
    std::fs::create_dir_all(&column_dir)
        .map_err(|e| MaintenanceFailed(format!("coalesce dir for '{}': {e}", window.column)))?;

    // The dictionaries are streamed sequentially, each once. The postings are not: the merge
    // reads record `at[i]` of whichever layer holds the least key, interleaving across
    // layers rather than walking each in order.
    let dicts: Vec<tessera_filter::SortedDict> = window
        .extents
        .iter()
        .map(|extent| {
            tessera_filter::SortedDict::open(
                &ctx.prefix_dir.join(&extent.dict),
                tessera_filter::Access::MappedSequential,
            )
        })
        .collect::<Result<_, _>>()
        .map_err(|e: tessera_filter::DictError| {
            MaintenanceFailed(format!("text extent for '{}': {e}", window.column))
        })?;
    let postings: Vec<tessera_filter::ColumnPostings> = window
        .extents
        .iter()
        .map(|extent| {
            tessera_filter::ColumnPostings::open(&ctx.prefix_dir.join(&extent.postings), true)
        })
        .collect::<std::io::Result<_>>()
        .map_err(|e| MaintenanceFailed(format!("text extent for '{}': {e}", window.column)))?;
    let presences: Vec<croaring::Bitmap> = window
        .extents
        .iter()
        .map(|extent| {
            std::fs::read(ctx.prefix_dir.join(&extent.presence))
                .map(|bytes| croaring::Bitmap::deserialize::<croaring::Portable>(&bytes))
        })
        .collect::<std::io::Result<_>>()
        .map_err(|e| MaintenanceFailed(format!("text presence for '{}': {e}", window.column)))?;
    let inputs: Vec<tessera_filter_write::TextLayerRef<'_>> = dicts
        .iter()
        .zip(postings.iter())
        .zip(presences.iter())
        .map(
            |((dict, postings), present)| tessera_filter_write::TextLayerRef {
                dict,
                postings,
                present: Some(present),
            },
        )
        .collect();

    let extent = TextExtent {
        column: window.column.clone(),
        view: window.view.clone(),
        incarnation: window.incarnation,
        dict: format!("{column_rel}/{}", tessera_filter::DICT_FILE),
        postings: format!("{column_rel}/postings.arrow"),
        presence: format!("{column_rel}/presence.roaring"),
    };
    let dict_path = ctx.prefix_dir.join(&extent.dict);
    let postings_path = ctx.prefix_dir.join(&extent.postings);
    let presence_path = ctx.prefix_dir.join(&extent.presence);
    // Scratch, removed on every exit path so a left-behind spool does not accumulate.
    let spool_path = column_dir.join("postings.spool");
    let outcome = tessera_filter_write::coalesce_text_extents(
        &inputs,
        &dict_path,
        &postings_path,
        &presence_path,
        &spool_path,
    );
    let _ = std::fs::remove_file(&spool_path);
    outcome.map_err(|e| MaintenanceFailed(format!("text coalesce for '{}': {e}", window.column)))?;
    drop(inputs);
    drop(postings);
    drop(dicts);

    for rel in extent.files() {
        files.insert(rel.to_string(), digest_of(&ctx.prefix_dir.join(rel))?);
    }
    // Reopened before the manifest can name it: the two halves are checked against each
    // other here, so a merge defect fails the pass rather than publishing a layer whose
    // ordinals name the wrong words.
    let reopened_dict = tessera_filter::SortedDict::open(&dict_path, tessera_filter::Access::Read)
        .map_err(|e| MaintenanceFailed(format!("the coalesced text extent does not reopen: {e}")))?;
    let reopened_postings = tessera_filter::ColumnPostings::open(&postings_path, false)
        .map_err(|e| MaintenanceFailed(format!("the coalesced text extent does not reopen: {e}")))?;
    if reopened_dict.len() != reopened_postings.record_count() {
        return Err(MaintenanceFailed(format!(
            "the coalesced text extent for '{}' holds {} terms and {} postings records",
            window.column,
            reopened_dict.len(),
            reopened_postings.record_count()
        )));
    }
    Ok(extent)
}

/// Entity-to-term extents: the window merged by concatenation. The merge walks the inputs'
/// entity sets in ascending order and copies each list verbatim; there is no remap, because a
/// term ordinal is a dictionary position and the dictionary is append-only. It retires
/// nothing: no tombstone parameter exists to pass.
fn coalesce_entity_terms(
    plan: &CoalescePlan,
    ctx: &CoalesceContext,
    files: &mut BTreeMap<String, FileDigest>,
) -> Result<Option<EntityTermsExtent>, MaintenanceFailed> {
    if plan.terms.is_empty() {
        return Ok(None);
    }
    let terms_rel = format!("{}/entities/terms", ctx.out_rel);
    let terms_dir = ctx.prefix_dir.join(&terms_rel);
    std::fs::create_dir_all(&terms_dir)
        .map_err(|e| MaintenanceFailed(format!("coalesce dir for the transpose: {e}")))?;
    let inputs: Vec<tessera_store::EntityTerms> = plan
        .terms
        .iter()
        .map(|extent| {
            tessera_store::EntityTerms::open(
                &ctx.prefix_dir.join(&extent.hasrow),
                &ctx.prefix_dir.join(&extent.offsets),
                &ctx.prefix_dir.join(&extent.terms),
                &ctx.prefix_dir.join(&extent.bases),
            )
        })
        .collect::<Result<_, _>>()
        .map_err(|e| MaintenanceFailed(format!("entity-terms extent: {e}")))?;
    let expected: u64 = inputs.iter().map(tessera_store::EntityTerms::len).sum();
    let refs: Vec<&tessera_store::EntityTerms> = inputs.iter().collect();

    let extent = EntityTermsExtent {
        hasrow: format!("{terms_rel}/{}", tessera_store::ENTITY_TERMS_HASROW_FILE),
        offsets: format!("{terms_rel}/{}", tessera_store::ENTITY_TERMS_OFFSETS_FILE),
        terms: format!("{terms_rel}/{}", tessera_store::ENTITY_TERMS_TERMS_FILE),
        bases: format!("{terms_rel}/{}", tessera_store::ENTITY_TERMS_BASES_FILE),
    };
    let written = tessera_store::coalesce_entity_terms_extents(
        &refs,
        &ctx.prefix_dir.join(&extent.hasrow),
        &ctx.prefix_dir.join(&extent.offsets),
        &ctx.prefix_dir.join(&extent.terms),
        &ctx.prefix_dir.join(&extent.bases),
    )
    .map_err(|e| MaintenanceFailed(format!("entity-terms coalesce: {e}")))?;
    // A count short of the sum means an input's has-row bitmap named an entity its offsets
    // did not: publishing that would lose a flush's worth of label sets with no symptom.
    if written != expected {
        return Err(MaintenanceFailed(format!(
            "the coalesced entity-terms extent holds {written} entities where its inputs hold \
             {expected}"
        )));
    }
    for rel in extent.files() {
        files.insert(rel.to_string(), digest_of(&ctx.prefix_dir.join(rel))?);
    }
    // Reopened before the manifest can name it, for the same reason as the other axes.
    drop(inputs);
    tessera_store::EntityTerms::open(
        &ctx.prefix_dir.join(&extent.hasrow),
        &ctx.prefix_dir.join(&extent.offsets),
        &ctx.prefix_dir.join(&extent.terms),
        &ctx.prefix_dir.join(&extent.bases),
    )
    .map_err(|e| {
        MaintenanceFailed(format!(
            "the coalesced entity-terms extent does not reopen: {e}"
        ))
    })?;
    Ok(Some(extent))
}

/// Apply `completed` to `manifest` in place, or `false` if it no longer rebases.
///
/// Every consumed entry must still be present, contiguous and in order, on every axis it
/// touched. A flush publishing while the pass ran only appends, so the ordinary case is that the
/// window is exactly where it was; otherwise the plan is stale and is discarded, leaving the
/// consumed entries standing for the next tick to re-plan.
///
/// The coalesced entry takes the window's position, never the end of the list. This preserves
/// recency on the run axis and every ordinal on the dictionary axis.
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
    // Keyed by the values path, the identity a listed attribute extent is found by.
    if plan.attrs.len() != completed.attrs.len() {
        return false;
    }
    let Some(attr_positions) =
        column_positions(&manifest.attr_extents, &plan.attrs, |e: &AttrExtent| {
            e.values.as_str()
        })
    else {
        return false;
    };

    // The record window: one contiguous run of `record_extents`, keyed by the blocks path.
    let record_paths: Vec<String> = plan.records.iter().map(|e| e.blocks.clone()).collect();
    let records = match window_of(&manifest.record_extents, &record_paths, |e| &e.blocks) {
        Some(at) => at,
        None => return false,
    };

    // The transpose window: one contiguous run of `entity_terms_extents`, keyed by the terms path.
    let terms_paths: Vec<String> = plan.terms.iter().map(|e| e.terms.clone()).collect();
    let terms = match window_of(&manifest.entity_terms_extents, &terms_paths, |e| &e.terms) {
        Some(at) => at,
        None => return false,
    };

    // The text axis, keyed by the dictionary path, the identity a text layer is named by.
    if plan.texts.len() != completed.texts.len() {
        return false;
    }
    let Some(text_positions) =
        column_positions(&manifest.text_extents, &plan.texts, |e: &TextExtent| {
            e.dict.as_str()
        })
    else {
        return false;
    };

    // Every file a consumed extent names, its dictionary included, so nothing digested here is
    // left for a layer no list names.
    let attr_paths: Vec<String> = plan
        .attrs
        .iter()
        .flat_map(|w| w.extents.iter())
        .flat_map(|e| e.files().map(String::from))
        .collect();
    let text_paths: Vec<String> = plan
        .texts
        .iter()
        .flat_map(|w| w.extents.iter())
        .flat_map(|e| e.files().map(String::from))
        .collect();
    let record_files: Vec<String> = plan
        .records
        .iter()
        .flat_map(|e| e.files().map(String::from))
        .collect();
    let terms_files: Vec<String> = plan
        .terms
        .iter()
        .flat_map(|e| e.files().map(String::from))
        .collect();
    for rel in plan
        .tiers
        .iter()
        .chain(&plan.runs)
        .chain(&locator_paths)
        .chain(&dict_paths)
        .chain(&attr_paths)
        .chain(&record_files)
        .chain(&terms_files)
        .chain(&text_paths)
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
    if let Some(extent) = &completed.record {
        // The window's position: nothing reads `record_extents` by position, but the manifest's
        // bytes must not depend on when the pass ran.
        manifest.record_extents.splice(records, [extent.clone()]);
    }
    if let Some(extent) = &completed.terms {
        manifest
            .entity_terms_extents
            .splice(terms, [extent.clone()]);
    }
    if !completed.attrs.is_empty() {
        // Both edits (the files above and this list) must land together: a bundle whose
        // `attr_extents` lost a window whose bytes were written answers filters missing every
        // entity that window held, with no symptom.
        let coalesced: Vec<&AttrExtent> = completed.attrs.iter().map(|a| &a.extent).collect();
        splice_columns(&mut manifest.attr_extents, &attr_positions, &coalesced);
    }
    if !completed.texts.is_empty() {
        let coalesced: Vec<&TextExtent> = completed.texts.iter().collect();
        splice_columns(&mut manifest.text_extents, &text_positions, &coalesced);
    }
    true
}

/// Where `needle` sits in `haystack`, as a contiguous run of equal keys, or the empty range at 0
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

/// Where each window's consumed extents sit in `entries`, as positions in the whole list, or
/// `None` if one of them no longer does. The window is contiguous within its own
/// `(column, view, incarnation)` subsequence, not within the whole list, because the list
/// interleaves the columns a flush publishes for. `identity` names the file a listed extent is
/// recognised by.
fn column_positions<E: ColumnExtent>(
    entries: &[E],
    windows: &[ColumnWindow<E>],
    identity: impl Fn(&E) -> &str,
) -> Option<Vec<Vec<usize>>> {
    let mut positions = Vec::with_capacity(windows.len());
    for window in windows {
        let key = (
            window.column.as_str(),
            window.view.as_deref(),
            window.incarnation,
        );
        let subsequence: Vec<usize> = entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.key() == key)
            .map(|(i, _)| i)
            .collect();
        let listed: Vec<&str> = subsequence.iter().map(|i| identity(&entries[*i])).collect();
        let consumed: Vec<&str> = window.extents.iter().map(&identity).collect();
        let at = window_of(&listed, &consumed, |s| s)?;
        positions.push(subsequence[at].to_vec());
    }
    Some(positions)
}

/// Drops the consumed positions from `entries` and puts each window's coalesced extent where
/// its window began, so the manifest's bytes do not depend on when the pass ran.
fn splice_columns<E: Clone>(entries: &mut Vec<E>, positions: &[Vec<usize>], coalesced: &[&E]) {
    let removed: BTreeSet<usize> = positions.iter().flatten().copied().collect();
    let inserts: BTreeMap<usize, &E> = positions
        .iter()
        .zip(coalesced)
        .map(|(at, extent)| (at[0], *extent))
        .collect();
    let mut next = Vec::with_capacity(entries.len());
    for (i, extent) in entries.iter().enumerate() {
        if let Some(coalesced) = inserts.get(&i) {
            next.push((*coalesced).clone());
        }
        if !removed.contains(&i) {
            next.push(extent.clone());
        }
    }
    *entries = next;
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
            dict_extents: vec![DictExtent {
                path: "terms/terms-0.dict".to_string(),
                records: 4,
            }],
            external_id_runs: vec!["entities/external-ids-0.arrow".to_string()],
            ..SegmentsManifest::empty()
        };
        for i in 0..flushes {
            let seg = format!("partitions/{PARTITION}/views/s0/segments/flush-{i}-1");
            for name in [
                "delta.arrow",
                "external-ids.arrow",
                "ext-locator.u32",
                "terms-0.dict",
            ] {
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
            // Two filterable columns, both extended by every flush — so the list interleaves them
            // exactly as a flush leaves it, and a selection that read the list rather than a
            // column's own subsequence would take one of each.
            for column in COLUMNS {
                let extent = attr_extent_at(PARTITION, column, &format!("flush-{i}-1"));
                manifest.files.insert(extent.values.clone(), digest(1024));
                manifest.files.insert(extent.presence.clone(), digest(64));
                manifest.attr_extents.push(extent);
            }
        }
        (manifest, build_files)
    }

    /// The fixture roster: every view of every extent here is live at the build's incarnation, so
    /// the liveness filter is a no-op and each test is about the axis it names. The one test that
    /// is about the filter supplies its own.
    fn all_live(_view: &str, _incarnation: tessera_types::view::ViewIncarnation) -> bool {
        true
    }

    /// The two filterable columns every fixture manifest carries extents for.
    const COLUMNS: [&str; 2] = ["title", "department"];

    /// One flush's extent for one **view's** column of a group-scoped family (`views.md` §5).
    fn scoped_extent_at(partition: &str, column: &str, view: &str, flush: &str) -> AttrExtent {
        let (group, key) = view.split_once(':').expect("a view of a group");
        let dir = format!("partitions/{partition}/attrs/{column}/{group}/{key}/extents");
        AttrExtent {
            // Present exactly when `view` is (decision 0115); the fixture's views are the build's.
            incarnation: Some(tessera_store::manifest::DECLARED_INCARNATION),
            column: column.to_string(),
            view: Some(view.to_string()),
            values: format!("{dir}/{flush}.arrow"),
            presence: format!("{dir}/{flush}.roaring"),
            dict: None,
            postings: None,
            offsets: None,
        }
    }

    /// One flush's extent for one view's column at a **named** incarnation, so a test can put two
    /// incarnations of one key in the list (decision 0115).
    fn scoped_extent_of(
        partition: &str,
        column: &str,
        view: &str,
        incarnation: tessera_types::view::ViewIncarnation,
        flush: &str,
    ) -> AttrExtent {
        let mut extent = scoped_extent_at(partition, column, view, flush);
        extent.incarnation = Some(incarnation);
        extent
    }

    /// **A dead incarnation's extents are not coalesced** (decision 0115).
    ///
    /// The hazard is not wasted work. `coalesced_column_rel` derives the output path from
    /// `(column, view)` and nothing else, so a dead incarnation's window and the live one's
    /// resolve to the *same* files — two merges, one path, each truncating the other's mapped
    /// output. The live view would then serve whichever landed last, under a digest describing
    /// neither. Skipping the dead window is what makes the path collision unreachable, and it is
    /// also correct on its own terms: those files are the fold's to reclaim.
    ///
    /// **Mutation this kills:** drop the `live_window` guard in `plan_coalesce` and the plan
    /// carries two windows for one view id.
    #[test]
    fn a_dead_incarnations_window_is_not_planned() {
        let (mut manifest, build_files) = manifest_with(0);
        let view = "quarter:2026-Q1";
        // The key was dropped at incarnation 0 and created again at 4. Both incarnations' extents
        // are in the list, because the fold that reclaims the first has not run.
        for i in 0..3 {
            for incarnation in [0, 4] {
                let extent = scoped_extent_of(
                    PARTITION,
                    "mood",
                    view,
                    incarnation,
                    &format!("flush-{i}-{incarnation}"),
                );
                manifest.files.insert(extent.values.clone(), digest(1024));
                manifest.files.insert(extent.presence.clone(), digest(64));
                manifest.attr_extents.push(extent);
            }
        }
        let dead_extents: Vec<AttrExtent> = manifest
            .attr_extents
            .iter()
            .filter(|e| e.incarnation == Some(0))
            .cloned()
            .collect();
        assert_eq!(dead_extents.len(), 3, "the fixture holds the dead ones too");

        let plan = plan_coalesce(
            PARTITION,
            &manifest,
            &build_files,
            policy(),
            // Incarnation 4 is what the roster says this key is now.
            &|v: &str, incarnation| v == view && incarnation == 4,
        )
        .expect("the live incarnation's window still qualifies");
        assert_eq!(
            plan.attrs.len(),
            1,
            "one window, and it is the live incarnation's — two would write one path twice"
        );
        let window = &plan.attrs[0];
        assert_eq!(window.view.as_deref(), Some(view));
        assert_eq!(window.incarnation, Some(4));
        assert!(
            window.extents.iter().all(|e| e.incarnation == Some(4)),
            "no dead extent is inside the live window either"
        );
        // And the dead extents are left exactly as they were: the plan consumes none of them, so
        // the fold still finds them to omit.
        let consumed: BTreeSet<&str> = plan
            .attrs
            .iter()
            .flat_map(|w| w.extents.iter())
            .map(|e| e.values.as_str())
            .collect();
        assert!(
            dead_extents
                .iter()
                .all(|e| !consumed.contains(e.values.as_str())),
            "the dead incarnation's files are untouched"
        );
    }

    /// **A scoped family's window is its `(column, view)`'s, not its column's** (`views.md` §5).
    ///
    /// A family has one column per view of its group and they share the column's *name*, so a
    /// selection keyed on the name alone would put two views' extents in one window — and the
    /// merge would then write one file claiming both views' entities, under one view's directory.
    /// Every answer either view gave afterwards would be a plausible wrong one, which is why this
    /// is asserted on the plan rather than left to the pass.
    #[test]
    fn a_scoped_familys_window_is_one_views_own() {
        let (mut manifest, build_files) = manifest_with(0);
        // Three extents per view, interleaved exactly as two views flushing in turn leave them, so
        // a selection reading the list rather than each column's own subsequence would take one of
        // each.
        for i in 0..3 {
            for view in ["quarter:2026-Q1", "quarter:2026-Q3"] {
                let extent = scoped_extent_at(PARTITION, "mood", view, &format!("flush-{i}-1"));
                manifest.files.insert(extent.values.clone(), digest(1024));
                manifest.files.insert(extent.presence.clone(), digest(64));
                manifest.attr_extents.push(extent);
            }
        }
        let plan =
            plan_coalesce(PARTITION, &manifest, &build_files, policy(), &all_live).expect("a plan");
        assert_eq!(
            plan.attrs.len(),
            2,
            "one window per view, not one per column"
        );
        for window in &plan.attrs {
            assert_eq!(window.column, "mood");
            let view = window
                .view
                .as_deref()
                .expect("a scoped window names its view");
            assert!(
                window
                    .extents
                    .iter()
                    .all(|e| e.view.as_deref() == Some(view)),
                "{view}'s window holds only {view}'s extents"
            );
            let (group, key) = view.split_once(':').unwrap();
            assert!(
                window
                    .extents
                    .iter()
                    .all(|e| e.values.contains(&format!("/{group}/{key}/"))),
                "{view}'s extents live under its own directory"
            );
        }
        let views: BTreeSet<&str> = plan
            .attrs
            .iter()
            .filter_map(|w| w.view.as_deref())
            .collect();
        assert_eq!(
            views,
            BTreeSet::from(["quarter:2026-Q1", "quarter:2026-Q3"]),
            "both views' columns are taken"
        );
    }

    /// **And an entity-scoped column's window is still keyed on the column alone**, which is what
    /// makes the pair above the identity rather than the view: a bundle with no family at all
    /// plans exactly what it planned before the field existed.
    #[test]
    fn an_entity_scoped_window_names_no_view() {
        let (manifest, build_files) = manifest_with(3);
        let plan =
            plan_coalesce(PARTITION, &manifest, &build_files, policy(), &all_live).expect("a plan");
        assert!(!plan.attrs.is_empty(), "the fixture's own columns qualify");
        assert!(
            plan.attrs.iter().all(|w| w.view.is_none()),
            "a declared column belongs to no view"
        );
    }

    fn attr_extent_at(partition: &str, column: &str, flush: &str) -> AttrExtent {
        let dir = format!("partitions/{partition}/attrs/{column}/extents");
        AttrExtent {
            incarnation: None,
            column: column.to_string(),
            view: None,
            values: format!("{dir}/{flush}.arrow"),
            presence: format!("{dir}/{flush}.roaring"),
            dict: None,
            postings: None,
            offsets: None,
        }
    }

    /// A completed pass carrying one coalesced extent per planned window, with an opened column
    /// standing in for the merged one. The reader is real — an empty extent is still a column —
    /// because `CompletedCoalesce` carries the opened reader and a double there would be a second
    /// definition of what an extent is.
    fn completed_attrs(plan: &CoalescePlan, out_rel: &str) -> Vec<crate::filter::OpenedExtent> {
        plan.attrs
            .iter()
            .map(|window| {
                let column_rel =
                    coalesced_column_rel(out_rel, &window.column, window.view.as_deref());
                crate::filter::OpenedExtent {
                    extent: AttrExtent {
                        incarnation: window.incarnation,
                        column: window.column.clone(),
                        view: window.view.clone(),
                        values: format!("{column_rel}/values.arrow"),
                        presence: format!("{column_rel}/presence.roaring"),
                        dict: None,
                        postings: None,
                        offsets: None,
                    },
                    values: Arc::new(
                        tessera_filter::ValueColumn::partial(
                            tessera_filter::Codes::U32(Vec::<u32>::new().into()),
                            croaring::Bitmap::new(),
                        )
                        .expect("an empty extent"),
                    ),
                    dict: None,
                }
            })
            .collect()
    }

    /// **The build's own artefacts are never taken**, on any axis. Rewriting a file
    /// `MANIFEST.json` digests means writing a new prefix — compaction under another name — and the
    /// base locator's ordinals are positions in the build's runs, so consuming one renumbers the
    /// whole reverse direction for every entity the build knew about.
    ///
    /// **Mutation:** drop the run axis's `is_build` guard and the plan takes run 0; drop the
    /// dictionary axis's skip of its first entry and the plan takes dict extent 0.
    #[test]
    fn the_builds_own_run_and_dictionary_extent_are_never_selected() {
        let (manifest, build_files) = manifest_with(3);
        let plan =
            plan_coalesce(PARTITION, &manifest, &build_files, policy(), &all_live).expect("a plan");
        assert!(
            !plan
                .runs
                .contains(&"entities/external-ids-0.arrow".to_string()),
            "the build's run: {:?}",
            plan.runs
        );
        assert!(
            !plan.dicts.iter().any(|e| e.path == "terms/terms-0.dict"),
            "the build's dictionary extent: {:?}",
            plan.dicts
        );
    }

    /// **A fold's carry-forward must not freeze the dictionary axis.** A fold digest-names every
    /// carried file in the new prefix's `MANIFEST.json` (durability for the hard links,
    /// compaction §4) and carries `dict_extents` forward verbatim — the one guarded axis it does
    /// not rebuild. Judging eligibility by that digest home froze every carried extent, so the
    /// axis ratcheted linearly in the fold count — the endurance tier measured 6 → 58 across 24
    /// fold cycles, against write-path §7's claim that the coalesce bounds it. Eligibility is
    /// positional instead: the base dictionary — always first — is never taken, and every later
    /// extent stays takeable whichever files map digests it.
    ///
    /// **Mutation:** restore the `is_build` test on the dictionary axis and this plans no
    /// dictionary window; admit the first entry and the window starts at the base.
    #[test]
    fn a_folds_carried_dictionary_extents_are_still_selected() {
        let (mut manifest, mut build_files) = manifest_with(3);
        // A fold's publication: every carried file's digest moves to the new prefix's
        // `MANIFEST.json` and the side-manifest's own files map starts empty — every digest a
        // fold publishes goes in `MANIFEST.json` (compaction §4).
        build_files.extend(std::mem::take(&mut manifest.files));

        let plan =
            plan_coalesce(PARTITION, &manifest, &build_files, policy(), &all_live).expect("a plan");
        let dicts: Vec<&str> = plan.dicts.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            dicts,
            [
                format!("partitions/{PARTITION}/views/s0/segments/flush-0-1/terms-0.dict"),
                format!("partitions/{PARTITION}/views/s0/segments/flush-1-1/terms-0.dict"),
                format!("partitions/{PARTITION}/views/s0/segments/flush-2-1/terms-0.dict"),
            ],
            "the carried extents coalesce, and the base dictionary is not among them"
        );
    }

    /// Locator extents whose spans overlap are not one span. `external_id_of_checked` finds an
    /// extent by the first span containing the entity, so a coalesced extent overlapping another
    /// would answer one entity's ordinal against another run's keys.
    #[test]
    fn overlapping_locator_spans_are_refused_on_the_run_axis() {
        let (mut manifest, build_files) = manifest_with(3);
        manifest.locator_extents[1].entity_lo = 0; // now overlaps extent 0
        let plan =
            plan_coalesce(PARTITION, &manifest, &build_files, policy(), &all_live).expect("a plan");
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
        let plan = plan_coalesce(PARTITION, &manifest, &build_files, policy(), &all_live);
        assert!(
            plan.as_ref().is_none_or(|p| p.tiers.is_empty()),
            "a 64 MiB tier and two 1 KiB ones are not one class"
        );
    }

    /// Below the width nothing is selected — the ordinary answer at all but one tick in `width`.
    #[test]
    fn nothing_is_selected_below_the_width() {
        let (manifest, build_files) = manifest_with(2);
        assert!(plan_coalesce(PARTITION, &manifest, &build_files, policy(), &all_live).is_none());
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
        let plan =
            plan_coalesce(PARTITION, &manifest, &build_files, policy, &all_live).expect("a plan");

        let dir = tempfile::TempDir::new().unwrap();
        let attrs = completed_attrs(&plan, "c");
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
            attrs,
            record: None,
            texts: Vec::new(),
            terms: None,
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

    /// **The attribute axis selects per column, over that column's own subsequence.**
    ///
    /// The selection unit is the column because that is the identity the format carries — an
    /// `AttrExtent` records no flush, and filter-index §2.5 forbids recovering one from the path.
    ///
    /// **Mutation:** select over `attr_extents` as one list and each window holds both columns'
    /// extents, which the merge then refuses as interleaved — after the pass has done its IO.
    #[test]
    fn the_attribute_axis_selects_a_window_of_each_columns_own_extents() {
        let (manifest, build_files) = manifest_with(4);
        let plan =
            plan_coalesce(PARTITION, &manifest, &build_files, policy(), &all_live).expect("a plan");
        assert_eq!(plan.attrs.len(), 2, "one window per column");
        for window in &plan.attrs {
            assert_eq!(window.extents.len(), 3, "the policy's width, per column");
            assert!(
                window.extents.iter().all(|e| e.column == window.column),
                "a window took another column's extent: {:?}",
                window.extents
            );
        }
    }

    /// **The input cap applies per column, and narrows the window rather than stalling the axis.**
    ///
    /// The cap bounds the pass transient — the window's values and presence held during the
    /// merge — so a text column whose values outgrow it must stall *itself* and never its
    /// neighbours; and where it can still take a narrower window it takes one, because reverting to
    /// unbounded file growth is the failure this axis exists to prevent.
    #[test]
    fn a_column_over_the_input_cap_narrows_its_window_and_stalls_only_itself() {
        let (mut manifest, build_files) = manifest_with(4);
        let mut policy = policy();
        policy.max_input_bytes = 5 << 20;
        // `title`'s extents are 2 MiB each: three exceed the cap, two do not. Same size tier
        // throughout, so it is the cap doing the narrowing and not the ladder.
        for extent in manifest.attr_extents.iter().filter(|e| e.column == "title") {
            manifest
                .files
                .insert(extent.values.clone(), digest(2 << 20));
        }
        let plan =
            plan_coalesce(PARTITION, &manifest, &build_files, policy, &all_live).expect("a plan");
        let window = |column: &str| {
            plan.attrs
                .iter()
                .find(|w| w.column == column)
                .map(|w| w.extents.len())
        };
        assert_eq!(window("title"), Some(2), "narrowed to what fits the cap");
        assert_eq!(window("department"), Some(3), "the neighbour is unaffected");

        // And a column one extent of which alone exceeds the cap is genuinely uncoalesceable: it
        // waits for the fold rather than being coalesced over the bound it was given.
        policy.max_input_bytes = 1 << 20;
        let plan =
            plan_coalesce(PARTITION, &manifest, &build_files, policy, &all_live).expect("a plan");
        assert!(
            !plan.attrs.iter().any(|w| w.column == "title"),
            "a column whose single extent exceeds the cap must not be selected"
        );
    }

    /// **A layer's dictionary counts toward the input cap.**
    ///
    /// The cap bounds the pass transient, and for a column whose values are ordinals the dictionary
    /// is the half that grows with distinct values rather than with entities — on a near-unique
    /// column, the larger half (records §7). Sizing the window on values and presence alone would
    /// bound the cheap term and let the expensive one through.
    ///
    /// Stated against [`select_window`] directly, with the two size functions side by side, and
    /// then against [`plan_coalesce`], whose per-column narrowing must reach the same answer.
    #[test]
    fn a_layers_dictionary_counts_toward_the_input_cap() {
        let mut sizes: BTreeMap<String, u64> = BTreeMap::new();
        let extents: Vec<AttrExtent> = (0..3)
            .map(|i| {
                let mut extent = attr_extent_at(PARTITION, "submitter", &format!("flush-{i}-1"));
                extent.dict = Some(format!(
                    "partitions/{PARTITION}/attrs/submitter/extents/flush-{i}-1.dict"
                ));
                sizes.insert(extent.values.clone(), 1 << 20);
                sizes.insert(extent.presence.clone(), 0);
                sizes.insert(extent.dict.clone().expect("a dictionary"), 1 << 20);
                extent
            })
            .collect();
        let policy = CoalescePolicy {
            width: 3,
            floor_bytes: 1 << 20,
            max_input_bytes: 4 << 20,
        };
        let counted = |extent: &AttrExtent| {
            Some(
                sizes[&extent.values]
                    + sizes[&extent.presence]
                    + extent.dict.as_ref().map_or(0, |d| sizes[d]),
            )
        };
        let values_only =
            |extent: &AttrExtent| Some(sizes[&extent.values] + sizes[&extent.presence]);
        assert_eq!(
            select_window(&extents, policy.width, policy, values_only),
            Some(0..3),
            "three 1 MiB values files fit a 4 MiB cap on their own"
        );
        assert_eq!(
            select_window(&extents, policy.width, policy, counted),
            None,
            "counting the dictionaries, the same window is 6 MiB and must not be selected"
        );

        // Through the planner: the same three extents, digested in the manifest, narrow to the
        // two that fit the cap with their dictionaries counted.
        let (mut manifest, build_files) = manifest_with(0);
        for extent in &extents {
            manifest
                .files
                .insert(extent.values.clone(), digest(1 << 20));
            manifest.files.insert(extent.presence.clone(), digest(0));
            manifest
                .files
                .insert(extent.dict.clone().expect("a dictionary"), digest(1 << 20));
            manifest.attr_extents.push(extent.clone());
        }
        let plan =
            plan_coalesce(PARTITION, &manifest, &build_files, policy, &all_live).expect("a plan");
        let window = plan
            .attrs
            .iter()
            .find(|w| w.column == "submitter")
            .expect("the keyword column is selected");
        assert_eq!(
            window.extents.len(),
            2,
            "narrowed to the two extents whose values and dictionaries fit the cap"
        );
    }

    /// **A column whose layers carry their own dictionaries is selected on the same policy as every
    /// other**, its window naming every extent's dictionary beside its values. The merge for it
    /// renumbers, and the containment is the manifest record's and the composition's (module doc);
    /// nothing at selection needs to know the family beyond counting the dictionary toward the cap.
    ///
    /// **Mutation this kills:** restore a `dict.is_some()` skip at the selection and `title` is
    /// never selected, so an indexed keyword column gains one extent per flush until the fold.
    #[test]
    fn a_column_with_per_layer_dictionaries_is_selected_like_any_other() {
        let (mut manifest, build_files) = manifest_with(4);
        for extent in manifest
            .attr_extents
            .iter_mut()
            .filter(|e| e.column == "title")
        {
            let dict = format!("{}.dict", extent.values);
            manifest.files.insert(dict.clone(), digest(64));
            extent.dict = Some(dict);
        }
        let plan =
            plan_coalesce(PARTITION, &manifest, &build_files, policy(), &all_live).expect("a plan");
        let window = plan
            .attrs
            .iter()
            .find(|w| w.column == "title")
            .expect("the keyword column's window is planned");
        assert_eq!(window.extents.len(), 3, "the policy's width");
        assert!(
            window.extents.iter().all(|e| e.dict.is_some()),
            "every extent of the window names the dictionary its ordinals are read against"
        );
        assert!(
            plan.attrs.iter().any(|w| w.column == "department"),
            "the neighbour is selected as before"
        );
    }

    /// **A coalesced keyword extent replaces its window in both halves of the manifest, dictionaries
    /// included** — and a flush of the same column landing between the plan and the rebase leaves
    /// the window where it was, with the flush's extent and its own dictionary untouched.
    ///
    /// The consumed dictionaries leave `files` with the values and presence they numbered: a
    /// dictionary left digested for a layer no list names is a file the fold's sweep reclaims and
    /// the manifest meanwhile misdescribes. The coalesced entry names its merged dictionary, and
    /// that file is digested — a keyword entry without one is a layer the reader refuses at open.
    ///
    /// **Mutation:** drop `e.dict` from the `attr_paths` chain and the consumed dictionaries stay
    /// digested; drop `dict` from the coalesced entry and `FilterColumns::open` refuses the bundle.
    #[test]
    fn a_coalesced_keyword_extent_replaces_its_window_and_its_dictionaries_in_both_halves() {
        let (mut manifest, build_files) = manifest_with(4);
        for extent in manifest
            .attr_extents
            .iter_mut()
            .filter(|e| e.column == "title")
        {
            let dict = format!("{}.dict", extent.values);
            manifest.files.insert(dict.clone(), digest(64));
            extent.dict = Some(dict);
        }
        let plan =
            plan_coalesce(PARTITION, &manifest, &build_files, policy(), &all_live).expect("a plan");
        let title = plan
            .attrs
            .iter()
            .find(|w| w.column == "title")
            .expect("the keyword window");
        let consumed: Vec<String> = title
            .extents
            .iter()
            .flat_map(|e| {
                [e.values.clone(), e.presence.clone()]
                    .into_iter()
                    .chain(e.dict.clone())
            })
            .collect();
        assert_eq!(consumed.len(), 9, "three files per consumed keyword extent");

        // The flush that landed while the pass ran: a fifth `title` extent, with its own
        // dictionary, appended after the window.
        let late = {
            let mut extent = attr_extent_at(PARTITION, "title", "flush-9-1");
            extent.dict = Some(format!("{}.dict", extent.values));
            extent
        };
        manifest.files.insert(late.values.clone(), digest(1024));
        manifest.files.insert(late.presence.clone(), digest(64));
        manifest
            .files
            .insert(late.dict.clone().expect("a dictionary"), digest(64));
        manifest.attr_extents.push(late.clone());

        let out_rel = "partitions/p0/coalesced/coalesce-1-1";
        let mut attrs = completed_attrs(&plan, out_rel);
        let merged_dict_rel = format!("{out_rel}/attrs/title/dict.bin");
        for attr in attrs.iter_mut().filter(|a| a.extent.column == "title") {
            attr.extent.dict = Some(merged_dict_rel.clone());
        }
        let files: BTreeMap<String, FileDigest> = attrs
            .iter()
            .flat_map(|a| {
                [
                    (a.extent.values.clone(), digest(3072)),
                    (a.extent.presence.clone(), digest(96)),
                ]
                .into_iter()
                .chain(a.extent.dict.clone().map(|d| (d, digest(192))))
            })
            .collect();
        let dir = tempfile::TempDir::new().unwrap();
        let completed = CompletedCoalesce {
            tier: Some(tier_at(dir.path())),
            run: None,
            dict: None,
            attrs,
            record: None,
            texts: Vec::new(),
            terms: None,
            files,
            plan,
            prefix: "v00000".to_string(),
        };
        assert!(
            rebase_into(&mut manifest, &completed),
            "a flush appending the same column's extent does not move the window"
        );

        let listed: Vec<&AttrExtent> = manifest
            .attr_extents
            .iter()
            .filter(|e| e.column == "title")
            .collect();
        assert_eq!(
            listed.len(),
            3,
            "3 became 1, 1 untouched, and the late flush's: {listed:?}"
        );
        assert_eq!(
            listed[0].dict.as_deref(),
            Some(merged_dict_rel.as_str()),
            "the coalesced entry names the merged dictionary beside its values"
        );
        assert!(
            manifest.files.contains_key(&merged_dict_rel),
            "the merged dictionary is digested"
        );
        assert_eq!(
            listed[2].dict, late.dict,
            "the late flush's extent keeps its own dictionary"
        );
        assert!(manifest.files.contains_key(late.dict.as_deref().unwrap()));
        for rel in &consumed {
            assert!(
                !manifest.files.contains_key(rel),
                "a consumed extent file is still digested: {rel}"
            );
        }
    }

    /// One flush's keyword extent of `title`, written with the flush's own writer into `prefix_dir`
    /// and listed in `manifest` with its three files digested. `keys` is one key per entity, in
    /// `entities`' order; the extent's dictionary is the sorted distinct set of them, so each
    /// extent numbers its keys its own way.
    fn write_keyword_flush(
        prefix_dir: &std::path::Path,
        manifest: &mut SegmentsManifest,
        flush: &str,
        entities: &[u32],
        keys: &[&str],
    ) -> AttrExtent {
        let mut sorted: Vec<&str> = keys.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        let codes: Vec<u32> = keys
            .iter()
            .map(|k| sorted.binary_search(k).expect("from these") as u32)
            .collect();
        let mut presence = croaring::Bitmap::new();
        for e in entities {
            presence.add(*e);
        }
        let column_dir = prefix_dir.join(format!("partitions/{PARTITION}/attrs/title"));
        let (values, presence_path, dict) = tessera_filter::write_extent(
            &column_dir,
            flush,
            &tessera_filter::Codes::U32(codes.into()),
            &presence,
            Some(&sorted),
        )
        .expect("the flush's writer writes a keyword extent");
        let rel = |path: &std::path::Path| {
            path.strip_prefix(prefix_dir)
                .expect("under the prefix")
                .to_str()
                .expect("utf-8")
                .to_string()
        };
        let extent = AttrExtent {
            incarnation: None,
            column: "title".to_string(),
            view: None,
            values: rel(&values),
            presence: rel(&presence_path),
            dict: Some(rel(&dict.expect("a keyword extent names its dictionary"))),
            postings: None,
            offsets: None,
        };
        for path in [&extent.values, &extent.presence]
            .into_iter()
            .chain(extent.dict.as_ref())
        {
            manifest.files.insert(
                path.clone(),
                tessera_store::digest_of(&prefix_dir.join(path)).unwrap(),
            );
        }
        manifest.attr_extents.push(extent.clone());
        extent
    }

    /// **A keyword window executes into one extent whose dictionary numbers its ordinals, and the
    /// completed pass carries the pair the manifest entry names** — run over real files with the
    /// flush's own writer and the merge the pass runs, rather than the stubbed reader the manifest
    /// tests use.
    ///
    /// The three inputs number their keys three different ways (`alpha` is 0 in the first and
    /// absent from the others; `gamma` is 1 in the first, 0 in the second, 1 in the third), and the
    /// merged dictionary numbers all five keys a fourth way. Every entity then reads its own key
    /// through the coalesced pair — through the opened readers the executor installs, and again
    /// through the files the rebased manifest names, which is what a restart opens.
    ///
    /// **Mutation this kills:** leave `dict` off the `OpenedExtent` or the `AttrExtent` and the
    /// entry names ordinals with nothing to read them against; run the byte-preserving merge on
    /// the window and entity 30 reads `alpha` where it carried `gamma`.
    #[test]
    fn a_keyword_window_executes_into_one_extent_whose_dictionary_numbers_its_ordinals() {
        let dir = tempfile::TempDir::new().unwrap();
        let prefix_dir = dir.path().join("v00000");
        let (mut manifest, build_files) = manifest_with(0);
        let flushes: [(&[u32], &[&str]); 3] = [
            (&[10, 11], &["gamma", "alpha"]),
            (&[20, 21], &["gamma", "delta"]),
            (&[30, 31, 32], &["gamma", "beta", "epsilon"]),
        ];
        let mut expected: BTreeMap<u32, &str> = BTreeMap::new();
        for (i, (entities, keys)) in flushes.iter().enumerate() {
            write_keyword_flush(
                &prefix_dir,
                &mut manifest,
                &format!("flush-{i}-1"),
                entities,
                keys,
            );
            expected.extend(entities.iter().copied().zip(keys.iter().copied()));
        }
        let plan =
            plan_coalesce(PARTITION, &manifest, &build_files, policy(), &all_live).expect("a plan");
        assert_eq!(plan.attrs.len(), 1, "the one keyword window");
        let out_rel = format!("partitions/{PARTITION}/coalesced/coalesce-1-1");
        let completed = execute_coalesce(
            plan,
            CoalesceContext {
                prefix_dir: prefix_dir.clone(),
                prefix: "v00000".to_string(),
                out_rel: out_rel.clone(),
            },
        )
        .expect("the keyword window merges");

        let attr = &completed.attrs[0];
        let dict_rel = attr
            .extent
            .dict
            .as_deref()
            .expect("the coalesced entry names the merged dictionary");
        assert!(dict_rel.starts_with(&out_rel));
        assert!(
            completed.files.contains_key(dict_rel),
            "the merged dictionary is digested with the values it numbers"
        );
        let dict = attr
            .dict
            .as_ref()
            .expect("the completed pass carries the dictionary opened, beside the values");
        assert_eq!(dict.len(), 5, "alpha, beta, delta, epsilon, gamma");
        let mut scratch = Vec::new();
        for (entity, key) in &expected {
            let ordinal = attr
                .values
                .value_of(*entity)
                .expect("every consumed entity is present")
                .raw();
            assert_eq!(
                dict.key_of(ordinal, &mut scratch).expect("in range"),
                *key,
                "entity {entity} reads another key through the merged pair"
            );
        }

        // The manifest edit, and the files it names reopened from disc as a restart would.
        assert!(rebase_into(&mut manifest, &completed));
        let listed: Vec<&AttrExtent> = manifest
            .attr_extents
            .iter()
            .filter(|e| e.column == "title")
            .collect();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].dict.as_deref(), Some(dict_rel));
        let values = tessera_filter::open_extent(
            &prefix_dir.join(&listed[0].values),
            &prefix_dir.join(&listed[0].presence),
            tessera_filter::Access::Read,
        )
        .expect("the listed values open");
        let dict = tessera_filter::SortedDict::open(
            &prefix_dir.join(dict_rel),
            tessera_filter::Access::Read,
        )
        .expect("the listed dictionary opens");
        for (entity, key) in &expected {
            let ordinal = values.value_of(*entity).expect("present").raw();
            assert_eq!(dict.key_of(ordinal, &mut scratch).expect("in range"), *key);
        }
    }

    /// **A keyword window the merge refuses installs nothing.** The merge's guards run before any
    /// ordinal is written, and the executor's only commit point is the manifest edit, so a refusal
    /// leaves the consumed entries standing, their files digested, and the output directory as an
    /// orphan the fold reclaims.
    ///
    /// The fault here is an input the merge cannot read consistently: an extent whose ordinals
    /// reach past its own dictionary. A wrong *remap* — the merge's own defect — is refused by the
    /// same guard family at the merge (`tessera_filter_write::keyword`'s tests inject one), and
    /// the pass treats every refusal alike: `Err`, and nothing published.
    #[test]
    fn a_keyword_window_the_merge_refuses_installs_nothing() {
        let dir = tempfile::TempDir::new().unwrap();
        let prefix_dir = dir.path().join("v00000");
        let (mut manifest, build_files) = manifest_with(0);
        write_keyword_flush(
            &prefix_dir,
            &mut manifest,
            "flush-0-1",
            &[10, 11],
            &["b", "a"],
        );
        let faulted = write_keyword_flush(
            &prefix_dir,
            &mut manifest,
            "flush-1-1",
            &[20, 21],
            &["d", "c"],
        );
        write_keyword_flush(&prefix_dir, &mut manifest, "flush-2-1", &[30], &["e"]);
        // The second extent's dictionary replaced by one of a single key, so its ordinal 1 names
        // nothing. The manifest still digests the original bytes; the pass reads the file.
        let mut dictionary = Vec::new();
        let mut writer =
            tessera_filter::SortedDictWriter::new(&mut dictionary).expect("a writer opens");
        writer.push("c").unwrap();
        writer.finish().unwrap();
        std::fs::write(
            prefix_dir.join(faulted.dict.as_deref().unwrap()),
            dictionary,
        )
        .unwrap();
        let before = serde_json::to_string(&manifest).unwrap();

        let plan =
            plan_coalesce(PARTITION, &manifest, &build_files, policy(), &all_live).expect("a plan");
        let out_rel = format!("partitions/{PARTITION}/coalesced/coalesce-1-1");
        let err = match execute_coalesce(
            plan,
            CoalesceContext {
                prefix_dir: prefix_dir.clone(),
                prefix: "v00000".to_string(),
                out_rel: out_rel.clone(),
            },
        ) {
            Ok(_) => panic!("an ordinal past its dictionary must be refused"),
            Err(e) => e,
        };
        assert!(
            err.0.contains("keyword coalesce for 'title'"),
            "the refusal names the merge and the column: {err:?}"
        );
        assert_eq!(
            serde_json::to_string(&manifest).unwrap(),
            before,
            "the pass has no commit point before the manifest edit, and never reached it"
        );
        assert!(
            !prefix_dir
                .join(&out_rel)
                .join("attrs/title")
                .join(tessera_filter::VALUES_FILE)
                .exists(),
            "no ordinal was written under the merged dictionary"
        );
        for extent in &manifest.attr_extents {
            assert!(prefix_dir.join(&extent.values).exists());
            assert!(prefix_dir.join(extent.dict.as_deref().unwrap()).exists());
        }
    }

    /// **Both obligations land in one manifest edit: the files and the `attr_extents` entries.**
    ///
    /// Doing one without the other yields a bundle that opens cleanly and answers filters missing
    /// every entity the consumed window held — a wrong answer with no symptom, and strictly worse
    /// than a refusal to open (filter-index §6.2).
    ///
    /// **Mutation:** drop the `attr_paths` chain from the `files` removal and the consumed digests
    /// stand; drop the `attr_extents` rebuild and the manifest names the coalesced bytes nowhere.
    #[test]
    fn a_coalesced_attr_extent_replaces_its_window_in_both_halves_of_the_manifest() {
        let (mut manifest, build_files) = manifest_with(4);
        let plan =
            plan_coalesce(PARTITION, &manifest, &build_files, policy(), &all_live).expect("a plan");
        let consumed: Vec<String> = plan
            .attrs
            .iter()
            .flat_map(|w| w.extents.iter())
            .flat_map(|e| [e.values.clone(), e.presence.clone()])
            .collect();
        let out_rel = "partitions/p0/coalesced/coalesce-1-1";
        let attrs = completed_attrs(&plan, out_rel);
        let files: BTreeMap<String, FileDigest> = attrs
            .iter()
            .flat_map(|a| {
                [
                    (a.extent.values.clone(), digest(3072)),
                    (a.extent.presence.clone(), digest(96)),
                ]
            })
            .collect();
        let dir = tempfile::TempDir::new().unwrap();
        let completed = CompletedCoalesce {
            tier: Some(tier_at(dir.path())),
            run: None,
            dict: None,
            attrs,
            record: None,
            texts: Vec::new(),
            terms: None,
            files,
            plan,
            prefix: "v00000".to_string(),
        };
        assert!(rebase_into(&mut manifest, &completed));

        for column in COLUMNS {
            let listed: Vec<&AttrExtent> = manifest
                .attr_extents
                .iter()
                .filter(|e| e.column == column)
                .collect();
            assert_eq!(
                listed.len(),
                2,
                "3 extents became 1, 1 untouched: {listed:?}"
            );
            assert_eq!(
                listed[0].values,
                format!("{out_rel}/attrs/{column}/values.arrow"),
                "the coalesced extent takes the window's position in its column's subsequence"
            );
        }
        for rel in &consumed {
            assert!(
                !manifest.files.contains_key(rel),
                "a consumed extent file is still digested: {rel}"
            );
        }
        for attr in &completed.attrs {
            for rel in [&attr.extent.values, &attr.extent.presence] {
                assert!(
                    manifest.files.contains_key(rel),
                    "the coalesced extent's bytes are named in `attr_extents` but not digested: \
                     {rel}"
                );
            }
        }
    }

    /// A window a flush has since moved out from under no longer rebases — and a flush that
    /// *appends* another column's extent mid-list does not disturb it, because the contiguity that
    /// matters is contiguity in the column's own subsequence.
    #[test]
    fn an_attr_window_rebases_through_another_columns_flush_but_not_through_its_own() {
        let (mut manifest, build_files) = manifest_with(4);
        let plan =
            plan_coalesce(PARTITION, &manifest, &build_files, policy(), &all_live).expect("a plan");
        let dir = tempfile::TempDir::new().unwrap();
        let attrs = completed_attrs(&plan, "c");
        let completed = CompletedCoalesce {
            tier: Some(tier_at(dir.path())),
            run: None,
            dict: None,
            attrs,
            record: None,
            texts: Vec::new(),
            terms: None,
            files: BTreeMap::new(),
            plan,
            prefix: "v00000".to_string(),
        };

        // Another column's extent, inserted between two of `title`'s — which is precisely what a
        // flush publishing both columns produces, and must not discard the pass.
        let mut interleaved = manifest.clone();
        interleaved
            .attr_extents
            .insert(1, attr_extent_at(PARTITION, "elsewhere", "flush-9-1"));
        assert!(rebase_into(&mut interleaved, &completed));

        // Its own extent gone, however, is the state the plan was made against being gone.
        let consumed = completed.plan.attrs[0].extents[1].values.clone();
        manifest.attr_extents.retain(|e| e.values != consumed);
        assert!(!rebase_into(&mut manifest, &completed));
    }

    /// **A group-scoped column's window rebases within its own view's subsequence**, which is the
    /// `(column, view, incarnation)` the planner grouped it by. Two views of one family interleave
    /// their extents under one column name, so a subsequence taken on the name alone holds neither
    /// window contiguously and every finished coalesce of a scoped family is discarded.
    #[test]
    fn a_scoped_columns_window_rebases_within_its_own_views_extents() {
        let (mut manifest, build_files) = manifest_with(0);
        let views = ["quarter:2026-Q1", "quarter:2026-Q3"];
        for i in 0..4 {
            for view in views {
                let extent = scoped_extent_at(PARTITION, "mood", view, &format!("flush-{i}-1"));
                manifest.files.insert(extent.values.clone(), digest(1024));
                manifest.files.insert(extent.presence.clone(), digest(64));
                manifest.attr_extents.push(extent);
            }
        }
        let plan =
            plan_coalesce(PARTITION, &manifest, &build_files, policy(), &all_live).expect("a plan");
        assert_eq!(plan.attrs.len(), 2, "one window per view");
        let consumed: Vec<String> = plan
            .attrs
            .iter()
            .flat_map(|w| w.extents.iter())
            .flat_map(|e| [e.values.clone(), e.presence.clone()])
            .collect();
        let untouched: Vec<String> = manifest
            .attr_extents
            .iter()
            .filter(|e| !consumed.contains(&e.values))
            .map(|e| e.values.clone())
            .collect();

        let out_rel = "partitions/p0/coalesced/coalesce-1-1";
        let attrs = completed_attrs(&plan, out_rel);
        let files: BTreeMap<String, FileDigest> = attrs
            .iter()
            .flat_map(|a| {
                [
                    (a.extent.values.clone(), digest(3072)),
                    (a.extent.presence.clone(), digest(96)),
                ]
            })
            .collect();
        let coalesced: Vec<String> = attrs.iter().map(|a| a.extent.values.clone()).collect();
        let completed = CompletedCoalesce {
            tier: None,
            run: None,
            dict: None,
            attrs,
            record: None,
            texts: Vec::new(),
            terms: None,
            files,
            plan,
            prefix: "v00000".to_string(),
        };
        assert!(rebase_into(&mut manifest, &completed));

        let listed: Vec<&str> = manifest
            .attr_extents
            .iter()
            .map(|e| e.values.as_str())
            .collect();
        let expected: Vec<&str> = coalesced
            .iter()
            .chain(&untouched)
            .map(String::as_str)
            .collect();
        assert_eq!(
            listed, expected,
            "each view's coalesced extent lands where that view's window began, once, and the \
             later flush's extents keep their order"
        );
        for rel in &consumed {
            assert!(
                !manifest.files.contains_key(rel),
                "a consumed extent file is still digested: {rel}"
            );
        }
        for extent in &manifest.attr_extents {
            assert!(
                manifest.files.contains_key(&extent.values),
                "a listed extent's bytes are not digested: {}",
                extent.values
            );
        }
    }

    /// **A group-scoped text column's window rebases within its own view's subsequence** — the
    /// attribute axis's rule over `text_extents`, for its reason.
    #[test]
    fn a_scoped_text_columns_window_rebases_within_its_own_views_extents() {
        let (mut manifest, build_files) = manifest_with(0);
        let views = ["quarter:2026-Q1", "quarter:2026-Q3"];
        for i in 0..4 {
            for view in views {
                let (group, key) = view.split_once(':').expect("a view of a group");
                let dir = format!("partitions/{PARTITION}/text/notes/{group}/{key}/extents");
                let extent = TextExtent {
                    column: "notes".to_string(),
                    view: Some(view.to_string()),
                    incarnation: Some(tessera_store::manifest::DECLARED_INCARNATION),
                    dict: format!("{dir}/flush-{i}-1.dict"),
                    postings: format!("{dir}/flush-{i}-1.postings"),
                    presence: format!("{dir}/flush-{i}-1.roaring"),
                };
                manifest.files.insert(extent.dict.clone(), digest(1024));
                manifest.files.insert(extent.postings.clone(), digest(1024));
                manifest.files.insert(extent.presence.clone(), digest(64));
                manifest.text_extents.push(extent);
            }
        }
        let plan =
            plan_coalesce(PARTITION, &manifest, &build_files, policy(), &all_live).expect("a plan");
        assert_eq!(plan.texts.len(), 2, "one window per view");
        let consumed: Vec<String> = plan
            .texts
            .iter()
            .flat_map(|w| w.extents.iter())
            .flat_map(|e| e.files().map(String::from))
            .collect();
        let untouched: Vec<String> = manifest
            .text_extents
            .iter()
            .filter(|e| !consumed.contains(&e.dict))
            .map(|e| e.dict.clone())
            .collect();

        let out_rel = "partitions/p0/coalesced/coalesce-1-1";
        let texts: Vec<TextExtent> = plan
            .texts
            .iter()
            .map(|window| {
                let column_rel =
                    coalesced_column_rel(out_rel, &window.column, window.view.as_deref());
                TextExtent {
                    column: window.column.clone(),
                    view: window.view.clone(),
                    incarnation: window.incarnation,
                    dict: format!("{column_rel}/text.dict"),
                    postings: format!("{column_rel}/text.postings"),
                    presence: format!("{column_rel}/text.roaring"),
                }
            })
            .collect();
        let files: BTreeMap<String, FileDigest> = texts
            .iter()
            .flat_map(|e| {
                [
                    (e.dict.clone(), digest(3072)),
                    (e.postings.clone(), digest(3072)),
                    (e.presence.clone(), digest(96)),
                ]
            })
            .collect();
        let coalesced: Vec<String> = texts.iter().map(|e| e.dict.clone()).collect();
        let completed = CompletedCoalesce {
            tier: None,
            run: None,
            dict: None,
            attrs: Vec::new(),
            record: None,
            texts,
            terms: None,
            files,
            plan,
            prefix: "v00000".to_string(),
        };
        assert!(rebase_into(&mut manifest, &completed));

        let listed: Vec<&str> = manifest
            .text_extents
            .iter()
            .map(|e| e.dict.as_str())
            .collect();
        let expected: Vec<&str> = coalesced
            .iter()
            .chain(&untouched)
            .map(String::as_str)
            .collect();
        assert_eq!(
            listed, expected,
            "each view's coalesced extent lands where that view's window began, once, and the \
             later flush's extents keep their order"
        );
        for rel in &consumed {
            assert!(
                !manifest.files.contains_key(rel),
                "a consumed extent file is still digested: {rel}"
            );
        }
        for extent in &manifest.text_extents {
            assert!(
                manifest.files.contains_key(&extent.dict),
                "a listed extent's bytes are not digested: {}",
                extent.dict
            );
        }
    }

    /// **The record axis selects a window of `record_extents` and replaces it in place, in both
    /// halves of the manifest** — the entry list and the files map. The same silent-failure shape
    /// as the attribute axis: a bundle that lost the window's entry while keeping its bytes (or
    /// the reverse) opens cleanly and answers drill-downs short, with no symptom.
    #[test]
    fn the_record_axis_selects_a_window_and_replaces_it_in_both_manifest_halves() {
        let (mut manifest, build_files) = manifest_with(4);
        for i in 0..4 {
            let dir = "partitions/p0/attrs/record/extents";
            let extent = RecordExtent {
                blocks: format!("{dir}/flush-{i}-1.blocks.bin"),
                hasrow: format!("{dir}/flush-{i}-1.hasrow.roaring"),
                directory: format!("{dir}/flush-{i}-1.directory.arrow"),
            };
            manifest.files.insert(extent.blocks.clone(), digest(1024));
            manifest.files.insert(extent.hasrow.clone(), digest(64));
            manifest.files.insert(extent.directory.clone(), digest(128));
            manifest.record_extents.push(extent);
        }
        let plan =
            plan_coalesce(PARTITION, &manifest, &build_files, policy(), &all_live).expect("a plan");
        assert_eq!(plan.records.len(), 3, "the policy's width");
        let consumed: Vec<String> = plan
            .records
            .iter()
            .flat_map(|e| e.files().map(String::from))
            .collect();

        let coalesced = RecordExtent {
            blocks: "c/attrs/record/blocks.bin".to_string(),
            hasrow: "c/attrs/record/hasrow.roaring".to_string(),
            directory: "c/attrs/record/directory.arrow".to_string(),
        };
        let dir = tempfile::TempDir::new().unwrap();
        let attrs = completed_attrs(&plan, "c");
        let files: BTreeMap<String, FileDigest> = [
            (coalesced.blocks.clone(), digest(3072)),
            (coalesced.hasrow.clone(), digest(96)),
            (coalesced.directory.clone(), digest(256)),
        ]
        .into_iter()
        .collect();
        let completed = CompletedCoalesce {
            tier: Some(tier_at(dir.path())),
            run: None,
            dict: None,
            attrs,
            record: Some(coalesced.clone()),
            texts: Vec::new(),
            terms: None,
            files,
            plan,
            prefix: "v00000".to_string(),
        };
        assert!(rebase_into(&mut manifest, &completed));

        assert_eq!(
            manifest.record_extents.len(),
            2,
            "3 extents became 1, 1 untouched: {:?}",
            manifest.record_extents
        );
        assert_eq!(
            manifest.record_extents[0].blocks, coalesced.blocks,
            "the coalesced extent takes the window's position"
        );
        for rel in &consumed {
            assert!(
                !manifest.files.contains_key(rel),
                "a consumed extent file is still digested: {rel}"
            );
        }
        for rel in [&coalesced.blocks, &coalesced.hasrow, &coalesced.directory] {
            assert!(
                manifest.files.contains_key(rel),
                "the coalesced extent's bytes are named in `record_extents` but not digested: {rel}"
            );
        }

        // And a window a fold (or another pass) has since consumed no longer rebases.
        let gone = completed.plan.records[1].blocks.clone();
        manifest.record_extents.retain(|e| e.blocks != gone);
        assert!(!rebase_into(&mut manifest, &completed));
    }

    /// **The entity→term axis selects a window of `entity_terms_extents` and replaces it in place,
    /// in both halves of the manifest** — the record axis's claim over the record axis's shape.
    /// The silent failure it guards is the sharper one of the two: a bundle that lost the window's
    /// entry while keeping its bytes answers *unknown* for those entities' labels, which on the
    /// write path is the join rule's `409` failing to fire.
    #[test]
    fn the_entity_terms_axis_selects_a_window_and_replaces_it_in_both_manifest_halves() {
        let (mut manifest, build_files) = manifest_with(4);
        for i in 0..4 {
            let dir = "partitions/p0/entities/terms/extents";
            let extent = EntityTermsExtent {
                hasrow: format!("{dir}/flush-{i}-1.hasrow.roaring"),
                offsets: format!("{dir}/flush-{i}-1.offsets.u32"),
                terms: format!("{dir}/flush-{i}-1.terms.u32"),
                bases: format!("{dir}/flush-{i}-1.bases.u64"),
            };
            manifest.files.insert(extent.hasrow.clone(), digest(64));
            manifest.files.insert(extent.offsets.clone(), digest(128));
            manifest.files.insert(extent.terms.clone(), digest(1024));
            manifest.files.insert(extent.bases.clone(), digest(8));
            manifest.entity_terms_extents.push(extent);
        }
        let plan =
            plan_coalesce(PARTITION, &manifest, &build_files, policy(), &all_live).expect("a plan");
        assert_eq!(plan.terms.len(), 3, "the policy's width");
        let consumed: Vec<String> = plan
            .terms
            .iter()
            .flat_map(|e| {
                [
                    e.hasrow.clone(),
                    e.offsets.clone(),
                    e.terms.clone(),
                    e.bases.clone(),
                ]
            })
            .collect();

        let coalesced = EntityTermsExtent {
            hasrow: "c/entities/terms/hasrow.roaring".to_string(),
            offsets: "c/entities/terms/offsets.u32".to_string(),
            terms: "c/entities/terms/terms.u32".to_string(),
            bases: "c/entities/terms/bases.u64".to_string(),
        };
        let dir = tempfile::TempDir::new().unwrap();
        let attrs = completed_attrs(&plan, "c");
        let files: BTreeMap<String, FileDigest> = [
            (coalesced.hasrow.clone(), digest(96)),
            (coalesced.offsets.clone(), digest(384)),
            (coalesced.terms.clone(), digest(3072)),
        ]
        .into_iter()
        .collect();
        let completed = CompletedCoalesce {
            tier: Some(tier_at(dir.path())),
            run: None,
            dict: None,
            attrs,
            record: None,
            texts: Vec::new(),
            terms: Some(coalesced.clone()),
            files,
            plan,
            prefix: "v00000".to_string(),
        };
        assert!(rebase_into(&mut manifest, &completed));

        assert_eq!(
            manifest.entity_terms_extents.len(),
            2,
            "3 extents became 1, 1 untouched: {:?}",
            manifest.entity_terms_extents
        );
        assert_eq!(
            manifest.entity_terms_extents[0].terms, coalesced.terms,
            "the coalesced extent takes the window's position"
        );
        for rel in &consumed {
            assert!(
                !manifest.files.contains_key(rel),
                "a consumed extent file is still digested: {rel}"
            );
        }
        for rel in [&coalesced.hasrow, &coalesced.offsets, &coalesced.terms] {
            assert!(
                manifest.files.contains_key(rel),
                "the coalesced extent's bytes are listed but not digested: {rel}"
            );
        }

        // And a window a fold (or another pass) has since consumed no longer rebases.
        let gone = completed.plan.terms[1].terms.clone();
        manifest.entity_terms_extents.retain(|e| e.terms != gone);
        assert!(!rebase_into(&mut manifest, &completed));
    }

    /// A plan whose window is gone no longer rebases, and the publication is discarded rather than
    /// forced — its files orphans nothing references, every consumed entry still standing.
    #[test]
    fn a_plan_whose_window_moved_does_not_rebase() {
        let (mut manifest, build_files) = manifest_with(3);
        let plan =
            plan_coalesce(PARTITION, &manifest, &build_files, policy(), &all_live).expect("a plan");
        let dir = tempfile::TempDir::new().unwrap();
        let attrs = completed_attrs(&plan, "c");
        let completed = CompletedCoalesce {
            tier: Some(tier_at(dir.path())),
            run: None,
            dict: None,
            attrs,
            record: None,
            texts: Vec::new(),
            terms: None,
            files: BTreeMap::new(),
            plan,
            prefix: "v00000".to_string(),
        };
        manifest.deltas.remove(1);
        assert!(!rebase_into(&mut manifest, &completed));
    }
}
