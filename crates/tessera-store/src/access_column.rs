//! The one reader of an access column, for a points file at the build and an ingest batch on a
//! running service. Each value is one whole label, read through
//! [`tessera_types::label::label_value`].

use std::collections::HashMap;

use arrow::array::{Array, ArrayRef, AsArray, GenericListArray, LargeListArray, ListArray};
use arrow::array::OffsetSizeTrait;
use arrow::datatypes::DataType;
use tessera_types::label::label_value;

use crate::utf8::{is_utf8, Utf8Column};

/// Whether a column of this type is one [`read_access_column`] reads.
pub fn is_access_type(found: &DataType) -> bool {
    match found {
        DataType::List(inner) | DataType::LargeList(inner) => is_utf8(inner.data_type()),
        DataType::Dictionary(_, values) => is_utf8(values),
        other => is_utf8(other),
    }
}

/// One batch's access column: its distinct labels, and each row's indices into them.
pub struct AccessBatch<'a> {
    distinct: Vec<&'a str>,
    /// Row `i`'s labels are `indices[bounds[i]..bounds[i + 1]]`.
    bounds: Vec<usize>,
    indices: Vec<u32>,
}

impl<'a> AccessBatch<'a> {
    fn with_rows(rows: usize) -> AccessBatch<'a> {
        let mut batch = AccessBatch {
            distinct: Vec::new(),
            bounds: Vec::with_capacity(rows + 1),
            indices: Vec::with_capacity(rows),
        };
        batch.bounds.push(0);
        batch
    }

    /// The batch's distinct labels, trimmed, in first-seen order.
    pub fn distinct(&self) -> &[&'a str] {
        &self.distinct
    }

    /// Row `row`'s labels, as indices into [`Self::distinct`]. Empty where the row has no label.
    pub fn row(&self, row: usize) -> &[u32] {
        &self.indices[self.bounds[row]..self.bounds[row + 1]]
    }

    /// Row `row`'s labels, trimmed.
    pub fn labels(&self, row: usize) -> impl Iterator<Item = &'a str> + '_ {
        self.row(row).iter().map(|&i| self.distinct[i as usize])
    }

    /// Add one value to the row being built. A value empty after trimming is dropped.
    fn push(&mut self, seen: &mut HashMap<&'a str, u32>, value: &'a str) {
        if let Some(at) = self.intern(seen, value) {
            self.indices.push(at);
        }
    }

    /// Where a value's trimmed label sits in [`Self::distinct`], adding it there if new.
    fn intern(&mut self, seen: &mut HashMap<&'a str, u32>, value: &'a str) -> Option<u32> {
        let label = label_value(value)?;
        let next = self.distinct.len() as u32;
        Some(*seen.entry(label).or_insert_with(|| {
            self.distinct.push(label);
            next
        }))
    }

    fn end_row(&mut self) {
        self.bounds.push(self.indices.len());
    }
}

