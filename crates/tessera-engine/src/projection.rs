//! The cached row-space projection of one session's frozen fragment: how it is built, and how a
//! publication patches it instead of rebuilding it.
//!
//! [`RowProjection`] is the cached `Permutation::project` output for one `(token, view, pin)` —
//! computed once — seconds at 10⁹ rows — and reused across every
//! viewport and every `compose` call in that session, never recomputed on a per-viewport path.

use std::io;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

use croaring::Bitmap;

use tessera_authz::{DeltaTier, FrozenFragment, PostingsReader};
use tessera_store::term_images::{
    choose, chooser_inputs, ChooserInputs, Route, TermImages, ROUTE_COSTS,
};
use tessera_store::RowSpace;
use tessera_types::TermId;

/// What a row projection is built from, beside the row space it is built into.
///
/// The fragment alone decided this once. The other four fields are what the split route needs:
/// which terms the session satisfies, where their base and delta postings are, and the bundle's
/// images of those postings if the view has any. They travel together because the route is chosen
/// from all of them at once, before any of them is read.
pub struct ProjectionInputs<'a> {
    /// The session's mask fragment, in entity space.
    pub fragment: &'a FrozenFragment,
    /// The terms the session satisfies, ascending and deduplicated. This is the `T` of the
    /// exactness argument in `tessera_store::term_images`.
    pub satisfied: &'a [TermId],
    /// The generation's base postings.
    pub postings: &'a PostingsReader,
    /// The generation's live delta tiers.
    pub deltas: &'a [Arc<DeltaTier>],
    /// This view's term images, where the bundle carries them and they opened.
    pub images: Option<&'a TermImages>,
    /// A route to take instead of the chosen one. `None` in every shipped path: only
    /// `Engine::force_projection_route_for_test` ever sets it, and only under `fault-injection`.
    pub force: Option<ProjectionRoute>,
}

/// How a row projection was built. Every route returns the identical projection; what differs is
/// the work done to reach it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectionRoute {
    /// The grant covers the whole entity domain and the mapping is a bijection onto its rows, so
    /// the base's contribution is the row range and no page is read.
    WholeDomain,
    /// Every held entity is walked through the mapping.
    Walk,
    /// The images of the terms the session holds are unioned, and only the residual is walked.
    Split,
    /// The entities outside the grant are walked and subtracted from the row range.
    Complement,
}

impl ProjectionRoute {
    /// Every route, in the order [`ProjectionRoute::index`] numbers them, which is the order the
    /// engine's per-route counters and `/control/status` publish.
    pub const ALL: [ProjectionRoute; 4] = [
        ProjectionRoute::WholeDomain,
        ProjectionRoute::Walk,
        ProjectionRoute::Split,
        ProjectionRoute::Complement,
    ];

    /// This route's position in [`ProjectionRoute::ALL`].
    pub fn index(self) -> usize {
        match self {
            ProjectionRoute::WholeDomain => 0,
            ProjectionRoute::Walk => 1,
            ProjectionRoute::Split => 2,
            ProjectionRoute::Complement => 3,
        }
    }

    /// The name this route is published under.
    pub fn name(self) -> &'static str {
        match self {
            ProjectionRoute::WholeDomain => "whole_domain",
            ProjectionRoute::Walk => "walk",
            ProjectionRoute::Split => "split",
            ProjectionRoute::Complement => "complement",
        }
    }
}

/// The per-route build counters, the forced route a test may fix, and the one place a projection
/// build is counted.
///
/// **One value shared by the two sites that build a full projection**, the request path's
/// `Engine::session_geometry` and the background refresh's rung 3. A forced route therefore reaches
/// both, and neither can count into a gauge the other does not.
#[derive(Debug, Default)]
pub struct ProjectionRoutes {
    counts: [AtomicU64; 4],
    /// The forced route as `ProjectionRoute::index() + 1`, or zero for the chosen route. Zero in
    /// every shipped build: `Engine::force_projection_route_for_test` is the only writer and
    /// exists only under `fault-injection`.
    forced: AtomicU8,
}

impl ProjectionRoutes {
    /// Builds so far, in [`ProjectionRoute::ALL`]'s order.
    pub fn counts(&self) -> [u64; 4] {
        std::array::from_fn(|index| self.counts[index].load(Ordering::Relaxed))
    }

    /// The route every build must take, where one has been fixed.
    pub fn forced(&self) -> Option<ProjectionRoute> {
        self.forced
            .load(Ordering::Relaxed)
            .checked_sub(1)
            .and_then(|index| ProjectionRoute::ALL.get(index as usize).copied())
    }

