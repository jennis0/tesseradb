//! The compaction fold's pass 1 — row space (compaction §3, "Pass 1 — row space").
//!
//! **What this is: a third producer for [`SegmentWriter`], not a second one.** Flush's
//! `write_segment` takes an already-sorted in-memory batch; [`crate::merge::execute_merge`]'s
//! k-way merge takes a *contiguous, non-overlapping* window of adjacent segments. This pass takes
//! **every live segment of one (partition, slice)** — the base plus every extent, named by the
//! fold's plan-time snapshot — and merges them the same way `execute_merge` does: a heap over one
//! `(morton, tessera_id)` key per input cursor, each cursor an index into a pair of mapped files.
//! Nothing here decodes a batch and nothing here re-sorts; the merged order falls out of the heap
//! because every input already carries it (contracts §2.6).
//!
//! **Two differences from `execute_merge`, and they are the whole of this module.**
//!
//! 1. **No adjacency precondition.** `execute_merge`'s inputs are a selected, contiguous run of
//!    entity space (`MergePolicy::select`'s job), so it can refuse anything else. A fold's inputs
//!    are the *whole* row space at the snapshot — base and every extent — and those interleave in
//!    **Morton** space by construction (that is what makes a fold worth doing at all: entity-space
//!    contiguity says nothing about spatial locality). Entity space itself stays partitioned across
//!    live segments the way it always has (row space is a bijection — one row per live entity), but
//!    this pass never checks or relies on that; it needs only that each input is internally
//!    `(morton, tessera_id)`-ascending, which `MortonSlice::load` already guarantees. Accordingly
//!    [`FoldSegmentInput`] carries no entity range, unlike [`crate::merge::MergeInput`] — requiring
//!    one here would reimport exactly the precondition this pass must not inherit.
//! 2. **Rows are dropped.** A row whose entity is in the caller's tombstone set is never appended
//!    and never scattered. `execute_merge` is explicitly row-count preserving (dropping a row is
//!    "invariant-bearing work this layer must not do", merge.rs) — this pass is the layer that may.
//!
//! ## The tombstone set is `D₀`, and this function does not derive it
//!
//! [`FoldRowSpaceSpec::tombstones`] is the fold plan's tombstone clone, `D₀` — a parameter, not a
//! quantity this pass computes. Compaction §5 draws the line precisely and it is worth restating
//! here because getting it backwards is the fail-open r3 found: **`executed`, the set that actually
//! retires from the overlay, is derived at *publication*, from what the carry-forward artefacts
//! demonstrably no longer name** — hours after this pass has already run and finished. A row-space
//! pass that tried to use `executed` instead would have to predict it at plan time, which is
//! indistinguishable from the bug compaction §5 replaced (a delete accepted mid-fold, against an
//! entity this pass's snapshot did not cover, must not be treated as executed just because the
//! plan's `D₀` already named it — the overlay entry for such an entity survives this fold and the
//! next one takes it). So: passes 1–3 execute over `D₀` — this function takes exactly that, as
//! `tombstones`, and has no way to reach for anything else.
//!
//! ## Byte-exact through the code, never through coordinates
//!
//! A segment stores a row's Morton code and its residual — never an axis pair (`write.rs`'s
//! `SegmentRow` doc). This pass reads both straight off the input's mapped bytes (the code as the
//! heap key, the residual via [`ColumnsRef::residual`]) and writes them straight through
//! [`SegmentWriter::append`]. Nothing here calls [`tessera_spatial::unsplit32`] or
//! [`tessera_spatial::split32`] — a dequantise-then-requantise round trip would move every
//! surviving point by up to a cell, silently, and no row count would show it (write-path §7).
//!
//! ## The scatter is why `permutation.bin` is written through a mapping, not a `Vec`
//!
//! `execute_merge` learns its extent in row order and can fill an in-memory `Vec` sized to its
//! (bounded, policy-capped) entity span. This pass has no such bound — its span is the *whole*
//! entity space — and it learns `perm[entity] = row` in `(morton, tessera_id)` order, which is not
//! entity order. [`PermutationWriter`] exists precisely for this: it scatters into a memory-mapped
//! `permutation.bin` in whatever order rows arrive, at the cost of `bound × 4` bytes of `0xFF`
//! written up front (dirty shared mapping, not anonymous — 4 GB at a 10⁹-entity bound, compaction
//! §3's memory rule) so that every entity this pass drops, or never had a row to begin with, reads
//! back as **absent** rather than aliasing row 0 (see [`FoldRowSpaceSpec::permutation_bound`]).
//!
//! ## Row ids are not stable
//!
//! A dropped row shifts the row id of every row after it — that is the entire reason compaction §6
//! exists. This pass never assumes otherwise: `permutation.bin`'s scatter uses a counter over rows
//! actually **emitted**, not an input row index, an entity's declared range, or anything computed
//! before the drop decision for that row is known.
//!
//! ## Peak memory does not scale with row count
//!
//! *k* cursors (one mapped `(morton.u32, columns.arrow)` pair per live segment) plus
//! [`SegmentWriter`]'s spool buffers, exactly as `execute_merge`. Nothing here accumulates a
//! corpus-sized `Vec` — the one structure that would have (an entity-span-sized extent, as
//! `execute_merge` builds) is what the mapped permutation writer replaces.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::path::{Path, PathBuf};

