//! The control (admin) plane: ingest, changes, declarations, status, flush and compact.
//! Every route requires the operator credential, checked once at the router by
//! [`require_operator_credential`]; `/healthz` and `/readyz` are served on the viewer and session
//! listeners, not here.
//!
//! A write is acknowledged only after its WAL append is fsynced: parse, allocate ids, append,
//! fsync, apply, then 200. A deletion or suppression whose append fails is still applied to the
//! live overlay and answered 500, because an accepted deny left unapplied would fail open.

use std::sync::Arc;

use arrow::array::Array;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};

use sha2::{Digest, Sha256};

use tessera_engine::{MetaView, ScopedScalar};
use tessera_lifecycle::ChangeOp;

use tessera_types::view::ViewMetadataValue;
use tessera_types::{EntityId, TesseraId};

use crate::decode::{
    labels_col, parse_ingest_batch, Address, BodyEncoding, DecodeError, ParsedBatch,
};
use crate::error::{
    map_accept_error, map_change_batch_error, map_engine_error, map_join_error, map_store_error,
    ApiError,
};
use crate::health::is_ready;
use crate::state::{ApiJson, ApiJsonRejection, ApiQuery, AppState};

/// The deny lane's runtime, whose blocking pool runs every `/control/changes` body. The deny lane
/// never shares tokio's blocking pool with ingest: an ingest closure holds its thread until its
/// receipt, so a suppression on a shared pool could queue behind every in-flight ingest. Built at
/// startup by [`init_deny_runtime`] and never dropped; a runtime that loses the `set` race goes
/// to [`discard_losing_runtime`], since dropping one on a reactor thread panics.
static DENY_RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();

/// Disposes of a runtime that lost the `OnceLock::set` race, possibly from an async context.
/// `shutdown_background` returns without blocking, so it is legal on a reactor thread, and a
/// losing runtime has no spawned work for it to abandon.
fn discard_losing_runtime(rt: tokio::runtime::Runtime) {
    rt.shutdown_background();
}

/// Builds the deny lane's runtime once; `crate::prepare` calls it so a failure is a startup
/// failure. Later calls are no-ops. Concurrent callers may both build one: the `set` decides, and
/// the loser is discarded.
pub fn init_deny_runtime() -> std::io::Result<()> {
    if DENY_RUNTIME.get().is_some() {
        return Ok(());
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        // All work here is `spawn_blocking`, which runs on the blocking pool; the one worker
        // only has to exist.
        .worker_threads(1)
        .thread_name("tessera-deny")
        .max_blocking_threads(DENY_MAX_BLOCKING_THREADS)
        .build()?;
    if let Err(loser) = DENY_RUNTIME.set(rt) {
        discard_losing_runtime(loser);
    }
    Ok(())
}

/// Runs one `/control/changes` body on the deny lane; the only way a handler reaches it.
fn spawn_on_deny_lane<F, T>(f: F) -> tokio::task::JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    // Embedders and tests that skip `prepare` build the runtime here, on first use.
    match DENY_RUNTIME.get() {
        Some(rt) => rt.spawn_blocking(f),
        None => {
            if init_deny_runtime().is_ok() {
                if let Some(rt) = DENY_RUNTIME.get() {
                    return rt.spawn_blocking(f);
                }
            }
            // Running on the shared pool is better than refusing a suppression.
            tracing::error!(
                "ALARM: the deny lane's runtime is unavailable; falling back to the shared \
                 blocking pool, where a suppression can queue behind unbounded ingest work"
            );
            tokio::task::spawn_blocking(f)
        }
    }
}

/// The deny runtime's blocking-pool bound, tokio's default stated so it cannot move silently.
/// A thread is held for a whole changes request, so this bounds concurrent requests.
const DENY_MAX_BLOCKING_THREADS: usize = 512;

pub fn router(state: Arc<AppState>) -> Router {
    // A body's byte cap is enforced here, as the body is read, so no more than the cap is ever
    // buffered; the handler maps axum's 413 to a 422 naming the limit. The row cap can only be
    // checked after decoding.
    let ingest_route = post(ingest).layer(axum::extract::DefaultBodyLimit::max(
        state.limits.ingest_max_batch_bytes,
    ));
    let changes_route =
        post(changes).layer(axum::extract::DefaultBodyLimit::max(CHANGES_MAX_BODY_BYTES));
    let router = Router::new()
        .route("/control/ingest", ingest_route)
        .route("/control/changes", changes_route)
        .route("/control/status", get(status))
        .route("/control/flush", post(flush))
        .route("/control/compact", post(compact))
        // Declarations are `PUT` because the name is the identity: an identical redeclaration
        // answers what exists and a differing one is refused. They are small and keep axum's
        // 2 MiB default body limit.
        .route("/control/layers", axum::routing::put(register_layer))
        .route("/control/layers/{name}", axum::routing::delete(drop_layer))
        .route("/control/attributes", axum::routing::put(declare_attribute))
        .route(
            "/control/vocabularies/{name}",
            axum::routing::put(declare_vocabulary),
        )
        .route(
            "/control/vocabularies/{name}/values",
            axum::routing::patch(mint_vocabulary_values),
        )
        .route(
            "/control/views/{group}/{key}",
            axum::routing::put(create_view).delete(drop_view),
        )
        // A plain view's path has one segment and a group's view two, so plain view names and
        // group names share one namespace.
        .route(
            "/control/view_groups/{name}",
            axum::routing::put(create_view_group),
        )
        .route(
            "/control/views/{name}",
            axum::routing::put(create_plain_view),
        )
        .route(
            "/control/layers/{name}/artifacts",
            // `PUT` publishes with a first page of each membership and `PATCH` sends the rest; one
            // route entry, so both verbs share `publish_max_body_bytes`.
            axum::routing::put(publish_artifacts)
                .patch(grow_memberships)
                .layer(axum::extract::DefaultBodyLimit::max(
                    state.limits.publish_max_body_bytes,
                )),
        );
    // Fault arming exists only in a fault-injection build, behind the credential like every route.
    #[cfg(feature = "fault-injection")]
    let router = router
        .route("/control/faults/arm", post(faults_arm))
        .route("/control/faults/arrivals", get(faults_arrivals))
        .route("/control/faults/release", post(faults_release));
    router
        // `Router::layer`, not `route_layer`, so every route, and every unrouted path, is behind
        // the credential.
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&state),
            require_operator_credential,
        ))
        // Outside the credential layer, so a refused request still runs the allocator trim check.
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&state),
            crate::memory::trim_after_response,
        ))
        .with_state(state)
}

