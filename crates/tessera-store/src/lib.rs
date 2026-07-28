//! Segment file writers: `columns.arrow`, `morton.u64`, `permutation.bin` (contracts §2.1,
//! Reference Sheet R4). Byte-level only — no on-disk index/mask construction lives here yet.

pub mod write;