    /// Fix the route every build takes, or return to the chosen one on `None`.
    pub fn force(&self, route: Option<ProjectionRoute>) {
        // One line, with the ordering on it: `check-layers.sh`'s sole-publisher rule tells an
        // atomic store from an `ArcSwap` publication by the `Ordering::` argument beside it.
        let encoded = route.map_or(0, |route| route.index() as u8 + 1);
        self.forced.store(encoded, Ordering::Relaxed);
    }

    /// Build a projection by [`RowProjection::new`] and count the route it took.
    ///
    /// **Postings the split route cannot read leave it walking instead**, warned and counted as a
    /// walk. The two reads are the chooser's sum over the satisfied terms' delta postings and the
    /// residual itself, both over the postings and tiers the session's fragment was unioned from
    /// moments earlier, so a failure here is a host condition rather than a state the request can
    /// reach. What matters is that the fallback is the reference computation and not a narrower
    /// one. The
    /// walk crosses the whole fragment and returns the identical rows, so a session served this
    /// way is served the same set more slowly. Refusing instead would cost a session its map for a
    /// fault that costs it nothing, and the two call sites cannot carry an error out in any case:
    /// the single-flight slot has no way to hold one and caching it would be I13a.
    pub fn build(&self, inputs: &ProjectionInputs<'_>, rows: &RowSpace) -> RowProjection {
        match RowProjection::new(inputs, rows) {
            Ok((projection, route)) => {
                self.counts[route.index()].fetch_add(1, Ordering::Relaxed);
                projection
            }
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "the postings the split route prices and walks could not be read; this \
                     projection was built by the walk, which produces the same rows"
                );
                self.counts[ProjectionRoute::Walk.index()].fetch_add(1, Ordering::Relaxed);
                RowProjection::walk(inputs.fragment, rows)
            }
        }
    }
}

/// A cached row-space projection of one frozen fragment, for one `(token, view, pin)`.
///
/// Constructing this crosses entity space into row space via `Permutation::project`, which
/// touches every set bit of the fragment and then sorts the result — cheap at the 10k scale this
/// phase's fixtures use, but *seconds* at 10⁹ rows (see `Permutation::project`'s doc). Callers
/// must cache the result per `(token, view, pin)` and never reconstruct it on a per-viewport
/// path (shared-context constraint 8) — `compose` itself only ever reads it via `range_cardinality`
/// / `contains`, both O(containers touched), never re-derives it from the fragment.
pub struct RowProjection {
    rows: Bitmap,
    /// How many of the view's extents this projection already covers — the index
    /// `RowSpace::project_extents_from` resumes at when a flush extends it (see
    /// [`RowProjection::extend`]).
    extents_covered: usize,
    /// The `seg_id` of the last extent covered, or `None` when only the base is.
    ///
    /// **This is what makes the patch sound across a merge**, and it is the whole of the check.
    /// A flush appends, so extents `[0, extents_covered)` are untouched and the patch is exact. A
    /// merge collapses an adjacent run into one segment with a **new** `seg_id` — ids are never
    /// reused, across merges or prefixes (contracts §2.1) — so if the run it collapsed overlapped
    /// this projection's covered prefix, the extent now sitting at `extents_covered - 1` is either
    /// a different segment or out of range. Comparing that one id is therefore exact rather than
    /// heuristic, and costs one string comparison against a *measured* 1 277 ms rebuild.
    boundary_seg_id: Option<String>,
    /// The base permutation's row count at construction — the boundary between the part of this
    /// projection a merge can never move and the part it can.
    ///
    /// Every row below it comes from `permutation.bin`, which no flush and no merge rewrites (a
    /// merge's inputs are flush segments; consuming the base is compaction under another name, and
    /// compaction publishes a new *prefix*, which the cache key discriminates on). That is what
    /// makes [`Self::rebase_extents`] exact.
    base_rows: u32,
    /// `rows.cardinality()`, computed once at construction.
    ///
    /// Memoised because §7.2's θ anchor needs the projection's total cardinality on **every**
    /// viewport (see [`EffectiveMask::visible_total`]), and `Bitmap::cardinality` is O(containers)
    /// — roughly 15k containers for a 2.5x10⁸-row projection at 10⁹. Paying that per request would
    /// make the anchor a per-viewport cost rather than the "almost nothing" it is advertised as.
    /// The projection is immutable, so this can never go stale.
    cardinality: u64,
}

