use super::geometry::convex_hull_of_sorted;

/// How many cells the quantising grid spans along the artifact's longer axis, before the shape is
/// computed. **1,024, and the number is chosen against what is drawn.**
///
/// **What it does.** Every construction here consumed one position per visible member to produce
/// something whose resolution is bounded by the drawing: the largest shape on the measurement layer
/// is 757 vertices over 2.42M members, drawn about a thousand pixels wide. So the members are
/// binned to a square grid over their own bounding box and the shape is computed over **one real
/// member per occupied cell** ([`quantise`]). It is a *quantisation and not a sample*: every member
/// falls in some cell, every occupied cell contributes, and every vertex is still a visible
/// member's own position, so §1's vertex property is untouched.
///
/// **Why the resolution is relative to the artifact and not to the request's zoom.** A shape drawn
/// at all is drawn at most a viewport wide, so a cell of 1/1,024 of the artifact's own longer axis
/// is at most a viewport pixel or two of displacement in the case that matters, and less than that
/// in every other. Making it depend on the request's zoom instead would key the cache on zoom, give
/// a viewer a shape that flickers as they zoom, and hand the identifier route — which carries no
/// zoom — no answer at all. The stability is worth more than the extra fidelity at a deep zoom,
/// where the client's own drawn curve is already the coarser of the two: the served ring is
/// smoothed by a periodic cubic B-spline that leaves it by up to a third of the longest adjacent
/// edge (`artifact-shapes.md` §9), and those edges are α-scale — thousands of times a cell.
///
/// **What it costs and what it buys, measured** (`tests/hull_geometry.rs`,
/// `the_quantisation_sweep`, over the 197-artifact `clusters/hdbscan` layer of `notebook-2m4` at
/// full membership, release build, one thread). 22 of the 197 artifacts are dense enough to reduce
/// at all, and between them 9,287,043 members become 1,717,984 representatives. Over those 22:
///
/// | | median | p90 | worst |
/// |---|---|---|---|
/// | boundary's departure from the unreduced shape, as a fraction of the artifact's own extent | 0.005 | 0.025 | 0.044 |
/// | area, against the unreduced shape | 1.000 | — | 0.988 … 1.007 |
///
/// α is **exactly** unchanged on every artifact of the layer ([`extreme_octagon`] is what makes
/// that true rather than nearly true). The whole layer's digging goes **1,759 ms → 862 ms**, one
/// artifact's shape from p50 1.7 ms / p90 20.9 ms / worst 265 ms to **p50 1.6 ms / p90 14.4 ms /
/// worst 84 ms**, and the corpus root — 2,422,486 members, which reduce to 139,732 — from 167 ms to
/// 43 ms with an area ratio of 1.0000 and no measurable departure at all.
///
/// **Where it is not invisible, stated rather than averaged away.** At the median the departure is
/// half a percent of the artifact's extent, which is a pixel or two. On one artifact of the 197 it
/// is 4.4%: a single concavity that the unreduced dig opens and the reduced one does not, because
/// the members that would have been dug to are no longer candidates. The area is within 1.2% there,
/// so it is one notch rather than a shape that has moved. Below 1,024 divisions that case gets
/// common enough to matter — at 512 the worst departure is 17% of an artifact's extent — which is
/// what fixes the resolution here rather than lower, where the time would be better.
///
/// **The residual, stated rather than smoothed over.** A member may now fall outside its own
/// artifact's shape, by at most one cell — the owner's ruling of 2026-08-28
/// (`artifact-shapes.md` §4's head) is what permits it, containment having been a bar the shape no
/// longer has to clear. Measured over the layer: 1,965 member positions of 12,808,679, and at most
/// 0.09% of any one artifact's members.
pub(super) const QUANTISE_DIVISIONS: u32 = 1_024;

/// How many visible members an artifact needs before its shape is computed over representatives
/// rather than over every one of them.
///
/// **100,000, and the floor is about what is drawn rather than about what the reduction costs.**
/// The reduction moves a shape's boundary by up to a cell and puts a small fraction of the members
/// outside their own outline (§7.1), which is invisible on an artifact drawn a viewport wide and is
/// not what a viewer who has zoomed into a small cluster is looking at. Below the floor the exact
/// shape is affordable — a *measured* 0.5 ms at under 10,000 members and 5 ms at 50,000 — so there
/// is nothing to buy with the fidelity.
///
/// **Without a floor the reduction reaches artifacts it has nothing to offer.** Measured over the
/// four layers of `notebook-2m4`: `clusters/kmeans` (13,658 … 73,360 members) put 6.7% of one
/// artifact's members outside its shape and lost 45 of its 190 rings, `clusters/toponymy` level 3
/// (1,211 … 11,682) 6.5% of one, and `topics/hdbscan` — 200 members an artifact — 5.0% of one, for
/// 49 ms, 2 ms and nothing across the three layers. The floor is where that stops.
pub(super) const REDUCTION_FLOOR: usize = 75_000;

/// [`QUANTISE_DIVISIONS`], for the measurement seams — the family comparison in
/// `tests/hull_triangulation.rs` has to give the triangulated route the same reduced input the dig
/// receives, or it is comparing two constructions over two different clouds.
#[doc(hidden)]
pub const SERVED_QUANTISE_DIVISIONS: u32 = QUANTISE_DIVISIONS;

