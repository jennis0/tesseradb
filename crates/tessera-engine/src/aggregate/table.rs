//! One grouping's table: its groups, its rows from a position, and its Arrow batches.
//!
//! A table's rows are its groups in order, the listed ones then the rest then none, and within a
//! group its cells ascending. A row appears where the set or the reference holds an item of it,
//! and a named value this viewer may be told of appears with no item. A page takes the rows after
//! its position up to the page's rows and bytes, and the position moves to its last row.

use std::sync::Arc;

use arrow::array::{
    ArrayRef, DictionaryArray, Float64Builder, Int8Array, StringArray, StringBuilder, UInt64Array,
    UInt64Builder,
};
use arrow::datatypes::{Field as ArrowField, Int8Type, Schema};
use arrow::record_batch::RecordBatch;

use rayon::prelude::*;

use super::artifacts::Layer;
use super::cursor::Position;
use super::set::Cx;
use super::values::Field;
use super::{AggregateTimings, By, Grouping, TableHead};
use crate::cells::{count_chunk, count_chunk_by_ranges, CellCount, CellSet, GroupTable, RowGroups};
use crate::engine::Engine;
use crate::error::{EngineError, Result};
use crate::records::{same_publication, RecordsLimits};
use crate::session::Session;
use crate::Generation;

/// A grouping resolved against the request's generation.
pub(super) struct Plan {
    grouping: u32,
    outer: Outer,
    cells: Option<u8>,
}

enum Outer {
    None,
    Field(Field),
    Layer(Layer),
}

/// One page of a table.
pub(super) struct Page {
    /// The table's head as this page counted it.
    pub(super) head: TableHead,
    pub(super) batch: RecordBatch,
    pub(super) bytes: usize,
    /// Whether the page stopped short of its rows for its bytes.
    pub(super) cut_by_bytes: bool,
    /// Where the next page starts.
    pub(super) next: Position,
}

/// A table's groups as one page counts them.
pub(super) struct Groups {
    /// The listed groups, as the cursor carries them.
    pub(super) chosen: Vec<u64>,
    /// For each group, the listed ones then the rest then none: its items in the set and in the
    /// reference.
    pub(super) sizes: Vec<(u64, u64)>,
    /// For each listed group, whether its row appears with no item.
    pub(super) always: Vec<bool>,
    /// For each listed group, its key.
    pub(super) keys: Vec<Key>,
    /// For each listed group, its title, on a field.
    pub(super) titles: Option<Vec<Option<String>>>,
    /// The groups with an item in the set, before the cut.
    pub(super) distinct: u64,
}

/// A listed group's key as a row carries it.
#[derive(Debug, Clone)]
pub(super) enum Key {
    Text(String),
    Id(u64),
}

/// The rows a page of a cell table counted past its end, held for the next page of the same
/// response while the publication and the mask they were counted under hold.
pub(super) struct Spill {
    at: Position,
    under: Arc<Generation>,
    mask: crate::histogram::MaskIdentity,
    rows: Vec<Row>,
    /// Whether the rows run to the table's end.
    complete: bool,
}

impl Spill {
    /// Whether these rows are the next page's from `position` under `cx`, and enough for it.
    fn holds(&self, cx: &Cx<'_>, position: &Position, limit: usize) -> bool {
        self.at == *position
            && same_publication(&self.under, cx.generation)
            && self.mask == cx.open.served.mask_identity
            && (self.complete || self.rows.len() > limit)
    }
}

/// One row of a table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Row {
    /// The group, as a position in the listed groups followed by the rest and none.
    group: u32,
    /// The cell, on a table with cells; 0 otherwise.
    cell: u64,
    count: u64,
    reference: u64,
}

