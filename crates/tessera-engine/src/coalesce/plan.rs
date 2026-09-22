use std::collections::BTreeMap;

use tessera_store::manifest::{
    DictExtent, EntityTermsExtent, FileDigest, LocatorExtent, RecordExtent, SegmentsManifest,
};
use tessera_store::merge::size_tier;

use super::{CoalescePlan, CoalescePolicy, ColumnExtent, ColumnWindow, WindowKey};

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
pub(super) fn select_window<T>(
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
