//! The row-projection cache seam: the key, the byte bound's arithmetic, and the two pruners.
//!
//! A named wrapper around the generic [`SingleFlightCache`], carrying the policy that is specific
//! to projections: a byte bound, LRU eviction, and removal of entries whose key no request can
//! produce.
//!
//! The single-flight state machine, the eviction rules and the counted choke point live in
//! `tessera-cache` and are argued there. What lives here is specific to projections: the key, the
//! weight function, the two pruners' safety argument, and the arithmetic an operator needs to size
//! a box.
//!
//! # What removal can and cannot do (I3's cache half)
//!
//! Nothing here modifies a cached value — lifecycle §7 requires entries to be immutable and
//! "invalidation is key rotation, never mutation", and both eviction and pruning only ever
//! *remove*. That is what makes a rebuilt projection identical to the evicted one: the miss path
//! builds `RowProjection::new` over `ProjectionInputs` carrying the session's own frozen fragment,
//! its satisfied terms and the *pinned* generation's postings, tiers, images and row space — all of
//! which the key names or the request pins. The route that build takes is priced from those same
//! inputs and every route returns the identical rows (`crate::projection::RowProjection::new`), so a
//! miss that prices differently from the build before it still produces what was evicted.
//! **There is no route by which a miss composes against a different mask than a hit**, which is the
//! property `eviction_never_widens_a_mask` exists to keep true.

use std::sync::Arc;

use croaring::Portable;
use rustc_hash::{FxHashMap, FxHashSet};

use tessera_authz::FrozenFragment;
use tessera_types::TermId;

use crate::cancel::CancelToken;
use crate::projection::RowProjection;
use tessera_cache::{CacheStats, CacheWeight, SingleFlightCache, WaitEnded};

/// `(token_id, view, segments_version)` — the row-projection cache's key (shared-context
/// constraint 8). `token_id` rather than the token string so the cache never has to hash or
/// compare a full bearer token.
///
/// **Named fields, not a tuple, and the reason is a disclosure.** Two of the
/// three components are `u64`, so as `(u64, String, u64)` a transposition at the construction site
/// compiles, runs, and keys every session's projection on `(segments_version, view, token_id)` —
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
///    in-memory and dies with the process.* Persisting it across restarts would break that, and
///    the day anyone does, token-id reuse becomes exactly the cross-principal mask reuse the
///    paragraph above describes. Persisting this cache requires widening the key first.
///
///    **The reason to want it has largely gone.** This read "a 10.7 s miss makes persisting it an
///    obvious future optimisation" when a miss cost that; a miss is now a measured 1 277 ms
///    (`probes/2026-08-14-project-decomposition/`), which is an ordinary cold start rather than
///    something worth carrying a disclosure hazard to avoid. The hazard did not shrink with the
///    cost, so the trade moved decisively one way.
/// 2. **`segments_version` is globally unique within a process**, which is why
///    [`RowProjectionCache::prune_generations_below`]'s depth test reads it alone.
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
    /// The view this projection addresses — its `Permutation` is what defines the row space.
    pub view: String,
    /// The geometry generation the row space belongs to. A bundle swap changes it, and entries
    /// more than [`KEEP_SUPERSEDED_GENERATIONS`] behind become
    /// [`RowProjectionCache::prune_generations_below`]'s work at the next publication, which keeps
    /// only a session's newest in the live prefix; an *overlay* swap must not (I11).
    pub segments_version: u64,
    /// The prefix that geometry belongs to — see fact 3 above for why this is here when
    /// `segments_version` already discriminates within a process.
    pub prefix: String,
}

/// A losing arrival's outcome: another caller is already building this key, and this call did not
/// wait for it. Carries nothing — the caller only needs to know to retry.
///
/// Distinct from `tessera_cache::Building` so the wrapper's callers depend on this module's
/// contract rather than on the generic cache's.
#[derive(Debug)]
pub(crate) struct CacheBusy;

/// Why a waiting caller ([`RowProjectionCache::get_or_derive_waiting`]) gave up: the wait budget
/// expired, or the client disconnected. Distinct from `tessera_cache::WaitEnded` for the reason
/// [`CacheBusy`] is distinct from `tessera_cache::Building`.
#[derive(Debug)]
pub(crate) enum CacheWaitEnded {
    Budget,
    Cancelled,
}

/// What [`RowProjectionCache::peek`] found. **Three states, not two**: a request under decision
/// 0044 answers `Building` and `Absent` differently — the first means a refresh or a racer is
/// producing this key and the request should fall back rather than start a second build, the
/// second means nothing is coming and it may build.
pub(crate) enum Peek {
    Ready(Arc<SessionGeometry>),
    Building,
    Absent,
}

