//! Segment file writers (`columns.arrow`, `morton.u32`, `permutation.bin`; contracts §2.1),
//! the bundle read protocol and zero-copy loader, and `Permutation` — the
//! only legal EntityId→RowId path in the codebase (invariant I4).

pub mod coalesce;
pub mod error;
pub mod flush;
pub mod fold;
mod locator;
pub mod manifest;
pub mod manifest_write;
pub mod merge;
pub mod pairs;
pub mod permutation;
pub mod read;
pub mod row_entity;
pub mod reclaim;
mod segment_cursor;
mod sidecar;
pub mod vocabulary;
pub mod write;

pub use coalesce::coalesce_external_id_runs;
pub use coalesce::fold_external_id_runs;
pub use error::{Result, StoreError};
pub use flush::{digest_of, write_flush_segment, FlushInput, FlushOutput, FlushRow};
pub use fold::{fold_row_space, FoldRowSpaceOutput, FoldRowSpaceSpec, FoldSegmentInput};
pub use manifest_write::{
    fsync_written, write_current, write_manifest_json, write_segments_manifest,
};
pub use pairs::PairsParquetWriter;
pub use permutation::{Permutation, RowSpace, SegmentExtent};
pub use row_entity::{write_row_entity, RowToEntity, ROW_ENTITY_FILE};
pub use read::{
    open_bundle, open_written_prefix, tile_ranges, tile_ranges_all, tile_ranges_within, Bundle,
    ColumnsRef, MortonSlice, PartitionData, ScalarSlice, SegmentData, SliceData,
};
pub use reclaim::{hard_link_forward, reclaim_prefix};
pub use sidecar::ExternalIdSidecar;
