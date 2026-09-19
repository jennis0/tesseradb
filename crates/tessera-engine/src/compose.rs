//! I1 mask composition: `M_auth = (fragment \ L) ∪ direct_eval(L)` — evaluated as diffs over a
//! cached row-space projection of the frozen fragment, never by recomputing the whole mask.
//!
//! [`compose`] walks `L = keys(buffer)` exactly once per entity, resolving each with the fixed
//! precedence `deleted > suppressed > buffered`, and turns the
//! result into two row-space bitmaps against `base`:
//! `minus = {row(e) : e fails} ∩ base` and `plus = {row(e) : e passes} ∖ base`. The `∩ base` /
//! `∖ base` clamps are load-bearing, not cosmetic: without them, denying an entity the session's
//! fragment never contained would corrupt every count over its tile (a spurious −1, possibly
//! driving a count negative), and a pass already inside the fragment would double-count
//! its tile by the same mechanism in the other direction.
//!
//! **Deletions and suppressions are not in that walk.** They arrive as `Generation::denied` — the
//! row-space image of `deleted ∪ suppressed`, derived by [`derive_denied`] — and are folded in as
//! `minus ∪= denied ∩ base`, `plus ∖= denied`. Two reasons, and the second is the load-bearing
//! one. The walk was O(denies **ever accepted**) per request, which is a cost curve nothing
//! retires: one of the two retirement rules does not exist, so the deny set only grows. And
//! `andnot` is self-clamping, so the deny half can no longer get the clamps above wrong at all —
//! a deny cannot lose an ordering argument it never enters.
//!
//! [`verdict`] remains the single expression of the precedence, for the entity-space verbs
//! ([`visible_to`], label gating, cluster visibility) and for this walk alike. The two
//! representations are licensed by the differential obligation that they agree for every entity
//! with a row (`tests/deny_mask.rs`).

use std::ops::Range;
use std::sync::Arc;

use croaring::Bitmap;
use rustc_hash::FxHashSet;

use tessera_authz::FrozenFragment;
use tessera_lifecycle::{IngestBuffer, Overlay};
use tessera_store::{Bundle, RowSpace};
use tessera_types::{EntityId, TermId};

pub use crate::projection::RowProjection;
use crate::DenyMask;

/// An attribute filter's rows, **with the part of row space they are an answer about**.
///
/// The two crossings from the filter's entity-space result into row space produce sets of
/// different extent, and the difference is not an implementation detail a consumer may ignore.
/// [`RowSpace::project`](tessera_store::permutation::RowSpace::project) crosses the whole result
/// and yields every matching row in the view; the per-tile route tests only the rows the
/// request's tiles actually span, and is *silent* — not negative — everywhere else. Handing a
/// counting path the second while it believes it holds the first under-reports `matched` with no
/// error anywhere, which is why the extent travels with the bitmap rather than in a comment at
/// the call site.
pub enum FilterRows {
    /// Every matching row in the view. Exact at any range.
    Complete(Bitmap),
    /// Only the rows inside `domain` were tested. Outside it the bitmap is empty and that
    /// emptiness means nothing at all.
    ///
    /// `domain` is ascending, disjoint and maximally merged — [`Self::covers`] binary-searches it.
    Viewport {
        rows: Bitmap,
        domain: Vec<Range<u32>>,
    },
}

impl FilterRows {
    pub(crate) fn rows(&self) -> &Bitmap {
        match self {
            FilterRows::Complete(rows) => rows,
            FilterRows::Viewport { rows, .. } => rows,
        }
    }

    /// Is `r` a range this set can answer about?
    ///
    /// The debug assertions below are the only callers. They are assertions rather than a
    /// fail-closed check because the domain is not a permission boundary: it is the request's own
    /// tile set, computed three statements earlier in the same function, and a range outside it is
    /// a coding error in this crate rather than anything a caller can provoke. What the assertion
    /// buys is that the error surfaces in the test suite instead of as a quietly low `matched`.
    fn covers(&self, r: &Range<u32>) -> bool {
        let domain = match self {
            FilterRows::Complete(_) => return true,
            FilterRows::Viewport { domain, .. } => domain,
        };
        if r.start >= r.end {
            return true;
        }
        // The range containing `r.start`, if any — `domain` is disjoint and ascending, so at most
        // one qualifies, and `r` is covered exactly when that one also reaches `r.end`.
        let found = domain.binary_search_by(|d| {
            if d.end <= r.start {
                std::cmp::Ordering::Less
            } else if d.start > r.start {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        });
        matches!(found, Ok(i) if domain[i].end >= r.end)
    }
}

/// The composed, effective visibility mask for one request: `base` plus a small diff (`minus`,
/// `plus`) capturing every overlay/buffer effect since `base` was cached. Every question an
/// artifact's membership asks costs O(containers touched) in the diffs, which are expected to be
/// tiny relative to `base`, and materialises nothing.
///
/// [`WholeMask::visible_all`] is the one answer that needs the whole set. It borrows `base` where
/// the diffs are empty, and otherwise materialises once into [`Self::whole`], so a request that
/// asks for it twice pays for it once.
pub struct EffectiveMask {
    base: Arc<RowProjection>,
    minus: Bitmap,
    plus: Bitmap,
    /// The rows an attribute filter admits, or `None` when the request carried no filter.
    ///
    /// **Applied above composition, never folded into `base`** (`filter-surface.md` §5.1). Folding
    /// it in would break the structural invariants the diffs are asserted against and the
    /// unfiltered total θ must anchor on — and the anchor staying unfiltered is **I12**: a filter
    /// may move the frontier up, never down.
    ///
    /// It is a *row-space* set because that is the space counts are taken in. Its entity-space
    /// origin already met the composed verdict, so intersecting here narrows and cannot widen.
    filter: Option<FilterRows>,
    /// The rows the request's **highlight** admits, or `None` where it carried none
    /// (`highlight-and-hierarchy.md` §2).
    ///
    /// **A second field beside `filter`, never folded into it**, and that separation is the whole
    /// of what makes a highlight a highlight: [`Self::rows_in_range`] — which selection draws
    /// from, and which every `visible`/`matched` count is taken through — never reads this, so the
    /// served set is identical with and without one. Only the three answers §2 adds read it: the
    /// per-tile `highlighted` count, the per-point bit and the per-artifact bit.
    ///
    /// Its extent is the request's own tiles: a highlight always takes the per-tile crossing, so
    /// [`FilterRows::Viewport`] is the ordinary shape here and every question asked of it is
    /// inside the domain by construction.
    highlight: Option<FilterRows>,
    /// `(base − minus) ∪ plus` materialised, filled by the first [`WholeMask::visible_all`] that
    /// needs it and left empty on a mask that denies nothing.
    ///
    /// Nothing narrows or widens it after composition. [`Self::with_filter`] and
    /// [`Self::with_highlight`] set fields this is not derived from, so a filtered mask and the
    /// unfiltered one it was built from hold the same set here. I12 requires that of every quantity
    /// an artifact's existence is decided by.
    whole: std::sync::OnceLock<Bitmap>,
}

/// The two questions an artifact's membership asks of a viewer's mask.
///
/// **A trait so that the one production implementor is [`EffectiveMask`] and nothing else.** An
/// artifact's masked count must be taken against the *composed* mask — base minus the overlay's
/// denials plus the buffer's — and the pre-overlay projection strictly contains it after any
/// accepted delete or suppression. A predicate that could be handed a raw bitmap would serve counts
/// over items already denied, and would do it silently. The `Bitmap` implementor below is
/// test-only, and its absence from a release build is what makes that a compile error rather than a
/// review finding.
pub trait MaskedSet {
    /// `|set ∩ mask|` — an artifact's **masked count**.
    ///
    /// O(containers touched), never O(cardinality), which is what lets the count be taken over a
    /// whole membership rather than tile by tile.
    ///
    /// **Deliberately blind to any attribute filter**, exactly as [`EffectiveMask::visible_total`]
    /// is. The count beside an artifact is what the *principal* may see, not what their current
    /// search box admits; a filtered count there would make the artifact's existence criterion a
    /// function of the filter, so an artifact would appear and disappear as a viewer typed — a
    /// filter moving the frontier down, which **I12** forbids. Whether an artifact should *also*
    /// carry a filtered figure beside the masked one is ⊘ the open filter axis (architecture §8.4),
    /// and adding one here without ruling it would settle it by accident.
    fn count_intersection(&self, set: &Bitmap) -> u64;

