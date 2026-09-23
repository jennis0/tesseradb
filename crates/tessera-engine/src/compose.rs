//! Composes the effective visibility mask for one request: `M_auth = (fragment \ L) ∪
//! direct_eval(L)`, evaluated as diffs against a cached row-space projection of the frozen
//! fragment, not by recomputing the whole mask.
//!
//! [`compose`] walks the generation's per-view list of buffered entities that have a row
//! ([`derive_buffered_rows`]), resolving each with the precedence
//! `deleted > suppressed > buffered` ([`verdict`]), into two row-space bitmaps against `base`:
//! `minus = {row(e) : e fails} ∩ base`, `plus = {row(e) : e passes} ∖ base`. Without the
//! `∩ base` / `∖ base` clamps, denying an entity the fragment never contained would drive a
//! tile's count negative, and passing an entity already in it would count the tile twice.
//! Deletions and suppressions are not in that walk: they arrive as `denied`, the row-space image
//! of `deleted ∪ suppressed` ([`derive_denied`]), folded in as `minus ∪= denied ∩ base`,
//! `plus ∖= denied` — `andnot` clamps itself, so a deny cannot violate the two clamps above, and
//! this keeps per-request work independent of how many denies have ever been accepted.
//! [`verdict`] is the one expression of the precedence, read here and by the entity-space verbs
//! ([`visible_to`], label gating, cluster visibility) alike.

use std::ops::Range;
use std::sync::Arc;

use croaring::Bitmap;
use rustc_hash::FxHashSet;

use tessera_authz::FrozenFragment;
use tessera_lifecycle::{BufferedItem, IngestBuffer, Overlay};
use tessera_store::{Bundle, RowSpace};
use tessera_types::{EntityId, TermId};

use crate::projection::RowProjection;
use crate::DenyMask;

