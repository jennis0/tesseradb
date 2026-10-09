//! The flush: planning on the executor, writing on the background pool. A plan takes the
//! buffered rows of one view from the live generation. Executing it writes a segment for the rows
//! and entity-space extents for their values. Publication is in `write/executor`.
//!
//! A suppressed entity is flushed like any other, so that a later unsuppress has a row to
//! reveal. A deleted entity is not written. A delete accepted after the plan was taken leaves a
//! row that only its overlay entry hides; the fold (compaction) removes such rows.
//!
//! Coordinates are not re-checked here. `Engine::ingest` refuses an out-of-extent
//! coordinate before a row is buffered.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustc_hash::FxHashMap;
use mosaica_authz::{write_delta_tier, DeltaTier, Dict, DictStreamWriter};
use mosaica_lifecycle::wal::WalScalar;
use mosaica_lifecycle::{BufferedItem, Overlay};
use mosaica_spatial::tiler::ScalarType;
use mosaica_store::manifest::{DictExtent, FileDigest, Quantisation, RecordExtent};
use mosaica_store::permutation::SegmentExtent;
use mosaica_store::read::SegmentData;
use mosaica_store::{write_flush_segment, FlushInput, FlushRow};
use mosaica_types::{EntityId, IdentityKey, TermId};

use crate::write::StageMark;
use crate::Generation;

/// The small/large postings threshold for this module's delta-tier writes, fixed rather than
/// read from the bundle's `small_term_threshold`.
pub(crate) const SMALL_TERM_THRESHOLD: u32 = 32;

/// The stages of one flush, timed under the `bench-timing` feature. The executor plans a flush
/// (`Plan`, `Dispatch`) and later publishes it (`Compose` through `PublishWall`); the pool
/// executes it (`Promote` through `PoolWall`). Each variant accumulates nanoseconds since the
/// executor started; without `bench-timing` every value is zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlushStage {
    // ---- the executor thread ----
    /// `plan_flush` over every view at the tick: the buffer scan, the item clones and the sort.
    Plan,
    /// `dispatch_flushes` up to the pool spawn: schemas, the novel-descriptor snapshot, the context.
    Dispatch,
    /// `publish_flush`'s gates, and composing the flush's filter, record and text extents.
    Compose,
    /// Assembling the side-manifest from the live partition manifest, deny state included. The
    /// three stages below partition it.
    Manifest,
    /// Within [`FlushStage::Manifest`]: cloning the live partition manifest to assemble onto.
    ManifestClone,
    /// Within [`FlushStage::Manifest`]: `write_deny_state` — the overlay's suppressions and
    /// tombstones, restated in full at every publication.
    ManifestDenyState,
    /// Within [`FlushStage::Manifest`]: `write_vocabulary_extensions` — every live binding beyond
    /// what the bundle's own manifest carries.
    ManifestVocabExtensions,
    /// `commit_side_manifest`: the live registry, roster and declarations restated, the manifest
    /// write and its fsyncs. The commit point.
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
    /// Dropping the superseded generation's buffer, O(buffered), when no request still holds it.
    DropSuperseded,
    /// A publication that discarded its flush: the time from its last lap to its return.
    Discarded,
    /// The whole of `publish_flush`; overlaps `Compose` through `Discarded` rather than adding.
    PublishWall,
    // ---- the pool ----
    /// `promote`: the dictionary-first resolve, the extent write and the postings transpose.
    Promote,
    /// Narrowing each buffered row to the segment's render tail.
    Rows,
    /// `write_flush_segment`: the Morton sort, the segment's four files, their digests and fsyncs.
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
    /// `write_text_extents`: the entity-scoped `text` layers, partitioned by `Text*` below.
    TextExtents,
    /// Gathering one column's rows from the plan and creating the layer's directory.
    TextRows,
    /// Tokenising each row's prose and inserting each token into the term-to-postings map.
    TextTokeniseTerms,
    /// `write_sorted_dict` over the term map's already-ordered keys.
    TextDict,
    /// Collecting and encoding the posting lists and writing `postings.arrow`.
    TextPostings,
    /// Serialising and writing the presence bitmap.
    TextPresence,
    /// Reading every file written outside the segment directory back for its SHA-256.
    Digests,
    /// Memory-mapping the segment's `morton.u32` and `columns.arrow` for the generation.
    Reopen,
    /// Resolving the segment against every spatial level of its view.
    Shapes,
    /// Dropping the plan's buffered items and the promotion's postings, O(rows).
    DropPlan,
    /// An execution that failed: the time from its last lap to its return.
    Failed,
    /// The whole of `execute_flush`; overlaps `Promote` through `Failed`. Declared last, which
    /// [`FlushStage::COUNT`] is checked against.
    PoolWall,
}

// COUNT sizes every array indexed by `as usize`; keep it after the last variant.
const _: () = assert!(FlushStage::COUNT == FlushStage::PoolWall as usize + 1);

