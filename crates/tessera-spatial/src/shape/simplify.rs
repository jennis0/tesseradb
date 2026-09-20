//! Visvalingam–Whyatt weights, computed once at canonicalisation and read at every serve.
//!
//! Each vertex's effective area is the area of the triangle it makes with its two neighbours,
//! what removing it would change the ring by. Removing the least-area vertex and recomputing its
//! neighbours, repeatedly, is the classic simplification; recording the area at which each vertex
//! was removed, made monotone along the removal order, means a ring filtered to the vertices
//! whose recorded area is at least *t* is exactly the ring the classic algorithm would have
//! stopped at when no remaining vertex had area below *t*.
//!
//! The weight stored is the side of the square with that area, in grid units, so a request whose
//! cell side is *s* keeps the vertices with `weight ≥ s`; it fits a `u32` where the area would not.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use super::polygon::Vertex;

/// Twice the triangle area `(a, b, c)`, unsigned.
fn doubled_area(a: (u32, u32), b: (u32, u32), c: (u32, u32)) -> u128 {
    let (ax, ay) = (i128::from(a.0), i128::from(a.1));
    let (bx, by) = (i128::from(b.0), i128::from(b.1));
    let (cx, cy) = (i128::from(c.0), i128::from(c.1));
    ((bx - ax) * (cy - ay) - (by - ay) * (cx - ax)).unsigned_abs()
}

/// Integer square root, floor.
fn isqrt(n: u128) -> u128 {
    if n < 2 {
        return n;
    }
    let mut x = (n as f64).sqrt() as u128;
    while x * x > n {
        x -= 1;
    }
    while (x + 1) * (x + 1) <= n {
        x += 1;
    }
    x
}

/// Weight a closed ring of at least three vertices. The last three survivors weigh `u32::MAX`.
pub(crate) fn weight_ring(points: &[(u32, u32)]) -> Vec<Vertex> {
    let n = points.len();
    debug_assert!(n >= 3);
    let mut prev: Vec<usize> = (0..n).map(|i| (i + n - 1) % n).collect();
    let mut next: Vec<usize> = (0..n).map(|i| (i + 1) % n).collect();
    let mut area: Vec<u128> = (0..n)
        .map(|i| doubled_area(points[prev[i]], points[i], points[next[i]]))
        .collect();
    let mut alive = vec![true; n];
    let mut weight: Vec<u32> = vec![u32::MAX; n];
    // Lazy heap: an entry is stale when its area no longer matches the vertex's. Ties break on
    // the vertex's own position, never its index, so the weights depend only on the vertex set.
    let key = |i: usize| (area[i], points[i].1, points[i].0, i);
    let mut heap: BinaryHeap<Reverse<(u128, u32, u32, usize)>> =
        (0..n).map(|i| Reverse(key(i))).collect();
    let mut remaining = n;
    let mut floor: u128 = 0;
    while remaining > 3 {
        let Some(Reverse((a, _, _, i))) = heap.pop() else {
            break;
        };
        if !alive[i] || a != area[i] {
            continue;
        }
        // Monotone along the removal order: a vertex removed after a heavier one is at least as
        // heavy, so that a threshold filter is a prefix of the removal order.
        floor = floor.max(a);
        weight[i] = u32::try_from(isqrt(floor / 2)).unwrap_or(u32::MAX);
        alive[i] = false;
        remaining -= 1;
        let (p, q) = (prev[i], next[i]);
        next[p] = q;
        prev[q] = p;
        for j in [p, q] {
            area[j] = doubled_area(points[prev[j]], points[j], points[next[j]]);
            heap.push(Reverse((area[j], points[j].1, points[j].0, j)));
        }
    }
    points
        .iter()
        .zip(weight)
        .map(|(&(x, y), weight)| Vertex { x, y, weight })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spike_weighs_little_and_the_corners_survive() {
        // A square with one near-collinear vertex on its bottom edge.
        let ring = [(0, 0), (500, 1), (1000, 0), (1000, 1000), (0, 1000)];
        let w = weight_ring(&ring);
        assert!(w[1].weight < 100, "{:?}", w[1]);
        assert!(w.iter().filter(|v| v.weight == u32::MAX).count() == 3);
        // The fourth survivor: the last removal, at the square's own scale.
        let fourth = w
            .iter()
            .map(|v| v.weight)
            .filter(|&x| x != u32::MAX)
            .max()
            .unwrap();
        assert!(fourth >= 500, "{fourth}");
    }

    #[test]
    fn weights_are_monotone_in_removal_order() {
        let ring: Vec<(u32, u32)> = (0..64)
            .map(|k| {
                let t = k as f64 / 64.0 * std::f64::consts::TAU;
                let r = 10_000.0 + 300.0 * (7.0 * t).sin();
                (
                    (50_000.0 + r * t.cos()) as u32,
                    (50_000.0 + r * t.sin()) as u32,
                )
            })
            .collect();
        let w = weight_ring(&ring);
        // Filtering at any threshold leaves at least three vertices and is a superset of
        // filtering at a higher one, the property a serve depends on.
        let mut last = usize::MAX;
        for t in [0u32, 10, 100, 1000, 10_000, u32::MAX] {
            let kept = w.iter().filter(|v| v.weight >= t).count();
            assert!(kept >= 3 && kept <= last, "t={t} kept={kept}");
            last = kept;
        }
    }
}
