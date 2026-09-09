//! **The occupancy stage** — `N_occ`'s ladder, filled in the background at authorise so the
//! session's first paint does not pay for it.
//!
//! ## What it moves, and off what
//!
//! θ's second anchor is `N_occ(d)` (§7.2), memoised on
//! [`crate::occupancy::OccupancyKey`] — which carries the depth — and therefore **paid on the
//! first request at each new depth**. Nothing requires it to be paid there. The walk needs the
//! session's composed mask and the view's Morton column and nothing a request supplies: it is a
//! function of `(mask, view, generation, depth)`, viewport-invariant by §7.2's own rule. So it can
//! be taken the moment the session exists, which is here.
//!
//! ## Two rungs of staging, and why the deeper one is 12 rather than 16
//!
//! **`0..=`[`STAGE_DEPTH`] first.** The client's first request falls back to `budget.ts`'s average
//! model and starts shallow — `MIN_DEPTH` is 3 — so the shallow rungs are the ones first paint
//! actually reads, and they are the cheap end of the ladder: a walk at depth 6 emits at most `4^6`
//! tiles where a walk at 16 emits `N_occ(16)`.
//!
//! **Then `0..=`[`BACKGROUND_DEPTH`].** On `geonames`, `N_occ(12)` is 1.4 × 10⁶ against
//! `N_occ(16)`'s 1.15 × 10⁷: the deepest four rungs are roughly eight times the rest of the ladder
//! put together, and a budget-limited client rarely reaches past 12–13
//! (`clients/ts/core/src/budget.ts` — the tile count a request carries is `B / m_target`
//! independently of zoom, so the depth it lands on is bounded by the corpus's occupancy, not by
//! the grid). Staging to 16 would spend eight times as much to cover the depths fewest sessions
//! reach. **Above 12 the ladder extends lazily, on demand**, exactly as it did before this module
//! existed.
//!
//! The two stages are two walks and not one, and the second subsumes the first. That is cheap for
//! [`crate::occupancy::occupied_tiles_ladder`]'s reason — one walk at depth *d* fills every rung at
//! or below it, so a shallow walk is dominated by the deep one that follows — and it is what makes
//! the shallow rungs available *early* rather than at the end of the deep walk. A request landing
//! between the two finds `0..=6` filled.
//!
//! ## What it costs a session that never views
//!
//! **The row projection.** `N_occ` must be counted over the composed [`EffectiveMask`], so the
//! stage resolves the session's [`crate::cache::SessionGeometry`] — and on a cold session nothing
//! else has built it yet, so the stage is the builder. That is the first request's own work moved
//! earlier, not new work, for every session that goes on to view something; for a session that
//! authorises and never views it is speculative, and it is why this whole path is cancellable and
//! why nothing on a request's critical path ever waits for it.
//!
//! **It cannot be avoided by riding on the projection build itself.** The projection is `M_auth`
//! *before* the overlay diff, and a HyperLogLog cannot subtract: a sketch filled from the
//! projection could never have a suppressed tile removed from it, so a deny that emptied a tile
//! would leave `N_occ` where it was. That is exactly the I2 property
//! `n_occ_falls_when_a_suppression_empties_a_tile` pins. The ladder comes from the composed mask
//! or it does not come at all.
//!
//! **Every visible view, in the manifest's order.** A session may reach several views and nothing
//! at authorise says which one it will ask for, so staging one of them is a coin flip that is
//! wrong whenever the client picks another. What that costs is `V − 1` extra resident projections
//! for a session that views one of `V` views; the view it does ask for would have been resident
//! either way. A `view` hint on `/session/authorise` would remove the multiplier and is not
//! designed.
//!
//! ## The three things this must not do
//!
//! **It must not wait on the row-projection cache.** This runs on a rayon worker and
//! `RowProjection::new` fans out across the same pool, so parking a worker on work that needs
//! workers is a starvation deadlock rather than a wasted stage. It takes
//! [`RowProjectionCache::get_or_derive`], never the waiting form — `crate::refresh` is the other
//! caller under that rule and its doc argues it at length.
//!
//! **It must not shed a request.** A key found `Building` is skipped: whoever owns it will publish
//! it, and duplicating the build would cost the pool the very time this exists to save.
//!
//! **It must not outlive its session.** [`Engine::prune_token`] flips this session's token at
//! revoke; the stage checks it before each view and between the two rungs, and the deepest
//! individual step it cannot interrupt is one walk.
//!
//! ## What it deliberately does not cover
//!
//! **The rungs go stale on every publication.** [`crate::occupancy::OccupancyKey`] carries
//! `segments_version`, `overlay_version` and the fragment's identity and watermark, so a flush
//! invalidates the whole ladder for every session and the next request at each depth walks again.
//! Warming from [`crate::refresh`], which already rebuilds each resident session's projection per
//! publication, would cover that too and would be worth more than this under continuous ingest.
//! It is not built: the refresh pass is a measured ~0.7 s round that the shed's margin is already
//! close to (`crate::refresh::RefreshDeps::in_flight`), and adding a walk per entry to it is a
//! change to that budget rather than to this one.

