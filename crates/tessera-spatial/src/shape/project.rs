//! A shape declared in longitude and latitude, placed in the view's frame
//! (`polygon-membership.md` §4.3 and R10, `projections.md` §10).
//!
//! **The space a shape is declared in defines the plane its edges are straight in** (R10). An edge
//! written in longitude and latitude is straight in the longitude/latitude plane, so its image in
//! the frame is a **curve**, and joining the projected endpoints with a straight chord would
//! select a different set of rows — 21.5 km of midpoint apart on an edge spanning the United
//! Kingdom, tens of cells. So an edge is **densified before it is projected**, to a tolerance of
//! one depth-16 cell, which bounds the departure to the grid's own resolution and is the error
//! R11 permits. A circle or an ellipse in longitude and latitude densifies to a polygon by the
//! same rule, a projected circle no longer being one.
//!
//! **The view's own declared transform, and no other.** [`Space::Wgs84`] carries the projection
//! the view's points went through, so a shape cannot be placed by a function the corpus was not —
//! the mismatch R12 exists to forbid. `projection = "none"` is a view with one space and nothing
//! to convert from, so it refuses.
//!
//! What leaves here is a [`ShapeF64`] in the frame's own coordinates, which
//! [`ShapeF64::canonical`] then clips, quantises and tidies exactly as it does a shape that
//! arrived in them.

use crate::morton::Bounds;
use crate::projection::Projection;

use super::canon::{CanonError, ShapeF64};

/// The space a shape's coordinates are written in, with the transform that reaches the view's.
///
/// The projection travels *inside* the `Wgs84` variant rather than beside it, so that placing a
/// shape without naming the function that placed the points is not a thing a caller can write.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Space {
    /// The view's own coordinates — the frame the points are quantised in, whatever produced it.
    View,
    /// Longitude and latitude in degrees, to be put through the view's own projection.
    Wgs84(Projection),
}

/// Cells per axis on the grid a position is stored against (contracts §2.5): 16 bits of cell.
const CELLS_PER_AXIS: f64 = 65_536.0;

/// **The densification tolerance is one depth-16 cell** (`polygon-membership.md` §4.3) — one cell
/// of the grid the view's own points are stored on, whatever zoom offset that view's frame sits
/// at, and not one cell of some fixed world grid. Expressed against the frame it is a constant
/// `1 / 65536` of the extent on each axis, which is what makes the bound the same statement about
/// resolution at every frame §4.1 permits.
pub const DENSIFY_TOLERANCE_CELLS: f64 = 1.0;

/// The criterion the subdivision actually stops at, in depth-16 cells.
///
/// Half the tolerance, because the test that decides whether to split *samples* the edge at
/// [`SAMPLES`] interior parameters rather than maximising the departure in closed form: the
/// sampled maximum is a lower bound on the true one, and stopping at half leaves the room that
/// gap needs. Measured worst true departure over the fixtures in `tests/shape_wgs84.rs` is well
/// inside one cell, which is the claim; the margin costs about 1.4× the vertices, edges being
/// subdivided until a quadratic departure falls below the criterion.
const CRITERION_CELLS: f64 = DENSIFY_TOLERANCE_CELLS / 2.0;

/// Interior parameters tested per candidate chord, at `k / SAMPLES` for `k` in `1..SAMPLES`.
///
/// **Not one midpoint.** An edge symmetric about the equator has its projected midpoint exactly
/// on the chord — Mercator's inflection is there — so a midpoint test would accept, at any
/// tolerance, an edge whose quarter points are kilometres off. Seven interior samples see both
/// lobes.
const SAMPLES: usize = 8;

/// How far an edge may be halved. At the cap an edge is 2^16 chords, which no edge of a shape
/// that survives the vertex cap reaches; a shape that did is refused by that cap, naming it.
const MAX_SUBDIVISIONS: u32 = 16;

/// Initial arcs a circle or an ellipse is cut into before subdivision — the eight the served ring
/// already takes as its floor (`Conic::ring_guarded`), so a curve is never approximated by fewer.
const CONIC_ARCS: usize = 8;

