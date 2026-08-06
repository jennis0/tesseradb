//! Merge selection: which segments a merge takes, and why adjacency is the first rule.
//!
//! Flush publishes one segment per tick. Without merge that is a serving cliff on two axes — a tile
//! resolves to one contiguous range **per live segment** (arch §11.3), and a fragment build unions
//! across **every live delta tier** — and a 90 s period produces roughly a thousand of each per day.
//!
//! **Selection is size-tiered over entity-adjacent segments, and adjacency is not an optimisation.**
//! A size-only policy is free to merge segments covering discontiguous entity sets; the result is a
//! row-space extent whose entity range interleaves with its neighbours', so the extent list
//! fragments monotonically and nothing but compaction can repair it. Requiring the inputs to be
//! adjacent keeps every merged extent a single contiguous entity range, which is the property
//! `RowSpace` is built on.
//!
//! **The cost of that rule, stated rather than hidden:** a large segment sitting between two small
//! ones blocks their merge. The alternative — merging across it — is what fragments the extent
//! list, so this is the trade taken deliberately, and [`MergePolicy::select`]'s doc says where it
//! shows up.
//!
//! **The base segment is excluded by the size bound, not by a rule.** A merge that swallowed the
//! base would be a legal row-space-only rewrite; the objection is that it pays compaction's entire
//! cost — a full permutation rewrite, up to 10⁹ rows of columns re-emitted — and banks none of
//! compaction's benefit. There is a sharper form: base files live in `MANIFEST.files`, so a merge
//! consuming the base must either leave them digested there with nothing referencing them, or write
//! a new prefix, at which point it *is* compaction under another name.
//!
//! **No deletes-percentage trigger.** Reclaiming a tombstoned row is a *fold*, and a fold is
//! invariant-bearing work that belongs to compaction: dropping the row changes what a viewer may
//! see, which is authorisation state this layer must not touch (`architecture.md` §11.3, which
//! states the rule after the borrowed policy that offered the trigger was retired at r33). A merge
//! that dropped rows would have left this module.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};
use std::fs;
use std::path::Path;

use tessera_spatial::tiler::ScalarType;
use tessera_types::{IdentityKey, TesseraId, ROW_ABSENT};

use crate::coalesce::{merge_runs, open_runs};
use crate::error::{Result, StoreError};
use crate::segment_cursor::{gather_scalars, SegmentCursor};
use crate::flush::{digest_of, FlushOutput};
use crate::manifest::{LocatorExtent, SegmentDescriptor};
use crate::permutation::SegmentExtent;
use crate::write::{SegmentRow, SegmentWriter};

/// What a merge is allowed to take.
///
/// The three knobs are independent and each answers a different question: how many segments make a
/// merge worth doing, when two segments count as the same size, and how large a single merge may
/// get.
#[derive(Debug, Clone, Copy)]
pub struct MergePolicy {
    /// How many same-tier, adjacent segments select a merge. Below this, nothing is merged.
    pub tier_width: usize,
    /// Sizes at or below this compare **equal**, so a tail of tiny segments forms one tier rather
    /// than a ladder of singletons that never reaches `tier_width`.
    ///
    /// Without it, a deployment whose flushes vary in size by a few bytes produces a size class per
    /// flush and merges nothing at all — the failure is silent, and looks like a policy that is
    /// simply never triggered.
    pub segment_floor_bytes: u64,
    /// The largest total a single merge may produce. Bounds the pool time and the write
    /// amplification of one merge, and is what keeps the base segment out of selection.
    pub max_merged_segment_bytes: u64,
}