/// The representatives [`dig_rings_at`](super::dig_rings_at) would compute a shape over — **the third measurement seam,
/// and public for [`dig_rings`](super::dig_rings)'s reason**.
///
/// The sweep has to ask what binning did to α, which is a statistic of the representatives' own
/// convex wrap, and a test binary that rebuilt the cell arithmetic from the constant would be
/// measuring its own copy of it.
#[doc(hidden)]
pub fn quantised(points: &[[u32; 2]], divisions: u32) -> Option<Vec<[u32; 2]>> {
    quantise(points, divisions, REDUCTION_FLOOR, None).map(|(reduced, _)| reduced)
}

/// One real member per occupied cell of a square grid over the members' own bounding box, or `None`
/// where the grid cannot reduce the input — see [`QUANTISE_DIVISIONS`] for what this is for.
///
/// **The representative is the member nearest its cell's centre**, ties broken on the position
/// itself, which keeps the choice a function of the member positions alone rather than of the order
/// they were gathered in. The cheaper rule — the lexicographically smallest member of the cell —
/// was declined because its error is *directional*: the left edge of a cloud would be exact and the
/// right edge would pull inward by a cell, so the shape would shrink rather than blur.
///
/// **`None` where the grid holds at least as many cells as there are members**, which is both the
/// case where there is nothing to gain and the case where the grid would cost more memory than the
/// input it is reducing. Small artifacts therefore take the unquantised path, which is the right
/// answer twice over: they are the cheap ones (a *measured* 0.5 ms at under 10,000 members), and
/// they are the ones a viewer zooms into.
///
/// The cell side is the same on both axes so that a long thin cloud is not stretched, and it is
/// derived from the longer axis so that the shorter one is never binned more coarsely than
/// [`QUANTISE_DIVISIONS`] asks for.
///
/// `floor` is [`REDUCTION_FLOOR`] on every serving route. It is a parameter rather than a constant
/// read here because the reduction's own properties — a representative is a member, every member
/// has one within a cell, α survives, the answer does not depend on the arrival order — hold at any
/// size and are tested on clouds small enough to check exhaustively, where the served floor would
/// switch the reduction off and leave nothing to assert about.
///
/// `bounds` is `points`'s own bounding box where a caller has already traversed them
/// ([`concave_rings`](super::dig::concave_rings)), and `None` where it has not; it changes no answer, only the number of
/// passes.
///
/// The cell side is returned beside the representatives because it is the input's own resolution,
/// which is what a [`DigFloor`](super::DigFloor) is measured in.
pub(super) fn quantise(
    points: &[[u32; 2]],
    divisions: u32,
    floor: usize,
    bounds: Option<[u32; 4]>,
) -> Option<(Vec<[u32; 2]>, u64)> {
    let runs = Runs::fold(points, grid_shift(points, divisions, floor, bounds)?);

    // A run count is the occupied-cell count exactly where the runs ascend, and an upper bound on
    // it otherwise, since merging equal keys only ever removes cells. So the ordered case — every
    // serving route, a view's rows being Morton rank — answers the test here and leaves the
    // artifact without paying for a representative it is about to throw away.
    if runs.descents == 0 && occupancy_loses(runs.len(), points.len()) {
        return None;
    }

    let (reps, dists) = runs.representatives();
    let merged = match runs.descents {
        0 => None,
        _ => Some(runs.merge(&reps, &dists)?),
    };
    let cell_count = merged.as_ref().map_or(runs.len(), |m| m.len());

    let octagon = runs.extreme_octagon_over(&reps);
    // The representatives are the bulk of what this returns, so they are the vector the candidates
    // are appended to rather than a second one copied into it.
    let mut out = merged.unwrap_or(reps);
    out.reserve(cell_count / 8);
    runs.push_hull_candidates(&octagon, &mut out);

    // Order and duplicates are left for the caller's sort and dedup, which every route into this
    // already pays: a member may be both its cell's representative and a hull candidate. What this
    // returns as a *set* is a function of the member positions alone — the fold's representative
    // rule is order-independent and the merge above restores it where the runs were not ordered —
    // and the set is what the shape is computed from.
    Some((out, runs.side))
}

/// The binning grid's cell side as a shift, or `None` where the reduction is refused before a cell
/// is ever computed — see [`quantise`] for what each refusal is.
fn grid_shift(
    points: &[[u32; 2]],
    divisions: u32,
    floor: usize,
    bounds: Option<[u32; 4]>,
) -> Option<u32> {
    // The squared distance below is `u64`, and the bound that keeps it from overflowing is that a
    // cell side is at most `2^32 / divisions` rounded up to a power of two: at 16 divisions a
    // half-side is under 2^29 and its square under 2^58. Every caller is the constant or the sweep,
    // both far above that.
    // `0` is the seam's "no quantisation at all", and every other caller is far above 16.
    if divisions == 0 || points.len() < floor.max(4) {
        return None;
    }
    debug_assert!(
        divisions >= 16,
        "the cell-centre distance is bounded by the side"
    );
    if divisions < 16 {
        return None;
    }
    let [x0, y0, x1, y1] = bounds.unwrap_or_else(|| {
        let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0u32, 0u32);
        for q in points {
            x0 = x0.min(q[0]);
            y0 = y0.min(q[1]);
            x1 = x1.max(q[0]);
            y1 = y1.max(q[1]);
        }
        [x0, y0, x1, y1]
    });
    debug_assert!(
        [
            points.iter().map(|q| q[0]).min(),
            points.iter().map(|q| q[1]).min(),
            points.iter().map(|q| q[0]).max(),
            points.iter().map(|q| q[1]).max(),
        ] == [Some(x0), Some(y0), Some(x1), Some(y1)],
        "a supplied box must be the members' own, tightly: the grid is scaled from it"
    );
    let (wx, wy) = ((x1 - x0) as u64 + 1, (y1 - y0) as u64 + 1);
    // **A power of two, so a cell index is a shift rather than a division.** Two integer divisions
    // per member is a real cost at these sizes — this pass is the one thing every member is still
    // read for — and rounding the side up only ever makes the grid coarser than `divisions` asked,
    // never finer, so the resolution argument is unaffected.
    let shift = wx
        .max(wy)
        .div_ceil(divisions as u64)
        .next_power_of_two()
        .trailing_zeros();
    if 1u64 << shift <= 1 {
        // The grid is already the position lattice, so binning is the identity on a deduplicated
        // input and buys nothing.
        return None;
    }
    Some(shift)
}