/// An attribute filter's matching rows, with the part of row space they answer for: the per-tile
/// route (`Viewport`) is silent, not negative, outside the tiles it tested, so a counting path
/// handed it as if it were `Complete` would undercount `matched` with no error.
pub enum FilterRows {
    /// Every matching row in the view. Exact at any range.
    Complete(Bitmap),
    /// Only the rows inside `domain` were tested; outside it the bitmap is empty and that
    /// emptiness means nothing.
    Viewport {
        rows: Bitmap,
        /// Ascending, disjoint and maximally merged — [`Self::covers`] binary-searches it.
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

    /// Is `r` a range this set can answer about? Debug-only: `domain` is the request's own tile
    /// set, not a permission boundary, so a range outside it is a coding error in this crate.
    fn covers(&self, r: &Range<u32>) -> bool {
        let domain = match self {
            FilterRows::Complete(_) => return true,
            FilterRows::Viewport { domain, .. } => domain,
        };
        if r.start >= r.end {
            return true;
        }
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
/// `plus`) capturing every overlay/buffer change since `base` was cached. An artifact's
/// membership question costs O(containers touched) in the diffs and materialises nothing.
pub struct EffectiveMask {
    base: Arc<RowProjection>,
    minus: Bitmap,
    plus: Bitmap,
    /// The rows an attribute filter admits, or `None` when the request carried no filter. Never
    /// folded into `base`: that would let a filter narrow the unfiltered total θ anchors on,
    /// coarsening visibility instead of only refining it.
    filter: Option<FilterRows>,
    /// The rows the request's highlight admits, or `None` where it carried none. Never folded
    /// into `filter`: [`Self::rows_in_range`] never reads this, so the served set is the same
    /// with or without a highlight.
    highlight: Option<FilterRows>,
    /// `(base − minus) ∪ plus` materialised, filled by the first [`WholeMask::visible_all`] that
    /// needs it and left empty on a mask that denies nothing.
    whole: std::sync::OnceLock<Bitmap>,
}

/// The two questions an artifact's membership asks of a viewer's mask. A trait so the one
/// production implementor is [`EffectiveMask`]: an artifact's masked count must be taken against
/// the composed mask, strictly smaller than the pre-overlay projection after an accepted delete
/// or suppression. A predicate that could take a raw bitmap would serve counts over items
/// already denied. The `Bitmap` implementor below is test-only, so its absence from a release
/// build makes that a compile error.
pub trait MaskedSet {
    /// `|set ∩ mask|` — an artifact's masked count, O(containers touched). Blind to any
    /// attribute filter: a filtered count here would make an artifact appear and disappear as a
    /// viewer typed, instead of showing what the principal may see regardless of their search box.
    fn count_intersection(&self, set: &Bitmap) -> u64;

    /// Whether `set` holds any visible row — candidacy, answered as a masked question.
    fn intersects_set(&self, set: &Bitmap) -> bool;

    /// `set ∩ mask`, materialised — the rows of an artifact's membership this viewer may see.
    /// The one input to derived content: a property computed over the whole membership and
    /// served to a viewer who may see only part of it describes documents they may not see, so
    /// a derived property must come from asking this and nothing else.
    fn visible_rows(&self, set: &Bitmap) -> Bitmap;

    /// `|[r.start, r.end) ∩ mask|` — the masked count of a contiguous row range, filter-blind
    /// for the same reason as [`MaskedSet::count_intersection`]. The default materialises and
    /// intersects; [`EffectiveMask`] overrides it with cheaper arithmetic.
    fn count_range(&self, r: Range<u32>) -> u64 {
        self.count_intersection(&Bitmap::from_range(r))
    }
}

/// The whole composed mask, materialised — every row this viewer may see, in this view's row
/// space. Two callers: the row-major count ([`crate::row_column::RowColumn::histogram`]), which
/// has no per-artifact membership and so walks the mask to read off which artifact each visible
/// row belongs to; and [`crate::tile_index::Viewport::compose`], which borrows this set directly.
/// Its own trait rather than a method on [`MaskedSet`], because this asks nothing about any
/// artifact. The one production implementor is [`EffectiveMask`], so a whole-mask walk cannot be
/// taken against the pre-overlay projection, which strictly contains `M_auth` after any accepted
/// delete. The [`Bitmap`] implementor below is test-only.
pub trait WholeMask {
    /// See the trait's doc. Filter-blind, as [`MaskedSet::count_intersection`] is, so
    /// `visible_all().and_cardinality(set)` and `count_intersection(set)` are the same number,
    /// asserted by the test beside the implementation.
    ///
    /// Borrowed, and materialised at most once per mask: the set is the projection's own size,
    /// so a caller that copies it per request pays that copy per request.
    fn visible_all(&self) -> &Bitmap;

    /// `|M_auth|`, without materialising the set. [`EffectiveMask`] overrides the default with
    /// the arithmetic [`EffectiveMask::visible_total`] takes, the one θ anchors on.
    fn visible_count(&self) -> u64 {
        self.visible_all().cardinality()
    }
}

impl WholeMask for EffectiveMask {
    /// `(base − minus) ∪ plus`. `minus ⊆ base` and `plus ∩ base = ∅` hold structurally
    /// ([`compose`] asserts them), so the cardinality equals [`EffectiveMask::visible_total`]
    /// exactly. A mask that denies nothing returns `base` itself, copying nothing; otherwise it
    /// copies once, into [`Self::whole`], since `base` is shared with every other session
    /// holding the same projection.
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

    /// [`EffectiveMask::visible_total`] under the trait, so the anchor and this cannot disagree.
    fn visible_count(&self) -> u64 {
        EffectiveMask::visible_total(self)
    }
}

/// Test-only — see [`MaskedSet`].
#[cfg(test)]
impl WholeMask for Bitmap {
    fn visible_all(&self) -> &Bitmap {
        self
    }
}

impl MaskedSet for EffectiveMask {
    /// The same term-by-term arithmetic as [`EffectiveMask::count_range`]: exact because
    /// `minus ⊆ base` and `plus ∩ base = ∅` are the structural invariants [`compose`] asserts,
    /// so nothing is subtracted twice and nothing is added that was already there.
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
    /// then `plus ∩ set`. Equals [`Self::count_intersection`] exactly — asserted by the test
    /// beside it, because a derived property computed over a different set from the count
    /// served beside it is a disagreement a viewer would see and could not explain.
    fn visible_rows(&self, set: &Bitmap) -> Bitmap {
        let mut visible = self.base.bitmap().and(set);
        visible.andnot_inplace(&self.minus);
        visible.or_inplace(&self.plus.and(set));
        visible
    }

    /// [`EffectiveMask::count_range`] under the trait, so a range counted through the predicate
    /// and one counted directly cannot disagree.
    fn count_range(&self, r: Range<u32>) -> u64 {
        EffectiveMask::count_range(self, r)
    }
}

/// Test-only, so no release build can put an uncomposed set where a composed one belongs.
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
    /// Narrow this mask by an attribute filter's row-space set. Consumes and returns, so a
    /// filtered mask cannot be built by mutating one already handed to a counting path.
    pub fn with_filter(mut self, rows: FilterRows) -> Self {
        self.filter = Some(rows);
        self
    }

    /// Attach the request's highlight — see [`Self::highlight`].
    pub fn with_highlight(mut self, rows: FilterRows) -> Self {
        self.highlight = Some(rows);
        self
    }

    /// Whether this request carried a highlight, which decides whether the points frame has a
    /// `highlighted` column at all, and whether the artifacts frame's bit is `null`.
    pub fn has_highlight(&self) -> bool {
        self.highlight.is_some()
    }

    /// The rows in `r` visible, matching the filter and satisfying the highlight —
    /// `TileCount::highlighted`. Equal to [`Self::count_matched_range`] with no highlight, so
    /// the column is always present on the wire. `highlighted ≤ matched ≤ visible` always holds.
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

    /// Whether one served row satisfies the highlight — the points frame's bit. `false` with no
    /// highlight; no caller reads it then.
    pub fn is_highlighted(&self, row: u32) -> bool {
        self.highlight
            .as_ref()
            .is_some_and(|highlight| highlight.rows().contains(row))
    }

    /// The rows of `here` satisfying both the filter and the highlight — the artifacts frame's
    /// `highlighted` bit — or `None` with no highlight. `here` must come from
    /// [`MaskedSet::visible_rows`].
    pub fn highlighted_rows(&self, here: &Bitmap) -> Option<Bitmap> {
        let highlight = self.highlight.as_ref()?;
        Some(match self.matched_rows(here) {
            Some(matched) => matched.and(highlight.rows()),
            None => here.and(highlight.rows()),
        })
    }

    /// `base.range_cardinality(r) − |minus ∩ r| + |plus ∩ r|`.
    pub fn count_range(&self, r: Range<u32>) -> u64 {
        let base_count = self.base.range_cardinality(r.clone());
        let minus_count = self.minus.range_cardinality(r.clone());
        let plus_count = self.plus.range_cardinality(r);
        base_count - minus_count + plus_count
    }

    /// The total number of visible rows in this mask, over the whole row space — the quantity
    /// θ's anchor is derived from ([`crate::select::Threshold::at_depth`]; [`crate::occupancy`]
    /// takes the same rule for `N_occ(d)`). The composed figure, not the projection's: `base` is
    /// `M_auth` before the overlay diff, and strictly contains it after an accepted delete or
    /// suppression. Anchoring on `base.cardinality()` alone would let a viewer difference it
    /// against its own summed per-tile `visible` and recover how many of its own items have
    /// been denied. The same arithmetic as [`Self::count_range`], so the anchor and the
    /// per-tile counts cannot disagree. Blind to `filter`: anchoring on a filtered count would
    /// make θ move as a viewer typed, coarsening the visible frontier rather than only refining
    /// it.
    pub fn visible_total(&self) -> u64 {
        self.base.cardinality() - self.minus.cardinality() + self.plus.cardinality()
    }

    /// The rows in `r` that are visible and match the request's filter — `TileCount::matched`,
    /// distinct from [`Self::count_range`]'s composed visible count: `visible` is how many
    /// items in this tile the principal may see, `matched` how many the filter admits. Without a
    /// filter the two agree, and this returns `count_range` rather than materialising a bitmap.
    pub fn count_matched_range(&self, r: Range<u32>) -> u64 {
        match &self.filter {
            None => self.count_range(r),
            // With a filter present the term-by-term arithmetic does not hold — a row can be in
            // `base` and out of the filter — so this materialises the intersection instead.
            // O(containers in the range), not O(rows).
            Some(_) => self.rows_in_range(r).cardinality(),
        }
    }

    /// The rows of `here` this request's filter admits — `here ∩ M_sel` — or `None` with no
    /// filter. Not on [`MaskedSet`], because that trait stays filter-blind and a filter reaching
    /// the masked count or the existence criterion would make an artifact appear and vanish as a
    /// viewer typed. `here` must be `viewport ∩ M_auth`, from [`MaskedSet::visible_rows`], so
    /// this only narrows an already-composed set, and scoped to the viewport: the per-tile
    /// crossing is silent, not negative, outside the request's tiles ([`FilterRows`]).
    pub fn matched_rows(&self, here: &Bitmap) -> Option<Bitmap> {
        self.filter.as_ref().map(|filter| here.and(filter.rows()))
    }

    /// Every range this mask is asked about must be one the filter was evaluated over — see
    /// [`FilterRows`]. Compiled out in release.
    #[inline]
    fn debug_assert_in_domain(&self, r: &Range<u32>) {
        debug_assert!(
            self.filter.as_ref().is_none_or(|f| f.covers(r)),
            "filtered count over {r:?}, which the per-tile crossing never tested — the answer \
             would be silently low. Either the range came from outside the request's tile set or \
             the domain was built from something other than `ranges`."
        );
    }

    /// The effective mask restricted to `r`: `(base ∩ r) ∖ minus ∪ (plus ∩ r)`, as a bitmap, not
    /// an iterator: an eagerly materialised `Vec<u32>` of every visible row in a large viewport
    /// can run to tens of megabytes per request, against a cost model of O(containers touched),
    /// not O(cardinality). Callers iterate the bitmap lazily instead: per value with `.iter()`,
    /// per run via [`Self::for_each_visible_run`], or via [`Self::decode_source`].
    pub fn rows_in_range(&self, r: Range<u32>) -> Bitmap {
        self.debug_assert_in_domain(&r);
        let range_mask = Bitmap::from_range(r);
        let mut result = self.base.bitmap().and(&range_mask);
        result.andnot_inplace(&self.minus);
        let plus_in_range = self.plus.and(&range_mask);
        // `plus ∩ base = ∅` (the structural invariant `compose` asserts), so this adds nothing
        // already in `result` and cannot double-count.
        result.or_inplace(&plus_in_range);
        // The filter is applied last, by intersection only: applying it earlier — to `base`, or
        // before the `plus` union — would let a diff reinstate a row the filter had excluded.
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

    /// Are the overlay diffs empty and there is no filter — i.e. is the composed mask exactly
    /// `base`? A filter counts as a diff: the steady-state route walks `base` in place because
    /// there is nothing to subtract, but a filter is something to subtract, so a filtered mask
    /// must never take that route — it would serve every visible row under a filter that had
    /// narrowed nothing.
    pub fn diffs_are_empty(&self) -> bool {
        self.minus.is_empty() && self.plus.is_empty() && self.filter.is_none()
    }

    /// The three bitmaps composition produced, and whether a filter narrows them.
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
    /// decode path, contiguous ranges the caller scans as slices instead of per-value iteration.
    /// Diffs empty walks `base` in place with a croaring cursor; any non-empty diff takes the
    /// [`Self::rows_in_range`] fallback whole, since a cursor over `base` can never see a `plus`
    /// row (`plus ∩ base = ∅`). The union of the yielded runs equals `rows_in_range(r)` exactly.
    pub fn for_each_visible_run(&self, r: Range<u32>, mut f: impl FnMut(Range<u32>)) {
        if self.diffs_are_empty() {
            for_each_run_in(self.base.bitmap(), r, &mut f);
        } else {
            let composed = self.rows_in_range(r.clone());
            for_each_run_in(&composed, r, &mut f);
        }
    }

    /// The bitmap a caller-driven, per-value decode of `r` should walk, for callers whose hot
    /// loop cannot afford a closure boundary (the tier gate lives at
    /// `select.rs::RUN_DECODE_MIN_DENSITY_PCT`). Same route structure as
    /// [`Self::for_each_visible_run`]. Walking the result clamped to `r` yields
    /// `rows_in_range(r)`: `Base` is the whole projection, not clamped, so the caller's stop
    /// condition does the clamping; `Composed` is already `rows_in_range(r)`.
    pub fn decode_source(&self, r: Range<u32>) -> DecodeSource<'_> {
        if self.diffs_are_empty() {
            DecodeSource::Base(self.base.bitmap())
        } else {
            DecodeSource::Composed(self.rows_in_range(r))
        }
    }

    /// The two structural invariants: `minus ⊆ base` and `plus ∩ base = ∅`.
    pub fn check_structural_invariants(&self) -> bool {
        self.minus.is_subset(self.base.bitmap()) && self.plus.and(self.base.bitmap()).is_empty()
    }
}

/// How many runs one cursor read decodes: 64 × 8 B is a 512 B stack buffer, no heap allocation
/// per call, and one FFI crossing per 64 runs.
const RUN_BUF_LEN: usize = 64;

/// Walk `bitmap ∩ r` as ascending, non-overlapping, half-open runs. croaring yields
/// `{start, last}` with `last` inclusive, and `last + 1` at `last == u32::MAX` is a debug panic
/// and a release wrap to 0, so the end-clamp test runs first: clamping to `r.end` covers that
/// case too, since `r.end` is exclusive and so never reaches `u32::MAX`, matching
/// [`Bitmap::from_range`]. No start-clamp is needed: `reset_at_or_after(r.start)` already begins
/// the first run at the first set value ≥ `r.start`.
pub(crate) fn for_each_run_in(bitmap: &Bitmap, r: Range<u32>, f: &mut impl FnMut(Range<u32>)) {
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
/// route choice, observable in timing, not in output.
pub enum DecodeSource<'a> {
    /// The whole projection, borrowed. Not clamped to the requested range; the caller's walk must
    /// stop at `r.end`.
    Base(&'a Bitmap),
    /// The materialised `rows_in_range(r)` — already exactly the visible set of `r`.
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

/// The per-entity verdict, `deleted > suppressed > buffered`, or `None` when the overlay and the
/// buffer have no opinion and the frozen fragment already carries the answer. The one expression
/// of this precedence — [`compose`] turns it into row-space diffs, [`visible_to`] reads it
/// directly — so do not re-derive this logic anywhere else; a second transcription is how a
/// suppression could stop suppressing without the code that lifts it ever running. An
/// unsuppress removes the id from the suppression bitmap, so a still-buffered entity falls
/// through to the buffer rule and its own terms decide.
pub(crate) fn verdict(
    overlay: &Overlay,
    buffer: &IngestBuffer,
    satisfied: &FxHashSet<TermId>,
    entity: EntityId,
) -> Option<bool> {
    verdict_of(overlay, satisfied, entity, buffer.get(entity))
}

/// [`verdict`] for a caller walking the buffer, which already holds the entity's item. `item` must
/// be [`IngestBuffer::get`]'s answer for `entity`; [`IngestBuffer::iter`] yields exactly that,
/// both taking the first non-join row of the entity's list.
pub(crate) fn verdict_of(
    overlay: &Overlay,
    satisfied: &FxHashSet<TermId>,
    entity: EntityId,
    item: Option<&BufferedItem>,
) -> Option<bool> {
    if overlay.is_deleted(entity) || overlay.is_suppressed(entity) {
        return Some(false);
    }

    // An entity's postings are written by the flush of its own row, and the buffer holds that row
    // only until then (replay drops a row its view already holds), so an item here is never an
    // entity the fragment covers.
    item.map(|item| item.terms.iter().any(|t| satisfied.contains(t)))
}

/// Derive the row-space deny mask from the authoritative entity-space stores:
/// `{row_of(e) : e ∈ deleted ∪ suppressed}`, per view, taken from [`Overlay::denied`] so the
/// union is expressed once; this function's result is the only legal value of
/// [`Generation::denied`]. Additions may be applied incrementally, since a window of deletes and
/// suppressions only grows the union; any removal must re-derive from scratch, because
/// subtracting a row on unsuppress is wrong — after delete → suppress → unsuppress the row must
/// stay masked, `deleted` still holding the entity, and re-deriving is what stops that case
/// re-exposing a deleted item. Every geometry publication re-derives too, row ids being
/// meaningful only within one `segments_version`. An entity with no row — still buffered, or
/// belonging to another view — contributes nothing: the mask governs row-space questions,
/// `verdict` the entity-space ones.
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

/// One view's deny mask — see [`derive_denied`].
pub fn denied_rows_of(overlay: &Overlay, row_space: &RowSpace) -> Bitmap {
    let mut rows = Bitmap::new();
    for entity in overlay.denied().iter() {
        if let Some(row) = row_space.row_of(EntityId::new(entity as u64)) {
            rows.add(row.raw());
        }
    }
    rows
}

/// Derive, per view, the buffered entities [`compose`]'s walk has anything to say about: those
/// holding an own row in the buffer that already have a row in that view. An entity gains one only
/// when a publication gives it one, so this is re-derived by every publication and may otherwise be
/// carried; the result is the only legal value of [`Generation::buffered_rows`]. The overlay is not
/// consulted, so a deny or its lift changes nothing here and `compose` asks [`Overlay::touches`]
/// for itself.
pub(crate) fn derive_buffered_rows(buffer: &IngestBuffer, bundle: &Bundle) -> crate::BufferedRows {
    let mut out = crate::BufferedRows::default();
    for partition in bundle.partitions.values() {
        for (view, view_data) in &partition.views {
            // Every view gets an entry, empty or not, on [`derive_denied`]'s rule.
            out.insert(view.clone(), buffered_rows_of(buffer, &view_data.row_space));
        }
    }
    out
}

/// One view's list — see [`derive_buffered_rows`]. Sorted, so two derivations of one state are the
/// same vector whatever order the buffer's map was walked in.
pub fn buffered_rows_of(buffer: &IngestBuffer, row_space: &RowSpace) -> Vec<EntityId> {
    let mut entities: Vec<EntityId> = buffer
        .iter()
        .map(|(entity, _)| *entity)
        .filter(|entity| row_space.row_of(*entity).is_some())
        .collect();
    entities.sort_unstable();
    entities
}

/// Compose the effective mask for one request. See this module's doc for the precedence rule
/// and the clamp rationale. `base` is already the frozen fragment's row-space projection, so the
/// fragment itself is not a parameter: an entity with no verdict falls through directly to it.
/// `satisfied` is the viewer's granted term set, already resolved to `TermId`s by the auth
/// plugin path; `row_space` is used only for per-entity `row_of` lookups (O(log k), not the
/// O(bound) `project` cost).
pub fn compose(
    satisfied: &FxHashSet<TermId>,
    overlay: &Overlay,
    buffer: &IngestBuffer,
    base: Arc<RowProjection>,
    row_space: &RowSpace,
    denied: &Bitmap,
    buffered: Option<&[EntityId]>,
) -> EffectiveMask {
    let mut fail_rows: Vec<u32> = Vec::new();
    let mut pass_rows: Vec<u32> = Vec::new();

    // The buffer is the whole of the walk: deletions and suppressions are `denied`, folded in
    // below as one `andnot`, which keeps per-request work independent of how many denies have
    // ever been accepted. `verdict` is still the single expression of the precedence, so the
    // entity-space verbs and this walk cannot drift apart. An overlay entry is a deny, covered
    // by the fold below, so a buffered entity with one is skipped here.
    let mut visit = |entity: EntityId| {
        if overlay.touches(entity) {
            return;
        }
        // The row is asked for here rather than carried, so an entity the list names and the row
        // space does not contributes nothing. It has a row where a flush of another view has
        // published one for it while its own row is still buffered: an entity ingested into one
        // view and joined to a second can have the second's row published first. A join writes no
        // postings, so the entity is in no fragment, and the `plus` branch is what draws its mark
        // there.
        let Some(row) = row_space.row_of(entity) else {
            return;
        };
        if let Some(pass) = verdict_of(overlay, satisfied, entity, buffer.get(entity)) {
            if pass {
                pass_rows.push(row.raw());
            } else {
                fail_rows.push(row.raw());
            }
        }
    };
    match buffered {
        // The generation's list: the buffered entities with a row in this view, which is almost
        // never any of them, so per-request work is independent of how much is buffered.
        Some(entities) => entities.iter().copied().for_each(&mut visit),
        // No list for this view: the whole buffer, which reaches the same entities the long way
        // round.
        None => buffer.iter().for_each(|(&entity, _)| visit(entity)),
    }

    fail_rows.sort_unstable();
    pass_rows.sort_unstable();
    let fail_bitmap = Bitmap::of(&fail_rows);
    let pass_bitmap = Bitmap::of(&pass_rows);

    let base_bitmap = base.bitmap();
    // The deny mask is folded in last, unconditionally: `minus` gains every denied row `base`
    // carries and `plus` loses every denied row it proposed, so a denied entity is masked
    // whatever any other rule concluded about it. `andnot` clamps itself, so the deny half can
    // never violate the `∩ base` / `∖ base` clamps above.
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
        // Composition never filters: a request that carries a filter narrows the result
        // afterwards via `with_filter`, so the structural invariants above, and the unfiltered
        // total θ anchors on, stay properties of composition alone.
        filter: None,
        highlight: None,
        whole: std::sync::OnceLock::new(),
    }
}

/// `entity`'s raw id, cast down to the `u32` the fragment's bitmap operates over. Checked rather
/// than a truncating cast, so a violated invariant fails loudly instead of testing the wrong
/// entity.
fn entity_as_u32(entity: EntityId) -> u32 {
    u32::try_from(entity.raw())
        .expect("entity ids are capped at u32::MAX by the I9 allocator (contracts §2.6 r6)")
}

/// Is `entity` visible to this session — the one bit `/v1/items` needs. Answered in entity
/// space: the overlay and buffer are hash probes, `fragment.contains` is an O(1) Roaring probe,
/// and no `RowProjection` is built, so work is identical for an entity that does not exist, one
/// that is invisible, and one that is visible — closing the `/v1/items` timing channel rather
/// than narrowing it. Equivalent to `compose(...).contains_row(perm.row_of(entity))` wherever a
/// row exists: a `false` verdict lands in `minus` or outside `base`, a `true` verdict in `base`
/// or `plus`, and no verdict falls through to `base`, `project`'s image of the fragment, either
/// way.
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
    //! Unit tests for [`for_each_run_in`]: the closed→half-open conversion and its edges. Tested
    //! here because a run ending at `u32::MAX` needs no 2³²-entry permutation to build. Mask-level
    //! equivalence lives in `tests/compose.rs` and `tests/selection.rs`.

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
        // `last + 1` at `last == u32::MAX` would panic in debug and wrap in release.
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
    // The inverted range is the guard's own input, not a mistaken iteration bound.
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

#[cfg(test)]
mod walk_tests {
    //! [`compose`]'s walk over the derived list against a reference walk over the whole buffer,
    //! which resolves every buffered entity through [`verdict`] before it asks whether the entity
    //! has a row. Randomised states, so the two are compared over entities with a row and without,
    //! with an overlay entry and without, and against several satisfied sets.

    use super::*;

    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};
    use tessera_lifecycle::wal::WalRow;
    use tessera_lifecycle::ChangeOp;
    use tessera_store::write::write_permutation;
    use tessera_store::Permutation;

    const BOUND: u64 = 2_000;

    /// The reference walk's `(minus, plus)`: every buffered entity resolved through [`verdict`],
    /// which looks its item up in the buffer again, and its row asked for only once its terms have
    /// decided. The clamp and deny arithmetic is [`compose`]'s, restated so that what is compared
    /// is the walk alone.
    fn reference_diffs(
        satisfied: &FxHashSet<TermId>,
        overlay: &Overlay,
        buffer: &IngestBuffer,
        base: &RowProjection,
        row_space: &RowSpace,
        denied: &Bitmap,
    ) -> (Bitmap, Bitmap) {
        let mut fail_rows: Vec<u32> = Vec::new();
        let mut pass_rows: Vec<u32> = Vec::new();
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
            }
        }
        fail_rows.sort_unstable();
        pass_rows.sort_unstable();
        let base_bitmap = base.bitmap();
        (
            Bitmap::of(&fail_rows)
                .and(base_bitmap)
                .or(&denied.and(base_bitmap)),
            Bitmap::of(&pass_rows).andnot(base_bitmap).andnot(denied),
        )
    }

