//! `[serve] request_log`: one JSON line per viewer-plane and session-plane request, appended to a
//! file by a thread of its own.
//!
//! A request's line is sent when its response body ends or is dropped, since the viewport and
//! bulk routes stream and a client that pans away cancels mid-body. A request whose handler is
//! dropped before it answers is logged as cancelled with no status. Lines go through a channel
//! bounded in lines and in bytes, so a slow disk never holds a request: past the byte budget a
//! line is kept without its body, and past the line bound it is dropped and counted on the next
//! line written. The writer flushes whenever it has drained the channel, and at least once a
//! second under constant load, so a process that exits without unwinding loses little.
//!
//! Each line carries the run, a random id taken when the log is opened, since `token_id`s start
//! again in each process.
//!
//! Request bodies are written, including `/session/authorise`'s, which is what replaying a session
//! needs; the file is created readable by its owner only. Never written: the bearer token, the
//! session plane's credential, any other header, and every response body. The
//! `token_id` an authorisation issued is noted by the handler rather than read from the response.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TryRecvError, TrySendError};
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
use serde::Serialize;
use serde_json::value::RawValue;

use crate::state::AppState;

/// How often the writer flushes while the channel never empties.
const FLUSH_EVERY: Duration = Duration::from_secs(1);

/// Lines that may wait for the writer before new ones are dropped.
const QUEUE_LINES: usize = 1 << 16;

/// Bytes that may wait for the writer before new lines are queued without their bodies.
const QUEUE_BYTES: usize = 64 << 20;

/// What a queued line costs beyond its path and body.
const LINE_OVERHEAD_BYTES: usize = 256;

/// Request-body bytes kept for the line. A larger body is logged by its size alone.
const BODY_CAPTURE_BYTES: usize = 1 << 20;

/// The open log: the sending half of the channel and the writer thread that drains it.
pub struct RequestLog {
    tx: Option<SyncSender<Queued>>,
    writer: Option<std::thread::JoinHandle<()>>,
    /// Bytes queued for the writer, which subtracts each line's cost once it is written.
    queued_bytes: Arc<AtomicUsize>,
    /// Lines dropped since the last line queued, written on the next one.
    dropped: AtomicU64,
    dropped_total: AtomicU64,
}

impl RequestLog {
    /// Opens `path` for appending, creating it readable by its owner only if absent, and starts
    /// the writer.
    pub fn open(path: &Path) -> std::io::Result<RequestLog> {
        let mut options = std::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let file = options.open(path)?;
        let (tx, rx) = sync_channel::<Queued>(QUEUE_LINES);
        let queued_bytes = Arc::new(AtomicUsize::new(0));
        let writer = {
            let shown = path.display().to_string();
            let queued_bytes = Arc::clone(&queued_bytes);
            let run = run_id();
            std::thread::Builder::new()
                .name("request-log".into())
                .spawn(move || write_lines(rx, file, &shown, &run, &queued_bytes))?
        };
        Ok(RequestLog {
            tx: Some(tx),
            writer: Some(writer),
            queued_bytes,
            dropped: AtomicU64::new(0),
            dropped_total: AtomicU64::new(0),
        })
    }

    fn send(&self, line: Line) {
        let Some(tx) = &self.tx else { return };
        let captured = std::mem::take(&mut *line.body.lock());
        let body_bytes = captured.size;
        let mut body = (captured.size > 0 && captured.size as usize == captured.bytes.len())
            .then_some(captured.bytes);
        let base = line.path.len() + LINE_OVERHEAD_BYTES;
        let queued = self.queued_bytes.load(Ordering::Relaxed);
        if queued + base + body.as_ref().map_or(0, Vec::len) > QUEUE_BYTES {
            body = None;
        }
        let cost = base + body.as_ref().map_or(0, Vec::len);
        self.queued_bytes.fetch_add(cost, Ordering::Relaxed);
        let dropped_before = self.dropped.swap(0, Ordering::Relaxed);
        let queued = Queued {
            line,
            body,
            body_bytes,
            dropped_before,
            cost,
        };
        if let Err(TrySendError::Full(queued) | TrySendError::Disconnected(queued)) =
            tx.try_send(queued)
        {
            self.queued_bytes.fetch_sub(cost, Ordering::Relaxed);
            self.dropped
                .fetch_add(queued.dropped_before + 1, Ordering::Relaxed);
            let total = self.dropped_total.fetch_add(1, Ordering::Relaxed) + 1;
            if total.is_power_of_two() {
                tracing::warn!(
                    dropped = total,
                    "the request log is behind; lines are being dropped rather than holding \
                     requests"
                );
            }
        }
    }
}

