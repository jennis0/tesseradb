//! `N_occ(d)` — how many depth-*d* tiles hold at least one row this session may see.
//!
//! Design §7.2 anchors the selection threshold at `θ_d = m_target · N_occ(d) / V_total`.
//! [`EffectiveMask::visible_total`] counts `V_total` exactly, and
//! [`crate::select::Threshold::at_depth`] turns the pair into a cut point.
//!
//! **One segment counts `N_occ(d)`; two or more estimate it with a [`TileSketch`]**
//! ([decision 0138](../../../docs/decisions/0138-n-occ-is-a-sketch-above-one-segment-and-its-ladder-is-filled-in-the-background.md)).
//! [`occupied_tiles_ladder`] owns that predicate and gives the measurement behind it. A freshly
//! built bundle has one segment per view and so does every conformance fixture; a live deployment
//! accumulates one per flush and so is estimating within a publication or two of opening.
//!
//! # It is a composed quantity, and I2 requires it
//!
//! `N_occ(d)` is counted over the **composed** mask — the [`EffectiveMask`] after the overlay
//! diff — and never over the cached [`crate::compose::RowProjection`], which is `M_auth` *before*
//! that diff. The argument is [`EffectiveMask::visible_total`]'s, term for term: a viewer can
//! aggregate mark counts across tiles, solve for θ, and difference it against the per-tile
//! `visible` §7.1 discloses exactly. If either factor of θ were pre-overlay, that difference is a
//! running estimate of how many of the viewer's own items have been denied — a count of items
//! outside `M_auth`, which Appendix C admits nowhere. `N_occ` carries the signal as directly as
//! `V_total` does: a suppression that empties a tile lowers it.
//!
//! It is also **filter-blind**, and for **I12**'s reason: [`crate::compose::EffectiveMask`] with a
//! filter attached is a narrower set, so counting occupied tiles under one would make θ a function
//! of what the viewer typed and move the frontier down. The call site takes the anchor before the
//! filter is evaluated.
//!
//! # Why an estimate is enough, and what it is not enough for
//!
//! §7.2 asks two things of `N_occ`, and exactness is neither of them. It must be **monotone in
//! depth**, because that is what the nesting proof turns into θ non-decreasing and so into a zoom
//! that does not drop the marks its parent drew. And it must be computed **inside the viewer's
//! mask**, because θ is a disclosure surface (below). A relative error of 1% moves the mean marks
//! per occupied tile from `m_target` to `0.99 m_target`; even 20% would move 16 marks to 13 or 19,
//! which is not a difference a viewer can see.
//!
//! What the estimate costs is **the observability of a single tile**, and it costs it only where
//! the estimate runs. `N_occ` is exact at one segment, so a suppression that empties one tile
//! lowers it by exactly one and `n_occ_falls_when_a_suppression_empties_a_tile` asserts that on a
//! single-segment fixture. Above one segment no such assertion could be written at scale: at a
//! million occupied tiles one emptied tile is far inside the sketch's error. The property — the
//! anchor is composed, not pre-overlay — is unchanged; the resolution at which it can be asserted
//! moves with the route.
//!
//! # Three properties, and where each comes from
//!
//! **Monotone in depth.** Every occupied depth-*d* tile has at least one occupied child, and
//! children of distinct parents are distinct tiles, so `N_occ(d+1) >= N_occ(d)` — of the *true*
//! quantity, which is what the counted route returns. Two adjacent *estimates* of it need not obey
//! that, and where `N_occ` barely grows between two depths a 1% error either way can invert them:
//! sixteen measured cells do (`treeoflife-1m` under a 5% mask, depths 14 to 15, where the raw
//! estimate falls 50,352 → 50,187, a 0.33% step —
//! `probes/2026-09-09-nocc-sketch`). [`OccupancyLadder`] therefore carries a **running maximum**
//! across depths: `at(d)` is the largest rung at or below *d*, and after it those sixteen
//! inversions are zero. That is structural rather than hopeful, and it errs in the safe direction —
//! serving more marks than the formula asks is harmless where serving fewer breaks nesting.
//!
//! Architecture §7.2 forbade exactly this until r63 — "No implementation may clamp θ or carry a
//! running maximum over depth" — a sentence written for an exact count, where a fall between
//! depths can only be a bug. Under an estimate it is false, and r63 deletes it rather than
//! qualifying it (owner ruling, 2026-09-09): monotonicity now comes from the running maximum, and
//! the miscount the sentence feared is caught by the differential and by
//! `the_occupied_tile_count_is_the_sketch_of_the_right_tiles_monotone_and_bounded`, which pins the
//! walk's emissions against a linear scan.
//!
//! **`N_occ(d) <= 4^d`.** There are only `4^d` tiles at depth *d*, so a rung is clamped there
//! before the running maximum. This is what makes the shallow rungs *exact* on the estimated
//! route: depth 0 is one tile whatever the data does, and the ladder answers 1 rather than the
//! sketch's 1-or-2.
//!
//! **Deterministic.** [`SKETCH_SEED`] is a constant, the mixer is SplitMix64's finalizer, and every
//! step of the estimator is integer arithmetic — no float, so no libm and no summation order for
//! two processes to disagree over. `reference/oracle/occupancy.py` is the same arithmetic in
//! Python, pinned vector for vector by
//! `the_ladder_matches_the_python_oracle_vector_for_vector` here and
//! `test_the_ladder_matches_the_engine_vector_for_vector` there, which is what keeps
//! `conformance/tests/test_i7_selection.py` an exact differential rather than one within a band.
//!
//! # The walk, and the two routes that were measured and rejected
//!
//! Rows are stored in Morton order within a segment, so the code column ascends with the row id
//! and a tile is a contiguous range of it. The walk takes the mask's visible runs and, inside a
//! run, hops from one occupied tile's code boundary to the next by exponential search
//! ([`gallop`]). Cost is `O((runs + N_occ(d)) · log)` per depth, against the oracle's one pass per
//! visible row.
//!
//! **Endpoint arithmetic on a run is wrong.** Crediting `tile(end) − tile(start) + 1` per run
//! counts the tile indices a run *spans*, not the occupied tiles inside it: a run's Morton codes
//! are not dense in tile-index space, so every empty tile between two occupied ones is counted.
//! Measured against the linear-scan oracle it is wrong by 1.6×–225× at depth 12 across four
//! corpora. The gallop is what makes the answer exact.
//!
//! **A descent over the tile tree is not affordable.** Visiting occupied nodes breadth-first pays
//! a Morton binary search per node to find that node's row range plus one mask question to decide
//! whether it is empty, and the node count is the answer being computed: reaching depth 16 visits
//! `Σ_d N_occ(d)` nodes where this walk visits one depth's runs. Measured over 1M–13.5M-row
//! corpora a descent took 9.2–59 s for 17 depths against 21–191 ms for this walk, and the variant
//! testing emptiness with a short-circuiting intersection rather than a full count stayed far
//! behind.
//!
//! Both figures are from the route comparison recorded in
//! [decision 0137](../../../docs/decisions/0137-theta-is-anchored-on-the-occupied-tile-count.md).
//!
//! # What it costs, measured
//!
//! `tessera-bench`'s `occupancy_sketch` prices four **complete** routes — walk and accumulator
//! together — against one mask in one process: `main`'s Roaring union, the tiered branch's
//! bitset-and-buffer pair, one sketch per requested depth, and this ladder. Each corpus's own
//! Morton column, re-dealt into 1 to 512 segments as a base plus flush ticks, whole mask, minimum
//! of three timed calls, 2026-09-09.
//!
//! **What a session pays for the whole ladder** — the sum over all seventeen depths, milliseconds:
//!
//! | corpus | S | union | tiered | one sketch per depth | this ladder, deepest depth first |
//! |---|---|---|---|---|---|
//! | `treeoflife-1m` (10⁶ rows) | 1 | 24.0 | 24.0 | 35.9 | **24.8** |
//! | | 16 | 60.2 | 36.5 | 36.3 | **25.2** |
//! | | 512 | 243.9 | 97.5 | 59.7 | **33.0** |
//! | `geonames` (1.35 × 10⁷ rows) | 1 | 218 | 217 | 263 | **189** |
//! | | 16 | 427 | 376 | 270 | **185** |
//! | | 512 | 1685 | 1094 | 515 | **276** |
//!
//! **The two sketch columns are the bounds on one policy, and a session lands between them.** A
//! request at depth *d* walks once and fills every rung `0..=d` that is not already memoised. A
//! session that jumps to its deepest zoom and works outwards pays the right-hand column — one
//! walk for the whole ladder. One that steps down a level at a time finds every shallower rung
//! already filled and pays the left — one walk per level, with only that level's sketch to update.
//! [`crate::stage`] then takes the whole of that off a session's first paint by walking to depth 12
//! at authorise; above 12 a request still walks for itself.
//!
//! **The ladder is not free, and where the walk is the whole cost it does not pay.** Filling every
//! rung costs `Σ_{d' <= d} N_occ(d')` sketch updates, which on these corpora is three to five
//! times the deepest rung's emissions; a walk that fills all seventeen rungs at once therefore
//! costs about what four walks cost. Filling them **again** at every depth a session visits is
//! the naive policy and it is a loss at every cell measured — 188 ms against the tiered arm's 97.5
//! over 10⁶ rows at 512 segments, 1286 against 1094 over 1.35 × 10⁷. That is why the fill is
//! against the memo rather than unconditional.
//!
//! **Accuracy, and why the register count is 2¹⁴.** Over 9,792 cells — both corpora, three split
//! shapes, 1 to 512 segments, a 5% mask, every depth 0 to 16 — the estimate's absolute relative
//! error has a **median of 0.323%, a p90 of 1.562% and a maximum of 2.041%** (`treeoflife-1m`, one
//! segment, depth 3: 49 occupied tiles read as 48). θ scales linearly with `N_occ`, so 2.041% is
//! 2.041% of θ: a `m_target` of 16 marks per occupied tile becomes 15.67. Precision 12 — 4 kB a
//! rung instead of 16, and 68 kB a ladder instead of 272 — was swept beside it and **declined**: it
//! is 9% faster (a median 0.913× at 45 of 56 multi-segment cells, the cache argument holding), but
//! it doubles the error to 4.082% and takes the raw inversions from 16 to 96, and with the ladder
//! staged off the request path (see [`crate::stage`]) a 9% saving on the walk buys nothing a
//! viewer can see.
//!
//! **Sixteen cells invert before the running maximum and none after it.** They are the one cell at
//! every segment count — `treeoflife-1m` under a 5% mask, depths 14 to 15, where the raw estimate
//! falls 50,352 → 50,187, a 0.33% step under a 0.81% standard error. The maximum is what §7.2's
//! nesting proof rests on now, not a belt-and-braces guard against a miscount.
//!
//! **Memory.** The register plane is `(d + 1) · 2^SKETCH_PRECISION` bytes and nothing else grows:
//! a measured peak of **278,528 bytes at depth 16, at every segment count and on both corpora**,
//! which differ by 13.5× in rows. Against it, over the same sweep, the union reached 3.35 MB and
//! 22.1 MB, the tiered arm 8.19 MB and 92.6 MB, and a ladder of seventeen exact accumulators behind
//! the same walk 23.6 MB and 263 MB — all three of which grow with the data. The counted route's
//! whole state is two `[u64; 17]` arrays, 272 bytes.
//!
//! **The walk, at the scale that matters**, measured end to end through the response trailer's
//! `theta_occupancy_ns` against `treeoflife` (2.33 × 10⁸ rows, one segment, so the counted route;
//! full coverage, 2026-09-09): **33 ms at depth 3, 63 ms at 12 and 191 ms at 16** for the first
//! request at a depth, against a warm request's 216–313 ms in total. Staged, a first request at
//! depth 3 to 12 pays none of it. `probes/2026-09-09-nocc-stage` carries the breakdown and the
//! projection to 3.65 × 10⁹.