use std::sync::Arc;

use crate::cache::{RowProjectionCache, RowProjectionKey, SessionGeometry};
use crate::cancel::CancelToken;
use crate::compose::{compose, RowProjection};
use crate::occupancy::{OccupancyKey, OccupiedTiles};
use crate::Generation;

/// The in-flight map, through a poisoned lock.
///
/// **A poisoned mutex must not stop a revoke from cancelling a stage.** The map holds nothing but
/// cancellation tokens; a panic while it was held leaves it structurally intact, and refusing to
/// look at it afterwards would leave every later stage uncancellable for the process's life.
fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The deepest rung the first stage fills — see the module doc.
pub(crate) const STAGE_DEPTH: u8 = 6;

/// The deepest rung the second stage fills. Above it the ladder extends on demand.
pub(crate) const BACKGROUND_DEPTH: u8 = 12;

/// Everything one session's stage needs from that session, copied at authorise.
///
/// **A value rather than a borrow of `Session`**, because the stage outlives the `authorise` call
/// that started it: the server holds the `Session` in its own registry and the engine has no
/// session registry at all (`crate::refresh`'s module doc makes the same observation about the
/// refresh pass, and solves it the same way — everything the background work needs travels with
/// the work).
pub(crate) struct SessionStage {
    pub(crate) token_id: u64,
    pub(crate) satisfied: rustc_hash::FxHashSet<tessera_types::TermId>,
    pub(crate) satisfied_sorted: Arc<Vec<tessera_types::TermId>>,
    pub(crate) auth_data_hash: [u8; 32],
    pub(crate) segments_version_at_authorise: u64,
    pub(crate) fragment: Arc<tessera_authz::FrozenFragment>,
    pub(crate) visible_views: Arc<crate::gate::VisibleViews>,
}

/// The engine handles one stage runs against, taken once at [`crate::Engine::open`].
pub(crate) struct StageDeps {
    pub(crate) generation: Arc<crate::GenerationHandle>,
    pub(crate) cache: Arc<RowProjectionCache>,
    pub(crate) occupancy: Arc<
        crate::single_flight::SingleFlightCache<OccupancyKey, OccupiedTiles>,
    >,
    pub(crate) pool: Arc<rayon::ThreadPool>,
    /// [`crate::Engine::full_projection_builds`]' counter, shared rather than duplicated: a
    /// projection this stage builds is a full `Permutation::project` like any other, and an
    /// operator gauge that counted only the request path's would under-report the pool time a
    /// deployment is actually spending.
    pub(crate) full_projection_builds: Arc<std::sync::atomic::AtomicU64>,
    /// Whether the stage runs at all. Always `true` in a shipped build; a test turns it off so an
    /// assertion about what a *request* computed is not answered by work a background task did
    /// first.
    pub(crate) enabled: Arc<std::sync::atomic::AtomicBool>,
    /// One cancellation token per session with a stage in flight, flipped and dropped by
    /// [`crate::Engine::prune_token`]. Bounded by the number of stages running at once, not by the
    /// number of sessions: a stage removes its own entry when it ends.
    pub(crate) in_flight: Arc<std::sync::Mutex<rustc_hash::FxHashMap<u64, CancelToken>>>,
}

