//! Coalesces entity-space maintenance artefacts without touching row space: delta tiers,
//! external-id runs and their locator extents, dictionary extents, attribute extents, the record
//! blob, text extents and entity-to-term extents.
//!
//! Each axis merges several small extents into one, preserving the set of entries it holds. Some
//! merges renumber ordinals against a merged dictionary; others concatenate. A coalesce changes no
//! row id, bumps no `segments_version`, invalidates no cache or projection, and never takes the
//! base external-id run or the base dictionary. It retires nothing: no posting is dropped and no
//! tombstone is applied.
//!
//! [`plan_coalesce`] runs on the executor and chooses what to take. [`execute_coalesce`] runs on
//! the background pool and writes the merged files. [`rebased`] applies the result to the live
//! manifest, or discards it if a flush moved the entries it planned against.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use tessera_authz::DeltaTier;
use tessera_store::manifest::{
    AttrExtent, DictExtent, EntityTermsExtent, FileDigest, LocatorExtent, RecordExtent,
    TextExtent,
};

use crate::flush::MaintenanceFailed;

mod execute;
mod plan;
mod rebase;
#[cfg(test)]
mod tests;

use execute::{
    coalesce_attr_window, coalesce_dicts, coalesce_entity_terms, coalesce_records, coalesce_runs,
    coalesce_text_window, coalesce_tiers,
};
pub(crate) use plan::plan_coalesce;
pub(crate) use rebase::rebased;

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

/// One coalesce's immutable plan: which entries of which kinds it consumes. An empty list is a
/// kind this pass does not take.
///
/// Entries are named by path, not index. Paths are never reused, so a path still in the live
/// manifest at publication is still the same bytes: a flush that published while the pass ran can
/// only append, never move what the plan named.
#[derive(Debug, Default)]
pub(crate) struct CoalescePlan {
    pub(crate) partition: String,
    /// Consumed `deltas` entries, in list order.
    pub(crate) tiers: Vec<String>,
    /// Consumed `locator_extents` entries, in list order. Each names its run, and the runs are a
    /// contiguous block of `external_id_runs` in the same order.
    pub(crate) locators: Vec<LocatorExtent>,
    /// Consumed `dict_extents` entries, in list order.
    pub(crate) dicts: Vec<DictExtent>,
    /// Consumed `attr_extents` entries, one window per column.
    pub(crate) attrs: Vec<ColumnWindow<AttrExtent>>,
    /// Consumed `record_extents` entries, in list order.
    pub(crate) records: Vec<RecordExtent>,
    /// Consumed `text_extents` entries, one window per text column.
    pub(crate) texts: Vec<ColumnWindow<TextExtent>>,
    /// Consumed `entity_terms_extents` entries, in list order.
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

impl<E> ColumnWindow<E> {
    fn key(&self) -> WindowKey<'_> {
        (self.column.as_str(), self.view.as_deref(), self.incarnation)
    }
}

impl CoalescePlan {
    pub(crate) fn is_empty(&self) -> bool {
        self.tiers.is_empty()
            && self.locators.is_empty()
            && self.dicts.is_empty()
            && self.attrs.is_empty()
            && self.records.is_empty()
            && self.texts.is_empty()
            && self.terms.is_empty()
    }
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

/// A window a coalesce consumed and the entry that replaces it.
#[derive(Debug)]
pub(crate) struct Merged<W, O> {
    pub(crate) consumed: W,
    pub(crate) output: O,
}

impl<W, O> Merged<W, O> {
    fn of(
        consumed: W,
        merge: impl FnOnce(&W) -> Result<O, MaintenanceFailed>,
    ) -> Result<Self, MaintenanceFailed> {
        let output = merge(&consumed)?;
        Ok(Merged { consumed, output })
    }
}

/// A coalesced tier's path and its opened reader.
pub(crate) type OpenedTier = (String, Arc<DeltaTier>);

/// A coalesce whose files are durable, awaiting the manifest edit and the swap on the executor.
/// `None` or empty is a kind the pass did not take.
pub(crate) struct CompletedCoalesce {
    pub(crate) partition: String,
    pub(crate) prefix: String,
    pub(crate) tier: Option<Merged<Vec<String>, OpenedTier>>,
    /// The consumed locator extents and the one covering their union span, which names the
    /// coalesced run.
    pub(crate) run: Option<Merged<Vec<LocatorExtent>, LocatorExtent>>,
    pub(crate) dict: Option<Merged<Vec<DictExtent>, DictExtent>>,
    /// Opened, so publication is a pointer push on the executor and cannot fail on IO after the
    /// manifest edit.
    pub(crate) attrs: Vec<Merged<ColumnWindow<AttrExtent>, crate::filter::OpenedExtent>>,
    pub(crate) record: Option<Merged<Vec<RecordExtent>, RecordExtent>>,
    pub(crate) texts: Vec<Merged<ColumnWindow<TextExtent>, TextExtent>>,
    pub(crate) terms: Option<Merged<Vec<EntityTermsExtent>, EntityTermsExtent>>,
    /// Every file this pass wrote, prefix-relative, with its digest.
    pub(crate) files: BTreeMap<String, FileDigest>,
}

/// A kind's consumed entries, or `None` if the plan does not take the kind.
fn taken<E>(entries: Vec<E>) -> Option<Vec<E>> {
    (!entries.is_empty()).then_some(entries)
}

/// Turn a plan into durable files. Runs on the background pool, over immutable inputs.
pub(crate) fn execute_coalesce(
    plan: CoalescePlan,
    ctx: CoalesceContext,
) -> Result<CompletedCoalesce, MaintenanceFailed> {
    std::fs::create_dir_all(ctx.prefix_dir.join(&ctx.out_rel))
        .map_err(|e| MaintenanceFailed(format!("coalesce dir: {e}")))?;
    let mut files: BTreeMap<String, FileDigest> = BTreeMap::new();

    let tier = taken(plan.tiers)
        .map(|w| Merged::of(w, |w| coalesce_tiers(w, &ctx, &mut files)))
        .transpose()?;
    let run = taken(plan.locators)
        .map(|w| Merged::of(w, |w| coalesce_runs(w, &ctx, &mut files)))
        .transpose()?;
    let dict = taken(plan.dicts)
        .map(|w| Merged::of(w, |w| coalesce_dicts(w, &ctx, &mut files)))
        .transpose()?;
    let attrs = plan
        .attrs
        .into_iter()
        .map(|w| Merged::of(w, |w| coalesce_attr_window(w, &ctx, &mut files)))
        .collect::<Result<_, _>>()?;
    let record = taken(plan.records)
        .map(|w| Merged::of(w, |w| coalesce_records(w, &ctx, &mut files)))
        .transpose()?;
    let texts = plan
        .texts
        .into_iter()
        .map(|w| Merged::of(w, |w| coalesce_text_window(w, &ctx, &mut files)))
        .collect::<Result<_, _>>()?;
    let terms = taken(plan.terms)
        .map(|w| Merged::of(w, |w| coalesce_entity_terms(w, &ctx, &mut files)))
        .transpose()?;

    Ok(CompletedCoalesce {
        partition: plan.partition,
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
