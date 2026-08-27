//! Derived content: what an artifact looks like to *this* viewer.
//!
//! **One closure rule governs the whole vocabulary** (`annotations.md` §4.2):
//!
//! > A derived property is a function of `membership ∩ M_auth` and of nothing else.
//!
//! That is **I2** restated at the artifact, and it is what lets the vocabulary grow without a new
//! leak-register row each time: *median publication year of visible members* satisfies it and needs
//! no review; *total membership* does not, and is a disclosure. Every function in this module takes
//! the visible rows and nothing else, and the only way to obtain those is
//! [`MaskedSet::visible_rows`](crate::compose::MaskedSet::visible_rows) — so a property computed
//! over full membership is not merely forbidden, it has no input to be computed from.
//!
//! ## Why this is the fail-open to watch, and not the count
//!
//! An artifact whose *own terms* authorise it is authorised to **exist**, not to describe its
//! members (`annotations.md` §4). A build-time hull served beside a masked count discloses exactly
//! the members the gate did not cover — and it does so in a shape that looks like the geometry the
//! engine would have derived anyway, which is why §4.1 puts caller-supplied geometry in a different
//! row of the table from this one. Anything computed here is safe by construction; anything
//! *supplied* carries a generating set and is gated by containment instead.
//!
//! ## Grid units, not data coordinates
//!
//! Positions travel in the 32-bit fixed-point grid the Morton code is built from — the same units
//! the point path puts on the wire, where a client needs no quantisation extent to interpret what
//! it draws (`clients/ts/core/src/decode.ts`). A centroid is a mean and so is fractional; a box and
//! a hull are lattice positions of real members and stay integral.
//!
//! ## The cost, and why the vocabulary is declared
//!
//! A count is one bitmap operation, O(containers touched). Everything here is O(visible members)
//! per artifact per request, because it reads a position for each. A client drawing only centroids
//! should not pay hull cost for every artifact on screen, which is what the layer's declaration is
//! for — a **cost** control, not a security one, since every value it can take is safe.
//!
//! The hull is the expensive one and got dearer: it is a **concave** shape over the visible members
//! rather than their convex wrap ([`concave_hull`]), which costs a sort, a bucketing pass and a
//! bounded number of digs — 1.5× the convex path over a whole 197-artifact layer, measured in
//! `docs/evidence/memos/2026-08-26-concave-hulls.md`. It discloses nothing the wrap did not, and
//! the argument is in [`concave_hull`]'s own documentation rather than restated here.

use croaring::Bitmap;
use tessera_spatial::morton::unsplit32;
use tessera_store::read::SegmentData;
pub use tessera_types::layer::ComputedProperty;
use tessera_types::MortonCode;

/// What a viewer is told about an artifact's shape, beside its masked count.
///
/// Each field is present exactly when its layer declared it and the artifact has at least one
/// visible member. A served artifact always has one — an artifact with none fails candidacy in the
/// viewport, and on the identifier route a zero count with a declared geometry gives an **empty**
/// geometry rather than a fabricated one.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DerivedContent {
    /// Mean position, grid units. Fractional, so `f64`.
    pub centroid: Option<[f64; 2]>,
    /// `[qx_min, qy_min, qx_max, qy_max]`, grid units.
    pub bbox: Option<[u32; 4]>,
    /// The hull's vertices, counter-clockwise, grid units — a **concave (alpha) shape** over the
    /// visible members, not their convex wrap. A membership of one visible member gives one vertex,
    /// of two gives two: the hull of a point set is that point set when it is degenerate, and
    /// rounding it up to a triangle would draw an area no member occupies.
    pub hull: Option<Vec<[u32; 2]>>,
}

impl DerivedContent {
    pub fn is_empty(&self) -> bool {
        self.centroid.is_none() && self.bbox.is_none() && self.hull.is_none()
    }
}

/// The segments of one view, ascending in `row_base`, resolving a view row to the segment holding
/// it.
///
/// The same reverse-scan shape [`crate::select::SelectionParts::resolve_indexed`] uses, and for the
/// same reason: the live segment count is a handful, so a scan beats a binary search and is
/// obviously correct. It is a separate type only because artifact geometry is gathered outside a
/// tile selection — the rows come from a membership, not from a viewport's parts.
pub struct RowLocator<'a> {
    segments: Vec<(&'a SegmentData, u32)>,
}

