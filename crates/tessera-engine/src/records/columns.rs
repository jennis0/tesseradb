//! A page's columns: every field read for the rows a walk took, and the Arrow batch they make.
//!
//! Every read here is for a row the walk already took from inside the composed mask. Rendered
//! fields are read from the requested view's row tail, value columns per item, and the record
//! store once per run over its items. A page reads its rows in runs and appends each row to typed
//! Arrow builders while the row fits the page's byte ceiling, counted from the builders' own
//! buffers. The first run is short, and each after it is sized from the bytes a row has come to
//! so far. However skewed the rows' sizes, a run keeps the record store's values only for the
//! leading rows that fit what the page has left, so it holds at most one row past it.

use std::collections::BinaryHeap;
use std::sync::Arc;

use arrow::array::{
    Array, ArrayBuilder, ArrayRef, BinaryBuilder, BooleanBuilder, DictionaryArray, Float32Builder,
    Float64Builder, Int16Builder, Int32Builder, Int64Builder, Int8Builder, ListBuilder,
    StringBuilder, TimestampMicrosecondBuilder, UInt16Builder, UInt32Builder, UInt64Builder,
    UInt8Builder,
};
use arrow::datatypes::{DataType, Field, Int32Type, Schema};
use arrow::record_batch::RecordBatch;
use rustc_hash::FxHashMap;
use tessera_filter::RecordValue as RV;
use tessera_spatial::tiler::ScalarType;
use tessera_spatial::unfixed32;
use tessera_types::{EntityId, MortonCode};

use super::plan::{FieldPlan, Home, Named, SystemField};
use super::walk::{PageCx, Taken};
use crate::error::{EngineError, Result};
use crate::viewport::{
    category_code, category_key, slice_value, stored_field_out, OpenView, ScalarOut,
};
use crate::Generation;

/// The rows the first read of a page covers.
const FIRST_READ: usize = 256;
/// The most values one read holds before they are appended, across its named fields.
const READ_VALUES: usize = 1 << 16;

/// One system field's values over a run, in run order.
enum SystemValues {
    Position(Vec<(f64, f64)>),
    ExternalId(Vec<Option<Vec<u8>>>),
    Labels(Vec<Vec<String>>),
}

/// The fields of one run of rows as their homes hold them.
struct Run {
    named: Vec<Vec<Option<RV>>>,
    system: Vec<SystemValues>,
    /// The leading rows whose fields were all kept: never fewer than one.
    complete: usize,
}

/// One named field's value for one row, converted and waiting to be appended.
enum Pending<'v> {
    Scalar(Option<ScalarOut>),
    /// A category's code, and where the page does not hold the code yet, the key it would add.
    Category {
        code: Option<u32>,
        fresh: Option<Option<&'v str>>,
    },
}

/// A named field's column as the page builds it.
enum Column {
    Bool(BooleanBuilder),
    U8(UInt8Builder),
    U16(UInt16Builder),
    U32(UInt32Builder),
    U64(UInt64Builder),
    I8(Int8Builder),
    I16(Int16Builder),
    I32(Int32Builder),
    I64(Int64Builder),
    F32(Float32Builder),
    F64(Float64Builder),
    TimestampUs(TimestampMicrosecondBuilder),
    Utf8(StringBuilder),
    /// A category: each row's position in `dictionary`, which holds each code's key once, in the
    /// order rows first carry it. An absent code, or one no binding explains, is null.
    Category {
        vocabulary: String,
        keys: Int32Builder,
        dictionary: StringBuilder,
        position: FxHashMap<u32, Option<i32>>,
    },
}

/// A system field's column as the page builds it.
enum SystemColumn {
    Position(Float64Builder, Float64Builder),
    ExternalId(BinaryBuilder),
    Labels(ListBuilder<StringBuilder>),
}

