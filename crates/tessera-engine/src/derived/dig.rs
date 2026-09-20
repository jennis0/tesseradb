use super::buckets::Buckets;
use super::geometry::{convex_hull_of_sorted, on_segment, segments_meet, sq_len};
use super::groups::alpha_groups;
use super::reduce::{quantise, QUANTISE_DIVISIONS, REDUCTION_FLOOR};

/// The vertices digging may add on top of the groups' convex wraps, **per artifact and not per
/// ring**.
///
/// **A budget for the digging, not an absolute cap, and the difference is forced.** Every vertex is
/// a visible member's position and each ring contains every member of its own group, so the groups'
/// wrap vertex counts are a floor: reducing them means either dropping a member outside every ring
/// or inventing a vertex no member occupies, and both are worse than a wide polygon. What digging
/// adds is what a cap can bound, and this bounds it at 2,048 vertices — 16 KB of `shape_x`/`shape_y`
/// per artifact at the worst case, on top of the wraps' own count, which is what already rode on
/// every response.
///
/// **2,048 is a wire-size guard and not a fidelity control, which is the whole point of the
/// number** (`artifact-shapes.md` §8 B). It was 64, and at 64 the cap *was* the fidelity control:
/// 108 of 197 artifacts on `clusters/hdbscan` ran out of budget with a bridging edge still live,
/// 34 of 64 on `clusters/kmeans` and 100 of 574 on `clusters/toponymy` level 3, so what the served
/// shape followed was the cap rather than the members. Swept over those three layers at the
/// grouping as it is now built, the dig **runs out of work on its own** at 732, 833 and 197 digs
/// respectively: past those every column — vertices, bytes, area, time — is identical to the
/// unbounded dig, and no artifact on any of the three is capped at 1,024 or beyond. 2,048 sits
/// 2.5× clear of the largest of them, so a corpus rougher than these three still gets the shape
/// its members ask for rather than the shape the cap allows.
///
/// **What it costs**, over `clusters/hdbscan` at full membership: 12,497 → 28,459 hull vertices,
/// 100,836 → 228,532 bytes of `shape_x`/`shape_y` for the whole layer, and 0.89 → 1.89 s of
/// derivation for all 197 artifacts. The shapes come in from 0.870 to 0.803 of the area of the
/// rings the grouping alone would have drawn, and the tightest from 0.290 to 0.255. Rerun with
/// `cargo run --release -p tessera-bench --bin hull_cost -- <bundle>`.
///
/// The fidelity cost of running out — for the pathological membership this still bounds — is that
/// a shape stops refining its *shortest* remaining bridges, because digging spends the budget
/// longest edge first: a coarser shape, never a wrong one, since it still holds every member and
/// every ring is still inside its group's wrap.
pub(super) const DIG_BUDGET: usize = 2_048;

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
/// M_auth`, gathered by [`compute`](super::compute) and nothing else), the derivation is the same per-request one,
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
///
/// `bounds` is `points`'s own bounding box where the caller has already traversed them — the
/// binning grid is scaled from it, and a caller that computed a `box` has the answer in hand. It
/// is a *cache and not a parameter*: passing a box that is not `points`'s own would move the grid
/// and so the shape, which is why nothing outside this module can supply it.
pub(super) fn concave_rings(points: &[[u32; 2]], bounds: Option<[u32; 4]>) -> Vec<Vec<[u32; 2]>> {
    dig_rings_within(points, DIG_BUDGET, QUANTISE_DIVISIONS, bounds).0
}

/// [`concave_rings`] at a budget the caller names, with whether the budget bound the dig — a seam
/// for `tessera-bench`'s `hull_cost` and for the budget's own test.
#[doc(hidden)]
pub fn dig_rings(points: &[[u32; 2]], budget: usize) -> (Vec<Vec<[u32; 2]>>, bool) {
    dig_rings_within(points, budget, QUANTISE_DIVISIONS, None)
}