/// The operator-credential check for every path on the control listener, routed or not, with no
/// exemption. It runs as a layer before the body is read, so an unauthenticated caller buffers
/// nothing and meets 401 ahead of any 404, 422 or 429; no handler checks the credential itself.
async fn require_operator_credential(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, ApiError> {
    state.check_bearer(crate::state::bearer_token(request.headers()), &state.operator_credential)?;
    Ok(next.run(request).await)
}

/// The frame, projection and quantisation extent, of each of the layer's own views; a layer's
/// views need not share one, and a shape goes through each view's own transform. The layer's
/// views, not the bundle's, because a layer need not be drawn on every view.
fn layer_frames(
    meta: &tessera_engine::EngineMeta,
    views: &[&str],
) -> Result<Vec<tessera_engine::shapes::ViewFrame>, ApiError> {
    if views.is_empty() {
        return Err(ApiError::Contract(
            "this layer declares no view to publish a shape into".into(),
        ));
    }
    views
        .iter()
        .map(|name| {
            let view = meta
                .views
                .iter()
                .find(|v| v.id == *name)
                .ok_or_else(|| ApiError::Unknown(format!("unknown view '{name}'")))?;
            Ok(tessera_engine::shapes::ViewFrame::new(
                &view.id,
                view.projection,
                crate::filter_dto::view_extent(view),
            ))
        })
        .collect()
}

/// `/control/changes`'s body limit, stated so the 422 can name it. At about 200 bytes an item it
/// admits roughly ten thousand changes; a caller with more splits them across requests.
const CHANGES_MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

/// `/control/changes`'s item count per request. Over it is a 422 naming the limit; a caller
/// pages its denies, and none is refused.
const CHANGES_MAX_ITEMS: usize = 10_000;

/// A declaration's body cap: axum's `Json` default, which declaration routes inherit, named so
/// `/control/status` can publish it.
const DECLARATION_MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

/// What an over-cap `/control/ingest` caller does next.
const INGEST_BODY_REMEDY: &str = " (ingest.ingest_max_batch_bytes); send fewer rows per batch";

/// The content type that selects Arrow IPC on a record-bearing route.
const ARROW_CONTENT_TYPE: &str = "application/vnd.apache.arrow.stream";

/// Which encoding a request's `content-type` names. Absent is JSON and parameters are ignored;
/// any other type is refused rather than sniffed, so a mislabelled body meets a refusal and not
/// the other decoder's error.
fn body_encoding(headers: &HeaderMap) -> Result<BodyEncoding, ApiError> {
    let Some(value) = headers.get(axum::http::header::CONTENT_TYPE) else {
        return Ok(BodyEncoding::Json);
    };
    let raw = value.to_str().map_err(|_| {
        ApiError::Contract("content-type is not valid UTF-8, so it names no encoding".to_string())
    })?;
    let essence = raw
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match essence.as_str() {
        "" | "application/json" | "application/x-ndjson" => Ok(BodyEncoding::Json),
        ARROW_CONTENT_TYPE => Ok(BodyEncoding::Arrow),
        other => Err(ApiError::Contract(format!(
            "content-type '{other}' names no encoding this route takes; send \
             `application/json`, `application/x-ndjson` or `{ARROW_CONTENT_TYPE}`"
        ))),
    }
}

/// A body a write route could not take, as a 422 rather than axum's 413: over `cap` bytes, with
/// `remedy` saying what to send instead, or not read to completion.
fn body_refusal(status: StatusCode, body: &str, cap: usize, remedy: &str) -> ApiError {
    if status == StatusCode::PAYLOAD_TOO_LARGE {
        ApiError::Contract(format!(
            "the {body} body is over the {cap}-byte per-request limit{remedy}"
        ))
    } else {
        ApiError::Contract(format!(
            "the {body} body could not be read: the connection failed mid-upload, the transfer \
             encoding is malformed, or it is not the body this route takes"
        ))
    }
}

/// The `x-tessera-batch-id` header; a value that is not UTF-8 is refused as a missing one is.
fn batch_id_header(headers: &HeaderMap) -> Result<String, ApiError> {
    headers
        .get("x-tessera-batch-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .ok_or_else(|| ApiError::Contract("missing x-tessera-batch-id header".to_string()))
}

/// The `x-tessera-view` header, if given; a value that is not UTF-8 is refused.
fn view_header(headers: &HeaderMap) -> Result<Option<String>, ApiError> {
    headers
        .get("x-tessera-view")
        .map(|value| {
            value.to_str().map(str::to_string).map_err(|_| {
                ApiError::Contract(
                    "x-tessera-view is not valid UTF-8, so it names no view".to_string(),
                )
            })
        })
        .transpose()
}

/// The view a write batch names in `x-tessera-view`. With no header the deployment must have
/// exactly one view (422 otherwise, naming them); an unknown view is 404, not 422, as on the
/// viewer plane. Resolved by [`tessera_engine::EngineMeta::resolve_view`], the one resolution
/// both planes use.
fn resolve_view<'a>(
    view: Option<&str>,
    meta: &'a tessera_engine::EngineMeta,
) -> Result<&'a MetaView, ApiError> {
    let views = &meta.views;
    match view {
        None if views.len() > 1 => Err(ApiError::Contract(format!(
            "this bundle has {} views, so name one of them in x-tessera-view: {}",
            views.len(),
            views
                .iter()
                .map(|v| v.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
        None => views.first().ok_or_else(|| {
            ApiError::Contract("this bundle declares no view to ingest into".into())
        }),
        Some(id) => meta
            .resolve_view(id)
            .ok_or_else(|| ApiError::Unknown(format!("unknown view '{id}'"))),
    }
}

#[derive(serde::Serialize)]
struct IngestResp {
    /// Rows the batch carried.
    rows: u64,
    /// Rows that named no item and created one.
    created: u64,
    /// Rows that changed an item they named.
    edited: u64,
    /// Rows that added an item they named to the batch's view.
    added: u64,
    /// Rows that named an item and changed nothing, which cost no write.
    unchanged: u64,
    over_bound: u64,
    /// The positions in the batch of the first 100 over-bound rows.
    over_bound_rows: Vec<u64>,
    /// Rows whose coordinates fell outside the view's projection domain and were stored on the
    /// frame's edge. Always 0 under `projection = "none"`.
    clipped: u64,
    /// Rows whose coordinates fell outside the view's extent and were stored on its edge.
    clamped: u64,
    /// One `tessera_id` per row, in request order: the item the row created or named. Decimal
    /// strings, since a JSON number loses `u64` precision past 2^53 in JavaScript.
    tessera_ids: Vec<String>,
    /// Artifacts this batch's membership columns created, for keys no artifact held on an open
    /// layer. Reported because a minted artifact cannot be undone.
    minted: u64,
    /// Memberships this batch's membership columns added, to artifacts it created and to held
    /// ones alike.
    joined: u64,
    /// This body was already accepted under this batch id. Every count is then 0, so a client
    /// summing them over retried pages does not double-count; `tessera_ids` is the first
    /// acceptance's.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    replayed: bool,
    /// The cycle this batch's rows become visible in. A replay names the next cycle to open,
    /// later than the one that published its rows; waiting on it is still sound.
    publication: u64,
    /// `wait=visible` only: whether the counter reached `publication` inside
    /// `serve.visible_wait_max_secs`.
    #[serde(skip_serializing_if = "Option::is_none")]
    visible: Option<bool>,
}

/// The blocking half of `POST /control/ingest`, from decode to the fsynced WAL append. It is
/// never behind the viewer planes' compute admission.
fn run_ingest(
    state: &AppState,
    encoding: BodyEncoding,
    body: &[u8],
    batch_id: String,
    view: Option<&str>,
) -> Result<IngestResp, ApiError> {
    let body_hash: [u8; 32] = Sha256::digest(body).into();

    // One manifest snapshot for the decode, so the view, projection and declared columns all come
    // from one generation.
    let meta = state.engine.meta();
    // The view the batch names: the header's, or the deployment's only view. A deployment with
    // several and no header gives the batch no view, and its rows carry no coordinates.
    let view: Option<&MetaView> = match view {
        Some(_) => Some(resolve_view(view, &meta)?),
        None if meta.views.len() == 1 => meta.views.first(),
        None => None,
    };
    let extent = view.map(crate::filter_dto::view_extent);
    let frame = view.zip(extent.as_ref()).map(|(view, extent)| crate::decode::Frame {
        projection: view.projection,
        extent,
    });
    let view_id = view.map(|view| view.id.clone());

    // The group-scoped families this batch may carry: those whose group owns this view's key,
    // including a sharing group's view. A plain view gets none, so a scoped column on it is refused
    // as undeclared. Layers are looked up per column in the engine's registry.
    let scoped: Vec<ScopedScalar> = match &view_id {
        None => Vec::new(),
        Some(view) => meta
            .scoped_scalars
            .iter()
            .filter(|f| meta.owning_key(view, &f.group).is_some())
            .cloned()
            .collect(),
    };
    let ParsedBatch {
        items,
        artifacts,
        clipped,
        clamped,
    } = parse_ingest_batch(
        encoding,
        body,
        frame,
        &meta.declared_scalars,
        &scoped,
        &meta.vocabularies,
        &|name| state.engine.registered_layer(name).map(|l| l.declaration),
        &|group| {
            view_id
                .as_deref()
                .and_then(|view| meta.owning_key(view, group))
                .map(str::to_string)
        },
    )
    .map_err(|DecodeError(detail)| ApiError::Contract(detail))?;

    // The row cap can only be checked after the whole body is decoded, and runs before any label
    // is resolved, so an over-cap batch interns no term.
    if items.len() > state.limits.ingest_max_batch_rows {
        return Err(ApiError::Contract(format!(
            "ingest batch has {} rows, over the {}-row limit (ingest.ingest_max_batch_rows); \
             send at most that many per batch",
            items.len(),
            state.limits.ingest_max_batch_rows
        )));
    }

    // The buffer bound: the command queue drains in milliseconds, so between flushes the buffer
    // is what grows. Checked before submission, so a 429 costs no entity id or WAL append; the
    // figure may lag by one apply. `Retry-After` is the next tick plus the observed flush cost.
    let buffered = state.engine.buffered_items();
    if buffered >= state.limits.ingest_buffer_max_items {
        return Err(ApiError::Backpressure {
            retry_after_s: tessera_engine::estimate_buffer_retry_after_s(
                &state.engine.write_executor_stats(),
                buffered as u64,
            ),
            cause: crate::error::ShedCause::WriteQueue,
        });
    }

    // An empty label is the view's default, and without one the batch is refused before anything
    // is interned. A label left out is the engine's to decide: it keeps a named item's label.
    let unlabelled = items
        .iter()
        .filter(|item| item.labels.as_ref().is_some_and(Vec::is_empty))
        .count();
    let default = view.and_then(|view| view.point_default.as_ref());
    if unlabelled > 0 && default.is_none() {
        let named = view.map_or("the batch names no view, so none".to_string(), |view| {
            format!("view '{}'", view.id)
        });
        return Err(ApiError::Contract(format!(
            "{unlabelled} row(s) carry an empty access label and {named} declares no \
             `point_visibility.default`; label the rows, or declare the default"
        )));
    }
    let mut items = items;
    for item in &mut items {
        if let (Some(labels), Some(default)) = (&mut item.labels, default) {
            if labels.is_empty() {
                labels.push(default.as_bytes().to_vec());
            }
        }
    }

    let rows = items.len() as u64;
    let rows_in: Vec<tessera_engine::IngestRow> = items
        .into_iter()
        .map(|item| tessera_engine::IngestRow {
            tessera_id: item.tessera_id,
            labels: item.labels,
            position: item.position,
            scalars: item.scalars,
            scoped: item.scoped,
            omitted: item.omitted,
        })
        .collect();
    let receipt = state
        .engine
        .ingest(tessera_engine::IngestRequest {
            batch_id,
            body_hash,
            view: view_id,
            rows: rows_in,
            artifacts,
        })
        .map_err(|e| {
            tracing::debug!(detail = %e, "an ingest batch was refused");
            map_accept_error(e)
        })?;

    // An over-bound row is counted and kept, never refused.
    let over_bound_rows: Vec<u64> = receipt
        .over_bound
        .iter()
        .take(100)
        .map(|&at| at as u64)
        .collect();
    Ok(IngestResp {
        rows,
        created: receipt.created,
        edited: receipt.edited,
        added: receipt.added,
        unchanged: receipt.unchanged,
        over_bound: receipt.over_bound.len() as u64,
        over_bound_rows,
        clipped,
        clamped,
        tessera_ids: receipt
            .tessera_ids
            .iter()
            .map(|id| id.raw().to_string())
            .collect(),
        minted: receipt.minted,
        joined: receipt.joined,
        replayed: receipt.replayed,
        // Filled by the handler, which is where the wait can be awaited.
        publication: 0,
        visible: None,
    })
}

/// `POST /control/ingest`. Refusals come in this order, so an unauthenticated caller learns
/// nothing about pressure or limits: 401, 422 (body cap, batch id), 429 (admission), 422 (row
/// cap), 429 (queue). Connections are not bounded in-process; the deployment bounds them.
async fn ingest(
    State(state): State<Arc<AppState>>,
    ApiQuery(wait): ApiQuery<WaitQuery>,
    headers: HeaderMap,
    body: Result<Bytes, axum::extract::rejection::BytesRejection>,
) -> Result<Json<IngestResp>, ApiError> {
    let body = body.map_err(|rejection| {
        body_refusal(
            rejection.status(),
            "ingest",
            state.limits.ingest_max_batch_bytes,
            INGEST_BODY_REMEDY,
        )
    })?;
    let batch_id = batch_id_header(&headers)?;
    let encoding = body_encoding(&headers)?;
    let view = view_header(&headers)?;

    // Admission is checked before `spawn_blocking`, since a blocking thread is what it bounds.
    let Some(permit) = state.ingest_admission.try_admit() else {
        // `debug!` because this runs on the reactor for every shed request; the operator signal
        // is `ingest.shed_total` on `/control/status`.
        tracing::debug!("the ingest admission bound is saturated; answering 429 backpressure");
        return Err(ApiError::Backpressure {
            retry_after_s: crate::error::admission_retry_after_s(
                &state.engine.write_executor_stats(),
            ),
            cause: crate::error::ShedCause::IngestAdmission,
        });
    };

    // The permit moves into the closure: if the client disconnects, the closure still holds its
    // thread, and the permit must be held as long.
    let mut resp = state
        .blocking(move |state| {
            let _permit = permit;
            run_ingest(state, encoding, &body, batch_id, view.as_deref())
        })
        .await?;

    // Read after the rows are buffered, so the number names a cycle that carries them.
    let ack = publication_ack(&state, &wait).await?;
    resp.publication = ack.publication;
    resp.visible = ack.visible;
    Ok(Json(resp))
}

/// One `/control/changes` item as sent.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ChangeItem {
    /// A decimal string, since a JSON number loses `u64` precision past 2^53 in JavaScript.
    #[serde(default)]
    tessera_id: Option<String>,
    /// A unique field, with `value`: the other way to name an item.
    #[serde(default)]
    field: Option<String>,
    /// One value of `field`, as its text: a keyword itself, an integer or a timestamp in decimal
    /// digits.
    #[serde(default)]
    value: Option<String>,
    op: String,
}

/// A change item whose shape is valid and whose address is not yet resolved; every item's
/// address is resolved in one batched call.
struct DecodedChange {
    address: Address,
    op: ChangeOp,
}

/// The blocking body of `/control/changes`, run on the deny lane. Every item is validated and
/// resolved before any is enqueued, so an invalid request applies nothing; a WAL failure during
/// the apply cannot be rolled back.
fn run_changes(state: &AppState, items: Vec<ChangeItem>) -> Result<(), ApiError> {
    // Shape is checked over the whole request before any address is resolved, so a malformed
    // item's 422 is answered ahead of another item's 404.
    let mut decoded: Vec<DecodedChange> = Vec::with_capacity(items.len());
    for item in &items {
        let op = match item.op.as_str() {
            // Kept so the refusal names the edit flow instead of answering "unknown op".
            "predicate" => {
                return Err(ApiError::Contract(
                    "there is no predicate op; to change an item's access labels, send an ingest \
                     row naming the item with the new labels"
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

        let address = match (&item.tessera_id, &item.field, &item.value) {
            (Some(tessera_id), None, None) => {
                let id: u64 = tessera_id.parse().map_err(|_| {
                    ApiError::Contract(
                        "tessera_id must be a base-10 string, such as \"12345\", not a JSON \
                         number"
                            .to_string(),
                    )
                })?;
                Address::Tessera(TesseraId::new(id))
            }
            (None, Some(field), Some(value)) => Address::Unique {
                field: field.clone(),
                value: value.clone(),
            },
            _ => {
                return Err(ApiError::Contract(
                    "a change names its item by a tessera_id, or by a unique field and a value \
                     of it; send `tessera_id`, or `field` and `value`"
                        .to_string(),
                ))
            }
        };

        decoded.push(DecodedChange { address, op });
    }

    // An item an edit moved while the request waited, and whose old entity a fold then retired,
    // is found by resolving the request's names again.
    let ops: Vec<ChangeOp> = decoded.iter().map(|d| d.op).collect();
    let mut attempts = 0;
    loop {
        attempts += 1;
        let changes = resolve_changes(state, &decoded)?;
        match state.engine.accept_changes(changes) {
            Err(tessera_engine::AcceptError::Exec(tessera_lifecycle::ExecError::Stale))
                if attempts < 3 =>
            {
                continue
            }
            answered => return answered.map_err(|e| map_change_batch_error(&ops, e)),
        }
    }
}

/// Every change's entity, in order. Resolved before anything is enqueued: an unissued
/// `tessera_id` is answered ahead of a value naming nothing, wherever each sits in the request.
fn resolve_changes(
    state: &AppState,
    decoded: &[DecodedChange],
) -> Result<Vec<(EntityId, ChangeOp)>, ApiError> {
    let resolved = resolve_addresses(state, decoded.iter().map(|d| &d.address))?;
    for tessera_first in [true, false] {
        let unknown = decoded.iter().zip(&resolved).position(|(d, entity)| {
            matches!(d.address, Address::Tessera(_)) == tessera_first && entity.is_none()
        });
        if let Some(index) = unknown {
            let what = match tessera_first {
                true => "tessera_id",
                false => "value",
            };
            return Err(ApiError::Unknown(format!(
                "the {what} of item {index} names nothing this deployment holds"
            )));
        }
    }
    Ok(decoded
        .iter()
        .zip(resolved)
        .map(|(d, entity)| (entity.expect("every address resolved above"), d.op))
        .collect())
}

/// Resolves each address to its entity, in order, `None` where it names nothing: one batched call
/// for the `tessera_id`s and one for each unique field's values.
fn resolve_addresses<'a>(
    state: &AppState,
    addresses: impl IntoIterator<Item = &'a Address>,
) -> Result<Vec<Option<EntityId>>, ApiError> {
    let addresses: Vec<&Address> = addresses.into_iter().collect();
    let mut resolved: Vec<Option<EntityId>> = vec![None; addresses.len()];
    let tessera: Vec<(usize, TesseraId)> = addresses
        .iter()
        .enumerate()
        .filter_map(|(at, address)| match address {
            Address::Tessera(id) => Some((at, *id)),
            Address::Unique { .. } => None,
        })
        .collect();
    let ids: Vec<TesseraId> = tessera.iter().map(|(_, id)| *id).collect();
    let found = state
        .engine
        .resolve_tessera_ids(&ids)
        .map_err(map_store_error)?;
    for ((at, _), entity) in tessera.iter().zip(found) {
        resolved[*at] = entity;
    }
    let mut by_field: std::collections::BTreeMap<&str, (Vec<usize>, Vec<String>)> =
        std::collections::BTreeMap::new();
    for (at, address) in addresses.iter().enumerate() {
        if let Address::Unique { field, value } = address {
            let (positions, values) = by_field.entry(field.as_str()).or_default();
            positions.push(at);
            values.push(value.clone());
        }
    }
    for (field, (positions, values)) in by_field {
        let found = state
            .engine
            .resolve_unique_values(field, &values)
            .map_err(map_engine_error)?;
        for (at, entity) in positions.into_iter().zip(found) {
            resolved[at] = entity;
        }
    }
    Ok(resolved)
}

/// `POST /control/changes`: deletions, suppressions and unsuppressions. Never answers 429, and has
/// no readiness gate. It runs on the deny lane, which never shares the blocking pool with ingest;
/// a deletion or suppression is applied from the moment it is accepted, even when its WAL append
/// fails, because an accepted deny left unapplied would fail open. The 200 follows the fsync.
async fn changes(
    State(state): State<Arc<AppState>>,
    ApiQuery(wait): ApiQuery<WaitQuery>,
    body: Result<ApiJson<Vec<ChangeItem>>, ApiJsonRejection>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let ApiJson(items) = body.map_err(|rejection| match rejection {
        ApiJsonRejection::Shape(e) => e,
        ApiJsonRejection::Body(rejection) => body_refusal(
            rejection.status(),
            "change request",
            CHANGES_MAX_BODY_BYTES,
            "; split it into smaller requests",
        ),
    })?;

    // The item cap, checked before anything is resolved.
    if items.len() > CHANGES_MAX_ITEMS {
        return Err(ApiError::Contract(format!(
            "the change request carries {} items, over the {CHANGES_MAX_ITEMS}-item limit \
             (limits.changes.max_changes_per_request); split it into smaller requests",
            items.len()
        )));
    }

    // No readiness gate: a node with a poisoned WAL still applies denies to the overlay, and
    // gating on readiness would refuse them unapplied.
    let engine = Arc::clone(&state);
    spawn_on_deny_lane(move || run_changes(&engine, items))
        .await
        .map_err(map_join_error)??;

    // The deny is in force and durable when this is sent, without waiting for a publication cycle.
    // `publication` is carried only for uniformity; `wait=visible` here waits for the next cycle.
    acknowledge(&state, &wait, StatusCode::OK, serde_json::json!({})).await
}

/// How often a `wait=visible` wait re-reads the publication counter; this is the delay between a
/// publication and the answer.
const VISIBLE_WAIT_POLL: std::time::Duration = std::time::Duration::from_millis(5);

/// The `wait` query parameter every write route takes. Its one value is `visible`; any other value,
/// or any other parameter, is a 422, so `wait=true` or `wiat=visible` is not mistaken for a wait.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaitQuery {
    #[serde(default)]
    wait: Option<String>,
}

impl WaitQuery {
    fn asked(&self) -> Result<bool, ApiError> {
        match self.wait.as_deref() {
            None => Ok(false),
            Some("visible") => Ok(true),
            Some(other) => Err(ApiError::Contract(format!(
                "wait takes 'visible' and nothing else; got '{other}'"
            ))),
        }
    }
}

/// What a write acknowledgement says about when its work becomes visible.
struct PublicationAck {
    /// The cycle this write's work is published in.
    publication: u64,
    /// Present only where `wait=visible` was asked: whether the counter reached `publication`
    /// within `serve.visible_wait_max_secs`.
    visible: Option<bool>,
}

impl PublicationAck {
    /// Write the two fields into an object body.
    fn merge(&self, body: &mut serde_json::Value) {
        body["publication"] = serde_json::json!(self.publication);
        if let Some(visible) = self.visible {
            body["visible"] = serde_json::json!(visible);
        }
    }
}

/// A write route's answer: `status`, and `body` carrying the publication ack.
async fn acknowledge(
    state: &AppState,
    wait: &WaitQuery,
    status: StatusCode,
    mut body: serde_json::Value,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    publication_ack(state, wait).await?.merge(&mut body);
    Ok((status, Json(body)))
}

/// A declaration's status: `200` where the name already carried this identity, `201` where the
/// declaration created it.
fn declared(existing: bool) -> StatusCode {
    if existing {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    }
}

/// The publication number a write acknowledgement carries, read after the write is buffered.
/// With `wait=visible` it pulls the tick forward as `/control/flush` does and holds the answer
/// until the counter reaches it, or answers `visible: false` at `serve.visible_wait_max_secs`.
async fn publication_ack(state: &AppState, wait: &WaitQuery) -> Result<PublicationAck, ApiError> {
    if !wait.asked()? {
        return Ok(PublicationAck {
            publication: state.engine.publication_target(),
            visible: None,
        });
    }
    Ok(await_publication(state, state.engine.request_flush_publication()).await)
}

/// Holds until the counter reaches `publication` or `serve.visible_wait_max_secs` passes; a
/// ceiling too large to add to the clock waits without one.
async fn await_publication(state: &AppState, publication: u64) -> PublicationAck {
    let deadline = std::time::Instant::now()
        .checked_add(std::time::Duration::from_secs(state.limits.visible_wait_max_secs));
    loop {
        if state.engine.publication() >= publication {
            return PublicationAck {
                publication,
                visible: Some(true),
            };
        }
        if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
            return PublicationAck {
                publication,
                visible: Some(false),
            };
        }
        tokio::time::sleep(VISIBLE_WAIT_POLL).await;
    }
}

/// `POST /control/flush`: pulls the next tick forward and answers 202 without waiting for it. Two
/// requests before one tick share it. `publication` names the first cycle certain to include work
/// buffered before the request; `?wait=visible` holds the 202 until it is reached.
async fn flush(
    State(state): State<Arc<AppState>>,
    ApiQuery(wait): ApiQuery<WaitQuery>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    // Read before the flush is armed, so an unknown value arms nothing.
    let asked = wait.asked()?;
    let publication = state.engine.request_flush_publication();
    let ack = if asked {
        await_publication(&state, publication).await
    } else {
        PublicationAck {
            publication,
            visible: None,
        }
    };
    let mut body = serde_json::json!({});
    ack.merge(&mut body);
    Ok((StatusCode::ACCEPTED, Json(body)))
}

/// `POST /control/compact`: sets a flag the executor reads at its next tick, and answers 202. A
/// request while a fold runs is dropped with a log warning, and the 202 cannot say so;
/// `/control/status`'s `compaction` block shows what happened.
async fn compact(State(state): State<Arc<AppState>>) -> StatusCode {
    state.engine.request_fold();
    StatusCode::ACCEPTED
}

/// `PUT /control/layers`: registers one annotation layer. Synchronous: the answer follows the
/// fsynced append and carries the layer's `tessera_id`, its only address for a later suppression.
/// A failure means the layer does not exist, unlike a suppression, which is applied even when its
/// append fails. A declaration that breaks the deployment's rules is a 422 saying why.
async fn register_layer(
    State(state): State<Arc<AppState>>,
    ApiQuery(wait): ApiQuery<WaitQuery>,
    body: ApiJson<tessera_types::layer::LayerDeclaration>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let mut declaration = body.0;
    let name = declaration.name.clone();
    // A group's name is every view the group holds now, as it is at a build.
    let meta = state.engine.meta();
    let declared_views = std::mem::take(&mut declaration.views);
    declaration.views = tessera_types::layer::expand_views(
        &declared_views,
        |view| {
            meta.groups
                .iter()
                .find(|g| g.name == view)
                .map(|g| g.views.clone())
        },
        |view| meta.resolve_view(view).is_some(),
    )
    .map_err(|view| {
        ApiError::Contract(format!(
            "layer '{name}' declares view '{view}', which is neither a view nor a view group of \
             this deployment; name one of: {}",
            meta.views
                .iter()
                .map(|v| v.id.as_str())
                .chain(meta.groups.iter().map(|g| g.name.as_str()))
                .collect::<Vec<_>>()
                .join(", ")
        ))
    })?;
    if let Some(group) = declaration.scope.group() {
        tessera_types::layer::check_scoped_views(
            &declared_views,
            group,
            meta.groups
                .iter()
                .map(|g| (g.name.as_str(), g.members_of.as_deref())),
            |view| {
                meta.resolve_view(view)
                    .and_then(|v| v.roster.as_ref())
                    .map(|roster| roster.group.clone())
            },
        )
        .map_err(|outside| {
            ApiError::Contract(format!(
                "layer '{name}' is scoped to group '{group}' and names view '{}', which holds no \
                 key of it; name {} or a view of {}, or drop the scope",
                outside.view,
                outside.sharing.join(" or "),
                if outside.sharing.len() == 1 { "it" } else { "them" }
            ))
        })?;
    }
    // A shape layer over both projected and unprojected views is refused at the declaration
    // rather than at its first artifact.
    if declaration.membership == tessera_types::layer::MembershipSource::Spatial {
        let views: Vec<&str> = declaration.views.iter().map(String::as_str).collect();
        let frames = layer_frames(&meta, &views)?;
        tessera_engine::shapes::check_shape_span(
            &frames,
            tessera_engine::shapes::ShapeSpace::Wgs84,
        )
        .map_err(|e| ApiError::Contract(format!("layer '{name}': {e}")))?;
    }
    // The shared blocking pool, not the deny lane: a declaration is not a deny.
    let id = state
        .write(move |state| state.engine.register_layer(declaration))
        .await?;
    let body = serde_json::json!({ "name": name, "tessera_id": id.raw().to_string() });
    acknowledge(&state, &wait, StatusCode::CREATED, body).await
}

/// `DELETE /control/layers/{name}`: drops a layer and tombstones its name for ever, since
/// bookmarks, edges and suppressions refer to a layer by name. Its entity ids are not reused.
async fn drop_layer(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(name): axum::extract::Path<String>,
    ApiQuery(wait): ApiQuery<WaitQuery>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    state
        .write(move |state| state.engine.drop_layer(name))
        .await?;
    // 200 with a body rather than 204, so it carries the publication number like every write.
    acknowledge(&state, &wait, StatusCode::OK, serde_json::json!({})).await
}

/// `PUT /control/attributes`' body: the `[[attribute]]` block without its acquisition keys.
/// Unknown fields are refused, because a column cannot be redeclared and a misspelt flag would
/// silently take its default.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AttributeBody {
    name: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(rename = "type")]
    ty: String,
    #[serde(default)]
    vocabulary: Option<String>,
    #[serde(default)]
    analyser: Option<String>,
    #[serde(default)]
    index: bool,
    #[serde(default)]
    render: bool,
    /// `"entity"`, or `{"group": "<view_group>"}`, spelled as the block spells it.
    #[serde(default)]
    scope: tessera_types::layer::LayerScope,
    /// No two items may hold one value. On a column that exists, the one change accepted: the
    /// answer waits while the column's index is built, and a column already holding a value twice
    /// is refused, naming the values.
    #[serde(default)]
    unique: bool,
}

/// `PUT /control/attributes`: declares one attribute column, synchronously, so a batch sent after
/// the answer may carry it. 201 when new, 200 when the name already has this identity or differed
/// from it in `unique` alone, 409 when it has another or holds a value twice, 422 for a rule
/// broken. The engine refuses `render = true` on this route.
async fn declare_attribute(
    State(state): State<Arc<AppState>>,
    ApiQuery(wait): ApiQuery<WaitQuery>,
    body: ApiJson<AttributeBody>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let body = body.0;
    let name = body.name.clone();
    let request = tessera_engine::AttributeRequest {
        name: body.name,
        title: body.title,
        ty: body.ty,
        vocabulary: body.vocabulary,
        analyser: body.analyser,
        index: body.index,
        render: body.render,
        scope: body.scope,
        unique: body.unique,
    };
    let existing = state
        .write(move |state| state.engine.declare_attribute(request))
        .await?;
    let body = serde_json::json!({ "name": name, "existing": existing });
    acknowledge(&state, &wait, declared(existing), body).await
}

/// `PUT /control/vocabularies/{name}`' body: the `[[vocabulary]]` block without its acquisition
/// keys. Unknown fields are refused, so a body naming a `code` is refused: the server assigns
/// every code.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct VocabularyBody {
    #[serde(default)]
    title: Option<String>,
    /// `closed` refuses an unknown key at ingest; `open` mints it.
    value_set: ValueSet,
    /// `public` or `derived`: whether the existence of a value is sensitive. Takes no access label.
    visibility: tessera_types::vocabulary::Visibility,
    /// The code space's width: `u8`, `u16` or `u32`.
    width: String,
    #[serde(default)]
    values: Vec<VocabularyValueBody>,
    /// Retired codes, never assigned.
    #[serde(default)]
    reserved: Vec<u32>,
}

/// A value set as a declaration spells it; the manifest records `declared` and `discovered`.
#[derive(serde::Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum ValueSet {
    Closed,
    Open,
}

