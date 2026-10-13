//! The row-projection cache seam: the key, the byte bound's arithmetic, and the pruner.
//!
//! A named wrapper around the generic [`SingleFlightCache`], carrying the policy that is specific
//! to projections: a byte bound, LRU eviction, and removal of entries whose key no request can
//! produce.
//!
//! The single-flight state machine, the eviction rules and the counted choke point live in
//! `mosaica-cache` and are argued there. What lives here is specific to projections: the key, the
//! weight function, the pruner's safety argument, and the arithmetic an operator needs to size a
//! box.
//!
//! # What removal can and cannot do (I3's cache half)
//!
//! Nothing here modifies a cached value — lifecycle §7 requires entries to be immutable and
//! "invalidation is key rotation, never mutation", and both eviction and pruning only ever
//! *remove*. That is what makes a rebuilt projection identical to the evicted one: the miss path
//! builds `RowProjection::new` over `ProjectionInputs` carrying the grant's frozen fragment, the
//! grant's keys and the *pinned* generation's postings, tiers, images and row space — all of
//! which the key names or the request pins. The route that build takes is priced from those same
//! inputs and every route returns the identical rows (`crate::projection::RowProjection::new`), so a
//! miss that prices differently from the build before it still produces what was evicted.
//! **There is no route by which a miss composes against a different mask than a hit**, which is the
//! property `eviction_never_widens_a_mask` exists to keep true.

use std::sync::Arc;

use croaring::Portable;
use rustc_hash::FxHashMap;

use mosaica_authz::FrozenFragment;
use mosaica_types::ShardId;

use crate::cancel::CancelToken;
use crate::projection::RowProjection;
use mosaica_cache::{CacheStats, CacheWeight, SingleFlightCache, WaitEnded};

/// The row-projection cache's key: one grant's projection of one view in one shard, at one
/// geometry. Every session with the same grant reads the same entry.
///
/// **Named fields, not a tuple**: a transposition of two of the `u64`s or the two digests would
/// still compile, and would hand one grant's projection to another.
///
/// # What this key's safety rests on, which the type cannot enforce
///
/// 1. **`grant` is the grant whole.** It is `Session::terms_digest`, SHA-256 over the grant's
///    sorted index keys or over a fixed input for `read-all`, and the projection is a function of
///    those keys and the shard's state alone: the fragment unions them, and the route is priced
///    from them. Nothing about the credential beyond the grant reaches the value, which is why
///    sessions holding different credentials may share it; anything per credential is read from
///    the session.
/// 2. **`segments_version` is globally unique within a process**, which is why
///    [`RowProjectionCache::prune_generations_below`]'s depth test reads it alone.
///    `crate::pins::check_publishable` refuses any publication that does not strictly increase it.
///    Relax that guard and this pruner starts removing the wrong generation's entries.
/// 3. **`prefix` and `identity` are carried, and are not redundant with `segments_version`.**
///    A compaction publishes a new prefix and a new shard identity, and an entry from before it
///    must never answer a request after it, should `segments_version` ever restart within one or
///    this cache outlive the process. The entries are in memory only.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct RowProjectionKey {
    /// The grant's digest (`Session::terms_digest`).
    pub grant: [u8; 32],
    /// The view this projection addresses — its `Permutation` is what defines the row space.
    pub view: String,
    /// The shard whose row space the projection is in.
    pub shard: ShardId,
    /// The shard's identity (`ShardParts::identity`), which a compaction moves.
    pub identity: [u8; 32],
    /// The geometry generation the row space belongs to. Entries more than
    /// [`KEEP_SUPERSEDED_GENERATIONS`] behind become
    /// [`RowProjectionCache::prune_generations_below`]'s work at the next publication; an
    /// *overlay* swap must not move it (I11).
    pub segments_version: u64,
    /// The prefix that geometry belongs to — see fact 3 above.
    pub prefix: String,
}

/// A losing arrival's outcome: another caller is already building this key, and this call did not
/// wait for it. Carries nothing — the caller only needs to know to retry.
///
/// Distinct from `mosaica_cache::Building` so the wrapper's callers depend on this module's
/// contract rather than on the generic cache's.
#[derive(Debug)]
pub(crate) struct CacheBusy;

/// Why a waiting caller ([`RowProjectionCache::get_or_derive_waiting`]) gave up: the wait budget
/// expired, or the client disconnected. Distinct from `mosaica_cache::WaitEnded` for the reason
/// [`CacheBusy`] is distinct from `mosaica_cache::Building`.
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

