//! `tessera-wire` — Arrow IPC viewer payloads, plus the per-session handle table the wire keeps
//! for node handles.
//!
//! This crate is the I10 trust boundary (design §4): entity IDs never cross to a client.
//! [`payload::viewport_ipc`] serialises the viewer plane's points batch from a `tessera_id: u64`
//! column and plain scalar columns only (contracts §2.6; per-session handles are retired from
//! this plane by [decision 0006], and [`handles`]'s module doc says why the mechanism is kept
//! rather than deleted), with no dependency edge to `tessera-engine`, `tessera-store` or
//! `tessera-authz` (`scripts/check-layers.sh`). `EntityId` is importable only inside the
//! `handles` module, which node handles will use.
//!
//! [decision 0006]: ../../../docs/decisions/0006-per-session-handles-retired.md

pub mod handles;
pub mod payload;

pub use handles::HandleTable;
pub use payload::{viewport_ipc, ScalarColumn, ViewportColumns};
