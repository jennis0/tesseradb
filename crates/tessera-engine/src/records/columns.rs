//! A page's columns: every field read for the rows a walk took, and the Arrow batch they make.
//!
//! Every read here is for a row the walk already took from inside the composed mask. Rendered
//! fields are read from the requested view's row tail, value columns per item, and the record
//! store once per read over its items. Rows are read a run at a time, the first run short and
//! each after it twice as long, and taken into the page while they fit its byte ceiling, so the
//! rows past the ceiling are read at most once and never kept.

use std::sync::Arc;

use arrow::array::{
    Array, ArrayRef, BinaryArray, BooleanArray, DictionaryArray, Float32Array, Float64Array,
    Int16Array, Int32Array, Int64Array, Int8Array, ListBuilder, StringArray, StringBuilder,
    TimestampMicrosecondArray, UInt16Array, UInt32Array, UInt64Array, UInt8Array,
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

/// The rows the first read of a page covers. Each read after it covers twice as many.
const FIRST_READ: usize = 256;

/// One named field's values over the rows a page has taken, in page order.
enum Values {
    Scalar {
        ty: ScalarType,
        values: Vec<Option<ScalarOut>>,
    },
    /// A category: each row's position in `dictionary`, which holds each code's key once, in the
    /// order rows first carry it. An absent code, or one no binding explains, is null.
    Category {
        vocabulary: String,
        dictionary: Vec<String>,
        position: FxHashMap<u32, Option<i32>>,
        keys: Vec<Option<i32>>,
    },
}

/// One system field's values, in page order.
enum SystemValues {
    Position(Vec<(f64, f64)>),
    ExternalId(Vec<Option<Vec<u8>>>),
    Labels(Vec<Vec<String>>),
}

/// One named field's value for one row, converted and waiting to be taken into the page.
enum Pending<'v> {
    Scalar(Option<ScalarOut>),
    /// A category's code, and where the page does not hold the code yet, the key it would add.
    Category {
        code: Option<u32>,
        fresh: Option<Option<&'v str>>,
    },
}

/// The fields of one run of rows as their homes hold them.
struct Run {
    named: Vec<Vec<Option<RV>>>,
    system: Vec<SystemValues>,
}

/// The columns of the rows a page has taken, and the Arrow bytes they come to.
struct PageValues {
    tessera_ids: Vec<u64>,
    named: Vec<(String, Values)>,
    system: Vec<SystemValues>,
    /// Present under `keep_unmatched`.
    matched: Option<Vec<bool>>,
    bytes: usize,
    /// The validity bytes every row adds: one per eight columns.
    validity: usize,
}

/// The batch of the leading `rows` whose fields fit in `max_bytes`, never fewer than one row,
/// how many rows that is, and their bytes: every value and offset a row adds to its column, a
/// bool as a byte, each distinct category key once, and a byte per eight columns for validity.
pub(super) fn read_page(
    cx: &PageCx<'_>,
    plan: &FieldPlan,
    rows: &[Taken],
    keep_unmatched: bool,
    max_bytes: usize,
) -> Result<(RecordBatch, usize, usize)> {
    let mut page = PageValues::new(plan, keep_unmatched);
    let (mut start, mut length) = (0usize, FIRST_READ);
    while start < rows.len() {
        let end = rows.len().min(start + length);
        let run = read_run(cx, plan, &rows[start..end])?;
        if !page.take(run, &rows[start..end], plan, cx.generation, max_bytes)? {
            break;
        }
        start = end;
        length = length.saturating_mul(2);
    }
    let (kept, bytes) = (page.tessera_ids.len(), page.bytes);
    Ok((page.into_batch()?, kept, bytes))
}

