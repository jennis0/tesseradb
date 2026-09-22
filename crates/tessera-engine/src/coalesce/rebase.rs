use tessera_store::manifest::{
    AttrExtent, DictExtent, EntityTermsExtent, LocatorExtent, RecordExtent, SegmentsManifest,
    TextExtent,
};

use super::{ColumnExtent, CompletedCoalesce};
use crate::merge::contiguous;

/// `manifest` with `completed` applied, or `None` if it no longer rebases.
///
/// Every consumed entry must still be present, contiguous and in order. A flush publishing while
/// the pass ran only appends, so the ordinary case is that the window is exactly where it was;
/// otherwise the plan is stale and is discarded, leaving the consumed entries standing for the
/// next tick to re-plan.
///
/// The coalesced entry takes the window's position, never the end of the list. This preserves
/// recency on the run list and every ordinal on the dictionary list, and keeps the manifest's
/// bytes independent of when the pass ran.
pub(crate) fn rebased(
    manifest: &SegmentsManifest,
    completed: &CompletedCoalesce,
) -> Option<SegmentsManifest> {
    let mut next = manifest.clone();
    if let Some(m) = &completed.tier {
        let tier = m.output.0.clone();
        replace_window(&mut next.deltas, &m.consumed, tier, |rel| rel, all)?;
    }
    if let Some(m) = &completed.run {
        let runs: Vec<String> = m.consumed.iter().map(|e| e.external_id_run.clone()).collect();
        let run = m.output.external_id_run.clone();
        replace_window(&mut next.external_id_runs, &runs, run, |rel| rel, all)?;
        let locator = m.output.clone();
        replace_window(&mut next.locator_extents, &m.consumed, locator, |e| &e.path, all)?;
    }
    if let Some(m) = &completed.dict {
        let dict = m.output.clone();
        replace_window(&mut next.dict_extents, &m.consumed, dict, |e| &e.path, all)?;
    }
    for m in &completed.attrs {
        let (window, attr) = (&m.consumed, m.output.extent.clone());
        let same_column = |e: &AttrExtent| e.key() == window.key();
        replace_window(&mut next.attr_extents, &window.extents, attr, |e| &e.values, same_column)?;
    }
    if let Some(m) = &completed.record {
        let record = m.output.clone();
        replace_window(&mut next.record_extents, &m.consumed, record, |e| &e.blocks, all)?;
    }
    for m in &completed.texts {
        let (window, text) = (&m.consumed, m.output.clone());
        let same_column = |e: &TextExtent| e.key() == window.key();
        replace_window(&mut next.text_extents, &window.extents, text, |e| &e.dict, same_column)?;
    }
    if let Some(m) = &completed.terms {
        let terms = m.output.clone();
        replace_window(&mut next.entity_terms_extents, &m.consumed, terms, |e| &e.terms, all)?;
    }

    for rel in completed.consumed_files() {
        next.files.remove(rel);
    }
    next.files.extend(completed.files.clone());
    Some(next)
}

/// Replaces `consumed` in `entries` with `replacement` at the window's first position, or `None`
/// if the window is gone. The window must be contiguous among the entries `within` selects; an
/// entry is recognised by `key`.
fn replace_window<T: Clone>(
    entries: &mut Vec<T>,
    consumed: &[T],
    replacement: T,
    key: impl Fn(&T) -> &String,
    within: impl Fn(&T) -> bool,
) -> Option<()> {
    let positions: Vec<usize> = (0..entries.len()).filter(|&i| within(&entries[i])).collect();
    let listed: Vec<&String> = positions.iter().map(|&i| key(&entries[i])).collect();
    let needle: Vec<&String> = consumed.iter().map(&key).collect();
    let taken = &positions[contiguous(&listed, &needle, |k| k)?];
    for &i in taken.iter().rev() {
        entries.remove(i);
    }
    entries.insert(taken[0], replacement);
    Some(())
}

fn all<T>(_: &T) -> bool {
    true
}

impl CompletedCoalesce {
    /// Every file the consumed entries name.
    fn consumed_files(&self) -> impl Iterator<Item = &str> {
        let tiers = self.tier.iter().flat_map(|m| m.consumed.iter().map(String::as_str));
        let runs = self.run.iter().flat_map(|m| &m.consumed);
        let dicts = self.dict.iter().flat_map(|m| &m.consumed);
        let attrs = self.attrs.iter().flat_map(|m| &m.consumed.extents);
        let records = self.record.iter().flat_map(|m| &m.consumed);
        let texts = self.texts.iter().flat_map(|m| &m.consumed.extents);
        let terms = self.terms.iter().flat_map(|m| &m.consumed);
        tiers
            .chain(runs.flat_map(LocatorExtent::files))
            .chain(dicts.flat_map(DictExtent::files))
            .chain(attrs.flat_map(AttrExtent::files))
            .chain(records.flat_map(RecordExtent::files))
            .chain(texts.flat_map(TextExtent::files))
            .chain(terms.flat_map(EntityTermsExtent::files))
    }
}
