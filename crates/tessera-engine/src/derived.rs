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
//! digs, 3.3× the convex path over a whole 197-artifact layer (`docs/design/artifact-shapes.md`
//! §7). It discloses nothing the wrap did not, and the argument is in [`concave_rings`]'s own
//! documentation rather than restated here.
//!
//! ## This module is single-threaded, deliberately
//!
//! **Nothing here uses `rayon`, and nothing here may acquire one** (owner ruling, 2026-08-28).
//! Parallelism in this engine lives at the **request** level — `Engine::viewport` installs the one
//! shared compute pool for a request's tile loop — so that concurrent requests use the cores. A
//! `par_iter` over an artifact's members, or over a response's artifacts, would let one request
//! oversubscribe the pool the others are queued behind, which trades a served viewer's latency for
//! a hovering one's. The two ways this module was made cheap instead are the ones a second thread
//! would have hidden: the input is reduced before the shape is computed
//! ([`QUANTISE_DIVISIONS`]), and the answer is not computed twice ([`crate::derived_cache`]).

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

    /// Every visible row's position, in row order — what [`compute`] gathers, walked segment by
    /// segment instead of row by row.
    ///
    /// **The same answer as calling [`position`](Self::position) on each row, at a fraction of the
    /// cost, and it is the row-space property that makes it so.** Rows are Morton rank, and a
    /// segment holds a contiguous range of them, so the visible rows of one segment are a
    /// contiguous stretch of the mask: the segment is resolved once for the stretch rather than
    /// re-resolved for every row, and the two columns are read in ascending index order rather than
    /// through a reverse scan that starts again each time. Over the 197-artifact measurement layer
    /// at full membership this is a *measured* 168 ms → 44 ms for 12.8M members
    /// (`artifact-shapes.md` §7.1).
    ///
    /// A row past its segment's columns is dropped, as [`position`](Self::position) drops it and
    /// for the same reason — `(0, 0)` is a real position on the map. A segment's extent is clipped
    /// to the next segment's base, so a row that both could claim is resolved to the later one,
    /// which is what the reverse scan does.
    pub fn positions(&self, visible: &Bitmap) -> Vec<[u32; 2]> {
        let mut out: Vec<[u32; 2]> = Vec::with_capacity(visible.cardinality() as usize);
        for (i, &(segment, row_base)) in self.segments.iter().enumerate() {
            let morton = segment.morton.u32();
            let residual = segment.columns.residual();
            let rows = morton.len().min(residual.len()) as u64;
            let next = self
                .segments
                .get(i + 1)
                .map_or(u32::MAX as u64 + 1, |&(_, base)| base as u64);
            let hi = (row_base as u64 + rows).min(next);
            if hi <= row_base as u64 {
                continue;
            }
            let mut it = visible.iter();
            it.reset_at_or_after(row_base);
            for row in it {
                if row as u64 >= hi {
                    break;
                }
                let idx = (row - row_base) as usize;
                let (qx, qy) = unsplit32(MortonCode::new(morton[idx]), residual[idx]);
                out.push([qx, qy]);
            }
        }
        out
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
    let positions = locator.positions(visible);
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
/// adds is what a cap can bound, and this bounds it at 2,048 vertices — 16 KB of `hull_x`/`hull_y`
/// per artifact at the worst case, on top of the wraps' own count, which is what already rode on
/// every response.
///
/// **2,048 is a wire-size guard and not a fidelity control, which is the whole point of the
/// number** (`artifact-shapes.md` §8 B). It was 64, and at 64 the cap *was* the fidelity control:
/// 108 of 197 artifacts on `clusters/hdbscan` ran out of budget with a bridging edge still live,
/// 34 of 64 on `clusters/kmeans` and 100 of 574 on `clusters/toponymy` level 3, so what the served
/// shape followed was the cap rather than the members. Swept over those three layers at the
/// grouping as it is now built (`tests/hull_geometry.rs`, `the_budget_sweep`), the dig **runs out
/// of work on its own** at 732, 833 and 197 digs respectively: past those every column — vertices,
/// bytes, area, time — is identical to the unbounded dig, and no artifact on any of the three is
/// capped at 1,024 or beyond. 2,048 sits 2.5× clear of the largest of them, so a corpus rougher
/// than these three still gets the shape its members ask for rather than the shape the cap allows.
///
/// **What it costs**, over `clusters/hdbscan` at full membership: 12,497 → 28,459 hull vertices,
/// 100,836 → 228,532 bytes of `hull_x`/`hull_y` for the whole layer, and 0.89 → 1.89 s of
/// derivation for all 197 artifacts. The shapes come in from 0.870 to 0.803 of the area of the
/// rings the grouping alone would have drawn, and the tightest from 0.290 to 0.255.
///
/// The fidelity cost of running out — for the pathological membership this still bounds — is that
/// a shape stops refining its *shortest* remaining bridges, because digging spends the budget
/// longest edge first: a coarser shape, never a wrong one, since it still holds every member and
/// every ring is still inside its group's wrap.
const DIG_BUDGET: usize = 2_048;

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
    dig_rings(points, DIG_BUDGET).0
}

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
const QUANTISE_DIVISIONS: u32 = 1_024;

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
const REDUCTION_FLOOR: usize = 75_000;

