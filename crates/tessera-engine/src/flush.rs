//! What a flush takes from the buffer, and the two states in which it takes nothing (§3.5).
//!
//! Planning is the executor's half of a flush: it runs against the live generation, decides which
//! buffered items acquire geometry, and hands an immutable plan to the background pool. Writing the
//! segment and publishing it are elsewhere — this module is the part where a disposition decides an
//! item's fate, which is the part that is invariant-bearing.
//!
//! ## The rules are relative to the buffer snapshot the flush took
//!
//! A delete accepted *after* the snapshot produces a deleted entity that **does** have a row,
//! hidden by its standing overlay entry alone. That is safe today only because nothing retires —
//! deletion denies never retire, there being no stamp ledger — and it is an obligation the
//! compaction spec inherits rather than a caveat this one absorbs.
//!
//! ## The two dispositions do different things, and uniformity here is fail-open
//!
//! Lifecycle §3.1 gives each a different relationship to the postings, so each gets a different
//! answer:
//!
//! - **Suppressed → flushed normally.** A suppression never touches postings and retires only on
//!   unsuppress, so a flush that skipped it would leave a later unsuppress with **nothing to
//!   reveal** — the item would have no row, and unsuppressing it would show nothing.
//! - **Deleted → never written into the segment.** The ID stays burned (I9), no row is created,
//!   and the deny entry stands.
//!
//! A third disposition, an evaluate entry, is **deleted** (decision 0048): a flush wrote the
//! buffered row's terms and let the entry stand, because writing the *entry's* current terms would
//! have been the fold — invariant-bearing, and compaction's. Nothing here needs that rule any more,
//! but the rule it protected still holds for the two above: this pass never folds.
//!
//! ## Every buffered row has a cell (§6)
//!
//! This module quantises whatever the buffer holds and does not re-check the extent. That is not an
//! omission: `Engine::accept_ingest` refuses an out-of-extent coordinate before anything is acked
//! or WAL-durable, at the boundary where rows enter the buffer, so the state a check here would
//! detect cannot arise. A second copy of the predicate is how the two would come to disagree —
//! `Quantisation::contains` is the one definition, and issue #72 (quantisation moves to
//! `ViewDescriptor`) is the change that would otherwise have to update both.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustc_hash::FxHashMap;
use tessera_authz::{write_delta_tier, DeltaTier, Dict, DictStreamWriter};
use tessera_lifecycle::wal::WalScalar;
use tessera_lifecycle::{BufferedItem, Overlay};
use tessera_spatial::tiler::{ScalarType, ScalarValue};
use tessera_store::manifest::{DictExtent, FileDigest, Quantisation, RecordExtent};
use tessera_store::permutation::SegmentExtent;
use tessera_store::read::SegmentData;
use tessera_store::{write_flush_segment, FlushInput, FlushRow};
use tessera_types::{EntityId, IdentityKey, TermId};

use crate::write::StageMark;
use crate::Generation;

/// The tag rule a tier's postings use — `postings.arrow`'s, unchanged (see
/// [`tessera_authz::write_delta_tier`]). Taken from the bundle's own `small_term_threshold` would
/// be better still; it is a constant here because a flush's postings are small by construction
/// (one tick's arrivals) and the threshold only decides an encoding, never a content.
pub(crate) const SMALL_TERM_THRESHOLD: u32 = 32;

/// The stages of one flush, for attribution under `bench-timing`.
///
/// A flush runs on two threads. The executor plans it (`Plan`), builds the pool's inputs
/// (`Dispatch`) and later publishes the completed unit: `Compose` to `Discarded` partition
/// `Executor::publish_flush`, and `PublishWall` is that call's whole duration. The pool turns the
/// plan into durable files: `Promote` to `Failed` partition [`execute_flush`], and `PoolWall` is
/// that call's whole duration. Both partitions hold whichever way the call returns: a discard or
/// a failure charges its tail to the stage of that name. `TextExtents` is itself partitioned by
/// the `Text*` stages ([`FlushStage::TEXT`]), which are in [`FlushStage::POOL`] and not in
/// [`FlushStage::EXECUTE`]: they are read beside the stage they divide, never added to it.
/// `/control/status` reports the two sets in separate maps, and
/// neither is added to the executor's [`crate::WriteStage`] laps: the pool's time is wall clock on
/// another thread, and the ingest attribution's partition (executor stages plus queueing equals
/// submit-to-receipt) holds only while those laps stay the executor's own.
///
/// Every stage is nanoseconds accumulated since the executor started. A pool stage per flush is
/// its total over `flush_executions`; a publication stage per flush is its total over `flushes`;
/// per row, divide by `flush_rows_executed` or `flush_rows_published`. All zero without
/// `bench-timing`, where the marks read no clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlushStage {
    // ---- the executor thread ----
    /// `plan_flush` over every view at the tick: the buffer scan, the item clones and the sort.
    Plan,
    /// `dispatch_flushes` up to the pool spawn: the schemas, the novel-descriptor snapshot and
    /// the context.
    Dispatch,
    /// `publish_flush`'s gates, and composing the flush's filter, record and text extents onto
    /// the live columns.
    Compose,
    /// Assembling the side-manifest from the live partition manifest, deny state included.
    Manifest,
    /// `commit_side_manifest`: the manifest write and its fsyncs. The commit point.
    Commit,
    /// `Bundle::with_segment`: the new bundle and its extended row space.
    WithSegment,
    /// Installing the segment's shape memberships into the live levels.
    ShapesInstall,
    /// The buffer clone and the removal of the consumed ids. O(buffered) on the executor.
    BufferRebase,
    /// `derive_denied` against the new row space.
    Denied,
    /// Extending every stored artifact level's held row form with the segment's rows.
    Artifacts,
    /// The generation swap, the refresh dispatch and the cache prunes.
    Swap,
    /// `rotate_wal`: the overlay snapshot, the new member and the reclaim.
    Rotate,
    /// Dropping the superseded generation, whose buffer holds every row buffered before the swap.
    /// O(buffered) on the executor when no request still holds it, beside the O(buffered) clone
    /// `BufferRebase` measures.
    DropSuperseded,
    /// A publication that discarded its flush: the time from its last lap to its return.
    Discarded,
    /// The whole of `publish_flush`, from the drain's call to its return. Overlaps `Compose`
    /// through `Discarded` rather than partitioning beside them.
    PublishWall,
    // ---- the pool ----
    /// `promote`: the dictionary-first resolve, the extent write and the postings transpose.
    Promote,
    /// Narrowing each buffered row to the segment's render tail.
    Rows,
    /// `write_flush_segment`: the Morton sort, the segment's four files and their digests, and
    /// the store's own fsyncs.
    Segment,
    /// The delta postings tier: its tally, its write and its reopen.
    DeltaTier,
    /// `write_filter_extents`: one extent per filterable column.
    FilterExtents,
    /// `write_entity_terms_extent`: the entity-to-term transpose.
    EntityTerms,
    /// `write_scoped_extents`: the group-scoped families' extents and any base a view acquired.
    ScopedExtents,
    /// `write_record_extent`: the record-blob extent.
    RecordExtent,
    /// `write_text_extents`: the text layers of the entity-scoped `text` columns. Partitioned by
    /// the five `Text*` stages that follow; a group-scoped family's text layer is written by
    /// `write_scoped_extents` and is in `ScopedExtents`, undivided.
    TextExtents,
    /// Within [`FlushStage::TextExtents`]: gathering one column's rows from the plan and creating
    /// the layer's directory.
    TextRows,
    /// Within [`FlushStage::TextExtents`]: the analyser over each row's prose
    /// (`Analyser::for_each_token`: the case fold, the normalisation and the word segmenter, each
    /// token borrowed) and, in its callback, the token's insert into the term map, a `BTreeMap`
    /// from term to its posting list. One lap for both: the callback interleaves them.
    TextTokeniseTerms,
    /// Within [`FlushStage::TextExtents`]: `write_sorted_dict` over the term map's keys, which
    /// the map already holds in order. None of the three writes fsyncs.
    TextDict,
    /// Within [`FlushStage::TextExtents`]: collecting the posting lists, encoding each and writing
    /// `postings.arrow`.
    TextPostings,
    /// Within [`FlushStage::TextExtents`]: serialising and writing the presence bitmap.
    TextPresence,
    /// Reading every file written outside the segment directory back for its SHA-256.
    Digests,
    /// Memory-mapping the segment's `morton.u32` and `columns.arrow` for the generation.
    Reopen,
    /// Resolving the segment against every spatial level of its view.
    Shapes,
    /// Dropping the plan's buffered items and the promotion's postings once the unit is built.
    /// O(rows) frees on the pool.
    DropPlan,
    /// An execution that failed: the time from its last lap to its return.
    Failed,
    /// The whole of `execute_flush`, whichever way it returns. Overlaps `Promote` through
    /// `Failed` rather than partitioning beside them. **Declared last**, which is what
    /// [`FlushStage::COUNT`] is checked against.
    PoolWall,
}

// `COUNT` sizes every array indexed by `as usize`; a variant added after `PoolWall` without
// moving this would index past them.
const _: () = assert!(FlushStage::COUNT == FlushStage::PoolWall as usize + 1);

impl FlushStage {
    pub const COUNT: usize = 35;
    /// The executor thread's stages, in the order they run. `Plan` and `Dispatch` run at the
    /// tick; the rest run at publication.
    pub const EXECUTOR: [FlushStage; 15] = [
        FlushStage::Plan,
        FlushStage::Dispatch,
        FlushStage::Compose,
        FlushStage::Manifest,
        FlushStage::Commit,
        FlushStage::WithSegment,
        FlushStage::ShapesInstall,
        FlushStage::BufferRebase,
        FlushStage::Denied,
        FlushStage::Artifacts,
        FlushStage::Swap,
        FlushStage::Rotate,
        FlushStage::DropSuperseded,
        FlushStage::Discarded,
        FlushStage::PublishWall,
    ];
    /// The stages that partition `PublishWall`.
    pub const PUBLISH: [FlushStage; 12] = [
        FlushStage::Compose,
        FlushStage::Manifest,
        FlushStage::Commit,
        FlushStage::WithSegment,
        FlushStage::ShapesInstall,
        FlushStage::BufferRebase,
        FlushStage::Denied,
        FlushStage::Artifacts,
        FlushStage::Swap,
        FlushStage::Rotate,
        FlushStage::DropSuperseded,
        FlushStage::Discarded,
    ];
    /// The pool's stages, in the order they run, the `Text*` sub-stages after the stage they
    /// partition.
    pub const POOL: [FlushStage; 20] = [
        FlushStage::Promote,
        FlushStage::Rows,
        FlushStage::Segment,
        FlushStage::DeltaTier,
        FlushStage::FilterExtents,
        FlushStage::EntityTerms,
        FlushStage::ScopedExtents,
        FlushStage::RecordExtent,
        FlushStage::TextExtents,
        FlushStage::TextRows,
        FlushStage::TextTokeniseTerms,
        FlushStage::TextDict,
        FlushStage::TextPostings,
        FlushStage::TextPresence,
        FlushStage::Digests,
        FlushStage::Reopen,
        FlushStage::Shapes,
        FlushStage::DropPlan,
        FlushStage::Failed,
        FlushStage::PoolWall,
    ];
    /// The stages that partition `PoolWall`.
    pub const EXECUTE: [FlushStage; 14] = [
        FlushStage::Promote,
        FlushStage::Rows,
        FlushStage::Segment,
        FlushStage::DeltaTier,
        FlushStage::FilterExtents,
        FlushStage::EntityTerms,
        FlushStage::ScopedExtents,
        FlushStage::RecordExtent,
        FlushStage::TextExtents,
        FlushStage::Digests,
        FlushStage::Reopen,
        FlushStage::Shapes,
        FlushStage::DropPlan,
        FlushStage::Failed,
    ];
    /// The stages that partition `TextExtents`, in the order they run for each text column.
    pub const TEXT: [FlushStage; 5] = [
        FlushStage::TextRows,
        FlushStage::TextTokeniseTerms,
        FlushStage::TextDict,
        FlushStage::TextPostings,
        FlushStage::TextPresence,
    ];
    pub fn name(self) -> &'static str {
        match self {
            FlushStage::Plan => "plan",
            FlushStage::Dispatch => "dispatch",
            FlushStage::Compose => "compose",
            FlushStage::Manifest => "manifest",
            FlushStage::Commit => "manifest_commit",
            FlushStage::WithSegment => "with_segment",
            FlushStage::ShapesInstall => "shapes_install",
            FlushStage::BufferRebase => "buffer_rebase",
            FlushStage::Denied => "denied",
            FlushStage::Artifacts => "artifacts",
            FlushStage::Swap => "swap",
            FlushStage::Rotate => "rotate",
            FlushStage::DropSuperseded => "drop_superseded",
            FlushStage::Discarded => "discarded",
            FlushStage::PublishWall => "publish_wall",
            FlushStage::Promote => "promote",
            FlushStage::Rows => "rows",
            FlushStage::Segment => "segment",
            FlushStage::DeltaTier => "delta_tier",
            FlushStage::FilterExtents => "filter_extents",
            FlushStage::EntityTerms => "entity_terms",
            FlushStage::ScopedExtents => "scoped_extents",
            FlushStage::RecordExtent => "record_extent",
            FlushStage::TextExtents => "text_extents",
            FlushStage::TextRows => "text_rows",
            FlushStage::TextTokeniseTerms => "text_tokenise_terms",
            FlushStage::TextDict => "text_dict",
            FlushStage::TextPostings => "text_postings",
            FlushStage::TextPresence => "text_presence",
            FlushStage::Digests => "digests",
            FlushStage::Reopen => "reopen",
            FlushStage::Shapes => "shapes",
            FlushStage::DropPlan => "drop_plan",
            FlushStage::Failed => "failed",
            FlushStage::PoolWall => "pool_wall",
        }
    }
}

/// One `execute_flush`'s laps, accumulated on the pool and handed to the executor's health when
/// the call returns (`ExecutorHealth::record_flush_execution`). Local rather than shared so a
/// flush still running on the pool is in no total.
#[derive(Debug)]
pub(crate) struct FlushLaps {
    /// Written by [`FlushLaps::lap`] and read by `ExecutorHealth::record_flush_execution`, both
    /// of which compile to nothing without the feature.
    #[cfg_attr(not(feature = "bench-timing"), allow(dead_code))]
    pub(crate) nanos: [u64; FlushStage::COUNT],
}

// Written out because `Default` is derived for arrays of 32 elements and fewer only.
impl Default for FlushLaps {
    fn default() -> Self {
        FlushLaps {
            nanos: [0; FlushStage::COUNT],
        }
    }
}

impl FlushLaps {
    /// Charge the time since `mark` to `stage`, and return a fresh mark. A no-op without
    /// `bench-timing`, where it returns `mark` unchanged and reads no clock.
    #[inline(always)]
    #[allow(unused_variables)]
    pub(crate) fn lap(&mut self, stage: FlushStage, mark: StageMark) -> StageMark {
        #[cfg(feature = "bench-timing")]
        {
            let now = std::time::Instant::now();
            self.nanos[stage as usize] += now.duration_since(mark.0).as_nanos() as u64;
            StageMark(now)
        }
        #[cfg(not(feature = "bench-timing"))]
        {
            mark
        }
    }
}

/// The `Text*` sub-laps of one text layer's write ([`FlushStage::TEXT`]): the pool's laps and
/// the mark they run from. Passed as `None` where the layer's time belongs to another stage.
struct TextLaps<'a> {
    laps: &'a mut FlushLaps,
    mark: &'a mut StageMark,
}

/// Charge the time since the mark to `stage` and move the mark, where there are laps to charge.
#[inline(always)]
fn text_lap(sub: &mut Option<TextLaps<'_>>, stage: FlushStage) {
    if let Some(sub) = sub {
        *sub.mark = sub.laps.lap(stage, *sub.mark);
    }
}

/// One flush's immutable plan: the items of one view that will acquire geometry.
///
/// The view is not carried: `plan_flush` is called per view and the caller already holds it, so
/// a copy here would be a second answer to a question that has one.
#[derive(Debug)]
pub(crate) struct FlushPlan {
    /// **Ascending by entity id, deleted entities already removed.** Contiguity is I9's doing —
    /// ids are issued monotonically from the high-water — and it is what makes the segment's
    /// extent dense.
    ///
    /// The segment's entity range is this list's ends, and is deliberately not carried separately:
    /// a deleted entity at either end contributes no row, so a range taken from the *buffer's*
    /// bounds would claim one it does not have.
    pub(crate) items: Vec<(EntityId, BufferedItem)>,
    /// The cells an accepted `POST /control/values` batch filled on entities that already exist
    /// (`ingest.md` §1.4), ascending by entity id.
    ///
    /// **These acquire no geometry.** A fill names an entity whose row is in a segment or in
    /// `items`, so it is in no row space, contributes no term and no external id, and is read by
    /// the value passes alone — the family's entity-space extent, the text layer and the record
    /// blob. Kept beside `items` rather than in it because the segment's entity range is `items`'
    /// ends, and an old entity's fill in that list would claim a range the segment does not have.
    ///
    /// An entity holding both an entity-scoped and a scoped fill under this view appears twice,
    /// each row carrying one tail's values and the other's absence, so no pass sees one entity's
    /// cell twice.
    pub(crate) fills: Vec<(EntityId, BufferedItem)>,
    /// Every entity-scoped fill of this view the plan looked at, written or not — what the
    /// publication removes from the buffer, on `consumed`'s rule.
    ///
    /// **Wider than `fills`, and that is the whole of it.** A restart re-buffers fills a flush has
    /// already written; those write nothing and are absent from `fills`, and a fill that is never
    /// consumed pins the log at its `ValuesBatch` record for ever.
    pub(crate) consumed_fills: Vec<EntityId>,
    /// The same for the group-scoped fills, by the `(entity, owner view)` cell they address.
    pub(crate) consumed_scoped_fills: Vec<(EntityId, String)>,
}