    /// Whether `set` holds any visible row — candidacy, answered as a **masked** question.
    ///
    /// The alternative an early draft of the artifact design took was a build-time bounding box
    /// over full membership, served wherever the box intersected the viewport. That discloses the
    /// unmasked extent by panning: a viewer sees a shape's edge in a region holding nothing they
    /// may see. Asking the mask instead makes the fault unexpressible.
    fn intersects_set(&self, set: &Bitmap) -> bool;

    /// `set ∩ mask`, materialised — the rows of an artifact's membership that **this** viewer may
    /// see.
    ///
    /// The one input to derived content, and the reason it is on this trait rather than beside it:
    /// *a derived property is a function of `membership ∩ M_auth` and of nothing else*
    /// (`annotations.md` §4.2). A centroid computed over the membership and then served to a viewer
    /// who sees a tenth of it describes documents they may not see — the count's failure mode in a
    /// shape nobody thinks to check, because the geometry *looks* like something the engine derived.
    /// Handing the computation a bitmap it did not get from here is the way that happens, so the
    /// only way to obtain one is to ask the composed mask for it.
    ///
    /// Materialising is the cost derived content opts into: O(visible members), against the count's
    /// O(containers touched). That asymmetry is why the vocabulary is declared per layer.
    fn visible_rows(&self, set: &Bitmap) -> Bitmap;

    /// `|[r.start, r.end) ∩ mask|` — the masked count of a contiguous **row range**.
    ///
    /// **The one question a range-shaped membership asks**, and it is on this trait rather than
    /// beside it for [`MaskedSet`]'s own reason: a spatial level's membership is a set of ranges,
    /// so this *is* asking the mask about an artifact's membership, one contiguous piece at a time.
    /// A caller that summed `count_intersection` over materialised ranges would get the same number
    /// and pay a bitmap per range to do it.
    ///
    /// **Filter-blind, exactly as [`MaskedSet::count_intersection`] is** — the count beside an
    /// artifact is what the principal may see, not what their search box admits (**I12**).
    ///
    /// **The default is the general answer and the production one is the cheap answer.** Any mask
    /// can answer this by materialising the range and intersecting; [`EffectiveMask`] overrides it
    /// with the same three-term arithmetic its other counts take, which is O(containers touched)
    /// and allocates nothing. Both compute the same number from inside `M_auth`, which is what
    /// makes the default safe rather than merely convenient.
    fn count_range(&self, r: Range<u32>) -> u64 {
        self.count_intersection(&Bitmap::from_range(r))
    }
}

/// The **whole** composed mask, materialised — every row this viewer may see, in this view's row
/// space.
///
/// **Two callers, and the first is the row-major count** ([`crate::row_column::RowColumn::histogram`]).
/// A row-major level has no per-artifact membership to intersect, so its only route to
/// `|membership ∩ M_auth|` is a walk of the mask reading off which artifact each visible row belongs
/// to — the one place [decision 0093](../../../docs/decisions/0093-nothing-is-materialised-per-token-over-the-artifact-population.md)
/// admits a structure sized by the artifact population per session, and it is admitted because there
/// is no other route.
///
/// **The second is [`crate::tile_index::Viewport::compose`]**, which borrows this set where the
/// viewport holds every row the viewer may see, rather than intersecting to a copy of it. It reads
/// the same set the histogram walks, which is what makes the two agree at that viewport.
///
/// **Its own trait rather than a third method on [`MaskedSet`]**, for a reason that is about the
/// question rather than about tidiness: `MaskedSet` is *the questions an artifact's membership asks
/// of a viewer's mask* — both its methods take the membership as an argument — and this asks nothing
/// about any artifact. Keeping it separate also keeps the probe that measures the routes
/// (`tessera-bench`) implementing exactly the trait the per-artifact routes need.
///
/// **The safety property is the same one and it is unchanged**: the one production implementor is
/// [`EffectiveMask`], so a whole-mask walk cannot be taken against the pre-overlay projection, which
/// strictly contains `M_auth` after any accepted delete. The [`Bitmap`] implementor below is
/// test-only.
pub trait WholeMask {
    /// See the trait's doc.
    ///
    /// **Filter-blind, exactly as [`MaskedSet::count_intersection`] is.** The count beside an
    /// artifact is what the *principal* may see, not what their current search box admits;
    /// anchoring it on a filtered set would make an artifact's existence criterion a function of the
    /// filter, which is **I12**'s forbidden direction. `visible_all().and_cardinality(set)` and
    /// `count_intersection(set)` are therefore the same number by construction, which the test
    /// beside the implementation asserts rather than assumes.
    ///
    /// Borrowed, and materialised at most once for the life of the mask. The set is the
    /// projection's own size, 437 MB at the rung 6 corpus, so a caller that copies it per request
    /// pays that copy per request. Where the mask denies nothing the borrow is of the projection
    /// itself and nothing is materialised. Where it denies something the difference is materialised
    /// on the first ask and every later ask in the request borrows it.
    fn visible_all(&self) -> &Bitmap;