/// One session's geometry for one generation: the mask fragment **and** the projection taken over
/// it, as a single value.
///
/// **They are one entry because stale-serve breaks the ordering that used to couple them**
/// (decision 0044; review finding F5). Until stale-serve, `Engine::viewport` resolved the fragment
/// first and always at the live watermark, so the projection it then built or derived was
/// necessarily over that fragment — a coupling held by request ordering and by nothing in the
/// types. Serving a one-generation-stale projection deliberately breaks that ordering: a
/// projection derived from a stale fragment but inserted under the *new* `segments_version` key
/// would pin the session's freshly flushed items invisible until the next publication — fail-closed,
/// and a silent falsification of the ack→visibility bound. Carrying the pair makes the mismatch
/// unexpressible instead of forbidden.
///
/// **What the refresh needs to produce the next one**, so a background pass needs no session
/// registry — the engine has none, sessions being values the server holds. `satisfied_sorted` is
/// what the [`tessera_authz::FragmentCache`] is asked with; `auth_data_hash` is a digest of the
/// credential, never the credential.
pub(crate) struct SessionGeometry {
    /// The fragment `projection` was taken over — never the live one, unless they coincide.
    pub(crate) fragment: Arc<FrozenFragment>,
    pub(crate) projection: Arc<RowProjection>,
    /// The credential's granted terms, sorted — the fragment cache's key component.
    pub(crate) satisfied_sorted: Arc<Vec<TermId>>,
    /// `sha256(auth_data)`, part of the mask identity.
    pub(crate) auth_data_hash: [u8; 32],
}

impl CacheWeight for SessionGeometry {
    /// **The projection's bytes alone, and the omission is deliberate.** The fragment is an
    /// `Arc` shared with the session that authorised it and with `FragmentCache`'s own bound, so
    /// charging it here would bound the same bytes twice and shrink this cache to nothing at the
    /// operating point it is sized for. What this cache owns is the projection.
    fn cache_weight_bytes(&self) -> u64 {
        self.projection.cache_weight_bytes()
    }
}

impl CacheWeight for RowProjection {
    /// Serialised size — `O(containers touched)` and microseconds, the primitive design §10.4 names
    /// for exactly this.
    ///
    /// **It under-estimates in-memory footprint for array containers carrying capacity slack (up to
    /// ~2×), so the bound is approximate — but that caveat does not apply where it matters here.**
    /// At the ≥25%-coverage dense bound this cache is sized against (a *measured* 125.12 MB per
    /// entry at 10⁹), the mask is bitmap-container dominated and in-memory size equals serialised
    /// size.
    ///
    /// **A projection is run-optimised at construction** (`crate::projection::RowProjection::from_rows`),
    /// so a grant covering runs of row space is charged the run containers it holds rather than the
    /// bitmap containers it would otherwise hold. That moves the charge down and never up — a
    /// container is converted only where the run form is smaller — so the 125.12 MB above stays an
    /// upper bound at that coverage, and an entry that runs well is charged what it costs.
    ///
    /// The floor applied on top of this (`tessera_cache::PER_ENTRY_FLOOR_BYTES`) is what stops the
    /// *opposite* error — a bound that charges a near-empty projection its true handful of bytes
    /// bounds no number of entries.
    fn cache_weight_bytes(&self) -> u64 {
        self.bitmap().get_serialized_size_in_bytes::<Portable>() as u64
    }
}

/// Cached row-space projections, keyed `(token_id, view, segments_version)` — never recomputed on
/// the per-viewport path (shared-context constraint 8; see `crate::projection::RowProjection`'s doc
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
/// box from the config key alone will under-provision.**
pub(crate) struct RowProjectionCache {
    inner: SingleFlightCache<RowProjectionKey, SessionGeometry>,
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