impl FlushPlan {
    /// The rows that carry **entity-space** facts: this entity's label, its attributes, its prose
    /// (`views.md` §4).
    ///
    /// **A join is excluded, and that exclusion is the join rule's teeth.** A joining row is the
    /// same document in a second view: its entity, its label and its entity-scoped attributes are
    /// the ones it already has, and they are already in the postings, the dictionary, the
    /// attribute columns and the record extent — put there by the flush that gave the entity its
    /// first row. Writing them again from a *second* row is how a second view would come to
    /// re-label an entity with no overlay entry, or to give one entity two values for one
    /// attribute column. The segment write below takes every row, joins included, because that is
    /// geometry and geometry is what a join contributes.
    pub(crate) fn entity_space_items(&self) -> impl Iterator<Item = &(EntityId, BufferedItem)> {
        self.items.iter().filter(|(_, item)| !item.join)
    }

    /// The rows that carry an **entity-scoped value**: [`Self::entity_space_items`] and the
    /// plan's fills, merged so the whole sequence ascends by entity id.
    ///
    /// Ascent is what the extent writers require: a column's codes are positional against its
    /// presence bitmap's own ascending order, and the record blob's rows are pushed in entity
    /// order. The merge is what a fill needs and a join does not — a fill's entity was allocated
    /// before this tick's, but a fill on an entity buffered in another view interleaves with this
    /// view's own.
    pub(crate) fn value_rows(&self) -> impl Iterator<Item = &(EntityId, BufferedItem)> {
        merge_by_entity(
            self.fills.iter(),
            self.items.iter().filter(|(_, item)| !item.join),
        )
    }

    /// The rows that carry a **group-scoped** value: every row this flush publishes, a join
    /// included (`views.md` §5 — the value belongs to the `(entity, view)` pair, which is what a
    /// join into a second view of the group brings), and the plan's fills, merged as above.
    pub(crate) fn scoped_value_rows(&self) -> impl Iterator<Item = &(EntityId, BufferedItem)> {
        merge_by_entity(self.fills.iter(), self.items.iter())
    }
}

/// Merge two runs already ascending by entity id into one ascending run.
///
/// A `Vec` of references rather than an iterator adaptor: the two runs are the plan's own and are
/// walked once per column, and a merge state machine written by hand here would be the third
/// place in this module that has to agree about ascent.
fn merge_by_entity<'a>(
    fills: impl Iterator<Item = &'a (EntityId, BufferedItem)>,
    items: impl Iterator<Item = &'a (EntityId, BufferedItem)>,
) -> std::vec::IntoIter<&'a (EntityId, BufferedItem)> {
    let mut merged: Vec<&'a (EntityId, BufferedItem)> = fills.chain(items).collect();
    merged.sort_by_key(|(entity, _)| entity.raw());
    merged.into_iter()
}

/// Why a tick published nothing. Each is a distinct operator-facing condition, and two of them are
/// fail-closed postures rather than absences of work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoFlush {
    /// Nothing buffered for this view, or everything buffered for it is deleted.
    NothingToFlush,
    /// **The WAL is poisoned** (§3.5). Under the apply-anyway rule an under-durable delete is in
    /// force in memory and was answered 500, and contracts §3.1's residual is that a restart makes
    /// the item visible again. A flush honouring such a delete would skip the entity and advance
    /// the watermark past it; replay would then discard the delete record, leaving the item in no
    /// segment and no buffer — the un-acked delete made **permanent**.
    ///
    /// Costs ingest visibility during WAL degradation, when nothing new is being made durable
    /// anyway.
    WalPoisoned,
    /// **The in-memory overlay has diverged from the durable WAL** (§7.2).
    ///
    /// `Wal::discard_undurable` deliberately does not un-apply — "a restart will not carry them" —
    /// so after an in-process recovery the node returns to `Running` while holding dispositions no
    /// record backs, and the poisoned gate no longer covers it. Publishing a manifest from that
    /// overlay would make a 500'd, never-acked deny **permanent**, contradicting contracts §3.1's
    /// residual.
    ///
    /// A diverged node keeps serving and keeps applying denies, but publishes no flush and rotates
    /// no WAL until it is restarted, alarmed throughout. Re-appending the divergent entries to
    /// converge the WAL was the alternative, and it is rejected because it produces a state **no
    /// restart could have produced** — which is lifecycle §4's central argument.
    OverlayDiverged,
    /// **A partition is serving a stepped-down side-manifest** (owner-ruled gate, 2026-08-04;
    /// write-path §5.6). A flush from this state assembles its manifest from the *older served*
    /// partition state and commits it at a higher `n`, permanently shadowing the stepped-past
    /// segment; and §7.1's buffer reconstruction against the served row space means the shadowed
    /// rows' WAL members are the only recovery material left. Publishing nothing while stepped
    /// down costs ingest visibility on a node whose newest manifest's files are damaged — the
    /// same trade the poisoned and diverged gates make, for the same reason.
    SteppedDown,
}

/// Plan a flush of `view` against `generation`.
///
/// Pure: it reads the generation and nothing else, so the same generation always yields the same
/// plan. The two postures are passed in rather than read here, because they are the executor's
/// health and not the generation's.
pub(crate) fn plan_flush(
    generation: &Generation,
    view: &str,
    wal_poisoned: bool,
    overlay_diverged: bool,
) -> Result<FlushPlan, NoFlush> {
    // The gates first, and before any work: a poisoned, diverged or stepped-down node publishes
    // nothing, and deciding that after building a plan would only mean building one to throw
    // away. Step-down is read off the generation's own bundle — it is bundle state, not executor
    // health — so it is checked here rather than passed in.
    if wal_poisoned {
        return Err(NoFlush::WalPoisoned);
    }
    if overlay_diverged {
        return Err(NoFlush::OverlayDiverged);
    }
    if generation
        .bundle
        .partitions
        .values()
        .any(|p| p.stepped_down())
    {
        return Err(NoFlush::SteppedDown);
    }

    let mut items: Vec<(EntityId, BufferedItem)> = generation
        .buffer
        .rows()
        .filter(|(entity, item)| item.view == view && !is_deleted(&generation.overlay, **entity))
        .map(|(entity, item)| (*entity, item.clone()))
        .collect();
    // **The cells an accepted values batch filled** (`ingest.md` §1.4), which acquire no geometry
    // and are written into the family's entity-space structure, the text layer and the record
    // blob alone. A deleted entity is excluded here for the reason a row is: those homes are what
    // a fill reaches, and writing one for an entity the overlay denies would give a deletion
    // something to leave behind.
    //
    // **Every fill this pass looks at is consumed, and only the ones with a cell left to write
    // are written.** A restart re-buffers fills the flush has already written; `fill_as_item`
    // drops the cells the flushed homes hold, and an emptied fill still enters `consumed` — a
    // fill that was written and never consumed would pin the log at its `ValuesBatch` record for
    // ever, since `IngestBuffer::oldest_wal_pos` counts fills.
    let mut consumed_fills: Vec<EntityId> = Vec::new();
    let mut consumed_scoped_fills: Vec<(EntityId, String)> = Vec::new();
    let mut fills: Vec<(EntityId, BufferedItem)> = Vec::new();
    // An entity-scoped fill whose named view has been dropped is written by the first live
    // view's pass: exactly one pass of a tick takes it, and the cells it writes name no view.
    let live_views = || generation.bundle.partitions.values().flat_map(|p| p.views.keys());
    let takes_orphans = live_views().min().is_some_and(|first| first == view);
    for (entity, fill) in generation.buffer.fills() {
        let orphaned = takes_orphans && !live_views().any(|live| *live == fill.view);
        if (fill.view != view && !orphaned) || is_deleted(&generation.overlay, *entity) {
            continue;
        }
        consumed_fills.push(*entity);
        let item = entity_fill_as_item(generation, *entity, fill);
        if item.scalars.iter().any(|v| !matches!(v, WalScalar::Null)) {
            fills.push((*entity, item));
        }
    }
    for ((entity, owner_view), fill) in generation.buffer.scoped_fills() {
        if fill.view != view || is_deleted(&generation.overlay, *entity) {
            continue;
        }
        consumed_scoped_fills.push((*entity, owner_view.clone()));
        let item = scoped_fill_as_item(generation, *entity, owner_view, fill);
        if item.scoped.iter().any(|v| !matches!(v, WalScalar::Null)) {
            fills.push((*entity, item));
        }
    }
    if items.is_empty() && consumed_fills.is_empty() && consumed_scoped_fills.is_empty() {
        return Err(NoFlush::NothingToFlush);
    }
    // **The arity is this generation's, and a row buffered under an earlier one is padded here**
    // (`ingest.md` §7.1). A column declared at a running service appends at the tail of
    // `declared_scalars`, so a row the log carried at the shorter arity, replayed into the
    // buffer, holds nothing for it; every read of `item.scalars` by declared position below this
    // plan (the render indices, the filter, text and record schemas) is against the padded row,
    // so one flush writes one schema.
    let declared = &generation.bundle.manifest.declared_scalars;
    for (_, item) in items.iter_mut().chain(fills.iter_mut()) {
        crate::attributes::pad_to_schema(&mut item.scalars, declared);
    }
    // The buffer is a hash map, so order is arbitrary until sorted. Ascending by entity id is what
    // `write_flush_segment` requires and what makes the extent dense; the fills are sorted for
    // the same reason one step out, [`FlushPlan::value_rows`] merging the two ascending runs.
    items.sort_unstable_by_key(|(entity, _)| entity.raw());
    fills.sort_by_key(|(entity, _)| entity.raw());

    Ok(FlushPlan {
        items,
        fills,
        consumed_fills,
        consumed_scoped_fills,
    })
}

/// One unflushed fill as the value passes read it: a row with the cells still owed and nothing
/// else.
///
/// The geometry, the label and the external id are the entity's own and are already written — a
/// fill creates nothing and names an entity that exists — so they are absent here, and the value
/// passes are the only ones that see it (`ingest.md` §1.4).
///
/// **A cell a flushed home already holds is dropped here**, which is what makes a replay
/// idempotent. The WAL member holding a values record is reclaimed on its own schedule, so a
/// restart re-buffers fills a flush has already written; writing them again would put a second
/// claimant on one column, which the extent composition refuses and the record blob would answer
/// two rows for. The fill rule refused any cell held *differently* when the batch was accepted, so
/// what is dropped here is exactly a restatement of what is stored.
fn entity_fill_as_item(
    generation: &Generation,
    entity: EntityId,
    fill: &tessera_lifecycle::Fill,
) -> BufferedItem {
    let manifest = &generation.bundle.manifest;
    let mut blob = crate::session::BlobRow::default();
    let mut scalars = fill.scalars.clone();
    for (at, declared) in manifest.declared_scalars.iter().enumerate() {
        let Some(value) = scalars.get(at) else {
            break;
        };
        if crate::session::scalar_is_absent(value, declared) {
            continue;
        }
        if crate::session::flushed_scalar_of(generation, entity, at, &mut blob)
            .is_some_and(|held| !crate::session::scalar_is_absent(&held, declared))
        {
            scalars[at] = WalScalar::Null;
        }
    }
    fill_item(&fill.view, scalars, Vec::new(), fill.wal_pos)
}

/// One `(entity, owner view)` cell's unflushed values as the scoped value pass reads them, on
/// [`entity_fill_as_item`]'s rule and dropping a cell the owner view's column already holds.
fn scoped_fill_as_item(
    generation: &Generation,
    entity: EntityId,
    owner_view: &str,
    fill: &tessera_lifecycle::ScopedFill,
) -> BufferedItem {
    let manifest = &generation.bundle.manifest;
    let families = crate::write::scoped_families_of_view(manifest, &fill.view);
    let mut scoped = fill.scoped.clone();
    for (at, family) in families.iter().enumerate() {
        let declared = crate::session::declared_of_scoped(family);
        let Some(value) = scoped.get(at) else {
            break;
        };
        if crate::session::scalar_is_absent(value, &declared) {
            continue;
        }
        let held = crate::session::flushed_scoped_of(generation, entity, family, owner_view)
            .is_some_and(|held| !crate::session::scalar_is_absent(&held, &declared))
            || crate::session::flushed_scoped_text_present(generation, entity, family, owner_view);
        if held {
            scoped[at] = WalScalar::Null;
        }
    }
    fill_item(&fill.view, Vec::new(), scoped, fill.wal_pos)
}

/// The row shape both fills take. One of the two tails is empty: an entity-scoped fill writes no
/// scoped cell and a scoped fill writes no entity-scoped one, so an entity holding both is two
/// rows of the plan and each pass sees a value in exactly one of them.
fn fill_item(
    view: &str,
    scalars: Vec<WalScalar>,
    scoped: Vec<WalScalar>,
    wal_pos: Option<u64>,
) -> BufferedItem {
    BufferedItem {
        terms: Vec::new(),
        view: view.to_string(),
        join: false,
        x: 0.0,
        y: 0.0,
        scalars,
        scoped,
        external_id: None,
        wal_pos,
    }
}

/// Everything the background pool needs to turn a [`FlushPlan`] into durable files.
///
/// Taken from the generation on the executor thread and then **immutable**: the pool holds no
/// reference to live state, which is what makes "execute on the pool over immutable inputs" (§1.1)
/// true rather than a description of intent.
pub(crate) struct FlushContext {
    pub(crate) prefix_dir: PathBuf,
    pub(crate) partition: String,
    pub(crate) view: String,
    /// The incarnation of `view` this flush writes into (decision 0115), resolved from the
    /// generation's manifest when the flush was planned and stamped into every artifact it
    /// writes. A view whose incarnation the manifest cannot resolve is not flushed at all.
    pub(crate) incarnation: tessera_types::view::ViewIncarnation,
    pub(crate) seg_id: String,
    pub(crate) row_base: u32,
    pub(crate) identity_key: IdentityKey,
    pub(crate) shard_id: u32,
    pub(crate) quantisation: Quantisation,
    pub(crate) scalar_schema: Vec<(String, ScalarType)>,
    /// Where each of `scalar_schema`'s columns sits in a buffered row's scalar list
    /// (`Manifest::render_indices`). A buffered row carries every declared column and a segment's
    /// tail carries only the render ones, so the two lists are the same length only where the
    /// schema declares nothing `filter`-only.
    pub(crate) render_indices: Vec<usize>,
    /// The filterable columns, and where each one's value sits in a buffered row's positional
    /// scalar list. Disjoint from `scalar_schema`'s purpose and often from its contents: that one
    /// is the *render* tail the segment writer takes, this one is entity-space and never reaches a
    /// row (per-point-attributes §3.9).
    pub(crate) filter_schema: Vec<FilterColumnSpec>,
    /// The blob-resident columns — neither indexed nor rendered, and never a category
    /// (records §4.2) — and where each one's value sits in a buffered row's positional scalar
    /// list. The third home's schema, beside the other two's.
    pub(crate) record_schema: Vec<RecordColumnSpec>,
    /// This view's **group-scoped** attribute families (`views.md` §5), in the owning group's
    /// manifest order — which is the order a buffered row's `scoped` list is positional against.
    /// Empty for a plain view and for a view whose key is in no scope.
    ///
    /// **A view of a group that only *shares* the keys has these too** (decision 0116): the address
    /// of a scoped value is the key, so either door writes the same cell, and this flush writes it
    /// into the owner's column — see [`FlushContext::scoped_view`].
    pub(crate) scoped_schema: Vec<ScopedColumnSpec>,
    /// The view id this flush's **scoped** columns are addressed by — the owning group's view of
    /// the same key, and [`FlushContext::view`] itself everywhere that is the same thing
    /// (`write::scoped_owner_view_of`, decision 0116).
    ///
    /// **Only the scoped columns take it.** Everything else this context writes belongs to the row
    /// space, which is the flush's own view; a scoped family's column belongs to the
    /// `(attribute, group, key)` cell, which a sharing group's view addresses under the owner's id.
    /// A flush that used `view` for both would put a sharing door's values in a directory no leaf
    /// resolves to and no reader opens — served as absence, with no error anywhere.
    pub(crate) scoped_view: String,
    /// The incarnation of [`FlushContext::scoped_view`] (decisions 0115, 0116): the cell's
    /// directory carries the **owner** view's incarnation, which under a sharing door is not
    /// [`FlushContext::incarnation`]'s view.
    pub(crate) scoped_incarnation: tessera_types::view::ViewIncarnation,
    /// One entry per lane in [`FlushContext::scalar_schema`]'s **scoped suffix**, giving where
    /// that lane's value sits in a buffered row's `scoped` list — `None` for a lane this view
    /// renders and does not write, which since decision 0116 is only a family this view's batches
    /// could not have named (`views.md` §3.3, §5).
    ///
    /// **Positional against the schema's suffix, exactly as `render_indices` is against its
    /// prefix.** A writer pairs each buffer with the next column's name, so a list built on a
    /// different predicate from the one that produced the schema would caption a family's values
    /// with another's name — and the schema is `write::view_scalar_schema_of`'s, the same one a
    /// merge and a fold of this view take.
    pub(crate) scoped_render: Vec<Option<usize>>,
    /// The indexed `text` columns, each with the analyser its declaration resolved. Separate from
    /// [`FlushContext::filter_schema`] because a text column has no value column for that pass to
    /// write — its extent is a dictionary, postings and presence, and nothing per entity.
    pub(crate) text_schema: Vec<TextColumnSpec>,
    /// The dictionary the plan's terms were resolved against, and the one promotion extends.
    pub(crate) dict: Arc<Dict>,
    /// The descriptor bytes behind every **extension** term id this plan's items carry (§3.2).
    ///
    /// **The inverse of `DescriptorResolver`'s extension map, and only the part this plan needs.**
    /// The buffer holds resolved `TermId`s and not descriptors, which is what made promotion look
    /// like it needed a WAL read; the resolver has held the bytes all along, deduped and carried
    /// across replay so the assignment stays continuous for the process's lifetime. Restricted to
    /// the plan's own ids because a descriptor no flushed item references has no posting to write,
    /// and interning it would put a descriptor into a durable artefact for no reason.
    ///
    /// Empty in the steady state, and `dispatch_flushes` does not take the resolver's lock to
    /// build it unless some planned item actually carries an extension id.
    pub(crate) novel_descriptors: FxHashMap<TermId, Vec<u8>>,
    /// The plugin's declared `max_distinct_terms`. Promotion is the one place a *caller* can grow
    /// the dictionary, so it is the one bound that is enforced rather than declared — see
    /// [`promote`].
    pub(crate) max_distinct_terms: u64,
    pub(crate) prefix: String,
    /// The view's spatial levels, as held when the flush was planned — what the new segment's
    /// rows are resolved against on the pool, before publication (`polygon-membership.md` §6.3,
    /// ruling (k); write-path §4.3).
    pub(crate) shapes: Vec<Arc<crate::shapes::ShapeLevel>>,
}

