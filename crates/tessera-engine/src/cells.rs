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
use tessera_store::RowEntities;

use crate::compose::EffectiveMask;
use crate::filter::EntityCodes;
use crate::row_column::RowColumn;

/// The most rows one chunk of the pass holds.
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

    /// Whether the set holds view row `row`.
    pub(crate) fn contains(&self, row: u32) -> bool {
        match self {
            CellSet::Mask(mask) => mask.contains_row(row),
            CellSet::Rows(set) => set.contains(row),
        }
    }

    pub(crate) fn for_each_run(&self, rows: Range<u32>, f: &mut impl FnMut(Range<u32>)) {
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
    /// Indexed by code, for codes below 2^16, code 0 included.
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
            dense[tessera_store::vocabulary::ABSENT_CODE as usize] = rest + 1;
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
        match &self.lookup {
            Lookup::Dense(dense) => dense.get(code as usize).copied().unwrap_or(self.listed),
            Lookup::Sparse(sparse) => {
                if code == tessera_store::vocabulary::ABSENT_CODE {
                    return self.none();
                }
                sparse.get(&code).copied().unwrap_or(self.listed)
            }
        }
    }
}

/// Which group each artifact of a row-major level counts in: a listed artifact in its position
/// in the list, one served and not listed in [`LabelTable::rest`], and one not served in none.
pub struct LabelTable {
    listed: u32,
    by_ordinal: Vec<u32>,
}

/// An ordinal no group counts.
const UNSERVED: u32 = u32::MAX;

impl LabelTable {
    /// The table over `ordinals` ordinals, `served` the ordinals served and `listed` those listed,
    /// in order. A listed ordinal past `ordinals` stands for no artifact and counts nothing.
    pub(crate) fn new(
        ordinals: usize,
        served: impl IntoIterator<Item = u32>,
        listed: &[u32],
    ) -> Self {
        let rest = listed.len() as u32;
        let mut by_ordinal = vec![UNSERVED; ordinals];
        for ordinal in served {
            if let Some(slot) = by_ordinal.get_mut(ordinal as usize) {
                *slot = rest;
            }
        }
        for (group, &ordinal) in listed.iter().enumerate().rev() {
            if let Some(slot) = by_ordinal.get_mut(ordinal as usize) {
                *slot = group as u32;
            }
        }
        LabelTable {
            listed: rest,
            by_ordinal,
        }
    }

    /// The group of the rows in a served artifact and in no listed one.
    pub(crate) fn rest(&self) -> u32 {
        self.listed
    }

    /// The group of the rows in no served artifact.
    pub(crate) fn none(&self) -> u32 {
        self.listed + 1
    }
}

/// Where the pass reads each row's groups.
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
    /// Each row's entity's code in an indexed column, through `table`: the row's entity from
    /// `tables`, the entity's code from `codes`.
    Entity {
        tables: RowEntities<'a>,
        codes: &'a EntityCodes<'a>,
        table: &'a GroupTable,
    },
    /// The artifacts a row-major level labels each row with, through `table`: every listed one
    /// labelling it, else the rest where a served one does, else none. A row can count in
    /// several listed groups.
    Labels {
        column: &'a RowColumn,
        table: &'a LabelTable,
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

    /// The groups of an indexed column's codes, read through each row's entity, by `table`.
    pub fn entity(
        tables: RowEntities<'a>,
        codes: &'a EntityCodes<'a>,
        table: &'a GroupTable,
    ) -> RowGroups<'a> {
        RowGroups::Entity {
            tables,
            codes,
            table,
        }
    }

    /// How many groups a row can fall in, numbered from 0.
    pub(crate) fn count(&self) -> usize {
        match self {
            RowGroups::None => 1,
            RowGroups::Drawn { table, .. } | RowGroups::Entity { table, .. } => {
                table.none() as usize + 1
            }
            RowGroups::Labels { table, .. } => table.none() as usize + 1,
        }
    }
}

