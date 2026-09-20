//! **The occupancy stage** — `N_occ`'s ladder, filled to [`BACKGROUND_DEPTH`] in the background so
//! that only a session's first request ever pays a walk.
//!
//! ## What it moves, and off what
//!
//! θ's second anchor is `N_occ(d)` (§7.2), memoised on [`crate::occupancy::OccupancyKey`] — which
//! carries the depth — and therefore **paid on the first request at each new depth**. Measured at
//! 2.33 × 10⁸ rows: 33 ms at depth 3, 63 ms at 12, 191 ms at 16, against a warm request's 216–313
//! ms in total (`probes/2026-09-09-nocc-stage`). Nothing requires it to be paid on a request. The
//! walk is a function of the composed mask, the view and the depth, and of nothing a request
//! supplies — it is viewport-invariant by §7.2's own rule — so once a session has a composed mask
//! at all, every rung it will ever need can be filled without one.
//!
//! **One walk fills every rung at or below it** ([`crate::occupancy::occupied_tiles_ladder`]), so
//! this is one walk per `(session, view, generation)` and not one per depth.
//!
//! ## Where it is triggered, and why not at authorise
//!
//! **A request that took θ's anchor spawns this**, carrying the geometry that request already
//! resolved. That is the earliest point at which the ladder can be filled at all: `N_occ` must be
//! counted over the **composed** `EffectiveMask` (**I2**), which needs the session's row
//! projection, and a session has no row projection until its first request builds one.
//!
//! **Filling it at authorise instead was measured and is not landed.** It works — a cold first
//! request at depth 3 falls from 840 ms to 285 ms at 2.33 × 10⁸ rows, because staging the ladder
//! stages the row projection with it — and it changes three things that are not this change's to
//! decide: a session acquires a resident row-projection entry before its first request, so decision
//! 0044's rung-2 stale serve reaches that first request where it previously could not;
//! `Engine::authorise` can be refused `FragmentBuilding` by a background task rather than by real
//! demand; and a session that authorises and never views pays a full `Permutation::project` per
//! visible view. The figures and the three consequences are in
//! `probes/2026-09-09-nocc-stage/README.md`; the ruling is the owner's.
//!
//! ## Why the ceiling is 12
//!
//! Measured exactly off the stored Morton column, independently of this walk
//! (`probes/2026-09-09-nocc-stage/n_occ_ladder.py`): on `treeoflife` at 2.33 × 10⁸ rows `N_occ(12)`
//! is 117,429 against `N_occ(16)`'s 8,330,745, so the four deepest rungs are **76 times** the rest
//! of the ladder in emissions and 3 times it in walk time (63 ms against 191 ms). On `geonames`,
//! 1,399,919 against 11,543,951. A client's requested depth is bounded by its own mark budget
//! rather than by the grid — the tile count a request carries is `B / m_target` independently of
//! zoom (`clients/ts/core/src/budget.ts`) — so it rarely reaches past 12–13. Filling to 16 would
//! spend that to cover the depths fewest sessions reach. **Above 12 the ladder is extended
//! lazily, from a request**, exactly as it was before this module existed.
//!
//! ## The three things this must not do
//!
//! **It must not repeat a walk the memo already answers.** The deepest rung is peeked before
//! anything is composed; refilling on every request is the naive policy `crate::occupancy` measures
//! as a loss at every cell.
//!
//! **It must not touch the row-projection cache.** It carries the [`SessionGeometry`] its request
//! already resolved, so there is nothing to look up and nothing to publish — which is what keeps it
//! invisible to `crate::viewport::Engine::session_geometry`'s three-rung ladder and to
//! `crate::refresh`.
//!
//! **It must not outlive its session.** [`crate::Engine::prune_token`] flips this session's token
//! at a revoke and [`crate::Engine::prune_tokens`] flips a batch of them at the registry's expiry
//! sweep, which is how a session that was never revoked stops filling. The deepest step neither
//! can interrupt is one walk.
//!
//! ## What it deliberately does not cover
//!
//! **The very first request at a session's shallowest depth still walks**, because nothing has
//! composed that session's mask before it. At 2.33 × 10⁸ that is a measured 36 ms of a 1.3 s cold
//! request. Everything after it is free: a session zooming from depth 3 to 12 pays that one walk
//! where it paid six, 36 ms against 255 ms, and the zoom back out was already free.
//!
//! **The rungs go stale on every publication.** [`crate::occupancy::OccupancyKey`] carries
//! `segments_version`, `overlay_version` and the fragment's identity and watermark, so a flush
//! invalidates the whole ladder for every session and the next request walks again — after which
//! this refills the rest. Warming from [`crate::refresh`], which already rebuilds each resident
//! session's projection per publication, would cover that too. It is not built: the refresh pass is
//! a measured ~0.7 s round whose shed margin is already under 2× (`crate::refresh::RefreshDeps`),
//! and adding a walk per entry is a change to that budget rather than to this one.

