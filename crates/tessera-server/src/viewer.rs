//! The viewer plane (R5): `GET /v1/meta`, `POST /v1/viewport`, `POST /v1/items/{tessera_id}`,
//! plus `/healthz`/`/readyz`. Bearer auth is a session token (Task 11's `Session::token`).

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use serde::{Deserialize, Serialize};

use tessera_types::{PinId, TesseraId};
use tessera_wire::{viewport_ipc, ScalarColumn, ViewportColumns};

use tessera_engine::viewport::ViewportRequest;

use crate::error::{map_engine_error, map_join_error, map_store_error, ApiError};
use crate::health::{healthz, readyz};
use crate::state::AppState;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/meta", get(meta))
        .route("/v1/viewport", post(viewport))
        .route("/v1/items/{tessera_id}", post(item))
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
    let selection = state.engine.config();
    Ok(Json(serde_json::json!({
        "api_version": meta.api_version,
        "bundle_format": meta.bundle_format,
        // contracts §2.2/§2.6 r6: the transport-identity epoch. The identity KEY never appears
        // in any response, log line or metric label (I10, Appendix C C17) -- this is the epoch
        // only, which is meaningless without the key and is what `POST /v1/items` checks against.
        "identity_epoch": meta.identity_epoch,
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
        // §7.2's selection constants. A client cannot read mark count as density without knowing
        // where the floor and the cap sit, so these are a genuine client need rather than test
        // convenience -- and the reference oracle cannot reproduce the definition without them.
        //
        // They disclose nothing. All three are deployment constants, identical for every principal.
        // Publishing `theta_target_marks` lets a client solve for theta's anchor, which is the
        // composed cardinality of its OWN mask over the whole slice -- precisely what a `zoom = 0`,
        // full-bbox request already returns as `visible` in a single call (§7.1). Already
        // obtainable, exactly.
        "selection": {
            "k_min": selection.k_min,
            "k_max_marks": selection.k_max_marks,
            "theta_target_marks": selection.theta_target_marks,
            "max_underlay_offset": selection.max_underlay_offset,
        },
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
    /// §3.3 density underlay: serve exact masked counts at depth `zoom + underlay_offset`. Absent
    /// or `0` means no underlay and no extra bytes.
    ///
    /// Rejected, never clamped, on all three bounds (configured maximum, the depth-16 grid limit,
    /// and the total sub-cell budget) — a Morton prefix carries no depth of its own, so a silently
    /// reduced offset would hand back cells the client could not interpret.
    #[serde(default)]
    underlay_offset: Option<u8>,
}

/// Everything a `spawn_blocking` viewport closure hands back to the async side: the wire bytes
/// already framed by `viewport_ipc`, the pin to echo in `x-tessera-pin`, and the timing figures
/// `x-tessera-stage-ns` needs — computed inside the closure since they describe work done there
/// (`arrow_serialise_ns`) or by the engine call it wraps (`timings`). Response/header
/// construction is deliberately NOT here (D-A): that stays on the reactor.
struct ViewportOutcome {
    bytes: Vec<u8>,
    pin: PinId,
    timings: tessera_engine::StageTimings,
    arrow_serialise_ns: u64,
}

/// The engine call through Arrow IPC framing (D-A scope for this handler): everything CPU-bound
/// or file-IO-bearing, run inside `spawn_blocking`. Takes `&AppState`/`&Session` by reference —
/// the caller owns both as `'static` values moved into the closure, so a reference borrowed for
/// the closure's own body lifetime is all this needs.
fn run_viewport(
    state: &AppState,
    session: &tessera_engine::Session,
    req: ViewportReq,
) -> Result<ViewportOutcome, ApiError> {
    let pin = req.pin.map(PinId::from);
    // Contracts §3.2's default. It is the deployment's own overplot ceiling rather than a
    // literal, so a client that expresses no preference gets the full budget this deployment will
    // serve and §7.2's proportional window is realised in full — at the old default of 30 against a
    // cap of 128 the window was 15 rather than 64, i.e. the default silently threw away most of the
    // density range the parameters were chosen for.
    let k = req
        .k
        .unwrap_or_else(|| state.engine.config().k_max_marks)
        .min(state.max_k);

    let out = state
        .engine
        .viewport(
            session,
            ViewportRequest::new(&req.slice, req.zoom, req.bbox, k)
                .pin(pin)
                .underlay_offset(req.underlay_offset),
        )
        .map_err(map_engine_error)?;

    let n = out.points.len();
    let mut point_ids = Vec::with_capacity(n);
    let mut xs = Vec::with_capacity(n);
    let mut ys = Vec::with_capacity(n);
    for point in &out.points {
        // I10, strengthened (contracts r6): no entity id is available to leak here — the engine
        // never gathers one on this path (see `tessera_engine::viewport::PointOut`'s doc). The
        // wire identity is `tessera_id` directly, carried through unchanged; there is no
        // per-session translation left to do (`tessera-wire`'s `HandleTable` is retained for
        // Phase 3's node handles, not this path — see its module doc).
        point_ids.push(point.tessera_id.raw());
        xs.push(point.x);
        ys.push(point.y);
    }

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
    let served: Vec<u64> = out.tiles.iter().map(|t| t.served).collect();

    // Absent, not empty, when the underlay was not requested: `viewport_ipc` emits zero trailing
    // bytes for `None`, which is what keeps the default payload byte-identical to the pre-underlay
    // format (see `tessera_wire::payload`'s module doc).
    let sub_cells: Option<(Vec<u64>, Vec<u64>)> =
        req.underlay_offset.filter(|&o| o > 0).map(|_| {
            (
                out.sub_cells.iter().map(|c| c.cell).collect(),
                out.sub_cells.iter().map(|c| c.count).collect(),
            )
        });

    // Everything from the engine's return to here is response assembly, not serialisation; the
    // engine's own breakdown stops at its last gather. Start the serialise clock at the call.
    let serialise_start = std::time::Instant::now();
    let bytes = viewport_ipc(&ViewportColumns {
        tile: &tiles,
        visible: &visible,
        matched: &matched,
        served: &served,
        points_tessera_ids: &point_ids,
        xs: &xs,
        ys: &ys,
        scalars: &scalar_refs,
        sub_cells: sub_cells
            .as_ref()
            .map(|(cells, counts)| (cells.as_slice(), counts.as_slice())),
    });
    let arrow_serialise_ns = serialise_start.elapsed().as_nanos() as u64;

    Ok(ViewportOutcome {
        bytes,
        pin: out.pin,
        timings: out.timings,
        arrow_serialise_ns,
    })
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

    // D-B: the two-stage admission gate. `admit()` sheds with `ApiError::Backpressure` (429) if
    // the outer slots semaphore has no permit to `try_acquire`, or if the inner compute semaphore
    // does not free one within `admission_timeout_ms`. `admission_us` is the queue wait —
    // `x-tessera-admission-us` below (D-E).
    let (gate_permits, admission_us) = state.compute_gate.admit().await?;

    // Task 16: server-side timing for the exit-criteria measurement (bench_p99.py, plan §5).
    // Not a wire-format field — an observability-only response header, reported to microseconds
    // so the <10ms exit gate can be checked without relying on end-to-end (client-observed)
    // latency, which also includes HTTP/TCP/loopback overhead outside the engine's control.
    //
    // D-E: this clock starts AFTER admission, so it keeps its pre-Task-4 meaning of "server
    // compute, excluding queueing" — bench baselines and the <10 ms exit gate both read it that
    // way. Note: pre-Task-4 (Task 3, D-A) the value could include a real blocking-pool scheduling
    // wait, because nothing bounded how many closures could be in flight on tokio's (512-thread)
    // blocking pool at once. `start` is still taken here, before `spawn_blocking` — a scheduling
    // wait still lands inside `server_us`, not `x-tessera-admission-us` — but the gate now BOUNDS
    // that wait rather than removing it from this measurement: at most `compute_admission`
    // closures are ever admitted at a time, far under the pool's size, so in practice the wait is
    // ~0 and this header is effectively compute-only again. `admission_us` carries only the gate
    // wait itself (`admit()`'s own two-stage acquire), never any blocking-pool scheduling delay.
    let start = std::time::Instant::now();

    // D-A: the engine call through Arrow IPC framing is CPU-bound (and, on a cold row-projection
    // or fragment build, file-IO-bearing) with no `.await` of its own — run synchronously here it
    // would monopolise this reactor thread for the whole viewport, starving every other request
    // sharing this process's tokio worker threads, `/healthz` included. `spawn_blocking` moves it
    // to tokio's blocking-thread pool instead.
    //
    // Closure capture: `state` is a cloned `Arc<AppState>` (cheap; `Engine: Send + Sync` is what
    // makes this sound — see this task's report), `entry` is the already-cloned
    // `Arc<SessionEntry>` `authenticated_session` returned, `req` is moved in whole (its fields
    // were only ever borrowed above), and `gate_permits` (D-B) moves in so both permits release
    // only when this closure returns — correct accounting even if the client has disconnected.
    let closure_state = Arc::clone(&state);
    let outcome = tokio::task::spawn_blocking(move || {
        let _gate_permits = gate_permits;
        run_viewport(&closure_state, &entry.session, req)
    })
    .await
    .map_err(map_join_error)??;

    let pin_header = serde_json::to_string(&PinDto::from(&outcome.pin))
        .expect("PinDto serialisation cannot fail");
    let server_us = start.elapsed().as_micros().to_string();

    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/octet-stream")
        .header("x-tessera-pin", pin_header)
        .header("x-tessera-server-us", server_us)
        .header("x-tessera-admission-us", admission_us.to_string());

    if state.stage_timing {
        if let Some(value) = stage_header(&outcome.timings, outcome.arrow_serialise_ns) {
            response = response.header("x-tessera-stage-ns", value);
        }
    }

    Ok(response
        .body(Body::from(outcome.bytes))
        .expect("response construction cannot fail"))
}

/// The `x-tessera-stage-ns` value: a fixed-order CSV of unsigned integers, no names.
///
/// **Returns `None` in a build without `bench-timing`**, so a config that turns `stage_timing` on
/// against a release binary emits nothing rather than a row of zeros that reads like a free
/// request path. That is the second of the two gates described on `Config::stage_timing`; the
/// first is the feature on the engine's `Probe`, which leaves every field at zero.
///
/// **I10 / SA §9.** Durations and row counts only — no entity id, no descriptor, no token, no
/// per-principal label. `sigma_visible` and the tile counts are already in the Arrow payload the
/// same response carries, so nothing here is reachable that was not already. What the header does
/// expose is the C4 timing channel in quantified form, which is the point: Appendix C leaves C4
/// open with "quantify before treating as acceptable", and this is the measurement. It is
/// nonetheless off by default and absent from release builds.
///
/// Field order is part of the contract with `scripts/bench_*.py` and `tessera-bench`; append
/// only, never reorder.
#[cfg(feature = "bench-timing")]
fn stage_header(t: &tessera_engine::StageTimings, arrow_serialise_ns: u64) -> Option<String> {
    // **Append-only.** This is a positional CSV, so inserting a field anywhere but the end silently
    // misaligns every existing consumer — the same reason `served` was appended to the tiles batch
    // rather than slotted next to `visible`. The three trailing fields (theta_anchor_ns,
    // underlay_ns, underlay_cells_evaluated) are therefore out of the durations-then-counters
    // grouping the rest follows; `tessera_bench::report::Stages` is JSON-by-name and keeps the
    // readable order.
    Some(format!(
        "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
        t.generation_resolve_ns,
        t.pin_resolve_ns,
        t.slice_lookup_ns,
        t.row_projection_ns,
        t.compose_ns,
        t.tiles_for_bbox_ns,
        t.tile_ranges_ns,
        t.count_ns,
        t.select_ns,
        t.gather_ns,
        arrow_serialise_ns,
        t.total_ns,
        t.tiles_resolved,
        t.tiles_nonempty,
        t.sigma_visible,
        t.rows_in_ranges,
        t.select_rows_visited,
        t.points_gathered,
        u64::from(t.row_projection_built),
        t.theta_anchor_ns,
        t.underlay_ns,
        t.underlay_cells_evaluated,
    ))
}

#[cfg(not(feature = "bench-timing"))]
fn stage_header(_t: &tessera_engine::StageTimings, _arrow_serialise_ns: u64) -> Option<String> {
    None
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
    /// Optional (contracts §2.2/§2.6 r6, owner ruling): the durable identifier is `external_id`,
    /// so a conforming consumer has no stale `tessera_id` to present in the first place, and
    /// rotation/repartitioning are deliberate breaking changes rather than scheduled hygiene. A
    /// caller that omits this accepts that a `tessera_id` from a past epoch may now name a
    /// different item after a repartitioning.
    #[serde(default)]
    epoch: Option<u32>,
}

#[derive(Debug, Serialize)]
struct ItemResp {
    scalars: Vec<serde_json::Value>,
    /// Base64, present only when the item has a caller-supplied external id. This is the only
    /// place a caller external id appears on the viewer plane (D4, D6) — the conformance
    /// byte-scanner's viewer-plane sweep must be scoped to exclude this endpoint's response.
    #[serde(skip_serializing_if = "Option::is_none")]
    external_id: Option<String>,
}

/// `POST /v1/items/{tessera_id}`.
///
/// **The epoch check runs before inversion and is entity-independent** — identical work and an
/// identical `409` for every presented `tessera_id`, so it opens no channel (contracts §2.2, C4).
///
/// **`404 unknown` is returned identically** for "no such id" and "exists but is not visible to
/// this principal" (owner ruling; contracts §3.2): one `Ok(None)` arm, one `ApiError::Unknown`
/// construction, no branch-dependent logging or metrics anywhere on this path — a second
/// construction site with a different detail string, or a `tracing`/metric call inside only one
/// of the two `None`-shaped cases, would be exactly the oracle this rule exists to prevent.
///
/// **The `Err` arm can never be reached by anything an attacker chooses.** `Engine::item` inverts
/// `id` (a pure function, no I/O) and tests visibility in entity space — the *same* O(1) work for
/// an id naming nothing and an id naming an invisible item (Critical C-5, closed not narrowed) —
/// before it ever touches the external-ID sidecar. `StoreError` can therefore only be raised for
/// an item already established visible, so a probing client can see a `500` only for an item it
/// can already see; it can never use `500` vs `404` to learn whether an id exists. **A future
/// edit that moves the sidecar read earlier than the visibility test would silently turn this
/// status into a visibility oracle — don't.**
/// `engine.item`'s sidecar read plus the scalar/external-id shaping that follows it (D-A scope
/// for this handler) — run inside `spawn_blocking`. See [`item`]'s doc for why the ordering
/// (visibility test before any sidecar touch) must not move.
fn run_item(
    state: &AppState,
    session: &tessera_engine::Session,
    raw: u64,
) -> Result<ItemResp, ApiError> {
    let item = match state.engine.item(session, TesseraId::new(raw)) {
        // A corrupt or unreadable sidecar is a SERVER fault, not "no such item". `.ok().flatten()`
        // here would serve a 200 with `external_id: null` and call a digest mismatch a missing
        // field -- fail-open, and precisely what Task 8's typed errors exist to prevent (Critical
        // N-3). See this function's doc for why this arm is unreachable by identifier choice.
        Err(e) => return Err(map_store_error(e)),
        // Owner ruling: identical 404 for "no such ID" and "exists but not visible". ONE arm, one
        // message, no branch above it -- a second construction site with a different detail
        // string would be the oracle this rule prevents.
        Ok(None) => return Err(ApiError::Unknown("unknown".to_string())),
        Ok(Some(item)) => item,
    };

    let scalars = item
        .scalars
        .into_iter()
        .map(|s| match s {
            tessera_engine::ScalarOut::U64(v) => serde_json::json!(v),
            tessera_engine::ScalarOut::F32(v) => serde_json::json!(v),
            tessera_engine::ScalarOut::Utf8(v) => serde_json::json!(v),
        })
        .collect();

    let external_id = item
        .external_id
        .map(|bytes| base64::engine::general_purpose::STANDARD.encode(bytes));

    Ok(ItemResp {
        scalars,
        external_id,
    })
}

async fn item(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(raw): AxumPath<u64>,
    Json(req): Json<ItemReq>,
) -> Result<Json<ItemResp>, ApiError> {
    let token = bearer_token(&headers).ok_or(ApiError::BadCredential)?;
    let entry = state.authenticated_session(token)?;

    // Checked HERE -- before inversion, and identically for every identifier, so it opens no
    // channel (contracts §2.2). Entity-independent: this branch does not depend on `raw` at all.
    // Stays on the reactor: a pure in-memory comparison against `meta()`, no engine mask work.
    if let Some(e) = req.epoch {
        if e != state.engine.meta().identity_epoch {
            return Err(ApiError::Conflict(
                "stale identity epoch; re-resolve by external_id".to_string(),
            ));
        }
    }

    // D-B: gated the same way as `/v1/viewport` (see its handler's comment) — `admit()` sheds
    // with 429 `backpressure` on either stage.
    let (gate_permits, _admission_us) = state.compute_gate.admit().await?;

    // D-A: `engine.item` inverts the id (pure, no IO) then reads the external-id sidecar for a
    // visible item — file IO, moved off the reactor. Closure capture: `state` cloned (`Arc`,
    // cheap), `entry` moved (already an `Arc<SessionEntry>`), `raw` is `Copy`, `gate_permits`
    // (D-B) moves in so both permits release only when this closure returns.
    let closure_state = Arc::clone(&state);
    let resp = tokio::task::spawn_blocking(move || {
        let _gate_permits = gate_permits;
        run_item(&closure_state, &entry.session, raw)
    })
    .await
    .map_err(map_join_error)??;

    Ok(Json(resp))
}