impl Plan {
    /// `grouping`, the request's `index`th, resolved in `view`, or the refusal it earns.
    pub(super) fn of(
        engine: &Engine,
        session: &Session,
        generation: &Generation,
        view: &str,
        index: usize,
        grouping: &Grouping,
    ) -> Result<Plan> {
        let outer = match &grouping.by {
            None => Outer::None,
            Some(By::Field { column, pick }) => Outer::Field(Field::of(generation, column, pick)?),
            Some(By::Layer { layer, level, pick }) => Outer::Layer(Layer::of(
                engine, session, generation, view, layer, *level, pick,
            )?),
        };
        Ok(Plan {
            grouping: index as u32,
            outer,
            cells: grouping.cells,
        })
    }

    /// Whether this table counts a set by its rows rather than its entities.
    pub(super) fn wants_rows(&self) -> bool {
        self.cells.is_some()
            || match &self.outer {
                Outer::None => false,
                Outer::Field(field) => field.wants_rows(),
                Outer::Layer(_) => true,
            }
    }

    /// The page of this table starting at `position`.
    pub(super) fn page(
        &self,
        cx: &Cx<'_>,
        position: &Position,
        page_rows: u32,
        limits: &RecordsLimits,
        timings: &mut AggregateTimings,
        spill: &mut Option<Spill>,
    ) -> Result<Page> {
        cx.check_cancelled()?;
        let mut served = None;
        let groups = match &self.outer {
            Outer::None => Groups::whole(cx),
            Outer::Field(field) => field.groups(cx, position.chosen.as_deref(), timings)?,
            Outer::Layer(layer) => {
                let (groups, held) = layer.groups(cx, position.chosen.as_deref(), timings)?;
                served = Some(held);
                groups
            }
        };
        let head = TableHead {
            grouping: self.grouping,
            total: cx.sets.set.size(),
            reference_total: cx.sets.reference.as_ref().map(|r| r.size()),
            groups: (!matches!(self.outer, Outer::None)).then_some(groups.distinct),
            resumed: position.chosen.is_some(),
        };
        let limit = page_rows as usize;
        let (mut rows, complete) = match self.cells {
            None => listed_rows(&groups, position.group, limit),
            Some(depth) => match spill.take().filter(|held| held.holds(cx, position, limit)) {
                Some(held) => (held.rows, held.complete),
                None => {
                    let started = std::time::Instant::now();
                    let table;
                    let codes;
                    let labels;
                    let parts;
                    let source = match &self.outer {
                        Outer::None => Source::Split(vec![(
                            cx.sets.set.cells(cx),
                            cx.sets.reference.as_ref().map(|r| r.cells(cx)),
                        )]),
                        Outer::Field(field) => {
                            let listed: Vec<u32> =
                                groups.chosen.iter().map(|&c| c as u32).collect();
                            table = GroupTable::new(&listed);
                            codes = field.entity_codes(cx.generation)?;
                            Source::Grouped(field.row_groups(cx, &table, &codes)?)
                        }
                        Outer::Layer(_) => {
                            let served = served.as_ref().expect("a layer's groups were read");
                            labels = served.label_table();
                            match &labels {
                                Some(labels) => Source::Grouped(served.row_groups(labels)),
                                None => {
                                    let set = served.group_rows(cx, cx.sets.set.rows(cx));
                                    let reference = cx
                                        .sets
                                        .reference
                                        .as_ref()
                                        .map(|r| served.group_rows(cx, r.rows(cx)));
                                    parts = (set, reference);
                                    Source::Split(
                                        parts
                                            .0
                                            .iter()
                                            .enumerate()
                                            .map(|(g, rows)| {
                                                (
                                                    CellSet::Rows(rows),
                                                    parts.1.as_ref().map(|r| CellSet::Rows(&r[g])),
                                                )
                                            })
                                            .collect(),
                                    )
                                }
                            }
                        }
                    };
                    let counted =
                        self.cell_rows(cx, &groups, &source, position, depth, limit, timings);
                    timings.cells_ns += started.elapsed().as_nanos() as u64;
                    counted?
                }
            },
        };
        let mut bytes = 0usize;
        let mut kept = 0usize;
        let mut cut_by_bytes = false;
        for row in rows.iter().take(limit) {
            let row_bytes = self.row_bytes(&groups, row);
            if kept > 0 && bytes + row_bytes > limits.max_page_bytes {
                cut_by_bytes = true;
                break;
            }
            bytes += row_bytes;
            kept += 1;
        }
        let next = if complete && kept == rows.len() {
            position.next_table()
        } else {
            let last = rows[kept - 1];
            Position {
                table: position.table,
                chosen: Some(groups.chosen.clone()),
                group: if self.cells.is_some() {
                    last.group
                } else {
                    last.group + 1
                },
                after_cell: self.cells.map(|_| last.cell),
                stamp: position.stamp,
            }
        };
        let batch = self.batch(&groups, &head, &rows[..kept])?;
        if self.cells.is_some() && kept < rows.len() {
            *spill = Some(Spill {
                at: next.clone(),
                under: Arc::clone(cx.generation),
                mask: cx.open.served.mask_identity,
                rows: rows.split_off(kept),
                complete,
            });
        }
        Ok(Page {
            head,
            batch,
            bytes,
            cut_by_bytes,
            next,
        })
    }

