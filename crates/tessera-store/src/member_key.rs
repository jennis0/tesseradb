//! A member key column: which Arrow types may carry a key, and how a cell is read as one. A build
//! reads a member table's key column here and the running service reads a batch's layer column
//! here, so one cell names one artifact at either.

use arrow::array::{
    Array, Int16Array, Int32Array, Int64Array, Int8Array, LargeStringArray, StringArray,
    StringViewArray, UInt16Array, UInt32Array, UInt64Array, UInt8Array,
};
use arrow::datatypes::DataType;

/// The types [`KeyColumn::new`] takes, for a refusal to name.
pub const KEY_TYPES: &str = "utf8, large_utf8, utf8_view or an integer";

/// A key column at the type its producer wrote. An integer key is its decimal spelling, so `3`
/// and `"3"` name one artifact.
#[derive(Clone, Copy)]
pub enum KeyColumn<'a> {
    Utf8(&'a StringArray),
    LargeUtf8(&'a LargeStringArray),
    Utf8View(&'a StringViewArray),
    I8(&'a Int8Array),
    I16(&'a Int16Array),
    I32(&'a Int32Array),
    I64(&'a Int64Array),
    U8(&'a UInt8Array),
    U16(&'a UInt16Array),
    U32(&'a UInt32Array),
    U64(&'a UInt64Array),
}

/// What one member row's key says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyRead<'a> {
    /// In no artifact: a null cell, or the integer [`tessera_types::layer::NOISE_KEY`].
    Unclustered,
    Named(&'a str),
    Numbered(i128),
}

impl<'a> KeyColumn<'a> {
    /// The column, or `None` where its type is not one of [`KEY_TYPES`].
    pub fn new(array: &'a dyn Array) -> Option<Self> {
        let any = array.as_any();
        Some(match array.data_type() {
            DataType::Utf8 => KeyColumn::Utf8(any.downcast_ref()?),
            DataType::LargeUtf8 => KeyColumn::LargeUtf8(any.downcast_ref()?),
            DataType::Utf8View => KeyColumn::Utf8View(any.downcast_ref()?),
            DataType::Int8 => KeyColumn::I8(any.downcast_ref()?),
            DataType::Int16 => KeyColumn::I16(any.downcast_ref()?),
            DataType::Int32 => KeyColumn::I32(any.downcast_ref()?),
            DataType::Int64 => KeyColumn::I64(any.downcast_ref()?),
            DataType::UInt8 => KeyColumn::U8(any.downcast_ref()?),
            DataType::UInt16 => KeyColumn::U16(any.downcast_ref()?),
            DataType::UInt32 => KeyColumn::U32(any.downcast_ref()?),
            DataType::UInt64 => KeyColumn::U64(any.downcast_ref()?),
            _ => return None,
        })
    }

    fn array(&self) -> &dyn Array {
        match self {
            KeyColumn::Utf8(a) => *a,
            KeyColumn::LargeUtf8(a) => *a,
            KeyColumn::Utf8View(a) => *a,
            KeyColumn::I8(a) => *a,
            KeyColumn::I16(a) => *a,
            KeyColumn::I32(a) => *a,
            KeyColumn::I64(a) => *a,
            KeyColumn::U8(a) => *a,
            KeyColumn::U16(a) => *a,
            KeyColumn::U32(a) => *a,
            KeyColumn::U64(a) => *a,
        }
    }

    pub fn is_null(&self, row: usize) -> bool {
        self.array().is_null(row)
    }

    /// The text at `row` of a string column, or the integer at `row` of an integer one. The row
    /// is not null.
    fn value(&self, row: usize) -> Result<&'a str, i128> {
        match self {
            KeyColumn::Utf8(a) => Ok(a.value(row)),
            KeyColumn::LargeUtf8(a) => Ok(a.value(row)),
            KeyColumn::Utf8View(a) => Ok(a.value(row)),
            KeyColumn::I8(a) => Err(a.value(row) as i128),
            KeyColumn::I16(a) => Err(a.value(row) as i128),
            KeyColumn::I32(a) => Err(a.value(row) as i128),
            KeyColumn::I64(a) => Err(a.value(row) as i128),
            KeyColumn::U8(a) => Err(a.value(row) as i128),
            KeyColumn::U16(a) => Err(a.value(row) as i128),
            KeyColumn::U32(a) => Err(a.value(row) as i128),
            KeyColumn::U64(a) => Err(a.value(row) as i128),
        }
    }

    /// The key at `row` as written, `None` where the cell is null. An artifact's own key: `-1`
    /// here is a name.
    pub fn key_at(&self, row: usize) -> Option<String> {
        if self.is_null(row) {
            return None;
        }
        Some(match self.value(row) {
            Ok(text) => text.to_string(),
            Err(integer) => integer.to_string(),
        })
    }

    /// What a member row's key says, without allocating.
    pub fn read_at(&self, row: usize) -> KeyRead<'a> {
        if self.is_null(row) {
            return KeyRead::Unclustered;
        }
        match self.value(row) {
            Ok(text) => KeyRead::Named(text),
            Err(tessera_types::layer::NOISE_KEY) => KeyRead::Unclustered,
            Err(integer) => KeyRead::Numbered(integer),
        }
    }

    /// The key a member cell names, or `None` where the point is in no artifact.
    pub fn member_key_at(&self, row: usize) -> Option<String> {
        match self.read_at(row) {
            KeyRead::Unclustered => None,
            KeyRead::Named(text) => Some(text.to_string()),
            KeyRead::Numbered(integer) => tessera_types::layer::integer_key(integer),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{Float64Array, ListArray};

    #[test]
    fn every_string_type_reads_the_same_keys() {
        let keys = vec![Some("a"), None, Some("-1")];
        let small = StringArray::from(keys.clone());
        let large = LargeStringArray::from(keys.clone());
        let view = StringViewArray::from(keys);
        for column in [
            KeyColumn::new(&small).unwrap(),
            KeyColumn::new(&large).unwrap(),
            KeyColumn::new(&view).unwrap(),
        ] {
            assert_eq!(column.read_at(0), KeyRead::Named("a"));
            assert_eq!(column.read_at(1), KeyRead::Unclustered);
            assert_eq!(column.member_key_at(2).as_deref(), Some("-1"));
            assert_eq!(column.key_at(1), None);
        }
    }

    #[test]
    fn an_integer_is_its_decimal_spelling_and_minus_one_is_no_artifact() {
        let signed = Int64Array::from(vec![Some(3), Some(-1), None]);
        let column = KeyColumn::new(&signed).unwrap();
        assert_eq!(column.member_key_at(0).as_deref(), Some("3"));
        assert_eq!(column.member_key_at(1), None);
        assert_eq!(column.key_at(1).as_deref(), Some("-1"));
        assert_eq!(column.member_key_at(2), None);

        let unsigned = UInt8Array::from(vec![3u8]);
        assert_eq!(
            KeyColumn::new(&unsigned).unwrap().member_key_at(0).as_deref(),
            Some("3")
        );
    }

    #[test]
    fn a_float_or_a_list_carries_no_key() {
        assert!(KeyColumn::new(&Float64Array::from(vec![1.0])).is_none());
        let list = ListArray::new_null(
            std::sync::Arc::new(arrow::datatypes::Field::new("item", DataType::Utf8, true)),
            1,
        );
        assert!(KeyColumn::new(&list).is_none());
    }
}
