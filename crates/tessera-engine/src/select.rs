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
//!   prefix-scan-then-fall-through path exists (§7.2 r21 directs that none be built).
//!   The two orders are identical because `priority` is a prefix, so this costs no correctness —
//!   only 8 B/row of scanned column where 2 B would do. Design Appendix A records that cost and
//!   §7.2 the trigger for revisiting it.
//!
//! ## Why there is no candidate-list route
//!
//! **NO CANDIDATE-LIST ROUTE.** Everything below evaluates the definition *directly* from the
//! mask, at every coverage. §7.2 specifies a second route — per-node precomputed lists of the
//! top `c·k` items by `tessera_id`, unmasked, filtered at query time — and the owner has
//! **declined** it (`docs/decisions/0008-candidate-list-route-declined.md`). This block records
//! why, with the evidence,
//! because a claim without its evidence gets re-litigated and the deletion of the direct path
//! is the specific mistake that has been made before, by others, in production.
//!
//! `scripts/check-layers.sh` fails if the marker above disappears. A comment CI cannot notice
//! being deleted is a comment that will be deleted.
//!
//! **1. The published analogue fails exactly here.** Tippecanoe's `--retain-points-multiplier`
//! (2.41.0, in production at Felt) is the only shipped system that samples after filtering with
//! cross-zoom stability. It over-retains N× per tile into *multiplier clusters* and serves the
//! first surviving feature from each — a fixed-width structure with no route back once a cluster
//! exhausts, so tiles go **empty** below a pass rate of roughly 1/N (prior-art synthesis §6,
//! prior-art 2 §"Addendum"). Candidate lists have the identical shape: a list of width `c·k`
//! yields about `c·k·coverage` survivors, so it produces `k` of them only above coverage `1/c`.
//! At `c = 4` that is **25% coverage**, which — 10⁴ grants against 10⁵–10⁶ categories — describes
//! almost no realistic principal (§7.2). Widening is linear in both help and cost: 1% needs
//! `c = 100`, comparable in size to the hot columns.
//!
//! **2. Realistic masks live on the wrong side of the crossover, measured.** Measurement over the
//! synthetic 10⁹ corpus (`probes/results.md` §5 — **re-read it rather than trusting these
//! numbers second-hand**) puts the run ratio of surnames, the most realistic principal shape
//! available, at **1.03–1.15** against a flat-hash control of exactly 1.00: masks are essentially
//! scattered under Morton order, so there is no spatial clustering for a per-node list to exploit.
//! The duty cycle follows (§7.2's r18 paragraph): at working coverages **12–99%** of occupied
//! depth-6 tiles fall below the ~5% crossover, and for tail-only principals essentially all do —
//! `probes/results.md` §5 measures 98.8% for a random `w=100` grant set at 0.13% coverage and
//! 63.3% for surnames at 4.6%. **The honest other end:** at surnames head-25% only 1.8% of tiles
//! fall below the crossover. That is the regime candidate lists would serve, and it is the dense
//! core of a head principal — which is precisely the scope §7.2 assigns them ("an optimisation
//! for high-coverage principals, not the general path") and precisely what a route built for the
//! *general* case cannot be sized from.
//!
//! **3. Below the crossover the direct route is also the faster one.** Descent multiplies work
//! rather than dividing it — merging four children's lists yields four times the candidates, so
//! reaching `k` survivors from `d` levels down visits the geometric sum of `4^d` nodes, work
//! proportional to **1/coverage, not log(1/coverage)** (a conflation corrected in prior-art 2 on
//! 2026-07-27, having first been published the wrong way round). At `c = 4`, `k = 30`, 10⁴-row
//! tiles: ~21 nodes at 5% coverage against ~10 pages for direct evaluation, 85 nodes at 1%, and
//! **5,461 nodes at 0.01% against a single page**. Direct evaluation is bounded by the tile's
//! row range in the scanned identity column and gets *cheaper* as coverage falls, because there
//! are fewer visible items to consider. So it is the **main route by measurement, not a
//! fallback** — the framing I7 and §7.2 r18 both insist on, and the one this module implements.
//!
//! **4. Deleting the direct path to "simplify" is the failure mode, not a tidy-up.** It would
//! reintroduce tippecanoe's empty-tile cliff silently — no error, no metric, just blank map
//! regions — and it would do so for the **sparsest** principals: the users with the least
//! coverage, the least context to recognise a wrong map, and the least standing to report one.
//! That is the I7 guarantee inverted. The floor clause (`k_min`) is what keeps their maps from
//! going blank and may not be removed as an optimisation either (§7.2).
//!
//! *Not foreclosed:* the **single-cell-tile fast path** (hot-path memo §6, B1's surviving
//! residual) is a fourth decode tier over this same direct route, not a candidate list — it
//! computes the identical served set from the identical mask. It sits on the perf ledger and is
//! deliberately outside the stage-2.1 plan.

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
    /// θ_d as a cut point: `P_d = ⌊m_target · N_occ(d) · 2⁶⁴ / V_total⌋`, saturating at θ_d ≥ 1.
    ///
    /// `n_occ` is `N_occ(d)`, the number of depth-*d* tiles holding at least one row this session
    /// can see ([`crate::occupancy::occupied_tiles`]). Priorities are uniform over the identity
    /// space, so `P(id < P_d) = P_d / 2⁶⁴` and a tile of *n* visible items serves `n · θ_d` marks.
    /// Summed over the occupied tiles that is `V_total · θ_d = m_target · N_occ(d)`, so the mean
    /// occupied tile draws `m_target` marks at every depth.
    ///
    /// **Both factors must come from the COMPOSED mask** — after the overlay diff, not from the
    /// cached `RowProjection`. This is an I2 requirement, not a preference: the pre-overlay
    /// projection strictly contains `M_auth` after any accepted delete or suppression, so anchoring
    /// on it would let a viewer aggregate mark counts across tiles, solve for θ, difference it
    /// against its own summed per-tile `visible` (which §7.1 discloses exactly), and recover **a
    /// running estimate of how many of its own items have been denied**. See
    /// [`EffectiveMask::visible_total`] for `V_total` and [`crate::occupancy`] for `N_occ`; the
    /// property is pinned by `the_theta_anchor_falls_when_an_item_is_suppressed` and
    /// `n_occ_falls_when_a_suppression_empties_a_tile`.
    ///
    /// **θ is monotone in depth without a clamp.** Every occupied depth-*d* tile has at least one
    /// occupied child and children of distinct parents are distinct, so `N_occ` is non-decreasing
    /// in depth and so is θ. §7.2's nesting proof needs that and gets it from the structure of the
    /// grid; a running maximum over depth would hide a counting bug rather than prevent one, and
    /// there is none here.
    ///
    /// **The saturation test comes before the shift, and that is what replaces the old
    /// `leading_zeros` guard.** θ_d ≥ 1 exactly when `m_target · N_occ >= V_total`, and answering
    /// that first bounds `numer` below `V_total < 2³²` — so `numer << 64` is under 2⁹⁶ and cannot
    /// lose a bit. The residual case uses `checked_mul` and not `checked_shl`: `checked_shl`
    /// returns `None` only for a shift at or past the type's width and *wraps* below it, which is
    /// how the depth-shift form produced `Cut(0)` — `C_θ = 0` in every tile at every depth, every
    /// tile drawing `k_min` for ever, with no error raised anywhere. Overflow here answers
    /// `Saturated`, which is the correct answer for a θ that large.
    pub fn at_depth(v_total: u64, m_target: u64, n_occ: u64) -> Self {
        if v_total == 0 {
            // No visible rows anywhere: every tile is empty and skipped. Saturated is the
            // harmless answer, and avoids a division by zero.
            return Threshold::Saturated;
        }
        let numer = (m_target as u128) * (n_occ as u128);
        if numer >= v_total as u128 {
            // θ_d ≥ 1 — this session's expected marks reach everything it can see.
            return Threshold::Saturated;
        }
        // u128 is required, not defensive: `numer << 64` does not fit in a u64 at all.
        match numer.checked_mul(1u128 << 64) {
            Some(scaled) => Threshold::Cut((scaled / v_total as u128) as u64),
            None => Threshold::Saturated,
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
/// **θ's anchor must be a whole-view total, never per-segment or per-partition** — a local anchor
/// makes "below the cut" mean different things in different segments, and the merge stops computing
/// the definition. A view spanning partitions fails closed today (`MultiPartitionView`); a
/// view spanning *segments* is now the ordinary case, flush appending one per tick;
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
    /// masked count over the tile, and the direct-evaluation route implemented here reads every
    /// visible row to obtain it.
    ///
    /// That is a property of *this route*, not of the definition, and the distinction is worth
    /// keeping straight because the opposite claim is easy to make and wrong. Exact sub-Σvisible
    /// evaluations exist: storage order is `(morton, tessera_id)`, so within a single leaf Morton
    /// cell the identity column is **sorted** — one binary search finds where ids reach `P_d`, and
    /// `C_θ` for that cell is a range cardinality over the mask, which is O(containers touched)
    /// rather than O(rows). A coarser tile is a merge of `4^(16-d)` such runs, so the trick pays
    /// where the runs are few or the tile is dense, and the scan wins where they are many. None of
    /// it is built, the obviously-correct single pass being preferred until the trigger §7.2 records
    /// is met.
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

/// The decode mechanism serving one tile, chosen from `(visible, range.len())` — two quantities
/// already in hand — never from the data itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeTier {
    /// `visible == range.len()`: the whole range is visible, so the visible set *is* the range —
    /// one contiguous id slice, no bitmap decode of any kind.
    FullRange,
    /// Density at or above [`RUN_DECODE_MIN_DENSITY_PCT`]: contiguous-run decode
    /// ([`EffectiveMask::for_each_visible_run`]).
    Runs,
    /// Everything else: batched value decode over [`EffectiveMask::decode_source`], with the
    /// scan loop owned by [`Selection::of`] — run-length-indifferent, so its cost is flat where
    /// the run tier's collapses.
    Values,
}

/// How many values one `next_many` read decodes in the value tier: 1024 × 4 B is a 4 KiB stack
/// buffer, enough to make the FFI crossing invisible and small enough to stay cache-resident
/// while the id column streams past it.
const VALUE_BUF_LEN: usize = 1024;

/// The minimum tile density (`visible / range.len()`, in percent) at which run decoding beats
/// the batched value decode.
///
/// **Measured, not guessed — and the measurement is re-runnable** (`examples/decode_tiers.rs`,
/// general-branch work, 1M rows, half-admitting cut, ns per visible row, at the deployment
/// operating point **cap = 500** — owner directive 2026-07-30; k defaults to the cap). Run
/// decode's cost is per *run*, so it collapses as runs lengthen and drowns as they shrink — a
/// Bernoulli mask at density d has mean run length 1/(1−d). Against the batch decode at cap 500
/// it wins ~19% at density 1.0 (5.64 vs 6.91) and ~7% at 0.95 (6.67 vs 7.14), loses ~7% at 0.90
/// (7.85 vs 7.32) and ~20% at 0.85 (9.01 vs 7.52), and is ~64% behind by 0.30 (20.8 vs 12.7);
/// the secondary cap-50 sweep puts the crossover in the same interval, so the constant is not
/// cap-sensitive in the measured band. The crossover sits between 90 and 95; 95 is the
/// conservative rounding — near the
/// boundary the gate prefers the batch decode, whose cost is flat in run length, over the run
/// decode, whose failure mode on scattered masks was a measured +7.8% end-to-end regression at
/// 2.4M (the regression that forced this gate to exist). Deleting the gate re-creates that
/// regression on every scattered mask; moving the constant without re-running the example is
/// guesswork.
pub const RUN_DECODE_MIN_DENSITY_PCT: u64 = 95;

/// The tier gate. Public so the equivalence tests stratify their corpora with the *same*
/// predicate the selection uses, rather than a transcription that could drift.
///
/// Both inputs are already disclosed per tile (§7.1 discloses `visible`; the tile grid discloses
/// `range.len()`), so the tier — though observable in timing — is a function of quantities the
/// viewer already has. (C19 register note pending owner sign-off; recorded at landing, not here.)
pub fn decode_tier(visible: u64, range_len: u64) -> DecodeTier {
    if visible == range_len {
        DecodeTier::FullRange
    } else if visible * 100 >= range_len * RUN_DECODE_MIN_DENSITY_PCT {
        DecodeTier::Runs
    } else {
        DecodeTier::Values
    }
}

/// One segment's contribution to one tile: the segment, its own row range for that tile, and
/// where its rows begin in the view's row space.
///
/// **Why a tile is a list of these rather than one range.** `tile_ranges` searches a *segment's*
/// Morton column and returns indices local to it, while the mask is a bitmap over the whole
/// **view** row space — the base segment at 0, each flush or merge segment at its extent's
/// `row_base`. A view holding more than one segment therefore resolves each one separately and
/// the tile is their union. Every mask operation below takes a view-space range
/// (`row_base + local`); every identity-column and gather read takes the local one.
#[derive(Debug, Clone)]
pub struct SelectionPart<'a> {
    pub segment: &'a SegmentData,
    /// Segment-local, as [`tessera_store::tile_ranges`] returns it.
    pub range: Range<u32>,
    /// This segment's `row_base` in the view's row space; 0 for the build segment.
    pub row_base: u32,
    /// `mask.count_range` over this part's **view-space** range. Supplied by the caller because
    /// it is already needed to decide whether the tile is empty at all.
    pub visible: u64,
}

