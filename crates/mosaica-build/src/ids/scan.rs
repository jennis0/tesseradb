//! The columns a file names its rows' items by, read as keys.

use std::collections::BTreeMap;
use std::ops::ControlFlow;
use std::path::Path;

use arrow::array::{Array, ArrayRef};
use arrow::datatypes::{DataType, TimeUnit};
use arrow::record_batch::RecordBatch;
use mosaica_spatial::tiler::ScalarType;
use mosaica_store::unique::{key_of_integer, UniqueKey};

use super::{CarriedField, LimitRead, TESSERA_ID_COLUMN};
use crate::config::ViewSelector;
use crate::error::{BuildError, Result};
use crate::row_groups::FileGroups;

/// One file as the rule reads it: the unique fields it carries, whether it has a `tessera_id`
/// column, the view whose rows are its rows where it holds several views', and the limit.
pub(crate) struct FileRead<'a> {
    pub path: &'a Path,
    pub groups: &'a FileGroups,
    pub carried: &'a [CarriedField],
    pub tessera: bool,
    pub select: Option<&'a ViewSelector>,
    /// How `--limit` reads the file's rows.
    pub limit: Option<LimitRead<'a>>,
}

/// One decoded batch: consecutive rows of the file from `first`.
pub(crate) struct Scanned {
    pub first: u64,
    pub len: usize,
    /// Whether each row is one of the read's rows, under the view's selection and the limit.
    /// `None` where every row is.
    pub selected: Option<Vec<bool>>,
    /// Each carried field's key at each row, in [`FileRead::carried`]'s order.
    pub keys: Vec<Vec<Option<UniqueKey>>>,
    /// Whether each row not read names no item `--limit` kept, and is reported as outside it.
    /// `None` where no row does.
    pub outside: Option<Vec<bool>>,
    /// Whether each row read is left out unless it names an item the build holds
    /// ([`LimitRead::Named`]). `None` where no row is.
    pub left_out: Option<Vec<bool>>,
    /// Whether each row carries a `tessera_id`, where the file has the column.
    pub tessera: Option<Vec<bool>>,
}

