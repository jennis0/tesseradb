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
//! The hull is the expensive one: it is a **concave** shape over the visible members rather than
//! their convex wrap, and **one ring per separated group of them** rather than one ring per
//! artifact ([`concave_rings`]) — a sort, a grouping pass, a bucketing pass and a bounded number of
//! digs, 1.6× the convex path over a whole 197-artifact layer (`docs/design/artifact-shapes.md`
//! §7). It discloses nothing the wrap did not, and the argument is in [`concave_rings`]'s own
//! documentation rather than restated here.

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
    /// The hull's rings, each counter-clockwise from its lowest vertex, in grid units — a
    /// **concave (alpha) shape** over the visible members, not their convex wrap, and **one ring per
    /// α-group of those members** rather than one ring per artifact ([`concave_rings`]).
    ///
    /// Rings are ordered by their first vertex, so the value is a function of the member positions
    /// and not of the order they were gathered in. A membership of one visible member gives one ring
    /// of one vertex, of two gives one ring of two: the hull of a point set is that point set when
    /// it is degenerate, and rounding it up to a triangle would draw an area no member occupies.
    pub hull: Option<Vec<Vec<[u32; 2]>>>,
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
            ComputedProperty::Hull => out.hull = Some(concave_rings(&positions)),
        }
    }
    out
}

/// The vertices digging may add on top of the groups' convex wraps, **per artifact and not per
/// ring**.
///
/// **A budget for the digging, not an absolute cap, and the difference is forced.** Every vertex is
/// a visible member's position and each ring contains every member of its own group, so the groups'
/// wrap vertex counts are a floor: reducing them means either dropping a member outside every ring
/// or inventing a vertex no member occupies, and both are worse than a wide polygon. What digging
/// adds is what a cap can bound, and this bounds it at 64 vertices — 512 bytes of `hull_x`/`hull_y`
/// per artifact at the worst case, against the wraps' own count, which is what already rode on
/// every response. **64 is where the knee was measured**
/// (`docs/evidence/memos/2026-08-26-concave-hulls.md`): over one 197-artifact layer it gives shapes
/// 14% tighter in area for four times the hull bytes, and doubling it again buys 5 more points of
/// area for another 43 KB. The fidelity cost of running out is that a shape stops refining its
/// *shortest* remaining bridges, because digging spends the budget longest edge first — a coarser
/// shape, never a wrong one, since it still holds every member and every ring is still inside its
/// group's wrap.
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

