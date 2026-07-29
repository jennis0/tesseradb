//! The viewer plane (R5): `GET /v1/meta`, `POST /v1/viewport`, `POST /v1/items/{handle}`, plus
//! `/healthz`/`/readyz`. Bearer auth is a session token (Task 11's `Session::token`).

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use tessera_types::{Handle, PinId};
use tessera_wire::{viewport_ipc, ScalarColumn};

use crate::error::{map_engine_error, ApiError};
use crate::health::{healthz, readyz};
use crate::state::AppState;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/meta", get(meta))
        .route("/v1/viewport", post(viewport))
        .route("/v1/items/{handle}", post(item))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .with_state(state)
}

/// The wire shape of a pin, both in `POST /v1/viewport`'s request body and the `x-tessera-pin`
/// response header (JSON either way — the header carries the same shape as a plain string so a
/// client can round-trip it without inventing its own encoding).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PinDto {
    prefix: String,
    segments_version: u64,
}

impl From<PinDto> for PinId {
    fn from(p: PinDto) -> Self {
        PinId {
            prefix: p.prefix,
            segments_version: p.segments_version,
        }
    }
}

impl From<&PinId> for PinDto {
    fn from(p: &PinId) -> Self {
        PinDto {
            prefix: p.prefix.clone(),
            segments_version: p.segments_version,
        }
    }
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
}

async fn meta(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Important 1 fix: this handler previously had no bearer check at all, against R5's "Bearer
    // auth on every plane" — it disclosed bundle extents/slices/declared-scalar schema to anyone
    // who could reach the viewer listener. The viewer plane's bearer is a session token (this
    // module's doc), so a valid, unexpired session is required here exactly as for `/v1/viewport`.
    let token = bearer_token(&headers).ok_or(ApiError::BadCredential)?;
    state.authenticated_session(token)?;

    let meta = state.engine.meta();
    Ok(Json(serde_json::json!({
        "api_version": meta.api_version,
        "bundle_format": meta.bundle_format,
        "slices": meta.slices.iter().map(|(id, name)| serde_json::json!({"id": id, "display_name": name})).collect::<Vec<_>>(),
        "quantisation": {
            "x_min": meta.quantisation.x_min,
            "x_max": meta.quantisation.x_max,
            "y_min": meta.quantisation.y_min,
            "y_max": meta.quantisation.y_max,
        },
        "declared_scalars": meta.declared_scalars.iter().map(|s| serde_json::json!({"name": s.name, "arrow_type": s.arrow_type})).collect::<Vec<_>>(),
        // Reference Sheet R5: filter operand names are `[]` in Phase 1 (no filters — scope
        // constraint 11).
        "filter_operands": Vec::<String>::new(),
    })))
}

#[derive(Debug, Deserialize)]
struct ViewportReq {
    slice: String,
    zoom: u8,
    bbox: [f64; 4],
    #[serde(default)]
    k: Option<usize>,
    #[serde(default)]
    pin: Option<PinDto>,
}

async fn viewport(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<ViewportReq>,
) -> Result<Response, ApiError> {
    let token = bearer_token(&headers).ok_or(ApiError::BadCredential)?;
    let entry = state.authenticated_session(token)?;

    if req.bbox.iter().any(|v| !v.is_finite())
        || req.bbox[0] > req.bbox[2]
        || req.bbox[1] > req.bbox[3]
    {
        return Err(ApiError::Contract(
            "bbox must be [x0, y0, x1, y1] with x0 <= x1, y0 <= y1, all finite".to_string(),
        ));
    }
    if req.zoom > 16 {
        return Err(ApiError::Contract("zoom must be in 0..=16".to_string()));
    }

    let pin = req.pin.map(PinId::from);
    let k = req.k.unwrap_or(30).min(state.max_k);

    let out = state
        .engine
        .viewport(&entry.session, &req.slice, req.zoom, req.bbox, k, pin)
        .map_err(map_engine_error)?;

    let mut handles = entry.handles.lock();
    let n = out.points.len();
    let mut point_handles = Vec::with_capacity(n);
    let mut xs = Vec::with_capacity(n);
    let mut ys = Vec::with_capacity(n);
    for point in &out.points {
        // I10: the entity id leaves `tessera-engine` here and is translated to a per-session
        // opaque handle immediately — nothing downstream of this line ever sees it again.
        point_handles.push(handles.handle_for(point.entity_id).raw());
        xs.push(point.x);
        ys.push(point.y);
    }
    drop(handles);

    let meta = state.engine.meta();
    let scalar_names: Vec<String> = meta
        .declared_scalars
        .iter()
        .map(|d| d.name.clone())
        .collect();
    let scalar_cols = build_scalar_columns(&out.points, &scalar_names);
    let scalar_refs: Vec<(&str, ScalarColumn)> = scalar_cols
        .iter()
        .map(|(name, col)| (name.as_str(), col.as_ref()))
        .collect();

    let tiles: Vec<u64> = out.tiles.iter().map(|t| t.tile).collect();
    let visible: Vec<u64> = out.tiles.iter().map(|t| t.visible).collect();
    let matched: Vec<u64> = out.tiles.iter().map(|t| t.matched).collect();

    let bytes = viewport_ipc(
        &tiles,
        &visible,
        &matched,
        &point_handles,
        &xs,
        &ys,
        &scalar_refs,
    );

    let pin_header =
        serde_json::to_string(&PinDto::from(&out.pin)).expect("PinDto serialisation cannot fail");

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/octet-stream")
        .header("x-tessera-pin", pin_header)
        .body(Body::from(bytes))
        .expect("response construction cannot fail"))
}