impl<'a> FileRead<'a> {
    pub(crate) fn new(
        path: &'a Path,
        groups: &'a FileGroups,
        carried: &'a [CarriedField],
        select: Option<&'a ViewSelector>,
        limit: Option<LimitRead<'a>>,
    ) -> FileRead<'a> {
        FileRead {
            path,
            groups,
            carried,
            tessera: groups.schema().column_with_name(TESSERA_ID_COLUMN).is_some(),
            select,
            limit,
        }
    }

    /// Whether every row of the file is a row of the read, whatever its values.
    pub(crate) fn takes_every_row(&self) -> bool {
        self.select.is_none() && self.limit.is_none()
    }

    /// The row groups that can hold a row of the read: where `--limit` reads a prefix, those it
    /// cannot rule out from the column's own statistics. Every group of any other file is read,
    /// and every group of a file with a `tessera_id` column, whose rows are refused.
    pub(crate) fn kept_groups(&self) -> Option<Vec<usize>> {
        let Some(LimitRead::Prefix { column, below }) = self.limit.filter(|_| !self.tessera) else {
            return None;
        };
        let index = self.groups.schema().column_with_name(column)?.0;
        Some(crate::input::prunable_row_groups(
            self.groups.metadata(),
            index,
            Some(below),
        ))
    }

    fn roots(&self) -> Result<Vec<usize>> {
        let schema = self.groups.schema();
        let mut roots: Vec<usize> = Vec::new();
        let mut index_of = |name: &str| -> Result<()> {
            let (index, _) = schema.column_with_name(name).ok_or_else(|| BuildError::Schema {
                path: self.path.to_path_buf(),
                detail: format!("missing required column '{name}'"),
            })?;
            if !roots.contains(&index) {
                roots.push(index);
            }
            Ok(())
        };
        for field in self.carried {
            index_of(&field.column)?;
        }
        if self.tessera {
            index_of(TESSERA_ID_COLUMN)?;
        }
        if let Some(select) = self.select {
            index_of(&select.column)?;
        }
        roots.sort_unstable();
        Ok(roots)
    }

    /// Every batch of the read's row groups, decoded on worker threads and handed to `visit` in
    /// no particular order.
    pub(crate) fn scan(&self, visit: impl FnMut(Scanned) -> Result<()>) -> Result<()> {
        self.scan_ordered(|| false, visit)
    }

    /// [`Self::scan`], each batch in row order on this thread for as long as `ordered` holds when
    /// a row group begins, and the row groups after that decoded on worker threads.
    pub(crate) fn scan_ordered(
        &self,
        ordered: impl Fn() -> bool,
        mut visit: impl FnMut(Scanned) -> Result<()>,
    ) -> Result<()> {
        let groups = self
            .kept_groups()
            .unwrap_or_else(|| (0..self.groups.count()).collect());
        let projection = self.groups.projection(&self.roots()?);
        let mut at = 0;
        while at < groups.len() && ordered() {
            self.groups
                .each_batch(self.path, &groups[at..=at], &projection, |first, batch| {
                    visit(self.decode(first, &batch)?)?;
                    Ok(ControlFlow::Continue(()))
                })?;
            at += 1;
        }
        if at == groups.len() {
            return Ok(());
        }
        self.groups.decode_parallel(
            self.path,
            &groups[at..],
            &projection,
            |first, batch| self.decode(first, &batch),
            |scanned| {
                visit(scanned)?;
                Ok(ControlFlow::Continue(()))
            },
        )
    }

    fn decode(&self, first: u64, batch: &RecordBatch) -> Result<Scanned> {
        let len = batch.num_rows();
        let column = |name: &str| -> Result<&ArrayRef> {
            batch.column_by_name(name).ok_or_else(|| BuildError::Schema {
                path: self.path.to_path_buf(),
                detail: format!("missing required column '{name}'"),
            })
        };
        let mut selected: Option<Vec<bool>> = match self.select {
            Some(select) => Some(crate::input::selected_rows(
                self.path,
                column(&select.column)?,
                select,
            )?),
            None => None,
        };
        let mut keys = Vec::with_capacity(self.carried.len());
        for field in self.carried {
            keys.push(keys_of(self.path, column(&field.column)?, field)?);
        }
        let tessera: Option<Vec<bool>> = match self.tessera {
            true => {
                let column = column(TESSERA_ID_COLUMN)?;
                Some((0..len).map(|row| !column.is_null(row)).collect())
            }
            false => None,
        };
        let carries_tessera = |row: usize| tessera.as_ref().is_some_and(|t| t[row]);
        // Whether each row's value of the limit's attribute is at or above it, or null. A row
        // carrying a `tessera_id` is read whatever its value, and refused for it.
        let beyond = |name: &str, below: u64| -> Result<Vec<bool>> {
            let values = crate::input::id_values(self.path, column(name)?.as_ref(), name)?;
            Ok((0..len)
                .map(|row| {
                    (values.is_null(row) || values.value(row) >= below) && !carries_tessera(row)
                })
                .collect())
        };
        let mut outside: Option<Vec<bool>> = None;
        let mut left_out: Option<Vec<bool>> = None;
        match self.limit {
            None => {}
            Some(LimitRead::Prefix { column, below }) => {
                let keep = selected.get_or_insert_with(|| vec![true; len]);
                for (keep, beyond) in keep.iter_mut().zip(beyond(column, below)?) {
                    *keep &= !beyond;
                }
            }
            Some(LimitRead::Values { column, below }) => {
                let keep = selected.get_or_insert_with(|| vec![true; len]);
                let mut beyond = beyond(column, below)?;
                for (keep, beyond) in keep.iter_mut().zip(beyond.iter_mut()) {
                    *beyond &= *keep;
                    *keep &= !*beyond;
                }
                outside = Some(beyond);
            }
            Some(LimitRead::Named {
                column,
                below,
                every_row,
            }) => {
                left_out = Some(match (every_row, column) {
                    (false, Some(column)) => beyond(column, below)?,
                    _ => (0..len).map(|row| !carries_tessera(row)).collect(),
                });
            }
        }
        Ok(Scanned {
            first,
            len,
            selected,
            keys,
            outside,
            left_out,
            tessera,
        })
    }

    /// Each of `rows` as a report names it: `field = value` for each unique value it carries and a
    /// `tessera_id` where it carries one, or its position in the file where it carries neither.
    pub(crate) fn values_at(&self, rows: &[u64]) -> Result<BTreeMap<u64, String>> {
        let mut out = BTreeMap::new();
        if rows.is_empty() {
            return Ok(out);
        }
        let projection = self.groups.projection(&self.roots()?);
        let groups: Vec<usize> = (0..self.groups.count())
            .filter(|&group| {
                let first = self.groups.start(group);
                let end = first + self.groups.group_rows(group);
                rows.iter().any(|&row| row >= first && row < end)
            })
            .collect();
        self.groups
            .each_batch(self.path, &groups, &projection, |first, batch| {
                let end = first + batch.num_rows() as u64;
                for &row in rows.iter().filter(|&&row| row >= first && row < end) {
                    let at = (row - first) as usize;
                    let mut parts = Vec::new();
                    for field in self.carried {
                        let column = batch.column_by_name(&field.column).expect("projected");
                        if let Some(text) = value_text(column.as_ref(), at) {
                            parts.push(format!("{} = {text}", field.attribute));
                        }
                    }
                    if self.tessera {
                        let column = batch.column_by_name(TESSERA_ID_COLUMN).expect("projected");
                        if let Some(text) = value_text(column.as_ref(), at) {
                            parts.push(format!("{TESSERA_ID_COLUMN} = {text}"));
                        }
                    }
                    out.insert(
                        row,
                        match parts.is_empty() {
                            true => format!("row {row}"),
                            false => parts.join(", "),
                        },
                    );
                }
                Ok(ControlFlow::Continue(()))
            })?;
        Ok(out)
    }
}

