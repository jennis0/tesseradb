//! Coalesces entity-space maintenance artefacts without touching row space: delta tiers,
//! external-id runs and their locator extents, dictionary extents, attribute extents, the record
//! blob, text extents and entity-to-term extents.
//!
//! Each axis merges several small extents into one, preserving the set of entries it holds. Some
//! merges renumber ordinals against a merged dictionary; others concatenate. A coalesce changes no
//! row id, bumps no `segments_version`, invalidates no cache or projection, and never takes the
//! base external-id run or the base dictionary. It retires nothing: no posting is dropped and no tombstone is applied.
//!
//! [`plan_coalesce`] runs on the executor and chooses what to take. [`execute_coalesce`] runs on
//! the background pool and writes the merged files. [`rebase_into`] applies the result to the live
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
pub(crate) use rebase::rebase_into;

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