impl<'a> SelectionPart<'a> {
    /// The single-segment case: a view whose only segment is the build one, whose rows therefore
    /// begin at 0, so segment-local and view-space rows coincide. What a bundle straight out of
    /// `tessera build` presents, and what the equivalence tests and the route-saving example
    /// construct.
    pub fn base(segment: &'a SegmentData, range: Range<u32>, visible: u64) -> Self {
        SelectionPart {
            segment,
            range,
            row_base: 0,
            visible,
        }
    }

    /// This part's range in view row space — what every mask operation takes.
    fn view_range(&self) -> Range<u32> {
        self.row_base + self.range.start..self.row_base + self.range.end
    }

    fn is_empty(&self) -> bool {
        self.range.start >= self.range.end
    }
}

/// The parts of one tile, with the view-space↔segment-local resolution they define.
///
/// **The union is a genuine union, not a concatenation, and that is §7.2's requirement rather than
/// a convenience.** `cap` and `k_min` are per *tile*: a tile spanning three segments has one `k`
/// budget to spend across all three, one `C_θ` counted over all three, and one served set that is
/// the `m` smallest `tessera_id`s in the union. Selecting per segment and concatenating would
/// serve up to `parts × cap` marks and would break I7's floor in the other direction too — a tile
/// with one visible row in each of three segments would draw `3·k_min`, not `k_min`.
pub struct SelectionParts<'a> {
    parts: &'a [SelectionPart<'a>],
}

