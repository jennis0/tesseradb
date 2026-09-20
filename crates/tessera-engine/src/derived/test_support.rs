use super::dig::concave_rings;
use super::geometry::{on_segment, segments_meet};

/// A deterministic sample of `count` positions from `region`, drawn over the box
/// `[-span, span]²` and offset into the unsigned grid.
///
/// **Not a lattice.** Real positions are a quantisation of a continuous embedding, so a third
/// member exactly on the line through two others is a fluke; a lattice makes it the common case
/// and turns every test into a test of the collinear path. That path has its own test below.
pub(super) fn sample(count: usize, span: i64, region: impl Fn(i64, i64) -> bool) -> Vec<[u32; 2]> {
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
pub(super) fn moon() -> Vec<[u32; 2]> {
    sample(4000, 1000, |x, y| {
        x * x + y * y <= 1000 * 1000 && (x - 1200) * (x - 1200) + y * y > 900 * 900
    })
}

/// The middle of the moon's bite, in grid coordinates.
pub(super) const IN_THE_BITE: [u32; 2] = [2_000_600, 2_000_000];

/// Seven lobes on a common centre, so the wrap bridges seven separate voids and the budget has
/// somewhere to be spent. Sampled finely enough that no single dig finishes a valley.
pub(super) fn flower() -> Vec<[u32; 2]> {
    sample(20_000, 1000, |x, y| {
        let (fx, fy) = (x as f64, y as f64);
        let r = (fx * fx + fy * fy).sqrt();
        r <= 400.0 + 550.0 * (7.0 * fy.atan2(fx)).cos()
    })
}

/// Twice the signed area of a ring — positive for counter-clockwise. Exact in `i128`.
pub(super) fn double_area(poly: &[[u32; 2]]) -> i128 {
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
pub(super) fn contains(poly: &[[u32; 2]], p: [u32; 2]) -> bool {
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
pub(super) fn is_simple(poly: &[[u32; 2]]) -> bool {
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
pub(super) fn one_ring(members: &[[u32; 2]]) -> Vec<[u32; 2]> {
    let rings = concave_rings(members, None);
    assert_eq!(rings.len(), 1, "expected one group, got {}", rings.len());
    rings.into_iter().next().unwrap()
}

/// Two disks of radius 400 whose centres are 3,000 apart — a membership that is honestly two
/// clouds, with a gap far wider than any α its own wrap can produce.
pub(super) fn two_clouds() -> Vec<[u32; 2]> {
    sample(3000, 2000, |x, y| {
        (x + 1500) * (x + 1500) + y * y <= 400 * 400
            || (x - 1500) * (x - 1500) + y * y <= 400 * 400
    })
}

/// Twice the area of the triangle `a b p`, over `|ab|` — the distance from `p` to the line
/// through `a` and `b`, clamped to the segment.
pub(super) fn point_to_segment(a: [u32; 2], b: [u32; 2], p: [u32; 2]) -> f64 {
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
pub(super) fn escape(rings: &[Vec<[u32; 2]>], m: [u32; 2]) -> f64 {
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
pub(super) fn cell_bound(members: &[[u32; 2]], divisions: u32) -> f64 {
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
