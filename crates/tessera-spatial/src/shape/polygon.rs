//! The polygon: rings on the grid, the even-odd test, and the per-tile context the descent
//! refines.
//!
//! # The tie rule
//!
//! Every test here is integer arithmetic over grid positions, and a grid has ties: a vertex on
//! the ray, an edge through a cell's corner, a point on an edge. The even-odd rule is made total
//! by one **symbolic perturbation**: every polygon vertex is taken to sit at `(x − ε, y − ε²)` for
//! an infinitesimal ε. Then no vertex lies on a grid line, no edge passes through a grid position,
//! and every crossing count is a count over a genuine set, so the parity carried down the descent
//! is a set partition rather than a topological argument. Concretely:
//!
//! - an edge straddles the horizontal line `y = Y` iff `(ay > Y) != (by > Y)` — a vertex *at* `Y`
//!   is below it;
//! - its crossing with that line is at `x_int − ε`, so a crossing exactly at an integer `X`
//!   counts as *left of* `X`;
//! - an edge straddles the vertical line `x = X` iff `(ax > X) != (bx > X)`;
//! - its crossing with that line is at `y_int + ε·(dy/dx) − ε²`, so a crossing exactly at an
//!   integer `Y` is *above* `Y` iff `dy·dx > 0`.
//!
//! The one rule that is not the perturbation's is the last one applied: **a position exactly on
//! an edge is inside** (`polygon-membership.md` §4.1), tested first and exactly.

use super::decompose::{Class, Rect, Region};
use super::{Bbox, GridPoint};

/// A vertex in grid units, with its simplification weight (`simplify.rs`): the side of the square
/// whose area the vertex's removal would change the ring by, so that a request at a depth whose
/// cell side is `s` drops every vertex with `weight < s`. `u32::MAX` on a vertex never dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Vertex {
    pub x: u32,
    pub y: u32,
    pub weight: u32,
}

/// A closed ring of at least three distinct, non-collinear vertices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ring {
    pub vertices: Vec<Vertex>,
}

/// One part of a multipolygon: an outer ring first, then its holes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    pub rings: Vec<Ring>,
}

/// A canonical polygon (`polygon-membership.md` §4.4): parts ordered by their lowest vertex,
/// each ring rotated to start at its own lowest, outer rings positive and holes negative in
/// shoelace sign.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Polygon {
    pub parts: Vec<Part>,
}

impl Polygon {
    pub fn vertex_count(&self) -> u64 {
        self.parts
            .iter()
            .flat_map(|p| p.rings.iter())
            .map(|r| r.vertices.len() as u64)
            .sum()
    }

    pub fn bounds(&self) -> Option<Bbox> {
        let mut it = self
            .parts
            .iter()
            .flat_map(|p| p.rings.iter())
            .flat_map(|r| r.vertices.iter());
        let first = it.next()?;
        let mut b = Bbox {
            min_x: first.x,
            min_y: first.y,
            max_x: first.x,
            max_y: first.y,
        };
        for v in it {
            b.min_x = b.min_x.min(v.x);
            b.min_y = b.min_y.min(v.y);
            b.max_x = b.max_x.max(v.x);
            b.max_y = b.max_y.max(v.y);
        }
        Some(b)
    }