use std::ops::Range;

use tessera_store::read::SegmentData;

use crate::compose::EffectiveMask;

/// How many rows one [`EffectiveMask::for_each_visible_run`] call covers.
///
/// **The walk is chunked rather than taken over the whole row space in one call.** That method
/// walks `base` in place only while the overlay diffs are empty; with any diff present it takes
/// the [`EffectiveMask::rows_in_range`] fallback, which materialises the composed mask over the
/// range it is handed. Passing the whole row space would therefore build a copy of a 233-million-
/// row bitmap on every request that arrives after a suppression. Chunking bounds that temporary to
/// one chunk's containers and costs, on the diffs-empty route, one extra cursor reset per chunk.
///
/// 2²² rows is 64 croaring containers — small enough that the temporary is a few hundred
/// kilobytes, large enough that a 233-million-row view is 56 chunks rather than thousands.
///
/// Splitting a run at a chunk boundary changes no answer: [`Walk::visit_run`] carries the last
/// credited tile across runs, so the two halves credit their shared tile once.
const CHUNK_ROWS: u32 = 1 << 22;

/// The depth-`depth` tile index of a Morton code.
///
/// `u64` throughout: at depth 0 the shift is 32, which is undefined on a `u32`. The result is
/// below `4^depth <= 2^32`, so it fits a `u32` for every depth the grid allows.
#[inline]
fn tile_of(code: u32, depth: u8) -> u64 {
    (code as u64) >> (32 - 2 * depth as u32)
}

