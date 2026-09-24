//! A member key column: which Arrow types may carry a key, how a cell is read as one, and how a
//! list of keys is read against the layer's hierarchy. A build reads a member table's key column
//! here and the running service reads a batch's layer column here, so one cell names one artifact
//! at either.

use std::fmt;
use std::ops::Range;

use arrow::array::{
    Array, FixedSizeListArray, Int16Array, Int32Array, Int64Array, Int8Array, LargeListArray,
    LargeStringArray, ListArray, StringArray, StringViewArray, UInt16Array, UInt32Array,
    UInt64Array, UInt8Array,
};
use arrow::datatypes::DataType;
use tessera_types::layer::ListMeaning;

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

/// The column beside a scalar member key that places it at a level: `uint32`, a null being level 0.
/// A list's positions carry its levels, so it reads no such column.
pub const LEVEL: &str = "level";

/// A [`LEVEL`] column, or `None` where it is not `uint32`.
pub fn read_levels(array: &dyn Array) -> Option<&UInt32Array> {
    array.as_any().downcast_ref()
}

/// A member key column's cells: one key per row, or a list of keys per row at any of Arrow's list
/// types.
#[derive(Clone, Copy)]
pub enum KeyCells<'a> {
    Scalar,
    List(&'a ListArray),
    LargeList(&'a LargeListArray),
    FixedSizeList(&'a FixedSizeListArray),
}

impl<'a> KeyCells<'a> {
    /// The cells of `array`, and the array its keys are read from: a list's elements, or the
    /// column itself.
    pub fn new(array: &'a dyn Array) -> (Self, &'a dyn Array) {
        let any = array.as_any();
        if let Some(list) = any.downcast_ref::<ListArray>() {
            return (KeyCells::List(list), list.values().as_ref());
        }
        if let Some(list) = any.downcast_ref::<LargeListArray>() {
            return (KeyCells::LargeList(list), list.values().as_ref());
        }
        if let Some(list) = any.downcast_ref::<FixedSizeListArray>() {
            return (KeyCells::FixedSizeList(list), list.values().as_ref());
        }
        (KeyCells::Scalar, array)
    }

    pub fn is_list(&self) -> bool {
        !matches!(self, KeyCells::Scalar)
    }

    /// Where row `row`'s keys sit in the key array; `None` for a null list.
    pub fn range(&self, row: usize) -> Option<Range<usize>> {
        match self {
            KeyCells::Scalar => Some(row..row + 1),
            KeyCells::List(list) => {
                let offsets = list.value_offsets();
                (!list.is_null(row)).then(|| offsets[row] as usize..offsets[row + 1] as usize)
            }
            KeyCells::LargeList(list) => {
                let offsets = list.value_offsets();
                (!list.is_null(row)).then(|| offsets[row] as usize..offsets[row + 1] as usize)
            }
            KeyCells::FixedSizeList(list) => (!list.is_null(row)).then(|| {
                let start = list.value_offset(row) as usize;
                start..start + list.value_length() as usize
            }),
        }
    }
}

/// A layer's member key column, read against what its hierarchy says a list's positions mean.
pub struct MemberColumn<'a> {
    pub cells: KeyCells<'a>,
    pub keys: KeyColumn<'a>,
    pub meaning: ListMeaning,
}

/// Why a column cannot carry a layer's member keys.
#[derive(Debug, Clone, PartialEq)]
pub enum MemberColumnError {
    /// The keys, the column's own or its list's elements, are of a type no key is.
    KeyType(DataType),
    /// A fixed-size list on a nested layer, whose lineages are as long as each point's branch.
    FixedLineage { size: usize },
    /// A fixed-size list whose size is not the layer's number of levels.
    FixedArity { size: usize, levels: usize },
}

impl fmt::Display for MemberColumnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MemberColumnError::KeyType(found) => {
                write!(f, "carries {found:?}; write its keys as {KEY_TYPES}")
            }
            MemberColumnError::FixedLineage { size } => write!(
                f,
                "is a fixed-size list of {size} and the layer is nested; write each point's \
                 lineage as a variable-length list"
            ),
            MemberColumnError::FixedArity { size, levels } => write!(
                f,
                "is a fixed-size list of {size} and the layer declares {levels} levels; write one \
                 entry per level"
            ),
        }
    }
}

/// A list row whose length is not the layer's number of levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WrongArity {
    pub found: usize,
    pub levels: usize,
}

impl fmt::Display for WrongArity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "names {} artifacts and the layer declares {} levels; write one entry per level, null \
             where the point is in no artifact at that level",
            self.found, self.levels
        )
    }
}

impl<'a> MemberColumn<'a> {
    pub fn new(array: &'a dyn Array, meaning: ListMeaning) -> Result<Self, MemberColumnError> {
        let (cells, values) = KeyCells::new(array);
        if let KeyCells::FixedSizeList(list) = cells {
            let size = list.value_length() as usize;
            match meaning {
                ListMeaning::Lineage => return Err(MemberColumnError::FixedLineage { size }),
                ListMeaning::Levelled { levels, .. } if size != levels => {
                    return Err(MemberColumnError::FixedArity { size, levels })
                }
                _ => {}
            }
        }
        let keys = KeyColumn::new(values)
            .ok_or_else(|| MemberColumnError::KeyType(values.data_type().clone()))?;
        Ok(MemberColumn {
            cells,
            keys,
            meaning,
        })
    }

