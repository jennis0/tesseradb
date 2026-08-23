//! A spatial level's membership: **row ranges, derived from a declared shape**.
//!
//! `membership = "spatial"` is *the points inside this shape*, and ruling R3 makes that exact
//! rather than approximate: **the ranges are the membership and the polygon is content**. An
//! artifact's members are the depth-`d` Morton tiles that cover its box — a point inside such a
//! tile is a member whether or not it is inside the box — so a decomposition at a different depth
//! is a *different* member set rather than a better approximation of the same one. That is why the
//! depth is part of the declaration and is disclosed beside the layer.
//!
//! # Why nothing here is stored
//!
//! Geometry is written in Morton order, so a tile is a contiguous row-ID range within a segment
//! (`contracts` §2.5). This resolves each tile against **this generation's own segments**, which
//! gives the property `annotation-representation.md` §2.0 claims for the source and no stored form
//! can have: **a point ingested inside a boundary is a member on the next request with nothing
//! rebuilt**. It is not a cache that has to be invalidated — a flush publishes a segment, the next
//! request resolves the same box against a segment list that now includes it, and the point counts.
//! A fold is the same statement one level up: it renumbers rows, and the ranges are re-derived over
//! the rows it wrote, so a box that named a region before the fold names the same region after it.
//!
//! The corollary is that this structure is a function of `(shapes, depth, extent, segments)` and of
//! nothing a request carries — it is held per generation beside the level's other derived forms and
//! rebuilt when the geometry moves, which is what [`crate::artifacts::ProjectionKey`]'s `live` term
//! is for.
//!
//! # What it replaces, and what it does not
//!
//! It replaces the tile index's walk, the row form's per-artifact bitmap and the masked-count
//! histogram, all three: candidacy is range-against-`viewport ∩ M_auth` arithmetic, the count is a
//! sum of masked range cardinalities, and the declared size is a sum of range widths. A spatial
//! level therefore uses the tile index not at all — the `everywhere` set is empty for one by
//! construction rather than by measurement.
//!
//! It does **not** replace containment or supplied content, because a predicate layer declares
//! neither (`LayerDeclaration::validate`).

use std::ops::Range;

use croaring::Bitmap;
use tessera_lifecycle::membership::Bbox;
use tessera_spatial::{tiles_for_bbox, Bounds};
use tessera_store::read::{tile_ranges_all, SegmentData};

use crate::compose::MaskedSet;

/// One level's memberships as ascending, disjoint row ranges — one set per ordinal.
///
/// **A flat offset table rather than a `Vec` per artifact**, on
/// [`crate::row_column::RowColumn`]'s own argument: a vector per ordinal is an allocation header
/// per artifact, and a level's ranges are a handful each.
pub struct RangeSets {
    /// `at[ordinal]..at[ordinal + 1]` indexes `ranges`. One longer than the ordinal count.
    at: Vec<u32>,
    /// `[lo, hi)` per range, in view row space, ascending and disjoint within each ordinal.
    ranges: Vec<(u32, u32)>,
}

impl std::fmt::Debug for RangeSets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RangeSets")
            .field("ordinals", &self.len())
            .field("ranges", &self.ranges.len())
            .finish()
    }
}

impl RangeSets {
    /// Decompose every artifact's box into the row ranges its covering tiles occupy, across every
    /// segment of the view.
    ///
    /// `shapes` is one entry per ordinal, `None` for a hole and for an artifact with no box —
    /// which on a shape layer is an artifact publication refuses, so it is a fail-closed absence
    /// here rather than a state to interpret.
    ///
    /// **The tiles come from `tessera_spatial` and the rows from `tessera_store`**, which is what
    /// makes this agree with the viewport's own tile resolution by construction: a request resolves
    /// its bbox through exactly these two calls, so a membership and a viewport that overlap on the
    /// map overlap here.
    pub fn build(
        shapes: impl Iterator<Item = Option<Bbox>>,
        depth: u8,
        extent: &Bounds,
        segments: &[(&SegmentData, u32)],
    ) -> Self {
        let mut at = vec![0u32];
        let mut ranges: Vec<(u32, u32)> = Vec::new();
        for shape in shapes {
            if let Some(shape) = shape {
                let tiles = tiles_for_bbox(shape.as_array(), depth, extent);
                let mut spans: Vec<(u32, u32)> = Vec::new();
                for (segment, row_base) in segments {
                    for span in tile_ranges_all(segment, &tiles) {
                        if span.start >= span.end {
                            continue;
                        }
                        spans.push((row_base + span.start, row_base + span.end));
                    }
                }
                // **Merged, and that is not tidiness.** Adjacent tiles are adjacent Morton ranges,
                // so merging turns a few hundred probes into a handful — the same reason
                // `crossing_domain` merges the viewport's own spans. Merging `[a, b)` with `[b, c)`
                // yields exactly their union, so the membership is never widened by it.
                spans.sort_unstable();
                let own = *at.last().expect("at is seeded with one entry") as usize;
                for (lo, hi) in spans {
                    // Merged only into **this** artifact's own tail: `ranges` is one flat table for
                    // the level, so a merge that reached back past `own` would fold two artifacts'
                    // memberships into one.
                    let mergeable = ranges.len() > own && ranges[ranges.len() - 1].1 >= lo;
                    if mergeable {
                        let last = ranges.last_mut().expect("length checked");
                        last.1 = last.1.max(hi);
                    } else {
                        ranges.push((lo, hi));
                    }
                }
            }
            at.push(ranges.len() as u32);
        }
        RangeSets { at, ranges }
    }

