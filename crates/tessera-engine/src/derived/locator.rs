use croaring::Bitmap;
use tessera_spatial::morton::unsplit32;
use tessera_store::read::SegmentData;
use tessera_types::MortonCode;

/// The segments of one view, ascending in `row_base`, resolving a view row to the segment holding
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

    /// The position of one view row, in grid units, or `None` for a row past every segment's
    /// extent.
    ///
    /// **`None` is dropped by the caller rather than defaulted**, because a position of `(0, 0)` is
    /// a real position on the map: a member the row space cannot place would otherwise pull every
    /// centroid towards the origin, which is a wrong answer that renders.
    pub fn position(&self, row: u32) -> Option<(u32, u32)> {
        let (segment, local) = self.resolve(row)?;
        let idx = local as usize;
        let cell = *segment.morton.u32().get(idx)?;
        let residual = *segment.columns.residual().get(idx)?;
        Some(unsplit32(MortonCode::new(cell), residual))
    }

    /// Every visible row's position, in row order — what [`compute`](super::compute) gathers, walked segment by
    /// segment instead of row by row.
    ///
    /// **The same answer as calling [`position`](Self::position) on each row, at a fraction of the
    /// cost, and it is the row-space property that makes it so.** Rows are Morton rank, and a
    /// segment holds a contiguous range of them, so the visible rows of one segment are a
    /// contiguous stretch of the mask: the segment is resolved once for the stretch rather than
    /// re-resolved for every row, and the two columns are read in ascending index order rather than
    /// through a reverse scan that starts again each time. Over the 197-artifact measurement layer
    /// at full membership this is a *measured* 168 ms → 44 ms for 12.8M members
    /// (`artifact-shapes.md` §7.1).
    ///
    /// A row past its segment's columns is dropped, as [`position`](Self::position) drops it and
    /// for the same reason — `(0, 0)` is a real position on the map. A segment's extent is clipped
    /// to the next segment's base, so a row that both could claim is resolved to the later one,
    /// which is what the reverse scan does.
    pub fn positions(&self, visible: &Bitmap) -> Vec<[u32; 2]> {
        let mut out: Vec<[u32; 2]> = Vec::with_capacity(visible.cardinality() as usize);
        for (i, &(segment, row_base)) in self.segments.iter().enumerate() {
            let morton = segment.morton.u32();
            let residual = segment.columns.residual();
            let rows = morton.len().min(residual.len()) as u64;
            let next = self
                .segments
                .get(i + 1)
                .map_or(u32::MAX as u64 + 1, |&(_, base)| base as u64);
            let hi = (row_base as u64 + rows).min(next);
            if hi <= row_base as u64 {
                continue;
            }
            let mut it = visible.iter();
            it.reset_at_or_after(row_base);
            // **Read the mask a block of rows at a time, not a row at a time.** The bitmap's
            // iterator crosses an FFI boundary on every `next`, which the compiler cannot inline
            // through, and this loop's whole body is two indexed loads and a bit permutation — so
            // the call was a *measured* third of the gather. `next_many` fills the block inside the
            // library and hands back a slice; the rows, and so the positions, are the same ones in
            // the same order.
            //
            // **A block is then read as its runs of consecutive rows**, which is what the mask
            // mostly holds: a membership is a cluster of a Morton-ordered corpus, and the largest
            // artifact of the measurement layer is 2,422,486 of 2,422,486 rows. A run is two
            // slices of the columns walked together, so the bit permutation runs over a contiguous
            // pair of arrays with no index arithmetic between elements and nothing to stop it
            // vectorising. Sparse masks fall out as runs of one and cost what the row-at-a-time
            // loop cost.
            let mut block = [0u32; 1_024];
            'segment: loop {
                let n = it.next_many(&mut block);
                if n == 0 {
                    break;
                }
                let mut i = 0usize;
                while i < n {
                    if block[i] as u64 >= hi {
                        break 'segment;
                    }
                    let mut j = i + 1;
                    while j < n && block[j] == block[j - 1] + 1 && (block[j] as u64) < hi {
                        j += 1;
                    }
                    let lo = (block[i] - row_base) as usize;
                    let run = lo..lo + (j - i);
                    out.extend(
                        morton[run.clone()]
                            .iter()
                            .zip(&residual[run])
                            .map(|(&cell, &sub)| {
                                let (qx, qy) = unsplit32(MortonCode::new(cell), sub);
                                [qx, qy]
                            }),
                    );
                    i = j;
                }
            }
        }
        out
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