/// One grant's geometry in one shard's view for one generation: the mask fragment **and** the
/// projection taken over it, as a single value. Shared by every session with the grant.
///
/// **They are one entry because stale-serve would otherwise pair them wrongly.** A projection
/// derived from a stale fragment but inserted under the *new* `segments_version` key would pin the
/// grant's freshly flushed items invisible until the next publication. Carrying the pair makes the
/// mismatch unexpressible instead of forbidden, and gives the next derivation the fragment the
/// projection was taken over.
///
/// **What the refresh needs to produce the next one**, so a background pass needs no session
/// registry — the engine has none, sessions being values the server holds. `grant` is what the
/// [`mosaica_authz::FragmentCache`] is asked with. Nothing here is per credential: what is, such
/// as the identity key's credential digest, is read from the session.
pub(crate) struct SessionGeometry {
    /// The fragment `projection` was taken over — never the live one, unless they coincide.
    pub(crate) fragment: Arc<FrozenFragment>,
    pub(crate) projection: Arc<RowProjection>,
    /// What the fragment is the union of, the fragment cache's key component. A `read-all`
    /// grant's is every key, so the refresh rebuilds it from the keys the generation carries.
    pub(crate) grant: mosaica_authz::Grant,
}

impl CacheWeight for SessionGeometry {
    /// **The projection's bytes alone, and the omission is deliberate.** The fragment is an
    /// `Arc` shared with `FragmentCache` and charged against its own bound, so charging it here
    /// would bound the same bytes twice and shrink this cache to nothing at the operating point
    /// it is sized for. What this cache owns is the projection.
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
    /// The floor applied on top of this (`mosaica_cache::PER_ENTRY_FLOOR_BYTES`) is what stops the
    /// *opposite* error — a bound that charges a near-empty projection its true handful of bytes
    /// bounds no number of entries.
    fn cache_weight_bytes(&self) -> u64 {
        self.bitmap().get_serialized_size_in_bytes::<Portable>() as u64
    }
}

/// Cached row-space projections, one per (grant, view, shard, generation) — never recomputed on
/// the per-viewport path (shared-context constraint 8; see `crate::projection::RowProjection`'s doc
/// for the cost this avoids).
///
/// # Sizing: what the bound bounds, and the number an operator actually needs
///
/// `row_projection_cache_bytes` bounds the bytes **resident in this cache**, which holds a
/// projection per grant in use rather than per session. Peak process usage is
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
    /// the old bitmap unioned with the new extents' own contribution and with the rows, in the
    /// base and the covered extents, of the entities the new fragment holds and the old did not:
    /// a join row can have been published, or folded into the base, before the entity's own row
    /// flushed (`RowProjection::extend`). That is *equal to*, not merely close to, what
    /// `RowSpace::project` would return over the whole space. Four premises hold it up:
    ///
    /// 1. An entity the old fragment held keeps every row it had, and the new fragment holds it
    ///    too: a flush never removes a posting, and an id a fold freed lost its rows at that fold.
    /// 2. A flush never rewrites the base `permutation.bin` or any earlier extent.
    /// 3. The key names the grant, whose keys never change, so the fragment the projection is
    ///    taken over unions the same keys throughout.
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
            mosaica_cache::Peek::Ready(value) => Peek::Ready(value),
            mosaica_cache::Peek::Building => Peek::Building,
            mosaica_cache::Peek::Absent => Peek::Absent,
        }
    }

    /// The freshest fragment `grant` has a resident entry for in `shard`, if any — **what keeps
    /// the drill-down and the viewport from drifting apart under stale-serve.**
    ///
    /// `Engine::item` answers an entity-space question against a fragment, and `Engine::viewport`
    /// answers the row-space one against a projection taken over one. Serving a
    /// one-generation-stale projection means a drill-down at the live watermark could call an
    /// item visible while the viewport beside it drew no mark for it — the two enforcement
    /// representations drifting. Taking the fragment from the grant's freshest entry in the shard
    /// keeps the card with the viewport of the view that entry is for, and takes the drill-down off
    /// the per-publication fragment rebuild (a *measured* ~200 ms per grant). A session served
    /// another view a generation stale, beside a newer entry of the grant's, may find an item on
    /// its card that its viewport does not yet draw: an item its grant may see.
    ///
    /// **The fragment is not view-scoped**, so any of the grant's entries in `shard` answers: the
    /// fragment cache keys on the shard's identity, the satisfied terms and the watermark, and
    /// none is a view. Another shard's fragment is in another entity space and never answers. The
    /// freshest is taken because a later watermark is a strictly better answer to an entity-space
    /// question.
    ///
    /// **Per grant, never per entity** — the scan cost cannot depend on which identifier was
    /// asked for, which is Critical C-5's constant-time property.
    ///
    /// **Scoped to `prefix`**: across a fold a superseded entry carries a fragment built from the
    /// term index the fold replaced, and `prune_generations_below` keeps the generation
    /// immediately under the live one, which after a fold is a pre-fold entry.
    ///
    /// **And bounded below by `floor`**, the publication's retention floor, so the drill-down is
    /// never staler than rung 2 of the viewport beside it. The prune keeps a grant's newest entry
    /// below the floor as the refresh's base; answering from it would call an item absent that
    /// the viewport, finding neither rung, rebuilds and draws.
    pub(crate) fn freshest_fragment(
        &self,
        grant: &[u8; 32],
        shard: &crate::Shard,
        prefix: &str,
        floor: u64,
    ) -> Option<Arc<FrozenFragment>> {
        self.inner
            .ready_entries()
            .into_iter()
            .filter(|(key, _)| {
                key.grant == *grant
                    && key.shard == shard.id()
                    && key.identity == shard.identity
                    && key.prefix == prefix
                    && key.segments_version >= floor
            })
            .max_by_key(|(key, _)| key.segments_version)
            .map(|(_, geometry)| Arc::clone(&geometry.fragment))
    }

    /// Each grant's newest `Ready` entry per view and shard, as `(key, value)` — what the
    /// background refresh iterates.
    ///
    /// **O(cache residency), never O(sessions)** (decision 0035's shape, and 0044's D1): the
    /// refresh's whole cost model is that it is bounded by what is resident rather than by how
    /// many sessions exist, and this is where that becomes true. A grant with no resident entry
    /// is not refreshed and pays a build on its next request, which is establishment, not
    /// update-induced work.
    ///
    /// **Only the newest entry per grant, view and shard.** Retention leaves an older entry beside
    /// a newer one — the one-back entry beside the live one, or a kept base beside what a refresh
    /// has since derived from it — and deriving from both would produce the same key twice and
    /// count it twice.
    pub(crate) fn resident(&self) -> Vec<(RowProjectionKey, Arc<SessionGeometry>)> {
        let ready = self.inner.ready_entries();
        let newest = newest_per_lineage(&ready);
        let bases: Vec<bool> = ready
            .iter()
            .map(|(key, _)| newest.get(&lineage(key)) == Some(&key.segments_version))
            .collect();
        ready
            .into_iter()
            .zip(bases)
            .filter_map(|(entry, base)| base.then_some(entry))
            .collect()
    }

    /// Drop every projection.
    #[cfg(feature = "fault-injection")]
    pub(crate) fn clear(&self) {
        self.inner.retain_keys(|_| false);
    }

    /// Drop every projection built against a generation older than `floor`, keeping `floor` and
    /// everything above it, **and keeping each grant's newest projection of each view and shard in
    /// the live prefix whatever its generation**. Called by the publication, with
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
    /// new generation's projection for a grant is the old one plus the new extent's rows, so the
    /// superseded entry is both the input the background refresh extends and the entry rung 2 of
    /// `Engine::session_geometry`'s ladder serves while the refresh runs. Deleting it at the
    /// instant of the swap deletes both, and every grant pays the full rebuild at every tick.
    ///
    /// **The newest entry is kept because it is the refresh's only base.** Two publications can
    /// land before the refresh for the first has taken its snapshot, a flush and then a merge
    /// under load. The depth alone would then drop the entry both refreshes derive from, both
    /// would find nothing, and every resident grant would pay the full rebuild at its next
    /// request. Keeping it lets a refresh derive across the gap: the rungs of `crate::refresh` are
    /// exact at any distance within a prefix. A kept entry survives until a newer `Ready` entry
    /// for its grant, view and shard, an eviction or a fold's publication removes it. Nothing
    /// reads it but the refresh — rung 2 and [`Self::freshest_fragment`] look no further back
    /// than the floor — so it is the first thing the byte bound evicts.
    ///
    /// **Only the live prefix.** Across a fold a kept entry is only a rebuild's input, and a merge
    /// landing while the fold's pass is still rebuilding would rebuild every grant that pass has
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
    /// is small (16 entries at the 2 GiB default and the *measured* 125.12 MB per entry); at its
    /// adversarial n, `bound / PER_ENTRY_FLOOR_BYTES`, the clone costs more than the removal,
    /// and both run on the publication path, not on a request handler.
    pub(crate) fn prune_generations_below(&self, floor: u64, live_prefix: &str) -> usize {
        let ready = self.inner.ready_entries();
        let newest = newest_per_lineage(&ready);
        self.inner.retain_keys(|key| {
            key.segments_version >= floor
                || (key.prefix == live_prefix
                    && newest.get(&lineage(key)) == Some(&key.segments_version))
        })
    }
}