/// [`QUANTISE_DIVISIONS`], for the measurement seams — the family comparison in
/// `tests/hull_triangulation.rs` has to give the triangulated route the same reduced input the dig
/// receives, or it is comparing two constructions over two different clouds.
#[doc(hidden)]
pub const SERVED_QUANTISE_DIVISIONS: u32 = QUANTISE_DIVISIONS;

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
/// The representatives [`dig_rings_at`] would compute a shape over — **the third measurement seam,
/// and public for [`dig_rings`]'s reason**.
///
/// The sweep has to ask what binning did to α, which is a statistic of the representatives' own
/// convex wrap, and a test binary that rebuilt the cell arithmetic from the constant would be
/// measuring its own copy of it.
#[doc(hidden)]
pub fn quantised(points: &[[u32; 2]], divisions: u32) -> Option<Vec<[u32; 2]>> {
    quantise(points, divisions, REDUCTION_FLOOR)
}

/// `floor` is [`REDUCTION_FLOOR`] on every serving route. It is a parameter rather than a constant
/// read here because the reduction's own properties — a representative is a member, every member
/// has one within a cell, α survives, the answer does not depend on the arrival order — hold at any
/// size and are tested on clouds small enough to check exhaustively, where the served floor would
/// switch the reduction off and leave nothing to assert about.
fn quantise(points: &[[u32; 2]], divisions: u32, floor: usize) -> Option<Vec<[u32; 2]>> {
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
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0u32, 0u32);
    for q in points {
        x0 = x0.min(q[0]);
        y0 = y0.min(q[1]);
        x1 = x1.max(q[0]);
        y1 = y1.max(q[1]);
    }
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
    let side = 1u64 << shift;
    if side <= 1 {
        // The grid is already the position lattice, so binning is the identity on a deduplicated
        // input and buys nothing.
        return None;
    }
    // **A cell is a contiguous row range, so the occupied cells are found by folding runs rather
    // than by binning into a grid** — the reduction's whole cost, on the input every route into it
    // actually supplies.
    //
    // The grid is anchored at the corpus's own origin rather than at the artifact's bounding box,
    // which makes each cell a Morton block of the corpus grid whenever its side is at least one
    // Morton cell. Rows are Morton rank (`architecture.md` §5.2, and `tile_index.rs`'s opening for
    // what else rests on it) and members reach here in row order, so the members of such a cell are
    // **consecutive**: one comparison per member finds every cell boundary, and nothing is
    // allocated for a cell that is not occupied. The anchor is the only thing that had to change to
    // make that true — an artifact-anchored cell straddles Morton blocks, and its members do not
    // arrive together.
    //
    // **What this replaces, and why three obvious routes lost.** The dense `nx × ny` grid it
    // replaces is *measured* at 215 ms of binning over the measurement layer, nearly all of it
    // faulting in a grid whose occupied fraction is small; a hash map keyed on the cell index is
    // 289 ms, a hash a member; indexing the dense grid in Morton order so its writes are local is
    // 260 ms, the locality bought back by a grid four times the size. Folding runs is one
    // comparison a member and no grid at all.
    //
    // **A *jump* per cell — the route the row-range property most obviously suggests — was refused
    // on measurement, and this is where the crossover is.** Skipping to the next cell by bitmap
    // arithmetic, a gallop over the segment's Morton column and a `reset_at_or_after` on the mask,
    // costs on the order of ten probes and a container walk against one sequential comparison per
    // member here. It can only win where a cell holds more members than that, and on
    // `notebook-2m4` it is not close: 12,808,679 members occupy 12,560,851 distinct Morton cells —
    // a **ratio of 1.02**, the corpus grid being 2^16 × 2^16 against 2.4M items — and at the served
    // resolution the densest artifact of the measurement layer holds 17.4 members to a cell against
    // a layer mean of 2.5 (`tests/hull_geometry.rs`, `the_cell_occupancy`). The jump becomes the cheaper route somewhere around a few tens of
    // members a cell, which is a corpus two orders of magnitude denser than this one.
    let half = side / 2;
    let mut runs: Vec<Run> = Vec::new();
    // A run out of Morton order is a cell that may already have been seen — row order restarts at
    // each segment of the view, and a caller outside the serving path need not be ordered at all.
    // Counted rather than assumed, and what it costs is one sort below.
    let mut descents = 0usize;
    let mut last = (u64::MAX, u64::MAX);
    let mut last_key = 0u64;
    for (i, q) in points.iter().enumerate() {
        let (cx, cy) = ((q[0] as u64) >> shift, (q[1] as u64) >> shift);
        let (mx, my) = ((cx << shift) + half, (cy << shift) + half);
        let (dx, dy) = ((q[0] as u64).abs_diff(mx), (q[1] as u64).abs_diff(my));
        let d = dx * dx + dy * dy;
        if (cx, cy) == last {
            let run = runs.last_mut().expect("a run exists once a cell has been seen");
            run.len += 1;
            if d < run.d || (d == run.d && *q < run.rep) {
                (run.rep, run.d) = (*q, d);
            }
        } else {
            let key = interleave_cell(cx, cy);
            if key < last_key {
                descents += 1;
            }
            last_key = key;
            runs.push(Run {
                cx: cx as u32,
                cy: cy as u32,
                rep: *q,
                d,
                start: i as u32,
                len: 1,
            });
            last = (cx, cy);
        }
    }

    // **One representative per occupied cell, and the same one whatever order the members arrived
    // in.** Where the runs were in Morton order each cell is exactly one run and there is nothing
    // to merge. Where they were not, merging equal keys under the rule the fold applies — nearest
    // the cell's centre, ties on the position — gives exactly what binning every member into a grid
    // would have given, because that rule is associative: a cell's representative is the
    // representative of its runs' representatives.
    let mut merged: Vec<(u64, u64, [u32; 2])> = Vec::new();
    if descents > 0 {
        merged = runs
            .iter()
            .map(|r| {
                (
                    interleave_cell(r.cx as u64, r.cy as u64),
                    r.d,
                    r.rep,
                )
            })
            .collect();
        merged.sort_unstable();
        merged.dedup_by_key(|r| r.0);
    }
    let cell_count = if descents > 0 { merged.len() } else { runs.len() };

    // **Nothing gained is reported as nothing done.** Where the members are spread thinner than the
    // grid, the occupied cells are nearly as numerous as the members and the representatives are
    // very nearly the input again — the same shape computed over a vector rebuilt for no reason.
    // Three quarters is where the reduction starts to return more than the sort it saves; below it
    // the artifact takes the exact path, which is the right answer twice over, since those are the
    // cheap ones (a *measured* 0.5 ms at under 10,000 members) and the ones a viewer zooms into.
    //
    // **The fold is what makes this an exact test rather than a guess about the grid.** What it
    // replaces asked whether the grid held more than four cells per member and skipped the
    // reduction when it did — a proxy for occupancy that put **170 of the 197** artifacts of the
    // measurement layer on the unreduced path at 1,024 divisions, which are the artifacts the shape
    // spends its time on (`artifact-shapes.md` §7.1). Counting the occupied cells costs one pass
    // and answers the question the proxy was standing in for.
    if cell_count * 4 > points.len() * 3 {
        return None;
    }

    // **The hull candidates are found per *cell*, so no member is tested on its own.** α is three
    // times the median edge of the whole membership's convex wrap, so every member that could be a
    // wrap vertex has to survive the reduction ([`extreme_octagon`]) — and testing 12.8M members
    // against eight edges is *measured* at 68 ms over the layer, on top of the 49 ms the eight
    // extremes cost in a pass of their own. Both questions are answered from the folded cells
    // instead, and **the answer is the same set**, not an approximation of it:
    //
    // - **The extremes are exact.** The best representative in a direction is a lower bound on the
    //   extreme, and a cell whose furthest corner falls short of that bound cannot hold it. The
    //   cells that do not fall short are a band one cell deep along the supporting line, and their
    //   members — and only theirs — are read.
    // - **The filter is exact.** `strictly_inside` is a conjunction of half-planes, so it is convex:
    //   a cell whose four corners are all strictly inside holds nothing that is not, and can be
    //   skipped whole. The cells that remain are the octagon's own boundary band.
    let octagon = extreme_octagon_over(points, &runs, side);
    let mut out: Vec<[u32; 2]> = Vec::with_capacity(cell_count + cell_count / 8);
    for run in &runs {
        let (cx0, cy0) = ((run.cx as u64) << shift, (run.cy as u64) << shift);
        let corners = [
            [cx0 as u32, cy0 as u32],
            [(cx0 + side - 1) as u32, cy0 as u32],
            [cx0 as u32, (cy0 + side - 1) as u32],
            [(cx0 + side - 1) as u32, (cy0 + side - 1) as u32],
        ];
        if corners.iter().all(|c| octagon.strictly_inside(*c)) {
            continue;
        }
        for q in &points[run.start as usize..run.start as usize + run.len as usize] {
            if !octagon.strictly_inside(*q) {
                out.push(*q);
            }
        }
    }

    if descents > 0 {
        out.extend(merged.into_iter().map(|(_, _, q)| q));
    } else {
        out.extend(runs.iter().map(|r| r.rep));
    }
    // Order and duplicates are left for the caller's sort and dedup, which every route into this
    // already pays: a member may be both its cell's representative and a hull candidate. What this
    // returns as a *set* is a function of the member positions alone — the fold's representative
    // rule is order-independent and the merge below restores it where the runs were not ordered —
    // and the set is what the shape is computed from.
    Some(out)
}

