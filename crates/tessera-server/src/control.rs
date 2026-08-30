//! The control (admin) plane (R5): `POST /control/ingest`, `POST /control/changes`,
//! `GET /control/status`. Bearer auth is the operator credential, applied **once, at the router**
//! by [`require_operator_credential`] rather than by each handler — see its doc for what that buys.
//!
//! **This plane is uniformly authenticated: every route on it requires the credential, with no
//! exemption.** `/healthz` and `/readyz` are *not* mounted here — they are on the viewer and session
//! listeners only (docs/decisions/0011-health-probes-off-control-plane.md; contracts §3.1). See
//! [`require_operator_credential`]'s "Why there is no exemption" for what that buys and the one
//! signal it gives up.
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

use tessera_engine::{
    AcceptError, DeclaredScalar, MetaView, Projection, ScalarType, Vocabularies, VocabularyKind,
    ABSENT_CODE, DENY_WINDOW_MAX_ENTRIES,
};
use tessera_lifecycle::{
    BatchArtifacts, BatchEdge, BatchMembership, ChangeOp, UnallocatedRow, WalScalar,
};

use tessera_types::{EntityId, TermId, TesseraId};

use crate::error::{
    map_accept_error, map_change_batch_error, map_join_error, map_store_error, ApiError,
};
use crate::health::is_ready;
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
/// The failure that shape produces is lifecycle §1.3's forbidden one, reintroduced *above* the
/// executor's priority lane where the executor cannot see it: with enough in-flight ingest requests,
/// a suppression's closure sits in tokio's FIFO behind them and never reaches the prioritised deny
/// queue at all.
///
/// The viewer plane is not exposed in the same way, and the asymmetry is why this separation is on
/// the deny side: `ComputeGate::admit` is `async` and is awaited **before** `spawn_blocking`, so a
/// queued viewport holds no blocking thread.
///
/// # Why a separate runtime rather than a big enough shared pool
///
/// The shared pool is in fact *sufficient by arithmetic*: `ingest_admission` bounds concurrent
/// ingest handlers, and `config::serving_blocking_threads` derives the serving pool as
/// `compute_admission + ingest_admission + BLOCKING_THREAD_RESERVE` rather than inheriting tokio's
/// default, so an admitted request can never find no thread. That is a statement about a derived
/// number, and it is exactly the kind of statement this lane must not depend on:
///
/// 1. **The arithmetic holds only where this workspace builds the runtime.** Embedders,
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
/// The repository's "structural, not disciplinary" test, and the admission bound does not retire it:
/// the bound limits a resource, this separates one. Deleting this runtime on the ground that ingest
/// is bounded would trade a structural property for a derived one, on the one lane where that trade
/// is not available.
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
/// `map_join_error`; the connection would drop with no status at all, violating I13a's "a panic is a
/// failed request, never an empty one".)
///
/// **The stored runtime is never dropped**, because a `OnceLock`'s value outlives every caller —
/// but that is only half the disposal question. `set` **returns the value back** when it loses a
/// race, so the *loser* of two concurrent [`init_deny_runtime`] calls is a live `Runtime` in hand at
/// a statement that may be inside `changes()`'s async body. `Runtime::drop` blocks, and dropping one
/// on a reactor thread panics with "Cannot drop a runtime in a context where blocking is not
/// allowed" — exactly the I13a shape above, on both tokio flavours. [`discard_losing_runtime`] is
/// where the loser goes instead.
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
/// **Stated rather than inherited.** The serving runtime's `max_blocking_threads` is a derived,
/// declared number (`config::serving_blocking_threads`), and this one would otherwise rest on
/// tokio's undeclared default. 512 *is* that default, so naming it changes no behaviour; what it
/// changes is that a tokio release cannot move the process's thread demand in silence.
///
/// **Deliberately not small**, and [`DENY_RUNTIME`]'s "Why it is NOT small" section is the
/// argument: isolation comes from the pool being *separate*, not from starving it. A thread here is
/// held for a whole request — external-id resolution, the enqueue, and the wait on the last
/// receipt — so the pool bounds concurrent `/control/changes` requests, and a small pool would make
/// one bulk revocation delay every other operator's suppression. Group commit shortens what a
/// thread waits for; it does not change what a thread is held across.
///
/// This number is also an operand of the pending-receipt bound: a handler holds at most
/// `DENY_WINDOW_MAX_ENTRIES` one-slot channels at a time (`run_changes` chunks its enqueue), so the
/// pool's worst case is that product rather than that many whole request bodies.
const DENY_MAX_BLOCKING_THREADS: usize = 512;

pub fn router(state: Arc<AppState>) -> Router {
    // The byte cap, enforced **here** rather than by a `body.len()` check in the handler, and the
    // difference is not stylistic.
    //
    // axum's own default request-body limit for the `Bytes` extractor is 2 MiB, well under this
    // deployment's `ingest_max_batch_bytes` (16 MiB by default). Left at the default, the configured
    // cap could never be the refusal a caller met, and an over-2-MiB batch would get a **413** — a
    // status outside contracts §3.1's closed code list. Setting the limit to the configured cap and
    // mapping the rejection ourselves closes both halves: buffering is bounded at exactly the number
    // the operator set, and the answer is the 422 §3.1's "bounds exceeded" row calls for. The
    // handler takes `Result<Bytes, _>` rather than `Bytes` so that mapping is possible at all.
    let ingest_route = post(ingest).layer(axum::extract::DefaultBodyLimit::max(
        state.ingest_max_batch_bytes,
    ));
    // The same remedy on the change lane. A bare `Json<..>` extractor answers axum's own **413** —
    // outside contracts §3.1's closed code list, with axum's own body, on the never-shed lane, to an
    // operator submitting tens of thousands of suppressions. The limit is stated here rather than
    // inherited so the 422's detail can name a number that is true.
    let changes_route =
        post(changes).layer(axum::extract::DefaultBodyLimit::max(CHANGES_MAX_BODY_BYTES));
    let router = Router::new()
        .route("/control/ingest", ingest_route)
        .route("/control/changes", changes_route)
        .route("/control/status", get(status))
        .route("/control/flush", post(flush))
        .route("/control/compact", post(compact))
        // A declaration is small — no batch shape and no body limit of its own, so it inherits
        // axum's 2 MiB default, which is three orders above the largest declaration anyone can
        // write. `PUT` rather than `POST` because the name is the identity and the operation is
        // refused rather than repeated if it is taken; there is no server-minted name to `POST` to.
        .route("/control/layers", axum::routing::put(register_layer))
        .route("/control/layers/{name}", axum::routing::delete(drop_layer))
        .route(
            "/control/layers/{name}/artifacts",
            axum::routing::put(publish_artifacts),
        );
    // The faults build's arming surface (decision 0071) — absent from a default build rather
    // than mounted and refusing, and above the credential layer below like every other route.
    #[cfg(feature = "fault-injection")]
    let router = router
        .route("/control/faults/arm", post(faults_arm))
        .route("/control/faults/arrivals", get(faults_arrivals))
        .route("/control/faults/release", post(faults_release));
    router
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
/// A per-handler `state.check_bearer(..)` is a rule spread across call sites, which the
/// repository's "structural, not disciplinary" test rejects — and it has already failed once on this
/// exact plane: [`status`] shipped returning `entity_id_high_water`, a global unmasked corpus-size
/// fact, to anyone who could reach the control listener, for no reason other than that the handler
/// did not call `check_bearer`. A handler that forgets is unauthenticated; a handler under this
/// layer cannot forget.
///
/// Three things this buys, in the order they matter:
///
/// 1. **No request body is buffered for an unauthenticated caller.** axum extractors run *inside*
///    the handler service, so with a per-handler check a 16 MiB `/control/ingest` body was resident
///    in full before `check_bearer` ever executed — and `ingest_max_batch_bytes` raises that window
///    to 16 MiB by default, 8× axum's own. A `tower` layer runs *outside* the extractors: this
///    returns 401 with the body still an unconsumed stream, so the exposure is closed in code rather
///    than by deployment posture.
///
///    **What it does NOT close.** A caller holding a *valid* operator credential still buffers up
///    to `ingest_max_batch_bytes` per in-flight request, and `axum::serve` applies no connection or
///    concurrency cap, so the *count* of connections remains unbounded. What each one costs is
///    bounded by `config::INGEST_MAX_BATCH_BYTES_CEILING`; how many there are is bounded by
///    deployment posture — the control plane is a unix socket reachable only by admin systems, and
///    where it is exposed more widely the connection bound is a reverse proxy's (SA §8). That
///    constant records the two in-process mechanisms assessed for the count and why each was
///    declined.
/// 2. **A control route cannot be added unauthenticated by omission.** The layer wraps the whole
///    router, so a `.route(..)` added to [`router`] tomorrow is behind the credential the moment it
///    exists. There is no opt-out to reach for and no list to be added to by accident.
/// 3. **401 ahead of every 422 and 429 is a property of the router, not of a handler.**
///    [`ingest`]'s doc enumerates that ordering and `backpressure_is_invisible_before_auth`
///    exercises it; neither depends on where a handler happens to put its check.
///
/// # Why there is no exemption
///
/// The probes live on the viewer and session listeners and this plane carries neither
/// (docs/decisions/0011-health-probes-off-control-plane.md), so the rule this layer enforces is
/// *every route on this plane requires the credential* — strictly stronger than *every route except
/// these two*, and with no carve-out a later route can fall into. The operational consequence is
/// that the control listener can be firewalled to admin-only with no health-probe hole in the
/// rule.
///
/// **Nothing is lost by not serving the probes here.** `healthz` is a constant and `readyz` is a
/// bare `StatusCode` with no body (`health.rs`), so all three listeners answered identically —
/// mounting it three times was three copies of one bit. The richer operator view (posture *string*,
/// executor counters, WAL appends and fsyncs, cache stats, ingest admission gauges)
/// is on the bearer-gated `/control/status` and is untouched; the boolean/string split is SA §9's.
///
/// **The one bit that IS lost, written down so it is not rediscovered as a surprise.** An
/// unauthenticated probe can no longer observe "the control listener is accepting connections". A
/// control listener that failed *independently* of the other two — its socket path removed, its
/// accept task panicked — would leave ingest and denies unable to land with no unauthenticated
/// signal of it. The answer is that anything that cares about the control plane is an admin system
/// that holds the operator credential, and one authenticated `/control/status` call answers the same
/// question *and* says why. A public liveness probe on this listener would answer only the first
/// half, at the cost of a permanent hole in the plane's firewall rule.
///
/// The layer sits ahead of the router's 404, so an unrouted path on the control listener answers 401
/// rather than 404 — `every_path_on_the_control_listener_needs_the_credential` pins that, including
/// for `/healthz` and `/readyz` themselves. With no exemption there is nothing left for a path to
/// match *nearly*, and an unauthenticated caller cannot map the plane's surface by probing: the
/// plane discloses nothing about itself, its removed routes included.
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
    // Unconditional: no path, routed or not, is exempt. See "Why there is no exemption" above.
    state.check_bearer(bearer_token(request.headers()), &state.operator_credential)?;
    Ok(next.run(request).await)
}

/// The projection a layer's declared views were placed under.
///
/// A shape layer whose views declare different projections is refused at the declaration
/// (`polygon-membership.md` §4.3), so every view here agrees and any one of them answers. What is
/// not safe is reading the *bundle's* first view: a layer need not be declared on it, and a shape
/// placed by a projection none of its own views declares holds the wrong rows with nothing saying
/// so.
fn layer_projection(
    meta: &tessera_engine::EngineMeta,
    views: &[&str],
) -> Result<tessera_engine::Projection, ApiError> {
    let first = views
        .first()
        .ok_or_else(|| {
            ApiError::Contract("this layer declares no view to publish a shape into".into())
        })?;
    meta.projection_of(first)
        .ok_or_else(|| ApiError::Unknown(format!("unknown view '{first}'")))
}

/// Every route [`router`] mounts, as `(method, path)` — the subject of
/// `every_control_route_requires_the_operator_credential`.
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
    ("POST", "/control/flush"),
    ("POST", "/control/compact"),
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
/// The first row of `items` whose coordinates fall outside `q`, if any.
///
/// **Inclusive of the maximum**, matching `fixed32`'s own clamp domain: a point exactly at
/// `x_max` quantises to the top of the grid and is a legitimate position, not an escape.
///
/// `Retry-After` for a buffer-occupancy 429.
///
/// **A flush period, not the queue estimator's figure.** `estimate_retry_after_s` models a client
/// queued behind work the executor is draining now; this client is queued behind a *flush*, which
/// happens on the tick and not before it, so the honest advice is "after the next tick". A default
/// tick is 90 s, and a client told 1 s would simply be refused ninety more times.
const INGEST_BUFFER_FULL_RETRY_AFTER_S: u64 = 90;

const CHANGES_MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
}

/// Contracts §1: external IDs are caller-supplied byte strings, capped at **≤ 64 bytes**.
/// Over-length is a typed error here and at build, never a truncation — truncating two callers'
/// keys down to a shared 64-byte prefix would silently merge two different items into one
/// entity, and sidecar disk scales linearly with key length, so the cap is load-bearing, not
/// cosmetic. `/control/ingest` is the only caller-supplied-bytes path in this workspace (the
/// build's external-id representation is fixed at exactly 8 bytes — `tessera-build`'s
/// `BuildError::ExternalIdTooLong` cannot be reached by any build input), so this is where the
/// cap is actually enforced and tested.
const EXTERNAL_ID_MAX_LEN: usize = 64;

#[derive(Debug)]
struct RawIngestItem {
    /// Optional (contracts §3.4): `None` when the caller supplied no external id. Such an item
    /// gets no sidecar entry and is addressable only by its `tessera_id` (returned per row in
    /// [`IngestResp`]).
    external_id: Option<Vec<u8>>,
    /// **The view's frame, never longitude and latitude** — the transform has already run
    /// (`projections.md` §3). Everything downstream of the decode reads a frame coordinate: the
    /// engine's out-of-frame check, the WAL record, the buffer and the flush's quantiser.
    x: f64,
    y: f64,
    access: Vec<u8>,
    scalars: Vec<WalScalar>,
}

/// The column names this schema gives a meaning of their own whatever the view, plus the
/// coordinate pair [`coordinate_columns`] resolves; everything else in a batch is a
/// caller-declared scalar **or a declared layer's name** (see [`parse_ingest_batch`]).
const RESERVED_COLUMNS: [&str; 3] = ["external_id", "access", "node_id"];

/// What this view's coordinate columns are called, and what the wrong spelling would have meant.
///
/// **A projected view spells them `lon` and `lat`; a view with no projection spells them `x` and
/// `y`** (`projections.md` §2). Longitude-then-latitude is the order GeoJSON and WKT use and the
/// opposite of the order many sources publish, and a corpus written with the two exchanged is
/// silently mirrored about the diagonal — so the axes are named for what they hold rather than
/// documented. The build applies the same rule to a points file's columns
/// (`tessera_build::config`'s `compile_projected_fields`), and it has to exist on both paths or a
/// projected view is something that can be built correctly and ingested into wrongly
/// (decision 0091).
fn coordinate_columns(projection: Projection) -> (&'static str, &'static str) {
    match projection {
        Projection::None => ("x", "y"),
        _ => ("lon", "lat"),
    }
}

/// One ingest batch, decoded: its rows, what its membership columns said, and how many of those
/// rows the view's projection clipped.
struct ParsedBatch {
    items: Vec<RawIngestItem>,
    artifacts: BatchArtifacts,
    /// Rows whose latitude fell outside the projection's own domain and were moved onto the
    /// frame's edge (`projections.md` §7). Always `0` under `projection = "none"`, which has no
    /// domain.
    clipped: u64,
}

/// A column named for a declared layer, and what its cells mean.
///
/// **Named for the layer, exactly as an attribute column is named for the attribute** — the
/// declaration's own `name`, never an acquisition-side spelling. `fields` on `[layer.members]` maps
/// a *file's* column name onto the canonical meaning and is build-only for that reason
/// (`configuration.md` §2): a deployment that never builds has no file to map from, and a name a
/// running node had to be told about could not be checked against anything.
struct MembershipColumn<'a> {
    layer: &'a str,
    meaning: tessera_types::layer::ListMeaning,
    cells: KeyCells<'a>,
}

