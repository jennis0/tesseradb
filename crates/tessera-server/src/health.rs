//! `/healthz` and `/readyz`, on the viewer and session listeners. The control listener has
//! neither, so that every route on it needs the operator credential; `/control/status` serves
//! there instead.
//!
//! A node is ready when its write executor is running and no partition has stepped down. The
//! bundle's digests, its segment manifests and the plugin are checked at startup, which fails
//! without them; nothing reopens a bundle at runtime, so they need no check here. An executor
//! hung inside `fsync` still reports running, so this can answer 200 while writes block.
//!
//! Not ready means route `/v1/*` elsewhere. It does not mean restart, since replay does not
//! restore a deny that was applied but never made durable, and it does not mean route
//! `/control/changes` elsewhere, since a node with a poisoned WAL still applies denies.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;

use tessera_engine::ExecutorPosture;

use crate::state::AppState;

pub async fn healthz() -> StatusCode {
    StatusCode::OK
}

/// Ready only when the write executor is `Running`. An unrecognised posture decodes as
/// `NotStarted`, so a variant added later reads as not ready.
pub(crate) fn is_ready(posture: ExecutorPosture) -> bool {
    matches!(posture, ExecutorPosture::Running)
}

/// A bare status with no body and no header that varies: it is unauthenticated on the viewer
/// listener, and the executor's posture is internal state. It must never take a lock.
pub async fn readyz(State(state): State<Arc<AppState>>) -> StatusCode {
    // A stepped-down partition always fails readiness: every node writes, and the ingest buffer
    // is rebuilt from the served watermark, so flushing from an older one would lose acked rows.
    if is_ready(state.engine.write_executor_posture()) && !state.engine.any_partition_stepped_down()
    {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `/control/status`'s `posture` strings are an operator-facing contract.
    #[test]
    fn posture_spellings_are_stable() {
        assert_eq!(ExecutorPosture::NotStarted.as_str(), "not-started");
        assert_eq!(ExecutorPosture::Running.as_str(), "running");
        assert_eq!(ExecutorPosture::WalPoisoned.as_str(), "wal-poisoned");
        assert_eq!(ExecutorPosture::Dead.as_str(), "dead");
    }

    /// Every posture, including `Dead`, which no HTTP test reaches. That a panicked executor
    /// becomes `Dead` is tested in `tessera-engine` (`an_executor_panic_is_reported_dead`).
    #[test]
    fn only_a_running_executor_is_ready() {
        assert!(is_ready(ExecutorPosture::Running));
        assert!(!is_ready(ExecutorPosture::NotStarted));
        assert!(
            !is_ready(ExecutorPosture::WalPoisoned),
            "a poisoned WAL is not ready — it still ACCEPTS and applies denies, which is why the \
             posture is not a shutdown, but it must not be routed to as healthy"
        );
        assert!(
            !is_ready(ExecutorPosture::Dead),
            "a dead writer must never report ready; a readiness gate green over it is the fail-open"
        );
    }
}