/// **Nothing gained is reported as nothing done.** Where the members are spread thinner than the
/// grid, the occupied cells are nearly as numerous as the members and the representatives are
/// very nearly the input again — the same shape computed over a vector rebuilt for no reason.
/// Three quarters is where the reduction starts to return more than the sort it saves; below it
/// the artifact takes the exact path, which is the right answer twice over, since those are the
/// cheap ones (a *measured* 0.5 ms at under 10,000 members) and the ones a viewer zooms into.
///
/// **The fold is what makes this an exact test rather than a guess about the grid.** What it
/// replaces asked whether the grid held more than four cells per member and skipped the
/// reduction when it did — a proxy for occupancy that put **170 of the 197** artifacts of the
/// measurement layer on the unreduced path at 1,024 divisions, which are the artifacts the shape
/// spends its time on (`artifact-shapes.md` §7.1). Counting the occupied cells costs one pass
/// and answers the question the proxy was standing in for.
fn occupancy_loses(cells: usize, members: usize) -> bool {
    cells * 4 > members * 3
}

/// The members grouped into the runs the fold found: one run per cell boundary the input crosses,
/// each run's members contiguous in `points`.
struct Runs<'a> {
    points: &'a [[u32; 2]],
    /// Where each run begins in `points`; it ends where the next begins.
    starts: Vec<u32>,
    shift: u32,
    side: u64,
    /// How many runs opened a cell whose key is below its predecessor's — the count that decides
    /// whether a cell can have been met twice.
    descents: usize,
}

impl<'a> Runs<'a> {
    /// **A cell is a contiguous row range, so the occupied cells are found by folding runs rather
    /// than by binning into a grid** — the reduction's whole cost, on the input every route into it
    /// actually supplies.
    ///
    /// The grid is anchored at the corpus's own origin rather than at the artifact's bounding box,
    /// which makes each cell a Morton block of the corpus grid whenever its side is at least one
    /// Morton cell. Rows are Morton rank (`architecture.md` §5.2, and `tile_index.rs`'s opening for
    /// what else rests on it) and members reach here in row order, so the members of such a cell are
    /// **consecutive**: one comparison per member finds every cell boundary, and nothing is
    /// allocated for a cell that is not occupied. The anchor is the only thing that had to change to
    /// make that true — an artifact-anchored cell straddles Morton blocks, and its members do not
    /// arrive together.
    ///
    /// **What this replaces, and why three obvious routes lost.** The dense `nx × ny` grid it
    /// replaces is *measured* at 215 ms of binning over the measurement layer, nearly all of it
    /// faulting in a grid whose occupied fraction is small; a hash map keyed on the cell index is
    /// 289 ms, a hash a member; indexing the dense grid in Morton order so its writes are local is
    /// 260 ms, the locality bought back by a grid four times the size. Folding runs is one
    /// comparison a member and no grid at all.
    ///
    /// **A *jump* per cell — the route the row-range property most obviously suggests — was refused
    /// on measurement, and this is where the crossover is.** Skipping to the next cell by bitmap
    /// arithmetic, a gallop over the segment's Morton column and a `reset_at_or_after` on the mask,
    /// costs on the order of ten probes and a container walk against one sequential comparison per
    /// member here. It can only win where a cell holds more members than that, and on
    /// `notebook-2m4` it is not close: 12,808,679 members occupy 12,560,851 distinct Morton cells —
    /// a **ratio of 1.02**, the corpus grid being 2^16 × 2^16 against 2.4M items — and at the served
    /// resolution the densest artifact of the measurement layer holds 17.4 members to a cell against
    /// a layer mean of 2.5 (`tests/hull_geometry.rs`, `the_cell_occupancy`). The jump becomes the
    /// cheaper route somewhere around a few tens of members a cell, which is a corpus two orders of
    /// magnitude denser than this one.
    ///
    /// **The fold reads a cell index and nothing else.** A cell at this resolution is the top bits
    /// of a member's position on both axes — the top bits of its Morton code, the grid being
    /// anchored at the corpus origin — so finding where one cell ends and the next begins is two
    /// shifts and a comparison a member.
    fn fold(points: &'a [[u32; 2]], shift: u32) -> Runs<'a> {
        let mut starts: Vec<u32> = Vec::with_capacity(points.len() / 4 + 1);
        // A run out of Morton order is a cell that may already have been seen — row order restarts
        // at each segment of the view, and a caller outside the serving path need not be ordered at
        // all. Counted rather than assumed, and what it costs is one sort below.
        let mut descents = 0usize;
        let mut last = (u64::MAX, u64::MAX);
        let mut last_key = 0u64;
        for (i, q) in points.iter().enumerate() {
            let cell = ((q[0] as u64) >> shift, (q[1] as u64) >> shift);
            if cell != last {
                let key = interleave_cell(cell.0, cell.1);
                if key < last_key {
                    descents += 1;
                }
                last_key = key;
                starts.push(i as u32);
                last = cell;
            }
        }
        Runs {
            points,
            starts,
            shift,
            side: 1u64 << shift,
            descents,
        }
    }

    fn len(&self) -> usize {
        self.starts.len()
    }

    /// Run `i`'s members, which all share one cell.
    fn members(&self, i: usize) -> &'a [[u32; 2]] {
        let lo = self.starts[i] as usize;
        let hi = self
            .starts
            .get(i + 1)
            .map_or(self.points.len(), |&s| s as usize);
        &self.points[lo..hi]
    }

