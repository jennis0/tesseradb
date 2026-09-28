//! One grouping's table: its groups, its rows from a position, and its Arrow batches.
//!
//! A table's rows are its groups in order, the listed ones then the rest then none, and within a
//! group its cells ascending. A row appears where the set or the reference holds an item of it,
//! and a named value this viewer may be told of appears with no item. A page takes the rows after
//! its position up to the page's rows and bytes, and the position moves to its last row.

use std::sync::Arc;

use arrow::array::{
    Array, ArrayRef, BooleanBufferBuilder, DictionaryArray, Float64Array, Int32Array, Int8Array,
    StringArray, UInt64Array,
};
use arrow::buffer::{NullBuffer, ScalarBuffer};
use arrow::datatypes::{Field as ArrowField, Int32Type, Int8Type, Schema};
use arrow::record_batch::RecordBatch;

use rayon::prelude::*;

use super::artifacts::{Layer, Served};
use super::cursor::Position;
use super::set::Cx;
use super::values::Field;
use super::{AggregateTimings, By, Grouping, TableHead};
use crate::cells::{
    count_chunk_by_ranges, count_chunk_into, CellSet, CellSink, GroupTable, RowGroups,
};
use crate::engine::Engine;
use crate::error::{EngineError, Result};
use crate::records::same_publication;
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

/// What the page about to be built may hold, and what the response can still send after it.
#[derive(Clone, Copy)]
pub(super) struct Budget {
    pub(super) page_rows: u32,
    pub(super) max_page_bytes: usize,
    pub(super) pages_left: u64,
    pub(super) response_bytes_left: u64,
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

/// What one page of a table counted that the next page of the same response can take while the
/// publication and the mask it was counted under hold: the table's groups, a layer's served
/// artifacts, and the rows counted past the page's end with where they start and whether they run
/// to the table's end.
pub(super) struct Held {
    table: u32,
    under: Arc<Generation>,
    mask: crate::histogram::MaskIdentity,
    groups: Groups,
    served: Option<Served>,
    spill: Option<(Position, Vec<Run>, bool)>,
}

impl Held {
    /// Whether this was counted for the table `position` continues, under `cx`.
    fn holds(&self, cx: &Cx<'_>, position: &Position) -> bool {
        self.table == position.table
            && position.chosen.as_deref() == Some(&self.groups.chosen[..])
            && same_publication(&self.under, cx.generation)
            && self.mask == cx.open.served.mask_identity
    }
}

/// A stretch of one group's rows in the order a table sends them: cells ascending, each with its
/// count in the set and, where the request names one, in the reference. A group's rows are one
/// run or several in order. The columns are the batch's own.
#[derive(Clone)]
pub(super) struct Run {
    group: u32,
    cells: UInt64Array,
    counts: UInt64Array,
    references: Option<UInt64Array>,
}

impl Run {
    fn of(group: u32, columns: Columns) -> Run {
        Run {
            group,
            cells: UInt64Array::from(columns.cells),
            counts: UInt64Array::from(columns.counts),
            references: columns.references.map(UInt64Array::from),
        }
    }

    fn len(&self) -> usize {
        self.cells.len()
    }

    fn slice(&self, offset: usize, len: usize) -> Run {
        Run {
            group: self.group,
            cells: self.cells.slice(offset, len),
            counts: self.counts.slice(offset, len),
            references: self.references.as_ref().map(|r| r.slice(offset, len)),
        }
    }
}

/// One group's rows as a count builds them.
#[derive(Default)]
struct Columns {
    cells: Vec<u64>,
    counts: Vec<u64>,
    /// `None` where the request names no reference.
    references: Option<Vec<u64>>,
}

impl Columns {
    fn len(&self) -> usize {
        self.cells.len()
    }