/// The columns of the rows a page has taken, and the bytes their buffers hold.
struct PageValues {
    rows: usize,
    tessera_ids: UInt64Builder,
    named: Vec<(String, ScalarType, Column)>,
    system: Vec<SystemColumn>,
    /// Present under `keep_unmatched`.
    matched: Option<BooleanBuilder>,
    bytes: usize,
}

/// The batch of the leading `rows` whose fields fit in `max_bytes`, never fewer than one row,
/// how many rows that is, and their bytes: every buffer the batch's columns hold.
pub(super) fn read_page(
    cx: &PageCx<'_>,
    plan: &FieldPlan,
    rows: &[Taken],
    keep_unmatched: bool,
    max_bytes: usize,
) -> Result<(RecordBatch, usize, usize)> {
    let mut page = PageValues::new(plan, keep_unmatched);
    let most = (READ_VALUES / (plan.named.len() + 1)).max(1);
    let mut start = 0usize;
    while start < rows.len() {
        let length = match page.rows {
            0 => FIRST_READ,
            taken => max_bytes.saturating_sub(page.bytes) / (page.bytes / taken).max(1) + 1,
        };
        let end = rows.len().min(start + length.clamp(1, most));
        let run = read_run(cx, plan, &rows[start..end], max_bytes.saturating_sub(page.bytes))?;
        let end = start + run.complete;
        if !page.take(run, &rows[start..end], plan, cx.generation, max_bytes)? {
            break;
        }
        start = end;
    }
    let (kept, bytes) = (page.rows, page.bytes);
    Ok((page.into_batch()?, kept, bytes))
}

/// Every field `plan` names for `rows`, as the fields' homes hold them. The record store's values
/// are kept for the earliest rows, in run order, that come to no more than `budget` bytes, and
/// for the first row it holds whatever that comes to: a value read past them is dropped as it is
/// read, so the run holds at most one row past the budget however the rows' sizes are skewed.
fn read_run(cx: &PageCx<'_>, plan: &FieldPlan, rows: &[Taken], budget: usize) -> Result<Run> {
    let (engine, generation, open) = (cx.engine, cx.generation, cx.open);
    let segments = &open.served.segments;
    let mut named: Vec<Vec<Option<RV>>> = Vec::with_capacity(plan.named.len());
    // Declaration position to the named field it fills, for the one record-store pass.
    let mut record_slot: FxHashMap<u16, usize> = FxHashMap::default();
    for (slot, field) in plan.named.iter().enumerate() {
        named.push(match &field.home {
            Home::Rendered => rendered_values(segments, field, rows),
            Home::ValueColumn(column) => generation
                .filter_columns
                .stored_values(column, rows.iter().map(|row| row.entity)),
            Home::Record(tag) => {
                record_slot.insert(*tag, slot);
                vec![None; rows.len()]
            }
        });
    }
    let mut complete = rows.len();
    if !record_slot.is_empty() {
        let mut at: FxHashMap<u32, usize> = FxHashMap::default();
        let mut entities: Vec<u32> = Vec::with_capacity(rows.len());
        for (i, row) in rows.iter().enumerate() {
            at.insert(row.entity, i);
            entities.push(row.entity);
        }
        entities.sort_unstable();
        // The rows whose values are held, by run position, with their bytes.
        let mut kept: BinaryHeap<(usize, usize)> = BinaryHeap::new();
        let mut held = 0usize;
        generation
            .filter_columns
            .records()
            .for_each_row_in(&croaring::Bitmap::of(&entities), &mut |entity, fields| {
                let Some(&i) = at.get(&entity) else {
                    return Ok(());
                };
                let mut bytes = 0;
                for field in fields {
                    if let Some(&slot) = record_slot.get(&field.tag) {
                        bytes += value_bytes(&field.value);
                        named[slot][i] = Some(field.value);
                    }
                }
                kept.push((i, bytes));
                held += bytes;
                while held > budget && kept.len() > 1 {
                    let (last, bytes) = kept.pop().expect("more than one row is held");
                    held -= bytes;
                    for &slot in record_slot.values() {
                        named[slot][last] = None;
                    }
                    complete = complete.min(last);
                }
                Ok(())
            })
            .map_err(|e| EngineError::Malformed(e.to_string()))?;
    }

    // The system fields are read only for the rows the page can take.
    let rows = &rows[..complete];
    let mut system = Vec::with_capacity(plan.system.len());
    for field in &plan.system {
        system.push(match field {
            SystemField::Position => {
                SystemValues::Position(positions(generation, open, rows)?)
            }
            SystemField::ExternalId => SystemValues::ExternalId(
                rows.iter()
                    .map(|row| {
                        engine
                            .external_id_of_in(generation, EntityId::new(u64::from(row.entity)))
                            .map_err(EngineError::Store)
                    })
                    .collect::<Result<_>>()?,
            ),
            SystemField::Labels => SystemValues::Labels(
                rows.iter()
                    .map(|row| engine.labels_for(generation, open.served.session, row.entity))
                    .collect::<Result<_>>()?,
            ),
        });
    }
    Ok(Run {
        named,
        system,
        complete,
    })
}

