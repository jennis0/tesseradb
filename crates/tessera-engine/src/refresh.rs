//! **The background refresh** — the mechanism decision 0044's D1 rules, and the whole of what
//! keeps a geometry publication off the request thread.
//!
//! ## The budget, and why nothing inline meets it
//!
//! 0044's D1, quoted from the owner: a client pays *"only a small penalty (< 0.2 ms), none, or we
//! need to ensure that longer (multisecond) delays are much rarer than the flush/merge rate"*.
//! Under continuous ingest the tick fires forever, so anything charged to a request thread per
//! publication is a **steady-state** cost. The two candidates were measured at 10⁹ over a wide
//! grant (`probes/2026-08-04-refresh-ladder/`):
//!
//! | | measured |
//! |---|---|
//! | full projection rebuild | 4 550 ms |
//! | the patch's bitmap clone alone | 40.9 ms |
//! | the union the patch then does | 0.24 ms |
//! | span rebase over a 4-extent merge | 44.6 ms |
//! | fragment rebuild per credential | ~200 ms |
//!
//! The patch is 40.9 ms because the cached value is immutable (lifecycle §7 — "invalidation is key
//! rotation, never mutation"), so a patch must **copy** before it unions; the copy is the cost and
//! no arrangement of an inline patch escapes it. Two orders over the budget. So the work moves
//! here, and the request path's face is [`crate::viewport`]'s three-rung ladder.
//!
//! ## What this pass does, and what bounds it
//!
//! One pool task per geometry publication, over **resident cache keys** — O(cache residency),
//! never O(sessions) (decision 0035's shape). At the ~16 wide-grant entries a 2 GiB bound holds,
//! one round is ~0.7 s of pool time. A session with no resident entry is not refreshed and builds
//! on its next request, which is establishment rather than update-induced work.
//!
//! **It needs no session registry, and the engine has none** — a `Session` is a value the server
//! holds. Everything a refresh needs is in the entry it is refreshing: the fragment cache's key
//! halves ride on [`crate::cache::SessionGeometry`], and the projection comes from the previous
//! entry by the same `extend` the request path used to do.
//!
//! ## The two orderings that are load-bearing
//!
//! **`refresh_in_flight` is set before the swap**, by the executor, not here. A racer landing
//! between the swap and this task's first insert must see it set, or after a merge it pays the
//! measured 4 550 ms rebuild inline — which is the 429 residual's whole point (review finding F5,
//! 2026-08-04).
//!
//! **The fragment and the projection are produced together, into one entry.** Resolving them
//! separately is what stale-serve breaks: a projection built over a live fragment but published
//! under a key a stale-serving request will later read pairs two artefacts from different
//! watermarks. See [`crate::cache::SessionGeometry`].

use std::sync::atomic::Ordering;
use std::sync::Arc;

use tessera_authz::{FragmentCache, FragmentCacheError};

use crate::cache::{RowProjectionCache, RowProjectionKey, SessionGeometry};
use crate::compose::RowProjection;
use crate::Generation;