/// `from + codes[from..].partition_point(|c| c < target)`, by exponential search out from `from`.
///
/// The walk's steps are short where occupied tiles are close together and long where they are far
/// apart, so the search cost follows the step rather than the length of the tail.
fn gallop(codes: &[u32], from: usize, target: u64) -> usize {
    let base = from.min(codes.len());
    let tail = &codes[base..];
    let mut lo = 0usize;
    let mut hi = 1usize;
    while hi <= tail.len() && (tail[hi - 1] as u64) < target {
        lo = hi;
        hi = hi.saturating_mul(2);
    }
    let hi = hi.min(tail.len());
    base + lo + tail[lo..hi].partition_point(|&c| (c as u64) < target)
}

/// One segment's walk state: the depth, the last tile credited, and where the credits go.
struct Walk<'a, F: FnMut(u64)> {
    codes: &'a [u32],
    depth: u8,
    /// The shift from a code to its depth-`depth` tile index.
    shift: u32,
    /// The highest tile index credited so far in this segment, or `u64::MAX` before the first.
    /// A `u32` code cannot produce that index at any depth, so it is a sound "nothing yet".
    last: u64,
    emit: F,
}

impl<F: FnMut(u64)> Walk<'_, F> {
    /// Credit every occupied tile in one segment-local run, ascending.
    fn visit_run(&mut self, run: Range<u32>) {
        let end = run.end as usize;
        let mut i = run.start as usize;
        while i < end {
            let tile = tile_of(self.codes[i], self.depth);
            if tile != self.last {
                self.last = tile;
                (self.emit)(tile);
            }
            // The first code at or above the next tile's low bound. At depth 0 the shift is 32 and
            // `(tile + 1) << 32` exceeds every code, so the gallop lands on `end` and the run
            // finishes in one step — one tile covers the grid.
            let target = (tile + 1) << self.shift;
            i = gallop(&self.codes[..end], i + 1, target).max(i + 1);
        }
    }
}

/// Credit every depth-`depth` tile of `segment` holding a row visible to `mask`, ascending and
/// without repetition.
///
/// `row_base` is where this segment's rows begin in the view's row space, which is the space the
/// mask is over; `segment.morton` is indexed segment-locally.
///
/// **Public so a measurement prices an accumulator over the identical walk.**
/// `tessera-bench`'s `occupancy_sketch` compares this arm against the ones it replaced, and a
/// transcription of the walk in the harness would compare two walks rather than two accumulators.
pub fn for_each_occupied_tile(
    mask: &EffectiveMask,
    segment: &SegmentData,
    row_base: u32,
    depth: u8,
    emit: impl FnMut(u64),
) {
    let codes = segment.morton.u32();
    let mut walk = Walk {
        codes,
        depth,
        shift: 32 - 2 * depth as u32,
        last: u64::MAX,
        emit,
    };
    let rows = segment.row_count.min(codes.len() as u32);
    let mut start = 0u32;
    while start < rows {
        let end = start.saturating_add(CHUNK_ROWS).min(rows);
        mask.for_each_visible_run(row_base + start..row_base + end, |run| {
            walk.visit_run(run.start - row_base..run.end - row_base);
        });
        start = end;
    }
}

/// `N_occ(d)` for every depth `0..=depth`, from one walk.
///
/// **Exact at one segment, estimated above it** — see [`occupied_tiles_ladder`] for the predicate
/// and the measurement behind it. Either way [`Self::at`] answers the same three properties, and
/// the two below hold whichever route filled the rungs.
///
/// **Monotone by construction, not by hope.** [`Self::at`] is a running maximum over the depths at
/// or below the one asked for, so no pair of adjacent depths can invert however the estimates fall.
/// Serving more marks than the formula asks for is harmless; serving fewer breaks nesting. On the
/// exact route the maximum never binds — `N_occ` is non-decreasing in depth by the structure of
/// the grid — and it is applied anyway rather than branched around, so the tail of this
/// computation is one piece of code with one set of properties.
///
/// **`counts[d] <= 4^d`.** There are only `4^d` tiles at depth *d*, so a rung is clamped there
/// before the running maximum — which is what makes the shallow depths exact on the sketch route
/// rather than merely close (`N_occ(0)` is 1 for any non-empty view, and the sketch alone would
/// answer 1 or 2). On the exact route it never binds either, for the same reason it is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OccupancyLadder {
    counts: [u64; 17],
    /// Each depth's rung **before** the running maximum, clamped to `4^d`. Kept so the
    /// monotonicity claim above is measurable rather than asserted: `tessera-bench`'s
    /// `occupancy_sketch` counts the inversions this array has and the one [`Self::at`] answers
    /// from does not.
    raw: [u64; 17],
    depth: u8,
    /// Whether the rungs are counts rather than estimates — the one-segment route.
    ///
    /// **A fact about how this ladder was filled, not a mode anything selects.** It exists so a
    /// test can assert which route ran, and so a measurement can say which number it is quoting;
    /// no request-path branch reads it, because θ's three properties are the same either way.
    exact: bool,
}

impl OccupancyLadder {
    /// `N_occ(depth)`. Panics above the depth this ladder was filled to, which is a programming
    /// error rather than a data one: a walk at depth *d* cannot answer for a deeper tile grid.
    pub fn at(&self, depth: u8) -> u64 {
        assert!(
            depth <= self.depth,
            "this ladder was filled to depth {} and was asked for depth {depth}",
            self.depth
        );
        self.counts[depth as usize]
    }

    /// The deepest depth this ladder answers for.
    pub fn depth(&self) -> u8 {
        self.depth
    }

    /// Whether these rungs are exact counts — see the field.
    pub fn is_exact(&self) -> bool {
        self.exact
    }

    /// `N_occ(depth)` as the route produced it, before the running maximum. For measurement
    /// only — nothing on the request path may read this, because nothing guarantees it is
    /// monotone.
    pub fn raw_at(&self, depth: u8) -> u64 {
        self.raw[depth as usize]
    }
}

