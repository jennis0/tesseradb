//! Rows that address items: a change's `match`, and the member tables of a publication or a
//! growth. Each row names an item by a `tessera_id` column and a column for each unique field,
//! and every row of a request is resolved in one call, by the identity rule
//! ([`tessera_engine::Engine::name_items`]). A row naming no item, or naming two, is refused:
//! listed in the answer with its reason while the rest apply, or, in a strict request, refusing
//! the request whole. A column that is neither `tessera_id` nor a unique field names nothing and
//! is ignored, as a build ignores it, and the answer names it.

use std::collections::BTreeMap;

use tessera_engine::{AddressTable, AddressValue};
use tessera_lifecycle::resolve::{Reason, Verdict};
use tessera_types::{EntityId, TesseraId};

use crate::error::{map_engine_error, ApiError};
use crate::state::AppState;

/// One table of addresses as a request wrote it: columns of equal length, `None` where a cell is
/// null.
#[derive(Debug, Clone, Default)]
pub(crate) struct Table {
    rows: usize,
    columns: Vec<(String, Vec<Option<AddressValue>>)>,
}

impl Table {
    pub(crate) fn len(&self) -> usize {
        self.rows
    }

    /// A table as a body carries it: columns of equal length keyed by `tessera_id` and unique
    /// field names. `what` names the table in a refusal.
    pub(crate) fn from_wire(what: &str, wire: WireTable) -> Result<Table, ApiError> {
        let mut table = Table::default();
        for (at, (name, cells)) in wire.0.into_iter().enumerate() {
            if at == 0 {
                table.rows = cells.len();
            } else if cells.len() != table.rows {
                return Err(ApiError::Contract(format!(
                    "{what}: column '{name}' has {} cells and the table's first column has {}; \
                     give every column one cell per row, null where it names nothing",
                    cells.len(),
                    table.rows
                )));
            }
            table
                .columns
                .push((name, cells.into_iter().map(|c| c.0).collect()));
        }
        Ok(table)
    }

    /// One row, from cells keyed by column: a change's `match`.
    pub(crate) fn one_row(cells: BTreeMap<String, Cell>) -> Table {
        Table {
            rows: 1,
            columns: cells
                .into_iter()
                .map(|(name, cell)| (name, vec![cell.0]))
                .collect(),
        }
    }

    /// A table from columns already read, each of `rows` cells: an Arrow member list.
    pub(crate) fn of_columns(
        rows: usize,
        columns: Vec<(String, Vec<Option<AddressValue>>)>,
    ) -> Table {
        debug_assert!(columns.iter().all(|(_, cells)| cells.len() == rows));
        Table { rows, columns }
    }
}

/// A member table in its JSON form, an object of arrays, decoded cell by cell as it is read.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(transparent)]
pub(crate) struct WireTable(BTreeMap<String, Vec<Cell>>);

/// One cell of an address: `null`, a string, an integer, or any other value, which names nothing
/// and is refused only in a column that names items.
#[derive(Debug, Clone)]
pub(crate) struct Cell(Option<AddressValue>);

