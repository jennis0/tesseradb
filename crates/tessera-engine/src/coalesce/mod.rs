//! Merges small maintenance extents in entity space: delta tiers, external-id runs with their
//! locator extents, and dictionary, attribute, record, text and entity-to-term extents. A coalesce
//! changes no row id, bumps no `segments_version`, invalidates no cache or projection, and retires
//! nothing: no posting is dropped and no tombstone applied.
//!
//! [`plan_coalesce`] runs on the executor, [`execute_coalesce`] writes on the background pool, and
//! [`rebased`] applies the result to a copy of the manifest on the executor. The commit point is
//! the side-manifest write in `publish_coalesce`. A pass discarded before it leaves orphan files
//! and every consumed entry standing.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use tessera_authz::DeltaTier;
use tessera_store::manifest::{
    AttrExtent, DictExtent, EntityTermsExtent, FileDigest, LocatorExtent, RecordExtent,
    TextExtent,
};

use crate::flush::{failed, MaintenanceFailed};

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

#[derive(Debug, Clone, Copy)]
pub(crate) struct CoalescePolicy {
    /// Same-tier entries needed to take a window. Below 2, no pass runs.
    pub(crate) width: usize,
    /// Sizes at or below this share one size tier. Without it, at a modest ingest rate every entry
    /// sits in its own tier and `width` is never reached.
    pub(crate) floor_bytes: u64,
    /// Input bytes one axis may take in one pass, which bounds the memory held while merging.
    pub(crate) max_input_bytes: u64,
    /// `width` and `floor_bytes` for the external-id runs and their locator extents. Every ingest
    /// duplicate check and item lookup walks the run list, so it is kept shorter than the others:
    /// below the floor every run joins one, and above it runs tier as the other axes do.
    pub(crate) run_width: usize,
    pub(crate) run_floor_bytes: u64,
}

impl Default for CoalescePolicy {
    fn default() -> Self {
        CoalescePolicy {
            width: 8,
            floor_bytes: 1 << 20,
            max_input_bytes: 256 << 20,
            run_width: 4,
            run_floor_bytes: 16 << 20,
        }
    }
}

/// The entries one coalesce consumes; an empty list is a kind it does not take. Entries are named
/// by path, and paths are never reused, so a path still listed when the pass publishes holds the
/// bytes the plan saw. A flush while the pass ran only appends.
#[derive(Debug, Default)]
pub(crate) struct CoalescePlan {
    pub(crate) partition: String,
    pub(crate) tiers: Vec<String>,
    pub(crate) locators: Vec<LocatorExtent>,
    pub(crate) dicts: Vec<DictExtent>,
    pub(crate) attrs: Vec<ColumnWindow<AttrExtent>>,
    pub(crate) records: Vec<RecordExtent>,
    pub(crate) texts: Vec<ColumnWindow<TextExtent>>,
    pub(crate) terms: Vec<EntityTermsExtent>,
}

/// A window's output directory, prefix-relative: `<out>/attrs/<column>/`, with the view's group
/// and key appended for a group-scoped family's column.
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

/// Column, view and the view's incarnation. The last two are `None` together for an
/// entity-scoped column.
type WindowKey<'a> = (
    &'a str,
    Option<&'a str>,
    Option<tessera_types::view::ViewIncarnation>,
);

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

/// One key's contiguous window within its own subsequence of an axis's list.
#[derive(Debug, Clone)]
pub(crate) struct ColumnWindow<E> {
    pub(crate) column: String,
    /// `None` for an entity-scoped column. A group-scoped family's views share one column name.
    pub(crate) view: Option<String>,
    /// `None` exactly when `view` is.
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

pub(crate) struct CoalesceContext {
    pub(crate) prefix_dir: PathBuf,
    pub(crate) prefix: String,
    /// Where this pass writes, prefix-relative. Never reused: two passes writing one path would
    /// truncate files the other has memory-mapped.
    pub(crate) out_rel: String,
}

/// A consumed window and the entry that replaces it.
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

pub(crate) type OpenedTier = (String, Arc<DeltaTier>);

/// A coalesce whose files are durable, awaiting the manifest edit on the executor. `None` or
/// empty is a kind the pass did not take.
pub(crate) struct CompletedCoalesce {
    pub(crate) partition: String,
    pub(crate) prefix: String,
    pub(crate) tier: Option<Merged<Vec<String>, OpenedTier>>,
    /// The consumed locator extents and one covering their union span, naming the merged run.
    pub(crate) run: Option<Merged<Vec<LocatorExtent>, LocatorExtent>>,
    pub(crate) dict: Option<Merged<Vec<DictExtent>, DictExtent>>,
    /// Opened on the pool, so publishing is a pointer push that cannot fail on IO after the
    /// manifest edit.
    pub(crate) attrs: Vec<Merged<ColumnWindow<AttrExtent>, crate::filter::OpenedExtent>>,
    pub(crate) record: Option<Merged<Vec<RecordExtent>, RecordExtent>>,
    pub(crate) texts: Vec<Merged<ColumnWindow<TextExtent>, TextExtent>>,
    pub(crate) terms: Option<Merged<Vec<EntityTermsExtent>, EntityTermsExtent>>,
    pub(crate) files: BTreeMap<String, FileDigest>,
}

fn taken<E>(entries: Vec<E>) -> Option<Vec<E>> {
    (!entries.is_empty()).then_some(entries)
}

/// Writes a plan's merged files. Runs on the background pool.
pub(crate) fn execute_coalesce(
    plan: CoalescePlan,
    ctx: CoalesceContext,
) -> Result<CompletedCoalesce, MaintenanceFailed> {
    std::fs::create_dir_all(ctx.prefix_dir.join(&ctx.out_rel))
        .map_err(failed("coalesce dir"))?;
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
