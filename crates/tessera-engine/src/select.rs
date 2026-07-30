//! Per-user LOD selection — design §7.2's definition, evaluated inside the mask (I7).
//!
//! For a tile *T* at depth *d*, with `vis(T)` its visible row set ordered ascending by `tessera_id`:
//!
//! ```text
//! cap    = min(request_k, k_max_marks)
//! C_θ(T) = |{ i ∈ vis(T) : tessera_id(i) < P_d }|
//! m(T)   = min(cap, max(min(k_min, cap), C_θ(T)))
//! served(T) = the min(m(T), |vis(T)|) smallest members of vis(T) by tessera_id
//! ```
//!
//! A **floor** of `k_min` (the I7 guarantee — the sparsest principals' maps are never empty), a
//! **threshold** at `P_d` (the density signal: a tile with *n* visible draws `θ_d·n` marks, and
//! tiles are equal screen area, so mark count *is* density), and a **cap**. §7.2 carries the
//! reasoning, the nesting proof and the accepted residuals; this module implements it.
//!
//! **Two things a reader needs that are not obvious from the code:**
//!
//! - **Nesting holds only for a fixed `cap`.** `cap = min(request_k, k_max_marks)` and
//!   `k_max_marks` is a server constant, so it varies only through the client's `k` — and a client
//!   that *reduces* `k` while zooming in forfeits nesting and will see marks pop out. The engine
//!   sees one request at a time and cannot enforce it; contracts §3.2 states it as a client
//!   obligation. Recorded here because §7.2 keeps its bit-reversal note for exactly this class of
//!   mistake being re-derived.
//! - **The comparator is the full `tessera_id`, not the `priority` prefix**, and no
//!   prefix-scan-then-fall-through path exists (§7.2 r21 directs that none be built in Phase 1).
//!   The two orders are identical because `priority` is a prefix, so this costs no correctness —
//!   only 8 B/row of scanned column where 2 B would do. Design Appendix A records that cost and
//!   §7.2 the trigger for revisiting it.

use std::collections::BinaryHeap;
use std::ops::Range;

use tessera_store::read::SegmentData;

use crate::compose::EffectiveMask;

/// The depth-*d* selection threshold, as a cut point over the identity space.
///
/// `Saturated` is a first-class state, **not** a value clamped to `u64::MAX`: at θ ≥ 1 the
/// threshold must admit *every* identity, and `Cut(u64::MAX)` would wrongly exclude the single row
/// whose `tessera_id` is `u64::MAX`. That exactness is what lets [`Selection::of`]'s fast path be
/// exact rather than conservative — see its doc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Threshold {
    /// Admits `tessera_id < cut`.
    Cut(u64),
    /// θ_d ≥ 1: admits every identity.
    Saturated,
}

impl Threshold {
    /// Anchor θ at depth 0 from the viewer's own total visible count.
    ///
    /// `P_0 = m_target · 2⁶⁴ / v_total`, so the *mean* occupied tile at any depth draws `m_target`
    /// marks: priorities are uniform over the identity space, so `P(id < P) = P / 2⁶⁴` and a tile of
    /// *n* visible items serves `n · P_d / 2⁶⁴`.
    ///
    /// **`v_total` must be the COMPOSED visible cardinality** — the mask *after* the overlay diff,
    /// not the cached `RowProjection`'s. This is an I2 requirement, not a preference: the
    /// pre-overlay projection strictly contains `M_auth` after any accepted delete or suppression,
    /// so anchoring there would let a viewer aggregate mark counts across tiles, solve for the
    /// anchor, difference it against its own summed per-tile `visible` (which §7.1 discloses
    /// exactly), and recover **a running estimate of how many of its own items have been denied**.
    /// See [`EffectiveMask::visible_total`]; the property is pinned by
    /// `the_theta_anchor_falls_when_an_item_is_suppressed`.
    ///
    /// The `4^d` progression assumes the viewer's items spread over ~`4^d` occupied tiles, which
    /// clustered corpora violate — so real tiles draw more than `m_target` and a band of mid-range
    /// depths pins at the cap. Accepted by the owner over both a measured anchor and a
    /// client-supplied θ; §7.2 carries the worked numbers and the exact extent of the flat region.
    pub fn anchor(v_total: u64, m_target: u64) -> Self {
        if v_total == 0 {
            // No visible rows anywhere: every tile is empty and skipped. Saturated is the
            // harmless answer, and avoids a division by zero.
            return Threshold::Saturated;
        }
        // u128 is required, not defensive: `m_target << 64` does not fit in a u64 at all.
        let p0 = ((m_target as u128) << 64) / (v_total as u128);
        if p0 >= 1u128 << 64 {
            // θ_0 ≥ 1 — the viewer can see no more than `m_target` items in total, so all of them
            // should be drawn.
            Threshold::Saturated
        } else {
            Threshold::Cut(p0 as u64)
        }
    }

