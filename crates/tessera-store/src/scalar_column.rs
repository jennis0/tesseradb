//! A column of an Arrow batch read as a declared scalar type: which Arrow types may carry each
//! declared type, and what each row's value becomes. A build reads a points file's attribute
//! columns here and the running service reads an ingest or values batch's columns here, so one
//! column loads the same way at either.
//!
//! Every integer type carries every integer declaration, and a row whose value does not fit the
//! declaration is refused rather than truncated. Either float width carries either float
//! declaration, and `f64` into `f32` rounds to the nearest `f32`. A string at either offset width
//! carries `utf8`, `keyword` and `text`. A timestamp carries an integer declaration only in
//! microseconds, because nothing records a unit and two units under one declaration would store
//! incomparable numbers.
//!
//! A category's column carries value keys, which only its vocabulary can resolve, so a category is
//! not read here.

use arrow::array::{
    Array, ArrayRef, BooleanArray, Float32Array, Float64Array, Int16Array, Int32Array, Int64Array,
    Int8Array, TimestampMicrosecondArray, UInt16Array, UInt32Array, UInt64Array, UInt8Array,
};
use arrow::buffer::NullBuffer;
use arrow::datatypes::{DataType, TimeUnit};
use tessera_spatial::tiler::{ScalarType, ScalarValue};

use crate::utf8::{is_utf8, Utf8Values};

/// Whether a column of Arrow type `found` carries a value declared `ty`. An integer's fit is
/// checked per row by [`ScalarColumn::value`], so for an integer declaration this answers only
/// whether the column holds integers.
pub fn carries(ty: ScalarType, found: &DataType) -> bool {
    match ty {
        ScalarType::Bool => matches!(found, DataType::Boolean),
        ScalarType::F32 | ScalarType::F64 => {
            matches!(found, DataType::Float32 | DataType::Float64)
        }
        ScalarType::Utf8 | ScalarType::Keyword | ScalarType::Text => is_utf8(found),
        ScalarType::U8
        | ScalarType::U16
        | ScalarType::U32
        | ScalarType::U64
        | ScalarType::I8
        | ScalarType::I16
        | ScalarType::I32
        | ScalarType::I64
        | ScalarType::TimestampUs => matches!(
            found,
            DataType::UInt8
                | DataType::UInt16
                | DataType::UInt32
                | DataType::UInt64
                | DataType::Int8
                | DataType::Int16
                | DataType::Int32
                | DataType::Int64
                | DataType::Timestamp(TimeUnit::Microsecond, _)
        ),
    }
}

/// One batch's worth of a column read as a declared type, decoded once so each row is an index.
pub struct ScalarColumn {
    ty: ScalarType,
    nulls: Option<NullBuffer>,
    values: Values,
}

enum Values {
    Bool(BooleanArray),
    /// Every integer column widened to `i64`, and narrowed back to the declared width per row.
    Ints(Vec<i64>),
    /// A `u64` column under a `u64` declaration, kept unwidened: widening a `u64` above
    /// `i64::MAX` would make it negative, and the range check would then refuse it.
    U64(Vec<u64>),
    F32(Vec<f32>),
    F64(Vec<f64>),
    Text(Utf8Values),
}

/// A row whose integer does not fit its declared type, which accepts `min..=max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutOfRange {
    pub value: i64,
    pub min: i64,
    pub max: i64,
}

impl ScalarColumn {
    /// The column read as `ty`, or `None` where [`carries`] says it cannot be.
    pub fn new(column: &ArrayRef, ty: ScalarType) -> Option<Self> {
        let any = column.as_any();
        let values = match ty {
            ScalarType::Bool => Values::Bool(any.downcast_ref::<BooleanArray>()?.clone()),
            ScalarType::F32 => Values::F32(if let Some(a) = any.downcast_ref::<Float32Array>() {
                a.values().to_vec()
            } else {
                let a = any.downcast_ref::<Float64Array>()?;
                a.values().iter().map(|v| *v as f32).collect()
            }),
            ScalarType::F64 => Values::F64(if let Some(a) = any.downcast_ref::<Float64Array>() {
                a.values().to_vec()
            } else {
                let a = any.downcast_ref::<Float32Array>()?;
                a.values().iter().map(|v| f64::from(*v)).collect()
            }),
            ScalarType::Utf8 | ScalarType::Keyword | ScalarType::Text => {
                Values::Text(Utf8Values::new(column)?)
            }
            ScalarType::U64 if any.is::<UInt64Array>() => Values::U64(
                any.downcast_ref::<UInt64Array>()
                    .expect("checked by is::<>")
                    .values()
                    .to_vec(),
            ),
            ScalarType::U8
            | ScalarType::U16
            | ScalarType::U32
            | ScalarType::U64
            | ScalarType::I8
            | ScalarType::I16
            | ScalarType::I32
            | ScalarType::I64
            | ScalarType::TimestampUs => Values::Ints(integers(column.as_ref())?),
        };
        Some(ScalarColumn {
            ty,
            nulls: column.nulls().cloned(),
            values,
        })
    }