use std::sync::Arc;

use rustc_hash::{FxHashMap, FxHashSet};
use tessera_types::TermId;

use crate::cache::SessionGeometry;
use crate::cancel::CancelToken;
use crate::compose::compose;
use crate::occupancy::{OccupancyKey, OccupiedTiles};
use crate::Generation;

/// The in-flight map, through a poisoned lock.
///
/// **A poisoned mutex must not stop a revoke from cancelling a fill.** The map holds nothing but
/// cancellation tokens; a panic while it was held leaves it structurally intact, and refusing to
/// look at it afterwards would leave every later fill uncancellable for the process's life.
fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The deepest rung the background fill reaches. Above it the ladder is extended on demand — see
/// the module doc for the measurement behind the number.
pub(crate) const BACKGROUND_DEPTH: u8 = 12;

/// Everything one fill needs, taken from the request that spawned it.
///
/// **A value rather than a borrow**, because the fill outlives the request: the engine holds no
/// session registry (`crate::refresh`'s module doc makes the same observation about the refresh
/// pass, and solves it the same way — everything the background work needs travels with the work).
/// The two `Arc`s are the request's own, so this clones a generation pointer and a geometry pointer
/// and copies a term set, not a mask and not a projection.
pub(crate) struct LadderTask {
    pub(crate) token_id: u64,
    pub(crate) view: String,
    pub(crate) satisfied: FxHashSet<TermId>,
    pub(crate) generation: Arc<Generation>,
    pub(crate) geometry: Arc<SessionGeometry>,
}

/// The engine handles one fill runs against, taken once at [`crate::Engine::open`].
pub(crate) struct StageDeps {
    /// `occupancy_walks`, shared rather than duplicated: a walk this fill makes is a walk of the
    /// same mask and the same column as a request's.
    pub(crate) counters: Arc<crate::status::ServeCounters>,
    pub(crate) occupancy:
        Arc<tessera_cache::SingleFlightCache<OccupancyKey, OccupiedTiles>>,
    pub(crate) pool: Arc<rayon::ThreadPool>,
    /// `occupancy_stage_enabled`, whether the fill runs at all.
    pub(crate) switches: Arc<crate::switches::TestSwitches>,
    /// One cancellation token per session with a fill in flight, flipped and dropped by
    /// [`crate::Engine::prune_token`]. Bounded by the number of fills running at once, not by the
    /// number of sessions: a fill removes its own entry when it ends.
    pub(crate) in_flight: Arc<std::sync::Mutex<FxHashMap<u64, CancelToken>>>,
}

