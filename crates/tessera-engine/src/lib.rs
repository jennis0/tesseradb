//! `tessera-engine` — the request-serving core: generation snapshots and the I1 mask composition.
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
// The pin drain list's public surface. `pins` itself stays private — `PinManager` and
// `PinnedGeometry` are engine-internal, and `PinnedGeometry` in particular exists to constrain
// what `viewport.rs` can reach, which a public type would undo. What escapes is only what a
// caller outside this crate genuinely needs: the reclaim record the cache pruner hooks, the
// refusal a publisher must handle, the gauges `/control/status` publishes, and the two depths —
// the one an operator alarms above and the one `tests/pins.rs` asserts the trim against.
pub use pins::{
    GeometryRefused, GeometryRefusedReason, PinStats, Reclaimed, DRAIN_DEPTH_ALARM, DRAIN_DEPTH_MAX,
};
pub use session::{default_compute_threads, Engine, EngineConfig, EngineError, Session};
// The row-projection cache's gauges. `single_flight` itself stays private — the cache, its slot state
// machine and its four eviction rules are engine-internal — but the numbers `/control/status`
// publishes have to cross the crate boundary, exactly as `PinStats` does above.
pub use single_flight::CacheStats;
// The fragment tier's gauges, under a distinguishing name because the two are the same shape and a
// bare second `CacheStats` in one namespace would be a coin toss at every call site.
//
// **This re-export is what makes `Engine::fragment_cache_stats`'s return type nameable at all.**
// `tessera-server` may not depend on `tessera-authz` (SA §3; `scripts/check-layers.sh` has
// `deny tessera-server tessera-authz`), and `tessera_authz::CacheStats` is not a public path even
// for a crate that could — its module is private there. Whoever wires `/control/status`
// writes `use tessera_engine::FragmentCacheStats;` and nothing else.
pub use tessera_authz::fragment::CacheStats as FragmentCacheStats;
pub use timing::{Probe, StageTimings};
pub use viewport::{
    EngineMeta, ItemOut, PointOut, ScalarOut, SubCellCount, TileCount, ViewportOut, ViewportRequest,
};
// `EngineMeta::declared_scalars`' element type, re-exported for the same layering reason
// `FragmentCacheStats` is: `check-layers.sh` denies a `tessera-server → tessera-store` edge
// (SA §3), and `/control/ingest` validates a batch's scalar tail against this declaration, so the
// type needs to be nameable from the crate that reads it. `/v1/meta` gets away without naming it
// only because it reads the two fields straight into JSON.
pub use tessera_store::manifest::DeclaredScalar;
// The write path's **outcome** vocabulary, and nothing else.
//
// `LifecycleHandle`, `LifecycleQueues`, `Job` and `Responder` are deliberately **not** here, and
// the tempting design where a handler holds the handle and submits through it is refused:
// `LifecycleHandle` is not `Clone` and `WritePath` is its sole owner, precisely so `WritePath::drop`
// can disconnect the queues and join the thread. No handler can hold one, and none of those four
// types appears anywhere outside `write.rs`.
//
// What a handler does hold is `Engine`, and it submits through `Engine::accept_ingest` /
// `Engine::accept_change` — blocking calls, hence inside `spawn_blocking`. What it needs from here
// is how to answer: `AcceptError` for the status mapping, and `ExecutorPosture`/`ExecutorStats`
// for `readyz` and `/control/status`.
pub use write::{
    AcceptError, ExecutorHealth, ExecutorPosture, ExecutorStartError, ExecutorStats, PendingChange,
    DENY_DURABILITY_ATTEMPTS, DENY_WINDOW_MAX_ENTRIES,
};
// The queue's own `retry_after_s` derivation. Exported because `tessera-server` derives a
// *second* 429 subject's value from the same estimator over a different depth (contracts §0.3
// deviation 11: the value is per-subject), and two independent implementations of one estimator is
// how the two subjects come to disagree about the same queue.
//
// **The floor and ceiling are exported for readers, not for callers.** `tessera-server` does not
// derive from them — it calls `estimate_retry_after_s` and inherits both. They stay `pub` because
// `estimate_retry_after_s`'s
// own doc names them as the bounds on its result, and a documented bound whose value is unreachable
// from the crate that reads the doc is a dead reference.
pub use write::{estimate_retry_after_s, RETRY_AFTER_MAX_SECS, RETRY_AFTER_MIN_SECS};

/// One immutable, atomically-swappable snapshot of engine state (lifecycle §1.1).
/// **⊘ Partially implemented:** §1.1's merge and compaction fields are absent, there being no
/// merge and no compaction; what is here is the bundle, the overlay, the buffer and the counters
/// that identify them.
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
/// `FrozenFragment`'s stored watermark both assume cannot happen.
pub type GenerationHandle = ArcSwap<Generation>;
