//! Per-user LOD selection (§7.2, design r22) — the definition, and the two ways of evaluating it.
//!
//! **The definition.** For a tile *T* at depth *d*, with `vis(T)` its visible row set ordered
//! ascending by the row's `tessera_id`:
//!
//! ```text
//! cap    = min(request_k, k_max_marks)
//! C_θ(T) = |{ i ∈ vis(T) : tessera_id(i) < P_d }|
//! m(T)   = min(cap, max(min(k_min, cap), C_θ(T)))
//! served(T) = the min(m(T), |vis(T)|) smallest members of vis(T) by tessera_id
//! ```
//!
//! Three clauses: a **floor** of `k_min` (the I7 guarantee — the sparsest principals' maps are
//! never empty), a **threshold** at `P_d` (the density signal: a tile with *n* visible draws
//! `θ_d·n` marks, and tiles are equal screen area, so mark count *is* density), and a **cap** at
//! `cap` (bounds work, wire and overplot).
//!
//! # Nesting holds for a fixed `cap`, and `k` must not decrease on zoom-in
//!
//! The stability property is that an item drawn in a parent tile is still drawn in whichever child
//! contains it, so marks never pop out on zoom-in. It rests on ranks only falling under a subset
//! (`vis(T') ⊆ vis(T)`) and on θ being monotone in depth — **and on `cap` being the same at both
//! depths**. Both surviving clauses need that:
//!
//! - *Threshold clause.* `i ∈ served(T)` gives `rank_{T'}(i) ≤ rank_T(i) ≤ cap`. If the child is
//!   evaluated at a smaller `cap'`, then `m(T') ≤ cap' < rank_{T'}(i)` is reachable and `i` pops.
//! - *Floor clause.* The effective floor is `min(k_min, cap)`, so a smaller `cap'` weakens the
//!   floor as well.
//!
//! `k_max_marks` is a server constant, so `cap = min(request_k, k_max_marks)` varies across depths
//! only through `request_k`. **A client that reduces `k` while zooming in therefore forfeits
//! nesting**; `k` must be non-decreasing on descent. This is a client obligation, stated in
//! contracts §3.2, not something the engine can enforce — it sees one request at a time. It is
//! recorded here because §7.2 keeps the bit-reversal note precisely because this class of mistake
//! gets re-derived.
//!
//! **Why the comparator is the full `tessera_id` and not the `priority` prefix.** `priority` is
//! defined as `high16(tessera_id)` (contracts §2.6 r6), so "k lowest by priority then by
//! `tessera_id`" is *identically* "k lowest by `tessera_id`" — there is no composite comparator to
//! get subtly wrong, and the sample is correct at any prefix width. Design §7.2 (r21) directs that
//! no runtime prefix-scan-then-fall-through path be built in Phase 1, so this module reads the
//! `tessera_id` column directly. The cost of not having that path is recorded rather than hidden:
//! the per-viewport *scanned* column goes from `priority` at 2 B/row to `tessera_id` at 8 B/row, a
//! 4x rise in page traffic. Contracts §2.6 already permits comparing the prefix first as an
//! optimisation; the design's trigger for doing so is `w ≈ log₂(V_max/k)`.
//!
//! **What this module does not do.** No candidate lists, no storage-order-scan route, no route
//! chooser (owner ruling 2026-07-30: those routes rested on an error in reasoning). The two
//! routes here are the definition evaluated directly, and an *exact* fast path for the case where
//! the definition provably serves everything visible.

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
    /// `P_0 = m_target · 2⁶⁴ / v_total`, so that the *mean* occupied tile at any depth draws
    /// `m_target` marks: priorities are uniform over the identity space by construction, so
    /// `P(id < P) = P / 2⁶⁴`, and a tile of *n* visible items serves `n · P_d / 2⁶⁴`.
    ///
    /// **`v_total` must be the COMPOSED visible cardinality** — the mask *after* the overlay diff,
    /// not the cached `RowProjection`'s cardinality. I2 requires every aggregate be computable
    /// from inside `M_auth` alone, and the pre-overlay projection is not: after any accepted
    /// delete or suppression it strictly contains `M_auth`. Anchoring on it would let a viewer
    /// aggregate mark counts across a few hundred tiles, solve for the anchor, difference it
    /// against its own summed per-tile `visible` (which §7.1 discloses exactly), and recover **a
    /// running estimate of how many of its own items have been denied** — a count of items outside
    /// `M_auth`, which no Appendix C row admits. The composed figure costs almost nothing: the
    /// overlay diffs are tiny by construction.
    ///
    /// **The accepted approximation** (§7.2 r22, owner decision 2026-07-30). The `4^d` progression
    /// assumes the viewer's items spread over ~`4^d` occupied tiles. Real corpora cluster, so the
    /// true occupied-cell count `O_d` is smaller and actual marks per tile is
    /// `m_target · 4^d / O_d` — inflated geometrically in depth for a point set of box-counting
    /// dimension below 2. The owner accepted this over both a measured per-session anchor and a
    /// client-supplied θ; memo §9 already accepts cap-flat regions, and the §3.3 underlay
    /// backstops them.
    ///
    /// **The cap-flat region, stated precisely, because the loose version misleads.** It is exactly
    /// the set of tiles with `C_θ >= cap`, which after θ saturates is the set with `V_tile > cap`.
    /// Its lower edge is depth 0, where the inflation is exactly 1 whatever the clustering
    /// (`O_0 = 4^0 = 1`). Its upper edge is **not** the saturation depth: after saturation
    /// `served = min(cap, V_tile)` — today's flat-`k` behaviour — which stays flat at every depth
    /// where occupied cells still hold more than `cap` items. Worked: 10^6 visible with
    /// `m_target = 16` saturates at `d >= 8`, but clustered into 6,400 cells at `d = 8` gives
    /// `V_tile ~= 156 > 128`, still pinned. The true upper edge is the depth at which the *largest*
    /// occupied cell falls below `cap`. Bounded either way — but a reader told "θ has saturated"
    /// would wrongly conclude flatness vanishes at fine zoom.
    ///
    /// Note what θ does *not* control: a cell with *n* visible draws `θ·n` marks, exactly
    /// proportional to density, and proportionality holds while `min(k_min, cap) <= θ·n <= cap` — a
    /// density ratio of `cap/k_min`, **independent of θ**. θ positions that window on the density
    /// axis; the floor and cap set its width.
    ///
    /// **That width is `min(request_k, k_max_marks)/k_min`, not `k_max_marks/k_min`.** Contracts
    /// §3.2 defaults `k` to 30, so the *default* request realises a window of 15 (~1.2 decades);
    /// density memo §4's figure of 64 is reached only by a client that explicitly asks for
    /// `k >= k_max_marks`. This narrowing is independent of the occupancy deficit above.
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
/// **θ's anchor must be a whole-slice total, never per-segment or per-partition.** Phase 1 fails
/// closed on a slice with more than one segment (`EngineError::MultiSegmentSlice`), so this is spec
/// ahead of code — but the trap is worth naming now, because the fix is not local. Where a tile
/// spans several segments the definition applies to the *union* of their visible sets: sum `C_θ`
/// across segments, then serve the global bottom-`m` of the union (each segment need only offer its
/// own bottom-`cap` for that merge to be exact). That is only well-formed while `P_d` is a single
/// predicate over the union — so a per-segment anchor would make "below the cut" mean different
/// things in different segments and the merge would stop computing the definition. The same holds
/// across partitions (§12.3).
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

