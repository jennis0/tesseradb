//! **A shape declared in longitude and latitude holds what its *curved* projected image holds**
//! (`polygon-membership.md` §4.3 and R10, `projections.md` §10).
//!
//! The space a polygon is declared in defines the plane its edges are straight in. An edge written
//! in degrees is straight in the longitude/latitude plane, so its image in a Web Mercator frame is
//! a curve — and the straight chord between the two projected endpoints is a *different boundary*,
//! selecting different rows. On the United Kingdom's diagonal, from 8°W 50°N to 2°E 58°N, the two
//! are 60 depth-16 cells apart at the midpoint (21.5 km on the ground), so the rows that
//! discriminate exist and can be named.
//!
//! **These tests would pass under either semantics if they compared a densified shape with itself.**
//! What they compare is the shape this module produces against *the shape the chord reading
//! produces* — the vertices projected and joined with straight lines, built here by hand — at
//! positions that lie between the two boundaries. Under the chord reading the two shapes are equal
//! and every one of these fails.

use tessera_spatial::morton::{fixed32, Bounds};
use tessera_spatial::shape::{Shape, ShapeF64, Space, DENSIFY_TOLERANCE_CELLS};
use tessera_spatial::Projection;

/// The whole Web Mercator world: the frame the United Kingdom takes, straddling as it does both
/// the prime meridian and no sub-square below it (`projections.md` §4.1).
const WORLD: Bounds = Bounds {
    x_min: 0.0,
    x_max: 1.0,
    y_min: 0.0,
    y_max: 1.0,
};

const WM: Projection = Projection::WebMercator;

/// The diagonal edge, in degrees. Its two readings are 21.5 km apart at the midpoint.
const A: (f64, f64) = (-8.0, 50.0);
const B: (f64, f64) = (2.0, 58.0);
/// The third vertex, north-west of the diagonal: the two edges that reach it are a meridian and a
/// parallel, which are straight in both planes, so the diagonal is the only edge that can differ.
const C: (f64, f64) = (-8.0, 58.0);

/// A place on the Earth, as the grid position the view's own transform puts it at.
fn place(lon: f64, lat: f64, extent: &Bounds) -> (u32, u32) {
    let (x, y) = WM.forward(lon, lat);
    (
        fixed32(x, extent.x_min, extent.x_max),
        fixed32(y, extent.y_min, extent.y_max),
    )
}

/// The triangle as this design reads it: declared in degrees, densified, then projected.
fn curved(extent: &Bounds) -> Shape {
    ShapeF64::Polygon(vec![vec![vec![A, B, C]]])
        .canonical(Space::Wgs84(WM), extent)
        .expect("a well-formed triangle in degrees")
        .0
}

/// The triangle as the reading the owner ruled against would produce: the three vertices put
/// through the transform and joined with straight lines in the frame.
///
/// Built here rather than taken from the module, so that a module which joined with chords would
/// produce a shape *equal* to this one and every assertion below would fail.
fn chorded(extent: &Bounds) -> Shape {
    let ring: Vec<(f64, f64)> = [A, B, C]
        .iter()
        .map(|&(lon, lat)| WM.forward(lon, lat))
        .collect();
    ShapeF64::Polygon(vec![vec![ring]])
        .canonical(Space::View, extent)
        .expect("a well-formed triangle in frame coordinates")
        .0
}

/// **The test that proves the phase.** Two named places lie between the curved edge and its chord,
/// and the two readings disagree about both.
///
/// At 3°W the diagonal's own latitude is 54.000°; the chord between the projected endpoints
/// crosses that meridian at 54.193°. Everything in between is inside the triangle under the
/// declared semantics — the triangle lies north-west of the diagonal — and outside it under the
/// chord reading, which has drawn its boundary 60 cells too far north.
#[test]
fn the_rows_between_the_curve_and_its_chord_belong_to_the_curve() {
    let curved = curved(&WORLD);
    let chorded = chorded(&WORLD);

    // Inside the true image, outside the chord's: the discriminating rows.
    for (lon, lat) in [(-3.0, 54.05), (-3.0, 54.15), (-1.0, 55.68), (-6.0, 51.66)] {
        let p = place(lon, lat, &WORLD);
        assert!(
            curved.contains(p),
            "({lon}, {lat}) is inside the edge's own image and the shape does not hold it"
        );
        assert!(
            !chorded.contains(p),
            "({lon}, {lat}) is inside the chord's image too — the edge was joined by a chord"
        );
    }

    // Well north of both boundaries, and well south of both: the two readings agree here, which is
    // what makes the disagreement above about the boundary and not about the shape.
    for (lon, lat) in [(-3.0, 55.5), (-6.0, 56.0), (0.0, 57.5)] {
        let p = place(lon, lat, &WORLD);
        assert!(curved.contains(p) && chorded.contains(p), "({lon}, {lat})");
    }
    for (lon, lat) in [(-3.0, 52.0), (-6.0, 50.5), (0.0, 54.0)] {
        let p = place(lon, lat, &WORLD);
        assert!(!curved.contains(p) && !chorded.contains(p), "({lon}, {lat})");
    }
}

