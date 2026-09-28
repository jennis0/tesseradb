//! A set's rows counted by map cell, at a depth from 0 to 32, optionally split into groups.
//!
//! A cell at depth `d` is the top `2d` bits of a row's 64-bit position: its 32-bit cell code, which
//! orders the rows of a segment, followed by its 32-bit sub-cell residual. For `d <= 16` a cell is
//! therefore a contiguous row range of each segment, and there are two ways to count it:
//!
//! - **Range counts** ([`count_by_ranges`]): find each occupied cell's rows with a binary search of
//!   the cell codes and count the set over that range. The cost is per occupied cell.
//! - **The pass** ([`pass`]): walk the set's rows in row order and emit a cell when its prefix
//!   changes. The cost is per row of the set. Rows within one depth-16 cell are in `tessera_id`
//!   order rather than position order, so below depth 16 each depth-16 cell's rows are sorted by
//!   their finer prefix before they are counted.
//!
//! A view's rows lie in several segments, each in its own cell-code order. The pass cuts the view
//! into chunks by cell prefix, so one chunk holds every segment's rows of the same cells and a
//! cell's rows from several segments are added before it is emitted.

use std::ops::Range;

use croaring::Bitmap;
use rayon::prelude::*;
use rustc_hash::FxHashMap;
use tessera_store::read::{first_code_at_or_past, ScalarSlice, SegmentData};

use crate::compose::EffectiveMask;

/// About how many rows one chunk of the pass holds.
const CHUNK_ROWS: u64 = 1 << 20;

/// The rows being counted, in view row space.
#[derive(Clone, Copy)]
pub enum CellSet<'a> {
    /// A request's composed mask.
    Mask(&'a EffectiveMask),
    /// Rows already composed under a mask.
    Rows(&'a Bitmap),
}

impl CellSet<'_> {
    /// How many of the set's rows lie in `rows`.
    pub fn count(&self, rows: Range<u32>) -> u64 {
        match self {
            CellSet::Mask(mask) => mask.count_range(rows),
            CellSet::Rows(set) => set.range_cardinality(rows),
        }
    }

    fn for_each_run(&self, rows: Range<u32>, f: &mut impl FnMut(Range<u32>)) {
        match self {
            CellSet::Mask(mask) => mask.for_each_visible_run(rows, f),
            CellSet::Rows(set) => tessera_roaring::for_each_run_in(set, rows, f),
        }
    }
}

/// What [`count_by_ranges`] found: every non-empty cell ascending with its count, and how many
/// `(segment, cell)` ranges it counted to find them.
pub struct RangeCounts {
    pub cells: Vec<(u64, u64)>,
    pub visited: u64,
}

/// Count every depth-`depth` cell in `cells` over the segment-local row ranges `parts`, each given
/// with its segment and the segment's first row in view row space, by `count` over view rows.
/// Moves from one occupied cell to the next by binary search, so an empty cell costs nothing.
pub fn count_by_ranges<'a>(
    parts: impl IntoIterator<Item = (&'a SegmentData, u32, Range<u32>)>,
    depth: u8,
    cells: Range<u64>,
    count: &dyn Fn(Range<u32>) -> u64,
) -> RangeCounts {
    assert!(depth <= 16, "a cell deeper than 16 is not a row range");
    let shift = 32 - 2 * u32::from(depth);
    let mut found: Vec<(u64, u64)> = Vec::new();
    let mut visited = 0u64;
    let mut several = false;
    for (k, (segment, row_base, within)) in parts.into_iter().enumerate() {
        several |= k > 0;
        let codes = segment.morton.u32();
        let start = first_code_at_or_past(segment, cells.start << shift, within.clone());
        let end = first_code_at_or_past(segment, cells.end << shift, start..within.end) as usize;
        let mut i = start as usize;
        while i < end {
            let cell = u64::from(codes[i]) >> shift;
            let j = i + codes[i..end].partition_point(|&c| u64::from(c) >> shift <= cell);
            visited += 1;
            let n = count(row_base + i as u32..row_base + j as u32);
            if n > 0 {
                found.push((cell, n));
            }
            i = j;
        }
    }
    if several {
        found = add_equal_keys(found);
    }
    RangeCounts {
        cells: found,
        visited,
    }
}

/// Sort `(key, count)` pairs and add the counts of equal keys.
fn add_equal_keys<K: Ord + Copy>(mut pairs: Vec<(K, u64)>) -> Vec<(K, u64)> {
    pairs.sort_unstable_by_key(|&(key, _)| key);
    let mut out: Vec<(K, u64)> = Vec::with_capacity(pairs.len());
    for (key, n) in pairs {
        match out.last_mut() {
            Some((last, total)) if *last == key => *total += n,
            _ => out.push((key, n)),
        }
    }
    out
}