    /// Claim `key`'s slot and produce its value, or refuse if someone already has it.
    ///
    /// **One caller, and it is not the request path.** The background refresh (`crate::refresh`)
    /// calls this at every geometry publication, once per resident key, and must not wait — its
    /// call site carries the argument. The request path takes
    /// [`Self::get_or_derive_waiting`] (decision 0058) at the last rung of
    /// `Engine::session_geometry`'s ladder; in steady state it reads through [`Self::peek`] and
    /// claims nothing.
    ///
    /// **Fallible on purpose, and `Err` does not mean failure.** `Err(CacheBusy)` means some other
    /// caller is building this key right now and this call declined to wait. `make` runs with no
    /// lock held and must be infallible — see [`SingleFlightCache::get_or_derive`].
    ///
    /// # Why the patch equals a rebuild
    ///
    /// A flush **appends**: the new extent's rows begin exactly where row space ended, so they are
    /// disjoint from every row the previous projection contains. The new projection is therefore
    /// the old bitmap unioned with the new extents' own contribution
    /// (`RowSpace::project_extents_from`), and that is *equal to*, not merely close to, what
    /// `RowSpace::project` would return over the whole space. Four premises hold it up, each a
    /// thing this design must maintain rather than happen to have (write-path §4.6):
    ///
    /// 1. The flushed entity range is contiguous, disjoint from everything below, and entirely at
    ///    or above the pre-flush watermark — I9's append-only allocation.
    /// 2. A flush never rewrites the base `permutation.bin` or any earlier extent.
    /// 3. The session's `satisfied` set is fixed at authorise and never re-resolved, so the
    ///    fragment the projection is taken over is the same one throughout.
    /// 4. `segments_version` strictly increases, so the source key names exactly one geometry
    ///    (`crate::geometry::check_publishable`).
    ///
    /// **Premise 2 is what a merge breaks.** A merge permutes row space within the merged span
    /// (`geometry-pinning.md` §4), so a projection from before it cannot be extended into one from
    /// after it. `RowProjection::extends_to` is the predicate that decides, and it is exact rather
    /// than heuristic — the refresh checks it before extending and falls to a full build
    /// otherwise, and the request path checks it before serving stale. The answer is identical
    /// either way; only the cost differs.
    pub(crate) fn get_or_derive(
        &self,
        key: RowProjectionKey,
        derive_from: Option<&RowProjectionKey>,
        make: impl FnOnce(Option<&SessionGeometry>) -> SessionGeometry,
    ) -> Result<Arc<SessionGeometry>, CacheBusy> {
        self.inner
            .get_or_derive(key, derive_from, make)
            .map_err(|_busy| CacheBusy)
    }

    /// [`Self::get_or_derive`], except that finding a build in flight parks on it and takes its
    /// result rather than refusing — decision 0058, and the route the request path takes. The
    /// value-equality argument below is unchanged: a waiter is served exactly what the winner
    /// built, which is what this method's own contract already required of any two callers of the
    /// same key.
    pub(crate) fn get_or_derive_waiting(
        &self,
        key: RowProjectionKey,
        derive_from: Option<&RowProjectionKey>,
        cancel: &CancelToken,
        make: impl FnOnce(Option<&SessionGeometry>) -> SessionGeometry,
    ) -> Result<Arc<SessionGeometry>, CacheWaitEnded> {
        self.inner
            .get_or_derive_waiting(key, derive_from, cancel, make)
            .map_err(|ended| match ended {
                WaitEnded::Budget => CacheWaitEnded::Budget,
                WaitEnded::Cancelled => CacheWaitEnded::Cancelled,
            })
    }

    /// See [`SingleFlightCache::set_wait_budget_ms`].
    pub(crate) fn set_wait_budget_ms(&self, wait_budget_ms: u64) {
        self.inner.set_wait_budget_ms(wait_budget_ms);
    }

    /// Look `key` up **without claiming its slot** — the stale-serve path's read.
    ///
    /// [`Self::get_or_derive`] cannot be used for this: a miss there inserts `Building` and
    /// commits the caller to producing a value, which is exactly what a request under decision
    /// 0044 must *not* do when a background refresh is about to. This reads and touches recency,
    /// and nothing else.
    pub(crate) fn peek(&self, key: &RowProjectionKey) -> Peek {
        match self.inner.peek(key) {
            tessera_cache::Peek::Ready(value) => Peek::Ready(value),
            tessera_cache::Peek::Building => Peek::Building,
            tessera_cache::Peek::Absent => Peek::Absent,
        }
    }