impl<'a> RowLocator<'a> {
    /// `segments` must be ascending in `row_base`, which
    /// [`crate::viewport::segments_with_row_bases`] guarantees by sorting.
    pub fn new(segments: Vec<(&'a SegmentData, u32)>) -> Self {
        RowLocator { segments }
    }

    /// The position of one view row, in grid units, or `None` for a row past every segment's
    /// extent.
    ///
    /// **`None` is dropped by the caller rather than defaulted**, because a position of `(0, 0)` is
    /// a real position on the map: a member the row space cannot place would otherwise pull every
    /// centroid towards the origin, which is a wrong answer that renders.
    pub fn position(&self, row: u32) -> Option<(u32, u32)> {
        let (segment, local) = self.resolve(row)?;
        let idx = local as usize;
        let cell = *segment.morton.u32().get(idx)?;
        let residual = *segment.columns.residual().get(idx)?;
        Some(unsplit32(MortonCode::new(cell), residual))
    }

    fn resolve(&self, row: u32) -> Option<(&'a SegmentData, u32)> {
        for &(segment, row_base) in self.segments.iter().rev() {
            if row >= row_base {
                return Some((segment, row - row_base));
            }
        }
        None
    }
}

/// Compute the declared properties over the rows a viewer may see.
///
/// `visible` comes from the composed mask and nothing else — see this module's doc. `declared` is
/// the layer's parsed vocabulary; an empty one costs one branch and no position read, which is what
/// keeps a count-only layer at count-only cost.
pub fn compute(
    declared: &[ComputedProperty],
    visible: &Bitmap,
    locator: &RowLocator<'_>,
) -> DerivedContent {
    let mut out = DerivedContent::default();
    if declared.is_empty() {
        return out;
    }

    // One pass over the visible rows, whatever is declared: the read is the cost, and reading a
    // position twice to compute a centroid and a box separately would double it.
    let mut positions: Vec<[u32; 2]> = Vec::with_capacity(visible.cardinality() as usize);
    for row in visible.iter() {
        if let Some((qx, qy)) = locator.position(row) {
            positions.push([qx, qy]);
        }
    }
    if positions.is_empty() {
        return out;
    }

    for property in declared {
        match property {
            ComputedProperty::Centroid => {
                // Summed as `f64` rather than `u64`: the grid is 2^32 wide, so a membership past
                // ~2^32 members would overflow a `u64` sum, and the mean is fractional in any case.
                let (mut sx, mut sy) = (0.0f64, 0.0f64);
                for p in &positions {
                    sx += p[0] as f64;
                    sy += p[1] as f64;
                }
                let n = positions.len() as f64;
                out.centroid = Some([sx / n, sy / n]);
            }
            ComputedProperty::Box => {
                let mut b = [u32::MAX, u32::MAX, 0u32, 0u32];
                for p in &positions {
                    b[0] = b[0].min(p[0]);
                    b[1] = b[1].min(p[1]);
                    b[2] = b[2].max(p[0]);
                    b[3] = b[3].max(p[1]);
                }
                out.bbox = Some(b);
            }
            ComputedProperty::Hull => out.hull = Some(concave_hull(&positions)),
        }
    }
    out
}

/// The vertices digging may add on top of the convex hull's own.
///
/// **A budget for the digging, not an absolute cap, and the difference is forced.** Every vertex is
/// a visible member's position and the shape contains every visible member, so the convex hull's
/// own vertex count is a floor: reducing it means either dropping a member outside the shape or
/// inventing a vertex no member occupies, and both are worse than a wide polygon. What digging adds
/// is what a cap can bound, and this bounds it at 64 vertices — 512 bytes of `hull_x`/`hull_y` per
/// artifact at the worst case, against the convex hull's own count, which is what already rode on
/// every response. **64 is where the knee was measured**
/// (`docs/evidence/memos/2026-08-26-concave-hulls.md`): over one 197-artifact layer it gives shapes
/// 14% tighter in area for four times the hull bytes, and doubling it again buys 5 more points of
/// area for another 43 KB. The fidelity cost of running out is that a shape stops refining its
/// *shortest* remaining bridges, because digging spends the budget longest edge first — a coarser
/// shape, never a wrong one, since it still holds every member and is still inside the wrap.
const DIG_BUDGET: usize = 64;

/// How many times the median edge an edge must exceed before it is treated as bridging a void.
///
/// **α is derived from the shape's own edges and never supplied by a caller**, so two principals'
/// shapes differ only because their memberships do and a request cannot dial one. The statistic is
/// the median squared length of the *convex* hull's edges, which is scale-free — it is a length
/// measured in the same cloud's own units — and robust, since a single long chord across a
/// concavity is exactly the outlier a median ignores and a mean would chase. An edge three times
/// longer than the typical edge of the same wrap is bridging empty space rather than following the
/// members; one that is not is left alone, which is why a densely sampled convex cloud keeps its
/// convex hull unchanged.
const BRIDGE_FACTOR: i128 = 3;

/// A concave (alpha) shape over the visible members, counter-clockwise, on the integer grid.
///
/// **Why not the convex wrap.** An HDBSCAN cluster is an irregular density region — crescent,
/// branching, often both — and its convex hull swallows the empty space between the arms, overlaps
/// every sibling and draws single straight edges across the whole viewport. The vertices honestly
/// describe a shape the cluster does not have, which is why no client-side smoothing can repair it.
///
/// **It discloses nothing the convex wrap did not.** The inputs are the same (`membership ∩
/// M_auth`, gathered by [`compute`] and nothing else), the derivation is the same per-request one,
/// every vertex is a visible member's position either way, and the result is a *subset* of the
/// convex hull — so it says less about where the members this viewer cannot see are sitting, not
/// more. No leak-register row: nothing here lets a viewer end up knowing something about data they
/// were not served (`architecture.md` Appendix C's inclusion test).
///
/// **The construction: dig inward from the convex hull.** Start at the convex hull, which contains
/// every member. Repeatedly take the longest edge `(a, b)` that exceeds α, find the member `c`
/// closest to the line through `a` and `b` among those on the interior side of `a → b` that project
/// inside the segment ([`Buckets::nearest_inside`]), and replace the edge with `(a, c)` and
/// `(c, b)` — carving the triangle `a c b` out of the shape.
///
/// Two properties fall out of `c` being the *closest*:
///
/// - **Containment is preserved.** The triangle `a c b` lies inside the strip between the
///   perpendiculars at `a` and at `b`, because `c` does and projection is affine — so a member
///   strictly inside it is itself a candidate, and being strictly closer to the line than `c`
///   contradicts `c`'s minimality. The triangle is empty, so removing it removes no member, and the
///   shape contains every member at every step by induction from the convex hull.
/// - **No arithmetic epsilon.** Minimising the perpendicular distance to the line through `a` and
///   `b` is minimising the cross product `(b − a) × (p − a)`, since the divisor `|ab|` is fixed per
///   edge. That is exact in `i128` (see [`convex_hull_of_sorted`] on why not `i64`), so the shape is a
///   function of the member positions and of nothing else — no float, no platform drift, no
///   tie-break that depends on iteration order.
///
/// A dig is refused, and its edge retired, when there is no such member or when the shape would stop
/// being simple — including when `c` already sits on the boundary, which would make the ring touch
/// itself. **A point set in convex position is therefore returned unchanged**: every member is a
/// convex hull vertex or lies along one of its edges, so every candidate is on the boundary already
/// and no dig is admissible, whatever α is.
///
/// **A concavity whose flanks are flush with the edge bridging it cannot be dug**, because the
/// members bounding the gap project onto that edge's endpoints and so are not candidates. Digging
/// reaches gaps whose interior is visible from the edge that spans them, which every concavity in
/// the measured corpus is; the failing shape is a comb of teeth flush with its own wrap, and there
/// the answer is the wrap rather than a wrong shape.
///
/// Cost is `O(n)` to bucket the members plus, per dig, one pruned pass over the buckets and one
/// pass over the boundary, against the convex hull's `O(n log n)` sort, which still dominates.
/// Measured over 197 artifacts of 6,146 … 2,422,486 members in
/// `docs/evidence/memos/2026-08-26-concave-hulls.md`: 1.5× the convex path over a whole layer, and
/// 158 ms against 129 ms on its largest artifact.
fn concave_hull(points: &[[u32; 2]]) -> Vec<[u32; 2]> {
    let mut p: Vec<[u32; 2]> = points.to_vec();
    p.sort_unstable();
    p.dedup();
    let convex = convex_hull_of_sorted(&p);
    // One member is that point and two are that segment, exactly as before: an area no member
    // occupies asserts more than the data does, and there is nothing to dig into.
    if convex.len() < 3 {
        return convex;
    }

    let alpha_sq = bridge_threshold(&convex);
    let grid = Buckets::build(&p);
    let budget = convex.len() + DIG_BUDGET;
    let mut poly: Vec<Vertex> = convex
        .into_iter()
        .map(|pos| Vertex { pos, retired: false })
        .collect();

    while poly.len() < budget {
        let Some(i) = longest_bridge(&poly, alpha_sq) else {
            break;
        };
        let n = poly.len();
        let (a, b) = (poly[i].pos, poly[(i + 1) % n].pos);
        match grid.nearest_inside(a, b) {
            Some(c) if dig_is_admissible(&poly, i, c) => {
                // The replaced edge's flag goes with it; `poly[i]` now carries `(a, c)` and the
                // inserted vertex carries `(c, b)`, both fresh.
                poly.insert(
                    i + 1,
                    Vertex {
                        pos: c,
                        retired: false,
                    },
                );
            }
            _ => poly[i].retired = true,
        }
    }

    poly.into_iter().map(|v| v.pos).collect()
}

/// One boundary vertex, and whether the edge leaving it has been retired — an edge whose dig was
/// refused is never retried, which is what bounds the loop's refusals as the budget bounds its
/// insertions.
struct Vertex {
    pos: [u32; 2],
    retired: bool,
}

/// α², as the squared length an edge must exceed to be dug. Squared throughout so no root is taken:
/// the median commutes with squaring, so this is `(BRIDGE_FACTOR × median edge)²`.
fn bridge_threshold(convex: &[[u32; 2]]) -> i128 {
    let mut lengths: Vec<i128> = (0..convex.len())
        .map(|i| sq_len(convex[i], convex[(i + 1) % convex.len()]))
        .collect();
    lengths.sort_unstable();
    BRIDGE_FACTOR * BRIDGE_FACTOR * lengths[lengths.len() / 2]
}

/// The index of the longest live edge above α, or `None` when the shape has stopped bridging.
fn longest_bridge(poly: &[Vertex], alpha_sq: i128) -> Option<usize> {
    let n = poly.len();
    let mut best: Option<(i128, usize)> = None;
    for i in 0..n {
        if poly[i].retired {
            continue;
        }
        let length = sq_len(poly[i].pos, poly[(i + 1) % n].pos);
        if length <= alpha_sq {
            continue;
        }
        if best.is_none_or(|(b, _)| length > b) {
            best = Some((length, i));
        }
    }
    best.map(|(_, i)| i)
}

/// Whether replacing edge `i` with `(a, c)` and `(c, b)` leaves a simple polygon.
///
/// **Checked rather than argued.** The dig triangle holds no *member*, so it holds no vertex — but
/// an edge from elsewhere on the boundary can still cross it with both its endpoints outside, which
/// is exactly what two arms of a crescent digging towards each other would do. The check is
/// `O(V)` against a boundary the budget bounds, so it costs nothing worth trading the property for.
fn dig_is_admissible(poly: &[Vertex], i: usize, c: [u32; 2]) -> bool {
    let n = poly.len();
    let (a, b) = (poly[i].pos, poly[(i + 1) % n].pos);

    let (prev, next) = ((i + n - 1) % n, (i + 1) % n);
    for j in 0..n {
        if j == i {
            continue;
        }
        let (f0, f1) = (poly[j].pos, poly[(j + 1) % n].pos);
        // `c` already on the boundary would make the new edges touch it rather than cross the
        // interior — and it is what leaves a point set in convex position untouched.
        if on_segment(f0, f1, c) {
            return false;
        }
        // The edge arriving at `a` legitimately meets `(a, c)` at `a`, and nowhere else.
        if j == prev {
            if on_segment(a, c, f0) {
                return false;
            }
        } else if segments_meet(a, c, f0, f1) {
            return false;
        }
        if j == next {
            if on_segment(b, c, f1) {
                return false;
            }
        } else if segments_meet(c, b, f0, f1) {
            return false;
        }
    }
    true
}

/// The members bucketed into a square grid, each bucket carrying the bounding box of what it holds.
///
/// **A bounding box per bucket, not the cell's own geometry**, because the box is tighter and needs
/// no cell-boundary arithmetic to be exact: the minimum of a cross product over a box is attained at
/// a corner, so one corner per bucket bounds every member in it and the whole bucket is skipped when
/// that bound cannot beat the best candidate found so far. The bound is exact in `i128`, so pruning
/// never discards the answer.
struct Buckets {
    /// Every member, reordered so each bucket's members are contiguous.
    points: Vec<[u32; 2]>,
    cells: Vec<Cell>,
}

struct Cell {
    min: [u32; 2],
    max: [u32; 2],
    start: usize,
    len: usize,
}

impl Buckets {
    fn build(p: &[[u32; 2]]) -> Buckets {
        let axis = bucket_axis(p.len()) as u64;
        let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0u32, 0u32);
        for q in p {
            x0 = x0.min(q[0]);
            y0 = y0.min(q[1]);
            x1 = x1.max(q[0]);
            y1 = y1.max(q[1]);
        }
        // Widths as `u64` and inclusive, so the extreme member lands in the last bucket rather than
        // one past it, and so a cloud spanning the whole 2^32 grid does not wrap.
        let (wx, wy) = ((x1 - x0) as u64 + 1, (y1 - y0) as u64 + 1);
        let key = |q: &[u32; 2]| -> usize {
            let cx = ((q[0] - x0) as u64 * axis / wx).min(axis - 1);
            let cy = ((q[1] - y0) as u64 * axis / wy).min(axis - 1);
            (cy * axis + cx) as usize
        };

