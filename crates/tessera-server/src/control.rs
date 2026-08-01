//! The control (admin) plane (R5): `POST /control/ingest`, `POST /control/changes`,
//! `GET /control/status`, plus `/healthz`/`/readyz`. Bearer auth is the operator credential, applied
//! **once, at the router** by [`require_operator_credential`] rather than by each handler — see its
//! doc for what that buys and for the two-path exemption the health probes need.
//!
//! This module owns the **ack contract**: parse -> allocate ids (`assign_sorted`) -> WAL append
//! -> fsync -> apply to buffer/overlay + generation swap -> 200. Never a 200 without fsync. For
//! `delete`/`suppress` changes specifically, the **deny-op append failure** rule (lifecycle §4)
//! applies: if the WAL append/fsync genuinely fails, the change is still applied to the live
//! overlay (the item is hidden immediately) and this returns 500 with an alarm log — durability
//! is owed and the caller must retry, but a refusal that leaves a deny unapplied is fail-open,
//! which is worse than an under-durable deny.

use std::sync::Arc;

use arrow::array::Array;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use rustc_hash::FxHashSet;
use sha2::{Digest, Sha256};

use tessera_engine::AcceptError;
use tessera_lifecycle::{ChangeOp, UnallocatedRow, WalScalar};
use tessera_types::{EntityId, TermId};

use crate::error::{
    map_accept_error, map_change_batch_error, map_join_error, map_store_error, ApiError,
};
use crate::health::{healthz, is_ready, readyz};
use crate::state::AppState;

/// The deny lane's own blocking execution resource (lifecycle §1.3).
///
/// # Why `/control/changes` does not share tokio's blocking pool
///
/// `spawn_blocking` dispatches onto a process-wide **unbounded FIFO** served by at most
/// `max_blocking_threads` threads. An ingest closure holds its thread across the Arrow decode, the
/// plugin's `terms_of_label` loop, the external-ID sidecar IO **and** its whole blocking wait on the
/// executor's receipt, and frees it only when all of that completes — an fsync plus an
/// `IngestBuffer` clone that is O(total buffered items).
///
/// **The original finding (Task 3b), and what Task 6 changed about it.** When this runtime was
/// built, `/control/ingest` was behind *no admission bound at all* and the pool was tokio's
/// undeclared 512, so above ~512 in-flight ingest requests a suppression's closure queued behind
/// ingest closures **inside tokio**, before it could reach the prioritised deny queue — lifecycle
/// §1.3's forbidden shape reintroduced one layer above the priority lane, where the executor cannot
/// see it. Task 6 (D2/D4) closed both of those operands: `ingest_admission` bounds concurrent ingest
/// handlers, and `config::serving_blocking_threads` *derives* the pool as `compute_admission +
/// ingest_admission + BLOCKING_THREAD_RESERVE` rather than inheriting tokio's default. **Every
/// sentence in the paragraph above is therefore false of the shipped binary, and this section
/// records the history rather than the state.**
///
/// The viewer plane was never part of the problem in the same way, and it is worth saying why
/// because the asymmetry is the reason this fix is on the deny side: `ComputeGate::admit` is `async`
/// and is awaited **before** `spawn_blocking`, so a queued viewport holds no blocking thread.
///
/// # Why the separate runtime is still right after D2 — the live argument
///
/// D2 makes the shared pool *sufficient by arithmetic*: the pool covers both admission bounds by
/// construction, so an admitted request can never find no thread. That is a statement about a
/// derived number, and it is exactly the kind of statement this lane must not depend on:
///
/// 1. **The arithmetic holds only where `tessera-cli` builds the runtime.** Embedders,
///    `mount_server`, `spawn_server_from_engine` and every integration test run under an ambient
///    runtime this crate did not size — `config::serving_blocking_threads`'s own doc says so. On
///    those, the pool is whatever the host chose, and the deny lane is the one thing that must not
///    degrade with it.
/// 2. **`BLOCKING_THREAD_RESERVE` is a reserve, not an enumeration.** tokio dispatches its own
///    blocking work (DNS, most visibly) onto the same pool, and nothing in this workspace can
///    enumerate what a future dependency adds. A shared pool is sufficient *on current evidence*;
///    a separate one needs no evidence.
/// 3. **Sufficiency is not isolation.** Even with a thread always available, a deny on the shared
///    pool takes its place in one FIFO behind up to `ingest_admission` closures, each of which is
///    holding a receipt open. Denies are never refused for load (contracts §3.1) and so have no
///    admission control of their own to fall back on; the owner's latency principle (under a minute)
///    is satisfied either way, but the *structural* guarantee that a deny is never queued behind
///    work of unbounded duration is not a property the arithmetic can give.
///
/// CLAUDE.md's "structural, not disciplinary" test, and D2 does not retire it: D2 bounds a resource,
/// this separates one. Deleting this runtime on the ground that ingest is now bounded would trade a
/// structural property for a derived one, on the one lane where that trade is not available.
///
/// # Why it is NOT small
///
/// Isolation comes from the pool being *separate*, not from it being *small*, and making it small
/// would recreate head-of-line blocking inside the never-shed lane itself: `run_changes` submits
/// and awaits each item **individually**, so one caller-sized batch occupies one thread for N
/// sequential fsyncs, and lifecycle §1.3's "queue-front + fsync" bound does **not** cover that (it
/// is a bound over the *executor's* queue, where one entry is one append). So this takes tokio's
/// default blocking bound. Blocking threads are spawned lazily and reaped when idle, so an unused
/// dedicated pool costs nothing at rest.
///
/// # Failure and lifetime
///
/// Built fallibly by [`init_deny_runtime`] from `prepare`, so a runtime that cannot be constructed
/// — `EAGAIN` under precisely the thread exhaustion this exists for — is a **fail-to-start**, not a
/// panic discovered by the first suppression. (A panic in the async handler body is not caught by
/// `map_join_error`; the connection would drop with no status at all, violating I13's "a panic is a
/// failed request, never an empty one".)
///
/// **The stored runtime is never dropped**, because a `OnceLock`'s value outlives every caller. That
/// is not the whole of it, and an earlier revision of this paragraph stopped there: `set` **returns
/// the value back** when it loses a race, so the *loser* of two concurrent
/// [`init_deny_runtime`] calls had a live `Runtime` to dispose of, at a statement inside `changes()`'s
/// async body — `Runtime::drop` blocks, and dropping one on a reactor thread panics with "Cannot
/// drop a runtime in a context where blocking is not allowed". Exactly the I13 shape above, and
/// reproducible on both tokio flavours. See [`discard_losing_runtime`], which is where the loser now
/// goes.
static DENY_RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();

/// Dispose of a runtime that lost the `OnceLock::set` race, **from wherever the loser happens to
/// be** — which is an async context whenever the lazy path in [`spawn_on_deny_lane`] raced.
///
/// `shutdown_background` rather than `drop`: it returns immediately instead of blocking on the
/// worker's shutdown, which is what makes it legal on a reactor thread. A losing runtime has no
/// spawned work at all — it was built two statements ago and never handed to anyone — so there is
/// nothing for the non-waiting shutdown to abandon.
///
/// `std::mem::forget` would also avoid the panic and is what a first reading suggests; it leaks the
/// worker thread instead of stopping it. Bounded (races happen only at first use) but pointless when
/// a non-blocking shutdown exists.
///
/// A named function with a test rather than an inline call, because the property under test is
/// "**this disposal is legal in an async context**", and that is a statement about the disposal, not
/// about the caller.
fn discard_losing_runtime(rt: tokio::runtime::Runtime) {
    rt.shutdown_background();
}