/// One occupied cell, as [`quantise`] folds it out of the members: the run of consecutive members
/// that fell in it, and the one nearest its centre.
///
/// It is a *run* and not a cell because a view's row order restarts at each of its segments, so one
/// cell can be met more than once; `reps` merges them and this list does not, which is what lets
/// the octagon phase read a run's members straight out of the input slice.
struct Run {
    cx: u32,
    cy: u32,
    /// The member nearest the cell's centre, ties broken on the position.
    rep: [u32; 2],
    /// `rep`'s squared distance to that centre.
    d: u64,
    /// Where this run starts in the members `quantise` was given.
    start: u32,
    len: u32,
}

/// The two cell coordinates interleaved — the key [`quantise`] folds runs on.
///
/// The standard bit-spreading, five shift-or-and steps an axis rather than a loop. Ordering by this
/// key is Morton order over the binning grid, which is what makes "this run is out of order" a
/// single comparison. [`tessera_spatial::morton::interleave`] is the same operation over the
/// *corpus* grid at 16 bits an axis; this one is over an artifact's binning grid, whose coordinates
/// run to 32 bits where the cell side is small, so it is a `u64` and not that function.
fn interleave_cell(cx: u64, cy: u64) -> u64 {
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
fn extreme_octagon_over(points: &[[u32; 2]], runs: &[Run], side: u64) -> Octagon {
    // Every representative is a member, so the best of them is a lower bound on each extreme —
    // established over every cell before any cell is opened, so the band below is as thin as the
    // representatives can make it.
    let mut best: [Option<(i64, [u32; 2])>; 8] = [None; 8];
    for run in runs {
        let (x, y) = (run.rep[0] as i64, run.rep[1] as i64);
        for (slot, (wx, wy)) in best.iter_mut().zip(OCTAGON_DIRECTIONS) {
            let score = wx * x + wy * y;
            if slot.is_none_or(|(s, b)| score > s || (score == s && run.rep < b)) {
                *slot = Some((score, run.rep));
            }
        }
    }
    let s1 = side as i64 - 1;
    for run in runs {
        // The cell's furthest corner in each direction is its low corner plus `side - 1` on
        // whichever axes that direction is positive on. A cell whose furthest corner scores below
        // the bound holds nothing that can beat it, in that direction or — over all eight — at all.
        let (lo_x, lo_y) = ((run.cx as u64 * side) as i64, (run.cy as u64 * side) as i64);
        let interesting =
            best.iter()
                .zip(OCTAGON_DIRECTIONS)
                .any(|(slot, (wx, wy))| {
                    let cx = lo_x + if wx > 0 { s1 } else { 0 };
                    let cy = lo_y + if wy > 0 { s1 } else { 0 };
                    wx * cx + wy * cy >= slot.map_or(i64::MIN, |(s, _)| s)
                });
        if !interesting {
            continue;
        }
        for q in &points[run.start as usize..run.start as usize + run.len as usize] {
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
}

/// How far inside every edge a member must be before [`extreme_octagon`]'s filter discards it, in
/// cross-product units. 2^16, against a *worst-case* `f64` error under 2^13 on a grid of 2^32 —
/// see [`extreme_octagon`] for why a margin makes an inexact test an exact answer.
const OCTAGON_MARGIN: f64 = 65_536.0;

#[doc(hidden)]
pub fn dig_rings(points: &[[u32; 2]], budget: usize) -> (Vec<Vec<[u32; 2]>>, bool) {
    dig_rings_at(points, budget, QUANTISE_DIVISIONS)
}

/// [`dig_rings`] at a quantising resolution the caller names, `0` meaning none — the second
/// measurement seam, and public for [`dig_rings`]'s reason.
///
/// The sweep that fixes [`QUANTISE_DIVISIONS`] has to run the same construction at several
/// resolutions, and against the unquantised shape, in one process.
#[doc(hidden)]
pub fn dig_rings_at(
    points: &[[u32; 2]],
    budget: usize,
    divisions: u32,
) -> (Vec<Vec<[u32; 2]>>, bool) {
    // **Reduce the input before computing the shape** ([`QUANTISE_DIVISIONS`]). It happens ahead of
    // the sort, which is where most of a large artifact's cost was: the corpus root's 2.42M
    // positions cost 121 ms to wrap and 167 ms to dig, and both figures are dominated by ordering
    // members whose individual positions the drawing cannot resolve.
    let reduced = quantise(points, divisions, REDUCTION_FLOOR);
    let points: &[[u32; 2]] = reduced.as_deref().unwrap_or(points);

    let mut p: Vec<[u32; 2]> = points.to_vec();
    p.sort_unstable();
    p.dedup();
    let convex = convex_hull_of_sorted(&p);
    // One member is that point and two are that segment, exactly as before: an area no member
    // occupies asserts more than the data does, and there is nothing to dig into or to group.
    if convex.len() < 3 {
        return (vec![convex], false);
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
    while inserted < budget {
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

    // Exhaustion is *the budget ran out while a bridge was still live*, which is what a cap acting
    // as a fidelity control looks like — distinct from a dig that stopped because every remaining
    // edge is shorter than α or has no candidate.
    let exhausted = inserted == budget && longest_bridge(&rings, alpha_sq).is_some();

    let mut out: Vec<Vec<[u32; 2]>> = rings
        .into_iter()
        .map(|r| r.poly.into_iter().map(|v| v.pos).collect())
        .collect();
    // Ordered by first vertex. Groups partition the members, so no two rings start at the same
    // position and the order is total — a shape is a ring list, not a ring list up to permutation.
    out.sort_unstable();
    (out, exhausted)
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
                .map(|pos| Vertex {
                    pos,
                    retired: false,
                })
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
    if ((d1 > 0) != (d2 > 0))
        && (d1 != 0 && d2 != 0)
        && ((d3 > 0) != (d4 > 0))
        && (d3 != 0 && d4 != 0)
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
        while hull.len() >= lower && orient(hull[hull.len() - 2], hull[hull.len() - 1], point) <= 0
        {
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
    ///
    /// **The served budget is deliberately not exhausted here.** This test asserted that the
    /// flower ran out of budget while [`DIG_BUDGET`] was 64, which made a cap that was acting as a
    /// fidelity control look like a property worth pinning. What the budget must do is bound the
    /// wire, so the bound is asserted at the served value and the *binding* is asserted through
    /// [`dig_rings`] at a budget small enough to bind — where the shape is still simple and still
    /// holds every member, which is the claim a truncated dig actually makes.
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
        assert!(is_simple(&concave));
        for m in &members {
            assert!(contains(&concave, *m));
        }

        // The cap binding, on the same members: 16 digs is far short of what the flower's seven
        // valleys want, so the shape stops exactly there — coarser, never wrong.
        let (truncated, exhausted) = dig_rings(&members, 16);
        assert!(exhausted, "16 digs did not bind on the flower");
        let truncated = &truncated[0];
        assert_eq!(truncated.len(), convex.len() + 16);
        assert!(truncated.len() < concave.len(), "the cap bought nothing");
        assert!(is_simple(truncated));
        for m in &members {
            assert!(contains(truncated, *m));
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
        assert!(
            groups > 1,
            "the fixture is supposed to be more than one group"
        );

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
        assert_eq!(
            starts, sorted,
            "the rings are not ordered by their first vertex"
        );
        starts.dedup();
        assert_eq!(
            starts.len(),
            forwards.len(),
            "two rings start at one position"
        );
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
        let bound = cell_bound(&members, QUANTISE_DIVISIONS);
        for m in &members {
            assert!(
                escape(&rings, *m) <= bound,
                "{m:?} is further from its shape than a quantising cell"
            );
        }
    }

    /// Twice the area of the triangle `a b p`, over `|ab|` — the distance from `p` to the line
    /// through `a` and `b`, clamped to the segment.
    fn point_to_segment(a: [u32; 2], b: [u32; 2], p: [u32; 2]) -> f64 {
        let (ax, ay) = (a[0] as f64, a[1] as f64);
        let (bx, by) = (b[0] as f64, b[1] as f64);
        let (px, py) = (p[0] as f64, p[1] as f64);
        let (vx, vy) = (bx - ax, by - ay);
        let len_sq = vx * vx + vy * vy;
        let t = if len_sq == 0.0 {
            0.0
        } else {
            (((px - ax) * vx + (py - ay) * vy) / len_sq).clamp(0.0, 1.0)
        };
        ((px - (ax + t * vx)).powi(2) + (py - (ay + t * vy)).powi(2)).sqrt()
    }

    /// How far `m` lies outside every ring of `rings`, in grid units; zero when it is inside one.
    fn escape(rings: &[Vec<[u32; 2]>], m: [u32; 2]) -> f64 {
        if rings.iter().any(|r| contains(r, m)) {
            return 0.0;
        }
        rings
            .iter()
            .flat_map(|r| {
                (0..r.len()).map(move |i| point_to_segment(r[i], r[(i + 1) % r.len()], m))
            })
            .fold(f64::INFINITY, f64::min)
    }

    /// The furthest a member can be from its own shape: the diagonal of one quantising cell.
    ///
    /// A cell side is `extent / QUANTISE_DIVISIONS` rounded up to a power of two, so at most twice
    /// that, and its diagonal at most √2 again. Every member shares its cell with a representative,
    /// which the dig does hold — so this is a bound and not a tolerance.
    fn cell_bound(members: &[[u32; 2]], divisions: u32) -> f64 {
        let (mut lo, mut hi) = ([u32::MAX; 2], [0u32; 2]);
        for m in members {
            for k in 0..2 {
                lo[k] = lo[k].min(m[k]);
                hi[k] = hi[k].max(m[k]);
            }
        }
        let extent = ((hi[0] - lo[0]).max(hi[1] - lo[1])) as f64;
        3.0 * extent / divisions as f64
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
        let reduced = quantise(&members, 32, 0).expect("a cloud this dense reduces");
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
            let reduced = quantise(&cloud, 32, 0).expect("a cloud this dense reduces");
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
        assert!(quantise(&members, QUANTISE_DIVISIONS, REDUCTION_FLOOR).is_none());
        assert_eq!(
            concave_rings(&members),
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
        let mut a = quantise(&members, 32, 0).expect("reduces");
        let mut b = quantise(&shuffled, 32, 0).expect("reduces");
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
                    quantise(&cloud, divisions, 0),
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

        let mut once = quantise(&sorted, 64, 0).expect("reduces");
        let mut twice = quantise(&twice_over, 64, 0).expect("reduces");
        once.sort_unstable();
        once.dedup();
        twice.sort_unstable();
        twice.dedup();
        assert_eq!(once, twice);
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
        assert!(
            !hull.contains(&[5, 0]),
            "a collinear member is not a vertex"
        );
        assert!(
            !hull.contains(&[5, 5]),
            "an interior member is not a vertex"
        );
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