/// A key column's shape: one artifact per row, or a list of them.
enum KeyCells<'a> {
    Scalar(&'a dyn Array),
    /// A `List`, whose rows may differ in length — which is what a lineage is.
    Variable(&'a arrow::array::ListArray),
    /// A `FixedSizeList`, every row of the arity its own type states.
    Fixed(&'a arrow::array::FixedSizeListArray),
}

impl KeyCells<'_> {
    /// The element array a row's entries are read out of — the column itself where it is a scalar.
    fn values(&self) -> &dyn Array {
        match self {
            KeyCells::Scalar(array) => *array,
            KeyCells::Variable(list) => {
                let values: &Arc<dyn Array> = list.values();
                values.as_ref()
            }
            KeyCells::Fixed(list) => {
                let values: &Arc<dyn Array> = list.values();
                values.as_ref()
            }
        }
    }

    /// Whether the cells are lists — a scalar carries one artifact per row and has no arity to
    /// disagree with a declaration.
    fn is_list(&self) -> bool {
        !matches!(self, KeyCells::Scalar(_))
    }

    /// The range of `values()` one row occupies, or `None` where the row named no artifact at all.
    ///
    /// **A null cell and an empty list are the whole row's "in no artifact"**, which is the scalar
    /// rule applied to a cell that holds no key: a point may be in no artifact at any resolution,
    /// and a clusterer that emitted nothing for it is the ordinary way of saying so.
    fn entries(&self, row: usize) -> Option<std::ops::Range<usize>> {
        let (start, end) = match self {
            KeyCells::Scalar(_) => (row, row + 1),
            KeyCells::Variable(list) => {
                if list.is_null(row) {
                    return None;
                }
                let offsets = list.value_offsets();
                (offsets[row] as usize, offsets[row + 1] as usize)
            }
            KeyCells::Fixed(list) => {
                if list.is_null(row) {
                    return None;
                }
                let start = list.value_offset(row) as usize;
                (start, start + list.value_length() as usize)
            }
        };
        (start != end).then_some(start..end)
    }
}

/// The key one cell names, or `None` where it names no artifact.
///
/// **Text or an integer, and `null` or `-1` means this point is in no artifact**
/// (`artifacts-from-points.md` §2). An integer key is read as its decimal spelling, so `3` and `"3"`
/// name one artifact — the rule is [`tessera_types::layer::integer_key`]'s, which is also what a
/// build reads a member table's key column by, because a membership spelled two ways must not
/// resolve two ways.
fn member_key_at(values: &dyn Array, index: usize) -> Option<String> {
    use arrow::array::{
        Int16Array, Int32Array, Int64Array, Int8Array, StringArray, UInt16Array, UInt32Array,
        UInt64Array, UInt8Array,
    };
    if values.is_null(index) {
        return None;
    }
    let any = values.as_any();
    let integer: i128 = if let Some(a) = any.downcast_ref::<StringArray>() {
        return Some(a.value(index).to_string());
    } else if let Some(a) = any.downcast_ref::<Int8Array>() {
        a.value(index) as i128
    } else if let Some(a) = any.downcast_ref::<Int16Array>() {
        a.value(index) as i128
    } else if let Some(a) = any.downcast_ref::<Int32Array>() {
        a.value(index) as i128
    } else if let Some(a) = any.downcast_ref::<Int64Array>() {
        a.value(index) as i128
    } else if let Some(a) = any.downcast_ref::<UInt8Array>() {
        a.value(index) as i128
    } else if let Some(a) = any.downcast_ref::<UInt16Array>() {
        a.value(index) as i128
    } else if let Some(a) = any.downcast_ref::<UInt32Array>() {
        a.value(index) as i128
    } else {
        // The last arm is `u64` and the fallthrough is unreachable: `membership_column` refuses any
        // other element type before a row is read.
        any.downcast_ref::<UInt64Array>()?.value(index) as i128
    };
    tessera_types::layer::integer_key(integer)
}

/// Read one column named for a layer, at the shapes that layer may carry.
///
/// **Two refusals, and both are the declaration and the data disagreeing about arity.** A
/// `FixedSizeList` states its length in its own type, so a levelled layer's is checked once here; a
/// nested layer's lineage is as deep as each point's own branch and has no fixed arity at all. A
/// plain list states its length a row at a time, and that check is in the row loop — reading the
/// type alone would refuse every producer whose Arrow binding writes a plain list, which is most of
/// them.
fn membership_column<'a>(
    name: &'a str,
    column: &'a Arc<dyn Array>,
    declaration: &tessera_types::layer::LayerDeclaration,
) -> Result<MembershipColumn<'a>, ApiError> {
    use arrow::array::{FixedSizeListArray, ListArray};
    use arrow::datatypes::DataType;

    // A predicate layer's membership is *evaluated* per request, so there is nothing for a column
    // to say: a stored answer beside a live predicate is what the artifact store refuses a
    // publication for, and this is the same refusal one step earlier, where the batch can still be
    // rejected without effect.
    if declaration.membership != tessera_types::layer::MembershipSource::Enumerated {
        return Err(ApiError::Contract(format!(
            "ingest body: column '{name}' names a layer whose membership is evaluated per \
             request rather than enumerated — there is no stored membership for a point to join, \
             and one written beside the predicate would diverge from it at the first write"
        )));
    }

    let meaning = declaration.list_meaning();
    let cells = match column.data_type() {
        DataType::List(_) => KeyCells::Variable(
            column
                .as_any()
                .downcast_ref::<ListArray>()
                .expect("a List column downcasts to a ListArray"),
        ),
        DataType::FixedSizeList(_, size) => {
            match meaning {
                tessera_types::layer::ListMeaning::Lineage => {
                    return Err(ApiError::Contract(format!(
                        "ingest body: column '{name}' is a fixed-size list of {size} and that \
                         layer is declared nested, whose lineage is as deep as each point's own \
                         branch — a fixed arity is one entry per level, which is the stacked and \
                         tiered shape"
                    )))
                }
                tessera_types::layer::ListMeaning::Levelled { levels, .. } if *size as usize != levels => {
                    return Err(ApiError::Contract(format!(
                        "ingest body: column '{name}' is a fixed-size list of {size} and that \
                         layer declares {levels} levels. Entry k is the artifact at level k, so \
                         the two counts are one number written twice"
                    )))
                }
                _ => {}
            }
            KeyCells::Fixed(
                column
                    .as_any()
                    .downcast_ref::<FixedSizeListArray>()
                    .expect("a FixedSizeList column downcasts to a FixedSizeListArray"),
            )
        }
        _ => KeyCells::Scalar(column.as_ref()),
    };
    let element = cells.values().data_type().clone();
    if !matches!(
        element,
        DataType::Utf8
            | DataType::Int8
            | DataType::Int16
            | DataType::Int32
            | DataType::Int64
            | DataType::UInt8
            | DataType::UInt16
            | DataType::UInt32
            | DataType::UInt64
    ) {
        return Err(ApiError::Contract(format!(
            "ingest body: column '{name}' names a layer and carries {element:?}; a member key is \
             text or an integer — an integer key is read as its decimal spelling, so `3` and \
             \"3\" name one artifact"
        )));
    }
    Ok(MembershipColumn {
        layer: name,
        meaning,
        cells,
    })
}

/// What one batch's membership columns said, gathered as the executor takes it.
///
/// **Keyed by `(layer, level, key)` and carrying row positions**, because a batch's entity ids do
/// not exist until its commit window closes. The executor resolves the key to an ordinal at
/// admission and turns the positions into entities after the assignment — see
/// [`tessera_lifecycle::BatchMembership`].
#[derive(Default)]
struct MembershipTally {
    rows_of: std::collections::BTreeMap<(String, u32, String), Vec<u32>>,
    edges: std::collections::BTreeSet<(String, u32, String, String)>,
}

impl MembershipTally {
    /// One row's cells of one membership column.
    ///
    /// `offset` is where this batch's rows start in the request's own numbering, since an Arrow IPC
    /// stream may carry several record batches and the executor indexes one flat list of rows.
    fn read(
        &mut self,
        column: &MembershipColumn<'_>,
        row: usize,
        offset: usize,
    ) -> Result<(), ApiError> {
        let Some(entries) = column.cells.entries(row) else {
            return Ok(());
        };
        // **The declaration and the data must agree — where the data is a list.** A stacked or
        // tiered layer's list is one entry per declared level, that being what makes entry k mean
        // level k, so a row of any other length is a lineage against a levelled declaration and
        // guessing which of the two the caller meant would store a hierarchy they did not write.
        //
        // **A scalar is not a short list**: it names one artifact at level 0, which is exactly what
        // a member table with no `level` column means on a levelled layer. Applying the arity check
        // to it would make the two entry points read one spelling two ways, which is the drift this
        // whole column is written against.
        if let Some(levels) = column.meaning.arity().filter(|_| column.cells.is_list()) {
            if entries.len() != levels {
                return Err(ApiError::Contract(format!(
                    "ingest body: column '{}' names {} artifacts at row {row} and the layer \
                     declares {levels} levels. A stacked or tiered layer's column is one entry \
                     per level, nullable where the point is in no artifact at that resolution",
                    column.layer,
                    entries.len(),
                )));
            }
        }
        // **Each entry carries its own position**, so the edge below reads the child's level off
        // the entry rather than searching for its key — a lineage may legitimately name one key
        // twice, and a search would then charge the edge to the wrong level.
        let values = column.cells.values();
        let keys: Vec<Option<(u32, String)>> = entries
            .enumerate()
            .map(|(position, index)| {
                member_key_at(values, index).map(|key| (column.meaning.level_of(position), key))
            })
            .collect();
        for (level, key) in keys.iter().flatten() {
            let at = self
                .rows_of
                .entry((column.layer.to_string(), *level, key.clone()))
                .or_default();
            // A row naming one artifact twice — a lineage that repeats a key — joins it once.
            let index = (offset + row) as u32;
            if at.last() != Some(&index) {
                at.push(index);
            }
        }
        if column.meaning.declares_edges() {
            // The adjacency is `tessera_types::layer::parent_edges`' — the same function a build
            // reads a member table's list column by, which is what keeps the two entry points from
            // inferring different trees from one file.
            for ((_, parent), (level, child)) in tessera_types::layer::parent_edges(&keys) {
                self.edges.insert((
                    column.layer.to_string(),
                    *level,
                    child.clone(),
                    parent.clone(),
                ));
            }
        }
        Ok(())
    }

    fn into_artifacts(self) -> BatchArtifacts {
        BatchArtifacts {
            memberships: self
                .rows_of
                .into_iter()
                .map(|((layer, level, key), rows)| BatchMembership {
                    layer,
                    level,
                    key,
                    rows,
                })
                .collect(),
            edges: self
                .edges
                .into_iter()
                .map(|(layer, level, child, parent)| BatchEdge {
                    layer,
                    level,
                    child,
                    parent,
                })
                .collect(),
        }
    }
}

/// One value out of an ingest batch's column, read **at the column's declared wire type**
/// ([`DeclaredScalar::wire_type`]) — `None` when the Arrow column is not that type, which the
/// caller refuses naming both types rather than by dropping the column.
///
/// # The declaration drives the decode, and the match is exhaustive over `ScalarType`
///
/// Two constructions have failed here, and this shape exists against both:
///
/// * **A second type table.** An earlier form carried its own type spellings — `uint64` where the
///   flush path parsed `u64` — so a manifest one path accepted was one the other refused. The
///   spellings are gone entirely: the expected type arrives as a [`ScalarType`], and the one
///   spelling in any refusal is [`ScalarType::arrow_type_name`]'s.
/// * **Inferring the type from the array.** The successor answered "which type is this column?"
///   by a chain of downcasts, and a chain holds exactly the types its author remembered — the
///   compiler has nothing to check it against. Six declarable types (`bool`, `i8`, `i16`, `i32`,
///   `f64`, `timestamp_us`) were declarable, buildable and un-ingestable that way,
///   `timestamp_us` invisibly so: Arrow's `TimestampMicrosecondArray` is a distinct type that no
///   `Int64Array` downcast reaches.
///
/// Matching on [`ScalarType`] makes the completeness structural rather than remembered: a type
/// added to the declarable set fails to compile here until this function says what an ingest
/// batch carries for it. `Keyword` and `Text` never arrive — `wire_type` maps both to `Utf8` —
/// but decode identically rather than panicking, because a string *is* their wire form.
fn scalar_at(col: &dyn Array, row: usize, ty: ScalarType) -> Option<WalScalar> {
    use arrow::array::{
        BooleanArray, Float32Array, Float64Array, Int16Array, Int32Array, Int64Array, Int8Array,
        StringArray, TimestampMicrosecondArray, UInt16Array, UInt32Array, UInt64Array, UInt8Array,
    };
    let any = col.as_any();
    Some(match ty {
        ScalarType::Bool => WalScalar::Bool(any.downcast_ref::<BooleanArray>()?.value(row)),
        ScalarType::U8 => WalScalar::U8(any.downcast_ref::<UInt8Array>()?.value(row)),
        ScalarType::U16 => WalScalar::U16(any.downcast_ref::<UInt16Array>()?.value(row)),
        ScalarType::U32 => WalScalar::U32(any.downcast_ref::<UInt32Array>()?.value(row)),
        ScalarType::U64 => WalScalar::U64(any.downcast_ref::<UInt64Array>()?.value(row)),
        ScalarType::I8 => WalScalar::I8(any.downcast_ref::<Int8Array>()?.value(row)),
        ScalarType::I16 => WalScalar::I16(any.downcast_ref::<Int16Array>()?.value(row)),
        ScalarType::I32 => WalScalar::I32(any.downcast_ref::<Int32Array>()?.value(row)),
        ScalarType::I64 => WalScalar::I64(any.downcast_ref::<Int64Array>()?.value(row)),
        ScalarType::F32 => WalScalar::F32(any.downcast_ref::<Float32Array>()?.value(row)),
        ScalarType::F64 => WalScalar::F64(any.downcast_ref::<Float64Array>()?.value(row)),
        ScalarType::TimestampUs => {
            WalScalar::TimestampUs(any.downcast_ref::<TimestampMicrosecondArray>()?.value(row))
        }
        ScalarType::Utf8 | ScalarType::Keyword | ScalarType::Text => {
            WalScalar::Utf8(any.downcast_ref::<StringArray>()?.value(row).to_string())
        }
    })
}

/// One category cell: its value key resolved to the pinned code, at the column's declared width.
///
/// **Resolution, never minting.** A handler that minted would let two requests racing one novel key
/// draw two codes for it, splitting its rows between them, and whichever binding survived would
/// recolour the other's. Minting happens once, on the write executor, where windows close serially
/// (write-path §1.1).
fn category_code(
    col: &dyn Array,
    row: usize,
    declared: &DeclaredScalar,
    vocabulary: &str,
    vocabularies: &Vocabularies,
) -> Result<WalScalar, ApiError> {
    use arrow::array::StringArray;
    let keys = col
        .as_any()
        .downcast_ref::<StringArray>()
        .expect("a category column was validated as utf8 above");
    if keys.is_null(row) {
        return Ok(code_at(declared.arrow_type, ABSENT_CODE));
    }
    let key = keys.value(row);
    if key.is_empty() {
        return Err(ApiError::Contract(format!(
            "ingest body: column '{}' carries the empty string, which is not a value key. An \
             item with no value for this column carries null, which is stored as *absent*; \
             minting a code for the empty string would make a typo a category \
             (per-point-attributes §3.4)",
            declared.name
        )));
    }
    let minter = vocabularies.get(vocabulary).ok_or_else(|| {
        ApiError::Contract(format!(
            "ingest body: column '{}' names vocabulary '{vocabulary}', which this bundle does \
             not carry",
            declared.name
        ))
    })?;
    if let Some(code) = minter.code_of(key) {
        return Ok(code_at(declared.arrow_type, code));
    }
    match minter.kind() {
        // Declare-then-use: the value set is closed, so a key nothing binds is a typo — and a
        // category carries properties and, through its postings, a visibility consequence. The
        // refusal is here rather than on the executor because the whole batch can still be
        // rejected without effect at this point, which is what a 422 promises.
        VocabularyKind::Declared => Err(ApiError::Contract(format!(
            "ingest body: column '{}' carries value '{key}', which vocabulary '{vocabulary}' \
             does not list. Under `vocabulary = \"declared\"` there is no auto-mint: a category \
             carries properties and, through its postings, a visibility consequence, so a typo \
             must not create one (per-point-attributes §5)",
            declared.name
        ))),
        // **The key travels as a key.** This handler must not mint: two requests racing one novel
        // key would each draw, and that key would end up with two codes and its rows split
        // between them. The commit-window close resolves it — serially, against the live bindings
        // — and the row's scalar becomes the code there, before the WAL append.
        VocabularyKind::Discovered => Ok(WalScalar::Utf8(key.to_string())),
    }
}

/// A code at its column's declared width. `is_category_width` admits `u8`/`u16`/`u32` only, so the
/// fallthrough is `u32` — the widest, which cannot truncate a code the other two could hold.
fn code_at(width: ScalarType, code: u32) -> WalScalar {
    match width {
        ScalarType::U8 => WalScalar::U8(code as u8),
        ScalarType::U16 => WalScalar::U16(code as u16),
        _ => WalScalar::U32(code),
    }
}