use croaring::Bitmap;

use tessera_spatial::tiler::ScalarType;
use tessera_types::{IdentityKey, TesseraId};

use crate::error::{Result, StoreError};
use crate::segment_cursor::{gather_scalars, SegmentCursor};
use crate::write::{PermutationWriter, SegmentRow, SegmentWriter};

/// One live segment pass 1 reads from — the base segment or one of its extents, wherever the
/// fold's plan-time snapshot found it.
///
/// **Carries no entity range**, unlike [`crate::merge::MergeInput`]. `execute_merge` needs one to
/// enforce "adjacent, ascending, non-overlapping" and to size its in-memory extent; this pass does
/// neither (see the module doc) — the only role `dir` plays is naming the two mapped files this
/// input's cursor opens.
pub struct FoldSegmentInput {
    /// For diagnostics only — every error this pass can raise about a bad input names it.
    pub seg_id: String,
    /// The directory holding this segment's `morton.u32` and `columns.arrow`. This is the *live*
    /// generation's directory, named by the fold's plan-time snapshot — not necessarily under the
    /// fold's own output prefix, since the fold reads the old generation and writes a new one.
    pub dir: PathBuf,
}

/// Everything [`fold_row_space`] needs beyond its inputs and where to write.
pub struct FoldRowSpaceSpec<'a> {
    /// Every live segment of one (partition, slice) at the fold's snapshot — the base plus every
    /// extent. Order does not matter: the merge is driven entirely by the heap over each cursor's
    /// `(morton, tessera_id)` key.
    pub inputs: &'a [FoldSegmentInput],
    pub identity_key: &'a IdentityKey,
    pub shard_id: u32,
    pub scalar_schema: &'a [(String, ScalarType)],
    /// `D₀` — the fold plan's tombstone clone (compaction §5), entity ids as a Roaring bitmap
    /// (matching `tessera_lifecycle::Overlay::deleted`'s representation). A row whose entity is a
    /// member is dropped: not appended to the output segment, not scattered into
    /// `permutation.bin`.
    ///
    /// **This is `D₀`, never `executed`, and the distinction is load-bearing — see the module
    /// doc's "The tombstone set is `D₀`" section.** This function has no way to compute `executed`
    /// (it is a publication-time quantity derived from what the fold's *other* passes carried
    /// forward) and must not be handed anything computed as though it were one.
    pub tombstones: &'a Bitmap,
    /// The bound for the output `permutation.bin` — entity ids in `[0, permutation_bound)`.
    /// [`PermutationWriter::create`] writes `permutation_bound × 4` bytes of the `0xFF` row-absent
    /// sentinel before this pass appends a single row (4 GB at a 10⁹-entity bound) — a dirty
    /// shared mapping, and therefore real resident memory under compaction §3's pre-flight budget,
    /// not the anonymous memory a `Vec` would cost. Callers size this against the live
    /// `entity_id_high_water` at the snapshot, not against the row count this pass will emit,
    /// which is smaller whenever any tombstone applies.
    pub permutation_bound: u64,
}

/// What [`fold_row_space`] produced.
pub struct FoldRowSpaceOutput {
    /// Rows actually emitted — every live row minus every tombstoned one. Never assume this
    /// equals any input's row count, or the sum of them minus `tombstones.cardinality()`: an
    /// entity named by `tombstones` that has no row anywhere in `inputs` costs nothing (see the
    /// module doc and `a_tombstone_naming_no_row_is_harmless` in the test suite).
    pub row_count: u32,
}

/// Run the fold's row-space pass: merge every entry of `spec.inputs` into one new segment under
/// `output_dir`, dropping tombstoned rows, and scatter the surviving entity→row mapping into a
/// fresh `permutation.bin` at `permutation_path`.
///
/// `output_dir` and `permutation_path` are the caller's to resolve — ordinarily paths under the
/// fold's new prefix (compaction §3's "five streaming passes into a new prefix"), created fresh by
/// this call ([`SegmentWriter::create`] and [`PermutationWriter::create`] each require their target
/// not already hold a stale file of the same name for a different generation). `output_dir` is
/// created if it does not exist; `permutation_path`'s parent must already exist.
///
/// See the module doc for the two ways this differs from [`crate::merge::execute_merge`], and for
/// why the tombstone parameter is not, and cannot be, `executed`.
/// Names this producer in any error the shared cursor or scalar adapter raises.
const OP: &str = "fold_row_space";