impl MergePolicy {
    /// The seg_ids a merge should take, or `None` if nothing qualifies.
    ///
    /// `sizes` is parallel to `segments`, in bytes. Both are in the manifest's listed order, which
    /// for row space is also entity order.
    ///
    /// **Three conditions, all of them necessary:**
    ///
    /// 1. **Adjacent in the list, with strictly increasing, non-overlapping entity ranges.** The
    ///    merged extent must be one contiguous entity range or the extent list fragments (see this
    ///    module's doc). Note this is *not* `hi + 1 == lo`: a deleted entity acquires no row, so a
    ///    flush's range legitimately has gaps, and requiring exact contiguity would stop merging
    ///    entirely on a deployment that deletes.
    /// 2. **The same size tier**, by power-of-two class over `max(size, segment_floor_bytes)`.
    /// 3. **Total within `max_merged_segment_bytes`.** This is where a large neighbour blocks a
    ///    merge, and where the base segment excludes itself.
    ///
    /// The **first** qualifying window in list order is taken rather than the best one. Merging is
    /// idempotent work on a cadence — whatever this leaves, the next tick reconsiders — so a
    /// search for the best window would buy a marginally better choice at the cost of a policy
    /// nobody can predict from the manifest.
    pub fn select(&self, segments: &[SegmentDescriptor], sizes: &[u64]) -> Option<Vec<String>> {
        if self.tier_width < 2 || segments.len() < self.tier_width || sizes.len() != segments.len()
        {
            return None;
        }

        for start in 0..=segments.len() - self.tier_width {
            let window = &segments[start..start + self.tier_width];
            let window_sizes = &sizes[start..start + self.tier_width];

            let adjacent = window
                .windows(2)
                .all(|pair| pair[0].entity_hi < pair[1].entity_lo);
            if !adjacent {
                continue;
            }

            let tier = self.tier_of(window_sizes[0]);
            if !window_sizes.iter().all(|s| self.tier_of(*s) == tier) {
                continue;
            }

            let total: u64 = window_sizes.iter().copied().sum();
            if total > self.max_merged_segment_bytes {
                continue;
            }

            return Some(window.iter().map(|s| s.seg_id.clone()).collect());
        }
        None
    }

    /// `size`'s tier — see [`size_tier`].
    fn tier_of(&self, size: u64) -> u32 {
        size_tier(size, self.segment_floor_bytes)
    }
}

/// `size`'s tier: the power-of-two class of `max(size, floor)`.
///
/// Clamping to the floor **before** taking the class is what makes the floor mean "these compare
/// equal" rather than "these are skipped": two segments of 1 and 999 bytes against a 1,000-byte
/// floor are one tier, which is the tail-of-tiny-artefacts case the floor exists for.
///
/// **Size tiering is what bounds write amplification, on every axis it is applied to.** A policy
/// that simply took the oldest *w* entries would re-read the artefact it produced last time at
/// every round, so a byte would be rewritten once per round for ever; requiring one size class
/// makes a byte move only when its artefact has doubled, which is O(log n) rewrites over the
/// deployment's life. The entity-space coalesce shares this function for exactly that reason.
pub fn size_tier(size: u64, floor: u64) -> u32 {
    size.max(floor).max(1).ilog2()
}

/// One segment a merge consumes, in listed (entity) order.
pub struct MergeInput {
    pub seg_id: String,
    pub entity_lo: u64,
    pub entity_hi: u64,
}

/// Everything [`execute_merge`] needs beyond its inputs — the same shape [`crate::flush::FlushInput`]
/// has, because publication does not care which produced the segment.
pub struct MergeSpec<'a> {
    /// The **new** segment's id. Never one of the inputs': `seg_id`s are never reused (contracts
    /// §2.1), which is what makes the publication rebase ABA-safe.
    pub seg_id: &'a str,
    pub inputs: &'a [MergeInput],
    pub identity_key: &'a IdentityKey,
    pub shard_id: u32,
    pub scalar_schema: &'a [(String, ScalarType)],
    /// Where the merged extent begins in slice row space — the **first consumed extent's**
    /// `row_base`. A merge emits exactly as many rows as it consumed, so no later extent's
    /// `row_base` moves and `RowSpace::collapsing` puts this where the consumed run was.
    pub row_base: u32,
    /// The live partition watermark, passed through untouched.
    ///
    /// **A merge must not move it, and deriving one from the inputs regresses it.** The output
    /// shape is a flush's, where `entity_hi + 1` is the right answer because a flush's entities are
    /// the newest in the partition. A merge's are not: merging an *interior* run and then
    /// publishing `entity_hi + 1` would move the watermark **backwards** past entities that
    /// already have rows, and composition treats everything at or above it as buffer-resident —
    /// so those entities would be looked for in a buffer that no longer holds them. Carried rather
    /// than computed, so the publication path can treat a merge exactly as it treats a flush
    /// without either of them knowing which it has.
    pub watermark: u64,
    /// The live allocator high-water, passed through untouched, for the reason above.
    pub entity_id_high_water: u64,
}

