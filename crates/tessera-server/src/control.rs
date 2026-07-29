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
use sha2::{Digest, Sha256};

use tessera_lifecycle::{ChangeOp, PendingItem, WalRecord, WalRow, WalScalar};
use tessera_types::TermId;

use crate::error::ApiError;
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
            items.push(RawIngestItem {
                external_id: ext.value(i).to_vec(),
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

    let mut pending: Vec<PendingItem> = items
        .iter()
        .zip(&terms_per_item)
        .map(|(item, terms)| PendingItem {
            external_id: item.external_id.clone(),
            terms: terms.clone(),
            entity_id: None,
        })
        .collect();
    // I9/§11.1: signature-sorted assignment, from day one, permanent.
    state.engine.allocate_sorted(&mut pending);

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

    let record = WalRecord::IngestBatch {
        batch_id: batch_id.clone(),
        body_hash,
        rows: rows.clone(),
    };

    // The ack contract: WAL append -> fsync -> apply+swap -> 200. Never 200 without fsync.
    {
        let mut wal = state.engine.wal().lock().unwrap();
        wal.append(&record).map_err(|e| {
            tracing::error!("wal append failed for an ingest batch");
            ApiError::FailClosed(format!("wal append failed: {e}"))
        })?;
        wal.fsync().map_err(|e| {
            tracing::error!("wal fsync failed for an ingest batch");
            ApiError::FailClosed(format!("wal fsync failed: {e}"))
        })?;
    }

    state.engine.apply_ingest(&rows, &terms_per_item);
    state.engine.record_accepted_batch(batch_id, body_hash);

    Ok(Json(IngestResp {
        accepted: rows.len() as u64,
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

async fn changes(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(items): Json<Vec<ChangeItem>>,
) -> Result<StatusCode, ApiError> {
    state.check_bearer(bearer_token(&headers), &state.operator_credential)?;

    for item in items {
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
            .ok_or_else(|| ApiError::Unknown("unknown external id".to_string()))?;

        let descriptors: Option<Vec<Vec<u8>>> = match &item.access {
            Some(access) => Some(
                state
                    .engine
                    .plugin()
                    .terms_of_label(access.as_bytes())
                    .map_err(|e| ApiError::Contract(format!("access field: {e}")))?,
            ),
            None => None,
        };
        let terms: Option<Vec<TermId>> = descriptors
            .as_ref()
            .map(|ds| state.engine.resolve_terms(ds));

        let record = WalRecord::Change {
            external_id: external_id_bytes,
            op,
            descriptors: descriptors.clone(),
        };

        let append_result = {
            let mut wal = state.engine.wal().lock().unwrap();
            wal.append(&record).and_then(|()| wal.fsync().map(|_| ()))
        };

        match append_result {
            Ok(()) => {
                state.engine.apply_change(entity, op, terms);
            }
            Err(e) => {
                if matches!(op, ChangeOp::Delete | ChangeOp::Suppress) {
                    // Deny-op append failure (lifecycle §4): apply anyway, alarm, 500. Never a
                    // refusal that leaves a deny unapplied.
                    tracing::error!(
                        op = ?op,
                        "ALARM: wal append/fsync failed for a deny-op change; applied to the \
                         in-memory overlay anyway (item hidden immediately) and returning 500 — \
                         durability is owed, caller must retry"
                    );
                    state.engine.apply_change(entity, op, terms);
                } else {
                    tracing::error!(
                        "wal append/fsync failed for a non-deny change; refusing without applying"
                    );
                }
                return Err(ApiError::FailClosed(format!(
                    "wal append/fsync failed: {e}"
                )));
            }
        }
    }

    // R5: `/control/changes` is 200 after fsync, never 429.
    Ok(StatusCode::OK)
}

async fn status(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "entity_id_high_water": state.engine.allocator_high_water(),
    }))
}