    /// θ_d from θ_0: `P_d = P_0 << 2d`, saturating.
    ///
    /// `P_{d+1} = 4·P_d` is what makes the per-tile expectation depth-stable (a child holds ~n/4
    /// items, so `4θ_d · n/4 = θ_d · n`) and what makes θ monotone in depth, which is what the
    /// nesting proof's threshold clause needs.
    ///
    /// **The overflow test is `leading_zeros`, and this is not a matter of taste.**
    /// `u64::checked_shl(n)` returns `None` only for `n >= 64`; for `n < 64` it performs a
    /// *wrapping* shift and silently discards the high bits. So `P_0 = 2⁶³` at depth 1 would yield
    /// `Some(0)`, i.e. `Cut(0)`, i.e. `C_θ = 0` in every tile at every depth — every tile drawing
    /// exactly `k_min` marks forever, with no error raised anywhere. There is no `saturating_shl`
    /// in std to reach for instead.
    pub fn at_depth(&self, depth: u8) -> Self {
        match *self {
            Threshold::Saturated => Threshold::Saturated,
            Threshold::Cut(p0) => {
                let shift = 2u32 * depth as u32;
                if shift >= 64 || p0.leading_zeros() < shift {
                    Threshold::Saturated
                } else {
                    Threshold::Cut(p0 << shift)
                }
            }
        }
    }

    /// Does this threshold admit `id`?
    pub fn admits(&self, id: u64) -> bool {
        match *self {
            Threshold::Saturated => true,
            Threshold::Cut(cut) => id < cut,
        }
    }

    pub fn is_saturated(&self) -> bool {
        matches!(self, Threshold::Saturated)
    }
}

/// The selection parameters for one request, resolved once and shared across every tile.
///
/// **θ's anchor must be a whole-slice total, never per-segment or per-partition** — a local anchor
/// makes "below the cut" mean different things in different segments, and the merge stops computing
/// the definition. Both cases fail closed today (`MultiSegmentSlice`, `MultiPartitionSlice`);
/// §7.2 and §12.3 carry the merge rule for when they no longer do.
#[derive(Debug, Clone, Copy)]
pub struct SelectParams {
    /// The floor clause (§7.2's `k_min`) — the I7 guarantee. Clamped to `cap` at use, so a request
    /// of `k = 1` against `k_min = 2` cannot produce `m > cap`.
    pub k_min: usize,
    /// `min(request_k, k_max_marks)`. Applied *inside* the definition, which is free: `served(T)`
    /// is a `tessera_id` prefix, so computing at `min(k, k_max_marks)` and computing at
    /// `k_max_marks` then truncating to `k` give identical output. Doing it inside bounds the
    /// selection heap and the output gather. **It does not bound the count pass** — `C_θ` is a
    /// masked count, so the tile's visible rows must be walked regardless.
    pub cap: usize,
    pub threshold: Threshold,
}

impl SelectParams {
    /// The effective floor: `k_min` clamped to `cap`.
    ///
    /// §7.2's memo form is `max(k_min, min(cap, C_θ))`, which exceeds `cap` whenever
    /// `k_min > cap`. The form used here — `min(cap, max(floor, C_θ))` with `floor = min(k_min,
    /// cap)` — is identical whenever `k_min <= cap` and well-defined when it is not.
    fn floor(&self) -> usize {
        self.k_min.min(self.cap)
    }
}

