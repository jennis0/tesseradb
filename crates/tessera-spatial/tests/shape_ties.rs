//! The descent's tie handling, on fixtures built to tie (`polygon-membership.md` §12, "stage 1
//! owes one more test file").
//!
//! The property tests in `shape.rs` draw star polygons at random `f64` positions and essentially
//! never put a vertex on a tile boundary — and a tile boundary is where a wrong carried parity
//! flips an *interior tile* rather than one cell. These fixtures are deterministic and lie exactly
//! on the grid: vertices on tile corners and tile edges at depths 4, 10 and 16, axis-aligned
//! edges along tile boundaries on both sides (the `x0` of one tile and the `x1` of its
//! neighbour), a horizontal edge along a cell's bottom, collinear runs the canonical form must
//! remove, a hole touching its outer at one vertex, two parts sharing an edge, and a ring through
//! `(0, 0)`, the root ray's origin. Every fixture is held to three things: the decomposition
//! answers as the direct test does at probes placed on those grid lines and one unit either side;
//! every boundary cell's carried parity is the full ray cast from its corner; and every interior
//! tile's four corners are inside by the direct test.

use std::collections::BTreeSet;

use tessera_spatial::morton::{split32, Bounds, Tile};
use tessera_spatial::shape::{
    Decomposition, Part, PolyCtx, Polygon, PreparedShape, Rect, Ring, Shape, ShapeF64, Vertex,
};

/// The side of a tile at depth `d`, in grid units.
const fn side(d: u32) -> u32 {
    (1u64 << (32 - d)) as u32
}

fn v(x: u32, y: u32) -> Vertex {
    Vertex {
        x,
        y,
        weight: u32::MAX,
    }
}

fn polygon(parts: &[&[&[(u32, u32)]]]) -> Shape {
    Shape::Polygon(Polygon {
        parts: parts
            .iter()
            .map(|rings| Part {
                rings: rings
                    .iter()
                    .map(|ring| Ring {
                        vertices: ring.iter().map(|&(x, y)| v(x, y)).collect(),
                    })
                    .collect(),
            })
            .collect(),
    })
}

/// A decomposition indexed for the lookup a probe makes — `shape.rs`'s `Lookup`.
struct Lookup<'a> {
    d: &'a Decomposition<PolyCtx>,
    ranges: Vec<(u64, u64)>,
    cells: Vec<(u32, usize)>,
}

impl<'a> Lookup<'a> {
    fn new(d: &'a Decomposition<PolyCtx>) -> Self {
        let mut ranges: Vec<(u64, u64)> = d
            .interior
            .iter()
            .chain(&d.cover)
            .map(Tile::code_range)
            .collect();
        ranges.sort_unstable();
        let mut cells: Vec<(u32, usize)> = d
            .boundary
            .iter()
            .enumerate()
            .map(|(i, b)| (b.cell.raw(), i))
            .collect();
        cells.sort_unstable();
        Lookup { d, ranges, cells }
    }

    fn contains(&self, shape: &PreparedShape<'_>, p: (u32, u32)) -> bool {
        let (cell, _) = split32(p.0, p.1);
        let code = u64::from(cell.raw());
        let k = self.ranges.partition_point(|&(lo, _)| lo <= code);
        if k > 0 && code < self.ranges[k - 1].1 {
            return true;
        }
        match self.cells.binary_search_by_key(&cell.raw(), |&(c, _)| c) {
            Err(_) => false,
            Ok(k) => {
                let b = &self.d.boundary[self.cells[k].1];
                shape.contains_in_cell(p, Rect::of_cell(cell), &b.ctx)
            }
        }
    }
}