impl RowProjection {
    /// Project `inputs.fragment` into this view's row space, by whichever route costs least, and
    /// say which route that was. Do not call this on the per-viewport path — see this struct's doc.
    ///
    /// **Every route returns the identical projection.** The walk crosses the whole fragment. The
    /// split unions the bundle's images of the terms the session holds — each image is the base
    /// projection of that term's posting, and every satisfied term's posting lies inside the
    /// fragment — then walks the residual and the extents. The complement walks the entities the
    /// grant does not hold and subtracts their rows from the base's row range, which is exact
    /// where the base's slots are a bijection onto that range. It adds the extents above that, as
    /// the walk's own route does. The whole-domain answer is the row range.
    /// `tessera_store::term_images`' module doc carries the algebra;
    /// `tests/term_images_route.rs` checks the routes against each other over a built corpus.
    ///
    /// **The route is chosen before any route runs, from the principal's own grant**: the
    /// fragment's cardinality below the bound, the image table's sizes for the terms the principal
    /// holds, and an overcount of the residual. Nothing about another principal, and nothing about
    /// the overlay, enters it. Appendix C's **C19** records the timing residual.
    ///
    /// **Pre-overlay, as this type has always been.** Deny, suppression and buffer composition are
    /// untouched: a suppressed entity stays in its term's posting and in its image exactly as it
    /// stays in the fragment, and `EffectiveMask` composes the overlay on every request.
    ///
    /// The error is the residual's: it reads postings, and a caller propagates it as it propagates
    /// a fragment build's.
    pub fn new(
        inputs: &ProjectionInputs<'_>,
        rows: &RowSpace,
    ) -> io::Result<(Self, ProjectionRoute)> {
        let fragment = inputs.fragment.view();
        let whole_domain = rows.base().whole_domain_rows(&fragment).is_some();
        let wanted = match inputs.force {
            Some(forced) => forced,
            None if whole_domain => ProjectionRoute::WholeDomain,
            None => Self::price(inputs, rows, &fragment)?,
        };

        let walked = |route| Ok((Self::over(rows.project(&fragment), rows), route));
        match wanted {
            // A forced whole-domain route over a grant that does not cover the domain has no
            // answer of its own. It walks, and the gauge says so rather than reporting a route
            // that did not run.
            ProjectionRoute::WholeDomain if whole_domain => walked(ProjectionRoute::WholeDomain),
            ProjectionRoute::Split => {
                let Some(images) = inputs.images else {
                    return walked(ProjectionRoute::Walk);
                };
                let (kept, unkept): (Vec<TermId>, Vec<TermId>) = inputs
                    .satisfied
                    .iter()
                    .copied()
                    .partition(|term| images.kept(*term));
                if kept.is_empty() {
                    return walked(ProjectionRoute::Walk);
                }
                // The views live inside `union` and are dropped when it returns: a session reads
                // the mapped bytes once and nothing caches a header across sessions.
                let mut union = images.union(&kept);
                let residual = tessera_authz::residual_fragment(
                    &unkept,
                    &kept,
                    inputs.postings,
                    inputs.deltas,
                    &fragment,
                )?;
                union.or_inplace(&rows.project_base(&residual));
                union.or_inplace(&rows.project_extents_from(&fragment, 0));
                Ok((Self::over(union, rows), ProjectionRoute::Split))
            }
            ProjectionRoute::Complement => match rows.project_complement_base(&fragment) {
                Some(mut base) => {
                    base.or_inplace(&rows.project_extents_from(&fragment, 0));
                    Ok((Self::over(base, rows), ProjectionRoute::Complement))
                }
                // The base's row count is not recorded, which is the whole of the route's
                // validity and is what the chooser is given, so this is a forced complement over
                // a row space that has no complement answer. The walk answers it.
                None => {
                    debug_assert!(
                        inputs.force.is_some(),
                        "the chooser is offered the complement only where the base records the \
                         row count it is a bijection onto, so a chosen complement with none is a \
                         chooser bug"
                    );
                    walked(ProjectionRoute::Walk)
                }
            },
            _ => walked(ProjectionRoute::Walk),
        }
    }