/// A flush whose files are durable, awaiting manifest assembly and the swap on the executor.
///
/// **The side-manifest is still the commit point (§7.3), and it is written at publication, not
/// here.** A crash while one of these is in flight leaves orphan files nothing references, and
/// the next tick re-plans; only once `publish_flush` has written the manifest does a crash leave
/// a bundle that opens at the new `n` with everything it names present. The manifest moved to
/// the executor because `n` cannot be allocated at plan time (a deny publication may take one
/// mid-flight) and the deny fields must reflect the overlay at publication — see the field docs
/// below.
pub(crate) struct CompletedFlush {
    pub(crate) partition: String,
    pub(crate) view: String,
    /// The entity ids removed from the buffer at publication. **Exactly what was consumed**, not a
    /// range: the rebase removes these from the *then-current* buffer, whatever arrived while the
    /// flush ran (§1.2).
    pub(crate) consumed: Vec<EntityId>,
    /// The entity-scoped fills this flush's plan consumed, removed from the buffer at publication
    /// on `consumed`'s rule (`ingest.md` §1.4) — every fill the plan looked at, not only the ones
    /// with a cell left to write, so a fill a restart re-buffered after its flush stops pinning
    /// the log.
    pub(crate) filled: Vec<EntityId>,
    /// The same for the group-scoped fills, by the `(entity, owner view)` cell they address.
    pub(crate) filled_scoped: Vec<(EntityId, String)>,
    /// The row space this flush gave the plan's rows, or `None` where it had none to give.
    ///
    /// **A values-only tick publishes no segment** (`ingest.md` §1.4). A fill acquires no
    /// geometry, so a tick whose only work is fills has no row for a segment to hold — and a
    /// segment with no rows is not publishable (`tessera_store::write_flush_segment`). The value
    /// extents, the record blob's layer and the text layers are written and published without
    /// one, which is what makes a fill visible at the tick on a corpus that is not also ingesting.
    pub(crate) segment: Option<SegmentFlush>,
    /// `Some` iff this flush promoted (§3.2); its digest is already in `files`.
    pub(crate) dict_extent: Option<DictExtent>,
    /// One entry per filterable column: this flush's values for the entities it published
    /// (`filter-index.md` §2.1). Their digests are already in `files`, and each is opened on the
    /// pool so that publication composes a pointer rather than doing file IO on the executor.
    pub(crate) filter_extents: Vec<FlushedExtent>,
    /// This flush's record-blob extent — the flushed entities' blob rows in their own blocks,
    /// has-row bitmap and directory (records §7) — or `None` where the schema declares no
    /// blob-resident column, in which case the file set owes nothing (the set stays a function of
    /// the schema, index §2.5's property). The three files' digests are already in `files`;
    /// publication pushes this entry onto the manifest's `record_extents` and nothing more —
    /// unlike a filter extent, no live reader composes it, because drill-down opens the stack
    /// from the manifest.
    pub(crate) record_extent: Option<RecordExtent>,
    /// This flush's slice of the entity→term transpose — the term lists of the entities it
    /// minted (contracts §2.4). **Never `None`**, unlike the record extent: the blob's shape is a
    /// function of the schema and a corpus may declare no blob-resident column, while every
    /// entity has a label set, the empty one included. A flush that published only joins writes
    /// an empty layer rather than none, so the manifest's list stays a complete history of what
    /// each flush minted.
    pub(crate) entity_terms_extent: tessera_store::manifest::EntityTermsExtent,
    /// The `(family, view)` pairs this flush gave a column — a view of a group the family had no
    /// column for, which is every view created since the build (`views.md` §5). "A column" is an
    /// entity-space one for an indexed family, whose empty base this flush also wrote, and a lane
    /// in the row tail for a rendered one, which has no base to write.
    ///
    /// **Published into `MANIFEST.groups[..].scoped_scalars[..].views`** and into the
    /// side-manifest's `scoped_columns`, which is what a restart derives that list back from:
    /// `/v1/meta`'s `scoped_scalars[..].views` reports it, the opener walks it, and a request's
    /// render list is decided by it, so a view left off renders nothing and is opened for
    /// nothing.
    pub(crate) scoped_columns: Vec<(String, String)>,
    /// The incarnation of [`FlushContext::view`] this flush wrote under (decision 0115), carried
    /// out so the publication can stamp the side-manifest entries that outlive a drop.
    pub(crate) incarnation: tessera_types::view::ViewIncarnation,
    /// This flush's text layers, one per indexed `text` column. Composed onto the live generation
    /// at publication, exactly as a filter extent is: a `match` over a batch flushed since the
    /// build must see it without waiting for a fold.
    pub(crate) text_extents: Vec<tessera_store::manifest::TextExtent>,
    /// Every file this flush wrote, prefix-relative, with its digest — computed on the pool.
    pub(crate) files: std::collections::BTreeMap<String, FileDigest>,
    /// The dictionary including this flush's promotions (§3.2), republished with the geometry.
    pub(crate) dict: Arc<Dict>,
    /// `Some(len)` if this flush wrote a dictionary extent, carrying the dictionary length its
    /// ordinals were assigned from; `None` if it promoted nothing.
    ///
    /// **What the publication guard reads.** A dict extent's ordinals are positions in the
    /// concatenation of `dict_extents` in listed order, so they are correct only if the extent
    /// lands where the flush assumed. A flush that promoted nothing has no such dependency — its
    /// tier names only ordinals below `len`, which append-only extension preserves — so the guard
    /// is scoped to this being `Some`.
    pub(crate) promoted_from_dict_len: Option<u32>,
    pub(crate) prefix: String,
}

/// The half of a completed flush that exists only where the plan gave rows geometry.
///
/// Grouped rather than eight `Option`s: they are published together or not at all — a segment
/// with no descriptor is a file no manifest names, and a descriptor with no locator extent is a
/// segment whose external ids nothing resolves — so one `Option` is the shape that cannot be
/// half-taken.
pub(crate) struct SegmentFlush {
    pub(crate) segment: SegmentData,
    pub(crate) extent: SegmentExtent,
    /// **The manifest's ingredients, not a manifest.** Contracts §2.3 makes a side-manifest
    /// complete current state for its partition, and *current* is decided at publication: the
    /// executor assembles this into the live partition manifest, with deny fields serialised
    /// fresh from the overlay of the generation being published. A manifest cloned at plan time
    /// would carry the deny state of a snapshot the flush's own flight has outlived.
    pub(crate) descriptor: tessera_store::manifest::SegmentDescriptor,
    pub(crate) watermark: u64,
    pub(crate) entity_id_high_water: u64,
    pub(crate) external_id_run: String,
    pub(crate) locator_extent: tessera_store::manifest::LocatorExtent,
    pub(crate) tier: Arc<DeltaTier>,
    /// The tier's prefix-relative path — `deltas`' entry for it (contracts §2.3 r18). Carried
    /// rather than re-derived at publication, because after a coalesce a tier's path is no longer
    /// a function of any segment's `seg_id`.
    pub(crate) tier_path: String,
    /// The tier measured as encoded (`FragmentationTally::of_tier`) — contracts §3.4's
    /// `fragmentation`, at the scope where between-window scatter is visible. Computed on the
    /// pool beside the write it measures; recorded by the executor only if the flush publishes.
    pub(crate) tier_tally: tessera_lifecycle::window::FragmentationTally,
    /// The segment's membership in every spatial level of its view, resolved on the pool and
    /// installed at publication.
    pub(crate) shape_pieces: Vec<crate::shapes::ShapePiece>,
}

/// Turn a plan into durable files. **Runs on the background pool, over immutable inputs** (§1.1).
///
/// The order is §7.3's, and the side-manifest is last because it is the commit point: a crash
/// before it leaves orphan files nothing references, and replay re-flushes deterministically.
///
/// `laps` receives the pool's [`FlushStage`]s: each stage's wall clock on this thread, and
/// `PoolWall` for the whole call whichever way it returns. The caller adds them to the executor's
/// health after the return.
pub(crate) fn execute_flush(
    plan: FlushPlan,
    ctx: FlushContext,
    laps: &mut FlushLaps,
) -> Result<CompletedFlush, FlushFailed> {
    let wall = StageMark::now();
    let mut mark = wall;
    let result = execute_flush_stages(plan, ctx, laps, &mut mark);
    if result.is_err() {
        // A failed flush's time since its last lap, so `PoolWall` stays partitioned whichever way
        // the call ends.
        laps.lap(FlushStage::Failed, mark);
    }
    laps.lap(FlushStage::PoolWall, wall);
    result
}

