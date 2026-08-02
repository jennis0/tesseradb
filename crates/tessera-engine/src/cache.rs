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
/// 3. **`prefix` is carried, and is not redundant with `segments_version`.** Within one process
///    `segments_version` is a sufficient discriminator (fact 2), and that is what the pruner keys
///    on. `prefix` is here for the case fact 2 does not cover: a compaction publishes a new prefix,
///    and if a future change ever let `n` restart within one — or let this cache outlive the
///    process — a prefix-A projection would answer a prefix-B request, which is I11's *"selects
///    arbitrary rows"*. It costs a string comparison on a path that already hashes one.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct RowProjectionKey {
    /// The session's process-local identity (`Session::token_id`), never the bearer token itself.
    pub token_id: u64,
    /// The slice this projection addresses — its `Permutation` is what defines the row space.
    pub slice: String,
    /// The geometry generation the row space belongs to. A bundle swap changes it, and entries
    /// more than [`KEEP_SUPERSEDED_GENERATIONS`] behind become
    /// [`RowProjectionCache::prune_generations_below`]'s work at the next publication; an *overlay*
    /// swap must not (I11).
    pub segments_version: u64,
    /// The prefix that geometry belongs to — see fact 3 above for why this is here when
    /// `segments_version` already discriminates within a process.
    pub prefix: String,
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
    /// `bound_bytes` is the resident-byte ceiling. `u64::MAX` means "no bound" — the unbounded
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

    /// Look up `key`, building it on this call if nobody else already is — **deriving from the
    /// immediately-preceding generation's entry when it is still resident**.
    ///
    /// **Fallible on purpose, and `Err` does not mean failure.** `Err(CacheBusy)` means some other
    /// caller is building this key right now and this call declined to wait. `make` runs with no
    /// lock held and must be infallible — see [`SingleFlightCache::get_or_derive`].
    ///
    /// # Why this exists, in one number
    ///
    /// Every flush publishes a new `segments_version`, so every flush rotates **every** session's
    /// projection key. A miss is a full `Permutation::project` over the session's whole fragment —
    /// a *measured* 10.7 s at 10⁹ — and flush §9 states the consequence plainly: *"the fallback is
    /// not an edge case but the steady state: a full 10.7 s projection per session per tick,
    /// synchronised across the session population"*. Without this, the flush tick's real floor is
    /// that rebuild, not any configured knob.
    ///
    /// # Why the patch equals a rebuild
    ///
    /// A flush **appends**: the new extent's rows begin exactly where row space ended, so they are
    /// disjoint from every row the previous projection contains. The new projection is therefore
    /// the old bitmap unioned with the new extents' own contribution
    /// (`RowSpace::project_extents_from`), and that is *equal to*, not merely close to, what
    /// `RowSpace::project` would return over the whole space. Four premises hold it up, each a
    /// thing this design must maintain rather than happen to have (flush §3.4):
    ///
    /// 1. The flushed entity range is contiguous, disjoint from everything below, and entirely at
    ///    or above the pre-flush watermark — I9's append-only allocation.
    /// 2. A flush never rewrites the base `permutation.bin` or any earlier extent.
    /// 3. The session's `satisfied` set is fixed at authorise and never re-resolved, so the
    ///    fragment the projection is taken over is the same one throughout.
    /// 4. `segments_version` strictly increases, so the source key names exactly one geometry
    ///    (`crate::geometry::check_publishable`).
    ///
    /// **Premise 2 is what a merge breaks, and a merge is why the source key must be exact.** A
    /// merge permutes row space within the merged span (`geometry-pinning.md` §4), so a projection
    /// from before it cannot be extended into one from after it. The caller supplies
    /// `derive_from` as the generation exactly one below the target and nothing else, and a merge —
    /// which advances `segments_version` like any other publication — therefore presents no source
    /// whose row space it has permuted **provided the caller derives only across an append**. That
    /// is the caller's obligation, and `Engine::viewport` discharges it by deriving only when the
    /// extents it is adding are the ones the source's row space does not already contain.
    ///
    /// A source miss is not a failure: `derive` falls back to the full build, so the answer is
    /// identical either way and only the cost differs.
    pub(crate) fn get_or_derive(
        &self,
        key: RowProjectionKey,
        derive_from: Option<&RowProjectionKey>,
        make: impl FnOnce(Option<&RowProjection>) -> RowProjection,
    ) -> Result<Arc<RowProjection>, CacheBusy> {
        self.inner
            .get_or_derive(key, derive_from, make)
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

    /// Drop every projection built against a generation older than `floor`, keeping `floor` and
    /// everything above it. Called by the publication, with
    /// `live - `[`KEEP_SUPERSEDED_GENERATIONS`].
    ///
    /// # Why a retention depth rather than a reclaim hook
    ///
    /// This used to hang off the pin drain list: the licence to prune a generation was a
    /// `Reclaimed` value, produced when a drain entry expired. With pins deleted
    /// (`geometry-pinning.md`) there is no drain list, and what remains is the thing the coupling
    /// was standing in for — **an explicit N-generations-back policy, stated where the cache is
    /// bounded**.
    ///
    /// **Pruning at the swap, with depth zero, would be wrong**, and it is worth being precise
    /// about why since the pin argument for that is gone. A flush *extends* row space: the new
    /// generation's projection for a session is the old one plus the new extent's rows, so the
    /// superseded entry is the input to the patch that avoids a *measured* 10.7 s rebuild at 10⁹.
    /// Deleting it at the instant of the swap deletes the input before any request can use it,
    /// and every session pays the full rebuild at every tick — which is exactly the steady-state
    /// cost flush §9 names. The depth is what keeps the input alive for one tick.
    ///
    /// Pruning on `segments_version` alone, ignoring the prefix, rests on [`RowProjectionKey`]'s
    /// fact 2 — and a merge is why it must: row ids inside a merged span name different entities
    /// afterwards, so the prefix is *not* a safe discriminator and `segments_version` is
    /// (`geometry-pinning.md` §4).
    ///
    /// Same cost note as [`Self::prune_token`], with one difference in its favour: this runs on the
    /// publication path, not on a request handler.
    pub(crate) fn prune_generations_below(&self, floor: u64) -> usize {
        self.inner.retain_keys(|key| key.segments_version >= floor)
    }
}

/// How many superseded generations' projections the cache keeps after a publication.
///
/// **One, and the number is the patch's input rather than a margin.** A flush appends, so the
/// generation immediately below the live one holds exactly the projection the next request's patch
/// derives from; a second one back is derivable from the first and is never consulted. Raising this
/// buys nothing and costs a *measured* 125.12 MB per entry per session at 10⁹; lowering it to zero
/// forfeits the patch and reinstates the full rebuild at every tick.
pub(crate) const KEEP_SUPERSEDED_GENERATIONS: u64 = 1;
