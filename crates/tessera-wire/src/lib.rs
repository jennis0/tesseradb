//! `tessera-wire` — Arrow IPC viewer payloads, plus the per-session handle tables retained for
//! Phase 3.
//!
//! This crate is the I10 trust boundary (design §4): entity IDs never cross to a client.
//! [`payload::viewport_ipc`] serialises the viewer plane's points batch from a `tessera_id: u64`
//! column and plain scalar columns only (contracts r6, owner decision 2026-07-29 — per-session
//! handles are retired from this plane; see [`handles`]'s module doc for why the mechanism is
//! kept, not deleted), with no dependency edge to `tessera-engine`, `tessera-store` or
//! `tessera-authz` (`scripts/check-layers.sh`). `EntityId` is importable only inside the
//! `handles` module, which Phase 3's node handles will use.

pub mod handles;
pub mod payload;

pub use handles::HandleTable;
pub use payload::{viewport_ipc, ScalarColumn};