impl<'de> serde::Deserialize<'de> for Cell {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Cell, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Cell;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a JSON value")
            }
            fn visit_bool<E>(self, value: bool) -> Result<Cell, E> {
                Ok(Cell(Some(AddressValue::Other(value.to_string()))))
            }
            fn visit_f64<E>(self, value: f64) -> Result<Cell, E> {
                Ok(Cell(Some(AddressValue::Other(format!(
                    "the number {value}"
                )))))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Cell, A::Error> {
                while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
                Ok(Cell(Some(AddressValue::Other("a list".to_string()))))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Cell, A::Error> {
                while map
                    .next_entry::<serde::de::IgnoredAny, serde::de::IgnoredAny>()?
                    .is_some()
                {}
                Ok(Cell(Some(AddressValue::Other("an object".to_string()))))
            }
            fn visit_unit<E>(self) -> Result<Cell, E> {
                Ok(Cell(None))
            }
            fn visit_none<E>(self) -> Result<Cell, E> {
                Ok(Cell(None))
            }
            fn visit_str<E>(self, text: &str) -> Result<Cell, E> {
                Ok(Cell(Some(AddressValue::Text(text.to_string()))))
            }
            fn visit_string<E>(self, text: String) -> Result<Cell, E> {
                Ok(Cell(Some(AddressValue::Text(text))))
            }
            fn visit_u64<E>(self, n: u64) -> Result<Cell, E> {
                Ok(Cell(Some(AddressValue::Integer(i128::from(n)))))
            }
            fn visit_i64<E>(self, n: i64) -> Result<Cell, E> {
                Ok(Cell(Some(AddressValue::Integer(i128::from(n)))))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

/// What one addressing row became.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Named {
    Item(EntityId),
    Refused(Reason),
}

/// Every table of one request as the one table the engine resolves, and each table's rows.
pub(crate) struct Merged {
    table: AddressTable,
    widths: Vec<usize>,
}

impl Merged {
    /// The tables' columns, moved into one table: a column a table lacks is null in its rows.
    pub(crate) fn of(tables: Vec<Table>) -> Result<Merged, ApiError> {
        let widths: Vec<usize> = tables.iter().map(|t| t.rows).collect();
        let rows: usize = widths.iter().sum();
        let mut tables = tables;
        let mut names: Vec<String> = Vec::new();
        for table in &tables {
            for (name, _) in &table.columns {
                if !names.contains(name) {
                    names.push(name.clone());
                }
            }
        }
        let mut table = AddressTable {
            rows,
            ..AddressTable::default()
        };
        for name in names {
            let mut cells: Vec<Option<AddressValue>> = Vec::with_capacity(rows);
            for one in &mut tables {
                match one.columns.iter_mut().find(|(n, _)| *n == name) {
                    Some((_, column)) => cells.append(column),
                    None => cells.resize(cells.len() + one.rows, None),
                }
            }
            if name == "tessera_id" {
                let ids = cells
                    .into_iter()
                    .map(|cell| cell.map(tessera_id_of).transpose())
                    .collect::<Result<_, _>>()?;
                table.tessera_id = Some(ids);
            } else {
                table.columns.push((name, cells));
            }
        }
        Ok(Merged { table, widths })
    }

    /// Resolve every row: one answer per row, table by table, and the columns ignored.
    pub(crate) fn name(
        &self,
        state: &AppState,
    ) -> Result<(Vec<Vec<Named>>, Vec<String>), ApiError> {
        let named = state
            .engine
            .name_items(&self.table)
            .map_err(map_engine_error)?;
        let mut verdicts = named.verdicts.into_iter().map(|verdict| match verdict {
            Verdict::Names(entity) => Named::Item(entity),
            Verdict::Refused(refusal) => Named::Refused(refusal.kind()),
            Verdict::Creates => unreachable!("a call that addresses items creates none"),
        });
        let per_table = self
            .widths
            .iter()
            .map(|rows| verdicts.by_ref().take(*rows).collect())
            .collect();
        Ok((per_table, named.ignored))
    }
}

/// A `tessera_id` cell, which is a base-10 string.
fn tessera_id_of(value: AddressValue) -> Result<TesseraId, ApiError> {
    let refuse = |shown: String| {
        ApiError::Contract(format!(
            "{shown} is not a tessera_id; send a tessera_id as a base-10 string, such as \"12345\""
        ))
    };
    match value {
        AddressValue::Text(text) => text
            .parse::<u64>()
            .map(TesseraId::new)
            .map_err(|_| refuse(format!("'{text}'"))),
        // A JSON number loses `u64` precision past 2^53 in JavaScript.
        AddressValue::Integer(n) => Err(refuse(format!("the number {n}"))),
        AddressValue::Other(shown) => Err(refuse(shown)),
    }
}

/// The answer to a strict request whose row `row` of `what` was refused for `reason`: 404 where
/// the row names no item, 409 where it names two. The row is named by position, never by a value.
pub(crate) fn strict_refusal(what: &str, row: usize, reason: Reason) -> ApiError {
    match reason {
        Reason::NamesNoItem | Reason::UnknownTesseraId => ApiError::Unknown(format!(
            "row {row} of {what} names nothing this deployment holds"
        )),
        Reason::NamesTwo => ApiError::Conflict(format!(
            "row {row} of {what} names two items; name one item in each row"
        )),
        Reason::OneItemTwice | Reason::OneValueTwice => ApiError::Conflict(format!(
            "row {row} of {what} names an item an earlier row names; send each item once"
        )),
    }
}

/// A refused row as an answer lists it.
pub(crate) fn refused_json(row: usize, reason: Reason) -> serde_json::Value {
    serde_json::json!({ "row": row, "reason": reason.as_str() })
}
