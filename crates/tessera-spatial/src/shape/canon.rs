//! From what a caller wrote to what the grid holds (`polygon-membership.md` §4.3–§4.4).
//!
//! A shape arrives in the space its submission declared — the view's own coordinates, or
//! longitude and latitude, which [`super::project`] places in the view's frame through the
//! view's own transform before anything here looks at it — and
//! leaves as a [`Shape`] in grid units: clipped to the extent, quantised through `fixed32`,
//! and for a polygon deduplicated, de-collinearised, oriented, rotated to a fixed start and
//! weighted. Everything that happened on the way is in the [`CanonReport`], which the build
//! prints and never refuses on: a clipped boundary and a ring that collapsed are facts about the
//! caller's data the caller can see, not disclosures (`CLAUDE.md`, *what the strictness is for*).
//! What does refuse is a coordinate that is not one.

use crate::morton::{fixed32, Bounds, FIXED_SPAN};

use super::conic::Conic;
use super::project::{place, Space};
use super::polygon::{Part, Polygon, Ring};
use super::simplify::weight_ring;
use super::{Bbox, Shape};

/// Parts → rings → `(x, y)`, in data units, as read from WKB or WKT.
pub type RingsF64 = Vec<Vec<Vec<(f64, f64)>>>;

/// A shape as a caller declares it, in data units.
#[derive(Debug, Clone, PartialEq)]
pub enum ShapeF64 {
    Bbox {
        min_x: f64,
        min_y: f64,
        max_x: f64,
        max_y: f64,
    },
    Circle {
        cx: f64,
        cy: f64,
        r: f64,
    },
    Ellipse {
        cx: f64,
        cy: f64,
        a: f64,
        b: f64,
        angle_degrees: f64,
    },
    Polygon(RingsF64),
}

/// What canonicalisation did, for the build report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CanonReport {
    /// Some of the shape lay outside the extent and was cut away.
    pub clipped: bool,
    /// The whole shape lay outside the extent: it holds nothing.
    pub outside: bool,
    /// Rings that collapsed below three distinct, non-collinear vertices after quantisation.
    pub rings_dropped: u32,
    pub vertices_in: u64,
    pub vertices_out: u64,
    /// Every coordinate lay within ±180 × ±90 while the extent does not: the shape may have been
    /// written in degrees for a view that is not (R12).
    pub degrees_looking: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanonError {
    NotFinite,
    /// A box whose max is below its min on an axis.
    InvertedBox,
    NonPositiveAxis,
    /// A `wgs84` coordinate outside ±180 × ±90, which is not a coordinate (`projections.md` §2).
    NotACoordinate,
    /// A `wgs84` shape on a view that projects nothing: one space, and nothing to convert from
    /// (`polygon-membership.md` §4.3).
    NoProjection,
}

impl std::fmt::Display for CanonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CanonError::NotFinite => write!(f, "a shape coordinate is not finite"),
            CanonError::InvertedBox => write!(f, "a box's max is below its min on an axis"),
            CanonError::NonPositiveAxis => {
                write!(f, "a circle's radius or an ellipse's axis is not positive")
            }
            CanonError::NotACoordinate => write!(
                f,
                "a `wgs84` coordinate is outside ±180 longitude or ±90 latitude, and a value \
                 outside that is not a coordinate (projections.md §2)"
            ),
            CanonError::NoProjection => write!(
                f,
                "`space = \"wgs84\"` on a view whose `projection` is `none`: such a view has one \
                 space and nothing to convert a degree from. Write the shape in the view's own \
                 coordinates with `space = \"view\"`, or declare a projection on the view \
                 (projections.md §5.3)"
            ),
        }
    }
}

impl std::error::Error for CanonError {}

