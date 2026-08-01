//! The row-projection cache seam: the key, the byte bound's arithmetic, and the two pruners.
//!
//! A named wrapper around the generic [`SingleFlightCache`], carrying the policy that is specific
//! to projections: a byte bound, LRU eviction, and removal of entries whose key no request can
//! produce.
//!
//! The single-flight state machine, the four eviction rules and the counted choke point all live in
//! [`crate::single_flight`] and are argued there. What lives *here* is what is specific to
//! projections: the key, the weight function, the two pruners' safety argument, and the arithmetic
//! an operator needs to size a box.
//!
//! # What removal can and cannot do (I3's cache half)
//!
//! Nothing here modifies a cached value — lifecycle §7 requires entries to be immutable and
//! "invalidation is key rotation, never mutation", and both eviction and pruning only ever
//! *remove*. That is what makes a rebuilt projection identical to the evicted one: the miss path
//! builds `RowProjection::new(&session.fragment, &slice_data.permutation)` from the session's own
//! frozen fragment and the *pinned* generation's permutation, both of which the key names or the
//! request pins. **There is no route by which a miss composes against a different mask than a
//! hit**, which is the property `eviction_never_widens_a_mask` exists to keep true.

use std::sync::Arc;

use croaring::Portable;

use crate::compose::RowProjection;
use crate::single_flight::{CacheStats, CacheWeight, SingleFlightCache};

/// `(token_id, slice, segments_version)` — the row-projection cache's key (shared-context
/// constraint 8). `token_id` rather than the token string so the cache never has to hash or
/// compare a full bearer token.
///
/// **Named fields, not a tuple, and the reason is a disclosure.** Two of the
/// three components are `u64`, so as `(u64, String, u64)` a transposition at the construction site
/// compiles, runs, and keys every session's projection on `(segments_version, slice, token_id)` —
/// at which point any two sessions whose `token_id` collides with the live `segments_version` share
/// a row projection. That is cross-principal mask reuse: one viewer composing against another's
/// `M_auth` — a principal's authorised set (I2/I3) — presenting as a cache-hit-rate improvement.
/// The two pruners below must each pick the right `u64` out of this key, so the shape earns its
/// keep twice over.
///
/// # Two facts this key's safety rests on, neither of which the type can enforce
///
/// Both are load-bearing for the pruners and neither is asserted anywhere in the code, so they
/// are written down here, at the type they constrain:
///
/// 1. **`token_id` is never reused.** `Engine::authorise` draws it from a monotonic `AtomicU64`
///    that starts at zero on every `Engine::open`. That is safe *only because this cache is
///    in-memory and dies with the process.* A 10.7 s miss makes persisting it an obvious future
///    optimisation — and the day anyone does, token-id reuse across restarts becomes exactly the
///    cross-principal mask reuse the paragraph above describes. Persisting this cache requires
///    widening the key first.
/// 2. **`segments_version` is globally unique within a process**, which is why
///    [`RowProjectionCache::prune_generation`] may prune on it alone and ignore `Reclaimed::prefix`.
///    `crate::pins::check_publishable` refuses any publication that does not strictly increase it,
///    and design §10.2 makes the prefix name *be* the segment-set version. Relax that guard and
///    this pruner starts removing the wrong generation's entries.
///
/// A multi-segment slice would widen this with `seg_id` — N segments means N row spaces and
/// therefore N projections — and a named struct makes that additive.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct RowProjectionKey {
    /// The session's process-local identity (`Session::token_id`), never the bearer token itself.
    pub token_id: u64,
    /// The slice this projection addresses — its `Permutation` is what defines the row space.
    pub slice: String,
    /// The geometry generation the row space belongs to. A bundle swap changes it, and the old
    /// entries become [`RowProjectionCache::prune_generation`]'s work at drain-list reclaim; an
    /// *overlay* swap must not (I11).
    pub segments_version: u64,
}

/// A losing arrival's outcome: another caller is already building this key, and this call did not
/// wait for it. Carries nothing — the caller only needs to know to retry.
///
/// Distinct from `single_flight::Building` so the wrapper's callers depend on this module's
/// contract rather than on the generic cache's internals.
#[derive(Debug)]
pub(crate) struct CacheBusy;