    /// The Arrow bytes a row adds to a page.
    fn row_bytes(&self, groups: &Groups, row: &Row) -> usize {
        let listed = groups.keys.get(row.group as usize);
        let mut bytes = 16;
        if !matches!(self.outer, Outer::None) {
            bytes += 1 + match listed {
                Some(Key::Text(key)) => 4 + key.len(),
                Some(Key::Id(_)) => 8,
                None => 4,
            };
        }
        if let Some(Some(title)) = groups
            .titles
            .as_ref()
            .and_then(|titles| titles.get(row.group as usize))
        {
            bytes += 4 + title.len();
        }
        if self.cells.is_some() {
            bytes += 8;
        }
        bytes
    }

    /// The rows as one batch, in the contract's column order.
    fn batch(&self, groups: &Groups, head: &TableHead, rows: &[Row]) -> Result<RecordBatch> {
        let mut fields: Vec<ArrowField> = Vec::new();
        let mut arrays: Vec<ArrayRef> = Vec::new();
        let mut push = |name: &str, array: ArrayRef, nullable: bool| {
            fields.push(ArrowField::new(name, array.data_type().clone(), nullable));
            arrays.push(array);
        };
        let listed = groups.keys.len() as u32;
        if !matches!(self.outer, Outer::None) {
            let kinds = Int8Array::from_iter_values(rows.iter().map(|row| match row.group {
                g if g < listed => 0i8,
                g if g == listed => 1,
                _ => 2,
            }));
            let values = Arc::new(StringArray::from(vec!["listed", "rest", "none"]));
            let group = DictionaryArray::<Int8Type>::try_new(kinds, values)
                .map_err(|e| EngineError::Malformed(format!("a group column: {e}")))?;
            push("group", Arc::new(group), false);
            match groups.keys.first() {
                Some(Key::Id(_)) => {
                    let mut b = UInt64Builder::with_capacity(rows.len());
                    for row in rows {
                        match groups.keys.get(row.group as usize) {
                            Some(Key::Id(id)) => b.append_value(*id),
                            _ => b.append_null(),
                        }
                    }
                    push("key", Arc::new(b.finish()), true);
                }
                _ => {
                    let mut b = StringBuilder::new();
                    for row in rows {
                        match groups.keys.get(row.group as usize) {
                            Some(Key::Text(key)) => b.append_value(key),
                            _ => b.append_null(),
                        }
                    }
                    push("key", Arc::new(b.finish()), true);
                }
            }
            if let Some(titles) = &groups.titles {
                let mut b = StringBuilder::new();
                for row in rows {
                    b.append_option(titles.get(row.group as usize).and_then(Option::as_deref));
                }
                push("title", Arc::new(b.finish()), true);
            }
        }
        if self.cells.is_some() {
            push(
                "cell",
                Arc::new(UInt64Array::from_iter_values(
                    rows.iter().map(|row| row.cell),
                )),
                false,
            );
        }
        push(
            "count",
            Arc::new(UInt64Array::from_iter_values(rows.iter().map(|r| r.count))),
            false,
        );
        if let Some(reference_total) = head.reference_total {
            push(
                "reference_count",
                Arc::new(UInt64Array::from_iter_values(
                    rows.iter().map(|r| r.reference),
                )),
                false,
            );
            let mut b = Float64Builder::with_capacity(rows.len());
            for row in rows {
                b.append_option(lift(row.count, head.total, row.reference, reference_total));
            }
            push("lift", Arc::new(b.finish()), true);
        }
        RecordBatch::try_new(Arc::new(Schema::new(fields)), arrays)
            .map_err(|e| EngineError::Malformed(format!("an aggregate page did not assemble: {e}")))
    }
}

