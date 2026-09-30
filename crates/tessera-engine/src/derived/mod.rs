//! Derived content: what an artifact looks like to *this* viewer.
//!
//! A derived property is a function of `membership ∩ M_auth`, the rows the composed mask makes
//! visible, and of nothing else; the only way to obtain those rows is
//! [`MaskedSet::visible_rows`](crate::compose::MaskedSet::visible_rows). Geometry a caller
//! supplies is a different thing, gated by containment elsewhere rather than derived here.
//!
//! Positions travel in the 32-bit fixed-point grid the Morton code is built from. A centroid is a
//! mean and so is fractional; a box and a hull are lattice positions of real members and stay
//! integral.
//!
//! A count is one bitmap operation, O(containers touched); everything here is O(visible members)
//! per artifact per request, which is why a layer declares what it wants: a cost control, not a
//! security one, since every value it can take is safe.
//!
//! Nothing in this module uses `rayon` or threads: parallelism in this engine is per request, and
//! a parallel loop here would let one request take the pool others are queued behind.


mod buckets;
pub mod cache;
mod dig;
mod geometry;
mod groups;
mod locator;
mod reduce;
#[cfg(test)]
mod test_support;

use croaring::Bitmap;
pub use tessera_types::layer::ComputedProperty;

use dig::concave_rings;
pub use dig::dig_rings;
pub use locator::RowLocator;
pub use reduce::quantised;

/// What a viewer is told about an artifact's shape, beside its masked count.
///
/// Each field is present exactly when its layer declared it and the artifact has at least one
/// visible member. On the identifier route a zero count with a declared geometry gives an empty
/// geometry rather than a fabricated one.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DerivedContent {
    /// Mean position, grid units. Fractional, so `f64`.
    pub centroid: Option<[f64; 2]>,
    /// `[qx_min, qy_min, qx_max, qy_max]`, grid units.
    pub bbox: Option<[u32; 4]>,
    /// The artifact's one drawn geometry: parts, then rings, then vertices in grid units. The
    /// derived hull this module computes, or the predicate or authored shape the serving pass
    /// fills in (`crate::shapes::served_rings`).
    ///
    /// A derived hull is a concave (alpha) shape over the visible members, not their convex wrap,
    /// with one ring per α-group of those members ([`concave_rings`]), each its own part, one
    /// outer ring and no holes. Rings run counter-clockwise from their lowest vertex and parts are
    /// ordered by their first vertex, so the value is a function of the member positions alone. A
    /// membership of one or two visible members gives a ring of one or two vertices, never a
    /// fabricated triangle.
    pub shape: Option<Vec<Vec<Vec<[u32; 2]>>>>,
}

impl DerivedContent {
    pub fn is_empty(&self) -> bool {
        self.centroid.is_none() && self.bbox.is_none() && self.shape.is_none()
    }

    /// How many vertices the drawn geometry carries, across every part and ring.
    pub fn shape_vertices(&self) -> u64 {
        self.shape.as_ref().map_or(0, |parts| {
            parts
                .iter()
                .flat_map(|rings| rings.iter())
                .map(|r| r.len() as u64)
                .sum()
        })
    }
}

/// [`compute`]'s answer for a level served from its column alone, read out of the accumulation one
/// pass over the mask produced for every artifact at once ([`crate::histogram::MaskedGeometry`]).
/// Both sum in `u64`, exact and unable to overflow, so this and the row-major route agree bit for
/// bit. A hull is not here: it is a function of the positions, not an accumulation over them, so a
/// layer deriving one keeps the artifact-major form ([`crate::artifacts::serves_column_only`]).
pub fn accumulated(
    declared: &[ComputedProperty],
    geometry: &crate::histogram::MaskedGeometry,
    ordinal: u32,
) -> DerivedContent {
    let mut out = DerivedContent::default();
    for property in declared {
        match property {
            ComputedProperty::Centroid => out.centroid = geometry.centroid(ordinal),
            ComputedProperty::Box => out.bbox = geometry.bbox(ordinal),
            // Unreachable: such a layer is never served column-only.
            ComputedProperty::Hull => {}
        }
    }
    out
}

/// Compute the declared properties over the rows a viewer may see.
///
/// `visible` comes from the composed mask and nothing else. `declared` is the layer's parsed
/// vocabulary; an empty one costs one branch and no position read.
pub fn compute(
    declared: &[ComputedProperty],
    visible: &Bitmap,
    locator: &RowLocator<'_>,
) -> DerivedContent {
    let mut out = DerivedContent::default();
    if declared.is_empty() {
        return out;
    }

    // One pass over the visible rows: reading a position twice per property would double the cost.
    let positions = locator.positions(visible);
    if positions.is_empty() {
        return out;
    }

    // The mean, the extremes and the hull's own binning box come off this one traversal.
    let (mut want_centroid, mut want_box, mut want_hull) = (false, false, false);
    for property in declared {
        match property {
            ComputedProperty::Centroid => want_centroid = true,
            ComputedProperty::Box => want_box = true,
            ComputedProperty::Hull => want_hull = true,
        }
    }

    // Summed as `u64`: a coordinate is a `u32`, so a per-axis sum is at most `(2^32 - 1)^2` and
    // cannot overflow. The division is the only floating-point step.
    let (mut sx, mut sy) = (0u64, 0u64);
    let mut b = [u32::MAX, u32::MAX, 0u32, 0u32];
    for p in &positions {
        sx += u64::from(p[0]);
        sy += u64::from(p[1]);
        b[0] = b[0].min(p[0]);
        b[1] = b[1].min(p[1]);
        b[2] = b[2].max(p[0]);
        b[3] = b[3].max(p[1]);
    }

    if want_centroid {
        let n = positions.len() as f64;
        out.centroid = Some([sx as f64 / n, sy as f64 / n]);
    }
    if want_box {
        out.bbox = Some(b);
    }
    if want_hull {
        // Each α-group is its own part; see [`DerivedContent::shape`].
        out.shape = Some(
            concave_rings(&positions, Some(b))
                .into_iter()
                .map(|ring| vec![ring])
                .collect(),
        );
    }
    out
}
