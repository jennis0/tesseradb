//! Maps failures to the API's closed set of error codes. Every body is
//! `{"error": code, "detail": string}`, with `retry_after_s` added on `backpressure`.
//!
//! No body carries an entity id, a term id, a bearer token, auth data or a server path; clients
//! see `tessera_id` and nothing internal. Store, WAL and panic text is logged and never sent. A
//! refusal's own detail is sent only when it names nothing but the caller's request and the
//! schema `/v1/meta` publishes.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

use tessera_engine::EngineError;

/// The API's closed set of errors. Every fallible handler maps its failures into one of these.
#[derive(Debug)]
pub enum ApiError {
    /// 401: missing, malformed, or unrecognised bearer credential.
    BadCredential,
    /// 403: a recognised token whose `expires_at` has passed. The engine does not check expiry;
    /// `AppState::authenticated_session` does.
    ExpiredToken,
    /// 404: a named view, unique value, or handle this bundle/session has never heard of.
    Unknown(String),
    /// 409: the request conflicts with what is already held, such as a batch id replayed with a
    /// different body. It had no effect.
    Conflict(String),
    /// 422: a request that parsed but breaks this API's contract, such as a malformed bbox or an
    /// unknown change op.
    Contract(String),
    /// 500: a failure the server will not answer partially, such as mask construction or WAL
    /// durability. Never used for a partial or best-effort result.
    FailClosed(String),
    /// 429: a bound shed the whole request. `Retry-After` and the body's `retry_after_s` carry the
    /// same number, chosen per [`ShedCause`].
    Backpressure { retry_after_s: u64, cause: ShedCause },
    /// 503: the write executor was never started, or stopped before it was handed the command, so
    /// this node took nothing. A receipt lost after the executor took the command is a 500 instead,
    /// because the command may be in force. The detail is fixed and there is no `Retry-After`: the
    /// executor's posture is shown only on `/control/status`.
    NotReady,
}

/// Which bound shed a request. Every cause answers `backpressure`; they differ in the `detail`
/// and in how `retry_after_s` is chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShedCause {
    /// The compute-admission gate had no slot, or no permit within `admission_timeout_ms`.
    /// `Retry-After` is [`RETRY_AFTER_SECS`]. Counted in that gate's `shed_total`.
    ComputeGate,
    /// `serve.bulk_admission` bulk reads were already running. `Retry-After` is
    /// [`RETRY_AFTER_SECS`].
    BulkGate,
    /// The ingest work queue or the ingest buffer is full; `retry_after_s` is estimated from the
    /// drain rate. `/control/changes` never answers 429: [`map_change_batch_error`] has no route to
    /// it.
    WriteQueue,
    /// `/control/ingest`'s admission bound, refused before any thread, queue slot or WAL byte is
    /// spent. `retry_after_s` is [`admission_retry_after_s`].
    IngestAdmission,
    /// A single-flight build (a row projection or a mask fragment) outlasted
    /// `serve.single_flight_wait_ms` after the compute gate admitted the request. The gate's
    /// `shed_total` does not count it. `Retry-After` is [`RETRY_AFTER_SECS`].
    SingleFlight,
    /// A suggestion walk is already in flight for this session.
    SuggestInFlight,
}

