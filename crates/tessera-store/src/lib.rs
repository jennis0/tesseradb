//! Segment file writers (`columns.arrow`, `morton.u32`, `permutation.bin`; contracts §2.1,
//! Reference Sheet R4), the bundle read protocol and zero-copy loader, and `Permutation` — the
//! only legal EntityId→RowId path in the codebase (invariant I4).

pub mod error;
pub mod external_ids;
pub mod manifest;
pub mod permutation;
pub mod read;
pub mod write;

pub use error::{Result, StoreError};
pub use external_ids::{ExternalIdExtent, ExternalIdIndex};
pub use permutation::Permutation;
pub use read::{
    open_bundle, tile_ranges, Bundle, ColumnsRef, MortonSlice, PartitionData, ScalarSlice,
    SegmentData, SliceData,
};
