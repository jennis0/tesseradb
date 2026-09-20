use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::ops::Range;

use super::geometry::{bounds, dot};
use super::reduce::interleave_cell;

/// A bounding box on the integer grid, over a bucket's members or over a subtree's buckets.
#[derive(Clone, Copy)]
struct Box2 {
    min: [u32; 2],
    max: [u32; 2],
}

impl Box2 {
    fn of(q: [u32; 2]) -> Box2 {
        Box2 { min: q, max: q }
    }

    fn include(&mut self, q: [u32; 2]) {
        self.min = [self.min[0].min(q[0]), self.min[1].min(q[1])];
        self.max = [self.max[0].max(q[0]), self.max[1].max(q[1])];
    }

    fn union(&self, other: &Box2) -> Box2 {
        Box2 {
            min: [
                self.min[0].min(other.min[0]),
                self.min[1].min(other.min[1]),
            ],
            max: [
                self.max[0].max(other.max[0]),
                self.max[1].max(other.max[1]),
            ],
        }
    }
}

/// The members bucketed into a square grid, each bucket carrying the bounding box of what it
/// holds, with a tree of boxes over the buckets so a candidate search descends to the few buckets
/// that can hold the answer. The box is exact in `i128` so pruning never discards the answer.
/// Scoring every bucket costs `O(C log C)` per dig, measured at 455 of the 775 ms a layer's shape
/// construction cost; the tree turns that into a bottom-up build pass plus a descent.
pub(super) struct Buckets {
    /// Every member, reordered so each bucket's members are contiguous.
    points: Vec<[u32; 2]>,
    /// The occupied buckets in Morton order, the order [`build_tree`] halves into compact regions.
    cells: Vec<Cell>,
    /// The tree over `cells`, built bottom-up so the root is the last node.
    tree: Vec<Node>,
    /// The descent's frontier, kept across digs so a search costs no allocation.
    heap: BinaryHeap<Reverse<(i128, u32)>>,
}

struct Cell {
    extent: Box2,
    start: usize,
    len: usize,
}

/// A node of the bucket tree: a box, and either a range in [`Buckets::cells`] or two children.
struct Node {
    extent: Box2,
    kind: Kind,
}

/// Which of the two a [`Node`] is.
enum Kind {
    Leaf { cells: Range<u32> },
    Inner { left: u32, right: u32 },
}