/// How many of a tile's visible rows the definition serves.
///
/// `m(T) = min(cap, max(floor, C_θ))`, then clamped to the tile's visible count — the outer clamp
/// matters when the floor exceeds what is actually there, and it is what makes
/// [`Selection::of`]'s fast path exact.
pub fn served_count(c_theta: u64, params: &SelectParams, visible: u64) -> usize {
    let c = usize::try_from(c_theta).unwrap_or(usize::MAX);
    let m = params.cap.min(params.floor().max(c));
    usize::try_from(visible).unwrap_or(usize::MAX).min(m)
}

/// Does the definition provably serve **every** visible row in a tile with `visible` of them?
///
/// **Both conditions are exact, not conservative.** Serving all of `vis(T)` is correct iff
/// `m(T) >= |vis(T)|`, and `C_θ` is unknowable without reading the column — so exactly two
/// conditions discharge it from quantities already in hand:
///
/// - `V <= min(k_min, cap)`, the floor alone covering the tile; or
/// - `Saturated ∧ V <= cap`, since θ ≥ 1 means `C_θ = V` *by construction*. This is why
///   [`Threshold::Saturated`] is a variant rather than `Cut(u64::MAX)` — the latter would exclude
///   `id == u64::MAX` and break the equivalence at the boundary.
///
/// `V <= k` alone would be unsound: the threshold clause deliberately serves fewer than `V`, so a
/// tile with `V = 100` and `C_θ = 5` serves 5, and serving all 100 would destroy the density signal.
///
/// Worth ~30% of selection cost on the tiles it covers — 75 µs per 300-tile viewport at `cap = 30`,
/// 2.7 ms at `cap = 1000` against a 10 ms p99 budget (`examples/route_saving.rs`, re-runnable). A
/// review costed it at the smaller figure and recommended deletion; the saving scales with `cap`,
/// which is why it survived.
fn serves_all_visible(params: &SelectParams, visible: u64) -> bool {
    visible <= params.floor() as u64
        || (params.threshold.is_saturated() && visible <= params.cap as u64)
}

/// One tile's selected rows, ascending by `tessera_id`.
pub struct Selection {
    /// Row indices, **ascending by the row's `tessera_id`** — not by row index.
    pub rows: Vec<u32>,
}

