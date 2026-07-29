//! HTTP error mapping to Reference Sheet R5's closed code list.
//!
//! Every response body is `{"error": code, "detail": string}`. `detail` strings built by this
//! crate never carry a bearer token, auth-data bytes, or an entity id (the logging rule Task 15
//! tests, honoured here too even though these are response bodies, not log lines).

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
        other => ApiError::FailClosed(other.to_string()),
    }
}