/// Which group each code of a drawn column counts in: a listed code in its position in the list,
/// code 0 in [`GroupTable::none`], and every other code in [`GroupTable::rest`].
pub struct GroupTable {
    listed: u32,
    lookup: Lookup,
}

enum Lookup {
    /// Indexed by code, for codes below 2^16.
    Dense(Vec<u32>),
    Sparse(FxHashMap<u32, u32>),
}

impl GroupTable {
    /// The table for `listed`, in order; a code listed twice keeps its first position.
    pub fn new(listed: &[u32]) -> GroupTable {
        let rest = listed.len() as u32;
        let lookup = if listed.iter().all(|&code| code < 1 << 16) {
            let mut dense = vec![rest; 1 << 16];
            for (group, &code) in listed.iter().enumerate().rev() {
                dense[code as usize] = group as u32;
            }
            Lookup::Dense(dense)
        } else {
            let mut sparse = FxHashMap::default();
            for (group, &code) in listed.iter().enumerate() {
                sparse.entry(code).or_insert(group as u32);
            }
            Lookup::Sparse(sparse)
        };
        GroupTable {
            listed: rest,
            lookup,
        }
    }

    /// The group of the codes not listed.
    pub fn rest(&self) -> u32 {
        self.listed
    }

    /// The group of the rows carrying no value.
    pub fn none(&self) -> u32 {
        self.listed + 1
    }

    #[inline]
    fn group(&self, code: u32) -> u32 {
        if code == tessera_store::vocabulary::ABSENT_CODE {
            return self.none();
        }
        match &self.lookup {
            Lookup::Dense(dense) => dense.get(code as usize).copied().unwrap_or(self.listed),
            Lookup::Sparse(sparse) => sparse.get(&code).copied().unwrap_or(self.listed),
        }
    }
}

/// Where the pass reads each row's group.
pub enum RowGroups<'a> {
    /// Every row in group 0.
    None,
    /// Each row's code in a column drawn in the view, from `columns.arrow`, through `table`.
    /// `codes[s]` is segment `s`'s column, `None` where the segment holds no such column and so
    /// every row of it carries no value.
    Drawn {
        codes: Vec<Option<ScalarSlice<'a>>>,
        table: &'a GroupTable,
    },
}

impl<'a> RowGroups<'a> {
    /// The groups of a column drawn in `segments`, by `table`.
    pub fn drawn(
        segments: &[(&'a SegmentData, u32)],
        column: &str,
        table: &'a GroupTable,
    ) -> RowGroups<'a> {
        RowGroups::Drawn {
            codes: segments
                .iter()
                .map(|(segment, _)| segment.columns.scalar(column))
                .collect(),
            table,
        }
    }

    /// How many groups a row can fall in, numbered from 0.
    fn count(&self) -> usize {
        match self {
            RowGroups::None => 1,
            RowGroups::Drawn { table, .. } => table.none() as usize + 1,
        }
    }

    #[inline]
    fn group(&self, segment: usize, local: usize) -> u32 {
        match self {
            RowGroups::None => 0,
            RowGroups::Drawn { codes, table } => {
                let code = match &codes[segment] {
                    Some(ScalarSlice::U8(v)) => u32::from(v[local]),
                    Some(ScalarSlice::U16(v)) => u32::from(v[local]),
                    Some(ScalarSlice::U32(v)) => v[local],
                    _ => tessera_store::vocabulary::ABSENT_CODE,
                };
                table.group(code)
            }
        }
    }
}

/// One cell of a pass's table: a depth-`d` cell, a group, and how many of the set's rows are both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CellCount {
    pub cell: u64,
    pub group: u32,
    pub count: u64,
}

/// Count the set's rows by depth-`depth` cell (0 to 32) and group, in one pass over its rows on the
/// current rayon pool. Entries ascend by cell then group, and only non-empty ones appear.
/// `segments` are the view's, each with its first row in view row space.
pub fn pass(
    set: CellSet<'_>,
    segments: &[(&SegmentData, u32)],
    depth: u8,
    groups: &RowGroups<'_>,
) -> Vec<CellCount> {
    pass_in_chunks(set, segments, depth, groups, CHUNK_ROWS)
}