/// The bytes one record value holds.
fn value_bytes(value: &RV) -> usize {
    std::mem::size_of::<RV>()
        + match value {
            RV::Utf8(s) => s.len(),
            RV::List(values) => values.iter().map(value_bytes).sum(),
            _ => 0,
        }
}

/// A rendered field's values: the slot in the row tail, where the segment holds the column and
/// the row carries a value. Absence is a number's presence bitmap and a category's code 0.
fn rendered_values(
    segments: &[(&tessera_store::read::SegmentData, u32)],
    field: &Named,
    rows: &[Taken],
) -> Vec<Option<RV>> {
    let slices: Vec<_> = segments
        .iter()
        .map(|(segment, _)| segment.columns.scalar(&field.name))
        .collect();
    let presences: Vec<_> = segments
        .iter()
        .map(|(segment, _)| segment.columns.presence(&field.name))
        .collect();
    rows.iter()
        .map(|row| {
            let slice = slices[row.seg].as_ref()?;
            if field.vocabulary.is_none() && !presences[row.seg].contains(row.local) {
                return None;
            }
            slice_value(slice, row.local as usize)
        })
        .collect()
}

fn mismatch(name: &str, ty: ScalarType) -> EngineError {
    EngineError::Malformed(format!(
        "field '{name}' holds a value of a type other than its declared {}, so the bundle and its \
         declaration disagree",
        ty.arrow_type_name()
    ))
}

/// Each row's stored position converted back through the view's frame and projection: the centre
/// of its grid step, so within half a step of the coordinate it was placed from.
fn positions(
    generation: &Generation,
    open: &OpenView<'_>,
    rows: &[Taken],
) -> Result<Vec<(f64, f64)>> {
    let view = open.served.name;
    let descriptor = generation
        .bundle
        .manifest
        .views
        .iter()
        .find(|v| v.id == view)
        .ok_or_else(|| EngineError::UnknownView(view.to_string()))?;
    let q = descriptor.quantisation;
    let segments = &open.served.segments;
    Ok(rows
        .iter()
        .map(|row| {
            let (segment, _) = segments[row.seg];
            let local = row.local as usize;
            let (qx, qy) = tessera_spatial::unsplit32(
                MortonCode::new(segment.morton.u32()[local]),
                segment.columns.residual()[local],
            );
            descriptor.projection.inverse(
                unfixed32(qx, q.x_min, q.x_max),
                unfixed32(qy, q.y_min, q.y_max),
            )
        })
        .collect())
}

/// The bytes a buffer of `bits`-wide values grows by from `rows` values to one more.
fn bits_growth(rows: usize, bits: usize) -> usize {
    ((rows + 1) * bits).div_ceil(8) - (rows * bits).div_ceil(8)
}

