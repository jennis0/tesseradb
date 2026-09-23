//! The row-space merge. It collapses an adjacent run of one view's flushed segments into one
//! segment, so a viewport resolves each tile against fewer segments. The executor selects the
//! run, the background pool writes the merged segment, and the executor publishes it by
//! rebasing the live manifest.
//!
//! A row id inside the merged span names a different entity afterwards. Anything cached in row
//! space must therefore be keyed on `segments_version`. A merge keeps every row; removing
//! deleted rows is the fold's work. It takes extents only and leaves the base segment alone.
//!
//! A merge touches row space only. The consumed segments' external-id runs and locator extents
//! stay listed, and only the entity-space coalesce merges them, so the two passes never want the
//! same manifest entries.

use tessera_store::manifest::SegmentDescriptor;
use tessera_store::merge::{execute_merge, MergeInput, MergeOutput, MergePolicy, MergeSpec};
use tessera_store::read::SegmentData;
use tessera_store::render_presence::RENDER_PRESENCE_DIR;
use tessera_types::IdentityKey;

use crate::flush::MaintenanceFailed;
use crate::Generation;

/// One merge's immutable plan: the segments it consumes, in listed (entity) order.
///
/// Segments are named by `seg_id`, never by index. Ids are never reused, so a `seg_id` still
/// present in the live generation at publication is the same segment the merge consumed, even
/// if a flush published while the merge ran.
pub(crate) struct MergePlan {
    pub(crate) partition: String,
    pub(crate) view: String,
    /// The incarnation of `view` the inputs carry and the output takes. A merge never crosses a
    /// drop: its inputs are the live row space's own extents.
    pub(crate) incarnation: tessera_types::view::ViewIncarnation,
    pub(crate) inputs: Vec<MergeInput>,
    /// Where the merged extent begins in view row space: the first consumed extent's `row_base`.
    pub(crate) row_base: u32,
}

/// Select a merge over `generation`, or `None` if nothing qualifies.
///
/// Sizes come from the manifest's `files` map rather than stat-ing the filesystem, so selection
/// stays IO-free.
pub(crate) fn plan_merge(generation: &Generation, policy: MergePolicy) -> Option<MergePlan> {
    let (partition, partition_data) = generation.bundle.partitions.iter().next()?;
    if partition_data.stepped_down() {
        return None;
    }
    for (view, view_data) in &partition_data.views {
        // Use the view's live incarnation, or skip it. A mismatch between the bundle and its
        // manifest is a state the composition already refuses to serve.
        let incarnation = view_data.incarnation;
        if !generation
            .bundle
            .manifest
            .is_live_incarnation(view, incarnation)
        {
            continue;
        }
        // Only extents may be merged. The base segment has no extent; `permutation.bin`
        // addresses it, and it is excluded by only ever selecting from the extent list.
        let extents = view_data.row_space.extents();
        if extents.len() < policy.tier_width {
            continue;
        }
        let descriptors: Vec<SegmentDescriptor> = extents
            .iter()
            .map(|extent| SegmentDescriptor {
                view: view.clone(),
                incarnation,
                seg_id: extent.seg_id.clone(),
                row_count: extent.row_count(),
                entity_lo: extent.entity_lo,
                entity_hi: extent.entity_hi,
            })
            .collect();
        let sizes: Vec<u64> = descriptors
            .iter()
            .map(|d| segment_bytes(&partition_data.manifest, partition, view, &d.seg_id))
            .collect();
        let Some(chosen) = policy.select(&descriptors, &sizes) else {
            continue;
        };
        let first = extents
            .iter()
            .find(|extent| extent.seg_id == chosen[0])
            .expect("select returns seg_ids from the list it was given");
        return Some(MergePlan {
            partition: partition.clone(),
            view: view.clone(),
            incarnation,
            inputs: descriptors
                .iter()
                .filter(|d| chosen.contains(&d.seg_id))
                .map(|d| MergeInput {
                    seg_id: d.seg_id.clone(),
                    entity_lo: d.entity_lo,
                    entity_hi: d.entity_hi,
                })
                .collect(),
            row_base: first.row_base,
        });
    }
    None
}

/// One segment's row-space bytes, from the manifest's own digests. A flush segment's run and
/// locator sit in its directory until a coalesce takes them, and are not counted, so a segment's
/// size does not depend on when the coalesce ran.
fn segment_bytes(
    manifest: &tessera_store::manifest::SegmentsManifest,
    partition: &str,
    view: &str,
    seg_id: &str,
) -> u64 {
    let dir = format!(
        "partitions/{partition}/{}/segments/{seg_id}/",
        tessera_store::view_rel(view)
    );
    manifest
        .files
        .iter()
        .filter(|(path, _)| {
            path.starts_with(&dir)
                && !path.ends_with("/external-ids.arrow")
                && !path.ends_with("/ext-locator.u32")
        })
        .map(|(_, digest)| digest.size)
        .sum()
}