    /// The edge table the descent and the test run over. Built once and held beside the polygon
    /// (`polygon-membership.md` §6.3): it borrows the vertices rather than copying them.
    pub fn region(&self) -> PolygonRegion<'_> {
        let mut rings: Vec<&[Vertex]> = Vec::new();
        let mut starts = Vec::new();
        let mut ring_of = Vec::with_capacity(self.vertex_count() as usize);
        for ring in self.parts.iter().flat_map(|p| p.rings.iter()) {
            let r = rings.len() as u32;
            starts.push(ring_of.len() as u32);
            ring_of.extend(std::iter::repeat_n(r, ring.vertices.len()));
            rings.push(ring.vertices.as_slice());
        }
        PolygonRegion {
            rings,
            starts,
            ring_of,
        }
    }

    /// The rings for the wire: vertices of weight at least `min_weight`, then the `budget`
    /// heaviest of what survives, in ring order.
    ///
    /// **A ring's role survives the filter or the ring does not** (`polygon-membership.md` §7.2).
    /// A part's first ring is its outer: filtered to nothing, it takes the whole part with it,
    /// because a surviving hole served first would be drawn as the polygon; left with one or two
    /// vertices, it is served as those, as a degenerate hull is. A hole filtered below three
    /// vertices is dropped rather than served degenerate.
    pub fn rings(&self, min_weight: u32, budget: usize) -> Vec<Vec<Vec<GridPoint>>> {
        self.rings_guarded(min_weight, budget).0
    }

    /// [`Polygon::rings`], and whether the budget cut vertices the depth alone would have kept.
    pub fn rings_guarded(&self, min_weight: u32, budget: usize) -> (Vec<Vec<Vec<GridPoint>>>, bool) {
        // The budget is spent by weight across the whole shape, so a many-ringed shape does not
        // multiply the wire: find the weight threshold at which `budget` vertices survive.
        let mut weights: Vec<u32> = self
            .parts
            .iter()
            .flat_map(|p| p.rings.iter())
            .flat_map(|r| r.vertices.iter())
            .map(|v| v.weight)
            .filter(|w| *w >= min_weight)
            .collect();
        // Everything strictly heavier than the budget-th weight survives; ties at it are taken
        // in ring order until the budget is met.
        let guarded = weights.len() > budget;
        let (threshold, mut left) = if guarded {
            weights.sort_unstable_by(|a, b| b.cmp(a));
            let t = weights[budget.max(1) - 1];
            let heavier = weights.iter().filter(|&&w| w > t).count();
            (t, budget.saturating_sub(heavier))
        } else {
            (min_weight, usize::MAX)
        };
        if budget == 0 {
            return (Vec::new(), guarded);
        }
        let mut out = Vec::new();
        for part in &self.parts {
            let mut rings = Vec::new();
            for (i, ring) in part.rings.iter().enumerate() {
                let mut kept = Vec::new();
                for v in &ring.vertices {
                    if v.weight < threshold.max(min_weight) {
                        continue;
                    }
                    if v.weight == threshold {
                        if left == 0 {
                            continue;
                        }
                        left -= 1;
                    }
                    kept.push((v.x, v.y));
                }
                if i == 0 {
                    if kept.is_empty() {
                        // The outer is gone: so is the part, holes untouched and unspent.
                        break;
                    }
                    rings.push(kept);
                } else if kept.len() >= 3 {
                    rings.push(kept);
                }
            }
            if !rings.is_empty() {
                out.push(rings);
            }
        }
        (out, guarded)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Edge {
    pub ax: i64,
    pub ay: i64,
    pub bx: i64,
    pub by: i64,
}

impl Edge {
    fn bounds(&self) -> (i64, i64, i64, i64) {
        (
            self.ax.min(self.bx),
            self.ay.min(self.by),
            self.ax.max(self.bx),
            self.ay.max(self.by),
        )
    }

    /// Whether `p` lies on the closed segment. Exact.
    fn carries(&self, (px, py): (i64, i64)) -> bool {
        let (x0, y0, x1, y1) = self.bounds();
        if px < x0 || px > x1 || py < y0 || py > y1 {
            return false;
        }
        cross(self.ax, self.ay, self.bx, self.by, px, py) == 0
    }

    /// Whether the perturbed edge crosses the horizontal line `y = Y` strictly right of `X`.
    fn crosses_right_of(&self, y: i64, x: i64) -> bool {
        if (self.ay > y) == (self.by > y) {
            return false;
        }
        let d = self.by - self.ay;
        let num = i128::from(self.ax - x) * i128::from(d)
            + i128::from(y - self.ay) * i128::from(self.bx - self.ax);
        // x_int − X = num / d; the tie x_int == X is left of X.
        (num.signum() * i128::from(d.signum())) > 0
    }

    /// Whether the perturbed edge crosses the vertical line `x = X` strictly above `Y`.
    fn crosses_above(&self, x: i64, y: i64) -> bool {
        if (self.ax > x) == (self.bx > x) {
            return false;
        }
        let e = self.bx - self.ax;
        let num = i128::from(self.ay - y) * i128::from(e)
            + i128::from(x - self.ax) * i128::from(self.by - self.ay);
        match num.signum() * i128::from(e.signum()) {
            1 => true,
            -1 => false,
            // Exactly at Y: above iff dy·dx > 0 (a horizontal edge is below).
            _ => (self.by - self.ay).signum() * e.signum() > 0,
        }
    }

    /// Whether the closed segment meets the closed rectangle. Exact, and conservative in the one
    /// way that is safe: a touch counts.
    fn meets(&self, r: Rect) -> bool {
        let (x0, y0, x1, y1) = self.bounds();
        let (rx0, ry0, rx1, ry1) = (
            i64::from(r.x0),
            i64::from(r.y0),
            i64::from(r.x1),
            i64::from(r.y1),
        );
        if x1 < rx0 || x0 > rx1 || y1 < ry0 || y0 > ry1 {
            return false;
        }
        let inside = |x: i64, y: i64| x >= rx0 && x <= rx1 && y >= ry0 && y <= ry1;
        if inside(self.ax, self.ay) || inside(self.bx, self.by) {
            return true;
        }
        // Both endpoints outside a rectangle the segment's box overlaps: it meets the rectangle
        // iff it meets one of the four sides.
        let sides = [
            Edge {
                ax: rx0,
                ay: ry0,
                bx: rx1,
                by: ry0,
            },
            Edge {
                ax: rx1,
                ay: ry0,
                bx: rx1,
                by: ry1,
            },
            Edge {
                ax: rx1,
                ay: ry1,
                bx: rx0,
                by: ry1,
            },
            Edge {
                ax: rx0,
                ay: ry1,
                bx: rx0,
                by: ry0,
            },
        ];
        sides.iter().any(|s| segments_meet(self, s))
    }
}

/// `(b − a) × (p − a)`, exact.
fn cross(ax: i64, ay: i64, bx: i64, by: i64, px: i64, py: i64) -> i128 {
    i128::from(bx - ax) * i128::from(py - ay) - i128::from(by - ay) * i128::from(px - ax)
}

/// Whether two closed segments share a point. Exact, collinear overlap included.
fn segments_meet(p: &Edge, q: &Edge) -> bool {
    let d1 = cross(q.ax, q.ay, q.bx, q.by, p.ax, p.ay).signum();
    let d2 = cross(q.ax, q.ay, q.bx, q.by, p.bx, p.by).signum();
    let d3 = cross(p.ax, p.ay, p.bx, p.by, q.ax, q.ay).signum();
    let d4 = cross(p.ax, p.ay, p.bx, p.by, q.bx, q.by).signum();
    if d1 * d2 < 0 && d3 * d4 < 0 {
        return true;
    }
    (d1 == 0 && q.carries((p.ax, p.ay)))
        || (d2 == 0 && q.carries((p.bx, p.by)))
        || (d3 == 0 && p.carries((q.ax, q.ay)))
        || (d4 == 0 && p.carries((q.bx, q.by)))
}

/// The polygon as the descent sees it: its edge table.
///
/// **What is held, and why it is this.** Edge `k` runs from the polygon's `k`-th vertex, in
/// part-then-ring order, to the next vertex of the same ring; the table holds only what finds
/// those two vertices in the borrowed ring slices — a ring number per edge and a first-vertex
/// index per ring — so that a held polygon costs its `u32` vertices once
/// (`polygon-membership.md` §6.3, §9): 4 B per edge over the polygon itself, against the 32 B
/// per edge a table of `i64` endpoints cost. The endpoints are widened to `i64` at the point of
/// use, where every predicate is exact `i64`/`i128` arithmetic. The table is built once per
/// artifact and held for the artifact's life; `refine` and `contains` are the hot path and never
/// rebuild it.
#[derive(Debug, Clone)]
pub struct PolygonRegion<'a> {
    /// Every ring's vertices, parts then rings, borrowed from the polygon.
    rings: Vec<&'a [Vertex]>,
    /// Per ring, the flat index of its first vertex — the number of the ring's first edge.
    starts: Vec<u32>,
    /// Per edge, the ring it belongs to.
    ring_of: Vec<u32>,
}