    /// The rows from `from` on: a chunk's cells ascend, so those before it are a prefix.
    fn starting_at(mut self, from: u64) -> Columns {
        let k = self.cells.partition_point(|&c| c < from);
        if k > 0 {
            self.cells.drain(..k);
            self.counts.drain(..k);
            if let Some(r) = &mut self.references {
                r.drain(..k);
            }
        }
        self
    }
}

/// A chunk's entries of the groups `first..first + groups.len()`, as columns; other groups' are
/// passed over.
struct ByGroup {
    first: u32,
    groups: Vec<(Vec<u64>, Vec<u64>)>,
}

impl ByGroup {
    /// Columns for `wanted`, a single group's reserved for `rows` entries, at most one a row, so
    /// they do not move as they grow.
    fn new(wanted: &std::ops::Range<u32>, rows: u64) -> ByGroup {
        let reserve = if wanted.len() == 1 { rows as usize } else { 0 };
        ByGroup {
            first: wanted.start,
            groups: (wanted.start..wanted.end)
                .map(|_| (Vec::with_capacity(reserve), Vec::with_capacity(reserve)))
                .collect(),
        }
    }
}

impl CellSink for ByGroup {
    #[inline]
    fn push(&mut self, cell: u64, group: u32, count: u64) {
        if let Some((cells, counts)) = group
            .checked_sub(self.first)
            .and_then(|g| self.groups.get_mut(g as usize))
        {
            cells.push(cell);
            counts.push(count);
        }
    }
}

/// Entries of one group's own set, which counts them in group 0, as `group`'s.
struct As<'s, S> {
    inner: &'s mut S,
    group: u32,
}

