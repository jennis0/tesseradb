use super::geometry::on_segment;

/// A deterministic sample of `count` positions from `region`, over the box `[-span, span]²`,
/// offset into the unsigned grid. Not a lattice, so a collinear third member stays a fluke.
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

/// A moon: radius 1000 minus radius 900 about `(1200, 0)`, the concavity a convex wrap swallows.
pub(super) fn moon() -> Vec<[u32; 2]> {
    sample(4000, 1000, |x, y| {
        x * x + y * y <= 1000 * 1000 && (x - 1200) * (x - 1200) + y * y > 900 * 900
    })
}

/// Seven lobes on a common centre: seven voids for the budget to spend on, sampled finely enough
/// that no single dig finishes a valley.
pub(super) fn flower() -> Vec<[u32; 2]> {
    sample(20_000, 1000, |x, y| {
        let (fx, fy) = (x as f64, y as f64);
        let r = (fx * fx + fy * fy).sqrt();
        r <= 400.0 + 550.0 * (7.0 * fy.atan2(fx)).cos()
    })
}

/// `p` is inside the ring or on its boundary; crossing number, exact in `i128`, boundary tested
/// first so an edge member counts as contained.
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

/// Two radius-400 disks 3,000 apart: a gap far wider than any α their wrap can produce.
pub(super) fn two_clouds() -> Vec<[u32; 2]> {
    sample(3000, 2000, |x, y| {
        (x + 1500) * (x + 1500) + y * y <= 400 * 400
            || (x - 1500) * (x - 1500) + y * y <= 400 * 400
    })
}

/// The distance from `p` to the line through `a` and `b`, clamped to the segment.
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

/// The furthest a member can be from its own shape: the diagonal of one quantising cell, a bound
/// and not a tolerance since every member shares its cell with a representative the dig holds.
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
