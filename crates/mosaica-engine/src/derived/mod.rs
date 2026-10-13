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
pub use mosaica_types::layer::ComputedProperty;

use dig::concave_rings;
pub use dig::dig_rings;
pub use locator::{place, Placement, RowLocator};
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

/// [`compute`]'s answer for a level served from its column alone, read off the level's figures
/// over every shard ([`crate::figures::LevelFigures`]). Both sum exactly and cannot overflow, so
/// this and the row-major route agree bit for bit. A hull is not here: it is a function of the
/// positions, not an accumulation over them, so a layer deriving one keeps the artifact-major form
/// ([`crate::artifacts::serves_column_only`]).
pub fn accumulated(
    declared: &[ComputedProperty],
    geometry: &crate::figures::LevelFigures,
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

/// Compute the declared properties over the rows a viewer may see in every shard: each shard's
/// visible rows beside the locator of its segments. The positions are gathered and every
/// property computed once over them, the hull's reduction and dig budget included.
///
/// Every visible set comes from the composed mask and nothing else. `declared` is the layer's
/// parsed vocabulary; an empty one costs one branch and no position read.
pub fn compute(
    declared: &[ComputedProperty],
    sides: &[(&RowLocator<'_>, Bitmap)],
) -> DerivedContent {
    let mut out = DerivedContent::default();
    if declared.is_empty() {
        return out;
    }

    // One pass over the visible rows: reading a position twice per property would double the cost.
    let mut sides = sides.iter();
    let Some((locator, visible)) = sides.next() else {
        return out;
    };
    let mut positions = locator.positions(visible);
    for (locator, visible) in sides {
        positions.extend(locator.positions(visible));
    }
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

    // Summed as `u128`: a coordinate is a `u32` and every shard holds at most `2^32` members, so
    // a per-axis sum over every shard cannot overflow. The division is the only floating-point
    // step.
    let (mut sx, mut sy) = (0u128, 0u128);
    let mut b = [u32::MAX, u32::MAX, 0u32, 0u32];
    for p in &positions {
        sx += u128::from(p[0]);
        sy += u128::from(p[1]);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A shard's segment over `points`, in their order from row 0: its two position columns.
    fn columns(points: &[[u32; 2]]) -> (Vec<u32>, Vec<u32>) {
        points
            .iter()
            .map(|&[x, y]| {
                let (cell, residual) = mosaica_spatial::split32(x, y);
                (cell.raw(), residual)
            })
            .unzip()
    }

    /// The members one shard holds, gathered from two that split them between them, give the same
    /// centroid, box and hull: above the reduction's floor, where the hull is dug over each cell's
    /// representative, and below it.
    #[test]
    fn positions_gathered_from_two_shards_derive_what_one_shard_holding_them_does() {
        let every = [
            ComputedProperty::Centroid,
            ComputedProperty::Box,
            ComputedProperty::Hull,
        ];
        for count in [2_000, 90_000] {
            let points = test_support::sample(count, 3_000, |x, y| {
                let r = ((x * x + y * y) as f64).sqrt();
                r <= 1_500.0 + 900.0 * (5.0 * (y as f64).atan2(x as f64)).cos()
            });
            assert_eq!(points.len() > reduce::REDUCTION_FLOOR, count > 75_000);
            let one = columns(&points);
            let one = RowLocator::of_columns(&[(0, &one.0, &one.1)]);
            let whole = compute(&every, &[(&one, (0..points.len() as u32).collect())]);

            let (even, odd): (Vec<_>, Vec<_>) =
                points.iter().enumerate().partition(|(at, _)| at % 3 != 1);
            let [even, odd]: [Vec<[u32; 2]>; 2] =
                [even, odd].map(|side| side.into_iter().map(|(_, p)| *p).collect());
            let (a, b) = (columns(&even), columns(&odd));
            let (a, b) = (
                RowLocator::of_columns(&[(0, &a.0, &a.1)]),
                RowLocator::of_columns(&[(0, &b.0, &b.1)]),
            );
            let split = compute(
                &every,
                &[
                    (&a, (0..even.len() as u32).collect()),
                    (&b, (0..odd.len() as u32).collect()),
                ],
            );
            assert!(whole.shape.is_some() && whole.centroid.is_some());
            assert_eq!(split, whole, "{count} members");
        }
    }
}
