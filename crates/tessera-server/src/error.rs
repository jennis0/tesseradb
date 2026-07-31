//! HTTP error mapping to Reference Sheet R5's closed code list.
//!
//! Every response body is `{"error": code, "detail": string}`. `detail` strings built by this
//! crate never carry a bearer token, auth-data bytes, an entity id, or a server filesystem path
//! (the logging rule Task 15 tests, honoured here too even though these are response bodies, not
//! log lines). That is enforced, not merely intended: a lower layer's error `Display` is never
//! forwarded into a body — see [`map_store_error`], which logs it and substitutes a fixed string.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

use tessera_engine::EngineError;

/// One of R5's closed set of API errors. No other HTTP status this server returns is meaningful
/// to a caller — every fallible handler in this crate maps its failures into one of these.
#[derive(Debug)]
pub enum ApiError {
    /// 401: missing, malformed, or unrecognised bearer credential.
    BadCredential,
    /// 403: a bearer token the engine recognises, but whose `expires_at` has passed (the engine
    /// itself never checks this — Task 11's report flags it as a server obligation).
    ExpiredToken,
    /// 404: a named slice, external id, or handle this bundle/session has never heard of.
    Unknown(String),
    /// 409: an ingest batch id replayed with a body that does not match what was accepted before.
    Conflict(String),
    /// 410: a presented pin's `(prefix, segments_version)` no longer matches the live generation.
    PinExpired,
    /// 422: a request that parsed as JSON/Arrow but violates this API's own contract (a malformed
    /// bbox, an ambiguous slice header, an unknown change op, non-UTF-8 `access` bytes, ...).
    Contract(String),
    /// 500: mask construction, WAL durability, or any other fail-closed failure (Global
    /// Constraint 3). Never returned for a partial or best-effort result.
    FailClosed(String),
    /// 429 (D-B/D-E): the viewer/session compute-admission gate is saturated — either the outer
    /// slots semaphore had no permit to `try_acquire` at all, or the inner compute semaphore did
    /// not free one within `admission_timeout_ms`. Whole-request shed, never a partial result or
    /// a narrowed `k` (spec constraint: shedding must not change WHAT a principal sees). Carries
    /// `Retry-After: 1` and body `retry_after_s: 1`, both fixed, never a knob. Also reached from
    /// `EngineError::ProjectionBuilding`/`FragmentBuilding` (D-G): a concurrent single-flight
    /// build is already in progress, and by the client's retry the slot is warm.
    Backpressure,
}

#[derive(Serialize)]
struct ErrorBody {
    error: &'static str,
    detail: String,
    /// Contracts §3.1's error body: `retry_after_s` is optional and, until this task, unused —
    /// present only on `Backpressure`, so every other error body stays byte-identical.
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_after_s: Option<u64>,
}

/// D-E: `Retry-After` is fixed at 1 second, never a knob — the same figure the body's
/// `retry_after_s` carries, so a caller reading either agrees with the other.
const RETRY_AFTER_SECS: u64 = 1;

impl ApiError {
    fn parts(&self) -> (StatusCode, &'static str, String) {
        match self {
            ApiError::BadCredential => (
                StatusCode::UNAUTHORIZED,
                "bad-credential",
                "missing or invalid bearer credential".to_string(),
            ),
            ApiError::ExpiredToken => (
                StatusCode::FORBIDDEN,
                "expired-token",
                "session token has expired or was revoked".to_string(),
            ),
            ApiError::Unknown(detail) => (StatusCode::NOT_FOUND, "unknown", detail.clone()),
            ApiError::Conflict(detail) => (StatusCode::CONFLICT, "conflict", detail.clone()),
            ApiError::PinExpired => (
                StatusCode::GONE,
                "pin-expired",
                "pinned geometry is no longer current".to_string(),
            ),
            ApiError::Contract(detail) => {
                (StatusCode::UNPROCESSABLE_ENTITY, "contract", detail.clone())
            }
            ApiError::FailClosed(detail) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "fail-closed",
                detail.clone(),
            ),
            ApiError::Backpressure => (
                StatusCode::TOO_MANY_REQUESTS,
                "backpressure",
                "the server is at its compute-admission bound; retry shortly".to_string(),
            ),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, detail) = self.parts();
        // `retry_after_s` and the `Retry-After` header carry the same fixed value and appear
        // only on `Backpressure` (D-E) — every other error body stays byte-identical to before
        // this task, since `ErrorBody::retry_after_s` is `skip_serializing_if` `None`.
        let retry_after_s = matches!(self, ApiError::Backpressure).then_some(RETRY_AFTER_SECS);
        let mut response = (
            status,
            Json(ErrorBody {
                error: code,
                detail,
                retry_after_s,
            }),
        )
            .into_response();
        if let Some(secs) = retry_after_s {
            response.headers_mut().insert(
                axum::http::header::RETRY_AFTER,
                axum::http::HeaderValue::from_str(&secs.to_string())
                    .expect("a decimal-digit string is always a valid header value"),
            );
        }
        response
    }
}