impl<'a> SelectionParts<'a> {
    pub fn new(parts: &'a [SelectionPart<'a>]) -> Self {
        SelectionParts { parts }
    }

    /// The parts themselves, for the callers that need to do their own per-segment work over the
    /// same decomposition — the §3.3 underlay, which resolves sub-cells inside each part's range.
    pub fn as_slice(&self) -> &'a [SelectionPart<'a>] {
        self.parts
    }

    /// The part owning `view_row`, and that row's segment-local index.
    ///
    /// A reverse linear scan rather than a binary search: the part list is the view's live
    /// segment count, which the merge policy bounds to a handful, and at that size the scan is
    /// both faster and obviously correct. Parts are ascending in `row_base` (row space is built by
    /// appending extents), so the first part starting at or below `view_row` owns it.
    pub fn resolve(&self, view_row: u32) -> (&'a SegmentData, u32) {
        let (_, segment, local) = self.resolve_indexed(view_row);
        (segment, local)
    }

    /// [`Self::resolve`], plus **which** part answered.
    ///
    /// The index is what lets a caller key per-segment work it has hoisted out of its row loop —
    /// the gather resolves each declared column's [`tessera_store::read::ScalarSlice`] once per
    /// part rather than once per row, and needs somewhere to look the resolved set up. Returning
    /// the index rather than having the caller match on the segment pointer keeps that lookup an
    /// array index, and keeps `resolve`'s own contract unchanged for everyone else.
    pub fn resolve_indexed(&self, view_row: u32) -> (usize, &'a SegmentData, u32) {
        for (i, part) in self.parts.iter().enumerate().rev() {
            if view_row >= part.row_base {
                return (i, part.segment, view_row - part.row_base);
            }
        }
        // Unreachable for a row this module itself produced: every such row came from a part's own
        // view range, and the first part's `row_base` is 0. A panic rather than a fallback,
        // because a wrong answer here gathers one entity's coordinates under another's identity.
        panic!("view row {view_row} lies below every part's row_base — not a row of this tile");
    }

    fn id_at(&self, view_row: u32) -> u64 {
        let (segment, local) = self.resolve(view_row);
        segment.columns.tessera_id()[local as usize]
    }
}