/// [`dig_rings`] at a quantising resolution the caller names, `0` meaning none, and with
/// `points`'s own bounding box already in hand — see [`concave_rings`].
pub(super) fn dig_rings_within(
    points: &[[u32; 2]],
    budget: usize,
    divisions: u32,
    bounds: Option<[u32; 4]>,
) -> (Vec<Vec<[u32; 2]>>, bool) {
    // **Reduce the input before computing the shape** ([`QUANTISE_DIVISIONS`]). It happens ahead of
    // the sort, which is where most of a large artifact's cost was: the corpus root's 2.42M
    // positions cost 121 ms to wrap and 167 ms to dig, and both figures are dominated by ordering
    // members whose individual positions the drawing cannot resolve.
    let reduced = quantise(points, divisions, REDUCTION_FLOOR, bounds);
    let points: &[[u32; 2]] = reduced.as_ref().map_or(points, |r| r.as_slice());

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

/// One boundary vertex, and whether the edge leaving it has been retired — an edge whose dig was
/// refused is never retried, which is what bounds the loop's refusals as the budget bounds its
/// insertions.
struct Vertex {
    pos: [u32; 2],
    retired: bool,
}

/// α², as the squared length an edge must exceed to be dug. Squared throughout so no root is taken:
/// the median commutes with squaring, so this is `(BRIDGE_FACTOR × median edge)²`.
pub(super) fn bridge_threshold(convex: &[[u32; 2]]) -> i128 {
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
///
/// **The box around the dig triangle retires most edges before any arithmetic is done on them.**
/// Every one of the four tests below needs a point of `f` inside the box spanned by `a`, `c` and
/// `b`: two are `segments_meet` against a segment lying in that box, and the other two ask whether a
/// named point lies on a closed segment inside it. So an edge whose own box misses the triangle's
/// answers all four with `false`, and four `u32` comparisons stand in for ten `i128` orientations.
/// It is a filter and not a rule — an edge that survives it is tested exactly as before — so the
/// admissible set is unchanged.
fn dig_is_admissible(poly: &[Vertex], i: usize, c: [u32; 2]) -> bool {
    let n = poly.len();
    let (a, b) = (poly[i].pos, poly[(i + 1) % n].pos);
    let lo = [a[0].min(b[0]).min(c[0]), a[1].min(b[1]).min(c[1])];
    let hi = [a[0].max(b[0]).max(c[0]), a[1].max(b[1]).max(c[1])];

    let (prev, next) = ((i + n - 1) % n, (i + 1) % n);
    for j in 0..n {
        if j == i {
            continue;
        }
        let (f0, f1) = (poly[j].pos, poly[(j + 1) % n].pos);
        if f0[0].min(f1[0]) > hi[0]
            || f0[0].max(f1[0]) < lo[0]
            || f0[1].min(f1[1]) > hi[1]
            || f0[1].max(f1[1]) < lo[1]
        {
            continue;
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::derived::geometry::convex_hull;
    use crate::derived::test_support::*;

    /// Whether the dig is admissible, tested against every edge of the ring rather than against the
    /// ones whose box meets the dig triangle's — the oracle the filter in [`dig_is_admissible`] is
    /// held to.
    fn admissible_by_scan(poly: &[Vertex], i: usize, c: [u32; 2]) -> bool {
        let n = poly.len();
        let (a, b) = (poly[i].pos, poly[(i + 1) % n].pos);
        let (prev, next) = ((i + n - 1) % n, (i + 1) % n);
        for j in 0..n {
            if j == i {
                continue;
            }
            let (f0, f1) = (poly[j].pos, poly[(j + 1) % n].pos);
            if on_segment(f0, f1, c) {
                return false;
            }
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

    /// The middle of the moon's bite, in grid coordinates.
    const IN_THE_BITE: [u32; 2] = [2_000_600, 2_000_000];

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
        let rings = concave_rings(members, None);
        assert_eq!(rings.len(), 1, "expected one group, got {}", rings.len());
        rings.into_iter().next().unwrap()
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

    /// **The box around the dig triangle is a filter and not a rule.** An edge it retires is one
    /// that cannot meet the two new edges at all, so the admissible set is the one the full pass
    /// finds — which is what keeps the ring simple, the property the wire's fill depends on.
    #[test]
    fn the_admissibility_filter_retires_only_edges_that_cannot_meet_the_dig() {
        for cloud in [moon(), flower(), two_clouds()] {
            let mut members = cloud.clone();
            members.sort_unstable();
            members.dedup();
            let convex = convex_hull_of_sorted(&members);
            if convex.len() < 3 {
                continue;
            }
            let poly: Vec<Vertex> = convex
                .iter()
                .map(|&pos| Vertex {
                    pos,
                    retired: false,
                })
                .collect();
            let mut asked = 0usize;
            for i in 0..poly.len() {
                // Every member is offered as the candidate, not only the one the dig would pick, so
                // the filter is asked about triangles a dig never reaches as well as the ones it does.
                for c in members.iter().step_by((members.len() / 60).max(1)) {
                    assert_eq!(
                        dig_is_admissible(&poly, i, *c),
                        admissible_by_scan(&poly, i, *c),
                        "the filter and the full pass disagree on edge {i} against {c:?}"
                    );
                    asked += 1;
                }
            }
            assert!(asked > 100, "the sweep asked {asked} questions");
        }
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

    /// **The multi-ring ruling, at its own case.** A membership that is two separated clouds gets a
    /// ring each, every member is inside exactly one of them, and the pair claims a fraction of the
    /// ground the one ring spanning both would claim.
    #[test]
    fn two_separated_clouds_get_a_ring_each() {
        let members = two_clouds();
        let rings = concave_rings(&members, None);
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

    /// The rings are a function of the member positions and not of the order they arrive in: the
    /// ring order is fixed by each ring's own lowest vertex, and groups partition the members, so
    /// no two rings can start at the same position.
    #[test]
    fn the_rings_do_not_depend_on_the_order_the_members_arrive_in() {
        let members = two_clouds();
        let forwards = concave_rings(&members, None);
        let backwards: Vec<[u32; 2]> = members.iter().copied().rev().collect();
        assert_eq!(forwards, concave_rings(&backwards, None));

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
        let rings = concave_rings(&members, None);
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
        let rings = concave_rings(&members, None);
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

    /// The degenerate cases keep the behaviour the convex wrap had, on the shape that replaced it.
    #[test]
    fn a_degenerate_shape_is_the_members_themselves() {
        assert_eq!(concave_rings(&[[3, 4]], None), vec![vec![[3, 4]]]);
        assert_eq!(concave_rings(&[[3, 4], [3, 4]], None), vec![vec![[3, 4]]]);
        assert_eq!(concave_rings(&[[0, 0], [1, 1]], None), vec![vec![[0, 0], [1, 1]]]);
        assert_eq!(
            concave_rings(&[[0, 0], [1, 1], [2, 2]], None),
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
}
