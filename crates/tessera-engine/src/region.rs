//! The region operand — a drawn shape, or a published one named by its `tessera_id`, as a set of
//! rows over the whole view (`selection-operand.md`; `polygon-membership.md` §8).
//!
//! **What is cached and what is not, and why the line sits where it does.** A shape's
//! decomposition against the Morton grid — the tiles wholly inside it, the depth-16 cells its
//! boundary crosses, and the row ranges each of those occupies in every segment — is a function
//! of `(view, segments_version, canonical shape, stop depth)` and of nothing about the principal,
//! so it is held once per generation in [`RegionDecomposition`] and shared across principals
//! (owner ruling 2026-08-29, selection-operand §10 (b)). The rows *inside* the shape are not held:
//! a boundary cell's rows are tested one by one, and the test runs over the rows the request's
//! **composed mask** admits and over no others (§8.2: the mask goes in first, not last). So the
//! cached value carries no authorisation and the tested value is never cached — which is why
//! [`RegionDecomposition::rows_under`] takes the mask as an argument rather than reading rows and
//! leaving the caller to intersect: the boundary path cannot be called without one, and a test
//! that the pre-mask cardinality is never materialised is unwritable as such, so the signature is
//! the assurance.
//!
//! **The pre-mask cardinality of the region is never computed.** The interior bitmap is built from
//! ranges and never counted; the boundary rows are enumerated under the mask; the row set the
//! filter returns is intersected with the composed mask by every consumer before a number leaves
//! it. `viewport.rs` skips its `filter_matched` probe count for a tree carrying a region leaf for
//! the same reason.
//!
//! **The verdict is a function of the shape and the grid alone.** `max_region_cells` bounds the
//! *boundary cells* held at one depth of the descent, never the rows in them, so whether an answer
//! is exact or a cover — and at what depth — says nothing about the corpus (selection-operand §6).
//! It is settled by the decomposition, before any row is read, which is what lets it ride as a
//! response header ahead of the streamed body.

use std::ops::Range;
use std::sync::Arc;

use croaring::Bitmap;
use sha2::{Digest, Sha256};

use tessera_spatial::shape::{BoundaryCell, PolyCtx, Rect, Shape};
use tessera_spatial::{unsplit32, Tile};
use tessera_store::read::{tile_ranges_all, SegmentData};
use tessera_types::MortonCode;

use crate::compose::EffectiveMask;
use tessera_cache::CacheWeight;

/// The default `max_region_cells` — the most boundary cells a region's descent may hold at one
/// depth before it stops and answers a cover (selection-operand §2). A box around the whole world
/// has a perimeter of 4 × 2¹⁶ depth-16 cells, which this admits exactly, so an ordinary drawn
/// selection is answered exact and only a shape with a very long edge is a cover. The same figure
/// as `max_tiles_per_request`'s default, for the same availability argument.
pub const DEFAULT_MAX_REGION_CELLS: usize = 262_144;

/// Whether a region's answer is exact for the shape, or exact for a cover of it taken at the depth
/// the budget stopped the descent (selection-operand §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionVerdict {
    Exact,
    Cover { depth: u8 },
}

impl RegionVerdict {
    /// The `x-tessera-region` header's value.
    pub fn header_value(self) -> String {
        match self {
            RegionVerdict::Exact => "exact".to_string(),
            RegionVerdict::Cover { depth } => format!("cover; depth={depth}"),
        }
    }

    /// The coarser of two verdicts — what a request carrying two region leaves reports: a cover
    /// anywhere makes the answer a cover, at the shallowest depth any leaf stopped at.
    pub fn coarser(self, other: RegionVerdict) -> RegionVerdict {
        match (self, other) {
            (RegionVerdict::Exact, v) | (v, RegionVerdict::Exact) => v,
            (RegionVerdict::Cover { depth: a }, RegionVerdict::Cover { depth: b }) => {
                RegionVerdict::Cover { depth: a.min(b) }
            }
        }
    }

    /// [`Self::coarser`] over two answers that may carry no verdict, where no region leaf was
    /// evaluated.
    pub fn coarsest(a: Option<RegionVerdict>, b: Option<RegionVerdict>) -> Option<RegionVerdict> {
        match (a, b) {
            (Some(a), Some(b)) => Some(a.coarser(b)),
            (a, b) => a.or(b),
        }
    }
}

/// One region leaf's answer for one request: its rows over the whole view, and the verdict.
#[derive(Debug, Clone)]
pub struct RegionRows {
    /// The rows inside the shape — interior tiles whole, boundary rows tested under the mask.
    /// Consumers intersect with the composed mask before any number is taken from it.
    pub rows: Bitmap,
    pub verdict: RegionVerdict,
}