/// Parse `/control/ingest`'s body: one Arrow IPC stream, schema
/// `(external_id: binary, x: float32|float64, y: float32|float64, access: utf8, node_id: utf8?,
/// ...scalars)` (R5). The coordinate columns take either float width and the narrower is widened —
/// see [`coordinate_col`] for why the widening runs in that one direction. `node_id` is accepted
/// — so a well-formed client request is never rejected for including it — but not stored: `WalRow`
/// has no `node_id` field, because a buffered item has no row geometry until the next build and
/// `node_id` is a segment-column concept.
///
/// # The scalar tail is validated against `MANIFEST.declared_scalars`, and misalignment is a 422
///
/// A row's scalars are stored **positionally**, against the manifest's declared order — nothing
/// downstream carries a name. So a batch whose scalar columns are not exactly the declared set, in
/// no matter what order, cannot be read back correctly, and three defects are the same defect:
///
/// * a column the manifest does not declare;
/// * a declared column the batch omits;
/// * a declared column present at the wrong arrow type.
///
/// Each is refused with **422 naming the column** (contracts §3.1's "malformed request"), and the
/// scalar vector is built in **declared** order rather than schema order, which is what makes the
/// positional read safe. Silently dropping a column would shorten the vector and shift every later
/// scalar by one: positional misalignment wearing a success's clothes, acknowledged with a 200.
///
/// # A category arrives as its key, and the key is checked for membership
///
/// The expected type is [`DeclaredScalar::wire_type`], not the declared width: a category column
/// is `utf8` value keys on the wire, whatever width stores its codes. Codes are the server's to
/// assign (per-point-attributes §3.1, §5), so a caller supplying one would be the minting
/// authority, and the server could then guarantee neither the scatter nor never-reuse that §3.4
/// exists for.
///
/// **This is what makes a category's value checkable at all.** A code can only be range-checked —
/// a `u16` column accepted any `u16`, so an unassigned code, a `reserved` code or a typo was stored
/// with no error anywhere and the row carried a code no key explains. A key can be
/// membership-checked, and membership is the rule: an unknown key under `value_set = "closed"`
/// is a 422 naming the column and the key, whole batch without effect (declare-then-use, §5,
/// views §80).
///
/// It also makes a schema/client disagreement visible: a plain `u16` scalar and a `u16` category
/// are now different types on the wire, so a client that thinks a column is one when the bundle
/// says the other gets a 422 naming it rather than plausible integers stored as codes.
///
/// A **null** key is *absent* — [`ABSENT_CODE`], the reserved sentinel (§3.6). The **empty string**
/// is not: it is what an unset field and a client bug both produce, so it is refused rather than
/// folded into absence, which would accept the same defect silently.
///
/// # A column named for a declared layer is that point's artifacts
///
/// The acceptance rule is **reserved, or a declared attribute's name, or a declared layer's name**
/// (`artifacts-from-points.md` §6.2) — the third being what
/// [decision 0091](../../../docs/decisions/0091-build-is-ingest-into-an-empty-database.md) obliges:
/// a point may name its artifacts in a file, so it may name them on the wire. The cell is a key, or
/// a list of keys, on exactly the rules a build reads a member table by — `tessera_types::layer`
/// holds them, and holds them for both readers.
///
/// **The layer's own `name`, exactly as an attribute column is named for the attribute's `name`.**
/// `fields` on `[layer.members]` renames a *file's* columns and is build-only for the same reason
/// `source` is: it says where rows come from rather than what they mean.
///
/// Reserved names are matched first and declared scalars second, so a layer sharing a name with
/// either is read as the other — a layer called `x` cannot make the geometry column mean a cluster.
fn parse_ingest_batch(
    body: &[u8],
    projection: Projection,
    declared: &[DeclaredScalar],
    vocabularies: &Vocabularies,
    layer_of: &dyn Fn(&str) -> Option<tessera_types::layer::LayerDeclaration>,
) -> Result<ParsedBatch, ApiError> {
    let cursor = std::io::Cursor::new(body);
    let reader = arrow::ipc::reader::StreamReader::try_new(cursor, None).map_err(|e| {
        ApiError::Contract(format!("ingest body is not a valid Arrow IPC stream: {e}"))
    })?;

    let (x_name, y_name) = coordinate_columns(projection);
    let mut items = Vec::new();
    let mut tally = MembershipTally::default();
    let mut clipped = 0u64;
    for batch in reader {
        let batch = batch
            .map_err(|e| ApiError::Contract(format!("ingest body: arrow decode error: {e}")))?;
        let schema = batch.schema();
        // Where this record batch's rows start in the request's own row numbering — what a
        // membership names, since the executor indexes one flat list of rows per batch id.
        let offset = items.len();

        let ext = optional_binary_col(&batch, "external_id")?;
        // **The spelling is checked before the columns are read**, so a batch that used the other
        // one meets a refusal naming what this view calls its axes rather than a bare "column 'x'
        // missing". Only fired where the right column is absent, so a `projection = "none"` view
        // whose declared scalars happen to include a `lon` is unaffected: this is the surface
        // every existing ingest uses.
        for (wrong, right) in wrong_spellings(projection) {
            if batch.column_by_name(wrong).is_some() && batch.column_by_name(right).is_none() {
                return Err(ApiError::Contract(format!(
                    "ingest body: {}, so its coordinate columns are '{x_name}' and '{y_name}', \
                     not '{wrong}' (projections.md §2, §3). The axes are named for what they hold \
                     because a corpus written with longitude and latitude exchanged is mirrored \
                     about the diagonal and malformed in no other way; rename '{wrong}' to \
                     '{right}'",
                    match projection {
                        Projection::None =>
                            "this view declares no projection, so it has no longitude".to_string(),
                        _ => format!("this view is projected `{}`", projection.name()),
                    }
                )));
            }
        }
        let mut x = coordinate_col(&batch, x_name)?;
        let mut y = coordinate_col(&batch, y_name)?;
        // **The transform runs here, at the boundary, before anything else looks at the numbers**
        // (`projections.md` §3) — the same place `tessera_build::input` runs it, which is what
        // makes a projected view ingestable rather than only buildable (decision 0091).
        clipped += project_columns(projection, &mut x, &mut y)?;
        let access = utf8_col(&batch, "access")?;

        // Whole-batch schema validation, before a single row is read: a batch whose scalar tail
        // does not match the declaration has no effect at all, exactly as a duplicate 409 does.
        let mut memberships: Vec<MembershipColumn> = Vec::new();
        let mut declarations = Vec::new();
        for field in schema.fields() {
            let name = field.name().as_str();
            if RESERVED_COLUMNS.contains(&name)
                || name == x_name
                || name == y_name
                || declared.iter().any(|d| d.name == name)
            {
                continue;
            }
            let Some(declaration) = layer_of(name) else {
                return Err(ApiError::Contract(format!(
                    "ingest body: column '{name}' is neither in MANIFEST.declared_scalars nor the \
                     name of a registered layer (contracts §2.2). Scalars are stored positionally \
                     against the declared order, so an undeclared column is refused rather than \
                     dropped — dropping it would shift every later scalar by one and acknowledge \
                     that with a 200"
                )));
            };
            declarations.push((name.to_string(), declaration));
        }
        for (name, declaration) in &declarations {
            let column = batch
                .column_by_name(name)
                .expect("the column was found in this batch's own schema");
            memberships.push(membership_column(name, column, declaration)?);
        }
        for d in declared {
            let Some(col) = batch.column_by_name(&d.name) else {
                return Err(ApiError::Contract(format!(
                    "ingest body: declared scalar '{}' is missing from this batch \
                     (contracts §2.2). Every declared column must be present: the scalar tail is \
                     read back by position, so an omission misaligns it exactly as a spurious \
                     column does",
                    d.name
                )));
            };
            // One row's worth is enough to identify the column's type, and a batch with no rows has
            // no scalar to mistype. The decode is keyed by the declaration (see `scalar_at`), so
            // "wrong type" and "a type this build cannot store" are one refusal: either way the
            // column is not what the manifest says an ingest batch carries for it.
            let expected = d.wire_type();
            if batch.num_rows() > 0 && scalar_at(col.as_ref(), 0, expected).is_none() {
                return Err(ApiError::Contract(format!(
                    "ingest body: column '{}' is {:?}, but MANIFEST.declared_scalars declares \
                     it {} (contracts §2.6); refused rather than dropped",
                    d.name,
                    col.data_type(),
                    expected.arrow_type_name()
                )));
            }
        }

        for i in 0..batch.num_rows() {
            // The artifacts this row names, read before its scalars so a malformed membership
            // column refuses the batch with nothing decoded into `items` — the whole-batch rule
            // every other refusal here is held to.
            for column in &memberships {
                tally.read(column, i, offset)?;
            }
            // Built in DECLARED order, not schema order — the vector is read back by position and
            // nothing downstream carries a name. Every column is present and correctly typed by the
            // validation above, so neither `expect` here can fire on a caller's input.
            let mut scalars = Vec::with_capacity(declared.len());
            for d in declared {
                let col = batch
                    .column_by_name(&d.name)
                    .expect("every declared column was found by the validation above");
                let value = match d.vocabulary.as_deref() {
                    // A category's absence is in band and `category_code` already spends it: null
                    // resolves to the reserved code 0, which its vocabulary keeps out of the value
                    // space.
                    Some(vocabulary) => {
                        category_code(col.as_ref(), i, d, vocabulary, vocabularies)?
                    }
                    // **Null is absence, and it must be carried rather than read through.**
                    // `a.value(row)` on a null slot returns whatever the values buffer holds
                    // there — 0 for every numeric width — so reading without this check stores an
                    // item with no score as one scoring zero, present and indistinguishable. It
                    // then matches `{gte: -10, lte: 10}`, which is a wrong answer rather than a
                    // missing feature (decision 0064). Every other family has somewhere in band to
                    // put absence; a number has no spare bit pattern, so it travels beside the
                    // value as `WalScalar::Null`.
                    None if col.is_null(i) => WalScalar::Null,
                    None => scalar_at(col.as_ref(), i, d.wire_type())
                        .expect("every declared column's type was checked above"),
                };
                scalars.push(value);
            }
            // Contracts §3.4: `external_id` is optional. Neither a missing column nor a null
            // within the column is an error -- both simply mean this item has no caller-supplied
            // external id and is addressable only by its `tessera_id`.
            let external_id = match &ext {
                Some(arr) if !arr.is_null(i) => Some(arr.value(i).to_vec()),
                _ => None,
            };
            // Contracts §1: a typed error, never a truncation -- see `EXTERNAL_ID_MAX_LEN`'s
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
                x: x[i],
                y: y[i],
                access: access.value(i).as_bytes().to_vec(),
                scalars,
            });
        }
    }
    Ok(ParsedBatch {
        items,
        artifacts: tally.into_artifacts(),
        clipped,
    })
}

/// The coordinate columns a batch for this view must *not* carry, each paired with what it should
/// have been called.
///
/// `x`/`y` and `lon`/`lat` are the only two spellings, so each view refuses exactly the other one
/// and the pair is total rather than a list that could be empty.
fn wrong_spellings(projection: Projection) -> [(&'static str, &'static str); 2] {
    match projection {
        Projection::None => [("lon", "x"), ("lat", "y")],
        _ => [("x", "lon"), ("y", "lat")],
    }
}

/// Project a batch's coordinate columns in place, returning how many rows the projection
/// **clipped** (`projections.md` §3, §7).
///
/// # Two things go wrong here and they are not the same thing
///
/// A coordinate outside WGS84's own range is **not a coordinate** and is refused, exactly as the
/// build refuses it (`projections.md` §2): the accepted input coordinate system is longitude
/// within ±180 and latitude within ±90, and a caller holding anything else converts before
/// arriving.
///
/// A latitude inside that range but outside the *projection's* domain — beyond ±85.0511287798066°
/// for `web_mercator` — is **clipped onto the frame's edge, counted, and never refused** (§7). The
/// same row builds, and a row a build accepts and an ingest rejects is a defect rather than a
/// policy. Clipping never earns a refusal at any proportion: a clipped point's position is the
/// projection's own domain boundary, which no choice of frame moves.
///
/// # Why the count is taken here and not downstream
///
/// The engine's out-of-frame check runs on what this function returns, and the frame's edge is
/// exactly where the quantisation rule says a point is *not* out of frame — so at the whole-world
/// frame that check structurally cannot see a single clipped row, however many there are. At a
/// sub-square frame the two do overlap, a clipped point landing on the *world's* edge and so
/// outside a frame that does not reach it; such a row is both clipped here and refused there,
/// which §7 states as correct rather than as an exception to carve out.
///
/// `Projection::None` returns without touching either column — the identity, bit for bit, which is
/// what keeps every existing ingest exactly as it was.
fn project_columns(projection: Projection, x: &mut [f64], y: &mut [f64]) -> Result<u64, ApiError> {
    if projection == Projection::None {
        return Ok(0);
    }
    let mut clipped = 0u64;
    for (row, (lon, lat)) in x.iter_mut().zip(y.iter_mut()).enumerate() {
        if !lon.is_finite() || !lat.is_finite() || lon.abs() > 180.0 || lat.abs() > 90.0 {
            return Err(ApiError::Contract(format!(
                "ingest body: row {row} is at lon {lon}, lat {lat}, which is not a place. This \
                 view is projected ({}), and the accepted input coordinate system is WGS84 \
                 degrees — longitude within ±180, latitude within ±90 (projections.md §2). The \
                 whole batch is refused, so nothing was queued or appended",
                projection.name()
            )));
        }
        clipped += u64::from(projection.is_clipped(*lat));
        let (px, py) = projection.forward(*lon, *lat);
        (*lon, *lat) = (px, py);
    }
    Ok(clipped)
}