/// Build the deny lane's runtime, once. Called by `crate::prepare` so failure is a startup failure.
///
/// Idempotent: a second call is a no-op, so tests that build an `AppState` directly (without
/// `prepare`) reach the same runtime through [`spawn_on_deny_lane`]'s lazy path.
///
/// **Not race-free at the `get`, and it does not need to be** — the `get` is a fast path, the `set`
/// is the arbiter, and the loser is disposed of by [`discard_losing_runtime`] rather than dropped
/// where it stands. Two racers is not a hypothetical: `mount_server`/`spawn_server_from_engine` do
/// not call `prepare`, so every integration test reaches this through the lazy path, and
/// `tests/http_write.rs` runs several `/control/changes` cases concurrently in one process.
pub fn init_deny_runtime() -> std::io::Result<()> {
    if DENY_RUNTIME.get().is_some() {
        return Ok(());
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        // One worker is enough and its only job is to exist: every unit of work here is a
        // `spawn_blocking` closure, which the blocking pool runs on its own threads. A worker
        // thread rather than `new_current_thread` so nothing depends on whether an undriven
        // current-thread runtime services blocking joins.
        .worker_threads(1)
        // Named so the lane is legible in a thread dump — an operator diagnosing deny latency must
        // be able to tell these apart from tokio's shared pool.
        .thread_name("tessera-deny")
        // See [`DENY_MAX_BLOCKING_THREADS`]: tokio's own default, stated here so it is not an
        // undeclared figure the process's thread demand rests on.
        .max_blocking_threads(DENY_MAX_BLOCKING_THREADS)
        .build()?;
    if let Err(loser) = DENY_RUNTIME.set(rt) {
        discard_losing_runtime(loser);
    }
    Ok(())
}

/// Run one `/control/changes` body on the deny lane.
///
/// **The single route from a handler to that lane, and it exists to be exactly that.** A rule
/// spread across call sites is a rule that gets half-applied by the next rewrite of this file;
/// with one function, `a_deny_does_not_queue_behind_ingest_in_the_blocking_pool` has one body to
/// mutate and the property is a fact about this function rather than about a convention.
fn spawn_on_deny_lane<F, T>(f: F) -> tokio::task::JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    // `prepare` has normally initialised this already; the lazy path is for embedders and for tests
    // that construct an `AppState` without it. A failure here would mean the process cannot spawn
    // threads at all, which `spawn_blocking` could not survive either.
    match DENY_RUNTIME.get() {
        Some(rt) => rt.spawn_blocking(f),
        None => {
            if init_deny_runtime().is_ok() {
                if let Some(rt) = DENY_RUNTIME.get() {
                    return rt.spawn_blocking(f);
                }
            }
            // Last resort rather than a panic on the deny lane: the shared pool is what this
            // function exists to avoid, but running there beats refusing a suppression outright.
            tracing::error!(
                "ALARM: the deny lane's runtime is unavailable; falling back to the shared \
                 blocking pool, where a suppression can queue behind unbounded ingest work"
            );
            tokio::task::spawn_blocking(f)
        }
    }
}

/// The deny runtime's own blocking-pool bound.
///
/// **Stated rather than inherited.** Task 6 made the serving runtime's `max_blocking_threads` a
/// derived, declared number (`config::serving_blocking_threads`); this runtime was still taking
/// tokio's undeclared default, so the process's thread demand would have gone on resting on a
/// figure no line of this repository states — just a different one. 512 is that default, so setting
/// it changes no behaviour; what changes is that a tokio release cannot move it in silence.
///
/// **Deliberately not small**, and [`DENY_RUNTIME`]'s "Why it is NOT small" section is the
/// argument: isolation comes from the pool being *separate*, and `run_changes` submits and awaits
/// each item individually, so one caller-sized batch occupies one thread for N sequential fsyncs.
const DENY_MAX_BLOCKING_THREADS: usize = 512;

pub fn router(state: Arc<AppState>) -> Router {
    // Task 6 (D1): the byte cap, enforced **here** rather than by a `body.len()` check in the
    // handler, and the difference is not stylistic.
    //
    // axum applies a default request-body limit of 2 MiB to the `Bytes` extractor, well under this
    // deployment's `ingest_max_batch_bytes` (16 MiB by default) — so before this layer existed the
    // configured cap could never be the refusal a caller met, and an over-2-MiB batch got a **413**,
    // a status outside contracts §3.1's closed code list. Both halves of that are closed by setting
    // the limit to the configured cap and mapping the rejection ourselves: buffering is bounded at
    // exactly the number the operator set, and the answer is the 422 §3.1's "bounds exceeded" row
    // calls for.
    //
    // The handler takes `Result<Bytes, _>` rather than `Bytes` so the rejection is **mapped** to
    // that 422 instead of escaping as axum's own 413. Until [`require_operator_credential`] existed
    // it carried a second, larger duty — keeping `check_bearer` ahead of the extractor — and that
    // duty has moved to the layer; see the auth layer's doc for what changed and what did not.
    let ingest_route = post(ingest).layer(axum::extract::DefaultBodyLimit::max(
        state.ingest_max_batch_bytes,
    ));
    // Fix round 1 (F6): the same remedy, transferred. `post(changes)` with a bare `Json<..>`
    // extractor answered axum's own **413** — outside contracts §3.1's closed code list — with
    // axum's own body, on the never-shed lane, to an operator submitting ~20 000 suppressions. The
    // limit is stated here rather than inherited so the 422's detail can name a number that is true.
    let changes_route =
        post(changes).layer(axum::extract::DefaultBodyLimit::max(CHANGES_MAX_BODY_BYTES));
    Router::new()
        .route("/control/ingest", ingest_route)
        .route("/control/changes", changes_route)
        .route("/control/status", get(status))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        // **The whole plane's credential check, in one place** — see
        // [`require_operator_credential`]. `Router::layer` rather than a `route_layer` per route:
        // the point of this construction is that a route added below inherits the check without
        // anyone remembering to ask for it, and a `route_layer` per route is the same discipline
        // this replaces, spelled differently.
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&state),
            require_operator_credential,
        ))
        .with_state(state)
}

