//! Segment file writers (`columns.arrow`, `morton.u32`, `permutation.bin`; contracts §2.1),
//! the bundle read protocol and zero-copy loader, and `Permutation` — the
//! only legal EntityId→RowId path in the codebase (invariant I4).

pub mod coalesce;
pub mod columns;
pub mod derived;
pub mod entity_terms;
pub mod error;
pub mod flush;
pub mod fold;
mod locator;
pub mod partition;
pub mod manifest;
pub mod manifest_write;
pub mod membership;
pub mod merge;
pub mod pairs;
pub mod permutation;
pub mod read;
pub mod reclaim;
pub mod render_presence;
pub mod row_entity;
mod segment_cursor;
mod sidecar;
mod view_path;
pub mod vocabulary;
pub mod write;

pub use coalesce::coalesce_external_id_runs;
pub use coalesce::fold_external_id_runs;
pub use entity_terms::{
    coalesce_entity_terms_extents, EntityTerms, EntityTermsExtentPaths, EntityTermsStack,
    EntityTermsWriter, ENTITY_TERMS_DIR, ENTITY_TERMS_HASROW_FILE, ENTITY_TERMS_OFFSETS_FILE,
    ENTITY_TERMS_TERMS_FILE,
};
pub use error::{Result, StoreError};
pub use flush::{digest_of, write_flush_segment, FlushInput, FlushOutput, FlushRow};
pub use fold::{fold_row_space, FoldRowSpaceOutput, FoldRowSpaceSpec, FoldSegmentInput};
pub use manifest_write::{
    fsync_dir, fsync_written, write_and_fsync, write_current, write_manifest_json,
    write_segments_manifest,
};
pub use pairs::PairsParquetWriter;
pub use permutation::{Permutation, RowSpace, SegmentExtent};
pub use read::{
    highest_side_manifest_n, open_bundle, open_written_prefix, tile_ranges, tile_ranges_all,
    tile_ranges_within, Bundle,
    ColumnsRef, CutIndex, MortonSlice, PartitionData, ScalarSlice, SegmentData, ViewData,
};
pub use reclaim::{hard_link_forward, reclaim_prefix, reclaim_unpublished_prefix};
pub use row_entity::{write_row_entity, RowToEntity, ROW_ENTITY_FILE};
pub use sidecar::ExternalIdSidecar;
pub use view_path::{
    scoped_column_components, scoped_column_rel, view_path, view_path_components, view_rel,
    GROUP_SEPARATOR,
};