/// Which route a tile takes, decided from quantities already in hand.
///
/// Recorded as a **C4 widening** in Appendix C: this is a per-tile branch keyed on the viewer's own
/// `V` and θ, so branch selection correlates with the principal's own coverage. C14's reasoning
/// already accepts that shape as benign; the point is that it is written down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// The definition provably serves every visible row in the tile — emit them all, run no
    /// counting pass and no selection.
    AllVisible,
    /// Evaluate the definition directly: one pass over the tile's visible rows.
    Direct,
}

/// Choose the route for a tile with `visible` visible rows.
///
/// **Both fast-path conditions are exact, not conservative.** Serving all of `vis(T)` is correct
/// iff `m(T) >= |vis(T)|`. `C_θ <= V` and is unknowable without reading the column, so exactly two
/// conditions discharge it from quantities already in hand:
///
/// - `V <= min(k_min, cap)` — the floor alone covers the tile, so `m >= min(cap, k_min) >= V`.
/// - `Saturated ∧ V <= cap` — θ ≥ 1 means `C_θ = V` *by construction*, so
///   `m >= min(cap, V) = V`. This is why [`Threshold::Saturated`] is an enum variant rather than
///   `Cut(u64::MAX)`: the latter would exclude `id == u64::MAX` and break the equivalence at the
///   boundary.
///
/// **The selection-route plan's original condition was `V <= k`, and that is unsound here.** Under
/// the density rule the threshold clause deliberately serves *fewer* than `V` — a tile with
/// `V = 100` and `C_θ = 5` serves 5, and serving 100 would destroy the density signal that is the
/// entire point of the rule. So the fast path survives only in the two forms above.
///
/// **Where the win actually comes from.** θ saturates at `d >= log₄(V_total / m_target)`: depth ~2
/// for a viewer with 10² visible, ~7 at 10⁵, ~13 at 10⁹. So for tail principals most of the zoom
/// range takes `AllVisible` — the case probes/results.md measured as dominant. The drawn-mark
/// budget's framing that "at k=10⁷ … V ≤ k across the board" does **not** survive the density
/// rule, because that rule exists to draw fewer than `V`; the win arrives through saturation
/// instead.
pub fn route_for(params: &SelectParams, visible: u64) -> Route {
    if visible <= params.floor() as u64 {
        return Route::AllVisible;
    }
    if params.threshold.is_saturated() && visible <= params.cap as u64 {
        return Route::AllVisible;
    }
    Route::Direct
}

