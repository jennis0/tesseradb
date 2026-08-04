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

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use tessera_spatial::tiler::{ScalarType, ScalarValue, TilerItem};
use tessera_spatial::{sort_batch, unsplit32};
use tessera_types::{EntityId, IdentityKey, MortonCode, TesseraId, ROW_ABSENT};

use crate::coalesce::{read_runs, write_coalesced_run};
use crate::error::{Result, StoreError};
use crate::flush::{digest_of, FlushOutput};
use crate::manifest::{LocatorExtent, SegmentDescriptor};
use crate::permutation::SegmentExtent;
use crate::read::{ColumnsRef, MortonSlice, ScalarSlice};
use crate::write::write_segment;

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
/// its residual, not the axes; [`tessera_spatial::unsplit32`] recovers the axes as a bit
/// permutation, so the merged segment's codes are identical to its inputs'. Dequantising to floats
/// and re-quantising would move every point by up to a quantisation step on every merge, silently.
///
/// **It re-sorts rather than k-way merging, deliberately.** The plan specified a linear merge-sort
/// over the inputs' code arrays; this concatenates and re-sorts through [`sort_batch`] instead, so
/// that the *one* writer that knows a segment's layout — [`crate::write::write_segment`] — is the
/// one that writes this too. A second writer is how the two come to disagree about a format, and
/// the merge policy's `max_merged_segment_bytes` is what keeps the sort's inputs in hand. The
/// result is identical either way: `sort_batch` orders by code then `tessera_id`, which is total.
///
/// **Sorting is unconditional** (arch §11.3). Lucene reorders a merged segment for doc-id
/// locality, an optimisation it may skip under pressure, and the policy this one was drawn from
/// offered that as a decorator. Here the Morton sort *is* the tile index — a segment that is not
/// internally sorted breaks `tile_ranges`' binary search outright — so sorting cannot be skipped
/// and there is nothing to make conditional on a document count.
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

    let mut items: Vec<TilerItem> = Vec::new();
    let mut entity_ids: Vec<EntityId> = Vec::new();
    let mut forward: Vec<(Vec<u8>, u32)> = Vec::new();

    for input in spec.inputs {
        let dir = seg_path(&input.seg_id);
        let morton = MortonSlice::load(&dir.join("morton.u32"))?;
        let columns = ColumnsRef::load(&dir.join("columns.arrow"))?;
        let codes = morton.u32();
        let tessera_ids = columns.tessera_id();
        let residuals = columns.residual();
        if codes.len() != tessera_ids.len() || codes.len() != residuals.len() {
            return Err(StoreError::MalformedBundle {
                detail: format!(
                    "execute_merge: segment '{}' has {} codes against {} identities",
                    input.seg_id,
                    codes.len(),
                    tessera_ids.len()
                ),
            });
        }

        for row in 0..codes.len() {
            let tessera_id = TesseraId::new(tessera_ids[row]);
            let (shard, entity) = spec.identity_key.invert(tessera_id);
            if shard != spec.shard_id {
                return Err(StoreError::MalformedBundle {
                    detail: format!(
                        "execute_merge: segment '{}' row {row} inverts to shard {shard}, not this \
                         bundle's {} — merging it would place another shard's entity in this \
                         slice's row space",
                        input.seg_id, spec.shard_id
                    ),
                });
            }
            let (qx, qy) = unsplit32(MortonCode::new(codes[row]), residuals[row]);
            items.push(TilerItem {
                tessera_id,
                qx,
                qy,
                scalars: gather_scalars(&columns, spec.scalar_schema, row, &input.seg_id)?,
            });
            entity_ids.push(entity);
        }

        forward.extend(read_runs(&[dir.join("external-ids.arrow")])?);
    }

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

    let row_count = items.len();
    let codes = sort_batch(&mut items, &mut entity_ids);
    write_segment(&out_dir, &items, &codes, spec.scalar_schema).map_err(|source| {
        StoreError::Io {
            path: out_dir.join("columns.arrow"),
            source,
        }
    })?;

    let mut extent_rows = vec![ROW_ABSENT; span];
    for (row, entity) in entity_ids.iter().enumerate() {
        extent_rows[(entity.raw() - entity_lo) as usize] = row as u32;
    }

    // The runs and their reverse locator are entity-space work, shared verbatim with the
    // entity-space coalesce publication that does it *without* a segment — see
    // [`crate::coalesce`] for the key order and the keep-newest rule.
    write_coalesced_run(forward, entity_lo, entity_hi, &out_dir)?;

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

/// This row's declared scalars, in schema order — the shape [`TilerItem`] wants.
///
/// **A column the input lacks, or holds under another type, fails the merge**, and the alternative
/// is why: a `filter_map` here drops the missing one and shifts every later scalar up a position,
/// so the merged segment's columns are silently transposed — every value present, every value
/// against the wrong name, no error anywhere. Unreachable through [`crate::write::write_segment`],
/// which emits the declared schema in full; reachable the moment a merge takes an input this
/// process did not write, which is what a stepped-down or hand-repaired bundle is.
fn gather_scalars(
    columns: &ColumnsRef,
    schema: &[(String, ScalarType)],
    row: usize,
    seg_id: &str,
) -> Result<Vec<ScalarValue>> {
    let mismatch = |declared: ScalarType, found: &str| StoreError::MalformedBundle {
        detail: format!(
            "execute_merge: segment '{seg_id}' holds scalar column of type {found} where the \
             bundle declares {declared:?}; merging it would write the value under another \
             column's name"
        ),
    };
    schema
        .iter()
        .map(|(name, declared)| {
            let slice = columns
                .scalar(name)
                .ok_or_else(|| StoreError::MalformedBundle {
                    detail: format!(
                        "execute_merge: segment '{seg_id}' has no scalar column '{name}', which \
                         this bundle declares; dropping it would shift every later scalar into \
                         the wrong column"
                    ),
                })?;
            match (slice, declared) {
                (ScalarSlice::U64(v), ScalarType::U64) => Ok(ScalarValue::U64(v[row])),
                (ScalarSlice::F32(v), ScalarType::F32) => Ok(ScalarValue::F32(v[row])),
                (ScalarSlice::Utf8(v), ScalarType::Utf8) => {
                    Ok(ScalarValue::Utf8(v.value(row).to_string()))
                }
                (ScalarSlice::U64(_), declared) => Err(mismatch(*declared, "u64")),
                (ScalarSlice::F32(_), declared) => Err(mismatch(*declared, "f32")),
                (ScalarSlice::Utf8(_), declared) => Err(mismatch(*declared, "utf8")),
            }
        })
        .collect()
}
