//! The framed Arrow payloads of the viewer plane.
//!
//! Identities reach this crate as `tessera_id: u64` columns. It depends on no other Tessera
//! crate, so it cannot name an entity id or the identity key; `scripts/check-layers.sh` holds
//! that.

pub mod payload;

pub use payload::{
    artifacts_frame, artifacts_identity_frame, points_frame, points_highlight_frame, split_frames,
    sub_cells_frame, tiles_frame, trailer_frame, ArtifactRow, FrameError, ScalarColumn,
    FRAME_ARTIFACTS, FRAME_HEADER_BYTES, FRAME_POINTS, FRAME_SUB_CELLS, FRAME_TILES,
    FRAME_TRAILER,
};