    /// `|M_auth|`, without materialising the set.
    ///
    /// The composition is three terms whose cardinalities add, so a caller that needs only the size
    /// never pays the copy. The default is the general answer; [`EffectiveMask`] overrides it with
    /// the arithmetic [`EffectiveMask::visible_total`] takes, which is the one θ anchors on.
    fn visible_count(&self) -> u64 {
        self.visible_all().cardinality()
    }
}

impl WholeMask for EffectiveMask {
    /// `(base − minus) ∪ plus` — the same three terms in the same order the count takes, with no
    /// `set` to narrow by. `minus ⊆ base` and `plus ∩ base = ∅` hold structurally ([`compose`]
    /// asserts them), so every row appears once and the cardinality of what comes back is
    /// [`EffectiveMask::visible_total`] exactly.
    ///
    /// A mask that denies nothing is its projection: `(base − ∅) ∪ ∅ = base`, so that arm copies
    /// nothing. The other arm copies, because `base` is shared with every other session holding the
    /// same projection and the difference is this mask's alone. It copies once, into
    /// [`Self::whole`], and that copy is what the request's viewport and its histogram walk both
    /// read.
    fn visible_all(&self) -> &Bitmap {
        if self.minus.is_empty() && self.plus.is_empty() {
            return self.base.bitmap();
        }
        self.whole.get_or_init(|| {
            let mut visible = self.base.bitmap().clone();
            visible.andnot_inplace(&self.minus);
            visible.or_inplace(&self.plus);
            visible
        })
    }

    /// [`EffectiveMask::visible_total`] under the trait — one implementation, so the anchor and
    /// this cannot disagree about what composition means.
    fn visible_count(&self) -> u64 {
        EffectiveMask::visible_total(self)
    }
}

/// A mask with no denials — **test-only**, for [`MaskedSet`]'s reason.
#[cfg(test)]
impl WholeMask for Bitmap {
    fn visible_all(&self) -> &Bitmap {
        self
    }
}

impl MaskedSet for EffectiveMask {
    /// **The same term-by-term arithmetic as [`EffectiveMask::count_range`]**, and exact for the
    /// same reason: `minus ⊆ base` and `plus ∩ base = ∅` are the structural invariants [`compose`]
    /// asserts, so nothing is subtracted twice and nothing is added that was already there.
    fn count_intersection(&self, set: &Bitmap) -> u64 {
        let base_count = self.base.bitmap().and_cardinality(set);
        let minus_count = self.minus.and_cardinality(set);
        let plus_count = self.plus.and_cardinality(set);
        base_count - minus_count + plus_count
    }

    fn intersects_set(&self, set: &Bitmap) -> bool {
        // `plus` first: it is tiny, and a hit there settles the question without touching `base`.
        if self.plus.intersect(set) {
            return true;
        }
        if !self.base.bitmap().intersect(set) {
            return false;
        }
        // `base` hits, so the only remaining question is whether every hit was denied. Materialised
        // only on this path, and only over `set` — an artifact's membership, not the row space.
        let mut visible = self.base.bitmap().and(set);
        visible.andnot_inplace(&self.minus);
        !visible.is_empty()
    }

    /// The same term-by-term composition the count takes, materialised: `(base ∩ set) − minus`,
    /// then the buffer's additions that fall inside `set`. `plus ∩ base = ∅` holds structurally, so
    /// the union adds each row once and the cardinality of what comes back equals
    /// [`Self::count_intersection`] exactly — which the test beside it asserts rather than assumes,
    /// because a derived property computed over a different set from the count served beside it is
    /// the disagreement a viewer would see and could not explain.
    fn visible_rows(&self, set: &Bitmap) -> Bitmap {
        let mut visible = self.base.bitmap().and(set);
        visible.andnot_inplace(&self.minus);
        visible.or_inplace(&self.plus.and(set));
        visible
    }

    /// [`EffectiveMask::count_range`] under the trait — one implementation, so a range counted
    /// through the predicate and one counted directly cannot disagree.
    fn count_range(&self, r: Range<u32>) -> u64 {
        EffectiveMask::count_range(self, r)
    }
}

/// A mask with no denials — **test-only**, so that no release build can put an uncomposed set where
/// a composed one belongs. See [`MaskedSet`].
#[cfg(test)]
impl MaskedSet for Bitmap {
    fn count_intersection(&self, set: &Bitmap) -> u64 {
        self.and_cardinality(set)
    }

    fn intersects_set(&self, set: &Bitmap) -> bool {
        self.intersect(set)
    }

    fn visible_rows(&self, set: &Bitmap) -> Bitmap {
        self.and(set)
    }
}

impl EffectiveMask {
    /// Narrow this mask by an attribute filter's row-space set.
    ///
    /// Consumes and returns, so a filtered mask cannot be built by mutating one already handed to
    /// a counting path — the unfiltered mask and the filtered one are different values.
    pub fn with_filter(mut self, rows: FilterRows) -> Self {
        self.filter = Some(rows);
        self
    }

    /// Attach the request's highlight — see [`Self::highlight`].
    ///
    /// Consumes and returns for [`Self::with_filter`]'s reason, and the order of the two is free:
    /// they are separate fields and neither is read by the other's answers.
    pub fn with_highlight(mut self, rows: FilterRows) -> Self {
        self.highlight = Some(rows);
        self
    }

    /// Whether this request carried a highlight — which decides whether the *points* frame has a
    /// `highlighted` column at all, and whether the artifacts frame's bit is `null`.
    pub fn has_highlight(&self) -> bool {
        self.highlight.is_some()
    }

    /// The rows in `r` that are visible, match the request's filter **and** satisfy its highlight
    /// — `TileCount::highlighted` (`highlight-and-hierarchy.md` §2).
    ///
    /// **Equal to [`Self::count_matched_range`] with no highlight**, which is what makes the
    /// column always present on the wire rather than optional: an absent highlight is the
    /// identity, and `highlighted = matched` says the same thing a missing column would, at eight
    /// bytes a tile and with no schema to branch on.
    ///
    /// `highlighted ≤ matched ≤ visible` holds by construction: this is the intersection of the
    /// set `count_matched_range` counts with one more.
    pub fn count_highlighted_range(&self, r: Range<u32>) -> u64 {
        match &self.highlight {
            None => self.count_matched_range(r),
            Some(highlight) => {
                debug_assert!(
                    highlight.covers(&r),
                    "a highlighted count over {r:?}, which the per-tile crossing never tested"
                );
                self.rows_in_range(r).and_cardinality(highlight.rows())
            }
        }
    }