/// Refresh every resident entry to `generation`, returning how many were produced.
///
/// **Every failure is per key and silent-but-counted.** A fragment build that cannot run leaves
/// that session's entry stale, and its next request takes rung 3 of the ladder — which, with the
/// pass finished and `refresh_in_flight` cleared, is a build rather than a 429. That is the
/// liveness floor under the whole mechanism: a refresh that never succeeds degrades to the
/// pre-0044 behaviour rather than wedging a session at 429 for ever.
pub(crate) fn refresh_resident(
    cache: &RowProjectionCache,
    fragments: &FragmentCache,
    pool: &rayon::ThreadPool,
    generation: &Generation,
) -> usize {
    let mut produced = 0usize;
    for (key, previous) in cache.resident() {
        if key.prefix != generation.prefix || key.segments_version >= generation.segments_version {
            continue;
        }
        let Some(slice_data) = generation
            .bundle
            .partitions
            .values()
            .find_map(|partition| partition.slices.get(&key.slice))
        else {
            continue;
        };

        // **Built before the cache call, because `make` must be infallible** — the single-flight
        // state machine has no way to carry a failure out of a slot, and stuffing one into the
        // value would cache it (I13a).
        let fragment = match fragments.get_or_build(
            &previous.satisfied_sorted,
            previous.auth_data_hash,
            generation.dict.len(),
            &generation.postings,
            &generation.delta_postings,
            generation.watermark,
        ) {
            Ok(fragment) => fragment,
            Err(FragmentCacheError::Building) => continue,
            Err(FragmentCacheError::Io(e)) => {
                tracing::warn!(
                    error = %e,
                    "a background fragment refresh failed; that session rebuilds on its next \
                     request rather than being served stale for ever"
                );
                continue;
            }
        };

        let next_key = RowProjectionKey {
            segments_version: generation.segments_version,
            ..key.clone()
        };
        let space = &slice_data.row_space;
        let previous_projection = Arc::clone(&previous.projection);
        let built = cache.get_or_derive(next_key, None, |_| {
            // **Append-patch, or full build.** `extends_to` is exact: it holds when every extent
            // the previous projection covers is still the same segment, which is what an append
            // leaves and what a merge over the covered prefix does not. Task 22b's span rebase
            // is the third rung and lands with the row-space merge publication (⊘ — not built).
            let projection = pool.install(|| {
                if previous_projection.extends_to(space) {
                    previous_projection.extend(&fragment, space)
                } else {
                    RowProjection::new(&fragment, space)
                }
            });
            SessionGeometry {
                fragment: Arc::clone(&fragment),
                projection: Arc::new(projection),
                satisfied_sorted: Arc::clone(&previous.satisfied_sorted),
                auth_data_hash: previous.auth_data_hash,
            }
        });
        if built.is_ok() {
            produced += 1;
        }
    }
    produced
}

/// Everything the executor hands a refresh task. Taken on the executor thread and then immutable,
/// exactly as a flush's context is.
pub(crate) struct RefreshDeps {
    pub(crate) cache: Arc<RowProjectionCache>,
    pub(crate) fragments: Arc<FragmentCache>,
    pub(crate) pool: Arc<rayon::ThreadPool>,
    pub(crate) in_flight: Arc<std::sync::atomic::AtomicBool>,
    pub(crate) refreshes: Arc<std::sync::atomic::AtomicU64>,
    /// Whether the pass runs at all. Always `true` in a shipped build; a test disables it to hold
    /// a session in the stale-serve window, which is otherwise a race to observe.
    pub(crate) enabled: Arc<std::sync::atomic::AtomicBool>,
}

impl RefreshDeps {
    /// Run one refresh round on the pool for `generation`, clearing the in-flight flag when it
    /// ends however it ends.
    ///
    /// **The caller sets the flag before the swap; this only clears it.** Setting it here would
    /// leave the window this exists to close — see the module doc.
    pub(crate) fn spawn(&self, generation: Arc<Generation>) {
        let cache = Arc::clone(&self.cache);
        let fragments = Arc::clone(&self.fragments);
        let pool = Arc::clone(&self.pool);
        let in_flight = Arc::clone(&self.in_flight);
        let refreshes = Arc::clone(&self.refreshes);
        if !self.enabled.load(Ordering::SeqCst) {
            // Nothing will produce the live key, so the flag must not stay set: rung 3 of the
            // ladder would 429 for ever instead of building.
            in_flight.store(false, Ordering::SeqCst);
            return;
        }
        let spawn_on = Arc::clone(&self.pool);
        spawn_on.spawn(move || {
            let produced = refresh_resident(&cache, &fragments, &pool, &generation);
            refreshes.fetch_add(produced as u64, Ordering::Relaxed);
            in_flight.store(false, Ordering::SeqCst);
        });
    }
}
