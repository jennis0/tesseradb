pub(in crate::filter) mod keyword;
pub(in crate::filter) mod text;

use croaring::Bitmap;
use tessera_filter::ValueColumn;

use self::keyword::scan_keyword;
use crate::filter::columns::Layer;
use crate::filter::error::FilterError;
use crate::filter::expr::FilterOperand;

/// One operand against one layer, whichever family the layer belongs to.
///
/// The layer's dictionary picks the keyword route, not the column's declaration: a keyword layer
/// always carries one, so reading it off the layer keeps a resolve and its ordinals from the same
/// layer rather than from the column's family.
pub(in crate::filter) fn scan_layer(
    column: &str,
    layer: &Layer,
    operand: &FilterOperand,
    candidate: &Bitmap,
) -> Result<Bitmap, FilterError> {
    match &layer.dict {
        Some(dict) => scan_keyword(&layer.values, dict, operand, candidate).map_err(|e| {
            FilterError::DictionaryUnreadable {
                column: column.to_string(),
                detail: e.to_string(),
            }
        }),
        None => Ok(scan(&layer.values, operand, candidate)),
    }
}

/// One operand against one layer's value column. The dispatch is on the operand, never the value.
pub(in crate::filter) fn scan(values: &ValueColumn, operand: &FilterOperand, candidate: &Bitmap) -> Bitmap {
    match operand {
        FilterOperand::Equals(v) => values.scan_eq(candidate, *v),
        FilterOperand::In(vs) => values.scan_in(candidate, vs),
        // A string operand against a layer with no dictionary answers empty: every string operand
        // is keyword, and a keyword layer always carries a dictionary, so this arm is unreachable
        // through the parse. Empty is the fail-safe reading.
        FilterOperand::TextEquals(_)
        | FilterOperand::TextIn(_)
        | FilterOperand::TextPrefix(_)
        | FilterOperand::TextContains(_)
        // `match` and `Phrase` reach no value column either: the text family has none.
        | FilterOperand::Match { .. }
        | FilterOperand::Phrase { .. } => Bitmap::new(),
        FilterOperand::NumEquals(n) => values.scan_num_eq(candidate, *n),
        FilterOperand::NumIn(ns) => values.scan_num_in(candidate, ns),
        FilterOperand::Range { lo, hi } => values.scan_range(candidate, *lo, *hi),
    }
}