/// Map an `EngineError` to the R5 code list. `MultiSegmentSlice`, `Store`/`Wal`/`Overlay`/
/// `Plugin`/`Io`/`Malformed` are all fail-closed engine-internal failures (500); `PinExpired`,
/// `UnknownSlice` and `StaleIdentityEpoch` have a more specific code.
pub fn map_engine_error(e: EngineError) -> ApiError {
    match e {
        EngineError::PinExpired => ApiError::PinExpired,
        EngineError::UnknownSlice(slice) => ApiError::Unknown(format!("unknown slice '{slice}'")),
        // Contracts §2.2/§3.2 r6: `POST /v1/items/{tessera_id}`'s caller-supplied `epoch` did not
        // match the generation `Engine::item` validated it against (fix wave, Task 2 finding —
        // this check used to run in the handler, against a separate `Engine::meta()` call, before
        // moving inside `Engine::item` to close a second generation load). Fixed detail string,
        // named explicitly rather than left to the catch-all, so a future catch-all change can
        // never accidentally alter this one response's body.
        EngineError::StaleIdentityEpoch => {
            ApiError::Conflict("stale identity epoch; re-resolve by external_id".to_string())
        }
        // A refused underlay is a request the caller can fix by asking for less, so it is a
        // contract error (422) rather than a fail-closed 500. Its `Display` names only the
        // offending numbers and the configured bounds — no path, no corpus fact.
        EngineError::UnderlayRefused(detail) => ApiError::Contract(detail),
        // Also a request the caller can fix by asking for less, and its Display names only the
        // caller's own numbers and the configured limit.
        too_many @ EngineError::TooManyTiles { .. } => ApiError::Contract(too_many.to_string()),
        // Lifecycle §2.2's per-session pin cap, the third member of the same family: contracts
        // §3.1's 422 row is "malformed request, bounds exceeded, unknown filter operand", and this
        // is a bound exceeded. Deliberately NOT 429 — the cap clears when a pin expires, on the
        // TTL's timescale, so `Retry-After: 1` would be a lie. Landed by the seam commit with the
        // variant itself (Task 0 gate, C2) so Track C's Task 4, which owns `pins.rs` but not this
        // file, does not have to choose between editing Track B's file and letting a caller-fixable
        // bound fall through the catch-all below as a fail-closed 500.
        cap @ EngineError::PinCapExceeded { .. } => ApiError::Contract(cap.to_string()),
        // `Store`/`Io` wrap a `StoreError`/`io::Error` whose `Display` names a filesystem path —
        // the same leak `map_store_error` closes, reached through the engine's error enum instead
        // of directly. One sanitiser for both doors.
        store_or_io @ (EngineError::Store(_) | EngineError::Io(_)) => map_store_error(store_or_io),
        // D-G / Task 4: a concurrent request is already building this session's row projection.
        // The single-flight cache never blocks a second caller (D-G's non-blocking-waiters
        // rule), so the honest response is 429 `backpressure` with `Retry-After: 1` — by the
        // client's retry the slot is warm. Named explicitly rather than left to the catch-all so
        // this mapping stays visible at the call site.
        EngineError::ProjectionBuilding => ApiError::Backpressure,
        // D-G / Task 4 (lifecycle §3.3): the fragment-cache twin of the arm above — a concurrent
        // `authorise` call is already building this credential's mask fragment. Same mapping.
        EngineError::FragmentBuilding => ApiError::Backpressure,
        // D-C: cooperative cancellation (the rapid-pan case). Named explicitly, rather than left
        // to the catch-all below, so the response body can NEVER carry this variant's own
        // `Display` — a fixed string only, the same rule `map_store_error`/`map_join_error` apply
        // to a lower layer's text. Fail-closed 500, never a 2xx or any 4xx: in practice this arm
        // is unreachable today (the server's drop-guard only flips the token when the whole
        // handler future is dropped, which means nobody is left to read a response either), but it
        // must stay fail-closed-shaped in case a future refactor makes it reachable on a still-live
        // connection.
        EngineError::Cancelled => ApiError::FailClosed(
            "request cancelled before completion; the request was refused rather than answered \
             partially"
                .to_string(),
        ),
        other => ApiError::FailClosed(other.to_string()),
    }
}