    /// The low corner of run `i`'s cell: the first member's position with its sub-cell bits
    /// cleared, which is the cell index without a cell index having to be carried.
    fn cell_corner(&self, i: usize) -> (u64, u64) {
        let q = self.points[self.starts[i] as usize];
        (
            ((q[0] as u64) >> self.shift) << self.shift,
            ((q[1] as u64) >> self.shift) << self.shift,
        )
    }

    /// **One member per run, the one nearest its cell's centre, ties broken on the position** — the
    /// rule stated at [`quantised`], answered per run rather than per member. All of a run's members
    /// share a cell, so the centre is computed once for the run and not once for each of them, and a
    /// run of one has no comparison to make at all.
    ///
    /// The distance arithmetic runs only for the members of a cell that holds more than one: over
    /// the measurement layer that is 9.5M of 12.8M members, and on an artifact the reduction then
    /// declines it is none of them, because the occupancy test is answered before a single distance
    /// is computed.
    ///
    /// The squared distances are returned beside the representatives only where a cell can be met
    /// twice, which is the one case that has to compare two runs' answers.
    fn representatives(&self) -> (Vec<[u32; 2]>, Vec<u64>) {
        let half = self.side / 2;
        let n = self.len();
        let mut reps: Vec<[u32; 2]> = Vec::with_capacity(n + n / 8);
        let mut dists: Vec<u64> = Vec::with_capacity(if self.descents > 0 { n } else { 0 });
        for i in 0..n {
            let members = self.members(i);
            let (mut rep, mut best) = (members[0], u64::MAX);
            if members.len() > 1 || self.descents > 0 {
                let (cx, cy) = ((rep[0] as u64) >> self.shift, (rep[1] as u64) >> self.shift);
                let (mx, my) = ((cx << self.shift) + half, (cy << self.shift) + half);
                for q in members {
                    let (dx, dy) = ((q[0] as u64).abs_diff(mx), (q[1] as u64).abs_diff(my));
                    let d = dx * dx + dy * dy;
                    if d < best || (d == best && *q < rep) {
                        (rep, best) = (*q, d);
                    }
                }
            }
            reps.push(rep);
            if self.descents > 0 {
                dists.push(best);
            }
        }
        (reps, dists)
    }

    /// **One representative per occupied cell, and the same one whatever order the members arrived
    /// in.** Where the runs were in Morton order each cell is exactly one run and there is nothing
    /// to merge, and this is not called at all. Where they were not, merging equal keys under the
    /// rule the fold applies — nearest the cell's centre, ties on the position — gives exactly what
    /// binning every member into a grid would have given, because that rule is associative: a
    /// cell's representative is the representative of its runs' representatives.
    ///
    /// `None` where the merged cells are too nearly as numerous as the members to be worth
    /// returning ([`occupancy_loses`]), which is the test the ordered case answers off the run
    /// count alone.
    fn merge(&self, reps: &[[u32; 2]], dists: &[u64]) -> Option<Vec<[u32; 2]>> {
        let mut merged: Vec<(u64, u64, [u32; 2])> = (0..self.len())
            .map(|i| {
                let q = self.points[self.starts[i] as usize];
                let key = interleave_cell((q[0] as u64) >> self.shift, (q[1] as u64) >> self.shift);
                (key, dists[i], reps[i])
            })
            .collect();
        merged.sort_unstable();
        merged.dedup_by_key(|r| r.0);
        if occupancy_loses(merged.len(), self.points.len()) {
            return None;
        }
        Some(merged.into_iter().map(|(_, _, q)| q).collect())
    }

    /// **The hull candidates are found per *cell*, so no member is tested on its own.** α is three
    /// times the median edge of the whole membership's convex wrap, so every member that could be a
    /// wrap vertex has to survive the reduction ([`extreme_octagon`]) — and testing 12.8M members
    /// against eight edges is *measured* at 68 ms over the layer, on top of the 49 ms the eight
    /// extremes cost in a pass of their own.
    ///
    /// **The filter is exact.** `strictly_inside` is a conjunction of half-planes, so it is convex:
    /// a cell whose four corners are all strictly inside holds nothing that is not, and can be
    /// skipped whole. The cells that remain are the octagon's own boundary band.
    fn push_hull_candidates(&self, octagon: &Octagon, out: &mut Vec<[u32; 2]>) {
        for i in 0..self.len() {
            let (cx0, cy0) = self.cell_corner(i);
            if octagon.cell_strictly_inside(cx0, cy0, self.side) {
                continue;
            }
            for q in self.members(i) {
                if !octagon.strictly_inside(*q) {
                    out.push(*q);
                }
            }
        }
    }
}