/// `N_occ(d)` for every `d` in `0..=depth`, from **one** walk of the mask and the Morton column.
///
/// # One walk fills every depth at or below it
///
/// A walk at depth *d* emits that depth's occupied tiles ascending; the depth-*d'* tile holding one
/// of them is `tile >> 2(d - d')`, so the shallower ladder is a function of the same emissions and
/// costs no second pass over the mask. `N_occ` is memoised per `(session, view, depth)` and paid on
/// the first request at each new depth, so a session that reaches depth *d* by any route other than
/// stepping down through every level pays one walk rather than one per level.
///
/// **Nothing here walks eagerly, and one thing outside here does.** A request at depth 6 walks at
/// depth 6 and this function walks at 16 on nobody's behalf. [`crate::stage`] fills rungs `0..=6`
/// and then `0..=12` in the background at authorise, so that the walk is not on a first paint;
/// above 12 the ladder is still extended on demand, by this function, from a request.
///
/// # The emissions ascend, so a shallower tile changes only occasionally
///
/// Hashing all 17 depths per emission would make this slower than the sort it replaces. It does not
/// have to: within a segment the emissions ascend, so the depth-*d'* ancestor changes exactly
/// `N_occ_s(d')` times, and the total sketch updates are `Σ_{d' <= d} N_occ_s(d')` rather than
/// `(d + 1) · N_occ_s(d)`. The descent below compares against the last ancestor seen at each depth
/// and **stops at the first match**: ancestors nest, so a depth whose ancestor is unchanged
/// guarantees every shallower depth is unchanged too.
///
/// The last-seen array is deliberately **not** reset between segments. A repeat is idempotent in a
/// sketch, so carrying it across a segment boundary can only save an update, never lose one — and
/// where two segments' walks meet on the same tile it saves the whole descent.
///
/// # One segment counts; two or more estimate
///
/// **The predicate is `segments.len() == 1` and it is the whole of the route choice.** At one
/// segment the walk emits each tile once and ascending, so the descent's "the ancestor changed"
/// test fires exactly `N_occ(d')` times at each depth and a **counter is the accumulator** — the
/// exact answer costs an increment where the sketch costs a mix, an index and a compare. Measured
/// over all seventeen rungs at one segment: 12.5 ms exact against 24.8 ms sketched over 10⁶ rows,
/// and 129 ms against 189 over 1.35 × 10⁷ (`probes/2026-09-09-nocc-sketch`, both corpora, whole
/// mask). Across that probe's eight one-segment cells the sketch lost at every one, by 0.61× to
/// 0.88×.
///
/// Above one segment the counter stops being available at all: a tile can hold rows in several
/// segments, the walks are per segment, and counting "the ancestor changed" would count a shared
/// tile once per segment that holds it. That is what the union, the sort and the direct-mapped
/// bitset the three earlier arms carried all existed to do, and it is the case the sketch removes:
/// adding a tile twice to a sketch is adding it once, so one sketch fed by every segment *is* the
/// union with nothing to choose between.
///
/// **θ therefore changes character across a flush**, from a count to an estimate at the first
/// publication that gives a view its second segment. That is accepted rather than tolerated: θ
/// already moves on every flush — `V_total` moves, and `N_occ` itself moves as new rows occupy new
/// tiles — and the memo is keyed on `segments_version`, so no session ever sees the two routes'
/// answers for one generation. What the boundary costs is that a deployment's θ carries the
/// sketch's error only while it is multi-segment, which is every live deployment and no freshly
/// built bundle.
///
/// **Every conformance fixture and every demo corpus is single-segment**, so the route the
/// differential exercises is the exact one; `reference/oracle/occupancy.py` says where that leaves
/// the estimator's coverage.
pub fn occupied_tiles_ladder(
    mask: &EffectiveMask,
    segments: &[(&SegmentData, u32)],
    depth: u8,
) -> OccupancyLadder {
    if segments.len() == 1 {
        occupied_tiles_ladder_exact(mask, segments, depth)
    } else {
        occupied_tiles_ladder_with_precision(mask, segments, depth, SKETCH_PRECISION)
    }
}

/// `N_occ(d)` for every `d` in `0..=depth`, **counted**, from one walk of a single segment.
///
/// # Sound only at one segment, and the caller is what makes it so
///
/// One segment's walk emits ascending and without repetition ([`Walk::visit_run`] carries the last
/// tile across runs and across chunks), so at each depth the ancestor changes exactly once per
/// distinct depth-*d* tile and the increments below are that depth's tile count. Two segments break
/// it in both directions — a tile split across segments would be counted twice, and the last-seen
/// array carried across a boundary would drop a tile the second segment re-enters — which is why
/// [`occupied_tiles_ladder`] and not this function owns the predicate.
///
/// **Public so a measurement prices the two accumulators over the identical walk**, exactly as
/// [`for_each_occupied_tile`] is. It asserts its own precondition rather than trusting a caller,
/// because the failure is a wrong number rather than a crash.
pub fn occupied_tiles_ladder_exact(
    mask: &EffectiveMask,
    segments: &[(&SegmentData, u32)],
    depth: u8,
) -> OccupancyLadder {
    debug_assert!(depth <= 16, "the grid is 2^16 x 2^16, so depth 16 is the deepest");
    assert_eq!(
        segments.len(),
        1,
        "the counted route is sound at one segment only; `occupied_tiles_ladder` owns the predicate"
    );
    let mut counted = [0u64; 17];
    // `u64::MAX` is not a tile index at any depth, so it is a sound "nothing seen yet".
    let mut last = [u64::MAX; 17];
    let (segment, row_base) = segments[0];
    for_each_occupied_tile(mask, segment, row_base, depth, |tile| {
        let mut d = depth;
        loop {
            let ancestor = tile >> (2 * u32::from(depth - d));
            if ancestor == last[d as usize] {
                break;
            }
            last[d as usize] = ancestor;
            counted[d as usize] += 1;
            if d == 0 {
                break;
            }
            d -= 1;
        }
    });
    finish_ladder(counted, depth, true)
}

/// [`occupied_tiles_ladder`]'s **sketch** route at an arbitrary precision, whatever the segment
/// count. The engine takes [`SKETCH_PRECISION`] and only above one segment; `tessera-bench`'s
/// `occupancy_sketch` sweeps this to choose it, and prices it at one segment against the counted
/// route above.
pub fn occupied_tiles_ladder_with_precision(
    mask: &EffectiveMask,
    segments: &[(&SegmentData, u32)],
    depth: u8,
    precision: u32,
) -> OccupancyLadder {
    debug_assert!(depth <= 16, "the grid is 2^16 x 2^16, so depth 16 is the deepest");
    // **One flat plane, not a `Vec` of sketches.** The descent below is the hottest loop in the
    // walk — it runs `Σ_{d' <= d} N_occ_s(d')` times — and a vector of vectors would pay a second
    // dependent load and a second bounds check on every one of them. The plane is
    // `(depth + 1) · 2^precision` bytes, which at [`SKETCH_PRECISION`] is 279 kB for the whole
    // seventeen-rung ladder and is the arm's entire memory bound.
    let stride = 1usize << precision;
    let mut plane = vec![0u8; stride * (depth as usize + 1)];
    // `u64::MAX` is not a tile index at any depth, so it is a sound "nothing seen yet".
    let mut last = [u64::MAX; 17];

    for (segment, row_base) in segments {
        for_each_occupied_tile(mask, segment, *row_base, depth, |tile| {
            let mut d = depth;
            loop {
                let ancestor = tile >> (2 * u32::from(depth - d));
                if ancestor == last[d as usize] {
                    break;
                }
                last[d as usize] = ancestor;
                let (index, rank) = register_of(ancestor, precision);
                let slot = &mut plane[d as usize * stride + index];
                if rank > *slot {
                    *slot = rank;
                }
                if d == 0 {
                    break;
                }
                d -= 1;
            }
        });
    }

    let mut estimated = [0u64; 17];
    for (d, rung) in estimated.iter_mut().enumerate().take(depth as usize + 1) {
        *rung = estimate_registers(&plane[d * stride..(d + 1) * stride], precision);
    }
    finish_ladder(estimated, depth, false)
}

