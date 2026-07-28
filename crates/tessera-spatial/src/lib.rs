//! Quantisation, Morton codes, and tile addressing (contracts §2.5).

pub mod morton;

pub use morton::{cell, interleave, interleave_bits, morton_of, tiles_for_bbox, Extent, Tile};