impl ValueSet {
    fn kind(self) -> tessera_types::vocabulary::VocabularyKind {
        match self {
            ValueSet::Closed => tessera_types::vocabulary::VocabularyKind::Declared,
            ValueSet::Open => tessera_types::vocabulary::VocabularyKind::Discovered,
        }
    }
}

/// One value on either vocabulary route. It carries no code: the executor draws every code.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct VocabularyValueBody {
    key: String,
    #[serde(default)]
    title: Option<String>,
}

/// `PATCH /control/vocabularies/{name}/values`' body: a page of values.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct VocabularyValuesBody {
    values: Vec<VocabularyValueBody>,
}

/// `PUT /control/vocabularies/{name}`: declares a vocabulary, synchronously. 201 when new; 200
/// when the name already has this identity, and its values are then applied as a page; 409 for
/// another identity, 422 for a rule broken.
async fn declare_vocabulary(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(name): axum::extract::Path<String>,
    ApiQuery(wait): ApiQuery<WaitQuery>,
    body: ApiJson<VocabularyBody>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let body = body.0;
    let request = tessera_engine::VocabularyRequest {
        name: name.clone(),
        title: body.title,
        kind: body.value_set.kind(),
        visibility: body.visibility,
        width: body.width,
        values: body
            .values
            .into_iter()
            .map(|v| tessera_engine::DeclaredValue {
                key: v.key,
                title: v.title,
            })
            .collect(),
        reserved: body.reserved,
    };
    let (existing, added, titles) = state
        .write(move |state| state.engine.declare_vocabulary(request))
        .await?;
    let body = serde_json::json!({
        "name": name,
        "existing": existing,
        "added": added,
        "titles": titles
    });
    acknowledge(&state, &wait, declared(existing), body).await
}