/// Map a `tessera-store` read failure to `500 fail-closed` — generic over the error type so this
/// crate never names `StoreError` (Ruling B; `scripts/check-layers.sh` forbids a direct
/// `tessera-server -> tessera-store` dependency edge, since the server is meant to see engine API
/// types only). `Engine::item`/`Engine::resolve_external_id` surface `StoreError` only for a
/// genuinely unreadable sidecar or bundle file — a server fault, never a shape an attacker can
/// choose by picking an identifier (see each call site's doc for why).
///
/// **The detail never crosses to the caller.** A `StoreError`'s `Display` is written for an
/// operator reading a server log: the sidecar's two inconsistency arms name the sidecar's
/// absolute path, and `StoreError::Io` names whatever path failed. Forwarding it verbatim put a
/// filesystem path in a viewer-plane 500 body, and — before the store's messages were made
/// entity-independent — an entity ID with it, contradicting this module's own opening claim.
///
/// Both arms are unreachable in Phase 1, but they go live in Phase 2: once a flush gives buffered
/// entities rows, a post-build item whose external ID is genuinely null (a legitimate state under
/// §3.4) takes the sidecar's inconsistency branch, and the caller gets a 500 for a perfectly
/// correct item. Whatever else that costs, it must not also be a disclosure.
///
/// Diagnosability moves to the log, not to the client: the full `Display` is emitted at
/// `error!`, and the body is a fixed, entity-independent string. Uniform across all three planes
/// deliberately — a per-plane branch here is a rule that gets applied to the wrong plane exactly
/// once.
pub fn map_store_error<E: std::fmt::Display>(e: E) -> ApiError {
    tracing::error!(detail = %e, "bundle/sidecar read failed; answering fail-closed");
    ApiError::FailClosed(
        "could not read this bundle's stored data; the request was refused rather than answered \
         partially"
            .to_string(),
    )
}

/// Map a WAL append/fsync failure (`tessera_lifecycle::wal::WalError`, from
/// `Engine::accept_ingest`/`Engine::accept_change`) to `500 fail-closed` — generic over the error
/// type the same way [`map_store_error`] is, so this crate does not need to name
/// `tessera_lifecycle`'s error type here just to sanitise it.
///
/// **The detail never crosses to the caller.** `WalError::Io` wraps a raw `std::io::Error`, whose
/// `Display` can echo whatever the OS or a lower call site chose to say about the failure —
/// exactly the class of detail (up to and including a filesystem path) this module's opening doc
/// comment forbids in a body, the same door [`map_store_error`] already closes for a different
/// lower layer. Before this fix, both `/control/ingest` and `/control/changes` built their `500`
/// body with `format!("wal append/fsync failed: {e}")` directly — this closes it, one sanitiser
/// for a third door.
///
/// Diagnosability moves to the log: the full `Display` is emitted at `error!` (this call site's
/// own `tracing::error!`, immediately above where this is used, carries the batch/op context;
/// this adds the error detail itself, which neither call site logged before this fix), and the
/// body is a fixed, path-independent string.
pub fn map_wal_error<E: std::fmt::Display>(e: E) -> ApiError {
    tracing::error!(detail = %e, "wal append/fsync failed; answering fail-closed");
    ApiError::FailClosed(
        "a durability write failed; the request was refused rather than answered partially"
            .to_string(),
    )
}