/// **The control plane's operator-credential gate, at the router rather than in each handler.**
///
/// Until 2026-08-01 each of `ingest`, `changes` and `status` opened with its own
/// `state.check_bearer(bearer_token(&headers), &state.operator_credential)`. That is a rule spread
/// across call sites, which CLAUDE.md's "structural, not disciplinary" test rejects, and it had
/// already failed once on this exact plane: `status`'s doc records that it *previously returned
/// `entity_id_high_water`* — a global, unmasked corpus-size fact — to anyone who could reach the
/// control listener, for no reason other than that the handler did not call `check_bearer`. A
/// handler that forgets is unauthenticated; a handler under this layer cannot forget.
///
/// Three things this buys, in the order they matter:
///
/// 1. **No request body is buffered for an unauthenticated caller.** axum extractors run *inside*
///    the handler service, so with a per-handler check a 16 MiB `/control/ingest` body was resident
///    in full before `check_bearer` ever executed (`ingest_max_batch_bytes`, raised from axum's
///    2 MiB default by Task 6 D1, widening that window 8×). A `tower` layer runs *outside* the
///    extractors: this returns 401 with the body still an unconsumed stream. That is the
///    unauthenticated half of the Task 6 gate's F11 (security IMPORTANT 1 + performance I4), closed
///    in code rather than by deployment posture.
///
///    **What it does NOT close, stated because it is still open.** A caller holding a *valid*
///    operator credential still buffers up to `ingest_max_batch_bytes` per in-flight request, and
///    `axum::serve` still applies no connection or concurrency cap, so the *count* of connections
///    remains unbounded. `config`'s relation 3 caps the size of one window; nothing caps how many
///    there are. That residue is unchanged by this layer and remains an owner item.
/// 2. **A control route cannot be added unauthenticated by omission.** The layer wraps the whole
///    router, so a `.route(..)` added to [`router`] tomorrow is behind the credential the moment it
///    exists. Opting *out* requires editing [`UNAUTHENTICATED_CONTROL_PATHS`], which is a deliberate
///    act with a test over it.
/// 3. **401 ahead of every 422 and 429 is now a property of the router.** `control::ingest`'s doc
///    enumerates that ordering and `backpressure_is_invisible_before_auth` exercised it one handler
///    at a time; the ordering no longer depends on where each handler happens to put its check.
///
/// # The exemption, and why matching is exact
///
/// `/healthz` and `/readyz` are mounted on this same router and **must stay unauthenticated on every
/// listener** (SA §9; `health.rs`). `/readyz` being a bare unauthenticated boolean is the deliberate
/// half of a pair: the posture *string* lives behind the bearer on `/control/status`, which is why
/// this layer must not simply cover everything it is mounted over.
///
/// The comparison is exact equality against the request's raw path. Not a prefix match, not a
/// normalising one: `/healthz/../control/status` and `/healthz/` are not `/healthz`, so they fall
/// through to the credential check. Every way of being *nearly* an exempt path is therefore
/// authenticated, which is the direction a mistake here has to fail in.
///
/// The layer also sits ahead of the router's 404, so an unrouted path on the control listener
/// answers 401 rather than 404 — `near_misses_of_the_exemption_are_authenticated` pins that
/// alongside the near-miss spellings. It is not the reason for this design, but it is the right way
/// round: an unauthenticated caller cannot map the plane's surface by probing.
///
/// # Scope: this plane only
///
/// **Deliberately not applied to the session or viewer routers, and they are not oversights.** They
/// authenticate against different secrets with different exemptions — `session::router` gates on
/// `state.session_credential`, and `viewer::router` on per-session tokens looked up by value in the
/// registry (`AppState::authenticated_session`, which also owns the 403-on-expiry that a
/// shared-secret check has no equivalent of). One layer over all three would have to carry three
/// credential sources and three exemption lists, which is a policy table — the thing this change was
/// explicitly not to build. Each plane keeps its own arrangement; only the control plane's moves.
async fn require_operator_credential(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, ApiError> {
    if !UNAUTHENTICATED_CONTROL_PATHS.contains(&request.uri().path()) {
        state.check_bearer(bearer_token(request.headers()), &state.operator_credential)?;
    }
    Ok(next.run(request).await)
}

/// The **only** paths on the control router that [`require_operator_credential`] lets through
/// without a credential. Adding to this list is how a route becomes public, and it is meant to be a
/// visible, arguable act rather than an omission.
///
/// Both entries are required to be here by SA §9 and by `health.rs`'s doc: a liveness and a
/// readiness probe are consumed by orchestration that holds no operator secret, on all three
/// listeners. `readyz` is a bare boolean precisely *so* that it can be public.
pub const UNAUTHENTICATED_CONTROL_PATHS: &[&str] = &["/healthz", "/readyz"];

/// Every route [`router`] mounts, as `(method, path)` — the subject of
/// `every_control_route_not_exempt_requires_the_operator_credential`.
///
/// **This list is the test's mechanism, and it is weaker than the layer it tests. Say so rather than
/// claim otherwise.** axum 0.8's `Router` exposes no route enumeration — there is no public iterator
/// over its `Method`/path table and no way to derive one — so a test cannot ask the router what it
/// serves. A hard-coded list is what is available, and its limitation is exactly what you would
/// expect: a route added to [`router`] and *not* added here is not covered by that test.
///
/// What makes that acceptable, and why this is not the discipline it replaces: the *layer* is
/// router-wide, so an unlisted new route is authenticated anyway. The two mechanisms cover each
/// other's gap — the layer makes a forgotten route safe, and this list makes a *removed or narrowed
/// layer* fail the build. Neither alone would do; the pairing is the argument. If axum ever exposes
/// its route table, this constant is the thing to delete.
pub const CONTROL_PLANE_ROUTES: &[(&str, &str)] = &[
    ("POST", "/control/ingest"),
    ("POST", "/control/changes"),
    ("GET", "/control/status"),
    ("GET", "/healthz"),
    ("GET", "/readyz"),
];

/// `/control/changes`'s request-body limit.
///
/// **Deliberately not a config key, and deliberately axum's own default value.** 2 MiB is what this
/// endpoint has always enforced — it just enforced it as an untyped 413 from inside the extractor,
/// before the bearer check. Stating it here changes no behaviour except the answer: the limit is now
/// a number this crate owns, so the 422 can name it, and a future decision to raise it is a decision
/// rather than an inherited default. It is **not** sized against `ingest_max_batch_bytes`: a change
/// item is order 200 B (see `config::RESERVED_DENY_HEADROOM_BYTES`), so this admits roughly ten
/// thousand suppressions in one request, and a caller with more than that has to split — which is
/// a latency cost on a batch, not a refusal of any individual deny.
const CHANGES_MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
}

/// Contracts §1 (r6): external IDs are caller-supplied byte strings, capped at **≤ 64 bytes**.
/// Over-length is a typed error here and at build, never a truncation — truncating two callers'
/// keys down to a shared 64-byte prefix would silently merge two different items into one
/// entity, and sidecar disk scales linearly with key length, so the cap is load-bearing, not
/// cosmetic. `/control/ingest` is the only caller-supplied-bytes path in this workspace (the
/// build's external-id representation is fixed at exactly 8 bytes — `tessera-build`'s
/// `BuildError::ExternalIdTooLong` cannot be reached by any build input), so this is where the
/// cap is actually enforced and tested.
const EXTERNAL_ID_MAX_LEN: usize = 64;

struct RawIngestItem {
    /// Optional (contracts §3.4 r6): `None` when the caller supplied no external id. Such an item
    /// gets no sidecar entry and is addressable only by its `tessera_id` (returned per row in
    /// [`IngestResp`]).
    external_id: Option<Vec<u8>>,
    x: f32,
    y: f32,
    access: Vec<u8>,
    scalars: Vec<WalScalar>,
}

/// Parse `/control/ingest`'s body: one Arrow IPC stream, schema
/// `(external_id: binary, x: float32, y: float32, access: utf8, node_id: utf8?, ...scalars)`
/// (R5). `node_id` is accepted (so a well-formed client request is never rejected for including
/// it) but not stored: `WalRow` has no `node_id` field in Phase 1 — buffered items have no row
/// geometry until the next `tessera build`, and `node_id` is a segment-column concept.
fn parse_ingest_batch(body: &[u8]) -> Result<Vec<RawIngestItem>, ApiError> {
    let cursor = std::io::Cursor::new(body);
    let reader = arrow::ipc::reader::StreamReader::try_new(cursor, None).map_err(|e| {
        ApiError::Contract(format!("ingest body is not a valid Arrow IPC stream: {e}"))
    })?;

    let mut items = Vec::new();
    for batch in reader {
        let batch = batch
            .map_err(|e| ApiError::Contract(format!("ingest body: arrow decode error: {e}")))?;
        let schema = batch.schema();

        let ext = optional_binary_col(&batch, "external_id")?;
        let x = f32_col(&batch, "x")?;
        let y = f32_col(&batch, "y")?;
        let access = utf8_col(&batch, "access")?;

        for i in 0..batch.num_rows() {
            let mut scalars = Vec::new();
            for field in schema.fields() {
                let name = field.name().as_str();
                if matches!(name, "external_id" | "x" | "y" | "access" | "node_id") {
                    continue;
                }
                let col = batch
                    .column_by_name(name)
                    .expect("field name came from this batch's own schema");
                if let Some(arr) = col.as_any().downcast_ref::<arrow::array::UInt64Array>() {
                    scalars.push(WalScalar::U64(arr.value(i)));
                } else if let Some(arr) = col.as_any().downcast_ref::<arrow::array::Float32Array>()
                {
                    scalars.push(WalScalar::F32(arr.value(i)));
                } else if let Some(arr) = col.as_any().downcast_ref::<arrow::array::StringArray>() {
                    scalars.push(WalScalar::Utf8(arr.value(i).to_string()));
                }
            }
            // Contracts §3.4 (r6): `external_id` is optional. Neither a missing column nor a null
            // within the column is an error -- both simply mean this item has no caller-supplied
            // external id and is addressable only by its `tessera_id`.
            let external_id = match &ext {
                Some(arr) if !arr.is_null(i) => Some(arr.value(i).to_vec()),
                _ => None,
            };
            // Contracts §1 (r6): a typed error, never a truncation -- see `EXTERNAL_ID_MAX_LEN`'s
            // doc. Checked here, inside the whole-batch parse, so an over-length id anywhere in
            // the batch fails the parse before anything downstream (replay check, dedup,
            // allocation, WAL append) ever runs: the batch has no effect, exactly as a duplicate
            // 409 must.
            if let Some(external_id) = &external_id {
                if external_id.len() > EXTERNAL_ID_MAX_LEN {
                    return Err(ApiError::Contract(format!(
                        "external id is {} bytes, exceeding the {EXTERNAL_ID_MAX_LEN}-byte cap \
                         (contracts §1); refused rather than truncated",
                        external_id.len()
                    )));
                }
            }
            items.push(RawIngestItem {
                external_id,
                x: x.value(i),
                y: y.value(i),
                access: access.value(i).as_bytes().to_vec(),
                scalars,
            });
        }
    }
    Ok(items)
}