    fn buffer_row(
        buffer: &mut IngestBuffer,
        entity: u64,
        view: &str,
        join: bool,
        terms: Vec<TermId>,
    ) {
        let row = WalRow {
            external_id: Some(entity.to_le_bytes().to_vec()),
            entity_id: EntityId::new(entity),
            view: view.to_string(),
            join,
            descriptors: Vec::new(),
            x: 0.0,
            y: 0.0,
            scalars: Vec::new(),
            scoped: Vec::new(),
        };
        buffer.insert_row_with_terms(&row, terms);
    }

    /// The four grants the states below are composed against, from holding nothing to holding
    /// every term a buffered item can carry.
    fn grants() -> Vec<FxHashSet<TermId>> {
        vec![
            FxHashSet::default(),
            [0u32, 1].into_iter().map(TermId::new).collect(),
            [2u32, 3, 4].into_iter().map(TermId::new).collect(),
            (0u32..8).map(TermId::new).collect(),
        ]
    }

    #[test]
    fn the_walk_composes_the_diffs_the_per_entity_resolution_composes() {
        let temp = tempfile::TempDir::new().unwrap();
        let mut saw_pass = false;
        let mut saw_fail = false;

        for seed in 0..16u64 {
            let mut rng = StdRng::seed_from_u64(seed);

            // Two entities in three hold a row, so the walk takes the no-row path for the rest,
            // as a real buffer's walk does for almost all of it.
            let with_row: Vec<EntityId> = (0..BOUND)
                .filter(|entity| entity % 3 != 0)
                .map(EntityId::new)
                .collect();
            let path = temp.path().join(format!("permutation-{seed}.bin"));
            write_permutation(&path, &with_row, BOUND).unwrap();
            let row_space = RowSpace::new(
                Arc::new(Permutation::load(&path).unwrap()),
                with_row.len() as u32,
            );

            // The frozen fragment reaches `compose` already projected, so a bitmap of rows is the
            // whole of what the walk clamps against.
            let mut base_rows = Bitmap::new();
            for row in 0..with_row.len() as u32 {
                if rng.gen_bool(0.5) {
                    base_rows.add(row);
                }
            }
            let base = Arc::new(RowProjection::from_rows(base_rows));

            let mut buffer = IngestBuffer::new();
            let mut overlay = Overlay::new();
            for entity in 0..BOUND {
                if !rng.gen_bool(0.2) {
                    continue;
                }
                let terms: Vec<TermId> = (0..rng.gen_range(0..3u32))
                    .map(|_| TermId::new(rng.gen_range(0..8)))
                    .collect();
                if rng.gen_bool(0.2) {
                    // Join-only: the walk never sees it, exactly as `get` answers `None` for it.
                    buffer_row(&mut buffer, entity, "s1", true, Vec::new());
                } else {
                    if rng.gen_bool(0.3) {
                        buffer_row(&mut buffer, entity, "s1", true, Vec::new());
                    }
                    buffer_row(&mut buffer, entity, "s0", false, terms);
                }
                match rng.gen_range(0..10u32) {
                    0 => overlay.apply(EntityId::new(entity), ChangeOp::Delete),
                    1 => overlay.apply(EntityId::new(entity), ChangeOp::Suppress),
                    2 => {
                        overlay.apply(EntityId::new(entity), ChangeOp::Suppress);
                        overlay.apply(EntityId::new(entity), ChangeOp::Unsuppress);
                    }
                    _ => {}
                }
            }
            let denied = denied_rows_of(&overlay, &row_space);

            let buffered = buffered_rows_of(&buffer, &row_space);
            for satisfied in grants() {
                let mask = compose(
                    &satisfied,
                    &overlay,
                    &buffer,
                    Arc::clone(&base),
                    &row_space,
                    &denied,
                    Some(&buffered),
                );
                let (minus, plus) =
                    reference_diffs(&satisfied, &overlay, &buffer, &base, &row_space, &denied);
                assert_eq!(mask.minus, minus, "seed {seed}: minus differs");
                assert_eq!(mask.plus, plus, "seed {seed}: plus differs");
                saw_pass = saw_pass || !plus.is_empty();
                saw_fail = saw_fail || !minus.andnot(&denied).is_empty();
            }
        }

        assert!(
            saw_pass,
            "no state gave a buffered entity with a row whose terms pass: the `plus` branch went \
             untested"
        );
        assert!(
            saw_fail,
            "no state gave a buffered entity with a row in `base` whose terms fail: the `minus` \
             branch went untested"
        );
    }
}