/// The coordinates a fixture ties on, each with one unit either side: every vertex coordinate,
/// every tile boundary at depths 4 and 10 across the shape's bounds, and the depth-16 cell
/// boundaries within two cells of every vertex. The probes are the cross product.
fn probes(shape: &Shape) -> Vec<(u32, u32)> {
    let b = shape.bounds().unwrap();
    let mut xs = BTreeSet::new();
    let mut ys = BTreeSet::new();
    let around = |set: &mut BTreeSet<u32>, c: u32| {
        set.insert(c);
        set.insert(c.saturating_sub(1));
        set.insert(c.saturating_add(1));
    };
    let Shape::Polygon(p) = shape else {
        unreachable!()
    };
    for vertex in p
        .parts
        .iter()
        .flat_map(|p| p.rings.iter())
        .flat_map(|r| r.vertices.iter())
    {
        around(&mut xs, vertex.x);
        around(&mut ys, vertex.y);
        for k in -2i64..=2 {
            let s = i64::from(side(16));
            let gx = (i64::from(vertex.x) / s + k) * s;
            let gy = (i64::from(vertex.y) / s + k) * s;
            if (0..=i64::from(u32::MAX)).contains(&gx) {
                around(&mut xs, gx as u32);
            }
            if (0..=i64::from(u32::MAX)).contains(&gy) {
                around(&mut ys, gy as u32);
            }
        }
    }
    for d in [4u32, 10] {
        let s = u64::from(side(d));
        let lo = u64::from(b.min_x).saturating_sub(s) / s;
        let hi = (u64::from(b.max_x) + s) / s;
        for k in lo..=hi.min(lo + 64) {
            if k * s <= u64::from(u32::MAX) {
                around(&mut xs, (k * s) as u32);
            }
        }
        let lo = u64::from(b.min_y).saturating_sub(s) / s;
        let hi = (u64::from(b.max_y) + s) / s;
        for k in lo..=hi.min(lo + 64) {
            if k * s <= u64::from(u32::MAX) {
                around(&mut ys, (k * s) as u32);
            }
        }
    }
    // The mid-lines too, so a probe row runs through the interior and not only along an edge.
    around(
        &mut xs,
        ((u64::from(b.min_x) + u64::from(b.max_x)) / 2) as u32,
    );
    around(
        &mut ys,
        ((u64::from(b.min_y) + u64::from(b.max_y)) / 2) as u32,
    );
    let mut out = Vec::with_capacity(xs.len() * ys.len());
    for &x in &xs {
        for &y in &ys {
            out.push((x, y));
        }
    }
    out
}

/// The three holds, on one fixture.
fn hold(name: &str, shape: &Shape) {
    let prepared = shape.prepared();
    let region = prepared.region().unwrap();
    let d = prepared.decompose(None);
    assert!(!d.is_cover(), "{name}: no budget, no cover");
    let look = Lookup::new(&d);
    let probes = probes(shape);
    assert!(probes.len() > 16, "{name}: {} probes", probes.len());
    let mut inside = 0usize;
    for p in probes.iter().copied() {
        let direct = prepared.contains(p);
        inside += usize::from(direct);
        assert_eq!(
            look.contains(&prepared, p),
            direct,
            "{name}: the descent disagrees with the direct test at {p:?}"
        );
    }
    assert!(inside > 0, "{name}: no probe fell inside");
    assert!(inside < probes.len(), "{name}: no probe fell outside");
    for b in &d.boundary {
        let r = Rect::of_cell(b.cell);
        assert_eq!(
            b.ctx.parity,
            region.ray_parity((r.x0, r.y0)),
            "{name}: carried parity is not the ray cast at cell {r:?}"
        );
    }
    assert!(
        !d.interior.is_empty(),
        "{name}: every fixture is wide enough to hold a tile"
    );
    for t in &d.interior {
        for c in Rect::of_tile(t).corners() {
            assert!(
                region.contains_direct(c),
                "{name}: interior tile {t:?} has corner {c:?} outside"
            );
        }
    }
}

/// Squares whose four corners are tile corners at depth `d`, from tile `(a, a)` to `(b, b)`: the
/// left and bottom edges lie on the `x0`/`y0` of a tile and the right and top on the `x0` of the
/// next, which is `x1 + 1` of the tile before it.
#[test]
fn a_square_on_tile_corners_at_three_depths() {
    for d in [4u32, 10, 16] {
        let s = side(d);
        let (a, b) = (2 * s, 5 * s);
        hold(
            &format!("corner square at depth {d}"),
            &polygon(&[&[&[(a, a), (b, a), (b, b), (a, b)]]]),
        );
    }
}