impl Plan {
    /// A table with cells: the rows from `position`, at least `limit` of them unless the table
    /// ends first, and whether they run to its end.
    ///
    /// Where no cell of the group in progress has been sent and every remaining row can fit the
    /// page, one walk counts every remaining group. Otherwise each group is counted alone, from
    /// the cell after the last one sent, a batch of chunks at a time until the page is full.
    #[allow(clippy::too_many_arguments)]
    fn cell_rows(
        &self,
        cx: &Cx<'_>,
        groups: &Groups,
        source: &Source<'_>,
        position: &Position,
        depth: u8,
        limit: usize,
        timings: &mut AggregateTimings,
    ) -> Result<(Vec<Row>, bool)> {
        let cells_at_depth = if depth >= 32 {
            u64::MAX
        } else {
            1u64 << (2 * u32::from(depth))
        };
        let most_cells = cells_at_depth.min(cx.view_rows());
        let ranges = |size: u64| depth <= 16 && most_cells.saturating_mul(RANGE_FACTOR) <= size;
        let methods: Vec<(bool, bool)> = groups
            .sizes
            .iter()
            .map(|&(set, reference)| (ranges(set), ranges(reference)))
            .collect();
        let method = match source {
            Source::Split(_) if methods.iter().all(|&(s, _)| s) => "ranges",
            _ => "pass",
        };
        if !timings.methods.contains(&(self.grouping, method)) {
            timings.methods.push((self.grouping, method));
        }
        let groups_count = groups.sizes.len() as u32;
        let walk = Walk {
            cx,
            source,
            depth,
            methods: &methods,
        };
        let bound: u64 = groups.sizes[position.group.min(groups_count) as usize..]
            .iter()
            .map(|&(set, reference)| (set + reference).min(most_cells))
            .sum();
        if position.after_cell.is_none() && bound <= limit as u64 {
            let (mut rows, _) =
                walk.window(position.group..groups_count, 0, usize::MAX, timings)?;
            rows.sort_unstable_by_key(|row| (row.group, row.cell));
            return Ok((rows, true));
        }
        let mut rows: Vec<Row> = Vec::new();
        let (mut group, mut after) = (position.group, position.after_cell);
        while group < groups_count && rows.len() <= limit {
            let (set, reference) = groups.sizes[group as usize];
            let from = match after {
                None => Some(0),
                Some(cell) => cell
                    .checked_add(1)
                    .filter(|&c| c < cells_at_depth || depth >= 32),
            };
            if let (true, Some(from)) = (set > 0 || reference > 0, from) {
                let (found, complete) =
                    walk.window(group..group + 1, from, limit + 1 - rows.len(), timings)?;
                rows.extend(found);
                if !complete {
                    return Ok((rows, false));
                }
            }
            group += 1;
            after = None;
        }
        let complete = group >= groups_count && rows.len() <= limit;
        Ok((rows, complete))
    }
}

/// The most rows of the view one chunk of a cell count spans.
const CHUNK_ROWS: u64 = 1 << 20;

/// The fewest rows of the view one chunk spans, so a small page is not cut into slivers whose
/// scheduling costs more than their rows.
const MIN_CHUNK_ROWS: u64 = 1 << 14;

