//! `/healthz` (liveness) and `/readyz` (readiness), on the **viewer and session** listeners
//! (R5, contracts §3.1 r11).
//!
//! **Not on the control listener**
//! (docs/decisions/0011-health-probes-off-control-plane.md). The control plane is uniformly
//! authenticated — every route on it requires the operator credential, with no exemption — which is
//! what lets the control listener be firewalled to admin-only with no health-probe hole. Nothing is
//! lost from the probe itself: both handlers below are identical on every listener they are mounted
//! on, so a third mount would be a third copy of one bit. What an unauthenticated caller cannot
//! observe is "the control listener is accepting connections"; the argument for accepting that, and
//! the `/control/status` call that replaces it, is in `control::require_operator_credential`'s doc.
//!
//! # What `/readyz` answers, and where each half of the answer is discharged
//!
//! Contracts §3.1 defines readiness as **"verified + fresh + pinned + plugin loaded + workers
//! ready"**. Only one of those five is a *runtime* condition in this build, and the honest artefact
//! is the table saying so rather than a conjunction of terms that are constant `true`:
//!
//! | §3.1 condition | discharged | where |
//! |---|---|---|
//! | **verified** (bundle digests) | at `prepare` — the process refuses to start | `open_bundle`'s digest checks → `Engine::open` → [`crate::prepare`] returns `Err` before [`crate::run`] binds a listener |
//! | **pinned** (a verifying `SEGMENTS-<n>.json` per partition) | at `prepare` — the process refuses to start | a deny-carrying candidate that fails verification → `StoreError::UnverifiedDenyManifest` → the per-partition loop propagates it out of `open_bundle` |
//! | **plugin loaded** | at `prepare` — the process refuses to start | `Engine::open` loads it |
//! | **workers ready** | **here** | `Engine::write_executor_posture()` — see [`is_ready`] |
//! | **fresh** (step-down lag bound) | **here, as a refusal rather than a bound** | `Engine::any_partition_stepped_down()` — see [`readyz`] |
//!
//! The freshness row is not the *lag bound* contracts §2.3 describes and roadmap O4 tracks: there
//! is no configured bound, and none is needed for correctness here. A candidate carrying
//! deny-disposition state is never stepped past at all (it is `UnverifiedDenyManifest`), so a
//! step-down can only cost *items*. What makes even that unacceptable is that every node writes:
//! the ingest buffer is reconstructed from the **served** watermark, so a stepped-down node that
//! flushed would lose the acked rows between the two watermarks. Hence an unconditional refusal
//! where a bound would otherwise go.
//!
//! **The first three are conditions on a process that started at all**, and they hold only while
//! nothing re-opens a bundle at runtime. Nothing does: `open_bundle`'s only caller in the serving
//! path is `Engine::open`. **A runtime bundle swap is what would make "verified" and "pinned"
//! genuine runtime conditions**, at which point they need real conjuncts here. Saying so is a true
//! statement, where a dead `&&` over three constants would read as coverage while providing none.
//!
//! # What this predicate cannot see
//!
//! It reports the executor's *posture*, which is a statement about having started and not yet
//! failed — **not** about making progress. An executor blocked *inside* `fsync` (a hung mount, a
//! stuck device) never returns, so the WAL's poison flag is never mirrored, the posture stays
//! `Running`, and this endpoint answers 200 while every `/control/changes` blocks indefinitely
//! awaiting a receipt. That is the one state in which the node is not ready and says it is. Closing
//! it needs a progress term (a last-ack timestamp against a deadline) and a request timeout,
//! neither of which exists — recorded here rather than left for an operator to infer from a green
//! probe.
//!
//! # Two obligations that live outside this process
//!
//! **A control-plane router must not drain a node on this signal.** `WalPoisoned` is a *posture*,
//! not a shutdown, precisely so the node keeps accepting and applying denies (lifecycle §4: never a
//! refusal that leaves a deny unapplied) — and on a poisoned node an applied-anyway suppression
//! exists **only** in that node's in-memory overlay, because §4 gates side-manifest publication on
//! WAL durability. Draining `/control/changes` away from it is therefore the fail-open the posture
//! exists to prevent, achieved from outside the process where no code here can stop it. Routing
//! `/v1/*` away is the intended use; routing `/control/changes` away is not.
//!
//! **Not-ready here means "stop routing", never "restart me".** Replay does not reinstate an
//! applied-anyway deny — that is what lifecycle §4 means by durability being *owed* — so an
//! automated restart on a red probe brings suppressed items back. The operator's signal for which
//! posture it is, and therefore what to do, is the bearer-gated `/control/status`.
//!
//! # Why this stays a bare status with no body
//!
//! `/readyz` is unauthenticated on both listeners that serve it, and the viewer listener is the
//! public one. Publishing the posture *variant* would hand internal write-path state to anyone who
//! can reach a socket (SA §9), so the response is a bare `StatusCode` — no
//! `ApiError`, hence no `detail` string, and `ApiError::NotReady` deliberately carries no
//! `Retry-After` either, since a posture-varying number is the same disclosure by another route.
//!
//! One bit does cross: "a write-path fault exists on this node". That is activity rather than
//! content, carries no item, count or label, and is derived from no principal's `M_auth`, so I2 is
//! untouched and it warrants no Appendix C row. The reason is written here rather than left to be
//! re-derived, along with the two constraints that keep it true: this handler must never take a
//! lock, and must never gain a posture-varying header or body.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;