/// The bytes a builder's validity bitmap grows by from `rows` rows to one more, where it `held`
/// one already: a builder makes one at its first null, a bit for every row it holds by then.
fn validity_growth(held: bool, null: bool, rows: usize) -> usize {
    match (held, null) {
        (true, _) => bits_growth(rows, 1),
        (false, true) => (rows + 1).div_ceil(8),
        (false, false) => 0,
    }
}

/// The bytes a builder's validity bitmap holds.
fn validity(slice: Option<&[u8]>) -> usize {
    slice.map_or(0, <[u8]>::len)
}

impl Column {
    fn new(field: &Named) -> Column {
        if let Some(vocabulary) = &field.vocabulary {
            return Column::Category {
                vocabulary: vocabulary.clone(),
                keys: Int32Builder::new(),
                dictionary: StringBuilder::new(),
                position: FxHashMap::default(),
            };
        }
        match field.ty {
            ScalarType::Bool => Column::Bool(BooleanBuilder::new()),
            ScalarType::U8 => Column::U8(UInt8Builder::new()),
            ScalarType::U16 => Column::U16(UInt16Builder::new()),
            ScalarType::U32 => Column::U32(UInt32Builder::new()),
            ScalarType::U64 => Column::U64(UInt64Builder::new()),
            ScalarType::I8 => Column::I8(Int8Builder::new()),
            ScalarType::I16 => Column::I16(Int16Builder::new()),
            ScalarType::I32 => Column::I32(Int32Builder::new()),
            ScalarType::I64 => Column::I64(Int64Builder::new()),
            ScalarType::F32 => Column::F32(Float32Builder::new()),
            ScalarType::F64 => Column::F64(Float64Builder::new()),
            ScalarType::TimestampUs => {
                Column::TimestampUs(TimestampMicrosecondBuilder::new().with_timezone("UTC"))
            }
            ScalarType::Utf8 | ScalarType::Keyword | ScalarType::Text => {
                Column::Utf8(StringBuilder::new())
            }
        }
    }

    /// Whether the column's builder holds a validity bitmap yet.
    fn holds_nulls(&self) -> bool {
        match self {
            Column::Bool(b) => b.validity_slice().is_some(),
            Column::U8(b) => b.validity_slice().is_some(),
            Column::U16(b) => b.validity_slice().is_some(),
            Column::U32(b) => b.validity_slice().is_some(),
            Column::U64(b) => b.validity_slice().is_some(),
            Column::I8(b) => b.validity_slice().is_some(),
            Column::I16(b) => b.validity_slice().is_some(),
            Column::I32(b) => b.validity_slice().is_some(),
            Column::I64(b) => b.validity_slice().is_some(),
            Column::F32(b) => b.validity_slice().is_some(),
            Column::F64(b) => b.validity_slice().is_some(),
            Column::TimestampUs(b) => b.validity_slice().is_some(),
            Column::Utf8(b) => b.validity_slice().is_some(),
            Column::Category { keys, .. } => keys.validity_slice().is_some(),
        }
    }

    /// The bytes the column's buffers hold.
    fn bytes(&self) -> usize {
        macro_rules! primitive {
            ($b:expr) => {
                std::mem::size_of_val($b.values_slice()) + validity($b.validity_slice())
            };
        }
        match self {
            Column::Bool(b) => b.values_slice().len() + validity(b.validity_slice()),
            Column::U8(b) => primitive!(b),
            Column::U16(b) => primitive!(b),
            Column::U32(b) => primitive!(b),
            Column::U64(b) => primitive!(b),
            Column::I8(b) => primitive!(b),
            Column::I16(b) => primitive!(b),
            Column::I32(b) => primitive!(b),
            Column::I64(b) => primitive!(b),
            Column::F32(b) => primitive!(b),
            Column::F64(b) => primitive!(b),
            Column::TimestampUs(b) => primitive!(b),
            Column::Utf8(b) => string_bytes(b),
            Column::Category {
                keys, dictionary, ..
            } => primitive!(keys) + string_bytes(dictionary),
        }
    }