/// A concave (alpha) shape over the visible members: **one simple ring per α-group**,
/// counter-clockwise from each ring's lowest vertex, on the integer grid.
///
/// **Why not the convex wrap.** An HDBSCAN cluster is an irregular density region — crescent,
/// branching, often both — and its convex hull swallows the empty space between the arms, overlaps
/// every sibling and draws single straight edges across the whole viewport. The vertices honestly
/// describe a shape the cluster does not have, which is why no client-side smoothing can repair it.
///
/// **Why not one ring.** A membership can be two separated clouds, and a single ring around both
/// claims the ground between them — a claim about where the members are that no α corrects, because
/// digging works inward from a boundary and a gap with a ring on both sides is not reachable from
/// either. So the members are grouped first ([`alpha_groups`]) and a ring is dug per group. The
/// order is what matters: the separation is decided before the vertex budget is spent, so it is
/// never a casualty of a cap.
///
/// **It discloses nothing the convex wrap did not.** The inputs are the same (`membership ∩
/// M_auth`, gathered by [`compute`] and nothing else), the derivation is the same per-request one,
/// every vertex is a visible member's position either way, and every ring is a *subset* of the
/// convex hull — so the shape says less about where the members this viewer cannot see are sitting,
/// not more. Several rings say less again: they are the same members drawn without the ground
/// between them. No leak-register row: nothing here lets a viewer end up knowing something about
/// data they were not served (`architecture.md` Appendix C's inclusion test).
///
/// **The construction: dig inward from each group's convex wrap.** Start each group at its convex
/// wrap, which contains that group's members. Repeatedly take the longest edge `(a, b)` above α
/// **across every ring**, find the member `c` of that ring's own group closest to the line through
/// `a` and `b` among those on the interior side of `a → b` that project inside the segment
/// ([`Buckets::nearest_inside`]), and replace the edge with `(a, c)` and `(c, b)` — carving the
/// triangle `a c b` out of that ring.
///
/// Two properties fall out of `c` being the *closest*:
///
/// - **Containment is preserved.** The triangle `a c b` lies inside the strip between the
///   perpendiculars at `a` and at `b`, because `c` does and projection is affine — so a member
///   strictly inside it is itself a candidate, and being strictly closer to the line than `c`
///   contradicts `c`'s minimality. The triangle is empty, so removing it removes no member, and
///   each ring contains every member of its own group at every step by induction from that group's
///   convex hull.
/// - **No arithmetic epsilon.** Minimising the perpendicular distance to the line through `a` and
///   `b` is minimising the cross product `(b − a) × (p − a)`, since the divisor `|ab|` is fixed per
///   edge. That is exact in `i128` (see [`convex_hull_of_sorted`] on why not `i64`), so the shape is a
///   function of the member positions and of nothing else — no float, no platform drift, no
///   tie-break that depends on iteration order.
///
/// A dig is refused, and its edge retired, when there is no such member or when that ring would
/// stop being simple — including when `c` already sits on the ring, which would make it touch
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
/// **What it does not carry is a hole.** Digging only ever moves a boundary inward, so an enclosed
/// void — one with members all the way around it — is not reachable and no ring encloses another.
/// The family that does produce interior rings is the α-complex, and it drops members outside its
/// own shape, which is the display contradiction the exact-only rule exists to prevent
/// (`artifact-shapes.md` §4, and §6 for the decision and its residual).
///
/// Cost is `O(n)` to group and to bucket the members plus, per dig, one pruned pass over the
/// buckets and one pass over the ring, against the convex hull's `O(n log n)` sort, which still
/// dominates. Measured over 197 artifacts of 6,146 … 2,422,484 members in
/// `docs/design/artifact-shapes.md` §7.
fn concave_rings(points: &[[u32; 2]]) -> Vec<Vec<[u32; 2]>> {
    let mut p: Vec<[u32; 2]> = points.to_vec();
    p.sort_unstable();
    p.dedup();
    let convex = convex_hull_of_sorted(&p);
    // One member is that point and two are that segment, exactly as before: an area no member
    // occupies asserts more than the data does, and there is nothing to dig into or to group.
    if convex.len() < 3 {
        return vec![convex];
    }

    let alpha_sq = bridge_threshold(&convex);
    let (labels, groups) = alpha_groups(&p, alpha_sq);
    let mut rings: Vec<Ring> = if groups == 1 {
        // The whole membership is one group, which is the ordinary case; the wrap is already
        // computed and the members are already sorted.
        vec![Ring::new(convex, &p)]
    } else {
        let mut members: Vec<Vec<[u32; 2]>> = vec![Vec::new(); groups];
        for (i, q) in p.iter().enumerate() {
            members[labels[i] as usize].push(*q);
        }
        members
            .iter()
            .map(|m| Ring::new(convex_hull_of_sorted(m), m))
            .collect()
    };

    // **One budget for the artifact, spent longest bridge first across every ring**, so a shape's
    // vertex count is the sum of its groups' wraps plus at most [`DIG_BUDGET`] — the same bound the
    // wire carried when there was one ring, and not a budget that multiplies with the group count.
    let mut inserted = 0usize;
    while inserted < DIG_BUDGET {
        let Some((r, i)) = longest_bridge(&rings, alpha_sq) else {
            break;
        };
        let ring = &mut rings[r];
        let n = ring.poly.len();
        let (a, b) = (ring.poly[i].pos, ring.poly[(i + 1) % n].pos);
        match ring.grid.nearest_inside(a, b) {
            Some(c) if dig_is_admissible(&ring.poly, i, c) => {
                // The replaced edge's flag goes with it; `poly[i]` now carries `(a, c)` and the
                // inserted vertex carries `(c, b)`, both fresh.
                ring.poly.insert(
                    i + 1,
                    Vertex {
                        pos: c,
                        retired: false,
                    },
                );
                inserted += 1;
            }
            _ => ring.poly[i].retired = true,
        }
    }

    let mut out: Vec<Vec<[u32; 2]>> = rings
        .into_iter()
        .map(|r| r.poly.into_iter().map(|v| v.pos).collect())
        .collect();
    // Ordered by first vertex. Groups partition the members, so no two rings start at the same
    // position and the order is total — a shape is a ring list, not a ring list up to permutation.
    out.sort_unstable();
    out
}

