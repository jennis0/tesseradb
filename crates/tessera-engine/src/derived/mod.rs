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
//!
//! The hull is the expensive one: it is a **concave** shape over the visible members rather than
//! their convex wrap, and **one ring per separated group of them** rather than one ring per
//! artifact ([`concave_rings`]) — a sort, a grouping pass, a bucketing pass and a bounded number of
//! digs, 3.3× the convex path over a whole 197-artifact layer (`docs/design/artifact-shapes.md`
//! §7). It discloses nothing the wrap did not, and the argument is in [`concave_rings`]'s own
//! documentation rather than restated here.
//!
//! ## This module is single-threaded, deliberately
//!
//! **Nothing here uses `rayon`, and nothing here may acquire one** (owner ruling, 2026-08-28).
//! Parallelism in this engine lives at the **request** level — `Engine::viewport` installs the one
//! shared compute pool for a request's tile loop — so that concurrent requests use the cores. A
//! `par_iter` over an artifact's members, or over a response's artifacts, would let one request
//! oversubscribe the pool the others are queued behind, which trades a served viewer's latency for
//! a hovering one's. The two ways this module was made cheap instead are the ones a second thread
//! would have hidden: the input is reduced before the shape is computed
//! ([`QUANTISE_DIVISIONS`](reduce::QUANTISE_DIVISIONS)), and the answer is not computed twice ([`crate::derived_cache`]).


mod buckets;
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
pub use dig::{dig_rings, dig_rings_at, dig_rings_at_floor, DigFloor};
pub use locator::RowLocator;
pub use reduce::{quantised, SERVED_QUANTISE_DIVISIONS};

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
    /// **The artifact's one drawn geometry**, in the wire's nesting — parts, then rings, then
    /// vertices in grid units (`polygon-membership.md` §7.1) — of whichever kind its layer
    /// declared: the **derived** hull this module computes, or the **predicate** or **authored**
    /// shape the serving pass fills in from the held shapes and the content blob
    /// (`crate::shapes::served_rings`).
    ///
    /// A derived hull is a **concave (alpha) shape** over the visible members, not their convex
    /// wrap, with **one ring per α-group of those members** ([`concave_rings`]) — and every
    /// α-group is its own **part**, one outer ring and no holes, because a second ring in one
    /// part is a hole to a renderer and two groups are two shapes, not a shape with a gap. Rings
    /// are counter-clockwise from their lowest vertex and parts are ordered by their first
    /// vertex, so the value is a function of the member positions and not of the order they were
    /// gathered in. A membership of one visible member gives one ring of one vertex, of two gives
    /// one ring of two: the hull of a point set is that point set when it is degenerate, and
    /// rounding it up to a triangle would draw an area no member occupies.
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

/// Compute the declared properties over the rows a viewer may see.
///
/// `visible` comes from the composed mask and nothing else — see this module's doc. `declared` is
/// the layer's parsed vocabulary; an empty one costs one branch and no position read, which is what
/// keeps a count-only layer at count-only cost.
/// [`compute`]'s answer for a level served from its column alone, read out of the accumulation one
/// pass over the mask produced for every artifact at once
/// ([`crate::histogram::MaskedGeometry`]).
///
/// **The same two quantities by another route, not two definitions of them.** A centroid is the
/// mean of `membership ∩ M_auth`'s positions and a box their extremes; summing and comparing as the
/// rows are read gives the same numbers as materialising the rows and traversing them, which is
/// what `tests/artifact_row_major.rs` asserts against the artifact-major route. **Exactly the same
/// numbers**: both sum in `u64` — the sum cannot overflow one and is exact in one — so the chunked
/// reduction and the sequential walk agree bit for bit, which floating-point addition past 2^53
/// would not give.
///
/// ⊘ **A hull is not here**, and cannot be: it is a function of the positions themselves rather
/// than an accumulation over them. A layer deriving one keeps the artifact-major form
/// ([`crate::artifacts::serves_column_only`]), so this is never asked for one.
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

pub fn compute(
    declared: &[ComputedProperty],
    visible: &Bitmap,
    locator: &RowLocator<'_>,
) -> DerivedContent {
    let mut out = DerivedContent::default();
    if declared.is_empty() {
        return out;
    }

    // One pass over the visible rows, whatever is declared: the read is the cost, and reading a
    // position twice to compute a centroid and a box separately would double it.
    let positions = locator.positions(visible);
    if positions.is_empty() {
        return out;
    }

    // The vocabulary decides which values are *kept*, not how many passes are made over the
    // positions: the mean and the extremes come off one traversal, and the bounding box the hull's
    // binning grid is scaled from is the same box `box` reports. Read separately — a loop for the
    // centroid, a loop for the box, a third inside the reduction — that is three traversals of an
    // array which is 8 bytes a member, 19 MB on the measurement layer's largest artifact.
    let (mut want_centroid, mut want_box, mut want_hull) = (false, false, false);
    for property in declared {
        match property {
            ComputedProperty::Centroid => want_centroid = true,
            ComputedProperty::Box => want_box = true,
            ComputedProperty::Hull => want_hull = true,
        }
    }

    // **Summed as `u64`, which is exact and cannot overflow**: a membership is a set of rows, a
    // row space is `u32`-addressed and a coordinate is a `u32`, so a per-axis sum is at most
    // `(2^32 - 1)^2`. The mean is fractional and the division is the only floating-point step, so
    // this and `crate::histogram::MaskedGeometry`'s accumulation produce the same number rather
    // than two roundings of it.
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
        // Each α-group is its own part: a hull has no holes, and a second ring of one part
        // would be read as one (see [`DerivedContent::shape`]).
        out.shape = Some(
            concave_rings(&positions, Some(b))
                .into_iter()
                .map(|ring| vec![ring])
                .collect(),
        );
    }
    out
}