/// Everything [`execute`] needs beyond its plan — taken from the generation on the executor thread
/// and then immutable, exactly as a flush's context is.
pub(crate) struct MergeContext {
    pub(crate) prefix_dir: std::path::PathBuf,
    pub(crate) prefix: String,
    pub(crate) seg_id: String,
    pub(crate) identity_key: IdentityKey,
    pub(crate) shard_id: u32,
    pub(crate) scalar_schema: Vec<(String, tessera_spatial::tiler::ScalarType)>,
    /// The columns an input segment may lawfully lack: the view's group-scoped render lanes and
    /// the entity-scoped columns declared at a running service and not yet folded. Any other
    /// missing column is a torn segment and fails the merge.
    pub(crate) absent_ok: Vec<String>,
}

/// A merge whose files are durable, awaiting the manifest edit and the swap on the executor.
pub(crate) struct CompletedMerge {
    pub(crate) plan: MergePlan,
    pub(crate) prefix: String,
    pub(crate) output: MergeOutput,
    pub(crate) segment: SegmentData,
}

/// Turn a plan into durable files. Runs on the background pool, over immutable inputs.
pub(crate) fn execute(
    plan: MergePlan,
    ctx: MergeContext,
) -> Result<CompletedMerge, MaintenanceFailed> {
    let output = execute_merge(
        &ctx.prefix_dir,
        &plan.partition,
        &plan.view,
        MergeSpec {
            incarnation: plan.incarnation,
            seg_id: &ctx.seg_id,
            inputs: &plan.inputs,
            identity_key: &ctx.identity_key,
            shard_id: ctx.shard_id,
            scalar_schema: &ctx.scalar_schema,
            absent_ok: &ctx.absent_ok,
            row_base: plan.row_base,
        },
    )
    .map_err(|e| MaintenanceFailed(format!("merge: {e}")))?;

    let seg_dir = tessera_store::view_path(
        &ctx.prefix_dir.join("partitions").join(&plan.partition),
        &plan.view,
    )
    .join("segments")
    .join(&ctx.seg_id);
    let segment = SegmentData::load(&seg_dir, &ctx.seg_id, output.segment.row_count)
        .map_err(|e| MaintenanceFailed(e.to_string()))?;

    Ok(CompletedMerge {
        plan,
        prefix: ctx.prefix,
        output,
        segment,
    })
}

/// Apply `completed` to `manifest` in place, or `false` if a consumed segment is no longer listed.
///
/// The consumed descriptors leave `segments` and the merged one takes the first's position, and
/// their row-space files leave `files`. Every other list is unchanged: delta tiers, external-id
/// runs and locator extents address entities, and the consumed segments' entities still have rows
/// in the merged segment. The runs and locators stay in `files` with their entries.
pub(crate) fn rebase_into(
    manifest: &mut tessera_store::manifest::SegmentsManifest,
    completed: &CompletedMerge,
) -> bool {
    let plan = &completed.plan;
    let consumed: Vec<&str> = plan.inputs.iter().map(|i| i.seg_id.as_str()).collect();
    let listed = |seg_id: &str| {
        manifest
            .segments
            .iter()
            .position(|s| s.seg_id == seg_id && s.view == plan.view)
    };
    let Some(first_segment) = listed(consumed[0]) else {
        return false;
    };
    if consumed.iter().any(|id| listed(id).is_none()) {
        return false;
    }

    manifest
        .segments
        .retain(|s| !(s.view == plan.view && consumed.contains(&s.seg_id.as_str())));
    manifest
        .segments
        .insert(first_segment, completed.output.segment.clone());

    // Presence bitmaps are removed by prefix because a column with no absence in a given segment
    // has no bitmap file there; the merged segment carries its own, permuted.
    for seg_id in &consumed {
        let seg_rel = format!(
            "partitions/{}/{}/segments/{seg_id}",
            plan.partition,
            tessera_store::view_rel(&plan.view)
        );
        for name in ["morton.u32", tessera_store::read::CutIndex::FILE, "columns.arrow"] {
            manifest.files.remove(&format!("{seg_rel}/{name}"));
        }
        let presence_prefix = format!("{seg_rel}/{RENDER_PRESENCE_DIR}/");
        manifest
            .files
            .retain(|rel, _| !rel.starts_with(&presence_prefix));
    }
    manifest.files.extend(
        completed
            .output
            .files
            .iter()
            .map(|(rel, digest)| (rel.clone(), digest.clone())),
    );
    true
}