    /// Row `row`'s value at the declared type: [`ScalarValue::Null`] where the row carries a null,
    /// and [`OutOfRange`] where its integer does not fit.
    pub fn value(&self, row: usize) -> Result<ScalarValue, OutOfRange> {
        // A null slot's value buffer holds an arbitrary number, usually 0, so it is never read.
        if self.nulls.as_ref().is_some_and(|n| n.is_null(row)) {
            return Ok(ScalarValue::Null);
        }
        Ok(match &self.values {
            Values::Bool(values) => ScalarValue::Bool(values.value(row)),
            Values::U64(values) => ScalarValue::U64(values[row]),
            Values::F32(values) => ScalarValue::F32(values[row]),
            Values::F64(values) => ScalarValue::F64(values[row]),
            Values::Text(values) => ScalarValue::Utf8(values.value(row).to_string()),
            Values::Ints(values) => {
                let value = values[row];
                let fit = |min: i64, max: i64| {
                    if (min..=max).contains(&value) {
                        Ok(value)
                    } else {
                        Err(OutOfRange { value, min, max })
                    }
                };
                match self.ty {
                    ScalarType::U8 => ScalarValue::U8(fit(0, u8::MAX.into())? as u8),
                    ScalarType::U16 => ScalarValue::U16(fit(0, u16::MAX.into())? as u16),
                    ScalarType::U32 => ScalarValue::U32(fit(0, u32::MAX.into())? as u32),
                    ScalarType::U64 => ScalarValue::U64(fit(0, i64::MAX)? as u64),
                    ScalarType::I8 => ScalarValue::I8(fit(i8::MIN.into(), i8::MAX.into())? as i8),
                    ScalarType::I16 => {
                        ScalarValue::I16(fit(i16::MIN.into(), i16::MAX.into())? as i16)
                    }
                    ScalarType::I32 => {
                        ScalarValue::I32(fit(i32::MIN.into(), i32::MAX.into())? as i32)
                    }
                    ScalarType::I64 => ScalarValue::I64(value),
                    ScalarType::TimestampUs => ScalarValue::TimestampUs(value),
                    ScalarType::Bool
                    | ScalarType::F32
                    | ScalarType::F64
                    | ScalarType::Utf8
                    | ScalarType::Keyword
                    | ScalarType::Text => {
                        unreachable!("`new` reads only an integer declaration as integers")
                    }
                }
            }
        })
    }
}