/// A same-typed column of scalar values, owned so it outlives the borrow `viewport_ipc` needs.
enum ColumnBuf {
    U64(Vec<u64>),
    F32(Vec<f32>),
    Utf8(Vec<String>),
}

impl ColumnBuf {
    fn as_ref(&self) -> ScalarColumn<'_> {
        match self {
            ColumnBuf::U64(v) => ScalarColumn::U64(v),
            ColumnBuf::F32(v) => ScalarColumn::F32(v),
            ColumnBuf::Utf8(v) => ScalarColumn::Utf8(v),
        }
    }
}

/// Transpose each point's `Vec<ScalarOut>` (row-major, per `tessera_engine::viewport`'s doc) into
/// column-major buffers named from the bundle's declared-scalar schema. Phase 1's build always
/// writes every declared scalar for every row, so this alignment-by-position holds; if it didn't,
/// there is nothing authorisation-relevant at stake in getting a name wrong here (scalars are
/// disclosed to a viewer only after the mask has already admitted the row).
fn build_scalar_columns(
    points: &[tessera_engine::PointOut],
    declared_names: &[String],
) -> Vec<(String, ColumnBuf)> {
    let Some(first) = points.first() else {
        return Vec::new();
    };
    let n_scalars = first.scalars.len();
    let mut columns = Vec::with_capacity(n_scalars);
    for i in 0..n_scalars {
        let name = declared_names
            .get(i)
            .cloned()
            .unwrap_or_else(|| format!("scalar_{i}"));
        let buf = match &first.scalars[i] {
            tessera_engine::ScalarOut::U64(_) => ColumnBuf::U64(
                points
                    .iter()
                    .map(|p| match p.scalars[i] {
                        tessera_engine::ScalarOut::U64(v) => v,
                        _ => 0,
                    })
                    .collect(),
            ),
            tessera_engine::ScalarOut::F32(_) => ColumnBuf::F32(
                points
                    .iter()
                    .map(|p| match p.scalars[i] {
                        tessera_engine::ScalarOut::F32(v) => v,
                        _ => 0.0,
                    })
                    .collect(),
            ),
            tessera_engine::ScalarOut::Utf8(_) => ColumnBuf::Utf8(
                points
                    .iter()
                    .map(|p| match &p.scalars[i] {
                        tessera_engine::ScalarOut::Utf8(v) => v.clone(),
                        _ => String::new(),
                    })
                    .collect(),
            ),
        };
        columns.push((name, buf));
    }
    columns
}

#[derive(Debug, Deserialize)]
struct ItemReq {
    #[allow(dead_code)]
    #[serde(default)]
    pin: Option<PinDto>,
}

#[derive(Debug, Serialize)]
struct ItemResp {
    scalars: Vec<serde_json::Value>,
}

async fn item(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(handle_raw): AxumPath<u32>,
    Json(_req): Json<ItemReq>,
) -> Result<Json<ItemResp>, ApiError> {
    let token = bearer_token(&headers).ok_or(ApiError::BadCredential)?;
    let entry = state.authenticated_session(token)?;

    let handle = Handle::new(handle_raw);
    let entity = entry
        .handles
        .lock()
        .entity_of(handle)
        .ok_or_else(|| ApiError::Unknown("unknown handle".to_string()))?;

    let scalars = state
        .engine
        .item(&entry.session, entity)
        .ok_or_else(|| ApiError::Unknown("item not found or not visible".to_string()))?;

    let scalars = scalars
        .into_iter()
        .map(|s| match s {
            tessera_engine::ScalarOut::U64(v) => serde_json::json!(v),
            tessera_engine::ScalarOut::F32(v) => serde_json::json!(v),
            tessera_engine::ScalarOut::Utf8(v) => serde_json::json!(v),
        })
        .collect();

    Ok(Json(ItemResp { scalars }))
}
