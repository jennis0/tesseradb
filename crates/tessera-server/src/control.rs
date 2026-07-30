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

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use rustc_hash::FxHashSet;
use sha2::{Digest, Sha256};

use tessera_lifecycle::{ChangeOp, PendingItem, WalRow, WalScalar};
use tessera_types::{EntityId, TermId};

use crate::error::{map_store_error, ApiError};
use crate::health::{healthz, readyz};
use crate::state::AppState;

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
    external_id: Vec<u8>,
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

        let ext = binary_col(&batch, "external_id")?;
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
            let external_id = ext.value(i).to_vec();
            // Contracts §1 (r6): a typed error, never a truncation -- see `EXTERNAL_ID_MAX_LEN`'s
            // doc. Checked here, inside the whole-batch parse, so an over-length id anywhere in
            // the batch fails the parse before anything downstream (replay check, dedup,
            // allocation, WAL append) ever runs: the batch has no effect, exactly as a duplicate
            // 409 must.
            if external_id.len() > EXTERNAL_ID_MAX_LEN {
                return Err(ApiError::Contract(format!(
                    "external id is {} bytes, exceeding the {EXTERNAL_ID_MAX_LEN}-byte cap \
                     (contracts §1); refused rather than truncated",
                    external_id.len()
                )));
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

fn binary_col<'a>(
    batch: &'a arrow::record_batch::RecordBatch,
    name: &str,
) -> Result<&'a arrow::array::BinaryArray, ApiError> {
    batch
        .column_by_name(name)
        .and_then(|c| c.as_any().downcast_ref::<arrow::array::BinaryArray>())
        .ok_or_else(|| {
            ApiError::Contract(format!(
                "ingest body: column '{name}' missing or not binary"
            ))
        })
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

    let body_hash: [u8; 32] = Sha256::digest(&body).into();

    let items = parse_ingest_batch(&body)?;

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
            if over_bound_ids.len() < 100 {
                over_bound_ids.push(String::from_utf8_lossy(&item.external_id).to_string());
            }
        }
        descriptor_lists.push(descriptors);
        terms_per_item.push(terms);
    }

    if let Some(prev_hash) = state.engine.accepted_batch(&batch_id) {
        if prev_hash == body_hash {
            // Idempotent replay of an already-acked batch: 200, no effect (R5).
            return Ok(Json(IngestResp {
                accepted: items.len() as u64,
                over_bound,
                over_bound_ids,
            }));
        }
        return Err(ApiError::Conflict(format!(
            "batch id '{batch_id}' was already accepted with a different body"
        )));
    }

    // Validate-first (contracts §3.1 r6): duplicate external ids are 409, detail lists them, and
    // the batch has NO effect -- so this runs entirely before `allocate_sorted`/WAL append below,
    // and after the batch-id replay check above, which stays first (an idempotent replay of an
    // already-acked batch must still be a 200 no-op, not get caught here as "already known").
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
        if !seen_in_batch.insert(item.external_id.as_slice()) {
            dup_ids.push(base64::engine::general_purpose::STANDARD.encode(&item.external_id));
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

    let batch_external_ids: Vec<Vec<u8>> =
        items.iter().map(|item| item.external_id.clone()).collect();
    let resolved = state
        .engine
        .resolve_external_ids(&batch_external_ids)
        .map_err(map_store_error)?;
    let existing_ids: Vec<String> = resolved
        .iter()
        .zip(&items)
        .filter(|(entity, _)| entity.is_some())
        .map(|(_, item)| base64::engine::general_purpose::STANDARD.encode(&item.external_id))
        .collect();
    if !existing_ids.is_empty() {
        return Err(ApiError::Conflict(format!(
            "duplicate external ids already known to this deployment: {}",
            existing_ids.join(", ")
        )));
    }

    let mut pending: Vec<PendingItem> = items
        .iter()
        .zip(&terms_per_item)
        .map(|(item, terms)| PendingItem {
            external_id: item.external_id.clone(),
            terms: terms.clone(),
            entity_id: None,
        })
        .collect();
    // I9/§11.1: signature-sorted assignment, from day one, permanent. The allocator refuses
    // rather than issue an ID at or above the u32 ceiling (plan Important I-1) -- fail closed,
    // never silently truncate or wrap.
    state
        .engine
        .allocate_sorted(&mut pending)
        .map_err(|e| ApiError::FailClosed(e.to_string()))?;

    let rows: Vec<WalRow> = items
        .iter()
        .zip(pending.iter())
        .zip(descriptor_lists.iter())
        .map(|((item, pending_item), descriptors)| WalRow {
            external_id: item.external_id.clone(),
            entity_id: pending_item
                .entity_id
                .expect("allocate_sorted assigns every item"),
            descriptors: descriptors.clone(),
            x: item.x,
            y: item.y,
            scalars: item.scalars.clone(),
        })
        .collect();

    // The ack contract, atomically: WAL append -> fsync -> apply+swap -> 200. Never 200 without
    // fsync. `Engine::accept_ingest` holds the WAL lock across the whole sequence (Critical 1
    // fix), so this can never race a concurrent `/control/changes` acceptance into a lost-update
    // generation swap.
    let accepted = rows.len() as u64;
    state
        .engine
        .accept_ingest(rows, terms_per_item, batch_id, body_hash)
        .map_err(|e| {
            tracing::error!("wal append/fsync failed for an ingest batch");
            ApiError::FailClosed(format!("wal append/fsync failed: {e}"))
        })?;

    Ok(Json(IngestResp {
        accepted,
        over_bound,
        over_bound_ids,
    }))
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

async fn changes(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(items): Json<Vec<ChangeItem>>,
) -> Result<StatusCode, ApiError> {
    state.check_bearer(bearer_token(&headers), &state.operator_credential)?;

    // Validate-first (Important 2 fix): parse every item's op, base64-decode and resolve its
    // external id, and validate its `access` field's shape — all *before* appending anything.
    // The previous item-by-item loop could append, fsync and apply items 1..n-1 before item n's
    // 404/422 aborted the request, leaving the caller with a single error for a batch that was
    // actually partially applied. Doing every fallible *validation* step first means a rejected
    // batch is rejected wholesale, with no side effect at all. (A WAL I/O failure partway through
    // the second, apply-only loop below is a different class of failure — an infrastructure
    // fault, not a client-correctable validation error — and is not, and cannot be, rolled back:
    // each item's `Engine::accept_change` call is its own complete ack-contract unit, exactly as
    // `/control/ingest`'s batches are.)
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

    for change in validated {
        state
            .engine
            .accept_change(
                change.external_id,
                change.entity,
                change.op,
                change.raw_descriptors,
            )
            .map_err(|e| {
                if matches!(change.op, ChangeOp::Delete | ChangeOp::Suppress) {
                    // Deny-op append failure (lifecycle §4): `Engine::accept_change` already
                    // applied the change to the live overlay before returning this error — never
                    // a refusal that leaves a deny unapplied.
                    tracing::error!(
                        op = ?change.op,
                        "ALARM: wal append/fsync failed for a deny-op change; applied to the \
                         in-memory overlay anyway (item hidden immediately) and returning 500 — \
                         durability is owed, caller must retry"
                    );
                } else {
                    tracing::error!(
                        "wal append/fsync failed for a non-deny change; refusing without applying"
                    );
                }
                ApiError::FailClosed(format!("wal append/fsync failed: {e}"))
            })?;
    }

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
    Ok(Json(serde_json::json!({
        "entity_id_high_water": state.engine.allocator_high_water(),
    })))
}