/// One segment's way of naming a row's groups, resolved once per segment so the walk over its rows
/// is compiled for it.
trait Grouper {
    /// Call `f` with each group of the row at `local` in its segment, `row` in the view.
    fn each(&self, local: usize, row: u32, f: impl FnMut(u32));
}

/// Every row in one group.
struct Constant(u32);

impl Grouper for Constant {
    #[inline(always)]
    fn each(&self, _: usize, _: u32, mut f: impl FnMut(u32)) {
        f(self.0)
    }
}

/// One group of another grouper, its other groups passed over.
struct Only<'g, G> {
    inner: &'g G,
    group: u32,
}

impl<G: Grouper> Grouper for Only<'_, G> {
    #[inline(always)]
    fn each(&self, local: usize, row: u32, mut f: impl FnMut(u32)) {
        let group = self.group;
        self.inner.each(local, row, |g| {
            if g == group {
                f(g)
            }
        })
    }
}

/// A drawn code of one width, through a table.
struct Drawn<'a, T> {
    codes: &'a [T],
    table: &'a GroupTable,
}

impl<T: Copy + Into<u32>> Grouper for Drawn<'_, T> {
    #[inline(always)]
    fn each(&self, local: usize, _: u32, mut f: impl FnMut(u32)) {
        f(self.table.group(self.codes[local].into()))
    }
}

/// An indexed code read through the row's entity.
struct ByEntity<'a> {
    tables: RowEntities<'a>,
    codes: &'a EntityCodes<'a>,
    table: &'a GroupTable,
}

impl Grouper for ByEntity<'_> {
    #[inline(always)]
    fn each(&self, _: usize, row: u32, mut f: impl FnMut(u32)) {
        f(self
            .table
            .group(self.codes.code_of(self.tables.entity_of(row))))
    }
}

/// A row-major level's labels.
struct ByLabel<'a> {
    column: &'a RowColumn,
    table: &'a LabelTable,
}