impl<S: CellSink> CellSink for As<'_, S> {
    #[inline]
    fn push(&mut self, cell: u64, _: u32, count: u64) {
        self.inner.push(cell, self.group, count);
    }
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

    /// The layer's entity, on a layer grouping.
    pub(super) fn layer_entity(&self) -> Option<u64> {
        match &self.outer {
            Outer::Layer(layer) => Some(layer.entity()),
            _ => None,
        }
    }

    /// The page of this table starting at `position`. What the previous page of the same
    /// response counted, its groups and any rows past its end, is taken from `held` while the
    /// publication and the mask are the ones it was counted under, and left there for the next.
    pub(super) fn page(
        &self,
        cx: &Cx<'_>,
        position: &Position,
        budget: &Budget,
        timings: &mut AggregateTimings,
        held: &mut Option<Held>,
    ) -> Result<Page> {
        let Budget {
            page_rows,
            max_page_bytes,
            pages_left,
            response_bytes_left,
        } = *budget;
        cx.check_cancelled()?;
        let (groups, served, spill) = match held.take().filter(|h| h.holds(cx, position)) {
            Some(h) => (h.groups, h.served, h.spill),
            None => {
                let (groups, served) = match &self.outer {
                    Outer::None => (Groups::whole(cx), None),
                    Outer::Field(field) => {
                        (field.groups(cx, position.chosen.as_deref(), timings)?, None)
                    }
                    Outer::Layer(layer) => {
                        let (groups, served) =
                            layer.groups(cx, position.chosen.as_deref(), timings)?;
                        (groups, Some(served))
                    }
                };
                (groups, served, None)
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
        let row_bytes = self.row_bytes(&groups, head.reference_total.is_some());
        let dictionary = self.dictionary_bytes(&groups);
        let spilled = spill.filter(|(at, runs, complete)| {
            at == position && (*complete || runs.iter().map(Run::len).sum::<usize>() > limit)
        });
        let (runs, complete) = match (self.cells, spilled) {
            (None, _) => listed_rows(&groups, position.group, head.reference_total.is_some()),
            (Some(_), Some((_, runs, complete))) => (runs, complete),
            (Some(depth), None) => {
                let started = std::time::Instant::now();
                let table;
                let codes;
                let labels;
                let source = match &self.outer {
                    Outer::None => Source::Split(vec![(
                        cx.sets.set.cells(cx),
                        cx.sets.reference.as_ref().map(|r| r.cells(cx)),
                    )]),
                    Outer::Field(field) => {
                        let listed: Vec<u32> = groups.chosen.iter().map(|&c| c as u32).collect();
                        table = GroupTable::new(&listed);
                        codes = field.entity_codes(cx.generation)?;
                        Source::Grouped(field.row_groups(cx, &table, &codes)?)
                    }
                    Outer::Layer(_) => {
                        let served = served.as_ref().expect("a layer's groups were read");
                        labels = served.label_table();
                        match (&labels, served.parts()) {
                            (Some(labels), _) => Source::Grouped(served.row_groups(labels)),
                            (None, Some((set, reference))) => Source::Split(
                                set.iter()
                                    .enumerate()
                                    .map(|(g, rows)| {
                                        (
                                            CellSet::Rows(rows),
                                            reference.as_ref().map(|r| CellSet::Rows(&r[g])),
                                        )
                                    })
                                    .collect(),
                            ),
                            (None, None) => Source::Split(Vec::new()),
                        }
                    }
                };
                let rows_left =
                    response_bytes_left.saturating_sub(dictionary as u64) / row_bytes as u64;
                let counted = self.cell_rows(
                    cx,
                    &groups,
                    &source,
                    position,
                    depth,
                    limit,
                    rows_left.min(pages_left.saturating_mul(limit as u64)),
                    timings,
                );
                timings.cells_ns += started.elapsed().as_nanos() as u64;
                counted?
            }
        };
        let total: usize = runs.iter().map(Run::len).sum();
        let fits = max_page_bytes.saturating_sub(dictionary) / row_bytes;
        let most = total.min(limit).min(fits.max(1));
        // A long run goes out as its own page, sent as the columns it was counted into.
        let alone = if self.cells.is_some() {
            ALONE_ROWS
        } else {
            usize::MAX
        };
        let (page, rest) = split_runs(runs, most, alone);
        let kept: usize = page.iter().map(Run::len).sum();
        let cut_by_bytes = kept == fits && fits < total.min(limit);
        let next = if complete && rest.is_empty() {
            position.next_table()
        } else {
            let last = page
                .last()
                .expect("a page that is not the table's end holds a row");
            Position {
                table: position.table,
                chosen: Some(groups.chosen.clone()),
                group: if self.cells.is_some() {
                    last.group
                } else {
                    last.group + 1
                },
                after_cell: self.cells.map(|_| last.cells.value(last.len() - 1)),
                stamp: position.stamp,
            }
        };
        let building = std::time::Instant::now();
        // A long page's columns are joined on the engine's threads; a short one's on this one.
        let batch = if kept > ALONE_ROWS {
            cx.engine
                .pool
                .install(|| self.batch(&groups, &head, &page))?
        } else {
            self.batch(&groups, &head, &page)?
        };
        timings.batch_ns += building.elapsed().as_nanos() as u64;
        if next.table == position.table {
            let spill =
                (self.cells.is_some() && !rest.is_empty()).then(|| (next.clone(), rest, complete));
            *held = Some(Held {
                table: position.table,
                under: Arc::clone(cx.generation),
                mask: cx.open.served.mask_identity,
                groups,
                served,
                spill,
            });
        }
        Ok(Page {
            head,
            batch,
            bytes: kept * row_bytes + dictionary,
            cut_by_bytes,
            next,
        })
    }

    /// The Arrow bytes a row adds to a page, besides the dictionaries.
    fn row_bytes(&self, groups: &Groups, reference: bool) -> usize {
        let mut bytes = 8;
        if !matches!(self.outer, Outer::None) {
            bytes += 1 + match self.outer {
                Outer::Layer(_) => 8,
                _ => 4,
            };
        }
        if groups.titles.is_some() {
            bytes += 4;
        }
        if self.cells.is_some() {
            bytes += 8;
        }
        if reference {
            bytes += 16;
        }
        bytes
    }

    /// The Arrow bytes a page's dictionaries take: the group names, and a field's keys and titles.
    fn dictionary_bytes(&self, groups: &Groups) -> usize {
        if matches!(self.outer, Outer::None) {
            return 0;
        }
        let text = |s: &str| 4 + s.len();
        let keys: usize = groups
            .keys
            .iter()
            .map(|key| match key {
                Key::Text(key) => text(key),
                Key::Id(_) => 0,
            })
            .sum();
        let titles: usize = groups
            .titles
            .iter()
            .flatten()
            .map(|title| text(title.as_deref().unwrap_or("")))
            .sum();
        ["listed", "rest", "none"].map(text).iter().sum::<usize>() + keys + titles
    }

    /// The runs as one batch, in the contract's column order.
    fn batch(&self, groups: &Groups, head: &TableHead, runs: &[Run]) -> Result<RecordBatch> {
        let malformed = |e: arrow::error::ArrowError| {
            EngineError::Malformed(format!("an aggregate page did not assemble: {e}"))
        };
        let mut fields: Vec<ArrowField> = Vec::new();
        let mut arrays: Vec<ArrayRef> = Vec::new();
        let mut push = |name: &str, array: ArrayRef, nullable: bool| {
            fields.push(ArrowField::new(name, array.data_type().clone(), nullable));
            arrays.push(array);
        };
        let listed = groups.keys.len() as u32;
        if !matches!(self.outer, Outer::None) {
            let kinds: Vec<i8> = repeated(runs, |g| (g >= listed) as i8 + (g > listed) as i8);
            let values = Arc::new(StringArray::from(vec!["listed", "rest", "none"]));
            let group = DictionaryArray::<Int8Type>::try_new(Int8Array::from(kinds), values)
                .map_err(malformed)?;
            push("group", Arc::new(group), false);
            // Null on the rest and none.
            let on_listed = valid(runs, |g| g < listed);
            match &self.outer {
                Outer::Layer(_) => {
                    let id = |g: u32| match groups.keys.get(g as usize) {
                        Some(Key::Id(id)) => *id,
                        _ => 0,
                    };
                    let ids = UInt64Array::new(repeated(runs, id).into(), on_listed);
                    push("key", Arc::new(ids), true);
                }
                _ => {
                    // Each listed value's key and title are written once, and each row carries the
                    // listed position.
                    let keys: Vec<&str> = groups
                        .keys
                        .iter()
                        .map(|key| match key {
                            Key::Text(text) => text.as_str(),
                            Key::Id(_) => "",
                        })
                        .collect();
                    let positions: ScalarBuffer<i32> =
                        repeated(runs, |g| g.min(listed) as i32).into();
                    let key = DictionaryArray::<Int32Type>::try_new(
                        Int32Array::new(positions.clone(), on_listed),
                        Arc::new(StringArray::from(keys)),
                    )
                    .map_err(malformed)?;
                    push("key", Arc::new(key), true);
                    if let Some(titles) = &groups.titles {
                        let titled = valid(runs, |g| g < listed && titles[g as usize].is_some());
                        let title = DictionaryArray::<Int32Type>::try_new(
                            Int32Array::new(positions, titled),
                            Arc::new(StringArray::from_iter(
                                titles.iter().map(|t| Some(t.as_deref().unwrap_or(""))),
                            )),
                        )
                        .map_err(malformed)?;
                        push("title", Arc::new(title), true);
                    }
                }
            }
        }
        if self.cells.is_some() {
            push("cell", joined(runs, |r| &r.cells), false);
        }
        let counts = joined(runs, |r| &r.counts);
        push("count", Arc::clone(&counts), false);
        if let Some(reference_total) = head.reference_total {
            let references = joined(runs, |r| {
                r.references.as_ref().expect("a reference was counted")
            });
            let (c, r) = (
                counts
                    .as_any()
                    .downcast_ref::<UInt64Array>()
                    .expect("counts"),
                references
                    .as_any()
                    .downcast_ref::<UInt64Array>()
                    .expect("references"),
            );
            let lifts =
                Float64Array::from_iter(c.values().iter().zip(r.values().iter()).map(
                    |(&count, &reference)| lift(count, head.total, reference, reference_total),
                ));
            push("reference_count", references, false);
            push("lift", Arc::new(lifts), true);
        }
        RecordBatch::try_new(Arc::new(Schema::new(fields)), arrays).map_err(malformed)
    }
}

/// One value per row, `of` each run's group along the run.
fn repeated<T: Copy>(runs: &[Run], of: impl Fn(u32) -> T) -> Vec<T> {
    let mut out = Vec::with_capacity(runs.iter().map(Run::len).sum());
    for run in runs {
        out.extend(std::iter::repeat_n(of(run.group), run.len()));
    }
    out
}

/// Which rows hold a value, by their run's group; `None` where every row does.
fn valid(runs: &[Run], holds: impl Fn(u32) -> bool) -> Option<NullBuffer> {
    if runs.iter().all(|run| holds(run.group)) {
        return None;
    }
    let mut bits = BooleanBufferBuilder::new(runs.iter().map(Run::len).sum());
    for run in runs {
        bits.append_n(run.len(), holds(run.group));
    }
    Some(NullBuffer::new(bits.finish()))
}

/// One column of the runs as one array: a single run's own, or the runs' copied side by side, a
/// long page's on the current pool into a buffer whose pages the copy is the first to touch.
fn joined(runs: &[Run], of: impl Fn(&Run) -> &UInt64Array + Sync) -> ArrayRef {
    if let [one] = runs {
        return Arc::new(of(one).clone());
    }
    let rows: usize = runs.iter().map(Run::len).sum();
    if rows <= ALONE_ROWS {
        let mut out = Vec::with_capacity(rows);
        for run in runs {
            out.extend_from_slice(of(run).values());
        }
        return Arc::new(UInt64Array::from(out));
    }
    let mut out = vec![0u64; rows];
    let mut parts: Vec<(&mut [u64], &Run)> = Vec::with_capacity(runs.len());
    let mut rest = &mut out[..];
    for run in runs {
        let (here, after) = rest.split_at_mut(run.len());
        parts.push((here, run));
        rest = after;
    }
    parts
        .into_par_iter()
        .for_each(|(here, run)| here.copy_from_slice(of(run).values()));
    Arc::new(UInt64Array::from(out))
}

impl Plan {
    /// A table with cells: the runs from `position`, at least `limit` rows of them unless the
    /// table ends first, and whether they run to its end.
    ///
    /// Where no cell of the group in progress has been sent and every remaining row fits in what
    /// this response can still send, `budget` rows, one walk counts every remaining group and the
    /// pages after this one take theirs from it. Otherwise each group is counted alone, from the
    /// cell after the last one sent, a batch of chunks at a time until the page is full.
    #[allow(clippy::too_many_arguments)]
    fn cell_rows(
        &self,
        cx: &Cx<'_>,
        groups: &Groups,
        source: &Source<'_>,
        position: &Position,
        depth: u8,
        limit: usize,
        budget: u64,
        timings: &mut AggregateTimings,
    ) -> Result<(Vec<Run>, bool)> {
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
            reference: cx.sets.reference.is_some(),
        };
        let bound: u64 = groups.sizes[position.group.min(groups_count) as usize..]
            .iter()
            .map(|&(set, reference)| (set + reference).min(most_cells))
            .sum();
        if position.after_cell.is_none() && bound <= budget.max(limit as u64) {
            let (pieces, _) = walk.window(position.group..groups_count, 0, usize::MAX, timings)?;
            let runs = pieces
                .into_iter()
                .zip(position.group..)
                .flat_map(|(pieces, group)| pieces.into_iter().map(move |p| Run::of(group, p)))
                .collect();
            return Ok((runs, true));
        }
        let mut runs: Vec<Run> = Vec::new();
        let mut rows = 0usize;
        let (mut group, mut after) = (position.group, position.after_cell);
        while group < groups_count && rows <= limit {
            let (set, reference) = groups.sizes[group as usize];
            let from = match after {
                None => Some(0),
                Some(cell) => cell
                    .checked_add(1)
                    .filter(|&c| c < cells_at_depth || depth >= 32),
            };
            if let (true, Some(from)) = (set > 0 || reference > 0, from) {
                let (mut found, complete) =
                    walk.window(group..group + 1, from, limit + 1 - rows, timings)?;
                for piece in found.pop().expect("one group was counted") {
                    rows += piece.len();
                    runs.push(Run::of(group, piece));
                }
                if !complete {
                    return Ok((runs, false));
                }
            }
            group += 1;
            after = None;
        }
        let complete = group >= groups_count && rows <= limit;
        Ok((runs, complete))
    }
}