/// A unique field's column as keys, `None` for a null. A value its attribute's type cannot hold,
/// and a column of another kind, are refused.
pub(crate) fn keys_of(
    path: &Path,
    column: &ArrayRef,
    field: &CarriedField,
) -> Result<Vec<Option<UniqueKey>>> {
    let refuse = |detail: String| BuildError::Schema {
        path: path.to_path_buf(),
        detail: format!(
            "attribute '{}' is read from column '{}', which {detail}",
            field.attribute, field.column
        ),
    };
    if field.ty == ScalarType::Keyword {
        let text = crate::utf8::Utf8Column::new(column.as_ref()).ok_or_else(|| {
            refuse(format!(
                "holds {:?}; a keyword is a string column",
                column.data_type()
            ))
        })?;
        return Ok((0..column.len())
            .map(|row| text.at(row).map(UniqueKey::keyword))
            .collect());
    }
    let integers = integers_of(column.as_ref()).ok_or_else(|| {
        refuse(format!(
            "holds {:?}; write it as an integer column",
            column.data_type()
        ))
    })?;
    integers
        .into_iter()
        .map(|value| match value {
            None => Ok(None),
            Some(value) => key_of_integer(field.ty, value).map(Some).ok_or_else(|| {
                refuse(format!(
                    "holds {value}, which a `{}` cannot hold",
                    field.ty.arrow_type_name()
                ))
            }),
        })
        .collect()
}

/// An integer or timestamp column's values, a null staying null; `None` for any other column.
fn integers_of(column: &dyn Array) -> Option<Vec<Option<i128>>> {
    use arrow::array::{
        Int16Array, Int32Array, Int64Array, Int8Array, TimestampMicrosecondArray, UInt16Array,
        UInt32Array, UInt64Array, UInt8Array,
    };
    let any = column.as_any();
    macro_rules! read {
        ($ty:ty) => {
            if let Some(values) = any.downcast_ref::<$ty>() {
                return Some(values.iter().map(|v| v.map(i128::from)).collect());
            }
        };
    }
    read!(UInt8Array);
    read!(UInt16Array);
    read!(UInt32Array);
    read!(UInt64Array);
    read!(Int8Array);
    read!(Int16Array);
    read!(Int32Array);
    read!(Int64Array);
    if matches!(column.data_type(), DataType::Timestamp(TimeUnit::Microsecond, _)) {
        read!(TimestampMicrosecondArray);
    }
    None
}

/// One cell as a report writes it: an integer in decimal, a string as itself.
pub(crate) fn value_text(column: &dyn Array, row: usize) -> Option<String> {
    if column.is_null(row) {
        return None;
    }
    if let Some(text) = crate::utf8::Utf8Column::new(column) {
        return text.at(row).map(str::to_string);
    }
    let integers = integers_of(&column.slice(row, 1))?;
    integers.first().copied().flatten().map(|v| v.to_string())
}
