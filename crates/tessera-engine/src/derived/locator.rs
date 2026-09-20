use croaring::Bitmap;
use tessera_spatial::morton::unsplit32;
use tessera_store::read::SegmentData;
use tessera_types::MortonCode;

/// The segments of one view, ascending in `row_base`, resolving a view row to the segment holding
/// it. The same reverse-scan shape [`crate::select::SelectionParts::resolve_indexed`] uses: the
/// live segment count is a handful, so a scan beats a binary search.
pub struct RowLocator<'a> {
    segments: Vec<(&'a SegmentData, u32)>,
}

impl<'a> RowLocator<'a> {
    /// `segments` must be ascending in `row_base` ([`crate::viewport::segments_with_row_bases`]).
    pub fn new(segments: Vec<(&'a SegmentData, u32)>) -> Self {
        RowLocator { segments }
    }

    /// The position of one view row, in grid units, or `None` for a row past every segment's
    /// extent. `None` is dropped by the caller rather than defaulted: `(0, 0)` is a real position
    /// on the map, and would pull every centroid towards the origin.
    pub fn position(&self, row: u32) -> Option<(u32, u32)> {
        let (segment, local) = self.resolve(row)?;
        let idx = local as usize;
        let cell = *segment.morton.u32().get(idx)?;
        let residual = *segment.columns.residual().get(idx)?;
        Some(unsplit32(MortonCode::new(cell), residual))
    }

    /// Every visible row's position, in row order, walked segment by segment instead of row by row.
    /// The same answer as calling [`position`](Self::position) on each row, at a fraction of the
    /// cost: rows are Morton rank, so the visible rows of one segment are a contiguous stretch of
    /// the mask, resolved once rather than re-resolved for every row. Measured at 168 ms to 44 ms
    /// for 12.8M members.
    ///
    /// A row past its segment's columns is dropped, as [`position`](Self::position) drops it. A
    /// segment's extent is clipped to the next segment's base, so a row both could claim resolves
    /// to the later one.
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
            // Read the mask a block of rows at a time: the iterator crosses an FFI boundary on
            // every `next`, measured at a third of the gather's cost. A block is then read as its
            // runs of consecutive rows, which a run walks as two column slices together; a sparse
            // mask falls out as runs of one.
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