/// The region cache's key: the canonical form's digest with everything the row set depends on.
///
/// `segments_version` is in the key so a stale entry is never usable (I11): a row-space artefact
/// keyed on anything else survives a merge that renumbered the rows under it. `prefix` for the
/// reason `RowProjectionKey` carries it. `max_cells` because the stop depth is a function of it.
/// **No principal in the key, deliberately** — see the module doc.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct RegionKey {
    pub view: String,
    pub prefix: String,
    pub segments_version: u64,
    pub digest: [u8; 16],
    pub max_cells: usize,
}

/// The first 128 bits of SHA-256 over a shape's canonical bytes — the content address.
pub(crate) fn digest_of(canonical: &[u8]) -> [u8; 16] {
    let full = Sha256::digest(canonical);
    let mut out = [0u8; 16];
    out.copy_from_slice(&full[..16]);
    out
}

/// A shape decomposed against one generation's row space — the cached, principal-free half.
pub struct RegionDecomposition {
    /// The canonical bytes, compared on every hit so a digest collision is detected rather than
    /// argued away (selection-operand §5).
    canonical: Vec<u8>,
    shape: Arc<Shape>,
    /// Every row of every interior tile — and, under a cover, every row of every cover tile —
    /// in view row space. Built from ranges; **never counted**.
    interior: Bitmap,
    /// The depth-16 cells the boundary crosses, ascending by code, each with the context its
    /// rows are tested against.
    boundary: Vec<BoundaryCell<PolyCtx>>,
    /// Per segment (in the request's segment order), parallel to `boundary`: the segment-local
    /// row range each boundary cell occupies.
    boundary_ranges: Vec<Vec<Range<u32>>>,
    verdict: RegionVerdict,
}

impl CacheWeight for RegionDecomposition {
    fn cache_weight_bytes(&self) -> u64 {
        let boundary: usize = self
            .boundary
            .iter()
            .map(|b| std::mem::size_of::<BoundaryCell<PolyCtx>>() + b.ctx.edges.len() * 4)
            .sum();
        let ranges: usize = self.boundary_ranges.iter().map(|r| r.len() * 8).sum();
        self.interior
            .get_serialized_size_in_bytes::<croaring::Portable>() as u64
            + boundary as u64
            + ranges as u64
            + self.canonical.len() as u64
            + std::mem::size_of::<Shape>() as u64
    }
}

impl RegionDecomposition {
    /// Decompose `shape` against `segments` under the cell budget. The descent and the range
    /// probes are the whole cost; no row is read.
    pub fn build(shape: Arc<Shape>, max_cells: usize, segments: &[(&SegmentData, u32)]) -> Self {
        let canonical = shape.encode();
        let prepared = shape.prepared();
        let decomposition = prepared.decompose(Some(max_cells));
        let verdict = match decomposition.cover_depth {
            Some(depth) => RegionVerdict::Cover { depth },
            None => RegionVerdict::Exact,
        };
        // Interior tiles whole; under a cover, the crossing tiles at the stop depth are taken as
        // inside too — the answer is then exact for a superset of the shape (§6).
        let whole: Vec<Tile> = decomposition
            .interior
            .iter()
            .chain(decomposition.cover.iter())
            .copied()
            .collect();
        let mut interior = Bitmap::new();
        for &(segment, base) in segments {
            for range in tile_ranges_all(segment, &whole) {
                if range.start < range.end {
                    interior.add_range(base + range.start..base + range.end);
                }
            }
        }
        interior.run_optimize();
        let mut boundary = decomposition.boundary;
        boundary.sort_by_key(|b| b.cell.raw());
        let cells: Vec<Tile> = boundary
            .iter()
            .map(|b| Tile {
                prefix: u64::from(b.cell.raw()),
                depth: 16,
            })
            .collect();
        let boundary_ranges = segments
            .iter()
            .map(|&(segment, _)| tile_ranges_all(segment, &cells))
            .collect();
        RegionDecomposition {
            canonical,
            shape,
            interior,
            boundary,
            boundary_ranges,
            verdict,
        }
    }

    /// Whether this entry was built from exactly these canonical bytes — the collision check.
    pub fn is_of(&self, canonical: &[u8]) -> bool {
        self.canonical == canonical
    }

    pub fn verdict(&self) -> RegionVerdict {
        self.verdict
    }

    /// How many boundary cells the descent held — for the traces.
    pub fn boundary_cells(&self) -> usize {
        self.boundary.len()
    }