    /// Append one row's value, which must be of the column's kind.
    fn append(&mut self, value: Pending<'_>, name: &str, ty: ScalarType) -> Result<()> {
        macro_rules! scalar {
            ($b:expr, $variant:ident, $value:expr) => {
                match $value {
                    None => $b.append_null(),
                    Some(ScalarOut::$variant(x)) => $b.append_value(x),
                    Some(_) => return Err(mismatch(name, ty)),
                }
            };
        }
        match (self, value) {
            (
                Column::Category {
                    keys,
                    dictionary,
                    position,
                    ..
                },
                Pending::Category { code, fresh },
            ) => {
                if let (Some(code), Some(key)) = (code, fresh) {
                    let at = key.map(|key| {
                        dictionary.append_value(key);
                        i32::try_from(dictionary.len() - 1).expect("a page's keys fit in i32")
                    });
                    position.insert(code, at);
                }
                keys.append_option(code.and_then(|code| position[&code]));
            }
            (Column::Bool(b), Pending::Scalar(v)) => scalar!(b, Bool, v),
            (Column::U8(b), Pending::Scalar(v)) => scalar!(b, U8, v),
            (Column::U16(b), Pending::Scalar(v)) => scalar!(b, U16, v),
            (Column::U32(b), Pending::Scalar(v)) => scalar!(b, U32, v),
            (Column::U64(b), Pending::Scalar(v)) => scalar!(b, U64, v),
            (Column::I8(b), Pending::Scalar(v)) => scalar!(b, I8, v),
            (Column::I16(b), Pending::Scalar(v)) => scalar!(b, I16, v),
            (Column::I32(b), Pending::Scalar(v)) => scalar!(b, I32, v),
            (Column::I64(b), Pending::Scalar(v)) => scalar!(b, I64, v),
            (Column::F32(b), Pending::Scalar(v)) => scalar!(b, F32, v),
            (Column::F64(b), Pending::Scalar(v)) => scalar!(b, F64, v),
            (Column::TimestampUs(b), Pending::Scalar(v)) => scalar!(b, TimestampUs, v),
            (Column::Utf8(b), Pending::Scalar(v)) => match v {
                None => b.append_null(),
                Some(ScalarOut::Utf8(s)) => b.append_value(s),
                Some(_) => return Err(mismatch(name, ty)),
            },
            _ => unreachable!("a field's pending value is of its column's kind"),
        }
        Ok(())
    }

    fn finish(self) -> Result<ArrayRef> {
        Ok(match self {
            Column::Bool(mut b) => Arc::new(b.finish()),
            Column::U8(mut b) => Arc::new(b.finish()),
            Column::U16(mut b) => Arc::new(b.finish()),
            Column::U32(mut b) => Arc::new(b.finish()),
            Column::U64(mut b) => Arc::new(b.finish()),
            Column::I8(mut b) => Arc::new(b.finish()),
            Column::I16(mut b) => Arc::new(b.finish()),
            Column::I32(mut b) => Arc::new(b.finish()),
            Column::I64(mut b) => Arc::new(b.finish()),
            Column::F32(mut b) => Arc::new(b.finish()),
            Column::F64(mut b) => Arc::new(b.finish()),
            Column::TimestampUs(mut b) => Arc::new(b.finish()),
            Column::Utf8(mut b) => Arc::new(b.finish()),
            Column::Category {
                mut keys,
                mut dictionary,
                ..
            } => Arc::new(
                DictionaryArray::<Int32Type>::try_new(keys.finish(), Arc::new(dictionary.finish()))
                    .map_err(|e| {
                        EngineError::Malformed(format!("a category column did not assemble: {e}"))
                    })?,
            ),
        })
    }
}

/// The bytes a string builder's buffers hold: its values, its offsets and its validity.
fn string_bytes(b: &StringBuilder) -> usize {
    b.values_slice().len() + std::mem::size_of_val(b.offsets_slice()) + validity(b.validity_slice())
}