impl ShapeF64 {
    /// Every coordinate the shape carries, for the finiteness and degrees checks.
    fn coordinates(&self) -> Vec<(f64, f64)> {
        match self {
            ShapeF64::Bbox {
                min_x,
                min_y,
                max_x,
                max_y,
            } => vec![(*min_x, *min_y), (*max_x, *max_y)],
            ShapeF64::Circle { cx, cy, r } => vec![(*cx, *cy), (cx + r, cy + r), (cx - r, cy - r)],
            ShapeF64::Ellipse { cx, cy, a, b, .. } => {
                let e = a.max(*b);
                vec![(*cx, *cy), (cx + e, cy + e), (cx - e, cy - e)]
            }
            ShapeF64::Polygon(parts) => parts.iter().flatten().flatten().copied().collect(),
        }
    }

    /// The canonical shape over `extent`, with the report. The extent must be valid
    /// (`Bounds::validate`).
    ///
    /// **`space` names the plane the shape's edges are straight in** (`polygon-membership.md`
    /// R10), and carries with it the transform that reaches the view's frame. A
    /// [`Space::Wgs84`] shape is densified and projected first ([`super::project::place`]), so
    /// that everything below this line is working in the coordinates the points are stored in and
    /// the shape and the corpus are placed by one function (R12).
    pub fn canonical(
        &self,
        space: Space,
        extent: &Bounds,
    ) -> Result<(Shape, CanonReport), CanonError> {
        debug_assert!(extent.validate().is_ok());
        let placed = place(self, space, extent)?;
        let (shape, mut report) = placed.canonical_in_view(extent)?;
        // **`vertices_in` is what the caller wrote**, not what densification produced: a `wgs84`
        // polygon reaching this line already carries the vertices its curved edges needed, and a
        // report saying `47 in → 47 out` would hide the transform the line exists to show.
        report.vertices_in = match self {
            ShapeF64::Polygon(parts) => parts.iter().flatten().map(|r| r.len() as u64).sum(),
            _ => 0,
        };
        Ok((shape, report))
    }

    /// [`ShapeF64::canonical`] for a shape already in the view's coordinates.
    fn canonical_in_view(&self, extent: &Bounds) -> Result<(Shape, CanonReport), CanonError> {
        let coords = self.coordinates();
        if coords.iter().any(|(x, y)| !x.is_finite() || !y.is_finite()) {
            return Err(CanonError::NotFinite);
        }
        let mut report = CanonReport::default();
        let extent_is_degrees = extent.x_min >= -180.0
            && extent.x_max <= 180.0
            && extent.y_min >= -90.0
            && extent.y_max <= 90.0;
        report.degrees_looking = !extent_is_degrees
            && !coords.is_empty()
            && coords
                .iter()
                .all(|(x, y)| x.abs() <= 180.0 && y.abs() <= 90.0);
        let qx = |v: f64| fixed32(v, extent.x_min, extent.x_max);
        let qy = |v: f64| fixed32(v, extent.y_min, extent.y_max);
        let in_x = |v: f64| v >= extent.x_min && v <= extent.x_max;
        let in_y = |v: f64| v >= extent.y_min && v <= extent.y_max;
        let shape = match self {
            ShapeF64::Bbox {
                min_x,
                min_y,
                max_x,
                max_y,
            } => {
                if max_x < min_x || max_y < min_y {
                    return Err(CanonError::InvertedBox);
                }
                report.outside = *max_x < extent.x_min
                    || *min_x > extent.x_max
                    || *max_y < extent.y_min
                    || *min_y > extent.y_max;
                report.clipped = !report.outside
                    && !(in_x(*min_x) && in_x(*max_x) && in_y(*min_y) && in_y(*max_y));
                if report.outside {
                    // Holds nothing: an empty polygon, which the descent discards at the root.
                    Shape::Polygon(Polygon::default())
                } else {
                    Shape::Bbox(Bbox {
                        min_x: qx(*min_x),
                        min_y: qy(*min_y),
                        max_x: qx(*max_x),
                        max_y: qy(*max_y),
                    })
                }
            }
            ShapeF64::Circle { cx, cy, r } => {
                self.conic(*cx, *cy, *r, *r, 0.0, extent, &mut report)?
            }
            ShapeF64::Ellipse {
                cx,
                cy,
                a,
                b,
                angle_degrees,
            } => self.conic(*cx, *cy, *a, *b, *angle_degrees, extent, &mut report)?,
            ShapeF64::Polygon(parts) => {
                report.vertices_in = parts.iter().flatten().map(|r| r.len() as u64).sum();
                let mut polygon = Polygon::default();
                let mut any_inside = false;
                for part in parts {
                    let mut out = Part { rings: Vec::new() };
                    for ring in part {
                        let (clipped, cut) = clip_ring(ring, extent);
                        report.clipped |= cut;
                        if !clipped.is_empty() {
                            any_inside = true;
                        }
                        let quantised: Vec<(u32, u32)> =
                            clipped.iter().map(|&(x, y)| (qx(x), qy(y))).collect();
                        match tidy_ring(&quantised) {
                            Some(r) => out.rings.push(r),
                            None => report.rings_dropped += 1,
                        }
                    }
                    if !out.rings.is_empty() {
                        polygon.parts.push(out);
                    }
                }
                report.outside = !any_inside && report.vertices_in > 0;
                orient_and_order(&mut polygon);
                report.vertices_out = polygon.vertex_count();
                Shape::Polygon(polygon)
            }
        };
        Ok((shape, report))
    }

