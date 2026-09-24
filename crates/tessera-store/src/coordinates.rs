//! A view's coordinate columns, read the same way from a points file at a build and from an ingest
//! batch at a running service.

use std::fmt;

use arrow::array::{Array, Float32Array, Float64Array};
use arrow::datatypes::DataType;

/// Why a coordinate column cannot be read.
#[derive(Debug, Clone, PartialEq)]
pub enum ColumnError {
    /// The column is of a type other than `float32` or `float64`.
    Type(DataType),
    /// Row `row` of the column is null.
    Null { row: usize },
}

impl fmt::Display for ColumnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ColumnError::Type(found) => write!(f, "is {found:?}; send float32 or float64"),
            ColumnError::Null { .. } => {
                write!(f, "is null; write a number, since every row needs both coordinates")
            }
        }
    }
}

/// A coordinate column as `f64`. `float32` is widened and `float64` kept as it is, since at a deep
/// frame narrowing would move a point into another cell.
pub fn read_coordinates(column: &dyn Array) -> Result<Vec<f64>, ColumnError> {
    let any = column.as_any();
    let values = if let Some(values) = any.downcast_ref::<Float64Array>() {
        values.values().to_vec()
    } else if let Some(values) = any.downcast_ref::<Float32Array>() {
        values.values().iter().map(|v| f64::from(*v)).collect()
    } else {
        return Err(ColumnError::Type(column.data_type().clone()));
    };
    // A null slot's value buffer holds an arbitrary number, so a null is found before any is used.
    if column.null_count() > 0 {
        let row = (0..column.len()).find(|&row| column.is_null(row));
        return Err(ColumnError::Null {
            row: row.expect("a column with a null count has a null row"),
        });
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::Int32Array;

    #[test]
    fn a_float32_column_is_widened_and_a_float64_one_kept() {
        let narrow = Float32Array::from(vec![0.1f32, -2.5]);
        assert_eq!(
            read_coordinates(&narrow),
            Ok(vec![f64::from(0.1f32), -2.5])
        );
        let wide = Float64Array::from(vec![1_000.000_02f64]);
        assert_eq!(read_coordinates(&wide), Ok(vec![1_000.000_02]));
    }

    #[test]
    fn any_other_type_is_refused() {
        let ints = Int32Array::from(vec![1]);
        assert_eq!(read_coordinates(&ints), Err(ColumnError::Type(DataType::Int32)));
    }

    #[test]
    fn a_null_is_refused_naming_its_row() {
        let column = Float64Array::from(vec![Some(1.0), Some(2.0), None, None]);
        assert_eq!(read_coordinates(&column), Err(ColumnError::Null { row: 2 }));
        let column = Float32Array::from(vec![None, Some(1.0f32)]);
        assert_eq!(read_coordinates(&column), Err(ColumnError::Null { row: 0 }));
    }
}