/// A tile's context: the edges that meet it, and the parity of the horizontal ray from its
/// lower corner `(x0, y0)` — under the tie rule, the count of edges crossing `y = y0` right of
/// `x0`, mod 2.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PolyCtx {
    pub edges: Vec<u32>,
    pub parity: bool,
}

impl PolygonRegion<'_> {
    pub fn edge_count(&self) -> usize {
        self.ring_of.len()
    }

    /// Edge `k`, its endpoints widened for the exact predicates.
    pub(crate) fn edge(&self, k: u32) -> Edge {
        let r = self.ring_of[k as usize] as usize;
        let ring = self.rings[r];
        let i = (k - self.starts[r]) as usize;
        let a = ring[i];
        let b = ring[if i + 1 == ring.len() { 0 } else { i + 1 }];
        Edge {
            ax: i64::from(a.x),
            ay: i64::from(a.y),
            bx: i64::from(b.x),
            by: i64::from(b.y),
        }
    }

    pub(crate) fn edges(&self) -> impl Iterator<Item = Edge> + '_ {
        (0..self.edge_count() as u32).map(|k| self.edge(k))
    }

    /// The direct even-odd test over every edge: on an edge, or an odd ray count.
    pub fn contains_direct(&self, p: GridPoint) -> bool {
        let q = (i64::from(p.0), i64::from(p.1));
        self.edges().any(|e| e.carries(q)) || self.ray_parity(p)
    }

    /// The parity of the horizontal ray from `p` under the tie rule, over every edge — what a
    /// tile's context carries for its lower corner, without the on-edge override.
    pub fn ray_parity(&self, (px, py): GridPoint) -> bool {
        let (x, y) = (i64::from(px), i64::from(py));
        self.edges().filter(|e| e.crosses_right_of(y, x)).count() % 2 == 1
    }

    /// Crossings of the path from `from` to `to` — horizontal first, then vertical — over the
    /// given edges, mod 2. `to` is right of and above `from`, or equal to it on either axis.
    fn path_parity(&self, edges: &[u32], from: (i64, i64), to: (i64, i64)) -> bool {
        if from == to {
            // The first child of every tile shares its parent's corner: an empty path.
            return false;
        }
        let mut parity = false;
        for &i in edges {
            let e = self.edge(i);
            // Horizontal leg along y = from.1 over (from.0, to.0]: crossings right of from.0
            // that are not right of to.0.
            if to.0 > from.0
                && e.crosses_right_of(from.1, from.0)
                && !e.crosses_right_of(from.1, to.0)
            {
                parity = !parity;
            }
            // Vertical leg along x = to.0 over (from.1, to.1].
            if to.1 > from.1 && e.crosses_above(to.0, from.1) && !e.crosses_above(to.0, to.1) {
                parity = !parity;
            }
        }
        parity
    }
}

