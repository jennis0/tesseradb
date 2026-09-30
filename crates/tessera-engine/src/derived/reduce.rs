use super::geometry::convex_hull_of_sorted;

/// How many cells the quantising grid spans along the artifact's longer axis, before the shape is
/// computed. 1,024, chosen against what is drawn: members are binned to a square grid over their
/// own bounding box and the shape computed over one real member per occupied cell ([`quantise`]),
/// a quantisation and not a sample, so every vertex is still a visible member's own position.
///
/// The resolution is relative to the artifact and not the request's zoom, so a cell is at most a
/// viewport pixel or two of displacement and the identifier route, which carries no zoom, still
/// has an answer.
///
/// Measured over a 197-artifact layer at full membership: median boundary departure from the
/// unreduced shape 0.5% of the artifact's own extent, worst 4.4% (17% at 512 divisions). α is
/// exactly unchanged on every artifact ([`extreme_octagon`] makes that exact). Digging over the
/// layer goes 1,759 ms to 862 ms; a member may fall outside its own shape by at most one cell,
/// 1,965 of 12,808,679 measured.
pub(super) const QUANTISE_DIVISIONS: u32 = 1_024;

/// How many visible members an artifact needs before its shape is computed over representatives
/// rather than over every one of them.
///
/// 75,000, chosen against what is drawn rather than what the reduction costs. Below the floor the
/// exact shape is affordable, 0.5 ms under 10,000 members and 5 ms at 50,000, and small artifacts
/// are the ones a viewer zooms into. Without a floor the reduction reaches artifacts it has
/// nothing to offer: small layers lost up to 6.7% of an artifact's members outside its shape.
pub(super) const REDUCTION_FLOOR: usize = 75_000;

/// The representatives the served dig computes its shape over, or `None` where it digs over every
/// member, for `tessera-bench`'s `hull_cost` to hold the reduction to a second implementation
/// written from the definition.
#[doc(hidden)]
pub fn quantised(points: &[[u32; 2]]) -> Option<Vec<[u32; 2]>> {
    quantise(points, QUANTISE_DIVISIONS, REDUCTION_FLOOR, None)
}

/// One real member per occupied cell of a square grid over the members' own bounding box, or `None`
/// where the grid cannot reduce the input ([`QUANTISE_DIVISIONS`]).
///
/// The representative is the member nearest its cell's centre, ties broken on the position, so the
/// choice is a function of the member positions alone. `None` where the grid holds at least as many
/// cells as there are members; the cell side is the same on both axes, derived from the longer one.
/// `floor` is [`REDUCTION_FLOOR`] on every serving route, a parameter here because the reduction's
/// properties are tested at sizes small enough to check exhaustively. `bounds` is `points`'s own
/// bounding box where a caller has already traversed them, `None` otherwise; it changes no answer.
pub(super) fn quantise(
    points: &[[u32; 2]],
    divisions: u32,
    floor: usize,
    bounds: Option<[u32; 4]>,
) -> Option<Vec<[u32; 2]>> {
    let runs = Runs::fold(points, grid_shift(points, divisions, floor, bounds)?);

    // A run count is the occupied-cell count where the runs ascend, an upper bound otherwise.
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
    // The representatives are the bulk of what this returns, so candidates append to them.
    let mut out = merged.unwrap_or(reps);
    out.reserve(cell_count / 8);
    runs.push_hull_candidates(&octagon, &mut out);

    // Order and duplicates are left for the caller's sort and dedup, which every route already
    // pays: a member may be both its cell's representative and a hull candidate.
    Some(out)
}

/// The binning grid's cell side as a shift, or `None` where the reduction is refused before a cell
/// is ever computed.
fn grid_shift(
    points: &[[u32; 2]],
    divisions: u32,
    floor: usize,
    bounds: Option<[u32; 4]>,
) -> Option<u32> {
    // The squared distance below is `u64`; a cell side of at most `2^32 / divisions` rounded up to
    // a power of two keeps it from overflowing. `0` is the seam's "no quantisation at all".
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
    let [x0, y0, x1, y1] = bounds.unwrap_or_else(|| super::geometry::bounds(points));
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
    // A power of two, so a cell index is a shift rather than a division; rounding the side up
    // only ever makes the grid coarser than `divisions` asked, never finer.
    let shift = wx
        .max(wy)
        .div_ceil(divisions as u64)
        .next_power_of_two()
        .trailing_zeros();
    if 1u64 << shift <= 1 {
        // The grid is already the position lattice, so binning buys nothing.
        return None;
    }
    Some(shift)
}

/// Where the members are spread thinner than the grid, the occupied cells are nearly as numerous
/// as the members, and reducing returns nearly the input again. Three quarters is where the
/// reduction starts to return more than the sort it saves. Counting the occupied cells costs one
/// pass and answers the question exactly, rather than a fixed cells-per-member proxy.
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
    /// How many runs opened a cell whose key is below its predecessor's, deciding whether a cell
    /// can have been met twice.
    descents: usize,
}

