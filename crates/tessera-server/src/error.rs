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
}

#[derive(Serialize)]
struct ErrorBody {
    error: &'static str,
    detail: String,
}

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
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, detail) = self.parts();
        (
            status,
            Json(ErrorBody {
                error: code,
                detail,
            }),
        )
            .into_response()
    }
}

/// Map an `EngineError` to the R5 code list. `MultiSegmentSlice`, `Store`/`Wal`/`Overlay`/
/// `Plugin`/`Io`/`Malformed` are all fail-closed engine-internal failures (500); only
/// `PinExpired` and `UnknownSlice` have a more specific code.
pub fn map_engine_error(e: EngineError) -> ApiError {
    match e {
        EngineError::PinExpired => ApiError::PinExpired,
        EngineError::UnknownSlice(slice) => ApiError::Unknown(format!("unknown slice '{slice}'")),
        // `Store`/`Io` wrap a `StoreError`/`io::Error` whose `Display` names a filesystem path —
        // the same leak `map_store_error` closes, reached through the engine's error enum instead
        // of directly. One sanitiser for both doors.
        store_or_io @ (EngineError::Store(_) | EngineError::Io(_)) => map_store_error(store_or_io),
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