    /// Price the routes over `fragment` and return the cheapest, or [`ProjectionRoute::Walk`]
    /// where there is nothing to price against.
    ///
    /// The complement is offered where the base records the row count its slots are a bijection
    /// onto, which is what `RowSpace::project_complement_base` needs and all it needs.
    ///
    /// **A view with no image table is priced too.** Images are written by the build and by each
    /// fold, so a view can be served without them, and a session can hold no term that has one.
    /// Either way there is no split to take, and what is left is the walk against the complement,
    /// which is a choice about the principal's grant and the row space rather than about any
    /// image. The residual is not priced in that case and the delta postings are not read: the
    /// split is not on offer for the residual estimate to change.
    fn price(
        inputs: &ProjectionInputs<'_>,
        rows: &RowSpace,
        fragment: &croaring::Bitmap,
    ) -> io::Result<ProjectionRoute> {
        let bound = rows.base().bound();
        // A bound of zero holds no entity, and one above the `u32` entity ceiling (I9) names
        // entities no mask can hold. Neither has a route to price against the walk.
        let Some(hi) = bound.checked_sub(1).and_then(|hi| u32::try_from(hi).ok()) else {
            return Ok(ProjectionRoute::Walk);
        };
        let held = fragment.range_cardinality(0..=hi);
        let complement_valid = rows.base().dense_rows().is_some();
        let unionable = inputs
            .images
            .filter(|images| inputs.satisfied.iter().any(|term| images.kept(*term)));
        let chooser = match unionable {
            Some(images) => chooser_inputs(
                images,
                inputs.satisfied,
                held,
                bound,
                complement_valid,
                tessera_authz::delta_entities(inputs.satisfied, inputs.deltas)?,
            ),
            None => ChooserInputs {
                held,
                bound,
                complement_valid,
                ..ChooserInputs::default()
            },
        };
        Ok(match choose(&chooser, &ROUTE_COSTS) {
            Route::Walk => ProjectionRoute::Walk,
            Route::Split => ProjectionRoute::Split,
            Route::Complement => ProjectionRoute::Complement,
        })
    }

    /// The walk, for a caller that holds a fragment and a row space and nothing else — a bench
    /// arm, an example, a fixture that builds a projection directly. Identical to what
    /// [`Self::new`] returns on [`ProjectionRoute::Walk`].
    pub fn walk(fragment: &FrozenFragment, rows: &RowSpace) -> Self {
        Self::over(rows.project(&fragment.view()), rows)
    }

    /// This projection extended to `rows`, adding only the extents it does not already cover.
    ///
    /// **Equal to `RowProjection::new` over the same inputs, not an approximation.** An extent's
    /// rows begin exactly where row space ended, so the parts are disjoint and projecting the whole
    /// is projecting the parts unioned. `projecting_the_whole_equals_the_union_of_the_parts` in
    /// `tessera-store` pins the row-space half of that, and `tests/projection_patch.rs` pins this
    /// one end to end.
    ///
    /// Callers must check [`Self::extends_to`] first — this does not, because the answer decides
    /// whether the caller derives at all, and re-deriving it here would be a second place to get it
    /// wrong.
    pub fn extend(&self, fragment: &FrozenFragment, rows: &RowSpace) -> Self {
        let mut extended = self.rows.clone();
        extended.or_inplace(&rows.project_extents_from(&fragment.view(), self.extents_covered));
        Self::over(extended, rows)
    }

    /// Whether this projection is a valid starting point for a projection over `rows` — i.e.
    /// whether `rows` **extends** the row space this was built over rather than permuting it.
    ///
    /// See [`Self::boundary_seg_id`] for why one id comparison settles it.
    pub fn extends_to(&self, rows: &RowSpace) -> bool {
        let extents = rows.extents();
        if extents.len() < self.extents_covered {
            return false;
        }
        match (&self.boundary_seg_id, self.extents_covered) {
            (None, 0) => true,
            (Some(seg_id), n) => extents[n - 1].seg_id == *seg_id,
            // Unreachable from `over`, which sets the two together. Refusing is the fail-safe
            // direction: a needless rebuild, never a wrong projection.
            _ => false,
        }
    }

