//! `/healthz` and `/readyz`, present on every listener (R5).
//!
//! `readyz`'s freshness-lag gate (contracts §2.3) is trivially satisfied in Phase 1 (a single
//! local writer, no replicas — R5's note) and its default is an open item (contracts §7),
//! acknowledged rather than implemented. Because `Engine::open` only returns once the bundle is
//! digest-verified, the WAL is replayed, and the plugin is loaded (R5: "ready = bundle verified +
//! WAL replayed + plugin loaded"), a running server is ready by construction the moment it is
//! able to answer requests at all — there is no intermediate "up but not ready" state to model.

use axum::http::StatusCode;

pub async fn healthz() -> StatusCode {
    StatusCode::OK
}

pub async fn readyz() -> StatusCode {
    StatusCode::OK
}
