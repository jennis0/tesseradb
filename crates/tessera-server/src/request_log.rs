//! `[serve] request_log`: one JSON line per viewer-plane and session-plane request, appended to a
//! file by a thread of its own.
//!
//! A request's line is sent when its response body ends or is dropped, since the viewport and
//! bulk routes stream and a client that pans away cancels mid-body. A request whose handler is
//! dropped before it answers is logged as cancelled with no status. Lines go through a bounded
//! channel; when it is full the line is dropped and counted, so a slow disk never holds a request.
//!
//! Never written: the bearer token, the session credential, any other header, and every response
//! body. `/session/authorise`'s request body is written, since replaying a session needs its
//! `auth_data`, and the `token_id` it issued is noted by the handler rather than read from the
//! response.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use bytes::Bytes;
use http_body::{Body as _, Frame, SizeHint};
use parking_lot::Mutex;

use crate::state::AppState;

/// How often the writer flushes when lines are waiting.
const FLUSH_EVERY: Duration = Duration::from_secs(1);

/// Lines that may wait for the writer before new ones are dropped.
const QUEUE_LINES: usize = 1 << 16;

/// Request-body bytes kept for the line. A larger body is logged by its size alone.
const BODY_CAPTURE_BYTES: usize = 1 << 20;

/// The open log: the sending half of the channel and the writer thread that drains it.
pub struct RequestLog {
    tx: Option<SyncSender<Line>>,
    writer: Option<std::thread::JoinHandle<()>>,
    dropped: AtomicU64,
}

impl RequestLog {
    /// Opens `path` for appending, creating it if absent, and starts the writer.
    pub fn open(path: &Path) -> std::io::Result<RequestLog> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        let (tx, rx) = sync_channel::<Line>(QUEUE_LINES);
        let shown = path.display().to_string();
        let writer = std::thread::Builder::new()
            .name("request-log".into())
            .spawn(move || write_lines(rx, file, &shown))?;
        Ok(RequestLog {
            tx: Some(tx),
            writer: Some(writer),
            dropped: AtomicU64::new(0),
        })
    }

    fn send(&self, line: Line) {
        let Some(tx) = &self.tx else { return };
        match tx.try_send(line) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
                if dropped.is_power_of_two() {
                    tracing::warn!(
                        dropped,
                        "the request log is behind; lines are being dropped rather than holding \
                         requests"
                    );
                }
            }
        }
    }
}

impl Drop for RequestLog {
    /// Closes the channel and waits for the writer to write and flush what was queued, so a
    /// server that is stopped and started again appends after every line of the first.
    fn drop(&mut self) {
        drop(self.tx.take());
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

fn write_lines(rx: std::sync::mpsc::Receiver<Line>, file: File, path: &str) {
    let mut out = BufWriter::new(file);
    let mut unflushed = false;
    let mut flushed_at = Instant::now();
    let mut failed = false;
    loop {
        let closed = match rx.recv_timeout(FLUSH_EVERY) {
            Ok(line) => {
                let mut text = line.into_json().to_string();
                text.push('\n');
                if let Err(e) = out.write_all(text.as_bytes()) {
                    if !failed {
                        tracing::warn!(path, error = %e, "the request log could not be written");
                        failed = true;
                    }
                }
                unflushed = true;
                false
            }
            Err(RecvTimeoutError::Timeout) => false,
            Err(RecvTimeoutError::Disconnected) => true,
        };
        if unflushed && (closed || flushed_at.elapsed() >= FLUSH_EVERY) {
            if let Err(e) = out.flush() {
                if !failed {
                    tracing::warn!(path, error = %e, "the request log could not be flushed");
                    failed = true;
                }
            }
            unflushed = false;
            flushed_at = Instant::now();
        }
        if closed {
            return;
        }
    }
}

/// How a request ended.
#[derive(Clone, Copy)]
enum Outcome {
    /// The whole body was sent.
    Completed,
    /// The client went away: the handler or the body was dropped before the body ended.
    Cancelled,
    /// The server ended the body with an error: a stream cut off by its deadline, a stall or an
    /// engine error.
    Aborted,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Outcome::Completed => "completed",
            Outcome::Cancelled => "cancelled",
            Outcome::Aborted => "aborted",
        }
    }
}

/// What a request's handler notes as it runs, read when the line is sent. `u64::MAX` is unset.
struct Notes {
    token_id: AtomicU64,
    admission_us: AtomicU64,
}

impl Notes {
    fn new() -> Notes {
        Notes {
            token_id: AtomicU64::new(u64::MAX),
            admission_us: AtomicU64::new(u64::MAX),
        }
    }

    fn read(slot: &AtomicU64) -> Option<u64> {
        match slot.load(Ordering::Relaxed) {
            u64::MAX => None,
            v => Some(v),
        }
    }
}

tokio::task_local! {
    static NOTES: Arc<Notes>;
}

/// Notes the session this request runs as. Does nothing outside a logged request.
pub(crate) fn note_token_id(token_id: u64) {
    let _ = NOTES.try_with(|notes| notes.token_id.store(token_id, Ordering::Relaxed));
}

/// Adds `us` to this request's wait for admission. Does nothing outside a logged request.
pub(crate) fn note_admission_us(us: u64) {
    let _ = NOTES.try_with(|notes| {
        let before = Notes::read(&notes.admission_us).unwrap_or(0);
        notes
            .admission_us
            .store(before.saturating_add(us).min(u64::MAX - 1), Ordering::Relaxed);
    });
}

/// The request body as the handler read it, up to [`BODY_CAPTURE_BYTES`].
#[derive(Default)]
struct Captured {
    bytes: Vec<u8>,
    size: u64,
}