impl ShedCause {
    fn detail(self, retry_after_s: u64) -> String {
        match self {
            ShedCause::ComputeGate => {
                "the server is at its compute-admission bound; retry shortly".to_string()
            }
            ShedCause::BulkGate => {
                "the server is at its bulk-read admission bound; retry shortly".to_string()
            }
            ShedCause::WriteQueue => {
                format!("the write queue is full; retry after {retry_after_s}s")
            }
            ShedCause::IngestAdmission => {
                format!("the server is at its ingest-admission bound; retry after {retry_after_s}s")
            }
            ShedCause::SingleFlight => "a concurrent request is already building this session's \
                 row projection or mask fragment; retry shortly"
                .to_string(),
            ShedCause::SuggestInFlight => "a suggestion request for this session is already in \
                 flight; retry shortly"
                .to_string(),
        }
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: &'static str,
    detail: String,
    /// Present only on `backpressure`.
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_after_s: Option<u64>,
}

/// `Retry-After` for a shed that clears on one request's timescale.
pub const RETRY_AFTER_SECS: u64 = 1;

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
            ApiError::Contract(detail) => {
                (StatusCode::UNPROCESSABLE_ENTITY, "contract", detail.clone())
            }
            ApiError::FailClosed(detail) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "fail-closed",
                detail.clone(),
            ),
            ApiError::Backpressure {
                retry_after_s,
                cause,
            } => (
                StatusCode::TOO_MANY_REQUESTS,
                "backpressure",
                cause.detail(*retry_after_s),
            ),
            ApiError::NotReady => (
                StatusCode::SERVICE_UNAVAILABLE,
                "not-ready",
                "this node is not accepting writes; nothing in this request was applied"
                    .to_string(),
            ),
        }
    }

    /// The `Retry-After` header and the body's `retry_after_s`. There is no `_` arm, so a new
    /// variant must say whether it carries one.
    fn retry_after_s(&self) -> Option<u64> {
        match self {
            ApiError::Backpressure { retry_after_s, .. } => Some(*retry_after_s),
            ApiError::BadCredential
            | ApiError::ExpiredToken
            | ApiError::Unknown(_)
            | ApiError::Conflict(_)
            | ApiError::Contract(_)
            | ApiError::FailClosed(_)
            // None on the 503: a number that varied with the executor's posture would disclose it
            // to an unauthenticated caller.
            | ApiError::NotReady => None,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, detail) = self.parts();
        // One source for the header and the body field.
        let retry_after_s = self.retry_after_s();
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

/// `retry_after_s` for [`ShedCause::IngestAdmission`]: one work item's service time, since a
/// permit frees when one handler completes. It under-states the wait, because a permit is also
/// held across decoding, term resolution and the receipt wait, which the executor does not time.
pub fn admission_retry_after_s(stats: &tessera_engine::ExecutorStats) -> u64 {
    tessera_engine::estimate_retry_after_s(1, stats.service_nanos_for_estimate())
}

/// Maps an `EngineError` to an API error. A variant not named is a fail-closed 500.
pub fn map_engine_error(e: EngineError) -> ApiError {
    match e {
        EngineError::UnknownView(view) => ApiError::Unknown(format!("unknown view '{view}'")),
        // The caller can ask for less; the detail names only its numbers and the configured bounds.
        EngineError::UnderlayRefused(detail) => ApiError::Contract(detail),
        // The detail names columns and families, which `/v1/meta` publishes to every principal.
        // `FilterRefused` is an unreadable artefact and stays a 500.
        EngineError::FilterMalformed(detail) => ApiError::Contract(detail),
        // The detail names only the field and the value the caller sent.
        EngineError::AddressMalformed(detail) => ApiError::Contract(detail),
        // Each refusal names a layer, a level or a page bound from `/v1/meta`, never an artifact.
        browse @ EngineError::BrowseRefused(_) => ApiError::Contract(browse.to_string()),
        // Each names only a field, a system field or a paging argument the caller sent.
        records @ EngineError::RecordsRefused(_) => ApiError::Contract(records.to_string()),
        // Each names a grouping's shape, a limit from `/v1/meta`, a field or a layer the caller
        // sent.
        aggregate @ EngineError::AggregateRefused(_) => ApiError::Contract(aggregate.to_string()),
        // One fixed detail for every reason a cursor did not open.
        cursor @ EngineError::CursorRefused => ApiError::Contract(cursor.to_string()),
        // The detail names only the caller's numbers and the configured limit.
        too_many @ EngineError::TooManyTiles { .. } => ApiError::Contract(too_many.to_string()),
        // Their text names filesystem paths, so they go through the store sanitiser.
        store_or_io @ (EngineError::Store(_) | EngineError::Io(_)) => map_store_error(store_or_io),
        // Another request is building this session's row projection. Not `ComputeGate`: that gate
        // admitted this request and may be idle.
        EngineError::ProjectionBuilding => ApiError::Backpressure {
            retry_after_s: RETRY_AFTER_SECS,
            cause: ShedCause::SingleFlight,
        },
        // Another `authorise` call is building this credential's mask fragment.
        EngineError::FragmentBuilding => ApiError::Backpressure {
            retry_after_s: RETRY_AFTER_SECS,
            cause: ShedCause::SingleFlight,
        },
        // Unreachable in practice, since cancellation fires only when the handler future is
        // dropped. A 500 with a fixed detail in case it ever arrives on a live connection.
        EngineError::Cancelled => {
            ApiError::FailClosed("the request was cancelled before it completed".to_string())
        }
        // A 500, not an empty 200: an empty value set is a real answer for a principal who may see
        // none of the values, and must stay distinct from one that could not be read. The detail
        // can carry a store failure's text, so only the column is sent.
        EngineError::VocabularyVisibilityUnavailable { column, detail } => {
            tracing::error!(%column, %detail, "a derived column's value visibility could not be read");
            ApiError::FailClosed(format!(
                "column '{column}''s per-viewer value visibility could not be derived, so its \
                 values are refused"
            ))
        }
        // A 500 for the same reason: an empty page is a real answer.
        EngineError::SuggestionUnavailable { column, detail } => {
            tracing::error!(%column, %detail, "a column's suggestion index could not be read");
            ApiError::FailClosed(format!("column '{column}' cannot be suggested over"))
        }
        // Everything else is the deployment's fault, and its text can name a path, a segment, a
        // bundle file (`Malformed`, `Wal`, an unreadable filter
        // artefact in `FilterRefused`), so it is logged and never sent.
        other => {
            tracing::error!(detail = %other, "an engine failure; answering fail-closed");
            ApiError::FailClosed("the server could not complete this request".to_string())
        }
    }
}

/// Maps a store read failure to a fail-closed 500. Generic, so this crate never names
/// `StoreError`. The error's text can name a path or an entity, so it is logged and the body is a
/// fixed string.
pub fn map_store_error<E: std::fmt::Display>(e: E) -> ApiError {
    tracing::error!(detail = %e, "a bundle read failed; answering fail-closed");
    ApiError::FailClosed(
        "could not read this bundle's stored data".to_string(),
    )
}

/// Maps a WAL append or fsync failure to a fail-closed 500. The OS error's text can name a path,
/// so it is logged and never sent.
pub fn map_wal_error<E: std::fmt::Display>(e: E) -> ApiError {
    tracing::error!(detail = %e, "wal append/fsync failed; answering fail-closed");
    ApiError::FailClosed(
        "a durability write failed".to_string(),
    )
}

/// Maps one write-executor outcome to its answer; every variant is named. A WAL failure's 500
/// says a delete or suppress may be in force, since those are applied anyway. For a
/// `/control/changes` batch, use [`map_change_batch_error`].
pub fn map_accept_error(e: tessera_engine::AcceptError) -> ApiError {
    use tessera_engine::AcceptError;
    use tessera_lifecycle::{ExecError, SubmitError};

    match e {
        AcceptError::Submit(SubmitError::QueueFull { retry_after_s }) => {
            // `debug!`: this is a routine shed, and `work_depth` on `/control/status` is the
            // operator's signal.
            tracing::debug!("the ingest work queue is full; answering 429 backpressure");
            ApiError::Backpressure {
                retry_after_s,
                cause: ShedCause::WriteQueue,
            }
        }
        AcceptError::Submit(SubmitError::ExecutorDead) => {
            tracing::error!("the write executor is not running; answering 503 not-ready");
            ApiError::NotReady
        }
        AcceptError::Submit(SubmitError::ReceiptLost) => {
            tracing::error!(
                "ALARM: the write executor died holding a command; its disposition is UNKNOWN and \
                 it may be in force — answering fail-closed, never not-ready"
            );
            ApiError::FailClosed(
                "the write executor stopped while holding this request; it may have been applied \
                 in full, so do not treat this as a no-op"
                    .to_string(),
            )
        }
        AcceptError::SteppedDown => {
            // A stepped-down node must not accept rows that a flush would bury under a manifest
            // built from older state. A 503, since the node is wrong and not the request.
            tracing::error!(
                "ALARM: ingest refused — a partition is serving a stepped-down side-manifest; \
                 repair or restore the damaged newest manifest's files"
            );
            ApiError::NotReady
        }
        AcceptError::Exec(ExecError::BatchConflict { batch_id }) => ApiError::Conflict(format!(
            "batch id '{batch_id}' was already accepted with a different body"
        )),
        AcceptError::Exec(e @ (ExecError::Wal(_) | ExecError::Alloc(_))) => {
            tracing::error!(detail = %e, "the write executor refused; answering fail-closed");
            ApiError::FailClosed(
                "the write could not be completed durably; a deletion or suppression in this \
                 request may nonetheless be in force, so do not treat this as a no-op"
                    .to_string(),
            )
        }
        // Forwarded: the detail names a vocabulary and its declared width. Widening the code space
        // instead would recolour every coded row.
        AcceptError::Exec(ExecError::VocabularyRefused { detail }) => ApiError::Contract(detail),
        // The detail names the caller's declaration against the published rules. Refused before any
        // allocation or WAL append, so nothing took effect.
        AcceptError::Exec(ExecError::LayerRefused { detail }) => ApiError::Contract(detail),
        // A fixed part held with a different value; a retry with the same bytes answers the same.
        // The detail never names the held value.
        AcceptError::Exec(ExecError::PartConflict { detail }) => ApiError::Conflict(detail),
        // A refused record is corrected; a taken or burnt key is replaced; an unknown group or key
        // is the same 404 as an unknown view everywhere else.
        AcceptError::Exec(ExecError::ViewRefused { detail }) => ApiError::Contract(detail),
        AcceptError::Exec(ExecError::ViewConflict { detail }) => ApiError::Conflict(detail),
        AcceptError::Exec(ExecError::ViewUnknown { detail }) => ApiError::Unknown(detail),
        // A refused declaration is corrected and resent. A name held under another identity cannot
        // be had, since a column's width is baked into every row. Neither took effect.
        AcceptError::Exec(ExecError::AttributeRefused { detail }) => ApiError::Contract(detail),
        AcceptError::Exec(ExecError::AttributeConflict { detail }) => ApiError::Conflict(detail),
        // A cell held differently is a 409 whose detail never names the held value. A row naming a
        // missing subject or layer key is a 422 the caller fixes and resends. Neither took effect.
        // A vocabulary, or a value's title, held under another identity; as for attributes above.
        AcceptError::Exec(ExecError::VocabularyConflict { detail }) => ApiError::Conflict(detail),
        // The detail names the values and the holders' `tessera_id`s, never an entity id.
        AcceptError::Exec(ExecError::UniqueTaken { detail }) => ApiError::Conflict(detail),
        // Reached only where the handler's one re-check was stale too.
        AcceptError::Exec(e @ ExecError::Stale) => ApiError::Conflict(e.to_string()),
        // The detail names rows by position, values as sent and items by `tessera_id`.
        AcceptError::Conflict(detail) => ApiError::Conflict(detail),
        AcceptError::Contract(detail) => ApiError::Contract(detail),
        // 404, as an unknown view is on every plane.
        AcceptError::UnknownView { view, .. } => {
            ApiError::Unknown(format!("unknown view '{view}'"))
        }
        AcceptError::Unreadable(detail) => {
            tracing::error!(%detail, "a unique index could not be read; answering fail-closed");
            ApiError::FailClosed(
                "a unique column's index could not be read, so the batch was not applied"
                    .to_string(),
            )
        }
        // The detail names the caller's row and the declared extent or arity, so it is sent.
        e @ (AcceptError::OutsideExtent { .. } | AcceptError::ScalarArity { .. }) => {
            ApiError::Contract(e.to_string())
        }
    }
}

/// The answer for a `/control/changes` request that failed: a 503 where it never reached the
/// executor, and otherwise a 500, since the request is one record and may be in force. Never a
/// 429, since `/control/changes` is never shed. The detail says whether a change may be in force:
/// a lost receipt may have completed in full, and a failed log write applies every deletion and
/// suppression in the request anyway.
pub fn map_change_batch_error(
    ops: &[tessera_lifecycle::ChangeOp],
    failure: tessera_engine::AcceptError,
) -> ApiError {
    let Some(may_be_in_force) = change_request_in_force(ops, &failure) else {
        tracing::error!(detail = %failure, "a change request did not reach the write executor; answering 503 not-ready");
        return ApiError::NotReady;
    };
    tracing::error!(
        detail = %failure,
        may_be_in_force,
        "a change request did not complete; answering fail-closed"
    );
    let mut detail = String::from("this change request did not complete");
    if may_be_in_force {
        detail.push_str(
            "; a deletion or suppression in it may be in force, so do not treat this as a no-op",
        );
    }
    detail.push_str("; send the whole request again");
    ApiError::FailClosed(detail)
}

/// Whether a failed change request may be in force, or `None` where it never reached the
/// executor. A lost receipt may have completed in full. A failed log write applies every deletion
/// and suppression anyway, and no unsuppression: applied without durability, one would re-expose
/// an item replay still hides.
fn change_request_in_force(
    ops: &[tessera_lifecycle::ChangeOp],
    failure: &tessera_engine::AcceptError,
) -> Option<bool> {
    use tessera_engine::AcceptError;
    use tessera_lifecycle::{ChangeOp, ExecError};
    Some(match failure {
        AcceptError::Submit(e) => {
            if !e.may_have_taken_effect() {
                return None;
            }
            true
        }
        AcceptError::Exec(ExecError::Wal(_)) => ops
            .iter()
            .any(|op| matches!(op, ChangeOp::Delete | ChangeOp::Suppress)),
        _ => false,
    })
}

/// Maps a panicked `spawn_blocking` closure to a fail-closed 500 rather than an empty body or a
/// dropped connection. The panic payload can carry a path or an entity id, so it is logged and
/// never sent.
pub fn map_join_error(e: tokio::task::JoinError) -> ApiError {
    tracing::error!(detail = %e, "spawn_blocking closure panicked; answering fail-closed");
    ApiError::FailClosed(
        "an internal error occurred while handling this request".to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use tessera_engine::AcceptError;
    use tessera_lifecycle::{ChangeOp, ExecError, SubmitError};

    fn wal_failure() -> AcceptError {
        AcceptError::Exec(ExecError::Wal(tessera_lifecycle::wal::WalError::Io(
            std::io::Error::other("no space left on device"),
        )))
    }

    /// A full ingest queue is a 429 carrying the queue's own `Retry-After`. The `7` is a value the
    /// compute gate's fixed arm cannot produce.
    #[test]
    fn queue_full_is_429_with_retry_after() {
        let response = map_accept_error(AcceptError::Submit(SubmitError::QueueFull {
            retry_after_s: 7,
        }))
        .into_response();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
            Some("7"),
            "the queue's own retry interval must reach the header, not the compute gate's fixed 1"
        );

        let (status, code, _) = map_accept_error(AcceptError::Submit(SubmitError::QueueFull {
            retry_after_s: 7,
        }))
        .parts();
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(code, "backpressure");
    }

    /// The body's `retry_after_s` agrees with the header.
    #[test]
    fn write_backpressure_body_carries_the_same_retry_after_as_the_header() {
        let body = ErrorBody {
            error: "backpressure",
            detail: String::new(),
            retry_after_s: ApiError::Backpressure {
                retry_after_s: 7,
                cause: ShedCause::WriteQueue,
            }
            .retry_after_s(),
        };
        let json = serde_json::to_string(&body).unwrap();
        assert!(
            json.contains("\"retry_after_s\":7"),
            "body must carry the queue's own interval, got: {json}"
        );
    }

    /// An executor that died before it was handed the command is a 503.
    #[test]
    fn a_dead_executor_is_503_not_ready() {
        let (status, code, _) =
            map_accept_error(AcceptError::Submit(SubmitError::ExecutorDead)).parts();
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(code, "not-ready");
    }

    /// A receipt lost to a dying executor is a 500: the command may already be durable and in
    /// force.
    #[test]
    fn a_lost_receipt_is_500_not_503_and_says_it_may_be_in_force() {
        let (status, code, _) =
            map_accept_error(AcceptError::Submit(SubmitError::ReceiptLost)).parts();
        assert_eq!(
            status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "a command the executor died HOLDING may be fully applied; 503 'not-ready' asserts the \
             node took nothing, which is the fail-open"
        );
        assert_eq!(code, "fail-closed");
    }

    /// `may_have_taken_effect` decides the fold's 503 arm.
    #[test]
    fn only_a_lost_receipt_may_have_taken_effect() {
        assert!(!SubmitError::ExecutorDead.may_have_taken_effect());
        assert!(!SubmitError::QueueFull { retry_after_s: 1 }.may_have_taken_effect());
        assert!(SubmitError::ReceiptLost.may_have_taken_effect());
    }

    /// A request that reached nothing is the only shape that may answer 503, and a queue-full
    /// refusal, which the deny lane cannot produce, is not a 429.
    #[test]
    fn a_change_request_that_reached_nothing_is_503_never_429() {
        for e in [SubmitError::ExecutorDead, SubmitError::QueueFull { retry_after_s: 1 }] {
            let (status, code, _) =
                map_change_batch_error(&[ChangeOp::Suppress], AcceptError::Submit(e)).parts();
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(code, "not-ready");
        }
    }

    /// A lost receipt, or a failed log write under a deletion or suppression, may be in force and
    /// is a 500. A failed log write under unsuppressions alone applied nothing, and is a 500 too.
    #[test]
    fn a_change_request_that_may_be_in_force_is_500() {
        let lost = AcceptError::Submit(SubmitError::ReceiptLost);
        assert_eq!(change_request_in_force(&[ChangeOp::Unsuppress], &lost), Some(true));
        let both = [ChangeOp::Unsuppress, ChangeOp::Delete];
        assert_eq!(change_request_in_force(&both, &wal_failure()), Some(true));
        assert_eq!(
            change_request_in_force(&[ChangeOp::Unsuppress], &wal_failure()),
            Some(false)
        );
        for failure in [lost, wal_failure()] {
            let (status, code, _) = map_change_batch_error(&both, failure).parts();
            assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
            assert_eq!(code, "fail-closed");
        }
    }

    /// A WAL error's text, which can name a path, never reaches the body.
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

    /// A store error's text, which can name a path and an entity, never reaches the body.
    #[test]
    fn map_store_error_does_not_forward_the_detail_to_the_caller() {
        let leaky = "invalid run at /srv/tessera/v00000/partitions/default/entities/\
                     unique/doc/base-0.keys: entity 123456 is inconsistent";
        let (status, code, detail) = map_store_error(leaky).parts();
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(code, "fail-closed");
        assert!(
            !detail.contains("123456") && !detail.contains('/'),
            "the body must carry neither an identifier nor a server path, got: {detail}"
        );
    }

    /// A panic's payload, which can name a path and an entity id, never reaches the body.
    #[tokio::test]
    async fn map_join_error_does_not_forward_the_detail_to_the_caller() {
        let join_error = tokio::spawn(async {
            panic!(
                "invalid run at /srv/tessera/v00000/partitions/default/entities/\
                 unique/doc/base-0.keys: entity 123456 is inconsistent"
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

    /// `ProjectionBuilding` is a 429 attributed to the single-flight wait, not the compute gate.
    #[test]
    fn map_engine_error_takes_projection_building_to_backpressure() {
        assert_single_flight_429(map_engine_error(EngineError::ProjectionBuilding));
    }

    /// `FragmentBuilding` is the same 429.
    #[test]
    fn map_engine_error_takes_fragment_building_to_backpressure() {
        assert_single_flight_429(map_engine_error(EngineError::FragmentBuilding));
    }

    fn assert_single_flight_429(e: ApiError) {
        assert!(
            matches!(e, ApiError::Backpressure { cause: ShedCause::SingleFlight, .. }),
            "a single-flight shed must not be reported as the compute gate's, got {e:?}"
        );
        let response = e.into_response();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
            Some("1")
        );
    }

    /// A refused underlay is a 422: the caller can ask for less.
    #[test]
    fn map_engine_error_takes_a_refused_underlay_to_422_contract() {
        let refused = EngineError::UnderlayRefused(
            "offset 5 is above the configured maximum of 4".to_string(),
        );
        let (status, code, _) = map_engine_error(refused).parts();
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(code, "contract");
    }

    /// Too many tiles is a 422: the caller can narrow the bbox.
    #[test]
    fn map_engine_error_takes_too_many_tiles_to_422_contract() {
        let (status, code, _) = map_engine_error(EngineError::TooManyTiles {
            demanded: 4_294_967_296,
            limit: 262_144,
        })
        .parts();
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(code, "contract");
    }

    /// `Cancelled` is a fail-closed 500, never a 2xx or a 4xx.
    #[test]
    fn map_engine_error_takes_cancelled_to_fail_closed_500() {
        let (status, code, _) = map_engine_error(EngineError::Cancelled).parts();
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(code, "fail-closed");
    }

    /// The compute gate's 429 carries `Retry-After: 1`.
    #[test]
    fn backpressure_carries_retry_after_header_and_body_field() {
        let response = ApiError::Backpressure {
            retry_after_s: RETRY_AFTER_SECS,
            cause: ShedCause::ComputeGate,
        }
        .into_response();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
            Some("1")
        );
    }

    /// An engine failure whose text is the deployment's answers a 500 whose body carries none of
    /// that text: a bundle file, a filter artefact and a derived column's postings.
    #[test]
    fn engine_failures_send_none_of_their_internal_text() {
        let secret = "/srv/bundles/p0/seg-0007/records.blob entity 144999";
        for e in [
            EngineError::Malformed(secret.to_string()),
            EngineError::FilterRefused(secret.to_string()),
            EngineError::Io(std::io::Error::other(secret)),
            EngineError::SegmentWithoutRowBase {
                view: "s0".to_string(),
                seg_id: secret.to_string(),
            },
            EngineError::VocabularyVisibilityUnavailable {
                column: "archive".to_string(),
                detail: secret.to_string(),
            },
            EngineError::SuggestionUnavailable {
                column: "archive".to_string(),
                detail: secret.to_string(),
            },
        ] {
            let (status, code, detail) = map_engine_error(e).parts();
            assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
            assert_eq!(code, "fail-closed");
            for fragment in ["/srv", "seg-0007", "records.blob", "144999"] {
                assert!(!detail.contains(fragment), "{detail:?} carries {fragment:?}");
            }
        }
    }

    /// A records refusal and a refused cursor are the caller's to correct: 422s.
    #[test]
    fn records_and_cursor_refusals_are_contract_refusals() {
        for e in [
            EngineError::RecordsRefused(tessera_engine::RecordsRefused::ZeroPageRows),
            EngineError::CursorRefused,
        ] {
            let (status, code, _) = map_engine_error(e).parts();
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
            assert_eq!(code, "contract");
        }
    }

    /// Every other error body omits `retry_after_s`.
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
