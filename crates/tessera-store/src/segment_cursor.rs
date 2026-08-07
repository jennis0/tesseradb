//! One position in an already-sorted segment, and the row-to-`SegmentRow` scalar adapter — the
//! input side of every k-way merge that feeds [`crate::write::SegmentWriter`].
//!
//! **Shared because the alternative is two copies of one rule about the scalar tail.** There are
//! two producers that merge mapped segments — `merge.rs`'s `execute_merge` over a policy-selected
//! window, and `fold.rs`'s `fold_row_space` over every live segment — and they arrived with a
//! cursor and a `gather_scalars` that were byte-identical bar a diagnostic string. That is the
//! shape `write.rs`'s module doc names as how two come to disagree: the writer is shared, so the
//! *bytes* cannot drift, but a check added to one copy of `gather_scalars` and not the other would
//! let a segment through one path that the other refuses. One definition, two callers, and the
//! operation's name is a parameter rather than a reason to fork.

use std::path::Path;

use tessera_spatial::tiler::{ScalarType, ScalarValue};

use crate::error::{Result, StoreError};
use crate::read::{ColumnsRef, MortonSlice, ScalarSlice};

/// A position in one already-sorted input segment: its two mapped files, and the row the merge has
/// reached in them.
///
/// **This is the whole of what a k-way merge costs per input.** Both files are mmapped and
/// uncompressed by contract (§10.3, contracts §2.6), so a cursor is an index into a mapping rather
/// than a decoded batch, and a merge holds *k* of these instead of the corpus.
pub(crate) struct SegmentCursor {
    pub(crate) seg_id: String,
    pub(crate) morton: MortonSlice,
    pub(crate) columns: ColumnsRef,
    pub(crate) row: usize,
    pub(crate) rows: usize,
}

impl SegmentCursor {
    /// Map `dir`'s `morton.u32` and `columns.arrow`. `op` names the caller in any error this
    /// raises — the one thing the two producers legitimately differ about.
    pub(crate) fn open(dir: &Path, seg_id: String, op: &str) -> Result<Self> {
        let morton = MortonSlice::load(&dir.join("morton.u32"))?;
        let columns = ColumnsRef::load(&dir.join("columns.arrow"))?;
        // **Both of this cursor's callers stream, and neither shares these mappings with a
        // request** (compaction §6.1, decision 0052). A fold reads every segment once and a merge
        // reads its selected ones once; a viewport reads a few tile ranges out of each, through
        // mappings of its own. So the reclaim bias `MADV_SEQUENTIAL` asks for is exactly right here
        // and would be exactly wrong on the request path's mapping of the same file — which is the
        // distinction that decides where the hint may go, since `madvise` applies to the mapping
        // and not to the file.
        morton.advise_sequential();
        columns.advise_sequential();
        let rows = morton.len();
        if rows != columns.tessera_id().len() || rows != columns.residual().len() {
            return Err(StoreError::MalformedBundle {
                detail: format!(
                    "{op}: segment '{seg_id}' has {rows} codes against {} identities",
                    columns.tessera_id().len()
                ),
            });
        }
        Ok(SegmentCursor {
            seg_id,
            morton,
            columns,
            row: 0,
            rows,
        })
    }

    /// This cursor's current `(morton, tessera_id)`, or `None` once it is spent.
    ///
    /// `MortonSlice::load` verified the codes ascend, so the sequence a cursor offers is
    /// non-decreasing and the heap's output is sorted — the property
    /// [`crate::write::SegmentWriter::append`] asserts and `tile_ranges`' binary search needs.
    pub(crate) fn key(&self) -> Option<(u32, u64)> {
        (self.row < self.rows).then(|| {
            (
                self.morton.u32()[self.row],
                self.columns.tessera_id()[self.row],
            )
        })
    }
}

/// This row's declared scalars, in schema order — the shape [`crate::write::SegmentRow`] wants.
///
/// **A column the input lacks, or holds under another type, fails the operation**, and the
/// alternative is why: a `filter_map` here drops the missing one and shifts every later scalar up a
/// position, so the output segment's columns are silently transposed — every value present, every
/// value under the wrong name, and no count or digest that would show it.
pub(crate) fn gather_scalars(
    columns: &ColumnsRef,
    schema: &[(String, ScalarType)],
    row: usize,
    seg_id: &str,
    op: &str,
) -> Result<Vec<ScalarValue>> {
    let mismatch = |declared: ScalarType, found: &str| StoreError::MalformedBundle {
        detail: format!(
            "{op}: segment '{seg_id}' holds scalar column of type {found} where the bundle \
             declares {declared:?}; writing it would put the value under another column's name"
        ),
    };
    schema
        .iter()
        .map(|(name, declared)| {
            let slice = columns
                .scalar(name)
                .ok_or_else(|| StoreError::MalformedBundle {
                    detail: format!(
                        "{op}: segment '{seg_id}' has no scalar column '{name}', which this \
                         bundle declares; dropping it would shift every later scalar into the \
                         wrong column"
                    ),
                })?;
            // Each arm pairs the *stored* type with the *declared* one and the fallthrough
            // refuses: a narrowing or widening coercion here would let a merge rewrite a column
            // at a width the manifest does not declare, which the next reader opens as garbage
            // rather than as an error.
            //
            // Generated for the flat members, because there are eleven and the failure mode of
            // hand-writing them is one pair transposed — which reads every row of two columns
            // into each other, with the row count and every type still agreeing.
            macro_rules! pairs {
                ($(($variant:ident, $value:ident)),* $(,)?) => {
                    match (slice, declared) {
                        $((ScalarSlice::$variant(v), ScalarType::$variant) => {
                            Ok(ScalarValue::$value(v[row]))
                        })*
                        (ScalarSlice::Bool(v), ScalarType::Bool) => {
                            Ok(ScalarValue::Bool(v.value(row)))
                        }
                        (ScalarSlice::Utf8(v), ScalarType::Utf8) => {
                            Ok(ScalarValue::Utf8(v.value(row).to_string()))
                        }
                        (stored, declared) => Err(mismatch(*declared, stored.type_name())),
                    }
                };
            }
            pairs! {
                (U8, U8),
                (U16, U16),
                (U32, U32),
                (U64, U64),
                (I8, I8),
                (I16, I16),
                (I32, I32),
                (I64, I64),
                (F32, F32),
                (F64, F64),
                (TimestampUs, TimestampUs),
            }
        })
        .collect()
}
