//! The descent holds to the direct test, for every kind (`polygon-membership.md` §12, stage 1).
//!
//! A shape's membership is defined by its direct test — `Shape::contains`, which for a polygon is
//! the naive even-odd walk over every edge and for a conic the quadratic. The decomposition must
//! answer identically for every position: inside an interior tile ⇒ inside; in a boundary cell ⇒
//! whatever the cell's own context says; anywhere else ⇒ outside. These tests draw random shapes
//! and random positions — dense near the boundary, where the tie rules live — and hold the two
//! answers together.

use proptest::prelude::*;
use tessera_spatial::morton::{split32, Bounds, Tile};
use tessera_spatial::shape::{
    read_wkt, Decomposition, PolyCtx, PreparedShape, Rect, Shape, ShapeF64, Space,
};

const E: Bounds = Bounds {
    x_min: 0.0,
    x_max: 1_000_000.0,
    y_min: 0.0,
    y_max: 1_000_000.0,
};

/// A decomposition indexed for the lookup a probe makes: tile code ranges sorted by their start,
/// boundary cells sorted by code. The descent's tiles are disjoint, so one binary search answers
/// each. The linear scan this replaced cost the polygon test fifty seconds at 64 cases.
struct Lookup<'a> {
    d: &'a Decomposition<PolyCtx>,
    /// `(lo, hi)` of every interior and cover tile, sorted by `lo`.
    ranges: Vec<(u64, u64)>,
    /// `(cell code, index into `d.boundary`)`, sorted by code.
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

    /// A position's membership as the decomposition sees it.
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

/// Positions to test: random ones, plus ones near every vertex and along every edge, where the
/// ties are.
fn probes(shape: &Shape, seed: u64) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    let mut s = seed | 1;
    let mut next = || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    let b = shape.bounds().unwrap_or(tessera_spatial::shape::Bbox {
        min_x: 0,
        min_y: 0,
        max_x: u32::MAX,
        max_y: u32::MAX,
    });
    let span_x = u64::from(b.max_x - b.min_x) + 1;
    let span_y = u64::from(b.max_y - b.min_y) + 1;
    for _ in 0..200 {
        let x = (u64::from(b.min_x) + next() % span_x) as u32;
        let y = (u64::from(b.min_y) + next() % span_y) as u32;
        out.push((x, y));
    }
    if let Shape::Polygon(p) = shape {
        for part in &p.parts {
            for ring in &part.rings {
                let n = ring.vertices.len();
                for i in 0..n {
                    let a = ring.vertices[i];
                    let c = ring.vertices[(i + 1) % n];
                    for (dx, dy) in [
                        (0i64, 0i64),
                        (1, 0),
                        (-1, 0),
                        (0, 1),
                        (0, -1),
                        (1, 1),
                        (-1, -1),
                    ] {
                        let x = (i64::from(a.x) + dx).clamp(0, i64::from(u32::MAX)) as u32;
                        let y = (i64::from(a.y) + dy).clamp(0, i64::from(u32::MAX)) as u32;
                        out.push((x, y));
                    }
                    for k in 1..8u64 {
                        let x = (i64::from(a.x) + (i64::from(c.x) - i64::from(a.x)) * k as i64 / 8)
                            as u32;
                        let y = (i64::from(a.y) + (i64::from(c.y) - i64::from(a.y)) * k as i64 / 8)
                            as u32;
                        out.push((x, y));
                        out.push((x.saturating_add(1), y));
                        out.push((x, y.saturating_add(1)));
                    }
                }
            }
        }
    }
    out
}

/// Whether every served ring is one stored ring with vertices removed — nothing added, nothing
/// moved, nothing reordered.
///
/// The stored rings are matched in order and each is used at most once, so a served ring that
/// appeared before the ring it came from, or twice, fails as well as one carrying a vertex the
/// shape does not hold. Serving may drop a whole part or hole (`polygon-membership.md` §7.2), so
/// a stored ring with no served counterpart is not itself a failure.
fn each_served_ring_is_a_stored_ring_filtered(
    served: &[Vec<Vec<(u32, u32)>>],
    stored: &[Vec<(u32, u32)>],
) -> Result<(), String> {
    let mut next = 0usize;
    for (i, ring) in served.iter().flatten().enumerate() {
        let found = (next..stored.len()).find(|&k| is_subsequence(ring, &stored[k]));
        match found {
            Some(k) => next = k + 1,
            None => {
                return Err(format!(
                    "served ring {i} ({} vertices) is not a subsequence of any stored ring at or \
                     after {next} of {}: a served vertex is not one of the shape's own, or the \
                     rings are out of order",
                    ring.len(),
                    stored.len()
                ))
            }
        }
    }
    Ok(())
}