/// Every field `plan` names for `rows`, as the fields' homes hold them.
fn read_run(cx: &PageCx<'_>, plan: &FieldPlan, rows: &[Taken]) -> Result<Run> {
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
    if !record_slot.is_empty() {
        let mut at: FxHashMap<u32, usize> = FxHashMap::default();
        let mut entities: Vec<u32> = Vec::with_capacity(rows.len());
        for (i, row) in rows.iter().enumerate() {
            at.insert(row.entity, i);
            entities.push(row.entity);
        }
        entities.sort_unstable();
        generation
            .filter_columns
            .records()
            .for_each_row_in(&croaring::Bitmap::of(&entities), &mut |entity, fields| {
                let Some(&i) = at.get(&entity) else {
                    return Ok(());
                };
                for field in fields {
                    if let Some(&slot) = record_slot.get(&field.tag) {
                        named[slot][i] = Some(field.value);
                    }
                }
                Ok(())
            })
            .map_err(|e| EngineError::Malformed(e.to_string()))?;
    }

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
    Ok(Run { named, system })
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

/// The bytes one value of a fixed-width type adds to its column.
fn width(ty: ScalarType) -> usize {
    match ty {
        ScalarType::Bool | ScalarType::U8 | ScalarType::I8 => 1,
        ScalarType::U16 | ScalarType::I16 => 2,
        ScalarType::U32 | ScalarType::I32 | ScalarType::F32 => 4,
        ScalarType::U64 | ScalarType::I64 | ScalarType::F64 | ScalarType::TimestampUs => 8,
        // A string's offset; its bytes are counted per value.
        ScalarType::Utf8 | ScalarType::Keyword | ScalarType::Text => 4,
    }
}

impl PageValues {
    fn new(plan: &FieldPlan, keep_unmatched: bool) -> PageValues {
        let named: Vec<(String, Values)> = plan
            .named
            .iter()
            .map(|field| {
                let values = match &field.vocabulary {
                    None => Values::Scalar {
                        ty: field.ty,
                        values: Vec::new(),
                    },
                    Some(vocabulary) => Values::Category {
                        vocabulary: vocabulary.clone(),
                        dictionary: Vec::new(),
                        position: FxHashMap::default(),
                        keys: Vec::new(),
                    },
                };
                (field.name.clone(), values)
            })
            .collect();
        let system: Vec<SystemValues> = plan
            .system
            .iter()
            .map(|field| match field {
                SystemField::Position => SystemValues::Position(Vec::new()),
                SystemField::ExternalId => SystemValues::ExternalId(Vec::new()),
                SystemField::Labels => SystemValues::Labels(Vec::new()),
            })
            .collect();
        let columns = 1
            + named.len()
            + plan
                .system
                .iter()
                .map(|field| match field {
                    SystemField::Position => 2,
                    _ => 1,
                })
                .sum::<usize>()
            + usize::from(keep_unmatched);
        PageValues {
            tessera_ids: Vec::new(),
            named,
            system,
            matched: keep_unmatched.then(Vec::new),
            bytes: 0,
            validity: columns.div_ceil(8),
        }
    }

    /// Take `run`'s rows into the page in order while they fit `max_bytes`, and the first row
    /// whatever it comes to. `false` once a row did not fit.
    fn take(
        &mut self,
        mut run: Run,
        rows: &[Taken],
        plan: &FieldPlan,
        generation: &Generation,
        max_bytes: usize,
    ) -> Result<bool> {
        let vocabularies = &generation.vocabularies;
        for (i, row) in rows.iter().enumerate() {
            let mut row_bytes = 8 + self.validity + usize::from(self.matched.is_some());
            let mut pending: Vec<Pending<'_>> = Vec::with_capacity(plan.named.len());
            for ((field, (_, values)), raw) in
                plan.named.iter().zip(&self.named).zip(&mut run.named)
            {
                let value = raw[i].take();
                match values {
                    Values::Scalar { ty, .. } => {
                        let out = value.and_then(|v| stored_field_out(v, *ty, None, vocabularies));
                        row_bytes += width(*ty)
                            + match &out {
                                Some(ScalarOut::Utf8(s)) => s.len(),
                                _ => 0,
                            };
                        pending.push(Pending::Scalar(out));
                    }
                    Values::Category {
                        vocabulary,
                        position,
                        ..
                    } => {
                        row_bytes += 4;
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
                        if let Some(Some(key)) = fresh {
                            row_bytes += 4 + key.len();
                        }
                        pending.push(Pending::Category { code, fresh });
                    }
                }
            }
            for values in &run.system {
                row_bytes += match values {
                    SystemValues::Position(_) => 16,
                    SystemValues::ExternalId(ids) => 4 + ids[i].as_ref().map_or(0, Vec::len),
                    SystemValues::Labels(labels) => {
                        4 + labels[i].iter().map(|l| 4 + l.len()).sum::<usize>()
                    }
                };
            }
            if !self.tessera_ids.is_empty() && self.bytes + row_bytes > max_bytes {
                return Ok(false);
            }
            self.bytes += row_bytes;
            self.tessera_ids.push(row.tessera_id);
            if let Some(matched) = &mut self.matched {
                matched.push(row.matched);
            }
            for ((_, values), value) in self.named.iter_mut().zip(pending) {
                match (values, value) {
                    (Values::Scalar { values, .. }, Pending::Scalar(out)) => values.push(out),
                    (
                        Values::Category {
                            dictionary,
                            position,
                            keys,
                            ..
                        },
                        Pending::Category { code, fresh },
                    ) => {
                        if let (Some(code), Some(key)) = (code, fresh) {
                            let at = key.map(|key| {
                                dictionary.push(key.to_string());
                                i32::try_from(dictionary.len() - 1)
                                    .expect("a page's keys fit in i32")
                            });
                            position.insert(code, at);
                        }
                        keys.push(code.and_then(|code| position[&code]));
                    }
                    _ => unreachable!("a field's pending value is of its column's kind"),
                }
            }
            for (page, run) in self.system.iter_mut().zip(&mut run.system) {
                match (page, run) {
                    (SystemValues::Position(page), SystemValues::Position(run)) => {
                        page.push(run[i])
                    }
                    (SystemValues::ExternalId(page), SystemValues::ExternalId(run)) => {
                        page.push(run[i].take())
                    }
                    (SystemValues::Labels(page), SystemValues::Labels(run)) => {
                        page.push(std::mem::take(&mut run[i]))
                    }
                    _ => unreachable!("a run's system fields are the page's, in the page's order"),
                }
            }
        }
        Ok(true)
    }

    /// The page as one batch: `tessera_id`, the named fields, the system fields, then the
    /// matched column.
    fn into_batch(self) -> Result<RecordBatch> {
        let mut fields: Vec<Field> = vec![Field::new("tessera_id", DataType::UInt64, false)];
        let mut arrays: Vec<ArrayRef> = vec![Arc::new(UInt64Array::from(self.tessera_ids))];
        for (name, values) in self.named {
            let array = match values {
                Values::Scalar { ty, values } => scalar_array(&name, ty, values)?,
                Values::Category {
                    dictionary, keys, ..
                } => category_array(dictionary, keys)?,
            };
            fields.push(Field::new(name, array.data_type().clone(), true));
            arrays.push(array);
        }
        for values in self.system {
            match values {
                SystemValues::Position(xy) => {
                    for (name, axis) in [("tessera:x", 0), ("tessera:y", 1)] {
                        fields.push(Field::new(name, DataType::Float64, false));
                        arrays.push(Arc::new(Float64Array::from_iter_values(
                            xy.iter().map(|p| if axis == 0 { p.0 } else { p.1 }),
                        )));
                    }
                }
                SystemValues::ExternalId(ids) => {
                    fields.push(Field::new("tessera:external_id", DataType::Binary, true));
                    arrays.push(Arc::new(BinaryArray::from_iter(ids)));
                }
                SystemValues::Labels(labels) => {
                    let mut builder = ListBuilder::new(StringBuilder::new());
                    for row in labels {
                        for label in row {
                            builder.values().append_value(label);
                        }
                        builder.append(true);
                    }
                    let array = builder.finish();
                    fields.push(Field::new("tessera:labels", array.data_type().clone(), false));
                    arrays.push(Arc::new(array));
                }
            }
        }
        if let Some(matched) = self.matched {
            fields.push(Field::new("tessera:matched", DataType::Boolean, false));
            arrays.push(Arc::new(BooleanArray::from(matched)));
        }
        RecordBatch::try_new(Arc::new(Schema::new(fields)), arrays)
            .map_err(|e| EngineError::Malformed(format!("a page did not assemble: {e}")))
    }
}

/// A category column over `dictionary`, which holds exactly the keys `keys` carries.
fn category_array(dictionary: Vec<String>, keys: Vec<Option<i32>>) -> Result<ArrayRef> {
    let array = DictionaryArray::<Int32Type>::try_new(
        Int32Array::from(keys),
        Arc::new(StringArray::from(dictionary)),
    )
    .map_err(|e| EngineError::Malformed(format!("a category column did not assemble: {e}")))?;
    Ok(Arc::new(array))
}

/// A column of `ty` from values in their served form.
fn scalar_array(name: &str, ty: ScalarType, values: Vec<Option<ScalarOut>>) -> Result<ArrayRef> {
    macro_rules! column {
        ($array:ty, $variant:ident) => {{
            let mut out = Vec::with_capacity(values.len());
            for value in values {
                out.push(match value {
                    None => None,
                    Some(ScalarOut::$variant(x)) => Some(x),
                    Some(_) => return Err(mismatch(name, ty)),
                });
            }
            <$array>::from(out)
        }};
    }
    Ok(match ty {
        ScalarType::Bool => Arc::new(column!(BooleanArray, Bool)),
        ScalarType::U8 => Arc::new(column!(UInt8Array, U8)),
        ScalarType::U16 => Arc::new(column!(UInt16Array, U16)),
        ScalarType::U32 => Arc::new(column!(UInt32Array, U32)),
        ScalarType::U64 => Arc::new(column!(UInt64Array, U64)),
        ScalarType::I8 => Arc::new(column!(Int8Array, I8)),
        ScalarType::I16 => Arc::new(column!(Int16Array, I16)),
        ScalarType::I32 => Arc::new(column!(Int32Array, I32)),
        ScalarType::I64 => Arc::new(column!(Int64Array, I64)),
        ScalarType::F32 => Arc::new(column!(Float32Array, F32)),
        ScalarType::F64 => Arc::new(column!(Float64Array, F64)),
        ScalarType::TimestampUs => {
            Arc::new(column!(TimestampMicrosecondArray, TimestampUs).with_timezone("UTC"))
        }
        ScalarType::Utf8 | ScalarType::Keyword | ScalarType::Text => {
            Arc::new(column!(StringArray, Utf8))
        }
    })
}