/// A binary column that may be null-within (any row) or absent entirely (contracts §3.4 r6:
/// `external_id` is optional). A present-but-wrong-typed column is still a typed error — only
/// "missing" and "null at this row" mean "no external id", never "this batch is malformed".
fn optional_binary_col<'a>(
    batch: &'a arrow::record_batch::RecordBatch,
    name: &str,
) -> Result<Option<&'a arrow::array::BinaryArray>, ApiError> {
    match batch.column_by_name(name) {
        None => Ok(None),
        Some(col) => col
            .as_any()
            .downcast_ref::<arrow::array::BinaryArray>()
            .map(Some)
            .ok_or_else(|| {
                ApiError::Contract(format!(
                    "ingest body: column '{name}' present but not binary"
                ))
            }),
    }
}

fn f32_col<'a>(
    batch: &'a arrow::record_batch::RecordBatch,
    name: &str,
) -> Result<&'a arrow::array::Float32Array, ApiError> {
    batch
        .column_by_name(name)
        .and_then(|c| c.as_any().downcast_ref::<arrow::array::Float32Array>())
        .ok_or_else(|| {
            ApiError::Contract(format!(
                "ingest body: column '{name}' missing or not float32"
            ))
        })
}

fn utf8_col<'a>(
    batch: &'a arrow::record_batch::RecordBatch,
    name: &str,
) -> Result<&'a arrow::array::StringArray, ApiError> {
    batch
        .column_by_name(name)
        .and_then(|c| c.as_any().downcast_ref::<arrow::array::StringArray>())
        .ok_or_else(|| {
            ApiError::Contract(format!("ingest body: column '{name}' missing or not utf8"))
        })
}

#[derive(serde::Serialize)]
struct IngestResp {
    accepted: u64,
    over_bound: u64,
    over_bound_ids: Vec<String>,
    /// Contracts §3.4 (r6): `external_id` is optional, so an accepted item may be addressable
    /// only by its `tessera_id` -- returned here per accepted row, in the same order as the
    /// request batch, so a caller can correlate. Present for every accepted row, whether or not
    /// that row carried an external id.
    tessera_ids: Vec<u64>,
}

/// The Arrow decode through the WAL append/fsync (D-A, review finding 7): everything CPU-bound
/// or fsync-bearing for one `/control/ingest` request, run inside `spawn_blocking`. **Never
/// behind the Task 4 admission gate** — that gate applies only to the viewer/session planes; an
/// ingest batch durability-syncing must not be throttled by the same budget a slow viewport
/// consumes, and more importantly a suppression on `/control/changes` must reach its own
/// `spawn_blocking` call (and thus the WAL mutex) without first queueing behind N ingest
/// *handlers* occupying reactor threads (lifecycle §1.3's deny priority lane).
fn run_ingest(state: &AppState, body: &[u8], batch_id: String) -> Result<IngestResp, ApiError> {
    let body_hash: [u8; 32] = Sha256::digest(body).into();

    let items = parse_ingest_batch(body)?;

    // Task 6 (D1): the row cap. 422 per contracts §3.1's "bounds exceeded" row, naming the bound
    // and the batch's own size.
    //
    // **What is already spent when this fires, stated rather than implied**: the whole Arrow
    // decode, because the row count is not knowable before it. That is the cost of the cap and it
    // is why the *byte* cap is enforced a layer earlier, on the route, where nothing has been
    // decoded at all.
    //
    // **Placed before the `terms_of_label`/`resolve_terms` loop below, which narrows a known
    // consequence without closing it.** Task 3a recorded that `resolve_terms` runs pre-submit, so
    // extension-id dictionary state grows on refused batches; putting this check first means an
    // over-large batch no longer contributes to that. A batch that is *under* the row cap and
    // fails later still does. This comment says which of those two it is on purpose — the check
    // does not close the path.
    if items.len() > state.ingest_max_batch_rows {
        return Err(ApiError::Contract(format!(
            "ingest batch has {} rows, exceeding the {}-row per-batch cap \
             (ingest.ingest_max_batch_rows); refused before ENTITY-ID allocation, so it cost no \
             entity id, no queue slot and no WAL append. The body was hashed and decoded in full \
             before this fired — the row count is not knowable earlier — so it is not free; the \
             byte cap on the route is the refusal that costs nothing",
            items.len(),
            state.ingest_max_batch_rows
        )));
    }

    // Resolve each item's descriptors and terms up front — idempotent even on a replayed
    // request, since `resolve_terms` looks up already-interned descriptors without reassigning
    // (see `Engine::resolve_terms`'s doc).
    let bounds = state.engine.declared_bounds();
    let mut descriptor_lists: Vec<Vec<Vec<u8>>> = Vec::with_capacity(items.len());
    let mut terms_per_item: Vec<Vec<TermId>> = Vec::with_capacity(items.len());
    let mut over_bound_ids: Vec<String> = Vec::new();
    let mut over_bound: u64 = 0;

    for item in &items {
        let descriptors = state
            .engine
            .plugin()
            .terms_of_label(&item.access)
            .map_err(|e| ApiError::Contract(format!("access field: {e}")))?;
        let terms = state.engine.resolve_terms(&descriptors);
        if terms.len() as u32 > bounds.max_terms_per_item {
            over_bound += 1;
            // A null external id has nothing to name it by in this list; it is still counted in
            // `over_bound` above (bounds warn, never exclude -- §6.2 r16), just not listed here.
            if over_bound_ids.len() < 100 {
                if let Some(external_id) = &item.external_id {
                    over_bound_ids.push(String::from_utf8_lossy(external_id).to_string());
                }
            }
        }
        descriptor_lists.push(descriptors);
        terms_per_item.push(terms);
    }

    if let Some((prev_hash, prev_entity_ids)) = state.engine.accepted_batch(&batch_id) {
        if prev_hash == body_hash {
            // Idempotent replay of an already-acked batch: 200, no effect (R5) -- same
            // `tessera_id`s as the original acceptance, recovered from the recorded entity ids
            // rather than re-derived from `external_id` (a null-external-id row has none to
            // re-derive from).
            let tessera_ids = tessera_ids_of(state, &prev_entity_ids)?;
            return Ok(IngestResp {
                accepted: items.len() as u64,
                over_bound,
                over_bound_ids,
                tessera_ids,
            });
        }
        return Err(ApiError::Conflict(format!(
            "batch id '{batch_id}' was already accepted with a different body"
        )));
    }

    // Validate-first (contracts §3.1 r6): duplicate external ids are 409, detail lists them, and
    // the batch has NO effect -- so this runs entirely before `allocate_sorted`/WAL append below,
    // and after the batch-id replay check above, which stays first (an idempotent replay of an
    // already-acked batch must still be a 200 no-op, not get caught here as "already known").
    // Contracts §3.4 r6: duplicate detection applies only *where an external id is supplied* --
    // a batch of items with no external id at all has no duplicates to find, and two null ids
    // must never be treated as colliding with each other. So every step below is scoped to
    // `Some(external_id)` items only.
    // Two checks, cheaper first:
    //   1. duplicates within this batch itself, by a hash set over the supplied bytes;
    //   2. collisions against existing state, in one call to `Engine::resolve_external_ids`,
    //      which checks the LIVE map (`Engine::established`) first -- Important I-8: an id
    //      ingested since the build lives only there, never in the sidecar, and is exactly the
    //      duplicate a retried client batch (under a fresh batch id) is most likely to produce --
    //      then falls back to one batched, sorted sidecar call over the residual keys, so the
    //      bundle's extents are opened at most once each rather than once per row.
    let mut seen_in_batch: FxHashSet<&[u8]> = FxHashSet::default();
    let mut dup_ids: Vec<String> = Vec::new();
    for item in &items {
        let Some(external_id) = &item.external_id else {
            continue;
        };
        if !seen_in_batch.insert(external_id.as_slice()) {
            dup_ids.push(base64::engine::general_purpose::STANDARD.encode(external_id));
        }
    }
    if !dup_ids.is_empty() {
        dup_ids.sort_unstable();
        dup_ids.dedup();
        return Err(ApiError::Conflict(format!(
            "duplicate external ids within this batch: {}",
            dup_ids.join(", ")
        )));
    }

    // Only the supplied external ids are worth asking the engine about -- a null id has no
    // sidecar/live-map entry to collide with, so it is filtered out here rather than passed
    // through as some sentinel value.
    let supplied: Vec<(usize, Vec<u8>)> = items
        .iter()
        .enumerate()
        .filter_map(|(i, item)| item.external_id.clone().map(|id| (i, id)))
        .collect();
    let supplied_ids: Vec<Vec<u8>> = supplied.iter().map(|(_, id)| id.clone()).collect();
    let resolved = state
        .engine
        .resolve_external_ids(&supplied_ids)
        .map_err(map_store_error)?;
    let existing_ids: Vec<String> = resolved
        .iter()
        .zip(&supplied)
        .filter(|(entity, _)| entity.is_some())
        .map(|(_, (_, id))| base64::engine::general_purpose::STANDARD.encode(id))
        .collect();
    if !existing_ids.is_empty() {
        return Err(ApiError::Conflict(format!(
            "duplicate external ids already known to this deployment: {}",
            existing_ids.join(", ")
        )));
    }

    // The rows go to the executor **unallocated**: entity-id assignment moved off the handler at
    // Task 3a and happens on the single writer thread, per command now and per commit window at
    // Task 7a. That is what makes design §11.1's signature-sort scope the *server's* window rather
    // than whatever chunk size a client happened to pick — and it is why this handler no longer
    // calls `allocate_sorted` at all. Calling it here after this change would double-allocate.
    //
    // **The three decoded intermediates are CONSUMED here, not cloned** (fix round 1, F2). This was
    // `items.iter().zip(&terms_per_item).zip(descriptor_lists.iter())` cloning `external_id`,
    // `descriptors`, `scalars` and `terms` out of them — so `items`, `descriptor_lists`,
    // `terms_per_item` *and* `rows` were all live simultaneously, and `accept_ingest` then blocks on
    // its receipt with all four in scope. At `ingest_admission` concurrent handlers that doubling is
    // multiplied by the admission bound, which is the term `INGEST_RESIDENT_CEILING_BYTES`'s
    // arithmetic is about. Moving instead of cloning also deletes four per-row allocations on the
    // path that must sustain 10⁹-scale ingest; the three source vectors drop at the end of this
    // statement.
    let rows: Vec<UnallocatedRow> = items
        .into_iter()
        .zip(terms_per_item)
        .zip(descriptor_lists)
        .map(|((item, terms), descriptors)| UnallocatedRow {
            external_id: item.external_id,
            descriptors,
            x: item.x,
            y: item.y,
            scalars: item.scalars,
            terms,
        })
        .collect();

    // The ack contract, on the executor: allocate -> WAL append -> fsync -> apply+swap -> 200.
    // Never 200 without fsync. Ordering is now a consequence of single ownership rather than of a
    // mutex held across four steps (see `tessera_engine`'s `write` module).
    let accepted = rows.len() as u64;
    let entity_ids = state
        .engine
        .accept_ingest(rows, batch_id, body_hash)
        .map_err(|e| {
            // Batch-level context, kept alongside the mapper's own `error!` rather than folded into
            // one line, so an operator sees both without the body ever carrying either.
            tracing::error!("an ingest batch was refused by the write executor");
            map_accept_error(e)
        })?;

    // Contracts §3.4 (r6): the 200 response returns each accepted row's `tessera_id`, in batch
    // order, so a caller who supplied no external id for an item still learns the identity it
    // was given -- otherwise that item would be unreachable by anyone.
    let tessera_ids = tessera_ids_of(state, &entity_ids)?;

    Ok(IngestResp {
        accepted,
        over_bound,
        over_bound_ids,
        tessera_ids,
    })
}