impl StageDeps {
    /// Fill this `(session, view, generation)`'s ladder to [`BACKGROUND_DEPTH`] on the pool.
    ///
    /// Returns immediately, and returns having done nothing if this session already has a fill
    /// running: one walk per session at a time is the whole of the concurrency this needs, and it
    /// is what stops a client panning at ten frames a second from queueing ten walks for one
    /// ladder.
    ///
    /// **`make` runs only once the fill is going to happen**, so a request that finds one already
    /// in flight — the common case for every request after the first — pays a lock and a hash
    /// lookup and does not clone a term set or a view name.
    pub(crate) fn spawn(&self, token_id: u64, make: impl FnOnce() -> LadderTask) {
        if !self
            .switches
            .occupancy_stage_enabled
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        let cancel = CancelToken::new();
        {
            let mut in_flight = lock(&self.in_flight);
            if in_flight.contains_key(&token_id) {
                return;
            }
            in_flight.insert(token_id, cancel.clone());
        }
        let task = make();
        debug_assert_eq!(task.token_id, token_id, "the task must be this session's");
        let occupancy = Arc::clone(&self.occupancy);
        let in_flight = Arc::clone(&self.in_flight);
        let counters = Arc::clone(&self.counters);
        self.pool.spawn(move || {
            run(&occupancy, &counters, &task, &cancel);
            // However it ended — completion, cancellation, a view that could not be resolved —
            // this session is no longer filling and the entry must go, or the map grows by one per
            // session for the process's life.
            lock(&in_flight).remove(&token_id);
        });
    }

    /// Cancel this session's fill, if one is running. A no-op otherwise, which is the ordinary
    /// case: a fill is one walk and most sessions are revoked long after theirs finished.
    pub(crate) fn cancel(&self, token_id: u64) {
        if let Some(cancel) = lock(&self.in_flight).remove(&token_id) {
            cancel.cancel();
        }
    }

    /// How many fills are in flight. Test-facing: it is how a test waits for the background work to
    /// settle rather than sleeping on a guess.
    #[cfg(feature = "fault-injection")]
    pub(crate) fn in_flight(&self) -> usize {
        lock(&self.in_flight).len()
    }
}

/// Fill `0..=`[`BACKGROUND_DEPTH`] for one `(session, view, generation)`.
fn run(
    occupancy: &tessera_cache::SingleFlightCache<OccupancyKey, OccupiedTiles>,
    counters: &crate::status::ServeCounters,
    task: &LadderTask,
    cancel: &CancelToken,
) {
    let generation = &task.generation;
    let mut key = OccupancyKey {
        token_id: task.token_id,
        view: task.view.clone(),
        depth: BACKGROUND_DEPTH,
        segments_version: generation.segments_version,
        overlay_version: generation.overlay_version,
        fragment_identity: task.geometry.fragment.identity,
        fragment_watermark: task.geometry.fragment.watermark,
    };
    // **The memo first, before anything is composed.** A session already served at this depth has
    // every rung below it too, so there is nothing here to do and no mask worth building.
    if cancel.is_cancelled()
        || matches!(occupancy.peek(&key), tessera_cache::Peek::Ready(_))
    {
        return;
    }

    let Some(view_data) = generation
        .bundle
        .partitions
        .values()
        .find_map(|partition| partition.views.get(&task.view))
    else {
        return;
    };
    let Some(denied) = generation.denied().get(&task.view) else {
        return;
    };
    let Ok(segments) = crate::viewport::segments_with_row_bases(&task.view, view_data) else {
        return;
    };
    // **Composed here rather than carried from the request**, because a mask borrows the projection
    // and this runs after the request has returned. It is the same composition over the same
    // inputs — the request's own generation and geometry travel with the task — so the rungs are
    // exactly what that session's next zoom would have computed for itself.
    let mask = compose(
        &task.satisfied,
        &generation.overlay,
        &generation.buffer,
        Arc::clone(&task.geometry.projection),
        &view_data.row_space,
        denied,
    );
    if cancel.is_cancelled() {
        return;
    }
    counters
        .occupancy_walks
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let ladder = crate::occupancy::occupied_tiles_ladder(&mask, &segments, BACKGROUND_DEPTH);
    for rung in 0..=BACKGROUND_DEPTH {
        key.depth = rung;
        let _ = occupancy.get_or_derive(key.clone(), None, |_| OccupiedTiles(ladder.at(rung)));
    }
}