/// The two cell coordinates interleaved — the key [`quantise`] folds runs on.
///
/// The standard bit-spreading, five shift-or-and steps an axis rather than a loop. Ordering by this
/// key is Morton order over the binning grid, which is what makes "this run is out of order" a
/// single comparison. [`tessera_spatial::morton::interleave`] is the same operation over the
/// *corpus* grid at 16 bits an axis; this one is over an artifact's binning grid, whose coordinates
/// run to 32 bits where the cell side is small, so it is a `u64` and not that function.
pub(super) fn interleave_cell(cx: u64, cy: u64) -> u64 {
    fn spread(mut v: u64) -> u64 {
        v &= 0x0000_0000_ffff_ffff;
        v = (v | (v << 16)) & 0x0000_ffff_0000_ffff;
        v = (v | (v << 8)) & 0x00ff_00ff_00ff_00ff;
        v = (v | (v << 4)) & 0x0f0f_0f0f_0f0f_0f0f;
        v = (v | (v << 2)) & 0x3333_3333_3333_3333;
        v = (v | (v << 1)) & 0x5555_5555_5555_5555;
        v
    }
    spread(cx) | (spread(cy) << 1)
}

/// [`extreme_octagon`] over the folded cells — the same eight extremes and the same polygon, found
/// without a pass over the members.
///
/// **The cells bound the members, so the search is a band and not a sweep.** For each supporting
/// direction the best representative is a lower bound on the extreme's score, and a cell whose
/// furthest corner scores below that bound cannot hold a member that beats it. What is left is the
/// cells within one cell of the supporting line, and only their members are read — so the answer is
/// the exact extreme of the whole membership, arrived at by reading a band rather than everything.
///
/// Ties are broken on the position exactly as [`extreme_octagon`] breaks them, so the two functions
/// return the same polygon for the same members; `tests::the_octagon_over_cells_is_the_octagon`
/// pins that on inputs the band and the sweep disagree about if the bound is wrong.
impl Runs<'_> {
    fn extreme_octagon_over(&self, reps: &[[u32; 2]]) -> Octagon {
        // Every representative is a member, so the best of them is a lower bound on each extreme —
        // established over every cell before any cell is opened, so the band below is as thin as
        // the representatives can make it.
        let mut best: [Option<(i64, [u32; 2])>; 8] = [None; 8];
        for rep in reps {
            let (x, y) = (rep[0] as i64, rep[1] as i64);
            for (slot, (wx, wy)) in best.iter_mut().zip(OCTAGON_DIRECTIONS) {
                let score = wx * x + wy * y;
                if slot.is_none_or(|(s, b)| score > s || (score == s && *rep < b)) {
                    *slot = Some((score, *rep));
                }
            }
        }
        let s1 = self.side as i64 - 1;
        for i in 0..self.len() {
            // The cell's furthest corner in each direction is its low corner plus `side - 1` on
            // whichever axes that direction is positive on. A cell whose furthest corner scores
            // below the bound holds nothing that can beat it, in that direction or — over all
            // eight — at all.
            let (cx0, cy0) = self.cell_corner(i);
            let (lo_x, lo_y) = (cx0 as i64, cy0 as i64);
            let interesting = best
                .iter()
                .zip(OCTAGON_DIRECTIONS)
                .any(|(slot, (wx, wy))| {
                    let cx = lo_x + if wx > 0 { s1 } else { 0 };
                    let cy = lo_y + if wy > 0 { s1 } else { 0 };
                    wx * cx + wy * cy >= slot.map_or(i64::MIN, |(s, _)| s)
                });
            if !interesting {
                continue;
            }
            for q in self.members(i) {
                let (x, y) = (q[0] as i64, q[1] as i64);
                for (slot, (wx, wy)) in best.iter_mut().zip(OCTAGON_DIRECTIONS) {
                    let score = wx * x + wy * y;
                    if slot.is_none_or(|(s, b)| score > s || (score == s && *q < b)) {
                        *slot = Some((score, *q));
                    }
                }
            }
        }
        octagon_of(best)
    }
}

/// The eight supporting directions, as `(wx, wy)` in `wx·x + wy·y`.
const OCTAGON_DIRECTIONS: [(i64, i64); 8] = [
    (1, 0),
    (-1, 0),
    (0, 1),
    (0, -1),
    (1, 1),
    (1, -1),
    (-1, 1),
    (-1, -1),
];

/// The polygon eight extremes span, as one linear form per edge — shared by
/// [`extreme_octagon`] and [`extreme_octagon_over`] so the two cannot drift apart.
fn octagon_of(best: [Option<(i64, [u32; 2])>; 8]) -> Octagon {
    let mut extremes: Vec<[u32; 2]> = best.into_iter().flatten().map(|(_, q)| q).collect();
    extremes.sort_unstable();
    extremes.dedup();
    // `A·x + B·y + C` per edge, which is the same cross product with the vertex subtracted out
    // once rather than per member — two multiplications instead of four, on the one test every
    // member takes.
    let ring = convex_hull_of_sorted(&extremes);
    let edges = (0..ring.len())
        .map(|i| {
            let (a, b) = (ring[i], ring[(i + 1) % ring.len()]);
            let (ax, ay) = (a[0] as f64, a[1] as f64);
            let (bx, by) = (b[0] as f64, b[1] as f64);
            [-(by - ay), bx - ax, (by - ay) * ax - (bx - ax) * ay]
        })
        .collect();
    Octagon {
        edges,
        degenerate: ring.len() < 3,
    }
}

