//! Quantisation, Morton codes, tile addressing (contracts §2.5) — and shapes over the grid
//! (`polygon-membership.md`).

pub mod morton;
pub mod shape;
pub mod tiler;

pub use morton::{
    cell, fixed32, interleave, interleave_bits, morton_of, split32, tiles_for_bbox,
    tiles_for_bbox_count, unsplit32, Bounds, Tile,
};
pub use tiler::{sort_batch, ScalarType, ScalarValue, TilerItem};