    /// Row `row`'s entries, as positions in [`Self::keys`], or `None` where the row is a null or
    /// empty list and names no artifact. A scalar row is one entry, whose key may itself say the
    /// point is in no artifact. A list on a levelled layer holds one entry per level.
    pub fn entries(&self, row: usize) -> Result<Option<Range<usize>>, WrongArity> {
        let Some(range) = self.cells.range(row).filter(|range| !range.is_empty()) else {
            return Ok(None);
        };
        match self.meaning.arity() {
            Some(levels) if self.cells.is_list() && range.len() != levels => Err(WrongArity {
                found: range.len(),
                levels,
            }),
            _ => Ok(Some(range)),
        }
    }

    /// The level of the entry at `position` of row `row`: a list's position, read by the layer's
    /// hierarchy, and for a scalar key the row's [`LEVEL`] where `levels` is given.
    pub fn level_at(&self, levels: Option<&UInt32Array>, row: usize, position: usize) -> u32 {
        match self.cells {
            KeyCells::Scalar => levels
                .filter(|levels| !levels.is_null(row))
                .map_or(0, |levels| levels.value(row)),
            _ => self.meaning.level_of(position),
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

    fn keys_of(column: &MemberColumn<'_>, row: usize) -> Option<Vec<Option<String>>> {
        column.entries(row).unwrap().map(|range| {
            range
                .map(|index| column.keys.member_key_at(index))
                .collect()
        })
    }

    #[test]
    fn every_list_type_reads_the_same_entries() {
        use arrow::array::{FixedSizeListBuilder, GenericListBuilder, Int64Builder};

        let mut list = GenericListBuilder::<i32, _>::new(Int64Builder::new());
        let mut large = GenericListBuilder::<i64, _>::new(Int64Builder::new());
        for row in [vec![Some(1), Some(10)], vec![Some(1), None]] {
            list.values().extend(row.clone());
            list.append(true);
            large.values().extend(row);
            large.append(true);
        }
        list.append(false);
        large.append(false);
        let mut fixed = FixedSizeListBuilder::new(Int64Builder::new(), 2);
        fixed.values().extend([Some(1), Some(10), Some(1), None, None, None]);
        fixed.append(true);
        fixed.append(true);
        fixed.append(false);
        let (list, large, fixed) = (list.finish(), large.finish(), fixed.finish());
        let meaning = ListMeaning::Levelled {
            levels: 2,
            edges: true,
        };
        for array in [&list as &dyn Array, &large, &fixed] {
            let column = MemberColumn::new(array, meaning).unwrap();
            assert!(column.cells.is_list());
            let at = |row| keys_of(&column, row);
            assert_eq!(at(0), Some(vec![Some("1".into()), Some("10".into())]));
            assert_eq!(at(1), Some(vec![Some("1".into()), None]));
            assert_eq!(at(2), None, "a null list names no artifact");
            assert_eq!(column.level_at(None, 0, 1), 1);
        }
    }

    #[test]
    fn a_levelled_list_holds_one_entry_per_level_and_an_empty_one_names_nothing() {
        use arrow::array::{GenericListBuilder, StringBuilder};

        let mut list = GenericListBuilder::<i32, _>::new(StringBuilder::new());
        list.values().append_value("a");
        list.append(true);
        list.append(true);
        let list = list.finish();
        let levelled = ListMeaning::Levelled {
            levels: 2,
            edges: false,
        };
        let column = MemberColumn::new(&list, levelled).unwrap();
        assert_eq!(
            column.entries(0),
            Err(WrongArity {
                found: 1,
                levels: 2
            })
        );
        assert_eq!(column.entries(1), Ok(None));
        let column = MemberColumn::new(&list, ListMeaning::Lineage).unwrap();
        assert_eq!(column.entries(0), Ok(Some(0..1)));
    }

    #[test]
    fn a_fixed_size_list_is_refused_on_a_nested_layer_or_at_another_arity() {
        use arrow::array::{FixedSizeListBuilder, Int64Builder};

        let mut fixed = FixedSizeListBuilder::new(Int64Builder::new(), 3);
        fixed.values().extend([Some(1), Some(2), Some(3)]);
        fixed.append(true);
        let fixed = fixed.finish();
        assert!(matches!(
            MemberColumn::new(&fixed, ListMeaning::Lineage),
            Err(MemberColumnError::FixedLineage { size: 3 })
        ));
        let levelled = ListMeaning::Levelled {
            levels: 2,
            edges: false,
        };
        assert!(matches!(
            MemberColumn::new(&fixed, levelled),
            Err(MemberColumnError::FixedArity { size: 3, levels: 2 })
        ));
        assert!(MemberColumn::new(&fixed, ListMeaning::Unordered).is_ok());
    }

    #[test]
    fn a_scalar_key_sits_at_its_rows_level_and_a_null_level_is_zero() {
        let keys = Int64Array::from(vec![Some(3), Some(-1), None]);
        let column = MemberColumn::new(&keys, ListMeaning::Unordered).unwrap();
        assert!(!column.cells.is_list());
        assert_eq!(keys_of(&column, 0), Some(vec![Some("3".into())]));
        assert_eq!(keys_of(&column, 1), Some(vec![None]));
        let levels = UInt32Array::from(vec![Some(2), None, Some(1)]);
        assert_eq!(column.level_at(Some(&levels), 0, 0), 2);
        assert_eq!(column.level_at(Some(&levels), 1, 0), 0);
        assert_eq!(column.level_at(None, 0, 0), 0);
        let floats = Float64Array::from(vec![1.0]);
        assert!(matches!(
            MemberColumn::new(&floats, ListMeaning::Unordered),
            Err(MemberColumnError::KeyType(DataType::Float64))
        ));
    }
}