/// The `4^d` ceiling and the running maximum, applied to raw rungs from either route.
///
/// **One tail for both routes**, so the two properties θ's nesting proof reads off a ladder —
/// `N_occ(d) <= 4^d` and non-decreasing in depth — are established in one place and hold whether
/// the rungs were counted or estimated. On the counted route neither step ever binds; it is not
/// branched around, because a tail that behaved differently by route would be two sets of
/// properties to keep true rather than one.
fn finish_ladder(raw_rungs: [u64; 17], depth: u8, exact: bool) -> OccupancyLadder {
    let mut counts = [0u64; 17];
    let mut raw = [0u64; 17];
    let mut running = 0u64;
    for d in 0..=depth as usize {
        // `4^d`, which is `2^32` at depth 16 and so needs the `u64`.
        let ceiling = 1u64 << (2 * d as u32);
        let rung = raw_rungs[d].min(ceiling);
        raw[d] = rung;
        running = running.max(rung);
        counts[d] = running;
    }
    OccupancyLadder {
        counts,
        raw,
        depth,
        exact,
    }
}

/// `N_occ(depth)` over the whole view: an estimate of how many depth-`depth` tiles hold at least
/// one row visible to this mask.
///
/// **Never restricted to a viewport.** θ is viewport-invariant (§7.2), so this takes the view's
/// whole row space exactly as `V_total` does. A bbox or a zoom reaching this would make θ move on
/// a pan.
///
/// `segments` is the view's segments with their row bases, in the order
/// `viewport::segments_with_row_bases` returns them.
///
/// This discards the shallower rungs [`occupied_tiles_ladder`] filled on the way. Callers that
/// memoise — the request path does — should take the ladder and keep them.
pub fn occupied_tiles(mask: &EffectiveMask, segments: &[(&SegmentData, u32)], depth: u8) -> u64 {
    occupied_tiles_ladder(mask, segments, depth).at(depth)
}

// ------------------------------------------------------------------------------------------
// The sketch
// ------------------------------------------------------------------------------------------

/// `log2` of the register count. 2¹⁴ registers of one byte is **16 KiB per depth**, 272 KiB for a
/// whole depth-16 ladder, and a relative standard error of `1.04 / sqrt(2^14)` = **0.81%**.
///
/// The register array is the arm's whole memory bound and it is a constant: it does not move with
/// the depth, the segment count, the corpus size or the mask.
pub const SKETCH_PRECISION: u32 = 14;

/// The additive constant SplitMix64 stirs into an input before its finalizer.
///
/// **Fixed, never randomised.** `N_occ` is memoised per `(session, view, generation, depth)` and
/// two processes serving the same bundle must answer the same number for the same mask, so a
/// per-process seed — `RandomState`'s, or any `HashMap` default — would make θ a function of which
/// server answered. The conformance differential reproduces this hash in Python and compares the
/// served set exactly; a randomised hasher would end that.
const SKETCH_SEED: u64 = 0x9E37_79B9_7F4A_7C15;