/// One group's ring under construction, with the buckets over that group's own members.
struct Ring {
    poly: Vec<Vertex>,
    grid: Buckets,
}

impl Ring {
    /// `members` must be sorted and deduplicated, and `convex` must be their convex wrap.
    fn new(convex: Vec<[u32; 2]>, members: &[[u32; 2]]) -> Ring {
        Ring {
            poly: convex
                .into_iter()
                .map(|pos| Vertex { pos, retired: false })
                .collect(),
            grid: Buckets::build(members),
        }
    }
}

/// How many cells of the grouping grid span α.
///
/// **Two, and the trade it sets is measured.** The grouping joins members whose cells are within
/// this many cells of each other along both axes, so a cell side of α/`GROUP_CELLS_PER_ALPHA` makes
/// the join *complete* — every pair within α is joined — while joining members as far apart as
/// √2·(1 + 1/`GROUP_CELLS_PER_ALPHA`)·α, which at 2 is 2.12α. Over the 197-artifact measurement
/// layer the result agrees with exact single-linkage at α on 192 artifacts and coarsens the rest;
/// at one cell per α it agrees on 190, and at four on 192 (`artifact-shapes.md` §5). The exact
/// alternative needs a Delaunay triangulation, which is the route ruling C measured and declined.
const GROUP_CELLS_PER_ALPHA: u64 = 2;

/// The α-groups of the visible members: one label per member, and how many groups there are.
///
/// **Grid connectivity at α, conservative in the direction that cannot lie.** The members are
/// bucketed into a square grid anchored at their own bounding box, with a cell side of
/// α/[`GROUP_CELLS_PER_ALPHA`], and two members are joined when their cells are within
/// [`GROUP_CELLS_PER_ALPHA`] cells of each other along both axes. A displacement of at most α moves
/// a cell index by at most that many cells per axis, so **every pair within α lands in one group**:
/// the grouping never separates members that single-linkage at α would join. It does join members
/// further apart than α, and that is the safe direction — an over-joined group draws the single
/// ring the wire drew before, while an over-split one would claim a gap the members do not have.
///
/// **The grid never holds more cells than there are members.** Where the members are so scattered
/// that a cell side of α/[`GROUP_CELLS_PER_ALPHA`] would need more, the side doubles until they
/// fit, which only ever joins more. That keeps the pass `O(members)` with no data-dependent worst
/// case — the exact route, cutting the Delaunay edges longer than α, has none either but costs a
/// triangulation, measured at 1.4 s on the largest artifact of the measurement layer against 0.16 s
/// for the whole dig (`artifact-shapes.md` §4.1).
fn alpha_groups(p: &[[u32; 2]], alpha_sq: i128) -> (Vec<u32>, usize) {
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0u32, 0u32);
    for q in p {
        x0 = x0.min(q[0]);
        y0 = y0.min(q[1]);
        x1 = x1.max(q[0]);
        y1 = y1.max(q[1]);
    }
    let (wx, wy) = ((x1 - x0) as u64 + 1, (y1 - y0) as u64 + 1);
    // α as a length. The square root is the only float in this module, and it is safe here for two
    // reasons rather than one: it is IEEE-754 correctly rounded, so it is identical on every
    // platform; and it is rounded *up* to a whole grid unit, so `2 · side ≥ α` holds with an
    // integer's margin that a last-bit error cannot cross — which is the inequality the
    // completeness argument above rests on. Every join is then an integer comparison of cell
    // indices.
    let alpha = (alpha_sq as f64).sqrt();
    let mut side = (alpha / GROUP_CELLS_PER_ALPHA as f64).ceil().max(1.0) as u64;
    let (mut nx, mut ny) = (wx.div_ceil(side), wy.div_ceil(side));
    while nx.saturating_mul(ny) > p.len() as u64 {
        side = side.saturating_mul(2);
        nx = wx.div_ceil(side);
        ny = wy.div_ceil(side);
    }

    let total = (nx * ny) as usize;
    let cell = |q: &[u32; 2]| -> usize {
        let cx = (q[0] - x0) as u64 / side;
        let cy = (q[1] - y0) as u64 / side;
        (cy * nx + cx) as usize
    };
    let mut occupied = vec![false; total];
    for q in p {
        occupied[cell(q)] = true;
    }

    // Union-find over the *cells*, not the members: the grid has at most one cell per member and
    // usually far fewer, so the join costs a bounded sweep over cells rather than a neighbourhood
    // query per member.
    let mut parent: Vec<u32> = (0..total as u32).collect();
    let r = GROUP_CELLS_PER_ALPHA as i64;
    for cy in 0..ny as i64 {
        for cx in 0..nx as i64 {
            let k = (cy * nx as i64 + cx) as usize;
            if !occupied[k] {
                continue;
            }
            // Half the neighbourhood; the other half is reached from the cell on its own side.
            for dx in 0..=r {
                for dy in -r..=r {
                    if dx == 0 && dy <= 0 {
                        continue;
                    }
                    let (ax, ay) = (cx + dx, cy + dy);
                    if ax < 0 || ay < 0 || ax >= nx as i64 || ay >= ny as i64 {
                        continue;
                    }
                    let j = (ay * nx as i64 + ax) as usize;
                    if occupied[j] {
                        union(&mut parent, k, j);
                    }
                }
            }
        }
    }

    // Labels are minted in cell order, so they are a function of the positions rather than of the
    // order the members were gathered in.
    let mut label = vec![u32::MAX; total];
    let mut groups = 0u32;
    for k in 0..total {
        if !occupied[k] {
            continue;
        }
        let root = find(&mut parent, k as u32) as usize;
        if label[root] == u32::MAX {
            label[root] = groups;
            groups += 1;
        }
        label[k] = label[root];
    }
    (p.iter().map(|q| label[cell(q)]).collect(), groups as usize)
}