/// Range counts are taken for a group when its items number at least this many times the cells
/// that could hold them: a range count costs about as much as reading this many rows.
const RANGE_FACTOR: u64 = 64;

/// Where each group's rows are read for a cell count.
enum Source<'s> {
    /// One pass over the set reads each row's groups.
    Grouped(RowGroups<'s>),
    /// Each group's rows as a set of its own, in the set and in the reference.
    Split(Vec<(CellSet<'s>, Option<CellSet<'s>>)>),
}

/// One page's walk over the view's chunks.
struct Walk<'w> {
    cx: &'w Cx<'w>,
    source: &'w Source<'w>,
    depth: u8,
    /// For each group, whether its set and its reference are counted by range.
    methods: &'w [(bool, bool)],
}

impl Walk<'_> {
    /// The rows of `groups` whose cell is `from` or after, a batch of chunks at a time until at
    /// least `need` are found, and whether the walk reached the last chunk.
    ///
    /// The chunks are cut so that one batch across the pool reads about `need` rows of the view,
    /// and each later batch takes as many chunks as the rows still wanted need at the yield so far,
    /// so a page reads little past its own rows however sparse the set is.
    fn window(
        &self,
        groups: std::ops::Range<u32>,
        from: u64,
        need: usize,
        timings: &mut AggregateTimings,
    ) -> Result<(Vec<Row>, bool)> {
        let threads = self.cx.engine.pool.current_num_threads().max(1);
        let chunk_rows = (need as u64)
            .div_ceil(threads as u64)
            .clamp(MIN_CHUNK_ROWS, CHUNK_ROWS);
        let chunks = crate::cells::chunks(self.cx.segments(), self.depth, chunk_rows);
        let fine = 2 * u32::from(self.depth - self.depth.min(16));
        let start = chunks.partition_point(|chunk| chunk.end <= from >> fine);
        let mut at = start;
        let mut batch = threads;
        let mut rows: Vec<Row> = Vec::new();
        while at < chunks.len() && rows.len() < need {
            self.cx.check_cancelled()?;
            let end = (at + batch).min(chunks.len());
            let parts: Vec<Vec<Row>> = self.cx.engine.pool.install(|| {
                chunks[at..end]
                    .par_iter()
                    .map(|prefixes| self.chunk(groups.clone(), prefixes.clone()))
                    .collect()
            });
            timings.cells_walked += (end - at) as u64;
            rows.reserve(parts.iter().map(Vec::len).sum());
            for part in parts {
                rows.extend(part.into_iter().filter(|row| row.cell >= from));
            }
            at = end;
            // Enough chunks for the rows still wanted at the yield so far.
            let found = (rows.len() as u64).max(1);
            let wanted = (need - rows.len().min(need)) as u64;
            let chunks_wanted = wanted.saturating_mul((at - start) as u64).div_ceil(found);
            batch = (chunks_wanted as usize + 1).clamp(1, threads * 64);
        }
        Ok((rows, at == chunks.len()))
    }

    /// One chunk's rows of `groups`, ascending by cell then group.
    fn chunk(&self, groups: std::ops::Range<u32>, prefixes: std::ops::Range<u64>) -> Vec<Row> {
        let segments = self.cx.segments();
        let depth = self.depth;
        let wanted = |entry: &CellCount| groups.contains(&entry.group);
        match self.source {
            Source::Grouped(row_groups) => {
                let set = &self.cx.sets.set;
                let mut counted: Vec<CellCount> = count_chunk(
                    set.cells(self.cx),
                    segments,
                    depth,
                    row_groups,
                    prefixes.clone(),
                );
                counted.retain(wanted);
                let mut reference: Vec<CellCount> = match &self.cx.sets.reference {
                    None => Vec::new(),
                    Some(r) => count_chunk(r.cells(self.cx), segments, depth, row_groups, prefixes),
                };
                reference.retain(wanted);
                merge(counted, reference)
            }
            Source::Split(sets) => {
                let one = |set: CellSet<'_>, ranges: bool, group: u32| -> Vec<CellCount> {
                    if ranges {
                        count_chunk_by_ranges(set, segments, depth, prefixes.clone())
                            .cells
                            .into_iter()
                            .map(|(cell, count)| CellCount { cell, group, count })
                            .collect()
                    } else {
                        count_chunk(set, segments, depth, &RowGroups::None, prefixes.clone())
                            .into_iter()
                            .map(|entry| CellCount { group, ..entry })
                            .collect()
                    }
                };
                let mut rows: Vec<Row> = Vec::new();
                for group in groups.clone() {
                    let (set, reference) = sets[group as usize];
                    let (set_ranges, reference_ranges) = self.methods[group as usize];
                    let counted = one(set, set_ranges, group);
                    let referenced =
                        reference.map_or_else(Vec::new, |r| one(r, reference_ranges, group));
                    rows.extend(merge(counted, referenced));
                }
                rows.sort_unstable_by_key(|row| (row.cell, row.group));
                rows
            }
        }
    }
}