/// Any integer column, or a microsecond timestamp, as `i64`: one conversion per batch. A `u64`
/// above `i64::MAX` wraps to a negative number. `None` for any other type.
pub fn integers(column: &dyn Array) -> Option<Vec<i64>> {
    let any = column.as_any();
    macro_rules! widen {
        ($($dt:pat => $arr:ident),* $(,)?) => {
            match column.data_type() {
                $($dt => any
                    .downcast_ref::<$arr>()?
                    .values()
                    .iter()
                    .map(|v| *v as i64)
                    .collect(),)*
                DataType::Timestamp(TimeUnit::Microsecond, _) => {
                    any.downcast_ref::<TimestampMicrosecondArray>()?.values().to_vec()
                }
                _ => return None,
            }
        };
    }
    Some(widen! {
        DataType::UInt8 => UInt8Array,
        DataType::UInt16 => UInt16Array,
        DataType::UInt32 => UInt32Array,
        DataType::UInt64 => UInt64Array,
        DataType::Int8 => Int8Array,
        DataType::Int16 => Int16Array,
        DataType::Int32 => Int32Array,
        DataType::Int64 => Int64Array,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{LargeStringArray, StringArray, TimestampMillisecondArray};
    use std::sync::Arc;

    const DECLARED: [ScalarType; 15] = [
        ScalarType::Bool,
        ScalarType::U8,
        ScalarType::U16,
        ScalarType::U32,
        ScalarType::U64,
        ScalarType::I8,
        ScalarType::I16,
        ScalarType::I32,
        ScalarType::I64,
        ScalarType::F32,
        ScalarType::F64,
        ScalarType::TimestampUs,
        ScalarType::Utf8,
        ScalarType::Keyword,
        ScalarType::Text,
    ];

    #[test]
    fn the_schema_check_and_the_decode_agree() {
        let columns: Vec<ArrayRef> = vec![
            Arc::new(BooleanArray::from(vec![true])),
            Arc::new(UInt8Array::from(vec![1u8])),
            Arc::new(UInt16Array::from(vec![1u16])),
            Arc::new(UInt32Array::from(vec![1u32])),
            Arc::new(UInt64Array::from(vec![1u64])),
            Arc::new(Int8Array::from(vec![1i8])),
            Arc::new(Int16Array::from(vec![1i16])),
            Arc::new(Int32Array::from(vec![1i32])),
            Arc::new(Int64Array::from(vec![1i64])),
            Arc::new(Float32Array::from(vec![1.0f32])),
            Arc::new(Float64Array::from(vec![1.0f64])),
            Arc::new(StringArray::from(vec!["k"])),
            Arc::new(LargeStringArray::from(vec!["k"])),
            Arc::new(TimestampMicrosecondArray::from(vec![1i64])),
            Arc::new(TimestampMillisecondArray::from(vec![1i64])),
        ];
        for ty in DECLARED {
            for column in &columns {
                assert_eq!(
                    carries(ty, column.data_type()),
                    ScalarColumn::new(column, ty).is_some(),
                    "declared {ty:?} against {:?}",
                    column.data_type()
                );
            }
        }
    }

    #[test]
    fn an_integer_is_narrowed_to_its_declaration_or_refused() {
        let column: ArrayRef = Arc::new(Int64Array::from(vec![255, 256, -1]));
        let read = ScalarColumn::new(&column, ScalarType::U8).unwrap();
        assert_eq!(read.value(0), Ok(ScalarValue::U8(255)));
        assert_eq!(
            read.value(1),
            Err(OutOfRange {
                value: 256,
                min: 0,
                max: 255
            })
        );
        assert!(read.value(2).is_err());
    }

    #[test]
    fn a_u64_column_keeps_its_full_range_under_a_u64_declaration() {
        let column: ArrayRef = Arc::new(UInt64Array::from(vec![u64::MAX]));
        let read = ScalarColumn::new(&column, ScalarType::U64).unwrap();
        assert_eq!(read.value(0), Ok(ScalarValue::U64(u64::MAX)));
    }

    #[test]
    fn a_null_is_absent_whatever_its_slot_holds() {
        let column: ArrayRef = Arc::new(Int64Array::from(vec![None, Some(7)]));
        let read = ScalarColumn::new(&column, ScalarType::U8).unwrap();
        assert_eq!(read.value(0), Ok(ScalarValue::Null));
        assert_eq!(read.value(1), Ok(ScalarValue::U8(7)));
        let column: ArrayRef = Arc::new(LargeStringArray::from(vec![None, Some("")]));
        let read = ScalarColumn::new(&column, ScalarType::Utf8).unwrap();
        assert_eq!(read.value(0), Ok(ScalarValue::Null));
        assert_eq!(read.value(1), Ok(ScalarValue::Utf8(String::new())));
    }

    #[test]
    fn either_float_width_carries_either_float_declaration() {
        let wide: ArrayRef = Arc::new(Float64Array::from(vec![0.1f64]));
        let narrow: ArrayRef = Arc::new(Float32Array::from(vec![0.5f32]));
        assert_eq!(
            ScalarColumn::new(&wide, ScalarType::F32).unwrap().value(0),
            Ok(ScalarValue::F32(0.1f32))
        );
        assert_eq!(
            ScalarColumn::new(&narrow, ScalarType::F64).unwrap().value(0),
            Ok(ScalarValue::F64(0.5))
        );
    }
}