/// Place a shape in the frame `extent` is written in.
///
/// [`Space::View`] is the identity — the shape is already there. [`Space::Wgs84`] projects, having
/// first refused what is not a coordinate: a longitude outside ±180 or a latitude outside ±90
/// (`projections.md` §2), and a view with no projection, which has one space and nothing to
/// convert from.
///
/// A **box** in longitude and latitude comes back a box: every projection in the set is
/// cylindrical, so a meridian is a vertical line in the frame and a parallel a horizontal one, and
/// the two readings of its four edges agree exactly. Everything else densifies.
pub fn place(shape: &ShapeF64, space: Space, extent: &Bounds) -> Result<ShapeF64, CanonError> {
    let projection = match space {
        Space::View => return Ok(shape.clone()),
        Space::Wgs84(Projection::None) => return Err(CanonError::NoProjection),
        Space::Wgs84(p) => p,
    };
    check_degrees(shape)?;
    // Frame units per depth-16 cell, per axis. An aligned square is square, so the two agree for
    // every frame a projected view can take; taking them separately costs nothing and keeps the
    // bound a statement about the grid rather than about the frame's shape.
    let scale = (
        CELLS_PER_AXIS / (extent.x_max - extent.x_min),
        CELLS_PER_AXIS / (extent.y_max - extent.y_min),
    );
    Ok(match shape {
        ShapeF64::Bbox {
            min_x,
            min_y,
            max_x,
            max_y,
        } => {
            // y runs south, so the box's minimum latitude is its maximum y (`projections.md` §4).
            let (x_min, y_max) = projection.forward(*min_x, *min_y);
            let (x_max, y_min) = projection.forward(*max_x, *max_y);
            ShapeF64::Bbox {
                min_x: x_min,
                min_y: y_min,
                max_x: x_max,
                max_y: y_max,
            }
        }
        ShapeF64::Circle { cx, cy, r } => {
            ShapeF64::Polygon(vec![vec![conic_ring(*cx, *cy, *r, *r, 0.0, projection, scale)]])
        }
        ShapeF64::Ellipse {
            cx,
            cy,
            a,
            b,
            angle_degrees,
        } => ShapeF64::Polygon(vec![vec![conic_ring(
            *cx,
            *cy,
            *a,
            *b,
            angle_degrees.to_radians(),
            projection,
            scale,
        )]]),
        ShapeF64::Polygon(parts) => ShapeF64::Polygon(
            parts
                .iter()
                .map(|part| {
                    part.iter()
                        .map(|ring| project_ring(ring, projection, scale))
                        .collect()
                })
                .collect(),
        ),
    })
}

/// Refuse anything that is not a longitude and a latitude (`projections.md` §2), and the two
/// degeneracies a closed form is refused for.
///
/// The coordinate check is over the declared parameters rather than over the generated boundary:
/// every point either kind generates lies inside the box checked here, so one pass at the front
/// is the whole check. The degeneracies are checked here too because a curve leaves this module
/// as a polygon, and [`ShapeF64::canonical`]'s own refusals for them would no longer be reached.
fn check_degrees(shape: &ShapeF64) -> Result<(), CanonError> {
    let ok = |lon: f64, lat: f64| -> Result<(), CanonError> {
        if !lon.is_finite() || !lat.is_finite() {
            return Err(CanonError::NotFinite);
        }
        if lon.abs() > 180.0 || lat.abs() > 90.0 {
            return Err(CanonError::NotACoordinate);
        }
        Ok(())
    };
    match shape {
        ShapeF64::Bbox {
            min_x,
            min_y,
            max_x,
            max_y,
        } => {
            ok(*min_x, *min_y)?;
            ok(*max_x, *max_y)?;
        }
        ShapeF64::Circle { cx, cy, r } => {
            ok(*cx, *cy)?;
            ok(cx - r.abs(), cy - r.abs())?;
            ok(cx + r.abs(), cy + r.abs())?;
        }
        ShapeF64::Ellipse {
            cx,
            cy,
            a,
            b,
            angle_degrees,
        } => {
            ok(*cx, *cy)?;
            // The rotated ellipse's own half-extents, not the larger axis on both: a shape near a
            // pole should be refused for where it reaches, not for how it is written.
            let (s, c) = angle_degrees.to_radians().sin_cos();
            let hw = ((a * c).powi(2) + (b * s).powi(2)).sqrt();
            let hh = ((a * s).powi(2) + (b * c).powi(2)).sqrt();
            ok(cx - hw, cy - hh)?;
            ok(cx + hw, cy + hh)?;
        }
        ShapeF64::Polygon(parts) => {
            for &(lon, lat) in parts.iter().flatten().flatten() {
                ok(lon, lat)?;
            }
        }
    }
    // Every coordinate is finite by here, so these comparisons are total.
    match shape {
        ShapeF64::Bbox {
            min_x,
            min_y,
            max_x,
            max_y,
        } if max_x < min_x || max_y < min_y => Err(CanonError::InvertedBox),
        ShapeF64::Circle { r, .. } if *r <= 0.0 => Err(CanonError::NonPositiveAxis),
        ShapeF64::Ellipse { a, b, .. } if *a <= 0.0 || *b <= 0.0 => {
            Err(CanonError::NonPositiveAxis)
        }
        _ => Ok(()),
    }
}