        let total = (axis * axis) as usize;
        let mut offsets = vec![0usize; total + 1];
        for q in p {
            offsets[key(q) + 1] += 1;
        }
        for i in 0..total {
            offsets[i + 1] += offsets[i];
        }
        let mut cursor = offsets.clone();
        let mut points = vec![[0u32; 2]; p.len()];
        for q in p {
            let k = key(q);
            points[cursor[k]] = *q;
            cursor[k] += 1;
        }

        let mut cells = Vec::new();
        for k in 0..total {
            let (start, end) = (offsets[k], offsets[k + 1]);
            if start == end {
                continue;
            }
            let (mut min, mut max) = (points[start], points[start]);
            for q in &points[start..end] {
                min = [min[0].min(q[0]), min[1].min(q[1])];
                max = [max[0].max(q[0]), max[1].max(q[1])];
            }
            cells.push(Cell {
                min,
                max,
                start,
                len: end - start,
            });
        }
        Buckets { points, cells }
    }

    /// The member closest to the line through `a` and `b`, among those on the interior side of
    /// `a → b` — the line itself included — **and** projecting strictly inside the segment. `None`
    /// where there is no such member.
    ///
    /// **A member lying on the segment itself is the nearest there is, and is returned.** Digging to
    /// it carves a triangle of zero area: the shape does not change, the edge simply gains that
    /// member as a vertex, and the two sub-edges can then be dug on their own. Excluding it instead
    /// is what a first draft did, and it cut collinear members off the shape — they sat on the
    /// boundary that the dig moved inwards, so containment broke on lattice-aligned data. It costs a
    /// vertex from the budget, which is the right price on real positions (a third member exactly on
    /// the line through two others is a fluke of quantisation) and a poor one on a synthetic
    /// axis-aligned cloud, where the shape degrades towards its convex wrap rather than breaking.
    ///
    /// **Both restrictions are part of the emptiness argument, not a tidying-up.** The dig triangle
    /// `a c b` lies inside the strip between the perpendiculars at `a` and at `b` exactly because
    /// `c` does, projection being affine — so a member strictly inside that triangle is itself a
    /// candidate, and being strictly closer to the line than `c` contradicts `c`'s minimality. Drop
    /// the projection restriction and the argument goes with it: the nearest member to the *line* is
    /// routinely one just past an endpoint, lying along the arm the edge springs from, and digging to
    /// it carves a sliver along the edge instead of into the void the edge was bridging.
    ///
    /// Exhaustive over the members that qualify — the bucket bound only skips buckets that provably
    /// cannot hold a better candidate — because it is that minimality, and nothing else, that makes
    /// the dig triangle empty and so keeps every member inside the shape.
    fn nearest_inside(&self, a: [u32; 2], b: [u32; 2]) -> Option<[u32; 2]> {
        let (dx, dy) = (b[0] as i128 - a[0] as i128, b[1] as i128 - a[1] as i128);
        let cross =
            |q: [u32; 2]| dx * (q[1] as i128 - a[1] as i128) - dy * (q[0] as i128 - a[0] as i128);

        // Buckets in increasing bound, so the first one visited holds a near-answer and every later
        // bound is compared against a `best` that is already small. Walked in bucket order instead,
        // the pruning test only starts biting after a bucket that happens to be near the edge turns
        // up, and on a large membership that is most of the grid scanned before it does.
        let mut order: Vec<(i128, usize)> = self
            .cells
            .iter()
            .enumerate()
            .map(|(i, cell)| {
                // The cross product is affine in the position, so its minimum over the bucket's box
                // sits at whichever corner the two coefficients — `dx` on y, `−dy` on x — select.
                let corner = [
                    if dy <= 0 { cell.min[0] } else { cell.max[0] },
                    if dx >= 0 { cell.min[1] } else { cell.max[1] },
                ];
                (cross(corner), i)
            })
            .collect();
        order.sort_unstable();

        let mut best: Option<(i128, [u32; 2])> = None;
        for (bound, index) in order {
            if let Some((found, _)) = best {
                if bound > found {
                    break;
                }
            }
            let cell = &self.cells[index];
            for &q in &self.points[cell.start..cell.start + cell.len] {
                let d = cross(q);
                if d < 0 || dot(a, b, q) <= 0 || dot(b, a, q) <= 0 {
                    continue;
                }
                let better = match best {
                    None => true,
                    // Ties are broken on the position itself, so the answer does not depend on the
                    // order the buckets happen to be walked in.
                    Some((bd, bq)) => d < bd || (d == bd && q < bq),
                };
                if better {
                    best = Some((d, q));
                }
            }
        }
        best.map(|(_, q)| q)
    }
}