/// `x-tessera-view` (contracts §3.4): optional when the bundle has one view, `422` if ambiguous.
///
/// | header | views | answer |
/// |---|---|---|
/// | absent | 0 or 1 | accepted — there is nothing to be ambiguous about |
/// | absent | ≥ 2 | **422**, naming what it could have meant |
/// | present, unknown | any | **404** |
/// | present, known | any | accepted |
///
/// **Unknown is 404, not 422**, and the difference is not cosmetic: contracts §3.1's code list is
/// **closed**, and its 404 row reads "unknown `tessera_id`, node, external ID **or view**". The
/// viewer plane already answers exactly that (`map_engine_error` maps `EngineError::UnknownView`
/// to `ApiError::Unknown`), and two planes disagreeing about what an unknown view id is would be a
/// contradiction inside a closed list. §3.4's 422 is licensed for *ambiguity*, which is the second
/// row, not the third.
///
/// **The resolved id is what every row of the batch is stored under** — `WalRow::view`, and from
/// there `BufferedItem::view` and the flush segment's row space. Absent-with-one-view resolves to
/// that view's id; it is never defaulted to a literal, because a defaulted view is how a row
/// silently joins the wrong row space the day partitioning lands. A bundle declaring no view at
/// all has no row space to ingest into, so it is refused here rather than accepted into nothing.
///
/// No build path emits a multi-view bundle (`tessera-build` writes exactly one `ViewDescriptor`),
/// so the second row is unreachable. It is implemented rather than asserted-away because it is a
/// contract clause and it costs one comparison.
fn resolve_view<'a>(view: Option<&str>, views: &'a [MetaView]) -> Result<&'a MetaView, ApiError> {
    match view {
        None if views.len() > 1 => Err(ApiError::Contract(format!(
            "this bundle has {} views ({}), so x-tessera-view is required — which one a batch \
             belongs to is not inferable (contracts §3.4)",
            views.len(),
            views
                .iter()
                .map(|v| v.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
        None => views
            .first()
            .ok_or_else(|| ApiError::Contract("this bundle declares no view to ingest into".into())),
        Some(id) => views
            .iter()
            .find(|v| v.id == id)
            .ok_or_else(|| ApiError::Unknown(format!("unknown view '{id}'"))),
    }
}

/// A binary column that may be null-within (any row) or absent entirely (contracts §3.4:
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

/// A coordinate column, as `f64` — **`float32` and `float64` are both accepted and the narrower is
/// widened**, which is the rule the build reads a points file's coordinate columns by
/// (`tessera_build::input`'s `read_f64_column`), stated here because ingest and build must not
/// disagree about which files can be loaded (decision 0091).
///
/// The widening direction is the only one: an `f64` column is never narrowed. A frame at zoom
/// offset *k* resolves `2^(8−k)` `f32` steps per cell, so past roughly offset 8 the narrowing would
/// decide the **cell** a point occupies (`projections.md` §6), and it would do so inside a request
/// the caller was acked for. A whole-world frame is served perfectly well by `float32`, which is
/// why the narrower width stays acceptable rather than being refused.
fn coordinate_col(
    batch: &arrow::record_batch::RecordBatch,
    name: &str,
) -> Result<Vec<f64>, ApiError> {
    let column = batch.column_by_name(name).ok_or_else(|| {
        ApiError::Contract(format!(
            "ingest body: column '{name}' missing or not float32/float64"
        ))
    })?;
    let any = column.as_any();
    if let Some(a) = any.downcast_ref::<arrow::array::Float64Array>() {
        Ok(a.values().to_vec())
    } else if let Some(a) = any.downcast_ref::<arrow::array::Float32Array>() {
        Ok(a.values().iter().map(|v| f64::from(*v)).collect())
    } else {
        Err(ApiError::Contract(format!(
            "ingest body: column '{name}' missing or not float32/float64"
        )))
    }
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
    /// Rows this view's projection **clipped** — a latitude outside its domain, stored on the
    /// frame's edge rather than where it was written (`projections.md` §7, §8).
    ///
    /// **Reported rather than refused, and reported here because nothing else would mention it.**
    /// The build prints a clip count in its frame report; a projected view fed polar rows one
    /// batch at a time has no build to print anything, so this is the only report those rows get.
    /// Always `0` under `projection = "none"`, which has no domain to leave.
    clipped: u64,
    /// Contracts §3.4: `external_id` is optional, so an accepted item may be addressable
    /// only by its `tessera_id` -- returned here per accepted row, in the same order as the
    /// request batch, so a caller can correlate. Present for every accepted row, whether or not
    /// that row carried an external id.
    tessera_ids: Vec<u64>,
    /// How many artifacts this batch's membership column **created** — a key no artifact held, on
    /// a layer declared `value_set = "open"` (`artifacts-from-points.md` §3). Zero for every batch
    /// that carries no membership column, and for every one whose keys all existed.
    ///
    /// **Reported because minting cannot be undone.** An open layer trades the roster's refusal
    /// for a cluster existing because a point says it does, so a mistyped key now creates a
    /// permanent object instead of being refused — and the mitigation is that the caller who made
    /// the typo is told the number in the same 200 that took their rows.
    minted: u64,
}

/// The Arrow decode through the WAL append/fsync: everything CPU-bound or fsync-bearing for one
/// `/control/ingest` request, run inside `spawn_blocking`. **Never behind `ComputeGate`** — that
/// gate applies only to the viewer/session planes; an
/// ingest batch durability-syncing must not be throttled by the same budget a slow viewport
/// consumes, and more importantly a suppression on `/control/changes` must reach its own
/// `spawn_blocking` call (and thus the WAL mutex) without first queueing behind N ingest
/// *handlers* occupying reactor threads (lifecycle §1.3's deny priority lane).
fn run_ingest(
    state: &AppState,
    body: &[u8],
    batch_id: String,
    view: Option<&str>,
) -> Result<IngestResp, ApiError> {
    let body_hash: [u8; 32] = Sha256::digest(body).into();

    // One `Engine::meta()` call per batch — not per row — for the two things the manifest decides
    // about a batch: which views exist, and what scalar tail is declared. `meta()` rather than a
    // narrower accessor on purpose: it is the one definition of what this bundle declares, the one
    // `/v1/meta` publishes, and a second accessor is a second definition that can drift from it.
    let meta = state.engine.meta();
    let view = resolve_view(view, &meta.views)?;
    // **The projection is the view's, read from the bundle manifest, and it decides both what the
    // coordinate columns are called and what the numbers in them mean** (`projections.md` §3).
    // Read once per batch beside the scalar tail, from the same `meta()` snapshot, so the two
    // cannot come from different generations.
    let projection = view.projection;
    let view = view.id.clone();

    // **The layer lookup is the engine's registry, per column, not per row.** A registration is a
    // WAL append on the executor, so a column naming a layer registered a moment ago resolves here
    // exactly as the publication that created its artifacts did — and a name nothing registered is
    // refused with the undeclared-column message rather than accepted into nothing.
    let ParsedBatch {
        items,
        artifacts,
        clipped,
    } = parse_ingest_batch(
        body,
        projection,
        &meta.declared_scalars,
        &meta.vocabularies,
        &|name| state.engine.registered_layer(name).map(|l| l.declaration),
    )?;

    // The row cap. 422 per contracts §3.1's "bounds exceeded" row, naming the bound and the
    // batch's own size.
    //
    // **What is already spent when this fires, stated rather than implied**: the whole Arrow
    // decode, because the row count is not knowable before it. That is the cost of the cap and it
    // is why the *byte* cap is enforced a layer earlier, on the route, where nothing has been
    // decoded at all.
    //
    // **Placed before the `terms_of_label`/`resolve_terms` loop below, which narrows a known
    // consequence without closing it.** `resolve_terms` runs pre-submit, so extension-id dictionary
    // state grows even on refused batches; checking the row cap first keeps an over-large batch out
    // of that. A batch that is *under* the row cap and fails later still contributes. Stated because
    // the check narrows the path rather than closing it.
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
            // `over_bound` above (bounds warn, never exclude -- §6.2), just not listed here.
            if over_bound_ids.len() < 100 {
                if let Some(external_id) = &item.external_id {
                    // **base64, like every other external-id surface here** — both duplicate lists
                    // below, and `/control/changes`' input. External ids are arbitrary bytes
                    // (contracts §1) and JSON has no binary type, so this is the only lossless
                    // encoding available. `String::from_utf8_lossy` turned every non-UTF-8 byte
                    // into U+FFFD, which destroys an 8-byte little-endian id outright — and
                    // identity is the whole of what makes an over-bound warn a usable data-quality
                    // signal rather than a count (§6.2).
                    over_bound_ids
                        .push(base64::engine::general_purpose::STANDARD.encode(external_id));
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
                // Clipping is a property of the rows, not an effect of accepting them, so a
                // replay reports the same count its first acceptance did — recomputed from the
                // identical body, which is what makes the two agree. `minted` is 0 beside it
                // because minting *is* an effect and this submission had none.
                clipped,
                tessera_ids,
                // A replay creates nothing: the artifacts this batch's keys named were minted when
                // it was first accepted, and this submission had no effect at all.
                minted: 0,
            });
        }
        return Err(ApiError::Conflict(format!(
            "batch id '{batch_id}' was already accepted with a different body"
        )));
    }

    // Validate-first (contracts §3.1): duplicate external ids are 409, detail lists them, and the
    // batch has NO effect -- so this runs entirely before the executor's allocation and WAL append,
    // and after the batch-id replay check above, which stays first (an idempotent replay of an
    // already-acked batch must still be a 200 no-op, not get caught here as "already known").
    // Contracts §3.4: duplicate detection applies only *where an external id is supplied* --
    // a batch of items with no external id at all has no duplicates to find, and two null ids
    // must never be treated as colliding with each other. So every step below is scoped to
    // `Some(external_id)` items only.
    // Two checks, cheaper first:
    //   1. duplicates within this batch itself, by a hash set over the supplied bytes;
    //   2. collisions against existing state, in one call to `Engine::resolve_external_ids`,
    //      which checks the LIVE map (`Engine::established`) first -- an id
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
    // **A deleted holder is not a duplicate** (decision 0047: edit is delete + re-ingest, and our
    // retention of a dead binding must never refuse a user's write). A **suppressed** holder
    // still is one — suppression is temporary hiding, and re-ingesting a byte-identical copy past
    // it is the exact hole this check exists to close. Resolution is newest-binding-first, so a
    // re-ingested id's live holder is the one consulted here.
    let overlay_generation = state.engine.generation();
    let existing_ids: Vec<String> = resolved
        .iter()
        .zip(&supplied)
        .filter(|(entity, _)| entity.is_some_and(|e| !overlay_generation.overlay.is_deleted(e)))
        .map(|(_, (_, id))| base64::engine::general_purpose::STANDARD.encode(id))
        .collect();
    if !existing_ids.is_empty() {
        return Err(ApiError::Conflict(format!(
            "duplicate external ids already known to this deployment: {}",
            existing_ids.join(", ")
        )));
    }

    // **The buffer-occupancy bound (§1.3).** Checked here, before submission, and distinct from
    // `ingest_queue_bound`: that one bounds the *command queue* — 32 jobs by default — and the
    // executor drains a job into the buffer in milliseconds, so no ingest rate produces a 429 by
    // buffer size through it. Between ticks the buffer is what grows, and a tick-only
    // publication cadence needs a bound on the thing that grows. This is it.
    //
    // Placed after the duplicate checks and before the submission so a refusal costs no entity id,
    // no queue slot and no WAL append — the same standard the row cap above is held to.
    //
    // The figure read lags by at most one apply; see `Engine::buffered_items` for why that is the
    // right shape for a ceiling with an order of magnitude of headroom rather than a quota.
    let buffered = state.engine.buffered_items();
    if buffered >= state.ingest_buffer_max_items {
        return Err(ApiError::WriteBackpressure {
            retry_after_s: INGEST_BUFFER_FULL_RETRY_AFTER_S,
        });
    }

    // The rows go to the executor **unallocated**: entity-id assignment happens on the single writer
    // thread, per commit window. That is what makes design §11.1's signature-sort scope the
    // *server's* window rather than whatever chunk size a client happened to pick — and it is why
    // this handler does not allocate at all (the assignment run is `CommitWindow::allocate`, reached
    // through `LiveState::with_allocator` on the executor thread). Allocating here would
    // double-allocate.
    //
    // **The three decoded intermediates are CONSUMED here, not cloned.** Zipping them by reference
    // and cloning `external_id`, `descriptors`, `scalars` and `terms` out would leave `items`,
    // `descriptor_lists`, `terms_per_item` *and* `rows` live simultaneously while `accept_ingest`
    // blocks on its receipt with all four in scope — a doubling multiplied by `ingest_admission`
    // concurrent handlers, which is the term `INGEST_RESIDENT_CEILING_BYTES`'s arithmetic is about.
    // Moving also deletes four per-row allocations on the path that must sustain 10⁹-scale ingest;
    // the three source vectors drop at the end of this statement.
    let rows: Vec<UnallocatedRow> = items
        .into_iter()
        .zip(terms_per_item)
        .zip(descriptor_lists)
        .map(|((item, terms), descriptors)| UnallocatedRow {
            external_id: item.external_id,
            view: view.clone(),
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
    let (entity_ids, minted) = state
        .engine
        .accept_ingest_joining(rows, batch_id, body_hash, artifacts)
        .map_err(|e| {
            // Batch-level context, kept alongside the mapper's own `error!` rather than folded into
            // one line, so an operator sees both without the body ever carrying either.
            tracing::error!("an ingest batch was refused by the write executor");
            map_accept_error(e)
        })?;

    // Contracts §3.4: the 200 response returns each accepted row's `tessera_id`, in batch
    // order, so a caller who supplied no external id for an item still learns the identity it
    // was given -- otherwise that item would be unreachable by anyone.
    let tessera_ids = tessera_ids_of(state, &entity_ids)?;

    Ok(IngestResp {
        accepted,
        over_bound,
        over_bound_ids,
        clipped,
        tessera_ids,
        minted,
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
/// **`body: Result<Bytes, _>` rather than `Bytes` is what maps the extractor's own rejection.**
/// Step 1 is a router layer, so the credential can no longer be preceded by an extractor at all;
/// what the signature still buys is that an *authenticated* over-cap caller meets the 422 above
/// rather than axum's bare 413, which is outside contracts §3.1's closed code list.
///
/// # The buffered body: what the layer closes, and what it does not
///
/// **Closed for an unauthenticated caller.** Extractors run inside the handler service, so a
/// `check_bearer` in this function's body would see the request only once it was resident in full —
/// up to `ingest_max_batch_bytes`, 16 MiB by default, 8× axum's own limit.
/// [`require_operator_credential`] runs *outside* the extractors, so a caller with no credential
/// meets a 401 while the body is still an unconsumed stream. Nothing is buffered on their behalf.
///
/// **Not closed for a valid-credentialed caller.** An authenticated request buffers up to
/// `ingest_max_batch_bytes` before `ingest_admission` is consulted, and `axum::serve` applies no
/// connection or concurrency cap, so N authenticated connections pin `N × ingest_max_batch_bytes`.
/// The two factors are bounded by different things, and separating them is the whole of the answer:
///
/// - **the per-connection factor** is bounded by `config::INGEST_MAX_BATCH_BYTES_CEILING`, a
///   startup refusal on the key itself. Without it, small admission and queue bounds with a
///   gigabyte batch cap satisfy every relation over the *admitted* window — which is what
///   `INGEST_RESIDENT_CEILING_BYTES` weighs — and then die on the second concurrent upload, in
///   front of it;
/// - **the count** is bounded by deployment posture, not by this process. The constant's doc
///   carries the argument, including why a `tower` concurrency limit and a listener-level
///   connection cap were both declined: the first queues rather than sheds and, on the whole
///   control router, would put `/control/changes` behind an in-flight bound shared with
///   receipt-blocking ingest handlers — lifecycle §1.3's forbidden shape, the exact thing
///   [`DENY_RUNTIME`] exists to prevent; the second refuses by not accepting, which leaves the
///   caller in the kernel's accept backlog with no status code at all.
///
/// **The shape is what makes this a bound rather than a fix.** This endpoint buffers the whole body
/// because it decodes the whole Arrow batch at once; streaming it is a change to the write path
/// that belongs with flush, and no configuration bound substitutes for it.
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

    // Read here rather than inside `run_ingest` because a `HeaderMap` is the handler's, not the
    // blocking closure's. A header whose bytes are not valid UTF-8 names no view any manifest can
    // hold, so it is refused rather than lossily decoded — the same rule this handler applies to
    // external ids one function over.
    let view = match headers.get("x-tessera-view") {
        None => None,
        Some(value) => Some(
            value
                .to_str()
                .map_err(|_| {
                    ApiError::Contract(
                        "x-tessera-view is not valid UTF-8, so it names no view".to_string(),
                    )
                })?
                .to_string(),
        ),
    };

    // The admission bound, **before** `spawn_blocking`. See `IngestAdmission`.
    let Some(permit) = state.ingest_admission.try_admit() else {
        // `debug!`, not `warn!` and certainly not `error!`. A `warn!` here is a synchronous
        // formatted write **on the reactor, per refused request** — under a sustained shed at 10⁹
        // ingest rates the log becomes a second bottleneck on exactly the path that exists to be
        // cheap. `tracing`'s macros check interest before evaluating their fields, so at any level
        // above DEBUG this costs a load and a branch.
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

    // Closure capture is `state` (moved in directly — nothing after this
    // `.await` needs the handler's own copy), `body` (an owned `Bytes` — cheap, refcounted clone
    // of the request body already read off the socket, not a copy) and `batch_id` (owned
    // `String`). Never gated by `ComputeGate` (see `run_ingest`'s doc).
    //
    // The permit is **moved in**, not held across the `.await`: a disconnected client's handler
    // future is dropped while this closure keeps running and keeps its thread, so releasing on
    // handler-drop would under-count exactly when the pool is under pressure.
    let resp = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        run_ingest(&state, &body, batch_id, view.as_deref())
    })
    .await
    .map_err(map_join_error)??;

    Ok(Json(resp))
}

/// `EntityId` -> `tessera_id`, per row, in the caller's given order. `Engine::tessera_id_of` is
/// fallible only for an entity id at or above `u32::MAX`, which the I9 allocator's ceiling makes
/// unreachable in practice — still propagated as a typed 500 here, never `.unwrap()`-ed away, since
/// an internal invariant violation must fail closed.
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
    /// Exactly one of `external_id` / `tessera_id`, never both and never neither.
    #[serde(default)]
    external_id: Option<String>,
    /// **String-encoded**, deliberately: a bare JSON number loses `u64`s past 2⁵³ in every
    /// JavaScript client, silently, and a mis-parsed identifier denies the wrong entity.
    #[serde(default)]
    tessera_id: Option<String>,
    /// Required with `tessera_id`, refused without it. The deployment's current identifier set,
    /// from `/v1/meta` — see [`DecodedChange`] for what it guards.
    #[serde(default)]
    idset: Option<u32>,
    op: String,
}

/// How one item names its entity: the two address forms, already shape-validated.
enum Address {
    External(Vec<u8>),
    Tessera { id: TesseraId, idset: u32 },
}

/// One `/control/changes` item whose shape is validated but whose external id is not yet resolved.
///
/// The intermediate exists because resolution is done for the **whole request in one call**: the
/// batch form of external-id resolution opens each bundle extent at most once, where the per-item
/// form opens one per item and was the largest remaining per-item cost of a change request once the
/// WAL fsyncs were amortised.
struct DecodedChange {
    address: Address,
    op: ChangeOp,
}

/// One `/control/changes` item, fully validated but not yet applied — see [`changes`]'s doc.
struct ValidatedChange {
    entity: EntityId,
    op: ChangeOp,
}

/// The validate-then-apply body of `/control/changes`: external-id resolution (sidecar IO) and
/// every item's WAL append/fsync, run inside `spawn_blocking`. Same
/// never-gated rule as [`run_ingest`] — this is the deny priority lane a suppression must reach
/// without queueing behind concurrent ingest handlers on the reactor.
fn run_changes(state: &AppState, items: Vec<ChangeItem>) -> Result<(), ApiError> {
    // Validate-first: parse every item's op, base64-decode and resolve its external id, and
    // validate its `access` field's shape — all *before* appending anything. An item-by-item loop
    // would append, fsync and apply items 1..n-1 before item n's 404/422 aborted the request,
    // leaving the caller a single error for a batch that was in fact partially applied. Doing every
    // fallible *validation* step first means a rejected batch is rejected wholesale, with no side
    // effect at all. (A WAL I/O failure partway through
    // the second, apply-only loop below is a different class of failure — an infrastructure
    // fault, not a client-correctable validation error — and is not, and cannot be, rolled back:
    // each item's WAL record is durable or it is not, exactly as `/control/ingest`'s batches are.
    // Nor does it abort the loops below; see the comment there.)
    //
    // **External ids are resolved for the WHOLE request in one call**, the same way
    // `/control/ingest`'s duplicate check does it. `Engine::resolve_external_id` consults the live
    // map and then the bundle's external-id sidecar, and the single-key form opens a bundle extent
    // per call — so resolving per item made a request of N denies N sidecar traversals, which once
    // the WAL fsyncs were amortised was the largest remaining per-item term in the request. The
    // batch form (`resolve_external_ids`) opens each extent at most once regardless of N and
    // answers in the caller's order, so the 404 below still names the first unresolved item.
    //
    // **What batching reorders, since it is wire-visible.** For a request invalid in two ways at
    // once — say item 3 names an unknown external id and item 5 is not valid base64 — the answer is
    // item 5's 422 rather than item 3's 404, because shape validation runs over the whole request
    // before resolution does. Both are wholesale refusals with no side effect, and contracts §3.1
    // orders neither against the other; what the ordering has to preserve is that an invalid request
    // applies nothing, and it does.
    let mut decoded: Vec<DecodedChange> = Vec::with_capacity(items.len());
    for item in &items {
        let op = match item.op.as_str() {
            // **Withdrawn** (decision 0047, owner-ruled 2026-08-04): edit is delete + re-ingest.
            // The arm stays so the refusal names the flow rather than answering "unknown op"; the
            // machinery behind it was deleted by decision 0048.
            "predicate" => {
                return Err(ApiError::Contract(
                    "the predicate op is withdrawn: edit is delete + re-ingest (decision 0047) — \
                     delete the item, then re-ingest it under the same external_id with its new \
                     access labels; a deleted holder does not block re-ingest"
                        .to_string(),
                ));
            }
            "delete" => ChangeOp::Delete,
            "suppress" => ChangeOp::Suppress,
            "unsuppress" => ChangeOp::Unsuppress,
            other => {
                return Err(ApiError::Contract(format!("unknown change op '{other}'")));
            }
        };

        // **Exactly one address form.** Both is ambiguous and neither is unaddressable; either
        // way the request is refused wholesale before anything is enqueued.
        let address = match (&item.external_id, &item.tessera_id) {
            (Some(external_id), None) => {
                if item.idset.is_some() {
                    return Err(ApiError::Contract(
                        "idset accompanies tessera_id, never external_id: an external id means \
                         the same entity under every identity key, so there is nothing for it to \
                         guard"
                            .to_string(),
                    ));
                }
                Address::External(
                    base64::engine::general_purpose::STANDARD
                        .decode(external_id)
                        .map_err(|e| {
                            ApiError::Contract(format!("external_id is not valid base64: {e}"))
                        })?,
                )
            }
            (None, Some(tessera_id)) => {
                let id: u64 = tessera_id.parse().map_err(|_| {
                    ApiError::Contract(
                        "tessera_id must be a base-10 string: it is a u64, and a bare JSON number \
                         loses precision past 2^53 in most clients"
                            .to_string(),
                    )
                })?;
                let idset = item.idset.ok_or_else(|| {
                    ApiError::Contract(
                        "a tessera_id-addressed change must carry the idset it was minted under \
                         (GET /v1/meta): identifiers are keyed, so one gathered before a rotation \
                         names a different item after it"
                            .to_string(),
                    )
                })?;
                Address::Tessera {
                    id: TesseraId::new(id),
                    idset,
                }
            }
            (Some(_), Some(_)) => {
                return Err(ApiError::Contract(
                    "a change names exactly one of external_id / tessera_id, never both"
                        .to_string(),
                ))
            }
            (None, None) => {
                return Err(ApiError::Contract(
                    "a change names exactly one of external_id / tessera_id, and this names \
                     neither"
                        .to_string(),
                ))
            }
        };

        decoded.push(DecodedChange { address, op });
    }

    // **Each address form resolved in one batched call, both before anything is enqueued.** The
    // external half opens each bundle extent at most once regardless of N; the tessera half takes
    // one generation snapshot for the idset check and every inversion, so a swap cannot land
    // between them.
    //
    // **The idset decides first, and for the whole request.** A caller whose list was gathered
    // before a key rotation is refused as a 409 before a single identifier is inverted — its ids
    // would otherwise be reinterpreted under the new key and name different live items (decision
    // 0025). Every tessera-addressed item must agree on the idset, because there is one per
    // deployment and a request mixing two was assembled from a state that never existed.
    let mut idsets = decoded.iter().filter_map(|d| match &d.address {
        Address::Tessera { idset, .. } => Some(*idset),
        Address::External(_) => None,
    });
    if let Some(idset) = idsets.next() {
        if idsets.any(|other| other != idset) {
            return Err(ApiError::Contract(
                "one request carries two different idsets; there is one per deployment, so this \
                 list was assembled from a state that never existed"
                    .to_string(),
            ));
        }
        let ids: Vec<TesseraId> = decoded
            .iter()
            .filter_map(|d| match &d.address {
                Address::Tessera { id, .. } => Some(*id),
                Address::External(_) => None,
            })
            .collect();
        // Refuses with `StaleIdSet` before inverting anything — see `resolve_tessera_ids`.
        let resolved = state
            .engine
            .resolve_tessera_ids(&ids, idset)
            .map_err(crate::error::map_engine_error)?;
        if let Some(position) = resolved.iter().position(|e| e.is_none()) {
            return Err(ApiError::Unknown(format!(
                "tessera_id at tessera-addressed position {position} names nothing this \
                 deployment issued"
            )));
        }
        let mut resolved = resolved.into_iter();
        let external_keys: Vec<Vec<u8>> = decoded
            .iter()
            .filter_map(|d| match &d.address {
                Address::External(key) => Some(key.clone()),
                Address::Tessera { .. } => None,
            })
            .collect();
        let mut external = state
            .engine
            .resolve_external_ids(&external_keys)
            .map_err(map_store_error)?
            .into_iter();

        let mut validated = Vec::with_capacity(decoded.len());
        for d in decoded {
            let entity = match &d.address {
                Address::Tessera { .. } => resolved
                    .next()
                    .flatten()
                    .expect("checked complete just above"),
                Address::External(_) => external
                    .next()
                    .flatten()
                    .ok_or_else(|| ApiError::Unknown("unknown external id".to_string()))?,
            };
            validated.push(ValidatedChange { entity, op: d.op });
        }
        return apply_validated(state, validated);
    }

    let keys: Vec<Vec<u8>> = decoded
        .iter()
        .map(|d| match &d.address {
            Address::External(key) => key.clone(),
            Address::Tessera { .. } => unreachable!("no tessera address reaches here"),
        })
        .collect();
    let resolved = state
        .engine
        .resolve_external_ids(&keys)
        .map_err(map_store_error)?;
    let mut validated = Vec::with_capacity(decoded.len());
    for (d, entity) in decoded.into_iter().zip(resolved) {
        let entity = entity.ok_or_else(|| ApiError::Unknown("unknown external id".to_string()))?;
        validated.push(ValidatedChange { entity, op: d.op });
    }

    apply_validated(state, validated)
}

/// Enqueue and collect a validated batch — the apply half of [`run_changes`], reached by both
/// address forms.
///
/// Extracted rather than duplicated: the enqueue/collect discipline below is the whole of this
/// endpoint's fsync amortisation *and* its fail-closed batch semantics, and two copies of it is
/// how one of them comes to abort early.
fn apply_validated(state: &AppState, mut validated: Vec<ValidatedChange>) -> Result<(), ApiError> {
    // **Enqueue the whole chunk, then collect it — never one item at a time.**
    //
    // This is the difference between one fsync per request and one fsync per item. The executor
    // gathers the denies it finds queued into a single commit window (`Executor::run_deny_pass`):
    // k WAL appends, one fsync, one overlay clone, one generation swap, k acks. A loop that waited
    // for each item's receipt before submitting the next never lets more than one job be queued,
    // so the window it can build has one entry in it and the amortisation is unreachable. Measured
    // Measured with one fsync per item: ~300 denies/second, i.e. tens of minutes for a bulk
    // revocation with every other deny queued behind it.
    //
    // **Chunked at `DENY_WINDOW_MAX_ENTRIES`**, which is also the window's own bound, so chunking
    // costs no extra fsyncs — the executor would have closed a window at that count anyway. What it
    // buys is a bound on pending receipts: this handler runs on a pool of `DENY_MAX_BLOCKING_THREADS`
    // threads and the request body admits far more items than that constant, so an unchunked
    // enqueue would let the pool hold (threads × body) one-slot channels on the one lane that
    // structurally cannot refuse.
    //
    // **Every item is submitted, even after one fails.** The obvious `?` in either loop aborts the
    // batch at the first failure, and that is fail-open at batch scope now that a WAL failure is a
    // sustained *posture* rather than a one-off: `WalPoisoned` refuses every subsequent append, so
    // an aborting loop applies exactly the first item of a multi-item change batch, on every retry,
    // until the WAL is reopened — every other suppression in the request silently unapplied behind
    // a 500 that reads as "retry for durability". Continuing is strictly more fail-closed and is
    // this lane's whole ethos (lifecycle §4: never a refusal that leaves a deny unapplied): each
    // remaining `Delete`/`Suppress` is applied to the live overlay by the executor even though its
    // append fails, so the items are hidden and the caller still gets a 500.
    //
    // The enqueue/collect split makes that rule structurally stronger rather than merely preserving
    // it. A WAL failure can only be observed in the *collect* loop, by which point every item in the
    // chunk is already enqueued and the executor will answer for all of them — so no abort can
    // un-submit anything, and what an aborting collect loop would lose is dispositions, not
    // effects.
    //
    // Validation is already wholesale above, so nothing reached here can be a client-correctable
    // fault: everything below is an infrastructure failure and every one of them is alarmed
    // individually.
    //
    // **The batch's answer is a FOLD over dispositions, not the first item's status.** An item's
    // status describes an item. The constructible bad case is "item 1 applied successfully, the
    // executor then died, item 2 refused" — first-error reporting has no error at all for item 1 and
    // answers item 2's **503 `not-ready`**, i.e. "this node did not take your write", for a batch
    // containing a durable, in-force suppression.
    //
    // So every failure is collected with the count that succeeded, and `map_change_batch_error`
    // decides once, over all of them. See its doc for the rules and for why 500 dominates 503.
    //
    // **Each failure is collected WITH ITS OP.** Lifecycle §4's apply-anyway rule is scoped to
    // `Delete`/`Suppress` and the executor applies exactly that scope, so "did this failure leave an
    // effect in force?" cannot be answered from the error alone — an `Unsuppress` whose append
    // failed was refused without applying, and an op-blind fold would report it as possibly in
    // force *and* omit it from the "not applied" half. The op is already in hand at the enqueue, so
    // it is carried rather than re-derived at the fold.
    let mut failures: Vec<(ChangeOp, AcceptError)> = Vec::new();
    let mut applied = 0usize;
    for chunk in validated.chunks_mut(DENY_WINDOW_MAX_ENTRIES) {
        let mut pending = Vec::with_capacity(chunk.len());
        for change in chunk.iter_mut() {
            let op = change.op;
            match state.engine.submit_change(change.entity, op) {
                Ok(p) => pending.push((op, p)),
                Err(e) => {
                    alarm_change_failure(op, &e);
                    failures.push((op, e));
                }
            }
        }
        for (op, p) in pending {
            match p.wait() {
                Ok(()) => applied += 1,
                Err(e) => {
                    alarm_change_failure(op, &e);
                    failures.push((op, e));
                }
            }
        }
    }

    match map_change_batch_error(&failures, applied) {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Alarm one failed `/control/changes` item, saying which of the two things happened to it.
///
/// **Keyed on the disposition, never on the variant or on which loop produced it.** The enqueue
/// half and the collect half are not the "nothing happened" / "something might have" boundary:
/// `Engine::submit_change` can return `SubmitError::ReceiptLost` from the doorbell, *after* the job
/// is queued, and the executor's shutdown pass drains and executes the deny lane before it observes
/// the disconnect — so a `Suppress` that failed at the enqueue may nonetheless be hidden. Asking
/// `may_have_taken_effect` is asking the same question `map_change_batch_error` folds, so the log
/// and the status can never disagree about an item.
fn alarm_change_failure(op: ChangeOp, e: &AcceptError) {
    let in_force = match e {
        AcceptError::Submit(s) => s.may_have_taken_effect(),
        // Lifecycle §4: a `Delete`/`Suppress` whose append failed was applied to the live overlay
        // anyway before the error was returned — never a refusal that leaves a deny unapplied.
        AcceptError::Exec(_) => matches!(op, ChangeOp::Delete | ChangeOp::Suppress),
        // Ingest-only, and refused before the submit — unreachable from a change, and in force in
        // no sense even if it were.
        AcceptError::OutsideExtent { .. }
        | AcceptError::ScalarArity { .. }
        | AcceptError::SteppedDown => false,
    };
    if in_force {
        tracing::error!(
            op = ?op,
            "ALARM: a change failed with its effect possibly IN FORCE (item hidden immediately) \
             and returning 500 — durability is owed, and the caller must not read this as a \
             no-op; re-issuing is safe, treating the item as visible is not"
        );
    } else {
        tracing::error!(
            op = ?op,
            "a change failed and nothing was applied for it; it must be re-issued"
        );
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
                "the change request body is not a valid JSON array of {external_id, op} \
                 items; nothing in it was applied"
                    .to_string(),
            )
        }
    })?;

    // **No readiness gate here, and that is load-bearing** (lifecycle §4). A
    // `WalPoisoned` node still applies `Delete`/`Suppress` to the live overlay before returning its
    // error, so gating this endpoint on `readyz` would apply the first failing suppression and then
    // refuse every subsequent one *without applying it* — refused **and** unapplied, which is the
    // fail-open the posture exists to prevent. Readiness governs routing, never deny acceptance.
    //
    // Closure captures `state` (moved in directly — nothing after this
    // `.await` needs the handler's own copy) and `items` (moved — the request body is already
    // fully decoded to owned `Vec<ChangeItem>` by this point, so there is nothing left to borrow).
    // `spawn_on_deny_lane`, not `tokio::task::spawn_blocking`: see its doc.
    spawn_on_deny_lane(move || run_changes(&state, items))
        .await
        .map_err(map_join_error)??;

    // R5: `/control/changes` is 200 after fsync, never 429.
    Ok(StatusCode::OK)
}

/// `POST /control/flush` (contracts §3.4): **accepted at any time, executed promptly.**
///
/// The request pulls the tick's deadline forward and wakes an idle executor, so the flush runs at
/// the executor's next loop iteration — through the one tick path, with everything a tick
/// guarantees (`Engine::request_flush`). The 202 still means "accepted, not yet done": the
/// segment write is pool work of real duration and this response never waits on it.
///
/// This operator trigger is deliberately the only thing that may pull the tick — it is
/// rate-decoupled from ingest, so it cannot recreate the publish-on-trip hazard that got
/// `flush_max_items` deleted (decision 0045): a publication period proportional to load, rotating
/// every session's projection key at that rate.
///
/// Idempotent: two requests before one tick are satisfied by that tick together, because what is
/// recorded is a flag and not a count.
async fn flush(State(state): State<Arc<AppState>>) -> StatusCode {
    state.engine.request_flush();
    StatusCode::ACCEPTED
}

/// `POST /control/compact` (contracts §3.4): **accepted at any time, and then minutes to hours.**
///
/// The same shape as [`flush`] and through the same door — a flag the executor reads at its next
/// tick, so a requested fold plans on the one thread that publishes and inherits everything a tick
/// guarantees. What differs is only how long the 202 stands for: a flush's segment write is
/// seconds, and a fold re-reads and rewrites the whole corpus (compaction §3).
///
/// **A request while a fold runs is refused, not queued, and this route cannot tell the caller
/// which happened.** At most one fold is in flight (`Executor::dispatch_fold`), and what is
/// recorded here is a flag rather than a count, so two requests before one tick are satisfied by
/// that tick together and a request during a fold is consumed with a warning in the log. Answering
/// 409 instead would mean this handler reading in-flight state and racing the tick that clears it —
/// a lie half the time. `/control/status`'s `compaction` block is where a caller sees what actually
/// happened: `fold_requested` while it waits, then `folds` or `fold_failures` moving.
///
/// **This is not the automatic trigger and does not go through it.** The schedule (compaction §9,
/// decision 0056) consults its gauges directly at the tick; a request and a schedule are two routes
/// into one dispatch, so an operator asking for a fold neither disturbs nor is disturbed by the
/// window.
async fn compact(State(state): State<Arc<AppState>>) -> StatusCode {
    state.engine.request_fold();
    StatusCode::ACCEPTED
}

/// `PUT /control/layers` — register one annotation layer.
///
/// **Synchronous, unlike [`flush`] and [`compact`] beside it, and the difference is what the
/// caller is owed.** Those two answer 202 for work that takes minutes; a registration is a WAL
/// append and an fsync, and the caller cannot proceed without the `tessera_id` it returns — that
/// identifier is the only address by which the layer can later be suppressed, since an entity id
/// never crosses the boundary (**I10**).
///
/// **A failure means the layer does not exist**, which is the opposite of `/control/changes`'s
/// posture and deliberately so: a suppression is applied even when its append fails, because
/// leaving an accepted deny unapplied is a fail-open. A layer has no such asymmetry — one that
/// existed in memory and not in the log would come back from a restart as a free name, having
/// already handed this caller an identifier for its entity.
///
/// Refusals are 422 and say why: the caller's own declaration measured against the deployment's
/// published rules — a name already taken, a name tombstoned, a nested layer that declared levels.
async fn register_layer(
    State(state): State<Arc<AppState>>,
    body: Json<tessera_types::layer::LayerDeclaration>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let declaration = body.0;
    let name = declaration.name.clone();
    // The **shared** blocking pool, not the deny runtime beside it. That runtime exists so a
    // suppression is never queued behind ingest; a registration is not a deny, and delaying one
    // under ingest load is backpressure working rather than a security operation refused.
    let id = tokio::task::spawn_blocking(move || state.engine.register_layer(declaration))
        .await
        .map_err(crate::error::map_join_error)?
        .map_err(crate::error::map_accept_error)?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "name": name, "tessera_id": id.raw().to_string() })),
    ))
}

