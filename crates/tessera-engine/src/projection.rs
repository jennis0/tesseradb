//! The cached row-space projection of one session's frozen fragment.
//!
//! [`RowProjection`] is the `Permutation::project` output for one `(token, view, pin)`, built once
//! and reused for the session's life: never rebuild it per viewport, it costs seconds at 10⁹ rows.
//! A publication patches a resident projection instead, where it can: [`RowProjection::extend`]
//! after a flush, which only appends extents; [`RowProjection::rebase_extents`] after a merge,
//! which rewrites row space within its span but never the base; a full [`RowProjection::new`]
//! otherwise.

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

/// What a row projection is built from, beside the row space it is built into: enough for the
/// route to be priced and chosen before any of it is read.
pub struct ProjectionInputs<'a> {
    /// The session's mask fragment, in entity space.
    pub fragment: &'a FrozenFragment,
    /// The terms the session satisfies, ascending and deduplicated.
    pub satisfied: &'a [TermId],
    /// The generation's base postings.
    pub postings: &'a PostingsReader,
    /// The generation's live delta tiers.
    pub deltas: &'a [Arc<DeltaTier>],
    /// This view's term images, where the bundle carries them and they opened.
    pub images: Option<&'a TermImages>,
    /// A route to take instead of the chosen one. `None` in every shipped path.
    pub force: Option<ProjectionRoute>,
}

/// How a row projection was built. Every route returns the identical projection; only the work
/// differs. A grant covering the whole domain takes [`ProjectionRoute::WholeDomain`] and reads no
/// page. Otherwise the cheapest of the other three is priced from the grant alone, before any of
/// them runs: a small grant walks, a grant whose terms mostly have a stored image unions the
/// images and walks only the residual (entities of the unimaged terms, plus anything held only in
/// a delta posting), and a grant so wide that what it excludes is small walks that exclusion and
/// subtracts it from the row range instead. The per-entity and per-container rates the prices are
/// built from are `tessera_store::term_images::ROUTE_COSTS`; the walk wins a tie.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectionRoute {
    /// The grant covers the whole entity domain and the mapping is a bijection onto its rows.
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
/// build is counted. Shared by the request path and the background refresh, so a forced route
/// reaches both.
#[derive(Debug, Default)]
pub struct ProjectionRoutes {
    counts: [AtomicU64; 4],
    /// The forced route as `ProjectionRoute::index() + 1`, or zero for the chosen route.
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
        let encoded = route.map_or(0, |route| route.index() as u8 + 1);
        self.forced.store(encoded, Ordering::Relaxed);
    }

    /// Build a projection by [`RowProjection::new`] and count the route it took.
    ///
    /// A split whose postings cannot be read falls back to the walk instead, warned and counted as
    /// a walk: the same rows, served more slowly.
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

/// A cached row-space projection of one frozen fragment, for one `(token, view, pin)`. `compose`
/// reads it only through `range_cardinality` and `contains`, both O(containers touched), never
/// re-deriving it from the fragment.
pub struct RowProjection {
    rows: Bitmap,
    /// How many of the view's extents this projection covers, the index [`RowProjection::extend`]
    /// resumes projecting from when a flush adds more.
    extents_covered: usize,
    /// The `seg_id` of the last extent covered, or `None` when only the base is. Segment ids are
    /// never reused, so this one comparison detects a merge: a flush leaves extents
    /// `[0, extents_covered)` untouched, but a merge collapsing an overlapping run gives the extent
    /// now at `extents_covered - 1` a different id, or puts it out of range.
    boundary_seg_id: Option<String>,
    /// The base permutation's row count at construction: the boundary between rows a merge can
    /// never move and rows it can. Every row below it comes from `permutation.bin`, which no flush
    /// or merge rewrites within one prefix.
    base_rows: u32,
    /// `rows.cardinality()`, computed once because the density anchor reads it on every viewport
    /// and `Bitmap::cardinality` is O(containers): roughly 15,000 for a 2.5×10⁸-row projection at
    /// 10⁹ rows.
    cardinality: u64,
}