/// One ring's vertices, each edge densified and projected.
fn project_ring(
    ring: &[(f64, f64)],
    projection: Projection,
    scale: (f64, f64),
) -> Vec<(f64, f64)> {
    let mut pts: Vec<(f64, f64)> = ring.to_vec();
    if pts.len() > 1 && pts.first() == pts.last() {
        pts.pop();
    }
    if pts.is_empty() {
        return Vec::new();
    }
    let n = pts.len();
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let a = pts[i];
        let b = pts[(i + 1) % n];
        let curve = |t: f64| (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t);
        out.push(projection.forward(a.0, a.1));
        // The edge's interior only: the next edge contributes its own start.
        let mut interior = Vec::new();
        densify(
            &curve,
            projection,
            0.0,
            1.0,
            scale,
            0,
            &mut interior,
        );
        interior.pop();
        out.append(&mut interior);
    }
    out
}

/// A circle or an ellipse in longitude and latitude, as a projected ring.
fn conic_ring(
    cx: f64,
    cy: f64,
    a: f64,
    b: f64,
    angle: f64,
    projection: Projection,
    scale: (f64, f64),
) -> Vec<(f64, f64)> {
    let (s, c) = angle.sin_cos();
    let curve = |theta: f64| {
        let (u, v) = (a * theta.cos(), b * theta.sin());
        (cx + c * u - s * v, cy + s * u + c * v)
    };
    let mut out = Vec::new();
    for k in 0..CONIC_ARCS {
        let t0 = std::f64::consts::TAU * k as f64 / CONIC_ARCS as f64;
        let t1 = std::f64::consts::TAU * (k + 1) as f64 / CONIC_ARCS as f64;
        let (lon, lat) = curve(t0);
        out.push(projection.forward(lon, lat));
        let mut arc = Vec::new();
        densify(&curve, projection, t0, t1, scale, 0, &mut arc);
        arc.pop();
        out.append(&mut arc);
    }
    out
}

/// The projected image of `curve` over `[t0, t1]`, as frame points **after** `t0` and including
/// `t1`, subdivided until no sampled point of the true image departs from the chord by more than
/// [`CRITERION_CELLS`].
///
/// The departure is measured **to the chord segment** and not between points at equal parameter:
/// a meridian's image is the chord — the same set of frame points, walked at a different rate —
/// and a parameter-wise measure would densify it for a difference no membership can see. That is
/// what makes a meridional and an equatorial edge come back untouched.
fn densify(
    curve: &dyn Fn(f64) -> (f64, f64),
    projection: Projection,
    t0: f64,
    t1: f64,
    scale: (f64, f64),
    depth: u32,
    out: &mut Vec<(f64, f64)>,
) {
    let project_at = |t: f64| {
        let (lon, lat) = curve(t);
        projection.forward(lon, lat)
    };
    let p0 = project_at(t0);
    let p1 = project_at(t1);
    if depth < MAX_SUBDIVISIONS {
        let mut worst = 0.0f64;
        for k in 1..SAMPLES {
            let t = t0 + (t1 - t0) * (k as f64 / SAMPLES as f64);
            worst = worst.max(departure(project_at(t), p0, p1, scale));
        }
        if worst > CRITERION_CELLS {
            let mid = 0.5 * (t0 + t1);
            densify(curve, projection, t0, mid, scale, depth + 1, out);
            densify(curve, projection, mid, t1, scale, depth + 1, out);
            return;
        }
    }
    out.push(p1);
}

