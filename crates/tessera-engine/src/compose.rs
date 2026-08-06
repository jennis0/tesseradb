//! I1 mask composition: `M_auth = (fragment \ L) ∪ direct_eval(L)` — evaluated as diffs over a
//! cached row-space projection of the frozen fragment, never by recomputing the whole mask.
//!
//! [`RowProjection`] is the cached `Permutation::project` output for one `(token, slice, pin)` —
//! computed once — seconds at 10⁹ rows — and reused across every
//! viewport and every `compose` call in that session, never recomputed on a per-viewport path.
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
//! retires: two of the three retirement rules do not exist, so the deny set only grows. And
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

use crate::DenyMask;

/// A cached row-space projection of one frozen fragment, for one `(token, slice, pin)`.
///
/// Constructing this crosses entity space into row space via `Permutation::project`, which
/// touches every set bit of the fragment and then sorts the result — cheap at the 10k scale this
/// phase's fixtures use, but *seconds* at 10⁹ rows (see `Permutation::project`'s doc). Callers
/// must cache the result per `(token, slice, pin)` and never reconstruct it on a per-viewport
/// path (shared-context constraint 8) — `compose` itself only ever reads it via `range_cardinality`
/// / `contains`, both O(containers touched), never re-derives it from the fragment.
pub struct RowProjection {
    rows: Bitmap,
    /// How many of the slice's extents this projection already covers — the index
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
    /// heuristic, and costs one string comparison against a *measured* 10.7 s rebuild.
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
    /// Project `fragment`'s entity-space bitmap into this slice's row space. Do not call this on
    /// the per-viewport path — see this struct's doc.
    pub fn new(fragment: &FrozenFragment, rows: &RowSpace) -> Self {
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
    /// was a full rebuild: a *measured* 4 550 ms at 10⁹, essentially all of it the base
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
    pub fn from_rows(rows: Bitmap) -> Self {
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

/// The composed, effective visibility mask for one request: `base` plus a small diff (`minus`,
/// `plus`) capturing every overlay/buffer effect since `base` was cached. Never materialises the
/// full mask — every operation below costs O(containers touched) in the diffs, which are
/// expected to be tiny relative to `base`.
pub struct EffectiveMask {
    base: Arc<RowProjection>,
    minus: Bitmap,
    plus: Bitmap,
}

impl EffectiveMask {
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
    /// a count of items *outside* `M_auth`. See [`crate::select::Threshold::anchor`].
    ///
    /// **Deliberately the same arithmetic as [`Self::count_range`]**, one term at a time, so the
    /// anchor and the per-tile counts can never disagree about what composition means. Two
    /// transcriptions of the composition rule is the same failure mode as two transcriptions of the
    /// deny precedence — see [`verdict`]'s doc.
    ///
    /// O(containers in the diffs): `base`'s cardinality is memoised
    /// ([`RowProjection::cardinality`]) and the diffs are tiny by construction.
    pub fn visible_total(&self) -> u64 {
        self.base.cardinality() - self.minus.cardinality() + self.plus.cardinality()
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
    /// `docs/evidence/memos/2026-07-30-f1-selection-overdraw.md`.
    ///
    /// **Where that memo's row counter went.** It asked for `iter_range`'s `ExactSizeIterator` so
    /// the row counter could be incremented by a free `.len()`. Returning a bitmap loses
    /// `ExactSizeIterator`, so `Selection::of` counts the rows it reads as it reads them
    /// (`Selection::rows_visited`) — one increment on a loop that was already running. Deriving it
    /// from the caller's `visible` instead would be cheaper still and worthless: the counter exists
    /// to be compared against `visible`.
    pub fn rows_in_range(&self, r: Range<u32>) -> Bitmap {
        let range_mask = Bitmap::from_range(r);
        let mut result = self.base.bitmap().and(&range_mask);
        result.andnot_inplace(&self.minus);
        let plus_in_range = self.plus.and(&range_mask);
        // `or_inplace` on already-disjoint-from-`result` content (plus ∩ base = ∅ by
        // construction — see the structural invariant asserted in `compose`) — no double count.
        result.or_inplace(&plus_in_range);
        result
    }

    pub fn contains_row(&self, row: u32) -> bool {
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
    pub fn diffs_are_empty(&self) -> bool {
        self.minus.is_empty() && self.plus.is_empty()
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
fn verdict(
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
    // entity-allocation order and flush order coincided — one slice per partition — so removing it
    // takes a silent multi-slice hazard out with it.
    buffer
        .get(entity)
        .map(|item| item.terms.iter().any(|t| satisfied.contains(t)))
}

/// Derive the row-space deny mask from the authoritative entity-space stores.
///
/// **`{row_of(e) : e ∈ deleted ∪ suppressed}`, per slice, and nothing else.** The union is taken
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
/// An entity with no row — still buffered, or belonging to another slice — contributes nothing:
/// the mask is complete for what it governs, which is row-space questions, and `verdict` answers
/// the entity-space ones.
pub(crate) fn derive_denied(overlay: &Overlay, bundle: &Bundle) -> DenyMask {
    let mut out = DenyMask::default();
    for partition in bundle.partitions.values() {
        for (slice, slice_data) in &partition.slices {
            // Every slice gets an entry, empty or not: a missing one must mean "the mask and the
            // bundle disagree", never "nothing is denied here".
            out.insert(
                slice.clone(),
                denied_rows_of(overlay, &slice_data.row_space),
            );
        }
    }
    out
}

/// One slice's deny mask — see [`derive_denied`], whose per-slice body this is.
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
/// `watermark` alone, to gate rule 4; that gate is gone (see [`verdict`]), and with it the last
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

    EffectiveMask { base, minus, plus }
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

    /// The documented [`EffectiveMask::decode_source`] contract walk: seek to `r.start`, batch
    /// decode, stop at the first value `≥ r.end` — the exact loop `select.rs`'s value tier
    /// drives, transcribed once here so its clamp semantics are pinned at the bitmap level.
    fn source_walk(bitmap: &Bitmap, r: Range<u32>) -> Vec<u32> {
        let mut out = Vec::new();
        if r.start >= r.end {
            return out;
        }
        let mut iter = bitmap.iter();
        iter.reset_at_or_after(r.start);
        let mut buf = [0u32; 64];
        'outer: loop {
            let n = iter.next_many(&mut buf);
            if n == 0 {
                break;
            }
            for &v in &buf[..n] {
                if v >= r.end {
                    break 'outer;
                }
                out.push(v);
            }
        }
        out
    }

    /// The contract walk yields exactly `bitmap ∩ r` — [`assert_flattens_to_intersection`]'s
    /// sibling for the caller-driven value decode.
    fn assert_values_match_intersection(bitmap: &Bitmap, r: Range<u32>) {
        let expected: Vec<u32> = bitmap.and(&Bitmap::from_range(r.clone())).to_vec();
        assert_eq!(
            source_walk(bitmap, r.clone()),
            expected,
            "values disagree with bitmap ∩ {r:?}"
        );
    }

    #[test]
    fn value_walk_excludes_u32_max_and_clamps_to_the_range_end() {
        let mut b = Bitmap::new();
        b.add_range(u32::MAX - 100..=u32::MAX);
        // `r.end` is exclusive and cannot exceed u32::MAX, so the value u32::MAX is never
        // emitted — same parity with `Bitmap::from_range` as the run walk, with no `+ 1`
        // arithmetic to protect at all.
        let r = (u32::MAX - 50)..u32::MAX;
        let got = source_walk(&b, r.clone());
        assert_eq!(got.first().copied(), Some(u32::MAX - 50));
        assert_eq!(got.last().copied(), Some(u32::MAX - 1));
        assert_values_match_intersection(&b, r);
    }

    #[test]
    fn value_walk_seeks_to_the_range_start_and_stops_at_its_end() {
        let mut b = Bitmap::new();
        b.add_range(0..=9_999);
        assert_values_match_intersection(&b, 0..10_000);
        assert_values_match_intersection(&b, 100..5_000);
        assert_eq!(source_walk(&b, 4_000..6_000).len(), 2_000);
    }

    #[test]
    fn value_walk_yields_nothing_on_empty_or_disjoint_inputs() {
        let empty = Bitmap::new();
        assert!(source_walk(&empty, 0..1000).is_empty());
        let mut b = Bitmap::new();
        b.add_range(100..=200);
        assert!(source_walk(&b, 50..50).is_empty(), "empty range");
        assert!(source_walk(&b, 0..100).is_empty(), "range before");
        assert!(source_walk(&b, 201..300).is_empty(), "range after");
    }

    #[test]
    fn decode_source_walk_equals_rows_in_range_on_both_variants() {
        // At the unit level only the Base variant's bitmap is constructible without a full
        // compose fixture; the mask-level both-variant equivalence lives in `tests/compose.rs`.
        let mut b = Bitmap::new();
        b.add_range(10..=99);
        b.add(150);
        let clamped: Vec<u32> = source_walk(&b, 50..151);
        let from_range: Vec<u32> = b.and(&Bitmap::from_range(50..151)).to_vec();
        assert_eq!(clamped, from_range);
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
                assert_values_match_intersection(&b, a.min(z)..a.max(z));
            }
        }
    }
}