/// `PATCH /control/vocabularies/{name}/values`: adds a page of values and never removes one. A
/// title for a held key replaces its title; a key's code never changes (409 if a page would). An
/// unknown vocabulary is 404, since a page carries no width or visibility to create one.
async fn mint_vocabulary_values(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(name): axum::extract::Path<String>,
    ApiQuery(wait): ApiQuery<WaitQuery>,
    body: ApiJson<VocabularyValuesBody>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let values: Vec<tessera_engine::DeclaredValue> = body
        .0
        .values
        .into_iter()
        .map(|v| tessera_engine::DeclaredValue {
            key: v.key,
            title: v.title,
        })
        .collect();
    let vocabulary = name.clone();
    let (added, existing, titles) = state
        .write(move |state| state.engine.mint_vocabulary_values(vocabulary, values))
        .await?;
    let body = serde_json::json!({
        "name": name,
        "added": added,
        "existing": existing,
        "titles": titles
    });
    acknowledge(&state, &wait, StatusCode::OK, body).await
}

/// The frame a view or group declares, in frame coordinates: the bounds a Morton code is a
/// fraction of. There is no `auto` or degree box, which a build resolves to this same box.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtentBody {
    x: [f64; 2],
    y: [f64; 2],
}

impl ExtentBody {
    fn frame(&self) -> tessera_engine::DeclaredFrame {
        tessera_engine::DeclaredFrame {
            x_min: self.x[0],
            x_max: self.x[1],
            y_min: self.y[0],
            y_max: self.y[1],
        }
    }
}

/// `point_visibility` at a running service: only the `default` a point with no label is given,
/// since a batch carries its own labels.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PointVisibilityBody {
    #[serde(default)]
    default: Option<String>,
}

/// One declared metadata field of a group. Groups carry a list of these, not a map, because the
/// roster serves metadata in declaration order.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct MetadataFieldBody {
    name: String,
    #[serde(rename = "type")]
    ty: tessera_types::view::ViewMetadataType,
    #[serde(default)]
    vocabulary: Option<String>,
}

/// `PUT /control/view_groups/{name}`' body: the `[[view_group]]` block without its roster and
/// source. Unknown fields are refused, because a group's frame and gate never change.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ViewGroupBody {
    #[serde(default)]
    title: Option<String>,
    #[serde(default = "projection_none")]
    projection: String,
    extent: ExtentBody,
    /// One label or a list, each element one term. Absent is `public`.
    #[serde(default)]
    visibility: Option<tessera_types::view::DeclaredGate>,
    #[serde(default)]
    point_visibility: Option<PointVisibilityBody>,
    /// Another group's name, whose views this group shares.
    #[serde(default)]
    members: Option<String>,
    #[serde(default)]
    metadata: Vec<MetadataFieldBody>,
}

/// `PUT /control/views/{name}`' body: the `[[view]]` block without its source.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PlainViewBody {
    #[serde(default)]
    title: Option<String>,
    #[serde(default = "projection_none")]
    projection: String,
    extent: ExtentBody,
    #[serde(default)]
    visibility: Option<tessera_types::view::DeclaredGate>,
    #[serde(default)]
    point_visibility: Option<PointVisibilityBody>,
}

/// The default `projection`: coordinates are taken as written, in the frame.
fn projection_none() -> String {
    "none".to_string()
}

/// `PUT /control/view_groups/{name}`: declares a view group, synchronously, with an empty roster.
/// 201 when new, 200 when the name has this identity, 409 for another, 422 for a rule broken, and
/// 404 when `members` names a group this deployment does not carry.
async fn create_view_group(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(name): axum::extract::Path<String>,
    ApiQuery(wait): ApiQuery<WaitQuery>,
    body: ApiJson<ViewGroupBody>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let body = body.0;
    let declaration = tessera_engine::ViewGroupDeclaration {
        name: name.clone(),
        title: body.title,
        projection: body.projection,
        frame: body.extent.frame(),
        visibility: body
            .visibility
            .map(tessera_types::view::DeclaredGate::into_labels),
        point_default: body.point_visibility.and_then(|p| p.default),
        members: body.members,
        metadata: body
            .metadata
            .into_iter()
            .map(|f| tessera_types::view::GroupMetadataField {
                name: f.name,
                ty: f.ty,
                vocabulary: f.vocabulary,
            })
            .collect(),
    };
    let existing = state
        .write(move |state| state.engine.create_view_group(declaration))
        .await?;
    let body = serde_json::json!({ "group": name, "existing": existing });
    acknowledge(&state, &wait, declared(existing), body).await
}

/// `PUT /control/views/{name}`: creates a plain view with an empty row space, which takes rows at
/// its first flush. The statuses are [`create_view_group`]'s.
async fn create_plain_view(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(name): axum::extract::Path<String>,
    ApiQuery(wait): ApiQuery<WaitQuery>,
    body: ApiJson<PlainViewBody>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let body = body.0;
    let declaration = tessera_engine::PlainViewDeclaration {
        name: name.clone(),
        title: body.title,
        projection: body.projection,
        frame: body.extent.frame(),
        visibility: body
            .visibility
            .map(tessera_types::view::DeclaredGate::into_labels),
        point_default: body.point_visibility.and_then(|p| p.default),
    };
    let existing = state
        .write(move |state| state.engine.create_plain_view(declaration))
        .await?;
    let body = serde_json::json!({ "view": name, "existing": existing });
    acknowledge(&state, &wait, declared(existing), body).await
}

/// `PUT /control/views/{group}/{key}`'s body: the roster record. Unknown fields are refused,
/// because the record is immutable and a misspelt name could never be supplied later.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ViewRecord {
    /// This view's own gate; absent takes the group's. One label or a list, each element one
    /// term.
    #[serde(default)]
    visibility: Option<tessera_types::view::DeclaredGate>,
    /// One entry per name the group declared. A `timestamp_us` is microseconds since the epoch,
    /// as a JSON integer.
    #[serde(default)]
    metadata: serde_json::Map<String, serde_json::Value>,
}

/// One supplied metadata value, typed by its JSON shape; the executor checks the type against
/// the group's declaration. Integers and floats are kept apart, not coerced.
fn metadata_value(name: &str, value: &serde_json::Value) -> Result<ViewMetadataValue, ApiError> {
    match value {
        serde_json::Value::Bool(v) => Ok(ViewMetadataValue::Bool(*v)),
        serde_json::Value::String(v) => Ok(ViewMetadataValue::Text(v.clone())),
        serde_json::Value::Number(n) => {
            if let Some(v) = n.as_i64() {
                // An integer may be an `int`, a category code or a `timestamp_us`; the
                // declaration decides which.
                Ok(ViewMetadataValue::Int(v))
            } else if let Some(v) = n.as_f64() {
                Ok(ViewMetadataValue::Float(v))
            } else {
                Err(ApiError::Contract(format!(
                    "metadata '{name}' is a number this build cannot store"
                )))
            }
        }
        serde_json::Value::Null => Err(ApiError::Contract(format!(
            "metadata '{name}' is null; give it a value, since every name a view group declares \
             is required"
        ))),
        _ => Err(ApiError::Contract(format!(
            "metadata '{name}' is an array or an object; give one string, number or boolean, or \
             declare a per-point value as an attribute"
        ))),
    }
}

/// `PUT /control/views/{group}/{key}`: creates a view of a group. The executor checks the group
/// and key, so two racing requests cannot both take one key: 404 for an unknown group, 409 for a
/// live key, 422 for a bad record. A dropped key is reusable and the new view starts empty.
async fn create_view(
    State(state): State<Arc<AppState>>,
    axum::extract::Path((group, key)): axum::extract::Path<(String, String)>,
    ApiQuery(wait): ApiQuery<WaitQuery>,
    body: ApiJson<ViewRecord>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let record = body.0;
    let mut metadata = std::collections::BTreeMap::new();
    for (name, value) in &record.metadata {
        metadata.insert(name.clone(), metadata_value(name, value)?);
    }
    let visibility = record
        .visibility
        .map(tessera_types::view::DeclaredGate::into_labels);
    let (group_name, view_key) = (group.clone(), key.clone());
    state
        .write(move |state| {
            state
                .engine
                .create_view(group_name, view_key, visibility, metadata)
        })
        .await?;
    let body = serde_json::json!({
        "view": format!("{group}:{key}"),
        "group": group,
        "key": key,
    });
    acknowledge(&state, &wait, StatusCode::CREATED, body).await
}

/// `DELETE /control/views/{group}/{key}`: drops a view and frees its key; its data stays on disc,
/// unreachable, until the fold. It deletes the items it leaves in no view, and the body reports
/// how many.
async fn drop_view(
    State(state): State<Arc<AppState>>,
    axum::extract::Path((group, key)): axum::extract::Path<(String, String)>,
    ApiQuery(wait): ApiQuery<WaitQuery>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let dropped = state
        .write(move |state| state.engine.drop_view(group, key))
        .await?;
    let body = serde_json::json!({
        "deleted": dropped.deleted,
    });
    acknowledge(&state, &wait, StatusCode::OK, body).await
}

/// An artifact record's `access` labels as the plugin's descriptors, by the rule the build reads
/// an artifact's labels with. Absent and empty are no label.
fn access_descriptors(
    state: &AppState,
    labels: Option<Vec<String>>,
) -> Result<Vec<Vec<u8>>, ApiError> {
    tessera_plugin::artifact_access(state.engine.plugin().as_ref(), &labels.unwrap_or_default())
        .map_err(ApiError::Contract)
}

/// Turns a flat member offset back into `(artifact index, member index)` for a refusal to name.
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

/// A JSON body on an artifact route, decoded as `T`; a field `T` does not take is refused.
fn artifact_json<T: serde::de::DeserializeOwned>(body: &[u8], noun: &str) -> Result<T, ApiError> {
    serde_json::from_slice(body).map_err(|e| {
        ApiError::Contract(format!(
            "the {noun} body is not the JSON this route takes: {e}"
        ))
    })
}

