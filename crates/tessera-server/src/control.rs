//! The control (admin) plane (R5): `POST /control/ingest`, `POST /control/changes`,
//! `GET /control/status`, plus `/healthz`/`/readyz`. Bearer auth is the operator credential.
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
/// `max_blocking_threads` threads (512 by default). Three facts compose badly:
///
/// - `/control/ingest` is behind **no admission bound at all** — the control plane is deliberately
///   never gated by `ComputeGate` (D13), so concurrent ingest handlers are bounded by nothing;
/// - an ingest closure holds its blocking thread across the Arrow decode, the plugin's
///   `terms_of_label` loop, the external-ID sidecar IO **and** its whole blocking wait on the
///   executor's receipt;
/// - a thread frees only when one of those completes, which costs an fsync plus an `IngestBuffer`
///   clone that is O(total buffered items).
///
/// So above ~512 in-flight ingest requests a suppression's closure queues **behind ingest closures,
/// inside tokio**, before it can reach the prioritised deny queue at all. That is exactly lifecycle
/// §1.3's forbidden shape — a deny queued behind work of unbounded duration — reintroduced one
/// layer *above* the priority lane, where the executor cannot see it. No test observed it: the
/// existing `concurrent_ingests_do_not_delay_a_control_changes_suppress` runs eight batches.
///
/// The viewer plane is not part of the problem in the same way and it is worth saying why, because
/// the asymmetry is the reason this fix is on the deny side: `ComputeGate::admit` is `async` and is
/// awaited **before** `spawn_blocking`, so a queued viewport holds no blocking thread and viewer
/// demand is bounded by `compute_admission`.
///
/// # Why a separate runtime rather than a bound on ingest
///
/// Bounding ingest needs an arithmetic over `max_blocking_threads` (tokio's default, set outside
/// this crate), `compute_admission` (config) and a new ingest bound — three operands, two of which
/// someone can move without touching this reasoning, and it is Task 6's arithmetic by charter.
/// A separate resource needs to know none of them and cannot be invalidated by a change to any:
/// CLAUDE.md's "structural, not disciplinary" test.
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
/// failed request, never an empty one".) The `OnceLock` is never dropped, which also means
/// `Runtime::drop` can never fire inside an async context.
static DENY_RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();

/// Build the deny lane's runtime, once. Called by `crate::prepare` so failure is a startup failure.
///
/// Idempotent: a second call is a no-op, so tests that build an `AppState` directly (without
/// `prepare`) reach the same runtime through [`deny_runtime`]'s lazy path.
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
        .build()?;
    let _ = DENY_RUNTIME.set(rt);
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

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/control/ingest", post(ingest))
        .route("/control/changes", post(changes))
        .route("/control/status", get(status))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .with_state(state)
}

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
    let rows: Vec<UnallocatedRow> = items
        .iter()
        .zip(&terms_per_item)
        .zip(descriptor_lists.iter())
        .map(|((item, terms), descriptors)| UnallocatedRow {
            external_id: item.external_id.clone(),
            descriptors: descriptors.clone(),
            x: item.x,
            y: item.y,
            scalars: item.scalars.clone(),
            terms: terms.clone(),
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

async fn ingest(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<IngestResp>, ApiError> {
    state.check_bearer(bearer_token(&headers), &state.operator_credential)?;

    let batch_id = headers
        .get("x-tessera-batch-id")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::Contract("missing x-tessera-batch-id header".to_string()))?
        .to_string();

    // D-A / review finding 7: closure capture is `state` (moved in directly — nothing after this
    // `.await` needs the handler's own copy), `body` (an owned `Bytes` — cheap, refcounted clone
    // of the request body already read off the socket, not a copy) and `batch_id` (owned
    // `String`). Never gated (see `run_ingest`'s doc).
    let resp = tokio::task::spawn_blocking(move || run_ingest(&state, &body, batch_id))
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
    // The **first** error is the one reported, so the status a caller sees does not depend on which
    // item happened to fail last. Validation is already wholesale above, so nothing reached here
    // can be a client-correctable fault: everything below is an infrastructure failure and every
    // one of them is alarmed individually.
    // **The batch's answer is a FOLD over dispositions, not the first item's status.** Task 3a
    // reported `first_error`, which is wrong in a way that matters now that the mapping table
    // distinguishes 503 from 500: an item's status describes an item. The constructible bad case is
    // "item 1 applied successfully, the executor then died, item 2 refused" — first-error reporting
    // has no error at all for item 1 and answers item 2's **503 `not-ready`**, i.e. "this node did
    // not take your write", for a batch containing a durable, in-force suppression.
    //
    // So every failure is collected with the count that succeeded, and `map_change_batch_error`
    // decides once, over all of them. See its doc for the rules and for why 500 dominates 503.
    let mut failures = Vec::new();
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
                failures.push(e);
            }
        }
    }

    match map_change_batch_error(&failures, applied) {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

async fn changes(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(items): Json<Vec<ChangeItem>>,
) -> Result<StatusCode, ApiError> {
    // **Deliberately outside the deny lane.** The bearer check stays on the reactor so an
    // unauthenticated flood cannot occupy `spawn_on_deny_lane`'s threads — the resource that fix
    // exists to keep free. Body decoding is likewise on the reactor, ahead of this check (an axum
    // extractor), which is pre-existing and bounded by axum's default body limit.
    state.check_bearer(bearer_token(&headers), &state.operator_credential)?;

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

async fn status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Important 1 fix: R5 requires bearer auth on every plane, including this one — this handler
    // previously returned `entity_id_high_water` (a global, unmasked corpus-size fact) to anyone
    // who could reach the control listener at all, which may be loopback TCP, not only a unix
    // socket (config.rs's `ControlListen::Tcp`).
    state.check_bearer(bearer_token(&headers), &state.operator_credential)?;
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
        },
        "pins": {
            "drain_depth": pins.drain_depth,
            "oldest_retired_secs": pins.oldest_retired_secs,
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
}
