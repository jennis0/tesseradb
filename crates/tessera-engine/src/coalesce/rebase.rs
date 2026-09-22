use std::collections::{BTreeMap, BTreeSet};

use tessera_store::manifest::{AttrExtent, SegmentsManifest, TextExtent};

use super::{ColumnExtent, ColumnWindow, CompletedCoalesce, Merged};

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
    let consumed_tiers = consumed(&completed.tier);
    let consumed_locators = consumed(&completed.run);
    let consumed_dicts = consumed(&completed.dict);
    let consumed_records = consumed(&completed.record);
    let consumed_terms = consumed(&completed.terms);

    let tiers = match window_of(&manifest.deltas, consumed_tiers, |rel| rel) {
        Some(at) => at,
        None => return false,
    };
    let run_paths: Vec<String> = consumed_locators
        .iter()
        .map(|e| e.external_id_run.clone())
        .collect();
    let runs = match window_of(&manifest.external_id_runs, &run_paths, |rel| rel) {
        Some(at) => at,
        None => return false,
    };
    let locator_paths: Vec<String> = consumed_locators.iter().map(|e| e.path.clone()).collect();
    let locators = match window_of(&manifest.locator_extents, &locator_paths, |e| &e.path) {
        Some(at) => at,
        None => return false,
    };
    let dict_paths: Vec<String> = consumed_dicts.iter().map(|e| e.path.clone()).collect();
    let dicts = match window_of(&manifest.dict_extents, &dict_paths, |e| &e.path) {
        Some(at) => at,
        None => return false,
    };
    // Keyed by the values path, the identity a listed attribute extent is found by.
    let Some(attr_positions) = column_positions(
        &manifest.attr_extents,
        completed.attrs.iter().map(|m| &m.consumed),
        |e: &AttrExtent| e.values.as_str(),
    ) else {
        return false;
    };

    // The record window: one contiguous run of `record_extents`, keyed by the blocks path.
    let record_paths: Vec<String> = consumed_records.iter().map(|e| e.blocks.clone()).collect();
    let records = match window_of(&manifest.record_extents, &record_paths, |e| &e.blocks) {
        Some(at) => at,
        None => return false,
    };

    // The transpose window: one contiguous run of `entity_terms_extents`, keyed by the terms path.
    let terms_paths: Vec<String> = consumed_terms.iter().map(|e| e.terms.clone()).collect();
    let terms = match window_of(&manifest.entity_terms_extents, &terms_paths, |e| &e.terms) {
        Some(at) => at,
        None => return false,
    };

    // The text axis, keyed by the dictionary path, the identity a text layer is named by.
    let Some(text_positions) = column_positions(
        &manifest.text_extents,
        completed.texts.iter().map(|m| &m.consumed),
        |e: &TextExtent| e.dict.as_str(),
    ) else {
        return false;
    };

    // Every file a consumed extent names, its dictionary included, so nothing digested here is
    // left for a layer no list names.
    let attr_paths: Vec<String> = completed
        .attrs
        .iter()
        .flat_map(|m| m.consumed.extents.iter())
        .flat_map(|e| e.files().map(String::from))
        .collect();
    let text_paths: Vec<String> = completed
        .texts
        .iter()
        .flat_map(|m| m.consumed.extents.iter())
        .flat_map(|e| e.files().map(String::from))
        .collect();
    let record_files: Vec<String> = consumed_records
        .iter()
        .flat_map(|e| e.files().map(String::from))
        .collect();
    let terms_files: Vec<String> = consumed_terms
        .iter()
        .flat_map(|e| e.files().map(String::from))
        .collect();
    for rel in consumed_tiers
        .iter()
        .chain(&run_paths)
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

    if let Some(tier) = &completed.tier {
        manifest.deltas.splice(tiers, [tier.output.0.clone()]);
    }
    if let Some(run) = &completed.run {
        manifest
            .external_id_runs
            .splice(runs, [run.output.external_id_run.clone()]);
        manifest.locator_extents.splice(locators, [run.output.clone()]);
    }
    if let Some(dict) = &completed.dict {
        manifest.dict_extents.splice(dicts, [dict.output.clone()]);
    }
    if let Some(record) = &completed.record {
        // The window's position: nothing reads `record_extents` by position, but the manifest's
        // bytes must not depend on when the pass ran.
        manifest.record_extents.splice(records, [record.output.clone()]);
    }
    if let Some(merged) = &completed.terms {
        manifest
            .entity_terms_extents
            .splice(terms, [merged.output.clone()]);
    }
    let coalesced: Vec<&AttrExtent> = completed.attrs.iter().map(|m| &m.output.extent).collect();
    splice_columns(&mut manifest.attr_extents, &attr_positions, &coalesced);
    let coalesced: Vec<&TextExtent> = completed.texts.iter().map(|m| &m.output).collect();
    splice_columns(&mut manifest.text_extents, &text_positions, &coalesced);
    true
}

/// The entries a single-window kind consumed, or none if the pass did not take it.
fn consumed<E, O>(merged: &Option<Merged<Vec<E>, O>>) -> &[E] {
    merged.as_ref().map_or(&[], |m| m.consumed.as_slice())
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
fn column_positions<'w, E: ColumnExtent + 'w>(
    entries: &[E],
    windows: impl Iterator<Item = &'w ColumnWindow<E>>,
    identity: impl Fn(&E) -> &str,
) -> Option<Vec<Vec<usize>>> {
    let mut positions = Vec::new();
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