/// One request, filled in as it runs and sent to the writer when it ends.
struct Line {
    start_us: u64,
    plane: &'static str,
    method: String,
    path: String,
    body: Arc<Mutex<Captured>>,
    notes: Arc<Notes>,
    status: Option<u16>,
    headers_us: Option<u64>,
    end_us: u64,
    bytes: u64,
    outcome: Outcome,
}

impl Line {
    fn into_json(self) -> serde_json::Value {
        let captured = std::mem::take(&mut *self.body.lock());
        let mut line = serde_json::json!({
            "start_us": self.start_us,
            "plane": self.plane,
            "method": self.method,
            "path": self.path,
            "body_bytes": captured.size,
            "token_id": Notes::read(&self.notes.token_id),
            "admission_us": Notes::read(&self.notes.admission_us),
            "status": self.status,
            "bytes": self.bytes,
            "headers_us": self.headers_us,
            "end_us": self.end_us,
            "outcome": self.outcome.as_str(),
        });
        if captured.size > 0 && captured.size as usize == captured.bytes.len() {
            if let Ok(body) = serde_json::from_slice::<serde_json::Value>(&captured.bytes) {
                line["body"] = body;
            }
        }
        line
    }
}

/// The viewer plane's logging middleware.
pub(crate) async fn viewer(state: State<Arc<AppState>>, req: Request, next: Next) -> Response {
    log_request(state, "viewer", req, next).await
}

/// The session plane's logging middleware.
pub(crate) async fn session(state: State<Arc<AppState>>, req: Request, next: Next) -> Response {
    log_request(state, "session", req, next).await
}

async fn log_request(
    State(state): State<Arc<AppState>>,
    plane: &'static str,
    req: Request,
    next: Next,
) -> Response {
    let path = req.uri().path();
    if path == "/healthz" || path == "/readyz" {
        return next.run(req).await;
    }
    let started = Instant::now();
    let start_us = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as u64);
    let body = Arc::new(Mutex::new(Captured::default()));
    let notes = Arc::new(Notes::new());
    let line = Line {
        start_us,
        plane,
        method: req.method().to_string(),
        path: req
            .uri()
            .path_and_query()
            .map_or_else(|| path.to_string(), |p| p.as_str().to_string()),
        body: Arc::clone(&body),
        notes: Arc::clone(&notes),
        status: None,
        headers_us: None,
        end_us: 0,
        bytes: 0,
        outcome: Outcome::Cancelled,
    };
    let req = req.map(|inner| {
        Body::new(TeeBody {
            inner,
            captured: body,
        })
    });

    let mut unanswered = Unanswered {
        state: Arc::clone(&state),
        line: Some(line),
        started,
    };
    let response = NOTES.scope(notes, next.run(req)).await;
    let mut line = unanswered.line.take().expect("taken only here");
    line.status = Some(response.status().as_u16());
    line.headers_us = Some(started.elapsed().as_micros() as u64);
    response.map(|inner| {
        Body::new(LoggedBody {
            inner,
            state,
            line: Some(line),
            started,
        })
    })
}

/// Logs a request whose handler was dropped before it answered: the client went away.
struct Unanswered {
    state: Arc<AppState>,
    line: Option<Line>,
    started: Instant,
}

impl Drop for Unanswered {
    fn drop(&mut self) {
        if let Some(mut line) = self.line.take() {
            line.end_us = self.started.elapsed().as_micros() as u64;
            if let Some(log) = &self.state.request_log {
                log.send(line);
            }
        }
    }
}

/// The request body, passed through unchanged while a copy of its first bytes is kept.
struct TeeBody {
    inner: Body,
    captured: Arc<Mutex<Captured>>,
}

impl http_body::Body for TeeBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        let polled = Pin::new(&mut self.inner).poll_frame(cx);
        if let Poll::Ready(Some(Ok(frame))) = &polled {
            if let Some(data) = frame.data_ref() {
                let mut captured = self.captured.lock();
                captured.size += data.len() as u64;
                if captured.bytes.len() + data.len() <= BODY_CAPTURE_BYTES {
                    captured.bytes.extend_from_slice(data);
                }
            }
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// The response body, passed through unchanged, which sends the request's line when it ends or
/// is dropped.
struct LoggedBody {
    inner: Body,
    state: Arc<AppState>,
    line: Option<Line>,
    started: Instant,
}

impl LoggedBody {
    fn finish(&mut self, outcome: Outcome) {
        if let Some(mut line) = self.line.take() {
            line.outcome = outcome;
            line.end_us = self.started.elapsed().as_micros() as u64;
            if let Some(log) = &self.state.request_log {
                log.send(line);
            }
        }
    }
}

impl http_body::Body for LoggedBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        let polled = Pin::new(&mut self.inner).poll_frame(cx);
        match &polled {
            Poll::Ready(Some(Ok(frame))) => {
                if let (Some(data), Some(line)) = (frame.data_ref(), self.line.as_mut()) {
                    line.bytes += data.len() as u64;
                }
                // A body of known length is not polled again once it reports its end.
                if self.inner.is_end_stream() {
                    self.finish(Outcome::Completed);
                }
            }
            Poll::Ready(Some(Err(_))) => self.finish(Outcome::Aborted),
            Poll::Ready(None) => self.finish(Outcome::Completed),
            Poll::Pending => {}
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for LoggedBody {
    fn drop(&mut self) {
        // An empty body may never be polled.
        let outcome = if self.inner.is_end_stream() {
            Outcome::Completed
        } else {
            Outcome::Cancelled
        };
        self.finish(outcome);
    }
}