/// The two shapes are not the same shape, and the difference is the band between the boundaries.
///
/// Swept along the diagonal rather than at the four named places, so the count is a measurement
/// rather than an anecdote: of 901 probes on a 0.01° grid between the two boundaries, every one
/// belongs to the curve and none to the chord.
#[test]
fn the_band_between_the_two_readings_is_tens_of_cells_wide_the_whole_way() {
    let curved = curved(&WORLD);
    let chorded = chorded(&WORLD);
    let mut probes = 0;
    let mut widest_cells = 0.0f64;
    // Every hundredth of a degree of longitude across the edge, away from the two ends where the
    // readings meet and the band closes.
    for k in 0..=900 {
        let lon = -7.5 + f64::from(k) * 0.01;
        let t = (lon - A.0) / (B.0 - A.0);
        let edge_lat = A.1 + (B.1 - A.1) * t;
        // Where the chord crosses this meridian, back in degrees.
        let (ax, ay) = WM.forward(A.0, A.1);
        let (bx, by) = WM.forward(B.0, B.1);
        let x = ax + (bx - ax) * t;
        let chord_y = ay + (by - ay) * t;
        let (_, chord_lat) = WM.inverse(x, chord_y);
        let (_, edge_y) = WM.forward(lon, edge_lat);
        widest_cells = widest_cells.max((edge_y - chord_y).abs() * 65_536.0);
        // Halfway up the band: inside the curve's triangle, outside the chord's.
        let p = place(lon, 0.5 * (edge_lat + chord_lat), &WORLD);
        assert!(curved.contains(p), "at {lon}° the curve lost a row");
        assert!(!chorded.contains(p), "at {lon}° the chord gained one");
        probes += 1;
    }
    assert_eq!(probes, 901);
    assert!(
        widest_cells > 55.0,
        "the two readings are only {widest_cells} cells apart at their widest"
    );
}

/// **The tolerance holds**: no point of the true projected image departs from the densified
/// boundary by more than one depth-16 cell — measured on the *canonical* shape, which is what
/// membership is tested against, and at 10,001 points per edge rather than the eight the
/// subdivision test samples.
///
/// Run at the world frame and at a zoom-offset 6 sub-square, because the tolerance is one cell of
/// the *view's* grid: a fixed world-grid tolerance would leave the finer frame 2^6 cells of error
/// and break R11. **Measured** worst departure over this fixture: 0.493 cells at the world frame
/// over 30,003 sampled points, 0.307 at the sub-square over 26,003 — the room the subdivision's
/// own sampled stopping criterion leaves, and half the bound this asserts.
#[test]
fn no_point_of_the_true_image_leaves_the_densified_boundary_by_a_cell() {
    // The tile at zoom offset 6 whose square contains the edge's eastern half.
    // A square inside the triangle, so the shape is not clipped away before it can be measured.
    let sub = tessera_spatial::snap_outward(&Bounds {
        x_min: WM.forward(-5.5, 56.0).0,
        x_max: WM.forward(-4.5, 56.0).0,
        y_min: WM.forward(-5.0, 56.5).1,
        y_max: WM.forward(-5.0, 55.5).1,
    })
    .square
    .bounds();

    for extent in [WORLD, sub] {
        let Shape::Polygon(polygon) = curved(&extent) else {
            panic!("a polygon");
        };
        let ring: Vec<(u32, u32)> = polygon.parts[0].rings[0]
            .vertices
            .iter()
            .map(|v| (v.x, v.y))
            .collect();
        let cell_x = (extent.x_max - extent.x_min) / 65_536.0;
        let cell_y = (extent.y_max - extent.y_min) / 65_536.0;
        let mut worst = 0.0f64;
        let mut measured = 0u32;
        for (from, to) in [(A, B), (B, C), (C, A)] {
            for k in 0..=10_000 {
                let t = f64::from(k) / 10_000.0;
                let (lon, lat) = (
                    from.0 + (to.0 - from.0) * t,
                    from.1 + (to.1 - from.1) * t,
                );
                let (x, y) = WM.forward(lon, lat);
                // Only the part of the image the frame holds: a sub-square clips the shape, and
                // the boundary outside the frame is the frame's edge rather than the edge's image.
                if x < extent.x_min || x > extent.x_max || y < extent.y_min || y > extent.y_max {
                    continue;
                }
                measured += 1;
                // In grid units, where one depth-16 cell is 65,536 whatever the frame.
                let p = (
                    (x - extent.x_min) / cell_x * 65_536.0,
                    (y - extent.y_min) / cell_y * 65_536.0,
                );
                let mut best = f64::INFINITY;
                for i in 0..ring.len() {
                    let a = ring[i];
                    let b = ring[(i + 1) % ring.len()];
                    best = best.min(to_segment(
                        p,
                        (f64::from(a.0), f64::from(a.1)),
                        (f64::from(b.0), f64::from(b.1)),
                    ));
                }
                worst = worst.max(best);
            }
        }
        // In depth-16 cells: 65,536 grid units to a cell.
        let cells = worst / 65_536.0;
        assert!(
            cells <= DENSIFY_TOLERANCE_CELLS,
            "the true image leaves the boundary by {cells} depth-16 cells on the frame \
             [{}, {}] x [{}, {}]",
            extent.x_min,
            extent.x_max,
            extent.y_min,
            extent.y_max
        );
        assert!(
            measured > 1_000,
            "only {measured} points of the image fell inside the frame — the fixture measures \
             nothing"
        );
    }
}

