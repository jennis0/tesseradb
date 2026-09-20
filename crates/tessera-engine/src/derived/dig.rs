use super::buckets::Buckets;
use super::geometry::{convex_hull_of_sorted, on_segment, segments_meet, sq_len};
use super::groups::alpha_groups;
use super::reduce::{quantise, QUANTISE_DIVISIONS, REDUCTION_FLOOR};

/// The vertices digging may add on top of the groups' convex wraps, per artifact and not per ring.
///
/// A wire-size guard and not a fidelity control: on the three measured layers the dig ran out of
/// work on its own at 732, 833 and 197 digs, well short of the 2,048 cap, so every served shape
/// follows the members rather than the budget. A shape that does run out stops refining its
/// shortest remaining bridges first, since digging spends the budget longest edge first, which is
/// coarser but never wrong. Rerun with
/// `cargo run --release -p tessera-bench --bin hull_cost -- <bundle>`.
pub(super) const DIG_BUDGET: usize = 2_048;

/// How many times the median edge an edge must exceed before it is treated as bridging a void.
///
/// The median of the convex hull's own edge lengths, squared: scale-free and robust to the single
/// long chord a concavity's bridge is, which a mean would chase instead of ignoring.
const BRIDGE_FACTOR: i128 = 3;

/// A concave (alpha) shape over the visible members: one simple ring per α-group, counter-clockwise
/// from each ring's lowest vertex, on the integer grid.
///
/// A convex wrap swallows the empty space between the arms of an irregular cluster and overlaps
/// its neighbours; a single ring around a membership that is really two separated clouds claims the
/// ground between them, unreachable to α since digging works inward from a boundary. So the members
/// are grouped first ([`alpha_groups`]) and a ring is dug per group, separated before the budget is
/// spent. It discloses nothing the convex wrap did not: the input is the same (`membership ∩
/// M_auth`) and every ring is a subset of the convex hull, so the shape says less, never more.
///
/// The construction: start each group at its convex wrap. Repeatedly take the longest edge `(a, b)`
/// above α across every ring, find the member `c` of that ring's own group closest to the line
/// through `a` and `b` among those on the interior side of `a → b` that project inside the segment
/// ([`Buckets::nearest_inside`]), and replace the edge with `(a, c)` and `(c, b)`, carving the
/// triangle `a c b` out of the ring. A dig is refused, and its edge retired, when there is no such
/// member or the ring would stop being simple, including `c` already sitting on the ring.
///
/// `c` being closest keeps the triangle empty: it lies inside the strip between the perpendiculars
/// at `a` and `b` because `c` does, projection being affine, so a member strictly inside it would
/// be a closer candidate than `c`, contradicting `c`'s minimality. Removing an empty triangle
/// removes no member, so containment holds by induction from the group's convex hull. A point set
/// in convex position is returned unchanged, since every member already lies on the boundary.
/// Digging only moves a boundary inward, so an enclosed void is never reachable and no ring
/// encloses another.
///
/// Exact in `i128` throughout, with ties broken on position, so the shape is a function of the
/// member positions alone, identical on every platform.
///
/// `bounds` is `points`'s own bounding box: a cache and not a parameter, since a box that is not
/// `points`'s own would move the grid and the shape.
pub(super) fn concave_rings(points: &[[u32; 2]], bounds: Option<[u32; 4]>) -> Vec<Vec<[u32; 2]>> {
    dig_rings_within(points, DIG_BUDGET, QUANTISE_DIVISIONS, bounds).0
}

/// [`concave_rings`] at a budget the caller names, with whether the budget bound the dig: a seam
/// for `tessera-bench`'s `hull_cost` and the budget's own test.
#[doc(hidden)]
pub fn dig_rings(points: &[[u32; 2]], budget: usize) -> (Vec<Vec<[u32; 2]>>, bool) {
    dig_rings_within(points, budget, QUANTISE_DIVISIONS, None)
}

