//! Rows that address items: a change's `match`, and the member tables of a publication or a
//! growth. Each row names an item by a `tessera_id` column and a column for each unique field,
//! and every row of a request is resolved in one call, by the identity rule
//! ([`tessera_engine::Engine::name_items`]). A row naming no item, or naming two, is refused:
//! listed in the answer with its reason while the rest apply, or, in a strict request, refusing
//! the request whole.

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

    /// A table in its JSON form: an object of equal-length arrays keyed by `tessera_id` and
    /// unique field names. `what` names the table in a refusal.
    pub(crate) fn from_json(what: &str, value: &serde_json::Value) -> Result<Table, ApiError> {
        let serde_json::Value::Object(columns) = value else {
            return Err(ApiError::Contract(format!(
                "{what} is not a table; send an object of equal-length arrays keyed by \
                 `tessera_id` and unique field names, such as {{\"tessera_id\": [\"12\", \"40\"]}}"
            )));
        };
        let mut table = Table::default();
        for (at, (name, cells)) in columns.iter().enumerate() {
            let serde_json::Value::Array(cells) = cells else {
                return Err(ApiError::Contract(format!(
                    "{what}: column '{name}' is not an array; each column is an array with one \
                     cell per row"
                )));
            };
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
                .iter()
                .enumerate()
                .map(|(row, cell)| cell_text(what, name, row, cell))
                .collect::<Result<_, _>>()?;
            table.columns.push((name.clone(), cells));
        }
        Ok(table)
    }

    /// One row, from an object of cells keyed by column: a change's `match`.
    pub(crate) fn one_row(
        what: &str,
        cells: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<Table, ApiError> {
        let columns = cells
            .iter()
            .map(|(name, cell)| Ok((name.clone(), vec![cell_text(what, name, 0, cell)?])))
            .collect::<Result<_, ApiError>>()?;
        Ok(Table { rows: 1, columns })
    }

    /// A table from columns already read, each of `rows` cells: an Arrow member list.
    pub(crate) fn of_columns(rows: usize, columns: Vec<(String, Vec<Option<String>>)>) -> Table {
        debug_assert!(columns.iter().all(|(_, cells)| cells.len() == rows));
        Table { rows, columns }
    }
}

/// A cell as its column's text: a string as itself, an integer in decimal digits, `null` as none.
fn cell_text(
    what: &str,
    column: &str,
    row: usize,
    cell: &serde_json::Value,
) -> Result<Option<String>, ApiError> {
    match cell {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::String(text) => Ok(Some(text.clone())),
        // A JSON number loses `u64` precision past 2^53 in JavaScript.
        serde_json::Value::Number(n) if column != "tessera_id" && (n.is_i64() || n.is_u64()) => {
            Ok(Some(n.to_string()))
        }
        _ if column == "tessera_id" => Err(ApiError::Contract(format!(
            "{what}: row {row} of column 'tessera_id' is not a string; send a tessera_id as a \
             base-10 string, such as \"12345\""
        ))),
        _ => Err(ApiError::Contract(format!(
            "{what}: row {row} of column '{column}' is neither a string, an integer nor null; \
             send a keyword as a string and an integer as a number or in decimal digits"
        ))),
    }
}

/// What one addressing row became.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Named {
    Item(EntityId),
    Refused(Reason),
}

/// Resolve every row of `tables` in one call: one answer per row, table by table.
pub(crate) fn name_items(state: &AppState, tables: &[&Table]) -> Result<Vec<Vec<Named>>, ApiError> {
    let rows: usize = tables.iter().map(|t| t.rows).sum();
    if rows == 0 {
        return Ok(tables.iter().map(|_| Vec::new()).collect());
    }
    let mut names: Vec<&str> = Vec::new();
    for table in tables {
        for (name, _) in &table.columns {
            if !names.contains(&name.as_str()) {
                names.push(name);
            }
        }
    }
    let mut merged = AddressTable {
        rows,
        ..AddressTable::default()
    };
    for name in names {
        let cells: Vec<Option<String>> = tables
            .iter()
            .flat_map(|table| match table.columns.iter().find(|(n, _)| n == name) {
                Some((_, cells)) => cells.clone(),
                None => vec![None; table.rows],
            })
            .collect();
        if name == "tessera_id" {
            let ids = cells
                .into_iter()
                .map(|cell| cell.map(|text| tessera_id_of(&text)).transpose())
                .collect::<Result<_, _>>()?;
            merged.tessera_id = Some(ids);
        } else {
            merged.unique.push((name.to_string(), cells));
        }
    }
    let verdicts = state.engine.name_items(&merged).map_err(map_engine_error)?;
    let mut verdicts = verdicts.into_iter().map(|verdict| match verdict {
        Verdict::Names(entity) => Named::Item(entity),
        Verdict::Refused(refusal) => Named::Refused(refusal.kind()),
        Verdict::Creates => unreachable!("a call that addresses items creates none"),
    });
    Ok(tables
        .iter()
        .map(|table| verdicts.by_ref().take(table.rows).collect())
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
