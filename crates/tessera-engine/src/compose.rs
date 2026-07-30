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
use tessera_types::{EntityId, TermId};

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
    /// Project `fragment`'s entity-space bitmap into this segment's row space via `perm`. Do not
    /// call this on the per-viewport path — see this struct's doc.
    pub fn new(fragment: &FrozenFragment, perm: &Permutation) -> Self {
        Self::from_rows(perm.project(&fragment.view()))
    }

    /// Build directly from an already-projected row-space bitmap (e.g. in tests, or when a
    /// caller has its own reason to hold the projection independently of a `FrozenFragment`).
    pub fn from_rows(rows: Bitmap) -> Self {
        let cardinality = rows.cardinality();
        RowProjection { rows, cardinality }
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

/// The per-entity verdict, `deleted > suppressed > evaluate_terms > buffered`, or `None` when
/// the overlay and the buffer have no opinion and the frozen fragment already carries the
/// answer. **The single source of this precedence** — [`compose`] turns it into row-space diffs
/// for range arithmetic, [`visible_to`] reads it directly for a single entity. Two transcriptions
/// of a precedence rule is how a suppression stops suppressing (lifecycle §3, caught twice in
/// review) — do not re-derive this logic anywhere else.
///
/// An overlay entry — even a *neutral* one (present, but currently no active verdict, e.g. after
/// `suppress → unsuppress` with no `Predicate` ever applied) — takes precedence over the buffer:
/// its `None` here is correct, not a fall-through, because rule 4 below only ever applies when
/// the overlay has no entry at all.
fn verdict(
    overlay: &Overlay,
    buffer: &IngestBuffer,
    satisfied: &FxHashSet<TermId>,
    watermark: u64,
    entity: EntityId,
) -> Option<bool> {
    if let Some(entry) = overlay.get(entity) {
        return if entry.deleted || entry.suppressed {
            Some(false)
        } else {
            entry
                .evaluate_terms
                .as_ref()
                .map(|terms| terms.iter().any(|t| satisfied.contains(t)))
        };
    }

    // Rule 4: buffered entities at or past the fragment's own watermark, with no overlay entry
    // (handled above — an overlay entry, even a neutral one, takes precedence).
    if entity.raw() < watermark {
        return None;
    }
    buffer
        .get(entity)
        .map(|item| item.terms.iter().any(|t| satisfied.contains(t)))
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

    // Rules 1–3: every entity the overlay has an opinion on, resolved exactly once via the
    // shared `verdict` function. A neutral entry yields no verdict at all — it is correctly
    // already reflected in `base`, and rule 4 does not pick it up either (see `verdict`'s doc),
    // so it contributes nothing to the diff. This is deliberate, not an oversight: recomputing
    // "no verdict" from scratch every time is what makes unsuppress a pure subtraction from
    // `minus` rather than a special case.
    for (&entity, _) in overlay.iter() {
        if let Some(pass) = verdict(overlay, buffer, satisfied, watermark, entity) {
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
    for (&entity, _) in buffer.iter() {
        if entity.raw() < watermark {
            continue;
        }
        if overlay.get(entity).is_some() {
            continue;
        }
        if let Some(pass) = verdict(overlay, buffer, satisfied, watermark, entity) {
            if let Some(row) = perm.row_of(entity) {
                if pass {
                    pass_rows.push(row.raw());
                } else {
                    fail_rows.push(row.raw());
                }
            }
            // Phase 1 buffered entities have no row anywhere (no flush yet) — this branch is
            // kept and tested (with a synthetic permutation) against the day a later phase gives
            // buffered items provisional rows, but today it always takes the "no row" path
            // above.
        }
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
/// Appendix C, C4 annotation; Critical C-5).
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
    verdict(overlay, buffer, satisfied, fragment.watermark, entity)
        .unwrap_or_else(|| fragment.view().contains(entity_as_u32(entity)))
}
