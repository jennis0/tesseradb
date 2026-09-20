use std::cmp::Reverse;
use std::collections::BinaryHeap;

use super::geometry::dot;
use super::reduce::interleave_cell;

/// The members bucketed into a square grid, each bucket carrying the bounding box of what it holds,
/// **with a tree of boxes over the buckets themselves** so that a candidate search descends to the
/// few buckets that can hold the answer instead of scoring every one of them.
///
/// **A bounding box per bucket, not the cell's own geometry**, because the box is tighter and needs
/// no cell-boundary arithmetic to be exact: the minimum of a cross product over a box is attained at
/// a corner, so one corner per bucket bounds every member in it and the whole bucket is skipped when
/// that bound cannot beat the best candidate found so far. The bound is exact in `i128`, so pruning
/// never discards the answer.
///
/// **The tree is what made the dig cheap, and the flat list is what made it expensive.** Scoring
/// every bucket and sorting the scores is `O(C log C)` *per dig*, and a large artifact's grid is
/// thousands of buckets against a search that then reads two or three of them — measured at **455
/// of the 775 ms** the whole layer's shape construction cost, against 57 ms for the admissibility
/// check the same profile was expected to find at the top (`artifact-shapes.md` §7.4). The tree
/// costs one bottom-up pass at build and turns the per-dig term into a descent.
pub(super) struct Buckets {
    /// Every member, reordered so each bucket's members are contiguous.
    points: Vec<[u32; 2]>,
    /// The occupied buckets in Morton order of their cell coordinates, which is the order
    /// [`build_tree`] halves — a range of it is a compact region rather than a strip of rows, so
    /// the boxes above it are tight enough to prune against.
    cells: Vec<Cell>,
    /// The tree over `cells`, built bottom-up so that **the root is the last node**; an empty
    /// bucket list has no nodes at all.
    tree: Vec<Node>,
    /// The descent's frontier, kept across digs so a search costs no allocation.
    heap: BinaryHeap<Reverse<(i128, u32)>>,
}

struct Cell {
    min: [u32; 2],
    max: [u32; 2],
    start: usize,
    len: usize,
}

/// A node of the bucket tree: the box over the buckets it covers, and either their range in
/// [`Buckets::cells`] (a leaf) or its two children.
struct Node {
    min: [u32; 2],
    max: [u32; 2],
    /// The child node indices, or `(range start, range end)` when `leaf`.
    a: u32,
    b: u32,
    leaf: bool,
}

impl Buckets {
    pub(super) fn build(p: &[[u32; 2]]) -> Buckets {
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

        // **In Morton order of the cell coordinates, not in row-major order**, because it is the
        // order [`build_tree`] halves: a range of a row-major list is a strip of rows spanning the
        // whole grid in `x`, and a box over it prunes against nothing.
        let mut cells: Vec<(u64, Cell)> = Vec::new();
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
            let (cx, cy) = (k as u64 % axis, k as u64 / axis);
            cells.push((
                interleave_cell(cx, cy),
                Cell {
                    min,
                    max,
                    start,
                    len: end - start,
                },
            ));
        }
        cells.sort_unstable_by_key(|(key, _)| *key);
        let cells: Vec<Cell> = cells.into_iter().map(|(_, cell)| cell).collect();