/// A random id for this process's lines. `RandomState` is seeded from the operating system's
/// random source, so two processes started in the same instant still differ.
fn run_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    hasher.write_u32(std::process::id());
    format!("{:016x}", hasher.finish())
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

/// Writes each queued line and flushes whenever the channel is empty, or a second after the last
/// flush if it never is. Returns once every sender is gone and the last line is flushed.
fn write_lines(
    rx: Receiver<Queued>,
    file: File,
    path: &str,
    run: &str,
    queued_bytes: &AtomicUsize,
) {
    let mut out = BufWriter::new(file);
    let mut failed = false;
    let mut report = |e: std::io::Error| {
        if !failed {
            tracing::warn!(path, error = %e, "the request log could not be written");
            failed = true;
        }
    };
    let write = |out: &mut BufWriter<File>, queued: Queued| -> std::io::Result<()> {
        queued_bytes.fetch_sub(queued.cost, Ordering::Relaxed);
        let mut text = serde_json::to_string(&queued.written(run))?;
        text.push('\n');
        out.write_all(text.as_bytes())
    };
    // Blocks only with nothing unflushed: every drain ends in a flush.
    while let Ok(queued) = rx.recv() {
        write(&mut out, queued).unwrap_or_else(&mut report);
        let mut flushed_at = Instant::now();
        loop {
            match rx.try_recv() {
                Ok(queued) => {
                    write(&mut out, queued).unwrap_or_else(&mut report);
                    if flushed_at.elapsed() >= FLUSH_EVERY {
                        out.flush().unwrap_or_else(&mut report);
                        flushed_at = Instant::now();
                    }
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => {
                    out.flush().unwrap_or_else(&mut report);
                    break;
                }
            }
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
    /// The server ended the body with an error: a stream cut off by its deadline or by an engine
    /// error. A stream cut off because its client stopped reading is seen as `Cancelled`, since
    /// the body is not polled again before the connection goes.
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

/// A line on its way to the writer, with the request body taken out of the request.
struct Queued {
    line: Line,
    /// The whole body, unless it was too large to keep or the queue's byte budget was spent.
    body: Option<Vec<u8>>,
    body_bytes: u64,
    dropped_before: u64,
    /// What this line counts against [`QUEUE_BYTES`].
    cost: usize,
}

/// One line as written.
#[derive(Serialize)]
struct Written<'a> {
    run: &'a str,
    start_us: u64,
    plane: &'static str,
    method: &'a str,
    path: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<Box<RawValue>>,
    body_bytes: u64,
    token_id: Option<u64>,
    admission_us: Option<u64>,
    status: Option<u16>,
    bytes: u64,
    headers_us: Option<u64>,
    end_us: u64,
    outcome: &'static str,
    #[serde(skip_serializing_if = "is_zero")]
    dropped_before: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// The body as JSON to embed, if it is JSON: as sent, unless it spans lines.
fn json_body(bytes: &[u8]) -> Option<Box<RawValue>> {
    let raw: Box<RawValue> = serde_json::from_slice(bytes).ok()?;
    if raw.get().contains(['\n', '\r']) {
        let value: serde_json::Value = serde_json::from_str(raw.get()).ok()?;
        return RawValue::from_string(value.to_string()).ok();
    }
    Some(raw)
}

impl Queued {
    fn written<'a>(&'a self, run: &'a str) -> Written<'a> {
        let line = &self.line;
        Written {
            run,
            start_us: line.start_us,
            plane: line.plane,
            method: &line.method,
            path: &line.path,
            body: self.body.as_deref().and_then(json_body),
            body_bytes: self.body_bytes,
            token_id: Notes::read(&line.notes.token_id),
            admission_us: Notes::read(&line.notes.admission_us),
            status: line.status,
            bytes: line.bytes,
            headers_us: line.headers_us,
            end_us: line.end_us,
            outcome: line.outcome.as_str(),
            dropped_before: self.dropped_before,
        }
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