    /// Whether one **served** row satisfies the highlight — the *points* frame's bit.
    ///
    /// The row came out of selection, so it is already inside `M_auth` and inside the filter's
    /// candidate; what is left to ask is the highlight alone, which is why this is one `contains`
    /// and not a composition. `false` where the request carried no highlight, which no caller
    /// reads: the column is absent from the frame then.
    pub fn is_highlighted(&self, row: u32) -> bool {
        self.highlight
            .as_ref()
            .is_some_and(|highlight| highlight.rows().contains(row))
    }

    /// The rows of `here` that satisfy **`all_of[filters, highlight]`** — the artifacts frame's
    /// `highlighted` bit (`highlight-and-hierarchy.md` §2), or `None` where the request carried no
    /// highlight and there is no question to answer.
    ///
    /// [`Self::matched_rows`]'s rules hold unchanged, `here` having to come from
    /// [`MaskedSet::visible_rows`]: this narrows that answer by one more set and so stays inside
    /// `M_auth` whatever either expression matched. It is decision 0104's bit computed for the
    /// conjunction, which is why it is the same shape and not a second kind of answer.
    pub fn highlighted_rows(&self, here: &Bitmap) -> Option<Bitmap> {
        let highlight = self.highlight.as_ref()?;
        Some(match self.matched_rows(here) {
            Some(matched) => matched.and(highlight.rows()),
            None => here.and(highlight.rows()),
        })
    }

    /// `base.range_cardinality(r) − |minus ∩ r| + |plus ∩ r|` — see this module's doc for why the
    /// two clamps in [`compose`] make this arithmetic exact rather than merely approximate.
    pub fn count_range(&self, r: Range<u32>) -> u64 {
        let base_count = self.base.range_cardinality(r.clone());
        let minus_count = self.minus.range_cardinality(r.clone());
        let plus_count = self.plus.range_cardinality(r);
        base_count - minus_count + plus_count
    }

    /// The total number of visible rows in this mask, over the whole row space — §7.2's `V_total`,
    /// the quantity θ's anchor is derived from.
    ///
    /// **This is the composed figure, not the projection's.** I2 requires every aggregate be
    /// computable from inside `M_auth` alone, and `base` is `M_auth` *before* the overlay diff:
    /// after an accepted delete or suppression it strictly contains `M_auth`. Anchoring θ on
    /// `base.cardinality()` alone would let a viewer aggregate mark counts across tiles, solve for
    /// the anchor, difference it against its own summed per-tile `visible` (which §7.1 discloses
    /// exactly), and recover a running estimate of how many of its own items have been denied —
    /// a count of items *outside* `M_auth`. See [`crate::select::Threshold::at_depth`], and
    /// [`crate::occupancy`] for `N_occ(d)`, θ's second anchor, which takes this rule for the same
    /// reason.
    ///
    /// **Deliberately the same arithmetic as [`Self::count_range`]**, one term at a time, so the
    /// anchor and the per-tile counts can never disagree about what composition means. Two
    /// transcriptions of the composition rule is the same failure mode as two transcriptions of the
    /// deny precedence — see [`verdict`]'s doc.
    ///
    /// O(containers in the diffs): `base`'s cardinality is memoised
    /// ([`RowProjection::cardinality`]) and the diffs are tiny by construction.
    /// **Deliberately blind to `filter`**, which is what makes it θ's anchor.
    ///
    /// §8.4 and **I12**: anchoring the selection threshold on *filtered* counts would make θ a
    /// function of the filter, so the frontier would coarsen as a viewer typed — a filter moving
    /// the frontier down, which I12 forbids. The anchor is the composed mask's own total, filter or
    /// no filter, and this method is the one `Threshold::at_depth` takes `V_total` from.
    pub fn visible_total(&self) -> u64 {
        self.base.cardinality() - self.minus.cardinality() + self.plus.cardinality()
    }

    /// The rows in `r` that are visible **and** match the request's filter — `TileCount::matched`.
    ///
    /// **Distinct from [`Self::count_range`], which stays the composed *visible* count.** The two
    /// are separate wire fields because they answer different questions: `visible` is how many
    /// items in this tile the principal may see, `matched` how many of those the filter admits.
    /// Collapsing them would make a filter look like a permission change, and would put a
    /// filter-dependent quantity where §7.1 discloses an exact composed one.
    ///
    /// Without a filter the two agree, and this returns `count_range` rather than materialising a
    /// bitmap to reach the same number.
    pub fn count_matched_range(&self, r: Range<u32>) -> u64 {
        match &self.filter {
            None => self.count_range(r),
            // With a filter the term-by-term arithmetic no longer holds — a row can be in `base`
            // and out of the filter — so this materialises the intersection. O(containers in the
            // range), not O(rows).
            Some(_) => self.rows_in_range(r).cardinality(),
        }
    }

    /// The rows of `here` this request's filter admits — `here ∩ M_sel` — or `None` where the
    /// request carried no filter and there is no question to answer
    /// ([decision 0104](../../../docs/decisions/0104-a-filter-answers-a-boolean-per-served-artifact.md)).
    ///
    /// **The one filter-aware question an artifact may ask**, and it is on this type rather than on
    /// [`MaskedSet`] because that trait is deliberately filter-blind: the masked count and the
    /// existence criterion read it, and a filter reaching either would make an artifact appear and
    /// vanish as a viewer typed (**I12**). The bit this feeds is a boolean beside the count and
    /// never a second count.
    ///
    /// **`here` must be `viewport ∩ M_auth`** — a set obtained from
    /// [`MaskedSet::visible_rows`] and from nothing else, exactly as derived content's input is.
    /// The intersection then narrows an already-composed set and cannot widen it, and the answer
    /// stays inside `M_auth` whatever the filter matched.
    ///
    /// **Scoped to the viewport because that is the extent every crossing route can answer over.**
    /// The per-tile crossing and the render-column route are *silent* outside the request's tiles
    /// rather than negative there ([`FilterRows`]), so a whole-membership bit would be exact on one
    /// route and quietly narrow on the other two. `here` is inside the domain by construction, its
    /// rows being the request's own tiles.
    pub fn matched_rows(&self, here: &Bitmap) -> Option<Bitmap> {
        self.filter.as_ref().map(|filter| here.and(filter.rows()))
    }

    /// Every range this mask is asked about must be one the filter was evaluated over — see
    /// [`FilterRows`]. Called from the two methods that read `filter`; compiled out in release.
    #[inline]
    fn debug_assert_in_domain(&self, r: &Range<u32>) {
        debug_assert!(
            self.filter.as_ref().is_none_or(|f| f.covers(r)),
            "filtered count over {r:?}, which the per-tile crossing never tested — the answer \
             would be silently low. Either the range came from outside the request's tile set or \
             the domain was built from something other than `ranges`."
        );
    }

