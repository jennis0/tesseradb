//! Quantisation, Morton codes, tile addressing (contracts §2.5) — shapes over the grid
//! (`polygon-membership.md`), and the projections that put a place on the Earth into the grid in
//! the first place (`projections.md`).

pub mod morton;
pub mod projection;
pub mod shape;
pub mod tiler;

pub use morton::{
    cell, fixed32, interleave, interleave_bits, morton_of, split32, tiles_for_bbox,
    tiles_for_bbox_count, unsplit32, Bounds, Tile,
};
pub use projection::{Projection, WEB_MERCATOR_MAX_LATITUDE_DEG};
pub use tiler::{sort_batch, ScalarType, ScalarValue, TilerItem};