fn find(parent: &mut [u32], mut i: u32) -> u32 {
    while parent[i as usize] != i {
        parent[i as usize] = parent[parent[i as usize] as usize];
        i = parent[i as usize];
    }
    i
}

fn union(parent: &mut [u32], a: usize, b: usize) {
    let (ra, rb) = (find(parent, a as u32), find(parent, b as u32));
    if ra != rb {
        parent[ra as usize] = rb;
    }
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

/// The ring and edge index of the longest live edge above α, or `None` when no ring is still
/// bridging.
///
/// **Across every ring, not one ring at a time**, because the budget is the artifact's: spending it
/// on a group's longest remaining bridge is what the single-ring construction did, and doing it per
/// ring in turn would spend vertices on a small group's short bridges while a large group's long
/// one went undug.
fn longest_bridge(rings: &[Ring], alpha_sq: i128) -> Option<(usize, usize)> {
    let mut best: Option<(i128, usize, usize)> = None;
    for (r, ring) in rings.iter().enumerate() {
        let n = ring.poly.len();
        // A degenerate ring — one member, or two, or members in convex position along a line — has
        // no interior to dig into.
        if n < 3 {
            continue;
        }
        for i in 0..n {
            if ring.poly[i].retired {
                continue;
            }
            let length = sq_len(ring.poly[i].pos, ring.poly[(i + 1) % n].pos);
            if length <= alpha_sq {
                continue;
            }
            if best.is_none_or(|(b, _, _)| length > b) {
                best = Some((length, r, i));
            }
        }
    }
    best.map(|(_, r, i)| (r, i))
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
/// deduplicated — [`concave_rings`] does that once and then buckets the same vector, rather than
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
/// [`concave_rings`], which has already sorted.
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

    /// The single ring of a membership that is one α-group, with that being asserted rather than
    /// assumed — a test that silently accepted a second ring would stop testing what it says.
    fn one_ring(members: &[[u32; 2]]) -> Vec<[u32; 2]> {
        let rings = concave_rings(members);
        assert_eq!(rings.len(), 1, "expected one group, got {}", rings.len());
        rings.into_iter().next().unwrap()
    }

    #[test]
    fn the_shape_contains_every_member() {
        let members = moon();
        let hull = one_ring(&members);
        for m in &members {
            assert!(contains(&hull, *m), "member {m:?} fell outside {hull:?}");
        }
    }

    #[test]
    fn the_shape_is_a_simple_ring() {
        let hull = one_ring(&moon());
        assert!(hull.len() >= 3);
        assert!(is_simple(&hull), "the ring crosses itself: {hull:?}");
    }

    /// The whole point of the change: the wrap's straight edge across the bite is replaced by a
    /// boundary that follows it, so the empty middle of the bite stops being inside the shape.
    #[test]
    fn a_crescent_is_tighter_than_its_convex_wrap() {
        let members = moon();
        let convex = convex_hull(&members);
        let concave = one_ring(&members);

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
        assert_eq!(one_ring(&ring), convex_hull(&ring));
    }

    /// Digging spends a bounded budget, longest edge first, so a shape's vertex count is its wrap's
    /// plus at most [`DIG_BUDGET`]. The wrap's own count is a floor rather than a target — see
    /// [`DIG_BUDGET`] for why it cannot be capped without either losing a member or inventing a
    /// vertex.
    #[test]
    fn the_vertex_budget_holds() {
        let members = flower();
        let convex = convex_hull(&members);
        let concave = one_ring(&members);
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
        // The three bridging members chain the two blocks into one group, so this is still one ring
        // — which is what makes it a test of the dug edge rather than of the grouping.
        let hull = one_ring(&members);
        assert!(is_simple(&hull));
        for m in &members {
            assert!(contains(&hull, *m), "{m:?} fell outside {hull:?}");
        }
    }

    /// Two disks of radius 400 whose centres are 3,000 apart — a membership that is honestly two
    /// clouds, with a gap far wider than any α its own wrap can produce.
    fn two_clouds() -> Vec<[u32; 2]> {
        sample(3000, 2000, |x, y| {
            (x + 1500) * (x + 1500) + y * y <= 400 * 400
                || (x - 1500) * (x - 1500) + y * y <= 400 * 400
        })
    }

    /// **The multi-ring ruling, at its own case.** A membership that is two separated clouds gets a
    /// ring each, every member is inside exactly one of them, and the pair claims a fraction of the
    /// ground the one ring spanning both would claim.
    #[test]
    fn two_separated_clouds_get_a_ring_each() {
        let members = two_clouds();
        let rings = concave_rings(&members);
        assert_eq!(rings.len(), 2, "two clouds gave {} rings", rings.len());

        for m in &members {
            let inside = rings.iter().filter(|r| contains(r, *m)).count();
            assert_eq!(inside, 1, "{m:?} is inside {inside} rings, not exactly one");
        }
        for r in &rings {
            assert!(is_simple(r), "a ring crosses itself: {r:?}");
            for v in r {
                assert!(members.contains(v), "{v:?} is not a member's position");
            }
        }

        let wrap = convex_hull(&members);
        let drawn: i128 = rings.iter().map(|r| double_area(r)).sum();
        assert!(
            drawn * 2 < double_area(&wrap),
            "two rings claim {drawn} against the wrap's {} — the gap is still being drawn",
            double_area(&wrap)
        );
    }

    /// **The grouping never separates members single-linkage at α would join**, which is the whole
    /// of its soundness: it may join members further apart, and that only ever draws the wider
    /// shape the wire drew before. Checked exhaustively against the definition on a cloud small
    /// enough to compare every pair.
    #[test]
    fn a_pair_within_alpha_is_never_split_across_groups() {
        let members = two_clouds();
        let mut p = members.clone();
        p.sort_unstable();
        p.dedup();
        let alpha_sq = bridge_threshold(&convex_hull_of_sorted(&p));
        let (labels, groups) = alpha_groups(&p, alpha_sq);
        assert!(groups > 1, "the fixture is supposed to be more than one group");

        let mut joined = 0usize;
        for i in 0..p.len() {
            for j in (i + 1)..p.len() {
                if sq_len(p[i], p[j]) <= alpha_sq {
                    assert_eq!(
                        labels[i], labels[j],
                        "{:?} and {:?} are within α and landed in different groups",
                        p[i], p[j]
                    );
                    joined += 1;
                }
            }
        }
        assert!(joined > 0, "no pair was within α, so nothing was tested");
    }

    /// The rings are a function of the member positions and not of the order they arrive in: the
    /// ring order is fixed by each ring's own lowest vertex, and groups partition the members, so
    /// no two rings can start at the same position.
    #[test]
    fn the_rings_do_not_depend_on_the_order_the_members_arrive_in() {
        let members = two_clouds();
        let forwards = concave_rings(&members);
        let backwards: Vec<[u32; 2]> = members.iter().copied().rev().collect();
        assert_eq!(forwards, concave_rings(&backwards));

        let mut starts: Vec<[u32; 2]> = forwards.iter().map(|r| r[0]).collect();
        let sorted = {
            let mut s = starts.clone();
            s.sort_unstable();
            s
        };
        assert_eq!(starts, sorted, "the rings are not ordered by their first vertex");
        starts.dedup();
        assert_eq!(starts.len(), forwards.len(), "two rings start at one position");
    }

    /// **A void with members all the way around it stays inside the ring, and that is the decision
    /// rather than an oversight.** Digging works inward from a boundary, so an enclosed void is not
    /// reachable from one; the family that does produce interior rings is the α-complex, and it
    /// leaves members outside their own shape. The residual is stated here so a reader meets it at
    /// the mechanism: an annulus of members is drawn as a disk.
    #[test]
    fn an_enclosed_void_is_drawn_as_filled_because_the_wire_carries_no_holes() {
        let members = sample(6000, 1000, |x, y| {
            let r = x * x + y * y;
            (600 * 600..=1000 * 1000).contains(&r)
        });
        let rings = concave_rings(&members);
        assert_eq!(rings.len(), 1, "an annulus is one group");
        assert!(
            contains(&rings[0], [2_000_000, 2_000_000]),
            "the hole is outside the ring, so a hole was carried after all"
        );
        for m in &members {
            assert!(contains(&rings[0], *m));
        }
    }

    /// **The budget is the artifact's, not the ring's.** Several groups share one allowance of
    /// digs, spent on the longest bridge anywhere, so the wire bound is the sum of the groups'
    /// wraps plus [`DIG_BUDGET`] — the bound one ring carried, and not one that multiplies with the
    /// group count.
    #[test]
    fn the_budget_is_shared_across_the_rings() {
        // Three flowers, far enough apart to be three groups, each with valleys to spend on.
        let mut members = Vec::new();
        for (k, offset) in [0u32, 40_000, 80_000].into_iter().enumerate() {
            for m in flower() {
                members.push([m[0] + offset, m[1] + (k as u32) * 3]);
            }
        }
        let rings = concave_rings(&members);
        assert_eq!(rings.len(), 3, "three flowers gave {} rings", rings.len());

        let mut floor = 0usize;
        for r in &rings {
            let own: Vec<[u32; 2]> = members
                .iter()
                .copied()
                .filter(|m| contains(r, *m))
                .collect();
            floor += convex_hull(&own).len();
            assert!(is_simple(r));
        }
        let vertices: usize = rings.iter().map(|r| r.len()).sum();
        assert!(
            vertices <= floor + DIG_BUDGET,
            "{vertices} vertices over three wraps of {floor} — the budget multiplied"
        );
        for m in &members {
            assert!(rings.iter().any(|r| contains(r, *m)), "{m:?} fell outside every ring");
        }
    }

    /// The degenerate cases keep the behaviour the convex wrap had, on the shape that replaced it.
    #[test]
    fn a_degenerate_shape_is_the_members_themselves() {
        assert_eq!(concave_rings(&[[3, 4]]), vec![vec![[3, 4]]]);
        assert_eq!(concave_rings(&[[3, 4], [3, 4]]), vec![vec![[3, 4]]]);
        assert_eq!(concave_rings(&[[0, 0], [1, 1]]), vec![vec![[0, 0], [1, 1]]]);
        assert_eq!(
            concave_rings(&[[0, 0], [1, 1], [2, 2]]),
            vec![vec![[0, 0], [2, 2]]],
            "collinear members leave two endpoints"
        );
    }

    /// The winding and the starting vertex are the wrap's, because digging only ever inserts
    /// between two existing vertices.
    #[test]
    fn the_shape_starts_at_the_lowest_vertex_and_winds_counter_clockwise() {
        let hull = one_ring(&[[10, 0], [0, 10], [0, 0], [10, 10]]);
        assert_eq!(hull, vec![[0, 0], [10, 0], [10, 10], [0, 10]]);
        let moon = one_ring(&moon());
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

        let broad_hull = one_ring(&broad);
        let narrow_hull = one_ring(&narrow);
        assert_ne!(broad_hull, narrow_hull, "the thinning changed nothing");

        for v in &narrow_hull {
            assert!(narrow.contains(v), "{v:?} is not a member this viewer sees");
        }
        for m in &narrow {
            assert!(contains(&narrow_hull, *m));
        }
        // Recomputed over the same set it is the same shape: no request input, no iteration-order
        // tie-break, no float.
        assert_eq!(narrow_hull, one_ring(&narrow));
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