/// Buckets per axis: about 64 members to a bucket, and never more than 64 axis divisions, so a small
/// membership degrades to a single bucket and a plain scan rather than to a sparse grid whose
/// per-bucket overhead exceeds the scan it replaces.
fn bucket_axis(n: usize) -> u32 {
    let mut axis = 1u32;
    while axis < 64 {
        let next = axis as usize + 1;
        if next * next * 64 > n {
            break;
        }
        axis += 1;
    }
    axis
}

fn sq_len(a: [u32; 2], b: [u32; 2]) -> i128 {
    let (dx, dy) = (b[0] as i128 - a[0] as i128, b[1] as i128 - a[1] as i128);
    dx * dx + dy * dy
}

/// `(b − a) · (c − a)`: positive exactly when `c` projects onto the ray from `a` through `b`.
fn dot(a: [u32; 2], b: [u32; 2], c: [u32; 2]) -> i128 {
    (b[0] as i128 - a[0] as i128) * (c[0] as i128 - a[0] as i128)
        + (b[1] as i128 - a[1] as i128) * (c[1] as i128 - a[1] as i128)
}

fn orient(o: [u32; 2], a: [u32; 2], b: [u32; 2]) -> i128 {
    let (ox, oy) = (o[0] as i128, o[1] as i128);
    (a[0] as i128 - ox) * (b[1] as i128 - oy) - (a[1] as i128 - oy) * (b[0] as i128 - ox)
}

