//! The viewer plane (R5): `GET /v1/meta`, `POST /v1/viewport`, `POST /v1/items/{tessera_id}`,
//! plus `/healthz`/`/readyz`. Bearer auth is a session token (`Session::token`), minted by the
//! session plane's `/session/authorise`.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use serde::{Deserialize, Serialize};

use tessera_types::{GenerationStamp, TesseraId};
use tessera_wire::{viewport_ipc, ScalarColumn, ViewportColumns};

use tessera_engine::viewport::ViewportRequest;
use tessera_engine::CancelToken;

use crate::error::{map_engine_error, map_join_error, ApiError};
use crate::health::{healthz, readyz};
use crate::state::AppState;

pub fn router(state: Arc<AppState>) -> Router {
    // The dev-only browser seam: absent `serve.dev_cors_origins` mounts nothing, so the seam is
    // structurally absent from this router rather than present and configured empty.
    let dev_cors = crate::cors::dev_layer(&state.dev_cors_origins);
    let router = Router::new()
        .route("/v1/meta", get(meta))
        .route("/v1/viewport", post(viewport))
        .route("/v1/items/{tessera_id}", post(item))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .with_state(state);
    match dev_cors {
        Some(layer) => router.layer(layer),
        None => router,
    }
}

/// The wire shape of a generation stamp, both in `POST /v1/viewport`'s request body and the
/// `x-tessera-pin` response header (JSON either way — the header carries the same shape as a plain
/// string so a client can round-trip it without inventing its own encoding).
///
/// **The header keeps its name and loses its meaning** (contracts §3.1, §3.2). It used to select
/// geometry: presenting a superseded one was a `410 pin-expired`, and the server retained
/// superseded generations so it could be honoured. It is now a stamp — the client echoes back what
/// it is holding, the server answers from live geometry regardless, and the only effect is the
/// `stale` flag on the response (`geometry-pinning.md` §7). The name is kept because renaming a
/// header is a client-visible break that buys nothing; the meaning change is in the contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PinDto {
    prefix: String,
    segments_version: u64,
}

impl From<PinDto> for GenerationStamp {
    fn from(p: PinDto) -> Self {
        GenerationStamp {
            prefix: p.prefix,
            segments_version: p.segments_version,
        }
    }
}