impl StageDeps {
    /// Start this session's stage on the pool. Returns immediately.
    pub(crate) fn spawn(&self, session: SessionStage) {
        if !self.enabled.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let cancel = CancelToken::new();
        let token_id = session.token_id;
        // **Inserted before the spawn**, so a revoke that lands between here and the worker
        // picking the task up still finds a token to flip. Replacing an entry for a token id that
        // is somehow already staging would leave the earlier stage uncancellable, so the earlier
        // one is cancelled as it is displaced — `token_id` is never reused within a process
        // (`crate::cache::RowProjectionKey`'s fact 1), so this cannot happen and costs one branch
        // to make true rather than assumed.
        if let Some(previous) = lock(&self.in_flight).insert(token_id, cancel.clone()) {
            previous.cancel();
        }
        let generation = Arc::clone(&self.generation);
        let cache = Arc::clone(&self.cache);
        let occupancy = Arc::clone(&self.occupancy);
        let pool = Arc::clone(&self.pool);
        let in_flight = Arc::clone(&self.in_flight);
        let builds = Arc::clone(&self.full_projection_builds);
        self.pool.spawn(move || {
            run(&generation, &cache, &occupancy, &pool, &builds, &session, &cancel);
            // Whatever ended it — completion, cancellation, a view that could not be resolved —
            // this session is no longer staging and the entry must go, or the map grows by one per
            // session for the process's life.
            lock(&in_flight).remove(&token_id);
        });
    }

    /// Cancel this session's stage, if one is running. A no-op otherwise, which is the ordinary
    /// case: most sessions finish staging long before they are revoked.
    pub(crate) fn cancel(&self, token_id: u64) {
        if let Some(cancel) = lock(&self.in_flight).remove(&token_id) {
            cancel.cancel();
        }
    }

    /// How many stages are in flight. Test-facing: it is how a test waits for the background work
    /// to settle rather than sleeping on a guess.
    #[cfg(feature = "fault-injection")]
    pub(crate) fn in_flight(&self) -> usize {
        lock(&self.in_flight).len()
    }
}

