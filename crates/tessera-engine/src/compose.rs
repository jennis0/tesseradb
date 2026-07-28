//! I1 mask composition: `M_auth = (fragment \ L) ∪ direct_eval(L)` — evaluated as diffs over a
//! cached row-space projection of the frozen fragment, never by recomputing the whole mask
//! (task-10 brief).
//!
//! [`RowProjection`] is the cached `Permutation::project` output for one `(token, slice, pin)` —
//! computed once (seconds at 10⁹ rows, shared-context constraint 8) and reused across every
//! viewport and every `compose` call in that session, never recomputed on a per-viewport path.
//!
//! [`compose`] walks `L = keys(overlay) ∪ {buffered entities ≥ fragment.watermark}` exactly once
//! per entity, resolving each with the fixed precedence `deleted > suppressed > evaluate_terms >
//! buffered`, and turns the result into two row-space bitmaps against `base`:
//! `minus = {row(e) : e fails} ∩ base` and `plus = {row(e) : e passes} ∖ base`. The `∩ base` /
//! `∖ base` clamps are load-bearing, not cosmetic: without them, denying an entity the session's
//! fragment never contained would corrupt every count over its tile (a spurious −1, possibly
//! driving a count negative), and an evaluate-pass already inside the fragment would double-count
//! its tile by the same mechanism in the other direction.

use std::ops::Range;
use std::sync::Arc;

use croaring::Bitmap;
use rustc_hash::FxHashSet;

use tessera_authz::FrozenFragment;
use tessera_lifecycle::{IngestBuffer, Overlay};
use tessera_store::Permutation;
use tessera_types::TermId;

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
}

impl RowProjection {
    /// Project `fragment`'s entity-space bitmap into this segment's row space via `perm`. Do not
    /// call this on the per-viewport path — see this struct's doc.
    pub fn new(fragment: &FrozenFragment, perm: &Permutation) -> Self {
        RowProjection {
            rows: perm.project(&fragment.view()),
        }
    }

    /// Build directly from an already-projected row-space bitmap (e.g. in tests, or when a
    /// caller has its own reason to hold the projection independently of a `FrozenFragment`).
    pub fn from_rows(rows: Bitmap) -> Self {
        RowProjection { rows }
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

    /// Merged, ascending iteration over the effective mask restricted to `r`: `(base ∩ r) ∖ minus
    /// ∪ (plus ∩ r)`.
    pub fn iter_range(&self, r: Range<u32>) -> impl Iterator<Item = u32> + '_ {
        let range_mask = Bitmap::from_range(r.clone());
        let mut result = self.base.bitmap().and(&range_mask);
        result.andnot_inplace(&self.minus);
        let plus_in_range = self.plus.and(&range_mask);
        // `or_inplace` on already-disjoint-from-`result` content (plus ∩ base = ∅ by
        // construction — see the structural invariant asserted in `compose`) — no double count.
        result.or_inplace(&plus_in_range);
        result.to_vec().into_iter()
    }

    pub fn contains_row(&self, row: u32) -> bool {
        if self.minus.contains(row) {
            return false;
        }
        self.base.bitmap().contains(row) || self.plus.contains(row)
    }

    /// The two structural invariants (task-10 brief): `minus ⊆ base` and `plus ∩ base = ∅`.
    /// Exposed for tests; also asserted in debug builds at construction time in [`compose`].
    pub fn check_structural_invariants(&self) -> bool {
        self.minus.is_subset(self.base.bitmap()) && self.plus.and(self.base.bitmap()).is_empty()
    }
}

/// Compose the effective mask for one request. See this module's doc for the precedence rule and
/// the clamp rationale.
///
/// `fragment` is consulted only for its `watermark` (the SEGMENTS watermark the frozen fragment
/// was built against — a buffered entity below it would mean a bundle/WAL inconsistency and is
/// excluded from `L` defensively, even though Phase 1 never actually produces one). `satisfied`
/// is the viewer's granted term set (already resolved to `TermId`s by the auth plugin path).
/// `base` is the cached row-space projection (see [`RowProjection`]'s doc); `perm` is used only
/// for per-entity `row_of` lookups (O(1)-ish, not the O(bound) `project` cost).
pub fn compose(
    fragment: &FrozenFragment,
    satisfied: &FxHashSet<TermId>,
    overlay: &Overlay,
    buffer: &IngestBuffer,
    base: Arc<RowProjection>,
    perm: &Permutation,
) -> EffectiveMask {
    let watermark = fragment.watermark;

    let mut fail_rows: Vec<u32> = Vec::new();
    let mut pass_rows: Vec<u32> = Vec::new();

    // Rules 1–3: every entity the overlay has an opinion on, resolved exactly once with
    // precedence deleted > suppressed > evaluate_terms. An entry that is present but currently
    // neutral (e.g. `suppress → unsuppress`, with no `Predicate` ever applied) yields no verdict
    // at all — it is correctly already reflected in `base`, and rule 4 does not pick it up either
    // (see below), so it contributes nothing to the diff. This is deliberate, not an oversight:
    // recomputing "no verdict" from scratch every time is what makes unsuppress a pure
    // subtraction from `minus` rather than a special case.
    for (&entity, entry) in overlay.iter() {
        let verdict = if entry.deleted || entry.suppressed {
            Some(false)
        } else {
            entry
                .evaluate_terms
                .as_ref()
                .map(|terms| terms.iter().any(|t| satisfied.contains(t)))
        };

        if let Some(pass) = verdict {
            if let Some(row) = perm.row_of(entity) {
                if pass {
                    pass_rows.push(row.raw());
                } else {
                    fail_rows.push(row.raw());
                }
            }
            // No row in this segment: Phase 1 has no cross-segment geometry, so this entity
            // simply cannot contribute to this segment's diff either way.
        }
    }

    // Rule 4: buffered entities at or past the fragment's own watermark, with no overlay entry
    // (an overlay entry — even a neutral one — takes precedence per the rule ordering above, and
    // was already resolved, or deliberately given no verdict, in the loop above).
    for (&entity, item) in buffer.iter() {
        if entity.raw() < watermark {
            continue;
        }
        if overlay.get(entity).is_some() {
            continue;
        }
        let pass = item.terms.iter().any(|t| satisfied.contains(t));
        if let Some(row) = perm.row_of(entity) {
            if pass {
                pass_rows.push(row.raw());
            } else {
                fail_rows.push(row.raw());
            }
        }
        // Phase 1 buffered entities have no row anywhere (no flush yet) — this branch is kept
        // and tested (with a synthetic permutation) against the day a later phase gives buffered
        // items provisional rows, but today it always takes the "no row" path above.
    }

    fail_rows.sort_unstable();
    pass_rows.sort_unstable();
    let fail_bitmap = Bitmap::of(&fail_rows);
    let pass_bitmap = Bitmap::of(&pass_rows);

    let base_bitmap = base.bitmap();
    let minus = fail_bitmap.and(base_bitmap);
    let plus = pass_bitmap.andnot(base_bitmap);

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