impl CacheWeight for RowProjection {
    /// Serialised size — `O(containers touched)` and microseconds, the primitive design §10.4 names
    /// for exactly this.
    ///
    /// **It under-estimates in-memory footprint for array containers carrying capacity slack (up to
    /// ~2×), so the bound is approximate — but that caveat does not apply where it matters here.**
    /// At the ≥25%-coverage dense bound this cache is sized against (a *measured* 125.12 MB per
    /// entry at 10⁹), the mask is bitmap-container dominated and in-memory size equals serialised
    /// size. The caveat is stated here, at the site that computes the number, because the same claim
    /// is easy to reach for in `tessera-server::config` to justify a margin it does not explain.
    ///
    /// The floor applied on top of this (`single_flight::PER_ENTRY_FLOOR_BYTES`) is what stops the
    /// *opposite* error — a bound that charges a near-empty projection its true handful of bytes
    /// bounds no number of entries.
    fn cache_weight_bytes(&self) -> u64 {
        self.bitmap().get_serialized_size_in_bytes::<Portable>() as u64
    }
}

/// Cached row-space projections, keyed `(token_id, slice, segments_version)` — never recomputed on
/// the per-viewport path (shared-context constraint 8; see `crate::compose::RowProjection`'s doc
/// for the cost this avoids).
///
/// # Sizing: what the bound bounds, and the number an operator actually needs
///
/// `row_projection_cache_bytes` bounds the bytes **resident in this cache**. Peak process usage is
/// higher, and eviction is what creates the gap: every admitted request holds an
/// `Arc<RowProjection>` for its whole duration whether or not the cache still contains it, so
///
/// ```text
/// peak ≈ row_projection_cache_bytes + compute_admission × per_entry
/// ```
///
/// At the *measured* 125.12 MB per entry at 10⁹ and the branch's 48-way admission width, that
/// second term is ~6 GB — larger than some deployments' whole cache bound. **An operator sizing a
/// box from the config key alone will under-provision.** Stated here rather than at the config site
/// because this is where the second term's operand lives; `tessera-server`'s startup validation
/// refuses a bound below `expected_concurrent_sessions × per_entry`, which is a floor on the first
/// term and says nothing about the second.
pub(crate) struct RowProjectionCache {
    inner: SingleFlightCache<RowProjectionKey, RowProjection>,
}

impl RowProjectionCache {
    /// `bound_bytes` is the resident-byte ceiling. `u64::MAX` means "no bound" — the pre-Task-5
    /// behaviour, and what every non-server construction site gets until
    /// [`crate::Engine::set_cache_bounds`] is called.
    pub(crate) fn new(bound_bytes: u64) -> Self {
        RowProjectionCache {
            inner: SingleFlightCache::new(bound_bytes),
        }
    }

    /// See [`SingleFlightCache::set_bound_bytes`].
    pub(crate) fn set_bound_bytes(&self, bound_bytes: u64) {
        self.inner.set_bound_bytes(bound_bytes);
    }

    /// Slots currently held, `Building` and `Ready` both counted — a diagnostic, not a capacity
    /// bound. `Engine::row_projection_cache_len` publishes this, and its semantics are deliberately
    /// left alone: it is the observable behind "`Engine::item` must construct no projection, warm or
    /// cold", which counts slots in either state.
    pub(crate) fn len(&self) -> usize {
        self.inner.len()
    }

    /// The operator gauges — see [`CacheStats`]. Lock-free.
    pub(crate) fn stats(&self) -> CacheStats {
        self.inner.stats()
    }

    /// Look up `key`, building it on this call if nobody else already is.
    ///
    /// **Fallible on purpose, and `Err` does not mean failure.** `Err(CacheBusy)` means some other
    /// caller is building this key right now and this call declined to wait. `build` runs with no
    /// lock held and must be infallible — see [`SingleFlightCache::get_or_build`].
    pub(crate) fn get_or_build(
        &self,
        key: RowProjectionKey,
        build: impl FnOnce() -> RowProjection,
    ) -> Result<Arc<RowProjection>, CacheBusy> {
        self.inner
            .get_or_build(key, build)
            .map_err(|_busy| CacheBusy)
    }