    /// The freshest fragment this token has a resident entry for, if any — **what keeps the
    /// drill-down and the viewport from drifting apart under stale-serve.**
    ///
    /// `Engine::item` answers an entity-space question against a fragment, and `Engine::viewport`
    /// answers the row-space one against a projection taken over one. Until stale-serve both
    /// resolved the fragment at the live watermark, so they necessarily agreed. Serving a
    /// one-generation-stale projection breaks that: a drill-down at the live watermark would call
    /// an item visible while the viewport beside it drew no mark for it — the two enforcement
    /// representations drifting, which write-path §14's obligation 27 forbids. Taking the fragment
    /// from the same entry the viewport serves restores the agreement by construction, and takes
    /// the drill-down off the per-publication fragment rebuild at the same time (a *measured*
    /// ~200 ms per credential — `probes/2026-08-04-refresh-ladder/`).
    ///
    /// **The fragment is not view-scoped**, so any of this token's entries answers: the fragment
    /// cache keys on the satisfied terms and the watermark, and neither is a view. The freshest is
    /// taken because a later watermark is a strictly better answer to an entity-space question.
    ///
    /// **Per token, never per entity** — the scan cost cannot depend on which identifier was
    /// asked for, which is Critical C-5's constant-time property.
    ///
    /// **Scoped to `prefix`, and taking the max by `segments_version` alone was a pre-fold fragment
    /// holder** (compaction §4). Within one prefix the version is a sufficient discriminator and
    /// the freshest entry is the best answer; across a fold it is not, because a fold rewrites the
    /// term index and every entry from the superseded prefix carries a fragment built from it.
    /// Both prunes miss those entries in the window that matters — `prune_generations_below` keeps
    /// the generation immediately under the live one, which after a fold is a pre-fold entry — so
    /// the scoping is here, at the read, rather than arranged for by eviction.
    ///
    /// **And bounded below by `floor`**, the publication's retention floor, so the drill-down is
    /// never staler than rung 2 of the viewport beside it. The prune keeps a session's newest
    /// entry below the floor as the refresh's base; answering from it would call an item absent
    /// that the viewport, finding neither rung, rebuilds and draws.
    pub(crate) fn freshest_fragment(
        &self,
        token_id: u64,
        prefix: &str,
        floor: u64,
    ) -> Option<Arc<FrozenFragment>> {
        self.inner
            .ready_entries()
            .into_iter()
            .filter(|(key, _)| {
                key.token_id == token_id && key.prefix == prefix && key.segments_version >= floor
            })
            .max_by_key(|(key, _)| key.segments_version)
            .map(|(_, geometry)| Arc::clone(&geometry.fragment))
    }