/// Merge `spec.inputs` into one segment under `prefix_dir`.
///
/// **Row-count preserving, and that is the invariant this function exists to keep.** Dropping a row
/// — because its entity is tombstoned, because a predicate changed — is the compaction *fold*, and
/// a fold is invariant-bearing work this layer must not do. Every input row is re-emitted.
///
/// **Byte-exact through the code, never through coordinates.** A segment stores the Morton code and
/// its residual, not the axes, and both are carried through untouched — nothing here dequantises,
/// so nothing here can re-quantise a point onto a neighbouring cell.
///
/// **A k-way merge over the inputs' mapped bytes, feeding [`SegmentWriter`].** Every input is
/// already `(morton, tessera_id)` ascending (contracts §2.6) and mmapped uncompressed, so a cursor
/// into one costs two integers and the merged order falls out of a heap over *k* keys. No sort, no
/// decoded batch, and **no second writer**: this feeds [`SegmentWriter`], whose other producer is
/// `write_segment` (write-path §7).
///
/// *This replaced a concatenate-and-re-sort, which is why decision 0049 could not raise
/// `max_merged_segment_bytes`: the old path decoded every input into `TilerItem`s and doubled again
/// at the sort, for a **measured 4.4–4.9×** peak over the inputs' on-disk bytes
/// (`probes/2026-08-04-maintenance-memory/`), so the cap was a memory bound rather than a
/// write-amplification knob. The result is identical either way — a merge of runs each sorted by
/// `(morton, tessera_id)` is exactly `sort_batch`'s total order — which is what makes this a
/// substitution rather than a change of output.*
///
/// **The external-id runs stream too**, by the same k-way shape over inputs already sorted on the
/// key the output needs — [`merge_runs`], which is also compaction's pass 3 (compaction §3, §10).
/// What a merge still materialises is the **extent**: `ROW_ABSENT`-filled and 4 B per entity in
/// the merged span, which is bounded by `max_merged_segment_bytes` here and is the term the fold
/// must instead write through a mapping.
///
/// **Sorting is unconditional** (arch §11.3). Lucene reorders a merged segment for doc-id
/// locality, an optimisation it may skip under pressure, and the policy this one was drawn from
/// offered that as a decorator. Here the Morton order *is* the tile index — a segment that is not
/// internally sorted breaks `tile_ranges`' binary search outright — so it cannot be skipped and
/// there is nothing to make conditional on a document count.
/// Names this producer in any error the shared cursor or scalar adapter raises.
const OP: &str = "execute_merge";