/// SplitMix64's finalizer: a bijection on `u64` with full avalanche.
///
/// A bijection rather than a hash with collisions, which is what a cardinality sketch wants: the
/// tile indices at one depth are distinct by construction, so the register a tile lands in is
/// decided by the mixing alone.
#[inline]
const fn mix64(z: u64) -> u64 {
    let z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    let z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A HyperLogLog over tile indices: constant memory, one pass, and **mergeable by taking the
/// maximum per register**, which is exactly the union.
///
/// [`occupied_tiles_ladder_with_precision`] does not hold seventeen of these — it holds one flat
/// register plane and calls [`register_of`] and [`estimate_registers`] directly, for the reason
/// its own comment gives. This type is what a caller with one depth to count uses, and what the
/// tests exercise.
///
/// That last property is why the multi-segment case stops being a special case. A tile can hold
/// rows in several segments and `N_occ` counts tiles rather than per-segment shares of them, so an
/// exact accumulator has to combine the segments' emissions — by a Roaring union, a sort, or a
/// direct-mapped bitset, each with its own memory and its own crossover. Adding the same tile
/// twice to a sketch is idempotent, so one sketch fed by every segment *is* the union and there is
/// nothing to choose between.
///
/// # Every arithmetic step is integer, so the answer is bit-identical everywhere
///
/// A `u64` per `(mask, view, generation, depth)` must not depend on the libm a process linked or
/// the order a summation happened to take. The estimator below therefore uses no floating point at
/// all: the harmonic sum is an exact `u128` in units of `2^-RANK_MAX`, α is an exact rational, and
/// the small-range branch's logarithm is a fixed-point series in `Q32` using only integer
/// multiplication, shifts and truncating division. `reference/oracle/viewport.py` is the same
/// arithmetic in Python, and `conformance/tests/test_i7_selection.py` compares the served set
/// exactly rather than to a tolerance.
#[derive(Debug, Clone)]
pub struct TileSketch {
    precision: u32,
    registers: Vec<u8>,
}

impl Default for TileSketch {
    fn default() -> Self {
        Self::new()
    }
}

impl TileSketch {
    /// A sketch at [`SKETCH_PRECISION`].
    pub fn new() -> Self {
        Self::with_precision(SKETCH_PRECISION)
    }

    /// A sketch at an arbitrary precision. `tessera-bench` sweeps this; the engine takes
    /// [`SKETCH_PRECISION`].
    pub fn with_precision(precision: u32) -> Self {
        assert!(
            (7..=18).contains(&precision),
            "α_m's closed form is stated for m >= 128 and the register array must stay small"
        );
        Self {
            precision,
            registers: vec![0u8; 1usize << precision],
        }
    }

    /// Record one tile index.
    #[inline]
    pub fn add(&mut self, tile: u64) {
        let (index, rank) = register_of(tile, self.precision);
        let slot = &mut self.registers[index];
        if rank > *slot {
            *slot = rank;
        }
    }

    /// The estimated number of distinct tiles recorded.
    pub fn estimate(&self) -> u64 {
        estimate_registers(&self.registers, self.precision)
    }

    /// The register array's size in bytes — the arm's whole memory bound.
    pub fn bytes(&self) -> usize {
        self.registers.len()
    }
}

/// Which register a tile index lands in, and the rank it proposes for it.
///
/// The top `precision` bits of the hash choose the register and the run of zeros below them is the
/// rank. `| 1 << (precision - 1)` caps the run at `64 - precision`, so a hash whose low bits are
/// all zero produces the largest rank a register can hold rather than one it cannot.
#[inline]
fn register_of(tile: u64, precision: u32) -> (usize, u8) {
    let h = mix64(tile.wrapping_add(SKETCH_SEED));
    let index = (h >> (64 - precision)) as usize;
    let rank = ((h << precision) | (1u64 << (precision - 1))).leading_zeros() as u8 + 1;
    (index, rank)
}

/// `α_m · 2³²`, exactly, by integer arithmetic.
///
/// `α_m = 0.7213 / (1 + 1.079/m)`, so `α_m · 2³² = 7213 · m · 2³² / (10 · (1000m + 1079))`,
/// rounded to nearest. Written this way rather than as an `f64` literal so the constant is the
/// same in Rust and in the Python oracle without either transcribing the other's rounding.
fn alpha_q32(precision: u32) -> u128 {
    let m = 1u128 << precision;
    let num = 7213u128 * m * (1u128 << 32);
    let den = 10u128 * (1000 * m + 1079);
    (2 * num + den) / (2 * den)
}

/// The estimated number of distinct values behind one register array.
///
/// **A histogram of the ranks, not a pass of `u128` shifts.** There are at most `64 - precision + 1`
/// distinct rank values, so counting them into a small array that stays in L1 and combining
/// afterwards turns `2^precision` wide shift-adds into `2^precision` byte increments. It matters
/// because the estimator runs once per rung: a depth-0 call at precision 14 — one rung, so the
/// estimator and the allocation are nearly the whole of it — fell from 19.5 µs to 7.0 µs when this
/// replaced the `u128` pass (`occupancy_sketch`, `treeoflife-1m`, 2 × 10⁵ rows, 2026-09-09).
fn estimate_registers(registers: &[u8], precision: u32) -> u64 {
    let rank_max = 64 - precision + 1;
    let mut hist = [0u32; 65];
    for &r in registers {
        hist[r as usize] += 1;
    }
    // `Σ_j 2^-M[j]`, in units of `2^-rank_max` so every term is an exact integer. The sum is at
    // most `m · 2^rank_max = 2^65`, which is why it is a `u128`.
    let mut inv: u128 = 0;
    for (rank, &count) in hist.iter().enumerate().take(rank_max as usize + 1) {
        inv += u128::from(count) << (rank_max - rank as u32);
    }
    let zeros = u64::from(hist[0]);

    // `E = α_m · m² / Σ 2^-M[j]`, which with the scaling above is
    // `(α_m · 2³²) · 2^(2·precision) · 2^rank_max / (2³² · inv)` = `α_q32 · 2^(precision+33) / inv`.
    let m = 1u128 << precision;
    // Saturating rather than truncating: `inv` bottoms out at `m` (every register at `rank_max`),
    // which puts `raw` at `α · 2^65` — above `u64` — for a cardinality nothing could reach. The
    // Python oracle saturates at the same place, and a wrapping `as` there would be a silent
    // disagreement at the one input that produces it.
    let raw = u64::try_from((alpha_q32(precision) << (precision + 33)) / inv).unwrap_or(u64::MAX);

    // **Linear counting below 2.5m**, which is where HyperLogLog's estimator is biased and where a
    // register array with empty slots has a better one available: with `V` of `m` registers still
    // empty, `m · ln(m/V)` is the balls-into-bins estimate.
    if zeros > 0 && u128::from(raw) <= (5 * m) / 2 {
        let ln_ratio_q32 = u64::from(precision) * LN2_Q32 - ln_q32(zeros);
        return ((m as u64) * ln_ratio_q32) >> 32;
    }
    // No large-range correction: the hash is 64 bits wide, so the `2^32/30` threshold a 32-bit
    // HyperLogLog needs is unreachable here.
    raw
}

/// `ln 2 · 2³²`, rounded to nearest.
const LN2_Q32: u64 = 2_977_044_472;

/// `ln(v) · 2³²` for `v >= 1`, by integer arithmetic alone.
///
/// `v = 2^k · f` with `f` in `[1, 2)`, so `ln v = k · ln 2 + ln f`, and `ln f = 2 · atanh(z)` for
/// `z = (f − 1) / (f + 1)` in `[0, 1/3]`. The series `z + z³/3 + z⁵/5 + …` loses a factor of nine
/// per term there, so twenty terms are far past `Q32`'s last bit; the count is fixed rather than
/// tested against zero so the loop is the same shape in both implementations.
fn ln_q32(v: u64) -> u64 {
    debug_assert!(v >= 1, "ln is taken of a register count, which is at least one here");
    let k = u64::from(63 - v.leading_zeros());
    let one = 1u128 << 32;
    // `f` in Q32, in `[2³², 2³³)`.
    let f = (u128::from(v) << 32) >> k;
    let z = ((f - one) << 32) / (f + one);
    let z2 = (z * z) >> 32;
    let mut term = z;
    let mut acc = z;
    let mut i = 3u128;
    while i <= 41 {
        term = (term * z2) >> 32;
        acc += term / i;
        i += 2;
    }
    k * LN2_Q32 + (2 * acc) as u64
}

/// What `N_occ(d)` is a function of: the composed mask, the view's row space, and the depth.
///
/// **Named fields rather than a tuple**, on [`crate::cache::RowProjectionKey`]'s argument: four of
/// the terms are `u64`, so a transposition at the construction site would compile, run, and answer
/// one principal's θ from another's occupancy.
///
/// Every term is a reason the composed mask or the row space moved, and the set is
/// [`crate::histogram::MaskIdentity`]'s plus the view and the depth:
///
/// - **`token_id`** — the count is inside one principal's mask, and is never shared across
///   principals. Never the bearer token, and never reused within a process
///   ([`crate::cache::RowProjectionKey`]'s fact 1 is what makes that sound).
/// - **`view`** — `N_occ` is counted over one view's rows, exactly as `V_total` is.
/// - **`depth`** — the quantity is per depth. A session touches a handful of depths, which is why
///   this is evaluated per requested depth rather than for all 17 at once.
/// - **`segments_version`** — row ids mean something only within one geometry, and a flush changes
///   which tiles hold rows.
/// - **`overlay_version`** — a suppression or a deletion removes rows from the composed mask and
///   can empty a tile. The response to a deny is at accept, so the correction may not wait for a
///   refresh.
/// - **the fragment's identity and watermark** — a session may be served a one-generation-stale
///   projection (decision 0044), so two requests at one `segments_version` can compose against
///   different fragments.
///
/// **The attribute filter is not a term and must not become one**: θ is anchored on the unfiltered
/// composed mask (**I12**), so there is no filtered variant of this quantity to key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct OccupancyKey {
    pub token_id: u64,
    pub view: String,
    pub depth: u8,
    pub segments_version: u64,
    pub overlay_version: u64,
    pub fragment_identity: [u8; 32],
    pub fragment_watermark: u64,
}