use tessera_engine::ExecutorPosture;

use crate::state::AppState;

pub async fn healthz() -> StatusCode {
    StatusCode::OK
}

/// The readiness predicate: **ready iff the write executor is `Running`.**
///
/// A named function so every posture can be classified in a unit test.
///
/// Every non-`Running` posture is not ready, `NotStarted` included. That is not a live state for a
/// server — [`crate::prepare`] starts the executor and propagates its failure before any listener
/// is bound — but the enum is `pub` and reachable by embedders and by tests, and the classification
/// is total by construction: `ExecutorPosture::from_u8` maps an unrecognised discriminant to
/// `NotStarted`, so a variant added without visiting this function reads as **not ready** rather
/// than as ready.
pub(crate) fn is_ready(posture: ExecutorPosture) -> bool {
    matches!(posture, ExecutorPosture::Running)
}

pub async fn readyz(State(state): State<Arc<AppState>>) -> StatusCode {
    // A stepped-down partition fails readiness **unconditionally**, and the qualifier a reader
    // expects — "unless this node is a read-only replica" — deliberately is not here. Every node
    // today writes, and §7.1 reconstructs the ingest buffer from the *served* watermark: after a
    // rotation the WAL rows between an older manifest's watermark and the newest one's are gone,
    // so re-flushing from a stepped-down watermark silently loses acked ingest. Lifecycle §6's
    // reader/writer distinction is what would make the qualifier meaningful; it is unbuilt.
    //
    // Failing closed costs availability on a node whose newest segment files are damaged, which
    // is the trade SA §9 prescribes.
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

    /// The whole readiness table, including the row no HTTP test reaches today.
    ///
    /// `Dead` is the row this exists for; see [`is_ready`] for what blocks the end-to-end version of
    /// it (a declined dev-dependency, not a race). The engine-level half — that a panicked executor
    /// reaches `Dead` at all, and that its in-flight caller is handed `ReceiptLost` — is
    /// `tessera-engine`'s `an_executor_panic_is_reported_dead`.
    /// `/control/status`'s `posture` strings are an operator-facing contract, so a Rust variant
    /// rename must not silently change one. Pinned here — where `/control/status` is consumed —
    /// rather than beside `as_str`, so the spellings are asserted against the surface that
    /// publishes them.
    #[test]
    fn posture_spellings_are_stable() {
        assert_eq!(ExecutorPosture::NotStarted.as_str(), "not-started");
        assert_eq!(ExecutorPosture::Running.as_str(), "running");
        assert_eq!(ExecutorPosture::WalPoisoned.as_str(), "wal-poisoned");
        assert_eq!(ExecutorPosture::Dead.as_str(), "dead");
    }

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