impl<'a> Runs<'a> {
    /// A cell is a contiguous row range, so the occupied cells are found by folding runs rather
    /// than binning into a grid. The grid is anchored at the corpus's own origin, so each cell is
    /// a Morton block and, rows being Morton rank, a cell's members arrive consecutively: one
    /// comparison per member finds every boundary and nothing is allocated for an unoccupied cell.
    ///
    /// Folding runs needs no grid; a dense grid or a hash map per cell measured 215 to 289 ms over
    /// the same layer this replaces, against one comparison a member here. A jump to the next cell
    /// by bitmap arithmetic loses too: it costs on the order of ten probes against one comparison,
    /// and only wins where a cell holds tens of members, a corpus two orders of magnitude denser
    /// than this one.
    ///
    /// The fold reads a cell index and nothing else: the top bits of a member's position on both
    /// axes, so finding where one cell ends and the next begins is two shifts and a comparison.
    fn fold(points: &'a [[u32; 2]], shift: u32) -> Runs<'a> {
        let mut starts: Vec<u32> = Vec::with_capacity(points.len() / 4 + 1);
        // A run out of Morton order is a cell that may already have been seen: row order restarts
        // at each segment of the view. Counted rather than assumed; the cost is one sort below.
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

    /// One member per run, the one nearest its cell's centre, ties broken on the position, answered
    /// per run rather than per member: a run of one has no comparison to make at all. The distance
    /// arithmetic runs only for a cell holding more than one member, and not at all where the
    /// occupancy test declines the reduction first. The squared distances are returned beside the
    /// representatives only where a cell can be met twice, the one case that compares two runs'
    /// answers.
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

    /// One representative per occupied cell, the same one whatever order the members arrived in.
    /// Where the runs were already in Morton order each cell is one run and this is not called.
    /// Merging equal keys under the fold's rule is associative, so it gives what binning every
    /// member into a grid would. `None` where the merged cells are too nearly as numerous as the
    /// members to be worth returning ([`occupancy_loses`]).
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

    /// The hull candidates are found per cell, so no member is tested on its own. The filter is
    /// exact: `strictly_inside` is a conjunction of half-planes, so a cell whose four corners are
    /// all strictly inside holds nothing that is not, and is skipped whole.
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

/// The two cell coordinates interleaved, the key [`quantise`] folds runs on: standard
/// bit-spreading, five shift-or-and steps an axis. Ordering by this key is Morton order over the
/// binning grid, so "this run is out of order" is a single comparison. A `u64` and not
/// [`tessera_spatial::morton::interleave`], because the binning grid's coordinates run to 32 bits
/// where the cell side is small, against the corpus grid's 16.
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

/// [`extreme_octagon`] over the folded cells, without a pass over the members: the cells bound
/// the members, so the search is a band, a cell whose furthest corner scores below the best
/// representative's bound holding nothing that beats it. Ties break as [`extreme_octagon`] breaks
/// them, so the two return the same polygon for the same members.
impl Runs<'_> {
    fn extreme_octagon_over(&self, reps: &[[u32; 2]]) -> Octagon {
        // Every representative is a member, so the best of them is a lower bound on each extreme,
        // established before any cell is opened.
        let mut best: [Option<(i64, [u32; 2])>; 8] = [None; 8];
        for rep in reps {
            offer(&mut best, *rep);
        }
        let s1 = self.side as i64 - 1;
        for i in 0..self.len() {
            // A cell's furthest corner is its low corner plus `side - 1` on positive axes.
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
                offer(&mut best, *q);
            }
        }
        octagon_of(best)
    }
}

/// `q` against each of the eight running extremes, kept where it scores higher. Ties break on the
/// position, so the octagon is a function of the member positions alone.
fn offer(best: &mut [Option<(i64, [u32; 2])>; 8], q: [u32; 2]) {
    let (x, y) = (q[0] as i64, q[1] as i64);
    for (slot, (wx, wy)) in best.iter_mut().zip(OCTAGON_DIRECTIONS) {
        let score = wx * x + wy * y;
        if slot.is_none_or(|(s, b)| score > s || (score == s && q < b)) {
            *slot = Some((score, q));
        }
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

/// The polygon eight extremes span, one linear form per edge, shared so [`extreme_octagon`] and
/// [`extreme_octagon_over`] cannot drift apart.
fn octagon_of(best: [Option<(i64, [u32; 2])>; 8]) -> Octagon {
    let mut extremes: Vec<[u32; 2]> = best.into_iter().flatten().map(|(_, q)| q).collect();
    extremes.sort_unstable();
    extremes.dedup();
    // `A·x + B·y + C` per edge: the cross product with the vertex subtracted out once.
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

/// α must not move when the input is reduced, so every member that could be a convex-hull vertex
/// survives [`quantise`]. α follows the sampling density: the wrap over the representatives alone
/// measured moving it by up to 3.6x on one artifact. The filter is Akl-Toussaint's, discarding a
/// member strictly inside the polygon spanned by the extremes of `x`, `y`, `x + y` and `x - y`,
/// 94% to 99.9% of a large artifact's members in one pass. `f64` appears here, made exact by
/// [`OCTAGON_MARGIN`], so the set kept is identical on every platform.
#[cfg(test)]
fn extreme_octagon(points: &[[u32; 2]]) -> Octagon {
    let mut best: [Option<(i64, [u32; 2])>; 8] = [None; 8];
    for q in points {
        offer(&mut best, *q);
    }
    octagon_of(best)
}

/// The polygon spanned by the eight extremes, one linear form per edge ([`extreme_octagon`]).
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
    /// [`strictly_inside`](Self::strictly_inside), at one corner's cost: an edge's minimum over a
    /// box is at the corner the signs of `a` and `b` pick out.
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
/// cross-product units. 2^16, against a worst-case `f64` error under 2^13: a member near an edge
/// is kept, so no hull vertex is ever discarded and α is exactly what the full membership gives.
const OCTAGON_MARGIN: f64 = 65_536.0;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::derived::dig::{bridge_threshold, concave_rings, dig_rings_within, DIG_BUDGET};
    use crate::derived::geometry::convex_hull;
    use crate::derived::test_support::*;

    /// The set the fold returns is the set a dense binning would have returned, cell for cell and
    /// candidate for candidate, so the route to the occupied cells is a speed change and not a
    /// shape change. Would catch a divergence between the fold and a from-the-definition oracle
    /// that shares nothing with [`quantise`] but the cell arithmetic itself.
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
                normalise(quantise(cloud, divisions, 0, None)),
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

    /// The reduction is a quantisation and not a sample: every representative is a member's own
    /// position, and every member has a representative within a cell of it. Run at a coarse
    /// resolution, because the properties do not depend on it and a cloud dense enough to reduce
    /// at the served resolution is millions of members.
    #[test]
    fn every_representative_is_a_member_and_every_member_has_one() {
        let members = sample(5_000, 5_000, |x, y| x * x + y * y <= 5_000 * 5_000);
        let reduced = quantise(&members, 32, 0, None).expect("a cloud this dense reduces");
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

    /// α does not move when the input is reduced, because every member that could be a convex hull
    /// vertex survives the reduction whatever cell it falls in ([`extreme_octagon`]).
    #[test]
    fn the_reduction_leaves_the_convex_wrap_and_so_alpha_exact() {
        for cloud in [
            sample(20_000, 5_000, |x, y| x * x + y * y <= 5_000 * 5_000),
            sample(20_000, 5_000, |x, y| x.abs() + y.abs() <= 5_000),
            sample(20_000, 5_000, |x, y| {
                x * x + y * y <= 5_000 * 5_000 && (x < 0 || y.abs() > 2_000)
            }),
        ] {
            let reduced = quantise(&cloud, 32, 0, None).expect("a cloud this dense reduces");
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

    /// A membership too small for the grid to reduce is dug over every member, so a viewer's
    /// zoomed-in artifact keeps full fidelity.
    #[test]
    fn a_small_membership_is_not_reduced_at_all() {
        let members = moon();
        assert!(quantise(&members, QUANTISE_DIVISIONS, REDUCTION_FLOOR, None).is_none());
        assert_eq!(
            concave_rings(&members, None),
            dig_rings_within(&members, DIG_BUDGET, 0, None).0
        );
    }

    /// The reduction is a function of the member positions, not of arrival order: the cell's
    /// representative is the member nearest its centre, ties broken on the position itself.
    #[test]
    fn the_reduction_does_not_depend_on_the_order_the_members_arrive_in() {
        let members = sample(20_000, 5_000, |x, y| x * x + y * y <= 5_000 * 5_000);
        let mut shuffled = members.clone();
        shuffled.reverse();
        let mut a = quantise(&members, 32, 0, None).expect("reduces");
        let mut b = quantise(&shuffled, 32, 0, None).expect("reduces");
        a.sort_unstable();
        b.sort_unstable();
        assert_eq!(a, b);
    }

    /// The band [`extreme_octagon_over`] searches is the whole membership's octagon, not an
    /// approximation of it, on clouds chosen where a wrong bound would show: a ring, a diagonal
    /// band, an outlier, and a cloud in convex position.
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
                    quantise(&cloud, divisions, 0, None),
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

    /// A cell folded out of one run is the cell folded out of many: the merge, taken only when the
    /// members do not arrive in row order, must answer as one pass over an ordered input would.
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

        let mut once = quantise(&sorted, 64, 0, None).expect("reduces");
        let mut twice = quantise(&twice_over, 64, 0, None).expect("reduces");
        once.sort_unstable();
        once.dedup();
        twice.sort_unstable();
        twice.dedup();
        assert_eq!(once, twice);
    }
}
