//! `POST /v1/items`: every item a viewer may see in one view that matches a filter, page by page,
//! in the viewport's framing. The engine serves one response per request and keeps nothing
//! between them; the caller carries the read forward by passing each response's cursor back.
//!
//! Runs under its own admission limit, `serve.bulk_admission`, so a long read takes no slot from
//! the viewport and item routes, and on a blocking thread that streams each page as it is built. A client that goes
//! away cancels the engine through the body's guard. The stream deadline cancels it too, and the
//! engine then ends the response with a short page and a trailer, so a read cut by the deadline
//! resumes from that trailer's cursor.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;

use tessera_engine::{
    CancelToken, ItemsHead, ItemsLimits, ItemsPageEnd, ItemsRequest, ItemsSink, RecordsOrder,
    RegionVerdict, SinkClosed, SinkResult,
};
use tessera_wire::{
    items_head_frame, page_end_frame, records_frame, trailer_frame, RecordsCompression,
};

use crate::error::{map_engine_error, ApiError};
use crate::state::{AppState, GatePermits, ViewerSession};
use crate::stream::{CancelGuard, Producer};
use crate::viewer::FilterParser;

/// The request body. Every field but `view` and `fields` may be left out; an unknown one is a
/// `422`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ItemsReq {
    view: String,
    fields: Vec<String>,
    #[serde(default)]
    system_fields: Vec<String>,
    /// Parsed by [`crate::filter_dto`] against the view, as the viewport's is.
    #[serde(default)]
    filters: Option<serde_json::Value>,
    #[serde(default)]
    keep_unmatched: bool,
    #[serde(default)]
    count: bool,
    #[serde(default)]
    order: Option<OrderReq>,
    #[serde(default)]
    page_rows: Option<u32>,
    #[serde(default)]
    pages: Option<u32>,
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    compression: Option<CompressionReq>,
    #[serde(default)]
    idset: Option<u32>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum OrderReq {
    Map,
    Stored,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CompressionReq {
    Zstd,
}

/// What the handler needs to answer, sent when the engine delivers the head.
struct Opening {
    /// Absent only when the response was cancelled before its first page opened the view.
    identity_key: Option<[u8; 16]>,
    region: Option<RegionVerdict>,
    /// The head frame: the body's first bytes.
    head: Vec<u8>,
    /// Microseconds from admission to the head, sent as `x-tessera-server-us`.
    server_us: u64,
}

/// The engine's [`ItemsSink`]: the head to the handler, then each page as a records frame and a
/// page end. The engine's batches carry `tessera_id` and the named fields only.
struct RecordsSink {
    producer: Producer<Opening>,
    /// Held until the response ends: a bulk read computes for its whole length.
    _permits: GatePermits,
    compression: RecordsCompression,
    /// Taken after admission, so blocking-pool wait counts in `server_us`.
    start: Instant,
    /// A batch the IPC writer refused, for the log; the engine sees only a closed sink.
    unwritable: Option<arrow::error::ArrowError>,
}

impl ItemsSink for RecordsSink {
    fn head(&mut self, head: &ItemsHead) -> SinkResult {
        let mut json = serde_json::json!({
            "order": head.order.as_str(),
            "page_rows": head.page_rows,
        });
        if let Some(counts) = head.counts {
            json["visible"] = counts.visible.into();
            json["matched"] = counts.matched.into();
        }
        self.producer.open(Opening {
            identity_key: head.identity_key,
            region: head.region,
            head: items_head_frame(json.to_string().as_bytes()),
            server_us: self.start.elapsed().as_micros() as u64,
        })
    }

    fn page(&mut self, batch: &arrow::record_batch::RecordBatch, end: &ItemsPageEnd) -> SinkResult {
        let records = records_frame(batch, self.compression).map_err(|e| {
            self.unwritable = Some(e);
            SinkClosed
        })?;
        self.producer.send(records)?;
        let json = serde_json::json!({
            "next": end.next,
            "ended_by": end.ended_by.as_str(),
        });
        self.producer.send(page_end_frame(json.to_string().as_bytes()))
    }
}

/// Aborts the stream-deadline timer when the response ends before it.
struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// `POST /v1/items`. A body that is not the request object is a `422` saying what serde found; a
/// body that is not JSON, or not sent as JSON, keeps axum's own refusal.
pub(crate) async fn items(
    State(state): State<Arc<AppState>>,
    ViewerSession(session): ViewerSession,
    body: Result<Json<ItemsReq>, JsonRejection>,
) -> Result<Response, ApiError> {
    let req = match body {
        Ok(Json(req)) => req,
        Err(JsonRejection::JsonDataError(e)) => {
            return Err(ApiError::Contract(format!(
                "{}; send the fields this route defines, with the values its schema allows",
                e.body_text()
            )))
        }
        Err(other) => return Ok(other.into_response()),
    };

    // Created before admission so one token covers the whole request; the guard then moves into
    // the response body.
    let cancel = CancelToken::new();
    let cancel_guard = CancelGuard::new(cancel.clone());
    let (permits, admission_us) = state.bulk_gate.admit().await?;
    let start = Instant::now();

    // The stream deadline cancels the engine, which ends the response with a trailer; the sender
    // itself never refuses a frame for time, so that trailer can go out.
    let deadline = Duration::from_millis(state.limits.stream_deadline_ms);
    let timer = {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(deadline).await;
            cancel.cancel();
        })
    };
    let timer = AbortOnDrop(timer.abort_handle());

    let (producer, pending) = crate::stream::channel(
        cancel_guard,
        Duration::from_millis(state.limits.stream_write_stall_ms),
        None,
    );
    let sink = RecordsSink {
        producer,
        _permits: permits,
        compression: match req.compression {
            Some(CompressionReq::Zstd) => RecordsCompression::Zstd,
            None => RecordsCompression::None,
        },
        start,
        unwritable: None,
    };
    let closure_state = Arc::clone(&state);
    drop(tokio::task::spawn_blocking(move || {
        let _timer = timer;
        run_items_stream(&closure_state, &session, req, cancel, sink);
    }));

    let (opening, body) = pending.opened("items", "first frame").await?;
    Ok(crate::stream::response_head(
        opening.identity_key.as_ref(),
        opening.server_us,
        admission_us,
        opening.region,
    )
    .body(body.into_body(opening.head))
    .expect("response construction cannot fail"))
}