/// The pool's stages, each lapped as it ends. `mark` is left at the last lap so the caller can
/// charge a failure's tail.
fn execute_flush_stages(
    plan: FlushPlan,
    ctx: FlushContext,
    laps: &mut FlushLaps,
    mark: &mut StageMark,
) -> Result<CompletedFlush, FlushFailed> {
    let consumed: Vec<EntityId> = plan.items.iter().map(|(entity, _)| *entity).collect();
    let filled = plan.consumed_fills.clone();
    let filled_scoped = plan.consumed_scoped_fills.clone();

    // ---- promotion (§3.2) -------------------------------------------------------------------
    //
    // `buffer.rs` allocates term ids for descriptors the dictionary has never seen from the top of
    // the `u32` range downward, precisely so they are unsatisfiable: a novel descriptor can buffer
    // an item but can never make it visible. This is where that ends for the items being flushed.
    //
    // **The tier's postings are written in promoted ordinals, never extension ids.** An extension
    // id is process-local and its meaning changes at the next replay, so a tier carrying one would
    // name whatever descriptor interned into that slot next — the same hazard `buffer.rs` counts
    // downward from `u32::MAX` to avoid, arriving by a different route.
    let promotion = promote(&plan, &ctx)?;
    let promoted_from = promotion.extent.as_ref().map(|_| ctx.dict.len());
    *mark = laps.lap(FlushStage::Promote, *mark);

    // ---- the segment, its extents and its locator -------------------------------------------
    //
    // **The row's scalars are narrowed to the render columns, positionally.** A buffered row
    // carries one value per *declared* column — that is the contract the commit window indexes a
    // category key by — while the segment's tail is the render subset (`scalar_schema_of`), which
    // is what keeps a `filter`-only column out of the hot column entirely (§10.3). Handing the
    // writer the full list pairs each value with the next render column's name, and the writer
    // catches that only where the two types happen to differ.
    //
    // **A tick whose only work is fills writes no segment** (`ingest.md` §1.4). A fill acquires
    // no geometry, so there is no row for a segment to hold, and a segment with no rows is not
    // publishable; the value extents below are written and published without one.
    let mut rows: Vec<FlushRow> = Vec::with_capacity(plan.items.len());
    for (entity, item) in &plan.items {
        let mut scalars = Vec::with_capacity(ctx.render_indices.len() + ctx.scoped_render.len());
        // `scalar_schema`'s prefix is positionally parallel to `render_indices` — both are the
        // render subset in declaration order — so this takes exactly that subset's values, in the
        // order the writer's own schema names them; its suffix is `scoped_render`'s, below.
        for &index in &ctx.render_indices {
            let value = item.scalars.get(index).ok_or_else(|| {
                FlushFailed(format!(
                    "a buffered row carries {} scalars, but a render column is declared at \
                     position {index}",
                    item.scalars.len()
                ))
            })?;
            // **The absence travels, and is not resolved here.** `write_flush_segment` records it
            // in the column's presence bitmap and only then writes the type's zero into the
            // non-nullable column (contracts R4, decision 0064) — and it must, because the bitmap
            // is over rows and the sort that decides them happens inside that call. Substituting
            // the zero here would leave the writer nothing to tell "scores zero" from "has no
            // score".
            scalars.push(to_scalar_value(value));
        }
        // **The group-scoped render lanes, after the declared ones** (`views.md` §5) — the order
        // the build writes them in, and the order `scalar_schema`'s suffix names them. A row that
        // carried no value for a family takes the absence the entity-scoped lanes take: it travels
        // as `WalScalar::Null` and `write_flush_segment` records it in the lane's presence bitmap
        // before writing the type's zero (decision 0064).
        for index in &ctx.scoped_render {
            // `None` is a lane this view renders and does not write — a view of a group that only
            // shares the family's views (`views.md` §5). It takes the absence every other absence
            // takes, and the lane is written so that every segment of the view holds the same
            // columns.
            let value = index
                .and_then(|index| item.scoped.get(index))
                .unwrap_or(&WalScalar::Null);
            scalars.push(to_scalar_value(value));
        }
        rows.push(FlushRow {
            entity_id: *entity,
            external_id: item.external_id.clone(),
            x: item.x,
            y: item.y,
            scalars,
        });
    }
    *mark = laps.lap(FlushStage::Rows, *mark);
    let out = if rows.is_empty() {
        None
    } else {
        Some(
            write_flush_segment(
                &ctx.prefix_dir,
                &ctx.partition,
                &ctx.view,
                FlushInput {
                    seg_id: &ctx.seg_id,
                    incarnation: ctx.incarnation,
                    rows,
                    quantisation: ctx.quantisation,
                    identity_key: &ctx.identity_key,
                    shard_id: ctx.shard_id,
                    scalar_schema: &ctx.scalar_schema,
                    row_base: ctx.row_base,
                },
            )
            .map_err(|e| FlushFailed(format!("segment: {e}")))?,
        )
    };
    *mark = laps.lap(FlushStage::Segment, *mark);

    // ---- the delta postings tier ------------------------------------------------------------
    //
    // The tier belongs to the segment: it carries the postings of the entities the segment gave
    // rows, and a values-only tick promotes nothing and has none.
    let tier_bits = if out.is_some() {
        let tier_tally = tessera_lifecycle::window::FragmentationTally::of_tier(
            &promotion.postings,
            plan.items.len() as u64,
        );
        let tier_rel = format!(
            "partitions/{}/{}/segments/{}/delta.arrow",
            ctx.partition,
            tessera_store::view_rel(&ctx.view),
            ctx.seg_id
        );
        let tier_path = ctx.prefix_dir.join(&tier_rel);
        write_delta_tier(&tier_path, &promotion.postings, SMALL_TERM_THRESHOLD)
            .map_err(|e| FlushFailed(format!("delta tier: {e}")))?;
        let tier =
            Arc::new(DeltaTier::open(&tier_path).map_err(|e| FlushFailed(format!("tier: {e}")))?);
        Some((tier, tier_rel, tier_tally))
    } else {
        None
    };
    *mark = laps.lap(FlushStage::DeltaTier, *mark);

    // ---- the manifest's ingredients, not the manifest ---------------------------------------
    //
    // **This function no longer writes the side-manifest.** It computes everything the manifest
    // will name — the files and their digests, all on the pool where the IO belongs — and the
    // executor assembles and writes it at publication (`Executor::publish_flush`). Two reasons,
    // both structural. `n` cannot be allocated here: a deny publication may take one while this
    // flush is in flight, and a manifest committed at a lower `n` than the newest is a manifest a
    // restore never reads — the segment silently lost. And the deny fields must reflect the
    // overlay at *publication*, not at plan time, which is a thing only the executor holds.
    //
    // **Every file written below the segment's own four is digested in one pass, after the last
    // write.** The segment writer digests its own files as part of `out.files`; everything else
    // this function writes is collected here by prefix-relative path and read back under one
    // stage, so the digest cost is attributable on its own rather than spread across the stages
    // that wrote the files.
    let mut files = out
        .as_ref()
        .map(|out| out.files.clone())
        .unwrap_or_default();
    let mut to_digest: Vec<String> = tier_bits
        .as_ref()
        .map(|(_, rel, _)| vec![rel.clone()])
        .unwrap_or_default();
    let dict_extent = match promotion.extent {
        Some(extent) => {
            to_digest.push(extent.path.clone());
            Some(extent)
        }
        None => None,
    };

    // ---- the filter columns' extents (filter-index §2.1) ------------------------------------
    let filter_extents = write_filter_extents(&plan, &ctx)?;
    for extent in &filter_extents {
        // The dictionary is digested with the pair it belongs to, not beside it: a keyword
        // extent's ordinals cannot be read at all without it, so a bundle whose manifest named the
        // values and omitted the dictionary would be one this check called complete (records §7).
        for rel in [&extent.values_rel, &extent.presence_rel]
            .into_iter()
            .chain(extent.dict_rel.as_ref())
        {
            to_digest.push(rel.clone());
        }
    }
    *mark = laps.lap(FlushStage::FilterExtents, *mark);

    // ---- the entity→term transpose extent (contracts §2.4) ----------------------------------
    //
    // Written from the promotion, so its ordinals are the durable ones the tier beside it carries.
    let entity_terms_extent = write_entity_terms_extent(&promotion.per_entity, &ctx)?;
    for rel in entity_terms_extent.files() {
        to_digest.push(rel.to_string());
    }
    *mark = laps.lap(FlushStage::EntityTerms, *mark);

    // ---- the group-scoped column families' extents (`views.md` §5) --------------------------
    //
    // Beside the entity-scoped extents above and composed at publication exactly as they are —
    // the only thing the scope changes is which directory the files land in.
    let ScopedWrite {
        extents: scoped_extents,
        texts: scoped_texts,
        created: scoped_columns,
    } = write_scoped_extents(&plan, &ctx)?;
    for extent in &scoped_extents {
        for rel in [&extent.values_rel, &extent.presence_rel]
            .into_iter()
            .chain(extent.dict_rel.as_ref())
        {
            to_digest.push(rel.clone());
        }
    }
    // **The base a view acquired at this flush is digested too.** It is named by
    // `MANIFEST.files` nowhere — the build wrote no such view — so the side-manifest is where it
    // enters the bundle's file set, and a base outside it is a file `ensure_verified` finds
    // unaccounted for.
    for (column, view) in &scoped_columns {
        // The writer's own derivation, not a second copy of it: the base is digested at the path
        // it was written to, incarnation suffix included (decision 0115).
        let rel =
            tessera_store::scoped_column_rel(&ctx.partition, column, view, ctx.scoped_incarnation);
        for name in [
            tessera_filter::VALUES_FILE,
            tessera_filter::PRESENCE_FILE,
            tessera_filter::DICT_FILE,
            "postings.arrow",
        ] {
            if ctx.prefix_dir.join(&rel).join(name).exists() {
                to_digest.push(format!("{rel}/{name}"));
            }
        }
    }
    let filter_extents = {
        let mut all = filter_extents;
        all.extend(scoped_extents);
        all
    };
    *mark = laps.lap(FlushStage::ScopedExtents, *mark);

    // ---- the record-blob extent (records §7) ------------------------------------------------
    let record_extent = write_record_extent(&plan, &ctx)?;
    if let Some(extent) = &record_extent {
        for rel in extent.files() {
            to_digest.push(rel.to_string());
        }
    }
    *mark = laps.lap(FlushStage::RecordExtent, *mark);
    let mut text_extents = write_text_extents(&plan, &ctx, laps, *mark)?;
    text_extents.extend(scoped_texts);
    // **All three files of every text extent, and the omission was not cosmetic.** A digest is not
    // only an integrity check here: `publish_fold` carries a flight extent forward by looking its
    // digest up in the live manifests and *discards the whole fold* when it finds none. So a
    // deployment with an indexed text column and continuous ingest would have folded, spent minutes
    // to hours rewriting the corpus, discarded at the last step, left an orphan prefix, and retired
    // no deletion — for ever, since the next trigger re-plans into the same wall. The three travel
    // together for the reason the manifest entry states: an extent's postings are positions in its
    // own dictionary, and its presence is what stops an entity whose prose analysed to no terms
    // reading as absent.
    for extent in &text_extents {
        for rel in extent.files() {
            to_digest.push(rel.to_string());
        }
    }
    *mark = laps.lap(FlushStage::TextExtents, *mark);

    // ---- the digests, one pass over everything written above ---------------------------------
    for rel in to_digest {
        files.insert(
            rel.clone(),
            digest_of(&ctx.prefix_dir.join(&rel)).map_err(FlushFailed)?,
        );
    }
    *mark = laps.lap(FlushStage::Digests, *mark);

    let segment = match &out {
        None => None,
        Some(out) => {
            let seg_dir = segment_dir(&ctx);
            Some(
                SegmentData::load(&seg_dir, &ctx.seg_id, out.segment.row_count)
                    .map_err(|e| FlushFailed(e.to_string()))?,
            )
        }
    };
    *mark = laps.lap(FlushStage::Reopen, *mark);

    // ---- the shape memberships (`polygon-membership.md` §6.3) ------------------------------
    //
    // **Resolved here, on the pool, as part of the flush's own unit of work and before the
    // generation that carries this segment is published** — so a point ingested inside a shape is
    // a member on the next request with nothing rebuilt on that request. Interior tiles are whole
    // row ranges; the rows in boundary cells are tested one by one over their exact stored
    // position, with each cell's edges derived once for this segment. A panic here fails the
    // flush whole, exactly as a segment write would: nothing is published and the buffer stands.
    // A values-only flush wrote no segment and so resolves nothing: a fill acquires no row, and a
    // shape level's membership is over rows.
    let mut shape_pieces = Vec::with_capacity(ctx.shapes.len());
    for (level, segment) in ctx.shapes.iter().zip(segment.iter().cycle()) {
        let (rows, cost) = level.resolve(segment);
        tracing::info!(
            layer = %level.layer,
            level = level.level,
            view = %ctx.view,
            seg_id = %ctx.seg_id,
            rows = segment.row_count,
            rows_tested = cost.rows_tested,
            rows_interior = cost.rows_interior,
            artifacts_skipped = cost.artifacts_skipped,
            elapsed_ms = cost.elapsed_ms,
            "a flush resolved its segment against a spatial level's shapes"
        );
        shape_pieces.push(crate::shapes::ShapePiece {
            level: Arc::clone(level),
            rows: Arc::new(rows),
            cost,
        });
    }
    *mark = laps.lap(FlushStage::Shapes, *mark);

    // The segment half travels whole or not at all: it exists exactly where the plan gave rows
    // geometry, which is what wrote every file it names.
    let segment = match (segment, out, tier_bits) {
        (Some(segment), Some(out), Some((tier, tier_path, tier_tally))) => Some(SegmentFlush {
            segment,
            extent: out.extent,
            descriptor: out.segment,
            watermark: out.watermark,
            entity_id_high_water: out.entity_id_high_water,
            external_id_run: out.external_id_run,
            locator_extent: out.locator_extent,
            tier,
            tier_path,
            tier_tally,
            shape_pieces,
        }),
        _ => None,
    };
    let completed = CompletedFlush {
        partition: ctx.partition,
        view: ctx.view,
        consumed,
        filled,
        filled_scoped,
        segment,
        dict_extent,
        filter_extents,
        record_extent,
        entity_terms_extent,
        text_extents,
        scoped_columns,
        incarnation: ctx.incarnation,
        files,
        dict: promotion.dict,
        promoted_from_dict_len: promoted_from,
        prefix: ctx.prefix,
    };
    // **Freed here, under a stage, rather than at the return.** The plan holds one `BufferedItem`
    // per row and the promotion holds the postings in both orientations; freeing them is O(rows)
    // of allocator work that a lap at the return would leave unattributed. Measured at 0.4 to
    // 0.6 µs per row on medcpt-1m (`probes/2026-09-05-flush-attribution/`).
    drop(plan);
    drop(promotion.postings);
    drop(promotion.per_entity);
    laps.lap(FlushStage::DropPlan, *mark);
    Ok(completed)
}

/// Why a flush produced nothing. **Every failure is "nothing happened, retry next tick"** (§10):
/// the side-manifest is the only commit point, so a failure before it leaves orphan files nothing
/// references and a failure after it cannot happen — there is nothing left to fail.
#[derive(Debug)]
pub(crate) struct FlushFailed(pub(crate) String);

impl std::fmt::Display for FlushFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What promotion produced: the dictionary to republish, the extent naming the new ordinals, and
/// the tier's postings in those ordinals.
struct Promotion {
    dict: Arc<Dict>,
    extent: Option<DictExtent>,
    /// `(term, entities)` ascending by term — [`write_delta_tier`]'s contract.
    postings: Vec<(TermId, Vec<u32>)>,
    /// The same relation transposed: `(entity, terms)` ascending by entity, each list sorted and
    /// deduplicated — [`tessera_store::EntityTermsWriter`]'s contract, and the extent this flush
    /// owes `entities/terms/` (contracts §2.4).
    ///
    /// **Built here rather than beside the extent write, because this is where the ordinals are.**
    /// A term still carrying an extension id is process-local; promotion is what turns it into the
    /// durable ordinal a later session resolves against, and a transpose assembled from
    /// `item.terms` afterwards would store the process-local number.
    per_entity: Vec<(u32, Vec<u32>)>,
}

/// Promote every extension-id descriptor the plan carries to a durable dictionary ordinal, and
/// express the plan's postings in those ordinals (§3.2).
///
/// **Two fail-closed consequences, neither obvious and both left standing.** A promoted descriptor
/// is satisfiable only by sessions authorised *after* this flush, because `satisfied` is fixed per
/// session at authorise — which is also what makes §3.4's patch-equals-a-rebuild equality hold. And
/// an item still buffered under an old extension id for an already-promoted descriptor stays
/// invisible until *its own* flush, even to a viewer holding the term.
///
/// **The dictionary is consulted before anything is interned, and that is the writer half of the
/// no-duplicate rule.** An extent that repeated a descriptor the dictionary already holds would
/// make [`Dict::load`] and [`Dict::load_extending`] assign different ordinals — a running process
/// and the same bundle reopened disagreeing about what every ordinal after the repeat means. The
/// reader closes that too ([`Dict::load`]'s doc); this closes it at the source, and either alone
/// would leave a viewer being served another compartment's items with no error anywhere.
///
/// **An extension id with no descriptor fails the flush.** It cannot happen — the resolver's map
/// only grows, and rotation retains the WAL records of anything still buffered, so replay
/// re-interns every id a plan can name — but silently dropping a term is precisely the bug this
/// function existed to have, and it must not survive as the error path.
fn promote(plan: &FlushPlan, ctx: &FlushContext) -> Result<Promotion, FlushFailed> {
    let dict_len = ctx.dict.len();
    let mut by_term: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    // Descriptors this flush interns, in assignment order — the extent file's contents, and the
    // sequence `Dict::extended_with` is handed. Order is the two artefacts' shared contract.
    let mut interned: Vec<Vec<u8>> = Vec::new();
    let mut assigned: FxHashMap<u32, u32> = FxHashMap::default();

    // One entry per entity-space item, in the plan's order — including an item whose label set is
    // empty, which is a value and not an absence (`tessera_store::entity_terms`).
    let mut per_entity: Vec<(u32, Vec<u32>)> = Vec::with_capacity(plan.items.len());
    for (entity, item) in plan.entity_space_items() {
        let Ok(entity) = u32::try_from(entity.raw()) else {
            return Err(FlushFailed(format!(
                "entity {} does not fit the u32 posting space (I9's ceiling)",
                entity.raw()
            )));
        };
        let mut mine: Vec<u32> = Vec::with_capacity(item.terms.len());
        for term in &item.terms {
            // Below the dictionary's length: already a durable ordinal, nothing to do.
            let ordinal = if term.raw() < dict_len {
                term.raw()
            } else if let Some(&ordinal) = assigned.get(&term.raw()) {
                ordinal
            } else {
                let descriptor = ctx.novel_descriptors.get(term).ok_or_else(|| {
                    FlushFailed(format!(
                        "extension term {} has no descriptor in this flush's snapshot — see \
                         promote()'s doc; a term must never be dropped silently",
                        term.raw()
                    ))
                })?;
                let ordinal = match ctx.dict.lookup(descriptor) {
                    // An earlier flush already promoted it. Use that ordinal and write nothing:
                    // this is both the no-duplicate rule and what makes an item buffered under a
                    // stale extension id become visible at its own flush (§3.2).
                    Some(existing) => existing.raw(),
                    None => {
                        let next = u64::from(dict_len) + interned.len() as u64;
                        // **The one plugin bound that is enforced, not declared.** Promotion is
                        // the only path by which a caller grows the dictionary, and
                        // `EXTENSION_ID_START > max_distinct_terms` is what keeps a dictionary
                        // ordinal from ever aliasing a live extension id. Past the declaration
                        // that assertion stops holding, so this fails the flush — buffer
                        // retained, ingest sheds at its own bound, which is the intended
                        // backpressure.
                        if next >= ctx.max_distinct_terms {
                            return Err(FlushFailed(format!(
                                "promoting this flush's novel descriptors would carry the \
                                 dictionary to {next}, at or past the plugin's declared \
                                 max_distinct_terms of {}; refusing rather than assigning an \
                                 ordinal that could alias an extension id",
                                ctx.max_distinct_terms
                            )));
                        }
                        interned.push(descriptor.clone());
                        next as u32
                    }
                };
                assigned.insert(term.raw(), ordinal);
                ordinal
            };
            by_term.entry(ordinal).or_default().push(entity);
            mine.push(ordinal);
        }
        // A buffered row's descriptors are not deduplicated on the write path, so this is the same
        // required-not-defensive normalisation the postings below get.
        mine.sort_unstable();
        mine.dedup();
        per_entity.push((entity, mine));
    }
    // The plan's items are the buffer's, which is entity-ascending; sorted anyway because the
    // extent's ranks address its lists and a writer that trusted the caller's order would produce
    // a layer whose every answer is one entity out.
    per_entity.sort_unstable_by_key(|(entity, _)| *entity);

    let mut postings = Vec::with_capacity(by_term.len());
    for (term, mut entities) in by_term {
        // `encode_posting` hard-fails on a non-strictly-ascending list, and a buffered item's
        // descriptors are not deduplicated on the write path, so this is required rather than
        // defensive. Set semantics, so it folds no authorisation state.
        entities.sort_unstable();
        entities.dedup();
        postings.push((TermId::new(term), entities));
    }

    if interned.is_empty() {
        // The steady state: no extent, no dictionary clone, nothing published but the tier.
        return Ok(Promotion {
            dict: Arc::clone(&ctx.dict),
            extent: None,
            postings,
            per_entity,
        });
    }

    let seg_dir = segment_dir(ctx);
    std::fs::create_dir_all(&seg_dir).map_err(|e| FlushFailed(format!("dict extent dir: {e}")))?;
    let mut writer = DictStreamWriter::new(&seg_dir);
    for descriptor in &interned {
        writer.append(descriptor);
    }
    writer
        .finish()
        .map_err(|e| FlushFailed(format!("dict extent: {e}")))?;

    Ok(Promotion {
        // **Built from the same sequence that named the tier, never re-read from the file just
        // written.** Nothing compares the two, so a bad read would silently *become* the live
        // assignment while the tier holds the intended one. That the file agrees is a restart
        // property, proved by a test against the file.
        dict: Arc::new(ctx.dict.extended_with(&interned)),
        extent: Some(DictExtent {
            path: format!(
                "partitions/{}/{}/segments/{}/terms-0.dict",
                ctx.partition,
                tessera_store::view_rel(&ctx.view),
                ctx.seg_id
            ),
            records: interned.len() as u64,
        }),
        postings,
        per_entity,
    })
}

/// One filterable column, and where its value sits in a buffered row's positional scalar list.
///
/// **The index is positional against `MANIFEST.declared_scalars`**, which is the same contract the
/// commit window indexes a row's scalars by when it resolves a category key to a code. Carrying the
/// index rather than looking the column up by name at the write is what keeps the two from
/// disagreeing about which value belongs to which column.
#[derive(Debug, Clone)]
pub(crate) struct FilterColumnSpec {
    pub(crate) index: usize,
    pub(crate) name: String,
    pub(crate) ty: ScalarType,
    /// A category, so its values are vocabulary codes and code 0 means *absent*.
    pub(crate) category: bool,
}

