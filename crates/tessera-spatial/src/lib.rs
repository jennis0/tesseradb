//! Quantisation, Morton codes, and tile addressing (contracts §2.5).

pub mod morton;
pub mod tiler;

pub use morton::{
    cell, interleave, interleave_bits, morton_of, tiles_for_bbox, tiles_for_bbox_count, Extent,
    Tile,
};
pub use tiler::{sort_batch, ScalarType, ScalarValue, TilerItem};