/// Distance from a frame point to the chord `a`–`b`, in depth-16 cells.
fn departure(p: (f64, f64), a: (f64, f64), b: (f64, f64), scale: (f64, f64)) -> f64 {
    let (px, py) = ((p.0 - a.0) * scale.0, (p.1 - a.1) * scale.1);
    let (bx, by) = ((b.0 - a.0) * scale.0, (b.1 - a.1) * scale.1);
    let len2 = bx * bx + by * by;
    if len2 == 0.0 {
        return (px * px + py * py).sqrt();
    }
    let t = ((px * bx + py * by) / len2).clamp(0.0, 1.0);
    let (dx, dy) = (px - t * bx, py - t * by);
    (dx * dx + dy * dy).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole Web Mercator world — the frame the United Kingdom takes, straddling the meridian
    /// (`projections.md` §4.1).
    const WORLD: Bounds = Bounds {
        x_min: 0.0,
        x_max: 1.0,
        y_min: 0.0,
        y_max: 1.0,
    };

    fn ring(shape: &ShapeF64) -> Vec<(f64, f64)> {
        match shape {
            ShapeF64::Polygon(parts) => parts[0][0].clone(),
            other => panic!("not a polygon: {other:?}"),
        }
    }

    #[test]
    fn a_view_shape_is_untouched() {
        let p = ShapeF64::Polygon(vec![vec![vec![(0.1, 0.2), (0.3, 0.4), (0.5, 0.1)]]]);
        assert_eq!(place(&p, Space::View, &WORLD).unwrap(), p);
    }

    #[test]
    fn a_view_with_no_projection_has_no_second_space() {
        let p = ShapeF64::Polygon(vec![vec![vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0)]]]);
        assert_eq!(
            place(&p, Space::Wgs84(Projection::None), &WORLD),
            Err(CanonError::NoProjection)
        );
    }

    #[test]
    fn what_is_not_a_coordinate_refuses() {
        let space = Space::Wgs84(Projection::WebMercator);
        for bad in [
            ShapeF64::Polygon(vec![vec![vec![(0.0, 0.0), (181.0, 0.0), (1.0, 1.0)]]]),
            ShapeF64::Polygon(vec![vec![vec![(0.0, 0.0), (1.0, 90.5), (1.0, 1.0)]]]),
            ShapeF64::Bbox {
                min_x: -190.0,
                min_y: 0.0,
                max_x: 1.0,
                max_y: 1.0,
            },
            ShapeF64::Circle {
                cx: 0.0,
                cy: 89.0,
                r: 2.0,
            },
        ] {
            assert_eq!(
                place(&bad, space, &WORLD),
                Err(CanonError::NotACoordinate),
                "{bad:?}"
            );
        }
        // The boundary itself is a coordinate.
        assert!(place(
            &ShapeF64::Bbox {
                min_x: -180.0,
                min_y: -90.0,
                max_x: 180.0,
                max_y: 90.0
            },
            space,
            &WORLD
        )
        .is_ok());
    }

    /// A box in degrees is a box in the frame: a meridian is vertical and a parallel horizontal
    /// under every projection in the set.
    #[test]
    fn a_degree_box_projects_to_a_box() {
        let uk = ShapeF64::Bbox {
            min_x: -8.0,
            min_y: 49.9,
            max_x: 2.0,
            max_y: 60.9,
        };
        let placed = place(&uk, Space::Wgs84(Projection::WebMercator), &WORLD).unwrap();
        let ShapeF64::Bbox {
            min_x,
            min_y,
            max_x,
            max_y,
        } = placed
        else {
            panic!("a box stayed a box");
        };
        let wm = Projection::WebMercator;
        assert_eq!((min_x, max_y), wm.forward(-8.0, 49.9));
        assert_eq!((max_x, min_y), wm.forward(2.0, 60.9));
        assert!(min_x < max_x && min_y < max_y);
    }

    /// A meridional edge and an equatorial edge are straight in both planes, so densification
    /// leaves them alone. These are the cases where the two readings of R10 agree.
    #[test]
    fn a_meridian_and_a_parallel_are_not_densified() {
        let space = Space::Wgs84(Projection::WebMercator);
        let meridian = ShapeF64::Polygon(vec![vec![vec![
            (0.0, -60.0),
            (0.0, 60.0),
            (0.0001, 60.0),
        ]]]);
        // Three vertices in, three out: the long meridional edge gained nothing.
        assert_eq!(ring(&place(&meridian, space, &WORLD).unwrap()).len(), 3);
        let equator = ShapeF64::Polygon(vec![vec![vec![
            (-170.0, 0.0),
            (170.0, 0.0),
            (170.0, 0.0001),
        ]]]);
        assert_eq!(ring(&place(&equator, space, &WORLD).unwrap()).len(), 3);
        // Equirectangular is linear on both axes, so nothing is ever densified under it.
        let diagonal = ShapeF64::Polygon(vec![vec![vec![
            (-8.0, 50.0),
            (2.0, 58.0),
            (2.0, 50.0),
        ]]]);
        assert_eq!(
            ring(&place(&diagonal, Space::Wgs84(Projection::PLATE_CARREE), &WORLD).unwrap()).len(),
            3
        );
    }

    /// **The tolerance holds**: no point of the true projected image departs from the densified
    /// boundary by more than one depth-16 cell, sampled far finer than the subdivision test does.
    #[test]
    fn the_densified_boundary_holds_the_true_image_to_one_cell() {
        let wm = Projection::WebMercator;
        let edges = [
            // The United Kingdom's diagonal — 21.5 km of midpoint between the two readings.
            ((-8.0, 50.0), (2.0, 58.0)),
            // Sixty degrees on both axes: 586 km.
            ((-30.0, -30.0), (30.0, 30.0)),
            // Symmetric about the equator, where the projected midpoint is *on* the chord.
            ((-40.0, -55.0), (40.0, 55.0)),
            // Near the domain's cut, where the transform steepens fastest.
            ((-20.0, 70.0), (20.0, 84.9)),
        ];
        let mut worst_overall = 0.0f64;
        for (a, b) in edges {
            let shape =
                ShapeF64::Polygon(vec![vec![vec![a, b, (b.0, a.1)]]]);
            let placed = place(&shape, Space::Wgs84(wm), &WORLD).unwrap();
            let chain = ring(&placed);
            // The densified image of the first edge: from a's projection up to b's.
            let end = chain
                .iter()
                .position(|&p| p == wm.forward(b.0, b.1))
                .expect("b's projection is a vertex");
            let dense = &chain[..=end];
            let mut worst = 0.0f64;
            for k in 0..=20_000 {
                let t = k as f64 / 20_000.0;
                let p = wm.forward(a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t);
                let mut best = f64::INFINITY;
                for w in dense.windows(2) {
                    best = best.min(departure(
                        p,
                        w[0],
                        w[1],
                        (CELLS_PER_AXIS, CELLS_PER_AXIS),
                    ));
                }
                worst = worst.max(best);
            }
            assert!(
                worst <= DENSIFY_TOLERANCE_CELLS,
                "{a:?}→{b:?} departs by {worst} depth-16 cells, over {} chords",
                dense.len() - 1
            );
            worst_overall = worst_overall.max(worst);
        }
        // Measured, not assumed: the worst departure over these four edges is far inside the
        // stated cell, which is the room the sampled stopping criterion needs.
        assert!(worst_overall < 0.5, "worst departure {worst_overall} cells");
    }

    /// A circle in degrees is not a circle in the frame, and comes back a polygon that holds the
    /// true projected oval to the same tolerance.
    #[test]
    fn a_degree_circle_densifies_to_a_polygon() {
        let wm = Projection::WebMercator;
        let circle = ShapeF64::Circle {
            cx: 0.0,
            cy: 60.0,
            r: 20.0,
        };
        let chain = ring(&place(&circle, Space::Wgs84(wm), &WORLD).unwrap());
        assert!(chain.len() >= CONIC_ARCS, "at least the eight arcs");
        let mut worst = 0.0f64;
        for k in 0..20_000 {
            let theta = std::f64::consts::TAU * k as f64 / 20_000.0;
            let p = wm.forward(20.0 * theta.cos(), 60.0 + 20.0 * theta.sin());
            let mut best = f64::INFINITY;
            for i in 0..chain.len() {
                best = best.min(departure(
                    p,
                    chain[i],
                    chain[(i + 1) % chain.len()],
                    (CELLS_PER_AXIS, CELLS_PER_AXIS),
                ));
            }
            worst = worst.max(best);
        }
        assert!(worst <= DENSIFY_TOLERANCE_CELLS, "{worst} cells");
    }

    /// The tolerance is the *view's* grid, so a sub-square frame densifies further for the same
    /// edge — a fixed world-grid tolerance would leave a zoom-offset frame with 2^k cells of
    /// error and break R11.
    #[test]
    fn a_finer_frame_densifies_further() {
        let space = Space::Wgs84(Projection::WebMercator);
        let edge = ShapeF64::Polygon(vec![vec![vec![(0.5, 51.0), (1.5, 52.0), (1.5, 51.0)]]]);
        let world = ring(&place(&edge, space, &WORLD).unwrap()).len();
        // The tile at z6 containing Greater London's east — a frame 2^6 finer per axis.
        let sub = Bounds {
            x_min: 0.5,
            x_max: 0.515_625,
            y_min: 0.32,
            y_max: 0.335_625,
        };
        let finer = ring(&place(&edge, space, &sub).unwrap()).len();
        assert!(
            finer > world,
            "a 2^6 finer frame produced {finer} vertices against the world's {world}"
        );
    }
}
