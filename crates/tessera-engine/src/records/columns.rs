//! A page's columns: every field read for the rows a walk took, and the Arrow batch they make.
//!
//! Every read here is for a row the walk already took from inside the composed mask. Rendered
//! fields are read from the requested view's row tail, value columns per item, and the record
//! store once per page over the page's items.

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
use super::walk::Taken;
use crate::engine::Engine;
use crate::error::{EngineError, Result};
use crate::session::Session;
use crate::viewport::{
    category_code, category_key, slice_value, stored_field_out, OpenView, ScalarOut,
};
use crate::Generation;

/// One named field's values over a page, in page order.
enum Values {
    Scalar {
        ty: ScalarType,
        values: Vec<Option<ScalarOut>>,
    },
    /// A category: each row's position in `dictionary`, which holds each code's key once, in the
    /// order rows first carry it. An absent code, or one no binding explains, is null.
    Category {
        dictionary: Vec<String>,
        keys: Vec<Option<i32>>,
    },
}

/// One system field's values over a page, in page order.
enum SystemValues {
    Position(Vec<(f64, f64)>),
    ExternalId(Vec<Option<Vec<u8>>>),
    Labels(Vec<Vec<String>>),
}

/// Every column of one page, before it is cut to the byte ceiling.
pub(super) struct PageValues {
    tessera_ids: Vec<u64>,
    named: Vec<(String, Values)>,
    system: Vec<SystemValues>,
    /// Present under `keep_unmatched`.
    matched: Option<Vec<bool>>,
}

impl Engine {
    /// Read every field `plan` names for `rows`, from the generation the page was walked in.
    pub(super) fn read_page(
        &self,
        session: &Session,
        generation: &Generation,
        open: &OpenView<'_>,
        plan: &FieldPlan,
        rows: &[Taken],
        keep_unmatched: bool,
    ) -> Result<PageValues> {
        let segments = &open.served.segments;
        let mut named: Vec<(String, Vec<Option<RV>>)> = Vec::with_capacity(plan.named.len());
        // Declaration position to the named field it fills, for the one record-store pass.
        let mut record_slot: FxHashMap<u16, usize> = FxHashMap::default();
        for (slot, field) in plan.named.iter().enumerate() {
            let values = match &field.home {
                Home::Rendered => rendered_values(segments, field, rows),
                Home::ValueColumn(column) => generation
                    .filter_columns
                    .stored_values(column, rows.iter().map(|row| row.entity)),
                Home::Record(tag) => {
                    record_slot.insert(*tag, slot);
                    vec![None; rows.len()]
                }
            };
            named.push((field.name.clone(), values));
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
                            named[slot].1[i] = Some(field.value);
                        }
                    }
                    Ok(())
                })
                .map_err(|e| EngineError::Malformed(e.to_string()))?;
        }
        let named = plan
            .named
            .iter()
            .zip(named)
            .map(|(field, (name, values))| Ok((name, typed(field, values, generation)?)))
            .collect::<Result<Vec<_>>>()?;

        let mut system = Vec::with_capacity(plan.system.len());
        for field in &plan.system {
            system.push(match field {
                SystemField::Position => {
                    SystemValues::Position(positions(generation, open, rows)?)
                }
                SystemField::ExternalId => SystemValues::ExternalId(
                    rows.iter()
                        .map(|row| {
                            self.external_id_of_in(generation, EntityId::new(u64::from(row.entity)))
                                .map_err(EngineError::Store)
                        })
                        .collect::<Result<_>>()?,
                ),
                SystemField::Labels => SystemValues::Labels(
                    rows.iter()
                        .map(|row| self.labels_for(generation, session, row.entity))
                        .collect::<Result<_>>()?,
                ),
            });
        }
        Ok(PageValues {
            tessera_ids: rows.iter().map(|row| row.tessera_id).collect(),
            named,
            system,
            matched: keep_unmatched.then(|| rows.iter().map(|row| row.matched).collect()),
        })
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