/// One flush's extent for one column: durable, digested by the caller, and open.
///
/// **The three paths travel together because the layer's files must swap atomically** (records §7,
/// review B2). An extent's ordinals are positions in *that extent's own* dictionary, so a reader
/// that saw a new dictionary beside old ordinals would recolour the window's values with no error
/// anywhere. Carrying the dictionary here — rather than letting the publication rediscover it from
/// a path convention — is what puts all three in one manifest record.
pub(crate) struct FlushedExtent {
    pub(crate) column: String,
    /// The view whose column of a **group-scoped family** this extends, or `None` for an ordinary
    /// entity-scoped column ([`tessera_store::manifest::AttrExtent::view`], `views.md` §5).
    pub(crate) view: Option<String>,
    pub(crate) values_rel: String,
    pub(crate) presence_rel: String,
    /// The extent's own sorted dictionary — keyword columns only, `None` for every family whose
    /// values file carries the values themselves.
    pub(crate) dict_rel: Option<String>,
    /// Opened here on the pool, so publication is a pointer push on the executor thread.
    pub(crate) values: Arc<tessera_filter::ValueColumn>,
    /// The dictionary those values are ordinals into, opened on the pool for the reason beside
    /// it — and travelling with them, because an extent's ordinals mean nothing against any other
    /// dictionary. Publication takes the pair or neither (`filter.rs`'s composition refuses a
    /// half), which is the in-process half of the atomic swap `AttrExtent` makes on disc.
    pub(crate) dict: Option<Arc<tessera_filter::SortedDict>>,
}

/// Write one extent per filterable column, covering exactly the entities this flush publishes.
///
/// **Every declared filter column gets one, including a column no flushed entity carries a value
/// in.** The file set is then a function of the schema rather than of the data, so what a flush
/// produces is predictable from the manifest alone; an empty extent costs a few hundred bytes and
/// composes to nothing.
///
/// **A deleted entity is already gone from the plan**, so it acquires no slot here any more than it
/// acquires a row — which is what keeps write-path §5.4's Rule F the only route by which a deletion
/// touches an artefact, rather than this pass quietly becoming a second one.
///
/// **A keyword's sort and front-code happen here, on the pool, and that placement is load-bearing**
/// (records §7, review N9; write-path §4.3). This function runs at flush *execution*; the serial
/// group-commit section write-path §2.3 defines is upstream of it and its latency is shared by
/// every ingest and every deny in flight. A batch's distinct values are bounded by the batch, and
/// there is no shared dictionary to promote into — which is what makes the near-unique-string
/// quadratic hazard (filter-index §5's history) impossible here rather than merely avoided.
fn write_filter_extents(
    plan: &FlushPlan,
    ctx: &FlushContext,
) -> Result<Vec<FlushedExtent>, FlushFailed> {
    let mut out = Vec::with_capacity(ctx.filter_schema.len());
    for spec in &ctx.filter_schema {
        let column = extent_values(spec, entity_scoped_rows(spec, plan)?)?;
        let column_rel = format!("partitions/{}/attrs/{}", ctx.partition, spec.name);
        let column_dir = ctx.prefix_dir.join(&column_rel);
        let (values_path, presence_path, dict_path) = tessera_filter::write_extent(
            &column_dir,
            &ctx.seg_id,
            &column.codes,
            &column.presence,
            column.dict_keys.as_deref(),
        )
        .map_err(|e| FlushFailed(format!("filter extent for '{}': {e}", spec.name)))?;
        // Derived from the paths just written rather than formatted a second time: the manifest
        // names what is on disk, or it names nothing.
        let rel = |path: &PathBuf| -> Result<String, FlushFailed> {
            path.strip_prefix(&ctx.prefix_dir)
                .ok()
                .and_then(|p| p.to_str())
                .map(|p| p.to_string())
                .ok_or_else(|| {
                    FlushFailed(format!(
                        "filter extent path {} is not under the prefix",
                        path.display()
                    ))
                })
        };
        let values = tessera_filter::open_extent(
            &values_path,
            &presence_path,
            tessera_filter::Access::Mapped,
        )
        .map_err(|e| FlushFailed(format!("filter extent for '{}': {e}", spec.name)))?;
        let dict = dict_path
            .as_ref()
            .map(|path| {
                tessera_filter::SortedDict::open(path, tessera_filter::Access::Mapped)
                    .map(Arc::new)
                    .map_err(|e| {
                        FlushFailed(format!("keyword dictionary for '{}': {e}", spec.name))
                    })
            })
            .transpose()?;
        out.push(FlushedExtent {
            column: spec.name.clone(),
            view: None,
            values_rel: rel(&values_path)?,
            presence_rel: rel(&presence_path)?,
            dict_rel: dict_path.as_ref().map(rel).transpose()?,
            values: Arc::new(values),
            dict,
        });
    }
    Ok(out)
}

/// One column's values for this flush's entities, and the entities that carry one.
///
/// **Absence is out of band, and each family spends a different thing on it** — the same rule the
/// batch build writes by (`tessera-build`'s `write_column_values`), restated here because the two
/// read different shapes: the build reads a `ScalarValue` out of a points file, and this reads the
/// `WalScalar` the buffer holds, on the other side of a crate boundary the layer script draws. A
/// category spends the reserved code 0, which its vocabulary keeps out of the value space, so an
/// item carrying it gets no slot at all. Every other family has no spare value to spend — every bit
/// pattern of a number is a legal number, and contracts §2.4 refuses the empty string on the ingest
/// plane precisely because an unset field and a client bug both produce it — so absence travels as
/// `WalScalar::Null` and lands in the presence bitmap (decision 0064).
///
/// A value of the wrong shape for its declared column **fails the flush** rather than being
/// dropped: the commit window narrows a category key to its declared width before the row is
/// buffered, so a mismatch here is a defect on the write path and not caller input, and filtering
/// on a value this code invented is worse than not flushing.
fn extent_values<'a>(
    spec: &FilterColumnSpec,
    entities: Vec<(u32, &'a WalScalar)>,
) -> Result<ExtentColumn<'a>, FlushFailed> {
    use tessera_filter::Codes;

    let mut presence = croaring::Bitmap::new();

    let wrong = |value: &WalScalar| {
        FlushFailed(format!(
            "column '{}' is declared {:?} but a buffered row carries {value:?}",
            spec.name, spec.ty
        ))
    };

    if spec.ty == ScalarType::Keyword {
        // The batch's own present values, in entity order — the slot sequence. A keyword arrives
        // as a string on the wire and in the WAL (records §7): the ordinal is minted below, here,
        // and exists nowhere upstream of this layer.
        let mut held: Vec<&str> = Vec::with_capacity(entities.len());
        for (entity, value) in entities {
            if matches!(value, WalScalar::Null) {
                continue;
            }
            let WalScalar::Utf8(text) = value else {
                return Err(wrong(value));
            };
            presence.add(entity);
            held.push(text.as_str());
        }
        // **This extent's own dictionary, over this batch alone** (records §7). Its ordinals are
        // positions in it and are not comparable with the base's or any other extent's; the
        // coalesce remaps them and the fold rebuilds them from nothing, which is what keeps term
        // identity layer-scoped and never durable.
        let mut keys: Vec<&str> = held.clone();
        keys.sort_unstable();
        keys.dedup();
        let mut ordinals = Vec::with_capacity(held.len());
        for text in held {
            let ordinal = keys.binary_search(&text).map_err(|_| {
                FlushFailed(format!(
                    "column '{}': the value {text:?} is absent from the dictionary built from it",
                    spec.name
                ))
            })?;
            ordinals.push(ordinal as u32);
        }
        return Ok(ExtentColumn {
            codes: Codes::U32(ordinals.into()),
            presence,
            dict_keys: Some(keys),
        });
    }

    if spec.category {
        let mut held: Vec<u32> = Vec::with_capacity(entities.len());
        for (entity, value) in entities {
            let code = match value {
                WalScalar::U8(c) => u32::from(*c),
                WalScalar::U16(c) => u32::from(*c),
                WalScalar::U32(c) => *c,
                // The ingest plane resolves a null key to the reserved code, so this arm is
                // belt-and-braces rather than the live route — and it lands on the same answer.
                WalScalar::Null => tessera_store::vocabulary::ABSENT_CODE,
                other => return Err(wrong(other)),
            };
            if code == tessera_store::vocabulary::ABSENT_CODE {
                continue;
            }
            presence.add(entity);
            held.push(code);
        }
        // The declared width is the storage width, exactly as at the build: the column is priced
        // at 1 GB per byte of width per 10⁹ items, so a `u8` category stored as `u32` costs three
        // times what it needs — and an extent that stored a different width from its base would
        // make the two disagree about what the column *is* at the next fold that concatenates them.
        let codes = match spec.ty {
            ScalarType::U8 => Codes::U8(held.iter().map(|&c| c as u8).collect::<Vec<_>>().into()),
            ScalarType::U16 => {
                Codes::U16(held.iter().map(|&c| c as u16).collect::<Vec<_>>().into())
            }
            _ => Codes::U32(held.into()),
        };
        return Ok(ExtentColumn::flat(codes, presence));
    }

    // A plain numeric: absent where the item carried no value, present otherwise — the same rule
    // the batch build writes by (`tessera-build`'s `write_column_values`), so a flushed entity and a
    // built one answer a range identically. An extent that treated every entity as present would
    // make one flush's items match a range containing zero while the base's did not.
    macro_rules! gather {
        ($variant:ident, $ctor:expr) => {{
            let mut held = Vec::with_capacity(entities.len());
            for (entity, value) in entities {
                match value {
                    WalScalar::$variant(x) => held.push(*x),
                    WalScalar::Null => continue,
                    other => return Err(wrong(other)),
                }
                presence.add(entity);
            }
            $ctor(held.into())
        }};
    }
    let codes = match spec.ty {
        ScalarType::Bool => {
            let mut held = Vec::with_capacity(entities.len());
            for (entity, value) in entities {
                match value {
                    WalScalar::Bool(b) => held.push(u8::from(*b)),
                    WalScalar::Null => continue,
                    other => return Err(wrong(other)),
                }
                presence.add(entity);
            }
            Codes::U8(held.into())
        }
        ScalarType::U8 => gather!(U8, Codes::U8),
        ScalarType::U16 => gather!(U16, Codes::U16),
        ScalarType::U32 => gather!(U32, Codes::U32),
        ScalarType::U64 => gather!(U64, Codes::U64),
        ScalarType::I8 => gather!(I8, Codes::I8),
        ScalarType::I16 => gather!(I16, Codes::I16),
        ScalarType::I32 => gather!(I32, Codes::I32),
        ScalarType::I64 => gather!(I64, Codes::I64),
        ScalarType::F32 => gather!(F32, Codes::F32),
        ScalarType::F64 => gather!(F64, Codes::F64),
        ScalarType::TimestampUs => gather!(TimestampUs, Codes::I64),
        ScalarType::Keyword => unreachable!("a keyword is handled above"),
        // **Text owes no value column at all**, so it never reaches this gather — its flush extent
        // is a token dictionary, postings and a presence bitmap, written by `write_text_extents`
        // on its own track. `filter::owes_value_column` is where that is decided and
        // `write::filter_schema_of` is what applies it, which is what makes this arm unreachable
        // rather than a panic waiting for a declaration.
        ScalarType::Text => unreachable!("text owes no value column, so it has no extent column"),
        // `utf8` survives as the *wire* type of a keyword's value and of a category's key
        // (`DeclaredScalar::wire_type`); the schema parse refuses it as a declared type, so no
        // column's storage is one.
        ScalarType::Utf8 => unreachable!("`utf8` is not a declarable type"),
    };
    Ok(ExtentColumn::flat(codes, presence))
}

/// The `(entity, value)` pairs one **entity-scoped** column's extent covers: the plan's own rows,
/// joins excluded, each row's value at the column's position in the declared tail.
///
/// **Joins are excluded because a join row carries no entity-scoped value** (`views.md` §4): it
/// arrives with its entity already decided and writes nothing in entity space, so a slot for it
/// here would claim an entity an earlier layer already holds a value for.
fn entity_scoped_rows<'a>(
    spec: &FilterColumnSpec,
    plan: &'a FlushPlan,
) -> Result<Vec<(u32, &'a WalScalar)>, FlushFailed> {
    let mut out = Vec::with_capacity(plan.items.len() + plan.fills.len());
    for (entity, item) in plan.value_rows() {
        let entity = u32::try_from(entity.raw()).map_err(|_| {
            FlushFailed(format!(
                "entity {} does not fit the u32 entity space (I9's ceiling)",
                entity.raw()
            ))
        })?;
        let value = item.scalars.get(spec.index).ok_or_else(|| {
            FlushFailed(format!(
                "a buffered row carries {} scalars, but column '{}' is declared at position {}",
                item.scalars.len(),
                spec.name,
                spec.index
            ))
        })?;
        out.push((entity, value));
    }
    Ok(out)
}

/// The `(entity, value)` pairs one view's column of a **group-scoped** family covers
/// (`views.md` §5) — **every** row this flush publishes, a join included.
///
/// **That is the whole difference from the entity-scoped gather above**, and it is the rule rather
/// than an oversight: a scoped value belongs to the `(entity, view)` pair this flush is giving a
/// row, not to the entity, so a join into a second view of the group is exactly the row that
/// carries this view's value. Disjointness still holds — the entity has at most one row per view
/// (§4's 409), so at most one layer of this view's column ever claims it.
fn scoped_rows<'a>(
    spec: &ScopedColumnSpec,
    plan: &'a FlushPlan,
) -> Result<Vec<(u32, &'a WalScalar)>, FlushFailed> {
    let mut out = Vec::with_capacity(plan.items.len() + plan.fills.len());
    for (entity, item) in plan.scoped_value_rows() {
        let entity = u32::try_from(entity.raw()).map_err(|_| {
            FlushFailed(format!(
                "entity {} does not fit the u32 entity space (I9's ceiling)",
                entity.raw()
            ))
        })?;
        // A row buffered before the family was declared, or one whose view named a group with a
        // shorter list, has nothing here — absence, and the ordinary reading of it.
        let value = item.scoped.get(spec.index).unwrap_or(&WalScalar::Null);
        out.push((entity, value));
    }
    Ok(out)
}

/// One column's extent content: the values, the entities that carry one, and the dictionary those
/// values are ordinals into where the family has one.
///
/// A struct rather than a tuple because the third field is only meaningful beside the first: a
/// keyword's `codes` are positions in `dict_keys` and nothing else, so returning them apart would
/// invite a caller to write one without the other.
struct ExtentColumn<'a> {
    codes: tessera_filter::Codes,
    presence: croaring::Bitmap,
    /// Sorted and distinct, `Some` for keyword columns only.
    dict_keys: Option<Vec<&'a str>>,
}

impl ExtentColumn<'_> {
    /// A column whose values file carries the values themselves — every family but keyword.
    fn flat(codes: tessera_filter::Codes, presence: croaring::Bitmap) -> Self {
        ExtentColumn {
            codes,
            presence,
            dict_keys: None,
        }
    }
}

/// One blob-resident column, and where its value sits in a buffered row's positional scalar list.
///
/// The index is positional against `MANIFEST.declared_scalars` — [`FilterColumnSpec`]'s contract,
/// for its reason — and it is also the row's **field tag**: the blob's format tags a field by the
/// column's position in the declaration (records §3), which is the same identity the build's blob
/// stage writes, so a flushed row and a built one carry one tag for one column.
#[derive(Debug, Clone)]
pub(crate) struct RecordColumnSpec {
    pub(crate) index: usize,
    pub(crate) name: String,
    pub(crate) ty: ScalarType,
}

/// Write this flush's record-blob extent — the flushed entities' blob rows, in their own blocks,
/// has-row bitmap and directory under `attrs/record/extents/` — or nothing where the schema
/// declares no blob-resident column (records §7).
///
/// **Written whenever the schema owes it, even if no flushed entity carries a blob value**: an
/// empty extent is a valid blob (zero blocks, zero rows) and costs a few hundred bytes, and the
/// file set stays a function of the schema rather than of the data — `write_filter_extents`'
/// property, kept here for the same operator-predictability reason.
///
/// **A deleted entity is already gone from the plan**, so its row is never written — which keeps
/// Rule F the only route by which a deletion touches the blob, exactly as it keeps it the only
/// route for the value columns. A suppressed entity's row **is** written: the blob must hold what
/// a later unsuppress reveals, and Rule S forbids this pass any opinion about it.
///
/// The extent is reopened before it is named, so a writer defect refuses the flush here rather
/// than publishing a manifest whose extent the fail-closed reader then refuses on every
/// drill-down.
/// One indexed `text` column, for the flush's own pass over it.
///
/// The analyser is carried rather than looked up per row: constructing one deserialises the
/// segmenter's dictionaries, and a flush that built one per value would pay that per value.
#[derive(Clone)]
pub(crate) struct TextColumnSpec {
    /// Position in a buffered row's scalar list.
    pub(crate) index: usize,
    pub(crate) name: String,
    pub(crate) analyser: std::sync::Arc<tessera_analyse::Analyser>,
}