/// The producer: the filter parse, the engine call and the trailer, on a blocking thread for the
/// whole response. The bulk-read permits release when `sink` drops at the end.
fn run_items_stream(
    state: &AppState,
    session: &tessera_engine::Session,
    req: ItemsReq,
    cancel: CancelToken,
    mut sink: RecordsSink,
) {
    let meta = state.engine.meta();
    // An unknown view and one this principal cannot reach are the viewport's one 404.
    let Some(view) = meta.resolve_visible_view(&req.view, session.visible_views()) else {
        sink.producer
            .refuse(ApiError::Unknown(format!("unknown view '{}'", req.view)));
        return;
    };
    let filter = match req.filters.as_ref().map(|value| {
        FilterParser::new(
            &meta,
            view,
            session.visible_views(),
            state.limits.max_region_vertices,
        )
        .parse(value)
    }) {
        None => None,
        Some(Ok(expr)) => Some(expr),
        Some(Err(e)) => {
            sink.producer.refuse(e);
            return;
        }
    };
    let request = ItemsRequest {
        view: &view.id,
        fields: &req.fields,
        system_fields: &req.system_fields,
        filter,
        keep_unmatched: req.keep_unmatched,
        count: req.count,
        order: req.order.map(|order| match order {
            OrderReq::Map => RecordsOrder::Map,
            OrderReq::Stored => RecordsOrder::Stored,
        }),
        page_rows: req.page_rows,
        pages: req.pages,
        cursor: req.cursor.as_deref(),
        idset: req.idset,
        limits: ItemsLimits {
            max_page_rows: state.limits.max_page_rows,
            max_page_bytes: state.limits.max_page_bytes,
            response_bytes: state.limits.bulk_response_bytes,
            response_time: Duration::from_millis(state.limits.bulk_response_ms),
        },
        cancel: Some(cancel),
    };

    match state.engine.items_stream(session, request, &mut sink) {
        Ok(trailer) => {
            let json = serde_json::json!({
                "pages": trailer.pages,
                "rows": trailer.rows,
                "next": trailer.next,
                "ended_by": trailer.ended_by.as_str(),
                "stream_us": sink.start.elapsed().as_micros() as u64,
            });
            sink.producer
                .finish(trailer_frame(json.to_string().as_bytes()));
        }
        // Before the head nothing is committed and the handler is waiting, so the refusal keeps
        // its status.
        Err(e) if !sink.producer.is_open() => sink.producer.refuse(map_engine_error(e)),
        // After it the 200 is committed: the body ends without a trailer, which a client reads as
        // incomplete and resumes from the last page end. A client that went away logs nothing.
        Err(e) => {
            if let Some(e) = &sink.unwritable {
                tracing::error!(view = %view.id, error = %e, "a records page could not be written");
            } else if let Some(shed) = sink.producer.shed() {
                tracing::warn!(
                    view = %view.id,
                    elapsed_ms = sink.start.elapsed().as_millis() as u64,
                    stall_ms = sink.producer.stall().as_millis() as u64,
                    "items stream SHED mid-body by the server: {}",
                    shed.detail()
                );
            } else if !matches!(e, tessera_engine::EngineError::Cancelled) {
                tracing::warn!(error = %e, "items stream aborted mid-body");
            }
            sink.producer.abort();
        }
    }
}