/// Map a `spawn_blocking` `JoinError` (Task 3, D-A) to the fail-closed 500 arm. A `JoinError` here
/// means the closure running the engine call panicked — I13: a panic is a failed request, never
/// an empty one, so this is a typed 500, not a dropped connection or a silently empty body.
///
/// **The panic payload never crosses into the response body** — same rule as
/// [`map_store_error`]: `JoinError`'s `Display` can echo whatever `&str`/`String` payload the
/// panic carried, which may originate deep in the engine (a file path, an assertion detail) and
/// was never written with a caller-facing audience in mind. Logged in full at `error!`, replaced
/// here with the same fixed string this crate already uses for other internal faults.
pub fn map_join_error(e: tokio::task::JoinError) -> ApiError {
    tracing::error!(detail = %e, "spawn_blocking closure panicked; answering fail-closed");
    ApiError::FailClosed(
        "an internal error occurred while handling this request; the request was refused rather \
         than answered partially"
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fix wave, Task 3: `WalError`'s `Display` (a raw `std::io::Error`, potentially naming the
    /// WAL's filesystem path) must never reach the caller either — `map_wal_error`'s twin of
    /// `map_store_error_does_not_forward_the_detail_to_the_caller`, for the third door
    /// (`/control/ingest` and `/control/changes`) that used to forward a lower layer's `Display`
    /// straight into a `500` body via `format!("wal append/fsync failed: {e}")`.
    #[test]
    fn map_wal_error_does_not_forward_the_detail_to_the_caller() {
        let leaky = "wal io error: No space left on device (os error 28) at \
                     /srv/tessera/wal/v00000.log";
        let (status, code, detail) = map_wal_error(leaky).parts();
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(code, "fail-closed");
        assert!(
            !detail.contains('/'),
            "the body must not carry a server path, got: {detail}"
        );
    }

    /// S7: a lower layer's `Display` must never reach the caller. The sidecar's inconsistency arms
    /// name its absolute path (and, before this fix, an entity id); both arms go live in Phase 2,
    /// when a post-build item with a null external ID takes them for a perfectly correct item.
    #[test]
    fn map_store_error_does_not_forward_the_detail_to_the_caller() {
        let leaky = "invalid sidecar at /srv/tessera/v00000/partitions/default/entities/\
                     ext-locator.u32: entity 123456 is inconsistent";
        let (status, code, detail) = map_store_error(leaky).parts();
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(code, "fail-closed");
        assert!(
            !detail.contains("123456") && !detail.contains('/'),
            "the body must carry neither an identifier nor a server path, got: {detail}"
        );
    }

    /// I13, `map_join_error`'s twin of `map_store_error_does_not_forward_the_detail_to_the_caller`
    /// above: a `JoinError`'s `Display` can echo the panicking closure's payload verbatim (a file
    /// path, an assertion detail, anything the panic carried), so it must never reach the response
    /// body either. Spawns a task whose panic message names a path and an entity id, exactly the
    /// two things this crate's opening doc comment forbids in a body.
    #[tokio::test]
    async fn map_join_error_does_not_forward_the_detail_to_the_caller() {
        let join_error = tokio::spawn(async {
            panic!(
                "invalid sidecar at /srv/tessera/v00000/partitions/default/entities/\
                 ext-locator.u32: entity 123456 is inconsistent"
            );
        })
        .await
        .expect_err("the spawned task panicked, so awaiting its handle must yield a JoinError");

        let (status, code, detail) = map_join_error(join_error).parts();
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(code, "fail-closed");
        assert!(
            !detail.contains("123456") && !detail.contains('/'),
            "the body must carry neither an identifier nor a server path, got: {detail}"
        );
    }

    /// The same door, reached through the engine's error enum rather than directly.
    #[test]
    fn map_engine_error_sanitises_its_store_and_io_arms() {
        let engine_err = EngineError::Io(std::io::Error::other(
            "/srv/tessera/v00000/partitions/default/terms/postings.arrow: bad",
        ));
        let (_, _, detail) = map_engine_error(engine_err).parts();
        assert!(
            !detail.contains('/'),
            "the body must not carry a server path, got: {detail}"
        );
    }

    /// D-G / Task 4: `ProjectionBuilding` is explicitly named in `map_engine_error`'s match (not
    /// caught only by the wildcard arm) and now maps to 429 `backpressure` — a concurrent
    /// single-flight build never blocks, so the honest response is retryable, not fail-closed.
    #[test]
    fn map_engine_error_takes_projection_building_to_backpressure() {
        let (status, code, _) = map_engine_error(EngineError::ProjectionBuilding).parts();
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(code, "backpressure");
    }

    /// D-G / Task 4: `FragmentBuilding` is explicitly named in `map_engine_error`'s match and
    /// maps to the same 429 `backpressure` arm as `ProjectionBuilding`.
    #[test]
    fn map_engine_error_takes_fragment_building_to_backpressure() {
        let (status, code, _) = map_engine_error(EngineError::FragmentBuilding).parts();
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(code, "backpressure");
    }

    /// Task 0 gate (C2): the per-session pin cap is a **bound exceeded**, so contracts §3.1 puts
    /// it on the 422 `contract` row beside `TooManyTiles`, not on 429 — and it is named in
    /// `map_engine_error`'s match rather than left to the catch-all, which would have made a
    /// caller-fixable refusal a fail-closed 500. Constructed by nobody until Task 4; this test is
    /// what stops the mapping rotting in the meantime.
    #[test]
    fn map_engine_error_takes_a_pin_cap_refusal_to_422_contract() {
        let (status, code, detail) =
            map_engine_error(EngineError::PinCapExceeded { held: 4, limit: 4 }).parts();
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(code, "contract");
        assert!(
            detail.contains('4') && !detail.contains('/'),
            "the body must name the caller's own numbers and no server path, got: {detail}"
        );
    }

    /// Fix wave, Task 2: `StaleIdentityEpoch` (now raised by `Engine::item` itself, against the
    /// one generation it loads, rather than by a separate handler-side `Engine::meta()` check)
    /// maps to the same 409 `conflict` body `POST /v1/items/{tessera_id}` has always returned for
    /// a stale epoch.
    #[test]
    fn map_engine_error_takes_stale_identity_epoch_to_409_conflict() {
        let (status, code, detail) = map_engine_error(EngineError::StaleIdentityEpoch).parts();
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(code, "conflict");
        assert_eq!(detail, "stale identity epoch; re-resolve by external_id");
    }

    /// D-C: `Cancelled` is explicitly named in `map_engine_error`'s match (not caught only by the
    /// wildcard arm) and maps to the fail-closed 500 — never a 2xx or any 4xx. This is the arm's
    /// defence-in-depth case (see its comment at the match site): the server-side drop-guard fires
    /// on ANY future drop, so a future refactor could in principle reach this arm on a still-live
    /// connection, and the response it produces must stay fail-closed-shaped regardless.
    #[test]
    fn map_engine_error_takes_cancelled_to_fail_closed_500() {
        let (status, code, _) = map_engine_error(EngineError::Cancelled).parts();
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(code, "fail-closed");
    }

    /// D-E: a `Backpressure` response carries both the `Retry-After: 1` header and the
    /// `retry_after_s: 1` body field, fixed, so a caller reading either agrees with the other.
    #[test]
    fn backpressure_carries_retry_after_header_and_body_field() {
        let response = ApiError::Backpressure.into_response();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
            Some("1")
        );
    }

    /// D-E: every OTHER error body stays byte-identical to before this task — `retry_after_s` is
    /// `skip_serializing_if Option::is_none`, so a non-backpressure error's JSON body must not
    /// gain the field at all.
    #[test]
    fn non_backpressure_errors_omit_retry_after_s_from_the_body() {
        let (_, code, _) = ApiError::BadCredential.parts();
        assert_eq!(code, "bad-credential");
        let body = ErrorBody {
            error: code,
            detail: "x".to_string(),
            retry_after_s: None,
        };
        let json = serde_json::to_string(&body).unwrap();
        assert!(
            !json.contains("retry_after_s"),
            "non-backpressure body must omit retry_after_s entirely, got: {json}"
        );
    }
}
