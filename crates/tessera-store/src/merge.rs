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

use croaring::Bitmap;

use tessera_spatial::tiler::ScalarType;
use tessera_types::{IdentityKey, TesseraId, ROW_ABSENT};

use crate::error::{Result, StoreError};
use crate::flush::{digest_of, write_render_presence};
use crate::manifest::{FileDigest, SegmentDescriptor};
use crate::permutation::SegmentExtent;
use crate::render_presence::RENDER_PRESENCE_DIR;
use crate::segment_cursor::{gather_scalars, SegmentCursor};
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
    /// amplification of one merge.
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
    ///    merge.
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

/// Everything [`execute_merge`] needs beyond its inputs.
pub struct MergeSpec<'a> {
    /// The incarnation of the view this merge writes into (decision 0115) — the inputs' own, a
    /// merge never crossing a drop.
    pub incarnation: tessera_types::view::ViewIncarnation,
    /// The **new** segment's id. Never one of the inputs': `seg_id`s are never reused (contracts
    /// §2.1), which is what makes the publication rebase ABA-safe.
    pub seg_id: &'a str,
    pub inputs: &'a [MergeInput],
    pub identity_key: &'a IdentityKey,
    pub shard_id: u32,
    pub scalar_schema: &'a [(String, ScalarType)],
    /// The columns of `scalar_schema` an input may lawfully lack (`segment_cursor::gather_scalars`):
    /// the view's group-scoped render lanes and the columns declared at a running service since
    /// the inputs were written. Any other column an input lacks fails the operation.
    pub absent_ok: &'a [String],
    /// Where the merged extent begins in view row space — the **first consumed extent's**
    /// `row_base`. A merge emits exactly as many rows as it consumed, so no later extent's
    /// `row_base` moves and `RowSpace::collapsing` puts this where the consumed run was.
    pub row_base: u32,
}

/// What a merge produced: the merged segment's descriptor and extent, and its files.
///
/// It writes no external-id run and no locator extent. Those of the consumed segments stay listed,
/// and only the entity-space coalesce merges them.
#[derive(Debug)]
pub struct MergeOutput {
    pub segment: SegmentDescriptor,
    pub extent: SegmentExtent,
    /// Every file written, prefix-relative, for the manifest's `files` map.
    pub files: BTreeMap<String, FileDigest>,
}

/// Names this producer in any error the shared cursor or scalar adapter raises.
const OP: &str = "execute_merge";

