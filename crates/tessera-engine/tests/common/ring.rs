//! Ring geometry for the tests that check a served hull: a convex wrap, a containment test and a
//! signed area, all exact in `i128`.
//!
//! **Written from the definitions rather than shared with the engine's.** A hull is checked here for
//! containing every member and for being tighter than the wrap it replaced, and a check that called
//! `tessera_engine::derived`'s own arithmetic would agree with it by construction. Compiled into
//! each test binary that needs it (`#[path = "common/ring.rs"] mod ring;`), which is why the
//! unused-item allowance is here rather than at each use.

#![allow(dead_code)]

/// Andrew's monotone chain, counter-clockwise.
pub fn convex_hull(points: &[[u32; 2]]) -> Vec<[u32; 2]> {
    let mut p = points.to_vec();
    p.sort_unstable();
    p.dedup();
    if p.len() <= 2 {
        return p;
    }
    let turn = |o: [u32; 2], a: [u32; 2], b: [u32; 2]| -> i128 {
        let (ox, oy) = (o[0] as i128, o[1] as i128);
        (a[0] as i128 - ox) * (b[1] as i128 - oy) - (a[1] as i128 - oy) * (b[0] as i128 - ox)
    };
    let mut hull: Vec<[u32; 2]> = Vec::with_capacity(p.len() + 1);
    for &point in &p {
        while hull.len() >= 2 && turn(hull[hull.len() - 2], hull[hull.len() - 1], point) <= 0 {
            hull.pop();
        }
        hull.push(point);
    }
    let lower = hull.len() + 1;
    for &point in p.iter().rev().skip(1) {
        while hull.len() >= lower && turn(hull[hull.len() - 2], hull[hull.len() - 1], point) <= 0 {
            hull.pop();
        }
        hull.push(point);
    }
    hull.pop();
    hull
}

/// `p` lies on the closed segment `a b`.
fn on_segment(a: [u32; 2], b: [u32; 2], p: [u32; 2]) -> bool {
    let (ax, ay, bx, by) = (a[0] as i128, a[1] as i128, b[0] as i128, b[1] as i128);
    let (px, py) = (p[0] as i128, p[1] as i128);
    (bx - ax) * (py - ay) - (by - ay) * (px - ax) == 0
        && p[0] >= a[0].min(b[0])
        && p[0] <= a[0].max(b[0])
        && p[1] >= a[1].min(b[1])
        && p[1] <= a[1].max(b[1])
}

/// `p` is inside the ring or on its boundary — crossing number, with the boundary tested first so a
/// member sitting on an edge counts as contained.
pub fn contains(poly: &[[u32; 2]], p: [u32; 2]) -> bool {
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

/// Twice the signed area of a ring — positive for counter-clockwise.
pub fn double_area(poly: &[[u32; 2]]) -> i128 {
    let n = poly.len();
    (0..n)
        .map(|i| {
            let (a, b) = (poly[i], poly[(i + 1) % n]);
            a[0] as i128 * b[1] as i128 - b[0] as i128 * a[1] as i128
        })
        .sum()
}