impl Region for PolygonRegion<'_> {
    type Ctx = PolyCtx;

    fn root(&self) -> PolyCtx {
        PolyCtx {
            edges: (0..self.edge_count() as u32).collect(),
            parity: self.ray_parity((0, 0)),
        }
    }

    fn refine(&self, parent: &PolyCtx, from: Rect, to: Rect) -> PolyCtx {
        let edges: Vec<u32> = parent
            .edges
            .iter()
            .copied()
            .filter(|&i| self.edge(i).meets(to))
            .collect();
        // The path from the parent's corner to the child's runs along the parent's bottom side
        // and then up the child's left side, so it lies inside the parent and can only be crossed
        // by the parent's own edges.
        let step = self.path_parity(
            &parent.edges,
            (i64::from(from.x0), i64::from(from.y0)),
            (i64::from(to.x0), i64::from(to.y0)),
        );
        PolyCtx {
            edges,
            parity: parent.parity ^ step,
        }
    }

    fn classify(&self, _rect: Rect, ctx: &PolyCtx) -> Class {
        if ctx.edges.is_empty() {
            if ctx.parity {
                Class::Inside
            } else {
                Class::Disjoint
            }
        } else {
            Class::Crossed
        }
    }

    fn contains(&self, (px, py): GridPoint, rect: Rect, ctx: &PolyCtx) -> bool {
        let p = (i64::from(px), i64::from(py));
        if ctx.edges.iter().any(|&i| self.edge(i).carries(p)) {
            return true;
        }
        ctx.parity ^ self.path_parity(&ctx.edges, (i64::from(rect.x0), i64::from(rect.y0)), p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(x: u32, y: u32) -> Vertex {
        Vertex {
            x,
            y,
            weight: u32::MAX,
        }
    }

    fn one_ring(vertices: Vec<Vertex>) -> Polygon {
        Polygon {
            parts: vec![Part {
                rings: vec![Ring { vertices }],
            }],
        }
    }

    fn square() -> Polygon {
        one_ring(vec![v(10, 10), v(30, 10), v(30, 30), v(10, 30)])
    }

    #[test]
    fn a_square_holds_its_interior_its_edges_and_its_corners() {
        let p = square();
        let s = p.region();
        assert!(s.contains_direct((20, 20)));
        assert!(s.contains_direct((10, 20)));
        assert!(s.contains_direct((30, 30)));
        assert!(s.contains_direct((10, 10)));
        assert!(!s.contains_direct((31, 20)));
        assert!(!s.contains_direct((9, 10)));
        assert!(!s.contains_direct((20, 31)));
    }

    #[test]
    fn a_ring_through_the_ray_vertex_counts_once() {
        // A diamond whose vertex sits exactly on the ray from (0, 20): the tie rule places the
        // vertex below the line, so exactly one of its two edges straddles it.
        let p = one_ring(vec![v(20, 10), v(30, 20), v(20, 30), v(10, 20)]);
        let d = p.region();
        assert!(!d.contains_direct((0, 20)));
        assert!(d.contains_direct((20, 20)));
        assert!(d.contains_direct((10, 20))); // on the vertex: on an edge, inside
    }

    #[test]
    fn refinement_agrees_with_the_direct_test_at_every_corner() {
        let p = square();
        let s = p.region();
        let root = s.root();
        let from = Rect::ALL;
        for k in 0..4u64 {
            let child = crate::morton::Tile {
                prefix: k,
                depth: 1,
            };
            let to = Rect::of_tile(&child);
            let ctx = s.refine(&root, from, to);
            let direct = s
                .edges()
                .filter(|e| e.crosses_right_of(i64::from(to.y0), i64::from(to.x0)))
                .count()
                % 2
                == 1;
            assert_eq!(ctx.parity, direct, "child {k}");
        }
    }

    #[test]
    fn the_edge_table_indexes_every_ring_and_wraps_each() {
        let p = Polygon {
            parts: vec![
                Part {
                    rings: vec![
                        Ring {
                            vertices: vec![v(0, 0), v(100, 0), v(100, 100), v(0, 100)],
                        },
                        Ring {
                            vertices: vec![v(10, 10), v(10, 20), v(20, 20)],
                        },
                    ],
                },
                Part {
                    rings: vec![Ring {
                        vertices: vec![v(200, 0), v(300, 0), v(200, 100)],
                    }],
                },
            ],
        };
        let r = p.region();
        assert_eq!(r.edge_count(), 10);
        let e = |k| {
            let e = r.edge(k);
            ((e.ax, e.ay), (e.bx, e.by))
        };
        assert_eq!(e(3), ((0, 100), (0, 0)));
        assert_eq!(e(4), ((10, 10), (10, 20)));
        assert_eq!(e(6), ((20, 20), (10, 10)));
        assert_eq!(e(9), ((200, 100), (200, 0)));
        assert!(r.contains_direct((50, 50)));
        assert!(!r.contains_direct((12, 18)));
        assert!(r.contains_direct((250, 10)));
    }

    #[test]
    fn a_part_whose_outer_filters_away_goes_with_its_hole() {
        // A light outer around a heavy hole: past the outer's weight the hole must not be served
        // first and drawn as the polygon.
        let w = |x, y, weight| Vertex { x, y, weight };
        let p = Polygon {
            parts: vec![Part {
                rings: vec![
                    Ring {
                        vertices: vec![w(0, 0, 5), w(100, 0, 5), w(100, 100, 5), w(0, 100, 5)],
                    },
                    Ring {
                        vertices: vec![
                            w(40, 40, 900),
                            w(40, 60, 900),
                            w(60, 60, 900),
                            w(60, 40, 900),
                        ],
                    },
                ],
            }],
        };
        assert_eq!(p.rings(0, usize::MAX).len(), 1);
        assert_eq!(p.rings(0, usize::MAX)[0].len(), 2);
        // Above the outer's weight, by the filter.
        assert!(p.rings(6, usize::MAX).is_empty());
        // And by the budget: the four heaviest are the hole's.
        assert!(p.rings(0, 4).is_empty());
        // Five: the outer keeps one vertex and is served as it, the hole intact.
        let five = p.rings(0, 5);
        assert_eq!(five.len(), 1);
        assert_eq!(five[0][0].len(), 1);
        assert_eq!(five[0][1].len(), 4);
    }

    #[test]
    fn a_hole_filtered_below_three_vertices_is_dropped() {
        let w = |x, y, weight| Vertex { x, y, weight };
        let p = Polygon {
            parts: vec![Part {
                rings: vec![
                    Ring {
                        vertices: vec![
                            w(0, 0, 900),
                            w(100, 0, 900),
                            w(100, 100, 900),
                            w(0, 100, 900),
                        ],
                    },
                    Ring {
                        vertices: vec![w(40, 40, 50), w(40, 60, 50), w(60, 60, 5), w(60, 40, 5)],
                    },
                ],
            }],
        };
        let served = p.rings(10, usize::MAX);
        assert_eq!(served.len(), 1);
        assert_eq!(served[0].len(), 1, "a two-vertex hole is not served");
        assert_eq!(served[0][0].len(), 4);
    }
}
