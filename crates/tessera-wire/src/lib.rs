//! `tessera-wire` — the viewer plane's framed Arrow IPC payloads, plus the per-session handle
//! table the wire keeps for node handles.
//!
//! This crate is the I10 trust boundary (design §4): entity IDs never cross to a client.
//! [`payload`]'s frame builders serialise the streamed `/v1/viewport` response
//! (`docs/design/streamed-serving.md`) from a `tessera_id: u64` column and plain scalar columns
//! only (contracts §2.6; per-session handles are retired from this plane by [decision 0006], and
//! [`handles`]'s module doc says why the mechanism is kept rather than deleted), with no
//! dependency edge to `tessera-engine`, `tessera-store` or `tessera-authz`
//! (`scripts/check-layers.sh`). `EntityId` is importable only inside the `handles` module, which
//! node handles will use.
//!
//! [decision 0006]: ../../../docs/decisions/0006-per-session-handles-retired.md

pub mod handles;
pub mod payload;

pub use handles::HandleTable;
pub use payload::{
    artifacts_frame, artifacts_identity_frame, points_frame, split_frames, sub_cells_frame,
    tiles_frame, trailer_frame, ArtifactRow, FrameError, ScalarColumn, FRAME_ARTIFACTS,
    FRAME_HEADER_BYTES, FRAME_POINTS, FRAME_SUB_CELLS, FRAME_TILES, FRAME_TRAILER,
};