/// **The ordering here is the whole of `backpressure_is_invisible_before_auth`, and it is
/// load-bearing.** An unauthenticated caller must not be able to learn anything about this server's
/// ingest pressure — or about its configured batch cap — by reading a status code. So:
///
/// 1. the operator credential → **401**, in [`require_operator_credential`], which is a router
///    layer and therefore runs before this function and before its extractors;
/// 2. the body's own rejection (over `ingest_max_batch_bytes`) → **422**;
/// 3. the missing batch-id header → **422**;
/// 4. the admission bound → **429**, evaluated before `spawn_blocking` because the resource it
///    bounds is the thing `spawn_blocking` takes;
/// 5. the row cap, inside `run_ingest` after the Arrow decode → **422**;
/// 6. the queue bound, inside the executor's `submit` → **429**.
///
/// **Step 1 used to be the first statement of this function, and moving it to the router changed
/// what `body: Result<Bytes, _>` is for.** It was doing two jobs: keeping `check_bearer` ahead of
/// the extractor, and turning the extractor's rejection into a mapped 422 instead of axum's own 413.
/// The layer discharges the first outright — an extractor rejection can no longer precede the
/// credential, because the credential is checked one service out. The second job is unchanged and is
/// why the signature stays: with plain `Bytes`, an *authenticated* over-cap caller would still get a
/// 413, outside contracts §3.1's closed code list. Its rejection type is `Infallible`, so this is
/// total.
///
/// # The buffered body: what the layer closed, and what it did not
///
/// **Closed for an unauthenticated caller.** This paragraph used to say the opposite, and it was
/// true when it was written: extractors run inside the handler service, so with `check_bearer` as
/// this function's first statement the body was already resident in full — up to
/// `ingest_max_batch_bytes`, 16 MiB by default since Task 6 D1 raised it from axum's 2 MiB, an 8×
/// widening — before the credential was seen. [`require_operator_credential`] runs *outside* the
/// extractors, so a caller with no credential now meets a 401 while the body is still an unconsumed
/// stream. Nothing is buffered on their behalf.
///
/// **Not closed for a valid-credentialed caller, and not closed for connection count.** An
/// authenticated request still buffers up to `ingest_max_batch_bytes` before `ingest_admission` is
/// consulted, and `axum::serve` applies no connection or concurrency cap, so N authenticated
/// connections still pin `N × ingest_max_batch_bytes`. `config`'s third startup relation —
/// `(ingest_queue_bound + ingest_admission) × ingest_max_batch_bytes ≤ INGEST_RESIDENT_CEILING_BYTES`
/// — caps the *size* of one window (roughly 170 MiB at the default bounds, refusing the
/// `ingest_queue_bound = 1, ingest_max_batch_bytes = 1 GiB` configuration every earlier relation
/// admitted). It does not cap the *count*, because nothing in this crate can. **That residual is an
/// open owner decision** and is on the stage ledger.
///
/// **A `tower` concurrency-limit layer was assessed as a close for the count and declined** (Task 6
/// gate, F11), and this layer does not revisit it — it is an *authentication* layer, not a
/// concurrency bound, and it adds no per-request wait. The declined assessment stands: on the
/// *whole* control router a concurrency limit is disqualified outright, since it would put
/// `/control/changes` behind an in-flight bound shared with ingest handlers that block on receipts,
/// which is lifecycle §1.3's "a deny queued behind work of unbounded duration" reintroduced at the
/// router — the exact shape [`DENY_RUNTIME`] exists to prevent, given up to close a memory window.
/// On the *ingest route alone* it is technically available but not proportionate: `tower::limit`
/// **queues** rather than sheds, so sized at or below `ingest_admission` it swallows D2's prompt 429
/// (requests wait for a layer permit instead of being refused), and sized above it, it needs a
/// second bound nobody has argued — while still leaving `limit × ingest_max_batch_bytes` resident.
async fn ingest(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Result<Bytes, axum::extract::rejection::BytesRejection>,
) -> Result<Json<IngestResp>, ApiError> {
    // Contracts §3.1's 422 row is "malformed request, **bounds exceeded**, unknown filter operand",
    // and a `BytesRejection` is either of the first two. **Branched on the rejection's own status,
    // not collapsed**: this arm used to report every `BytesRejection` as "your batch is too big",
    // and the variant also covers a client disconnecting mid-upload and a malformed transfer
    // encoding — so an operator whose 4 KB batch was truncated by a flaky link was told to shrink
    // their batches. The rejection's `Display` is still never forwarded (this module's rule).
    let body = body.map_err(|rejection| {
        if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
            ApiError::Contract(format!(
                "ingest body exceeds the {}-byte per-batch cap (ingest.ingest_max_batch_bytes); \
                 refused before decoding, so it cost no queue slot and no WAL append",
                state.ingest_max_batch_bytes
            ))
        } else {
            ApiError::Contract(
                "the ingest request body could not be read to completion — the connection failed \
                 mid-upload, or the transfer encoding is malformed. This is NOT the per-batch cap; \
                 nothing was decoded, queued or appended"
                    .to_string(),
            )
        }
    })?;

    let batch_id = headers
        .get("x-tessera-batch-id")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::Contract("missing x-tessera-batch-id header".to_string()))?
        .to_string();

    // Task 6 (D2): the admission bound, **before** `spawn_blocking`. See `IngestAdmission`.
    let Some(permit) = state.ingest_admission.try_admit() else {
        // `debug!`, not `warn!` and certainly not `error!`. This was a `warn!`, which is a
        // synchronous formatted write **on the reactor, per refused request** — under a sustained
        // shed at 10⁹ ingest rates the log becomes a second bottleneck on exactly the path that
        // exists to be cheap. `tracing`'s macros check interest before evaluating their fields, so
        // at any level above DEBUG this costs a load and a branch.
        //
        // **The operator signal is the counter, not the line**: `ingest.shed_total` on
        // `/control/status` counts every one of these, and `ingest.in_flight` says why. A per-event
        // line adds nothing a counter does not, which is the same argument `map_accept_error`
        // already makes for the queue's 429 one level down.
        tracing::debug!("the ingest admission bound is saturated; answering 429 backpressure");
        return Err(ApiError::IngestAdmissionBackpressure {
            retry_after_s: crate::error::admission_retry_after_s(
                &state.engine.write_executor_stats(),
            ),
        });
    };

    // D-A / review finding 7: closure capture is `state` (moved in directly — nothing after this
    // `.await` needs the handler's own copy), `body` (an owned `Bytes` — cheap, refcounted clone
    // of the request body already read off the socket, not a copy) and `batch_id` (owned
    // `String`). Never gated by `ComputeGate` (see `run_ingest`'s doc).
    //
    // The permit is **moved in**, not held across the `.await`: a disconnected client's handler
    // future is dropped while this closure keeps running and keeps its thread, so releasing on
    // handler-drop would under-count exactly when the pool is under pressure.
    let resp = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        run_ingest(&state, &body, batch_id)
    })
    .await
    .map_err(map_join_error)??;

    Ok(Json(resp))
}