    #[allow(clippy::too_many_arguments)]
    fn conic(
        &self,
        cx: f64,
        cy: f64,
        a: f64,
        b: f64,
        angle: f64,
        extent: &Bounds,
        report: &mut CanonReport,
    ) -> Result<Shape, CanonError> {
        if !(a > 0.0 && b > 0.0) {
            return Err(CanonError::NonPositiveAxis);
        }
        let e = a.max(b);
        report.outside = cx + e < extent.x_min
            || cx - e > extent.x_max
            || cy + e < extent.y_min
            || cy - e > extent.y_max;
        report.clipped = !report.outside
            && (cx - e < extent.x_min
                || cx + e > extent.x_max
                || cy - e < extent.y_min
                || cy + e > extent.y_max);
        if report.outside {
            return Ok(Shape::Polygon(Polygon::default()));
        }
        let sx = FIXED_SPAN / (extent.x_max - extent.x_min);
        let sy = FIXED_SPAN / (extent.y_max - extent.y_min);
        let centre = (
            fixed32(cx, extent.x_min, extent.x_max),
            fixed32(cy, extent.y_min, extent.y_max),
        );
        Conic::from_ellipse(centre, a, b, angle, (sx, sy))
            .map(Shape::Conic)
            .ok_or(CanonError::NonPositiveAxis)
    }
}

/// Sutherland–Hodgman against the extent's rectangle. Returns the clipped ring and whether
/// anything was cut. A ring wholly outside comes back empty.
fn clip_ring(ring: &[(f64, f64)], e: &Bounds) -> (Vec<(f64, f64)>, bool) {
    // Drop an explicit closing vertex: the ring is implicitly closed here.
    let mut pts: Vec<(f64, f64)> = ring.to_vec();
    if pts.len() > 1 && pts.first() == pts.last() {
        pts.pop();
    }
    let all_inside = pts
        .iter()
        .all(|&(x, y)| x >= e.x_min && x <= e.x_max && y >= e.y_min && y <= e.y_max);
    if all_inside {
        return (pts, false);
    }
    // Each side as: inside predicate and intersection with the side's line.
    // Sutherland–Hodgman, one side at a time: the axis, the bound, and whether inside is above it.
    for (axis, bound, above) in [
        (0, e.x_min, true),
        (0, e.x_max, false),
        (1, e.y_min, true),
        (1, e.y_max, false),
    ] {
        if pts.is_empty() {
            break;
        }
        let along = |p: (f64, f64)| if axis == 0 { (p.0, p.1) } else { (p.1, p.0) };
        let inside = |p: (f64, f64)| {
            let (v, _) = along(p);
            if above {
                v >= bound
            } else {
                v <= bound
            }
        };
        let cut = |a: (f64, f64), b: (f64, f64)| {
            let ((av, aw), (bv, bw)) = (along(a), along(b));
            along((bound, aw + (bw - aw) * (bound - av) / (bv - av)))
        };
        let mut out = Vec::with_capacity(pts.len() + 4);
        let n = pts.len();
        for i in 0..n {
            let cur = pts[i];
            let prev = pts[(i + n - 1) % n];
            let (ci, pi) = (inside(cur), inside(prev));
            if ci {
                if !pi {
                    out.push(cut(prev, cur));
                }
                out.push(cur);
            } else if pi {
                out.push(cut(prev, cur));
            }
        }
        pts = out;
    }
    (pts, true)
}