impl RowProjection {
    /// Project `inputs.fragment` into this view's row space, by whichever route costs least, and
    /// say which route that was. Do not call this on the per-viewport path — see this struct's doc.
    ///
    /// The route is chosen from the principal's own grant, before any route runs; nothing about
    /// another principal or the overlay enters it. The projection is pre-overlay, as this type has
    /// always been: a suppressed entity stays in its term's posting and image exactly as it stays
    /// in the fragment, and the overlay is composed on every request, not here.
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
            // A forced whole-domain route with no answer walks, and the gauge reports that instead.
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
                // The views live inside `union`, dropped when it returns; nothing caches them.
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
                // The base's row count is not recorded, so the walk answers a forced complement too.
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
    /// where there is nothing to price against. The complement is only offered where the base
    /// records the row count its slots are a bijection onto; a view with no image table, or a
    /// session holding no term with one, has no split to price either.
    fn price(
        inputs: &ProjectionInputs<'_>,
        rows: &RowSpace,
        fragment: &croaring::Bitmap,
    ) -> io::Result<ProjectionRoute> {
        let bound = rows.base().bound();
        // A bound of zero, or above the `u32` entity ceiling, has no route to price against the walk.
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

    /// The walk, identical to what [`Self::new`] returns on [`ProjectionRoute::Walk`].
    pub fn walk(fragment: &FrozenFragment, rows: &RowSpace) -> Self {
        Self::over(rows.project(&fragment.view()), rows)
    }

    /// This projection extended to `rows`, adding only the extents it does not already cover.
    /// Exact: an extent's rows begin exactly where row space ended, so projecting the whole is
    /// projecting the disjoint parts unioned. Callers must check [`Self::extends_to`] first; this
    /// does not check it.
    pub fn extend(&self, fragment: &FrozenFragment, rows: &RowSpace) -> Self {
        let mut extended = self.rows.clone();
        extended.or_inplace(&rows.project_extents_from(&fragment.view(), self.extents_covered));
        Self::over(extended, rows)
    }

    /// Whether `rows` extends the row space this was built over, rather than permuting it, so
    /// [`Self::extend`] is valid to call.
    pub fn extends_to(&self, rows: &RowSpace) -> bool {
        let extents = rows.extents();
        if extents.len() < self.extents_covered {
            return false;
        }
        match (&self.boundary_seg_id, self.extents_covered) {
            (None, 0) => true,
            (Some(seg_id), n) => extents[n - 1].seg_id == *seg_id,
            // Unreachable from `over`, which sets the two together.
            _ => false,
        }
    }

    /// This projection rebased onto `rows`: the base kept, every extent re-projected. What a merge
    /// needs, since it permutes row space within its span and [`Self::extend`] cannot be used
    /// there; exact because a merge never rewrites the base. A full rebuild measured 1,277 ms at
    /// 10⁹ rows, almost all of it the base projection; this measured 0.24 ms per extent instead.
    /// Callers must check [`Self::can_rebase_extents`] first; this does not check it.
    pub fn rebase_extents(&self, fragment: &FrozenFragment, rows: &RowSpace) -> Self {
        let mut rebased = self.rows.clone();
        rebased.remove_range(self.base_rows..u32::MAX);
        rebased.remove(u32::MAX);
        rebased.or_inplace(&rows.project_extents_from(&fragment.view(), 0));
        Self::over(rebased, rows)
    }

    /// Whether the base this projection was built over is the one `rows` addresses, so
    /// [`Self::rebase_extents`] is exact. A differing base row count means a compaction, never a
    /// flush or merge within one prefix.
    pub fn can_rebase_extents(&self, rows: &RowSpace) -> bool {
        rows.base_rows() == self.base_rows
    }

    fn over(rows: Bitmap, space: &RowSpace) -> Self {
        let extents = space.extents();
        Self::covering(
            rows,
            extents.len(),
            extents.last().map(|e| e.seg_id.clone()),
            space.base_rows(),
        )
    }

    /// Build over `rows` with the coverage it claims: the one construction site, so a projection
    /// is never half-initialised.
    fn covering(
        mut rows: Bitmap,
        extents_covered: usize,
        boundary_seg_id: Option<String>,
        base_rows: u32,
    ) -> Self {
        // A run container states four bytes plus a count where a bitmap container spends 8 KiB, so
        // this keeps a contiguous grant small: a whole-corpus grant at 3.5×10⁹ rows is 437 MB as
        // bitmap containers against roughly a megabyte run-optimised. `cache_weight_bytes` charges
        // the cache on the result.
        rows.run_optimize();
        let cardinality = rows.cardinality();
        RowProjection {
            rows,
            extents_covered,
            boundary_seg_id,
            base_rows,
            cardinality,
        }
    }

    /// Build directly from an already-projected row-space bitmap. Covers no extents, so
    /// [`Self::extends_to`] and [`Self::can_rebase_extents`] hold only over a row space with none;
    /// a projection meant to be extended must come from [`Self::new`].
    pub fn from_rows(rows: Bitmap) -> Self {
        Self::covering(rows, 0, None, 0)
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