/// One view's column of a **group-scoped** attribute family, and where its value sits in a
/// buffered row's `scoped` list (`views.md` §5).
///
/// **The counterpart of [`FilterColumnSpec`], and the fields it adds are the two the scope
/// decides**: which directory the extent goes in — `attrs/<column>/<group>/<key>/`, derived from
/// the flush's own view — and whether the bundle already holds a base there. Everything else is
/// the entity-scoped family's, because a scoped column *is* one: same types, same absence rules,
/// same writer.
pub(crate) struct ScopedColumnSpec {
    /// Position in a buffered row's `scoped` list.
    pub(crate) index: usize,
    pub(crate) name: String,
    pub(crate) ty: ScalarType,
    pub(crate) category: bool,
    /// Declared `index = true`, or `render = true` on a family that has a value column — the
    /// family is on the filter surface, so its column is opened and this flush owes it an extent
    /// (`filter::scoped_is_filterable`).
    pub(crate) filterable: bool,
    /// The family's values have an **entity-space value column**, which every family but `text`
    /// does — a `text` family's extent is a token dictionary and positional postings and holds no
    /// value per entity.
    ///
    /// **This, not [`Self::filterable`], is what decides whether this flush owes an extent** for a
    /// family carrying neither flag. Such a family is stored and served at the drill-down without
    /// being searchable or drawn (owner ruling), and gating the write on the *filter* licence left
    /// it serving the build's values and nothing ingested since — the extent that would have
    /// carried them was never written. The two predicates coincide for every family that has a
    /// flag, so nothing else moves.
    ///
    /// ⊘ **A local predicate pending `ScopedScalar::has_value_column()`**, which lands with the
    /// drill-down's own branch; the two say the same thing and the store's helper is the one to
    /// keep.
    pub(crate) has_value_column: bool,
    /// Declared `render = true` — the family occupies a lane in this view's row tail, which is a
    /// column for the purposes of `scoped_scalars[..].views` even where the family is on no filter
    /// surface at all.
    pub(crate) render: bool,
    /// The manifest already names this view in the family's `views`, so a base column is on disc.
    /// `false` for a view created since the build, whose base this flush writes empty.
    pub(crate) has_base: bool,
    /// The analyser a `text` family's terms are produced by — `Some` exactly for that family.
    pub(crate) analyser: Option<std::sync::Arc<tessera_analyse::Analyser>>,
}

/// This flush's text layers: per indexed `text` column, its own dictionary over the terms this
/// batch produced, postings against that dictionary, and the entities it holds a value for.
///
/// **Its own dictionary, not the base's** — the ordinals are positions in *this* extent's term set
/// and name nothing against another's, which is what makes the three files one atomic manifest
/// record (§7, review B2). A flushed batch is bounded and there is no shared dictionary to promote
/// into, which is what keeps the near-unique-string quadratic hazard structurally impossible rather
/// than merely avoided.
///
/// **Presence is stored rather than derived from the postings**, because an entity whose text
/// analyses to no terms at all — an empty string, a field of pure punctuation — carries a value and
/// appears in no posting. Deriving coverage from the postings would report it absent.
///
/// The analysis runs here, at flush execution on the pool (write-path §4.3), and never on the
/// serial group-commit section whose latency both the ingest and the deny lane share.
///
/// `laps` takes the `Text*` sub-laps ([`FlushStage::TEXT`]), run from a copy of `mark`, the
/// caller's last lap. The caller's own mark does not move: it laps `TextExtents` over the whole
/// call on its return, and the sub-laps sum to at most that.
fn write_text_extents(
    plan: &FlushPlan,
    ctx: &FlushContext,
    laps: &mut FlushLaps,
    mark: StageMark,
) -> Result<Vec<tessera_store::manifest::TextExtent>, FlushFailed> {
    if ctx.text_schema.is_empty() {
        return Ok(Vec::new());
    }
    let mut mark = mark;
    let mut out = Vec::with_capacity(ctx.text_schema.len());
    for spec in &ctx.text_schema {
        let mut rows = Vec::with_capacity(plan.items.len() + plan.fills.len());
        for (entity, row) in plan.value_rows() {
            let entity = u32::try_from(entity.raw()).map_err(|_| {
                FlushFailed(format!(
                    "entity {} does not fit the u32 entity space (I9's ceiling)",
                    entity.raw()
                ))
            })?;
            rows.push((
                entity,
                row.scalars.get(spec.index).unwrap_or(&WalScalar::Null),
            ));
        }
        let rel_dir = format!("partitions/{}/attrs/{}/extents", ctx.partition, spec.name);
        let sub = Some(TextLaps {
            laps: &mut *laps,
            mark: &mut mark,
        });
        if let Some(extent) = write_text_layer(
            &rel_dir,
            &spec.name,
            None,
            &spec.analyser,
            rows,
            TextTarget::of(ctx),
            sub,
        )? {
            out.push(extent);
        }
    }
    Ok(out)
}

/// Where a text layer's files go: the prefix they are written under, the segment they are named
/// for, and the incarnation an extent of a view carries. What [`write_text_layer`] needs of a
/// [`FlushContext`], so a test can write a layer without building one.
struct TextTarget<'a> {
    prefix_dir: &'a Path,
    seg_id: &'a str,
    incarnation: tessera_types::view::ViewIncarnation,
}

impl<'a> TextTarget<'a> {
    fn of(ctx: &'a FlushContext) -> Self {
        TextTarget {
            prefix_dir: &ctx.prefix_dir,
            seg_id: &ctx.seg_id,
            incarnation: ctx.incarnation,
        }
    }
}

/// One text layer: its own dictionary over this batch's terms, the postings against it, and the
/// entities that carried prose — written under `rel_dir` and named for the flush.
///
/// **One body for both scopes** (`views.md` §5). An entity-scoped column's layer goes under
/// `attrs/<column>/extents/` and a group-scoped family's under
/// `attrs/<column>/<group>/<key>/extents/`, and `view` is what the manifest entry carries to say
/// which — the difference between them being the directory and nothing about how prose becomes an
/// index.
///
/// `None` where no row carried a value, for the reason [`write_text_extents`] gives.
///
/// `sub` takes the `Text*` sub-laps where the caller's stage is `TextExtents`, and is `None` from
/// the scoped pass, whose layer is in `ScopedExtents`. The per-row lap reads the clock once a row
/// under `bench-timing` and is a moved mark otherwise.
///
/// The tokens are borrowed (`Analyser::for_each_token`) and a term allocates its key on its first
/// sighting alone; a term seen before is looked up by `&str`. Measured on medcpt-1m against the
/// owned-token form (`probes/2026-09-05-flush-attribution/`).
fn write_text_layer(
    rel_dir: &str,
    column: &str,
    view: Option<String>,
    analyser: &tessera_analyse::Analyser,
    rows: Vec<(u32, &WalScalar)>,
    target: TextTarget<'_>,
    mut sub: Option<TextLaps<'_>>,
) -> Result<Option<tessera_store::manifest::TextExtent>, FlushFailed> {
    let dir = target.prefix_dir.join(rel_dir);
    std::fs::create_dir_all(&dir).map_err(|e| FlushFailed(format!("{}: {e}", dir.display())))?;
    text_lap(&mut sub, FlushStage::TextRows);

    let mut terms: std::collections::BTreeMap<String, Vec<u32>> = std::collections::BTreeMap::new();
    let mut presence = croaring::Bitmap::new();
    let mut scratch = tessera_analyse::TokenScratch::default();
    for (entity, value) in rows {
        let prose = match value {
            WalScalar::Utf8(s) => s.as_str(),
            // Absence carries no value and no terms; anything else is a buffered row whose shape
            // disagrees with the declaration, which the flush refuses rather than guesses at.
            WalScalar::Null => continue,
            other => {
                return Err(FlushFailed(format!(
                    "column '{column}' is text but a buffered row carries {other:?}"
                )))
            }
        };
        presence.add(entity);
        analyser.for_each_token(
            prose,
            &mut scratch,
            &mut |token| match terms.get_mut(token) {
                Some(postings) => {
                    if postings.last() != Some(&entity) {
                        postings.push(entity);
                    }
                }
                None => {
                    terms.insert(token.to_string(), vec![entity]);
                }
            },
        );
        text_lap(&mut sub, FlushStage::TextTokeniseTerms);
    }
    if presence.is_empty() {
        return Ok(None);
    }

    let dict_rel = format!("{rel_dir}/{}-dict.bin", target.seg_id);
    let postings_rel = format!("{rel_dir}/{}-postings.arrow", target.seg_id);
    let presence_rel = format!("{rel_dir}/{}-presence.roaring", target.seg_id);
    tessera_filter::write_sorted_dict(
        &target.prefix_dir.join(&dict_rel),
        terms.keys().map(String::as_str),
    )
    .map_err(|e| FlushFailed(format!("{dict_rel}: {e}")))?;
    text_lap(&mut sub, FlushStage::TextDict);
    let per_term: Vec<Vec<u32>> = terms.into_values().collect();
    tessera_authz::postings::write_postings(
        &target.prefix_dir.join(&postings_rel),
        &per_term,
        tessera_types::SMALL_TERM_THRESHOLD_DEFAULT,
    )
    .map_err(|e| FlushFailed(format!("{postings_rel}: {e}")))?;
    text_lap(&mut sub, FlushStage::TextPostings);
    std::fs::write(
        target.prefix_dir.join(&presence_rel),
        presence.serialize::<croaring::Portable>(),
    )
    .map_err(|e| FlushFailed(format!("{presence_rel}: {e}")))?;
    text_lap(&mut sub, FlushStage::TextPresence);

    Ok(Some(tessera_store::manifest::TextExtent {
        column: column.to_string(),
        // The incarnation travels with the view, and is `None` for the same rows `view` is:
        // an entity-scoped column belongs to no view (decision 0115).
        incarnation: view.as_ref().map(|_| target.incarnation),
        view,
        dict: dict_rel,
        postings: postings_rel,
        presence: presence_rel,
    }))
}

/// Where one view's column of a group-scoped family lives, prefix-relative —
/// `partitions/<p>/attrs/<column>/<group>/<key>/` (`views.md` §5), through the one place a view id
/// and its incarnation become a path so the writer cannot drift from `FilterColumns::open`'s
/// reader. Above the declared incarnation the last component is `<key>@<n>` (decision 0115), so a
/// recreated key's base never lands on the path its predecessor's occupies.
fn scoped_column_rel(ctx: &FlushContext, column: &str) -> String {
    // **`scoped_view` and its own incarnation, not `view`'s** (decisions 0115, 0116): the
    // directory is the cell's address, the cell is `(attribute → its group, key)`, and the
    // incarnation suffix is the owner view's — a sharing group's door writes the owner's path,
    // and a recreated key's base never lands on its predecessor's.
    tessera_store::scoped_column_rel(
        &ctx.partition,
        column,
        &ctx.scoped_view,
        ctx.scoped_incarnation,
    )
}

/// Write this flush's extent for every **group-scoped** family of its view's group, and the empty
/// base a view created since the build has none of (`views.md` §5).
///
/// # What a flush owes a family, and why the base is written here
///
/// A family's column for one view is what its entity-scoped counterpart is bundle-wide — values
/// and presence, plus a dictionary for a keyword and keyed postings for a category, or, for text,
/// a token dictionary and positional postings and no value column at all. The build writes one per
/// view it declares. A view created while the service runs has none, and every reader of a family
/// opens a column per view it names: so the first flush of such a view writes the **base** as well
/// as its extent, empty, and publication puts the view on the family's list.
///
/// Writing an empty base rather than teaching every reader to tolerate a missing one is the same
/// choice `FilterColumns::open` makes everywhere else: a declared artefact that is absent is a
/// bundle that is not what its manifest says, and a reader that treated absence as "no entity
/// carries a value" would answer a filter wrongly while looking right. An empty base costs a few
/// hundred bytes and composes to nothing.
///
/// # A join row is in this pass and in no other
///
/// `plan.items` rather than `plan.entity_space_items()`, and that is the rule rather than an
/// oversight (`scoped_rows`): a scoped value belongs to the `(entity, view)` pair this flush is
/// giving a row, so a row joining an entity into a second view of the group is exactly the row
/// that carries that view's value.
fn write_scoped_extents(plan: &FlushPlan, ctx: &FlushContext) -> Result<ScopedWrite, FlushFailed> {
    let mut extents = Vec::new();
    let mut texts = Vec::new();
    let mut created = Vec::new();
    for spec in &ctx.scoped_schema {
        // **The view enters the family's list whatever the family's surface**, because this flush
        // gives it a column of one kind or the other: an entity-space column below for a family
        // that has one, a lane in the row tail for a rendered one — and a rendered family takes
        // both. `scoped_scalars[..].views` is what decides them — the opener walks it, the
        // request's render list walks it, and so does the drill-down — so a view left off it
        // renders nothing, is opened for nothing and serves nothing.
        let owes_extent = spec.filterable || spec.has_value_column;
        if !spec.has_base && (owes_extent || spec.render) {
            created.push((spec.name.clone(), ctx.scoped_view.clone()));
        }
        // **A family with a value column owes an extent whatever its flags.** A family declaring
        // neither `index` nor `render` is stored and served at the drill-down without being
        // searchable or drawn (owner ruling), so gating this on the *filter* licence left such a
        // family serving the build's values and nothing ingested since: the flush wrote no extent
        // for the values to be in. A **text** family has no value column and is the one that stays
        // on the filter licence — its extent is a dictionary and postings, which nothing but the
        // filter surface reads.
        if !owes_extent {
            continue;
        }
        let column_rel = scoped_column_rel(ctx, &spec.name);
        let column_dir = ctx.prefix_dir.join(&column_rel);
        std::fs::create_dir_all(&column_dir)
            .map_err(|e| FlushFailed(format!("scoped column dir '{column_rel}': {e}")))?;
        let rel_of = |path: &std::path::Path| -> Result<String, FlushFailed> {
            path.strip_prefix(&ctx.prefix_dir)
                .ok()
                .and_then(|p| p.to_str())
                .map(str::to_string)
                .ok_or_else(|| {
                    FlushFailed(format!(
                        "scoped extent path {} is not under the prefix",
                        path.display()
                    ))
                })
        };

        if !spec.has_base {
            write_empty_scoped_base(&column_dir, spec)?;
        }

        if let Some(analyser) = &spec.analyser {
            // Text: a dictionary, postings over it and the entities that carry prose — the same
            // three files `write_text_extents` writes bundle-wide, in this view's own directory.
            if let Some(extent) = write_text_layer(
                &format!("{column_rel}/{}", tessera_filter::EXTENTS_DIR),
                &spec.name,
                Some(ctx.scoped_view.clone()),
                analyser,
                scoped_rows(spec, plan)?,
                TextTarget::of(ctx),
                None,
            )? {
                texts.push(extent);
            }
            continue;
        }

        let column = extent_values(
            &FilterColumnSpec {
                // Unused by `extent_values`, which is handed its rows: the position a scoped value
                // sits at is `ScopedColumnSpec::index`, into a different list.
                index: spec.index,
                name: spec.name.clone(),
                ty: spec.ty,
                category: spec.category,
            },
            scoped_rows(spec, plan)?,
        )?;
        let (values_path, presence_path, dict_path) = tessera_filter::write_extent(
            &column_dir,
            &ctx.seg_id,
            &column.codes,
            &column.presence,
            column.dict_keys.as_deref(),
        )
        .map_err(|e| FlushFailed(format!("scoped extent for '{}': {e}", spec.name)))?;
        let values = tessera_filter::open_extent(
            &values_path,
            &presence_path,
            tessera_filter::Access::Mapped,
        )
        .map_err(|e| FlushFailed(format!("scoped extent for '{}': {e}", spec.name)))?;
        let dict = dict_path
            .as_ref()
            .map(|path| {
                tessera_filter::SortedDict::open(path, tessera_filter::Access::Mapped)
                    .map(Arc::new)
                    .map_err(|e| {
                        FlushFailed(format!("scoped keyword dictionary '{}': {e}", spec.name))
                    })
            })
            .transpose()?;
        extents.push(FlushedExtent {
            column: spec.name.clone(),
            view: Some(ctx.scoped_view.clone()),
            values_rel: rel_of(&values_path)?,
            presence_rel: rel_of(&presence_path)?,
            dict_rel: dict_path.as_ref().map(|p| rel_of(p)).transpose()?,
            values: Arc::new(values),
            dict,
        });
    }
    Ok(ScopedWrite {
        extents,
        texts,
        created,
    })
}

/// What [`write_scoped_extents`] produced: this flush's extents for the families of its view's
/// group, their text layers, and the `(family, view)` pairs whose base it had to write.
pub(crate) struct ScopedWrite {
    pub(crate) extents: Vec<FlushedExtent>,
    pub(crate) texts: Vec<tessera_store::manifest::TextExtent>,
    pub(crate) created: Vec<(String, String)>,
}