/// `EntityId` -> `tessera_id`, per row, in the caller's given order. `Engine::tessera_id_of` is
/// fallible only for an entity id the I9 allocator's ceiling makes unreachable in practice
/// (Important I-1) — still propagated as a typed 500 here, never `.unwrap()`-ed away, since an
/// internal invariant violation must fail closed.
fn tessera_ids_of(state: &AppState, entity_ids: &[EntityId]) -> Result<Vec<u64>, ApiError> {
    entity_ids
        .iter()
        .map(|&entity| {
            state
                .engine
                .tessera_id_of(entity)
                .map(|id| id.raw())
                .map_err(|e| ApiError::FailClosed(e.to_string()))
        })
        .collect()
}

/// `external_id` is base64 (external ids are arbitrary bytes — contracts §2.1's `binary` type —
/// not necessarily valid UTF-8; JSON has no native binary type, so base64 is the only
/// lossless encoding available here, matching `/control/ingest`'s Arrow `binary` column).
#[derive(serde::Deserialize)]
struct ChangeItem {
    external_id: String,
    op: String,
    #[serde(default)]
    access: Option<String>,
}

/// One `/control/changes` item, fully validated but not yet applied — see [`changes`]'s doc.
struct ValidatedChange {
    external_id: Vec<u8>,
    entity: EntityId,
    op: ChangeOp,
    /// Raw descriptor bytes (never `TermId`s — see `Engine::accept_change`'s doc for why
    /// resolution is deferred past this validation pass, until after this item's own WAL
    /// append/fsync succeeds).
    raw_descriptors: Option<Vec<Vec<u8>>>,
}

