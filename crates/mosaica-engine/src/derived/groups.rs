/// How many cells of the grouping grid span α. At 2 every pair within α is joined and pairs up to
/// about 2.12α may be; the groups match exact single linkage on 192 of 197 measured artifacts
/// (190 at one cell per α, 192 at four).
const GROUP_CELLS_PER_ALPHA: u64 = 2;

/// The α-groups of the visible members: one label per member, and how many groups there are.
///
/// The members sit in a square grid, cell side α/[`GROUP_CELLS_PER_ALPHA`], joined when their
/// cells are within [`GROUP_CELLS_PER_ALPHA`] cells of each other on both axes: a displacement of
/// at most α moves a cell index by at most that many cells, so every pair within α lands in one
/// group; a wider join only draws a wider ring, never a gap the members do not have. The grid never
/// holds more cells than there are members, doubling the cell side until they fit. `O(members)`
/// against a Delaunay route that measured 1.4 s to this pass's 0.16 s on the largest artifact.
pub(super) fn alpha_groups(p: &[[u32; 2]], alpha_sq: i128) -> (Vec<u32>, usize) {
    let [x0, y0, x1, y1] = super::geometry::bounds(p);
    let (wx, wy) = ((x1 - x0) as u64 + 1, (y1 - y0) as u64 + 1);
    // α as a length. The root is a float, rounded up to a whole grid unit so `2 · side ≥ α` holds
    // with an integer's margin a last-bit error cannot cross.
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

    // Union-find over the cells, not the members: the grid has at most one cell per member, so the
    // join costs a bounded sweep over cells rather than a neighbourhood query per member.
    let mut parent: Vec<u32> = (0..total as u32).collect();
    let r = GROUP_CELLS_PER_ALPHA as i64;
    for cy in 0..ny as i64 {
        for cx in 0..nx as i64 {
            let k = (cy * nx as i64 + cx) as usize;
            if !occupied[k] {
                continue;
            }
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

    // Labels are minted in cell order, a function of the positions rather than of arrival order.
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

    /// The grouping never separates members single-linkage at α would join, checked exhaustively
    /// on a cloud small enough to compare every pair.
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