/// **α must not move when the input is reduced, so every member that could be a convex-hull vertex
/// survives [`quantise`] whether or not it is its cell's representative.**
///
/// α is three times the median edge of the visible members' *own* convex wrap, and that statistic
/// is a function of the sampling density rather than only of the cloud: the hull of a sparser
/// sample of the same region has fewer vertices and longer edges. Measured over the 197-artifact
/// layer at a resolution of 512, computing the wrap over the representatives alone moved α by up to
/// **3.6×** on one artifact, which took its shape from 0.29 to 1.29 of the unquantised one's area —
/// a visibly different shape rather than a blurred one. Carrying the candidates removes the drift
/// at its source: the wrap of `representatives ∪ candidates` **is** the wrap of every member, so α
/// is not approximated at all.
///
/// The filter is Akl–Toussaint's: a member strictly inside the polygon spanned by the extremes of
/// `x`, `y`, `x + y` and `x − y` is inside the hull of those eight members and so cannot be a hull
/// vertex. It costs one pass and discards the interior, which on this corpus is *measured* at 94% …
/// 99.9% of a large artifact's members.
///
/// **`f64` here, and it is the only inexact arithmetic in this module's construction — with a
/// margin that makes the answer exact anyway.** Grid coordinates are below 2^32 and exact in `f64`,
/// so each cross product carries an absolute error under 2^13; a member is discarded only when
/// every edge puts it more than [`OCTAGON_MARGIN`] inside, which is eight times that bound. A
/// member near an edge is therefore *kept*, and a kept member costs a slot in a vector that is
/// about to be sorted. There is no rounding under which a hull vertex is discarded, so the wrap —
/// and α, and the shape — stay exactly what the exact monotone chain makes of the whole membership.
///
/// **The set it keeps is also identical on every platform**, which is what the shape being a
/// function of the member positions alone requires (§1): every operation here is an IEEE-754
/// multiply, add or compare on values a `f64` represents exactly, all correctly rounded and none
/// contracted, so a member kept on one machine is kept on every machine. Soundness would hold
/// without that; determinism would not, because a kept member is a candidate the dig can dig to.
#[cfg(test)]
fn extreme_octagon(points: &[[u32; 2]]) -> Octagon {
    // One pass for all eight, not one pass each: the members are read once here and once again to
    // bin them, and a third to eighth pass over a 2.4M-member cloud is the cost this whole
    // construction is about.
    let mut best: [Option<(i64, [u32; 2])>; 8] = [None; 8];
    for q in points {
        let (x, y) = (q[0] as i64, q[1] as i64);
        for (slot, (wx, wy)) in best.iter_mut().zip(OCTAGON_DIRECTIONS) {
            let score = wx * x + wy * y;
            // Ties broken on the position, so the octagon is a function of the member positions
            // rather than of the order they were gathered in.
            if slot.is_none_or(|(s, b)| score > s || (score == s && *q < b)) {
                *slot = Some((score, *q));
            }
        }
    }
    octagon_of(best)
}

/// The polygon spanned by the eight extremes, as one linear form per edge — see
/// [`extreme_octagon`].
struct Octagon {
    edges: Vec<[f64; 3]>,
    /// A polygon of fewer than three vertices encloses nothing, so every member is a candidate.
    degenerate: bool,
}

impl Octagon {
    /// Whether `q` is inside every edge by more than [`OCTAGON_MARGIN`].
    fn strictly_inside(&self, q: [u32; 2]) -> bool {
        if self.degenerate {
            return false;
        }
        let (x, y) = (q[0] as f64, q[1] as f64);
        self.edges
            .iter()
            .all(|[a, b, c]| a * x + b * y + c > OCTAGON_MARGIN)
    }

    /// Whether every position of the cell `[lo_x, lo_x + side) × [lo_y, lo_y + side)` is
    /// [`strictly_inside`](Self::strictly_inside) — the same answer as asking it of all four
    /// corners, at one corner's cost.
    ///
    /// **An edge's form is linear and separable, so its minimum over a box is at the corner the
    /// signs of `a` and `b` pick out**, and asking the other three adds nothing: floating-point
    /// addition and multiplication are monotone, so the corner that minimises the exact value
    /// minimises the rounded one too. The answer is therefore *identical* to the four-corner test
    /// rather than an approximation of it, which matters because this decides which cells' members
    /// are read for hull candidates.
    fn cell_strictly_inside(&self, lo_x: u64, lo_y: u64, side: u64) -> bool {
        if self.degenerate {
            return false;
        }
        let (x0, y0) = (lo_x as f64, lo_y as f64);
        let s1 = (side - 1) as f64;
        self.edges.iter().all(|[a, b, c]| {
            let x = if *a < 0.0 { x0 + s1 } else { x0 };
            let y = if *b < 0.0 { y0 + s1 } else { y0 };
            a * x + b * y + c > OCTAGON_MARGIN
        })
    }
}