/// [`pass`] with chunks of about `chunk_rows` rows, so a test can cut a small view into many.
#[doc(hidden)]
pub fn pass_in_chunks(
    set: CellSet<'_>,
    segments: &[(&SegmentData, u32)],
    depth: u8,
    groups: &RowGroups<'_>,
    chunk_rows: u64,
) -> Vec<CellCount> {
    assert!(depth <= 32, "a position has 32 levels");
    let chunks = chunks(segments, depth.min(16), chunk_rows);
    let parts: Vec<Vec<CellCount>> = chunks
        .par_iter()
        .map(|prefixes| {
            let coarse = u32::from(depth.min(16));
            let shift = 32 - 2 * coarse;
            let mut entries: Vec<CellCount> = Vec::new();
            for (s, &(segment, row_base)) in segments.iter().enumerate() {
                let n = segment.row_count;
                let lo = first_code_at_or_past(segment, prefixes.start << shift, 0..n);
                let hi = first_code_at_or_past(segment, prefixes.end << shift, lo..n);
                if lo < hi {
                    let rows = row_base + lo..row_base + hi;
                    count_segment(set, segment, s, row_base, rows, depth, groups, &mut entries);
                }
            }
            if segments.len() > 1 {
                entries = add_equal_keys(
                    entries
                        .into_iter()
                        .map(|e| ((e.cell, e.group), e.count))
                        .collect(),
                )
                .into_iter()
                .map(|((cell, group), count)| CellCount { cell, group, count })
                .collect();
            }
            entries
        })
        .collect();
    parts.concat()
}

/// Ranges of depth-`coarse` cell prefixes, ascending and together covering every cell, each
/// holding about `chunk_rows` rows. Cut at the largest segment's quantiles.
fn chunks(segments: &[(&SegmentData, u32)], coarse: u8, chunk_rows: u64) -> Vec<Range<u64>> {
    let cells = 1u64 << (2 * u32::from(coarse));
    let total: u64 = segments.iter().map(|(s, _)| u64::from(s.row_count)).sum();
    let Some((largest, _)) = segments.iter().max_by_key(|(s, _)| s.row_count) else {
        return Vec::new();
    };
    let codes = largest.morton.u32();
    let shift = 32 - 2 * u32::from(coarse);
    let k = total.div_ceil(chunk_rows.max(1)).max(1);
    let mut bounds: Vec<u64> = vec![0];
    for i in 1..k {
        let at = (i * codes.len() as u64 / k) as usize;
        let prefix = u64::from(codes[at]) >> shift;
        if prefix > *bounds.last().expect("bounds start at 0") {
            bounds.push(prefix);
        }
    }
    bounds.push(cells);
    bounds.windows(2).map(|w| w[0]..w[1]).collect()
}

/// Add to `out` the counts of one segment's rows `rows` (view row space), whose depth-16 cells
/// the chunk holds whole.
#[allow(clippy::too_many_arguments)]
fn count_segment(
    set: CellSet<'_>,
    segment: &SegmentData,
    s: usize,
    row_base: u32,
    rows: Range<u32>,
    depth: u8,
    groups: &RowGroups<'_>,
    out: &mut Vec<CellCount>,
) {
    let morton = segment.morton.u32();
    if depth <= 16 {
        let shift = 32 - 2 * u32::from(depth);
        // One counter per group, and the groups the current cell has touched.
        let mut counts = vec![0u64; groups.count()];
        let mut touched: Vec<u32> = Vec::new();
        let mut current: Option<u64> = None;
        let emit =
            |at: u64, counts: &mut [u64], touched: &mut Vec<u32>, out: &mut Vec<CellCount>| {
                touched.sort_unstable();
                for &group in touched.iter() {
                    out.push(CellCount {
                        cell: at,
                        group,
                        count: std::mem::take(&mut counts[group as usize]),
                    });
                }
                touched.clear();
            };
        set.for_each_run(rows, &mut |run| {
            for row in run {
                let i = (row - row_base) as usize;
                let at = u64::from(morton[i]) >> shift;
                if current != Some(at) {
                    if let Some(done) = current {
                        emit(done, &mut counts, &mut touched, out);
                    }
                    current = Some(at);
                }
                let group = groups.group(s, i);
                let slot = &mut counts[group as usize];
                if *slot == 0 {
                    touched.push(group);
                }
                *slot += 1;
            }
        });
        if let Some(done) = current {
            emit(done, &mut counts, &mut touched, out);
        }
    } else {
        let residual = segment.columns.residual();
        let shift = 64 - 2 * u32::from(depth);
        let mut cell: Vec<((u64, u32), u64)> = Vec::new();
        let mut current: Option<u32> = None;
        let emit = |cell: &mut Vec<((u64, u32), u64)>, out: &mut Vec<CellCount>| {
            let tallied = add_equal_keys(std::mem::take(cell));
            out.extend(tallied.into_iter().map(|((at, group), count)| CellCount {
                cell: at,
                group,
                count,
            }));
        };
        set.for_each_run(rows, &mut |run| {
            for row in run {
                let i = (row - row_base) as usize;
                if current != Some(morton[i]) {
                    emit(&mut cell, out);
                    current = Some(morton[i]);
                }
                let position = (u64::from(morton[i]) << 32) | u64::from(residual[i]);
                cell.push(((position >> shift, groups.group(s, i)), 1));
            }
        });
        emit(&mut cell, out);
    }
}