/// Remove consecutive duplicates and collinear runs (spikes included) from a closed ring of
/// grid positions; `None` if fewer than three vertices survive.
fn tidy_ring(pts: &[(u32, u32)]) -> Option<Ring> {
    let cross = |a: (u32, u32), b: (u32, u32), c: (u32, u32)| -> i128 {
        let (ax, ay) = (i128::from(a.0), i128::from(a.1));
        (i128::from(b.0) - ax) * (i128::from(c.1) - ay)
            - (i128::from(b.1) - ay) * (i128::from(c.0) - ax)
    };
    let mut stack: Vec<(u32, u32)> = Vec::with_capacity(pts.len());
    for &p in pts {
        if stack.last() == Some(&p) {
            continue;
        }
        while stack.len() >= 2 && cross(stack[stack.len() - 2], stack[stack.len() - 1], p) == 0 {
            stack.pop();
        }
        stack.push(p);
    }
    // The seam: the last vertices against the first, and the first against the last.
    loop {
        let n = stack.len();
        if n < 3 {
            return None;
        }
        if stack[0] == stack[n - 1] {
            stack.pop();
            continue;
        }
        if cross(stack[n - 2], stack[n - 1], stack[0]) == 0 {
            stack.pop();
            continue;
        }
        if cross(stack[n - 1], stack[0], stack[1]) == 0 {
            stack.remove(0);
            continue;
        }
        break;
    }
    Some(Ring {
        vertices: weight_ring(&stack),
    })
}

/// Twice the signed area of a ring.
fn doubled_signed_area(ring: &Ring) -> i128 {
    let n = ring.vertices.len();
    let mut s = 0i128;
    for i in 0..n {
        let a = ring.vertices[i];
        let b = ring.vertices[(i + 1) % n];
        s += i128::from(a.x) * i128::from(b.y) - i128::from(b.x) * i128::from(a.y);
    }
    s
}

/// Outer rings positive, holes negative; every ring rotated to its lowest `(y, x)` vertex;
/// parts ordered by their outer ring's first vertex.
fn orient_and_order(polygon: &mut Polygon) {
    for part in &mut polygon.parts {
        for (i, ring) in part.rings.iter_mut().enumerate() {
            let area = doubled_signed_area(ring);
            if (i == 0 && area < 0) || (i > 0 && area > 0) {
                // Reversing changes the sign, not the vertex set; weights travel with vertices.
                ring.vertices.reverse();
            }
            let low = ring
                .vertices
                .iter()
                .enumerate()
                .min_by_key(|(_, v)| (v.y, v.x))
                .map(|(i, _)| i)
                .unwrap_or(0);
            ring.vertices.rotate_left(low);
        }
    }
    // Each outer ring now starts at its lowest vertex, so this is the part's lowest vertex.
    polygon
        .parts
        .sort_by_key(|p| (p.rings[0].vertices[0].y, p.rings[0].vertices[0].x));
}

#[cfg(test)]
mod tests {
    use super::*;

    const E: Bounds = Bounds {
        x_min: 0.0,
        x_max: 1000.0,
        y_min: 0.0,
        y_max: 1000.0,
    };