pub fn execute_merge(
    prefix_dir: &Path,
    partition: &str,
    slice: &str,
    spec: MergeSpec<'_>,
) -> Result<FlushOutput> {
    if spec.inputs.is_empty() {
        return Err(StoreError::MalformedBundle {
            detail: "execute_merge: no inputs".to_string(),
        });
    }
    if !spec
        .inputs
        .windows(2)
        .all(|w| w[0].entity_hi < w[1].entity_lo)
    {
        return Err(StoreError::MalformedBundle {
            detail: "execute_merge: inputs must be in ascending, non-overlapping entity order — \
                     the merged extent is one contiguous span (see MergePolicy::select)"
                .to_string(),
        });
    }

    let seg_path = |seg_id: &str| {
        prefix_dir
            .join("partitions")
            .join(partition)
            .join("slices")
            .join(slice)
            .join("segments")
            .join(seg_id)
    };

    let mut cursors: Vec<SegmentCursor> = Vec::with_capacity(spec.inputs.len());
    let mut run_paths: Vec<std::path::PathBuf> = Vec::with_capacity(spec.inputs.len());
    for input in spec.inputs {
        let dir = seg_path(&input.seg_id);
        cursors.push(SegmentCursor::open(&dir, input.seg_id.clone(), OP)?);
        run_paths.push(dir.join("external-ids.arrow"));
    }
    // Opened here, with the segment cursors, so a missing or malformed run fails the merge before
    // a byte of output is written. The cursors themselves hold only mapped batches and a position.
    let run_cursors = open_runs(&run_paths)?;

    let entity_lo = spec.inputs[0].entity_lo;
    let entity_hi = spec.inputs[spec.inputs.len() - 1].entity_hi;
    let span =
        usize::try_from(entity_hi - entity_lo + 1).map_err(|_| StoreError::MalformedBundle {
            detail: format!("execute_merge: entity span {entity_lo}..={entity_hi} is too wide"),
        })?;

    let out_dir = seg_path(spec.seg_id);
    fs::create_dir_all(&out_dir).map_err(|source| StoreError::Io {
        path: out_dir.clone(),
        source,
    })?;

    let io = |source| StoreError::Io {
        path: out_dir.join("columns.arrow"),
        source,
    };
    let mut writer = SegmentWriter::create(&out_dir, spec.scalar_schema).map_err(io)?;
    let mut extent_rows = vec![ROW_ABSENT; span];

    // The merged order, from a heap over one key per live cursor. `Reverse` because
    // `BinaryHeap` is a max-heap and row order is ascending; the cursor index is the last
    // component so the ordering is total even though `(morton, tessera_id)` already is —
    // `tessera_id` is a bijection and each entity has one row, so no two cursors can offer the
    // same pair.
    let mut heap: BinaryHeap<Reverse<(u32, u64, usize)>> = BinaryHeap::with_capacity(cursors.len());
    for (index, cursor) in cursors.iter().enumerate() {
        if let Some((morton, tessera_id)) = cursor.key() {
            heap.push(Reverse((morton, tessera_id, index)));
        }
    }

    let mut row_count: usize = 0;
    while let Some(Reverse((morton, tessera_raw, index))) = heap.pop() {
        let cursor = &mut cursors[index];
        let row = cursor.row;
        let tessera_id = TesseraId::new(tessera_raw);
        let (shard, entity) = spec.identity_key.invert(tessera_id);
        if shard != spec.shard_id {
            return Err(StoreError::MalformedBundle {
                detail: format!(
                    "execute_merge: segment '{}' row {row} inverts to shard {shard}, not this \
                     bundle's {} — merging it would place another shard's entity in this \
                     slice's row space",
                    cursor.seg_id, spec.shard_id
                ),
            });
        }
        let scalars = gather_scalars(&cursor.columns, spec.scalar_schema, row, &cursor.seg_id, OP)?;
        writer
            .append(SegmentRow {
                tessera_id,
                morton,
                residual: cursor.columns.residual()[row],
                scalars: &scalars,
            })
            .map_err(io)?;
        // The extent is filled as rows are emitted, so it needs no companion permutation of the
        // entity axis: the merged row *is* the emission ordinal.
        extent_rows[(entity.raw() - entity_lo) as usize] = row_count as u32;
        row_count += 1;

        cursor.row += 1;
        if let Some((next_morton, next_id)) = cursor.key() {
            heap.push(Reverse((next_morton, next_id, index)));
        }
    }
    writer.finish().map_err(io)?;
    // The inputs' mappings go before the coalesce and the digests below, which read whole files:
    // holding *k* segment mappings across work that does not need them is the one place this
    // function could reintroduce a resident term it just removed.
    drop(cursors);

    // The runs and their reverse locator are entity-space work, shared verbatim with the
    // entity-space coalesce publication that does it *without* a segment — see
    // [`crate::coalesce`] for the key order and the keep-newest rule.
    merge_runs(&run_cursors, entity_lo, entity_hi, &out_dir)?;

    let rel = |name: &str| {
        format!(
            "partitions/{partition}/slices/{slice}/segments/{}/{name}",
            spec.seg_id
        )
    };
    let mut files = BTreeMap::new();
    for name in [
        "morton.u32",
        "columns.arrow",
        "external-ids.arrow",
        "ext-locator.u32",
    ] {
        files.insert(rel(name), digest_of(&out_dir.join(name))?);
    }

    Ok(FlushOutput {
        segment: SegmentDescriptor {
            slice: slice.to_string(),
            seg_id: spec.seg_id.to_string(),
            row_count: row_count as u32,
            entity_lo,
            entity_hi,
        },
        extent: SegmentExtent {
            entity_lo,
            entity_hi,
            seg_id: spec.seg_id.to_string(),
            row_base: spec.row_base,
            rows: extent_rows,
        },
        external_id_run: rel("external-ids.arrow"),
        locator_extent: LocatorExtent {
            path: rel("ext-locator.u32"),
            entity_lo,
            entity_hi,
            external_id_run: rel("external-ids.arrow"),
        },
        files,
        // **A merge moves neither watermark**, and both are therefore the caller's live values
        // rather than anything derived from the inputs — see `MergeSpec::watermark` for why
        // deriving `entity_hi + 1` here regresses it on any interior merge.
        watermark: spec.watermark,
        entity_id_high_water: spec.entity_id_high_water,
    })
}

