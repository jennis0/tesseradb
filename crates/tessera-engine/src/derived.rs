//! Derived content: what an artifact looks like to *this* viewer.
//!
//! **One closure rule governs the whole vocabulary** (`annotations.md` §4.2):
//!
//! > A derived property is a function of `membership ∩ M_auth` and of nothing else.
//!
//! That is **I2** restated at the artifact, and it is what lets the vocabulary grow without a new
//! leak-register row each time: *median publication year of visible members* satisfies it and needs
//! no review; *total membership* does not, and is a disclosure. Every function in this module takes
//! the visible rows and nothing else, and the only way to obtain those is
//! [`MaskedSet::visible_rows`](crate::compose::MaskedSet::visible_rows) — so a property computed
//! over full membership is not merely forbidden, it has no input to be computed from.
//!
//! ## Why this is the fail-open to watch, and not the count
//!
//! An artifact whose *own terms* authorise it is authorised to **exist**, not to describe its
//! members (`annotations.md` §4). A build-time hull served beside a masked count discloses exactly
//! the members the gate did not cover — and it does so in a shape that looks like the geometry the
//! engine would have derived anyway, which is why §4.1 puts caller-supplied geometry in a different
//! row of the table from this one. Anything computed here is safe by construction; anything
//! *supplied* carries a generating set and is gated by containment instead.
//!
//! ## Grid units, not data coordinates
//!
//! Positions travel in the 32-bit fixed-point grid the Morton code is built from — the same units
//! the point path puts on the wire, where a client needs no quantisation extent to interpret what
//! it draws (`clients/ts/core/src/decode.ts`). A centroid is a mean and so is fractional; a box and
//! a hull are lattice positions of real members and stay integral.
//!
//! ## The cost, and why the vocabulary is declared
//!
//! A count is one bitmap operation, O(containers touched). Everything here is O(visible members)
//! per artifact per request, because it reads a position for each. A client drawing only centroids
//! should not pay hull cost for every artifact on screen, which is what the layer's declaration is
//! for — a **cost** control, not a security one, since every value it can take is safe.

use croaring::Bitmap;
use tessera_spatial::morton::unsplit32;
use tessera_store::read::SegmentData;
pub use tessera_types::layer::DerivedProperty;
use tessera_types::MortonCode;

/// What a viewer is told about an artifact's shape, beside its masked count.
///
/// Each field is present exactly when its layer declared it and the artifact has at least one
/// visible member. A served artifact always has one — an artifact with none fails candidacy in the
/// viewport, and on the identifier route a zero count with a declared geometry gives an **empty**
/// geometry rather than a fabricated one.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DerivedContent {
    /// Mean position, grid units. Fractional, so `f64`.
    pub centroid: Option<[f64; 2]>,
    /// `[qx_min, qy_min, qx_max, qy_max]`, grid units.
    pub bbox: Option<[u32; 4]>,
    /// The convex hull's vertices, counter-clockwise, grid units. A membership of one visible
    /// member gives one vertex, of two gives two: the hull of a point set is that point set when it
    /// is degenerate, and rounding it up to a triangle would draw an area no member occupies.
    pub hull: Option<Vec<[u32; 2]>>,
}

impl DerivedContent {
    pub fn is_empty(&self) -> bool {
        self.centroid.is_none() && self.bbox.is_none() && self.hull.is_none()
    }
}

/// The segments of one slice, ascending in `row_base`, resolving a slice row to the segment holding
/// it.
///
/// The same reverse-scan shape [`crate::select::SelectionParts::resolve_indexed`] uses, and for the
/// same reason: the live segment count is a handful, so a scan beats a binary search and is
/// obviously correct. It is a separate type only because artifact geometry is gathered outside a
/// tile selection — the rows come from a membership, not from a viewport's parts.
pub struct RowLocator<'a> {
    segments: Vec<(&'a SegmentData, u32)>,
}