    #[test]
    fn a_ring_is_canonical_whatever_its_start_and_direction() {
        let a = ShapeF64::Polygon(vec![vec![vec![
            (10.0, 10.0),
            (90.0, 10.0),
            (90.0, 90.0),
            (10.0, 90.0),
            (10.0, 10.0),
        ]]]);
        let b = ShapeF64::Polygon(vec![vec![vec![
            (90.0, 90.0),
            (90.0, 10.0),
            (50.0, 10.0),
            (10.0, 10.0),
            (10.0, 90.0),
        ]]]);
        let (sa, ra) = a.canonical(Space::View, &E).unwrap();
        let (sb, rb) = b.canonical(Space::View, &E).unwrap();
        assert_eq!(sa.encode(), sb.encode());
        assert_eq!(ra.vertices_out, 4);
        assert_eq!(rb.vertices_out, 4);
        assert!(!ra.clipped && !ra.outside);
    }

    #[test]
    fn a_shape_past_the_extent_is_clipped_and_one_beyond_it_is_empty() {
        let over = ShapeF64::Polygon(vec![vec![vec![
            (500.0, 500.0),
            (1500.0, 500.0),
            (1500.0, 1500.0),
            (500.0, 1500.0),
        ]]]);
        let (s, r) = over.canonical(Space::View, &E).unwrap();
        assert!(r.clipped && !r.outside);
        assert!(s.contains((u32::MAX, u32::MAX)));
        assert!(s.contains((1 << 31, u32::MAX)));
        assert!(!s.contains((1 << 30, 1 << 30)));
        let gone = ShapeF64::Polygon(vec![vec![vec![
            (2000.0, 2000.0),
            (3000.0, 2000.0),
            (3000.0, 3000.0),
        ]]]);
        let (s, r) = gone.canonical(Space::View, &E).unwrap();
        assert!(r.outside);
        assert_eq!(s.vertex_count(), 0);
        assert!(!s.contains((0, 0)));
    }

    #[test]
    fn a_collapsed_ring_is_dropped_and_reported() {
        // Three collinear points and a duplicate.
        let flat = ShapeF64::Polygon(vec![vec![vec![
            (1.0, 1.0),
            (2.0, 2.0),
            (2.0, 2.0),
            (3.0, 3.0),
        ]]]);
        let (s, r) = flat.canonical(Space::View, &E).unwrap();
        assert_eq!(r.rings_dropped, 1);
        assert_eq!(s.vertex_count(), 0);
    }

    #[test]
    fn degrees_on_a_metre_extent_are_reported_not_refused() {
        let e = Bounds {
            x_min: -20_000_000.0,
            x_max: 20_000_000.0,
            y_min: -20_000_000.0,
            y_max: 20_000_000.0,
        };
        let p = ShapeF64::Polygon(vec![vec![vec![(-1.0, 51.0), (0.5, 51.0), (0.5, 52.0)]]]);
        let (_, r) = p.canonical(Space::View, &e).unwrap();
        assert!(r.degrees_looking);
        let degrees = Bounds {
            x_min: -180.0,
            x_max: 180.0,
            y_min: -90.0,
            y_max: 90.0,
        };
        let (_, r) = p.canonical(Space::View, &degrees).unwrap();
        assert!(!r.degrees_looking);
    }

    #[test]
    fn what_is_not_a_coordinate_refuses() {
        assert_eq!(
            ShapeF64::Circle {
                cx: f64::NAN,
                cy: 0.0,
                r: 1.0
            }
            .canonical(Space::View, &E),
            Err(CanonError::NotFinite)
        );
        assert_eq!(
            ShapeF64::Circle {
                cx: 1.0,
                cy: 0.0,
                r: 0.0
            }
            .canonical(Space::View, &E),
            Err(CanonError::NonPositiveAxis)
        );
        assert_eq!(
            ShapeF64::Bbox {
                min_x: 5.0,
                min_y: 0.0,
                max_x: 1.0,
                max_y: 1.0
            }
            .canonical(Space::View, &E),
            Err(CanonError::InvertedBox)
        );
    }
}