    /// The effective mask restricted to `r`: `(base ∩ r) ∖ minus ∪ (plus ∩ r)`, as a bitmap.
    ///
    /// **Returns the set, not an iterator, and that is the point.** This replaced an `iter_range`
    /// that ended in `result.to_vec().into_iter()` — an *eager* `Vec<u32>` of every visible row in
    /// the range, materialised in full before the caller's first `next()`. At 10⁹ with a viewport
    /// spanning ~2.5×10⁷ visible rows that is ~100 MB allocated and written per request, on a path
    /// whose entire cost model (this module's doc, and CLAUDE.md) is "O(containers touched), not
    /// O(cardinality)". The bitmap is O(containers) — roughly 3 MB for the same set — and callers
    /// iterate it lazily: per value with `.iter()`, per run via
    /// [`Self::for_each_visible_run`]'s diffs-present fallback, or caller-driven via
    /// [`Self::decode_source`]'s `Composed` variant (selection's decode paths).
    ///
    /// The eagerness was easy to miss because the old placeholder sampler `break`ed after *k* rows:
    /// the break saved the *gather*, never the materialisation, so the cost did not show up in the
    /// shape of the code. Selection now consumes every visible row by design — `C_θ` is a count over
    /// the whole tile — so the allocation was pure waste either way. This is Win 1 of
    /// docs/evidence/memos/2026-07-30-f1-selection-overdraw.md.
    ///
    /// **Where that memo's row counter went.** It asked for `iter_range`'s `ExactSizeIterator` so
    /// the row counter could be incremented by a free `.len()`. Returning a bitmap loses
    /// `ExactSizeIterator`, so `Selection::of` counts the rows it reads as it reads them
    /// (`Selection::rows_visited`) — one increment on a loop that was already running. Deriving it
    /// from the caller's `visible` instead would be cheaper still and worthless: the counter exists
    /// to be compared against `visible`.
    pub fn rows_in_range(&self, r: Range<u32>) -> Bitmap {
        self.debug_assert_in_domain(&r);
        let range_mask = Bitmap::from_range(r);
        let mut result = self.base.bitmap().and(&range_mask);
        result.andnot_inplace(&self.minus);
        let plus_in_range = self.plus.and(&range_mask);
        // `or_inplace` on already-disjoint-from-`result` content (plus ∩ base = ∅ by
        // construction — see the structural invariant asserted in `compose`) — no double count.
        result.or_inplace(&plus_in_range);
        // **Last, and by intersection only.** The filter narrows the composed result; applying it
        // earlier — to `base`, or before the `plus` union — would let a filtered row be reinstated
        // by the diff, which is the fold `filter-surface.md` §5.1 forbids.
        if let Some(filter) = &self.filter {
            result.and_inplace(filter.rows());
        }
        result
    }

    /// Whether one row is visible — the composed mask and the request's filter, for a single row.
    pub fn contains_row(&self, row: u32) -> bool {
        self.debug_assert_in_domain(&(row..row + 1));
        if self
            .filter
            .as_ref()
            .is_some_and(|f| !f.rows().contains(row))
        {
            return false;
        }
        if self.minus.contains(row) {
            return false;
        }
        self.base.bitmap().contains(row) || self.plus.contains(row)
    }

    /// Are the overlay diffs empty — i.e. is the composed mask exactly `base`?
    ///
    /// This is [`Self::for_each_visible_run`]'s route predicate, public so tests can assert which
    /// route a given mask exercises. The route choice is observable in timing (a diffs-empty mask
    /// skips the per-tile materialisation) but never in output — the C19-adjacent note in memo
    /// `2026-07-30-viewport-hot-path-and-bundle-size-review.md` §B9.
    /// **A filter counts as a diff.** The steady-state route walks `base` in place precisely
    /// because there is nothing to subtract from it; a filter is exactly something to subtract, so
    /// a filtered mask must never take that route. Omitting `filter` here served every visible row
    /// under a filter that had narrowed nothing — the whole narrowing bypassed by a fast path that
    /// predated it, with no error and a plausible-looking answer.
    ///
    /// This is the shape to watch for whenever a narrowing is added to this type: every route that
    /// asks "can I skip the diffs?" is asking "is `base` already the answer?", and a new field that
    /// makes it not the answer belongs in this predicate.
    pub fn diffs_are_empty(&self) -> bool {
        self.minus.is_empty() && self.plus.is_empty() && self.filter.is_none()
    }

    /// The three bitmaps composition produced, and whether a filter narrows them.
    ///
    /// Which route [`Self::for_each_visible_run`] takes, and what one run step costs, are
    /// properties of these — the container mix of `base` above all — and nothing else publishes
    /// them. For a measurement asking why one session's decode costs what it does.
    // Public for `tessera-bench`'s `identity_bands_probe`; not part of the engine's API.
    #[doc(hidden)]
    pub fn parts(&self) -> (&Bitmap, &Bitmap, &Bitmap, bool) {
        (
            self.base.bitmap(),
            &self.minus,
            &self.plus,
            self.filter.is_some(),
        )
    }

    /// Visit the visible rows of `r` as ascending, non-overlapping, half-open runs — selection's
    /// decode path, replacing per-value bitmap iteration with contiguous row ranges the caller can
    /// scan as slices.
    ///
    /// Two sources of runs, one consumer. Steady state (diffs empty — the norm between change
    /// events) walks `base` in place with a croaring cursor: no `from_range`, no AND, no
    /// temporaries. Any non-empty diff takes the [`Self::rows_in_range`] fallback whole and yields
    /// the temporary's runs the same way — necessarily, not merely simply: a cursor over `base`
    /// can never see a `plus` row (`plus ∩ base = ∅` by construction), so there is no partial
    /// fast route to salvage. The union of the yielded runs equals `rows_in_range(r)` exactly,
    /// whichever source ran.
    pub fn for_each_visible_run(&self, r: Range<u32>, mut f: impl FnMut(Range<u32>)) {
        if self.diffs_are_empty() {
            for_each_run_in(self.base.bitmap(), r, &mut f);
        } else {
            let composed = self.rows_in_range(r.clone());
            for_each_run_in(&composed, r, &mut f);
        }
    }

