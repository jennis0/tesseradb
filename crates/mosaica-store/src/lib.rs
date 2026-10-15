//! Segment file writers (`columns.arrow`, `morton.u32`, `permutation.bin`; contracts §2.1),
//! the bundle read protocol and zero-copy loader, and `Permutation` — the
//! only legal EntityId→RowId path in the codebase (invariant I4).

pub mod access_column;
pub mod bands;
pub mod columns;
pub mod coordinates;
pub mod declaration;
pub mod derived;
pub mod edited;
pub mod entity_terms;
pub mod error;
pub mod exact_sum;
pub mod field_tallies;
pub mod flush;
pub mod fold;
pub mod key_index;
pub mod layout;
pub mod manifest;
pub mod manifest_write;
pub mod member_key;
pub mod membership;
pub mod merge;
pub mod pairs;
pub mod partition;
pub mod permutation;
mod presence;
pub mod read;
pub mod reclaim;
pub mod render_presence;
pub mod row_entity;
pub mod row_members;
pub mod scalar_column;
mod segment_cursor;
pub mod tenancy;
pub mod term_images;
pub mod unique;
pub mod utf8;
pub mod vocabulary;
pub mod write;

/// The files every segment directory holds, whichever producer wrote it. A segment may also hold
/// render presence bitmaps and `edited-rows.u32`, which only some segments need.
pub const SEGMENT_FILES: [&str; 5] = [
    "morton.u32",
    read::CutIndex::FILE,
    bands::CELL_CODES_FILE,
    "columns.arrow",
    bands::BANDS_FILE,
];

/// The little-endian `u32` at byte `at` of `bytes`.
pub(crate) fn le_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

/// The little-endian `u64` at byte `at` of `bytes`.
pub(crate) fn le_u64(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("eight bytes"))
}

pub use entity_terms::{
    coalesce_entity_terms_extents, EntityTerms, EntityTermsExtentPaths, EntityTermsStack,
    EntityTermsWriter, ENTITY_TERMS_BASES_FILE, ENTITY_TERMS_BLOCK_SHIFT, ENTITY_TERMS_HASROW_FILE,
    ENTITY_TERMS_OFFSETS_FILE, ENTITY_TERMS_TERMS_FILE,
};
pub use error::{Result, StoreError};
pub use flush::{digest_of, write_flush_segment, FlushInput, FlushOutput, FlushRow};
pub use fold::{fold_row_space, FoldRowSpaceOutput, FoldRowSpaceSpec, FoldSegmentInput};
pub use layout::{view_path_components, LayerSpaceLayout, ShardLayout, GROUP_SEPARATOR};
pub use manifest_write::{
    fsync_dir, fsync_written, prune_superseded_segments_manifests, write_and_fsync, write_current,
    write_manifest_json, write_segments_manifest, SIDE_MANIFESTS_KEPT,
};
pub use pairs::PairsParquetWriter;
pub use permutation::{EntityBuckets, Permutation, RowEntities, RowSpace, SegmentExtent};
pub use read::{
    first_code_at_or_past, highest_side_manifest_n, open_bundle, open_written_prefix, tile_ranges,
    tile_ranges_all, tile_ranges_within, Bundle, ColumnsRef, CutIndex, MortonSlice, PartitionData,
    ScalarSlice, SegmentData, ShardData, ViewData,
};
pub use reclaim::{hard_link_forward, reclaim_prefix, reclaim_unpublished_prefix};
pub use row_entity::{write_row_entity, RowToEntity, ROW_ENTITY_FILE};