pub fn fold_row_space(
    output_dir: &Path,
    permutation_path: &Path,
    row_entity_path: &Path,
    spec: FoldRowSpaceSpec<'_>,
) -> Result<FoldRowSpaceOutput> {
    let mut cursors: Vec<SegmentCursor> = Vec::with_capacity(spec.inputs.len());
    for input in spec.inputs {
        cursors.push(SegmentCursor::open(&input.dir, input.seg_id.clone(), OP)?);
    }

    std::fs::create_dir_all(output_dir).map_err(|source| StoreError::Io {
        path: output_dir.to_path_buf(),
        source,
    })?;
    let columns_io = |source| StoreError::Io {
        path: output_dir.join("columns.arrow"),
        source,
    };
    let perm_io = |source| StoreError::Io {
        path: permutation_path.to_path_buf(),
        source,
    };

    // The row→entity table beside the permutation (`crate::row_entity`). Accumulated here rather
    // than derived afterwards because this loop *is* the row order: emitting a row and recording
    // its entity are the same event, so a second pass could only disagree with this one. A `u32`
    // per surviving row — the same 4 GB per 10⁹ rows the permutation costs, and the reason the
    // filtered viewport can walk a tile without a Feistel per row.
    let mut row_entity: Vec<u32> = Vec::new();

    let mut writer = SegmentWriter::create(output_dir, spec.scalar_schema).map_err(columns_io)?;
    let mut permutation =
        PermutationWriter::create(permutation_path, spec.permutation_bound).map_err(perm_io)?;

    // The merged order, from a heap over one key per live cursor — identical shape to
    // `execute_merge`'s, and for the same reason: `tessera_id` is a bijection and each live
    // entity has exactly one row across every live segment, so no two cursors can ever offer the
    // same `(morton, tessera_id)` pair, and the `index` tiebreak exists only to give `BinaryHeap`
    // a total order to work with.
    let mut heap: BinaryHeap<Reverse<(u32, u64, usize)>> = BinaryHeap::with_capacity(cursors.len());
    for (index, cursor) in cursors.iter().enumerate() {
        if let Some((morton, tessera_id)) = cursor.key() {
            heap.push(Reverse((morton, tessera_id, index)));
        }
    }

    // The row id a surviving entity gets — a count of rows *emitted*, never an input row index or
    // anything computed before this row's drop decision. This is what makes the shift a dropped
    // row causes to every later row automatic rather than something tracked.
    let mut row_count: u32 = 0;
    while let Some(Reverse((morton, tessera_raw, index))) = heap.pop() {
        let cursor = &mut cursors[index];
        let row = cursor.row;
        let tessera_id = TesseraId::new(tessera_raw);
        let (shard, entity) = spec.identity_key.invert(tessera_id);
        if shard != spec.shard_id {
            return Err(StoreError::MalformedBundle {
                detail: format!(
                    "fold_row_space: segment '{}' row {row} inverts to shard {shard}, not this \
                     bundle's {} — folding it would place another shard's entity in this slice's \
                     row space",
                    cursor.seg_id, spec.shard_id
                ),
            });
        }

        // Advance the cursor and refill the heap before the drop decision below, so an early
        // `continue` (the tombstoned case) can never skip it and stall this cursor.
        cursor.row += 1;
        if let Some((next_morton, next_id)) = cursor.key() {
            heap.push(Reverse((next_morton, next_id, index)));
        }

        // `entity.raw()` always fits `u32`: `IdentityKey::invert` builds it from the low half of
        // a Feistel round, itself a `u32` (identity.rs), so this is not a truncating cast — and
        // `croaring::Bitmap`'s domain is `u32` regardless.
        let entity_u32 = entity.raw() as u32;
        if spec.tombstones.contains(entity_u32) {
            continue;
        }

        let scalars = gather_scalars(&cursor.columns, spec.scalar_schema, row, &cursor.seg_id, OP)?;
        writer
            .append(SegmentRow {
                tessera_id,
                morton,
                residual: cursor.columns.residual()[row],
                scalars: &scalars,
            })
            .map_err(columns_io)?;
        permutation.set(entity, row_count).map_err(perm_io)?;
        row_entity.push(entity_u32);
        row_count = row_count
            .checked_add(1)
            .ok_or_else(|| StoreError::MalformedBundle {
                detail: "fold_row_space: row count exceeds u32::MAX".to_string(),
            })?;
    }

    let rows = writer.finish().map_err(columns_io)?;
    debug_assert_eq!(rows as u32, row_count);
    permutation.finish().map_err(perm_io)?;
    crate::row_entity::write_row_entity(row_entity_path, &row_entity).map_err(|source| {
        StoreError::Io {
            path: row_entity_path.to_path_buf(),
            source,
        }
    })?;
    // The inputs' mappings are dropped last, matching `execute_merge`: nothing after this point
    // reads them, so there is nothing to gain from dropping them earlier, and keeping the order
    // parallel is one less thing a reader has to reconcile between the two functions.
    drop(cursors);

    Ok(FoldRowSpaceOutput { row_count })
}