/// The base a view of a group acquires at its first flush carrying values — every artefact the
/// family's declaration owes, holding nothing (`views.md` §5).
///
/// **The file set is a function of the declaration**, exactly as the build's is and as a flush
/// extent's is: what is written here is what `FilterColumns::open` will demand of this directory,
/// so the two are one predicate rather than a convention and a hope.
fn write_empty_scoped_base(
    column_dir: &std::path::Path,
    spec: &ScopedColumnSpec,
) -> Result<(), FlushFailed> {
    let failed = |what: &str, e: &dyn std::fmt::Display| {
        FlushFailed(format!("scoped base for '{}' ({what}): {e}", spec.name))
    };
    if spec.analyser.is_some() {
        // Text: a dictionary of no terms and postings over it, and no value column at all.
        tessera_filter::write_sorted_dict(
            &column_dir.join(tessera_filter::DICT_FILE),
            std::iter::empty::<&str>(),
        )
        .map_err(|e| failed("the token dictionary", &e))?;
        tessera_authz::postings::write_postings(
            &column_dir.join("postings.arrow"),
            &[],
            tessera_types::SMALL_TERM_THRESHOLD_DEFAULT,
        )
        .map_err(|e| failed("the token postings", &e))?;
        return Ok(());
    }
    let codes = empty_codes(spec.ty, spec.category);
    let values_path = column_dir.join(tessera_filter::VALUES_FILE);
    let presence_path = column_dir.join(tessera_filter::PRESENCE_FILE);
    // **The presence bitmap is written, empty, and its presence is what says so.** A value column
    // with no presence file means "the entity id is the array index" — every entity present —
    // which for a column of no values would report the whole corpus as carrying one.
    tessera_filter::write_value_column(
        &values_path,
        &presence_path,
        &codes,
        Some(&croaring::Bitmap::new()),
    )
    .map_err(|e| failed("the values", &e))?;
    if spec.ty == ScalarType::Keyword {
        tessera_filter::write_sorted_dict(
            &column_dir.join(tessera_filter::DICT_FILE),
            std::iter::empty::<&str>(),
        )
        .map_err(|e| failed("the dictionary", &e))?;
    }
    if spec.category {
        let empty =
            tessera_filter::open_extent(&values_path, &presence_path, tessera_filter::Access::Read)
                .map_err(|e| failed("reopening the values", &e))?;
        tessera_filter_write::write_category_postings(
            &column_dir.join("postings.arrow"),
            &spec.name,
            &empty,
            tessera_filter_write::POSTINGS_BAND_ROWS,
        )
        .map_err(|e| failed("the postings", &e))?;
    }
    Ok(())
}

/// An empty `Codes` at a column's storage width — the base's values file, which must be the width
/// the extents beside it are or a fold that concatenates them would disagree about what the column
/// is.
fn empty_codes(ty: ScalarType, category: bool) -> tessera_filter::Codes {
    use tessera_filter::Codes;
    if category || ty == ScalarType::Keyword {
        return match ty {
            ScalarType::U8 => Codes::U8(Vec::new().into()),
            ScalarType::U16 => Codes::U16(Vec::new().into()),
            _ => Codes::U32(Vec::new().into()),
        };
    }
    match ty {
        ScalarType::Bool | ScalarType::U8 => Codes::U8(Vec::new().into()),
        ScalarType::U16 => Codes::U16(Vec::new().into()),
        ScalarType::U32 => Codes::U32(Vec::new().into()),
        ScalarType::U64 => Codes::U64(Vec::new().into()),
        ScalarType::I8 => Codes::I8(Vec::new().into()),
        ScalarType::I16 => Codes::I16(Vec::new().into()),
        ScalarType::I32 => Codes::I32(Vec::new().into()),
        ScalarType::I64 | ScalarType::TimestampUs => Codes::I64(Vec::new().into()),
        ScalarType::F32 => Codes::F32(Vec::new().into()),
        ScalarType::F64 => Codes::F64(Vec::new().into()),
        // Neither reaches here: text takes its own branch above and `utf8` is not a declarable
        // storage type (`DeclaredScalar::wire_type`).
        ScalarType::Keyword | ScalarType::Text | ScalarType::Utf8 => Codes::U32(Vec::new().into()),
    }
}

/// This flush's slice of `entities/terms/` — the term lists of the entities it minted, in the
/// promoted ordinals (contracts §2.4, `tessera_store::entity_terms`).
///
/// **Always written, even for a flush that minted nothing.** An empty layer costs four tiny files
/// and keeps the manifest's list a complete record of what each flush published; a conditional
/// write would make "no extent" mean either "no entities" or "an older writer", which is the
/// ambiguity the record blob avoids by making its own absence a function of the schema alone.
fn write_entity_terms_extent(
    per_entity: &[(u32, Vec<u32>)],
    ctx: &FlushContext,
) -> Result<tessera_store::manifest::EntityTermsExtent, FlushFailed> {
    let extents_rel = format!("partitions/{}/entities/terms/extents", ctx.partition);
    let extents_dir = ctx.prefix_dir.join(&extents_rel);
    std::fs::create_dir_all(&extents_dir)
        .map_err(|e| FlushFailed(format!("entity-terms extent dir: {e}")))?;
    let extent = tessera_store::manifest::EntityTermsExtent {
        hasrow: format!("{extents_rel}/{}.hasrow.roaring", ctx.seg_id),
        offsets: format!("{extents_rel}/{}.offsets.u32", ctx.seg_id),
        terms: format!("{extents_rel}/{}.terms.u32", ctx.seg_id),
        bases: format!("{extents_rel}/{}.bases.u64", ctx.seg_id),
    };
    let mut writer = tessera_store::EntityTermsWriter::create_at(
        &ctx.prefix_dir.join(&extent.hasrow),
        &ctx.prefix_dir.join(&extent.offsets),
        &ctx.prefix_dir.join(&extent.terms),
        &ctx.prefix_dir.join(&extent.bases),
    )
    .map_err(|e| FlushFailed(format!("entity-terms extent: {e}")))?;
    for (entity, terms) in per_entity {
        writer
            .push(*entity, terms)
            .map_err(|e| FlushFailed(format!("entity-terms extent: {e}")))?;
    }
    writer
        .finish()
        .map_err(|e| FlushFailed(format!("entity-terms extent: {e}")))?;
    Ok(extent)
}

/// Push one accumulated row, where there is an entity and it carries something. An entity with no
/// blob-resident value has no row and no has-row bit (records §3), which is what the empty-field
/// arm answers.
fn push_record_row(
    writer: &mut tessera_filter_write::RecordBlobWriter,
    entity: Option<u32>,
    fields: &[tessera_filter::RecordField],
) -> Result<(), FlushFailed> {
    let Some(entity) = entity else {
        return Ok(());
    };
    if fields.is_empty() {
        return Ok(());
    }
    let borrowed: Vec<tessera_filter::RecordFieldRef<'_>> = fields
        .iter()
        .map(|field| {
            field
                .value
                .as_ref()
                .map(|value| tessera_filter::RecordFieldRef {
                    tag: field.tag,
                    value,
                })
                .ok_or_else(|| {
                    FlushFailed(format!(
                        "record extent: entity {entity} carries a list at field tag {}; the \
                         multi surface has not landed (records §5)",
                        field.tag
                    ))
                })
        })
        .collect::<Result<_, _>>()?;
    writer
        .push_row(entity, &borrowed)
        .map_err(|e| FlushFailed(format!("record extent: {e}")))
}

fn write_record_extent(
    plan: &FlushPlan,
    ctx: &FlushContext,
) -> Result<Option<RecordExtent>, FlushFailed> {
    if ctx.record_schema.is_empty() {
        return Ok(None);
    }
    let extents_rel = format!("partitions/{}/attrs/record/extents", ctx.partition);
    let extents_dir = ctx.prefix_dir.join(&extents_rel);
    std::fs::create_dir_all(&extents_dir)
        .map_err(|e| FlushFailed(format!("record extent dir: {e}")))?;
    let extent = RecordExtent {
        blocks: format!("{extents_rel}/{}.blocks.bin", ctx.seg_id),
        hasrow: format!("{extents_rel}/{}.hasrow.roaring", ctx.seg_id),
        directory: format!("{extents_rel}/{}.directory.arrow", ctx.seg_id),
    };
    let blocks_path = ctx.prefix_dir.join(&extent.blocks);
    let hasrow_path = ctx.prefix_dir.join(&extent.hasrow);
    let directory_path = ctx.prefix_dir.join(&extent.directory);
    let mut writer = tessera_filter_write::RecordBlobWriter::create(
        &blocks_path,
        &hasrow_path,
        &directory_path,
        tessera_filter::RECORD_BLOCK_TARGET,
    )
    .map_err(|e| FlushFailed(format!("record extent: {e}")))?;

    // **One row per entity, however many of the plan's rows carry its cells.** A fill and the
    // buffered row of the entity it fills are two rows of one entity (`ingest.md` §1.4), and a
    // layer holds one row per entity — so the fields accumulate while the entity repeats and are
    // pushed once. The two never claim one column: the fill rule refused the batch where anything
    // already held the cell.
    let mut fields: Vec<tessera_filter::RecordField> = Vec::with_capacity(ctx.record_schema.len());
    let mut open: Option<u32> = None;
    for (entity, item) in plan.value_rows() {
        let entity = u32::try_from(entity.raw()).map_err(|_| {
            FlushFailed(format!(
                "entity {} does not fit the u32 entity space (I9's ceiling)",
                entity.raw()
            ))
        })?;
        if open != Some(entity) {
            push_record_row(&mut writer, open, &fields)?;
            fields.clear();
            open = Some(entity);
        }
        for spec in &ctx.record_schema {
            let value = item.scalars.get(spec.index).ok_or_else(|| {
                FlushFailed(format!(
                    "a buffered row carries {} scalars, but column '{}' is declared at position {}",
                    item.scalars.len(),
                    spec.name,
                    spec.index
                ))
            })?;
            let Some(value) = record_value_of(value, spec)? else {
                continue;
            };
            let tag = u16::try_from(spec.index).map_err(|_| {
                FlushFailed(format!(
                    "column '{}' is declared at position {}, past the u16 field-tag space",
                    spec.name, spec.index
                ))
            })?;
            fields.push(tessera_filter::RecordField { tag, value });
        }
    }
    push_record_row(&mut writer, open, &fields)?;
    writer
        .finish()
        .map_err(|e| FlushFailed(format!("record extent: {e}")))?;
    tessera_filter::RecordBlob::open(
        &blocks_path,
        &hasrow_path,
        &directory_path,
        tessera_filter::Access::Mapped,
    )
    .map_err(|e| FlushFailed(format!("record extent does not reopen: {e}")))?;
    Ok(Some(extent))
}

/// One buffered value as the blob row carries it, or `None` where the entity carries nothing in
/// this column — `WalScalar::Null` being the one spelling of absence every non-category family
/// has (a category is never blob-resident, so its reserved code needs no arm here).
///
/// **The declared type is checked, not assumed.** The ingest plane validated the batch against
/// the declaration, so a mismatch here is a defect on the write path — and storing a value this
/// code mis-transcribed would serve it at every later drill-down, so it fails the flush exactly
/// as `extent_values`' `wrong` does.
fn record_value_of(
    value: &WalScalar,
    spec: &RecordColumnSpec,
) -> Result<Option<tessera_filter::RecordValue>, FlushFailed> {
    use tessera_filter::RecordValue;
    let wrong = || {
        FlushFailed(format!(
            "column '{}' is declared {:?} but a buffered row carries {value:?}",
            spec.name, spec.ty
        ))
    };
    macro_rules! expect {
        ($variant:ident) => {
            match value {
                WalScalar::$variant(x) => RecordValue::$variant(*x),
                WalScalar::Null => return Ok(None),
                _ => return Err(wrong()),
            }
        };
    }
    Ok(Some(match spec.ty {
        ScalarType::Bool => expect!(Bool),
        ScalarType::U8 => expect!(U8),
        ScalarType::U16 => expect!(U16),
        ScalarType::U32 => expect!(U32),
        ScalarType::U64 => expect!(U64),
        ScalarType::I8 => expect!(I8),
        ScalarType::I16 => expect!(I16),
        ScalarType::I32 => expect!(I32),
        ScalarType::I64 => expect!(I64),
        ScalarType::F32 => expect!(F32),
        ScalarType::F64 => expect!(F64),
        ScalarType::TimestampUs => expect!(TimestampUs),
        // **A blob-resident keyword stores its bytes, not an ordinal.** The blob is the values'
        // only home when a column has no other (records §3), so there is no dictionary beside it
        // and no layer for an ordinal to be a position in; the row carries what the wire carried,
        // which is why this shares the arm of `utf8`, that same wire type
        // (`DeclaredScalar::wire_type`).
        ScalarType::Utf8 | ScalarType::Keyword | ScalarType::Text => match value {
            WalScalar::Utf8(text) => RecordValue::Utf8(text.clone()),
            WalScalar::Null => return Ok(None),
            _ => return Err(wrong()),
        },
    }))
}

/// This flush's segment directory. Both the segment writer and promotion address it; naming it
/// once keeps them from drifting apart.
fn segment_dir(ctx: &FlushContext) -> PathBuf {
    tessera_store::view_path(
        &ctx.prefix_dir.join("partitions").join(&ctx.partition),
        &ctx.view,
    )
    .join("segments")
    .join(&ctx.seg_id)
}

/// The WAL's scalar shape into the segment writer's — the same set in the same order, so this is
/// a variant-for-variant transcription and a missing arm is a compile error rather than a value
/// silently taking another type's place.
fn to_scalar_value(scalar: &WalScalar) -> ScalarValue {
    macro_rules! same {
        ($($v:ident),* $(,)?) => {
            match scalar {
                $(WalScalar::$v(x) => ScalarValue::$v(*x),)*
                WalScalar::Utf8(x) => ScalarValue::Utf8(x.clone()),
                WalScalar::Null => ScalarValue::Null,
            }
        };
    }
    same!(
        Bool,
        U8,
        U16,
        U32,
        U64,
        I8,
        I16,
        I32,
        I64,
        F32,
        F64,
        TimestampUs
    )
}

/// One file's size and hex SHA-256, by reading it back — `tessera_store::digest_of` with this
/// crate's error type.
///
/// **One definition, in the crate that owns the manifest format.** Three copies of this existed,
/// here, in `coalesce.rs` and in `tessera-store`, and all three read the whole file into memory;
/// the fold's pass 5 is where that stopped being affordable (probe P1), and a fix applied to one
/// copy would have left the other two. `pub(crate)` because compaction's pass 5 digests its own
/// outputs the same way — see `crate::compact::execute`, which states why the digest is taken from
/// a re-read rather than computed as the bytes are written.
pub(crate) fn digest_of(path: &Path) -> Result<FileDigest, String> {
    tessera_store::digest_of(path).map_err(|e| format!("digest {}: {e}", path.display()))
}

/// Whether `entity` is deleted as of this overlay.
///
/// **Only `deleted` excludes an item from a flush.** `suppressed` does not — the row must exist for
/// a later unsuppress to reveal. Reading any other field here is the fold arriving as a
/// simplification; see this module's doc.
///
/// Unreachable in the steady state and kept deliberately: a delete now drops its row from the
/// buffer as it applies (`tessera_lifecycle::overlay::drop_deleted`), so no live buffer holds a
/// deleted row for this to find. This is the statement of *which disposition* excludes an item,
/// which is the invariant-bearing half, and it must not follow the buffer's shape.
fn is_deleted(overlay: &Overlay, entity: EntityId) -> bool {
    overlay.is_deleted(entity)
}

#[cfg(test)]
mod tests {
    use super::*;
    

    

    use tessera_lifecycle::wal::{ChangeOp, WalRow, WalScalar};
    use tessera_lifecycle::IngestBuffer;
    
    
    
    use tessera_types::TermId;

    const VIEW: &str = "s0";

    fn item(terms: &[u32]) -> BufferedItem {
        BufferedItem {
            terms: terms.iter().map(|t| TermId::new(*t)).collect(),
            view: VIEW.to_string(),
            join: false,
            x: 0.5,
            y: 0.5,
            scalars: vec![WalScalar::U64(1)],
            scoped: Vec::new(),
            external_id: None,
            wal_pos: None,
        }
    }

    fn buffer_with(buffered: &[(u64, BufferedItem)]) -> IngestBuffer {
        let mut buffer = IngestBuffer::new();
        for (entity, item) in buffered {
            let row = WalRow {
                external_id: Some(format!("ext-{entity}").into_bytes()),
                entity_id: EntityId::new(*entity),
                view: item.view.clone(),
                join: false,
                descriptors: Vec::new(),
                x: item.x,
                y: item.y,
                scalars: item.scalars.clone(),
                scoped: Vec::new(),
            };
            buffer.insert_row_with_terms(&row, item.terms.clone());
        }
        buffer
    }

    /// A generation over `buffered`, with `changes` applied to its overlay.
    ///
    /// The bundle is empty: `plan_flush` reads the buffer and the overlay and nothing else, so a
    /// real one would make these tests about the fixture instead.
    fn generation_with(
        buffered: &[(u64, BufferedItem)],
        changes: &[(u64, ChangeOp)],
    ) -> Generation {
        let mut overlay = Overlay::new();
        for (entity, op) in changes {
            overlay.apply(EntityId::new(*entity), *op);
        }
        generation_of(overlay, buffer_with(buffered))
    }

    fn generation_of(overlay: Overlay, buffer: IngestBuffer) -> Generation {
        Generation::synthetic("v00000", 0, 0, overlay, buffer)
    }

    fn plan(generation: &Generation) -> Result<FlushPlan, NoFlush> {
        plan_flush(generation, VIEW, false, false)
    }

