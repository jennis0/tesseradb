//! The **row-space** merge: selection on the executor, execution on the pool, publication by
//! rebase — the half of merge that permutes row ids, and the one decision 0044's D1 mechanism
//! gates.
//!
//! Its entity-space twin is [`crate::coalesce`], which publishes without moving a row. What is
//! left to this module is the axis that half cannot bound: **segments**. A tile resolves to one
//! contiguous range per live segment (arch §11.3), so a viewport pays a binary search and a
//! `range_cardinality` per segment per tile — ~tens of milliseconds at 1,000 segments, which a
//! 90 s tick reaches in a day of sustained ingest.
//!
//! ## What makes this the gated half
//!
//! A merge collapses an adjacent run of extents into one, so **a row id inside the merged span
//! names a different entity afterwards** (I11; `geometry-pinning.md` §4). Two consequences, and
//! neither is optional:
//!
//! - **No row-space artefact may key on the prefix.** `segments_version` is the only safe
//!   discriminator, and the row-projection cache keys on it (`crate::cache`'s fact 2).
//! - **Stale-serve is unsound across it.** Decision 0044's rung 2 serves a one-generation-stale
//!   projection, which is exact for a flush because a flush appends; across a merge the stale
//!   entry's bits inside the span are simply wrong. `RowProjection::extends_to` refuses, and the
//!   request falls to rung 3 — a **429 for the refresh's bounded duration**, which is the residual
//!   0044 permits and the reason a merge is published as its own swap (D3) rather than riding a
//!   flush's.
//!
//! What keeps that residual short is the refresh's second rung: an extents-only re-projection
//! (`RowProjection::rebase_extents`) rather than the *measured* 1 277 ms full rebuild.
//!
//! ## What a merge must not do
//!
//! **Drop a row.** Reclaiming a tombstoned row is the compaction *fold*, which is
//! invariant-bearing work this layer must not perform; `execute_merge` is row-count preserving and
//! says so at the function. **Consume the base segment.** Its files live in `MANIFEST.files`, so a
//! merge that took it would need a new prefix — compaction under another name — and the enforced
//! relation is that `max_merged_segment_bytes` sits strictly below the base segment's size.

use tessera_store::manifest::SegmentDescriptor;
use tessera_store::merge::{execute_merge, MergeInput, MergePolicy, MergeSpec};
use tessera_store::read::SegmentData;
use tessera_store::render_presence::RENDER_PRESENCE_DIR;
use tessera_types::IdentityKey;

use crate::Generation;

/// One merge's immutable plan: the segments it consumes, in listed (entity) order.
///
/// **Named by `seg_id`, never by index.** Ids are never reused (contracts §2.1), so a `seg_id`
/// still present in the live generation at publication is the same segment the merge consumed —
/// which is what makes the rebase ABA-safe against the flushes that published while it ran.
pub(crate) struct MergePlan {
    pub(crate) partition: String,
    pub(crate) view: String,
    pub(crate) inputs: Vec<MergeInput>,
    /// Where the merged extent begins in view row space — the first consumed extent's `row_base`.
    pub(crate) row_base: u32,
}