/// A field's stored values in the form its column carries, by the conversions the item card
/// uses: a category's codes resolved to keys, each code looked up once, and every other family
/// through `stored_field_out`.
fn typed(field: &Named, values: Vec<Option<RV>>, generation: &Generation) -> Result<Values> {
    let vocabularies = &generation.vocabularies;
    let Some(vocabulary) = &field.vocabulary else {
        return Ok(Values::Scalar {
            ty: field.ty,
            values: values
                .into_iter()
                .map(|value| value.and_then(|v| stored_field_out(v, field.ty, None, vocabularies)))
                .collect(),
        });
    };
    let mut dictionary: Vec<String> = Vec::new();
    let mut position: FxHashMap<u32, Option<i32>> = FxHashMap::default();
    let mut keys = Vec::with_capacity(values.len());
    for value in values {
        let Some(value) = value else {
            keys.push(None);
            continue;
        };
        let code = category_code(&value).ok_or_else(|| mismatch(&field.name, field.ty))?;
        keys.push(*position.entry(code).or_insert_with(|| {
            let key = category_key(code, Some(vocabulary), vocabularies)?;
            dictionary.push(key.to_string());
            Some(i32::try_from(dictionary.len() - 1).expect("a page's keys fit in i32"))
        }));
    }
    Ok(Values::Category { dictionary, keys })
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
    /// How many leading rows fit in `max_bytes`, never fewer than one, and the bytes they come
    /// to: every value and offset the row adds to its column, a bool as a byte, each distinct
    /// category key once, and a byte per eight columns for validity.
    fn fit(&self, max_bytes: usize) -> (usize, usize) {
        let columns = 1
            + self.named.len()
            + self
                .system
                .iter()
                .map(|s| match s {
                    SystemValues::Position(_) => 2,
                    _ => 1,
                })
                .sum::<usize>()
            + usize::from(self.matched.is_some());
        let validity = columns.div_ceil(8);
        // Per category column, how many of its keys the rows so far have carried: keys are
        // numbered in the order rows first carry them, so a row carries a new one exactly when its
        // number is this count.
        let mut carried: Vec<i32> = vec![0; self.named.len()];
        let mut total = 0usize;
        for i in 0..self.tessera_ids.len() {
            let mut row = 8 + validity + usize::from(self.matched.is_some());
            for (slot, (_, values)) in self.named.iter().enumerate() {
                row += match values {
                    Values::Scalar { ty, values } => {
                        width(*ty)
                            + match &values[i] {
                                Some(ScalarOut::Utf8(s)) => s.len(),
                                _ => 0,
                            }
                    }
                    Values::Category { dictionary, keys } => {
                        4 + match keys[i] {
                            Some(at) if at == carried[slot] => {
                                carried[slot] += 1;
                                4 + dictionary[at as usize].len()
                            }
                            _ => 0,
                        }
                    }
                };
            }
            for values in &self.system {
                row += match values {
                    SystemValues::Position(_) => 16,
                    SystemValues::ExternalId(ids) => 4 + ids[i].as_ref().map_or(0, Vec::len),
                    SystemValues::Labels(labels) => {
                        4 + labels[i].iter().map(|l| 4 + l.len()).sum::<usize>()
                    }
                };
            }
            if i > 0 && total + row > max_bytes {
                return (i, total);
            }
            total += row;
        }
        (self.tessera_ids.len(), total)
    }

    /// The batch of the leading rows that fit in `max_bytes`, how many that is, and their bytes.
    pub(super) fn into_batch(mut self, max_bytes: usize) -> Result<(RecordBatch, usize, usize)> {
        let (kept, bytes) = self.fit(max_bytes);
        self.tessera_ids.truncate(kept);
        let mut fields: Vec<Field> = vec![Field::new("tessera_id", DataType::UInt64, false)];
        let mut arrays: Vec<ArrayRef> = vec![Arc::new(UInt64Array::from(self.tessera_ids))];
        for (name, values) in self.named {
            let array = match values {
                Values::Scalar { ty, mut values } => {
                    values.truncate(kept);
                    scalar_array(&name, ty, values)?
                }
                Values::Category {
                    mut dictionary,
                    mut keys,
                } => {
                    keys.truncate(kept);
                    let carried = keys.iter().flatten().max().map_or(0, |&at| at as usize + 1);
                    dictionary.truncate(carried);
                    category_array(dictionary, keys)?
                }
            };
            fields.push(Field::new(name, array.data_type().clone(), true));
            arrays.push(array);
        }
        for values in self.system {
            match values {
                SystemValues::Position(mut xy) => {
                    xy.truncate(kept);
                    for (name, axis) in [("tessera:x", 0), ("tessera:y", 1)] {
                        fields.push(Field::new(name, DataType::Float64, false));
                        arrays.push(Arc::new(Float64Array::from_iter_values(
                            xy.iter().map(|p| if axis == 0 { p.0 } else { p.1 }),
                        )));
                    }
                }
                SystemValues::ExternalId(mut ids) => {
                    ids.truncate(kept);
                    fields.push(Field::new("tessera:external_id", DataType::Binary, true));
                    arrays.push(Arc::new(BinaryArray::from_iter(ids)));
                }
                SystemValues::Labels(mut labels) => {
                    labels.truncate(kept);
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
        if let Some(mut matched) = self.matched {
            matched.truncate(kept);
            fields.push(Field::new("tessera:matched", DataType::Boolean, false));
            arrays.push(Arc::new(BooleanArray::from(matched)));
        }
        let batch = RecordBatch::try_new(Arc::new(Schema::new(fields)), arrays)
            .map_err(|e| EngineError::Malformed(format!("a page did not assemble: {e}")))?;
        Ok((batch, kept, bytes))
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
