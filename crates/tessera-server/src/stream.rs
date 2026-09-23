//! What the streamed viewer routes share: a producer on a blocking thread that answers the handler
//! once, before the body starts, and then writes frames into a bounded channel; the body that
//! drains that channel; the cancellation a dropped body sends the engine; and the headers every
//! streamed response carries.
//!
//! A refusal before the producer's opening keeps its status, and the opening carries what the
//! response headers need. After it the 200 is committed. The body ends cleanly only if the
//! producer's last frame went out; otherwise the connection is cut, so a client reads a body
//! without its trailer as incomplete.

use std::pin::Pin;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::StatusCode;
use bytes::Bytes;
use tokio::sync::{mpsc, oneshot};

use tessera_engine::{CancelToken, RegionVerdict, SinkClosed, SinkResult};

use crate::error::ApiError;

/// The channel's capacity in frames: one in flight, one built ahead. This bounds what a stalled
/// client holds beyond hyper's write buffer.
const CHANNEL_FRAMES: usize = 2;

/// [`StreamBody`]'s three completion states, published by the producer before it drops the
/// channel sender, so the body's end-of-channel read is never ambiguous.
const RUNNING: u8 = 0;
const COMPLETE: u8 = 1;
const ABORTED: u8 = 2;

/// Cancels a [`CancelToken`] on drop. Held by the handler and then by the [`StreamBody`], so a
/// client disconnect at any phase cancels the engine call, which holds only a clone. Disarmed
/// when the stream completes, so only a request cut short is cancelled.
pub(crate) struct CancelGuard {
    token: CancelToken,
    armed: bool,
}

impl CancelGuard {
    pub(crate) fn new(token: CancelToken) -> Self {
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

/// A frame refusal the server chose, as opposed to the client going away.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Shed {
    /// The whole-stream budget from the viewport's first flush, `serve.stream_deadline_ms`.
    Deadline,
    /// The per-send stall budget, `serve.stream_write_stall_ms`: a reader that stopped reading.
    Stall,
}

impl Shed {
    pub(crate) fn detail(self) -> &'static str {
        match self {
            Shed::Deadline => {
                "the whole-stream deadline fired: the response was committed and the work behind \
                 its next frame outran serve.stream_deadline_ms. A cold request over a level whose \
                 derived structures the prefix does not carry is the shape to check first — the \
                 build's artifact pass writes them, and an open reporting no adoptions says they \
                 were not taken"
            }
            Shed::Stall => {
                "the per-send stall budget fired: the client stopped reading and \
                 serve.stream_write_stall_ms elapsed with the body channel full"
            }
        }
    }
}

/// The producer's half of a stream, moved onto the blocking thread with the engine call. It
/// answers the handler once, through [`Self::open`] or [`Self::refuse`], then sends frames.
pub(crate) struct Producer<T> {
    opening: Option<oneshot::Sender<Result<T, ApiError>>>,
    tx: mpsc::Sender<Bytes>,
    state: Arc<AtomicU8>,
    stall: Duration,
    deadline: Duration,
    /// Set by [`Self::start_deadline`]; the whole-stream deadline is measured from it.
    deadline_from: Option<Instant>,
    /// Why this producer refused a frame, when the server chose to. The engine reports any
    /// refusal as cancellation, which is not logged, so without this a server-side shed would go
    /// unlogged.
    shed: Option<Shed>,
}

/// The handler's half of a stream, until the producer opens it.
pub(crate) struct Pending<T> {
    opening: oneshot::Receiver<Result<T, ApiError>>,
    body: StreamBody,
}

/// The body of a stream the producer has opened, to be given the opening's frames.
pub(crate) struct Opened(StreamBody);

/// A stream whose sends are refused after `stall` of a full channel, or after `deadline` from
/// [`Producer::start_deadline`]. `cancel_guard` moves into the body.
pub(crate) fn channel<T>(
    cancel_guard: CancelGuard,
    stall: Duration,
    deadline: Duration,
) -> (Producer<T>, Pending<T>) {
    let (opening_tx, opening_rx) = oneshot::channel();
    let (tx, rx) = mpsc::channel::<Bytes>(CHANNEL_FRAMES);
    let state = Arc::new(AtomicU8::new(RUNNING));
    let producer = Producer {
        opening: Some(opening_tx),
        tx,
        state: Arc::clone(&state),
        stall,
        deadline,
        deadline_from: None,
        shed: None,
    };
    let pending = Pending {
        opening: opening_rx,
        body: StreamBody {
            first: None,
            rx,
            state,
            cancel_guard,
            done: false,
        },
    };
    (producer, pending)
}

impl<T> Producer<T> {
    /// Answers the waiting handler with a refusal, which keeps its status. Does nothing once the
    /// stream is open.
    pub(crate) fn refuse(&mut self, e: ApiError) {
        if let Some(tx) = self.opening.take() {
            let _ = tx.send(Err(e));
        }
    }

    /// Hands the handler what its response needs. Refused when the handler has gone, which is a
    /// client that disconnected before the opening.
    pub(crate) fn open(&mut self, opening: T) -> SinkResult {
        self.opening
            .take()
            .expect("a stream is opened once")
            .send(Ok(opening))
            .map_err(|_| SinkClosed)
    }

    /// Whether the handler has had its answer, after which an error can only cut the body.
    pub(crate) fn is_open(&self) -> bool {
        self.opening.is_none()
    }

    /// Measures the whole-stream deadline from now.
    pub(crate) fn start_deadline(&mut self) {
        self.deadline_from = Some(Instant::now());
    }