impl From<&GenerationStamp> for PinDto {
    fn from(p: &GenerationStamp) -> Self {
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

/// Flips a [`CancelToken`] on drop. Created before the admission-gate acquire so it lives
/// for the whole `viewport` handler body, and held as a local there — axum dropping the handler's
/// future (the only signal this transport gives for "the client went away": there is no explicit
/// disconnect callback) drops this guard too, which is what flips the flag the `spawn_blocking`
/// closure's engine call is polling. Only a *clone* of the token moves into that closure; this
/// guard keeps the original.
///
/// **Disarmed on the normal path**, just before the handler constructs its response, so a
/// completed request's own guard drop (at function return) does not flip a token nobody is
/// reading any more. Flipping it late would in fact be harmless — the engine call has already
/// returned by the time this guard would drop on that path — but disarming keeps "cancelled"
/// meaning what it says: this request was cut short, not merely finished.
struct CancelGuard {
    token: CancelToken,
    armed: bool,
}

impl CancelGuard {
    fn new(token: CancelToken) -> Self {
        CancelGuard { token, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        if self.armed {
            self.token.cancel();
        }
    }
}

async fn meta(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Authenticated like every other route on this plane (R5: bearer auth on every plane). It is
    // not a public endpoint: it discloses the bundle's extents, slices and declared-scalar schema,
    // so an unauthenticated `/v1/meta` would hand the corpus shape to anyone who can reach the
    // viewer listener. The bearer here is a session token, so a valid, unexpired session is
    // required exactly as for `/v1/viewport`.
    let token = bearer_token(&headers).ok_or(ApiError::BadCredential)?;
    state.authenticated_session(token)?;

    let meta = state.engine.meta();
    let selection = state.engine.config();
    Ok(Json(serde_json::json!({
        "api_version": meta.api_version,
        "bundle_format": meta.bundle_format,
        // contracts §2.2/§2.6: the idset. The identity KEY never appears
        // in any response, log line or metric label (I10, Appendix C C17) -- this is the idset
        // only, which is meaningless without the key and is what `POST /v1/items` checks against.
        "idset": meta.idset,
        "slices": meta.slices.iter().map(|(id, name)| serde_json::json!({"id": id, "display_name": name})).collect::<Vec<_>>(),
        "quantisation": {
            "x_min": meta.quantisation.x_min,
            "x_max": meta.quantisation.x_max,
            "y_min": meta.quantisation.y_min,
            "y_max": meta.quantisation.y_max,
        },
        "declared_scalars": meta.declared_scalars.iter().map(|s| serde_json::json!({"name": s.name, "arrow_type": s.arrow_type.arrow_type_name()})).collect::<Vec<_>>(),
        // Reference Sheet R5: the filter operand names a client may use.
        // ⊘ Specified, not implemented: no filter contract exists yet, so this is always `[]` and a
        // client must not read an empty list as "this deployment declined to expose its filters".
        "filter_operands": Vec::<String>::new(),
        // §7.2's selection constants. A client cannot read mark count as density without knowing
        // where the floor and the cap sit, so these are a genuine client need rather than test
        // convenience -- and the reference oracle cannot reproduce the definition without them.
        //
        // They disclose nothing. All of them are deployment constants, identical for every
        // principal. Publishing `theta_target_marks` lets a client solve for theta's anchor, which
        // is the composed cardinality of its OWN mask over the whole slice -- precisely what a
        // `zoom = 0`, full-bbox request already returns as `visible` in a single call (§7.1).
        // Already obtainable, exactly.
        //
        // `max_k` is published for the same reason. Contracts §3.2 tells the client the effective
        // cap is `min(k, max_k, k_max_marks)`, so publishing only one of the two ceilings would
        // leave a client unable to learn its own request bound, and an independent implementation
        // unable to tell a **cap-clause** refusal from a **machine-ceiling** one. That distinction
        // is exactly the one §7.2 insists on keeping -- the overplot ceiling and the machine
        // ceiling are deliberately not the same knob, because raising the machine ceiling on
        // transport evidence must not silently dissolve §7.2's cap clause. A client that cannot see
        // both cannot honour it either.
        "selection": {
            "k_min": selection.k_min,
            "k_max_marks": selection.k_max_marks,
            "max_k": state.max_k,
            "theta_target_marks": selection.theta_target_marks,
            "max_underlay_offset": selection.max_underlay_offset,
            // Published for exactly the reason `max_k` is: a client
            // that chooses its own request *depth* — the mark-budget work — is choosing a tile
            // count, and without this it cannot tell whether a refusal was its own arithmetic or
            // the deployment's ceiling. It discloses nothing: a deployment constant, identical for
            // every principal, and the tile grid is public.
            "max_tiles_per_request": selection.max_tiles_per_request,
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
/// already framed by `viewport_ipc`, the stamp to echo in `x-tessera-pin`, whether the client's own
/// stamp is stale, and the timing figures
/// `x-tessera-stage-ns` needs — computed inside the closure since they describe work done there
/// (`arrow_serialise_ns`) or by the engine call it wraps (`timings`). Response/header
/// construction is deliberately NOT here: that stays on the reactor, since it neither blocks nor
/// costs measurable CPU.
struct ViewportOutcome {
    bytes: Vec<u8>,
    stamp: GenerationStamp,
    stale: bool,
    timings: tessera_engine::StageTimings,
    arrow_serialise_ns: u64,
}

/// The engine call through Arrow IPC framing: everything CPU-bound or file-IO-bearing in this
/// handler, run inside `spawn_blocking`. Takes `&AppState`/`&Session` by reference —
/// the caller owns both as `'static` values moved into the closure, so a reference borrowed for
/// the closure's own body lifetime is all this needs.
fn run_viewport(
    state: &AppState,
    session: &tessera_engine::Session,
    req: ViewportReq,
    cancel: CancelToken,
) -> Result<ViewportOutcome, ApiError> {
    let stamp = req.pin.map(GenerationStamp::from);
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
                .stamp(stamp)
                .underlay_offset(req.underlay_offset)
                .cancel(Some(cancel)),
        )
        .map_err(map_engine_error)?;

    let n = out.points.len();
    let mut point_ids = Vec::with_capacity(n);
    let mut codes = Vec::with_capacity(n);
    for point in &out.points {
        // I10 (entity ids never cross the trust boundary) is upheld structurally: no entity id is
        // available to leak here, because the engine never gathers one on this path (see
        // `tessera_engine::viewport::PointOut`'s doc). The wire identity is `tessera_id` directly,
        // carried through unchanged; there is no per-session translation left to do
        // (`tessera-wire`'s `HandleTable` is retained for node handles, which are not on this path
        // — docs/decisions/0032-delete-the-dead-handle-table.md).
        point_ids.push(point.tessera_id.raw());
        codes.push(point.code);
    }

    // Names come from `out.scalar_names`, populated by `Engine::viewport` from the SAME
    // generation it already loaded for this request — not a second `state.engine.meta()` call.
    // That second call would `load_full()` the generation pointer again, against lifecycle
    // §1.1's "exactly once, at request start"; the names are identical either way (same
    // manifest, same order), so this changes no response byte.
    let scalar_cols = build_scalar_columns(&out.points, &out.scalar_names);
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
        codes: &codes,
        scalars: &scalar_refs,
        sub_cells: sub_cells
            .as_ref()
            .map(|(cells, counts)| (cells.as_slice(), counts.as_slice())),
    });
    let arrow_serialise_ns = serialise_start.elapsed().as_nanos() as u64;

    Ok(ViewportOutcome {
        bytes,
        stamp: out.stamp,
        stale: out.stale,
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

    // The cancellation token and its drop-guard, created before the admission-gate acquire below
    // so the guard's lifetime spans the whole handler — a disconnect during the queue
    // wait is already free (dropping the `admit().await` future releases nothing that was ever
    // acquired), but creating the guard here rather than after admission keeps one token identity
    // for the entire request and costs nothing extra.
    let cancel = CancelToken::new();
    let mut cancel_guard = CancelGuard::new(cancel.clone());

    // The two-stage admission gate. `admit()` sheds with `ApiError::Backpressure` (429) if the
    // outer slots semaphore has no permit to `try_acquire`, or if the inner compute semaphore does
    // not free one within `admission_timeout_ms`. `admission_us` is the queue wait, reported below
    // as `x-tessera-admission-us`.
    let (gate_permits, admission_us) = state.compute_gate.admit().await?;

    // Server-side timing, for the latency gate `scripts/bench_p99.py` checks. Not a wire-format
    // field — an observability-only response header, reported to microseconds so the gate can be
    // checked without relying on end-to-end (client-observed) latency, which also includes
    // HTTP/TCP/loopback overhead outside the engine's control.
    //
    // **The clock starts AFTER admission**, so `x-tessera-server-us` means "server compute,
    // excluding queueing"; bench baselines read it that way. `start` is nonetheless taken before
    // `spawn_blocking`, so a blocking-pool scheduling wait lands inside `server_us` rather than in
    // `x-tessera-admission-us`. The gate bounds that wait rather than removing it: at most
    // `compute_admission` closures are admitted at a time, far under the blocking pool's
    // 512-thread size, so in practice it is ~0 and this header is effectively compute-only.
    // `admission_us` carries only the gate wait itself (`admit()`'s own two-stage acquire).
    let start = std::time::Instant::now();

    // The engine call through Arrow IPC framing is CPU-bound (and, on a cold row-projection
    // or fragment build, file-IO-bearing) with no `.await` of its own — run synchronously here it
    // would monopolise this reactor thread for the whole viewport, starving every other request
    // sharing this process's tokio worker threads, `/healthz` included. `spawn_blocking` moves it
    // to tokio's blocking-thread pool instead.
    //
    // Closure capture: `state` is a cloned `Arc<AppState>` (cheap; `Engine: Send + Sync` is what
    // makes this sound), `entry` is the already-cloned `Arc<SessionEntry>`
    // `authenticated_session` returned, `req` is moved in whole (its fields were only ever
    // borrowed above), and `gate_permits` moves in so both permits release
    // only when this closure returns — correct accounting even if the client has disconnected.
    // Only a *clone* of `cancel` moves in — `cancel_guard` keeps the original outside the
    // closure, on the reactor, where a client disconnect can flip it. If the client disconnects
    // (axum drops this whole handler future), `cancel_guard` drops and flips the flag; the engine
    // call inside the closure observes it at its next checkpoint and returns
    // `Err(EngineError::Cancelled)`, so the closure itself returns `Err` and `_gate_permits` drops
    // — releasing both `OwnedSemaphorePermit`s well before the closure would otherwise have run to
    // completion.
    let closure_state = Arc::clone(&state);
    let closure_cancel = cancel.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let _gate_permits = gate_permits;
        run_viewport(&closure_state, &entry.session, req, closure_cancel)
    })
    .await
    .map_err(map_join_error)??;

    // Normal path reached — disarm the guard so its own drop (at this function's return,
    // whichever branch below) does not pointlessly flip a token nobody downstream is reading any
    // more. See `CancelGuard`'s doc for why leaving it armed here would be harmless, not merely
    // wrong-looking.
    cancel_guard.disarm();

    let pin_header = serde_json::to_string(&PinDto::from(&outcome.stamp))
        .expect("PinDto serialisation cannot fail");
    let server_us = start.elapsed().as_micros().to_string();

    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/octet-stream")
        .header("x-tessera-pin", pin_header)
        // The staleness signal (`geometry-pinning.md` §7). A header rather than a body field
        // because the body is Arrow IPC and this is one bit that every client — including one that
        // only reads counts — should be able to see without decoding a batch. Always present, so a
        // client never has to distinguish "fresh" from "the server did not say".
        .header("x-tessera-stale", if outcome.stale { "1" } else { "0" })
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
///
/// **The per-tile fields are CPU-time sums, not a partition of wall clock.** Once
/// `serve.compute_threads > 1`, `count_ns`, `select_ns`, `gather_ns`, `underlay_ns` and the row
/// counters alongside them are summed across the workers of the parallel tile sweep — see
/// `tessera_engine::StageTimings`'s doc for the full reasoning. A consumer summing this row's
/// duration fields and comparing the total against `x-tessera-server-us` will see the sum run
/// *ahead* of wall time under real parallelism, by roughly the achieved concurrency: that is
/// correct, not a discrepancy to chase. `arrow_serialise_ns` is unaffected — response assembly
/// (this handler) stays serial regardless of the engine's `compute_threads`.
///
/// **That only holds above the calibrated serial fallback.** Below
/// `tessera_engine::viewport::SERIAL_FALLBACK_MAX_ROWS` the tile sweep folds serially at ANY
/// `compute_threads` value — the `pool.install` fan-out does not run at all for those requests —
/// so below that line these per-tile fields do partition the request's own wall clock.
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
        t.stamp_compare_ns,
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
    Bool(Vec<bool>),
    U8(Vec<u8>),
    U16(Vec<u16>),
    U32(Vec<u32>),
    U64(Vec<u64>),
    I8(Vec<i8>),
    I16(Vec<i16>),
    I32(Vec<i32>),
    I64(Vec<i64>),
    F32(Vec<f32>),
    F64(Vec<f64>),
    TimestampUs(Vec<i64>),
    Utf8(Vec<String>),
}

/// The scalar families, once, for the three places this module walks them: the owned buffer's
/// borrow, the row-to-column transpose, and the drill-down's JSON. Three separate matches over
/// thirteen variants is three chances for a type to appear in two of them.
macro_rules! scalar_families {
    ($mac:ident) => {
        $mac! {
            Bool, U8, U16, U32, U64, I8, I16, I32, I64, F32, F64, TimestampUs,
        }
    };
}

impl ColumnBuf {
    fn as_ref(&self) -> ScalarColumn<'_> {
        macro_rules! arms {
            ($($v:ident),* $(,)?) => {
                match self {
                    $(ColumnBuf::$v(x) => ScalarColumn::$v(x),)*
                    ColumnBuf::Utf8(x) => ScalarColumn::Utf8(x),
                }
            };
        }
        scalar_families!(arms)
    }
}

/// Transpose each point's `Vec<ScalarOut>` (row-major, per `tessera_engine::viewport`'s doc) into
/// column-major buffers named from the bundle's declared-scalar schema. The build always writes
/// every declared scalar for every row, so this alignment-by-position holds; if it didn't, there is
/// nothing authorisation-relevant at stake in getting a name wrong here (scalars are disclosed to a
/// viewer only after the mask has already admitted the row).
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
        // One arm per type, generated: the hand-written form was three near-identical blocks and
        // is thirteen now, which is the shape a type added to twelve of them hides in.
        //
        // The `_ => default` fallback is unreachable — a column is one type for every row, the
        // segment schema having fixed it — and stays a default rather than a panic because a
        // response is not the place to discover a bundle defect, and nothing here is
        // authorisation-relevant: the mask admitted the row before its scalars were read.
        macro_rules! column_of {
            ($variant:ident, $default:expr) => {
                ColumnBuf::$variant(
                    points
                        .iter()
                        .map(|p| match &p.scalars[i] {
                            tessera_engine::ScalarOut::$variant(v) => v.clone(),
                            _ => $default,
                        })
                        .collect(),
                )
            };
        }
        let buf = match &first.scalars[i] {
            tessera_engine::ScalarOut::Bool(_) => column_of!(Bool, false),
            tessera_engine::ScalarOut::U8(_) => column_of!(U8, 0),
            tessera_engine::ScalarOut::U16(_) => column_of!(U16, 0),
            tessera_engine::ScalarOut::U32(_) => column_of!(U32, 0),
            tessera_engine::ScalarOut::U64(_) => column_of!(U64, 0),
            tessera_engine::ScalarOut::I8(_) => column_of!(I8, 0),
            tessera_engine::ScalarOut::I16(_) => column_of!(I16, 0),
            tessera_engine::ScalarOut::I32(_) => column_of!(I32, 0),
            tessera_engine::ScalarOut::I64(_) => column_of!(I64, 0),
            tessera_engine::ScalarOut::F32(_) => column_of!(F32, 0.0),
            tessera_engine::ScalarOut::F64(_) => column_of!(F64, 0.0),
            tessera_engine::ScalarOut::TimestampUs(_) => column_of!(TimestampUs, 0),
            tessera_engine::ScalarOut::Utf8(_) => column_of!(Utf8, String::new()),
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
    /// Optional (contracts §2.2/§2.6): the durable identifier is `external_id`,
    /// so a conforming consumer has no stale `tessera_id` to present in the first place, and
    /// rotation/repartitioning are deliberate breaking changes rather than scheduled hygiene. A
    /// caller that omits this accepts that a `tessera_id` from a past idset may now name a
    /// different item after a repartitioning.
    #[serde(default)]
    idset: Option<u32>,
}

#[derive(Debug, Serialize)]
struct ItemResp {
    scalars: Vec<serde_json::Value>,
    /// Base64, present only when the item has a caller-supplied external id. This is the only
    /// place a caller external id appears on the viewer plane — the conformance byte-scanner's
    /// viewer-plane sweep must be scoped to exclude this endpoint's response.
    #[serde(skip_serializing_if = "Option::is_none")]
    external_id: Option<String>,
}

/// `engine.item`'s sidecar read plus the scalar/external-id shaping that follows it — the CPU-bound
/// and file-IO-bearing part of `/v1/items/{tessera_id}`, run inside `spawn_blocking`.
///
/// ## The three ordering rules this arm structure encodes
///
/// **The idset check runs before inversion and is entity-independent** — identical work and an
/// identical `409` for every presented `tessera_id`, so it opens no channel (contracts §2.2, and C4
/// in the architecture's leak register). It runs *inside* `Engine::item`, against the same
/// generation snapshot that call already loads for the lookup that follows, rather than against a
/// separate `state.engine.meta()` ahead of it: lifecycle §1.1 requires exactly one
/// `generation.load_full()` per request, and a second one for a request that is nominally one
/// lookup breaks it. See [`tessera_engine::Engine::item`]'s doc for the full argument.
///
/// **`404 unknown` is returned identically** for "no such id" and "exists but is not visible to
/// this principal" (contracts §3.2): one `Ok(None)` arm, one `ApiError::Unknown` construction, no
/// branch-dependent logging or metrics anywhere on this path — a second construction site with a
/// different detail string, or a `tracing`/metric call inside only one of the two `None`-shaped
/// cases, would be exactly the oracle this rule exists to prevent.
///
/// **The store-backed `Err` arm can never be reached by anything an attacker chooses.**
/// `Engine::item` inverts `id` (a pure function, no I/O) and tests visibility in entity space — the
/// *same* O(1) work for an id naming nothing and an id naming an invisible item — before it ever
/// touches the external-ID sidecar. A store/IO failure can therefore only be raised for an item
/// already established visible, so a probing client can see a `500` only for an item it can already
/// see; it can never use `500` vs `404` to learn whether an id exists. **A future edit that moves
/// the sidecar read earlier than the visibility test would silently turn this status into a
/// visibility oracle — don't.**
fn run_item(
    state: &AppState,
    session: &tessera_engine::Session,
    raw: u64,
    idset: Option<u32>,
) -> Result<ItemResp, ApiError> {
    let item = match state.engine.item(session, TesseraId::new(raw), idset) {
        // `EngineError::StaleIdSet` -> 409, same fixed detail string as before this was
        // moved inside `Engine::item`. A corrupt or unreadable sidecar (`Store`/`Io`) is a SERVER
        // fault, not "no such item" -- `.ok().flatten()` here would serve a 200 with
        // `external_id: null` and call a digest mismatch a missing field -- fail-open, and
        // precisely what the typed error surface exists to prevent. See this function's doc for
        // why the store-backed arm is unreachable by identifier choice.
        Err(e) => return Err(map_engine_error(e)),
        // Identical 404 for "no such ID" and "exists but not visible". ONE arm, one
        // message, no branch above it -- a second construction site with a different detail
        // string would be the oracle this rule prevents.
        Ok(None) => return Err(ApiError::Unknown("unknown".to_string())),
        Ok(Some(item)) => item,
    };

    let scalars = item
        .scalars
        .into_iter()
        // Every width lands on a JSON number; the drill-down response is a presentation of the
        // value, not of its storage width, and a client reading `severity: 3` should not have to
        // know the column is a `u8`. The width is a residency decision (per-point-attributes
        // §3.6), and `/v1/meta` publishes it for a client that does care.
        .map(|s| {
            macro_rules! arms {
                ($($v:ident),* $(,)?) => {
                    match s {
                        $(tessera_engine::ScalarOut::$v(v) => serde_json::json!(v),)*
                        tessera_engine::ScalarOut::Utf8(v) => serde_json::json!(v),
                    }
                };
            }
            scalar_families!(arms)
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

    // Gated the same way as `/v1/viewport` (see its handler's comment) — `admit()` sheds with 429
    // `backpressure` on either stage.
    let (gate_permits, _admission_us) = state.compute_gate.admit().await?;

    // `engine.item` checks `req.idset` (if the caller sent one) against the ONE generation it
    // loads, inverts the id (pure, no IO), then reads the external-id sidecar for a visible item —
    // file IO, moved off the reactor.
    //
    // The idset check is inside that call rather than here on the reactor ahead of `admit()`,
    // because checking it here would need a second, independent `generation.load_full()` via
    // `state.engine.meta()`, against lifecycle §1.1's one-load-per-request rule. The cost of
    // keeping the rule is that a stale-idset request holds a gate permit for the length of the
    // `spawn_blocking` call rather than being rejected before `admit()` runs; see `Engine::item`'s
    // doc for the full argument.
    //
    // Closure capture: `state` moved in directly (nothing after this `.await` needs the handler's
    // own copy), `entry` moved (already an `Arc<SessionEntry>`), `raw`/`req.idset` are `Copy`,
    // `gate_permits` moves in so both permits release only when this closure returns.
    let resp = tokio::task::spawn_blocking(move || {
        let _gate_permits = gate_permits;
        run_item(&state, &entry.session, raw, req.idset)
    })
    .await
    .map_err(map_join_error)??;

    Ok(Json(resp))
}