/// Squares whose edges lie on the `x1`/`y1` of a tile — one unit short of the boundary at every
/// depth.
#[test]
fn a_square_on_the_far_edges_of_tiles() {
    for d in [4u32, 10, 16] {
        let s = side(d);
        let (a, b) = (2 * s - 1, 5 * s - 1);
        hold(
            &format!("far-edge square at depth {d}"),
            &polygon(&[&[&[(a, a), (b, a), (b, b), (a, b)]]]),
        );
    }
}

/// A triangle whose base is a horizontal edge along a depth-16 cell's bottom, with a vertex on
/// a cell corner and one strictly inside a cell.
#[test]
fn a_horizontal_edge_along_a_cells_bottom() {
    let s = side(16);
    hold(
        "flat-bottomed triangle",
        &polygon(&[&[&[
            (3 * s, 7 * s),
            (40 * s, 7 * s),
            (20 * s + 12_345, 30 * s + 6_789),
        ]]]),
    );
    // And with the base one unit under and over the boundary.
    hold(
        "triangle a unit under a cell bottom",
        &polygon(&[&[&[
            (3 * s, 7 * s - 1),
            (40 * s, 7 * s - 1),
            (20 * s + 12_345, 30 * s),
        ]]]),
    );
    hold(
        "triangle a unit over a cell bottom",
        &polygon(&[&[&[
            (3 * s, 7 * s + 1),
            (40 * s, 7 * s + 1),
            (20 * s + 12_345, 30 * s),
        ]]]),
    );
}

/// A diamond whose four vertices sit on depth-4 tile corners: a vertex on the ray at every tile
/// row it touches, and every edge crossing tile corners diagonally.
#[test]
fn a_diamond_on_tile_corners() {
    let s = side(4);
    hold(
        "diamond",
        &polygon(&[&[&[
            (6 * s, 2 * s),
            (10 * s, 6 * s),
            (6 * s, 10 * s),
            (2 * s, 6 * s),
        ]]]),
    );
    let s = side(10);
    hold(
        "diamond at depth 10",
        &polygon(&[&[&[
            (600 * s, 200 * s),
            (1000 * s, 600 * s),
            (600 * s, 1000 * s),
            (200 * s, 600 * s),
        ]]]),
    );
}

/// A square given with collinear runs on every edge and a doubled vertex, through the canonical
/// form, which must reduce it to four vertices on depth-4 corners — and then behave as the plain
/// square does.
#[test]
fn collinear_runs_are_removed_and_the_square_ties_as_before() {
    let s = f64::from(side(4));
    let (a, b) = (2.0 * s, 5.0 * s);
    let m = 3.0 * s;
    let ring = vec![
        (a, a),
        (m, a),
        (4.0 * s, a),
        (b, a),
        (b, m),
        (b, b),
        (b, b),
        (m, b),
        (a, b),
        (a, m),
        (a, a),
    ];
    let extent = Bounds {
        x_min: 0.0,
        x_max: 4_294_967_296.0,
        y_min: 0.0,
        y_max: 4_294_967_296.0,
    };
    let (shape, report) = ShapeF64::Polygon(vec![vec![ring]])
        .canonical(&extent)
        .unwrap();
    assert_eq!(report.vertices_out, 4);
    let Shape::Polygon(p) = &shape else {
        unreachable!()
    };
    let got: BTreeSet<(u32, u32)> = p.parts[0].rings[0]
        .vertices
        .iter()
        .map(|v| (v.x, v.y))
        .collect();
    let s = side(4);
    let want: BTreeSet<(u32, u32)> = [
        (2 * s, 2 * s),
        (5 * s, 2 * s),
        (5 * s, 5 * s),
        (2 * s, 5 * s),
    ]
    .into_iter()
    .collect();
    assert_eq!(got, want);
    hold("canonicalised square", &shape);
}

