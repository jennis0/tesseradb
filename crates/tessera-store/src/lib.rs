//! Segment file writers (`columns.arrow`, `morton.u32`, `permutation.bin`; contracts §2.1),
//! the bundle read protocol and zero-copy loader, and `Permutation` — the
//! only legal EntityId→RowId path in the codebase (invariant I4).

pub mod coalesce;
pub mod error;
pub mod flush;
pub mod fold;
mod locator;
pub mod manifest;
pub mod merge;
pub mod pairs;
pub mod permutation;
pub mod read;
mod segment_cursor;
mod sidecar;
pub mod write;

pub use coalesce::coalesce_external_id_runs;
pub use coalesce::fold_external_id_runs;
pub use error::{Result, StoreError};
pub use fold::{fold_row_space, FoldRowSpaceOutput, FoldRowSpaceSpec, FoldSegmentInput};
pub use flush::{write_flush_segment, FlushInput, FlushOutput, FlushRow};
pub use pairs::PairsParquetWriter;
pub use permutation::{Permutation, RowSpace, SegmentExtent};
pub use read::{
    open_bundle, open_written_prefix, tile_ranges, tile_ranges_all, tile_ranges_within, Bundle,
    ColumnsRef, MortonSlice, PartitionData, ScalarSlice, SegmentData, SliceData,
};
pub use sidecar::ExternalIdSidecar;
