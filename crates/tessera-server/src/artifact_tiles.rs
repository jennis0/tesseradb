//! `POST /v1/artifacts/viewport`: what things are here. The artifacts each requested layer serves
//! this viewer in each tile, one artifacts frame per tile in the request's order, after a frame of
//! the treed layers where they serve any, then a trailer.
//!
//! It runs under its own admission limit, `serve.artifact_admission`, on a blocking thread that
//! sends each tile's frame as it is walked, under the viewport's stall and deadline settings. A
//! client that goes away cancels the engine through the body's guard, which the engine checks
//! between tiles.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::response::Response;
use serde::Deserialize;

use tessera_engine::{
    ArtifactOut, CancelToken, ComputedSelection, SinkResult, ViewportArtifactsHead,
    ViewportArtifactsRequest, ViewportArtifactsSink,
};
use tessera_types::GenerationStamp;
use tessera_wire::{artifacts_frame, trailer_frame, ArtifactRow};

use crate::error::{map_engine_error, ApiError};
use crate::state::{ApiJson, AppState, GatePermits, ViewerSession};
use crate::stream::{CancelGuard, Producer};
use crate::viewer::{
    check_extent, distinct_tiles, layer_names, layer_selection, layers_named, level_numbers,
    level_selection, FilterParser, LayersReq, LevelsReq, PinDto,
};

/// The request body. An unknown field is a `422`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ViewportArtifactsReq {
    view: String,
    zoom: u8,
    /// Absent exactly when `tiles` is present.
    #[serde(default)]
    bbox: Option<[f64; 4]>,
    /// The depth-`zoom` Morton prefixes to answer for, in the order the frames follow.
    #[serde(default)]
    tiles: Option<Vec<u64>>,
    /// The layers to answer for, as the viewport names them. Absent or `[]` is none.
    #[serde(default)]
    layers: Option<LayersReq>,
    /// Which declared levels of each named layer, as the viewport names them.
    #[serde(default)]
    levels: Option<LevelsReq>,
    /// The most artifacts one level serves in one tile, at most `selection.max_artifacts_per_tile`.
    per_tile: usize,
    /// Which of `centroid` and `box` to serve, intersected with each layer's declaration; absent
    /// is the declaration's and `[]` is none.
    #[serde(default)]
    computed: Option<Vec<String>>,
    #[serde(default)]
    filters: Option<serde_json::Value>,
    #[serde(default)]
    highlight: Option<serde_json::Value>,
    #[serde(default)]
    pin: Option<PinDto>,
    /// The budget a treed layer is cut to, met by serving ancestors in place of descendants.
    #[serde(default)]
    budget: Option<u32>,
}

/// What the handler needs to answer, sent with the first frame.
struct Opening {
    head: ViewportArtifactsHead,
    first_frame: Vec<u8>,
    /// Microseconds from admission to the first frame, sent as `x-tessera-server-us`.
    server_us: u64,
}

/// The engine's sink: the head is held until the first frame, which opens the response, and every
/// later frame is sent into the body.
struct WireSink {
    head: Option<ViewportArtifactsHead>,
    producer: Producer<Opening>,
    /// Held until the response ends: the walk computes until its last tile.
    _permits: GatePermits,
    start: Instant,
    arrow_serialise_ns: u64,
    rows: u64,
    frames: u64,
}

impl WireSink {
    /// Send `frame`, opening the response with it where it is the first.
    fn send(&mut self, frame: Vec<u8>) -> SinkResult {
        if self.producer.is_open() {
            return self.producer.send(frame);
        }
        let head = self.head.take().expect("the head precedes every frame");
        self.producer.start_deadline();
        self.producer.open(Opening {
            head,
            first_frame: frame,
            server_us: self.start.elapsed().as_micros() as u64,
        })
    }
}

impl ViewportArtifactsSink for WireSink {
    fn head(&mut self, head: ViewportArtifactsHead) -> SinkResult {
        self.head = Some(head);
        Ok(())
    }

    fn frame(&mut self, tile: Option<u64>, artifacts: &[ArtifactOut]) -> SinkResult {
        let serialise_start = Instant::now();
        // A prefix at depth 16 or less fits 32 bits, and the handler refused any deeper.
        let tile = tile.map(|prefix| prefix as u32);
        let rows: Vec<ArtifactRow<'_>> = artifacts
            .iter()
            .map(|a| ArtifactRow {
                layer: a.layer.as_str(),
                tessera_id: a.tessera_id.raw(),
                key: a.key.as_deref(),
                masked_count: a.masked_count,
                centroid: a.derived.centroid,
                bbox: a.derived.bbox,
                content: &a.content,
                parent_ids: a.parent_ids.iter().map(|id| id.raw()).collect(),
                rung: a.rung,
                matched: a.matched,
                highlighted: a.highlighted,
                target: a.target.map(|id| id.raw()),
                tile,
            })
            .collect();
        let frame = artifacts_frame(&rows);
        self.arrow_serialise_ns += serialise_start.elapsed().as_nanos() as u64;
        self.rows += rows.len() as u64;
        self.frames += 1;
        self.send(frame)
    }
}