/// One tile's selected rows, ascending by `tessera_id`.
pub struct Selection {
    /// Row indices in **view row space**, **ascending by the row's `tessera_id`** — not by row
    /// index. Resolve each to its segment with [`SelectionParts::resolve`] before gathering.
    pub rows: Vec<u32>,
    /// How many rows this call actually read, counted **inside** the loops that read them.
    ///
    /// Counted here rather than inferred by the caller, and that distinction is the whole value of
    /// the field. An earlier version had the caller increment its stage counter from the tile's
    /// `visible` count instead — which made the resulting `visited == sigma_visible` assertion a
    /// tautology, since both sides came from the same variable and no behaviour of this function
    /// fed either. A counter derived from the thing it is supposed to be watching watches nothing.
    pub rows_visited: u64,
}

impl Selection {
    /// Evaluate §7.2's definition over `range` under `mask`.
    ///
    /// Rows come back ascending by `tessera_id` whichever branch runs — the nesting argument's
    /// client-truncation clause needs the payload to be a *prefix*, and a branch-dependent order
    /// would be a differential-oracle landmine.
    ///
    /// `visible` must be `Σ part.visible` exactly, and each `part.visible` must be
    /// `mask.count_range(part.view_range())`. It always carried correctness (the serve-all
    /// predicate and the `m` clamp read it); the [`DecodeTier::FullRange`] tier also decodes by it
    /// — `visible == range.len()` is taken as proof that the whole range is visible, which is only
    /// true of the *composed* count.
    ///
    /// **The tier is chosen per part, the definition is evaluated over the union.** Density is a
    /// property of how the mask sits over one contiguous range, so a view whose base segment is
    /// dense and whose fresh flush segment is sparse should decode each the way that segment's own
    /// density warrants — but `C_θ`, the heap, the cap and the floor are all single, tile-wide
    /// quantities. A single-part tile takes exactly the path it took before segments could be
    /// unioned, which is what keeps the existing byte-equality claims intact.
    pub fn of(
        mask: &EffectiveMask,
        parts: &SelectionParts<'_>,
        params: &SelectParams,
        visible: u64,
    ) -> Self {
        // A request that asks for no points still wants counts (the `k = 0` count-only arm the
        // benches measure). Without this the general branch would run a full counting pass whose
        // result is discarded, and the count-only benchmark would stop measuring counting.
        if params.cap == 0 {
            return Selection {
                rows: Vec::new(),
                rows_visited: 0,
            };
        }
        // The decode mechanism is chosen per part (`decode_tier`): the measured per-row cost of
        // the retired always-per-value code was dominated by decode machinery, not column loads
        // (memo `2026-07-30-viewport-hot-path-and-bundle-size-review.md` §B9), but run decoding
        // only wins where runs are long — see `RUN_DECODE_MIN_DENSITY_PCT` for the measured
        // crossover. Every tier reads the same rows in the same ascending order, so the output
        // is bit-identical whichever fires.
        //
        // An empty (or inverted) part holds nothing — decoded identically by every mechanism, but
        // the FullRange tier's slice indexing needs the well-formedness guarantee explicit, so
        // every loop below skips one rather than relying on the ranges being well-behaved.
        let mut rows_visited: u64 = 0;
        let rows: Vec<u32> = if serves_all_visible(params, visible) {
            // Everything visible is served, so there is nothing to count and nothing to select —
            // only ordering.
            //
            // **`sort_unstable_by_key`, and a decorate-sort was tried and is worse.** The key
            // extraction re-reads the identity column on every comparison, so this is V log V
            // strided lookups where V would do — which is why the decorate-sort (build
            // `Vec<(u64, u32)>`, sort it, discard the keys) looks like an obvious win. Measured, it
            // loses by ~3% at cap 500 and 1000 and by ~6% at cap 30
            // (`examples/route_saving.rs`): sorting 12-byte tuples moves three times the bytes of
            // sorting 4-byte row ids, and the extra allocation and the undecorate pass cost more
            // than the saved lookups — the ids being read are in cache for the sizes the serve-all
            // branch handles (V <= cap). Recorded because the reasoning for the other choice is
            // more persuasive than the measurement, and someone will make it again.
            // The counter takes what each tier actually reads — clamped run lengths, or one per
            // decoded value — which is the field's meaning. Capacity is exact — this branch runs
            // only when `visible <= cap`.
            let mut rows: Vec<u32> = Vec::with_capacity(params.cap.min(visible as usize));
            for part in parts.parts {
                if part.is_empty() {
                    continue;
                }
                let view_range = part.view_range();
                let range_len = u64::from(view_range.end - view_range.start);
                match decode_tier(part.visible, range_len) {
                    DecodeTier::FullRange => {
                        rows_visited += range_len;
                        rows.extend(view_range.clone());
                    }
                    DecodeTier::Runs => mask.for_each_visible_run(view_range.clone(), |run| {
                        rows_visited += u64::from(run.end - run.start);
                        rows.extend(run);
                    }),
                    DecodeTier::Values => {
                        // Reads are bounded by this part's `visible`, not just by the range-end
                        // check: after the seek, the next `visible` values of the source are
                        // exactly the part's visible set (diffs empty ⇒ source is the composed
                        // mask; diffs present ⇒ the source is already range-clamped), so an
                        // unbounded final `next_many` would decode up to a buffer's worth of rows
                        // past the part and throw them away — measured at ~15% of the whole
                        // request on ~500-visible tiles, since the waste is per tile. The
                        // `>= view_range.end` check stays as the fail-safe for a caller-
                        // miscounted `visible`.
                        let source = mask.decode_source(view_range.clone());
                        let mut iter = source.bitmap().iter();
                        iter.reset_at_or_after(view_range.start);
                        let mut buf = [0u32; VALUE_BUF_LEN];
                        let mut remaining = part.visible;
                        'decode: while remaining > 0 {
                            let want = remaining.min(VALUE_BUF_LEN as u64) as usize;
                            let n = iter.next_many(&mut buf[..want]);
                            if n == 0 {
                                break;
                            }
                            for &row in &buf[..n] {
                                if row >= view_range.end {
                                    break 'decode;
                                }
                                rows_visited += 1;
                                rows.push(row);
                            }
                            remaining -= n as u64;
                        }
                    }
                }
            }
            // One sort over the union, not one per part followed by a merge: the payload must be
            // a single `tessera_id` prefix across the whole tile (the nesting argument's
            // client-truncation clause), and a per-part sort would only be a prefix within each
            // segment.
            rows.sort_unstable_by_key(|&row| parts.id_at(row));
            rows
        } else {
            // One pass. `m(T) <= cap` always, so the `cap` smallest ids in the tile contain the
            // served set for *any* m the counting pass can produce — which is what makes a single
            // pass sufficient.
            //
            // In the slice-fed tiers the threshold count goes first, over the contiguous id
            // view — a branchless filter-count the compiler vectorises — and the heap feed
            // second; the value-fed tier interleaves them per row as the retired code did. The
            // split changes nothing observable because `c_theta` and the heap never read each
            // other.
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
            // Memory: O(min(cap, V)) for the heap. The steady-state decode route (diffs empty)
            // materialises nothing at all; with diffs present the run and value tiers pay one
            // temporary `rows_in_range` bitmap, O(containers touched) rather than O(V) — see its
            // doc for what that used to cost.
            let mut c_theta: u64 = 0;
            let heap_cap = params.cap.min(visible as usize).saturating_add(1);
            let mut heap: BinaryHeap<(u64, u32)> = BinaryHeap::with_capacity(heap_cap);

            // The slice consumer, shared by the two contiguous tiers — one transcription, so the
            // tiers cannot disagree about what a row means. A nested fn rather than a closure:
            // the state is passed explicitly, which keeps the value tier free to drive its own
            // loop over the same variables below.
            fn scan_slice(
                run_start: u32,
                slice: &[u64],
                params: &SelectParams,
                rows_visited: &mut u64,
                c_theta: &mut u64,
                heap: &mut BinaryHeap<(u64, u32)>,
            ) {
                *rows_visited += slice.len() as u64;
                match params.threshold {
                    Threshold::Saturated => *c_theta += slice.len() as u64,
                    Threshold::Cut(cut) => {
                        *c_theta += slice.iter().filter(|&&id| id < cut).count() as u64;
                    }
                }
                for (i, &id) in slice.iter().enumerate() {
                    if heap.len() == params.cap {
                        // Safe: len == cap >= 1 here, since cap == 0 returned early in `of`.
                        if id >= heap.peek().expect("non-empty at len == cap").0 {
                            continue;
                        }
                        heap.pop();
                    }
                    heap.push((id, run_start + i as u32));
                }
            }

            // **One `c_theta`, one heap, across every part.** This is where §7.2's "per tile, not
            // per segment" actually lands: the counting pass accumulates over the union, and the
            // heap's `cap` is the tile's whole budget, so the `cap` smallest ids in the *union*
            // are what survive. Selecting per part and concatenating would serve up to
            // `parts × cap` marks and would inflate the I7 floor by the same factor.
            for part in parts.parts {
                if part.is_empty() {
                    continue;
                }
                let ids = part.segment.columns.tessera_id();
                let base = part.row_base;
                let view_range = part.view_range();
                let range_len = u64::from(view_range.end - view_range.start);
                match decode_tier(part.visible, range_len) {
                    DecodeTier::FullRange => {
                        scan_slice(
                            view_range.start,
                            &ids[part.range.start as usize..part.range.end as usize],
                            params,
                            &mut rows_visited,
                            &mut c_theta,
                            &mut heap,
                        );
                    }
                    DecodeTier::Runs => mask.for_each_visible_run(view_range.clone(), |run| {
                        // `run` is in view space; the identity column is indexed segment-locally.
                        scan_slice(
                            run.start,
                            &ids[(run.start - base) as usize..(run.end - base) as usize],
                            params,
                            &mut rows_visited,
                            &mut c_theta,
                            &mut heap,
                        );
                    }),
                    DecodeTier::Values => {
                        // The loop is driven here rather than fed through a closure, and that is a
                        // measured decision, not style: routing this per-value state through a
                        // closure environment cost ~2× the whole scan (`examples/decode_tiers.rs`,
                        // its doc records the history).
                        // Bounded by this part's `visible` for the same reason as the serve-all
                        // arm above: the final unbounded read would decode a buffer's worth of
                        // rows past the part.
                        let source = mask.decode_source(view_range.clone());
                        let mut iter = source.bitmap().iter();
                        iter.reset_at_or_after(view_range.start);
                        let mut buf = [0u32; VALUE_BUF_LEN];
                        let mut remaining = part.visible;
                        'decode: while remaining > 0 {
                            let want = remaining.min(VALUE_BUF_LEN as u64) as usize;
                            let n = iter.next_many(&mut buf[..want]);
                            if n == 0 {
                                break;
                            }
                            for &row in &buf[..n] {
                                if row >= view_range.end {
                                    break 'decode;
                                }
                                rows_visited += 1;
                                let id = ids[(row - base) as usize];
                                if params.threshold.admits(id) {
                                    c_theta += 1;
                                }
                                if heap.len() == params.cap {
                                    // Safe: len == cap >= 1 here, since cap == 0 returned early
                                    // above.
                                    if id >= heap.peek().expect("non-empty at len == cap").0 {
                                        continue;
                                    }
                                    heap.pop();
                                }
                                heap.push((id, row));
                            }
                            remaining -= n as u64;
                        }
                    }
                }
            }

