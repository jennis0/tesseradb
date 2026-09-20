/// The members' own bounding box, `[x_min, y_min, x_max, y_max]` — the fold every grid in this
/// module is anchored and scaled by. Empty input gives an inverted box, which no caller has.
pub(super) fn bounds(points: &[[u32; 2]]) -> [u32; 4] {
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0u32, 0u32);
    for q in points {
        x0 = x0.min(q[0]);
        y0 = y0.min(q[1]);
        x1 = x1.max(q[0]);
        y1 = y1.max(q[1]);
    }
    [x0, y0, x1, y1]
}

pub(super) fn sq_len(a: [u32; 2], b: [u32; 2]) -> i128 {
    let (dx, dy) = (b[0] as i128 - a[0] as i128, b[1] as i128 - a[1] as i128);
    dx * dx + dy * dy
}

/// `(b − a) · (c − a)`: positive exactly when `c` projects onto the ray from `a` through `b`.
pub(super) fn dot(a: [u32; 2], b: [u32; 2], c: [u32; 2]) -> i128 {
    (b[0] as i128 - a[0] as i128) * (c[0] as i128 - a[0] as i128)
        + (b[1] as i128 - a[1] as i128) * (c[1] as i128 - a[1] as i128)
}

fn orient(o: [u32; 2], a: [u32; 2], b: [u32; 2]) -> i128 {
    let (ox, oy) = (o[0] as i128, o[1] as i128);
    (a[0] as i128 - ox) * (b[1] as i128 - oy) - (a[1] as i128 - oy) * (b[0] as i128 - ox)
}

/// `r` lies on the closed segment `p q`.
pub(super) fn on_segment(p: [u32; 2], q: [u32; 2], r: [u32; 2]) -> bool {
    orient(p, q, r) == 0
        && r[0] >= p[0].min(q[0])
        && r[0] <= p[0].max(q[0])
        && r[1] >= p[1].min(q[1])
        && r[1] <= p[1].max(q[1])
}

/// Whether two closed segments share any point at all — touching counts, because a boundary that
/// touches itself is not a shape a client can fill.
pub(super) fn segments_meet(p1: [u32; 2], p2: [u32; 2], p3: [u32; 2], p4: [u32; 2]) -> bool {
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
/// deduplicated — [`concave_rings`](super::dig::concave_rings) does that once and then buckets the same vector, rather than
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
pub(super) fn convex_hull_of_sorted(p: &[[u32; 2]]) -> Vec<[u32; 2]> {
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
/// [`concave_rings`](super::dig::concave_rings), which has already sorted.
#[cfg(test)]
pub(super) fn convex_hull(points: &[[u32; 2]]) -> Vec<[u32; 2]> {
    let mut p: Vec<[u32; 2]> = points.to_vec();
    p.sort_unstable();
    p.dedup();
    convex_hull_of_sorted(&p)
}

#[cfg(test)]
mod tests {
    use super::*;

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