    /// The rows inside the shape **for this mask**: the interior whole, and of each boundary
    /// cell's rows only those the composed mask admits, each recovered to its stored position
    /// through `unsplit32` and tested against the cell's own context.
    ///
    /// `mask` is the request's composed mask before its filter is attached — the same value
    /// every count is taken against. `segments` must be the slice the decomposition was built
    /// over, in the same order; the key guarantees the generation, and a segment count that
    /// disagrees is a coding error rather than a request the caller can provoke.
    pub fn rows_under(&self, mask: &EffectiveMask, segments: &[(&SegmentData, u32)]) -> Bitmap {
        debug_assert_eq!(segments.len(), self.boundary_ranges.len());
        let mut rows = self.interior.clone();
        if self.boundary.is_empty() {
            return rows;
        }
        let prepared = self.shape.prepared();
        for (s, &(segment, base)) in segments.iter().enumerate() {
            let Some(ranges) = self.boundary_ranges.get(s) else {
                continue;
            };
            let codes = segment.morton.u32();
            let residuals = segment.columns.residual();
            // The cells are ascending, so their ranges are; adjacent cells' ranges abut, and one
            // masked probe per run is the cheaper shape than one per cell. A row's own cell is
            // read back off the Morton column rather than remembered per range, so a run needs
            // no bookkeeping beyond its bounds.
            let mut run: Option<Range<u32>> = None;
            let mut runs: Vec<Range<u32>> = Vec::new();
            for range in ranges {
                if range.start >= range.end {
                    continue;
                }
                match &mut run {
                    Some(current) if range.start <= current.end => {
                        current.end = current.end.max(range.end);
                    }
                    _ => {
                        if let Some(current) = run.take() {
                            runs.push(current);
                        }
                        run = Some(range.clone());
                    }
                }
            }
            if let Some(current) = run {
                runs.push(current);
            }
            for run in runs {
                // Masked first: only the rows this principal is served are read at all.
                let visible = mask.rows_in_range(base + run.start..base + run.end);
                for row in visible.iter() {
                    let local = (row - base) as usize;
                    let cell = MortonCode::new(codes[local]);
                    let Ok(at) = self
                        .boundary
                        .binary_search_by_key(&cell.raw(), |b| b.cell.raw())
                    else {
                        // A run spans only boundary cells' ranges, which abut; a row between two
                        // that do not is unreachable, and a miss is answered as outside.
                        continue;
                    };
                    let position = unsplit32(cell, residuals[local]);
                    if prepared.contains_in_cell(
                        position,
                        Rect::of_cell(cell),
                        &self.boundary[at].ctx,
                    ) {
                        rows.add(row);
                    }
                }
            }
        }
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tessera_spatial::shape::{ShapeF64, Space};
    use tessera_spatial::Bounds;

    #[test]
    fn the_coarsest_of_several_answers_is_the_shallowest_cover_any_reached() {
        let cover = |depth| Some(RegionVerdict::Cover { depth });
        let exact = Some(RegionVerdict::Exact);
        assert_eq!(RegionVerdict::coarsest(None, None), None);
        assert_eq!(RegionVerdict::coarsest(exact, None), exact);
        assert_eq!(RegionVerdict::coarsest(None, cover(9)), cover(9));
        assert_eq!(RegionVerdict::coarsest(exact, cover(9)), cover(9));
        assert_eq!(RegionVerdict::coarsest(cover(4), cover(9)), cover(4));
    }

    fn extent() -> Bounds {
        Bounds {
            x_min: 0.0,
            x_max: 1000.0,
            y_min: 0.0,
            y_max: 1000.0,
        }
    }

    fn canonical_of(shape: ShapeF64) -> Arc<Shape> {
        Arc::new(
            shape
                .canonical(Space::View, &extent())
                .expect("a well-formed shape")
                .0,
        )
    }

    fn box_at(max_x: f64) -> Arc<Shape> {
        canonical_of(ShapeF64::Bbox {
            min_x: 100.0,
            min_y: 100.0,
            max_x,
            max_y: 800.0,
        })
    }

    /// **A cached decomposition answers for the bytes it was built from and for no others.**
    ///
    /// The region cache is keyed on a *truncated* digest — the first 128 bits of SHA-256
    /// ([`digest_of`]) — so the key alone cannot decide that an entry is the shape being asked
    /// for. What decides it is [`RegionDecomposition::is_of`], compared on every hit
    /// (`selection-operand.md` §5): a collision is detected and answered from a fresh
    /// decomposition rather than argued away. A collision cannot be forced in a test, so what is
    /// asserted here is the contract the guard rests on — the comparison is on content.
    ///
    /// Mutations this kills: `is_of` returning `true` unconditionally, or comparing anything
    /// derived from the bytes (a length, a digest) rather than the bytes, which is exactly the
    /// "the digest is the key, so the entry is the shape" simplification that would delete the
    /// guard.
    #[test]
    fn a_decomposition_is_of_its_own_canonical_bytes_and_of_no_others() {
        let shape = box_at(800.0);
        let other = box_at(801.0);
        let bytes = shape.encode();
        let other_bytes = other.encode();
        assert_ne!(
            bytes, other_bytes,
            "two shapes, or this test proves nothing"
        );

        let held = RegionDecomposition::build(Arc::clone(&shape), DEFAULT_MAX_REGION_CELLS, &[]);
        assert!(
            held.is_of(&bytes),
            "an entry is of the shape it was built from"
        );
        assert!(
            !held.is_of(&other_bytes),
            "another shape's bytes are another shape, whatever digest they hash to"
        );
        assert!(
            !held.is_of(&bytes[..bytes.len() - 1]),
            "a prefix of the bytes is not the shape either"
        );
    }
}