/// The default byte bound on the memo, and the only one an embedder that never calls
/// [`crate::Engine::set_occupancy_cache_bytes`] gets.
///
/// **The live working set is tiny and the key space is not.** A session holds one rung per depth
/// per view, so eight concurrent sessions over one view hold 17 × 8 entries — 70 KB at the
/// cache's 512 B per-entry floor. What grows is the superseded part: every flush, merge and fold
/// moves `segments_version`, the fragment identity or `overlay_version`, and every rung taken
/// before it becomes an entry no request can name again. Without a bound those entries stay for
/// the life of the process, at one ladder per (session, view) per publication.
///
/// 32 MiB admits 65,536 entries, which is 480 publications' worth of ladders for eight sessions
/// over one view before the LRU begins removing the coldest — and the coldest are exactly the
/// superseded ones, because a live rung is re-read on every request that composes θ. A bound below
/// the live set would cost walks, not correctness ([`crate::single_flight`]'s rule 3).
///
/// **What a deployment should set it to** (`serve.occupancy_cache_bytes`): the live set is
/// [`OCCUPANCY_LIVE_BYTES_PER_SESSION`] per concurrently-querying session per view, and the
/// headroom above it is how many publications of superseded ladders the memo carries before the
/// LRU takes them. This default is the live set of eight sessions over one view — the
/// `serve.expected_concurrent_sessions` default — with about 480 publications of headroom. A
/// deployment that raises `expected_concurrent_sessions` to 1,000 needs 8.7 MB for the live set
/// alone and should raise this in proportion if it wants the same headroom; leaving it here costs
/// walks rather than correctness, and `/control/status`' `occupancy.evictions` beside `walks` is
/// where that shows.
pub const DEFAULT_OCCUPANCY_CACHE_BYTES: u64 = 32 * 1024 * 1024;

/// What one session's live ladder charges the memo, over one view: one rung per depth at the
/// cache's per-entry floor.
///
/// The 17 depths are `0..=16`, the whole quantisation grid — a session touches a handful, and the
/// background fill ([`crate::stage`]) takes it to [`crate::stage::BACKGROUND_DEPTH`], so this is
/// the ceiling rather than the typical charge. It is `pub` because
/// `tessera_server::validate_cache_bounds` weighs the configured bound against it and the
/// arithmetic must have one home.
pub const OCCUPANCY_LIVE_BYTES_PER_SESSION: u64 =
    17 * crate::single_flight::PER_ENTRY_FLOOR_BYTES;

/// One memoised `N_occ(d)`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct OccupiedTiles(pub u64);