/// A hole that touches its outer at one vertex, the outer's lower-left corner — a vertex two
/// rings share, on a depth-4 tile corner.
#[test]
fn a_hole_touching_its_outer_at_one_vertex() {
    let s = side(4);
    let (a, b) = (2 * s, 6 * s);
    hold(
        "touching hole",
        &polygon(&[&[
            &[(a, a), (b, a), (b, b), (a, b)],
            &[(a, a), (a + s, a + 2 * s), (a + 2 * s, a + s)],
        ]]),
    );
    // The same, at depth 16 cells.
    let s = side(16);
    let (a, b) = (200 * s, 600 * s);
    hold(
        "touching hole at depth 16",
        &polygon(&[&[
            &[(a, a), (b, a), (b, b), (a, b)],
            &[
                (a, a),
                (a + 100 * s, a + 200 * s),
                (a + 200 * s, a + 100 * s),
            ],
        ]]),
    );
}

/// Two parts sharing an edge along a tile boundary: the shared edge is in both rings, so a ray
/// crosses it twice and the tie rule must count both or neither.
#[test]
fn two_parts_sharing_an_edge() {
    for d in [4u32, 16] {
        let s = side(d);
        let (a, b, c) = (2 * s, 5 * s, 8 * s);
        hold(
            &format!("shared edge at depth {d}"),
            &polygon(&[
                &[&[(a, a), (b, a), (b, b), (a, b)]],
                &[&[(b, a), (c, a), (c, b), (b, b)]],
            ]),
        );
        // Sharing a horizontal edge — the ray's own direction.
        hold(
            &format!("shared horizontal edge at depth {d}"),
            &polygon(&[
                &[&[(a, a), (b, a), (b, b), (a, b)]],
                &[&[(a, b), (b, b), (b, c), (a, c)]],
            ]),
        );
    }
}

/// Rings through `(0, 0)`, the origin of the root's ray: a triangle with a vertex there, a square
/// in the grid's corner, and a triangle whose edge passes through the origin without a vertex on
/// it.
#[test]
fn a_ring_through_the_origin() {
    let s = side(4);
    hold(
        "triangle at the origin",
        &polygon(&[&[&[(0, 0), (3 * s, 0), (0, 3 * s)]]]),
    );
    hold(
        "square in the corner",
        &polygon(&[&[&[(0, 0), (s, 0), (s, s), (0, s)]]]),
    );
    hold(
        "square in the far corner",
        &polygon(&[&[&[
            (u32::MAX - s + 1, u32::MAX - s + 1),
            (u32::MAX, u32::MAX - s + 1),
            (u32::MAX, u32::MAX),
            (u32::MAX - s + 1, u32::MAX),
        ]]]),
    );
    // An edge through the origin: from (0, 2s) to (2s, 0) passes (s, s), not (0, 0); so put a
    // vertex left of and below nothing — the grid has no negative side — and instead run an edge
    // along each axis from the origin.
    hold(
        "edges along both axes",
        &polygon(&[&[&[(0, 0), (4 * s, 0), (4 * s, 3 * s), (2 * s, s), (0, 4 * s)]]]),
    );
}

/// Vertices on tile corners at every depth at once: a coordinate that is a multiple of the
/// depth-4 side is a corner at every depth below it, so a ring on depth-4 corners already ties
/// at 10 and 16; this ring mixes them, one vertex on a depth-4 corner, one on a depth-10 corner
/// that is not a depth-4 one, one on a depth-16 corner that is neither.
#[test]
fn vertices_on_corners_of_mixed_depths() {
    let (s4, s10, s16) = (side(4), side(10), side(16));
    hold(
        "mixed corners",
        &polygon(&[&[&[
            (2 * s4, 2 * s4),
            (5 * s4 + 3 * s10, 2 * s4 + 7 * s10),
            (5 * s4 + 3 * s10 + 11 * s16, 5 * s4 + 9 * s16),
            (2 * s4 + s16, 5 * s4 + s10),
        ]]]),
    );
}