/// The validate-then-apply body of `/control/changes` (D-A, review finding 7): external-id
/// resolution (sidecar IO) and every item's WAL append/fsync, run inside `spawn_blocking`. Same
/// never-gated rule as [`run_ingest`] — this is the deny priority lane a suppression must reach
/// without queueing behind concurrent ingest handlers on the reactor.
fn run_changes(state: &AppState, items: Vec<ChangeItem>) -> Result<(), ApiError> {
    // Validate-first (Important 2 fix): parse every item's op, base64-decode and resolve its
    // external id, and validate its `access` field's shape — all *before* appending anything.
    // The previous item-by-item loop could append, fsync and apply items 1..n-1 before item n's
    // 404/422 aborted the request, leaving the caller with a single error for a batch that was
    // actually partially applied. Doing every fallible *validation* step first means a rejected
    // batch is rejected wholesale, with no side effect at all. (A WAL I/O failure partway through
    // the second, apply-only loop below is a different class of failure — an infrastructure
    // fault, not a client-correctable validation error — and is not, and cannot be, rolled back:
    // each item's `Engine::accept_change` call is its own complete ack-contract unit, exactly as
    // `/control/ingest`'s batches are. Nor does it abort the second loop; see the comment there.)
    let mut validated = Vec::with_capacity(items.len());
    for item in &items {
        let op = match item.op.as_str() {
            "predicate" => ChangeOp::Predicate,
            "delete" => ChangeOp::Delete,
            "suppress" => ChangeOp::Suppress,
            "unsuppress" => ChangeOp::Unsuppress,
            other => {
                return Err(ApiError::Contract(format!("unknown change op '{other}'")));
            }
        };

        let external_id_bytes = base64::engine::general_purpose::STANDARD
            .decode(&item.external_id)
            .map_err(|e| ApiError::Contract(format!("external_id is not valid base64: {e}")))?;
        let entity = state
            .engine
            .resolve_external_id(&external_id_bytes)
            .map_err(map_store_error)?
            .ok_or_else(|| ApiError::Unknown("unknown external id".to_string()))?;

        // `terms_of_label` only maps `access` bytes to descriptor *bytes* (deterministic, no
        // persistent state touched) — validating this here is safe and does not pre-empt
        // `Engine::accept_change`'s deferred `resolve_terms` (Important 3 fix), which is the step
        // that actually interns novel descriptors into the process-lifetime extension state.
        let raw_descriptors: Option<Vec<Vec<u8>>> = match &item.access {
            Some(access) => Some(
                state
                    .engine
                    .plugin()
                    .terms_of_label(access.as_bytes())
                    .map_err(|e| ApiError::Contract(format!("access field: {e}")))?,
            ),
            None => None,
        };

        validated.push(ValidatedChange {
            external_id: external_id_bytes,
            entity,
            op,
            raw_descriptors,
        });
    }

    // **Every item is submitted, even after one fails.** The obvious `?` here aborts the batch at
    // the first failure, and that is fail-open at batch scope now that Task 3a made a WAL failure a
    // sustained *posture* rather than a one-off: `WalPoisoned` refuses every subsequent append, so
    // an aborting loop applies exactly the first item of a multi-item change batch, on every retry,
    // until the WAL is reopened — every other suppression in the request silently unapplied behind
    // a 500 that reads as "retry for durability". Continuing is strictly more fail-closed and is
    // this lane's whole ethos (lifecycle §4: never a refusal that leaves a deny unapplied): each
    // remaining `Delete`/`Suppress` is applied to the live overlay by the executor even though its
    // append fails, so the items are hidden and the caller still gets a 500.
    //
    // Validation is already wholesale above, so nothing reached here can be a client-correctable
    // fault: everything below is an infrastructure failure and every one of them is alarmed
    // individually.
    //
    // **The batch's answer is a FOLD over dispositions, not the first item's status.** Task 3a
    // reported `first_error`, which is wrong in a way that matters now that the mapping table
    // distinguishes 503 from 500: an item's status describes an item. The constructible bad case is
    // "item 1 applied successfully, the executor then died, item 2 refused" — first-error reporting
    // has no error at all for item 1 and answers item 2's **503 `not-ready`**, i.e. "this node did
    // not take your write", for a batch containing a durable, in-force suppression.
    //
    // So every failure is collected with the count that succeeded, and `map_change_batch_error`
    // decides once, over all of them. See its doc for the rules and for why 500 dominates 503.
    //
    // **Each failure is collected WITH ITS OP.** Lifecycle §4's apply-anyway rule is scoped to
    // `Delete`/`Suppress` and the executor applies exactly that scope, so "did this failure leave an
    // effect in force?" cannot be answered from the error alone — a `Predicate` whose append failed
    // was refused without applying, and an op-blind fold reported it as possibly in force *and*
    // omitted it from the "not applied" half. The op is already in hand here, one line below, so it
    // is carried rather than re-derived at the fold.
    let mut failures: Vec<(ChangeOp, AcceptError)> = Vec::new();
    let mut applied = 0usize;
    for change in validated {
        let op = change.op;
        match state.engine.accept_change(
            change.external_id,
            change.entity,
            op,
            change.raw_descriptors,
        ) {
            Ok(()) => applied += 1,
            Err(e) => {
                if matches!(op, ChangeOp::Delete | ChangeOp::Suppress) {
                    // Deny-op append failure (lifecycle §4): the executor already applied the
                    // change to the live overlay before returning this error — never a refusal
                    // that leaves a deny unapplied.
                    tracing::error!(
                        op = ?op,
                        "ALARM: wal append/fsync failed for a deny-op change; applied to the \
                         in-memory overlay anyway (item hidden immediately) and returning 500 — \
                         durability is owed, caller must retry"
                    );
                } else {
                    tracing::error!(
                        op = ?op,
                        "wal append/fsync failed for a non-deny change; refusing without applying"
                    );
                }
                failures.push((op, e));
            }
        }
    }

    match map_change_batch_error(&failures, applied) {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// **The credential first, then the body's own rejection** — the same ordering `ingest` has, and by
/// the same mechanism: [`require_operator_credential`] is a router layer, so it answers 401 before
/// this function or its extractors run at all. That also keeps the unauthenticated flood off
/// [`spawn_on_deny_lane`]'s threads — the resource that lane exists to keep free — and now keeps it
/// off the JSON decode too, which used to run ahead of the check.
///
/// `body: Result<Json<..>, JsonRejection>` rather than `Json(items)` remains, for the surviving half
/// of its original reason: an extractor that rejects on its own answers a bare **413**, outside
/// contracts §3.1's closed code list, on the lane where an out-of-list status is least defensible.
/// The layer no longer lets that happen *pre-authentication*; this signature is what stops it
/// happening at all. Body decoding stays on the reactor, bounded by [`CHANGES_MAX_BODY_BYTES`].
async fn changes(
    State(state): State<Arc<AppState>>,
    body: Result<Json<Vec<ChangeItem>>, axum::extract::rejection::JsonRejection>,
) -> Result<StatusCode, ApiError> {
    // Contracts §3.1's 422 row is "malformed request, **bounds exceeded**, unknown filter operand",
    // which covers both shapes a `JsonRejection` carries. They are distinguished by the rejection's
    // own status rather than collapsed, because "your batch is too large" and "your JSON is
    // malformed" send an operator to different places. The rejection's `Display` is never forwarded
    // — this module's rule.
    let Json(items) = body.map_err(|rejection| {
        if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
            ApiError::Contract(format!(
                "the change request body exceeds the {CHANGES_MAX_BODY_BYTES}-byte per-request \
                 cap; split it into smaller requests. Nothing in this request was applied — and \
                 note that this is a cap on one REQUEST, never on a deny: /control/changes is never \
                 load-shed (contracts §3.1)"
            ))
        } else {
            ApiError::Contract(
                "the change request body is not a valid JSON array of {external_id, op, access?} \
                 items; nothing in it was applied"
                    .to_string(),
            )
        }
    })?;

    // **No readiness gate here, and that is load-bearing** (lifecycle §4; Task 3a's D6). A
    // `WalPoisoned` node still applies `Delete`/`Suppress` to the live overlay before returning its
    // error, so gating this endpoint on `readyz` would apply the first failing suppression and then
    // refuse every subsequent one *without applying it* — refused **and** unapplied, which is the
    // fail-open the posture exists to prevent. Readiness governs routing, never deny acceptance.
    //
    // D-A / review finding 7: closure captures `state` (moved in directly — nothing after this
    // `.await` needs the handler's own copy) and `items` (moved — the request body is already
    // fully decoded to owned `Vec<ChangeItem>` by this point, so there is nothing left to borrow).
    // `spawn_on_deny_lane`, not `tokio::task::spawn_blocking`: see its doc.
    spawn_on_deny_lane(move || run_changes(&state, items))
        .await
        .map_err(map_join_error)??;

    // R5: `/control/changes` is 200 after fsync, never 429.
    Ok(StatusCode::OK)
}

/// **The precedent for [`require_operator_credential`], and the reason it is a layer.** R5 requires
/// bearer auth on every plane, including this one; this handler nevertheless *shipped* returning
/// `entity_id_high_water` — a global, unmasked corpus-size fact — to anyone who could reach the
/// control listener, which may be loopback TCP and not only a unix socket
/// (`config::ControlListen::Tcp`). Nothing was wrong with the check; there simply was not one, and
/// no reviewer noticed because "every control handler calls `check_bearer`" was a convention rather
/// than a construction. Its own `check_bearer` call, added as "Important 1 fix", is now redundant
/// and has been deleted: the layer refuses this route before the handler is entered, and a redundant
/// copy would only make the layer's mutation tests pass for the wrong reason.
async fn status(State(state): State<Arc<AppState>>) -> Result<Json<serde_json::Value>, ApiError> {
    // D-B: the viewer/session admission gate's gauges. `in_flight`/`waiting` are read live off
    // the semaphores; `shed_total` is a single process-wide counter — no per-principal labels
    // anywhere on this plane (SA §9). `shed_total` counts only this gate's own two shed paths —
    // it does NOT include D-G single-flight builder 429s (`ProjectionBuilding`/`FragmentBuilding`,
    // Tasks 1-2), which happen after admission and are invisible to this gate (see
    // `ComputeGate::shed_total`'s doc).
    let gate = state.compute_gate.status();
    // The write executor's posture and counters. **This is where the posture string lives** — the
    // bearer-gated plane — because `/readyz` is unauthenticated on every listener and must stay a
    // bare boolean (SA §9; see `health.rs`). `ready` is computed by the *same* `is_ready` the probe
    // calls, not a second predicate, so the two can never drift.
    //
    // Contracts §3.4 specifies `readiness` as a **per-partition** field, beside `segments_version`
    // and `watermark`. This build has one partition and no per-partition status block yet, so the
    // flag lives inside `write_executor` rather than claiming the top-level `readiness` key that
    // stage 2.2 will need for the per-partition form.
    let executor = state.engine.write_executor_stats();
    // Track C's S1, deferred by Task 3a only because `Engine::pin_stats` did not exist on that
    // branch (Task 4 has since landed it). Lifecycle §2.2's drain list: `drain_depth` above
    // `DRAIN_DEPTH_ALARM` is the operator alarm, and `oldest_retired_secs` is what distinguishes
    // "deep because busy" from "deep because reclaim is not running".
    let pins = state.engine.pin_stats();
    // Task 6 (D6): Task 4's and Task 5's counters, wired here now that both have landed.
    //
    // **`tessera_engine::FragmentCacheStats`, never `tessera_authz::...`** — `check-layers.sh`
    // denies a `tessera-server → tessera-authz` edge (SA §3), and the re-export at
    // `tessera-engine`'s crate root exists precisely so this call site has a nameable type.
    let projection_cache: tessera_engine::CacheStats = state.engine.row_projection_cache_stats();
    let fragment_cache: tessera_engine::FragmentCacheStats = state.engine.fragment_cache_stats();
    let ingest = state.ingest_admission.status();
    let sessions_retained = state.sessions.lock().len();
    Ok(Json(serde_json::json!({
        "entity_id_high_water": state.engine.allocator_high_water(),
        "compute": {
            "admission": gate.admission,
            "queue": gate.queue,
            "in_flight": gate.in_flight,
            "waiting": gate.waiting,
            "shed_total": gate.shed_total,
        },
        "write_executor": {
            "posture": executor.posture.as_str(),
            "ready": is_ready(executor.posture),
            "work_submitted": executor.work_submitted,
            "deny_submitted": executor.deny_submitted,
            "wal_appends": executor.wal_appends,
            "wal_fsyncs": executor.wal_fsyncs,
            "apply_nanos_total": executor.apply_nanos_total,
            "apply_nanos_max": executor.apply_nanos_max,
            // Task 6: the queue-depth gauge and the drain estimate `retry_after_s` is derived
            // from. `work_depth` is a snapshot of two independently-advancing counters — see
            // `ExecutorStats::work_depth` — and `work_service_nanos_ewma` is an estimator, not a
            // bound; `estimate_retry_after_s`'s doc says what makes it one.
            "work_completed": executor.work_completed,
            "work_depth": executor.work_depth,
            "work_service_nanos_ewma": executor.work_service_nanos_ewma,
            // Published beside the EWMA rather than folded into it: the EWMA is written only when a
            // job finishes, so during one long job it reports the previous regime. The 429
            // derivations take `max` of the two (`ExecutorStats::service_nanos_for_estimate`); an
            // operator gets both numbers because "the last ten jobs took 3 ms and this one has been
            // running for 90 s" is the diagnosis, and either figure alone hides it.
            "work_in_flight_nanos": executor.work_in_flight_nanos,
        },
        // Task 6 (D2/D1). `admission` is the bound, `in_flight` is read live off the semaphore.
        // `shed_total` counts **this bound's** 429s only — the queue-full 429 is produced inside
        // the engine and is not counted here; `work_depth` above is its gauge.
        "ingest": {
            "admission": ingest.admission,
            "in_flight": ingest.in_flight,
            "shed_total": ingest.shed_total,
            "max_batch_rows": state.ingest_max_batch_rows,
            "max_batch_bytes": state.ingest_max_batch_bytes,
        },
        // Task 6 (D5). **It alarms; it does not act** — there is no fold until stage 2.3, so
        // `soft_limit_alarms` rising is a signal that the overlay is deep, never a mechanism that
        // makes it shallower. `depth` is read off the live generation, so it cannot drift from
        // what a request composes against.
        "overlay": {
            "depth": state.engine.overlay_depth(),
            "soft_limit_alarms": executor.overlay_soft_limit_alarms,
        },
        "pins": {
            "drain_depth": pins.drain_depth,
            "oldest_retired_secs": pins.oldest_retired_secs,
        },
        // Task 5's two caches (Task 3b deferred this to here by name, because Task 5 was not
        // merged on that branch).
        //
        // **`young_evictions` is an alarm, not an undifferentiated counter, and `thrashing` is the
        // predicate spelled out.** `> 0` is the argued threshold, not an arbitrary one: `prepare`
        // refuses at startup any bound below `expected_concurrent_sessions × the measured
        // per-entry size (see `validate_cache_bounds`), so a young eviction means the collapsing
        // regime was entered *another* way — a second slice per session, a generation swap's
        // transient duplicate, or entries larger than the measured figure. That is precisely what
        // `validate_cache_bounds`' own doc says this counter is for.
        "row_projection_cache": {
            "entries": projection_cache.entries,
            "bytes": projection_cache.bytes,
            "bound_bytes": projection_cache.bound_bytes,
            "hits": projection_cache.hits,
            "misses": projection_cache.misses,
            "building_refusals": projection_cache.building_refusals,
            "evictions": projection_cache.evictions,
            "young_evictions": projection_cache.young_evictions,
            "thrashing": projection_cache.young_evictions > 0,
            "oversized_admissions": projection_cache.oversized_admissions,
        },
        "fragment_cache": {
            "entries": fragment_cache.entries,
            "bytes": fragment_cache.bytes,
            "bound_bytes": fragment_cache.bound_bytes,
            "hits": fragment_cache.hits,
            "misses": fragment_cache.misses,
            "building_refusals": fragment_cache.building_refusals,
            "evictions": fragment_cache.evictions,
            "young_evictions": fragment_cache.young_evictions,
            "thrashing": fragment_cache.young_evictions > 0,
            "oversized_admissions": fragment_cache.oversized_admissions,
            // The observable that separates an in-memory eviction from a genuinely cold rebuild.
            "rebuilds": state.engine.fragment_cache_rebuilds(),
        },
        // The gauge only. **S17 — the expiry sweep — is out of scope** and remains an open owner
        // decision: an expired session is 403'd but never removed, and each retained one pins an
        // `Arc<FrozenFragment>`, so `fragment_cache.bytes` falling does not mean that memory was
        // released. Publishing the number is what makes that visible to whoever rules on it; a
        // count that only ever rises while `young_evictions` stays quiet is the signature.
        "sessions": {
            "retained": sessions_retained,
        },
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// **The deny lane does not share tokio's blocking pool** — the Task 3a security-lens finding,
    /// closed and demonstrated rather than argued.
    ///
    /// The ambient pool is saturated *provably*, not hopefully: each parked closure publishes its
    /// arrival before blocking, and the test waits on those arrivals. Then the deny lane is asked
    /// to run something. If it shared the pool, its closure would sit in tokio's FIFO behind the
    /// parked ones — which is exactly what happens to a suppression queued behind ingest closures
    /// in production.
    ///
    /// **The mutation is [`spawn_on_deny_lane`]'s body** — replace it with
    /// `tokio::task::spawn_blocking` and this test hangs, which is why the await is bounded. That
    /// is the whole reason the lane is reached through one named function: `changes()` has exactly
    /// one route to it, so a rewrite of this file that "simplifies away" the separate runtime lands
    /// here and goes red, rather than quietly reinstating the starvation.
    ///
    /// The timeout is in the **failing** path only; on a healthy build the deny closure resolves in
    /// microseconds. There is deliberately no assertion that the ambient probe *did not* run: a
    /// negative statement about another thread's progress cannot be established without waiting
    /// (Task 3a fix round 1, CRITICAL 1). The sound content is that the deny lane completed while
    /// the ambient pool was demonstrably full, and that is what is asserted.
    #[test]
    fn a_deny_does_not_queue_behind_a_saturated_blocking_pool() {
        const AMBIENT_BLOCKING_THREADS: usize = 2;

        let ambient = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(AMBIENT_BLOCKING_THREADS)
            .enable_all()
            .build()
            .unwrap();

        ambient.block_on(async {
            let (release_tx, release_rx) = mpsc::channel::<()>();
            let release_rx = Arc::new(std::sync::Mutex::new(release_rx));
            let (parked_tx, parked_rx) = mpsc::channel::<()>();

            for _ in 0..AMBIENT_BLOCKING_THREADS {
                let rx = Arc::clone(&release_rx);
                let tx = parked_tx.clone();
                tokio::task::spawn_blocking(move || {
                    tx.send(()).unwrap();
                    let _ = rx.lock().unwrap().recv();
                });
            }
            // Every ambient blocking thread has published its arrival, so the pool is full as a
            // fact rather than as a hope.
            for _ in 0..AMBIENT_BLOCKING_THREADS {
                parked_rx.recv().unwrap();
            }

            let ran = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                spawn_on_deny_lane(|| "the deny lane ran"),
            )
            .await
            .expect(
                "the deny lane did not run within 10s while tokio's blocking pool was saturated — \
                 a suppression is queued behind unbounded ingest work, which is lifecycle §1.3's \
                 forbidden shape one layer above the executor's priority lane",
            )
            .expect("the deny closure ran to completion");
            assert_eq!(ran, "the deny lane ran");

            for _ in 0..AMBIENT_BLOCKING_THREADS {
                let _ = release_tx.send(());
            }
        });
    }

    /// **The lazy path must not drop a `Runtime` on the reactor** — found by both fix-round-1 lenses
    /// independently, and reproducible.
    ///
    /// `init_deny_runtime` builds a runtime and then `set`s it. `OnceLock::set` hands the value
    /// **back** on a lost race, so with `let _ = DENY_RUNTIME.set(rt)` the loser's runtime dropped on
    /// that statement — and the lazy path is called from inside `changes()`'s async body, where
    /// `Runtime::drop`'s blocking shutdown panics with "Cannot drop a runtime in a context where
    /// blocking is not allowed". The panic is in the handler body, so `map_join_error` cannot see it:
    /// the connection drops with no status at all, which is the I13 violation the design gate used to
    /// reject a `LazyLock` here.
    ///
    /// `prepare` closes it for the shipped binary by initialising before any listener binds. It was
    /// open for **every integration test** (`mount_server`/`spawn_server_from_engine` never call
    /// `prepare`) and for embedders, which the lazy path exists for.
    ///
    /// Asserted on the disposal itself rather than by racing two `init_deny_runtime` calls, because
    /// `DENY_RUNTIME` is process-global and any other test in this binary may have already won it.
    /// **The mutation is [`discard_losing_runtime`]'s body**: replace `shutdown_background()` with
    /// `drop(rt)` (or delete the function and go back to `let _ = ...set(rt)`) and this panics.
    ///
    /// Both flavours, because the two have different blocking-permission machinery and the original
    /// finding reproduced on both.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_losing_deny_runtime_is_discarded_legally_on_a_multi_thread_reactor() {
        discard_losing_runtime(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_name("tessera-deny-loser")
                .build()
                .unwrap(),
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_losing_deny_runtime_is_discarded_legally_on_a_current_thread_reactor() {
        discard_losing_runtime(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_name("tessera-deny-loser")
                .build()
                .unwrap(),
        );
    }
}