    /// **A suppression never touches postings and retires only on unsuppress**, so a flush that
    /// skipped it would leave a later unsuppress with nothing to reveal: no row would exist, and
    /// unsuppressing the item would show nothing at all.
    #[test]
    fn a_suppressed_entity_is_flushed_so_a_later_unsuppress_has_something_to_reveal() {
        let generation = generation_with(&[(7, item(&[1]))], &[(7, ChangeOp::Suppress)]);
        let plan = plan(&generation).expect("a suppressed item still flushes");
        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.items[0].0, EntityId::new(7));
    }

    /// A deletion's ID stays burned (I9), no row is created, and the deny entry stands.
    #[test]
    fn a_deleted_entity_acquires_no_row() {
        let generation = generation_with(
            &[(7, item(&[1])), (8, item(&[1]))],
            &[(7, ChangeOp::Delete)],
        );
        let plan = plan(&generation).expect("the undeleted item still flushes");
        assert_eq!(plan.items.len(), 1);
        assert_eq!(
            plan.items[0].0,
            EntityId::new(8),
            "the deleted entity contributes no row"
        );
    }

    /// A deletion accepted *after* the snapshot is a different case, and is not this one's: it
    /// produces a deleted entity that **does** have a row, hidden by its overlay entry alone. Safe
    /// only because nothing retires, and an obligation the compaction spec inherits.
    #[test]
    fn a_delete_arriving_after_the_plan_does_not_unwrite_the_row() {
        let generation = generation_with(&[(7, item(&[1]))], &[]);
        let plan = plan(&generation).expect("nothing is denied at the snapshot");
        assert_eq!(plan.items.len(), 1, "the row is planned");
        // A later delete cannot reach this plan: it is a value, taken from one generation.
        let later = generation_with(&[(7, item(&[1]))], &[(7, ChangeOp::Delete)]);
        assert!(matches!(
            plan_flush(&later, VIEW, false, false),
            Err(NoFlush::NothingToFlush)
        ));
    }

    /// **A `WalPoisoned` node publishes nothing** (§3.5). A flush honouring an under-durable
    /// delete would skip the entity and advance the watermark past it; replay would then discard
    /// the delete record, leaving the item in no segment and no buffer — the un-acked delete made
    /// permanent, against contracts §3.1's residual that a restart makes it visible again.
    #[test]
    fn a_wal_poisoned_node_plans_nothing() {
        let generation = generation_with(&[(7, item(&[1]))], &[]);
        assert!(matches!(
            plan_flush(&generation, VIEW, true, false),
            Err(NoFlush::WalPoisoned)
        ));
    }

    /// **A node whose overlay has diverged from its durable WAL publishes nothing** (§7.2), and
    /// the poisoned gate does not cover it: `discard_undurable` returns the node to `Running`
    /// while it still holds dispositions no record backs.
    #[test]
    fn a_diverged_node_plans_nothing_even_though_its_wal_is_healthy() {
        let generation = generation_with(&[(7, item(&[1]))], &[]);
        assert!(matches!(
            plan_flush(&generation, VIEW, false, true),
            Err(NoFlush::OverlayDiverged)
        ));
    }

    /// Items of another view are not this view's to flush: a segment's entity range is
    /// contiguous only within one (§2.1).
    #[test]
    fn another_views_items_are_left_alone() {
        let mut other = item(&[1]);
        other.view = "elsewhere".to_string();
        let generation = generation_with(&[(7, other)], &[]);
        assert!(matches!(plan(&generation), Err(NoFlush::NothingToFlush)));
    }

    /// The two per-thread sets cover every stage once, and each wall's partition excludes the
    /// wall itself. `/control/status` and the attribution test index by these sets, so a stage
    /// added to the enum and left out of them would be measured and reported nowhere.
    #[test]
    fn the_stage_sets_cover_the_enum_once_and_each_partition_excludes_its_wall() {
        let mut seen = [false; FlushStage::COUNT];
        for stage in FlushStage::EXECUTOR.iter().chain(FlushStage::POOL.iter()) {
            assert!(!seen[*stage as usize], "{} is listed twice", stage.name());
            seen[*stage as usize] = true;
        }
        assert!(seen.iter().all(|s| *s), "a stage is in neither set");
        for stage in FlushStage::PUBLISH {
            assert!(FlushStage::EXECUTOR.contains(&stage));
            assert_ne!(stage, FlushStage::PublishWall);
        }
        for stage in FlushStage::EXECUTE {
            assert!(FlushStage::POOL.contains(&stage));
            assert_ne!(stage, FlushStage::PoolWall);
        }
        // The `Text*` sub-stages are on the pool, partition `TextExtents`, and are in `EXECUTE`
        // no more than the wall is: counted there they would double `TextExtents`.
        for stage in FlushStage::TEXT {
            assert!(FlushStage::POOL.contains(&stage));
            assert!(!FlushStage::EXECUTE.contains(&stage));
        }
        // Everything on the executor that is not the tick's two stages or the wall partitions
        // the wall; everything on the pool that is not the wall or a sub-stage partitions it.
        assert_eq!(FlushStage::PUBLISH.len() + 3, FlushStage::EXECUTOR.len());
        assert_eq!(
            FlushStage::EXECUTE.len() + FlushStage::TEXT.len() + 1,
            FlushStage::POOL.len()
        );
    }

    /// Ascending by entity id, because `write_flush_segment` requires it and because the extent is
    /// dense over the range. The buffer is a hash map, so nothing else establishes the order.
    #[test]
    fn the_plan_is_ascending_by_entity_id() {
        let generation = generation_with(&[(9, item(&[1])), (3, item(&[1])), (7, item(&[1]))], &[]);
        let plan = plan(&generation).unwrap();
        let ids: Vec<u64> = plan.items.iter().map(|(e, _)| e.raw()).collect();
        assert_eq!(ids, vec![3, 7, 9]);
    }

    // ---- the text layer's three files (write-path §4.3) -----------------------------------

    /// **The three files reconstruct the batch's terms, through the readers that serve them.**
    /// The expectation is computed here from `Analyser::tokens` a row at a time, so the writer's
    /// borrowed-token loop is checked against the owned-token segmentation it must agree with:
    /// the dictionary is the sorted distinct term set, each term's posting is the ascending list
    /// of the entities whose prose contains it (once, however often the row repeats it), and the
    /// presence bitmap holds every entity that carried prose, the one whose prose analyses to no
    /// term included.
    #[test]
    fn a_text_layer_round_trips_through_the_files_it_writes() {
        let analyser = tessera_analyse::Analyser::default();
        let prose: Vec<(u32, WalScalar)> = vec![
            (3, WalScalar::Utf8("Alpha beta".to_string())),
            (5, WalScalar::Utf8("beta GAMMA, beta!".to_string())),
            (7, WalScalar::Null),
            (9, WalScalar::Utf8(String::new())),
            (11, WalScalar::Utf8("gamma alpha alpha delta".to_string())),
        ];

        // The expectation, from the owned-token analyser one row at a time.
        let mut expected_terms: std::collections::BTreeMap<String, Vec<u32>> = Default::default();
        let mut expected_presence = Vec::new();
        for (entity, value) in &prose {
            let WalScalar::Utf8(text) = value else {
                continue;
            };
            expected_presence.push(*entity);
            let distinct: std::collections::BTreeSet<String> =
                analyser.tokens(text).into_iter().collect();
            for term in distinct {
                expected_terms.entry(term).or_default().push(*entity);
            }
        }
        assert!(
            expected_terms.values().any(|p| p.len() > 1),
            "the fixture shares a term across rows"
        );
        assert!(
            !analyser.tokens("beta GAMMA, beta!").len().eq(&2),
            "the fixture repeats a term within a row"
        );

        let dir = tempfile::TempDir::new().expect("a temp dir");
        let rows: Vec<(u32, &WalScalar)> = prose.iter().map(|(e, v)| (*e, v)).collect();
        let extent = write_text_layer(
            "attrs/title/extents",
            "title",
            None,
            &analyser,
            rows,
            TextTarget {
                prefix_dir: dir.path(),
                seg_id: "flush-1-0",
                incarnation: tessera_types::view::DECLARED_INCARNATION,
            },
            None,
        )
        .expect("the layer writes")
        .expect("rows carried prose");

        let dict = tessera_filter::SortedDict::open(
            &dir.path().join(&extent.dict),
            tessera_filter::Access::Mapped,
        )
        .expect("the dictionary reopens");
        dict.self_check().expect("the dictionary is well formed");
        let mut keys = Vec::new();
        dict.walk(|_, key| keys.push(key.to_string()))
            .expect("the dictionary walks");
        assert_eq!(
            keys,
            expected_terms.keys().cloned().collect::<Vec<_>>(),
            "the dictionary is the sorted distinct term set"
        );

        let postings = tessera_authz::postings::PostingsReader::open(
            &dir.path().join(&extent.postings),
            false,
        )
        .expect("the postings reopen");
        assert_eq!(postings.term_count() as usize, expected_terms.len());
        for (ordinal, (term, expected)) in expected_terms.iter().enumerate() {
            let entities: Vec<u32> = match postings
                .posting_at(ordinal as u32)
                .expect("the record decodes")
                .expect("every term has a record")
            {
                tessera_authz::postings::PostingRef::Array(bytes) => bytes
                    .chunks_exact(4)
                    .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                    .collect(),
                tessera_authz::postings::PostingRef::Roaring(view) => view.iter().collect(),
            };
            assert_eq!(&entities, expected, "the posting of '{term}'");
        }

        let presence = croaring::Bitmap::deserialize::<croaring::Portable>(
            &std::fs::read(dir.path().join(&extent.presence)).expect("the presence reads"),
        );
        assert_eq!(
            presence.iter().collect::<Vec<_>>(),
            expected_presence,
            "presence holds every entity with prose, the empty string's included"
        );
        assert_eq!(expected_presence, vec![3, 5, 9, 11]);
    }

    // ---- the keyword family's extent (records §4.3, §7) ------------------------------------

    /// One item carrying a keyword value, or none at all.
    fn keyword_item(value: Option<&str>) -> BufferedItem {
        let mut buffered = item(&[1]);
        buffered.scalars = vec![match value {
            Some(text) => WalScalar::Utf8(text.to_string()),
            None => WalScalar::Null,
        }];
        buffered
    }

    fn keyword_spec() -> FilterColumnSpec {
        FilterColumnSpec {
            index: 0,
            name: "doi".to_string(),
            ty: ScalarType::Keyword,
            category: false,
        }
    }

    /// The three things one flush's keyword column produces: a sorted, **distinct** dictionary of
    /// this batch's values, one `u32` ordinal per present entity naming a position in it, and the
    /// presence bitmap that says which entities those slots belong to.
    ///
    /// The repeated value is the point of the fixture: two entities carrying `zeta` share one key
    /// and one ordinal, which is the interning the family's byte win rests on.
    #[test]
    fn a_keyword_extent_interns_its_batchs_values_and_stores_ordinals() {
        let generation = generation_with(
            &[
                (3, keyword_item(Some("zeta"))),
                (5, keyword_item(Some("alpha"))),
                (7, keyword_item(None)),
                (9, keyword_item(Some("zeta"))),
            ],
            &[],
        );
        let plan = plan(&generation).expect("the batch flushes");
        let spec = keyword_spec();
        let rows = entity_scoped_rows(&spec, &plan).expect("the rows gather");
        let column = extent_values(&spec, rows).expect("the column gathers");

        assert_eq!(
            column.dict_keys.as_deref(),
            Some(&["alpha", "zeta"][..]),
            "sorted and distinct — the writer refuses anything else"
        );
        assert_eq!(
            column.presence.iter().collect::<Vec<_>>(),
            vec![3, 5, 9],
            "the entity carrying nothing occupies no slot"
        );
        match &column.codes {
            tessera_filter::Codes::U32(ordinals) => {
                assert_eq!(
                    ordinals.as_ref(),
                    &[1, 0, 1],
                    "slot k belongs to the k-th set bit: zeta, alpha, zeta"
                );
            }
            other => panic!("a keyword extent stores u32 ordinals, not {other:?}"),
        }
    }

    /// **The pair, through the files.** An extent's ordinals are positions in *that extent's own*
    /// dictionary, so what has to hold is that the two files this flush writes reconstruct the
    /// values the batch carried — read back through the readers that will serve them.
    #[test]
    fn a_keyword_extent_round_trips_through_the_files_it_writes() {
        let generation = generation_with(
            &[
                (3, keyword_item(Some("zeta"))),
                (5, keyword_item(Some("alpha"))),
                (7, keyword_item(None)),
                (9, keyword_item(Some("zeta"))),
            ],
            &[],
        );
        let plan = plan(&generation).expect("the batch flushes");
        let spec = keyword_spec();
        let rows = entity_scoped_rows(&spec, &plan).expect("the rows gather");
        let column = extent_values(&spec, rows).expect("the column gathers");

        let dir = tempfile::TempDir::new().expect("a temp dir");
        let (values_path, presence_path, dict_path) = tessera_filter::write_extent(
            dir.path(),
            "flush-1",
            &column.codes,
            &column.presence,
            column.dict_keys.as_deref(),
        )
        .expect("the extent writes");
        let dict_path = dict_path.expect("a keyword extent names a dictionary");

        let values = tessera_filter::open_extent(
            &values_path,
            &presence_path,
            tessera_filter::Access::Mapped,
        )
        .expect("the values reopen");
        let dict = tessera_filter::SortedDict::open(&dict_path, tessera_filter::Access::Mapped)
            .expect("the dictionary reopens");
        dict.self_check().expect("the dictionary is well formed");

        let mut scratch = Vec::new();
        for (entity, expected) in [(3u32, Some("zeta")), (5, Some("alpha")), (9, Some("zeta"))] {
            let ordinal = values
                .value_of(entity)
                .expect("a present entity has a slot");
            assert_eq!(
                dict.key_of(ordinal.raw(), &mut scratch)
                    .expect("it decodes"),
                expected.expect("present"),
                "entity {entity}"
            );
        }
        assert_eq!(values.value_of(7), None, "the absent entity has no slot");
    }

    /// **The file set is a function of the schema, not of the batch.** A flush whose items carry no
    /// value in a keyword column still writes the dictionary — empty — so an operator can predict
    /// what a flush produces from the manifest alone, and so the manifest record that names all
    /// three files never names one that is not there.
    #[test]
    fn a_keyword_extent_with_no_values_still_writes_an_empty_dictionary() {
        let generation = generation_with(&[(3, keyword_item(None)), (5, keyword_item(None))], &[]);
        let plan = plan(&generation).expect("the batch flushes");
        let spec = keyword_spec();
        let rows = entity_scoped_rows(&spec, &plan).expect("the rows gather");
        let column = extent_values(&spec, rows).expect("the column gathers");
        assert_eq!(column.dict_keys.as_deref(), Some(&[][..]));

        let dir = tempfile::TempDir::new().expect("a temp dir");
        let (_, _, dict_path) = tessera_filter::write_extent(
            dir.path(),
            "flush-1",
            &column.codes,
            &column.presence,
            column.dict_keys.as_deref(),
        )
        .expect("the extent writes");
        let dict = tessera_filter::SortedDict::open(
            &dict_path.expect("named even when empty"),
            tessera_filter::Access::Mapped,
        )
        .expect("an empty dictionary opens");
        assert!(dict.is_empty());
    }

    /// **Ordinals are per layer**, which is exactly what makes them unusable across layers. Two
    /// flushes of the same column mint their own numbering over their own batch, so the same
    /// ordinal names different values in the two — the property the read and lifecycle tracks must
    /// never assume away, and the reason a layer's files are one manifest record.
    #[test]
    fn two_flushes_of_one_column_mint_independent_numberings() {
        let first = plan(&generation_with(
            &[
                (3, keyword_item(Some("alpha"))),
                (5, keyword_item(Some("zeta"))),
            ],
            &[],
        ))
        .expect("the first batch flushes");
        let second = plan(&generation_with(
            &[
                (11, keyword_item(Some("zeta"))),
                (13, keyword_item(Some("omega"))),
            ],
            &[],
        ))
        .expect("the second batch flushes");

        let spec = keyword_spec();
        let a = extent_values(&spec, entity_scoped_rows(&spec, &first).expect("rows"))
            .expect("gathers");
        let b = extent_values(&spec, entity_scoped_rows(&spec, &second).expect("rows"))
            .expect("gathers");

        assert_eq!(a.dict_keys.as_deref(), Some(&["alpha", "zeta"][..]));
        assert_eq!(b.dict_keys.as_deref(), Some(&["omega", "zeta"][..]));
        // Ordinal 0 is `alpha` in one layer and `omega` in the other. Resolving the first
        // extent's ordinals against the second's dictionary would answer `omega` to a scan
        // looking for `alpha` — a recolouring with no error anywhere, which is why the two
        // travel in one manifest record and why nothing caches an ordinal across layers.
        assert_eq!(a.dict_keys.as_deref().unwrap()[0], "alpha");
        assert_eq!(b.dict_keys.as_deref().unwrap()[0], "omega");
    }

    /// A dictionary beside anything but an ordinal column is a pair that cannot be read together,
    /// and the extent writer refuses it rather than leaving a scan to read a `u64` as an ordinal.
    #[test]
    fn a_dictionary_beside_a_non_ordinal_column_is_refused() {
        let dir = tempfile::TempDir::new().expect("a temp dir");
        let mut presence = croaring::Bitmap::new();
        presence.add(3);
        let error = tessera_filter::write_extent(
            dir.path(),
            "flush-1",
            &tessera_filter::Codes::I64(vec![7i64].into()),
            &presence,
            Some(&["alpha"]),
        )
        .expect_err("the mismatch is refused");
        assert!(error.to_string().contains("not u32 ordinals"), "{error}");
    }
}