impl SystemColumn {
    fn bytes(&self) -> usize {
        match self {
            SystemColumn::Position(x, y) => {
                std::mem::size_of_val(x.values_slice()) + std::mem::size_of_val(y.values_slice())
            }
            SystemColumn::ExternalId(b) => {
                b.values_slice().len()
                    + std::mem::size_of_val(b.offsets_slice())
                    + validity(b.validity_slice())
            }
            SystemColumn::Labels(b) => {
                std::mem::size_of_val(b.offsets_slice()) + string_bytes(b.values_ref())
            }
        }
    }
}

impl PageValues {
    /// An empty page, whose bytes are what its builders hold before a row: a string's first
    /// offset.
    fn new(plan: &FieldPlan, keep_unmatched: bool) -> PageValues {
        let mut page = PageValues {
            rows: 0,
            tessera_ids: UInt64Builder::new(),
            named: plan
                .named
                .iter()
                .map(|field| (field.name.clone(), field.ty, Column::new(field)))
                .collect(),
            system: plan
                .system
                .iter()
                .map(|field| match field {
                    SystemField::Position => {
                        SystemColumn::Position(Float64Builder::new(), Float64Builder::new())
                    }
                    SystemField::ExternalId => SystemColumn::ExternalId(BinaryBuilder::new()),
                    SystemField::Labels => {
                        SystemColumn::Labels(ListBuilder::new(StringBuilder::new()))
                    }
                })
                .collect(),
            matched: keep_unmatched.then(BooleanBuilder::new),
            bytes: 0,
        };
        page.bytes = page.measure();
        page
    }

    /// The bytes every column's buffers hold.
    fn measure(&self) -> usize {
        std::mem::size_of_val(self.tessera_ids.values_slice())
            + self.named.iter().map(|(_, _, c)| c.bytes()).sum::<usize>()
            + self.system.iter().map(SystemColumn::bytes).sum::<usize>()
            + self.matched.as_ref().map_or(0, |b| b.values_slice().len())
    }

