//! The row-projection cache seam.
//!
//! A named wrapper around the generic [`SingleFlightCache`] the D-G work landed, carved out of
//! `session.rs` (Phase 2 stage 2.1, Task 0a) so the cache's own policy — today none beyond
//! single-flight — has a file of its own to grow in.
//!
//! **Nothing here is a policy change.** No eviction, no bound, no LRU: the wrapper preserves the
//! single-flight state machine exactly as it was, including the property that matters most about
//! it — the miss path is **fallible and non-blocking**. A concurrent arrival on a key whose build
//! is in flight does not wait for it; it gets [`CacheBusy`] and the caller retries. An infallible
//! `get_or_insert`-shaped wrapper would have to block to honour its own signature, which is the
//! exact serialisation D-G exists to remove.

use std::sync::Arc;

use crate::compose::RowProjection;
use crate::single_flight::SingleFlightCache;

/// `(token_id, slice, segments_version)` — the row-projection cache's key (shared-context
/// constraint 8). `token_id` rather than the token string so the cache never has to hash or
/// compare a full bearer token.
pub(crate) type RowProjectionKey = (u64, String, u64);

/// A losing arrival's outcome: another caller is already building this key, and this call did not
/// wait for it (D-G). Carries nothing — the caller only needs to know to retry.
///
/// Distinct from `single_flight::Building` so the wrapper's callers depend on this module's
/// contract rather than on the generic cache's internals.
#[derive(Debug)]
pub(crate) struct CacheBusy;

/// Cached row-space projections, keyed `(token_id, slice, segments_version)` — never
/// recomputed on the per-viewport path (shared-context constraint 8; see
/// `crate::compose::RowProjection`'s doc for the cost this avoids).
///
/// D-G slot-state single-flight (F4, `tessera-bench/src/arms/load.rs:34-76`): the map lock is
/// held only for the O(1) `Building`/`Ready` transition, never across `RowProjection::new`
/// itself — see [`SingleFlightCache`]'s doc. A concurrent arrival on the same key while a
/// build is in flight does not wait for it; it gets [`crate::session::EngineError::ProjectionBuilding`]
/// and retries. Unbounded growth (eviction) is out of scope here — a memory concern, not the
/// concurrency one this cache exists to fix.
pub(crate) struct RowProjectionCache {
    inner: SingleFlightCache<RowProjectionKey, RowProjection>,
}

impl RowProjectionCache {
    pub(crate) fn new() -> Self {
        RowProjectionCache {
            inner: SingleFlightCache::new(),
        }
    }

    /// Slots currently held, `Building` and `Ready` both counted — a diagnostic, not a capacity
    /// bound. `Engine::row_projection_cache_len` publishes this.
    pub(crate) fn len(&self) -> usize {
        self.inner.len()
    }

    /// Look up `key`, building it on this call if nobody else already is.
    ///
    /// **Fallible on purpose, and `Err` does not mean failure.** `Err(CacheBusy)` means some other
    /// caller is building this key right now and this call declined to wait (see the module doc).
    /// `build` runs with no lock held and must be infallible — see the invariant note on
    /// [`SingleFlightCache::get_or_build`].
    pub(crate) fn get_or_build(
        &self,
        key: RowProjectionKey,
        build: impl FnOnce() -> RowProjection,
    ) -> Result<Arc<RowProjection>, CacheBusy> {
        self.inner
            .get_or_build(key, build)
            .map_err(|_busy| CacheBusy)
    }
}