    /// Visit the visible rows of `r` one value at a time, ascending — the run-length-indifferent
    /// sibling of [`Self::for_each_visible_run`], for masks too scattered for run decoding to
    /// amortise (the tier gate and its measured basis live at
    /// `select.rs::RUN_DECODE_MIN_DENSITY_PCT`).
    ///
    /// This is the retired materialise-then-`.iter()` mechanism minus the materialisation: on the
    /// steady-state route the cursor walks `base` in place, so the per-tile temporaries are gone
    /// while the per-value cost is unchanged (measured marginally *better* —
    /// `examples/decode_tiers.rs`). A `next_many` batch decoder was measured for this role and
    /// rejected: the buffer round-trip lost to croaring's plain iterator at **every** density
    /// tried — see the example's table before reintroducing one. Route structure is the same as
    /// the run form: any non-empty diff falls back to the materialised composed bitmap.
    /// The bitmap a caller-driven decode of `r` should walk — the route choice as a value, for
    /// callers whose hot loop cannot afford a closure boundary.
    ///
    /// [`Self::for_each_visible_run`] hands runs to a closure, which is fine per run — the call
    /// amortises over the run's rows. A **per-value** decode is not so forgiving: the measured
    /// cost of routing the scan state through a closure environment was ~2× the whole loop, so
    /// the value tier's loop lives with its state in `select.rs` and only the route decision
    /// lives here. `examples/decode_tiers.rs`'s mask-batch column pins the shipped shape against
    /// the local reference loop so the overhead cannot creep back unnoticed.
    ///
    /// The contract mirrors the closure forms exactly: walking the returned bitmap clamped to
    /// `r` — seek to `r.start`, stop at the first row `≥ r.end` — yields `rows_in_range(r)`,
    /// whichever variant came back. `Base` is the whole projection, NOT clamped to `r` (the
    /// caller's stop condition does the clamping); `Composed` is already `rows_in_range(r)` and
    /// the same walk is simply exhaustive on it.
    pub fn decode_source(&self, r: Range<u32>) -> DecodeSource<'_> {
        if self.diffs_are_empty() {
            DecodeSource::Base(self.base.bitmap())
        } else {
            DecodeSource::Composed(self.rows_in_range(r))
        }
    }

    /// The two structural invariants: `minus ⊆ base` and `plus ∩ base = ∅`.
    /// Exposed for tests; also asserted in debug builds at construction time in [`compose`].
    pub fn check_structural_invariants(&self) -> bool {
        self.minus.is_subset(self.base.bitmap()) && self.plus.and(self.base.bitmap()).is_empty()
    }
}

/// How many runs one cursor read decodes: 64 × 8 B is a 512 B stack buffer, no heap allocation
/// per call, and one FFI crossing per 64 runs.
const RUN_BUF_LEN: usize = 64;

/// Walk `bitmap ∩ r` as ascending, non-overlapping, half-open runs.
///
/// The delicate step is the closed→half-open conversion: croaring yields `{start, last}` with
/// `last` *inclusive*, and `last + 1` at `last == u32::MAX` is a debug panic and a release wrap
/// to 0 — so the end-clamp test runs first, and clamping to `r.end` covers the `u32::MAX` case
/// for free (`r.end - 1 ≤ u32::MAX - 1 < last` whenever `last == u32::MAX`, since `r.end` is
/// exclusive). The row `u32::MAX` itself is therefore never emitted — exactly as
/// [`Bitmap::from_range`] excludes it, so the two decode routes agree at the top of the row
/// space. No start-clamp exists on purpose: after `reset_at_or_after(r.start)` the first run
/// already begins at the first set value ≥ `r.start`.
fn for_each_run_in(bitmap: &Bitmap, r: Range<u32>, f: &mut impl FnMut(Range<u32>)) {
    if r.start >= r.end {
        return;
    }
    let mut cursor = bitmap.cursor();
    cursor.reset_at_or_after(r.start);
    let mut buf = [croaring::RangeInclusive::<u32> { start: 0, last: 0 }; RUN_BUF_LEN];
    loop {
        let n = cursor.read_many_ranges(&mut buf);
        if n == 0 {
            // Exhausted — `read_many_ranges` returning 0 is the termination signal.
            return;
        }
        for run in &buf[..n] {
            if run.start >= r.end {
                return;
            }
            if run.last >= r.end - 1 {
                f(run.start..r.end);
                return;
            }
            f(run.start..run.last + 1);
        }
    }
}

/// See [`EffectiveMask::decode_source`]. Which variant a caller received is the diffs-empty
/// route choice — observable in timing, never in output, same C19-adjacent note as the closure
/// decoders.
pub enum DecodeSource<'a> {
    /// The whole projection, borrowed — the steady-state (diffs-empty) route. Not clamped to the
    /// requested range; the caller's walk must stop at its `r.end`.
    Base(&'a Bitmap),
    /// The materialised `rows_in_range(r)` — the diffs-present fallback. Already exactly the
    /// visible set of `r`.
    Composed(Bitmap),
}

impl DecodeSource<'_> {
    /// The bitmap to walk, whichever route produced it.
    #[inline]
    pub fn bitmap(&self) -> &Bitmap {
        match self {
            DecodeSource::Base(b) => b,
            DecodeSource::Composed(b) => b,
        }
    }
}

/// The per-entity verdict, `deleted > suppressed > buffered`, or `None` when
/// the overlay and the buffer have no opinion and the frozen fragment already carries the
/// answer. **The single source of this precedence** — [`compose`] turns it into row-space diffs
/// for range arithmetic, [`visible_to`] reads it directly for a single entity. Two transcriptions
/// of a precedence rule is how a suppression stops suppressing (lifecycle §3, caught twice in
/// review) — do not re-derive this logic anywhere else.
///
/// **There is no neutral entry any more.** Under a single map, `suppress → unsuppress` left an entry
/// whose every fact was inactive, and its bare presence outranked the ingest buffer here. An
/// unsuppress now removes the id from the suppression bitmap, so a still-buffered entity falls
/// through to the buffer rule and its own terms decide — which is what lifecycle §3.1 always said
/// ("unsuppress removes the entry") and what the previous representation did not do.
pub(crate) fn verdict(
    overlay: &Overlay,
    buffer: &IngestBuffer,
    satisfied: &FxHashSet<TermId>,
    entity: EntityId,
) -> Option<bool> {
    if overlay.is_deleted(entity) || overlay.is_suppressed(entity) {
        return Some(false);
    }

    // Buffered entities the overlay has no opinion on (anything it does have an opinion on
    // was resolved above).
    //
    // **No watermark gate.** There used to be one, `entity < watermark → None`, guarding against a
    // buffer that still held entities the fragment already accounts for. What made that reachable
    // was replay re-buffering every retained WAL row; `WritePath::reconstruct` now drops any row
    // whose entity already has geometry, so the buffer holds exactly the rows without it and a
    // hit here cannot be an entity the fragment covers. The gate was also only ever *exact* while
    // entity-allocation order and flush order coincided — one view per partition — so removing it
    // takes a silent multi-view hazard out with it.
    buffer
        .get(entity)
        .map(|item| item.terms.iter().any(|t| satisfied.contains(t)))
}