/// Two tables ascending by cell then group, joined into rows.
fn merge(set: Vec<CellCount>, reference: Vec<CellCount>) -> Vec<Row> {
    if reference.is_empty() {
        return set
            .into_iter()
            .map(|e| Row {
                group: e.group,
                cell: e.cell,
                count: e.count,
                reference: 0,
            })
            .collect();
    }
    let mut out: Vec<Row> = Vec::with_capacity(set.len().max(reference.len()));
    let (mut a, mut b) = (set.into_iter().peekable(), reference.into_iter().peekable());
    loop {
        let key = |e: &CellCount| (e.cell, e.group);
        let row = match (a.peek(), b.peek()) {
            (None, None) => break,
            (Some(x), Some(y)) if key(x) == key(y) => {
                let (x, y) = (a.next().unwrap(), b.next().unwrap());
                (x.cell, x.group, x.count, y.count)
            }
            (Some(x), Some(y)) if key(x) < key(y) => {
                let x = a.next().unwrap();
                (x.cell, x.group, x.count, 0)
            }
            (Some(_), None) => {
                let x = a.next().unwrap();
                (x.cell, x.group, x.count, 0)
            }
            _ => {
                let y = b.next().unwrap();
                (y.cell, y.group, 0, y.count)
            }
        };
        out.push(Row {
            group: row.1,
            cell: row.0,
            count: row.2,
            reference: row.3,
        });
    }
    out
}

/// `(count / total) / (reference / reference_total)`, `None` where `reference` or `total` is 0.
fn lift(count: u64, total: u64, reference: u64, reference_total: u64) -> Option<f64> {
    (reference > 0 && total > 0)
        .then(|| (count as f64 / total as f64) / (reference as f64 / reference_total as f64))
}

impl Groups {
    /// The one group of a table with no outer level: the whole set.
    fn whole(cx: &Cx<'_>) -> Groups {
        let reference = cx.sets.reference.as_ref().map_or(0, |r| r.size());
        Groups {
            chosen: Vec::new(),
            sizes: vec![(cx.sets.set.size(), reference)],
            always: vec![true],
            keys: Vec::new(),
            titles: None,
            distinct: 1,
        }
    }
}

/// A table without cells: a row per group from `from` with an item, or listed to appear anyway,
/// and whether every such row is here.
fn listed_rows(groups: &Groups, from: u32, limit: usize) -> (Vec<Row>, bool) {
    let rows: Vec<Row> = groups
        .sizes
        .iter()
        .enumerate()
        .skip(from as usize)
        .filter(|&(g, &(count, reference))| {
            count > 0 || reference > 0 || groups.always.get(g).copied().unwrap_or(false)
        })
        .map(|(g, &(count, reference))| Row {
            group: g as u32,
            cell: 0,
            count,
            reference,
        })
        .collect();
    let complete = rows.len() <= limit;
    (rows, complete)
}