impl<'a> RowLocator<'a> {
    /// `segments` must be ascending in `row_base`, which
    /// [`crate::viewport::segments_with_row_bases`] guarantees by sorting.
    pub fn new(segments: Vec<(&'a SegmentData, u32)>) -> Self {
        RowLocator { segments }
    }

    /// The position of one slice row, in grid units, or `None` for a row past every segment's
    /// extent.
    ///
    /// **`None` is dropped by the caller rather than defaulted**, because a position of `(0, 0)` is
    /// a real position on the map: a member the row space cannot place would otherwise pull every
    /// centroid towards the origin, which is a wrong answer that renders.
    fn position(&self, row: u32) -> Option<(u32, u32)> {
        let (segment, local) = self.resolve(row)?;
        let idx = local as usize;
        let cell = *segment.morton.u32().get(idx)?;
        let residual = *segment.columns.residual().get(idx)?;
        Some(unsplit32(MortonCode::new(cell), residual))
    }

    fn resolve(&self, row: u32) -> Option<(&'a SegmentData, u32)> {
        for &(segment, row_base) in self.segments.iter().rev() {
            if row >= row_base {
                return Some((segment, row - row_base));
            }
        }
        None
    }
}

/// Compute the declared properties over the rows a viewer may see.
///
/// `visible` comes from the composed mask and nothing else — see this module's doc. `declared` is
/// the layer's parsed vocabulary; an empty one costs one branch and no position read, which is what
/// keeps a count-only layer at count-only cost.
pub fn compute(
    declared: &[DerivedProperty],
    visible: &Bitmap,
    locator: &RowLocator<'_>,
) -> DerivedContent {
    let mut out = DerivedContent::default();
    if declared.is_empty() {
        return out;
    }

    // One pass over the visible rows, whatever is declared: the read is the cost, and reading a
    // position twice to compute a centroid and a box separately would double it.
    let mut positions: Vec<[u32; 2]> = Vec::with_capacity(visible.cardinality() as usize);
    for row in visible.iter() {
        if let Some((qx, qy)) = locator.position(row) {
            positions.push([qx, qy]);
        }
    }
    if positions.is_empty() {
        return out;
    }

    for property in declared {
        match property {
            DerivedProperty::Centroid => {
                // Summed as `f64` rather than `u64`: the grid is 2^32 wide, so a membership past
                // ~2^32 members would overflow a `u64` sum, and the mean is fractional in any case.
                let (mut sx, mut sy) = (0.0f64, 0.0f64);
                for p in &positions {
                    sx += p[0] as f64;
                    sy += p[1] as f64;
                }
                let n = positions.len() as f64;
                out.centroid = Some([sx / n, sy / n]);
            }
            DerivedProperty::Box => {
                let mut b = [u32::MAX, u32::MAX, 0u32, 0u32];
                for p in &positions {
                    b[0] = b[0].min(p[0]);
                    b[1] = b[1].min(p[1]);
                    b[2] = b[2].max(p[0]);
                    b[3] = b[3].max(p[1]);
                }
                out.bbox = Some(b);
            }
            DerivedProperty::Hull => out.hull = Some(convex_hull(&positions)),
        }
    }
    out
}

/// Andrew's monotone chain, counter-clockwise, on the integer grid.
///
/// **Integer arithmetic throughout, in `i128`.** Each component of a grid vector is bounded by
/// 2^32, so their product needs 64 bits and their *difference* needs 65: an `i64` cross product
/// overflows on a hull spanning most of the map, which is the ordinary case for a broad principal's
/// cluster rather than an edge one. In `i128` the orientation test is exact and there is no epsilon
/// to choose. A hull computed in floats would be non-deterministic across platforms for collinear
/// members, and the conformance suite compares this byte-for-byte against the Python oracle.
///
/// Collinear points are dropped (`<= 0` rather than `< 0`), so a hull carries vertices and not the
/// members lying along its edges.
fn convex_hull(points: &[[u32; 2]]) -> Vec<[u32; 2]> {
    let mut p: Vec<[u32; 2]> = points.to_vec();
    p.sort_unstable();
    p.dedup();
    if p.len() <= 2 {
        return p;
    }

    let cross = |o: [u32; 2], a: [u32; 2], b: [u32; 2]| -> i128 {
        let (ox, oy) = (o[0] as i128, o[1] as i128);
        (a[0] as i128 - ox) * (b[1] as i128 - oy) - (a[1] as i128 - oy) * (b[0] as i128 - ox)
    };

    let mut hull: Vec<[u32; 2]> = Vec::with_capacity(p.len() + 1);
    for &point in &p {
        while hull.len() >= 2 && cross(hull[hull.len() - 2], hull[hull.len() - 1], point) <= 0 {
            hull.pop();
        }
        hull.push(point);
    }
    let lower = hull.len() + 1;
    for &point in p.iter().rev().skip(1) {
        while hull.len() >= lower && cross(hull[hull.len() - 2], hull[hull.len() - 1], point) <= 0 {
            hull.pop();
        }
        hull.push(point);
    }
    hull.pop();
    hull
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hull_carries_vertices_and_not_the_members_along_its_edges() {
        // A square with a member at the midpoint of one edge and one in the middle.
        let points = [[0, 0], [10, 0], [10, 10], [0, 10], [5, 0], [5, 5]];
        let hull = convex_hull(&points);
        assert_eq!(hull.len(), 4, "four corners: {hull:?}");
        for corner in [[0, 0], [10, 0], [10, 10], [0, 10]] {
            assert!(hull.contains(&corner), "{corner:?} missing from {hull:?}");
        }
        assert!(!hull.contains(&[5, 0]), "a collinear member is not a vertex");
        assert!(!hull.contains(&[5, 5]), "an interior member is not a vertex");
    }

    /// A degenerate hull is the members themselves. Rounding one up to an area would draw a region
    /// no member occupies — a shape asserting more than the data does.
    #[test]
    fn a_degenerate_hull_is_the_members_themselves() {
        assert_eq!(convex_hull(&[[3, 4]]), vec![[3, 4]]);
        assert_eq!(convex_hull(&[[3, 4], [3, 4]]), vec![[3, 4]]);
        assert_eq!(convex_hull(&[[0, 0], [1, 1]]), vec![[0, 0], [1, 1]]);
        // Three collinear members are a segment, not a triangle.
        assert_eq!(
            convex_hull(&[[0, 0], [1, 1], [2, 2]]),
            vec![[0, 0], [2, 2]],
            "collinear members leave two endpoints"
        );
    }

    /// The hull's winding is fixed, because the oracle compares vertex lists and a hull that
    /// started at a different vertex or wound the other way would differ byte-for-byte while being
    /// the same shape.
    #[test]
    fn the_hull_starts_at_the_lowest_vertex_and_winds_counter_clockwise() {
        let hull = convex_hull(&[[10, 0], [0, 10], [0, 0], [10, 10]]);
        assert_eq!(hull, vec![[0, 0], [10, 0], [10, 10], [0, 10]]);
    }
}