/// Select a merge over `generation`, or `None` if nothing qualifies.
///
/// Pure, so the selection is testable without an executor. Sizes come from the manifest's `files`
/// map rather than the filesystem: a merge's inputs are files this process wrote and digested, and
/// stat-ing them per tick would put IO on the executor thread for a decision it can make from
/// state it already holds.
pub(crate) fn plan_merge(generation: &Generation, policy: MergePolicy) -> Option<MergePlan> {
    let (partition, partition_data) = generation.bundle.partitions.iter().next()?;
    if partition_data.stepped_down() {
        return None;
    }
    for (view, view_data) in &partition_data.views {
        // **Only extents may be merged, never the base segment.** The base is the one segment with
        // no extent — `permutation.bin` addresses it — so restricting selection to the extent list
        // excludes it structurally rather than by the size bound alone.
        let extents = view_data.row_space.extents();
        if extents.len() < policy.tier_width {
            continue;
        }
        let descriptors: Vec<SegmentDescriptor> = extents
            .iter()
            .map(|extent| SegmentDescriptor {
                view: view.clone(),
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

/// One segment's on-disk bytes, from the manifest's own digests.
fn segment_bytes(
    manifest: &tessera_store::manifest::SegmentsManifest,
    partition: &str,
    view: &str,
    seg_id: &str,
) -> u64 {
    let dir = format!("partitions/{partition}/views/{view}/segments/{seg_id}/");
    manifest
        .files
        .iter()
        .filter(|(path, _)| path.starts_with(&dir))
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
    /// The live partition watermark and allocator high-water, **passed through untouched**. A
    /// merge moves neither: deriving `entity_hi + 1` from the inputs would move the watermark
    /// *backwards* on any interior merge, and composition treats everything at or above it as
    /// buffer-resident — so entities that already have rows would be looked for in a buffer that
    /// no longer holds them. See `MergeSpec::watermark`.
    ///
    /// **Plan-time snapshots, and publication never reads them back**: a flush publishing during
    /// this merge's flight advances the live values, so [`rebase_into`] keeps the cloned live
    /// manifest's own — these exist only because `execute_merge`'s output shape requires them.
    pub(crate) watermark: u64,
    pub(crate) entity_id_high_water: u64,
}

/// A merge whose files are durable, awaiting the manifest edit and the swap on the executor.
pub(crate) struct CompletedMerge {
    pub(crate) plan: MergePlan,
    pub(crate) prefix: String,
    pub(crate) output: tessera_store::FlushOutput,
    pub(crate) segment: SegmentData,
}

#[derive(Debug)]
pub(crate) struct MergeFailed(pub(crate) String);

impl std::fmt::Display for MergeFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Turn a plan into durable files. **Runs on the background pool, over immutable inputs.**
///
/// # Memory: the row-space half streams, the entity-space half does not
///
/// The **measured 4.4–4.9× peak over the inputs' on-disk bytes**
/// (`probes/2026-08-04-maintenance-memory/`) was taken against a merge that decoded every input
/// into `TilerItem`s at once and doubled again at the sort. That multiplier — not the policy — is
/// why decision 0049 ruled `max_merged_segment_bytes` may not be raised: a cap that bounds
/// selection-time *file* bytes is a memory bound only through it, and 256 MiB modelled to a
/// ~1.1–1.3 GB pool transient.
///
/// `execute_merge` now k-way merges its inputs' mapped bytes into a streaming segment writer
/// (`tessera_store::write::SegmentWriter`), so the columns no longer materialise at all. **The
/// figure above is therefore stale rather than wrong, and nothing here has re-measured it** —
/// which is why the cap is unchanged. What still materialises is the **external-id runs**:
/// `read_runs` collects every consumed run's `(key, entity)` pairs before the coalesce sorts them,
/// and that term now dominates a merge's peak. Streaming it is compaction's pass 3.
///
/// Re-measuring is the precondition for raising the cap, and decision 0049 already makes that a
/// separate ruling rather than a consequence of this one.
///
/// **What remains unmeasured on any version**: tier coalescence (postings rather than rows — the
/// probe's shape does not transfer) and the **sum** when a flush, a merge and a coalesce overlap on
/// this pool, which nothing bounds.
pub(crate) fn execute(plan: MergePlan, ctx: MergeContext) -> Result<CompletedMerge, MergeFailed> {
    let output = execute_merge(
        &ctx.prefix_dir,
        &plan.partition,
        &plan.view,
        MergeSpec {
            seg_id: &ctx.seg_id,
            inputs: &plan.inputs,
            identity_key: &ctx.identity_key,
            shard_id: ctx.shard_id,
            scalar_schema: &ctx.scalar_schema,
            row_base: plan.row_base,
            watermark: ctx.watermark,
            entity_id_high_water: ctx.entity_id_high_water,
        },
    )
    .map_err(|e| MergeFailed(format!("merge: {e}")))?;

    let seg_dir = ctx
        .prefix_dir
        .join("partitions")
        .join(&plan.partition)
        .join("views")
        .join(&plan.view)
        .join("segments")
        .join(&ctx.seg_id);
    let segment = SegmentData {
        seg_id: ctx.seg_id.clone(),
        row_count: output.segment.row_count,
        morton: tessera_store::read::MortonSlice::load(&seg_dir.join("morton.u32"))
            .map_err(|e| MergeFailed(format!("morton: {e}")))?,
        columns: tessera_store::read::ColumnsRef::load(&seg_dir.join("columns.arrow"))
            .map_err(|e| MergeFailed(format!("columns: {e}")))?,
    };

    Ok(CompletedMerge {
        plan,
        prefix: ctx.prefix,
        output,
        segment,
    })
}

/// Apply `completed` to `manifest` in place, or `false` if it no longer rebases.
///
/// **Three lists move and one deliberately does not.**
///
/// - `segments`: the consumed descriptors out, the merged one in at the first's position.
/// - `external_id_runs` and `locator_extents`: `execute_merge` coalesced the consumed segments'
///   runs into the merged segment's own, so the consumed entries go and the merged one takes the
///   **first's position** — recency is list position, and decision 0047's resolution reads it
///   newest-first. Required to be contiguous, for the same reason [`crate::coalesce`] requires it:
///   a merged run at a position it did not earn answers a stale binding.
/// - **`deltas` does not move, and that is the rule most easily got wrong.** A tier's postings are
///   `(term, entity)` pairs and carry no row, so a row-space merge has no business rewriting them
///   — and the consumed segments' entities still have rows, in the merged segment, so dropping a
///   tier would make every item it carries invisible to every session. Their files therefore stay
///   digested in `files` too; what leaves is only the four row-space files the merged segment
///   replaces. Bounding the tier axis is [`crate::coalesce`]'s job, on its own cadence.
pub(crate) fn rebase_into(
    manifest: &mut tessera_store::manifest::SegmentsManifest,
    completed: &CompletedMerge,
) -> bool {
    let plan = &completed.plan;
    let consumed: Vec<&str> = plan.inputs.iter().map(|i| i.seg_id.as_str()).collect();

    let seg_at = |seg_id: &str| {
        manifest
            .segments
            .iter()
            .position(|s| s.seg_id == seg_id && s.view == plan.view)
    };
    let Some(first_segment) = seg_at(consumed[0]) else {
        return false;
    };
    if consumed.iter().any(|id| seg_at(id).is_none()) {
        return false;
    }

    let run_paths: Vec<String> = consumed
        .iter()
        .map(|seg_id| run_path(&plan.partition, &plan.view, seg_id))
        .collect();
    let Some(runs) = contiguous(&manifest.external_id_runs, &run_paths, |rel| rel) else {
        return false;
    };
    let locator_paths: Vec<String> = consumed
        .iter()
        .map(|seg_id| locator_path(&plan.partition, &plan.view, seg_id))
        .collect();
    let Some(locators) = contiguous(&manifest.locator_extents, &locator_paths, |e| &e.path) else {
        return false;
    };

    manifest
        .segments
        .retain(|s| !(s.view == plan.view && consumed.contains(&s.seg_id.as_str())));
    manifest
        .segments
        .insert(first_segment, completed.output.segment.clone());

    manifest
        .external_id_runs
        .splice(runs, [completed.output.external_id_run.clone()]);
    manifest
        .locator_extents
        .splice(locators, [completed.output.locator_extent.clone()]);

    // The four row-space files the merged segment replaces, and any presence bitmaps beside them
    // (decision 0064) — the merged segment carries its own, permuted. `delta.arrow` is **not**
    // among them — see this function's doc.
    //
    // The bitmaps go by prefix rather than by name because which columns have one is a property of
    // the consumed segments' *contents*, not of the schema: a column with no absence in a given
    // segment has no file there. A name left behind here is a manifest naming a file the reclaim
    // has removed, which refuses at the next open.
    for seg_id in &consumed {
        let seg_rel = format!(
            "partitions/{}/views/{}/segments/{seg_id}",
            plan.partition, plan.view
        );
        for name in [
            "morton.u32",
            "columns.arrow",
            "external-ids.arrow",
            "ext-locator.u32",
        ] {
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
    // `watermark` and `entity_id_high_water` keep the values `manifest` — a clone of the *live*
    // partition manifest, taken at publication — already carries. Those are the live values, and
    // the live values are what a merge publishes: it moves no entity into or out of the visible
    // set, so it has nothing to say about either (write-path §7). The completed unit's own copies
    // are plan-time snapshots, one flush stale whenever a flush published during the merge's
    // flight; a manifest stamped from them regresses on disc while the generation keeps the live
    // value, which `check_manifest_publishable` now refuses at the commit rather than trusting
    // every rebase to remember.
    true
}

fn run_path(partition: &str, view: &str, seg_id: &str) -> String {
    format!("partitions/{partition}/views/{view}/segments/{seg_id}/external-ids.arrow")
}

fn locator_path(partition: &str, view: &str, seg_id: &str) -> String {
    format!("partitions/{partition}/views/{view}/segments/{seg_id}/ext-locator.u32")
}

/// Where `needle` sits in `haystack` as a contiguous run of equal keys, in order.
fn contiguous<'a, T, K: PartialEq + 'a>(
    haystack: &'a [T],
    needle: &[K],
    key: impl Fn(&'a T) -> &'a K,
) -> Option<std::ops::Range<usize>> {
    if needle.is_empty() || haystack.len() < needle.len() {
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
