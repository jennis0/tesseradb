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
    /// 429 `backpressure` for the **write** path: the bounded ingest work queue was full
    /// (`SubmitError::QueueFull`; contracts §3.1's 429 row names ingest first).
    ///
    /// **A second variant for one wire code, and the wire code is what is closed** — contracts
    /// §3.1 lists `backpressure` once and both variants emit it. The split is here because the two
    /// 429s answer different questions about *when to come back*, and one variant cannot carry
    /// both docs truthfully: [`ApiError::Backpressure`] argues `Retry-After: 1` as fixed on the
    /// grounds that a compute-admission saturation clears on one request's timescale, which is not
    /// true of a write queue draining at fsync timescale.
    ///
    /// **`retry_after_s` is carried, not derived — and its only producer hard-codes `1` today**
    /// (`LifecycleHandle::submit`, where the field is labelled a placeholder). Until Task 6 derives
    /// it from window age and observed drain rate, this variant and `Backpressure` are
    /// byte-identical on the wire. The plumbing is what lands now; calling it "derived" before then
    /// would be a doc claiming a property the code does not have.
    ///
    /// **Unreachable from `/control/changes`, twice over.** A `Command::Change` goes to the
    /// unbounded deny lane by `Command::is_never_shed`, so it cannot produce `QueueFull`; and
    /// [`map_change_batch_error`] contains no route from that lane to this variant at all, because
    /// contracts §3.1 says `/control/changes` is **never** load-shed and a structural absence
    /// survives an edit to `is_never_shed` that a comment would not.
    WriteBackpressure { retry_after_s: u64 },
    /// 503 `not-ready`: contracts §3.1's row — "unverified bundle, unready worker, unloaded
    /// plugin". Reached when the write executor was never started, or is gone **without having
    /// been handed the command** (`SubmitError::ExecutorDead`).
    ///
    /// **Its meaning is "this node did not take your write", which is exactly why it is not the
    /// answer for a lost receipt.** `SubmitError::ReceiptLost` — the executor died *holding* the
    /// job — maps to the fail-closed 500 instead, because the command may be fully applied and
    /// in force. Answering 503 there would tell an operator nothing happened while a suppression
    /// was live, which is the fail-open this table exists to avoid.
    ///
    /// A unit variant with a fixed detail, like [`ApiError::PinExpired`]: there is one thing to
    /// say, and a parameterised detail would invite a future handler into distinguishing executor
    /// postures for a caller. The posture belongs on the bearer-gated `/control/status` (SA §9);
    /// `/readyz`, which answers the same question unauthenticated, is a bare status with no body.
    NotReady,
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
            ApiError::WriteBackpressure { retry_after_s } => (
                StatusCode::TOO_MANY_REQUESTS,
                "backpressure",
                format!(
                    "the write queue is full; retry after {retry_after_s}s. Deny-disposition \
                     changes are never shed for load and are unaffected"
                ),
            ),
            ApiError::NotReady => (
                StatusCode::SERVICE_UNAVAILABLE,
                "not-ready",
                "this node is not accepting writes; nothing in this request was applied"
                    .to_string(),
            ),
        }
    }

    /// The `Retry-After` header and the body's `retry_after_s`, which must always agree.
    ///
    /// **Exhaustive, with no `_` arm, and that is the point.** This was
    /// `matches!(self, ApiError::Backpressure).then_some(RETRY_AFTER_SECS)` — correct while there
    /// was one 429, and silently wrong the moment a second arrived: a new retryable variant would
    /// have shipped a 429 carrying neither the header nor the body field, with nothing failing to
    /// compile and the existing header test staying green over the *other* variant. Naming every
    /// variant makes a new one a compile error here, which is the same discipline
    /// [`map_accept_error`] applies to its own table.
    fn retry_after_s(&self) -> Option<u64> {
        match self {
            // D-E: fixed at one second, never a knob — see the variant's doc.
            ApiError::Backpressure => Some(RETRY_AFTER_SECS),
            ApiError::WriteBackpressure { retry_after_s } => Some(*retry_after_s),
            ApiError::BadCredential
            | ApiError::ExpiredToken
            | ApiError::Unknown(_)
            | ApiError::Conflict(_)
            | ApiError::PinExpired
            | ApiError::Contract(_)
            | ApiError::FailClosed(_)
            // Deliberately none: a `Retry-After` on the 503 would hand an unauthenticated caller a
            // number that varies with the executor's posture, which is the disclosure `/readyz`'s
            // bare-boolean shape exists to prevent. An operator learns the posture from
            // `/control/status`.
            | ApiError::NotReady => None,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, detail) = self.parts();
        // One source for the header and the body field, so a caller reading either agrees with the
        // other. Every non-retryable error body stays byte-identical, since
        // `ErrorBody::retry_after_s` is `skip_serializing_if` `None`.
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

/// Map a **single** write-executor outcome to its HTTP answer (contracts §3.1's closed code list).
///
/// | outcome | status | why |
/// |---|---|---|
/// | `Submit(QueueFull)` | 429 `backpressure` | the queue's own `retry_after_s`, not the gate's fixed 1 |
/// | `Submit(ExecutorDead)` | 503 `not-ready` | non-enqueue is **proven**, so "nothing happened" is true |
/// | `Submit(ReceiptLost)` | 500 `fail-closed` | enqueued; may be fully applied |
/// | `Exec(BatchConflict)` | 409 `conflict` | contract answer, no effect |
/// | `Exec(DuplicateExternalId)` | 409 `conflict` | contract answer, no effect |
/// | `Exec(Wal)` | 500 `fail-closed` | for a deny: **in force, not durable** (lifecycle §4) |
/// | `Exec(Alloc)` | 500 `fail-closed` | I9's ceiling; the batch has no effect |
///
/// **Every arm is named — there is no `other =>` over `AcceptError`'s own variants.** Adding one is
/// then a compile error here rather than a silent 500, which is the difference between a mapping
/// table and a fallback. (The inner `WalError`/`AllocError` families stay behind one arm each;
/// those *are* families whose members share a status.)
///
/// **Why `ExecutorDead` and `ReceiptLost` cannot share a status.** 503 `not-ready` asserts "this
/// node did not take your write". That is provable for `ExecutorDead` — every producer is a `send`
/// that failed, and a failed `send` returns the job — and false for `ReceiptLost`, where the
/// executor died holding a command it may have appended, fsynced, applied and swapped. A single
/// 503 over both would report an in-force suppression as a no-op. Found by four independent
/// reviewers at the Task 3b design gate; the variants were split at the source rather than
/// papered over here.
///
/// **The detail never reaches the caller** — the same rule as [`map_store_error`]. `ExecError`'s
/// `Display` composes `WalError`'s, which carries the WAL's filesystem path and the OS error
/// string; `map_wal_error_does_not_forward_the_detail_to_the_caller` is the standing regression
/// test for exactly that door.
///
/// **The 500 body does not say "refused".** [`map_wal_error`]'s wording — "the request was refused
/// rather than answered partially" — is false of the case that reaches here most often: a
/// `Delete`/`Suppress` whose WAL append failed is applied *anyway* (lifecycle §4), so the 500
/// accompanies an effect that is in force. Telling an operator the request was refused invites them
/// to retry a suppression that has already taken hold, and to believe the item is still visible
/// when it is not.
///
/// For a `/control/changes` **batch**, use [`map_change_batch_error`]: one item's answer is not the
/// batch's.
pub fn map_accept_error(e: tessera_engine::AcceptError) -> ApiError {
    use tessera_engine::AcceptError;
    use tessera_lifecycle::{ExecError, SubmitError};

    match e {
        AcceptError::Submit(SubmitError::QueueFull { retry_after_s }) => {
            // Deliberately not `tracing::error!`: once the queue bound bites, this is a routine,
            // expected shed and an ERROR per occurrence is an alarm flood, not a signal. Task 6
            // makes it routine; the level is set here so it is right when it does.
            tracing::warn!("the ingest work queue is full; answering 429 backpressure");
            ApiError::WriteBackpressure { retry_after_s }
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
        AcceptError::Exec(ExecError::BatchConflict { batch_id }) => ApiError::Conflict(format!(
            "batch id '{batch_id}' was already accepted with a different body"
        )),
        AcceptError::Exec(ExecError::DuplicateExternalId { count }) => ApiError::Conflict(format!(
            "{count} row(s) name an external id this deployment already knows; the batch had no \
             effect"
        )),
        AcceptError::Exec(e @ (ExecError::Wal(_) | ExecError::Alloc(_))) => {
            tracing::error!(detail = %e, "the write executor refused; answering fail-closed");
            ApiError::FailClosed(
                "the write could not be completed durably; a deletion or suppression in this \
                 request may nonetheless be in force, so do not treat this as a no-op"
                    .to_string(),
            )
        }
    }
}

/// The answer for a whole `/control/changes` batch — a **fold over dispositions**, never one item's
/// status promoted to the batch's.
///
/// # Why a fold at all
///
/// Task 3a made `run_changes` submit every validated item even after one fails, and report the
/// *first* error. That was right about continuing (aborting leaves later denies unapplied while
/// `WalPoisoned` persists) and wrong about reporting: an item's status describes an item.
/// Concretely, and this ordering **is** constructible — item 1's WAL append fails, so its suppress
/// is applied anyway and is in force; the executor then dies, so item 2 is refused outright:
///
/// - first-error reporting answers 500, which happens to be right;
/// - the mirror case — nothing failed at the executor, but one item was refused after an earlier
///   item was **applied successfully** — answers 503 `not-ready`, i.e. "this node did not take your
///   write", for a batch containing a durable, in-force suppression.
///
/// # The rules
///
/// 1. **If anything may have taken effect → 500 `fail-closed`.** Any successful item, any
///    `Exec(_)` failure (it reached the executor; for `Delete`/`Suppress` it was applied anyway),
///    or any `Submit` failure for which non-enqueue is not proven
///    ([`SubmitError::may_have_taken_effect`]). 500 dominates 503 because its body instructs the
///    operator to treat effects as possibly present and durability as owed, where 503's says
///    nothing happened; when the two disagree, the one that over-states the effect is the
///    fail-closed answer.
/// 2. **Otherwise → 503 `not-ready`.** Every item failed and every failure proves non-enqueue.
/// 3. **`QueueFull` never becomes a 429 here.** It is unreachable — `Command::is_never_shed` routes
///    a change to the unbounded lane — but the *absence of a route* is what survives an edit to
///    `is_never_shed`, where a comment would not. Contracts §3.1: `/control/changes` is **never**
///    load-shed.
///
/// # The body carries both halves
///
/// A 500 that says only "some of this may be in force" is not the statement an operator needs when
/// part of the batch was **definitely not applied** — they must re-submit, and they must not assume
/// the un-acked suppressions took hold. Contracts §3.1's body shape is closed
/// (`{error, detail, retry_after_s?}`), so `detail` is the only channel available and it says both.
///
/// Returns `None` for a batch with no failures: a fold that manufactured an error for a wholly
/// successful batch would be a 500 on the success path, and making that unrepresentable is cheaper
/// than documenting a precondition.
pub fn map_change_batch_error(
    failures: &[tessera_engine::AcceptError],
    applied: usize,
) -> Option<ApiError> {
    use tessera_engine::AcceptError;

    if failures.is_empty() {
        return None;
    }

    // All three quantities are derived here, never passed in: a caller computing them itself is one
    // forgotten variant away from turning a partially-applied batch into a 503, which is the exact
    // fail-open this function exists to close — reintroduced in its own signature.

    // Rule 2's condition, and it is deliberately the *narrowest* of the three: 503 asserts the node
    // took nothing at all, so anything that reached the executor — including a 409-class refusal,
    // which is a contract answer and not a readiness fault — disqualifies it.
    let reached_executor = applied > 0
        || failures.iter().any(|f| match f {
            AcceptError::Exec(_) => true,
            AcceptError::Submit(e) => e.may_have_taken_effect(),
        });
    if !reached_executor {
        tracing::error!(
            failures = failures.len(),
            "no item in this change batch reached the write executor; answering 503 not-ready"
        );
        return Some(ApiError::NotReady);
    }

    let may_be_in_force = applied > 0
        || failures.iter().any(|f| match f {
            AcceptError::Exec(e) => exec_failure_may_be_in_force(e),
            AcceptError::Submit(e) => e.may_have_taken_effect(),
        });
    let some_not_applied = failures.iter().any(|f| match f {
        AcceptError::Exec(e) => !exec_failure_may_be_in_force(e),
        AcceptError::Submit(e) => !e.may_have_taken_effect(),
    });

    tracing::error!(
        failures = failures.len(),
        applied,
        may_be_in_force,
        some_not_applied,
        "a change batch did not complete; answering fail-closed"
    );

    let mut detail = String::from("this change request was attempted in full and did not complete");
    if may_be_in_force {
        detail.push_str(
            "; a deletion or suppression in it may be in force but is not durable, so do not treat \
             this as a no-op",
        );
    }
    if some_not_applied {
        detail.push_str(
            ". At least one item was NOT applied — re-submit the whole request, and do not assume \
             its deny-disposition changes took hold",
        );
    }
    Some(ApiError::FailClosed(detail))
}

/// Whether an executed-and-failed change may nonetheless be in force.
///
/// Only `Wal` can be: lifecycle §4 applies a `Delete`/`Suppress` whose append failed **anyway**.
/// The two 409-class variants are contract answers with no effect by definition (contracts §3.1:
/// "a 409 batch had **no effect**"), and `Alloc` leaves the high-water unchanged.
///
/// Named rather than folded into a `matches!(.., Exec(_))`, because the four variants genuinely
/// differ and a wildcard here would over-report — and because Task 8 may make a change 409-able,
/// at which point this function is where that lands rather than a silent widening.
fn exec_failure_may_be_in_force(e: &tessera_lifecycle::ExecError) -> bool {
    use tessera_lifecycle::ExecError;
    match e {
        ExecError::Wal(_) => true,
        ExecError::BatchConflict { .. } | ExecError::DuplicateExternalId { .. } => false,
        ExecError::Alloc(_) => false,
    }
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

    use tessera_engine::AcceptError;
    use tessera_lifecycle::{ExecError, SubmitError};

    fn wal_failure() -> AcceptError {
        AcceptError::Exec(ExecError::Wal(tessera_lifecycle::wal::WalError::Io(
            std::io::Error::other("no space left on device"),
        )))
    }

    /// Task 3b's first fix, and a live defect rather than a tidy-up: Task 3a shipped a real bounded
    /// work queue while `map_accept_error` sent `QueueFull` to the fail-closed **500** arm, where
    /// contracts §3.1 says **429** with `Retry-After`. Phase 1 had no bound at all, so 3a is what
    /// opened the window.
    ///
    /// **The `7` is load-bearing.** `ApiError::Backpressure` sits next door with a hard-coded
    /// `Retry-After: 1`, so a build that routed write backpressure through *it* would pass any
    /// version of this test written with `1`. Asserting a value the fixed arm cannot produce is
    /// what distinguishes "the status is right" from "the queue's own interval reaches the caller"
    /// — which is the half Task 6 will start varying.
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

    /// The body's `retry_after_s` must agree with the header — one source, asserted on the new
    /// variant because `retry_after_s()`'s exhaustive match is what stops a future 429 variant
    /// shipping with neither.
    #[test]
    fn write_backpressure_body_carries_the_same_retry_after_as_the_header() {
        let body = ErrorBody {
            error: "backpressure",
            detail: String::new(),
            retry_after_s: ApiError::WriteBackpressure { retry_after_s: 7 }.retry_after_s(),
        };
        let json = serde_json::to_string(&body).unwrap();
        assert!(
            json.contains("\"retry_after_s\":7"),
            "body must carry the queue's own interval, got: {json}"
        );
    }

    /// A dead executor that was never handed the command is contracts §3.1's `not-ready` row.
    #[test]
    fn a_dead_executor_is_503_not_ready() {
        let (status, code, detail) =
            map_accept_error(AcceptError::Submit(SubmitError::ExecutorDead)).parts();
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(code, "not-ready");
        assert!(
            detail.contains("nothing in this request was applied"),
            "503's whole meaning is that the node took nothing; got: {detail}"
        );
    }

    /// **The unanimous CRITICAL from the design gate.** A receipt lost to a dying executor must
    /// never be 503: the executor's sequence is `append → fsync → apply → swap → ack`, so a death
    /// after the swap leaves a durable, in-force suppression with no receipt. 503 would tell an
    /// operator nothing happened while the item was already hidden.
    ///
    /// The mutation this kills is the one the design originally specified: map `ReceiptLost` to
    /// `NotReady` alongside `ExecutorDead`.
    #[test]
    fn a_lost_receipt_is_500_not_503_and_says_it_may_be_in_force() {
        let (status, code, detail) =
            map_accept_error(AcceptError::Submit(SubmitError::ReceiptLost)).parts();
        assert_eq!(
            status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "a command the executor died HOLDING may be fully applied; 503 'not-ready' asserts the \
             node took nothing, which is the fail-open"
        );
        assert_eq!(code, "fail-closed");
        assert!(
            detail.contains("do not treat this as a no-op"),
            "the operator must be told the effect may be present; got: {detail}"
        );
    }

    /// `may_have_taken_effect` is the whole basis of the fold's 503 arm, so pin the classification
    /// rather than trusting the variant names.
    #[test]
    fn only_a_lost_receipt_may_have_taken_effect() {
        assert!(!SubmitError::ExecutorDead.may_have_taken_effect());
        assert!(!SubmitError::QueueFull { retry_after_s: 1 }.may_have_taken_effect());
        assert!(SubmitError::ReceiptLost.may_have_taken_effect());
    }

    /// A batch where nothing reached the executor is the only shape that may answer 503.
    #[test]
    fn a_change_batch_that_reached_nothing_is_503() {
        let failures = vec![
            AcceptError::Submit(SubmitError::ExecutorDead),
            AcceptError::Submit(SubmitError::ExecutorDead),
        ];
        let (status, code, _) = map_change_batch_error(&failures, 0).unwrap().parts();
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(code, "not-ready");
    }

    /// **The constructible partial-application case, and the one first-error reporting gets wrong.**
    /// Item 1 succeeds — durable and in force. The executor then dies, so item 2 is refused with
    /// nothing attempted. First-error reporting sees only item 2 and answers **503 "this node did
    /// not take your write"** for a batch containing a live suppression.
    ///
    /// The body must carry **both** halves: something may be in force, and something was definitely
    /// not applied. One of those alone is not the statement an operator can act on.
    #[test]
    fn a_partially_applied_change_batch_is_500_not_503() {
        let failures = vec![AcceptError::Submit(SubmitError::ExecutorDead)];
        let (status, code, detail) = map_change_batch_error(&failures, 1).unwrap().parts();
        assert_eq!(
            status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "one item applied durably; 503 would report the batch as a no-op"
        );
        assert_eq!(code, "fail-closed");
        assert!(
            detail.contains("may be in force"),
            "the applied half must be reported; got: {detail}"
        );
        assert!(
            detail.contains("NOT applied"),
            "the un-applied half must be reported too, or the operator cannot know to re-submit; \
             got: {detail}"
        );
    }

    /// The other constructible mixed order: a WAL failure (applied anyway, not durable) followed by
    /// a refusal after the executor dies. 500, and both halves again.
    #[test]
    fn a_wal_failure_then_a_dead_executor_is_500_with_both_halves() {
        let failures = vec![
            wal_failure(),
            AcceptError::Submit(SubmitError::ExecutorDead),
        ];
        let (status, _, detail) = map_change_batch_error(&failures, 0).unwrap().parts();
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(detail.contains("may be in force"), "got: {detail}");
        assert!(detail.contains("NOT applied"), "got: {detail}");
    }

    /// A lost receipt anywhere in a batch is enough on its own: its disposition is unknown, so the
    /// batch cannot claim the node took nothing.
    #[test]
    fn a_lost_receipt_alone_keeps_the_batch_off_the_503_arm() {
        let failures = vec![AcceptError::Submit(SubmitError::ReceiptLost)];
        let (status, _, _) = map_change_batch_error(&failures, 0).unwrap().parts();
        assert_eq!(
            status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "an enqueued command with no receipt may be fully applied"
        );
    }

    /// Contracts §3.1: `/control/changes` is **never** load-shed. `QueueFull` is unreachable on
    /// that lane (`Command::is_never_shed` routes a change to the unbounded queue), and the fold
    /// contains no route to 429 regardless — the absence is what survives an edit to
    /// `is_never_shed` that a comment would not.
    #[test]
    fn a_change_batch_never_answers_429() {
        let failures = vec![AcceptError::Submit(SubmitError::QueueFull {
            retry_after_s: 1,
        })];
        let (status, code, _) = map_change_batch_error(&failures, 0).unwrap().parts();
        assert_ne!(
            status,
            StatusCode::TOO_MANY_REQUESTS,
            "refusing a security operation for load is fail-open (contracts §3.1)"
        );
        assert_eq!(code, "not-ready");
    }

    /// A fully successful batch has no error to fold. Returning `ApiError` unconditionally made a
    /// 500 representable on the success path; `Option` makes it not.
    #[test]
    fn a_successful_change_batch_folds_to_no_error() {
        assert!(map_change_batch_error(&[], 3).is_none());
    }

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