    pub(crate) fn deadline_from(&self) -> Option<Instant> {
        self.deadline_from
    }

    pub(crate) fn stall(&self) -> Duration {
        self.stall
    }

    pub(crate) fn deadline(&self) -> Duration {
        self.deadline
    }

    pub(crate) fn shed(&self) -> Option<Shed> {
        self.shed
    }

    /// Sends one frame, blocking, under the per-send stall budget (a reader that stopped) and the
    /// whole-stream deadline once started (a reader that drips). Refusal is [`SinkClosed`], which
    /// the engine treats as cancellation.
    pub(crate) fn send(&mut self, frame: Vec<u8>) -> SinkResult {
        let send_started = Instant::now();
        let mut item = Bytes::from(frame);
        loop {
            if self
                .deadline_from
                .is_some_and(|t| t.elapsed() >= self.deadline)
            {
                self.shed = Some(Shed::Deadline);
                return Err(SinkClosed);
            }
            match self.tx.try_send(item) {
                Ok(()) => return Ok(()),
                Err(mpsc::error::TrySendError::Full(back)) => {
                    if send_started.elapsed() >= self.stall {
                        self.shed = Some(Shed::Stall);
                        return Err(SinkClosed);
                    }
                    item = back;
                    // Poll with a short sleep: tokio's mpsc has no blocking send with a timeout,
                    // and 5 ms is fine against a stall budget of seconds.
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(mpsc::error::TrySendError::Closed(_)) => return Err(SinkClosed),
            }
        }
    }

    /// Sends the last frame. The body ends cleanly exactly when it went out.
    pub(crate) fn finish(&mut self, trailer: Vec<u8>) {
        let end = if self.send(trailer).is_ok() {
            COMPLETE
        } else {
            ABORTED
        };
        self.state.store(end, Ordering::SeqCst);
    }

    /// Ends the body by cutting the connection, after an error mid-body.
    pub(crate) fn abort(&self) {
        self.state.store(ABORTED, Ordering::SeqCst);
    }
}

impl<T> Pending<T> {
    /// Waits for the producer's opening. A refusal keeps its status; a producer that ended
    /// without answering, which is a panic, is a fail-closed 500 with a fixed detail.
    pub(crate) async fn opened(self, producer: &str) -> Result<(T, Opened), ApiError> {
        match self.opening.await {
            Ok(Ok(opening)) => Ok((opening, Opened(self.body))),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(ApiError::FailClosed(format!(
                "the {producer} producer terminated before its first flush"
            ))),
        }
    }
}

impl Opened {
    /// The response body: `first`, then the producer's frames.
    pub(crate) fn into_body(mut self, first: Vec<u8>) -> Body {
        self.0.first = Some(Bytes::from(first));
        Body::from_stream(self.0)
    }
}

/// The start of every streamed response: a 200 of framed bytes, the identity coordinate, the
/// server's compute from admission to the opening, the admission wait, and whether counts under a
/// region leaf are exact. The last is a header for the same reason `x-tessera-stale` is: it
/// depends on the shape and the grid, never on the rows.
pub(crate) fn response_head(
    identity_key: Option<&[u8; 16]>,
    server_us: u64,
    admission_us: u64,
    region: Option<RegionVerdict>,
) -> axum::http::response::Builder {
    let mut response = axum::http::Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/octet-stream");
    // The authorisation coordinate: whether a held band may be rendered at all, so it is the
    // client's cache partition key.
    if let Some(key) = identity_key {
        response = response.header("x-tessera-identity-key", hex16(key));
    }
    let response = response
        .header("x-tessera-server-us", server_us.to_string())
        .header("x-tessera-admission-us", admission_us.to_string());
    match region {
        Some(verdict) => response.header("x-tessera-region", verdict.header_value()),
        None => response,
    }
}

/// Lower-case hex of an opaque 16-byte coordinate. Not a checksum and not reversible by a client:
/// the only operation defined on it is equality against one the server minted earlier.
pub(crate) fn hex16(bytes: &[u8; 16]) -> String {
    let mut out = String::with_capacity(32);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// The streamed response body: the opening's frames, the producer's frames, then a clean end only
/// if the last frame went out. It owns the [`CancelGuard`], so a client disconnect drops this body,
/// cancelling the token and closing the channel.
struct StreamBody {
    first: Option<Bytes>,
    rx: mpsc::Receiver<Bytes>,
    state: Arc<AtomicU8>,
    cancel_guard: CancelGuard,
    /// Once the end is yielded, every later poll is `Ready(None)`, never a second error.
    done: bool,
}

impl futures_core::Stream for StreamBody {
    type Item = std::result::Result<Bytes, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.done {
            return Poll::Ready(None);
        }
        if let Some(first) = this.first.take() {
            return Poll::Ready(Some(Ok(first)));
        }
        match this.rx.poll_recv(cx) {
            Poll::Ready(Some(frame)) => Poll::Ready(Some(Ok(frame))),
            Poll::Ready(None) => {
                this.done = true;
                if this.state.load(Ordering::SeqCst) == COMPLETE {
                    // Clean end: the trailer was the last frame, so disarm; see `CancelGuard`.
                    this.cancel_guard.disarm();
                    Poll::Ready(None)
                } else {
                    // Aborted by an engine error, a stall or the deadline. An error makes hyper
                    // cut the connection instead of ending the chunked body cleanly.
                    Poll::Ready(Some(Err(std::io::Error::other(
                        "stream aborted before its trailer",
                    ))))
                }
            }
            Poll::Pending => Poll::Pending,
        }
    }
}
