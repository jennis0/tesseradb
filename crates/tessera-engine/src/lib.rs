//! `tessera-engine` — the request-serving core: generation snapshots and the I1 mask composition
//! (task-10 brief).
//!
//! [`Generation`] is one immutable snapshot of everything a request needs: the loaded bundle, the
//! live overlay and ingest buffer, and the version counters that identify it. A running process
//! holds the current generation behind an `arc_swap::ArcSwap`, atomically swapped whenever a
//! `/control/changes` or `/control/ingest` acceptance advances the overlay/buffer, or a
//! `tessera build` advances the bundle.

pub mod compose;
pub mod session;
pub mod viewport;

use std::sync::Arc;

use arc_swap::ArcSwap;

use tessera_lifecycle::{IngestBuffer, Overlay};
use tessera_store::Bundle;

pub use compose::{compose, EffectiveMask, RowProjection};
pub use session::{Engine, EngineConfig, EngineError, Session};
pub use viewport::{EngineMeta, PointOut, ScalarOut, TileCount, ViewportOut};

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