impl Selection {
    /// Evaluate §7.2's definition over `range` under `mask`.
    ///
    /// Rows come back ascending by `tessera_id` whichever branch runs — the nesting argument's
    /// client-truncation clause needs the payload to be a *prefix*, and a branch-dependent order
    /// would be a differential-oracle landmine.
    pub fn of(
        mask: &EffectiveMask,
        segment: &SegmentData,
        range: Range<u32>,
        params: &SelectParams,
        visible: u64,
    ) -> Self {
        // A request that asks for no points still wants counts (the `k = 0` count-only arm the
        // benches measure). Without this the general branch would run a full counting pass whose
        // result is discarded, and the count-only benchmark would stop measuring counting.
        if params.cap == 0 {
            return Selection { rows: Vec::new() };
        }

        let ids = segment.columns.tessera_id();
        let visible_rows = mask.rows_in_range(range);

        let rows: Vec<u32> = if serves_all_visible(params, visible) {
            // Everything visible is served, so there is nothing to count and nothing to select —
            // only ordering. Decorate-sort-undecorate rather than `sort_unstable_by_key`: the latter
            // re-reads the identity column on every comparison, so V log V strided lookups into an
            // 8 B/row mmap where V suffice. Immaterial at the old default cap of 30; at cap 500 it is
            // ~1.4M lookups per viewport against ~150k.
            let mut decorated: Vec<(u64, u32)> = visible_rows
                .iter()
                .map(|row| (ids[row as usize], row))
                .collect();
            decorated.sort_unstable();
            decorated.into_iter().map(|(_, row)| row).collect()
        } else {
            // One pass. `m(T) <= cap` always, so the `cap` smallest ids in the tile contain the
            // served set for *any* m the counting pass can produce — which is what makes a single
            // pass sufficient.
            //
            // A `BinaryHeap` is a max-heap, which is what is wanted: the largest of the `cap`
            // best-so-far sits at the root, so it is both the eviction candidate and the rejection
            // threshold.
            //
            // **The peek-reject is not a micro-optimisation.** Without it every visible row is
            // pushed and sifted before being thrown away — O(V log cap) sift work against O(V)
            // compares, measured at 19x the irreducible counting cost (~36 ms for 300 tiles of
            // 4,000 visible rows, against a 10 ms p99 budget) and worsening with V as the reject
            // rate rises. Output is identical: a row not smaller than the largest of the `cap`
            // smallest cannot be among them.
            //
            // Memory: O(min(cap, V)) for the heap, plus `rows_in_range`'s bitmap, which is
            // O(containers touched) rather than O(V) — see its doc for what that used to cost.
            let mut c_theta: u64 = 0;
            let heap_cap = params.cap.min(visible as usize).saturating_add(1);
            let mut heap: BinaryHeap<(u64, u32)> = BinaryHeap::with_capacity(heap_cap);
            for row in visible_rows.iter() {
                let id = ids[row as usize];
                if params.threshold.admits(id) {
                    c_theta += 1;
                }
                if heap.len() == params.cap {
                    // Safe: len == cap >= 1 here, since cap == 0 returned early above.
                    if id >= heap.peek().expect("non-empty at len == cap").0 {
                        continue;
                    }
                    heap.pop();
                }
                heap.push((id, row));
            }

            let m = served_count(c_theta, params, visible);
            let mut kept: Vec<(u64, u32)> = heap.into_vec();
            kept.sort_unstable();
            kept.truncate(m);
            kept.into_iter().map(|(_, row)| row).collect()
        };

        debug_assert!(
            rows.windows(2)
                .all(|w| ids[w[0] as usize] != ids[w[1] as usize]),
            "two rows in one tile share a tessera_id — the identity is a bijection over 2^64 with \
             one row per entity (contracts §2.6), and the determinism of the whole selection rests \
             on that being true"
        );
        Selection { rows }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depth_progression_is_times_four_until_saturation() {
        let t = Threshold::anchor(1_000_000, 16);
        let Threshold::Cut(p0) = t else {
            panic!("expected a cut for v_total=1e6, m_target=16");
        };
        for d in 0..5u8 {
            match t.at_depth(d) {
                Threshold::Cut(p) => assert_eq!(p, p0 << (2 * d as u32), "depth {d}"),
                Threshold::Saturated => panic!("depth {d} saturated unexpectedly"),
            }
        }
    }

    /// The `checked_shl` trap. `u64::checked_shl(n)` only fails for `n >= 64`; below that it
    /// wraps and discards the high bits, so a cut one bit below the boundary would silently
    /// become `Cut(0)` — every tile drawing exactly `k_min` at every depth, with no error.
    #[test]
    fn a_cut_one_bit_below_the_boundary_saturates_rather_than_wrapping_to_zero() {
        for shift in 0..32u32 {
            let p0 = 1u64 << (63 - shift.min(63));
            let t = Threshold::Cut(p0);
            for d in 0..32u8 {
                let want_saturated = 2 * d as u32 >= 64 || p0.leading_zeros() < 2 * d as u32;
                match t.at_depth(d) {
                    Threshold::Saturated => assert!(
                        want_saturated,
                        "p0={p0:#x} depth={d}: saturated when a shift would have fitted"
                    ),
                    Threshold::Cut(p) => {
                        assert!(
                            !want_saturated,
                            "p0={p0:#x} depth={d}: returned Cut where the shift overflows"
                        );
                        assert_ne!(p, 0, "p0={p0:#x} depth={d}: wrapped to Cut(0)");
                        assert_eq!(p, p0 << (2 * d as u32));
                    }
                }
            }
        }
    }

    #[test]
    fn saturation_is_sticky_across_depths() {
        let t = Threshold::Saturated;
        for d in 0..17u8 {
            assert!(t.at_depth(d).is_saturated(), "depth {d}");
        }
    }

    #[test]
    fn a_viewer_seeing_no_more_than_the_target_is_shown_everything() {
        for v in [0u64, 1, 8, 16] {
            assert!(
                Threshold::anchor(v, 16).is_saturated(),
                "v_total={v} must anchor saturated"
            );
        }
        assert!(
            !Threshold::anchor(17, 16).is_saturated(),
            "v_total just above the target must produce a cut"
        );
    }

    #[test]
    fn saturated_admits_the_maximum_identity() {
        // The reason Saturated is an enum variant and not Cut(u64::MAX): the latter excludes this
        // id, which would make the fast path inexact at the boundary.
        assert!(Threshold::Saturated.admits(u64::MAX));
        assert!(!Threshold::Cut(u64::MAX).admits(u64::MAX));
    }

    fn params(k_min: usize, cap: usize, threshold: Threshold) -> SelectParams {
        SelectParams {
            k_min,
            cap,
            threshold,
        }
    }

    #[test]
    fn served_count_obeys_floor_cap_and_the_visible_clamp() {
        let p = params(2, 128, Threshold::Cut(1));
        assert_eq!(
            served_count(0, &p, 1000),
            2,
            "floor applies when C_theta is 0"
        );
        assert_eq!(served_count(5, &p, 1000), 5, "the threshold clause governs");
        assert_eq!(served_count(500, &p, 1000), 128, "the cap governs");
        assert_eq!(served_count(0, &p, 1), 1, "never more than are visible");
        assert_eq!(served_count(500, &p, 3), 3, "never more than are visible");
    }

    #[test]
    fn served_count_never_exceeds_the_cap_even_when_the_floor_is_larger() {
        // A request of k = 1 against k_min = 2. The memo's `max(k_min, min(cap, C_theta))` form
        // returns 2 here, which exceeds the cap; the clamped form returns 1.
        let p = params(2, 1, Threshold::Cut(1));
        assert_eq!(served_count(0, &p, 1000), 1);
        assert_eq!(served_count(50, &p, 1000), 1);
    }

    #[test]
    fn a_zero_cap_serves_nothing() {
        let p = params(2, 0, Threshold::Saturated);
        assert_eq!(served_count(0, &p, 1000), 0);
        assert_eq!(served_count(900, &p, 1000), 0);
    }

    #[test]
    fn the_serve_all_predicate_holds_on_exactly_the_two_exact_conditions() {
        let sat = params(2, 128, Threshold::Saturated);
        let cut = params(2, 128, Threshold::Cut(1 << 32));

        // Limb A: the floor alone covers the tile, whatever the threshold.
        assert!(serves_all_visible(&cut, 1));
        assert!(serves_all_visible(&cut, 2));
        assert!(!serves_all_visible(&cut, 3));

        // Limb B: saturated and under the cap.
        assert!(serves_all_visible(&sat, 3));
        assert!(serves_all_visible(&sat, 128));
        assert!(
            !serves_all_visible(&sat, 129),
            "saturated but over the cap still needs selection"
        );
    }

    #[test]
    fn served_count_agrees_with_the_fast_path_wherever_it_fires() {
        // The serve-all branch claims to be exact, not conservative. That means: wherever
        // `serves_all_visible` holds, the definition's own `served_count` must equal the visible
        // count for every reachable C_theta — otherwise that branch serves a different set from the
        // definition it claims to evaluate. This is the algebraic half; `tests/selection.rs` checks
        // the same property against real data.
        for &threshold in &[Threshold::Saturated, Threshold::Cut(1 << 40)] {
            for cap in [1usize, 2, 30, 128] {
                for k_min in [0usize, 1, 2, 5] {
                    let p = params(k_min, cap, threshold);
                    for visible in 0..200u64 {
                        if !serves_all_visible(&p, visible) {
                            continue;
                        }
                        // Under Saturated, C_theta == visible by construction. Under a cut, the
                        // only fast-path limb available is the floor one, which holds for any
                        // C_theta at all — so sweep the whole reachable range.
                        let reachable: Vec<u64> = if threshold.is_saturated() {
                            vec![visible]
                        } else {
                            (0..=visible).collect()
                        };
                        for c in reachable {
                            assert_eq!(
                                served_count(c, &p, visible),
                                visible as usize,
                                "fast path is not exact at k_min={k_min} cap={cap} \
                                 visible={visible} c_theta={c} threshold={threshold:?}"
                            );
                        }
                    }
                }
            }
        }
    }
}