        let mut tree = Vec::with_capacity(2 * cells.len());
        if !cells.is_empty() {
            build_tree(&cells, 0, cells.len(), &mut tree);
        }
        Buckets {
            points,
            cells,
            tree,
            heap: BinaryHeap::new(),
        }
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
    pub(super) fn nearest_inside(&mut self, a: [u32; 2], b: [u32; 2]) -> Option<[u32; 2]> {
        if self.tree.is_empty() {
            return None;
        }
        let (dx, dy) = (b[0] as i128 - a[0] as i128, b[1] as i128 - a[1] as i128);
        let cross =
            |q: [u32; 2]| dx * (q[1] as i128 - a[1] as i128) - dy * (q[0] as i128 - a[0] as i128);
        // The cross product is affine in the position, so its minimum over a box sits at whichever
        // corner the two coefficients — `dx` on y, `−dy` on x — select. That is the same bound the
        // flat list took per bucket; what the tree adds is that one box retires a whole subtree.
        let bound = |min: [u32; 2], max: [u32; 2]| {
            cross([
                if dy <= 0 { min[0] } else { max[0] },
                if dx >= 0 { min[1] } else { max[1] },
            ])
        };
        // **A candidate has to be inside the slab as well as near the line, and the slab is what
        // does the pruning.** The three conditions a member must meet are each a linear form in its
        // position, so each is maximised over a box at the corner its two coefficients pick: a box
        // whose best corner already fails one of them holds no candidate at all, whatever its
        // distance to the line. Without this a bucket lying along the edge's own line — far past
        // either endpoint, and so holding nothing that projects inside the segment — scores a bound
        // near zero and is opened on every dig; with it the descent reaches the buckets over the
        // void the edge bridges and stops. **Measured at 428 ms of the layer's dig against 71 ms**
        // (`artifact-shapes.md` §7.4).
        let feasible = |min: [u32; 2], max: [u32; 2]| {
            let corner = |ux: i128, uy: i128| {
                [
                    if ux >= 0 { max[0] } else { min[0] },
                    if uy >= 0 { max[1] } else { min[1] },
                ]
            };
            cross(corner(-dy, dx)) >= 0
                && dot(a, b, corner(dx, dy)) > 0
                && dot(b, a, corner(-dx, -dy)) > 0
        };

        // **Best-first, so the first leaf reached holds a near answer and the descent stops as soon
        // as the smallest remaining bound cannot beat it.** The order the buckets are visited in is
        // an efficiency property and nothing more: a bucket is skipped only where its box provably
        // holds no better candidate, and `best` is the minimum of a total order on `(distance,
        // position)`, so the answer is the one an exhaustive scan finds, whatever the order.
        let mut best: Option<(i128, [u32; 2])> = None;
        let root = self.tree.len() as u32 - 1;
        self.heap.clear();
        let (min, max) = (self.tree[root as usize].min, self.tree[root as usize].max);
        if feasible(min, max) {
            self.heap.push(Reverse((bound(min, max), root)));
        }
        while let Some(Reverse((node_bound, index))) = self.heap.pop() {
            if let Some((found, _)) = best {
                if node_bound > found {
                    break;
                }
            }
            let node = &self.tree[index as usize];
            let (first, second, leaf) = (node.a, node.b, node.leaf);
            if !leaf {
                for child in [first, second] {
                    let n = &self.tree[child as usize];
                    if !feasible(n.min, n.max) {
                        continue;
                    }
                    self.heap.push(Reverse((bound(n.min, n.max), child)));
                }
                continue;
            }
            for cell in &self.cells[first as usize..second as usize] {
                if !feasible(cell.min, cell.max) {
                    continue;
                }
                if let Some((found, _)) = best {
                    if bound(cell.min, cell.max) > found {
                        continue;
                    }
                }
                for &q in &self.points[cell.start..cell.start + cell.len] {
                    let d = cross(q);
                    if d < 0 || dot(a, b, q) <= 0 || dot(b, a, q) <= 0 {
                        continue;
                    }
                    let better = match best {
                        None => true,
                        // Ties are broken on the position itself, so the answer does not depend on
                        // the order the buckets happen to be walked in.
                        Some((bd, bq)) => d < bd || (d == bd && q < bq),
                    };
                    if better {
                        best = Some((d, q));
                    }
                }
            }
        }
        best.map(|(_, q)| q)
    }
}

/// How many buckets a leaf of [`Buckets::tree`] holds. Four, so the tree is a fifth the size of the
/// bucket list and its last two levels — where a box is barely tighter than the buckets under it —
/// are a straight scan rather than a heap operation each.
const BUCKET_TREE_LEAF: usize = 4;