impl Grouper for ByLabel<'_> {
    #[inline(always)]
    fn each(&self, _: usize, row: u32, mut f: impl FnMut(u32)) {
        let rest = self.table.rest();
        let mut served = false;
        let mut listed = false;
        self.column.for_each_label(row, |ordinal| {
            match self.table.by_ordinal.get(ordinal as usize) {
                Some(&group) if group < rest => {
                    listed = true;
                    f(group);
                }
                Some(&group) if group == rest => served = true,
                _ => {}
            }
        });
        if !listed {
            f(if served { rest } else { self.table.none() });
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

/// Where a count's entries go, one call per non-empty `(cell, group)`, ascending by cell then
/// group within a chunk.
pub(crate) trait CellSink {
    fn push(&mut self, cell: u64, group: u32, count: u64);
}

impl CellSink for Vec<CellCount> {
    #[inline]
    fn push(&mut self, cell: u64, group: u32, count: u64) {
        Vec::push(self, CellCount { cell, group, count });
    }
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
    let rows: u64 = segments.iter().map(|(s, _)| u64::from(s.row_count)).sum();
    let pieces = rayon::current_num_threads() as u64 * 4;
    let chunk_rows = rows.div_ceil(pieces.max(1)).clamp(1 << 14, CHUNK_ROWS);
    pass_in_chunks(set, segments, depth, groups, chunk_rows)
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
    let parts: Vec<Vec<CellCount>> = chunks(segments, depth, chunk_rows)
        .par_iter()
        .map(|prefixes| count_chunk(set, segments, depth, groups, None, prefixes.clone()))
        .collect();
    parts.concat()
}

/// Ranges of depth-`min(depth, 16)` cell prefixes, ascending and together covering every cell,
/// each holding about `chunk_rows` rows of the view. Cut at the largest segment's quantiles.
pub(crate) fn chunks(
    segments: &[(&SegmentData, u32)],
    depth: u8,
    chunk_rows: u64,
) -> Vec<Range<u64>> {
    let coarse = depth.min(16);
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

/// The table of one chunk from [`chunks`]: the set's rows whose depth-`min(depth, 16)` prefix is in
/// `prefixes`, counted by depth-`depth` cell and group, ascending by cell then group. With
/// `only`, the rows of that group alone are counted; the others are read and passed over.
pub(crate) fn count_chunk(
    set: CellSet<'_>,
    segments: &[(&SegmentData, u32)],
    depth: u8,
    groups: &RowGroups<'_>,
    only: Option<u32>,
    prefixes: Range<u64>,
) -> Vec<CellCount> {
    let mut out = Vec::new();
    count_chunk_into(set, segments, depth, groups, only, prefixes, &mut out);
    out
}

/// How many of the set's rows lie in the chunk `prefixes` from [`chunks`]: the most entries one
/// group's count of it can hold.
pub(crate) fn rows_in_chunk(
    set: CellSet<'_>,
    segments: &[(&SegmentData, u32)],
    depth: u8,
    prefixes: Range<u64>,
) -> u64 {
    let shift = 32 - 2 * u32::from(depth.min(16));
    segments
        .iter()
        .map(|&(segment, row_base)| {
            let n = segment.row_count;
            let lo = first_code_at_or_past(segment, prefixes.start << shift, 0..n);
            let hi = first_code_at_or_past(segment, prefixes.end << shift, lo..n);
            set.count(row_base + lo..row_base + hi)
        })
        .sum()
}

/// [`count_chunk`] into `out`. Where the chunk's rows lie in one segment its entries go straight
/// to `out`; a cell's rows from several segments are added first.
pub(crate) fn count_chunk_into(
    set: CellSet<'_>,
    segments: &[(&SegmentData, u32)],
    depth: u8,
    groups: &RowGroups<'_>,
    only: Option<u32>,
    prefixes: Range<u64>,
    out: &mut impl CellSink,
) {
    let shift = 32 - 2 * u32::from(depth.min(16));
    let parts: Vec<(usize, Range<u32>)> = segments
        .iter()
        .enumerate()
        .filter_map(|(s, &(segment, row_base))| {
            let n = segment.row_count;
            let lo = first_code_at_or_past(segment, prefixes.start << shift, 0..n);
            let hi = first_code_at_or_past(segment, prefixes.end << shift, lo..n);
            (lo < hi).then(|| (s, row_base + lo..row_base + hi))
        })
        .collect();
    if let [(s, rows)] = &parts[..] {
        count_part(set, segments, *s, rows.clone(), depth, groups, only, out);
        return;
    }
    let mut entries: Vec<CellCount> = Vec::new();
    for (s, rows) in parts {
        count_part(set, segments, s, rows, depth, groups, only, &mut entries);
    }
    entries.sort_unstable_by_key(|e| (e.cell, e.group));
    let mut i = 0;
    while i < entries.len() {
        let (cell, group) = (entries[i].cell, entries[i].group);
        let mut count = 0;
        while i < entries.len() && entries[i].cell == cell && entries[i].group == group {
            count += entries[i].count;
            i += 1;
        }
        out.push(cell, group, count);
    }
}

/// One segment's rows `rows` of a chunk, into `out`, the grouper resolved for the segment.
#[allow(clippy::too_many_arguments)]
fn count_part(
    set: CellSet<'_>,
    segments: &[(&SegmentData, u32)],
    s: usize,
    rows: Range<u32>,
    depth: u8,
    groups: &RowGroups<'_>,
    only: Option<u32>,
    out: &mut impl CellSink,
) {
    let (segment, row_base) = segments[s];
    let width = groups.count();
    match groups {
        RowGroups::None => count_segment_alone(set, segment, row_base, rows, depth, out),
        RowGroups::Drawn { codes, table } => match &codes[s] {
            Some(ScalarSlice::U8(codes)) => {
                let g = Drawn { codes, table };
                count_as(set, segment, row_base, rows, depth, width, &g, only, out)
            }
            Some(ScalarSlice::U16(codes)) => {
                let g = Drawn { codes, table };
                count_as(set, segment, row_base, rows, depth, width, &g, only, out)
            }
            Some(ScalarSlice::U32(codes)) => {
                let g = Drawn { codes, table };
                count_as(set, segment, row_base, rows, depth, width, &g, only, out)
            }
            _ => {
                let g = Constant(table.none());
                count_as(set, segment, row_base, rows, depth, width, &g, only, out)
            }
        },
        RowGroups::Entity {
            tables,
            codes,
            table,
        } => {
            let g = ByEntity {
                tables: *tables,
                codes,
                table,
            };
            count_as(set, segment, row_base, rows, depth, width, &g, only, out)
        }
        RowGroups::Labels { column, table } => {
            let g = ByLabel { column, table };
            count_as(set, segment, row_base, rows, depth, width, &g, only, out)
        }
    }
}

/// [`count_chunk`] by range counts, for a depth of 16 or less and every row in group 0: the
/// occupied cells of `prefixes` found by binary search and each counted over its row range.
pub(crate) fn count_chunk_by_ranges(
    set: CellSet<'_>,
    segments: &[(&SegmentData, u32)],
    depth: u8,
    prefixes: Range<u64>,
) -> RangeCounts {
    count_by_ranges(
        segments
            .iter()
            .map(|&(segment, row_base)| (segment, row_base, 0..segment.row_count)),
        depth,
        prefixes,
        &|rows| set.count(rows),
    )
}

/// [`count_segment`] over `groups`, or over its group `only` where one is named.
#[allow(clippy::too_many_arguments)]
fn count_as<G: Grouper>(
    set: CellSet<'_>,
    segment: &SegmentData,
    row_base: u32,
    rows: Range<u32>,
    depth: u8,
    width: usize,
    groups: &G,
    only: Option<u32>,
    out: &mut impl CellSink,
) {
    match only {
        None => count_segment(set, segment, row_base, rows, depth, width, groups, out),
        Some(group) => {
            let only = Only {
                inner: groups,
                group,
            };
            count_segment(set, segment, row_base, rows, depth, width, &only, out)
        }
    }
}

/// [`count_segment`] with every row in group 0: a run of rows is read as a slice of cell codes and
/// a cell's rows are counted as the run of equal prefixes they are.
fn count_segment_alone(
    set: CellSet<'_>,
    segment: &SegmentData,
    row_base: u32,
    rows: Range<u32>,
    depth: u8,
    out: &mut impl CellSink,
) {
    let morton = segment.morton.u32();
    if depth <= 16 {
        let shift = 32 - 2 * u32::from(depth);
        let mut cell = u64::MAX;
        let mut n = 0u64;
        set.for_each_run(rows, &mut |run| {
            let codes = &morton[(run.start - row_base) as usize..(run.end - row_base) as usize];
            for &code in codes {
                let at = u64::from(code) >> shift;
                if at != cell {
                    if n > 0 {
                        out.push(cell, 0, n);
                    }
                    cell = at;
                    n = 0;
                }
                n += 1;
            }
        });
        if n > 0 {
            out.push(cell, 0, n);
        }
        return;
    }
    // Rows of one depth-16 cell are in `tessera_id` order, so their finer prefixes are sorted
    // before they are counted.
    let residual = segment.columns.residual();
    let shift = 64 - 2 * u32::from(depth);
    let mut cell16: Option<u32> = None;
    let mut prefixes: Vec<u64> = Vec::new();
    let flush = |prefixes: &mut Vec<u64>, out: &mut _| {
        emit_sorted(prefixes, |&p| (p, 0), out);
    };
    set.for_each_run(rows, &mut |run| {
        let lo = (run.start - row_base) as usize;
        let hi = (run.end - row_base) as usize;
        for i in lo..hi {
            if cell16 != Some(morton[i]) {
                flush(&mut prefixes, out);
                cell16 = Some(morton[i]);
            }
            prefixes.push(((u64::from(morton[i]) << 32) | u64::from(residual[i])) >> shift);
        }
    });
    flush(&mut prefixes, out);
}

/// Sort `entries`, push each run of equal `(cell, group)` keys to `out` with its length, and empty
/// `entries` for reuse.
fn emit_sorted<T: Ord + Copy>(
    entries: &mut Vec<T>,
    key: impl Fn(&T) -> (u64, u32),
    out: &mut impl CellSink,
) {
    entries.sort_unstable();
    let mut i = 0;
    while i < entries.len() {
        let at = entries[i];
        let j = i + entries[i..].partition_point(|e| *e == at);
        let (cell, group) = key(&at);
        out.push(cell, group, (j - i) as u64);
        i = j;
    }
    entries.clear();
}

/// Add to `out` the counts of one segment's rows `rows` (view row space), whose depth-16 cells
/// the chunk holds whole, each row in the groups `groups` names, of which there are `width`.
#[allow(clippy::too_many_arguments)]
fn count_segment<G: Grouper>(
    set: CellSet<'_>,
    segment: &SegmentData,
    row_base: u32,
    rows: Range<u32>,
    depth: u8,
    width: usize,
    groups: &G,
    out: &mut impl CellSink,
) {
    let morton = segment.morton.u32();
    if depth <= 16 {
        let shift = 32 - 2 * u32::from(depth);
        // One counter per group, and the groups the current cell has touched.
        let mut counts = vec![0u64; width];
        let mut touched: Vec<u32> = Vec::new();
        let mut current = u64::MAX;
        let emit = |at: u64, counts: &mut [u64], touched: &mut Vec<u32>, out: &mut _| {
            touched.sort_unstable();
            for &group in touched.iter() {
                CellSink::push(out, at, group, std::mem::take(&mut counts[group as usize]));
            }
            touched.clear();
        };
        set.for_each_run(rows, &mut |run| {
            for row in run {
                let i = (row - row_base) as usize;
                let at = u64::from(morton[i]) >> shift;
                if at != current {
                    emit(current, &mut counts, &mut touched, out);
                    current = at;
                }
                groups.each(i, row, |group| {
                    let slot = &mut counts[group as usize];
                    if *slot == 0 {
                        touched.push(group);
                    }
                    *slot += 1;
                });
            }
        });
        emit(current, &mut counts, &mut touched, out);
    } else {
        let residual = segment.columns.residual();
        let shift = 64 - 2 * u32::from(depth);
        let mut cell: Vec<(u64, u32)> = Vec::new();
        let mut current: Option<u32> = None;
        set.for_each_run(rows, &mut |run| {
            for row in run {
                let i = (row - row_base) as usize;
                if current != Some(morton[i]) {
                    emit_sorted(&mut cell, |&e| e, out);
                    current = Some(morton[i]);
                }
                let position = (u64::from(morton[i]) << 32) | u64::from(residual[i]);
                groups.each(i, row, |group| cell.push((position >> shift, group)));
            }
        });
        emit_sorted(&mut cell, |&e| e, out);
    }
}

#[cfg(test)]
mod tests {
    use super::GroupTable;

    /// A listed code falls in its first position in the list, code 0 in none, and every other code
    /// in the rest, whether the table is indexed by code or a map.
    #[test]
    fn the_group_table_sends_listed_codes_to_their_place_and_the_rest_to_rest() {
        for listed in [[7u32, 300, 7, 65_535], [7, 300, 7, 4_000_000_000]] {
            let table = GroupTable::new(&listed);
            assert_eq!((table.rest(), table.none()), (4, 5));
            assert_eq!(
                table.group(7),
                0,
                "a code listed twice keeps its first place"
            );
            assert_eq!(table.group(300), 1);
            assert_eq!(table.group(listed[3]), 3);
            assert_eq!(table.group(0), table.none());
            for unlisted in [1, 8, 65_534, 70_000, u32::MAX] {
                assert_eq!(table.group(unlisted), table.rest(), "code {unlisted}");
            }
        }
    }
}
