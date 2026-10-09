//! A polygon smaller than one depth-16 cell holds the point inside it, by the direct test and by
//! the boundary-cell route the segment resolution takes: the two must agree.
//!
//! The polygons are Overture divisions, each a few thousand grid units across, and the point
//! beside each is the one place whose lineage names it, at the coordinates the source gives.
//! Membership is of a point's stored position, its `f32` coordinates quantised, not its source
//! coordinates, and for both fixtures that quantisation moves the point across the edge.

use tessera_spatial::morton::{fixed32, split32, Bounds};
use tessera_spatial::shape::{contexts_at, read_wkt, Rect, ShapeF64, Space};

const E: Bounds = Bounds {
    x_min: 0.0,
    x_max: 1.0,
    y_min: 0.0,
    y_max: 1.0,
};

const CASES: &[(&str, f64, f64)] = &[
    (
        "POLYGON ((0.2819125730555556 0.5006139087445856, 0.28191289694444444 0.5006135362418161, 0.2819133252777778 0.5006138334662479, 0.2819129688888889 0.5006142551360528, 0.2819125730555556 0.5006139087445856))",
        0.2819126666666667,
        0.5006138328273543,
    ),
    (
        "POLYGON ((0.24877046083333337 0.45918236054392225, 0.24876938055555559 0.45918242396280046, 0.24876907694444444 0.4591789577244062, 0.2487695905555556 0.459178999908231, 0.24877032305555558 0.459179060170833, 0.24877046083333337 0.45918236054392225))",
        0.24876946,
        0.45917898960619513,
    ),
];

/// The two routes agree and both say inside, over a shape canonicalisation kept whole.
///
/// The agreement alone is not the claim: two routes answering outside agree too, and a
/// canonicalisation that dropped a sub-cell ring is exactly what makes them both say it. So the
/// ring's survival, the containment itself and the route taken are each asserted, not printed.
///
/// Mutations this kills: a canonicalisation that drops a ring smaller than one depth-16 cell; a
/// decomposition that offers a sub-cell shape no boundary cell to descend into.
#[test]
fn a_polygon_smaller_than_a_cell_holds_its_one_point_by_both_routes() {
    for (wkt, x, y) in CASES {
        let shape = ShapeF64::Polygon(read_wkt(wkt).unwrap());
        let (canonical, report) = shape.canonical(Space::View, &E).unwrap();
        eprintln!("{report:?} vertices {}", canonical.vertex_count());
        assert!(
            !report.outside && report.rings_dropped == 0,
            "the polygon must survive quantisation whole, or nothing below is being tested: \
             {report:?} for {wkt}"
        );
        assert!(
            canonical.vertex_count() >= 3,
            "a surviving ring keeps at least three vertices: {wkt}"
        );
        let p = (fixed32(*x, E.x_min, E.x_max), fixed32(*y, E.y_min, E.y_max));
        let direct = canonical.contains(p);
        let decomposition = canonical.decompose(None);
        let (cell, _) = split32(p.0, p.1);
        let in_interior = decomposition
            .interior
            .iter()
            .any(|t| (u64::from(cell.raw()) >> (2 * (16 - t.depth as u32))) == t.prefix);
        let prepared = canonical.prepared();
        let boundary = decomposition.boundary.iter().find(|c| c.cell == cell);
        let via_cell = match (in_interior, boundary) {
            (true, _) => true,
            (false, Some(_)) => {
                let ctx = contexts_at(prepared.region().unwrap(), &[cell]).remove(0);
                prepared.contains_in_cell(p, Rect::of_cell(cell), &ctx)
            }
            (false, None) => false,
        };
        eprintln!(
            "point {p:?} cell {:#x} interior {} boundary cells {} direct {direct} via_cell {via_cell}",
            cell.raw(),
            decomposition.interior.len(),
            decomposition.boundary.len()
        );
        assert!(
            in_interior || boundary.is_some(),
            "the point's cell is neither interior nor boundary, so the resolution's route never \
             descends into the shape at all: {wkt}"
        );
        assert!(
            direct,
            "the direct test must hold the point its source coordinates are inside: {wkt}"
        );
        assert_eq!(
            direct, via_cell,
            "the boundary-cell route the resolution takes disagrees with the direct test: {wkt}"
        );
    }
}