    /// How many ordinals this covers, holes included.
    pub fn len(&self) -> usize {
        self.at.len().saturating_sub(1)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// This artifact's ranges, ascending and disjoint. Empty for a hole.
    pub fn ranges(&self, ordinal: u32) -> &[(u32, u32)] {
        let at = ordinal as usize;
        let (Some(lo), Some(hi)) = (self.at.get(at), self.at.get(at + 1)) else {
            return &[];
        };
        &self.ranges[*lo as usize..*hi as usize]
    }

    /// The artifact's **unmasked** membership size — how many rows its ranges span.
    ///
    /// The proportional criterion's denominator if one were ever admitted here; ⊘ it is not, the
    /// criterion being refused on a predicate layer at parse. It is kept exact anyway because it is
    /// what a later ruling would read, and because a size derived from a different set from the
    /// count served beside it is the disagreement a viewer would see and could not explain.
    pub fn declared_size(&self, ordinal: u32) -> u64 {
        self.ranges(ordinal)
            .iter()
            .map(|(lo, hi)| u64::from(hi - lo))
            .sum()
    }

    /// `|membership ∩ M_auth|` — the number served, and the number the criterion reads.
    ///
    /// **A sum of masked range cardinalities, and no materialised set anywhere.** The ranges are
    /// disjoint, so the sum is exact rather than an over-count; and the mask is asked directly, so
    /// this is computed from inside `M_auth` alone (**I2**) with no gate applied after the fact.
    pub fn masked_count(&self, ordinal: u32, mask: &impl MaskedSet) -> u64 {
        self.ranges(ordinal)
            .iter()
            .map(|(lo, hi)| mask.count_range(*lo..*hi))
            .sum()
    }

    /// The artifact's rows in this view, materialised — for the routes that genuinely need a set
    /// rather than a count.
    pub fn rows(&self, ordinal: u32) -> Bitmap {
        let mut out = Bitmap::new();
        for (lo, hi) in self.ranges(ordinal) {
            out.add_range(*lo..*hi);
        }
        out.run_optimize();
        out
    }

    /// **Candidacy: which artifacts have a member this viewer can see inside the viewport.**
    ///
    /// `here` is `viewport ∩ M_auth`, and it must come from the composed mask and from nothing else
    /// — see [`MaskedSet::visible_rows`], which is the only way to obtain one. So every ordinal
    /// returned has a visible member in view by construction and there is nothing left for a
    /// verdict to ask about the geometry, exactly as the row-major scan's answer is.
    ///
    /// **Ranges, never a row walk.** The cost is one masked range probe per range per artifact —
    /// O(containers touched) each — against the row-major route's O(visible rows). A regional layer
    /// measures 1.0–1.6 blocks per artifact (`design/artifact-serving-at-scale.md` §5), which is
    /// the shape this arm is for: "large" and "numerous" are mutually exclusive, so a boundary set
    /// that multiplies is a boundary set whose members shrink.
    ///
    /// **Ascending**, which is what the cut downstream is entitled to.
    pub fn candidates(&self, here: &Bitmap) -> Bitmap {
        let mut out = Bitmap::new();
        for ordinal in 0..self.len() as u32 {
            if self
                .ranges(ordinal)
                .iter()
                .any(|(lo, hi)| here.range_cardinality(*lo..*hi) > 0)
            {
                out.add(ordinal);
            }
        }
        out.run_optimize();
        out
    }

    /// The mean number of ranges per live artifact — the locality figure an operator reads beside a
    /// spatial level, in the place `blocks_per_artifact` sits for a stored membership.
    pub fn ranges_per_artifact(&self) -> f64 {
        let live = (0..self.len() as u32)
            .filter(|ordinal| !self.ranges(*ordinal).is_empty())
            .count();
        if live == 0 {
            0.0
        } else {
            self.ranges.len() as f64 / live as f64
        }
    }
}

/// `[lo, hi)` as a `Range`, for the callers that want one.
pub fn as_range(span: (u32, u32)) -> Range<u32> {
    span.0..span.1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bbox(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Option<Bbox> {
        Bbox::new(min_x, min_y, max_x, max_y)
    }

    /// A set built by hand, so the range arithmetic is testable without a segment on disk.
    fn sets(rows: &[&[(u32, u32)]]) -> RangeSets {
        let mut at = vec![0u32];
        let mut ranges = Vec::new();
        for set in rows {
            ranges.extend_from_slice(set);
            at.push(ranges.len() as u32);
        }
        RangeSets { at, ranges }
    }

    /// **The three questions the route asks, over ranges.** A hole answers nothing, and past the
    /// level is not a panic.
    #[test]
    fn ranges_answer_candidacy_counts_and_sizes() {
        let sets = sets(&[&[(0, 4), (10, 12)], &[], &[(20, 21)]]);
        assert_eq!(sets.len(), 3);
        assert_eq!(sets.declared_size(0), 6);
        assert_eq!(sets.declared_size(1), 0, "a hole spans nothing");
        assert_eq!(sets.declared_size(2), 1);
        assert_eq!(sets.declared_size(99), 0, "past the level is not a panic");

        // The count is over the whole mask, and it is the sum over disjoint ranges.
        let mask: Bitmap = [0u32, 1, 11, 20, 30].into_iter().collect();
        assert_eq!(sets.masked_count(0, &mask), 3);
        assert_eq!(sets.masked_count(1, &mask), 0);
        assert_eq!(sets.masked_count(2, &mask), 1);

        // Candidacy is against `viewport ∩ M_auth`: row 11 is in artifact 0's second range only.
        let here: Bitmap = [11u32].into_iter().collect();
        assert_eq!(sets.candidates(&here).iter().collect::<Vec<_>>(), vec![0]);
        let here: Bitmap = [5u32, 30].into_iter().collect();
        assert!(
            sets.candidates(&here).is_empty(),
            "no range holds either row"
        );
        let here: Bitmap = [1u32, 20].into_iter().collect();
        assert_eq!(
            sets.candidates(&here).iter().collect::<Vec<_>>(),
            vec![0, 2]
        );
    }

    /// **The materialised set and the count agree**, which is what lets a route that needs rows and
    /// a route that needs a number be answered from one structure.
    #[test]
    fn the_rows_and_the_count_are_the_same_set() {
        let sets = sets(&[&[(0, 4), (10, 12)]]);
        let mask: Bitmap = (0..40u32).filter(|r| r % 3 == 0).collect();
        assert_eq!(
            sets.masked_count(0, &mask),
            mask.and_cardinality(&sets.rows(0))
        );
        assert_eq!(sets.rows(0).cardinality(), sets.declared_size(0));
    }

    /// A box is refused rather than normalised, so a transposition never becomes a membership.
    #[test]
    fn an_inverted_box_is_not_a_shape() {
        assert!(bbox(0.0, 0.0, 10.0, 10.0).is_some());
        assert!(bbox(10.0, 0.0, 0.0, 10.0).is_none());
        assert!(bbox(0.0, f64::NAN, 10.0, 10.0).is_none());
        assert!(
            bbox(0.0, 0.0, 0.0, 0.0).is_some(),
            "a point is a degenerate box, not an error"
        );
    }

    /// Ranges per artifact is the locality figure an operator reads, and a hole is not an artifact.
    #[test]
    fn locality_counts_live_artifacts_only() {
        assert_eq!(
            sets(&[&[(0, 4)], &[], &[(8, 9), (12, 13)]]).ranges_per_artifact(),
            1.5
        );
        assert_eq!(sets(&[&[], &[]]).ranges_per_artifact(), 0.0);
    }
}
