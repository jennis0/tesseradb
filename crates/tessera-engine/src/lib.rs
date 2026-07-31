//! `tessera-engine` — the request-serving core: generation snapshots and the I1 mask composition
//! (task-10 brief).
//!
//! [`Generation`] is one immutable snapshot of everything a request needs: the loaded bundle, the
//! live overlay and ingest buffer, and the version counters that identify it. A running process
//! holds the current generation behind an `arc_swap::ArcSwap`, atomically swapped whenever a
//! `/control/changes` or `/control/ingest` acceptance advances the overlay/buffer, or a
//! `tessera build` advances the bundle.

mod cache;
pub mod cancel;
pub mod compose;
mod pins;
pub mod select;
pub mod session;
mod single_flight;
pub mod timing;
pub mod viewport;
mod write;

use std::sync::Arc;

use arc_swap::ArcSwap;

use tessera_lifecycle::{IngestBuffer, Overlay};
use tessera_store::Bundle;

pub use cancel::CancelToken;
pub use compose::{compose, visible_to, EffectiveMask, RowProjection};
// The pin drain list's public surface (Task 4). `pins` itself stays private — `PinManager` and
// `PinnedGeometry` are engine-internal, and `PinnedGeometry` in particular exists to constrain
// what `viewport.rs` can reach, which a public type would undo. What escapes is only what a
// caller outside this crate genuinely needs: the reclaim record Task 5's cache pruner hooks, the
// gauges `/control/status` will publish, and the depth the operator alarms above.
pub use pins::{GeometryRefused, PinStats, Reclaimed, DRAIN_DEPTH_ALARM, DRAIN_DEPTH_MAX};
pub use session::{default_compute_threads, Engine, EngineConfig, EngineError, Session};
pub use timing::{Probe, StageTimings};
pub use viewport::{
    EngineMeta, ItemOut, PointOut, ScalarOut, SubCellCount, TileCount, ViewportOut, ViewportRequest,
};
// The write path's **outcome** vocabulary, and nothing else.
//
// An earlier draft also re-exported `LifecycleHandle`, `LifecycleQueues`, `Job` and `Responder`,
// with a comment saying `tessera-server`'s handlers hold the handle and submit through it. That
// design was cancelled in review (Task 3a's C1/A1) and the opposite is now true: `LifecycleHandle`
// is deliberately not `Clone` and `WritePath` is its sole owner, precisely so `WritePath::drop` can
// disconnect the queues and join the thread. No handler can hold one, and none of those four types
// appeared anywhere outside `write.rs` — a 3b worker following that comment would have found the
// handle unreachable and then reached for whatever compiled instead. They are `pub(crate)` now.
//
// What a handler does hold is `Engine`, and it submits through `Engine::accept_ingest` /
// `Engine::accept_change` — blocking calls, hence inside `spawn_blocking`. What it needs from here
// is how to answer: `AcceptError` for the status mapping (Task 3b owns the table) and
// `ExecutorPosture`/`ExecutorStats` for `readyz` and `/control/status`.
pub use write::{AcceptError, ExecutorHealth, ExecutorPosture, ExecutorStartError, ExecutorStats};

/// One immutable, atomically-swappable snapshot of engine state (lifecycle §1.1, slimmed for
/// Phase 1: no merge/compaction fields yet).
pub struct Generation {
    /// The bundle's `CURRENT` prefix (e.g. `"v00000"`) this generation was loaded from.
    pub prefix: String,
    /// Monotone counter identifying this generation's segment set — bumped only when a new
    /// bundle build is loaded, never by an overlay/buffer update.
    pub segments_version: u64,
    /// The SEGMENTS manifest's watermark: the highest entity id folded into the bundle's row
    /// geometry. Entities at or past this value live only in `buffer`, never in `bundle`'s
    /// permutations (I1 composition rule 4).
    pub watermark: u64,
    pub bundle: Arc<Bundle>,
    /// Monotone counter bumped on every overlay/buffer swap (independent of `segments_version` —
    /// an overlay change never touches the bundle).
    pub overlay_version: u64,
    pub overlay: Arc<Overlay>,
    pub buffer: Arc<IngestBuffer>,
}

/// The process-wide handle to the current generation. A request must load this pointer exactly
/// **once**, at request start, before acquiring any fragment or cache entry — loading it more
/// than once within a single request risks composing a fragment built against one generation's
/// bundle/watermark against an overlay or buffer swapped in from a later one, which is exactly
/// the kind of cross-generation mismatch `RowProjection`'s cache key (token, slice, pin) and
/// `FrozenFragment`'s stored watermark both assume can't happen. Wire this comment forward to the
/// actual request-handling load site when it lands (Task 13).
pub type GenerationHandle = ArcSwap<Generation>;