/// How far inside every edge a member must be before [`extreme_octagon`]'s filter discards it, in
/// cross-product units. 2^16, against a *worst-case* `f64` error under 2^13 on a grid of 2^32 —
/// see [`extreme_octagon`] for why a margin makes an inexact test an exact answer.
const OCTAGON_MARGIN: f64 = 65_536.0;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::derived::dig::{bridge_threshold, concave_rings, dig_rings_at, DIG_BUDGET};
    use crate::derived::geometry::convex_hull;
    use crate::derived::test_support::*;

    /// **The set the fold returns is the set a dense binning would have returned**, cell for cell
    /// and candidate for candidate — the assertion that makes the route to the occupied cells a
    /// speed change rather than a shape change.
    ///
    /// The oracle here is written from `artifact-shapes.md` §7.1 and shares nothing with
    /// [`quantise`] but the cell arithmetic the resolution defines: it puts every member into a map
    /// keyed on its cell rather than folding runs, and tests every member against the octagon on
    /// its own rather than opening a band of cells. Three clouds, because the routes differ in what
    /// they do with *order*: one in Morton order, one rotated so the fold meets a cell twice, and
    /// one whose members are spread thinly enough that the reduction declines.
    ///
    /// `tests/hull_geometry.rs`'s `the_reduction_is_the_definition` is the same assertion over the
    /// 197 real memberships of the measurement layer, and it requires the *rings* to match as well.
    #[test]
    fn the_fold_returns_what_a_dense_binning_would_have() {
        /// One member per occupied cell and every hull candidate, the obvious way.
        fn dense(points: &[[u32; 2]], divisions: u32) -> Option<Vec<[u32; 2]>> {
            use std::collections::HashMap;
            let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0u32, 0u32);
            for q in points {
                x0 = x0.min(q[0]);
                y0 = y0.min(q[1]);
                x1 = x1.max(q[0]);
                y1 = y1.max(q[1]);
            }
            let (wx, wy) = ((x1 - x0) as u64 + 1, (y1 - y0) as u64 + 1);
            let shift = wx
                .max(wy)
                .div_ceil(divisions as u64)
                .next_power_of_two()
                .trailing_zeros();
            let side = 1u64 << shift;
            if side <= 1 {
                return None;
            }
            let half = side / 2;
            let mut cells: HashMap<(u64, u64), ([u32; 2], u64)> = HashMap::new();
            for q in points {
                let (cx, cy) = ((q[0] as u64) >> shift, (q[1] as u64) >> shift);
                let (mx, my) = ((cx << shift) + half, (cy << shift) + half);
                let (dx, dy) = ((q[0] as u64).abs_diff(mx), (q[1] as u64).abs_diff(my));
                let d = dx * dx + dy * dy;
                cells
                    .entry((cx, cy))
                    .and_modify(|best| {
                        if d < best.1 || (d == best.1 && *q < best.0) {
                            *best = (*q, d);
                        }
                    })
                    .or_insert((*q, d));
            }
            if cells.len() * 4 > points.len() * 3 {
                return None;
            }
            let octagon = extreme_octagon(points);
            let mut out: Vec<[u32; 2]> = cells.values().map(|(rep, _)| *rep).collect();
            for q in points {
                if !octagon.strictly_inside(*q) {
                    out.push(*q);
                }
            }
            Some(out)
        }

        let normalise = |v: Option<Vec<[u32; 2]>>| {
            v.map(|mut v| {
                v.sort_unstable();
                v.dedup();
                v
            })
        };

        let dense_cloud = sample(20_000, 5_000, |x, y| x * x + y * y <= 5_000 * 5_000);
        // A rotation the fold meets as one descent, which is what a view's second segment looks
        // like to it: a cell it has already folded arrives again.
        let mut shuffled = dense_cloud.clone();
        shuffled.rotate_left(dense_cloud.len() / 3);
        let thin = sample(400, 5_000, |x, y| x * x + y * y <= 5_000 * 5_000);

        for (name, cloud, divisions) in [
            ("morton order", &dense_cloud, 32u32),
            ("a cell met twice", &shuffled, 32),
            ("too thin to reduce", &thin, 1_024),
        ] {
            assert_eq!(
                normalise(quantise(cloud, divisions, 0, None).map(|(r, _)| r)),
                normalise(dense(cloud, divisions)),
                "{name}: the fold and a dense binning disagree"
            );
        }
        assert!(
            quantise(&dense_cloud, 32, 0, None).is_some(),
            "the dense cloud must exercise the reduced path"
        );
        assert!(
            quantise(&thin, 1_024, 0, None).is_none(),
            "the thin cloud must exercise the declining path"
        );
    }

    /// **The reduction is a quantisation and not a sample**, which is the property the vertex
    /// guarantee rests on: every representative is a member's own position, and every member has a
    /// representative within a cell of it.
    ///
    /// Run at a coarse resolution rather than the served one, because the properties do not depend
    /// on the resolution and a cloud dense enough to reduce at 1,024 divisions is millions of
    /// members. The second half is what makes the escape bound in
    /// `the_budget_is_shared_across_the_rings` a bound rather than an observation.
    #[test]
    fn every_representative_is_a_member_and_every_member_has_one() {
        let members = sample(5_000, 5_000, |x, y| x * x + y * y <= 5_000 * 5_000);
        let reduced = quantise(&members, 32, 0, None).expect("a cloud this dense reduces").0;
        assert!(
            reduced.len() * 3 < members.len(),
            "no reduction: {} of {}",
            reduced.len(),
            members.len()
        );

        let held: std::collections::HashSet<[u32; 2]> = members.iter().copied().collect();
        assert!(
            reduced.iter().all(|q| held.contains(q)),
            "a representative is not a member's own position"
        );
        let bound = cell_bound(&members, 32);
        assert!(
            members.iter().all(|m| reduced
                .iter()
                .any(|k| point_to_segment(*k, *k, *m) <= bound)),
            "a member has no representative within a cell"
        );
    }

    /// **α does not move when the input is reduced**, because every member that could be a convex
    /// hull vertex survives the reduction whatever cell it falls in ([`extreme_octagon`]).
    ///
    /// α is three times the median edge of the members' own wrap, and that statistic follows the
    /// sampling density — measured on the corpus, taking it over the representatives alone moved it
    /// by up to 3.6× and changed the shape rather than blurring it.
    #[test]
    fn the_reduction_leaves_the_convex_wrap_and_so_alpha_exact() {
        for cloud in [
            sample(20_000, 5_000, |x, y| x * x + y * y <= 5_000 * 5_000),
            sample(20_000, 5_000, |x, y| x.abs() + y.abs() <= 5_000),
            sample(20_000, 5_000, |x, y| {
                x * x + y * y <= 5_000 * 5_000 && (x < 0 || y.abs() > 2_000)
            }),
        ] {
            let reduced = quantise(&cloud, 32, 0, None).expect("a cloud this dense reduces").0;
            assert_eq!(
                convex_hull(&reduced),
                convex_hull(&cloud),
                "the reduction lost a convex-hull vertex"
            );
            assert_eq!(
                bridge_threshold(&convex_hull(&reduced)),
                bridge_threshold(&convex_hull(&cloud)),
                "α moved"
            );
        }
    }

    /// A membership small enough that the grid cannot reduce it is dug over every member, so the
    /// shape is exactly what it was before the reduction existed — which is what keeps the
    /// artifacts a viewer zooms into at full fidelity.
    #[test]
    fn a_small_membership_is_not_reduced_at_all() {
        let members = moon();
        assert!(quantise(&members, QUANTISE_DIVISIONS, REDUCTION_FLOOR, None).is_none());
        assert_eq!(
            concave_rings(&members, None),
            dig_rings_at(&members, DIG_BUDGET, 0).0
        );
    }

    /// The reduction is a function of the member positions, not of the order they arrive in — the
    /// cell's representative is the member nearest its centre, ties broken on the position itself.
    #[test]
    fn the_reduction_does_not_depend_on_the_order_the_members_arrive_in() {
        let members = sample(20_000, 5_000, |x, y| x * x + y * y <= 5_000 * 5_000);
        let mut shuffled = members.clone();
        shuffled.reverse();
        let mut a = quantise(&members, 32, 0, None).expect("reduces").0;
        let mut b = quantise(&shuffled, 32, 0, None).expect("reduces").0;
        a.sort_unstable();
        b.sort_unstable();
        assert_eq!(a, b);
    }

    /// **The band the octagon is found over is the whole membership's octagon, not an
    /// approximation of it.** [`extreme_octagon_over`] reads a cell's members only where that
    /// cell's furthest corner could still beat the best representative, and the property that
    /// justifies the skip is the one this pins: on every cloud below, the polygon it returns is the
    /// polygon a pass over every member returns.
    ///
    /// The clouds are chosen where a wrong bound would show. A ring puts every extreme in a cell
    /// whose representative is far from it; a diagonal band is extreme in four directions the axes
    /// do not see; a cloud with one far outlier makes seven of the eight bounds loose at once; and a
    /// cloud in convex position has no interior for a slack bound to hide in.
    #[test]
    fn the_octagon_over_cells_is_the_octagon_over_the_members() {
        let clouds: Vec<(&str, Vec<[u32; 2]>)> = vec![
            (
                "disc",
                sample(20_000, 5_000, |x, y| x * x + y * y <= 5_000 * 5_000),
            ),
            (
                "ring",
                sample(20_000, 5_000, |x, y| {
                    let r = x * x + y * y;
                    (4_000 * 4_000..=5_000 * 5_000).contains(&r)
                }),
            ),
            (
                "diagonal band",
                sample(20_000, 5_000, |x, y| (x - y).abs() < 400),
            ),
            ("square", sample(20_000, 5_000, |_, _| true)),
        ];
        for (name, mut cloud) in clouds {
            for divisions in [16u32, 64, 256, 1_024] {
                cloud.push([9_000, 9_000]);
                let (Some(mine), reference) = (
                    quantise(&cloud, divisions, 0, None).map(|(r, _)| r),
                    extreme_octagon(&cloud),
                ) else {
                    continue;
                };
                // The reduction's own output is the observable: every member the reference octagon
                // keeps is a candidate the shape may dig to, so two octagons that differ give two
                // different sets here even where they enclose nearly the same area.
                let kept: Vec<[u32; 2]> = cloud
                    .iter()
                    .copied()
                    .filter(|q| !reference.strictly_inside(*q))
                    .collect();
                for q in kept {
                    assert!(
                        mine.contains(&q),
                        "{name} at {divisions}: the band dropped a candidate the sweep keeps: {q:?}"
                    );
                }
            }
        }
    }

    /// **A cell folded out of one run is the cell folded out of many**, which is what lets the
    /// reduction skip the merge when the members arrive in row order and still answer the same way
    /// when they do not.
    ///
    /// The members here are ordered so that every cell is met twice, once early and once late — the
    /// shape a view's row order takes when its segments are walked one after another — and the
    /// answer has to be the answer over the same members in one pass.
    #[test]
    fn a_cell_met_twice_folds_to_one_representative() {
        let mut sorted = sample(20_000, 5_000, |x, y| x * x + y * y <= 5_000 * 5_000);
        sorted.sort_unstable();
        // Every other member, then the ones between them: both halves cover the same cells, so
        // concatenating them meets each cell twice and takes the merging path.
        let twice_over: Vec<[u32; 2]> = sorted
            .iter()
            .step_by(2)
            .chain(sorted.iter().skip(1).step_by(2))
            .copied()
            .collect();

        let mut once = quantise(&sorted, 64, 0, None).expect("reduces").0;
        let mut twice = quantise(&twice_over, 64, 0, None).expect("reduces").0;
        once.sort_unstable();
        once.dedup();
        twice.sort_unstable();
        twice.dedup();
        assert_eq!(once, twice);
    }
}
