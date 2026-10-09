use std::collections::BTreeMap;

use mosaica_store::manifest::{
    DictExtent, EntityTermsExtent, FileDigest, RecordExtent, SegmentsManifest,
};
use mosaica_store::merge::size_tier;

use super::{CoalescePlan, CoalescePolicy, ColumnExtent, ColumnWindow, WindowKey};

/// Plans a coalesce over `manifest`, sizing files from its digests and from `build_files`, the
/// prefix's `MANIFEST.json` digests. Takes manifests rather than a generation so every selection
/// rule is testable without an engine. `None` when no axis qualifies.
pub(crate) fn plan_coalesce(
    partition: &str,
    manifest: &SegmentsManifest,
    build_files: &BTreeMap<String, FileDigest>,
    policy: CoalescePolicy,
    is_live: &dyn Fn(&str, mosaica_types::view::ViewIncarnation) -> bool,
) -> Option<CoalescePlan> {
    // A file neither manifest digests makes its entry ineligible.
    let size_of = |rel: &str| -> Option<u64> {
        manifest
            .files
            .get(rel)
            .or_else(|| build_files.get(rel))
            .map(|d| d.size)
    };

    let mut plan = CoalescePlan {
        partition: partition.to_string(),
        ..Default::default()
    };

    if let Some(window) =
        select_window(&manifest.deltas, policy.width, policy, |rel| size_of(rel))
    {
        plan.tiers = manifest.deltas[window].to_vec();
    }

    let runs_policy = CoalescePolicy {
        floor_bytes: policy.run_floor_bytes,
        ..policy
    };

    // An ordinal indexes the concatenation of dictionary extents in list order. The first is the
    // base dictionary and is never taken; the merged extent must land in the window's place, or
    // every later ordinal shifts.
    if let Some((_base, promoted)) = manifest.dict_extents.split_first() {
        if let Some(window) =
            select_window(promoted, policy.width, policy, |extent: &DictExtent| {
                size_of(&extent.path)
            })
        {
            plan.dicts = manifest.dict_extents[window.start + 1..window.end + 1].to_vec();
        }
    }

    // A layer's dictionary counts toward the input cap.
    plan.attrs = column_windows(&manifest.attr_extents, policy, is_live, |extent| {
        extent.files().map(&size_of).sum()
    });

    {
        let size = |extent: &RecordExtent| extent.files().map(&size_of).sum();
        if let Some(window) = widest_window(&manifest.record_extents, policy, size) {
            plan.records = manifest.record_extents[window].to_vec();
        }
    }

    // A text extent's dictionary, postings and presence all count toward the input cap.
    plan.texts = column_windows(&manifest.text_extents, policy, is_live, |extent| {
        extent.files().map(&size_of).sum()
    });

    {
        let size = |extent: &EntityTermsExtent| extent.files().map(&size_of).sum();
        if let Some(window) = widest_window(&manifest.entity_terms_extents, policy, size) {
            plan.terms = manifest.entity_terms_extents[window].to_vec();
        }
    }

    // A lookup reads every live run of a unique column, so they coalesce on the key runs' policy.
    for index in &manifest.unique_indexes {
        if let Some(window) =
            select_window(&index.live, policy.run_width, runs_policy, |rel| size_of(rel))
        {
            plan.unique.push(super::UniqueWindow {
                attribute: index.attribute.clone(),
                runs: index.live[window].to_vec(),
            });
        }
    }

    // A lookup reads every live run of both directions of the edited-items map too.
    for (direction, runs) in [
        (mosaica_store::edited::Direction::ByNumber, &manifest.edited_items.by_number),
        (mosaica_store::edited::Direction::ByEntity, &manifest.edited_items.by_entity),
    ] {
        if let Some(window) =
            select_window(&runs.live, policy.run_width, runs_policy, |rel| size_of(rel))
        {
            plan.edited.push(super::EditedWindow {
                direction,
                runs: runs.live[window].to_vec(),
            });
        }
    }

    (!plan.is_empty()).then_some(plan)
}

/// The first `width` consecutive entries that share one size tier and fit
/// `policy.max_input_bytes`. An entry `size_of` gives `None` breaks any window containing it.
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

/// The widest window, from `policy.width` down to 2, whose bytes fit the input cap. Nothing is
/// taken unless `policy.width` same-tier entries exist with the cap ignored.
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

/// One window per live (column, view, incarnation), over that key's own subsequence. A dead
/// incarnation is skipped because the output path is built from (column, view) alone and would
/// overwrite the live one's files. A view without an incarnation, or the reverse, is skipped.
fn column_windows<E: ColumnExtent + Clone>(
    entries: &[E],
    policy: CoalescePolicy,
    is_live: &dyn Fn(&str, mosaica_types::view::ViewIncarnation) -> bool,
    size_of: impl Fn(&E) -> Option<u64>,
) -> Vec<ColumnWindow<E>> {
    let mut by_column: BTreeMap<WindowKey<'_>, Vec<&E>> = BTreeMap::new();
    for extent in entries {
        by_column.entry(extent.key()).or_default().push(extent);
    }
    let mut windows = Vec::new();
    for ((column, view, incarnation), extents) in by_column {
        let live = match (view, incarnation) {
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