/// What a refresh carries an entry forward along: its grant, view and shard.
type Lineage<'a> = (&'a [u8; 32], &'a str, ShardId);

fn lineage(key: &RowProjectionKey) -> Lineage<'_> {
    (&key.grant, key.view.as_str(), key.shard)
}

/// The newest generation of each lineage among `entries`.
fn newest_per_lineage(
    entries: &[(RowProjectionKey, Arc<SessionGeometry>)],
) -> FxHashMap<Lineage<'_>, u64> {
    let mut newest: FxHashMap<Lineage<'_>, u64> = FxHashMap::default();
    for (key, _) in entries {
        let version = newest.entry(lineage(key)).or_insert(0);
        *version = (*version).max(key.segments_version);
    }
    newest
}

/// How many superseded generations' projections the cache keeps after a publication.
///
/// **One, and the number is the patch's input rather than a margin.** A flush appends, so the
/// generation immediately below the live one holds exactly the projection the next request's patch
/// derives from; a second one back is derivable from the first and is kept only while it is its
/// grant's newest in the live prefix ([`RowProjectionCache::prune_generations_below`]). Raising
/// this buys nothing and costs a *measured* 125.12 MB per entry per grant at 10⁹; lowering it to
/// zero forfeits the patch and reinstates the full rebuild at every tick.
pub(crate) const KEEP_SUPERSEDED_GENERATIONS: u64 = 1;