/// The most rows of the view one chunk of a cell count spans.
const CHUNK_ROWS: u64 = 1 << 20;

/// The fewest rows of the view one chunk spans, so a small page is not cut into slivers whose
/// scheduling costs more than their rows.
const MIN_CHUNK_ROWS: u64 = 1 << 14;

/// How many chunks a walk cuts per thread, so threads that finish early take more.
const CHUNKS_PER_THREAD: u64 = 4;

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
    reference: bool,
}

impl Walk<'_> {
    /// The rows of each of `groups` whose cell is `from` or after, a batch of chunks at a time
    /// until at least `need` are found, and whether the walk reached the last chunk.
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
    ) -> Result<(Vec<Vec<Columns>>, bool)> {
        let threads = self.cx.engine.pool.current_num_threads().max(1) as u64;
        let span = (need as u64).min(self.cx.view_rows()).max(1);
        let chunk_rows = span
            .div_ceil(threads * CHUNKS_PER_THREAD)
            .clamp(MIN_CHUNK_ROWS, CHUNK_ROWS);
        let chunks = crate::cells::chunks(self.cx.segments(), self.depth, chunk_rows);
        let fine = 2 * u32::from(self.depth - self.depth.min(16));
        let start = chunks.partition_point(|chunk| chunk.end <= from >> fine);
        let mut at = start;
        let mut batch = (threads * CHUNKS_PER_THREAD) as usize;
        let mut out: Vec<Vec<Columns>> = groups.clone().map(|_| Vec::new()).collect();
        let mut rows = 0usize;
        while at < chunks.len() && rows < need {
            self.cx.check_cancelled()?;
            let end = (at + batch).min(chunks.len());
            let walking = std::time::Instant::now();
            let parts: Vec<Vec<Columns>> = self.cx.engine.pool.install(|| {
                chunks[at..end]
                    .par_iter()
                    .map(|prefixes| self.chunk(groups.clone(), prefixes.clone(), chunk_rows))
                    .collect()
            });
            timings.pass_ns += walking.elapsed().as_nanos() as u64;
            timings.cells_walked += (end - at) as u64;
            for part in parts {
                for (pieces, columns) in out.iter_mut().zip(part) {
                    let columns = columns.starting_at(from);
                    if columns.len() > 0 {
                        rows += columns.len();
                        pieces.push(columns);
                    }
                }
            }
            at = end;
            // Enough chunks for the rows still wanted at the yield so far.
            let found = (rows as u64).max(1);
            let wanted = (need - rows.min(need)) as u64;
            let chunks_wanted = wanted.saturating_mul((at - start) as u64).div_ceil(found);
            batch = (chunks_wanted.saturating_add(1)).min(threads * 64).max(1) as usize;
        }
        Ok((out, at == chunks.len()))
    }

    /// One chunk's rows of each of `groups`, the chunk spanning about `rows` rows of the view.
    fn chunk(
        &self,
        groups: std::ops::Range<u32>,
        prefixes: std::ops::Range<u64>,
        rows: u64,
    ) -> Vec<Columns> {
        let segments = self.cx.segments();
        let depth = self.depth;
        let mut set = ByGroup::new(&groups, rows);
        let mut reference = ByGroup::new(&groups, rows);
        match self.source {
            Source::Grouped(row_groups) => {
                let only = (groups.len() == 1).then_some(groups.start);
                let cells = self.cx.sets.set.cells(self.cx);
                count_chunk_into(
                    cells,
                    segments,
                    depth,
                    row_groups,
                    only,
                    prefixes.clone(),
                    &mut set,
                );
                if let Some(r) = &self.cx.sets.reference {
                    let cells = r.cells(self.cx);
                    count_chunk_into(
                        cells,
                        segments,
                        depth,
                        row_groups,
                        only,
                        prefixes,
                        &mut reference,
                    );
                }
            }
            Source::Split(sets) => {
                let one = |cells: CellSet<'_>, ranges: bool, out: &mut As<'_, ByGroup>| {
                    if ranges {
                        for (cell, count) in
                            count_chunk_by_ranges(cells, segments, depth, prefixes.clone()).cells
                        {
                            out.push(cell, 0, count);
                        }
                    } else {
                        count_chunk_into(
                            cells,
                            segments,
                            depth,
                            &RowGroups::None,
                            None,
                            prefixes.clone(),
                            out,
                        );
                    }
                };
                for group in groups.clone() {
                    let (cells, in_reference) = sets[group as usize];
                    let (set_ranges, reference_ranges) = self.methods[group as usize];
                    one(
                        cells,
                        set_ranges,
                        &mut As {
                            inner: &mut set,
                            group,
                        },
                    );
                    if let Some(r) = in_reference {
                        one(
                            r,
                            reference_ranges,
                            &mut As {
                                inner: &mut reference,
                                group,
                            },
                        );
                    }
                }
            }
        }
        set.groups
            .into_iter()
            .zip(reference.groups)
            .map(|(set, reference)| merge(set, self.reference.then_some(reference)))
            .collect()
    }
}

