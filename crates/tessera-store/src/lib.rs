//! Segment file writers (`columns.arrow`, `morton.u32`, `permutation.bin`; contracts §2.1),
//! the bundle read protocol and zero-copy loader, and `Permutation` — the
//! only legal EntityId→RowId path in the codebase (invariant I4).

pub mod error;
pub mod flush;
pub mod manifest;
pub mod permutation;
pub mod read;
mod sidecar;
pub mod write;

pub use error::{Result, StoreError};
pub use flush::{write_flush_segment, FlushInput, FlushOutput, FlushRow};
pub use permutation::{Permutation, RowSpace, SegmentExtent};
pub use read::{
    open_bundle, tile_ranges, tile_ranges_all, tile_ranges_within, Bundle, ColumnsRef, MortonSlice,
    PartitionData, ScalarSlice, SegmentData, SliceData,
};
pub use sidecar::ExternalIdSidecar;
