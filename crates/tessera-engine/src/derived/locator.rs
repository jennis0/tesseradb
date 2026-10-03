use croaring::Bitmap;
use tessera_spatial::morton::unsplit32;
use tessera_store::read::SegmentData;
use tessera_types::MortonCode;

/// One segment's positions over its stretch of the view's rows: `[row_base, row_base + len)`,
/// where `len` is the shorter of its two position columns, cut short at the next segment's base.
/// A row past that stretch has no position.
#[derive(Clone, Copy)]
pub struct Placement<'a> {
    pub row_base: u32,
    morton: &'a [u32],
    residual: &'a [u32],
}

impl<'a> Placement<'a> {
    pub fn new(row_base: u32, morton: &'a [u32], residual: &'a [u32]) -> Self {
        let len = morton.len().min(residual.len());
        Placement {
            row_base,
            morton: &morton[..len],
            residual: &residual[..len],
        }
    }

    /// One past the last row this places.
    pub fn end(&self) -> u64 {
        u64::from(self.row_base) + self.morton.len() as u64
    }

    /// The grid position of `row`, which must be in `[row_base, end())`.
    #[inline]
    pub fn position(&self, row: u32) -> (u32, u32) {
        let at = (row - self.row_base) as usize;
        unsplit32(MortonCode::new(self.morton[at]), self.residual[at])
    }

    /// One per segment, ascending, each cut at the next one's base so a row both could claim
    /// belongs to the later one. `segments` must be ascending in `row_base`
    /// ([`crate::viewport::segments_with_row_bases`]).
    pub fn of_segments(segments: &[(&'a SegmentData, u32)]) -> Vec<Placement<'a>> {
        let columns: Vec<(u32, &'a [u32], &'a [u32])> = segments
            .iter()
            .map(|&(segment, row_base)| {
                (row_base, segment.morton.u32(), segment.columns.residual())
            })
            .collect();
        Self::of_columns(&columns)
    }

    /// [`Self::of_segments`] over each segment's base and its two position columns.
    pub fn of_columns(columns: &[(u32, &'a [u32], &'a [u32])]) -> Vec<Placement<'a>> {
        columns
            .iter()
            .enumerate()
            .map(|(i, &(row_base, morton, residual))| {
                let next = columns
                    .get(i + 1)
                    .map_or(u64::MAX, |&(base, _, _)| u64::from(base));
                let room = next.saturating_sub(u64::from(row_base));
                let len = (morton.len().min(residual.len()) as u64).min(room) as usize;
                Placement::new(row_base, &morton[..len], &residual[..len])
            })
            .collect()
    }
}

/// The position of `row` among `places` (ascending, as [`Placement::of_segments`] makes them), or
/// `None` for a row no segment places. `None` is dropped by the caller rather than defaulted:
/// `(0, 0)` is a real position on the map, and would pull every centroid towards the origin.
pub fn place(places: &[Placement<'_>], row: u32) -> Option<(u32, u32)> {
    let at = places.partition_point(|p| p.row_base <= row);
    let p = places.get(at.checked_sub(1)?)?;
    (u64::from(row) < p.end()).then(|| p.position(row))
}

/// The segments of one view, resolving a view row to its position.
pub struct RowLocator<'a> {
    places: Vec<Placement<'a>>,
}

impl<'a> RowLocator<'a> {
    /// `segments` must be ascending in `row_base` ([`crate::viewport::segments_with_row_bases`]).
    pub fn new(segments: Vec<(&'a SegmentData, u32)>) -> Self {
        RowLocator {
            places: Placement::of_segments(&segments),
        }
    }

    /// The position of one view row, in grid units, or `None` for a row no segment places.
    pub fn position(&self, row: u32) -> Option<(u32, u32)> {
        place(&self.places, row)
    }

    /// Every visible row's position, in row order, walked segment by segment instead of row by row.
    /// The same answer as calling [`position`](Self::position) on each row, at a fraction of the
    /// cost: rows are Morton rank, so the visible rows of one segment are a contiguous stretch of
    /// the mask, resolved once rather than re-resolved for every row. Measured at 168 ms to 44 ms
    /// for 12.8M members.
    pub fn positions(&self, visible: &Bitmap) -> Vec<[u32; 2]> {
        let mut out: Vec<[u32; 2]> = Vec::with_capacity(visible.cardinality() as usize);
        for place in &self.places {
            let hi = place.end();
            if hi <= u64::from(place.row_base) {
                continue;
            }
            let mut it = visible.iter();
            it.reset_at_or_after(place.row_base);
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
                    if u64::from(block[i]) >= hi {
                        break 'segment;
                    }
                    let mut j = i + 1;
                    while j < n && block[j] == block[j - 1] + 1 && u64::from(block[j]) < hi {
                        j += 1;
                    }
                    let lo = (block[i] - place.row_base) as usize;
                    let run = lo..lo + (j - i);
                    out.extend(
                        place.morton[run.clone()]
                            .iter()
                            .zip(&place.residual[run])
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
}