/// One group's cells in the set and, where there is one, the reference, joined: ascending by cell,
/// a cell present where either counts it.
fn merge(set: (Vec<u64>, Vec<u64>), reference: Option<(Vec<u64>, Vec<u64>)>) -> Columns {
    let (cells, counts) = set;
    let Some((reference_cells, reference_counts)) = reference else {
        return Columns {
            cells,
            counts,
            references: None,
        };
    };
    let mut out = Columns {
        cells: Vec::with_capacity(cells.len().max(reference_cells.len())),
        counts: Vec::new(),
        references: Some(Vec::new()),
    };
    let references = out.references.as_mut().expect("just set");
    let (mut i, mut j) = (0, 0);
    while i < cells.len() || j < reference_cells.len() {
        let a = cells.get(i).copied().unwrap_or(u64::MAX);
        let b = reference_cells.get(j).copied().unwrap_or(u64::MAX);
        let at = a.min(b);
        out.cells.push(at);
        out.counts.push(if a == at { counts[i] } else { 0 });
        references.push(if b == at { reference_counts[j] } else { 0 });
        i += usize::from(a == at && i < cells.len());
        j += usize::from(b == at && j < reference_cells.len());
    }
    out
}

/// Runs longer than this are not joined to another in one page.
const ALONE_ROWS: usize = 1 << 16;

