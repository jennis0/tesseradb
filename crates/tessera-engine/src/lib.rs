//! `tessera-engine` — the request-serving core: generation snapshots and the I1 mask composition.
//!
//! [`Generation`] is one immutable snapshot of everything a request needs: the loaded bundle, the
//! live overlay and ingest buffer, and the version counters that identify it. A running process
//! holds the current generation behind an `arc_swap::ArcSwap`, atomically swapped whenever a
//! `/control/changes` or `/control/ingest` acceptance advances the overlay/buffer, or a
//! `tessera build` advances the bundle.

mod cache;
pub mod cancel;
mod coalesce;
pub mod compose;
mod flush;
mod geometry;
mod merge;
mod refresh;
pub mod select;
pub mod session;
mod single_flight;
pub mod timing;
pub mod viewport;
mod write;

use std::sync::Arc;

use arc_swap::ArcSwap;

use tessera_authz::{DeltaTier, Dict, PostingsReader};
use tessera_lifecycle::{IngestBuffer, Overlay};
use tessera_store::Bundle;

pub use cancel::CancelToken;
pub use compose::{compose, denied_rows_of, visible_to, EffectiveMask, RowProjection};
// The publication guard's refusal, which a publisher outside this crate must handle.
// `check_publishable` itself stays private: whether a geometry may be published is this crate's
// judgement, and a caller that could ask separately could also act on a stale answer.
pub use geometry::{GeometryRefused, GeometryRefusedReason};
pub use session::{default_compute_threads, Engine, EngineConfig, EngineError, Session};
// The row-projection cache's gauges. `single_flight` itself stays private — the cache, its slot
// state machine and its four eviction rules are engine-internal — but the numbers
// `/control/status` publishes have to cross the crate boundary.
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
// `EngineMeta::quantisation`'s type, re-exported for the same layering reason `DeclaredScalar` is:
// `check-layers.sh` denies a `tessera-server → tessera-store` edge (SA §3), and `/control/ingest`
// validates an ingested coordinate against this declaration (§6), so the type needs to be nameable
// from the crate that reads it.
pub use tessera_store::manifest::Quantisation;
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
    PublishGeometryError, DENY_DURABILITY_ATTEMPTS, DENY_WINDOW_MAX_ENTRIES,
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
///
/// **⊘ Partially implemented:** §1.1's compaction fields are absent, there being no compaction.
/// Merge no longer is: both halves publish through this type — the entity-space coalesce without
/// moving `segments_version`, the row-space merge as its own swap (`crate::coalesce`,
/// `crate::merge`).
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
    /// The dictionary this generation's postings and buffered items resolve against.
    ///
    /// **Generation-scoped rather than process-scoped, because a flush promotes.** A novel
    /// descriptor buffers an item under an unsatisfiable extension id and becomes a durable
    /// ordinal only when the flush that carries it publishes a `dict_extents` entry (§3.2), so
    /// the dictionary grows with geometry and has to be republished alongside it. Ordinals are
    /// preserved across a promotion ([`tessera_authz::Dict::load_extending`]), so a session
    /// authorised against an older generation keeps evaluating the terms it was granted; what it
    /// does *not* get is the newly promoted one, which is fail-closed and is what §3.3's
    /// staleness hint exists to advertise.
    pub dict: Arc<Dict>,
    /// The base postings — the build's `terms/postings.arrow`, unchanged by any flush.
    pub postings: Arc<PostingsReader>,
    /// One sparse delta postings tier per flush segment, in publication order.
    ///
    /// A fragment build unions the base with every live tier over the session's satisfied terms
    /// (§5.2). They live on the generation rather than on the engine for the same reason the
    /// dictionary does: a flush publishes one, and a merge coalesces several into one, so the set
    /// changes exactly when geometry does. Empty in a bundle straight out of `tessera build`.
    pub delta_postings: Vec<Arc<DeltaTier>>,
    /// Monotone counter bumped on every overlay/buffer swap (independent of `segments_version` —
    /// an overlay change never touches the bundle).
    pub overlay_version: u64,
    pub overlay: Arc<Overlay>,
    pub buffer: Arc<IngestBuffer>,
    /// **The deny mask**: per slice, the row-space image of `deleted ∪ suppressed`, subtracted
    /// from every composed mask (I1).
    ///
    /// **Derived, never persisted, never a second source of truth.** The three entity-space stores
    /// on [`Overlay`] remain authoritative, and `compose::verdict` remains the single answer for
    /// every entity-space verb — `visible_to`, label gating, cluster visibility. This exists
    /// because the *row-space* question was being answered by walking the deny sets and resolving
    /// `row_of` per denied entity on every request, which made per-request work grow with denies
    /// **ever accepted**. Folded in as a bitmap, the deny half of composition costs one `andnot`.
    ///
    /// **It cannot go stale, because it never outlives its generation.** Row ids mean something
    /// only within one `segments_version`, so the mask is rebuilt by every geometry publication and
    /// travels with the row space it addresses — a request that loads one generation pointer gets
    /// the overlay, the buffer and the mask that agree.
    ///
    /// **The derivation rule is in [`crate::compose::derive_denied`]**, and the trap it names —
    /// that an unsuppress may not subtract a row — is the one way this could silently re-expose a
    /// deleted item. `publish` re-derives in debug and asserts equality, so a build site that gets
    /// it wrong fails in the test suite rather than in a viewer's map.
    ///
    /// Keyed by slice, because row space is. A slice the bundle carries always has an entry, empty
    /// when nothing is denied; a missing entry means the mask and the bundle disagree about what
    /// this generation holds, and the read path treats that as fail-closed rather than as "nothing
    /// denied".
    pub denied: Arc<DenyMask>,
}

/// Per-slice row-space deny masks — see [`Generation::denied`].
pub type DenyMask = rustc_hash::FxHashMap<String, croaring::Bitmap>;

/// The process-wide handle to the current generation. A request must load this pointer exactly
/// **once**, at request start, before acquiring any fragment or cache entry — loading it more
/// than once within a single request risks composing a fragment built against one generation's
/// bundle/watermark against an overlay or buffer swapped in from a later one, which is exactly
/// the kind of cross-generation mismatch `RowProjection`'s cache key and `FrozenFragment`'s
/// stored watermark both assume cannot happen. It is also the whole of I11's within-request rule
/// — see `crate::geometry`.
pub type GenerationHandle = ArcSwap<Generation>;