impl FlushStage {
    pub const COUNT: usize = 38;
    /// The executor's stages in run order, the `Manifest*` sub-stages after the stage they
    /// partition; `Plan`/`Dispatch` run at the tick, the rest at publication.
    pub const EXECUTOR: [FlushStage; 18] = [
        FlushStage::Plan,
        FlushStage::Dispatch,
        FlushStage::Compose,
        FlushStage::Manifest,
        FlushStage::ManifestClone,
        FlushStage::ManifestDenyState,
        FlushStage::ManifestVocabExtensions,
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
    /// The pool's stages in run order, the `Text*` sub-stages after the stage they partition.
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
    /// The stages that partition `Manifest`, in the order the publication runs them.
    pub const MANIFEST: [FlushStage; 3] = [
        FlushStage::ManifestClone,
        FlushStage::ManifestDenyState,
        FlushStage::ManifestVocabExtensions,
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
            FlushStage::ManifestClone => "  .manifest_clone",
            FlushStage::ManifestDenyState => "  .deny_state",
            FlushStage::ManifestVocabExtensions => "  .vocab_extensions",
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

/// One `execute_flush`'s laps, added to the executor's health when the call returns. Local to
/// the call so a flush still running is in no total.
#[derive(Debug)]
pub(crate) struct FlushLaps {
    /// Compiles to nothing without `bench-timing`.
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
    /// `bench-timing`.
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

/// The `Text*` sub-laps of one text layer's write: the pool's laps and the mark they run from.
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

/// One flush's immutable plan: the items of one view that will acquire geometry. The view itself
/// is not carried; the caller already holds it.
#[derive(Debug)]
pub(crate) struct FlushPlan {
    /// Ascending by entity id, deleted entities already removed; its ends give the segment's
    /// entity range.
    pub(crate) items: Vec<(EntityId, BufferedItem)>,
}

impl FlushPlan {
    /// The rows that carry entity-space facts: this entity's label, attributes and prose. A join
    /// is excluded, since its entity-space facts are already written by an earlier flush.
    pub(crate) fn entity_space_items(&self) -> impl Iterator<Item = &(EntityId, BufferedItem)> {
        self.items.iter().filter(|(_, item)| !item.join)
    }

    /// [`Self::entity_space_items`], ascending by entity id: the order extent writers require.
    pub(crate) fn value_rows(&self) -> impl Iterator<Item = &(EntityId, BufferedItem)> {
        self.entity_space_items()
    }

    /// Every row this flush publishes, joins included: a scoped value belongs to the
    /// `(entity, view)` pair, which a join brings.
    pub(crate) fn scoped_value_rows(&self) -> impl Iterator<Item = &(EntityId, BufferedItem)> {
        self.items.iter()
    }
}

/// Why a tick published nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoFlush {
    /// Nothing buffered for this view, or everything buffered for it is deleted.
    NothingToFlush,
    /// The WAL is poisoned: an under-durable delete is applied in memory but was answered 500,
    /// and a restart is expected to make the item visible again. Flushing it would make the
    /// un-acked delete permanent.
    WalPoisoned,
    /// The in-memory overlay has diverged from the durable WAL: a recovery left the node holding
    /// dispositions no WAL record backs. Publishing from that overlay would make a 500'd,
    /// never-acked deny permanent.
    OverlayDiverged,
    /// A partition is serving a stepped-down side-manifest. A flush from this state would commit
    /// a manifest from the older served state, permanently shadowing the stepped-past segment.
    SteppedDown,
}

/// Plan a flush of `view` against `generation`. Pure: reads only the generation, so the same
/// generation always yields the same plan. The WAL and overlay postures are passed in because
/// they are the executor's health, not the generation's.
pub(crate) fn plan_flush(
    generation: &Generation,
    view: &str,
    wal_poisoned: bool,
    overlay_diverged: bool,
) -> Result<FlushPlan, NoFlush> {
    // Checked before any work, so a gated tick never builds a plan only to discard it. Step-down
    // is read from the generation's bundle rather than passed in, since it is bundle state.
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
    if items.is_empty() {
        return Err(NoFlush::NothingToFlush);
    }
    // A row buffered under an earlier arity holds nothing for a column declared since; padding
    // here means every positional read below sees one consistent schema.
    let declared = &generation.bundle.manifest.declared_scalars;
    for (_, item) in items.iter_mut() {
        crate::attributes::pad_to_schema(&mut item.scalars, declared);
    }
    // The buffer is a hash map, so order is arbitrary until sorted. `write_flush_segment` requires
    // ascending entity id.
    items.sort_unstable_by_key(|(entity, _)| entity.raw());

    Ok(FlushPlan { items })
}

/// Everything the background pool needs to turn a [`FlushPlan`] into durable files. Taken from
/// the generation on the executor thread and then immutable: the pool holds no reference to live
/// state.
pub(crate) struct FlushContext {
    pub(crate) prefix_dir: PathBuf,
    pub(crate) partition: String,
    pub(crate) view: String,
    /// The incarnation of `view` this flush writes into. A view whose incarnation cannot be resolved is not flushed.
    pub(crate) incarnation: mosaica_types::view::ViewIncarnation,
    pub(crate) seg_id: String,
    pub(crate) row_base: u32,
    /// The view's entity floor when planned: the rows of entities below it are listed in the
    /// segment's extent.
    pub(crate) entity_floor: u64,
    pub(crate) identity_key: IdentityKey,
    pub(crate) shard_id: u32,
    pub(crate) quantisation: Quantisation,
    pub(crate) scalar_schema: Vec<(String, ScalarType)>,
    /// Where each of `scalar_schema`'s columns sits in a buffered row's scalar list.
    pub(crate) render_indices: Vec<usize>,
    /// The columns the bands copy beside the render tail, and where each sits in a buffered row.
    pub(crate) band_schema: Vec<(String, ScalarType)>,
    pub(crate) band_indices: Vec<usize>,
    /// The filterable columns and where each one's value sits in a buffered row, entity-space, separate from the render tail.
    pub(crate) filter_schema: Vec<FilterColumnSpec>,
    /// The blob-resident columns — neither indexed nor rendered, never a category — and their positions in a buffered row.
    pub(crate) record_schema: Vec<RecordColumnSpec>,
    /// This view's group-scoped attribute families, in the owning group's manifest order; empty for a plain view.
    pub(crate) scoped_schema: Vec<ScopedColumnSpec>,
    /// The view id this flush's scoped columns are addressed by: the owning group's view of the same key, or [`FlushContext::view`] itself.
    pub(crate) scoped_view: String,
    /// The incarnation of [`FlushContext::scoped_view`]; under a sharing door this differs from [`FlushContext::incarnation`]'s view.
    pub(crate) scoped_incarnation: mosaica_types::view::ViewIncarnation,
    /// One entry per lane in `scalar_schema`'s scoped suffix; `None` for a lane this view renders but this flush's batches could not name.
    pub(crate) scoped_render: Vec<Option<usize>>,
    /// The indexed `text` columns, each with the analyser its declaration resolved; a text column has no value column.
    pub(crate) text_schema: Vec<TextColumnSpec>,
    /// The dictionary the plan's terms were resolved against, and the one promotion extends.
    pub(crate) dict: Arc<Dict>,
    /// The descriptor bytes behind every extension term id this plan's items carry; empty in the steady state.
    pub(crate) novel_descriptors: FxHashMap<TermId, Vec<u8>>,
    /// [`mosaica_authz::MAX_DISTINCT_TERMS`] unless a test lowers it, enforced at promotion (see
    /// [`promote`]), the one path by which a caller grows the dictionary.
    pub(crate) max_distinct_terms: u64,
    pub(crate) prefix: String,
    /// The view's spatial levels as held when planned; the new segment's rows are resolved against these on the pool.
    pub(crate) shapes: Vec<Arc<crate::shapes::ShapeLevel>>,
    /// The unique columns when planned, each with its position in a buffered row and its type.
    pub(crate) unique_schema: Vec<(String, usize, ScalarType)>,
    /// The edited-items map when planned: each planned entity's number is read from it on the pool.
    pub(crate) edited: Arc<mosaica_store::edited::EditedIndex>,
    pub(crate) edited_live: Arc<crate::edited::EditedLive>,
}

/// A flush whose files are durable, awaiting manifest assembly and the swap on the executor. The side-manifest is
/// the commit point, written at publication, not here: a crash while one of these is in flight leaves orphan files
/// nothing references, and the next tick re-plans.
pub(crate) struct CompletedFlush {
    pub(crate) partition: String,
    pub(crate) view: String,
    /// The entity ids removed from the buffer at publication: exactly what was consumed, not a range.
    pub(crate) consumed: Vec<EntityId>,
    /// The row space this flush gave the plan's rows.
    pub(crate) segment: Option<SegmentFlush>,
    /// `Some` iff this flush promoted; its digest is already in `files`.
    pub(crate) dict_extent: Option<DictExtent>,
    /// One entry per filterable column: this flush's values for the entities it published.
    pub(crate) filter_extents: Vec<crate::filter::OpenedExtent>,
    /// This flush's record-blob extent, or `None` where the schema declares no blob-resident column.
    pub(crate) record_extent: Option<RecordExtent>,
    /// This flush's slice of the entity-to-term transpose. Never absent, unlike the record extent.
    pub(crate) entity_terms_extent: mosaica_store::manifest::EntityTermsExtent,
    /// The `(family, view)` pairs this flush gave a column; a view left off this list renders nothing on restart.
    pub(crate) scoped_columns: Vec<(String, String)>,
    /// The incarnation of the view [`CompletedFlush::scoped_columns`] names, which under a
    /// sharing door is the owning group's rather than the flushing view's.
    pub(crate) scoped_incarnation: mosaica_types::view::ViewIncarnation,
    /// This flush's text layers, one per indexed `text` column, composed onto the live generation at publication.
    pub(crate) text_extents: Vec<mosaica_store::manifest::TextExtent>,
    /// Every file this flush wrote, prefix-relative, with its digest — computed on the pool.
    pub(crate) files: std::collections::BTreeMap<String, FileDigest>,
    /// The dictionary including this flush's promotions, republished with the geometry.
    pub(crate) dict: Arc<Dict>,
    /// `Some(len)` if this flush wrote a dictionary extent; `None` if it promoted nothing. Read by the publication guard.
    pub(crate) promoted_from_dict_len: Option<u32>,
    pub(crate) prefix: String,
    /// The unique columns this flush was planned under; one published under another set would
    /// leave some rows' values in no index.
    pub(crate) unique_columns: Vec<String>,
    /// One entry per unique column: the runs this flush wrote for it and the entries in them.
    pub(crate) unique_runs: Vec<FlushedUnique>,
    /// The edited-items runs this flush wrote, prefix-relative and digested into `files`, and
    /// the entities whose live pairs they hold.
    pub(crate) edited_runs: FlushedEdited,
}

/// The edited-items runs one flush wrote, and the live pairs they replace.
#[derive(Default)]
pub(crate) struct FlushedEdited {
    pub(crate) by_number: Vec<mosaica_store::manifest::BaseKeyRun>,
    pub(crate) by_entity: Vec<mosaica_store::manifest::BaseKeyRun>,
    pub(crate) entities: Vec<u32>,
}

/// The runs one flush wrote into one unique column's index, and the live entries they replace.
pub(crate) struct FlushedUnique {
    pub(crate) attribute: String,
    /// Prefix-relative; already digested into [`CompletedFlush::files`].
    pub(crate) runs: Vec<String>,
    pub(crate) written: Vec<(mosaica_store::unique::UniqueKey, EntityId)>,
}

/// The half of a completed flush that exists only where the plan gave rows geometry.
pub(crate) struct SegmentFlush {
    pub(crate) segment: SegmentData,
    pub(crate) extent: SegmentExtent,
    /// Ingredients for the manifest: assembled at publication, with deny fields serialised fresh from the overlay.
    pub(crate) descriptor: mosaica_store::manifest::SegmentDescriptor,
    pub(crate) watermark: u64,
    pub(crate) entity_id_high_water: u64,
    pub(crate) tier: Arc<DeltaTier>,
    /// The tier's prefix-relative path; carried rather than re-derived, since a coalesce moves it.
    pub(crate) tier_path: String,
    /// The tier measured as encoded, computed on the pool beside the write it measures.
    pub(crate) tier_tally: mosaica_lifecycle::window::FragmentationTally,
    /// The segment's membership in every spatial level of its view, resolved on the pool.
    pub(crate) shape_pieces: Vec<crate::shapes::ShapePiece>,
}

/// Turn a plan into durable files. Runs on the background pool, over immutable inputs. The side-manifest is written
/// last because it is the commit point: a crash before it leaves orphan files nothing references.
///
/// `laps` receives the pool's [`FlushStage`]s: each stage's wall clock, and `PoolWall` for the whole call.
pub(crate) fn execute_flush(
    plan: FlushPlan,
    ctx: FlushContext,
    laps: &mut FlushLaps,
) -> Result<CompletedFlush, MaintenanceFailed> {
    let wall = StageMark::now();
    let mut mark = wall;
    let result = execute_flush_stages(plan, ctx, laps, &mut mark);
    if result.is_err() {
        // A failed flush's time since its last lap, so `PoolWall` stays partitioned.
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
) -> Result<CompletedFlush, MaintenanceFailed> {
    let consumed: Vec<EntityId> = plan.items.iter().map(|(entity, _)| *entity).collect();
    let numbers = numbers_of(&plan, &ctx)?;

    // ---- promotion: term ids allocated downward from `u32::MAX` stay unsatisfiable until promoted here.
    let promotion = promote(&plan, &ctx)?;
    let promoted_from = promotion.extent.as_ref().map(|_| ctx.dict.len());
    *mark = laps.lap(FlushStage::Promote, *mark);

    // ---- the segment: scalars narrowed to the render subset.
    let mut rows: Vec<FlushRow> = Vec::with_capacity(plan.items.len());
    for ((entity, item), number) in plan.items.iter().zip(&numbers) {
        let mut scalars = Vec::with_capacity(ctx.render_indices.len() + ctx.scoped_render.len());
        // Positionally parallel to `render_indices`, the render subset in declaration order.
        for &index in &ctx.render_indices {
            let value = buffered_value(item, BufferedPlace::Entity(index));
            // Absence is not resolved here: `write_flush_segment` records it in the presence bitmap.
            scalars.push(value.clone());
        }
        // The group-scoped render lanes follow the declared ones.
        for index in &ctx.scoped_render {
            // `None` is a lane this view renders but does not write; it takes the same absence.
            let value = index
                .and_then(|index| item.scoped.get(index))
                .unwrap_or(&WalScalar::Null);
            scalars.push(value.clone());
        }
        // Then the columns the bands copy, which `columns.arrow` does not hold.
        for &index in &ctx.band_indices {
            scalars.push(buffered_value(item, BufferedPlace::Entity(index)).clone());
        }
        rows.push(FlushRow {
            entity_id: *entity,
            number: *number,
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
                    indexed: &ctx.band_schema,
                    row_base: ctx.row_base,
                    entity_floor: ctx.entity_floor,
                },
            )
            .map_err(|e| MaintenanceFailed(format!("segment: {e}")))?,
        )
    };
    *mark = laps.lap(FlushStage::Segment, *mark);

    // ---- the delta postings tier: carries the postings of the entities the segment gave rows.
    let tier_bits = if out.is_some() {
        let tier_tally = mosaica_lifecycle::window::FragmentationTally::of_tier(
            &promotion.postings,
            plan.items.len() as u64,
        );
        let tier_rel = format!(
            "partitions/{}/{}/segments/{}/delta.arrow",
            ctx.partition,
            mosaica_store::view_rel(&ctx.view),
            ctx.seg_id
        );
        let tier_path = ctx.prefix_dir.join(&tier_rel);
        write_delta_tier(&tier_path, &promotion.postings, SMALL_TERM_THRESHOLD)
            .map_err(|e| MaintenanceFailed(format!("delta tier: {e}")))?;
        let tier = Arc::new(
            DeltaTier::open(&tier_path).map_err(|e| MaintenanceFailed(format!("tier: {e}")))?,
        );
        Some((tier, tier_rel, tier_tally))
    } else {
        None
    };
    *mark = laps.lap(FlushStage::DeltaTier, *mark);

    // ---- the manifest's ingredients: the executor assembles and writes the manifest at publication, since `n`
    // cannot be allocated here. Every file below the segment's own four is digested in one pass.
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

    // ---- the filter columns' extents ---------------------------------------------------------------
    let filter_extents = write_filter_extents(&plan, &ctx)?;
    for extent in &filter_extents {
        // The dictionary is digested with the values: a keyword extent's ordinals need it to be read.
        for rel in [&extent.extent.values, &extent.extent.presence]
            .into_iter()
            .chain(extent.extent.dict.as_ref())
        {
            to_digest.push(rel.clone());
        }
    }
    let unique_runs = write_unique_runs(&plan, &ctx)?;
    for flushed in &unique_runs {
        to_digest.extend(flushed.runs.iter().cloned());
    }
    let edited_runs = write_edited_runs(&plan, &numbers, &ctx)?;
    to_digest.extend(
        edited_runs
            .by_number
            .iter()
            .chain(&edited_runs.by_entity)
            .map(|run| run.path.clone()),
    );
    *mark = laps.lap(FlushStage::FilterExtents, *mark);

    // ---- the entity-to-term transpose extent: written from the promotion, so its ordinals match the tier's.
    let entity_terms_extent = write_entity_terms_extent(&promotion.per_entity, &ctx)?;
    for rel in entity_terms_extent.files() {
        to_digest.push(rel.to_string());
    }
    *mark = laps.lap(FlushStage::EntityTerms, *mark);

    // ---- the group-scoped column families' extents: composed like the entity-scoped extents above.
    let ScopedWrite {
        extents: scoped_extents,
        texts: scoped_texts,
        created: scoped_columns,
    } = write_scoped_extents(&plan, &ctx)?;
    for extent in &scoped_extents {
        for rel in [&extent.extent.values, &extent.extent.presence]
            .into_iter()
            .chain(extent.extent.dict.as_ref())
        {
            to_digest.push(rel.clone());
        }
    }
    // A base a view acquired at this flush is digested too: the build never wrote it.
    for (column, view) in &scoped_columns {
        // The writer's own derivation, so the base is digested at the exact path it was written to.
        let rel =
            mosaica_store::scoped_column_rel(&ctx.partition, column, view, ctx.scoped_incarnation);
        let prose = |name: &str| format!("{}/{name}", mosaica_store::manifest::SCOPED_PROSE_DIR);
        for name in [
            mosaica_filter::VALUES_FILE.to_string(),
            mosaica_filter::PRESENCE_FILE.to_string(),
            mosaica_filter::DICT_FILE.to_string(),
            "postings.arrow".to_string(),
            prose(mosaica_filter::RECORD_BLOCKS_FILE),
            prose(mosaica_filter::RECORD_HASROW_FILE),
            prose(mosaica_filter::RECORD_DIRECTORY_FILE),
        ] {
            if ctx.prefix_dir.join(&rel).join(&name).exists() {
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

    // ---- the record-blob extent --------------------------------------------------------------------
    let record_extent = write_record_extent(&plan, &ctx)?;
    if let Some(extent) = &record_extent {
        for rel in extent.files() {
            to_digest.push(rel.to_string());
        }
    }
    *mark = laps.lap(FlushStage::RecordExtent, *mark);
    let mut text_extents = write_text_extents(&plan, &ctx, laps, *mark)?;
    text_extents.extend(scoped_texts);
    // Every file of every text extent is digested: `publish_fold` discards the fold if one digest
    // is missing.
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
            digest_of(&ctx.prefix_dir.join(&rel))?,
        );
    }
    *mark = laps.lap(FlushStage::Digests, *mark);

    let segment = match &out {
        None => None,
        Some(out) => {
            let seg_dir = segment_dir(&ctx);
            let entities = mosaica_store::edited::RowEntities::Listed(std::sync::Arc::new(
                mosaica_store::edited::EditedRows::open(
                    &seg_dir,
                    mosaica_store::edited::lists_edited_rows(out.files.keys()),
                )
                .map_err(|e| MaintenanceFailed(e.to_string()))?,
            ));
            Some(
                SegmentData::load(&seg_dir, &ctx.seg_id, out.segment.row_count, entities)
                    .map_err(|e| MaintenanceFailed(e.to_string()))?,
            )
        }
    };
    *mark = laps.lap(FlushStage::Reopen, *mark);

    // ---- the shape memberships: resolved on the pool before the generation is published. A panic here fails the
    // flush whole: nothing is published and the buffer stands.
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

    // Exists exactly where the plan gave rows geometry.
    let segment = match (segment, out, tier_bits) {
        (Some(segment), Some(out), Some((tier, tier_path, tier_tally))) => Some(SegmentFlush {
            segment,
            extent: out.extent,
            descriptor: out.segment,
            watermark: out.watermark,
            entity_id_high_water: out.entity_id_high_water,
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
        segment,
        dict_extent,
        filter_extents,
        record_extent,
        entity_terms_extent,
        text_extents,
        scoped_columns,
        scoped_incarnation: ctx.scoped_incarnation,
        files,
        dict: promotion.dict,
        promoted_from_dict_len: promoted_from,
        prefix: ctx.prefix,
        unique_columns: ctx.unique_schema.iter().map(|(name, _, _)| name.clone()).collect(),
        unique_runs,
        edited_runs,
    };
    // Freed under a stage rather than at the return, so this O(rows) allocator work is attributed.
    drop(plan);
    drop(promotion.postings);
    drop(promotion.per_entity);
    laps.lap(FlushStage::DropPlan, *mark);
    Ok(completed)
}

/// Why a background pass — a flush, a merge, a coalesce or a fold — produced nothing. A manifest
/// is the only commit point, so a failure before it leaves orphan files nothing references and
/// the tick retries.
#[derive(Debug)]
pub(crate) struct MaintenanceFailed(pub(crate) String);

impl std::fmt::Display for MaintenanceFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// For `map_err`: an error from a step of a background pass, prefixed with what the step was.
pub(crate) fn failed<E: std::fmt::Display>(
    what: impl std::fmt::Display,
) -> impl FnOnce(E) -> MaintenanceFailed {
    move |e| MaintenanceFailed(format!("{what}: {e}"))
}

/// Removes the spool if `outcome` failed. On success the spool writer's `finish` has removed it.
pub(crate) fn remove_spool_on_error<T, E>(outcome: Result<T, E>, spool: &Path) -> Result<T, E> {
    if outcome.is_err() {
        let _ = std::fs::remove_file(spool);
    }
    outcome
}

/// What promotion produced: the dictionary to republish, the extent naming the new ordinals, and
/// the tier's postings in those ordinals.
struct Promotion {
    dict: Arc<Dict>,
    extent: Option<DictExtent>,
    /// `(term, entities)` ascending by term — [`write_delta_tier`]'s contract.
    postings: Vec<(TermId, Vec<u32>)>,
    /// The same relation transposed: `(entity, terms)` ascending by entity, sorted and deduplicated; built here, where the durable ordinals exist.
    per_entity: Vec<(u32, Vec<u32>)>,
}

/// Promote every extension-id descriptor the plan carries to a durable dictionary ordinal, and
/// express the plan's postings in those ordinals. A promoted descriptor is satisfiable only by
/// sessions authorised after this flush; an item still buffered under an old extension id for an
/// already-promoted descriptor stays invisible until its own flush.
///
/// The dictionary is checked before anything is interned, so two dictionary loads of the same
/// bundle assign the same ordinals. An extension id with no descriptor fails the flush rather
/// than dropping the term.
fn promote(plan: &FlushPlan, ctx: &FlushContext) -> Result<Promotion, MaintenanceFailed> {
    let dict_len = ctx.dict.len();
    let mut by_term: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    // Descriptors this flush interns, in assignment order: the extent's contents and `Dict::extended_with`'s input.
    let mut interned: Vec<Vec<u8>> = Vec::new();
    let mut assigned: FxHashMap<u32, u32> = FxHashMap::default();

    // One entry per entity-space item; an item with an empty label set is a value, not an absence.
    let mut per_entity: Vec<(u32, Vec<u32>)> = Vec::with_capacity(plan.items.len());
    for (entity, item) in plan.entity_space_items() {
        let entity = narrow_entity(*entity)?;
        let mut mine: Vec<u32> = Vec::with_capacity(item.terms.len());
        for term in &item.terms {
            // Below the dictionary's length: already a durable ordinal, nothing to do.
            let ordinal = if term.raw() < dict_len {
                term.raw()
            } else if let Some(&ordinal) = assigned.get(&term.raw()) {
                ordinal
            } else {
                let descriptor = ctx.novel_descriptors.get(term).ok_or_else(|| {
                    MaintenanceFailed(format!(
                        "extension term {} has no descriptor in this flush's snapshot — see \
                         promote()'s doc; a term must never be dropped silently",
                        term.raw()
                    ))
                })?;
                let ordinal = match ctx.dict.lookup(descriptor) {
                    // An earlier flush already promoted it: use that ordinal and write nothing.
                    Some(existing) => existing.raw(),
                    None => {
                        let next = u64::from(dict_len) + interned.len() as u64;
                        // Past `max_distinct_terms` a dictionary ordinal could alias a live extension id.
                        if next >= ctx.max_distinct_terms {
                            return Err(MaintenanceFailed(format!(
                                "promoting this flush's novel descriptors would carry the \
                                 dictionary to {next}, at or past the most it is sized \
                                 for, {}; refusing rather than assigning an \
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
        // A buffered row's descriptors are not deduplicated upstream, so this is required.
        mine.sort_unstable();
        mine.dedup();
        per_entity.push((entity, mine));
    }
    // Sorted even though the plan's items already ascend: the extent's ranks address its lists.
    per_entity.sort_unstable_by_key(|(entity, _)| *entity);

    let mut postings = Vec::with_capacity(by_term.len());
    for (term, mut entities) in by_term {
        // `encode_posting` requires a strictly ascending list; descriptors are not deduplicated upstream.
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
    std::fs::create_dir_all(&seg_dir)
        .map_err(|e| MaintenanceFailed(format!("dict extent dir: {e}")))?;
    let mut writer = DictStreamWriter::new(&seg_dir);
    for descriptor in &interned {
        writer
            .append(descriptor)
            .map_err(|e| MaintenanceFailed(format!("dict extent: {e}")))?;
    }
    writer
        .finish()
        .map_err(|e| MaintenanceFailed(format!("dict extent: {e}")))?;

    Ok(Promotion {
        // Built from the same sequence that named the tier, never re-read from the file just
        // written.
        dict: Arc::new(ctx.dict.extended_with(&interned)),
        extent: Some(DictExtent {
            path: format!(
                "partitions/{}/{}/segments/{}/terms-0.dict",
                ctx.partition,
                mosaica_store::view_rel(&ctx.view),
                ctx.seg_id
            ),
            records: interned.len() as u64,
        }),
        postings,
        per_entity,
    })
}

/// One filterable column, and where its value sits in a buffered row's positional scalar list.
/// The index is positional against `MANIFEST.declared_scalars`, the same contract the commit
/// window uses to resolve a category key to a code.
#[derive(Debug, Clone)]
pub(crate) struct FilterColumnSpec {
    pub(crate) index: usize,
    pub(crate) name: String,
    pub(crate) ty: ScalarType,
    /// A category, so its values are vocabulary codes and code 0 means *absent*.
    pub(crate) category: bool,
}

/// Write one extent per filterable column, covering exactly the entities this flush publishes.
/// Every declared filter column gets one, even a column no flushed entity carries a value in, so
/// the file set is predictable from the manifest alone. A deleted entity acquires no slot here.
/// A keyword's sort and front-coding happen here, on the pool, not on the serial commit path.
fn write_filter_extents(
    plan: &FlushPlan,
    ctx: &FlushContext,
) -> Result<Vec<crate::filter::OpenedExtent>, MaintenanceFailed> {
    let mut out = Vec::with_capacity(ctx.filter_schema.len());
    for spec in &ctx.filter_schema {
        let column = extent_values(spec, entity_scoped_rows(spec, plan)?)?;
        let column_rel = format!("partitions/{}/attrs/{}", ctx.partition, spec.name);
        let column_dir = ctx.prefix_dir.join(&column_rel);
        out.push(write_value_extent(
            ctx,
            &column_dir,
            &spec.name,
            None,
            &column,
        )?);
    }
    Ok(out)
}

/// Each planned entity's number: its live pair's, its run pair's, or the entity itself.
fn numbers_of(plan: &FlushPlan, ctx: &FlushContext) -> Result<Vec<EntityId>, MaintenanceFailed> {
    let mut numbers: Vec<EntityId> = plan.items.iter().map(|(entity, _)| *entity).collect();
    let mut unresolved: Vec<usize> = Vec::new();
    for (at, entity) in numbers.iter_mut().enumerate() {
        let raw = narrow_entity(*entity)?;
        match ctx.edited_live.number_of(raw) {
            Some(number) => *entity = EntityId::new(u64::from(number)),
            None => unresolved.push(at),
        }
    }
    if unresolved.is_empty() {
        return Ok(numbers);
    }
    // A flush's entities are mostly new ones, above every entity a run gives a number.
    let highest = ctx
        .edited
        .highest_entity()
        .map_err(failed("the edited items' runs"))?;
    let mut asked: Vec<u32> = Vec::new();
    unresolved.retain(|&at| {
        let raw = numbers[at].raw() as u32;
        let in_runs = highest.is_some_and(|highest| raw <= highest);
        if in_runs {
            asked.push(raw);
        }
        in_runs
    });
    for (at, number) in ctx
        .edited
        .numbers_of(&asked)
        .map_err(failed("the edited items' runs"))?
    {
        numbers[unresolved[at]] = EntityId::new(u64::from(number));
    }
    Ok(numbers)
}

/// Write the edited-items runs of every planned entity whose pair is live, fsynced: the first
/// flush to give such an entity a row holds its pair from then on.
fn write_edited_runs(
    plan: &FlushPlan,
    numbers: &[EntityId],
    ctx: &FlushContext,
) -> Result<FlushedEdited, MaintenanceFailed> {
    let mut pairs: Vec<(u32, u32)> = Vec::new();
    for ((entity, _), number) in plan.items.iter().zip(numbers) {
        let raw = narrow_entity(*entity)?;
        if ctx.edited_live.number_of(raw).is_some() {
            pairs.push((narrow_entity(*number)?, raw));
        }
    }
    pairs.sort_unstable();
    pairs.dedup();
    if pairs.is_empty() {
        return Ok(FlushedEdited::default());
    }
    let written = mosaica_store::edited::write_edited_runs(
        &ctx.prefix_dir,
        &ctx.partition,
        &ctx.seg_id,
        &pairs,
    )
    .map_err(failed("the edited items' runs"))?;
    let paths: Vec<std::path::PathBuf> = written.paths().map(Path::to_path_buf).collect();
    mosaica_store::fsync_written(&paths).map_err(failed("the edited items' runs"))?;
    let based = |runs: &[mosaica_store::key_index::WrittenRun<u32>]| {
        runs.iter()
            .map(|run| mosaica_store::edited::as_base(&ctx.prefix_dir, run))
            .collect::<Result<Vec<_>, _>>()
            .map_err(failed("the edited items' runs"))
    };
    Ok(FlushedEdited {
        by_number: based(&written.by_number)?,
        by_entity: based(&written.by_entity)?,
        entities: pairs.iter().map(|(_, entity)| *entity).collect(),
    })
}

/// Write a run per unique column from the values this flush writes, fsynced: an entity's own row
/// alone, never a join, which carries no entity-scoped value.
fn write_unique_runs(
    plan: &FlushPlan,
    ctx: &FlushContext,
) -> Result<Vec<FlushedUnique>, MaintenanceFailed> {
    let mut out = Vec::with_capacity(ctx.unique_schema.len());
    for (attribute, at, ty) in &ctx.unique_schema {
        let Some(kind) = mosaica_store::unique::KeyKind::of(*ty) else {
            continue;
        };
        let mut entries: Vec<(mosaica_store::unique::UniqueKey, u32)> = Vec::new();
        for (entity, item) in plan.value_rows() {
            if let Some(key) = item
                .scalars
                .get(*at)
                .and_then(|v| mosaica_store::unique::key_of(*ty, v))
            {
                entries.push((key, narrow_entity(*entity)?));
            }
        }
        entries.sort_unstable();
        entries.dedup();
        let dir_rel = mosaica_store::unique::index_dir_rel(&ctx.partition, attribute);
        let written = mosaica_store::unique::write_sorted_runs(
            kind,
            &ctx.prefix_dir.join(&dir_rel),
            &ctx.seg_id,
            &entries,
        )
        .map_err(failed(format!("unique index run for '{attribute}'")))?;
        let paths: Vec<PathBuf> = written.iter().map(|run| run.path.clone()).collect();
        mosaica_store::fsync_written(&paths)
            .map_err(failed(format!("unique index run for '{attribute}'")))?;
        let runs = written
            .iter()
            .map(|run| mosaica_store::unique::relative(&ctx.prefix_dir, &run.path))
            .collect::<Result<Vec<_>, _>>()
            .map_err(failed(format!("unique index run for '{attribute}'")))?;
        out.push(FlushedUnique {
            attribute: attribute.clone(),
            runs,
            written: entries
                .into_iter()
                .map(|(key, entity)| (key, EntityId::new(u64::from(entity))))
                .collect(),
        });
    }
    Ok(out)
}

/// Writes one column's values, presence and keyword dictionary under `column_dir` and reopens them for publication.
/// `view` is the group-scoped family's view, or `None` for an entity-scoped column.
fn write_value_extent(
    ctx: &FlushContext,
    column_dir: &Path,
    name: &str,
    view: Option<String>,
    column: &ExtentColumn<'_>,
) -> Result<crate::filter::OpenedExtent, MaintenanceFailed> {
    let scoped = view.is_some();
    let what = if scoped { "scoped extent" } else { "filter extent" };
    let (values_path, presence_path, dict_path) = mosaica_filter::write_extent(
        column_dir,
        &ctx.seg_id,
        &column.codes,
        &column.presence,
        column.dict_keys.as_deref(),
    )
    .map_err(|e| MaintenanceFailed(format!("{what} for '{name}': {e}")))?;
    // Derived from the paths just written, so the manifest names what is on disk or nothing.
    let rel = |path: &Path| -> Result<String, MaintenanceFailed> {
        path.strip_prefix(&ctx.prefix_dir)
            .ok()
            .and_then(|p| p.to_str())
            .map(str::to_string)
            .ok_or_else(|| {
                MaintenanceFailed(format!("{what} path {} is not under the prefix", path.display()))
            })
    };
    let values =
        mosaica_filter::open_extent(&values_path, &presence_path, mosaica_filter::Access::Mapped)
            .map_err(|e| MaintenanceFailed(format!("{what} for '{name}': {e}")))?;
    let dict = dict_path
        .as_ref()
        .map(|path| {
            mosaica_filter::SortedDict::open(path, mosaica_filter::Access::Mapped)
                .map(Arc::new)
                .map_err(|e| {
                    MaintenanceFailed(if scoped {
                        format!("scoped keyword dictionary '{name}': {e}")
                    } else {
                        format!("keyword dictionary for '{name}': {e}")
                    })
                })
        })
        .transpose()?;
    Ok(crate::filter::OpenedExtent {
        extent: mosaica_store::manifest::AttrExtent {
            column: name.to_string(),
            // `None` for an entity-scoped column, which belongs to no view: the incarnation
            // follows the view exactly, and a view here is the owner's rather than the flushing
            // one's.
            incarnation: view.as_ref().map(|_| ctx.scoped_incarnation),
            view,
            values: rel(&values_path)?,
            presence: rel(&presence_path)?,
            // One record, so the layer's files swap as one: an extent's ordinals are positions in
            // that extent's dictionary, and a reader that saw a new dictionary beside old
            // ordinals would recolour the window.
            dict: dict_path.as_ref().map(|p| rel(p)).transpose()?,
            postings: None,
            offsets: None,
        },
        values: Arc::new(values),
        dict,
    })
}

/// One column's values for this flush's entities, and the entities that carry one. Absence is out of band: a
/// category spends its reserved code 0 for it; every other family has no spare value, so absence travels as
/// `WalScalar::Null` in the presence bitmap. A value of the wrong shape fails the flush, since such a mismatch
/// is a defect on the write path, not caller input.
fn extent_values<'a>(
    spec: &FilterColumnSpec,
    entities: Vec<(u32, &'a WalScalar)>,
) -> Result<ExtentColumn<'a>, MaintenanceFailed> {
    use mosaica_filter::Codes;

    let mut presence = croaring::Bitmap::new();

    let wrong = |value: &WalScalar| {
        MaintenanceFailed(format!(
            "column '{}' is declared {:?} but a buffered row carries {value:?}",
            spec.name, spec.ty
        ))
    };

    if spec.ty == ScalarType::Keyword {
        // A keyword arrives as a string on the wire and in the WAL; its ordinal is minted here and
        // exists nowhere upstream of this layer.
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
        // This extent's own dictionary, over this batch alone: its ordinals are not comparable
        // with the base's or any other extent's.
        let mut keys: Vec<&str> = held.clone();
        keys.sort_unstable();
        keys.dedup();
        let mut ordinals = Vec::with_capacity(held.len());
        for text in held {
            let ordinal = keys.binary_search(&text).map_err(|_| {
                MaintenanceFailed(format!(
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
            let code = category_code(value).ok_or_else(|| wrong(value))?;
            if code == mosaica_store::vocabulary::ABSENT_CODE {
                continue;
            }
            presence.add(entity);
            held.push(code);
        }
        // The declared width is the storage width: an extent stored at a different width from its
        // base would disagree with it at the next fold that concatenates them.
        let codes = match spec.ty {
            ScalarType::U8 => Codes::U8(held.iter().map(|&c| c as u8).collect::<Vec<_>>().into()),
            ScalarType::U16 => {
                Codes::U16(held.iter().map(|&c| c as u16).collect::<Vec<_>>().into())
            }
            _ => Codes::U32(held.into()),
        };
        return Ok(ExtentColumn::flat(codes, presence));
    }

    // A plain numeric: absent where the item carried no value, present otherwise, so a flushed
    // entity and a built one answer a range query identically.
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
        // Text owes no value column at all; its extent is a token dictionary, postings and a
        // presence bitmap, written by `write_text_extents` on its own track.
        ScalarType::Text => unreachable!("text owes no value column, so it has no extent column"),
        // `utf8` is the wire type of a keyword's value and a category's key; schema parse refuses
        // it as a declared storage type.
        ScalarType::Utf8 => unreachable!("`utf8` is not a declarable type"),
    };
    Ok(ExtentColumn::flat(codes, presence))
}

/// Where a column's value sits in a buffered row.
#[derive(Debug, Clone, Copy)]
pub(crate) enum BufferedPlace {
    /// An entity-scoped column, at this position of the row's `scalars`.
    Entity(usize),
    /// A group-scoped family, at this position of the row's `scoped` list.
    Scoped(usize),
}

/// A buffered row's value for one column. A row buffered before the column was declared holds
/// nothing there and answers `Null`.
pub(crate) fn buffered_value(item: &BufferedItem, place: BufferedPlace) -> &WalScalar {
    let (values, index) = match place {
        BufferedPlace::Entity(index) => (&item.scalars, index),
        BufferedPlace::Scoped(index) => (&item.scoped, index),
    };
    values.get(index).unwrap_or(&WalScalar::Null)
}

/// A category value's code, [`mosaica_store::vocabulary::ABSENT_CODE`] for `Null`, or `None` for
/// a value that is not a code. The ingest plane resolves a key and a null to a code before a row
/// is buffered.
pub(crate) fn category_code(value: &WalScalar) -> Option<u32> {
    match value {
        WalScalar::U8(c) => Some(u32::from(*c)),
        WalScalar::U16(c) => Some(u32::from(*c)),
        WalScalar::U32(c) => Some(*c),
        WalScalar::Null => Some(mosaica_store::vocabulary::ABSENT_CODE),
        _ => None,
    }
}

/// The `(entity, value)` pairs one entity-scoped column's extent covers: the plan's own rows, joins
/// excluded, since a join row's entity-space value was already written by an earlier flush.
fn entity_scoped_rows<'a>(
    spec: &FilterColumnSpec,
    plan: &'a FlushPlan,
) -> Result<Vec<(u32, &'a WalScalar)>, MaintenanceFailed> {
    let mut out = Vec::with_capacity(plan.items.len());
    for (entity, item) in plan.value_rows() {
        let entity = narrow_entity(*entity)?;
        out.push((entity, buffered_value(item, BufferedPlace::Entity(spec.index))));
    }
    Ok(out)
}

/// The `(entity, value)` pairs one view's column of a group-scoped family covers: every row this flush publishes,
/// a join included, since a scoped value belongs to the `(entity, view)` pair, not the entity.
fn scoped_rows<'a>(
    spec: &ScopedColumnSpec,
    plan: &'a FlushPlan,
) -> Result<Vec<(u32, &'a WalScalar)>, MaintenanceFailed> {
    let mut out = Vec::with_capacity(plan.items.len());
    for (entity, item) in plan.scoped_value_rows() {
        let entity = narrow_entity(*entity)?;
        out.push((entity, buffered_value(item, BufferedPlace::Scoped(spec.index))));
    }
    Ok(out)
}

/// One column's extent content: the values, the entities that carry one, and the dictionary those
/// values are ordinals into where the family has one.
struct ExtentColumn<'a> {
    codes: mosaica_filter::Codes,
    presence: croaring::Bitmap,
    /// Sorted and distinct, `Some` for keyword columns only.
    dict_keys: Option<Vec<&'a str>>,
}

impl ExtentColumn<'_> {
    /// A column whose values file carries the values themselves — every family but keyword.
    fn flat(codes: mosaica_filter::Codes, presence: croaring::Bitmap) -> Self {
        ExtentColumn {
            codes,
            presence,
            dict_keys: None,
        }
    }
}

/// One blob-resident column, and where its value sits in a buffered row's positional scalar list.
/// The index is also the row's field tag: the blob format tags a field by the column's position.
#[derive(Debug, Clone)]
pub(crate) struct RecordColumnSpec {
    pub(crate) index: usize,
    pub(crate) name: String,
    pub(crate) ty: ScalarType,
}

/// One indexed `text` column, for the flush's own pass over it. The analyser is carried rather
/// than looked up per row, since constructing one deserialises the segmenter's dictionaries.
#[derive(Clone)]
pub(crate) struct TextColumnSpec {
    /// Position in a buffered row's scalar list.
    pub(crate) index: usize,
    pub(crate) name: String,
    pub(crate) analyser: std::sync::Arc<mosaica_analyse::Analyser>,
}

/// One view's column of a group-scoped attribute family, and where its value sits in a buffered
/// row's `scoped` list. The counterpart of [`FilterColumnSpec`], adding only what scope decides:
/// the directory the extent goes in, and whether the bundle already holds a base there.
pub(crate) struct ScopedColumnSpec {
    /// Position in a buffered row's `scoped` list.
    pub(crate) index: usize,
    pub(crate) name: String,
    pub(crate) ty: ScalarType,
    pub(crate) category: bool,
    /// Declared `index = true`, or `render = true` on a family with a value column: the family is
    /// on the filter surface, so this flush owes it an extent.
    pub(crate) filterable: bool,
    /// Whether the family has an entity-space value column: every family but `text`. This, not
    /// [`Self::filterable`], decides whether this flush owes an extent for a family carrying
    /// neither flag.
    pub(crate) has_value_column: bool,
    /// Declared `render = true`: the family occupies a lane in this view's row tail, which counts
    /// as a column for `scoped_scalars[..].views` even with no filter surface.
    pub(crate) render: bool,
    /// The manifest already names this view in the family's `views`, so a base column is on disc.
    /// `false` for a view created since the build, whose base this flush writes empty.
    pub(crate) has_base: bool,
    /// The analyser a `text` family's terms are produced by — `Some` exactly for that family.
    pub(crate) analyser: Option<std::sync::Arc<mosaica_analyse::Analyser>>,
}

/// This flush's text layers: per indexed `text` column, its own dictionary over the terms this
/// batch produced, postings against that dictionary, and the entities it holds a value for. An
/// entity whose text analyses to no terms still carries a value, in the presence bitmap. `laps`
/// takes the `Text*` sub-laps ([`FlushStage::TEXT`]), run from a copy of `mark`.
fn write_text_extents(
    plan: &FlushPlan,
    ctx: &FlushContext,
    laps: &mut FlushLaps,
    mark: StageMark,
) -> Result<Vec<mosaica_store::manifest::TextExtent>, MaintenanceFailed> {
    if ctx.text_schema.is_empty() {
        return Ok(Vec::new());
    }
    let mut mark = mark;
    let mut out = Vec::with_capacity(ctx.text_schema.len());
    for spec in &ctx.text_schema {
        let mut rows = Vec::with_capacity(plan.items.len());
        for (entity, row) in plan.value_rows() {
            rows.push((
                narrow_entity(*entity)?,
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

/// Where a text layer's files go. What [`write_text_layer`] needs of a [`FlushContext`], so a
/// test can write a layer without building one.
struct TextTarget<'a> {
    prefix_dir: &'a Path,
    seg_id: &'a str,
    /// Stamped on a layer that names a view, which is the owning group's rather than the
    /// flushing one's; an entity-scoped layer names none and carries no incarnation.
    scoped_incarnation: mosaica_types::view::ViewIncarnation,
}

impl<'a> TextTarget<'a> {
    fn of(ctx: &'a FlushContext) -> Self {
        TextTarget {
            prefix_dir: &ctx.prefix_dir,
            seg_id: &ctx.seg_id,
            scoped_incarnation: ctx.scoped_incarnation,
        }
    }
}

/// One text layer: its own dictionary over this batch's terms, the postings against it, and the
/// entities that carried prose — written under `rel_dir` and named for the flush. `None` where no
/// row carried a value. `view` records the scope: entity-scoped or group-scoped. `sub` takes the
/// `Text*` sub-laps where the caller's stage is `TextExtents`, and is `None` from the scoped pass.
fn write_text_layer(
    rel_dir: &str,
    column: &str,
    view: Option<String>,
    analyser: &mosaica_analyse::Analyser,
    rows: Vec<(u32, &WalScalar)>,
    target: TextTarget<'_>,
    mut sub: Option<TextLaps<'_>>,
) -> Result<Option<mosaica_store::manifest::TextExtent>, MaintenanceFailed> {
    let dir = target.prefix_dir.join(rel_dir);
    std::fs::create_dir_all(&dir)
        .map_err(|e| MaintenanceFailed(format!("{}: {e}", dir.display())))?;
    text_lap(&mut sub, FlushStage::TextRows);

    let mut terms: std::collections::BTreeMap<String, Vec<u32>> = std::collections::BTreeMap::new();
    let mut presence = croaring::Bitmap::new();
    let mut prose_rows: Vec<(u32, &str)> = Vec::new();
    let mut scratch = mosaica_analyse::TokenScratch::default();
    for (entity, value) in rows {
        let prose = match value {
            WalScalar::Utf8(s) => s.as_str(),
            WalScalar::Null => continue,
            other => {
                return Err(MaintenanceFailed(format!(
                    "column '{column}' is text but a buffered row carries {other:?}"
                )))
            }
        };
        presence.add(entity);
        if view.is_some() {
            prose_rows.push((entity, prose));
        }
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
    mosaica_filter::write_sorted_dict(
        &target.prefix_dir.join(&dict_rel),
        terms.keys().map(String::as_str),
    )
    .map_err(|e| MaintenanceFailed(format!("{dict_rel}: {e}")))?;
    text_lap(&mut sub, FlushStage::TextDict);
    let per_term: Vec<Vec<u32>> = terms.into_values().collect();
    mosaica_authz::postings::write_postings(
        &target.prefix_dir.join(&postings_rel),
        &per_term,
        mosaica_types::SMALL_TERM_THRESHOLD_DEFAULT,
    )
    .map_err(|e| MaintenanceFailed(format!("{postings_rel}: {e}")))?;
    text_lap(&mut sub, FlushStage::TextPostings);
    std::fs::write(
        target.prefix_dir.join(&presence_rel),
        presence.serialize::<croaring::Portable>(),
    )
    .map_err(|e| MaintenanceFailed(format!("{presence_rel}: {e}")))?;
    text_lap(&mut sub, FlushStage::TextPresence);
    // A group-scoped column's prose, which its postings cannot give back.
    let prose = match view {
        None => None,
        Some(_) => {
            let prose = mosaica_store::manifest::RecordExtent {
                blocks: format!("{rel_dir}/{}-prose.blocks.bin", target.seg_id),
                hasrow: format!("{rel_dir}/{}-prose.hasrow.roaring", target.seg_id),
                directory: format!("{rel_dir}/{}-prose.directory.arrow", target.seg_id),
            };
            prose_rows.sort_by_key(|(entity, _)| *entity);
            mosaica_filter_write::write_prose(
                &target.prefix_dir.join(&prose.blocks),
                &target.prefix_dir.join(&prose.hasrow),
                &target.prefix_dir.join(&prose.directory),
                prose_rows,
            )
            .map_err(|e| MaintenanceFailed(format!("{}: {e}", prose.blocks)))?;
            Some(prose)
        }
    };

    Ok(Some(mosaica_store::manifest::TextExtent {
        column: column.to_string(),
        // `None` for the same rows `view` is: an entity-scoped column belongs to no view.
        incarnation: view.as_ref().map(|_| target.scoped_incarnation),
        view,
        dict: dict_rel,
        postings: postings_rel,
        presence: presence_rel,
        prose,
    }))
}

/// Where one view's column of a group-scoped family lives, prefix-relative:
/// `partitions/<p>/attrs/<column>/<group>/<key>/`, through the one place a view id and its
/// incarnation become a path, so a recreated key's base never lands on its predecessor's path.
fn scoped_column_rel(ctx: &FlushContext, column: &str) -> String {
    // `scoped_view` and its own incarnation, not `view`'s.
    mosaica_store::scoped_column_rel(
        &ctx.partition,
        column,
        &ctx.scoped_view,
        ctx.scoped_incarnation,
    )
}

/// Write this flush's extent for every group-scoped family of its view's group, and the empty
/// base a view created since the build has none of. A view created while the service runs has no
/// base, so the first flush of such a view writes the base as well as its extent, empty, and
/// publication puts the view on the family's list.
///
/// `plan.items` is used rather than `plan.entity_space_items()`: a scoped value belongs to the
/// `(entity, view)` pair this flush is giving a row, so a join row carries that view's value.
fn write_scoped_extents(
    plan: &FlushPlan,
    ctx: &FlushContext,
) -> Result<ScopedWrite, MaintenanceFailed> {
    let mut extents = Vec::new();
    let mut texts = Vec::new();
    let mut created = Vec::new();
    for spec in &ctx.scoped_schema {
        // A view left off `scoped_scalars[..].views` renders nothing and is opened for nothing.
        let owes_extent = spec.filterable || spec.has_value_column;
        if !spec.has_base && (owes_extent || spec.render) {
            created.push((spec.name.clone(), ctx.scoped_view.clone()));
        }
        // A family with a value column owes an extent whatever its flags. A text family has no
        // value column, so it stays gated on the filter licence.
        if !owes_extent {
            continue;
        }
        let column_rel = scoped_column_rel(ctx, &spec.name);
        let column_dir = ctx.prefix_dir.join(&column_rel);
        std::fs::create_dir_all(&column_dir)
            .map_err(|e| MaintenanceFailed(format!("scoped column dir '{column_rel}': {e}")))?;

        if !spec.has_base {
            write_empty_scoped_base(&column_dir, spec)?;
        }

        if let Some(analyser) = &spec.analyser {
            // Text: the same three files `write_text_extents` writes, in this view's directory.
            if let Some(extent) = write_text_layer(
                &format!("{column_rel}/{}", mosaica_filter::EXTENTS_DIR),
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
        extents.push(write_value_extent(
            ctx,
            &column_dir,
            &spec.name,
            Some(ctx.scoped_view.clone()),
            &column,
        )?);
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
    pub(crate) extents: Vec<crate::filter::OpenedExtent>,
    pub(crate) texts: Vec<mosaica_store::manifest::TextExtent>,
    pub(crate) created: Vec<(String, String)>,
}

/// The base a view of a group acquires at its first flush carrying values: every artefact the
/// family's declaration owes, holding nothing. Matches what `FilterColumns::open` will demand.
fn write_empty_scoped_base(
    column_dir: &std::path::Path,
    spec: &ScopedColumnSpec,
) -> Result<(), MaintenanceFailed> {
    let failed = |what: &str, e: &dyn std::fmt::Display| {
        MaintenanceFailed(format!("scoped base for '{}' ({what}): {e}", spec.name))
    };
    if spec.analyser.is_some() {
        // Text: a dictionary of no terms and postings over it, and no value column at all.
        mosaica_filter::write_sorted_dict(
            &column_dir.join(mosaica_filter::DICT_FILE),
            std::iter::empty::<&str>(),
        )
        .map_err(|e| failed("the token dictionary", &e))?;
        mosaica_authz::postings::write_postings(
            &column_dir.join("postings.arrow"),
            &[],
            mosaica_types::SMALL_TERM_THRESHOLD_DEFAULT,
        )
        .map_err(|e| failed("the token postings", &e))?;
        let prose = column_dir.join(mosaica_store::manifest::SCOPED_PROSE_DIR);
        std::fs::create_dir_all(&prose).map_err(|e| failed("the prose", &e))?;
        mosaica_filter_write::write_prose(
            &prose.join(mosaica_filter::RECORD_BLOCKS_FILE),
            &prose.join(mosaica_filter::RECORD_HASROW_FILE),
            &prose.join(mosaica_filter::RECORD_DIRECTORY_FILE),
            std::iter::empty(),
        )
        .map_err(|e| failed("the prose", &e))?;
        return Ok(());
    }
    let codes = empty_codes(spec.ty, spec.category);
    let values_path = column_dir.join(mosaica_filter::VALUES_FILE);
    let presence_path = column_dir.join(mosaica_filter::PRESENCE_FILE);
    // Presence is written, empty: no presence file means every entity is present.
    mosaica_filter::write_value_column(
        &values_path,
        &presence_path,
        &codes,
        Some(&croaring::Bitmap::new()),
    )
    .map_err(|e| failed("the values", &e))?;
    if spec.ty == ScalarType::Keyword {
        mosaica_filter::write_sorted_dict(
            &column_dir.join(mosaica_filter::DICT_FILE),
            std::iter::empty::<&str>(),
        )
        .map_err(|e| failed("the dictionary", &e))?;
    }
    if spec.category {
        let empty =
            mosaica_filter::open_extent(&values_path, &presence_path, mosaica_filter::Access::Read)
                .map_err(|e| failed("reopening the values", &e))?;
        mosaica_filter_write::write_category_postings(
            &column_dir.join("postings.arrow"),
            &spec.name,
            &empty,
            mosaica_filter_write::POSTINGS_BAND_ROWS,
        )
        .map_err(|e| failed("the postings", &e))?;
    }
    Ok(())
}

/// An empty `Codes` at a column's storage width, matching the width of the extents beside it.
fn empty_codes(ty: ScalarType, category: bool) -> mosaica_filter::Codes {
    use mosaica_filter::Codes;
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
        // Neither reaches here: text is handled above and `utf8` is not declarable.
        ScalarType::Keyword | ScalarType::Text | ScalarType::Utf8 => Codes::U32(Vec::new().into()),
    }
}

/// This flush's slice of `entities/terms/`: the term lists of the entities it minted, in the
/// promoted ordinals. Always written, even for a flush that minted nothing.
fn write_entity_terms_extent(
    per_entity: &[(u32, Vec<u32>)],
    ctx: &FlushContext,
) -> Result<mosaica_store::manifest::EntityTermsExtent, MaintenanceFailed> {
    let extents_rel = format!("partitions/{}/entities/terms/extents", ctx.partition);
    let extents_dir = ctx.prefix_dir.join(&extents_rel);
    std::fs::create_dir_all(&extents_dir)
        .map_err(|e| MaintenanceFailed(format!("entity-terms extent dir: {e}")))?;
    let extent = mosaica_store::manifest::EntityTermsExtent {
        hasrow: format!("{extents_rel}/{}.hasrow.roaring", ctx.seg_id),
        offsets: format!("{extents_rel}/{}.offsets.u32", ctx.seg_id),
        terms: format!("{extents_rel}/{}.terms.u32", ctx.seg_id),
        bases: format!("{extents_rel}/{}.bases.u64", ctx.seg_id),
    };
    let mut writer = mosaica_store::EntityTermsWriter::create_at(
        &ctx.prefix_dir.join(&extent.hasrow),
        &ctx.prefix_dir.join(&extent.offsets),
        &ctx.prefix_dir.join(&extent.terms),
        &ctx.prefix_dir.join(&extent.bases),
    )
    .map_err(|e| MaintenanceFailed(format!("entity-terms extent: {e}")))?;
    for (entity, terms) in per_entity {
        writer
            .push(*entity, terms)
            .map_err(|e| MaintenanceFailed(format!("entity-terms extent: {e}")))?;
    }
    writer
        .finish()
        .map_err(|e| MaintenanceFailed(format!("entity-terms extent: {e}")))?;
    Ok(extent)
}

/// Push one accumulated row, where there is an entity and it carries something. An entity with no
/// blob-resident value has no row and no has-row bit.
fn push_record_row(
    writer: &mut mosaica_filter_write::RecordBlobWriter,
    entity: Option<u32>,
    fields: &[mosaica_filter::RecordField],
) -> Result<(), MaintenanceFailed> {
    let Some(entity) = entity else {
        return Ok(());
    };
    if fields.is_empty() {
        return Ok(());
    }
    let borrowed: Vec<mosaica_filter::RecordFieldRef<'_>> = fields
        .iter()
        .map(|field| {
            field
                .value
                .as_ref()
                .map(|value| mosaica_filter::RecordFieldRef {
                    tag: field.tag,
                    value,
                })
                .ok_or_else(|| {
                    MaintenanceFailed(format!(
                        "record extent: entity {entity} carries a list at field tag {}; the \
                         multi surface has not landed (records §5)",
                        field.tag
                    ))
                })
        })
        .collect::<Result<_, _>>()?;
    writer
        .push_row(entity, &borrowed)
        .map_err(|e| MaintenanceFailed(format!("record extent: {e}")))
}

/// Write this flush's record-blob extent — the flushed entities' blob rows, has-row bitmap and
/// directory under `attrs/record/extents/` — or `None` where the schema declares no blob-resident
/// column. Written even if no flushed entity carries a blob value. A suppressed entity's row is
/// written, since the blob must hold what a later unsuppress reveals.
fn write_record_extent(
    plan: &FlushPlan,
    ctx: &FlushContext,
) -> Result<Option<RecordExtent>, MaintenanceFailed> {
    if ctx.record_schema.is_empty() {
        return Ok(None);
    }
    let extents_rel = format!("partitions/{}/attrs/record/extents", ctx.partition);
    let extents_dir = ctx.prefix_dir.join(&extents_rel);
    std::fs::create_dir_all(&extents_dir)
        .map_err(|e| MaintenanceFailed(format!("record extent dir: {e}")))?;
    let extent = RecordExtent {
        blocks: format!("{extents_rel}/{}.blocks.bin", ctx.seg_id),
        hasrow: format!("{extents_rel}/{}.hasrow.roaring", ctx.seg_id),
        directory: format!("{extents_rel}/{}.directory.arrow", ctx.seg_id),
    };
    let blocks_path = ctx.prefix_dir.join(&extent.blocks);
    let hasrow_path = ctx.prefix_dir.join(&extent.hasrow);
    let directory_path = ctx.prefix_dir.join(&extent.directory);
    let mut writer = mosaica_filter_write::RecordBlobWriter::create(
        &blocks_path,
        &hasrow_path,
        &directory_path,
        mosaica_filter::RECORD_BLOCK_TARGET,
    )
    .map_err(|e| MaintenanceFailed(format!("record extent: {e}")))?;

    // One row per entity, however many of the plan's rows carry its cells: fields accumulate while
    // the entity repeats and are pushed once.
    let mut fields: Vec<mosaica_filter::RecordField> = Vec::with_capacity(ctx.record_schema.len());
    let mut open: Option<u32> = None;
    for (entity, item) in plan.value_rows() {
        let entity = narrow_entity(*entity)?;
        if open != Some(entity) {
            push_record_row(&mut writer, open, &fields)?;
            fields.clear();
            open = Some(entity);
        }
        for spec in &ctx.record_schema {
            let value = buffered_value(item, BufferedPlace::Entity(spec.index));
            let Some(value) = record_value_of(value, spec)? else {
                continue;
            };
            let tag = u16::try_from(spec.index).map_err(|_| {
                MaintenanceFailed(format!(
                    "column '{}' is declared at position {}, past the u16 field-tag space",
                    spec.name, spec.index
                ))
            })?;
            fields.push(mosaica_filter::RecordField { tag, value });
        }
    }
    push_record_row(&mut writer, open, &fields)?;
    writer
        .finish()
        .map_err(|e| MaintenanceFailed(format!("record extent: {e}")))?;
    mosaica_filter::RecordBlob::open(
        &blocks_path,
        &hasrow_path,
        &directory_path,
        mosaica_filter::Access::Mapped,
    )
    .map_err(|e| MaintenanceFailed(format!("record extent does not reopen: {e}")))?;
    Ok(Some(extent))
}

/// One buffered value as the blob row carries it, or `None` where the entity carries nothing in
/// this column. The declared type is checked, not assumed: a mismatch fails the flush.
fn record_value_of(
    value: &WalScalar,
    spec: &RecordColumnSpec,
) -> Result<Option<mosaica_filter::RecordValue>, MaintenanceFailed> {
    use mosaica_filter::RecordValue;
    let wrong = || {
        MaintenanceFailed(format!(
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
        // A blob-resident keyword stores its bytes, not an ordinal, sharing the arm with `utf8`.
        ScalarType::Utf8 | ScalarType::Keyword | ScalarType::Text => match value {
            WalScalar::Utf8(text) => RecordValue::Utf8(text.clone()),
            WalScalar::Null => return Ok(None),
            _ => return Err(wrong()),
        },
    }))
}

/// One entity id as the postings, the extents and the blob address it. Every artefact this module
/// writes is keyed by a `u32`, so an id past that ceiling fails the flush.
fn narrow_entity(entity: EntityId) -> Result<u32, MaintenanceFailed> {
    u32::try_from(entity.raw()).map_err(|_| {
        MaintenanceFailed(format!(
            "entity {} does not fit the u32 entity space",
            entity.raw()
        ))
    })
}

/// This flush's segment directory. Both the segment writer and promotion address it; naming it
/// once keeps them from drifting apart.
fn segment_dir(ctx: &FlushContext) -> PathBuf {
    mosaica_store::view_path(
        &ctx.prefix_dir.join("partitions").join(&ctx.partition),
        &ctx.view,
    )
    .join("segments")
    .join(&ctx.seg_id)
}

/// One file's size and hex SHA-256, by reading it back: `mosaica_store::digest_of` with this
/// crate's error type. `pub(crate)`: every background pass digests its own outputs this way.
pub(crate) fn digest_of(path: &Path) -> Result<FileDigest, MaintenanceFailed> {
    mosaica_store::digest_of(path)
        .map_err(|e| MaintenanceFailed(format!("digest {}: {e}", path.display())))
}

/// Whether `entity` is deleted in this overlay. Only deletion excludes an item from a flush; a
/// suppressed item is written so that an unsuppress can reveal it.
fn is_deleted(overlay: &Overlay, entity: EntityId) -> bool {
    overlay.is_deleted(entity)
}

#[cfg(test)]
mod tests {
    use super::*;
    

    

    use mosaica_lifecycle::wal::{ChangeOp, WalRow, WalScalar};
    use mosaica_lifecycle::IngestBuffer;
    
    
    
    use mosaica_types::TermId;

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
            wal_pos: None,
        }
    }

    fn buffer_with(buffered: &[(u64, BufferedItem)]) -> IngestBuffer {
        let mut buffer = IngestBuffer::new();
        for (entity, item) in buffered {
            let row = WalRow {
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

    /// **A suppression never touches postings, and an item's is lifted only by an unsuppress**, so
    /// a flush that skipped it would leave a later unsuppress with nothing to reveal: no row would
    /// exist, and unsuppressing the item would show nothing at all.
    #[test]
    fn a_suppressed_entity_is_flushed_so_a_later_unsuppress_has_something_to_reveal() {
        let generation = generation_with(&[(7, item(&[1]))], &[(7, ChangeOp::Suppress)]);
        let plan = plan(&generation).expect("a suppressed item still flushes");
        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.items[0].0, EntityId::new(7));
    }

    /// A deleted entity acquires no row, and the deny entry stands.
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
        // The `Manifest*` sub-stages are on the executor, partition `Manifest`, and are in
        // `PUBLISH` no more than the wall is: counted there they would double `Manifest`.
        for stage in FlushStage::MANIFEST {
            assert!(FlushStage::EXECUTOR.contains(&stage));
            assert!(!FlushStage::PUBLISH.contains(&stage));
        }
        // The `Text*` sub-stages are on the pool, partition `TextExtents`, and are in `EXECUTE`
        // no more than the wall is: counted there they would double `TextExtents`.
        for stage in FlushStage::TEXT {
            assert!(FlushStage::POOL.contains(&stage));
            assert!(!FlushStage::EXECUTE.contains(&stage));
        }
        // Everything on the executor that is not the tick's two stages or the wall partitions
        // the wall; everything on the pool that is not the wall or a sub-stage partitions it.
        assert_eq!(
            FlushStage::PUBLISH.len() + FlushStage::MANIFEST.len() + 3,
            FlushStage::EXECUTOR.len()
        );
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
        let analyser = mosaica_analyse::Analyser::default();
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
                scoped_incarnation: mosaica_types::view::DECLARED_INCARNATION,
            },
            None,
        )
        .expect("the layer writes")
        .expect("rows carried prose");

        let dict = mosaica_filter::SortedDict::open(
            &dir.path().join(&extent.dict),
            mosaica_filter::Access::Mapped,
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

        let postings = mosaica_authz::postings::PostingsReader::open(
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
                mosaica_authz::postings::PostingRef::Array(bytes) => bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|b| u32::from_le_bytes(*b))
                    .collect(),
                mosaica_authz::postings::PostingRef::Roaring(view) => view.iter().collect(),
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
            mosaica_filter::Codes::U32(ordinals) => {
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
        let (values_path, presence_path, dict_path) = mosaica_filter::write_extent(
            dir.path(),
            "flush-1",
            &column.codes,
            &column.presence,
            column.dict_keys.as_deref(),
        )
        .expect("the extent writes");
        let dict_path = dict_path.expect("a keyword extent names a dictionary");

        let values = mosaica_filter::open_extent(
            &values_path,
            &presence_path,
            mosaica_filter::Access::Mapped,
        )
        .expect("the values reopen");
        let dict = mosaica_filter::SortedDict::open(&dict_path, mosaica_filter::Access::Mapped)
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
        let (_, _, dict_path) = mosaica_filter::write_extent(
            dir.path(),
            "flush-1",
            &column.codes,
            &column.presence,
            column.dict_keys.as_deref(),
        )
        .expect("the extent writes");
        let dict = mosaica_filter::SortedDict::open(
            &dict_path.expect("named even when empty"),
            mosaica_filter::Access::Mapped,
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
        let error = mosaica_filter::write_extent(
            dir.path(),
            "flush-1",
            &mosaica_filter::Codes::I64(vec![7i64].into()),
            &presence,
            Some(&["alpha"]),
        )
        .expect_err("the mismatch is refused");
        assert!(error.to_string().contains("not u32 ordinals"), "{error}");
    }
}