impl Buckets {
    pub(super) fn build(p: &[[u32; 2]]) -> Buckets {
        let axis = bucket_axis(p.len()) as u64;
        let [x0, y0, x1, y1] = bounds(p);
        // Widths as `u64` and inclusive, so a cloud spanning the whole 2^32 grid does not wrap.
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

        // Morton order, not row-major: a row-major range is a strip spanning the grid in `x`,
        // which a box cannot prune against.
        let mut cells: Vec<(u64, Cell)> = Vec::new();
        for k in 0..total {
            let (start, end) = (offsets[k], offsets[k + 1]);
            if start == end {
                continue;
            }
            let mut extent = Box2::of(points[start]);
            for q in &points[start..end] {
                extent.include(*q);
            }
            let (cx, cy) = (k as u64 % axis, k as u64 / axis);
            cells.push((
                interleave_cell(cx, cy),
                Cell {
                    extent,
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
    /// `a → b` (the line included) and projecting strictly inside the segment, or `None`.
    ///
    /// A member on the segment is the nearest there is and is returned: digging to it carves a
    /// zero-area triangle, gaining that member as a vertex rather than cutting it off the shape.
    ///
    /// Both restrictions carry the emptiness argument: the dig triangle `a c b` lies inside the
    /// strip between the perpendiculars at `a` and `b` because `c` does, projection being affine,
    /// so a closer candidate would contradict `c`'s minimality; without the projection restriction
    /// the nearest member to the line is routinely one past an endpoint, carving a sliver along the
    /// edge instead of into the bridged void. Exhaustive over the members that qualify; the bucket
    /// bound only skips buckets that provably cannot hold a better candidate.
    pub(super) fn nearest_inside(&mut self, a: [u32; 2], b: [u32; 2]) -> Option<[u32; 2]> {
        if self.tree.is_empty() {
            return None;
        }
        let (dx, dy) = (b[0] as i128 - a[0] as i128, b[1] as i128 - a[1] as i128);
        let cross =
            |q: [u32; 2]| dx * (q[1] as i128 - a[1] as i128) - dy * (q[0] as i128 - a[0] as i128);
        // Affine, so its minimum over a box sits at the corner its two coefficients select.
        let bound = |e: &Box2| {
            cross([
                if dy <= 0 { e.min[0] } else { e.max[0] },
                if dx >= 0 { e.min[1] } else { e.max[1] },
            ])
        };
        // The slab test is what makes the descent cheap: each condition is a linear form maximised
        // over a box at a corner, so a bucket far along the edge's own line is pruned rather than
        // opened on every dig. Measured at 428 ms of the layer's dig against 71 ms with the test.
        let feasible = |e: &Box2| {
            let corner = |ux: i128, uy: i128| {
                [
                    if ux >= 0 { e.max[0] } else { e.min[0] },
                    if uy >= 0 { e.max[1] } else { e.min[1] },
                ]
            };
            cross(corner(-dy, dx)) >= 0
                && dot(a, b, corner(dx, dy)) > 0
                && dot(b, a, corner(-dx, -dy)) > 0
        };

        // Best-first order is an efficiency property only: `best` is the minimum of a total order
        // on `(distance, position)`, the same answer an exhaustive scan finds in any order.
        let mut best: Option<(i128, [u32; 2])> = None;
        let root = self.tree.len() as u32 - 1;
        self.heap.clear();
        let extent = self.tree[root as usize].extent;
        if feasible(&extent) {
            self.heap.push(Reverse((bound(&extent), root)));
        }
        while let Some(Reverse((node_bound, index))) = self.heap.pop() {
            if let Some((found, _)) = best {
                if node_bound > found {
                    break;
                }
            }
            match &self.tree[index as usize].kind {
                Kind::Inner { left, right } => {
                    for child in [*left, *right] {
                        let n = &self.tree[child as usize];
                        if !feasible(&n.extent) {
                            continue;
                        }
                        self.heap.push(Reverse((bound(&n.extent), child)));
                    }
                }
                Kind::Leaf { cells } => {
                    for cell in &self.cells[cells.start as usize..cells.end as usize] {
                        if !feasible(&cell.extent) {
                            continue;
                        }
                        if let Some((found, _)) = best {
                            if bound(&cell.extent) > found {
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
                                // Ties break on position; the answer does not depend on walk order.
                                Some((bd, bq)) => d < bd || (d == bd && q < bq),
                            };
                            if better {
                                best = Some((d, q));
                            }
                        }
                    }
                }
            }
        }
        best.map(|(_, q)| q)
    }
}

/// How many buckets a leaf of [`Buckets::tree`] holds: four.
const BUCKET_TREE_LEAF: usize = 4;

/// Builds [`Buckets::tree`] over `cells[lo..hi]` bottom-up, returning the node's own index. The
/// split is the range's midpoint, since the buckets are already in Morton order.
fn build_tree(cells: &[Cell], lo: usize, hi: usize, out: &mut Vec<Node>) -> u32 {
    if hi - lo <= BUCKET_TREE_LEAF {
        let mut extent = cells[lo].extent;
        for cell in &cells[lo..hi] {
            extent = extent.union(&cell.extent);
        }
        out.push(Node {
            extent,
            kind: Kind::Leaf {
                cells: lo as u32..hi as u32,
            },
        });
        return out.len() as u32 - 1;
    }
    let mid = lo + (hi - lo) / 2;
    let a = build_tree(cells, lo, mid, out);
    let b = build_tree(cells, mid, hi, out);
    let extent = out[a as usize].extent.union(&out[b as usize].extent);
    out.push(Node {
        extent,
        kind: Kind::Inner { left: a, right: b },
    });
    out.len() as u32 - 1
}

/// Buckets per axis: about 64 members to a bucket, never more than 64 divisions.
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

    /// The candidate a plain scan finds: the oracle the indexed search is held to.
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

    /// The bucket tree prunes and never chooses, so the descent returns what an exhaustive scan
    /// returns. Queries every pair of members at a stride, across the cloud and its boundary.
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