/// Fill every visible view's ladder to [`STAGE_DEPTH`] and then to [`BACKGROUND_DEPTH`].
fn run(
    generation: &crate::GenerationHandle,
    cache: &RowProjectionCache,
    occupancy: &crate::single_flight::SingleFlightCache<OccupancyKey, OccupiedTiles>,
    pool: &rayon::ThreadPool,
    builds: &std::sync::atomic::AtomicU64,
    session: &SessionStage,
    cancel: &CancelToken,
) {
    // **One generation for the whole stage**, taken once. A publication landing mid-stage leaves
    // the rungs filled under the superseded key, which the next request simply misses — the same
    // outcome as not having staged at all, and never a rung answered under the wrong geometry.
    let generation = generation.load_full();
    // Manifest order, filtered by the gate — so two sessions with the same grant stage in the same
    // order, and a view this session cannot reach is never touched.
    let views: Vec<String> = generation
        .bundle
        .manifest
        .views
        .iter()
        .map(|v| v.id.clone())
        .filter(|id| session.visible_views.contains_view(id))
        .collect();

    for view in views {
        if cancel.is_cancelled() {
            return;
        }
        let Some(geometry) = resolve_geometry(cache, pool, builds, &generation, session, &view)
        else {
            continue;
        };
        let Some(view_data) = generation
            .bundle
            .partitions
            .values()
            .find_map(|partition| partition.views.get(&view))
        else {
            continue;
        };
        let Some(denied) = generation.denied.get(&view) else {
            continue;
        };
        let Ok(segments) = crate::viewport::segments_with_row_bases(&view, view_data) else {
            continue;
        };
        let mask = compose(
            &session.satisfied,
            &generation.overlay,
            &generation.buffer,
            Arc::clone(&geometry.projection),
            &view_data.row_space,
            denied,
        );
        let key = OccupancyKey {
            token_id: session.token_id,
            view: view.clone(),
            depth: 0,
            segments_version: generation.segments_version,
            overlay_version: generation.overlay_version,
            fragment_identity: geometry.fragment.identity,
            fragment_watermark: geometry.fragment.watermark,
        };
        for depth in [STAGE_DEPTH, BACKGROUND_DEPTH] {
            if cancel.is_cancelled() {
                return;
            }
            // **The memo is consulted first, exactly as the request path consults it.** A session
            // that has already been served at this depth has the rung; re-walking would be the
            // naive refill policy `crate::occupancy` measures as a loss at every cell.
            let mut deepest = key.clone();
            deepest.depth = depth;
            if matches!(
                occupancy.peek(&deepest),
                crate::single_flight::Peek::Ready(_)
            ) {
                continue;
            }
            let ladder = crate::occupancy::occupied_tiles_ladder(&mask, &segments, depth);
            for rung in 0..=depth {
                let mut rung_key = key.clone();
                rung_key.depth = rung;
                let _ = occupancy.get_or_derive(rung_key, None, |_| OccupiedTiles(ladder.at(rung)));
            }
        }
    }
}

/// This session's geometry for one view, **without waiting for anyone else's build**.
///
/// The three rungs `crate::viewport::Engine::session_geometry` climbs are the request path's, and
/// two of them are wrong here: rung 2's stale serve exists to spare a *request* a rebuild, and
/// rung 3 parks. This resolves the live key or gives up — the module doc's second and third rules.
fn resolve_geometry(
    cache: &RowProjectionCache,
    pool: &rayon::ThreadPool,
    builds: &std::sync::atomic::AtomicU64,
    generation: &Generation,
    session: &SessionStage,
    view: &str,
) -> Option<Arc<SessionGeometry>> {
    let key = RowProjectionKey {
        token_id: session.token_id,
        view: view.to_string(),
        segments_version: generation.segments_version,
        prefix: generation.prefix.clone(),
    };
    if let crate::cache::Peek::Ready(geometry) = cache.peek(&key) {
        return Some(geometry);
    }
    let view_data = generation
        .bundle
        .partitions
        .values()
        .find_map(|partition| partition.views.get(view))?;

    // The same two tests `Engine::fragment_for` makes, and for its reasons: a fold rotates the
    // bundle identity without moving the watermark, so the identity comparison is not redundant.
    let fragment = if session.fragment.identity == generation.bundle_identity()
        && session.fragment.watermark >= generation.watermark
    {
        Arc::clone(&session.fragment)
    } else {
        generation
            .fragments
            .get_or_build(
                &session.satisfied_sorted,
                session.auth_data_hash,
                session.segments_version_at_authorise,
                &generation.postings,
                &generation.delta_postings,
                generation.watermark,
            )
            .ok()?
    };

    let space = &view_data.row_space;
    let satisfied_sorted = Arc::clone(&session.satisfied_sorted);
    let auth_data_hash = session.auth_data_hash;
    let satisfied_at = session.segments_version_at_authorise;
    cache
        .get_or_derive(key, None, |_| {
            builds.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let projection = pool.install(|| RowProjection::new(&fragment, space));
            SessionGeometry {
                fragment: Arc::clone(&fragment),
                projection: Arc::new(projection),
                satisfied_sorted,
                auth_data_hash,
                satisfied_at,
            }
        })
        .ok()
}