/// One tile's selected rows, ascending by `tessera_id`, together with the route taken.
pub struct Selection {
    /// Row indices, **ascending by the row's `tessera_id`** — not by row index.
    pub rows: Vec<u32>,
    pub route: Route,
}

impl Selection {
    /// Evaluate §7.2's definition over `range` under `mask`.
    ///
    /// Points are ordered by `tessera_id` on **both** routes. Two reasons, and neither is
    /// cosmetic: the nesting argument's client-truncation clause requires that a client truncating
    /// to its own budget is truncating a *prefix*, and a route-dependent payload order would be a
    /// differential-oracle landmine — the two routes must be indistinguishable from outside.
    pub fn of(
        mask: &EffectiveMask,
        segment: &SegmentData,
        range: Range<u32>,
        params: &SelectParams,
        visible: u64,
    ) -> Self {
        Self::via(
            mask,
            segment,
            range,
            params,
            visible,
            route_for(params, visible),
        )
    }

    /// Evaluate the definition over an explicitly chosen `route`.
    ///
    /// The route is a parameter rather than an internal decision so that a test can force
    /// [`Route::Direct`] on a tile the fast path would have claimed, and **compare** the two
    /// outputs. That comparison is the only thing that actually establishes the fast path's
    /// exactness — a test that re-derived the fast path's own reasoning would prove nothing. Both
    /// routes must be indistinguishable from outside, point order included.
    pub fn via(
        mask: &EffectiveMask,
        segment: &SegmentData,
        range: Range<u32>,
        params: &SelectParams,
        visible: u64,
        route: Route,
    ) -> Self {
        // A request that asks for no points still wants counts (the `k = 0` count-only arm the
        // benches measure). Without this, `Route::Direct` would run a full counting pass whose
        // result is discarded, and the count-only benchmark would silently stop measuring counting.
        if params.cap == 0 {
            return Selection {
                rows: Vec::new(),
                route,
            };
        }

        let ids = segment.columns.tessera_id();

        let rows = match route {
            Route::AllVisible => {
                let mut rows: Vec<u32> = mask.rows_in_range(range).iter().collect();
                rows.sort_unstable_by_key(|&row| ids[row as usize]);
                rows
            }
            Route::Direct => {
                // One pass. `m(T) <= cap` always, so the `cap` smallest ids in the tile contain the
                // served set for *any* m the counting pass can produce — which is what makes a
                // single pass sufficient.
                //
                // A `BinaryHeap` is a max-heap, which is what is wanted: the largest of the `cap`
                // best-so-far sits at the root, so it is both the eviction candidate and the
                // rejection threshold.
                //
                // **The peek-reject is not a micro-optimisation.** Without it, every visible row is
                // pushed and sifted before being thrown away, which is O(V log cap) sift work
                // against O(V) compares — measured at 19x the irreducible counting cost, ~36 ms for
                // 300 tiles of 4,000 visible rows against a 10 ms p99 budget, and worsening with V
                // because the reject rate rises. With it, a row past the current cut costs one
                // compare. Output is identical either way: a row not smaller than the largest of
                // the `cap` smallest cannot be among them.
                //
                // Memory: O(min(cap, V)) for the heap, plus `rows_in_range`'s bitmap, which is
                // O(containers touched) rather than O(V) — see its doc for why that distinction
                // used to be the other way round and what it cost.
                let mut c_theta: u64 = 0;
                let heap_cap = params.cap.min(visible as usize).saturating_add(1);
                let mut heap: BinaryHeap<(u64, u32)> = BinaryHeap::with_capacity(heap_cap);
                let visible_rows = mask.rows_in_range(range);
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
            }
        };

        debug_assert!(
            rows.windows(2)
                .all(|w| ids[w[0] as usize] != ids[w[1] as usize]),
            "two rows in one tile share a tessera_id — the identity is a bijection over 2^64 with \
             one row per entity (contracts §2.6), and the determinism of the whole selection rests \
             on that being true"
        );
        Selection { rows, route }
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
    fn the_fast_path_fires_on_exactly_the_two_exact_conditions() {
        let sat = params(2, 128, Threshold::Saturated);
        let cut = params(2, 128, Threshold::Cut(1 << 32));

        // Limb A: the floor alone covers the tile, whatever the threshold.
        assert_eq!(route_for(&cut, 1), Route::AllVisible);
        assert_eq!(route_for(&cut, 2), Route::AllVisible);
        assert_eq!(route_for(&cut, 3), Route::Direct);

        // Limb B: saturated and under the cap.
        assert_eq!(route_for(&sat, 3), Route::AllVisible);
        assert_eq!(route_for(&sat, 128), Route::AllVisible);
        assert_eq!(
            route_for(&sat, 129),
            Route::Direct,
            "saturated but over the cap still needs selection"
        );
    }

    #[test]
    fn served_count_agrees_with_the_fast_path_wherever_it_fires() {
        // The fast path claims to be exact, not conservative. That means: wherever `route_for`
        // says AllVisible, the definition's own `served_count` must equal the visible count for
        // every reachable C_theta — otherwise the fast path serves a different set from the
        // definition it claims to evaluate.
        for &threshold in &[Threshold::Saturated, Threshold::Cut(1 << 40)] {
            for cap in [1usize, 2, 30, 128] {
                for k_min in [0usize, 1, 2, 5] {
                    let p = params(k_min, cap, threshold);
                    for visible in 0..200u64 {
                        if route_for(&p, visible) != Route::AllVisible {
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