    /// Every `Ready` entry, as `(key, value)` — what the background refresh iterates.
    ///
    /// **O(cache residency), never O(sessions)** (decision 0035's shape, and 0044's D1): the
    /// refresh's whole cost model is that it is bounded by what is resident rather than by how
    /// many sessions exist, and this is where that becomes true. A session with no resident entry
    /// is not refreshed and pays a build on its next request, which is establishment, not
    /// update-induced work.
    pub(crate) fn resident(&self) -> Vec<(RowProjectionKey, Arc<SessionGeometry>)> {
        self.inner.ready_entries()
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
    /// **Revoke is not the common retention path; expiry is.** Most sessions are never revoked —
    /// they reach the 3600 s default lifetime and are refused. The registry sweeps those out on the
    /// next authorisation and hands their token ids to [`Engine::prune_tokens`], which is this
    /// removal in its batched form, so an expired session's entries go the same way a revoked
    /// one's do (`tessera_server::state::SessionRegistry`). What the byte bound is left to reclaim
    /// is the residue between sweeps, bounded at `2 × live` sessions.
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
    /// in a loop. `CacheStats::prune_scanned` makes the real n observable rather than assumed.
    ///
    /// **The sweep's batch pays the pass once, and not on the reactor.** [`Self::prune_tokens`]
    /// walks n for the whole batch rather than n per victim, and `/session/authorise` hands it to
    /// `spawn_blocking`, so a batch of expired sessions costs one pass on a blocking thread rather
    /// than `victims` passes on the thread answering requests. A
    /// secondary `token_id → keys` index would make the pass O(victims); it is declined here
    /// because a second index is a second bijection to keep in step — the failure `tessera-cache`'s
    /// rule 1 exists to prevent — and the exposure above does not justify it.
    pub(crate) fn prune_token(&self, token_id: u64) -> usize {
        self.inner.retain_keys(|key| key.token_id != token_id)
    }

    /// The same removal for a set of sessions, in **one** pass.
    ///
    /// The expiry sweep drops a batch — up to `2 × live` sessions at once
    /// (`tessera_server::state::SessionRegistry`) — and a loop over [`Self::prune_token`] would
    /// take this cache's mutex once per session and walk every surviving key each time. One pass
    /// with a set membership test is the same work for one session and O(n) rather than
    /// O(n × victims) for a batch, which is what keeps the sweep's cost off the request path's
    /// lock. Cost otherwise as [`Self::prune_token`] states it.
    pub(crate) fn prune_tokens(&self, token_ids: &FxHashSet<u64>) -> usize {
        self.inner
            .retain_keys(|key| !token_ids.contains(&key.token_id))
    }

    /// Drop every projection built against a generation older than `floor`, keeping `floor` and
    /// everything above it, **and keeping each session's newest projection of each view in the
    /// live prefix whatever its generation**. Called by the publication, with
    /// `live - `[`KEEP_SUPERSEDED_GENERATIONS`] and the live generation's prefix.
    ///
    /// # Why a retention depth rather than a reclaim hook
    ///
    /// This used to hang off the pin drain list: the licence to prune a generation was a
    /// `Reclaimed` value, produced when a drain entry expired. With pins deleted
    /// (`geometry-pinning.md`) there is no drain list, and what remains is the thing the coupling
    /// was standing in for — **an explicit N-generations-back policy, stated where the cache is
    /// bounded**.
    ///
    /// **Pruning at the swap, with depth zero, would be wrong.** A flush *extends* row space: the
    /// new generation's projection for a session is the old one plus the new extent's rows, so the
    /// superseded entry is both the input the background refresh extends and the entry rung 2 of
    /// `Engine::session_geometry`'s ladder serves while the refresh runs. Deleting it at the
    /// instant of the swap deletes both, and every session pays the full rebuild at every tick.
    ///
    /// **The newest entry is kept because it is the refresh's only base.** Two publications can
    /// land before the refresh for the first has taken its snapshot, a flush and then a merge
    /// under load. The depth alone would then drop the entry both refreshes derive from, both
    /// would find nothing, and every resident session would pay the full rebuild at its next
    /// request. Keeping it lets a refresh derive across the gap: the rungs of `crate::refresh` are
    /// exact at any distance within a prefix. A kept entry survives until a newer `Ready` entry
    /// for its session and view, an eviction, its token's prune or a fold's publication removes
    /// it. Nothing reads it but the refresh — rung 2 and [`Self::freshest_fragment`] look no
    /// further back than the floor — so it is the first thing the byte bound evicts.
    ///
    /// **Only the live prefix.** Across a fold a kept entry is only a rebuild's input, and a merge
    /// landing while the fold's pass is still rebuilding would rebuild every session that pass has
    /// not reached a second time, holding them at 429 behind a pass longer than the rebuild it
    /// saves. So across a fold the depth alone applies, as it did before.
    ///
    /// The depth test reads `segments_version` alone, ignoring the prefix, and rests on
    /// [`RowProjectionKey`]'s fact 2 — and a merge is why it must: row ids inside a merged span
    /// name different entities afterwards, so the prefix is *not* a safe discriminator and
    /// `segments_version` is (`geometry-pinning.md` §4). Only `Ready` entries count as newer: a
    /// build in flight may yet fail, and the entry it derives from is then still the base.
    ///
    /// # Cost
    ///
    /// Two passes under the request path's lock: [`SingleFlightCache::ready_entries`] clones every
    /// ready key and its `Arc`, then the removal walks every key. At the designed configuration n
    /// is small, as [`Self::prune_token`] states; at its adversarial n the clone costs more than
    /// the removal, and both run on the publication path, not on a request handler.
    pub(crate) fn prune_generations_below(&self, floor: u64, live_prefix: &str) -> usize {
        let ready = self.inner.ready_entries();
        let mut newest: FxHashMap<(u64, &str), u64> = FxHashMap::default();
        for (key, _) in &ready {
            let version = newest.entry((key.token_id, key.view.as_str())).or_insert(0);
            *version = (*version).max(key.segments_version);
        }
        self.inner.retain_keys(|key| {
            key.segments_version >= floor
                || (key.prefix == live_prefix
                    && newest.get(&(key.token_id, key.view.as_str()))
                        == Some(&key.segments_version))
        })
    }
}

/// How many superseded generations' projections the cache keeps after a publication.
///
/// **One, and the number is the patch's input rather than a margin.** A flush appends, so the
/// generation immediately below the live one holds exactly the projection the next request's patch
/// derives from; a second one back is derivable from the first and is kept only while it is its
/// session's newest in the live prefix ([`RowProjectionCache::prune_generations_below`]). Raising
/// this buys nothing and costs a *measured* 125.12 MB per entry per session at 10⁹; lowering it to
/// zero forfeits the patch and reinstates the full rebuild at every tick.
pub(crate) const KEEP_SUPERSEDED_GENERATIONS: u64 = 1;