    /// This projection rebased onto `rows`: **the base's contribution kept, every extent
    /// re-projected.**
    ///
    /// The rung a *merge* needs. A merge permutes row space inside the merged span, so
    /// [`Self::extend`] refuses (correctly — [`Self::extends_to`] is exact), and the alternative
    /// was a full rebuild: a *measured* 1 277 ms at 10⁹, essentially all of it the base
    /// permutation's `project` over a 25% grant. This keeps that part and re-does only the
    /// extents, at a *measured* 0.24 ms each (`probes/2026-08-04-refresh-ladder/`).
    ///
    /// **Exact for any publication that leaves the base alone**, which is every flush and every
    /// merge: rows below `base_rows` are the base's and no publication within one prefix rewrites
    /// it, and everything at or above it is re-projected from scratch. Callers must check
    /// [`Self::can_rebase_extents`] first — this does not, for the same reason [`Self::extend`]
    /// does not.
    ///
    /// **Why not the narrower span-local rebase** the plan's ladder also names (clear only the
    /// merged span's row range, re-project only that span): it needs the publication to hand the
    /// refresh the merged extent's row range and the old projection's coverage to be reconciled
    /// against a shortened extent list — bookkeeping that has to be right at every construction
    /// site of a `Generation` — and it buys the difference between re-projecting one extent and
    /// re-projecting all of them, which the merge policy itself bounds. At 0.24 ms per extent the
    /// difference is not worth a second correctness argument.
    pub fn rebase_extents(&self, fragment: &FrozenFragment, rows: &RowSpace) -> Self {
        let mut rebased = self.rows.clone();
        rebased.remove_range(self.base_rows..u32::MAX);
        rebased.remove(u32::MAX);
        rebased.or_inplace(&rows.project_extents_from(&fragment.view(), 0));
        Self::over(rebased, rows)
    }

    /// Whether [`Self::rebase_extents`] is exact over `rows` — i.e. whether the base this
    /// projection was built over is the one `rows` addresses.
    ///
    /// A differing base row count means a different `permutation.bin`, which within one prefix
    /// cannot happen and across prefixes is a compaction. Refusing is the fail-safe direction: a
    /// needless rebuild, never a projection over the wrong row space.
    pub fn can_rebase_extents(&self, rows: &RowSpace) -> bool {
        rows.base_rows() == self.base_rows
    }

    fn over(rows: Bitmap, space: &RowSpace) -> Self {
        let extents = space.extents();
        let mut projection = Self::from_rows(rows);
        projection.extents_covered = extents.len();
        projection.boundary_seg_id = extents.last().map(|e| e.seg_id.clone());
        projection.base_rows = space.base_rows();
        projection
    }

    /// Build directly from an already-projected row-space bitmap (e.g. in tests, or when a
    /// caller has its own reason to hold the projection independently of a `FrozenFragment`).
    ///
    /// The result covers no extents, so [`Self::extends_to`] holds only over a row space with none.
    /// A projection meant to be extended must come from [`Self::new`].
    pub fn from_rows(mut rows: Bitmap) -> Self {
        // **Run containers, because a projection is held for a session and read for its life.**
        // The rows a grant projects to are a contiguous range wherever the grant covers a run of
        // row space, and a bitmap container spends 8 KiB stating what a run container states in
        // four bytes plus a count. A whole-corpus grant at 3.5×10⁹ rows is 53 407 containers:
        // 437 MB of bitmap containers, or 53 407 run containers of one run each, on the order of a
        // megabyte. Run form is per container and never global — the count does not fall, only what
        // each container costs — and `run_optimize` converts one only where the run form is
        // smaller, so a projection that runs badly keeps the representation it had.
        //
        // This is also what `cache_weight_bytes` charges the row-projection cache, so the cache's
        // bound is over the bytes the projection actually holds rather than over the bytes it would
        // have held unoptimised.
        //
        // **Paid again at every publication, for every resident session.** The background refresh
        // (`crate::refresh`) rebuilds or patches each resident projection at each geometry
        // publication and every route lands here, so a flush pays this per session rather than
        // once. The cost is `O(containers)`: croaring walks each container to decide whether its
        // run form would be smaller, and for a scattered mask the answer is no everywhere and it
        // converts nothing. That is the shape of this cache's own sizing — containers, not
        // cardinality — and it is what a session with a scattered grant pays for the sessions with
        // contiguous ones.
        rows.run_optimize();
        let cardinality = rows.cardinality();
        RowProjection {
            rows,
            // Covering no extents, which is what an unattached bitmap can honestly claim. A caller
            // that wants a derivable projection goes through `RowProjection::new`.
            extents_covered: 0,
            boundary_seg_id: None,
            // Likewise: an unattached bitmap addresses no base, so `can_rebase_extents` holds only
            // over a row space with none.
            base_rows: 0,
            cardinality,
        }
    }

    /// The number of rows in this projection — O(1), memoised at construction.
    pub fn cardinality(&self) -> u64 {
        self.cardinality
    }

    pub fn bitmap(&self) -> &Bitmap {
        &self.rows
    }

    pub fn contains(&self, row: u32) -> bool {
        self.rows.contains(row)
    }

    pub fn range_cardinality(&self, r: Range<u32>) -> u64 {
        self.rows.range_cardinality(r)
    }
}
