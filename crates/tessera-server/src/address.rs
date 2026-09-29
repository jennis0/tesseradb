//! Rows that address items: a change's `match`, and the member tables of a publication or a
//! growth. Each row names an item by a `tessera_id` column and a column for each unique field,
//! and every row of a request is resolved in one call, by the identity rule
//! ([`tessera_engine::Engine::name_items`]). A row naming no item, or naming two, is refused:
//! listed in the answer with its reason while the rest apply, or, in a strict request, refusing
//! the request whole.

use std::collections::BTreeMap;

use tessera_engine::AddressTable;
use tessera_lifecycle::resolve::{Reason, Verdict};
use tessera_types::{EntityId, TesseraId};

use crate::error::{map_engine_error, ApiError};
use crate::state::AppState;

/// One table of addresses as a request wrote it: columns of equal length, each named
/// `tessera_id` or for a unique field, `None` where a cell is null.
#[derive(Debug, Clone, Default)]
pub(crate) struct Table {
    rows: usize,
    columns: Vec<(String, Vec<Option<String>>)>,
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
            let cells = cells
                .into_iter()
                .enumerate()
                .map(|(row, cell)| cell_text(what, &name, row, cell))
                .collect::<Result<_, _>>()?;
            table.columns.push((name, cells));
        }
        Ok(table)
    }

    /// One row, from cells keyed by column: a change's `match`.
    pub(crate) fn one_row(what: &str, cells: BTreeMap<String, Cell>) -> Result<Table, ApiError> {
        let columns = cells
            .into_iter()
            .map(|(name, cell)| Ok((name.clone(), vec![cell_text(what, &name, 0, cell)?])))
            .collect::<Result<_, ApiError>>()?;
        Ok(Table { rows: 1, columns })
    }

    /// A table from columns already read, each of `rows` cells: an Arrow member list.
    pub(crate) fn of_columns(rows: usize, columns: Vec<(String, Vec<Option<String>>)>) -> Table {
        debug_assert!(columns.iter().all(|(_, cells)| cells.len() == rows));
        Table { rows, columns }
    }
}

/// A member table in its JSON form, an object of arrays, decoded cell by cell as it is read.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(transparent)]
pub(crate) struct WireTable(BTreeMap<String, Vec<Cell>>);

/// One cell of an address: `null`, a string, or an integer, kept as its decimal digits.
#[derive(Debug, Clone)]
pub(crate) enum Cell {
    Null,
    Text(String),
    Integer(String),
}

impl<'de> serde::Deserialize<'de> for Cell {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Cell, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = Cell;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a string, an integer or null")
            }
            fn visit_unit<E>(self) -> Result<Cell, E> {
                Ok(Cell::Null)
            }
            fn visit_none<E>(self) -> Result<Cell, E> {
                Ok(Cell::Null)
            }
            fn visit_str<E>(self, text: &str) -> Result<Cell, E> {
                Ok(Cell::Text(text.to_string()))
            }
            fn visit_string<E>(self, text: String) -> Result<Cell, E> {
                Ok(Cell::Text(text))
            }
            fn visit_u64<E>(self, n: u64) -> Result<Cell, E> {
                Ok(Cell::Integer(n.to_string()))
            }
            fn visit_i64<E>(self, n: i64) -> Result<Cell, E> {
                Ok(Cell::Integer(n.to_string()))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

/// A cell as its column's text: a string as itself, an integer in decimal digits, `null` as none.
fn cell_text(what: &str, column: &str, row: usize, cell: Cell) -> Result<Option<String>, ApiError> {
    match cell {
        Cell::Null => Ok(None),
        Cell::Text(text) => Ok(Some(text)),
        // A JSON number loses `u64` precision past 2^53 in JavaScript.
        Cell::Integer(_) if column == "tessera_id" => Err(ApiError::Contract(format!(
            "{what}: row {row} of column 'tessera_id' is not a string; send a tessera_id as a \
             base-10 string, such as \"12345\""
        ))),
        Cell::Integer(digits) => Ok(Some(digits)),
    }
}

/// What one addressing row became.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Named {
    Item(EntityId),
    Refused(Reason),
}

/// Resolve every row of `tables` in one call: one answer per row, table by table.
pub(crate) fn name_items(
    state: &AppState,
    mut tables: Vec<Table>,
) -> Result<Vec<Vec<Named>>, ApiError> {
    let rows: usize = tables.iter().map(|t| t.rows).sum();
    let widths: Vec<usize> = tables.iter().map(|t| t.rows).collect();
    if rows == 0 {
        return Ok(tables.iter().map(|_| Vec::new()).collect());
    }
    let mut names: Vec<String> = Vec::new();
    for table in &tables {
        for (name, _) in &table.columns {
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
    }
    let mut merged = AddressTable {
        rows,
        ..AddressTable::default()
    };
    for name in names {
        let mut cells: Vec<Option<String>> = Vec::with_capacity(rows);
        for table in &mut tables {
            match table.columns.iter_mut().find(|(n, _)| *n == name) {
                Some((_, column)) => cells.append(column),
                None => cells.resize(cells.len() + table.rows, None),
            }
        }
        if name == "tessera_id" {
            let ids = cells
                .into_iter()
                .map(|cell| cell.map(|text| tessera_id_of(&text)).transpose())
                .collect::<Result<_, _>>()?;
            merged.tessera_id = Some(ids);
        } else {
            merged.unique.push((name, cells));
        }
    }
    let verdicts = state.engine.name_items(&merged).map_err(map_engine_error)?;
    let mut verdicts = verdicts.into_iter().map(|verdict| match verdict {
        Verdict::Names(entity) => Named::Item(entity),
        Verdict::Refused(refusal) => Named::Refused(refusal.kind()),
        Verdict::Creates => unreachable!("a call that addresses items creates none"),
    });
    Ok(widths
        .into_iter()
        .map(|rows| verdicts.by_ref().take(rows).collect())
        .collect())
}

/// A `tessera_id` cell, which is a base-10 string.
fn tessera_id_of(text: &str) -> Result<TesseraId, ApiError> {
    text.parse::<u64>().map(TesseraId::new).map_err(|_| {
        ApiError::Contract(format!(
            "'{text}' is not a tessera_id; send a tessera_id as a base-10 string, such as \"12345\""
        ))
    })
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