/// Derive the row-space deny mask from the authoritative entity-space stores.
///
/// **`{row_of(e) : e ∈ deleted ∪ suppressed}`, per view, and nothing else.** The union is taken
/// from [`Overlay::denied`] rather than assembled here, so the rule below has one expression.
///
/// **The derivation rule, stated where it is derived.** This function's result is the *only* legal
/// value of [`Generation::denied`]. Two update modes are licensed:
///
/// - **Additions may be incremental.** A window of `Delete`/`Suppress` only grows the union, so
///   adding `row_of(e)` per entry is provably equal to re-deriving.
/// - **Any removal re-derives.** A window containing an `Unsuppress` rebuilds from the union.
///   Subtracting `row_of(e)` on unsuppress **is wrong**: after `delete → suppress → unsuppress` the
///   row must stay masked, because `deleted` still holds the entity. That is the one way this mask
///   could silently re-expose a deleted item, and re-derivation makes it unmistakable. An
///   implementation wanting the incremental subtraction must prove `e ∉ deleted` at the site.
///
/// Every geometry publication re-derives too, row ids being meaningful only within one
/// `segments_version`. `Executor::publish` asserts this equality in debug builds, so a build site
/// that breaks the rule fails the suite rather than a viewer's map.
///
/// An entity with no row — still buffered, or belonging to another view — contributes nothing:
/// the mask is complete for what it governs, which is row-space questions, and `verdict` answers
/// the entity-space ones.
pub(crate) fn derive_denied(overlay: &Overlay, bundle: &Bundle) -> DenyMask {
    let mut out = DenyMask::default();
    for partition in bundle.partitions.values() {
        for (view, view_data) in &partition.views {
            // Every view gets an entry, empty or not: a missing one must mean "the mask and the
            // bundle disagree", never "nothing is denied here".
            out.insert(view.clone(), denied_rows_of(overlay, &view_data.row_space));
        }
    }
    out
}

/// One view's deny mask — see [`derive_denied`], whose per-view body this is.
pub fn denied_rows_of(overlay: &Overlay, row_space: &RowSpace) -> Bitmap {
    let mut rows = Bitmap::new();
    for entity in overlay.denied().iter() {
        if let Some(row) = row_space.row_of(EntityId::new(entity as u64)) {
            rows.add(row.raw());
        }
    }
    rows
}

/// Compose the effective mask for one request. See this module's doc for the precedence rule and
/// the clamp rationale.
///
/// **The frozen fragment is not a parameter**: `base` is already its row-space projection, and an
/// entity with no verdict falls through to `base` by construction. It used to be taken for its
/// `watermark` alone, to gate the buffer rule; that gate is gone (see [`verdict`]), and with it the last
/// reason for this function to see the fragment at all.
///
/// `satisfied` is the viewer's granted term set (already resolved to `TermId`s by the auth plugin
/// path).
/// `base` is the cached row-space projection (see [`RowProjection`]'s doc); `row_space` is used
/// only for per-entity `row_of` lookups (O(log k), not the O(bound) `project` cost).
pub fn compose(
    satisfied: &FxHashSet<TermId>,
    overlay: &Overlay,
    buffer: &IngestBuffer,
    base: Arc<RowProjection>,
    row_space: &RowSpace,
    denied: &Bitmap,
) -> EffectiveMask {
    let mut fail_rows: Vec<u32> = Vec::new();
    let mut pass_rows: Vec<u32> = Vec::new();

    // **The buffer is the whole of the walk.** Deletions and suppressions are not walked here: they
    // are `denied`, folded in below as one `andnot`, which is what stops per-request work growing
    // with denies ever accepted. A deny cannot lose an ordering argument it never enters. With the
    // evaluate store gone (decision 0048), nothing else in the overlay contributes a row-space diff
    // at all, so `L` is now `keys(buffer)` alone.
    //
    // `verdict` is still the single expression of the precedence and is called unchanged, so the
    // entity-space verbs and this walk cannot drift apart.
    //
    // Buffered entities with no overlay entry: an overlay entry is a deny, which the fold below
    // covers, so skipping it here loses nothing.
    for (&entity, _) in buffer.iter() {
        if overlay.touches(entity) {
            continue;
        }
        if let Some(pass) = verdict(overlay, buffer, satisfied, entity) {
            if let Some(row) = row_space.row_of(entity) {
                if pass {
                    pass_rows.push(row.raw());
                } else {
                    fail_rows.push(row.raw());
                }
            }
            // A buffered entity has no row by definition — that is what being buffered means — so
            // this always takes the "no row" path above. The branch is kept and tested against a
            // synthetic permutation, for the day buffered items get provisional rows.
        }
    }

    fail_rows.sort_unstable();
    pass_rows.sort_unstable();
    let fail_bitmap = Bitmap::of(&fail_rows);
    let pass_bitmap = Bitmap::of(&pass_rows);

    let base_bitmap = base.bitmap();
    // **The deny mask is folded in last, unconditionally.** `minus` gains every denied row that
    // `base` carries and `plus` loses every denied row it proposed, so a denied entity is masked
    // whatever any other rule concluded about it.
    //
    // `andnot` is self-clamping, which is the second half of why this is worth doing: the deny
    // half can no longer get the `∩ base` / `∖ base` clamps wrong, and the spurious-`−1` hazard
    // this module's doc describes cannot arise for denies at all (I2).
    let minus = fail_bitmap.and(base_bitmap).or(&denied.and(base_bitmap));
    let plus = pass_bitmap.andnot(base_bitmap).andnot(denied);

    debug_assert!(
        minus.is_subset(base_bitmap),
        "compose: minus ⊄ base — the ∩ base clamp is supposed to make this impossible"
    );
    debug_assert!(
        plus.and(base_bitmap).is_empty(),
        "compose: plus ∩ base ≠ ∅ — the ∖ base clamp is supposed to make this impossible"
    );

    EffectiveMask {
        base,
        minus,
        plus,
        // Composition never filters. A request that carries operands narrows the result afterwards
        // via `with_filter`, which is what keeps this function's structural invariants — and the
        // unfiltered total θ anchors on — properties of composition alone.
        filter: None,
        highlight: None,
        whole: std::sync::OnceLock::new(),
    }
}

/// `entity`'s raw id, cast down to the `u32` the fragment's bitmap operates over. Infallible in
/// practice — entities are capped at `u32::MAX` by the I9 allocator (see
/// `tessera_types::IdentityKey::forward`'s identical bound) — but checked rather than a silent
/// truncating cast, so a violated invariant fails loudly instead of testing the wrong entity.
fn entity_as_u32(entity: EntityId) -> u32 {
    u32::try_from(entity.raw())
        .expect("entity ids are capped at u32::MAX by the I9 allocator (contracts §2.6 r6)")
}