/// `PATCH /control/layers/{name}/artifacts`'s Arrow form: one row per artifact, `key: utf8` and
/// `members: list<utf8>` or `large_list<utf8>`, with `view` and `access` optional, and
/// `field` and `level` in the schema metadata. Decoded into the JSON form's body, so
/// the handler has one path.
fn grow_body_from_arrow(body: &[u8]) -> Result<GrowBody, ApiError> {
    use arrow::array::{LargeListArray, ListArray, StringArray};
    use arrow::datatypes::DataType;

    let reader = arrow::ipc::reader::StreamReader::try_new(std::io::Cursor::new(body), None)
        .map_err(|e| {
            ApiError::Contract(format!("growth body is not a valid Arrow IPC stream: {e}"))
        })?;
    let metadata = reader.schema().metadata().clone();
    // Any other column or metadata key is refused, as the JSON form refuses unknown fields.
    for field in reader.schema().fields() {
        if !matches!(field.name().as_str(), "key" | "members" | "view" | "access") {
            return Err(ApiError::Contract(format!(
                "growth body: column '{}' is not one this route takes; a growth carries `key`, \
                 `members`, `access` and, on a layer scoped to a group, `view`, and nothing else",
                field.name()
            )));
        }
    }
    for name in metadata.keys() {
        if !matches!(name.as_str(), "field" | "level") {
            return Err(ApiError::Contract(format!(
                "growth body: schema metadata `{name}` is not one this route takes; the envelope \
                 is `field` and `level`"
            )));
        }
    }
    let field = metadata.get("field").cloned();
    let level = match metadata.get("level") {
        None => 0,
        Some(text) => text.parse::<u32>().map_err(|_| {
            ApiError::Contract(format!(
                "growth body: schema metadata `level` is '{text}'; it is a level number"
            ))
        })?,
    };
    let mut artifacts = Vec::new();
    for batch in reader {
        let batch = batch
            .map_err(|e| ApiError::Contract(format!("growth body: arrow decode error: {e}")))?;
        let keys = batch
            .column_by_name("key")
            .and_then(|column| column.as_any().downcast_ref::<StringArray>())
            .ok_or_else(|| {
                ApiError::Contract(
                    "growth body: column 'key' is missing or not utf8; one row per artifact"
                        .to_string(),
                )
            })?;
        // The view each artifact belongs to, on a group-scoped layer. A null cell names none.
        let views = match batch.column_by_name("view") {
            None => None,
            Some(column) => Some(column.as_any().downcast_ref::<StringArray>().ok_or_else(|| {
                ApiError::Contract(
                    "growth body: column 'view' is not utf8; it names each artifact's view key"
                        .to_string(),
                )
            })?),
        };
        let access = labels_col("growth body", &batch, "access")
            .map_err(|DecodeError(detail)| ApiError::Contract(detail))?;
        let members = batch.column_by_name("members").ok_or_else(|| {
            ApiError::Contract(
                "growth body: column 'members' is missing; it is list<utf8> or large_list<utf8>, \
                 one address per element"
                    .to_string(),
            )
        })?;
        // A null cell is refused, as JSON's `"members": null` is; nothing joining is an empty list.
        let null_cell = |row: usize| {
            ApiError::Contract(format!(
                "growth body: row {row}, column 'members' is null; an artifact with nothing \
                 joining carries an empty list"
            ))
        };
        let entries = |row: usize| -> Result<Vec<String>, ApiError> {
            let (values, range) = match members.data_type() {
                DataType::List(_) => {
                    let list = members
                        .as_any()
                        .downcast_ref::<ListArray>()
                        .expect("a List column downcasts to a ListArray");
                    if list.is_null(row) {
                        return Err(null_cell(row));
                    }
                    let offsets = list.value_offsets();
                    (
                        list.values().clone(),
                        offsets[row] as usize..offsets[row + 1] as usize,
                    )
                }
                DataType::LargeList(_) => {
                    let list = members
                        .as_any()
                        .downcast_ref::<LargeListArray>()
                        .expect("a LargeList column downcasts to a LargeListArray");
                    if list.is_null(row) {
                        return Err(null_cell(row));
                    }
                    let offsets = list.value_offsets();
                    (
                        list.values().clone(),
                        offsets[row] as usize..offsets[row + 1] as usize,
                    )
                }
                other => {
                    return Err(ApiError::Contract(format!(
                        "growth body: column 'members' is {other:?}; it is list<utf8> or \
                         large_list<utf8>, one address per element"
                    )))
                }
            };
            let values = values
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| {
                    ApiError::Contract(
                        "growth body: column 'members' is a list whose elements are not utf8"
                            .to_string(),
                    )
                })?;
            range
                .map(|index| {
                    if values.is_null(index) {
                        return Err(ApiError::Contract(format!(
                            "growth body: row {row}, column 'members' has a null element; every \
                             element is one address"
                        )));
                    }
                    Ok(values.value(index).to_string())
                })
                .collect()
        };
        for row in 0..batch.num_rows() {
            if keys.is_null(row) {
                return Err(ApiError::Contract(format!(
                    "growth body: row {row}, column 'key' is null; every row names the artifact \
                     it grows"
                )));
            }
            let labels = access
                .as_ref()
                .map(|access| access.labels(row).map(str::to_string).collect());
            // The Arrow form carries joining members and labels only; content, shapes and ranked
            // pages travel on the JSON form.
            artifacts.push(GrowingArtifactBody {
                key: keys.value(row).to_string(),
                view: views
                    .filter(|views| !views.is_null(row))
                    .map(|views| views.value(row).to_string()),
                rank: None,
                members: entries(row)?,
                leaving: Vec::new(),
                parent: Vec::new(),
                attached_to: None,
                content: Vec::new(),
                bbox: None,
                circle: None,
                ellipse: None,
                wkt: None,
                space: None,
                access: labels,
            });
        }
    }
    Ok(GrowBody {
        level,
        field,
        default_space: None,
        artifacts,
    })
}

/// Resolves every member address of a batch to an entity at the boundary: a `tessera_id`, or a
/// value of `field` where the batch names one. `flat` holds every address and `widths` each
/// artifact's share. An unresolvable member refuses the batch, never dropped.
fn resolve_member_addresses(
    state: &AppState,
    field: Option<&str>,
    flat: &[&String],
    widths: &[usize],
    layout: &str,
) -> Result<Vec<tessera_types::EntityId>, ApiError> {
    let resolved: Vec<Option<tessera_types::EntityId>> = match field {
        None => {
            let ids = flat
                .iter()
                .map(|raw| {
                    raw.parse::<u64>()
                        .map(TesseraId::new)
                        .map_err(|_| ApiError::Contract(format!("'{raw}' is not a tessera_id")))
                })
                .collect::<Result<Vec<_>, _>>()?;
            state
                .engine
                .resolve_tessera_ids(&ids)
                .map_err(map_store_error)?
        }
        Some(field) => {
            let values: Vec<String> = flat.iter().map(|value| (*value).clone()).collect();
            state
                .engine
                .resolve_unique_values(field, &values)
                .map_err(map_engine_error)?
        }
    };

    if let Some(position) = resolved.iter().position(Option::is_none) {
        // Named by its position in the batch, never by the id the caller sent.
        let (artifact, member) = position_in_batch(widths, position);
        return Err(ApiError::Unknown(format!(
            "id {member} of artifact {artifact} names nothing this deployment holds, counting \
             {layout}"
        )));
    }

    Ok(resolved.into_iter().flatten().collect())
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PublishBody {
    /// Defaults to the layer's only level. A layer that declared none has exactly level 0.
    #[serde(default)]
    level: u32,
    /// The unique field whose values name the members, one form per request since a per-member
    /// tag would be most of a large membership's body. Absent, members are `tessera_id`s.
    #[serde(default)]
    field: Option<String>,
    /// The space of a row's shape and authored shape content where the row names none: `"view"`
    /// (the default) or `"wgs84"`, projected by the view's own transform. A view with projection
    /// `none` refuses `wgs84`.
    #[serde(default)]
    default_space: Option<String>,
    artifacts: Vec<IncomingArtifactBody>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct IncomingArtifactBody {
    /// Needed for any artifact another artifact's edge will name, since an edge names its target
    /// by key.
    #[serde(default)]
    key: Option<String>,
    /// The view key this artifact belongs to: required on a group-scoped layer and refused on an
    /// entity-scoped one. Keys are unique per view, and an edge may not cross views.
    #[serde(default)]
    view: Option<String>,
    /// Decimal `tessera_id` strings, or values of the batch's `field` where it names one. Absent
    /// only when `excluding` is given; an empty list is a membership that holds nobody.
    #[serde(default)]
    members: Option<Vec<String>>,
    /// The membership spelled by exclusion, addressed as `members` is and never beside it, at most
    /// `max_excluded_per_request` long. The executor stores the complement against the view.
    #[serde(default)]
    excluding: Option<Vec<String>>,
    /// Ranked contents, most specific first; the engine decides whether the layer takes them.
    #[serde(default)]
    content: Vec<IncomingContentBody>,
    /// The artifact this one is attached to, such as a label on a cluster. It is withheld
    /// wherever its target is.
    #[serde(default)]
    attached_to: Option<AttachmentBody>,
    /// Parent keys in the layer's hierarchy: at most one on a `nested` or `tiered` layer, any
    /// number on a `dag`. A parent must already exist or come earlier in this batch.
    #[serde(default)]
    parent: Vec<String>,
    /// The shape, in the field for the layer's kind: `bbox = [min_x, min_y, max_x, max_y]`,
    /// `circle = [cx, cy, r]`, `ellipse = [cx, cy, a, b, angle]`, or `wkt`. Required on a shape
    /// layer, refused on any other.
    #[serde(default)]
    bbox: Option<Vec<f64>>,
    #[serde(default)]
    circle: Option<Vec<f64>>,
    #[serde(default)]
    ellipse: Option<Vec<f64>>,
    #[serde(default)]
    wkt: Option<String>,
    /// This row's space, overriding `default_space` for its shape and authored content alike.
    #[serde(default)]
    space: Option<String>,
    /// The artifact's own access labels, one per element, taken whole. `null` and `[]` are no
    /// label, which the layer's `artifact_visibility.default` answers; absent states none, which a
    /// layer whose `artifact_visibility` names a field refuses. Labels are refused on a layer
    /// whose `artifact_visibility` names no field.
    #[serde(default, deserialize_with = "present")]
    access: Option<Option<Vec<String>>>,
}

/// A field whose absence differs from `null`: absent is `None`, and `null` is `Some(None)`.
fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

/// The shape fields of a publication's or a growth's row.
struct RowShape<'a> {
    bbox: &'a Option<Vec<f64>>,
    circle: &'a Option<Vec<f64>>,
    ellipse: &'a Option<Vec<f64>>,
    wkt: &'a Option<String>,
    space: &'a Option<String>,
}

/// A publication's or a growth's row, as [`canonical_batch_shapes`] reads it.
trait ShapedRow {
    /// The row's key, as its shape report names it.
    fn key(&self) -> serde_json::Value;
    fn row_shape(&self) -> RowShape<'_>;
    /// Whether the row's shape is read: a growth without a shape field fills no shape, where a
    /// publication's row on a shape layer is refused without one.
    fn fills_shape(&self) -> bool;
    /// Each ranked content's rank and values.
    fn contents_mut(&mut self) -> impl Iterator<Item = (usize, &mut Vec<String>)>;
}

impl ShapedRow for IncomingArtifactBody {
    fn key(&self) -> serde_json::Value {
        serde_json::json!(self.key)
    }

    fn row_shape(&self) -> RowShape<'_> {
        RowShape {
            bbox: &self.bbox,
            circle: &self.circle,
            ellipse: &self.ellipse,
            wkt: &self.wkt,
            space: &self.space,
        }
    }

    fn fills_shape(&self) -> bool {
        true
    }

    fn contents_mut(&mut self) -> impl Iterator<Item = (usize, &mut Vec<String>)> {
        self.content
            .iter_mut()
            .enumerate()
            .map(|(rank, content)| (rank, &mut content.values))
    }
}

impl ShapedRow for GrowingArtifactBody {
    fn key(&self) -> serde_json::Value {
        serde_json::json!(self.key)
    }

    fn row_shape(&self) -> RowShape<'_> {
        RowShape {
            bbox: &self.bbox,
            circle: &self.circle,
            ellipse: &self.ellipse,
            wkt: &self.wkt,
            space: &self.space,
        }
    }

    fn fills_shape(&self) -> bool {
        self.bbox.is_some() || self.circle.is_some() || self.ellipse.is_some() || self.wkt.is_some()
    }

    fn contents_mut(&mut self) -> impl Iterator<Item = (usize, &mut Vec<String>)> {
        self.content
            .iter_mut()
            .map(|content| (content.rank as usize, &mut content.values))
    }
}

/// One ranked content's authored shape canonicalised for every view of its layer, in the row's
/// space, so a drawing lands where the membership shape beside it does.
fn canonical_authored_content(
    state: &AppState,
    declaration: &tessera_types::layer::LayerDeclaration,
    index: usize,
    rank: usize,
    kind: tessera_types::layer::ShapeKind,
    space: tessera_engine::shapes::ShapeSpace,
    text: &str,
) -> Result<
    (
        tessera_lifecycle::membership::ArtifactShapes,
        serde_json::Value,
    ),
    ApiError,
> {
    use tessera_engine::shapes::{authored_shape_input, shape_input};
    let refuse = |detail: String| {
        ApiError::Contract(format!(
            "artifact {index}: content {rank}: the authored {} content: {detail}",
            kind.as_str()
        ))
    };
    let input = authored_shape_input(kind, text).map_err(|e| refuse(e.to_string()))?;
    let shape = shape_input(kind, input).map_err(|e| refuse(e.to_string()))?;
    canonical_for_layer(state, declaration, &shape, space, refuse)
}