/// The first `n` rows of `runs`, or fewer where a run longer than `alone` would share the page with
/// another, and the rest.
fn split_runs(runs: Vec<Run>, n: usize, alone: usize) -> (Vec<Run>, Vec<Run>) {
    let mut page: Vec<Run> = Vec::new();
    let mut rest = Vec::new();
    let mut left = n;
    for run in runs {
        let shared = page
            .last()
            .is_some_and(|last| last.len() > alone || run.len() > alone);
        if left == 0 || shared {
            left = 0;
            rest.push(run);
        } else if run.len() <= left {
            left -= run.len();
            page.push(run);
        } else {
            page.push(run.slice(0, left));
            rest.push(run.slice(left, run.len() - left));
            left = 0;
        }
    }
    (page, rest)
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
/// each a run of one row.
fn listed_rows(groups: &Groups, from: u32, reference: bool) -> (Vec<Run>, bool) {
    let runs = groups
        .sizes
        .iter()
        .enumerate()
        .skip(from as usize)
        .filter(|&(g, &(count, reference))| {
            count > 0 || reference > 0 || groups.always.get(g).copied().unwrap_or(false)
        })
        .map(|(g, &(count, in_reference))| {
            let columns = Columns {
                cells: vec![0],
                counts: vec![count],
                references: reference.then(|| vec![in_reference]),
            };
            Run::of(g as u32, columns)
        })
        .collect();
    (runs, true)
}