/// The producer: the engine call and the frames, on a blocking thread for the whole stream.
fn run(
    state: &AppState,
    session: &tessera_engine::Session,
    req: ViewportArtifactsReq,
    cancel: CancelToken,
    mut sink: WireSink,
) {
    let meta = state.engine.meta();
    // An unknown key, an undeclared name and a view the principal cannot reach all get the same
    // 404, as on the viewport.
    let Some(view) = meta.resolve_visible_view(&req.view, session.visible_views()) else {
        sink.producer
            .refuse(ApiError::Unknown(format!("unknown view '{}'", req.view)));
        return;
    };
    let parser = FilterParser::new(
        &meta,
        view,
        session.visible_views(),
        state.limits.max_region_vertices,
    );
    let parse = |value: Option<&serde_json::Value>| value.map(|v| parser.parse(v)).transpose();
    let (filter, highlight) = match (parse(req.filters.as_ref()), parse(req.highlight.as_ref())) {
        (Ok(filter), Ok(highlight)) => (filter, highlight),
        (Err(e), _) | (_, Err(e)) => {
            sink.producer.refuse(e);
            return;
        }
    };
    let tiles = distinct_tiles(req.tiles);
    let names = layer_names(req.layers.as_ref());
    let numbers = level_numbers(req.levels.as_ref());
    // The handler refused any other word.
    let computed: Vec<tessera_engine::ComputedProperty> = req
        .computed
        .iter()
        .flatten()
        .filter_map(|name| tessera_engine::ComputedProperty::parse_ask(name))
        .collect();
    let mut request = ViewportArtifactsRequest::new(
        &view.id,
        req.zoom,
        req.bbox.unwrap_or([0.0; 4]),
        req.per_tile,
    )
    .tiles(tiles.as_deref())
    .stamp(req.pin.map(GenerationStamp::from))
    .layers(layer_selection(req.layers.as_ref(), &names))
    .levels(level_selection(req.levels.as_ref(), &numbers))
    .computed(match &req.computed {
        Some(_) => ComputedSelection::Named(&computed),
        None => ComputedSelection::Declared,
    })
    .budget(req.budget)
    .cancel(Some(cancel));
    if let Some(filter) = filter {
        request = request.filter(filter);
    }
    if let Some(highlight) = highlight {
        request = request.highlight(highlight);
    }

    match state
        .engine
        .viewport_artifacts_stream(session, request, &mut sink)
    {
        Ok(()) => {
            let trailer = serde_json::json!({
                "stream_us": sink.start.elapsed().as_micros() as u64,
                "arrow_serialise_ns": sink.arrow_serialise_ns,
                "rows": sink.rows,
                "frames": sink.frames,
            });
            let trailer = trailer_frame(trailer.to_string().as_bytes());
            // A request of no tiles and no treed artifact has no frame to open with.
            if sink.producer.is_open() || sink.send(Vec::new()).is_ok() {
                sink.producer.finish(trailer);
            }
        }
        Err(e) if !sink.producer.is_open() => sink.producer.refuse(map_engine_error(e)),
        Err(e) => {
            if let Some(shed) = sink.producer.shed() {
                tracing::warn!(
                    view = %view.id,
                    zoom = req.zoom,
                    layers = %layers_named(req.layers.as_ref()),
                    elapsed_ms = sink.start.elapsed().as_millis() as u64,
                    frames = sink.frames,
                    "artifacts viewport stream SHED mid-body by the server — {}",
                    shed.detail()
                );
            } else if !matches!(e, tessera_engine::EngineError::Cancelled) {
                tracing::warn!(error = %e, "artifacts viewport stream aborted mid-body");
            }
            sink.producer.abort();
        }
    }
}

/// `POST /v1/artifacts/viewport`.
pub(crate) async fn viewport_artifacts(
    State(state): State<Arc<AppState>>,
    ViewerSession(session): ViewerSession,
    ApiJson(req): ApiJson<ViewportArtifactsReq>,
) -> Result<Response, ApiError> {
    check_extent(req.zoom, req.bbox.as_ref(), req.tiles.as_deref())?;
    let ceiling = state.limits.max_artifacts_per_tile;
    if req.per_tile > ceiling {
        return Err(ApiError::Contract(format!(
            "`per_tile` is {}, above this deployment's {ceiling} \
             (`selection.max_artifacts_per_tile`); ask for at most {ceiling}",
            req.per_tile
        )));
    }
    if let Some(bad) = req
        .computed
        .iter()
        .flatten()
        .find(|name| tessera_engine::ComputedProperty::parse_ask(name).is_none())
    {
        return Err(ApiError::Contract(format!(
            "`computed` names {bad:?}; this route computes {}, and an artifact's shape is read \
             by its `tessera_id`",
            tessera_engine::ComputedProperty::ASK_VOCABULARY.join(" and ")
        )));
    }

    // Created before admission so one token covers the whole request; the guard then moves into
    // the response body.
    let cancel = CancelToken::new();
    let cancel_guard = CancelGuard::new(cancel.clone());
    let (permits, admission_us) = state.artifact_gate.admit().await?;
    let start = Instant::now();

    let (producer, pending) = crate::stream::channel(
        cancel_guard,
        Duration::from_millis(state.limits.stream_write_stall_ms),
        Some(Duration::from_millis(state.limits.stream_deadline_ms)),
    );
    let sink = WireSink {
        head: None,
        producer,
        _permits: permits,
        start,
        arrow_serialise_ns: 0,
        rows: 0,
        frames: 0,
    };
    let closure_state = Arc::clone(&state);
    drop(tokio::task::spawn_blocking(move || {
        run(&closure_state, &session, req, cancel, sink);
    }));

    let (opening, body) = pending.opened("artifacts viewport", "first frame").await?;
    let head = opening.head;
    let pin = serde_json::to_string(&PinDto::from(&head.stamp))
        .expect("PinDto serialisation cannot fail");
    Ok(crate::stream::response_head(
        Some(&head.coordinates.identity_key),
        opening.server_us,
        admission_us,
        head.region,
    )
    .header(
        "etag",
        format!("\"{}\"", crate::stream::hex16(&head.coordinates.content_key)),
    )
    .header("x-tessera-pin", pin)
    .header("x-tessera-stale", if head.stale { "1" } else { "0" })
    .body(body.into_body(opening.first_frame))
    .expect("response construction cannot fail"))
}