/// One row's shape canonicalised for every view of its layer, as a build does it, with the
/// build's report returned in the response. A refusal names the row and the batch has no effect.
fn canonical_row_shape(
    state: &AppState,
    declaration: &tessera_types::layer::LayerDeclaration,
    index: usize,
    artifact: &RowShape<'_>,
    default_space: tessera_engine::shapes::ShapeSpace,
) -> Result<
    Option<(
        tessera_lifecycle::membership::ArtifactShapes,
        serde_json::Value,
    )>,
    ApiError,
> {
    use tessera_engine::shapes::{shape_input, ShapeInput, ShapeSpace};
    let refuse = |detail: String| ApiError::Contract(format!("artifact {index}: {detail}"));
    let mut carried: Vec<(&str, ShapeInput)> = Vec::new();
    let count = |field: &str, n: usize, want: usize| {
        refuse(format!("`{field}` has {n} value(s); it is exactly {want}"))
    };
    if let Some(v) = artifact.bbox {
        let [a, b, c, d] = v[..] else {
            return Err(count("bbox", v.len(), 4));
        };
        carried.push(("bbox", ShapeInput::Bbox([a, b, c, d])));
    }
    if let Some(v) = artifact.circle {
        let [a, b, c] = v[..] else {
            return Err(count("circle", v.len(), 3));
        };
        carried.push(("circle", ShapeInput::Circle([a, b, c])));
    }
    if let Some(v) = artifact.ellipse {
        let [a, b, c, d, e] = v[..] else {
            return Err(count("ellipse", v.len(), 5));
        };
        carried.push(("ellipse", ShapeInput::Ellipse([a, b, c, d, e])));
    }
    if let Some(text) = artifact.wkt {
        carried.push(("wkt", ShapeInput::Wkt(text.clone())));
    }
    let Some(kind) = declaration.shape.map(|s| s.kind) else {
        if !carried.is_empty() {
            return Err(refuse(
                "carries a shape, and this layer declares no `shape`; remove the shape"
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
                "carries no shape, and this layer's `shape` declares a {}; give the artifact \
                 one",
                kind.as_str()
            )))
        }
        1 => carried.pop().expect("one").1,
        _ => {
            return Err(refuse(format!(
                "carries {}; give one shape, in the field of its layer's kind",
                carried
                    .iter()
                    .map(|(f, _)| format!("`{f}`"))
                    .collect::<Vec<_>>()
                    .join(" and ")
            )))
        }
    };
    let shape = shape_input(kind, input).map_err(|e| refuse(e.to_string()))?;
    canonical_for_layer(state, declaration, &shape, space, refuse).map(Some)
}

/// One shape canonicalised for every view of its layer, and the per-view report of what that did.
fn canonical_for_layer(
    state: &AppState,
    declaration: &tessera_types::layer::LayerDeclaration,
    shape: &tessera_engine::shapes::ShapeF64,
    space: tessera_engine::shapes::ShapeSpace,
    refuse: impl Fn(String) -> ApiError,
) -> Result<
    (
        tessera_lifecycle::membership::ArtifactShapes,
        serde_json::Value,
    ),
    ApiError,
> {
    let meta = state.engine.meta();
    let views: Vec<&str> = declaration.views.iter().map(String::as_str).collect();
    let frames = layer_frames(&meta, &views)?;
    let canonical = tessera_engine::shapes::canonical_shapes(
        shape,
        &frames,
        space,
        state.limits.max_shape_vertices,
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
    Ok((shapes, serde_json::Value::Array(report)))
}

/// Canonicalises a batch's shapes before anything is resolved or allocated: each row's shape,
/// then the authored shape slot of each ranked content, which is replaced by its canonical text.
/// An unknown layer canonicalises nothing here; the engine refuses it.
fn canonical_batch_shapes(
    state: &AppState,
    declaration: Option<&tessera_types::layer::LayerDeclaration>,
    default_space: Option<&str>,
    rows: &mut [impl ShapedRow],
) -> Result<
    (
        Vec<Option<tessera_lifecycle::membership::ArtifactShapes>>,
        Vec<serde_json::Value>,
    ),
    ApiError,
> {
    use tessera_engine::shapes::ShapeSpace;
    let default_space = match default_space {
        None => ShapeSpace::View,
        Some(word) => ShapeSpace::parse(word)
            .map_err(|e| ApiError::Contract(format!("`default_space`: {e}")))?,
    };
    let mut shapes = Vec::with_capacity(rows.len());
    let mut reports: Vec<serde_json::Value> = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let canonical = match declaration {
            Some(declaration) if row.fills_shape() => {
                canonical_row_shape(state, declaration, index, &row.row_shape(), default_space)?
            }
            _ => None,
        };
        match canonical {
            Some((canonical, report)) => {
                shapes.push(Some(canonical));
                reports.push(serde_json::json!({
                    "key": row.key(),
                    "views": report,
                }));
            }
            None => shapes.push(None),
        }
    }
    let Some(declaration) = declaration else {
        return Ok((shapes, reports));
    };
    let Some((slot, kind)) = declaration.authored_shape() else {
        return Ok((shapes, reports));
    };
    for (index, row) in rows.iter_mut().enumerate() {
        let space = match row.row_shape().space.as_deref() {
            None => default_space,
            Some(word) => ShapeSpace::parse(word)
                .map_err(|e| ApiError::Contract(format!("artifact {index}: `space`: {e}")))?,
        };
        let key = row.key();
        for (rank, values) in row.contents_mut() {
            let Some(text) = values.get_mut(slot) else {
                // Short of a value: the engine refuses the row, naming the count.
                continue;
            };
            let (canonical, report) =
                canonical_authored_content(state, declaration, index, rank, kind, space, text)?;
            reports.push(serde_json::json!({
                "key": key,
                "content": rank,
                "views": report,
            }));
            *text = canonical.content_text();
        }
    }
    Ok((shapes, reports))
}

/// An attachment's target, named by key: callers never hold an ordinal. The target must already
/// exist.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AttachmentBody {
    layer: String,
    /// Defaults to the target layer's only level.
    #[serde(default)]
    level: u32,
    key: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct IncomingContentBody {
    /// One value per kind the layer declares, in the order of its `content.supplied` list.
    values: Vec<String>,
    /// The entities this content was generated from, all of which a viewer must see to be served
    /// it; addressed as `members` are. Empty asserts nothing about the corpus, and is refused on a
    /// layer whose content is corpus-derived.
    #[serde(default)]
    generated_from: Vec<String>,
}

/// `PUT /control/layers/{name}/artifacts`: publishes artifacts into one level, all or none; a held
/// key is filled under the fill rule. The answer gives each a `tessera_id`, never an ordinal,
/// since two ordinals disclose how many artifacts lie between them.
async fn publish_artifacts(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(name): axum::extract::Path<String>,
    ApiQuery(wait): ApiQuery<WaitQuery>,
    headers: HeaderMap,
    body: Result<Bytes, axum::extract::rejection::BytesRejection>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let body = body.map_err(|rejection| {
        body_refusal(
            rejection.status(),
            "publish",
            state.limits.publish_max_body_bytes,
            " (ingest.publish_max_body_bytes); send fewer artifacts per request, or publish a \
             large membership's first page and grow it with `PATCH`",
        )
    })?;
    // An artifact record is object-shaped, so publication has no Arrow form.
    if body_encoding(&headers)? == BodyEncoding::Arrow {
        return Err(ApiError::Contract(format!(
            "a publication takes JSON, not `{ARROW_CONTENT_TYPE}`; send `application/json`"
        )));
    }
    let PublishBody {
        level,
        field,
        default_space,
        mut artifacts,
    } = artifact_json(&body, "publication")?;

    if artifacts.is_empty() {
        return Err(ApiError::Contract(
            "a publication carries at least one artifact".to_string(),
        ));
    }
    // The artifact cap, before any shape is canonicalised or address resolved.
    if artifacts.len() > state.limits.max_artifacts_per_request {
        return Err(ApiError::Contract(format!(
            "the publication carries {} artifacts, over the {}-artifact limit \
             (ingest.max_artifacts_per_request); send fewer artifacts per request",
            artifacts.len(),
            state.limits.max_artifacts_per_request
        )));
    }

    // A row names `members` or `excluding`. Only an attached row may carry neither; it is served
    // over its target's membership.
    for (index, artifact) in artifacts.iter().enumerate() {
        if artifact.members.is_none()
            && artifact.excluding.is_none()
            && artifact.attached_to.is_none()
        {
            return Err(ApiError::Contract(format!(
                "artifact {index} of this publication carries neither `members` nor `excluding`, \
                 and attaches to nothing; give one of them, or `\"members\": []` for an empty \
                 membership"
            )));
        }
    }

    // `members` and `excluding` never together, and an exclusion list must fit one request, since
    // its complement cannot be taken until the whole list is in; a longer one is sent as members.
    for (index, artifact) in artifacts.iter().enumerate() {
        let Some(excluding) = artifact.excluding.as_ref() else {
            continue;
        };
        if artifact.members.is_some() {
            return Err(ApiError::Contract(format!(
                "artifact {index} of this publication carries both `members` and `excluding`; \
                 give one of them"
            )));
        }
        if excluding.len() > state.limits.max_excluded_per_request {
            return Err(ApiError::Contract(format!(
                "artifact {index} of this publication excludes {} entities, over the {}-entity \
                 limit (ingest.max_excluded_per_request); name its `members` instead",
                excluding.len(),
                state.limits.max_excluded_per_request
            )));
        }
    }

    // `view` is required on a group-scoped layer and refused on an entity-scoped one. The executor
    // checks again; this answers before any work. An unknown layer is refused by the engine.
    let declaration = state
        .engine
        .registered_layer(&name)
        .map(|registered| registered.declaration);
    if let Some(declaration) = &declaration {
        for (index, artifact) in artifacts.iter().enumerate() {
            match (declaration.scope.group(), artifact.view.as_deref()) {
                (Some(_), Some(_)) | (None, None) => {}
                (Some(group), None) => {
                    return Err(ApiError::Contract(format!(
                        "artifact {index} of this publication names no `view`, and layer '{name}' \
                         is scoped to the group '{group}'; name the key of the view it belongs to"
                    )))
                }
                (None, Some(view)) => {
                    return Err(ApiError::Contract(format!(
                        "artifact {index} of this publication names the view '{view}', and layer \
                         '{name}' is entity-scoped; remove `view`"
                    )))
                }
            }
        }
    }

    let accesses: Vec<Option<Vec<Vec<u8>>>> = artifacts
        .iter_mut()
        .map(|artifact| {
            artifact
                .access
                .take()
                .map(|labels| access_descriptors(&state, labels))
                .transpose()
        })
        .collect::<Result<_, _>>()?;

    // Shapes and addresses read the bundle, so they run on the blocking pool with the write.
    let (batch, keys, shape_reports) = state
        .blocking(move |state| {
            let (shapes, shape_reports) = canonical_batch_shapes(
                state,
                declaration.as_ref(),
                default_space.as_deref(),
                &mut artifacts,
            )?;

            // Every address in one flat list, resolved in one pass: per artifact its members,
            // its exclusions, then each content's generating set.
            let widths: Vec<usize> = artifacts
                .iter()
                .map(|a| {
                    a.members.as_ref().map_or(0, |m| m.len())
                        + a.excluding.as_ref().map_or(0, |e| e.len())
                        + a.content
                            .iter()
                            .map(|v| v.generated_from.len())
                            .sum::<usize>()
                })
                .collect();
            let flat: Vec<&String> = artifacts
                .iter()
                .flat_map(|a| {
                    a.members
                        .iter()
                        .flatten()
                        .chain(a.excluding.iter().flatten())
                        .chain(a.content.iter().flat_map(|v| v.generated_from.iter()))
                })
                .collect();

            let resolved = resolve_member_addresses(
                state,
                field.as_deref(),
                &flat,
                &widths,
                "its members first, then each content's generating set",
            )?;

            // Walked back in the order it was flattened.
            let mut entities = resolved.into_iter();
            let incoming: Vec<tessera_lifecycle::IncomingArtifact> = artifacts
                .into_iter()
                .zip(shapes)
                .zip(accesses)
                .map(|((artifact, shape), access)| {
                    let members: Vec<tessera_types::EntityId> = entities
                        .by_ref()
                        .take(artifact.members.as_ref().map_or(0, |m| m.len()))
                        .collect();
                    let excluded: Option<Vec<tessera_types::EntityId>> = artifact
                        .excluding
                        .as_ref()
                        .map(|list| entities.by_ref().take(list.len()).collect());
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
                    incoming.parent_keys = artifact.parent;
                    incoming.view = artifact.view;
                    incoming.access = access;
                    // The executor takes the complement against the view's entities.
                    if let Some(excluded) = excluded {
                        incoming.exclude(excluded);
                    }
                    incoming
                })
                .collect();
            let keys: Vec<Option<String>> = incoming.iter().map(|a| a.key.clone()).collect();

            let batch = state
                .engine
                .put_artifacts(name, level, incoming)
                .map_err(crate::error::map_accept_error)?;
            Ok((batch, keys, shape_reports))
        })
        .await?;

    let published: Vec<serde_json::Value> = batch
        .tessera_ids
        .iter()
        .zip(keys)
        .map(|(id, key)| serde_json::json!({ "key": key, "tessera_id": id.raw().to_string() }))
        .collect();
    // `shapes` is the build's shape report, present only when a shape was canonicalised.
    // `joined` counts memberships added to created and held artifacts alike. The status is 201
    // if anything was created, 200 if every key was held.
    let mut body = serde_json::json!({
        "artifacts": published,
        "created": batch.created,
        "without_content": batch.without_content,
        "filled": batch.filled,
        "joined": batch.joined,
    });
    if !shape_reports.is_empty() {
        body["shapes"] = serde_json::Value::Array(shape_reports);
    }
    acknowledge(&state, &wait, declared(batch.created == 0), body).await
}