    /// Take `run`'s rows into the page in order while they fit `max_bytes`, and the first row
    /// whatever it comes to. A row's bytes are reckoned before it is appended, from its values and
    /// what each builder already holds, a validity bitmap made at a column's first null included,
    /// and the page's are measured from its buffers after. `false` once a row did not fit.
    fn take(
        &mut self,
        mut run: Run,
        rows: &[Taken],
        plan: &FieldPlan,
        generation: &Generation,
        max_bytes: usize,
    ) -> Result<bool> {
        let vocabularies = &generation.vocabularies;
        let mut pending: Vec<Pending<'_>> = Vec::with_capacity(plan.named.len());
        for (i, row) in rows.iter().enumerate() {
            pending.clear();
            let n = self.rows;
            let mut row_bytes = 8 + self.matched.as_ref().map_or(0, |_| bits_growth(n, 1));
            for ((field, (_, _, column)), raw) in
                plan.named.iter().zip(&self.named).zip(&mut run.named)
            {
                let value = raw[i].take();
                let nulls = column.holds_nulls();
                match column {
                    Column::Category {
                        vocabulary,
                        position,
                        ..
                    } => {
                        let code = match value {
                            None => None,
                            Some(value) => Some(
                                category_code(&value)
                                    .ok_or_else(|| mismatch(&field.name, field.ty))?,
                            ),
                        };
                        let fresh = code
                            .filter(|code| !position.contains_key(code))
                            .map(|code| category_key(code, Some(vocabulary), vocabularies));
                        let null = match (code, fresh) {
                            (None, _) => true,
                            (Some(_), Some(key)) => key.is_none(),
                            (Some(code), None) => position[&code].is_none(),
                        };
                        row_bytes += 4 + validity_growth(nulls, null, n);
                        if let Some(Some(key)) = fresh {
                            row_bytes += 4 + key.len();
                        }
                        pending.push(Pending::Category { code, fresh });
                    }
                    _ => {
                        let out =
                            value.and_then(|v| stored_field_out(v, field.ty, None, vocabularies));
                        row_bytes += validity_growth(nulls, out.is_none(), n)
                            + match (field.ty.row_bits(), &out) {
                                (Some(bits), _) => bits_growth(n, bits as usize),
                                (None, Some(ScalarOut::Utf8(s))) => 4 + s.len(),
                                (None, _) => 4,
                            };
                        pending.push(Pending::Scalar(out));
                    }
                }
            }
            for (column, values) in self.system.iter().zip(&run.system) {
                row_bytes += match (column, values) {
                    (_, SystemValues::Position(_)) => 16,
                    (SystemColumn::ExternalId(b), SystemValues::ExternalId(ids)) => {
                        let held = b.validity_slice().is_some();
                        4 + ids[i].as_ref().map_or(0, Vec::len)
                            + validity_growth(held, ids[i].is_none(), n)
                    }
                    (_, SystemValues::Labels(labels)) => {
                        4 + labels[i].iter().map(|l| 4 + l.len()).sum::<usize>()
                    }
                    _ => unreachable!("a run's system fields are the page's, in the page's order"),
                };
            }
            if self.rows > 0 && self.bytes + row_bytes > max_bytes {
                return Ok(false);
            }
            self.tessera_ids.append_value(row.tessera_id);
            if let Some(matched) = &mut self.matched {
                matched.append_value(row.matched);
            }
            for ((name, ty, column), value) in self.named.iter_mut().zip(pending.drain(..)) {
                column.append(value, name, *ty)?;
            }
            for (column, values) in self.system.iter_mut().zip(&mut run.system) {
                match (column, values) {
                    (SystemColumn::Position(x, y), SystemValues::Position(run)) => {
                        x.append_value(run[i].0);
                        y.append_value(run[i].1);
                    }
                    (SystemColumn::ExternalId(b), SystemValues::ExternalId(run)) => {
                        b.append_option(run[i].take())
                    }
                    (SystemColumn::Labels(b), SystemValues::Labels(run)) => {
                        for label in std::mem::take(&mut run[i]) {
                            b.values().append_value(label);
                        }
                        b.append(true);
                    }
                    _ => unreachable!("a run's system fields are the page's, in the page's order"),
                }
            }
            self.rows += 1;
            self.bytes = self.measure();
        }
        Ok(true)
    }

    /// The page as one batch: `tessera_id`, the named fields, the system fields, then the
    /// matched column.
    fn into_batch(mut self) -> Result<RecordBatch> {
        let mut fields: Vec<Field> = vec![Field::new("tessera_id", DataType::UInt64, false)];
        let mut arrays: Vec<ArrayRef> = vec![Arc::new(self.tessera_ids.finish())];
        for (name, _, column) in self.named {
            let array = column.finish()?;
            fields.push(Field::new(name, array.data_type().clone(), true));
            arrays.push(array);
        }
        for column in self.system {
            match column {
                SystemColumn::Position(mut x, mut y) => {
                    fields.push(Field::new("tessera:x", DataType::Float64, false));
                    arrays.push(Arc::new(x.finish()));
                    fields.push(Field::new("tessera:y", DataType::Float64, false));
                    arrays.push(Arc::new(y.finish()));
                }
                SystemColumn::ExternalId(mut b) => {
                    fields.push(Field::new("tessera:external_id", DataType::Binary, true));
                    arrays.push(Arc::new(b.finish()));
                }
                SystemColumn::Labels(mut b) => {
                    let array = b.finish();
                    fields.push(Field::new("tessera:labels", array.data_type().clone(), false));
                    arrays.push(Arc::new(array));
                }
            }
        }
        if let Some(mut matched) = self.matched {
            fields.push(Field::new("tessera:matched", DataType::Boolean, false));
            arrays.push(Arc::new(matched.finish()));
        }
        RecordBatch::try_new(Arc::new(Schema::new(fields)), arrays)
            .map_err(|e| EngineError::Malformed(format!("a page did not assemble: {e}")))
    }
}