/// [`dig_rings`] at a quantising resolution the caller names, `0` meaning none, with `points`'s
/// own bounding box already in hand; see [`concave_rings`].
///
/// Kept as one function: splitting it measured 10 to 20% slower on some inputs.
pub(super) fn dig_rings_within(
    points: &[[u32; 2]],
    budget: usize,
    divisions: u32,
    bounds: Option<[u32; 4]>,
) -> (Vec<Vec<[u32; 2]>>, bool) {
    // Reduced ahead of the sort: 2.42M positions cost 121 ms to wrap and 167 ms to dig unreduced.
    let reduced = quantise(points, divisions, REDUCTION_FLOOR, bounds);
    let points: &[[u32; 2]] = reduced.as_ref().map_or(points, |r| r.as_slice());

    let mut p: Vec<[u32; 2]> = points.to_vec();
    p.sort_unstable();
    p.dedup();
    let convex = convex_hull_of_sorted(&p);
    // One member is that point and two are that segment; nothing to dig into or to group.
    if convex.len() < 3 {
        return (vec![convex], false);
    }

    let alpha_sq = bridge_threshold(&convex);
    let (labels, groups) = alpha_groups(&p, alpha_sq);
    let mut rings: Vec<Ring> = if groups == 1 {
        // The ordinary case: the wrap is already computed and the members already sorted.
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

    // Spent longest bridge first across every ring: not a budget per group.
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
                // `poly[i]` now carries `(a, c)` and the inserted vertex `(c, b)`, both fresh.
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

    // Exhaustion is the budget running out while a bridge was still live.
    let exhausted = inserted == budget && longest_bridge(&rings, alpha_sq).is_some();

    let mut out: Vec<Vec<[u32; 2]>> = rings
        .into_iter()
        .map(|r| r.poly.into_iter().map(|v| v.pos).collect())
        .collect();
    // Ordered by first vertex: groups partition the members, so the order is total.
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

/// One boundary vertex; a refused dig's edge is retired and never retried.
struct Vertex {
    pos: [u32; 2],
    retired: bool,
}

/// α², the squared length an edge must exceed to be dug: `(BRIDGE_FACTOR × median edge)²`.
pub(super) fn bridge_threshold(convex: &[[u32; 2]]) -> i128 {
    let mut lengths: Vec<i128> = (0..convex.len())
        .map(|i| sq_len(convex[i], convex[(i + 1) % convex.len()]))
        .collect();
    lengths.sort_unstable();
    BRIDGE_FACTOR * BRIDGE_FACTOR * lengths[lengths.len() / 2]
}

/// The ring and edge index of the longest live edge above α, or `None`.
///
/// Across every ring, not one at a time: the budget is the artifact's, not a per-ring allowance.
fn longest_bridge(rings: &[Ring], alpha_sq: i128) -> Option<(usize, usize)> {
    let mut best: Option<(i128, usize, usize)> = None;
    for (r, ring) in rings.iter().enumerate() {
        let n = ring.poly.len();
        // A degenerate ring has no interior to dig into.
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
/// Checked rather than argued: an edge from elsewhere on the boundary can cross the dig triangle
/// with both endpoints outside it, which two arms of a crescent digging towards each other would
/// do. The box spanned by `a`, `c` and `b` retires most edges before any arithmetic runs on them;
/// it is a filter and not a rule, so which digs are admissible is unchanged.
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
        // `c` already on the boundary would make the new edges touch it rather than cross it.
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

    /// The oracle [`dig_is_admissible`]'s box filter is held to: every edge, not only the ones
    /// whose box meets the dig triangle's.
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

    /// Twice the signed area of a ring, positive for counter-clockwise. Exact in `i128`.
    fn double_area(poly: &[[u32; 2]]) -> i128 {
        let n = poly.len();
        (0..n)
            .map(|i| {
                let (a, b) = (poly[i], poly[(i + 1) % n]);
                a[0] as i128 * b[1] as i128 - b[0] as i128 * a[1] as i128
            })
            .sum()
    }

    /// Every pair of edges meets only where the ring says it should.
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
                    // Collinear overlap past the shared vertex.
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

    /// The single ring of a membership that is one α-group, asserted rather than assumed.
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

    /// The box is a filter and not a rule: the admissible set is the one the full pass finds.
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
                // Every member is offered as the candidate, not only the one the dig would pick.
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

    /// The wrap's straight edge across the bite is replaced by a boundary that follows it.
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

    /// A point set in convex position has nothing to dig into, whatever α is.
    #[test]
    fn a_point_set_in_convex_position_keeps_its_convex_wrap() {
        // 2,000 distinct lattice positions on a radius-3000 ring.
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

    /// Asserted without exhausting the served budget; the binding is asserted through
    /// [`dig_rings`] at a budget small enough to bind.
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

        // 16 digs is far short of what the flower's seven valleys want.
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

    /// Members lying exactly on an edge about to be dug stay inside the shape.
    #[test]
    fn members_on_a_dug_edge_stay_inside_the_shape() {
        // Two blocks with a gap, and three members strung along the bridging edge across its top.
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
        // The bridging members chain the two blocks into one group, still one ring.
        let hull = one_ring(&members);
        assert!(is_simple(&hull));
        for m in &members {
            assert!(contains(&hull, *m), "{m:?} fell outside {hull:?}");
        }
    }

    /// Two separated clouds get a ring each, every member inside exactly one.
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

    /// The rings are a function of the member positions and not of arrival order.
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

    /// A void with members all round it stays inside the ring: an annulus is drawn as a disk.
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

    /// The budget is the artifact's, not the ring's: several groups share one allowance of digs.
    #[test]
    fn the_budget_is_shared_across_the_rings() {
        // Three flowers, far enough apart to be three groups.
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

    /// The winding and the starting vertex are the wrap's: digging only inserts between vertices.
    #[test]
    fn the_shape_starts_at_the_lowest_vertex_and_winds_counter_clockwise() {
        let hull = one_ring(&[[10, 0], [0, 10], [0, 0], [10, 10]]);
        assert_eq!(hull, vec![[0, 0], [10, 0], [10, 10], [0, 10]]);
        let moon = one_ring(&moon());
        assert_eq!(moon[0], *moon.iter().min().unwrap());
        assert!(double_area(&moon) > 0, "the ring winds clockwise");
    }

    /// A narrow principal's members are a subset of a broad one's, its shape a function of that.
    #[test]
    fn a_subset_of_the_members_gives_a_shape_over_that_subset_alone() {
        let broad = moon();
        // A deterministic thinning: the narrow principal sees one member in three.
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
        // Recomputed over the same set it is the same shape: no request input, no float.
        assert_eq!(narrow_hull, one_ring(&narrow));
    }
}