/// `DELETE /control/layers/{name}` — drop a layer and tombstone its name for ever.
///
/// **The name never comes back**, and that is the operation rather than a side effect of it.
/// Bookmarks, edges and suppressions all travel by name, so a recreated `clusters/topics` would
/// silently inherit every stale reference to the old one. A caller who wants the name again wants a
/// different name.
///
/// The entity ids do not come back either — the allocator is monotone with no free list, and
/// reclaiming them is decision 0072's work, which is settled, unbuilt, and conditional on the
/// membership-reconciliation clause shipping with it.
async fn drop_layer(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Result<StatusCode, ApiError> {
    tokio::task::spawn_blocking(move || state.engine.drop_layer(name))
        .await
        .map_err(crate::error::map_join_error)?
        .map_err(crate::error::map_accept_error)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Turn a flat member offset back into `(artifact index, member index)`, so a refusal names the
/// coordinate the caller's own pipeline holds rather than an offset into a flattening they never
/// wrote.
fn position_in_batch(widths: &[usize], flat: usize) -> (usize, usize) {
    let mut remaining = flat;
    for (artifact, width) in widths.iter().enumerate() {
        if remaining < *width {
            return (artifact, remaining);
        }
        remaining -= width;
    }
    (widths.len(), 0)
}

/// How a batch addresses its members. **One form per request, not per member**, which is the one
/// place this deliberately differs from `/control/changes`: a change names a handful of items and
/// pays a tagged object each; a clustering names its whole corpus, and a per-member tag would be
/// most of the request body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum Addressing {
    External,
    Tessera,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PublishBody {
    /// Defaults to the layer's only level. A layer that declared none has exactly level 0.
    #[serde(default)]
    level: u32,
    addressing: Addressing,
    /// Required for `tessera` addressing and refused otherwise — an external id means nothing to a
    /// key, so accepting an idset beside one would imply a check that never ran.
    #[serde(default)]
    idset: Option<u32>,
    /// The space every row's shape is in where the row names none (`polygon-membership.md`
    /// §4.3) — `"view"` if absent, or `"wgs84"`, which asks the view to project the coordinates
    /// with the same function it projected its points with. A view whose `projection` is `none`
    /// has one space and refuses the second; so does a coordinate outside ±180 × ±90 — each a
    /// `422` naming the row, the whole batch without effect.
    #[serde(default)]
    default_space: Option<String>,
    artifacts: Vec<IncomingArtifactBody>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct IncomingArtifactBody {
    /// **Effectively mandatory for a layer another layer's edges will point into**: an edge names
    /// its target, and at publish time the caller holds no `tessera_id` for it.
    #[serde(default)]
    key: Option<String>,
    /// Base64 external ids, or base-10 `tessera_id` strings, according to the batch's `addressing`.
    /// External ids are base64 on the same rule `/control/changes` follows — they are bytes, not
    /// text — and identifiers are strings because a bare JSON number loses a `u64` past 2⁵³ in
    /// every JavaScript client, silently.
    members: Vec<String>,
    /// The artifact's supplied content, as **ranked contents**, most specific first. Absent on a
    /// layer that declares no supplied content; required on one that does, and refused on one that
    /// does not — the engine decides that, since the declaration is its state and not this
    /// handler's.
    #[serde(default)]
    content: Vec<IncomingContentBody>,
    /// The artifact this one exists only as an attachment to — a label on a cluster.
    ///
    /// An attached artifact is withheld wherever its target is: suppress the cluster and its labels
    /// stop serving on every route, the ones that traverse no edge included.
    #[serde(default)]
    attached_to: Option<AttachmentBody>,
    /// The artifact's shape, in its layer's kind's field and no other — the same row shape the
    /// build reads (decision 0091; `polygon-membership.md` §6.1, §6.4): `bbox = [min_x, min_y,
    /// max_x, max_y]`, `circle = [cx, cy, r]`, `ellipse = [cx, cy, a, b, angle]`, or `wkt`.
    /// Required on a layer whose `shape` declares a kind, refused on every other.
    #[serde(default)]
    bbox: Option<Vec<f64>>,
    #[serde(default)]
    circle: Option<Vec<f64>>,
    #[serde(default)]
    ellipse: Option<Vec<f64>>,
    #[serde(default)]
    wkt: Option<String>,
    /// This row's own space, overriding the batch's `default_space`.
    #[serde(default)]
    space: Option<String>,
}

/// One ranked content's authored shape — the text at the layer's shape slot — canonicalised for
/// every view of its layer, by the route [`canonical_row_shape`] takes for a membership shape.
fn canonical_authored_content(
    state: &AppState,
    declaration: &tessera_types::layer::LayerDeclaration,
    index: usize,
    rank: usize,
    kind: tessera_types::layer::ShapeKind,
    text: &str,
) -> Result<
    (
        tessera_lifecycle::membership::ArtifactShapes,
        serde_json::Value,
    ),
    ApiError,
> {
    use tessera_engine::shapes::{authored_shape_input, canonical_shapes, shape_input};
    let refuse = |detail: String| {
        ApiError::Contract(format!(
            "artifact {index}: content {rank}: the authored {} content: {detail}",
            kind.as_str()
        ))
    };
    let input = authored_shape_input(kind, text).map_err(|e| refuse(e.to_string()))?;
    let shape = shape_input(kind, input).map_err(|e| refuse(e.to_string()))?;
    let meta = state.engine.meta();
    let q = meta.quantisation;
    let extent = tessera_engine::shapes::Bounds {
        x_min: q.x_min,
        x_max: q.x_max,
        y_min: q.y_min,
        y_max: q.y_max,
    };
    let views: Vec<&str> = declaration.views.iter().map(String::as_str).collect();
    // Authored content carries no `space` of its own — it is the drawing a declaration wrote in
    // the view's coordinates, exactly as the build reads it (`layers.rs`).
    let canonical = canonical_shapes(
        &shape,
        &views,
        tessera_engine::shapes::ShapeSpace::View,
        // **The projection of the layer's own views**, not the bundle's first. A shape layer
        // spanning views with different projections is refused at the declaration, so the views
        // agree and the first is the answer for all of them — but it must be *this layer's* first
        // and not the bundle's, or a shape is placed by a projection no view of it declares.
        layer_projection(&meta, &views)?,
        &extent,
        state.max_shape_vertices,
    )
    .map_err(|e| refuse(e.to_string()))?;
    let report: Vec<serde_json::Value> = canonical
        .reports
        .iter()
        .map(|(view, r, stats)| {
            serde_json::json!({
                "view": view,
                "clipped": r.clipped,
                "outside": r.outside,
                "rings_dropped": r.rings_dropped,
                "degrees_looking": r.degrees_looking,
                "vertices_in": r.vertices_in,
                "vertices_out": r.vertices_out,
                "parts": stats.parts,
                "rings": stats.rings,
            })
        })
        .collect();
    let shapes = tessera_lifecycle::membership::ArtifactShapes::new(canonical.by_view)
        .ok_or_else(|| refuse("canonicalised to no view".to_string()))?;
    Ok((shapes, serde_json::Value::Array(report)))
}

/// One row's shape as the caller wrote it, canonicalised for every view of its layer.
///
/// **The same canonicalisation the build applies, and the same report** — clipped, outside, rings
/// dropped, degrees-looking, the decomposition's size — returned in the response body rather than
/// printed, so a row published at ingest is published as it would have been at the build. What
/// refuses is what refuses there: a coordinate that is not one, a non-positive radius or axis, a
/// polygon over the vertex cap, a kind that is not the layer's, and a `space` the view cannot
/// honour — each naming the row, the whole batch without effect.
fn canonical_row_shape(
    state: &AppState,
    declaration: &tessera_types::layer::LayerDeclaration,
    index: usize,
    artifact: &IncomingArtifactBody,
    default_space: tessera_engine::shapes::ShapeSpace,
) -> Result<
    Option<(
        tessera_lifecycle::membership::ArtifactShapes,
        serde_json::Value,
    )>,
    ApiError,
> {
    use tessera_engine::shapes::{canonical_shapes, shape_input, ShapeInput, ShapeSpace};
    let refuse = |detail: String| ApiError::Contract(format!("artifact {index}: {detail}"));
    let mut carried: Vec<(&str, ShapeInput)> = Vec::new();
    let count = |field: &str, n: usize, want: usize| {
        refuse(format!("`{field}` has {n} value(s); it is exactly {want}"))
    };
    if let Some(v) = &artifact.bbox {
        let [a, b, c, d] = v[..] else {
            return Err(count("bbox", v.len(), 4));
        };
        carried.push(("bbox", ShapeInput::Bbox([a, b, c, d])));
    }
    if let Some(v) = &artifact.circle {
        let [a, b, c] = v[..] else {
            return Err(count("circle", v.len(), 3));
        };
        carried.push(("circle", ShapeInput::Circle([a, b, c])));
    }
    if let Some(v) = &artifact.ellipse {
        let [a, b, c, d, e] = v[..] else {
            return Err(count("ellipse", v.len(), 5));
        };
        carried.push(("ellipse", ShapeInput::Ellipse([a, b, c, d, e])));
    }
    if let Some(text) = &artifact.wkt {
        carried.push(("wkt", ShapeInput::Wkt(text.clone())));
    }
    let Some(kind) = declaration.shape.map(|s| s.kind) else {
        if !carried.is_empty() {
            return Err(refuse(
                "carries a shape, and this layer declares no `shape`. Its members come from the \
                 stored set its membership names, so a shape beside them is a region nothing \
                 evaluates"
                    .to_string(),
            ));
        }
        return Ok(None);
    };
    let space = match artifact.space.as_deref() {
        None => default_space,
        Some(word) => ShapeSpace::parse(word).map_err(|e| refuse(format!("`space`: {e}")))?,
    };
    let input = match carried.len() {
        0 => {
            return Err(refuse(format!(
                "carries no shape, and this layer's `shape` declares a {}. The shape is the \
                 whole of such an artifact's membership, so one published without it would count \
                 zero for every viewer",
                kind.as_str()
            )))
        }
        1 => carried.pop().expect("one").1,
        _ => {
            return Err(refuse(format!(
                "carries {} — one row has one shape, in its layer's kind's field",
                carried
                    .iter()
                    .map(|(f, _)| format!("`{f}`"))
                    .collect::<Vec<_>>()
                    .join(" and ")
            )))
        }
    };
    let shape = shape_input(kind, input).map_err(|e| refuse(e.to_string()))?;
    let meta = state.engine.meta();
    let q = meta.quantisation;
    let extent = tessera_engine::shapes::Bounds {
        x_min: q.x_min,
        x_max: q.x_max,
        y_min: q.y_min,
        y_max: q.y_max,
    };
    let views: Vec<&str> = declaration.views.iter().map(String::as_str).collect();
    let canonical = canonical_shapes(
        &shape,
        &views,
        space,
        // **The projection of the layer's own views**, not the bundle's first. A shape layer
        // spanning views with different projections is refused at the declaration, so the views
        // agree and the first is the answer for all of them — but it must be *this layer's* first
        // and not the bundle's, or a shape is placed by a projection no view of it declares.
        layer_projection(&meta, &views)?,
        &extent,
        state.max_shape_vertices,
    )
    .map_err(|e| refuse(e.to_string()))?;
    let report: Vec<serde_json::Value> = canonical
        .reports
        .iter()
        .map(|(view, r, stats)| {
            serde_json::json!({
                "view": view,
                "clipped": r.clipped,
                "outside": r.outside,
                "rings_dropped": r.rings_dropped,
                "degrees_looking": r.degrees_looking,
                "vertices_in": r.vertices_in,
                "vertices_out": r.vertices_out,
                "parts": stats.parts,
                "rings": stats.rings,
                "interior_tiles": stats.interior_tiles,
                "boundary_cells": stats.boundary_cells,
            })
        })
        .collect();
    let shapes = tessera_lifecycle::membership::ArtifactShapes::new(canonical.by_view)
        .ok_or_else(|| refuse("the layer is drawn in no view".to_string()))?;
    Ok(Some((shapes, serde_json::Value::Array(report))))
}

/// How a caller names an attachment's target.
///
/// **By the target's own key, because an ordinal never crosses the boundary** — a
/// publication answers with a `tessera_id` per artifact and no position in a level (C8), so a key
/// is the only address a caller holds. Refused if the target does not exist yet: an edge names a
/// position in a dense level, and one written first would name whatever later landed there.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AttachmentBody {
    layer: String,
    /// Defaults to the target layer's only level, as `level` does for the batch itself.
    #[serde(default)]
    level: u32,
    key: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct IncomingContentBody {
    /// One value per kind the layer declares, **positionally** — the order of its
    /// `content.supplied` list.
    values: Vec<String>,
    /// The documents this content was generated from: the set a viewer must be able to see
    /// **entirely** before they are served it. Addressed exactly as `members` is, by the batch's
    /// one `addressing` form, and resolved in the same pass.
    ///
    /// Empty is a declaration and not an omission — it says *this content asserts nothing about
    /// the corpus* — and it is refused on a layer whose content is corpus-derived.
    #[serde(default)]
    generated_from: Vec<String>,
}

/// `PUT /control/layers/{name}/artifacts` — publish a batch of artifacts into one level.
///
/// **The batch is the commit unit.** Ordinals are claimed contiguously from the level's cursor, so
/// a partial acceptance would leave the level's dense addressing describing artifacts that do not
/// exist. Every check therefore runs before anything is allocated, and a refusal spends nothing.
///
/// **Members are resolved to entities here, once, at the boundary** — the rule `/control/changes`
/// follows and for the same reason. A `tessera_id` is a keyed permutation of entity space, so a
/// membership stored under one would be reinterpreted by the next key rotation and would name a
/// different set of documents (I10, decision 0025).
///
/// **An unresolvable member refuses the batch rather than being dropped.** A silently dropped
/// member shrinks both the masked count a viewer is shown and the declared size the proportional
/// criterion divides by — so a typo in a pipeline would quietly move artifacts across their own
/// existence threshold, in the direction of hiding them, with nothing anywhere saying so.
///
/// The response carries a `tessera_id` per artifact and **never an ordinal**: an ordinal is a
/// position in a dense level, so two of them disclose how many artifacts sit between — a
/// corpus-wide count over objects the holder may not individually see (C8).
async fn publish_artifacts(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(name): axum::extract::Path<String>,
    body: Json<PublishBody>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let PublishBody {
        level,
        addressing,
        idset,
        default_space,
        mut artifacts,
    } = body.0;

    if artifacts.is_empty() {
        return Err(ApiError::Contract(
            "a publication carries at least one artifact".to_string(),
        ));
    }

    // **The shapes, canonicalised before anything is resolved or allocated** — a refusal spends
    // nothing, and the batch is the commit unit. The layer's declaration is the engine's state;
    // a layer this deployment does not hold is the engine's refusal below, so here it simply
    // canonicalises nothing.
    let default_space = match default_space.as_deref() {
        None => tessera_engine::shapes::ShapeSpace::View,
        Some(word) => tessera_engine::shapes::ShapeSpace::parse(word)
            .map_err(|e| ApiError::Contract(format!("`default_space`: {e}")))?,
    };
    let declaration = state
        .engine
        .registered_layer(&name)
        .map(|registered| registered.declaration);
    let mut shapes: Vec<Option<tessera_lifecycle::membership::ArtifactShapes>> =
        Vec::with_capacity(artifacts.len());
    let mut shape_reports: Vec<serde_json::Value> = Vec::new();
    for (index, artifact) in artifacts.iter().enumerate() {
        match &declaration {
            Some(declaration) => {
                match canonical_row_shape(&state, declaration, index, artifact, default_space)? {
                    Some((canonical, report)) => {
                        shapes.push(Some(canonical));
                        shape_reports.push(serde_json::json!({
                            "key": artifact.key,
                            "views": report,
                        }));
                    }
                    None => shapes.push(None),
                }
            }
            None => shapes.push(None),
        }
    }
    // **The authored shape content, read as a membership shape is** (`polygon-membership.md`
    // §6.1, ruling (h)): where the layer declares a `polygon`, `circle` or `ellipse` content, that
    // slot of every ranked content is canonicalised for every view of the layer — the same
    // reader, the same report, the same vertex cap — and the slot then holds the canonical bytes
    // in their content spelling, which is what the blob stores and the serve reads back into
    // `shape_x`/`shape_y`. Refused as a membership shape is refused, naming the row.
    if let Some((slot, kind)) = declaration.as_ref().and_then(|d| d.authored_shape()) {
        let declaration = declaration.as_ref().expect("an authored slot names a declaration");
        for (index, artifact) in artifacts.iter_mut().enumerate() {
            for (rank, content) in artifact.content.iter_mut().enumerate() {
                let Some(text) = content.values.get_mut(slot) else {
                    // Short of a value: the engine refuses the row below, naming the count.
                    continue;
                };
                let (canonical, report) = canonical_authored_content(
                    &state,
                    declaration,
                    index,
                    rank,
                    kind,
                    text,
                )?;
                shape_reports.push(serde_json::json!({
                    "key": artifact.key,
                    "content": rank,
                    "views": report,
                }));
                *text = canonical.content_text();
            }
        }
    }

    // Flattened once, so each address form is resolved in a single batched call whatever the shape
    // of the batch: the external half opens each bundle extent at most once regardless of N, and
    // the tessera half takes one generation snapshot for the idset check and every inversion.
    // **Members first, then each content's generating set**, per artifact — one flat list, one
    // resolution pass, whatever the shape. A generating set is resolved by the same route and at
    // the same boundary as a membership, and for the same reason: a `tessera_id` in durable state
    // would be reinterpreted by the next key rotation, and a containment test over a set that names
    // different documents than the caller wrote is a disclosure rather than a stale answer.
    let widths: Vec<usize> = artifacts
        .iter()
        .map(|a| a.members.len() + a.content.iter().map(|v| v.generated_from.len()).sum::<usize>())
        .collect();
    let flat: Vec<&String> = artifacts
        .iter()
        .flat_map(|a| {
            a.members
                .iter()
                .chain(a.content.iter().flat_map(|v| v.generated_from.iter()))
        })
        .collect();

    let resolved: Vec<Option<tessera_types::EntityId>> = match addressing {
        Addressing::Tessera => {
            let idset = idset.ok_or_else(|| {
                ApiError::Contract(
                    "tessera-addressed members carry the idset they were minted under, so a list \
                     gathered before a key rotation is refused rather than reinterpreted"
                        .to_string(),
                )
            })?;
            let ids = flat
                .iter()
                .map(|raw| {
                    raw.parse::<u64>()
                        .map(TesseraId::new)
                        .map_err(|_| ApiError::Contract(format!("'{raw}' is not a tessera_id")))
                })
                .collect::<Result<Vec<_>, _>>()?;
            // Refuses with `StaleIdSet` before inverting anything — see `resolve_tessera_ids`.
            state
                .engine
                .resolve_tessera_ids(&ids, idset)
                .map_err(crate::error::map_engine_error)?
        }
        Addressing::External => {
            if idset.is_some() {
                return Err(ApiError::Contract(
                    "idset accompanies tessera_id, never external_id: an external id means \
                     nothing to a key, so there is nothing for an idset to check"
                        .to_string(),
                ));
            }
            let keys: Vec<Vec<u8>> = flat
                .iter()
                .map(|s| {
                    base64::engine::general_purpose::STANDARD
                        .decode(s)
                        .map_err(|e| {
                            ApiError::Contract(format!("a member external_id is not base64: {e}"))
                        })
                })
                .collect::<Result<_, _>>()?;
            state
                .engine
                .resolve_external_ids(&keys)
                .map_err(map_store_error)?
        }
    };

    if let Some(position) = resolved.iter().position(Option::is_none) {
        // Reported as `(artifact, member)` rather than as a flat offset, which is the coordinate
        // the caller's own pipeline holds. The identifier itself is not echoed: it is the caller's
        // data, and `error.rs` keeps caller-supplied bytes out of a response body.
        let (artifact, member) = position_in_batch(&widths, position);
        return Err(ApiError::Unknown(format!(
            "id {member} of artifact {artifact} names nothing this deployment holds — its members \
             first, then each content's generating set. The batch was refused rather than \
             published without it: a dropped member moves both the count a viewer is shown and the \
             size its existence criterion divides by, and a dropped generating-set entry widens \
             who may read the content"
        )));
    }

    // Walked back in exactly the order it was flattened: members, then each content's set.
    let mut entities = resolved.into_iter().flatten();
    let incoming: Vec<tessera_lifecycle::IncomingArtifact> = artifacts
        .into_iter()
        .zip(shapes)
        .map(|(artifact, shape)| {
            let members: Vec<tessera_types::EntityId> =
                entities.by_ref().take(artifact.members.len()).collect();
            let contents: Vec<tessera_lifecycle::membership::IncomingContent> = artifact
                .content
                .into_iter()
                .map(|v| {
                    let set: Vec<tessera_types::EntityId> =
                        entities.by_ref().take(v.generated_from.len()).collect();
                    tessera_lifecycle::membership::IncomingContent::new(v.values, set)
                })
                .collect();
            let attached_to = artifact.attached_to.map(|a| {
                tessera_lifecycle::membership::IncomingAttachment {
                    layer: a.layer,
                    level: a.level,
                    key: a.key,
                }
            });
            let mut incoming = match attached_to {
                None => tessera_lifecycle::IncomingArtifact::with_content(
                    artifact.key,
                    members,
                    contents,
                ),
                Some(attached_to) => tessera_lifecycle::IncomingArtifact::attached(
                    artifact.key,
                    members,
                    contents,
                    attached_to,
                ),
            };
            incoming.shape = shape;
            incoming
        })
        .collect();
    let keys: Vec<Option<String>> = incoming.iter().map(|a| a.key.clone()).collect();

    // The **shared** blocking pool, on `register_layer`'s argument: a publication is not a deny,
    // and delaying one under ingest load is backpressure working.
    let ids = tokio::task::spawn_blocking(move || {
        state.engine.publish_artifacts(name, level, incoming)
    })
    .await
    .map_err(crate::error::map_join_error)?
    .map_err(crate::error::map_accept_error)?;

    let published: Vec<serde_json::Value> = ids
        .iter()
        .zip(keys)
        .map(|(id, key)| serde_json::json!({ "key": key, "tessera_id": id.raw().to_string() }))
        .collect();
    // **The same report the build prints, in the body** (`polygon-membership.md` §6.4): what
    // canonicalisation did to each shape, per view, and what its decomposition holds. Absent
    // where the layer declares no shape, so the enumerated case's response is unchanged.
    let mut body = serde_json::json!({ "artifacts": published });
    if !shape_reports.is_empty() {
        body["shapes"] = serde_json::Value::Array(shape_reports);
    }
    Ok((StatusCode::CREATED, Json(body)))
}

/// `POST /control/faults/arm` — the correctness suite's arming surface (decision 0071;
/// correctness-suite §12.3). **The faults build only**; a default build does not mount the route.
///
/// Minimal by design — arm a named site, observe a thread has arrived
/// ([`faults_arrivals`]), release ([`faults_release`]) — because that is everything a driver
/// needs to pause, kill and resume. Everything else the switchboard offers (failure injection,
/// the step log, `fire_after` counts, the panic action) stays in-process, where a test can hold
/// the invariants that make it meaningful.
///
/// `Stall` is the only action the wire can arm, deliberately: a crash is the driver's own
/// `SIGKILL` against a demonstrably parked process, and the panic action models an unwind — drop
/// guards run, the WAL closes — which no crash test may read as a crash
/// (`tessera_lifecycle::faults::PauseAction`'s own rule).
#[cfg(feature = "fault-injection")]
async fn faults_arm(
    State(state): State<Arc<AppState>>,
    Json(req): Json<FaultSiteRequest>,
) -> Result<StatusCode, ApiError> {
    let site = parse_pause_site(&req.site)?;
    state
        .faults
        .arm_pause(site, tessera_lifecycle::faults::PauseAction::Stall);
    Ok(StatusCode::OK)
}

/// `GET /control/faults/arrivals?site=<name>` — how many times the executor has reached the
/// named site since it was armed. The driver polls this rather than sleeping: a non-zero count
/// under a stall arming means a thread is demonstrably parked, which is the precondition for a
/// kill (correctness-suite §12.3's crash modifier).
#[cfg(feature = "fault-injection")]
async fn faults_arrivals(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(req): axum::extract::Query<FaultSiteRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let site = parse_pause_site(&req.site)?;
    Ok(Json(serde_json::json!({
        "site": site.name(),
        "arrivals": state.faults.arrivals(site),
    })))
}

/// `POST /control/faults/release` — release every parked thread and disarm every site. The
/// release-all form, matching `FaultSwitchboard::release`: a driver that finished (or abandoned)
/// a leg must not be able to leave the executor wedged on a site it forgot.
#[cfg(feature = "fault-injection")]
async fn faults_release(State(state): State<Arc<AppState>>) -> StatusCode {
    state.faults.release();
    StatusCode::OK
}

/// The one JSON/query shape both fault routes share: a pause site, by its switchboard name.
#[cfg(feature = "fault-injection")]
#[derive(serde::Deserialize)]
struct FaultSiteRequest {
    site: String,
}

#[cfg(feature = "fault-injection")]
fn parse_pause_site(name: &str) -> Result<tessera_lifecycle::faults::PauseSite, ApiError> {
    tessera_lifecycle::faults::PauseSite::from_name(name).ok_or_else(|| {
        ApiError::Contract(format!(
            "unknown pause site {name:?}; the sites are after_fsync, before_ack, \
             before_manifest_publish, before_current_flip, before_merge_publish"
        ))
    })
}

/// **The precedent for [`require_operator_credential`], and the reason it is a layer.** R5 requires
/// bearer auth on every plane, including this one; this handler nevertheless *shipped* returning
/// `entity_id_high_water` — a global, unmasked corpus-size fact — to anyone who could reach the
/// control listener, which may be loopback TCP and not only a unix socket
/// (`config::ControlListen::Tcp`). Nothing was wrong with the check; there simply was not one, and
/// no reviewer noticed, because "every control handler calls `check_bearer`" was a convention rather
/// than a construction. This handler deliberately carries no `check_bearer` of its own: the layer
/// refuses the route before the handler is entered, and a redundant copy would only make the layer's
/// mutation tests pass for the wrong reason.
async fn status(State(state): State<Arc<AppState>>) -> Result<Json<serde_json::Value>, ApiError> {
    // The viewer/session admission gate's gauges. `in_flight`/`waiting` are read live off the
    // semaphores; `shed_total` is a single process-wide counter — no per-principal labels anywhere
    // on this plane (SA §9). `shed_total` counts only this gate's own two shed paths — it does NOT
    // include the engine's single-flight builder 429s
    // (`ProjectionBuilding`/`FragmentBuilding`), which happen after admission and are invisible to
    // this gate (see `ComputeGate::shed_total`'s doc).
    let gate = state.compute_gate.status();
    // The write executor's posture and counters. **This is where the posture string lives** — the
    // bearer-gated plane — because `/readyz`, on the viewer and session listeners, is
    // unauthenticated and must stay a bare boolean (SA §9; see `health.rs`). It is deliberately not
    // served here at all, so on this listener the posture has exactly one door and it needs the
    // credential. `ready` is computed by the *same* `is_ready` the probe
    // calls, not a second predicate, so the two can never drift.
    //
    // Contracts §3.4's per-partition block — `{segments_version, watermark, readiness}` per
    // partition — is emitted below as `partitions`. `readiness` appears there *and* as
    // `write_executor.ready`: the first is the contract's field, the second is the operator's
    // glance beside the posture string it is derived from, and both are this one evaluation of
    // the same `is_ready` the probes call, so the three surfaces cannot drift.
    let executor = state.engine.write_executor_stats();
    let ready = is_ready(executor.posture);
    let partitions = state.engine.partition_status();
    // **`tessera_engine::FragmentCacheStats`, never `tessera_authz::...`** — `check-layers.sh`
    // denies a `tessera-server → tessera-authz` edge (SA §3), and the re-export at
    // `tessera-engine`'s crate root exists precisely so this call site has a nameable type.
    let projection_cache: tessera_engine::CacheStats = state.engine.row_projection_cache_stats();
    let fragment_cache: tessera_engine::FragmentCacheStats = state.engine.fragment_cache_stats();
    let ingest = state.ingest_admission.status();
    let sessions = state.sessions.lock().stats();
    // **`tessera_engine::ViewSegments`, for `FragmentCacheStats`' reason** — the server may not
    // depend on `tessera-store`, where the segment set actually lives.
    let segments: Vec<tessera_engine::ViewSegments> = state.engine.live_segment_counts();
    Ok(Json(serde_json::json!({
        "entity_id_high_water": state.engine.allocator_high_water(),
        // Contracts §3.4's per-partition block, and the write path's stage barrier
        // (correctness-suite §12.3): the version bump is how a harness knows a flush, merge or
        // fold *published* rather than was accepted, and the watermark is which entities the
        // published geometry covers — without them the only barrier is a sleep. A vector of one
        // until partitioning lands; the values are read in one generation load
        // (`Engine::partition_status`), so the pair cannot straddle a publication.
        "partitions": partitions
            .iter()
            .map(|p| serde_json::json!({
                "partition": p.partition,
                "segments_version": p.segments_version,
                "watermark": p.watermark,
                "readiness": ready,
            }))
            .collect::<Vec<_>>(),
        "compute": {
            "admission": gate.admission,
            "queue": gate.queue,
            "in_flight": gate.in_flight,
            "waiting": gate.waiting,
            // Emit-phase streams: slot held, compute released (`streamed-serving.md` §5).
            "streaming": gate.streaming,
            "shed_total": gate.shed_total,
        },
        "write_executor": {
            "posture": executor.posture.as_str(),
            "ready": ready,
            "work_submitted": executor.work_submitted,
            "deny_submitted": executor.deny_submitted,
            "wal_appends": executor.wal_appends,
            "wal_fsyncs": executor.wal_fsyncs,
            // The durability incident's only surviving trace. `posture` returns to `running` once a
            // WAL fault clears — which is what stops a transient device error costing a node its
            // routing for the life of the process — so a reader watching the posture alone sees
            // nothing afterwards. This is the number to alarm on: rising at all means denies were
            // answered 500 and their callers owe retries (contracts §3.1); rising repeatedly means a
            // disk failing slowly.
            "wal_recoveries": executor.wal_recoveries,
            "apply_nanos_total": executor.apply_nanos_total,
            "apply_nanos_max": executor.apply_nanos_max,
            // The queue-depth gauge and the drain estimate `retry_after_s` is derived from.
            // `work_depth` is a snapshot of two independently-advancing counters — see
            // `ExecutorStats::work_depth` — and `work_service_nanos_ewma` is an estimator, not a
            // bound; `estimate_retry_after_s`'s doc says what makes it one.
            "work_completed": executor.work_completed,
            "work_depth": executor.work_depth,
            "work_service_nanos_ewma": executor.work_service_nanos_ewma,
            // The flush cadence's operator surface (write-path §4.7; out of contract, §0.1).
            // `flush_skips` rising is the alarm the log line carries — a flush persistently
            // slower than its tick is a visibility-latency breach — and `flushable_items` is the
            // backlog gauge that distinguishes a gated node (stays at zero) from a failing one
            // (grows). `buffered_items` is the occupancy the ingest 429 is checked against; note
            // it counts items, not bytes (write-path §2.1).
            "flush": {
                "ticks": executor.ticks,
                "flushes": executor.flushes,
                "flush_skips": executor.flush_skips,
                "flush_failures": executor.flush_failures,
                "flushable_items": executor.flushable_items,
                "flush_requested": executor.flush_requested,
                "buffered_items": executor.buffered_items,
                "overlay_publications": executor.overlay_publications,
                // The visibility barrier's second half (decision 0044 D1; correctness-suite
                // §12.3): an established session serves its old row projection until the
                // background refresh replaces it, so a count read after the version bump alone
                // is short by exactly the round's batch — measured, not supposed. This counts
                // projections the refresh *produced* (`Engine::refreshes`), so it moves only
                // where a session's projection was resident to refresh, and it also moves at a
                // merge, whose publication runs the same pass.
                "refreshes": state.engine.refreshes(),
            },
            // The two maintenance publications, beside the flush counters for the same reader.
            // `merges` pairs with a `segments_version` bump; `coalesces` is the one stage whose
            // barrier *must* be a counter — the entity-space coalesce moves no row and bumps no
            // version by design (write-path §7), so this is the only wire evidence one ran.
            "coalesces": executor.coalesces,
            "merges": executor.merges,
            // Published beside the EWMA rather than folded into it: the EWMA is written only when a
            // job finishes, so during one long job it reports the previous regime. The 429
            // derivations take `max` of the two (`ExecutorStats::service_nanos_for_estimate`); an
            // operator gets both numbers because "the last ten jobs took 3 ms and this one has been
            // running for 90 s" is the diagnosis, and either figure alone hides it.
            "work_in_flight_nanos": executor.work_in_flight_nanos,
        },
        // `admission` is the bound, `in_flight` is read live off the semaphore.
        // `shed_total` counts **this bound's** 429s only — the queue-full 429 is produced inside
        // the engine and is not counted here; `work_depth` above is its gauge.
        "ingest": {
            "admission": ingest.admission,
            "in_flight": ingest.in_flight,
            "shed_total": ingest.shed_total,
            "max_batch_rows": state.ingest_max_batch_rows,
            "max_batch_bytes": state.ingest_max_batch_bytes,
        },
        // **A read-path constant, not a maintenance counter** (decision 0049). Merge's size ladder
        // saturates at `max_merged_segment_bytes`, so this settles at corpus bytes ÷ the saturation
        // size and then tracks the corpus — ~152 segments at 10⁹, which a 300-tile viewport pays
        // ~73 ms for against a 135–164 ms baseline. It is immaterial at 10⁷ and a ~50% regression by
        // 10⁹, so it is invisible to a soak and needs a gauge. Rising past the low hundreds means
        // merge has stopped bounding it and only a fold will reset it.
        //
        // A fold resets it to one segment per partition-view, and the schedule dispatches one at
        // `compaction_max_segments` at any hour, or at `compaction_window_min_segments` inside the
        // nightly window (compaction §9, decision 0056). So this gauge now has a lever, and reading
        // it climbing past the ceiling means the fold is being *refused* rather than not
        // scheduled — check the interval floor, the executor's gates, and the `compaction` block
        // below. `POST /control/compact` asks for one out of band.
        //
        // A list rather than a scalar because the trigger compaction §9 specifies is per
        // (partition, view); this build emits one of each, so the list has one element and will not
        // always.
        "segments": segments
            .iter()
            .map(|s| serde_json::json!({
                "partition": s.partition,
                "view": s.view,
                "count": s.segments,
            }))
            .collect::<Vec<_>>(),
        // **The alarm and the trigger read different numbers, deliberately.** `depth` is
        // `deleted ∪ suppressed` — the right thing for an operator to see — while the schedule's
        // retirable-depth route keys on the deletions alone, because a suppression never retires
        // and a fold keyed on the union would rewrite the corpus to retire nothing (compaction §9).
        // So `soft_limit_alarms` rising on a suppression-heavy deployment is a signal to look, not
        // a fold waiting to happen. `depth` is read off the live generation, so it cannot drift
        // from what a request composes against.
        // `retirable` is the same difference made legible: a suppression-heavy deployment has a
        // deep overlay and nothing for a fold to do, and only the pair says so.
        "overlay": {
            "depth": state.engine.overlay_depth(),
            "retirable": state.engine.retirable_deletions(),
            "soft_limit_alarms": executor.overlay_soft_limit_alarms,
        },
        // **The most expensive operation in the system, and until this block its only surface was a
        // log line.** A fold re-reads and rewrites the corpus, retires deletions, rotates the
        // bundle identity and reclaims the superseded prefix; `folds` and `fold_failures` had been
        // counted since it was built and published nowhere.
        //
        // `fold_failures` is the one to alarm on, and it is not the mirror of `folds`. Every
        // failure leaves a complete prefix under a name `CURRENT` never took — a bundle-sized tree
        // that nothing reclaims (compaction §7's startup sweep is ⊘) — so a fold that keeps
        // discarding costs disc before it costs anything else, and several discard causes are
        // *persistent*. Rising at all means read the log for the reason; rising repeatedly means
        // the interval floor is the only thing between the deployment and a full device.
        //
        // `last_secs` and `last_rss_bytes` are the last fold's cost. **`last_rss_bytes` is a
        // staircase maximum sampled at five pass boundaries, not a peak** — a spike inside a pass
        // is invisible to it, and probe P1 is what says how far under the true peak it sits. It is
        // published because compaction §3's memory budget is a *modelled* figure and this is the
        // only number a deployment has to compare against it. `passes` is the same staircase
        // unreduced: the gauges alarm, and the per-pass rows say which pass to look at.
        // **`live_rows` is the denominator of compaction §9's tombstoned-row gauge**, and the only
        // figure here that says how large the corpus is. The gauge itself is not published as a
        // ratio: an operator with the numerator (`overlay.retirable`) and the denominator can form
        // it, and a third derived number is a third thing to keep consistent.
        //
        // **`last_attr_bytes_*` are reported and never triggered on** (`filter-index.md` §6.2). The
        // attribute pass is the fold's streaming term — ~12 GB per `u32` column at 10⁹ read,
        // written and re-read for the digest — and the staircase attributes its time and residency
        // but not its IO, which is the axis the non-disruption claim is made on. They are not
        // gauges twice over: §5.2's coalesce bounds the extent axis continuously, and what escapes
        // it moves with the segment axis compaction §9 already gauges.
        //
        // **The dead-bytes gauge is deliberately absent.** It is a walk of the live prefix, and the
        // schedule pays for it at most once per tick and only when every cheaper route has
        // declined; recomputing it on every `/control/status` poll would make an operator's
        // dashboard the most expensive thing on the node. The trigger's own log line reports it
        // when it fires.
        "compaction": {
            "live_rows": state.engine.live_rows(),
            "folds": executor.folds,
            "fold_failures": executor.fold_failures,
            "fold_requested": executor.fold_requested,
            "last_secs": executor.last_fold_secs,
            "last_rss_bytes": executor.last_fold_rss,
            "last_attr_bytes_read": executor.last_fold_attr_read,
            "last_attr_bytes_written": executor.last_fold_attr_written,
            "passes": state.engine.last_fold_passes()
                .iter()
                .map(|p| serde_json::json!({
                    "pass": p.pass,
                    "ms": p.elapsed.as_millis() as u64,
                    "rss_bytes": p.rss,
                    "anon_bytes": p.anon,
                }))
                .collect::<Vec<_>>(),
        },
        // Contracts §3.4's `fragmentation`. Design §11.1 assigns entity ids in term-signature order
        // within one allocation run and nothing repairs the ordering afterwards, so the posting
        // compression a deployment collects erodes with the fraction of its corpus that arrived in
        // small runs — invisibly, because segment counts, watermark and overlay size all stay
        // healthy while union cost climbs. These two numbers are what make it observable, and they
        // are what lets the deferred index-ordinal split trigger on evidence rather than suspicion.
        //
        // **The headline ratios are tier-scope**: each delta tier is measured as encoded at its
        // flush, and a tier spans every commit window the buffer accumulated between ticks — so
        // between-window scatter, the erosion §11.1 records as permanent, is visible here where
        // the old window-scope ratios were structurally blind to it (both their run count and
        // their baseline were taken inside one window). Base postings are still outside the
        // figure: they are one build-time global sort whose contribution is static, and folding
        // them in arrives with compaction. `scope` stays in the body because a consumer reads
        // JSON, never a document.
        //
        // **`null`, not `0.0`, before the first flush publishes.** Zero is a value of this
        // quantity (`run_ratio = 1.0` is fully scattered; `0` is not reachable at all), so
        // publishing one for "nothing measured yet" would be a reading rather than an absence.
        //
        // The window-scope raw counters ride along under `allocation` (§0.1's out-of-contract
        // clause), ratios withheld deliberately: at any reachable window size they answered
        // "how well did one sorted run do against a random shuffle of itself", which reads as the
        // health signal and is not it.
        "fragmentation": {
            "scope": "delta-tiers",
            "postings_per_container": executor.tier_postings_per_container(),
            "run_ratio": executor.tier_run_ratio(),
            "postings": executor.tier_fragmentation.postings,
            "runs": executor.tier_fragmentation.runs,
            "containers": executor.tier_fragmentation.containers,
            "rows": executor.tier_fragmentation.rows,
            "tiers": executor.fragmentation_tiers,
            "allocation": {
                "scope": "commit-window-allocation",
                "postings": executor.fragmentation.postings,
                "runs": executor.fragmentation.runs,
                "containers": executor.fragmentation.containers,
                "rows": executor.fragmentation.rows,
                "windows": executor.fragmentation_windows,
            },
        },
        // **`young_evictions` is an alarm, not an undifferentiated counter, and `thrashing` is the
        // predicate spelled out.** `> 0` is the argued threshold, not an arbitrary one: `prepare`
        // refuses at startup any bound below `expected_concurrent_sessions × the measured
        // per-entry size (see `validate_cache_bounds`), so a young eviction means the collapsing
        // regime was entered *another* way — a second view per session, a generation swap's
        // transient duplicate, or entries larger than the measured figure. That is precisely what
        // `validate_cache_bounds`' own doc says this counter is for.
        "row_projection_cache": {
            "entries": projection_cache.entries,
            "bytes": projection_cache.bytes,
            "bound_bytes": projection_cache.bound_bytes,
            "hits": projection_cache.hits,
            "misses": projection_cache.misses,
            // **`building_refusals` and `waits_satisfied` are one figure split in two** (decision
            // 0058): same-key racers used to be refused and counted here, and are now served and
            // counted there. Sum them for the race rate an older dashboard read off this field
            // alone. `waiters_now` is a gauge, not a total, and is process-wide — it says whether
            // parked requests are holding compute permits, never whose (decision 0059).
            "building_refusals": projection_cache.building_refusals,
            "waits_satisfied": projection_cache.waits_satisfied,
            "waiters_now": projection_cache.waiters_now,
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
        // The registry sheds expired sessions on a growth-triggered sweep (`SessionRegistry`), and
        // all four numbers are here because the third of them is what makes the second admissible:
        // the sweep is an O(`retained`) pass under the mutex every viewer request takes, and this
        // repository's standard is that such a pass is acceptable only where its `n` is observable.
        // `sweep_at` is the bound above `retained` — `max(2 × live, 16)` — so an operator reads the
        // policy rather than inferring it.
        //
        // What the pair says: `retained` rising while `swept_total` stays at zero means the
        // registry is not shedding, which matters beyond the map entries because each retained
        // session pins an `Arc<FrozenFragment>` and `fragment_cache.bytes` falling therefore does
        // not mean that memory was released. Sweeping is memory hygiene only — an expired session
        // is refused by the deadline check whether or not a sweep has run, and a revocation takes
        // effect in its own handler.
        "sessions": {
            "retained": sessions.retained,
            "sweeps": sessions.sweeps,
            "swept_total": sessions.swept_total,
            "sweep_at": sessions.sweep_at,
        },
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    mod category_wire {
        use super::*;
        use arrow::array::{Float32Array, StringArray, UInt8Array};
        use arrow::datatypes::{DataType, Field, Schema};
        use arrow::record_batch::RecordBatch;
        use std::sync::Arc;
        use tessera_engine::{DeclaredScalar, Vocabularies};

        const CODE_OPS: u32 = 4711;

        fn declared() -> Vec<DeclaredScalar> {
            vec![
                DeclaredScalar {
                    name: "department".to_string(),
                    arrow_type: ScalarType::U16,
                    vocabulary: Some("departments".to_string()),
                    analyser: None,
                    index: false,
                    render: true,
                },
                DeclaredScalar {
                    name: "score".to_string(),
                    arrow_type: ScalarType::F32,
                    vocabulary: None,
                    analyser: None,
                    index: false,
                    render: true,
                },
            ]
        }

        fn vocabularies() -> Vocabularies {
            vocabularies_of(VocabularyKind::Declared)
        }

        fn vocabularies_of(kind: VocabularyKind) -> Vocabularies {
            Vocabularies::seed(
                &[tessera_engine::ManifestVocabulary {
                    name: "departments".to_string(),
                    kind,
                    visibility: tessera_engine::Visibility::Derived,
                    values: vec![tessera_engine::ManifestVocabularyValue {
                        key: "ops".to_string(),
                        code: CODE_OPS,
                        title: None,
                    }],
                    reserved: Vec::new(),
                }],
                &declared(),
                &[],
            )
            .expect("the fixture bundle is consistent")
        }

        /// One batch of the fixed columns plus `department` (as `column`) and `score`.
        fn body(column: arrow::array::ArrayRef, nullable: bool) -> Vec<u8> {
            let schema = Arc::new(Schema::new(vec![
                Field::new("x", DataType::Float32, false),
                Field::new("y", DataType::Float32, false),
                Field::new("access", DataType::Utf8, false),
                Field::new("department", column.data_type().clone(), nullable),
                Field::new("score", DataType::Float32, false),
            ]));
            let batch = RecordBatch::try_new(
                Arc::clone(&schema),
                vec![
                    Arc::new(Float32Array::from(vec![0.5])),
                    Arc::new(Float32Array::from(vec![0.5])),
                    Arc::new(StringArray::from(vec!["public"])),
                    column,
                    Arc::new(Float32Array::from(vec![1.0])),
                ],
            )
            .expect("the fixture batch is well-formed");
            let mut out = Vec::new();
            {
                let mut w = arrow::ipc::writer::StreamWriter::try_new(&mut out, &schema).unwrap();
                w.write(&batch).unwrap();
                w.finish().unwrap();
            }
            out
        }

        fn parse(
            column: arrow::array::ArrayRef,
            nullable: bool,
        ) -> Result<Vec<RawIngestItem>, ApiError> {
            parse_ingest_batch(
                &body(column, nullable),
                Projection::None,
                &declared(),
                &vocabularies(),
                &no_layers,
            )
            .map(|parsed| parsed.items)
        }

        /// These fixtures declare no layer, so every column here is a scalar or a refusal —
        /// the membership column has its own cases in `tests/membership_column.rs`, over HTTP and
        /// against a registry that holds one.
        fn no_layers(_: &str) -> Option<tessera_types::layer::LayerDeclaration> {
            None
        }

        /// A known key becomes its **pinned** code at the column's declared width. The code is
        /// never re-derived from the data, so this is the whole of what the wire decides.
        #[test]
        fn a_known_key_is_stored_as_its_pinned_code() {
            let items = parse(Arc::new(StringArray::from(vec!["ops"])), false)
                .expect("a declared key is accepted");
            assert_eq!(
                items[0].scalars[0],
                WalScalar::U16(CODE_OPS as u16),
                "the row carries the vocabulary's code, at the declared width"
            );
        }

        /// **Declare-then-use** (§5, views §80): a category carries properties and a visibility
        /// consequence, so a typo must not create one. The refusal names both the column and the
        /// key, and the whole batch is without effect.
        #[test]
        fn an_unknown_key_is_refused_naming_the_column_and_the_key() {
            let err = parse(Arc::new(StringArray::from(vec!["k9-unit"])), false)
                .expect_err("an undeclared key is refused");
            let ApiError::Contract(detail) = err else {
                panic!("declare-then-use is a contract violation, not a server error");
            };
            assert!(detail.contains("department"), "{detail}");
            assert!(detail.contains("k9-unit"), "{detail}");
        }

        /// **The hole this closes.** A code on the wire was accepted by range alone, so an
        /// unassigned code, a `reserved` code or a typo was stored with no error anywhere. The
        /// wire type is now `utf8`, so the same batch is a 422 naming the column.
        #[test]
        fn a_code_on_the_wire_is_refused_where_it_used_to_be_stored() {
            let err = parse(Arc::new(UInt8Array::from(vec![9u8])), false)
                .expect_err("a category is utf8 on the wire, whatever stores its codes");
            let ApiError::Contract(detail) = err else {
                panic!("a wrong wire type is a contract violation");
            };
            assert!(detail.contains("department"), "{detail}");
            assert!(detail.contains("utf8"), "{detail}");
        }

        /// Null means *absent* — the reserved code 0, which is why a `u8` category holds 255
        /// values and not 256.
        #[test]
        fn a_null_key_is_absent() {
            let items = parse(
                Arc::new(StringArray::from(vec![None as Option<&str>])),
                true,
            )
            .expect("an item may carry no value for a column");
            assert_eq!(items[0].scalars[0], WalScalar::U16(ABSENT_CODE as u16));
        }

        /// The empty string is **not** absence. It is what an unset field and a client bug both
        /// produce, so folding it into code 0 would accept the same defect silently.
        #[test]
        fn the_empty_string_is_refused_rather_than_folded_into_absence() {
            let err = parse(Arc::new(StringArray::from(vec![""])), false)
                .expect_err("the empty string is not a value key");
            let ApiError::Contract(detail) = err else {
                panic!("an empty key is a contract violation");
            };
            assert!(detail.contains("department"), "{detail}");
        }

        /// **Under a discovered vocabulary a novel key travels as a key**, for the write executor
        /// to mint against the live bindings.
        ///
        /// The handler must not mint it here. Two requests racing one novel key would each draw,
        /// and that key would end up with two codes with its rows split between them — whichever
        /// binding survived would recolour the other's rows, silently. Windows close serially, so
        /// resolving there is what makes the two agree.
        #[test]
        fn a_novel_key_under_a_discovered_vocabulary_travels_unresolved() {
            let items = parse_ingest_batch(
                &body(Arc::new(StringArray::from(vec!["k9-unit"])), false),
                Projection::None,
                &declared(),
                &vocabularies_of(VocabularyKind::Discovered),
                &no_layers,
            )
            .expect("a discovered vocabulary accepts a key it has not seen")
            .items;
            assert_eq!(
                items[0].scalars[0],
                WalScalar::Utf8("k9-unit".to_string()),
                "the key reaches the executor as a key; a code here would be a handler that mints"
            );
        }

        /// A key the discovered vocabulary already binds resolves in the handler like any other —
        /// only the *novel* case needs the executor, so the common path costs no extra work.
        #[test]
        fn a_bound_key_under_a_discovered_vocabulary_still_resolves_here() {
            let items = parse_ingest_batch(
                &body(Arc::new(StringArray::from(vec!["ops"])), false),
                Projection::None,
                &declared(),
                &vocabularies_of(VocabularyKind::Discovered),
                &no_layers,
            )
            .expect("a bound key is bound whatever the kind")
            .items;
            assert_eq!(items[0].scalars[0], WalScalar::U16(CODE_OPS as u16));
        }

        /// A plain scalar of the same width is unchanged and still arrives as an integer — so a
        /// client that thinks a column is a category when the bundle says otherwise gets a 422
        /// naming it, rather than plausible integers stored as codes.
        #[test]
        fn a_plain_scalar_is_unaffected_by_the_category_rule() {
            let items = parse(Arc::new(StringArray::from(vec!["ops"])), false).unwrap();
            assert_eq!(items[0].scalars[1], WalScalar::F32(1.0));
        }
    }

    /// **The deny lane does not share tokio's blocking pool** — demonstrated rather than argued.
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
    /// negative statement about another thread's progress cannot be established without waiting.
    /// The sound content is that the deny lane completed while the ambient pool was demonstrably
    /// full, and that is what is asserted.
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

    /// **The lazy path must not drop a `Runtime` on the reactor.**
    ///
    /// `init_deny_runtime` builds a runtime and then `set`s it. `OnceLock::set` hands the value
    /// **back** on a lost race, so `let _ = DENY_RUNTIME.set(rt)` would drop the loser's runtime on
    /// that statement — and the lazy path is called from inside `changes()`'s async body, where
    /// `Runtime::drop`'s blocking shutdown panics with "Cannot drop a runtime in a context where
    /// blocking is not allowed". The panic is in the handler body, so `map_join_error` cannot see
    /// it: the connection drops with no status at all, which is an I13a violation — a panic must be
    /// a failed request, never an empty one.
    ///
    /// `prepare` initialises before any listener binds, so the shipped binary does not race. Every
    /// integration test does (`mount_server`/`spawn_server_from_engine` never call `prepare`), and
    /// so do embedders, which the lazy path exists for.
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
