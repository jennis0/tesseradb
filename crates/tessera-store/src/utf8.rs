//! UTF-8 columns at either offset width, read through one route.
//!
//! Arrow spells a string column two ways: `Utf8`, whose offsets are 32-bit, and `LargeUtf8`,
//! whose offsets are 64-bit. The bytes are the same and the value a row carries is the same. The
//! width is the writer's choice — pandas 3 writes `LargeUtf8` for every string column it puts in
//! a Parquet file — so a reader that took one and refused the other would refuse a corpus for how
//! its producer happened to lay it out.
//!
//! A reader that wants a string column takes it through [`Utf8Column`], which holds a reference to
//! whichever array the file or batch carries and answers `value` from both. Nothing is converted:
//! the array is read where it lies. [`Utf8Values`] is the same choice held by value,
//! for a decoded batch column that outlives the borrow of its batch.
//!
//! The schema-only half of the same rule is [`is_utf8`], which is what `tessera check` asks of a
//! field it cannot downcast because it has no array.

use arrow::array::{Array, ArrayRef, LargeStringArray, StringArray};
use arrow::datatypes::DataType;

/// Whether a column of this type carries strings.
pub fn is_utf8(found: &DataType) -> bool {
    matches!(found, DataType::Utf8 | DataType::LargeUtf8)
}

/// A borrowed string column, at whichever offset width its file used.
#[derive(Clone, Copy)]
pub enum Utf8Column<'a> {
    /// 32-bit offsets.
    Small(&'a StringArray),
    /// 64-bit offsets.
    Large(&'a LargeStringArray),
}

impl<'a> Utf8Column<'a> {
    /// The column, or `None` where the array is not a string one.
    pub fn new(array: &'a dyn Array) -> Option<Self> {
        let any = array.as_any();
        if let Some(values) = any.downcast_ref::<StringArray>() {
            return Some(Utf8Column::Small(values));
        }
        any.downcast_ref::<LargeStringArray>()
            .map(Utf8Column::Large)
    }

    pub fn len(&self) -> usize {
        match self {
            Utf8Column::Small(values) => values.len(),
            Utf8Column::Large(values) => values.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn is_null(&self, row: usize) -> bool {
        match self {
            Utf8Column::Small(values) => values.is_null(row),
            Utf8Column::Large(values) => values.is_null(row),
        }
    }

    pub fn value(&self, row: usize) -> &'a str {
        match self {
            Utf8Column::Small(values) => values.value(row),
            Utf8Column::Large(values) => values.value(row),
        }
    }

    /// Row `row`'s value, `None` where the row carries a null.
    pub fn at(&self, row: usize) -> Option<&'a str> {
        (!self.is_null(row)).then(|| self.value(row))
    }
}

/// An owned string column, at whichever offset width its file used.
///
/// Cloning either array clones an `Arc` of its buffers, not its bytes.
#[derive(Clone)]
pub enum Utf8Values {
    Small(StringArray),
    Large(LargeStringArray),
}

impl Utf8Values {
    /// The column, or `None` where the array is not a string one.
    pub fn new(array: &ArrayRef) -> Option<Self> {
        let any = array.as_any();
        if let Some(values) = any.downcast_ref::<StringArray>() {
            return Some(Utf8Values::Small(values.clone()));
        }
        any.downcast_ref::<LargeStringArray>()
            .map(|values| Utf8Values::Large(values.clone()))
    }

    pub fn column(&self) -> Utf8Column<'_> {
        match self {
            Utf8Values::Small(values) => Utf8Column::Small(values),
            Utf8Values::Large(values) => Utf8Column::Large(values),
        }
    }

    pub fn len(&self) -> usize {
        self.column().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn is_null(&self, row: usize) -> bool {
        self.column().is_null(row)
    }

    pub fn value(&self, row: usize) -> &str {
        self.column().value(row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One fixture written at both widths reads the same, including its nulls.
    #[test]
    fn both_widths_read_the_same() {
        let rows = [Some("alpha"), None, Some("ω")];
        let small = StringArray::from(rows.to_vec());
        let large = LargeStringArray::from(rows.to_vec());
        for column in [Utf8Column::Small(&small), Utf8Column::Large(&large)] {
            assert_eq!(column.len(), 3);
            assert_eq!(column.at(0), Some("alpha"));
            assert!(column.is_null(1));
            assert_eq!(column.at(1), None);
            assert_eq!(column.at(2), Some("ω"));
        }
    }

    #[test]
    fn a_column_of_another_type_is_not_a_string_one() {
        let ints = arrow::array::UInt32Array::from(vec![1u32, 2]);
        assert!(Utf8Column::new(&ints).is_none());
        assert!(!is_utf8(ints.data_type()));
        assert!(is_utf8(&DataType::Utf8));
        assert!(is_utf8(&DataType::LargeUtf8));
    }
}