/// `r` lies on the closed segment `p q`.
fn on_segment(p: [u32; 2], q: [u32; 2], r: [u32; 2]) -> bool {
    orient(p, q, r) == 0
        && r[0] >= p[0].min(q[0])
        && r[0] <= p[0].max(q[0])
        && r[1] >= p[1].min(q[1])
        && r[1] <= p[1].max(q[1])
}

/// Whether two closed segments share any point at all — touching counts, because a boundary that
/// touches itself is not a shape a client can fill.
fn segments_meet(p1: [u32; 2], p2: [u32; 2], p3: [u32; 2], p4: [u32; 2]) -> bool {
    let (d1, d2) = (orient(p3, p4, p1), orient(p3, p4, p2));
    let (d3, d4) = (orient(p1, p2, p3), orient(p1, p2, p4));
    if ((d1 > 0) != (d2 > 0)) && (d1 != 0 && d2 != 0) && ((d3 > 0) != (d4 > 0)) && (d3 != 0 && d4 != 0)
    {
        return true;
    }
    (d1 == 0 && on_segment(p3, p4, p1))
        || (d2 == 0 && on_segment(p3, p4, p2))
        || (d3 == 0 && on_segment(p1, p2, p3))
        || (d4 == 0 && on_segment(p1, p2, p4))
}