impl crate::single_flight::CacheWeight for OccupiedTiles {
    fn cache_weight_bytes(&self) -> u64 {
        // The value is a `u64`; the per-entry floor the cache applies is what actually bounds the
        // entry count, and it is the honest charge for a key holding a view name.
        std::mem::size_of::<u64>() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One rung, for the memo's bound.
    fn rung(token_id: u64, depth: u8, segments_version: u64) -> OccupancyKey {
        OccupancyKey {
            token_id,
            view: "geo".to_string(),
            depth,
            segments_version,
            overlay_version: 0,
            fragment_identity: [0u8; 32],
            fragment_watermark: 0,
        }
    }

    /// **The memo is bounded, and the bound is above the live set by orders of magnitude.**
    ///
    /// The two halves the figure has to satisfy. Seventeen depths for each of eight sessions —
    /// `serve.expected_concurrent_sessions`' default, the concurrency every other bound is sized
    /// against — is the live set, and it must be resident together, because evicting a rung a
    /// request is about to read costs a mask walk. And a key space that moves with every
    /// publication must not accumulate, which is what the bound is for.
    #[test]
    fn the_memo_holds_the_live_set_and_bounds_the_superseded_one() {
        let cache = crate::single_flight::SingleFlightCache::new(DEFAULT_OCCUPANCY_CACHE_BYTES);
        for token_id in 0..8u64 {
            for depth in 0..=16u8 {
                let _ = cache.get_or_derive(rung(token_id, depth, 0), None, |_| OccupiedTiles(1));
            }
        }
        let live = cache.stats();
        assert_eq!(live.entries, 8 * 17, "the live set is resident together");
        assert_eq!(live.evictions, 0);

        // The superseded half is arithmetic rather than a filled cache: admitting 65,536 entries
        // one at a time to watch the 65,537th evict is a minute of a debug build for a property
        // the per-entry charge already fixes. `an_undersized_bound_evicts` below is where the
        // eviction itself is exercised.
        let admitted = DEFAULT_OCCUPANCY_CACHE_BYTES / crate::single_flight::PER_ENTRY_FLOOR_BYTES;
        assert_eq!(admitted, 65_536);
        assert!(
            admitted > 400 * (8 * 17),
            "the bound admits {admitted} entries, which is {} publications' worth of ladders for \
             eight sessions — close enough to the live set to evict a rung a request is about to \
             read",
            admitted / (8 * 17),
        );
    }

    /// The memo evicts under its bound rather than growing past it. Written against a bound small
    /// enough to reach in a few entries; the figure the deployment gets is
    /// [`DEFAULT_OCCUPANCY_CACHE_BYTES`], whose size is argued there.
    #[test]
    fn an_undersized_bound_evicts() {
        let bound = 8 * crate::single_flight::PER_ENTRY_FLOOR_BYTES;
        let cache = crate::single_flight::SingleFlightCache::new(bound);
        for publication in 0..32u64 {
            let _ = cache.get_or_derive(rung(0, 0, publication), None, |_| OccupiedTiles(1));
        }
        let stats = cache.stats();
        assert!(stats.evictions > 0, "32 entries under a bound of 8 evict");
        assert!(
            stats.bytes <= bound,
            "{} bytes over a bound of {bound}",
            stats.bytes
        );
    }

    /// **`ln_q32` is a logarithm**, to a tolerance far finer than anything downstream can see.
    ///
    /// Floating point appears here and nowhere in the estimator: this test is the check that the
    /// integer series reproduces the function, not part of the answer.
    #[test]
    fn the_fixed_point_logarithm_matches_the_real_one() {
        for v in [1u64, 2, 3, 5, 7, 16, 100, 1023, 4096, 16383, 16384, 65535] {
            let got = ln_q32(v) as f64 / 4_294_967_296.0;
            let want = (v as f64).ln();
            assert!(
                (got - want).abs() < 1e-8,
                "ln({v}): fixed point {got}, real {want}"
            );
        }
    }

    /// **α_m by integer arithmetic is α_m.**
    #[test]
    fn the_alpha_constant_is_the_closed_form() {
        for precision in 7..=18u32 {
            let m = (1u64 << precision) as f64;
            let want = 0.7213 / (1.0 + 1.079 / m);
            let got = alpha_q32(precision) as f64 / 4_294_967_296.0;
            assert!((got - want).abs() < 1e-9, "precision {precision}: {got} vs {want}");
        }
    }

    /// **The estimate tracks the cardinality across five orders of magnitude**, including the
    /// linear-counting range and the crossover into the raw estimator.
    ///
    /// The tolerance is five standard errors of a 2¹⁴-register sketch (0.81% each), widened to a
    /// floor of two below a hundred distinct values where the linear-counting estimate rounds.
    #[test]
    fn the_sketch_estimates_a_known_cardinality() {
        for n in [0u64, 1, 2, 10, 100, 1_000, 10_000, 40_000, 100_000, 1_000_000] {
            let mut sketch = TileSketch::new();
            // Strided rather than dense, so the input is not one contiguous block: the tile
            // indices a walk emits are neither.
            for i in 0..n {
                sketch.add(i.wrapping_mul(0x9E37_79B9).wrapping_add(7));
            }
            let got = sketch.estimate();
            let slack = (n as f64 * 0.0406).max(2.0);
            assert!(
                (got as f64 - n as f64).abs() <= slack,
                "n = {n}: estimated {got}, tolerance ±{slack:.1}"
            );
        }
    }

    /// **Adding a tile twice is adding it once**, which is what makes one sketch fed by every
    /// segment the union of the segments' tile sets rather than a sum of their sizes.
    #[test]
    fn the_sketch_is_idempotent_and_order_blind() {
        let mut once = TileSketch::new();
        let mut twice = TileSketch::new();
        for i in 0..5_000u64 {
            once.add(i * 13);
        }
        for i in (0..5_000u64).rev() {
            twice.add(i * 13);
            twice.add(i * 13);
        }
        assert_eq!(once.estimate(), twice.estimate());
        assert_eq!(once.registers, twice.registers);
    }

    /// **The same input gives the same registers**, in this process and in any other: the seed is
    /// a constant and the hash is not `RandomState`'s.
    #[test]
    fn the_sketch_is_deterministic() {
        let mut a = TileSketch::new();
        let mut b = TileSketch::new();
        for i in 0..1_000u64 {
            a.add(i);
            b.add(i);
        }
        assert_eq!(a.registers, b.registers);
        // The literal is the point: a thousand distinct tiles estimate to 994 here and to 994
        // in `reference/oracle/viewport.py`, on every box and in every process.
        assert_eq!(a.estimate(), 994, "a thousand distinct tiles is a fixed answer");
    }

    /// **The `4^d` ceiling and the running maximum are the tail of both routes**, and this pins
    /// them where the ladder's own walk cannot reach: raw rungs supplied directly.
    ///
    /// The middle case is an inversion — a deeper rung reading lower than a shallower one, which
    /// the estimator produces where `N_occ` grows by less than its error and which
    /// `probes/2026-09-09-nocc-sketch` measured at sixteen cells. The last is a rung above the
    /// `4^d` tiles the grid has, which only an estimate can produce.
    #[test]
    fn the_tail_clamps_to_the_grid_and_never_lets_a_rung_fall() {
        let mut raw = [0u64; 17];
        raw[..5].copy_from_slice(&[1, 4, 16, 64, 256]);
        let plain = finish_ladder(raw, 4, false);
        assert_eq!((0..=4).map(|d| plain.at(d)).collect::<Vec<_>>(), vec![1, 4, 16, 64, 256]);

        let mut inverted = [0u64; 17];
        inverted[..5].copy_from_slice(&[1, 4, 16, 50, 49]);
        let held = finish_ladder(inverted, 4, false);
        assert_eq!(held.at(4), 50, "the running maximum holds the deeper rung at the shallower");
        assert_eq!(held.raw_at(4), 49, "and the raw rung still records the inversion");

        let mut over = [0u64; 17];
        over[..3].copy_from_slice(&[2, 9, 400]);
        let clamped = finish_ladder(over, 2, false);
        assert_eq!(
            (0..=2).map(|d| clamped.at(d)).collect::<Vec<_>>(),
            vec![1, 4, 16],
            "no rung may exceed the 4^d tiles depth d has"
        );
    }

    /// **The cross-language vectors.** `reference/oracle/occupancy.py` asserts these same three
    /// lists in `reference/tests/test_occupancy_sketch.py`, which is what makes
    /// `conformance/tests/test_i7_selection.py` an exact differential rather than one within a
    /// band. A change to the seed, the mixer, the estimator, the ceiling or the running maximum
    /// moves these numbers, and the two sides fail together rather than drifting apart.
    #[test]
    fn the_ladder_matches_the_python_oracle_vector_for_vector() {
        let cases: [(&str, Vec<u64>, [u64; 17]); 3] = [
            (
                "small",
                (0..37u64).map(|i| i * 0x0001_0001).collect(),
                [1, 1, 1, 1, 1, 1, 3, 10, 37, 37, 37, 37, 37, 37, 37, 37, 37],
            ),
            (
                "mid",
                (0..5_000u64)
                    .map(|i| i.wrapping_mul(2_654_435_761) % (1u64 << 32))
                    .collect(),
                [
                    1, 4, 16, 63, 253, 1020, 3858, 5017, 5017, 5017, 5017, 5017, 5017, 5017, 5017,
                    5017, 5017,
                ],
            ),
            (
                "big",
                (0..250_000u64)
                    .map(|i| i.wrapping_mul(48_271) % (1u64 << 32))
                    .collect(),
                [
                    1, 4, 16, 63, 253, 1020, 4079, 16333, 65536, 156628, 253239, 253239, 253239,
                    253239, 253239, 253239, 253239,
                ],
            ),
        ];
        let stride = 1usize << SKETCH_PRECISION;
        for (name, tiles, want) in cases {
            let mut plane = vec![0u8; stride * 17];
            for &tile in &tiles {
                for d in 0..=16usize {
                    let ancestor = tile >> (2 * (16 - d) as u32);
                    let (index, rank) = register_of(ancestor, SKETCH_PRECISION);
                    let slot = &mut plane[d * stride + index];
                    if rank > *slot {
                        *slot = rank;
                    }
                }
            }
            let mut running = 0u64;
            for (d, expected) in want.iter().enumerate() {
                let estimate =
                    estimate_registers(&plane[d * stride..(d + 1) * stride], SKETCH_PRECISION)
                        .min(1u64 << (2 * d as u32));
                running = running.max(estimate);
                assert_eq!(running, *expected, "{name}, depth {d}");
            }
        }
    }
}