/// Builds [`Buckets::tree`] over `cells[lo..hi]` bottom-up, returning the node's own index.
///
/// The split is the range's midpoint rather than a median of coordinates, because the buckets are
/// already in Morton order: halving that order halves the region, and a split chosen on coordinates
/// would cost a pass per level to buy a box that is no tighter.
fn build_tree(cells: &[Cell], lo: usize, hi: usize, out: &mut Vec<Node>) -> u32 {
    if hi - lo <= BUCKET_TREE_LEAF {
        let (mut min, mut max) = (cells[lo].min, cells[lo].max);
        for cell in &cells[lo..hi] {
            min = [min[0].min(cell.min[0]), min[1].min(cell.min[1])];
            max = [max[0].max(cell.max[0]), max[1].max(cell.max[1])];
        }
        out.push(Node {
            min,
            max,
            a: lo as u32,
            b: hi as u32,
            leaf: true,
        });
        return out.len() as u32 - 1;
    }
    let mid = lo + (hi - lo) / 2;
    let a = build_tree(cells, lo, mid, out);
    let b = build_tree(cells, mid, hi, out);
    let (na, nb) = (&out[a as usize], &out[b as usize]);
    let min = [na.min[0].min(nb.min[0]), na.min[1].min(nb.min[1])];
    let max = [na.max[0].max(nb.max[0]), na.max[1].max(nb.max[1])];
    out.push(Node {
        min,
        max,
        a,
        b,
        leaf: false,
    });
    out.len() as u32 - 1
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::derived::test_support::*;

    /// The candidate a plain scan over every member finds, applying the three conditions
    /// [`Buckets::nearest_inside`] applies and nothing else — the oracle the indexed search is held
    /// to.
    fn nearest_inside_by_scan(members: &[[u32; 2]], a: [u32; 2], b: [u32; 2]) -> Option<[u32; 2]> {
        let (dx, dy) = (b[0] as i128 - a[0] as i128, b[1] as i128 - a[1] as i128);
        let mut best: Option<(i128, [u32; 2])> = None;
        for &q in members {
            let d = dx * (q[1] as i128 - a[1] as i128) - dy * (q[0] as i128 - a[0] as i128);
            if d < 0 || dot(a, b, q) <= 0 || dot(b, a, q) <= 0 {
                continue;
            }
            if best.is_none_or(|(bd, bq)| d < bd || (d == bd && q < bq)) {
                best = Some((d, q));
            }
        }
        best.map(|(_, q)| q)
    }

    /// **The bucket tree prunes and never chooses.** Every box it skips is one whose best corner
    /// fails a condition the candidate must meet, so the descent returns what an exhaustive scan
    /// returns — which is the property the dig's containment argument rests on, minimality being
    /// what makes the dug triangle empty.
    ///
    /// The queries are every pair of members at a stride, so they cover edges across the cloud, along
    /// its boundary and inside it — including the degenerate ones a ring never presents but the
    /// search must still answer the same way twice.
    #[test]
    fn the_indexed_candidate_search_answers_what_a_scan_answers() {
        for cloud in [moon(), flower(), two_clouds(), sample(4_000, 900, |_, _| true)] {
            let mut members = cloud.clone();
            members.sort_unstable();
            members.dedup();
            let mut grid = Buckets::build(&members);
            let stride = (members.len() / 40).max(1);
            let mut asked = 0usize;
            for i in (0..members.len()).step_by(stride) {
                for j in (0..members.len()).step_by(stride) {
                    if i == j {
                        continue;
                    }
                    let (a, b) = (members[i], members[j]);
                    assert_eq!(
                        grid.nearest_inside(a, b),
                        nearest_inside_by_scan(&members, a, b),
                        "the tree and the scan disagree on the edge {a:?} → {b:?}"
                    );
                    asked += 1;
                }
            }
            assert!(asked > 1_000, "the sweep asked {asked} questions");
        }
    }
}