/// Is `entity` visible to this session — the ONE BIT `/v1/items` needs.
///
/// This is an **entity-space** question and is answered in entity space: the overlay and buffer
/// are hash probes, `fragment.contains` is an O(1) Roaring probe on a borrowed mmap view, and no
/// `RowProjection` is constructed or consulted. It is therefore **identical work for an entity
/// that does not exist, one that exists and is invisible, and one that exists and is visible** —
/// which is what closes the `/v1/items` timing channel outright rather than narrowing it (design
/// Appendix C, C4 annotation).
///
/// Equivalent to `compose(...).contains_row(perm.row_of(entity))` wherever a row exists — see
/// this module's clamp doc: a `false` verdict lands in `minus` or outside `base` and is false
/// either way, a `true` verdict lands in `base` or `plus` and is true either way, and no verdict
/// falls through to `base`, which is `project`'s image of the fragment. The row-space form exists
/// for *range cardinalities*; a single-entity test does not need it.
pub fn visible_to(
    fragment: &FrozenFragment,
    satisfied: &FxHashSet<TermId>,
    overlay: &Overlay,
    buffer: &IngestBuffer,
    entity: EntityId,
) -> bool {
    verdict(overlay, buffer, satisfied, entity)
        .unwrap_or_else(|| fragment.view().contains(entity_as_u32(entity)))
}

#[cfg(test)]
mod tests {
    //! Unit tests for [`for_each_run_in`] — the closed→half-open conversion and its edges. Tested
    //! here, at the helper, because the one genuinely dangerous input (a run ending at
    //! `u32::MAX`) cannot be built through the integration fixtures without a 2³²-entry
    //! permutation. Mask-level equivalence (both routes against `rows_in_range`) lives in
    //! `tests/compose.rs` and `tests/selection.rs`.

    use super::*;

    fn runs_of(bitmap: &Bitmap, r: Range<u32>) -> Vec<Range<u32>> {
        let mut out = Vec::new();
        for_each_run_in(bitmap, r, &mut |run| out.push(run));
        out
    }

    /// Every emitted run flattens back to exactly `bitmap ∩ r`, half-open — the property every
    /// other test here is a named corner of.
    fn assert_flattens_to_intersection(bitmap: &Bitmap, r: Range<u32>) {
        let flat: Vec<u32> = runs_of(bitmap, r.clone()).into_iter().flatten().collect();
        let expected: Vec<u32> = bitmap.and(&Bitmap::from_range(r.clone())).to_vec();
        assert_eq!(flat, expected, "runs disagree with bitmap ∩ {r:?}");
    }

    #[test]
    fn a_run_ending_at_u32_max_is_clamped_not_overflowed() {
        // The R4 edge: `last + 1` at `last == u32::MAX` would panic in debug and wrap in release.
        let mut b = Bitmap::new();
        b.add_range(u32::MAX - 100..=u32::MAX);
        let r = (u32::MAX - 50)..u32::MAX;
        assert_eq!(runs_of(&b, r.clone()), vec![(u32::MAX - 50)..u32::MAX]);
        // Parity with `Bitmap::from_range`, which also cannot express u32::MAX: neither route
        // ever emits it.
        assert_flattens_to_intersection(&b, r);
    }

    #[test]
    fn a_run_spanning_the_container_boundary_comes_back_merged() {
        // croaring merges runs across the 65535/65536 container boundary — one run, not two.
        let mut b = Bitmap::new();
        b.add_range(65_530..=65_540);
        assert_eq!(runs_of(&b, 0..100_000), vec![65_530..65_541]);
        assert_flattens_to_intersection(&b, 0..100_000);
    }

    #[test]
    fn runs_touching_the_range_ends_are_clamped_to_it() {
        let mut b = Bitmap::new();
        b.add_range(0..=9);
        b.add(15);
        b.add_range(20..=29);
        // The first run starts before r (the cursor seek supplies the start, no clamp code), the
        // last extends past r.end (the end-clamp cuts it).
        assert_eq!(runs_of(&b, 5..25), vec![5..10, 15..16, 20..25]);
        assert_flattens_to_intersection(&b, 5..25);
    }

    #[test]
    // The inverted range below is deliberate: it is the guard's own input, not an iteration.
    #[allow(clippy::reversed_empty_ranges)]
    fn empty_mask_empty_range_and_no_overlap_all_yield_nothing() {
        let empty = Bitmap::new();
        assert!(runs_of(&empty, 0..1000).is_empty());

        let mut b = Bitmap::new();
        b.add_range(100..=200);
        assert!(runs_of(&b, 50..50).is_empty(), "empty range");
        assert!(runs_of(&b, 60..40).is_empty(), "inverted range");
        assert!(
            runs_of(&b, 0..100).is_empty(),
            "range wholly before the run"
        );
        assert!(
            runs_of(&b, 201..300).is_empty(),
            "range wholly after the run"
        );
    }

    #[test]
    fn more_runs_than_one_buffer_read_are_all_emitted() {
        // 200 single-value runs forces multiple `read_many_ranges` refills (RUN_BUF_LEN = 64).
        let mut b = Bitmap::new();
        for i in 0..200u32 {
            b.add(i * 3);
        }
        let runs = runs_of(&b, 0..600);
        assert_eq!(runs.len(), 200);
        assert_flattens_to_intersection(&b, 0..600);
    }

    #[test]
    fn run_walk_agrees_with_the_bitmap_route_on_random_container_mixes() {
        use rand::rngs::StdRng;
        use rand::{Rng, SeedableRng};
        let mut rng = StdRng::seed_from_u64(0xB9_0B9);
        for _ in 0..50 {
            let mut b = Bitmap::new();
            // A mix that lands array, bitmap and run containers: sparse randoms, a dense random
            // stretch, and a long literal run.
            for _ in 0..rng.gen_range(0..200) {
                b.add(rng.gen_range(0..300_000));
            }
            let dense_start = rng.gen_range(0..200_000);
            for v in dense_start..dense_start + 40_000 {
                if rng.gen_bool(0.5) {
                    b.add(v);
                }
            }
            let run_start = rng.gen_range(0..250_000);
            b.add_range(run_start..=run_start + rng.gen_range(0..30_000));

            for _ in 0..20 {
                let a = rng.gen_range(0..320_000);
                let z = rng.gen_range(0..320_000);
                assert_flattens_to_intersection(&b, a.min(z)..a.max(z));
            }
        }
    }
}