/// `PATCH /control/layers/{name}/artifacts`'s body. `field` and `default_space` are as on
/// [`PublishBody`].
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct GrowBody {
    /// Defaults to the layer's only level.
    #[serde(default)]
    level: u32,
    #[serde(default)]
    field: Option<String>,
    #[serde(default)]
    default_space: Option<String>,
    artifacts: Vec<GrowingArtifactBody>,
}

/// One artifact of a `PATCH`, by key: members joining or leaving a set, and parts to fill. Every
/// part is optional; a row with only a key changes nothing.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct GrowingArtifactBody {
    /// The key the artifact was published under. An unknown key refuses the batch; this verb
    /// mints nothing.
    key: String,
    /// The view the artifact belongs to, on a layer scoped to a group, as the publication's record
    /// names it. Required there and refused on an entity-scoped layer.
    #[serde(default)]
    view: Option<String>,
    /// Absent, the row moves the membership; present, the generating set of the content at that
    /// rank, and the row carries no part to fill.
    #[serde(default)]
    rank: Option<u16>,
    /// Members joining, addressed as a publication's are. Empty adds nothing.
    #[serde(default)]
    members: Vec<String>,
    /// Members leaving, applied after the joins. Only a generating set may shrink, so this needs a
    /// `rank`; a page that empties a set withdraws its content.
    #[serde(default)]
    leaving: Vec<String>,
    /// Parent keys: filled where the artifact holds none, accepted if identical, 409 otherwise.
    #[serde(default)]
    parent: Vec<String>,
    /// The attachment, under the same fill rule.
    #[serde(default)]
    attached_to: Option<AttachmentBody>,
    /// Contents by rank, each filled, identical or 409. A content here carries no generating set,
    /// so it is refused on a layer whose content requires every member visible.
    #[serde(default)]
    content: Vec<ContentFillBody>,
    /// The shape, as on a publication; absent where the row fills none.
    #[serde(default)]
    bbox: Option<Vec<f64>>,
    #[serde(default)]
    circle: Option<Vec<f64>>,
    #[serde(default)]
    ellipse: Option<Vec<f64>>,
    #[serde(default)]
    wkt: Option<String>,
    /// This row's own space, overriding `default_space`.
    #[serde(default)]
    space: Option<String>,
    /// The access labels, on [`IncomingArtifactBody::access`]'s terms and the fill rule: filled on
    /// an artifact that has none, accepted when identical, `409` otherwise.
    #[serde(default)]
    access: Option<Vec<String>>,
}

/// One content a `PATCH` fills: its rank, and one value per declared kind in order.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ContentFillBody {
    rank: u16,
    values: Vec<String>,
}

/// `PATCH /control/layers/{name}/artifacts`: pages members into artifacts the level already holds,
/// and fills parts they lack (409 for a differing part, never naming the held value). The batch
/// commits whole. The answer is 200 and never carries an ordinal or a membership size.
async fn grow_memberships(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(name): axum::extract::Path<String>,
    ApiQuery(wait): ApiQuery<WaitQuery>,
    headers: HeaderMap,
    body: Result<Bytes, axum::extract::rejection::BytesRejection>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let body = body.map_err(|rejection| {
        body_refusal(
            rejection.status(),
            "grow",
            state.limits.publish_max_body_bytes,
            " (ingest.publish_max_body_bytes); send fewer members per request",
        )
    })?;
    let GrowBody {
        level,
        field,
        default_space,
        mut artifacts,
    } = match body_encoding(&headers)? {
        BodyEncoding::Json => artifact_json(&body, "growth")?,
        BodyEncoding::Arrow => grow_body_from_arrow(&body)?,
    };
    let accesses: Vec<Vec<Vec<u8>>> = artifacts
        .iter_mut()
        .map(|artifact| access_descriptors(&state, artifact.access.take()))
        .collect::<Result<_, _>>()?;

    if artifacts.is_empty() {
        return Err(ApiError::Contract(
            "a growth names at least one artifact".to_string(),
        ));
    }

    // The member cap, over joining and leaving members, before any address is resolved.
    let members: usize = artifacts
        .iter()
        .map(|a| a.members.len() + a.leaving.len())
        .sum();
    if members > state.limits.max_members_per_request {
        return Err(ApiError::Contract(format!(
            "the growth names {} members, over the {}-member limit \
             (ingest.max_members_per_request); send fewer members per request",
            members,
            state.limits.max_members_per_request
        )));
    }

    let (grown, keys, shape_reports) = state
        .blocking(move |state| {
            // Shapes go through the publication's reader, so a filled shape is stored byte for
            // byte as a publication would store it.
            let declaration = state
                .engine
                .registered_layer(&name)
                .map(|registered| registered.declaration);
            let (shapes, shape_reports) = canonical_batch_shapes(
                state,
                declaration.as_ref(),
                default_space.as_deref(),
                &mut artifacts,
            )?;

            // Joining and leaving members are resolved in one pass, joins first in each row, so an
            // entity named in both resolves to one entity.
            let widths: Vec<usize> = artifacts
                .iter()
                .map(|a| a.members.len() + a.leaving.len())
                .collect();
            let flat: Vec<&String> = artifacts
                .iter()
                .flat_map(|a| a.members.iter().chain(a.leaving.iter()))
                .collect();
            let resolved = resolve_member_addresses(
                state,
                field.as_deref(),
                &flat,
                &widths,
                "its members",
            )?;

            let mut entities = resolved.into_iter();
            let joins: Vec<tessera_lifecycle::IncomingGrowth> = artifacts
                .into_iter()
                .zip(shapes)
                .zip(accesses)
                .map(|((artifact, shape), access)| {
                    let members: Vec<tessera_types::EntityId> =
                        entities.by_ref().take(artifact.members.len()).collect();
                    let leaving: Vec<tessera_types::EntityId> =
                        entities.by_ref().take(artifact.leaving.len()).collect();
                    // Every row carries all its fields; the executor refuses combinations it does
                    // not take, rather than this dropping them and answering 200.
                    let mut join = tessera_lifecycle::IncomingGrowth::page_of_entities(
                        artifact.key,
                        artifact.rank,
                        members,
                        leaving,
                    );
                    join.parts = tessera_lifecycle::FixedParts {
                        parent_keys: artifact.parent,
                        attached_to: artifact.attached_to.map(|a| {
                            tessera_lifecycle::membership::IncomingAttachment {
                                layer: a.layer,
                                level: a.level,
                                key: a.key,
                            }
                        }),
                        contents: artifact
                            .content
                            .into_iter()
                            .map(|c| (c.rank, c.values))
                            .collect(),
                        shape,
                        access,
                    };
                    join.view = artifact.view;
                    join
                })
                .collect();
            let keys: Vec<String> = joins.iter().map(|j| j.key.clone()).collect();

            let grown = state
                .engine
                .grow_memberships(name, level, joins)
                .map_err(crate::error::map_accept_error)?;
            Ok((grown, keys, shape_reports))
        })
        .await?;

    let artifacts: Vec<serde_json::Value> = grown
        .iter()
        .zip(keys)
        .map(|(receipt, key)| {
            let mut row = serde_json::json!({
                "key": key,
                "tessera_id": receipt.tessera_id.raw().to_string(),
                "joined": receipt.joined,
                "filled": receipt.filled,
                "left": receipt.left,
            });
            // The page emptied this rank's generating set; the content is gone for good.
            if let Some(rank) = receipt.withdrawn {
                row["withdrawn"] = serde_json::json!(rank);
            }
            row
        })
        .collect();
    let mut body = serde_json::json!({ "artifacts": artifacts });
    if !shape_reports.is_empty() {
        body["shapes"] = serde_json::Value::Array(shape_reports);
    }
    acknowledge(&state, &wait, StatusCode::OK, body).await
}
/// `POST /control/faults/arm`: arms a pause site with `Stall`, so a test driver can park the
/// executor, then kill or release it. Only a stall is armable over the wire: a crash is the
/// driver's own `SIGKILL`, and a panic unwinds, which is not a crash.
#[cfg(feature = "fault-injection")]
async fn faults_arm(
    State(state): State<Arc<AppState>>,
    ApiJson(req): ApiJson<FaultSiteRequest>,
) -> Result<StatusCode, ApiError> {
    let site = parse_pause_site(&req.site)?;
    state
        .faults
        .arm_pause(site, tessera_lifecycle::faults::PauseAction::Stall);
    Ok(StatusCode::OK)
}

/// `GET /control/faults/arrivals?site=<name>`: how often the executor has reached the site since
/// it was armed. Under a stall, non-zero means a thread is parked there.
#[cfg(feature = "fault-injection")]
async fn faults_arrivals(
    State(state): State<Arc<AppState>>,
    ApiQuery(req): ApiQuery<FaultSiteRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let site = parse_pause_site(&req.site)?;
    Ok(Json(serde_json::json!({
        "site": site.name(),
        "arrivals": state.faults.arrivals(site),
    })))
}

/// `POST /control/faults/release`: releases every parked thread and disarms every site, so a
/// driver cannot leave the executor wedged on a site it forgot.
#[cfg(feature = "fault-injection")]
async fn faults_release(State(state): State<Arc<AppState>>) -> StatusCode {
    state.faults.release();
    StatusCode::OK
}

/// A pause site by its switchboard name.
#[cfg(feature = "fault-injection")]
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
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