/// Andrew's monotone chain, counter-clockwise, on the integer grid, over members already sorted and
/// deduplicated — [`concave_hull`] does that once and then buckets the same vector, rather than
/// sorting it twice. It is the shape digging starts from, and the shape a point set in convex
/// position keeps.
///
/// **Integer arithmetic throughout, in `i128`.** Each component of a grid vector is bounded by
/// 2^32, so their product needs 64 bits and their *difference* needs 65: an `i64` cross product
/// overflows on a hull spanning most of the map, which is the ordinary case for a broad principal's
/// cluster rather than an edge one. In `i128` the orientation test is exact and there is no epsilon
/// to choose. A hull computed in floats would be non-deterministic across platforms for collinear
/// members, and the wire carries the vertex list itself.
///
/// Collinear points are dropped (`<= 0` rather than `< 0`), so a hull carries vertices and not the
/// members lying along its edges.
fn convex_hull_of_sorted(p: &[[u32; 2]]) -> Vec<[u32; 2]> {
    if p.len() <= 2 {
        return p.to_vec();
    }

    let mut hull: Vec<[u32; 2]> = Vec::with_capacity(p.len() + 1);
    for &point in p {
        while hull.len() >= 2 && orient(hull[hull.len() - 2], hull[hull.len() - 1], point) <= 0 {
            hull.pop();
        }
        hull.push(point);
    }
    let lower = hull.len() + 1;
    for &point in p.iter().rev().skip(1) {
        while hull.len() >= lower && orient(hull[hull.len() - 2], hull[hull.len() - 1], point) <= 0 {
            hull.pop();
        }
        hull.push(point);
    }
    hull.pop();
    hull
}

/// The convex hull of an arbitrary member list — the reference shape the tests below compare the
/// concave one against. The serving path reaches [`convex_hull_of_sorted`] through
/// [`concave_hull`], which has already sorted.
#[cfg(test)]
fn convex_hull(points: &[[u32; 2]]) -> Vec<[u32; 2]> {
    let mut p: Vec<[u32; 2]> = points.to_vec();
    p.sort_unstable();
    p.dedup();
    convex_hull_of_sorted(&p)
}

#[cfg(test)]
mod tests {
    use super::*;


