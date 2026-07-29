//! `tessera-wire` — per-session handle tables and Arrow IPC viewer payloads.
//!
//! This crate is the I10 trust boundary (design §4): entity IDs never cross to a client.
//! [`handles::HandleTable`] translates `EntityId` to a per-session opaque `Handle`;
//! [`payload::viewport_ipc`] then serialises the response from `Handle` and plain columns only,
//! with no dependency edge to `tessera-engine`, `tessera-store` or `tessera-authz`
//! (`scripts/check-layers.sh`). `EntityId` is importable only inside the `handles` module.

pub mod handles;
pub mod payload;

pub use handles::HandleTable;
pub use payload::{viewport_ipc, ScalarColumn};