/// **A meridian and a parallel are unchanged by densification**, being straight in both planes.
///
/// These are the cases where the two readings of R10 agree, and they anchor the ones where they do
/// not: a module that densified everything would still pass the tests above and fail these.
#[test]
fn a_meridional_and_an_equatorial_edge_come_back_as_they_went_in() {
    // A meridian from 60°S to 60°N and a parallel along the equator, closed by a hair's-breadth
    // third vertex so each is a ring rather than a segment.
    let meridian = ShapeF64::Polygon(vec![vec![vec![
        (0.0, -60.0),
        (0.0, 60.0),
        (0.001, 60.0),
    ]]]);
    let equator = ShapeF64::Polygon(vec![vec![vec![
        (-170.0, 0.0),
        (170.0, 0.0),
        (170.0, 0.001),
    ]]]);
    for shape in [meridian, equator] {
        let placed = shape
            .canonical(Space::Wgs84(WM), &WORLD)
            .expect("well-formed")
            .0;
        assert_eq!(
            placed.vertex_count(),
            3,
            "an edge straight in both planes gained vertices"
        );
    }
    // Equirectangular is linear on both axes, so no edge of any shape is ever densified under it.
    let diagonal = ShapeF64::Polygon(vec![vec![vec![A, B, C]]]);
    assert_eq!(
        diagonal
            .canonical(Space::Wgs84(Projection::PLATE_CARREE), &WORLD)
            .expect("well-formed")
            .0
            .vertex_count(),
        3
    );
    // And under Web Mercator the same diagonal is not three vertices, which is the whole point.
    assert!(diagonal
        .canonical(Space::Wgs84(WM), &WORLD)
        .expect("well-formed")
        .0
        .vertex_count()
        > 3);
}

/// The report says what the caller wrote, so the build's `vertices in → out` line shows what
/// densification cost rather than hiding it (`polygon-membership.md` §6.5).
#[test]
fn the_report_counts_the_declared_vertices_not_the_densified_ones() {
    let (shape, report) = ShapeF64::Polygon(vec![vec![vec![A, B, C]]])
        .canonical(Space::Wgs84(WM), &WORLD)
        .expect("well-formed");
    assert_eq!(report.vertices_in, 3);
    assert_eq!(report.vertices_out, shape.vertex_count());
    assert!(report.vertices_out > report.vertices_in);
    // A closed form has parameters rather than vertices, densified or not.
    let (_, report) = ShapeF64::Circle {
        cx: -3.0,
        cy: 54.0,
        r: 2.0,
    }
    .canonical(Space::Wgs84(WM), &WORLD)
    .expect("well-formed");
    assert_eq!(report.vertices_in, 0);
}

/// A `wgs84` coordinate outside ±180 × ±90 is not a coordinate, and a view that projects nothing
/// has one space (`projections.md` §2, §5.3).
#[test]
fn what_a_view_cannot_honour_refuses() {
    use tessera_spatial::shape::CanonError;
    let over = ShapeF64::Polygon(vec![vec![vec![(-8.0, 50.0), (2.0, 90.5), (-8.0, 58.0)]]]);
    assert_eq!(
        over.canonical(Space::Wgs84(WM), &WORLD),
        Err(CanonError::NotACoordinate)
    );
    let wrapped = ShapeF64::Bbox {
        min_x: 170.0,
        min_y: 0.0,
        max_x: 190.0,
        max_y: 10.0,
    };
    assert_eq!(
        wrapped.canonical(Space::Wgs84(WM), &WORLD),
        Err(CanonError::NotACoordinate)
    );
    // The domain's own boundary is a coordinate; the pole is inside ±90 and is clipped, not refused.
    assert!(ShapeF64::Bbox {
        min_x: -180.0,
        min_y: -90.0,
        max_x: 180.0,
        max_y: 90.0
    }
    .canonical(Space::Wgs84(WM), &WORLD)
    .is_ok());
    // A view with one space refuses the second, whatever the shape.
    assert_eq!(
        ShapeF64::Polygon(vec![vec![vec![A, B, C]]])
            .canonical(Space::Wgs84(Projection::None), &WORLD),
        Err(CanonError::NoProjection)
    );
}

/// Distance from `p` to the segment `a`–`b`, in the units all three are given in.
fn to_segment(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    let (px, py) = (p.0 - a.0, p.1 - a.1);
    let (bx, by) = (b.0 - a.0, b.1 - a.1);
    let len2 = bx * bx + by * by;
    if len2 == 0.0 {
        return (px * px + py * py).sqrt();
    }
    let t = ((px * bx + py * by) / len2).clamp(0.0, 1.0);
    let (dx, dy) = (px - t * bx, py - t * by);
    (dx * dx + dy * dy).sqrt()
}