    /// A deterministic sample of `count` positions from `region`, drawn over the box
    /// `[-span, span]²` and offset into the unsigned grid.
    ///
    /// **Not a lattice.** Real positions are a quantisation of a continuous embedding, so a third
    /// member exactly on the line through two others is a fluke; a lattice makes it the common case
    /// and turns every test into a test of the collinear path. That path has its own test below.
    fn sample(count: usize, span: i64, region: impl Fn(i64, i64) -> bool) -> Vec<[u32; 2]> {
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as i64
        };
        let mut points = Vec::with_capacity(count);
        let width = 2 * span + 1;
        while points.len() < count {
            let (x, y) = (next() % width - span, next() % width - span);
            if region(x, y) {
                points.push([(x + 2_000_000) as u32, (y + 2_000_000) as u32]);
            }
        }
        points.sort_unstable();
        points.dedup();
        points
    }

    /// A moon: the disk of radius 1000 about the origin with the disk of radius 900 about
    /// `(1200, 0)` bitten out of it. The bite is the concavity a convex wrap swallows, and
    /// `(600, 0)` sits in the middle of it.
    fn moon() -> Vec<[u32; 2]> {
        sample(4000, 1000, |x, y| {
            x * x + y * y <= 1000 * 1000 && (x - 1200) * (x - 1200) + y * y > 900 * 900
        })
    }

    /// The middle of the moon's bite, in grid coordinates.
    const IN_THE_BITE: [u32; 2] = [2_000_600, 2_000_000];

    /// Seven lobes on a common centre, so the wrap bridges seven separate voids and the budget has
    /// somewhere to be spent. Sampled finely enough that no single dig finishes a valley.
    fn flower() -> Vec<[u32; 2]> {
        sample(20_000, 1000, |x, y| {
            let (fx, fy) = (x as f64, y as f64);
            let r = (fx * fx + fy * fy).sqrt();
            r <= 400.0 + 550.0 * (7.0 * fy.atan2(fx)).cos()
        })
    }

    /// Twice the signed area of a ring — positive for counter-clockwise. Exact in `i128`.
    fn double_area(poly: &[[u32; 2]]) -> i128 {
        let n = poly.len();
        (0..n)
            .map(|i| {
                let (a, b) = (poly[i], poly[(i + 1) % n]);
                a[0] as i128 * b[1] as i128 - b[0] as i128 * a[1] as i128
            })
            .sum()
    }

    /// `p` is inside the ring or on its boundary. Crossing number, exact in `i128`, with the
    /// boundary tested first so a member sitting on an edge counts as contained.
    fn contains(poly: &[[u32; 2]], p: [u32; 2]) -> bool {
        let n = poly.len();
        if n == 1 {
            return poly[0] == p;
        }
        for i in 0..n {
            if on_segment(poly[i], poly[(i + 1) % n], p) {
                return true;
            }
        }
        if n == 2 {
            return false;
        }
        let mut inside = false;
        for i in 0..n {
            let (a, b) = (poly[i], poly[(i + 1) % n]);
            if (a[1] > p[1]) != (b[1] > p[1]) {
                let d = b[1] as i128 - a[1] as i128;
                let lhs = (p[0] as i128 - a[0] as i128) * d;
                let rhs = (p[1] as i128 - a[1] as i128) * (b[0] as i128 - a[0] as i128);
                if (d > 0 && lhs < rhs) || (d < 0 && lhs > rhs) {
                    inside = !inside;
                }
            }
        }
        inside
    }

    /// Every pair of edges meets only where the ring says it should: adjacent ones at their shared
    /// vertex, and no others anywhere.
    fn is_simple(poly: &[[u32; 2]]) -> bool {
        let n = poly.len();
        if n < 3 {
            return true;
        }
        for i in 0..n {
            for j in (i + 1)..n {
                let (a0, a1) = (poly[i], poly[(i + 1) % n]);
                let (b0, b1) = (poly[j], poly[(j + 1) % n]);
                let adjacent = j == i + 1 || (i == 0 && j == n - 1);
                if adjacent {
                    let shared = if j == i + 1 { a1 } else { a0 };
                    // Collinear overlap past the shared vertex is the failure adjacency hides.
                    let far_b = if j == i + 1 { b1 } else { b0 };
                    let far_a = if j == i + 1 { a0 } else { a1 };
                    if on_segment(shared, far_a, far_b) || on_segment(shared, far_b, far_a) {
                        return false;
                    }
                } else if segments_meet(a0, a1, b0, b1) {
                    return false;
                }
            }
        }
        true
    }

    #[test]
    fn the_shape_contains_every_member() {
        let members = moon();
        let hull = concave_hull(&members);
        for m in &members {
            assert!(contains(&hull, *m), "member {m:?} fell outside {hull:?}");
        }
    }

    #[test]
    fn the_shape_is_a_simple_ring() {
        let hull = concave_hull(&moon());
        assert!(hull.len() >= 3);
        assert!(is_simple(&hull), "the ring crosses itself: {hull:?}");
    }

    /// The whole point of the change: the wrap's straight edge across the bite is replaced by a
    /// boundary that follows it, so the empty middle of the bite stops being inside the shape.
    #[test]
    fn a_crescent_is_tighter_than_its_convex_wrap() {
        let members = moon();
        let convex = convex_hull(&members);
        let concave = concave_hull(&members);

        assert!(
            double_area(&concave) < double_area(&convex),
            "concave {} is not tighter than convex {}",
            double_area(&concave),
            double_area(&convex)
        );
        let in_the_bite = IN_THE_BITE;
        assert!(
            contains(&convex, in_the_bite),
            "the wrap is supposed to swallow the bite"
        );
        assert!(
            !contains(&concave, in_the_bite),
            "the concave shape still swallows the bite"
        );
    }

    /// A point set in convex position has nothing to dig into: every member is a wrap vertex or
    /// lies along one of its edges, so every candidate is on the boundary already. This holds
    /// whatever α is, which is why it is the property stated rather than a tolerance.
    #[test]
    fn a_point_set_in_convex_position_keeps_its_convex_wrap() {
        // A dense integer circle: 2,000 distinct lattice positions on a radius-3000 ring.
        let mut ring: Vec<[u32; 2]> = (0..2000)
            .map(|i| {
                let t = i as f64 * std::f64::consts::TAU / 2000.0;
                [
                    (4000.0 + 3000.0 * t.cos()).round() as u32,
                    (4000.0 + 3000.0 * t.sin()).round() as u32,
                ]
            })
            .collect();
        ring.sort_unstable();
        ring.dedup();
        assert_eq!(concave_hull(&ring), convex_hull(&ring));
    }

    /// Digging spends a bounded budget, longest edge first, so a shape's vertex count is its wrap's
    /// plus at most [`DIG_BUDGET`]. The wrap's own count is a floor rather than a target — see
    /// [`DIG_BUDGET`] for why it cannot be capped without either losing a member or inventing a
    /// vertex.
    #[test]
    fn the_vertex_budget_holds() {
        let members = flower();
        let convex = convex_hull(&members);
        let concave = concave_hull(&members);
        assert!(
            concave.len() <= convex.len() + DIG_BUDGET,
            "{} vertices over a wrap of {}",
            concave.len(),
            convex.len()
        );
        assert!(
            concave.len() >= convex.len() + DIG_BUDGET,
            "the flower's seven valleys did not exhaust the budget: {} over {}",
            concave.len(),
            convex.len()
        );
        assert!(is_simple(&concave));
        for m in &members {
            assert!(contains(&concave, *m));
        }
    }

    /// Members lying exactly on an edge the shape is about to dig stay inside it. They sit on the
    /// boundary the dig moves inwards, so a dig that ignored them would cut them off — which is what
    /// [`Buckets::nearest_inside`] returning a zero-distance candidate exists to prevent.
    #[test]
    fn members_on_a_dug_edge_stay_inside_the_shape() {
        // Two blocks with a wide empty gap between them, and three members strung along the wrap's
        // bridging edge across the gap's top.
        let mut members = Vec::new();
        for i in 0..40u32 {
            for j in 0..40u32 {
                members.push([1000 + i * 5, 1000 + j * 5]);
                members.push([3000 + i * 5, 1000 + j * 5]);
            }
        }
        members.push([1500, 1195]);
        members.push([2000, 1195]);
        members.push([2500, 1195]);
        let hull = concave_hull(&members);
        assert!(is_simple(&hull));
        for m in &members {
            assert!(contains(&hull, *m), "{m:?} fell outside {hull:?}");
        }
    }

    /// The degenerate cases keep the behaviour the convex wrap had, on the shape that replaced it.
    #[test]
    fn a_degenerate_shape_is_the_members_themselves() {
        assert_eq!(concave_hull(&[[3, 4]]), vec![[3, 4]]);
        assert_eq!(concave_hull(&[[3, 4], [3, 4]]), vec![[3, 4]]);
        assert_eq!(concave_hull(&[[0, 0], [1, 1]]), vec![[0, 0], [1, 1]]);
        assert_eq!(
            concave_hull(&[[0, 0], [1, 1], [2, 2]]),
            vec![[0, 0], [2, 2]],
            "collinear members leave two endpoints"
        );
    }

    /// The winding and the starting vertex are the wrap's, because digging only ever inserts
    /// between two existing vertices.
    #[test]
    fn the_shape_starts_at_the_lowest_vertex_and_winds_counter_clockwise() {
        let hull = concave_hull(&[[10, 0], [0, 10], [0, 0], [10, 10]]);
        assert_eq!(hull, vec![[0, 0], [10, 0], [10, 10], [0, 10]]);
        let moon = concave_hull(&moon());
        assert_eq!(moon[0], *moon.iter().min().unwrap());
        assert!(double_area(&moon) > 0, "the ring winds clockwise");
    }

    /// Two principals over one artifact: the narrow one's members are a subset of the broad one's,
    /// and its shape is a function of that subset alone — every vertex one of *its* members, and
    /// every one of its members inside. Nothing about the members it cannot see reaches it.
    #[test]
    fn a_subset_of_the_members_gives_a_shape_over_that_subset_alone() {
        let broad = moon();
        // A deterministic thinning — the narrow principal sees one member in three.
        let narrow: Vec<[u32; 2]> = broad.iter().copied().step_by(3).collect();

        let broad_hull = concave_hull(&broad);
        let narrow_hull = concave_hull(&narrow);
        assert_ne!(broad_hull, narrow_hull, "the thinning changed nothing");

        for v in &narrow_hull {
            assert!(narrow.contains(v), "{v:?} is not a member this viewer sees");
        }
        for m in &narrow {
            assert!(contains(&narrow_hull, *m));
        }
        // Recomputed over the same set it is the same shape: no request input, no iteration-order
        // tie-break, no float.
        assert_eq!(narrow_hull, concave_hull(&narrow));
    }

    #[test]
    fn a_hull_carries_vertices_and_not_the_members_along_its_edges() {
        // A square with a member at the midpoint of one edge and one in the middle.
        let points = [[0, 0], [10, 0], [10, 10], [0, 10], [5, 0], [5, 5]];
        let hull = convex_hull(&points);
        assert_eq!(hull.len(), 4, "four corners: {hull:?}");
        for corner in [[0, 0], [10, 0], [10, 10], [0, 10]] {
            assert!(hull.contains(&corner), "{corner:?} missing from {hull:?}");
        }
        assert!(!hull.contains(&[5, 0]), "a collinear member is not a vertex");
        assert!(!hull.contains(&[5, 5]), "an interior member is not a vertex");
    }

    /// A degenerate hull is the members themselves. Rounding one up to an area would draw a region
    /// no member occupies — a shape asserting more than the data does.
    #[test]
    fn a_degenerate_hull_is_the_members_themselves() {
        assert_eq!(convex_hull(&[[3, 4]]), vec![[3, 4]]);
        assert_eq!(convex_hull(&[[3, 4], [3, 4]]), vec![[3, 4]]);
        assert_eq!(convex_hull(&[[0, 0], [1, 1]]), vec![[0, 0], [1, 1]]);
        // Three collinear members are a segment, not a triangle.
        assert_eq!(
            convex_hull(&[[0, 0], [1, 1], [2, 2]]),
            vec![[0, 0], [2, 2]],
            "collinear members leave two endpoints"
        );
    }

    /// The hull's winding is fixed, because the oracle compares vertex lists and a hull that
    /// started at a different vertex or wound the other way would differ byte-for-byte while being
    /// the same shape.
    #[test]
    fn the_hull_starts_at_the_lowest_vertex_and_winds_counter_clockwise() {
        let hull = convex_hull(&[[10, 0], [0, 10], [0, 0], [10, 10]]);
        assert_eq!(hull, vec![[0, 0], [10, 0], [10, 10], [0, 10]]);
    }
}