/// Merge `spec.inputs` into one segment under `prefix_dir`.
///
/// Every input row is emitted once, in `(morton, tessera_id)` order from a heap over one cursor
/// per input, with its Morton code and residual copied unchanged; dropping a row is the fold's
/// work. The extent is held in memory at 4 bytes per entity in the merged span, which
/// `max_merged_segment_bytes` bounds.
pub fn execute_merge(
    prefix_dir: &Path,
    partition: &str,
    view: &str,
    spec: MergeSpec<'_>,
) -> Result<MergeOutput> {
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
        crate::view_path(&prefix_dir.join("partitions").join(partition), view)
            .join("segments")
            .join(seg_id)
    };

    let mut cursors: Vec<SegmentCursor> = spec
        .inputs
        .iter()
        .map(|input| SegmentCursor::open(&seg_path(&input.seg_id), input.seg_id.clone(), OP))
        .collect::<Result<_>>()?;

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

    // Where each input's rows land in the merged segment — `new_row_of[index][old_row]`, filled as
    // rows are emitted. Its cost is 4 B per merged row in total, the same order as `extent_rows`
    // above, which is what makes materialising it affordable *here* and not in the fold (whose
    // span is the whole corpus; see `fold_row_space`).
    let mut new_row_of: Vec<Vec<u32>> = cursors.iter().map(|c| vec![0u32; c.rows]).collect();

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
                     view's row space",
                    cursor.seg_id, spec.shard_id
                ),
            });
        }
        let scalars = gather_scalars(
            &cursor.columns,
            spec.scalar_schema,
            spec.absent_ok,
            row,
            &cursor.seg_id,
            OP,
        )?;
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
        new_row_of[index][row] = row_count as u32;
        row_count += 1;

        cursor.row += 1;
        if let Some((next_morton, next_id)) = cursor.key() {
            heap.push(Reverse((next_morton, next_id, index)));
        }
    }
    writer.finish().map_err(io)?;

    // ---- the render columns' presence, permuted (decision 0064) ------------------------------
    //
    // **A merge permutes row space within the merged span**, so an input's bitmap carried across
    // unchanged describes rows that have moved — and it fails *open*: wherever a present row's old
    // index lands on an absent row's new one, an item with no value starts matching a range
    // containing zero again. `RenderPresence::permuted` is the operation that answers that, driven
    // by the mapping the emission loop just recorded.
    //
    // Skipped entirely for a column no input has a file for, which is every category (its absence
    // is the reserved code 0, in the column) and every column with no absence anywhere.
    //
    // **A column an input's schema lacks is an absence in every row of that input**
    // (`ingest.md` §6.3): a segment written before the column was declared at a running service
    // carries no lane for it, and the merged segment takes the placeholder zero for those rows
    // with a presence bitmap that leaves them out. `presence()` answers all-present for a name it
    // has no file for, so the schema is asked first.
    let mut presence_written: Vec<&str> = Vec::new();
    for (name, _) in spec.scalar_schema {
        if !cursors.iter().any(|cursor| {
            cursor.columns.scalar(name).is_none()
                || cursor.columns.presence(name).bitmap().is_some()
        }) {
            continue;
        }
        let mut present = Bitmap::new();
        for (index, cursor) in cursors.iter().enumerate() {
            // An input without the column contributes no present row.
            if cursor.columns.scalar(name).is_none() {
                continue;
            }
            // Indexing `map` by an input row is in range because `ColumnsRef::load` refuses a
            // bitmap naming a row at or past its segment's row count, and every input row is
            // emitted — a merge drops none.
            let map = &new_row_of[index];
            let presence = cursor.columns.presence(name);
            match presence.bitmap() {
                // No file means every row of *this* input carries a value, and each contributes
                // its new row. Permuting cannot say that: an all-present bitmap permutes to
                // all-present, which adds nothing to a union and would leave every one of this
                // input's rows absent in the merged segment.
                None => present.add_many(map),
                Some(_) => {
                    if let Some(moved) = presence.permuted(|old| map[old as usize]).bitmap() {
                        present.or_inplace(moved);
                    }
                }
            }
        }
        if write_render_presence(&out_dir, name, present, row_count as u32)?.is_some() {
            presence_written.push(name);
        }
    }

    // The inputs' mappings go before the digests below, which read whole files.
    drop(cursors);

    let rel = |name: &str| {
        format!(
            "partitions/{partition}/{}/segments/{}/{name}",
            crate::view_rel(view),
            spec.seg_id
        )
    };
    let mut files = BTreeMap::new();
    for name in ["morton.u32", crate::read::CutIndex::FILE, "columns.arrow"] {
        files.insert(rel(name), digest_of(&out_dir.join(name))?);
    }
    for column in presence_written {
        let name = format!("{RENDER_PRESENCE_DIR}/{column}.roaring");
        files.insert(rel(&name), digest_of(&out_dir.join(&name))?);
    }

    Ok(MergeOutput {
        segment: SegmentDescriptor {
            view: view.to_string(),
            incarnation: spec.incarnation,
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
        files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tessera_spatial::tiler::ScalarValue;
    use tessera_types::EntityId;

    use crate::flush::{write_flush_segment, FlushInput, FlushRow};
    use crate::manifest::Quantisation;
    use crate::read::{ColumnsRef, ScalarSlice};

    const KEY_HEX: &str = "0123456789abcdef0123456789abcdef";

    fn schema() -> [(String, ScalarType); 1] {
        [("score".to_string(), ScalarType::I32)]
    }

    /// Write one input segment. Items are laid out along x alone, so Morton order is x order and a
    /// fixture can say exactly which rows the merge will interleave.
    fn segment(dir: &Path, seg_id: &str, rows: &[(u64, f64, ScalarValue)]) -> MergeInput {
        let key = IdentityKey::from_hex(KEY_HEX).expect("test key");
        let flush_rows: Vec<FlushRow> = rows
            .iter()
            .map(|(entity, x, score)| FlushRow {
                entity_id: EntityId::new(*entity),
                external_id: Some(format!("e-{entity}").into_bytes()),
                x: *x,
                y: 0.0,
                scalars: vec![score.clone()],
            })
            .collect();
        write_flush_segment(
            dir,
            "p",
            "s",
            FlushInput {
                incarnation: 0,
                seg_id,
                rows: flush_rows,
                quantisation: Quantisation {
                    x_min: 0.0,
                    x_max: 1.0,
                    y_min: 0.0,
                    y_max: 1.0,
                },
                identity_key: &key,
                shard_id: 0,
                scalar_schema: &schema(),
                row_base: 0,
            },
        )
        .expect("flush");
        MergeInput {
            seg_id: seg_id.to_string(),
            entity_lo: rows[0].0,
            entity_hi: rows[rows.len() - 1].0,
        }
    }

    /// **The merge trap, on a permutation that is not its own inverse.**
    ///
    /// The two inputs interleave in Morton space, so input A's rows 0..4 land at merged rows
    /// 0, 2, 4 and 6. A's row 1 carries no score, which the merged segment holds at row 2 — and A's
    /// *row 2* does carry one. A bitmap carried across the merge unchanged therefore says row 2 is
    /// present, which is the fail-open direction: an item with no value starts matching a range
    /// containing zero again, which is the defect decision 0064 exists to fix arriving by another
    /// route.
    ///
    /// Input B has no absence and so no file of its own, which is the other half of the operation:
    /// its rows must come out present, and they can only do so by being added — permuting an
    /// all-present bitmap yields all-present, which contributes nothing to the union.
    #[test]
    fn a_merge_permutes_each_input_bitmap_onto_the_rows_its_values_moved_to() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = segment(
            dir.path(),
            "seg-a",
            &[
                (0, 0.00, ScalarValue::I32(11)),
                (1, 0.20, ScalarValue::Null),
                (2, 0.40, ScalarValue::I32(13)),
                (3, 0.60, ScalarValue::I32(14)),
            ],
        );
        let b = segment(
            dir.path(),
            "seg-b",
            &[
                (4, 0.10, ScalarValue::I32(21)),
                (5, 0.30, ScalarValue::I32(22)),
                (6, 0.50, ScalarValue::I32(23)),
                (7, 0.70, ScalarValue::I32(24)),
            ],
        );
        let key = IdentityKey::from_hex(KEY_HEX).expect("test key");
        let out = execute_merge(
            dir.path(),
            "p",
            "s",
            MergeSpec {
                incarnation: 0,
                seg_id: "seg-m",
                inputs: &[a, b],
                identity_key: &key,
                shard_id: 0,
                scalar_schema: &schema(),
                absent_ok: &[],
                row_base: 0,
            },
        )
        .expect("merge");
        assert_eq!(out.segment.row_count, 8);

        let merged = dir.path().join("partitions/p/views/s/segments/seg-m");
        let columns = ColumnsRef::load(&merged.join("columns.arrow")).expect("columns");
        let ScalarSlice::I32(scores) = columns.scalar("score").expect("score column") else {
            panic!("score is declared i32");
        };
        assert_eq!(
            scores,
            [11, 21, 0, 22, 13, 23, 14, 24],
            "the fixture must interleave, or the permutation this test is about is the identity"
        );

        let presence = columns.presence("score");
        assert!(
            !presence.contains(2),
            "row 2 is the item with no score; carried across the merge unpermuted the bitmap \
             would call it present, and it would match a range containing zero"
        );
        assert!(
            (0..8)
                .filter(|&row| row != 2)
                .all(|row| presence.contains(row)),
            "every other row carries a value, input B's included — and B has no bitmap of its own"
        );
        assert!(out
            .files
            .contains_key("partitions/p/views/s/segments/seg-m/presence/score.roaring"));
    }

    /// A merge whose inputs have no absence between them writes no bitmap, exactly as a flush with
    /// none does: the file's absence is the representation of "every row", not a missing artefact.
    #[test]
    fn a_merge_of_columns_with_no_absence_writes_no_bitmap() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = segment(dir.path(), "seg-a", &[(0, 0.00, ScalarValue::I32(11))]);
        let b = segment(dir.path(), "seg-b", &[(1, 0.50, ScalarValue::I32(21))]);
        let key = IdentityKey::from_hex(KEY_HEX).expect("test key");
        let out = execute_merge(
            dir.path(),
            "p",
            "s",
            MergeSpec {
                incarnation: 0,
                seg_id: "seg-m",
                inputs: &[a, b],
                identity_key: &key,
                shard_id: 0,
                scalar_schema: &schema(),
                absent_ok: &[],
                row_base: 0,
            },
        )
        .expect("merge");

        let merged = dir.path().join("partitions/p/views/s/segments/seg-m");
        assert!(!merged.join(RENDER_PRESENCE_DIR).exists());
        assert!(!out
            .files
            .keys()
            .any(|rel| rel.contains(RENDER_PRESENCE_DIR)));
        let columns = ColumnsRef::load(&merged.join("columns.arrow")).expect("columns");
        assert!((0..2).all(|row| columns.presence("score").contains(row)));
    }
}