    /// Remove every projection belonging to `token_id`. Called when a session is revoked.
    ///
    /// # Why this is safe, and what it is not
    ///
    /// **It only removes**, so it cannot widen a mask; its worst failure is a needless rebuild.
    /// What makes the *keys* safe to drop is that `token_id` is never reused (see
    /// [`RowProjectionKey`]'s fact 1), so no future session can produce a key this pass removes.
    ///
    /// **It is a memory-hygiene mechanism, not a disclosure control, and the two must not be
    /// blurred.** A request that authenticated before the revoke already holds its
    /// `Arc<SessionEntry>` and can re-insert this key *after* the prune has run; the entry it
    /// publishes is that same session's own projection over the same geometry, reachable by nobody
    /// else, and the byte bound reclaims it. The disclosure control for a revoked session is
    /// `AppState::authenticated_session` returning `BadCredential`, not this.
    ///
    /// **Revoke is also not the common retention path** — an *expired* session is 403'd but never
    /// removed from the registry, and nothing prunes it, so at the 3600 s default lifetime the
    /// overwhelming majority of dead sessions' entries are reclaimed by the byte bound rather than
    /// by this. Pruning on revoke makes reclamation timely; it does not make it the mechanism.
    ///
    /// # Cost, stated rather than argued
    ///
    /// This is an **O(n) pass under the request-path lock**, and `/session/revoke` is deliberately
    /// outside the admission gate (D13). At the designed configuration n is small — 16 entries at
    /// the 2 GiB default and the *measured* 125.12 MB per entry, a few thousand at fixture scale —
    /// and the pass is microseconds. The adversarial n is `bound / PER_ENTRY_FLOOR_BYTES` ≈ 4.2 M,
    /// where a pass is tens of milliseconds while every admitted request blocks on the same mutex.
    /// **Reaching it requires the session credential**: `authorise` is behind `check_bearer`, so it
    /// is not a viewer-plane exposure, and a holder of that shared secret can already call revoke
    /// in a loop. `CacheStats::prune_scanned` makes the real n observable rather than assumed. A
    /// secondary `token_id → keys` index would make the pass O(victims); it is declined here
    /// because a second index is a second bijection to keep in step — the failure
    /// `crate::single_flight`'s rule 1 exists to prevent — and the exposure above does not justify
    /// it. Recorded so the trade is visible rather than rediscovered.
    pub(crate) fn prune_token(&self, token_id: u64) -> usize {
        self.inner.retain_keys(|key| key.token_id != token_id)
    }

    /// Remove every projection built against `segments_version`. Called when the pin drain list
    /// reclaims that geometry.
    ///
    /// # Why reclaim and not the swap — the coupling is the point
    ///
    /// A pinned request still needs its generation's projection, and its `segments_version` is
    /// exactly the key a swap-triggered prune would delete. Between the swap and the reclaim the
    /// geometry is *still resolvable* — `PinManager::resolve_drained` finds it on the drain list —
    /// so pruning at the swap would delete the very key an outstanding pin is about to ask for, and
    /// charge that request a multi-second rebuild in the middle of an interaction. Tying cache
    /// lifetime to the rule that governs the geometry those entries describe is why this hangs off
    /// the drain list rather than off the swap.
    ///
    /// **The licence to prune is a `Reclaimed` value, not any particular method.** `Engine` prunes
    /// from every site that produces one — see `Engine::prune_reclaimed`, called from both
    /// `reclaim_pins` and `publish_geometry`, the latter producing them from *two* places (the
    /// drain-depth trim and its own reclaim pass). Coupling to `reclaim_pins` alone would leave two
    /// live routes reclaiming generations that are never pruned, and since nothing calls
    /// `reclaim_pins` periodically yet, those are in practice the routes that fire.
    ///
    /// Pruning on `segments_version` alone, ignoring `Reclaimed::prefix`, rests on
    /// [`RowProjectionKey`]'s fact 2.
    ///
    /// Same cost note as [`Self::prune_token`], with one difference in its favour: this runs on the
    /// lifecycle path, not on a request handler.
    pub(crate) fn prune_generation(&self, segments_version: u64) -> usize {
        self.inner
            .retain_keys(|key| key.segments_version != segments_version)
    }
}
