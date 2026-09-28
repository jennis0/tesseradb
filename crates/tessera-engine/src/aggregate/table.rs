//! One grouping's table: its groups, its rows from a position, and its Arrow batches.
//!
//! A table's rows are its groups in order, the listed ones then the rest then none, and within a
//! group its cells ascending. A row appears where the set or the reference holds an item of it,
//! and a named value this viewer may be told of appears with no item. A page takes the rows after
//! its position up to the page's rows and bytes, and the position moves to its last row.

use std::sync::Arc;

use arrow::array::{
    ArrayRef, DictionaryArray, Float64Builder, Int8Array, StringArray, StringBuilder,
    UInt64Array, UInt64Builder,
};
use arrow::datatypes::{Field as ArrowField, Int8Type, Schema};
use arrow::record_batch::RecordBatch;

use super::cursor::Position;
use super::set::Cx;
use super::values::Field;
use super::{AggregateTimings, By, Grouping, TableHead};
use crate::engine::Engine;
use crate::error::{EngineError, Result};
use crate::records::RecordsLimits;
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

/// One row of a table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Row {
    /// The group, as a position in the listed groups followed by the rest and none.
    group: u32,
    cell: Option<u64>,
    count: u64,
    reference: u64,
}

impl Plan {
    /// `grouping`, the request's `index`th, resolved in `view`, or the refusal it earns.
    pub(super) fn of(
        _engine: &Engine,
        _session: &Session,
        generation: &Generation,
        _view: &str,
        index: usize,
        grouping: &Grouping,
    ) -> Result<Plan> {
        let outer = match &grouping.by {
            None => Outer::None,
            Some(By::Field { column, pick }) => Outer::Field(Field::of(generation, column, pick)?),
            Some(By::Layer { .. }) => {
                return Err(EngineError::Malformed(
                    "a layer grouping is not counted yet".to_string(),
                ))
            }
        };
        if grouping.cells.is_some() {
            return Err(EngineError::Malformed(
                "a cell level is not counted yet".to_string(),
            ));
        }
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
    ) -> Result<Page> {
        cx.check_cancelled()?;
        let groups = match &self.outer {
            Outer::None => Groups::whole(cx),
            Outer::Field(field) => field.groups(cx, position.chosen.as_deref(), timings)?,
        };
        let head = TableHead {
            grouping: self.grouping,
            total: cx.sets.set.size(),
            reference_total: cx.sets.reference.as_ref().map(|r| r.size()),
            groups: (!matches!(self.outer, Outer::None)).then_some(groups.distinct),
            resumed: position.chosen.is_some(),
        };
        let limit = page_rows as usize;
        let (rows, complete) = listed_rows(&groups, position.group, limit);
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
                after_cell: last.cell,
                stamp: position.stamp,
            }
        };
        let batch = self.batch(&groups, &head, &rows[..kept])?;
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
    fn batch(
        &self,
        groups: &Groups,
        head: &TableHead,
        rows: &[Row],
    ) -> Result<RecordBatch> {
        let mut fields: Vec<ArrowField> = Vec::new();
        let mut arrays: Vec<ArrayRef> = Vec::new();
        let mut push = |name: &str, array: ArrayRef, nullable: bool| {
            fields.push(ArrowField::new(name, array.data_type().clone(), nullable));
            arrays.push(array);
        };
        let listed = groups.keys.len() as u32;
        if !matches!(self.outer, Outer::None) {
            let kinds = Int8Array::from_iter_values(rows.iter().map(|row| {
                match row.group {
                    g if g < listed => 0i8,
                    g if g == listed => 1,
                    _ => 2,
                }
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
                    b.append_option(titles.get(row.group as usize).cloned().flatten());
                }
                push("title", Arc::new(b.finish()), true);
            }
        }
        if self.cells.is_some() {
            push(
                "cell",
                Arc::new(UInt64Array::from_iter_values(
                    rows.iter().map(|row| row.cell.unwrap_or(0)),
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

/// `(count / total) / (reference / reference_total)`, `None` where `reference` or `total` is 0.
fn lift(count: u64, total: u64, reference: u64, reference_total: u64) -> Option<f64> {
    (reference > 0 && total > 0).then(|| {
        (count as f64 / total as f64) / (reference as f64 / reference_total as f64)
    })
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
            cell: None,
            count,
            reference,
        })
        .collect();
    let complete = rows.len() <= limit;
    (rows, complete)
}