/// `GET /control/status`: the operator's view of the node, behind the credential like every
/// control route; it discloses corpus-wide figures such as `entity_id_high_water`.
///
/// `compute` is the admission limit the viewport, item and session routes share, and `bulk` the
/// one the bulk reads run under, `serve.bulk_admission`: its `admission`, its `queue` (always
/// 0, so a read past the limit is refused at once), the reads `in_flight`, which hold their
/// compute for the whole response, `waiting`, and the `shed_total` of 429s it answered.
async fn status(State(state): State<Arc<AppState>>) -> Result<Json<serde_json::Value>, ApiError> {
    // `shed_total` counts this gate's own sheds, not the engine's single-flight 429s.
    let gate = state.compute_gate.status();
    let bulk = state.bulk_gate.status();
    // The posture string is served only here, behind the credential; `/readyz` stays a bare
    // boolean. `ready` uses the probes' own `is_ready`, so they cannot disagree.
    let executor = state.engine.write_executor_stats();
    let ready = is_ready(executor.posture);
    let live = state.engine.generation_status();
    let partitions = live.partitions;
    let projection_cache: tessera_engine::CacheStats = state.engine.row_projection_cache_stats();
    let projection_routes = state.engine.projection_builds_by_route();
    let fragment_cache: tessera_engine::FragmentCacheStats = live.fragment_cache;
    let masked_counts = state.engine.masked_count_cache_stats();
    let region_cache: tessera_engine::CacheStats = state.engine.region_cache_stats();
    let derived_cache = state.engine.derived_cache_stats();
    let suggest_sets = state.engine.suggest_set_stats();
    let occupancy: tessera_engine::CacheStats = state.engine.occupancy_cache_stats();
    let heap = state.heap.stats();
    let ingest = state.ingest_admission.status();
    let sessions = state.sessions.lock().stats();
    let segments: Vec<tessera_engine::ViewSegments> = live.segments;
    Ok(Json(serde_json::json!({
        "entity_id_high_water": state.engine.allocator_high_water(),
        // Cycles completed since the executor started, one per cycle whatever it published: the
        // number a write acknowledgement's `publication` is compared against.
        "publication": state.engine.publication(),
        // A version bump shows a flush, merge or fold published; the watermark is which entities
        // the published geometry covers. Both come from one generation load.
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
            // Responses still streaming, which hold a slot but no compute.
            "streaming": gate.streaming,
            "shed_total": gate.shed_total,
        },
        // The bulk-read lane, `POST /v1/items` and `POST /v1/artifacts`, which holds its compute for the whole response.
        "bulk": {
            "admission": bulk.admission,
            "queue": bulk.queue,
            "in_flight": bulk.in_flight,
            "waiting": bulk.waiting,
            "shed_total": bulk.shed_total,
        },
        "write_executor": {
            "posture": executor.posture.as_str(),
            "ready": ready,
            "work_submitted": executor.work_submitted,
            "deny_submitted": executor.deny_submitted,
            "wal_appends": executor.wal_appends,
            "wal_fsyncs": executor.wal_fsyncs,
            // WAL faults recovered from. The posture returns to `running` afterwards, so this is
            // the alarm: any rise means denies were answered 500 and their callers owe retries.
            "wal_recoveries": executor.wal_recoveries,
            // The log's size, sampled on the executor at most once per flush period. `members`
            // should stay near two; `position` is cumulative, not a size. A pin names what keeps
            // the log from rotating: `growth` and `fill` pins clear only at a compaction fold.
            "wal": {
                "members": executor.wal.members,
                "bytes": executor.wal.bytes,
                "position": executor.wal.position,
                "pinned_by": executor.wal.pin.map(|(held_by, _)| held_by),
                "pinned_at": executor.wal.pin.map(|(_, pos)| pos),
                "pin_span_bytes": executor.wal.pin_span_bytes,
                "samples": executor.wal.samples,
            },
            "apply_nanos_total": executor.apply_nanos_total,
            "apply_nanos_max": executor.apply_nanos_max,
            // Side-manifests this node found written by someone else: evidence of a second writer
            // over the bundle root. Zero on a node whose bundle is its own.
            "foreign_side_manifests": executor.foreign_side_manifests,
            // `work_depth` is a snapshot of two counters and the EWMA an estimate; together they
            // give the 429's `Retry-After`.
            "work_completed": executor.work_completed,
            "work_depth": executor.work_depth,
            "work_service_nanos_ewma": executor.work_service_nanos_ewma,
            // `flush_skips` rising means flushes are slower than the tick. `flushable_items` counts
            // rows (an item in two views twice); `buffered_items`, the 429's measure, counts items.
            "flush": {
                "ticks": executor.ticks,
                "flushes": executor.flushes,
                "flush_skips": executor.flush_skips,
                "flush_failures": executor.flush_failures,
                "flushable_items": executor.flushable_items,
                "flush_requested": executor.flush_requested,
                // Whether a flush is running on the pool or finished and not yet published,
                // which the two flush counts cannot show.
                "in_flight": executor.flush_in_flight,
                "buffered_items": executor.buffered_items,
                "overlay_publications": executor.overlay_publications,
                // Session projections refreshed after a flush or merge. A live session serves
                // its old projection until then, so a version bump alone does not show the rows.
                "refreshes": state.engine.refreshes(),
                // Whether the refresh for the newest flush or merge is still running. Until it
                // ends, a resident session may be served the previous generation or refused with
                // 429.
                "refresh_in_flight": state.engine.refresh_in_flight(),
            },
            // A coalesce bumps no version, so this counter is the only sign one ran.
            "coalesces": executor.coalesces,
            "merges": executor.merges,
            // Whether a coalesce or a merge is running on the pool or finished and not yet
            // published.
            "coalesce_in_flight": executor.coalesce_in_flight,
            "merge_in_flight": executor.merge_in_flight,
            // How long the current job has run. The EWMA moves only when a job ends; the 429
            // estimate takes the larger of the two.
            "work_in_flight_nanos": executor.work_in_flight_nanos,
            // Stage laps in nanoseconds, recorded only under `bench-timing`; the flag says whether
            // zeros mean idle or unmeasured.
            "bench_timing": cfg!(feature = "bench-timing"),
            "stage_nanos": tessera_engine::WriteStage::ALL
                .iter()
                .map(|stage| {
                    (
                        stage.name().trim().to_owned(),
                        serde_json::json!(executor.stage_nanos[*stage as usize]),
                    )
                })
                .collect::<serde_json::Map<String, serde_json::Value>>(),
            // Flush laps in two maps, one per thread, not included in `stage_nanos`. Divide pool
            // stages by `executions` and executor stages by `flushes` for a per-flush figure.
            "flush_stages": {
                "bench_timing": cfg!(feature = "bench-timing"),
                "executions": executor.flush_executions,
                "rows_executed": executor.flush_rows_executed,
                "flushes": executor.flushes,
                "rows_published": executor.flush_rows_published,
                "executor_nanos": tessera_engine::FlushStage::EXECUTOR
                    .iter()
                    .map(|stage| {
                        (
                            stage.name().to_owned(),
                            serde_json::json!(executor.flush_stage_nanos[*stage as usize]),
                        )
                    })
                    .collect::<serde_json::Map<String, serde_json::Value>>(),
                "pool_nanos": tessera_engine::FlushStage::POOL
                    .iter()
                    .map(|stage| {
                        (
                            stage.name().to_owned(),
                            serde_json::json!(executor.flush_stage_nanos[*stage as usize]),
                        )
                    })
                    .collect::<serde_json::Map<String, serde_json::Value>>(),
            },
        },
        // `shed_total` counts the admission bound's 429s only, not the executor queue's.
        "ingest": {
            "admission": ingest.admission,
            "in_flight": ingest.in_flight,
            "shed_total": ingest.shed_total,
        },
        // Each write route's record and byte caps, so a client can size its pages. A page over
        // either is a 422 naming the limit, never a truncation.
        "limits": {
            "ingest": {
                "route": "POST /control/ingest",
                "max_batch_rows": state.limits.ingest_max_batch_rows,
                "max_batch_bytes": state.limits.ingest_max_batch_bytes,
            },
            "publish": {
                "route": "PUT /control/layers/{name}/artifacts",
                "max_artifacts_per_request": state.limits.max_artifacts_per_request,
                "max_body_bytes": state.limits.publish_max_body_bytes,
                "max_shape_vertices": state.limits.max_shape_vertices,
                "max_excluded_per_request": state.limits.max_excluded_per_request,
            },
            "grow": {
                "route": "PATCH /control/layers/{name}/artifacts",
                "max_members_per_request": state.limits.max_members_per_request,
                "max_body_bytes": state.limits.publish_max_body_bytes,
            },
            "changes": {
                "route": "POST /control/changes",
                "max_changes_per_request": CHANGES_MAX_ITEMS,
                "max_body_bytes": CHANGES_MAX_BODY_BYTES,
            },
            "declarations": {
                "route": "PUT /control/layers, PUT /control/attributes, \
                          PUT /control/vocabularies/{name}, \
                          PATCH /control/vocabularies/{name}/values, \
                          PUT /control/view_groups/{name}, PUT /control/views/{name}, \
                          PUT /control/views/{group}/{key}",
                "max_records_per_request": 1,
                "max_body_bytes": DECLARATION_MAX_BODY_BYTES,
            },
        },
        // Segments per partition and view, which every viewport pays for. Merge bounds the count
        // only down to corpus size over the largest merged segment; a fold resets it to one. A
        // count past `compaction_max_segments` means folds are being refused.
        "segments": segments
            .iter()
            .map(|s| serde_json::json!({
                "partition": s.partition,
                "view": s.view,
                "count": s.segments,
            }))
            .collect::<Vec<_>>(),
        // `depth` counts deletions and suppressions; `retirable` only the deletions, which a fold
        // can retire and which the fold schedule keys on. Suppressions never retire.
        "overlay": {
            "depth": live.overlay_depth,
            "retirable": live.retirable_deletions,
            "soft_limit_alarms": executor.overlay_soft_limit_alarms,
        },
        // Alarm on `fold_failures` (each leaves a bundle-sized tree nothing reclaims) and on
        // `fold_refusals` rising while `folds` is flat; `fold_refusals_by_gate` names the standing
        // gate. `last_rss_bytes` is sampled between passes, so it understates the peak.
        "compaction": {
            "live_rows": live.live_rows,
            "folds": executor.folds,
            "fold_failures": executor.fold_failures,
            "fold_requested": executor.fold_requested,
            "fold_refusals": executor.fold_refusals,
            "fold_refusals_by_gate": tessera_engine::FOLD_GATES
                .iter()
                .zip(executor.fold_refusals_by_gate)
                .map(|(gate, count)| (gate.to_string(), serde_json::json!(count)))
                .collect::<serde_json::Map<_, _>>(),
            "last_refusal": executor.last_fold_refusal.map(|r| serde_json::json!({
                "gate": r.gate,
                "need_bytes": r.need_bytes,
                "had_bytes": r.had_bytes,
                "at_unix": r.at_unix,
            })),
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
        // How scattered ingested postings are: entity ids are sorted by term signature only within
        // one allocation run, so small runs erode compression. Measured over delta tiers as each is
        // flushed; base postings are excluded. The ratios are `null` until the first flush.
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
        // A young eviction is an entry evicted before it was read again: the bound is smaller
        // than what the sessions hold at once.
        "row_projection_cache": {
            "entries": projection_cache.entries,
            "bytes": projection_cache.bytes,
            "bound_bytes": projection_cache.bound_bytes,
            "hits": projection_cache.hits,
            "misses": projection_cache.misses,
            // Same-key racers, refused or served by waiting; their sum is the race rate.
            // `waiters_now` is a process-wide gauge.
            "building_refusals": projection_cache.building_refusals,
            "waits_satisfied": projection_cache.waits_satisfied,
            "waiters_now": projection_cache.waiters_now,
            "evictions": projection_cache.evictions,
            "young_evictions": projection_cache.young_evictions,
            "thrashing": projection_cache.young_evictions > 0,
            "oversized_admissions": projection_cache.oversized_admissions,
        },
        // How session row projections were built, request path and refresh together. Every route
        // gives the same rows; the shares show whether the route chooser's cost model fits.
        "projection_builds_by_route": {
            "whole_domain": projection_routes[0],
            "walk": projection_routes[1],
            "split": projection_routes[2],
            "complement": projection_routes[3],
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
            // Tells an eviction from a genuinely cold rebuild.
            "rebuilds": live.fragment_cache_rebuilds,
        },
        // Expired sessions are swept when `retained` reaches `sweep_at`, under the lock viewer
        // requests take. Each retained session pins its fragment. Sweeping only frees memory: an
        // expired session is refused whether or not it has been swept.
        "sessions": {
            "retained": sessions.retained,
            "sweeps": sessions.sweeps,
            "swept_total": sessions.swept_total,
            "sweep_at": sessions.sweep_at,
        },
        // The per-session caches, pruned at revoke and at the expiry sweep, so an operator can
        // tell cache residency from allocator slack. `masked_count_cache`, `derived_cache` and
        // `suggest_sets` do not report their bound. Occupancy evictions are normal.
        "masked_count_cache": {
            "entries": masked_counts.entries,
            "bytes": masked_counts.resident_bytes,
            "hits": masked_counts.hits,
            "misses": masked_counts.misses,
            "evictions": masked_counts.evictions,
        },
        "region_cache": {
            "entries": region_cache.entries,
            "bytes": region_cache.bytes,
            "bound_bytes": region_cache.bound_bytes,
            "hits": region_cache.hits,
            "misses": region_cache.misses,
            "building_refusals": region_cache.building_refusals,
            "waits_satisfied": region_cache.waits_satisfied,
            "evictions": region_cache.evictions,
        },
        "derived_cache": {
            "entries": derived_cache.entries,
            "bytes": derived_cache.resident_bytes,
            "hits": derived_cache.hits,
            "misses": derived_cache.misses,
            "evictions": derived_cache.evictions,
            // `null` before any lookup, since zero is a reading.
            "hit_rate": derived_cache.hit_rate(),
        },
        // `declined` counts viewers too broad for a value set, which take the probe instead.
        "suggest_sets": {
            "entries": suggest_sets.entries,
            "bytes": suggest_sets.resident_bytes,
            "hits": suggest_sets.hits,
            "misses": suggest_sets.misses,
            "builds": suggest_sets.builds,
            "declined": suggest_sets.declined,
            "discarded": suggest_sets.discarded,
            "evictions": suggest_sets.evictions,
            "in_flight": suggest_sets.in_flight,
        },
        // The occupancy memo, per session, view, depth and generation. `walks` should rise once
        // per session, view and generation; once per request means the memo is failing.
        "occupancy": {
            "entries": occupancy.entries,
            "bytes": occupancy.bytes,
            "bound_bytes": occupancy.bound_bytes,
            "hits": occupancy.hits,
            "misses": occupancy.misses,
            "evictions": occupancy.evictions,
            "walks": state.engine.occupancy_walks(),
        },
        // The kernel's view of the process; the gap to the caches above is allocator retention. A
        // trim that returns much was retention; one that returns nothing while `anon_bytes` stays
        // high means the memory is live.
        "heap": {
            "anon_bytes": heap.anon_bytes,
            "file_bytes": heap.file_bytes,
            "resident_bytes": heap.resident_bytes,
            "trims": heap.trims,
            "last_trim_returned_bytes": heap.last_trim_returned_bytes,
            "last_trim_micros": heap.last_trim_micros,
            "trim_baseline_bytes": heap.trim_baseline_bytes,
            "trim_growth_bytes": crate::memory::TRIM_GROWTH_BYTES,
        },
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// The deny lane still runs while tokio's blocking pool is full. The timeout bounds only the
    /// failing path.
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
            // Every ambient blocking thread has arrived, so the pool is full.
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

    /// Discarding a losing runtime inside an async context does not panic, on either reactor
    /// flavour. Tested on the disposal directly because `DENY_RUNTIME` is process-global.
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