            let m = served_count(c_theta, params, visible);
            let mut kept: Vec<(u64, u32)> = heap.into_vec();
            kept.sort_unstable();
            kept.truncate(m);
            kept.into_iter().map(|(_, row)| row).collect()
        };

        // Across the union, not just within one segment — which is the stronger claim, and the one
        // that matters now that a tile can draw rows from several segments at once. Two segments
        // holding a row for the same entity would alias here rather than anywhere louder.
        debug_assert!(
            rows.windows(2)
                .all(|w| parts.id_at(w[0]) != parts.id_at(w[1])),
            "two rows in one tile share a tessera_id — the identity is a bijection over 2^64 with \
             one row per entity (contracts §2.6), and the determinism of the whole selection rests \
             on that being true"
        );
        Selection { rows, rows_visited }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// θ_d rises with the occupied-tile count and with nothing else.
    ///
    /// The old form multiplied by four per depth whatever the data did. The cut now tracks
    /// `N_occ(d)`: `n` times the occupied tiles is `n` times the cut, up to the one unit per tile
    /// that flooring the whole product rather than the unit can differ by.
    #[test]
    fn the_cut_is_proportional_to_the_occupied_tile_count() {
        let v_total = 1_000_000u64;
        let Threshold::Cut(one) = Threshold::at_depth(v_total, 16, 1) else {
            panic!("expected a cut at N_occ = 1");
        };
        for n_occ in [1u64, 2, 3, 100, 4096] {
            match Threshold::at_depth(v_total, 16, n_occ) {
                Threshold::Cut(p) => {
                    let scaled = one as u128 * n_occ as u128;
                    assert!(
                        (p as u128) >= scaled && (p as u128) < scaled + n_occ as u128,
                        "N_occ={n_occ}: {p} is not {scaled} to within {n_occ}"
                    );
                }
                Threshold::Saturated => panic!("N_occ={n_occ} saturated unexpectedly"),
            }
        }
    }

    /// The exactness the design states: `P_d = ⌊m_target · N_occ · 2⁶⁴ / V_total⌋`, floored once.
    ///
    /// Computing `⌊m_target · 2⁶⁴ / V_total⌋` first and multiplying by `N_occ` afterwards floors
    /// twice and disagrees with the reference oracle, which floors once.
    #[test]
    fn the_cut_floors_once_over_the_whole_product() {
        for (v_total, m_target, n_occ) in [
            (7u64, 1u64, 3u64),
            (1_000_003, 16, 97),
            (2_400_000, 16, 1_237),
            (233_000_000, 16, 1_048_573),
        ] {
            let want = (((m_target as u128) * (n_occ as u128)) << 64) / v_total as u128;
            match Threshold::at_depth(v_total, m_target, n_occ) {
                Threshold::Cut(p) => assert_eq!(p as u128, want, "{v_total}/{m_target}/{n_occ}"),
                Threshold::Saturated => panic!("{v_total}/{m_target}/{n_occ} saturated"),
            }
        }
    }

    /// The trap the old `leading_zeros` guard existed for, in its new shape: an arithmetic
    /// overflow must answer `Saturated` and never a wrapped `Cut(0)` — a zero cut is `C_θ = 0` in
    /// every tile at every depth, every tile drawing `k_min` for ever, with no error raised.
    ///
    /// The saturation test runs before the shift, so no product past 2⁶⁴ is ever formed.
    #[test]
    fn a_product_past_the_identity_space_saturates_rather_than_wrapping_to_zero() {
        for m_target in [1u64, 16, 1 << 40, u64::MAX] {
            for n_occ in [0u64, 1, 1 << 16, 1 << 32, u64::MAX] {
                for v_total in [1u64, 17, 1_000_000, u32::MAX as u64] {
                    let saturates = (m_target as u128) * (n_occ as u128) >= v_total as u128;
                    match Threshold::at_depth(v_total, m_target, n_occ) {
                        Threshold::Saturated => assert!(
                            saturates,
                            "{v_total}/{m_target}/{n_occ}: saturated where a cut fits"
                        ),
                        Threshold::Cut(p) => {
                            assert!(!saturates, "{v_total}/{m_target}/{n_occ}: cut where θ ≥ 1");
                            // `N_occ = 0` is a view with nothing visible in it, where a zero cut
                            // is the arithmetic answer and no tile is emitted to be thinned by it.
                            // Anywhere else a zero is the wrap this test exists for.
                            if n_occ > 0 {
                                assert_ne!(
                                    p, 0,
                                    "{v_total}/{m_target}/{n_occ}: wrapped to Cut(0)"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    /// `N_occ` is non-decreasing in depth, so θ is too — the nesting proof's threshold clause.
    #[test]
    fn a_non_decreasing_occupancy_gives_a_non_decreasing_cut() {
        let v_total = 5_000_000u64;
        // A measured occupancy ladder's shape: growth well below the 4× the old form assumed.
        let ladder = [1u64, 4, 13, 41, 130, 410, 1_300, 4_100, 13_000, 41_000];
        let mut previous = 0u128;
        for n_occ in ladder {
            let here = match Threshold::at_depth(v_total, 16, n_occ) {
                Threshold::Cut(p) => p as u128,
                Threshold::Saturated => u128::MAX,
            };
            assert!(here >= previous, "N_occ={n_occ}: the cut fell");
            previous = here;
        }
    }

    #[test]
    fn a_viewer_seeing_no_more_than_the_target_is_shown_everything() {
        for v in [0u64, 1, 8, 16] {
            assert!(
                Threshold::at_depth(v, 16, 1).is_saturated(),
                "v_total={v} must anchor saturated"
            );
        }
        assert!(
            !Threshold::at_depth(17, 16, 1).is_saturated(),
            "v_total just above the target must produce a cut"
        );
    }

    /// A view with nothing visible in it has no occupied tiles, and the empty product must not
    /// become a cut of zero: `Cut(0)` and `Saturated` differ in what they admit.
    #[test]
    fn an_empty_occupancy_is_a_zero_cut_and_not_a_saturation() {
        assert_eq!(Threshold::at_depth(1_000, 16, 0), Threshold::Cut(0));
        // With no visible rows at all there is no tile to draw in, so the answer is the harmless
        // one and the division is never taken.
        assert!(Threshold::at_depth(0, 16, 0).is_saturated());
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
    fn the_tier_gate_switches_exactly_at_full_coverage_and_the_density_constant() {
        // Full coverage is its own tier, not merely 100% density.
        assert_eq!(decode_tier(256, 256), DecodeTier::FullRange);
        assert_eq!(decode_tier(0, 0), DecodeTier::FullRange);

        // The boundary: exactly RUN_DECODE_MIN_DENSITY_PCT fires runs, one row below does not.
        assert_eq!(
            decode_tier(RUN_DECODE_MIN_DENSITY_PCT, 100),
            DecodeTier::Runs
        );
        assert_eq!(
            decode_tier(RUN_DECODE_MIN_DENSITY_PCT - 1, 100),
            DecodeTier::Values
        );
        assert_eq!(decode_tier(99, 100), DecodeTier::Runs);
        assert_eq!(decode_tier(0, 100), DecodeTier::Values);

        // No overflow at the row-space extremes: visible and range_len are both < 2^32, so the
        // ×100 stays far inside u64.
        assert_eq!(
            decode_tier(u32::MAX as u64 - 1, u32::MAX as u64),
            DecodeTier::Runs
        );
        assert_eq!(
            decode_tier((u32::MAX as u64) / 2, u32::MAX as u64),
            DecodeTier::Values
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