/// Decode one batch of an access column named `name`: a string, a list or large list of strings,
/// or a dictionary of strings under any integer key, at either string width. A null, a value empty
/// after trimming and an empty list are no label. Any other type is refused, naming it.
pub fn read_access_column<'a>(column: &'a ArrayRef, name: &str) -> Result<AccessBatch<'a>, String> {
    let rows = column.len();
    let mut batch = AccessBatch::with_rows(rows);
    let mut seen: HashMap<&str, u32> = HashMap::new();

    // A dictionary's keys are the indices already: each distinct value is trimmed once and
    // renumbered, since a page may carry values no row uses and a trim may make two of them one.
    if let Some(dictionary) = column.as_any_dictionary_opt() {
        let values = Utf8Column::new(dictionary.values().as_ref()).ok_or_else(|| {
            format!(
                "the access column '{name}' is a dictionary of {:?}; send a dictionary of strings",
                dictionary.values().data_type()
            )
        })?;
        let mapped: Vec<Option<u32>> = (0..values.len())
            .map(|key| values.at(key).and_then(|value| batch.intern(&mut seen, value)))
            .collect();
        let keys = dictionary.keys();
        let positions = if values.is_empty() {
            Vec::new()
        } else {
            dictionary.normalized_keys()
        };
        for row in 0..rows {
            if !keys.is_null(row) {
                if let Some(&Some(at)) = positions.get(row).map(|&key| &mapped[key]) {
                    batch.indices.push(at);
                }
            }
            batch.end_row();
        }
        return Ok(batch);
    }
    if let Some(values) = Utf8Column::new(column.as_ref()) {
        for row in 0..rows {
            if let Some(value) = values.at(row) {
                batch.push(&mut seen, value);
            }
            batch.end_row();
        }
        return Ok(batch);
    }
    fn lists<'v, O: OffsetSizeTrait>(
        name: &str,
        list: &'v GenericListArray<O>,
        batch: &mut AccessBatch<'v>,
        seen: &mut HashMap<&'v str, u32>,
    ) -> Result<(), String> {
        let values = list.values();
        let strings = Utf8Column::new(values.as_ref()).ok_or_else(|| {
            format!(
                "the access column '{name}' is a list of {:?}; send a list of strings",
                values.data_type()
            )
        })?;
        let offsets = list.value_offsets();
        for row in 0..list.len() {
            if !list.is_null(row) {
                for j in offsets[row].as_usize()..offsets[row + 1].as_usize() {
                    if let Some(value) = strings.at(j) {
                        batch.push(seen, value);
                    }
                }
            }
            batch.end_row();
        }
        Ok(())
    }
    if let Some(list) = column.as_any().downcast_ref::<ListArray>() {
        lists(name, list, &mut batch, &mut seen)?;
        return Ok(batch);
    }
    if let Some(list) = column.as_any().downcast_ref::<LargeListArray>() {
        lists(name, list, &mut batch, &mut seen)?;
        return Ok(batch);
    }
    Err(format!(
        "the access column '{name}' has type {:?}; send a string, a list of strings or a \
         dictionary of strings, one label per string",
        column.data_type()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{LargeStringArray, ListBuilder, StringArray, StringBuilder};
    use arrow::array::{DictionaryArray, GenericListBuilder, LargeStringBuilder};
    use arrow::datatypes::{Int32Type, Int8Type};
    use std::sync::Arc;

    fn rows(batch: &AccessBatch<'_>, n: usize) -> Vec<Vec<String>> {
        (0..n)
            .map(|row| batch.labels(row).map(str::to_string).collect())
            .collect()
    }

    fn want(rows: &[&[&str]]) -> Vec<Vec<String>> {
        rows.iter()
            .map(|row| row.iter().map(|l| l.to_string()).collect())
            .collect()
    }

    #[test]
    fn every_encoding_reads_the_same_trimmed_labels() {
        let scalar: ArrayRef = Arc::new(StringArray::from(vec![Some(" red "), Some(" "), None]));
        let large: ArrayRef =
            Arc::new(LargeStringArray::from(vec![Some(" red "), Some(""), None]));
        let dictionary: ArrayRef = Arc::new(
            vec![Some(" red "), Some("  "), None]
                .into_iter()
                .collect::<DictionaryArray<Int32Type>>(),
        );
        let narrow: ArrayRef = Arc::new(
            vec![Some(" red "), Some("  "), None]
                .into_iter()
                .collect::<DictionaryArray<Int8Type>>(),
        );
        for column in [scalar, large, dictionary, narrow] {
            let batch = read_access_column(&column, "access").unwrap();
            assert_eq!(rows(&batch, 3), want(&[&["red"], &[], &[]]), "{:?}", column.data_type());
        }

        let mut list = ListBuilder::new(StringBuilder::new());
        list.values().append_value(" red ");
        list.values().append_value("");
        list.values().append_null();
        list.values().append_value("red");
        list.append(true);
        list.values().append_value(" ");
        list.append(true);
        list.append(false);
        let list: ArrayRef = Arc::new(list.finish());
        let mut large = GenericListBuilder::<i64, _>::new(LargeStringBuilder::new());
        large.values().append_value(" red ");
        large.values().append_value("");
        large.values().append_null();
        large.values().append_value("red");
        large.append(true);
        large.values().append_value(" ");
        large.append(true);
        large.append(false);
        let large: ArrayRef = Arc::new(large.finish());
        for column in [list, large] {
            let batch = read_access_column(&column, "access").unwrap();
            assert_eq!(rows(&batch, 3), want(&[&["red", "red"], &[], &[]]));
            assert_eq!(batch.distinct(), ["red"]);
        }
    }

    #[test]
    fn a_column_of_another_type_is_refused() {
        let column: ArrayRef = Arc::new(arrow::array::Int32Array::from(vec![1]));
        assert!(read_access_column(&column, "access").is_err());
        assert!(!is_access_type(&DataType::Int32));
        assert!(is_access_type(&DataType::LargeUtf8));
    }
}