/// `a` is `b` with zero or more elements removed.
fn is_subsequence(a: &[(u32, u32)], b: &[(u32, u32)]) -> bool {
    let mut it = b.iter();
    a.iter().all(|v| it.any(|w| w == v))
}

fn rings_strategy() -> impl Strategy<Value = ShapeF64> {
    // A star-shaped ring around a centre with random radii, sometimes with a hole and
    // sometimes a second part: simple enough to be OGC-valid, jagged enough to cross many cells.
    let ring = |cx: f64, cy: f64, r: f64, n: usize| {
        proptest::collection::vec(0.2f64..1.0, n).prop_map(move |radii| {
            radii
                .iter()
                .enumerate()
                .map(|(k, f)| {
                    let t = k as f64 / radii.len() as f64 * std::f64::consts::TAU;
                    (cx + r * f * t.cos(), cy + r * f * t.sin())
                })
                .collect::<Vec<_>>()
        })
    };
    (
        (
            100_000f64..900_000.0,
            100_000f64..900_000.0,
            1_000f64..300_000.0,
            3usize..40,
        ),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_flat_map(move |((cx, cy, r, n), hole, second)| {
            let outer = ring(cx, cy, r, n);
            let hole = if hole {
                ring(cx, cy, r * 0.15, 5).prop_map(Some).boxed()
            } else {
                Just(None).boxed()
            };
            let second = if second {
                ring(cx + r * 3.0, cy, r * 0.5, 7).prop_map(Some).boxed()
            } else {
                Just(None).boxed()
            };
            (outer, hole, second).prop_map(|(o, h, s)| {
                let mut part = vec![o];
                if let Some(h) = h {
                    part.push(h);
                }
                let mut parts = vec![part];
                if let Some(s) = s {
                    parts.push(vec![s]);
                }
                ShapeF64::Polygon(parts)
            })
        })
}

fn conic_strategy() -> impl Strategy<Value = ShapeF64> {
    (
        100_000f64..900_000.0,
        100_000f64..900_000.0,
        500f64..200_000.0,
        500f64..200_000.0,
        0f64..180.0,
        any::<bool>(),
    )
        .prop_map(|(cx, cy, a, b, angle, circle)| {
            if circle {
                ShapeF64::Circle { cx, cy, r: a }
            } else {
                ShapeF64::Ellipse {
                    cx,
                    cy,
                    a,
                    b,
                    angle_degrees: angle,
                }
            }
        })
}

fn bbox_strategy() -> impl Strategy<Value = ShapeF64> {
    (
        0f64..1_000_000.0,
        0f64..1_000_000.0,
        0f64..1_000_000.0,
        0f64..1_000_000.0,
    )
        .prop_map(|(x0, y0, x1, y1)| ShapeF64::Bbox {
            min_x: x0.min(x1),
            min_y: y0.min(y1),
            max_x: x0.max(x1),
            max_y: y0.max(y1),
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn a_conic_decomposes_to_its_direct_test_away_from_rounding(shape in conic_strategy(), seed in any::<u64>()) {
        let (shape, _) = shape.canonical(Space::View, &E).unwrap();
        let prepared = shape.prepared();
        let d = prepared.decompose(None);
        let Shape::Conic(c) = &shape else { unreachable!() };
        let look = Lookup::new(&d);
        for p in probes(&shape, seed) {
            // Skip the positions within rounding of the curve: there the decomposition's corner
            // test and the direct test may legitimately fall either side of 1.0.
            let dx = f64::from(p.0) - f64::from(c.cx);
            let dy = f64::from(p.1) - f64::from(c.cy);
            let q = c.m11 * dx * dx + 2.0 * c.m12 * dx * dy + c.m22 * dy * dy;
            if (q - 1.0).abs() < 1e-9 {
                continue;
            }
            prop_assert_eq!(look.contains(&prepared, p), prepared.contains(p), "at {:?}", p);
        }
    }

    #[test]
    fn a_box_decomposes_to_its_direct_test(shape in bbox_strategy(), seed in any::<u64>()) {
        let (shape, _) = shape.canonical(Space::View, &E).unwrap();
        let prepared = shape.prepared();
        let d = prepared.decompose(None);
        let look = Lookup::new(&d);
        for p in probes(&shape, seed) {
            prop_assert_eq!(look.contains(&prepared, p), prepared.contains(p), "at {:?}", p);
        }
    }

    #[test]
    fn a_budget_yields_a_cover_that_is_a_superset(shape in rings_strategy(), seed in any::<u64>()) {
        let (shape, _) = shape.canonical(Space::View, &E).unwrap();
        let prepared = shape.prepared();
        let d = prepared.decompose(Some(64));
        let look = Lookup::new(&d);
        for p in probes(&shape, seed) {
            if prepared.contains(p) {
                prop_assert!(look.contains(&prepared, p), "cover lost {:?}", p);
            }
        }
        if d.is_cover() {
            prop_assert!(d.boundary.is_empty());
            let depth = d.cover_depth.unwrap();
            prop_assert!(d.cover.iter().all(|t| t.depth == depth));
        }
    }

    /// **A served ring is the stored ring filtered** (`polygon-membership.md` §7.2): every vertex
    /// on the wire is one of the shape's own, in the shape's own order, at every resolution. The
    /// vertex count falling is the weaker half and is checked alongside it.
    ///
    /// Mutations this kills: a serving path that resamples or interpolates a ring rather than
    /// dropping vertices from it (the count still falls, and every vertex is new); one that
    /// perturbs a served coordinate; one that reorders the rings or the vertices within one.
    #[test]
    fn the_served_rings_are_a_subsequence_at_every_resolution(shape in rings_strategy()) {
        let (shape, _) = shape.canonical(Space::View, &E).unwrap();
        let Shape::Polygon(poly) = &shape else { unreachable!() };
        // The stored rings, read from the polygon itself rather than back through the serving
        // path — a comparison against `rings(0, MAX)` would be the serving path agreeing with
        // itself.
        let stored: Vec<Vec<(u32, u32)>> = poly
            .parts
            .iter()
            .flat_map(|p| p.rings.iter())
            .map(|r| r.vertices.iter().map(|v| (v.x, v.y)).collect())
            .collect();
        let full = shape.rings(0, usize::MAX);
        prop_assert_eq!(full.iter().flatten().map(Vec::len).sum::<usize>() as u64, poly.vertex_count());
        // Unfiltered, the wire is the stored rings exactly — which is the base case of the
        // subsequence property and the guard that `stored` is the right reference.
        prop_assert_eq!(full.iter().flatten().cloned().collect::<Vec<_>>(), stored.clone());
        let mut last = poly.vertex_count() as usize;
        for w in [1u32 << 8, 1 << 12, 1 << 16, 1 << 20] {
            let rings = shape.rings(w, 2048);
            let n: usize = rings.iter().flatten().map(Vec::len).sum();
            prop_assert!(n <= last.max(2048).min(last), "w={w} n={n} last={last}");
            if let Err(why) = each_served_ring_is_a_stored_ring_filtered(&rings, &stored) {
                return Err(TestCaseError::fail(format!("w={w}: {why}")));
            }
            last = n;
        }
        let capped_rings = shape.rings(0, 8);
        if let Err(why) = each_served_ring_is_a_stored_ring_filtered(&capped_rings, &stored) {
            return Err(TestCaseError::fail(format!("budget 8: {why}")));
        }
        prop_assert!(capped_rings.iter().flatten().map(Vec::len).sum::<usize>() <= 8);
        // The guard says when it cut what the resolution alone would have kept
        // (`polygon-membership.md` §7.2), and only then.
        let (_, fired) = shape.rings_guarded(0, 8);
        prop_assert_eq!(fired, poly.vertex_count() > 8);
        let (_, unfired) = shape.rings_guarded(0, usize::MAX);
        prop_assert!(!unfired);
    }
}

/// A curve's guard fires when the chord tolerance asks for more vertices than the budget holds —
/// a large circle at a fine tolerance — and not when the budget is generous or the circle small.
#[test]
fn a_conics_guard_fires_only_when_the_budget_binds_its_densification() {
    let circle = ShapeF64::Circle {
        cx: 500.0,
        cy: 500.0,
        r: 400.0,
    };
    let (shape, _) = circle.canonical(Space::View, &E).unwrap();
    let (ring, fired) = shape.rings_guarded(1, 64);
    assert_eq!(ring[0][0].len(), 64);
    assert!(
        fired,
        "a 400-unit radius at a one-grid-unit tolerance wants far more than 64 chords"
    );
    let (ring, fired) = shape.rings_guarded(1, 1 << 20);
    assert!(ring[0][0].len() > 64 && !fired);
    let (_, fired) = shape.rings_guarded(u32::MAX, 8);
    assert!(
        !fired,
        "at a tolerance wider than the circle, eight chords are all it asks for"
    );
}

proptest! {
    // The two tests that decompose a full-perimeter polygon per case: a jagged ring of the
    // strategy's largest radius runs to ~700k boundary cells, and the descent is genuinely
    // linear in that (a second per shape unoptimised), so these run half the cases of the rest.
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn a_polygon_decomposes_to_exactly_its_direct_test(shape in rings_strategy(), seed in any::<u64>()) {
        let (shape, _) = shape.canonical(Space::View, &E).unwrap();
        let prepared = shape.prepared();
        let d = prepared.decompose(None);
        prop_assert!(!d.is_cover());
        let look = Lookup::new(&d);
        for p in probes(&shape, seed) {
            prop_assert_eq!(look.contains(&prepared, p), prepared.contains(p), "at {:?}", p);
        }
    }

    #[test]
    fn every_boundary_cells_corner_parity_is_the_full_ray_cast(shape in rings_strategy()) {
        let (shape, _) = shape.canonical(Space::View, &E).unwrap();
        let prepared = shape.prepared();
        let region = prepared.region().unwrap();
        let d = prepared.decompose(None);
        for b in &d.boundary {
            let r = Rect::of_cell(b.cell);
            // The context's parity is refined corner to corner down the descent; the full ray
            // cast from the cell's own corner must agree with it exactly.
            prop_assert_eq!(region.ray_parity((r.x0, r.y0)), b.ctx.parity, "cell {:?}", r);
        }
    }
}

#[test]
fn a_wkt_polygon_with_a_hole_excludes_the_hole_through_the_descent() {
    let rings = read_wkt(
        "POLYGON ((100000 100000, 900000 100000, 900000 900000, 100000 900000, 100000 100000), \
                  (400000 400000, 600000 400000, 600000 600000, 400000 600000, 400000 400000))",
    )
    .unwrap();
    let (shape, report) = ShapeF64::Polygon(rings).canonical(Space::View, &E).unwrap();
    assert_eq!(report.vertices_out, 8);
    let prepared = shape.prepared();
    let d = prepared.decompose(None);
    let look = Lookup::new(&d);
    let q = |v: f64| tessera_spatial::fixed32(v, 0.0, 1_000_000.0);
    assert!(look.contains(&prepared, (q(200_000.0), q(200_000.0))));
    assert!(!look.contains(&prepared, (q(500_000.0), q(500_000.0))));
    assert!(look.contains(&prepared, (q(400_000.0), q(500_000.0)))); // on the hole's edge
    assert!(!look.contains(&prepared, (q(50_000.0), q(500_000.0))));
    assert!(!d.interior.is_empty());
    // The boundary is the perimeter in depth-16 cells: an axis-aligned edge of length L cells
    // crosses at most L + 2 of them, and the eight edges sum to (4 × 0.8 + 4 × 0.2) × 65,536.
    let perimeter_cells = 4 * 65_536;
    assert!(
        d.boundary.len() <= perimeter_cells + 16,
        "{}",
        d.boundary.len()
    );
    // …and not the area, which is 0.8² × 2³² cells of the outer square alone.
    assert!(
        d.boundary.len() + d.interior.len() < 1 << 20,
        "{} + {}",
        d.boundary.len(),
        d.interior.len()
    );
}

#[test]
fn the_descent_is_linear_in_the_perimeter_not_the_area() {
    // A square 1/8 of the extent across and one 3/4 across: 6× the perimeter, 36× the area.
    let square = |half: f64| {
        let (c, h) = (500_000.0, half);
        ShapeF64::Polygon(vec![vec![vec![
            (c - h, c - h),
            (c + h, c - h),
            (c + h, c + h),
            (c - h, c + h),
        ]]])
    };
    let small = square(62_500.0)
        .canonical(Space::View, &E)
        .unwrap()
        .0
        .decompose(None);
    let large = square(375_000.0)
        .canonical(Space::View, &E)
        .unwrap()
        .0
        .decompose(None);
    let ratio = large.boundary.len() as f64 / small.boundary.len() as f64;
    assert!(
        (5.0..7.0).contains(&ratio),
        "boundary cells {} vs {}",
        large.boundary.len(),
        small.boundary.len()
    );
    // The interior tiles are the perimeter too, summed over the depths: a square whose edges do
    // not lie on tile boundaries is crossed by ≤ 4·(s·2^d + 2) tiles at depth d, where s is its
    // side as a fraction of the extent, and a tile crossed by one axis-aligned edge has at most
    // two children wholly inside. Interior tiles at every depth ≤ Σ_d 2·4·(s·2^d + 2) for
    // d < 16, which is 8·s·65,536 plus 128; the actual count is close to half that, one interior
    // child per crossing tile.
    let bound = |s: f64| (8.0 * s * 65_536.0) as usize + 128;
    assert!(
        small.interior.len() <= bound(0.125),
        "{}",
        small.interior.len()
    );
    assert!(
        large.interior.len() <= bound(0.75),
        "{}",
        large.interior.len()
    );
    let ratio = large.interior.len() as f64 / small.interior.len() as f64;
    assert!(
        (5.0..7.0).contains(&ratio),
        "interior tiles {} vs {}",
        large.interior.len(),
        small.interior.len()
    );
}
