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
pub(super) fn alpha_groups(p: &[[u32; 2]], alpha_sq: i128) -> (Vec<u32>, usize) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::derived::dig::bridge_threshold;
    use crate::derived::geometry::{convex_hull_of_sorted, sq_len};
    use crate::derived::test_support::two_clouds;

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
}
