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
/// **The dictionary decides, not the declaration, and that is the narrower of the two.** A layer
/// carries a dictionary exactly when it is a keyword layer — [`FilterColumns::compose`] refuses
/// every other pairing in both directions — so reading the route off the layer keeps the resolve
/// and the ordinals it is compared against inseparable by construction, where consulting the
/// column's family here would leave a keyword layer with a missing dictionary silently scanning
/// its ordinals as though they were bytes.
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

/// One operand against one layer's value column.
///
/// **The dispatch is here, once, rather than per layer inside a scan.** Each arm is the scan the
/// column crate exposes for that family, and the match is on the *operand* — never on the values —
/// so a layer costs what its share of the candidate costs and nothing about which value is sought
/// reaches this decision.
pub(in crate::filter) fn scan(values: &ValueColumn, operand: &FilterOperand, candidate: &Bitmap) -> Bitmap {
    match operand {
        FilterOperand::Equals(v) => values.scan_eq(candidate, *v),
        FilterOperand::In(vs) => values.scan_in(candidate, vs),
        // **A string operand against a layer that carries no dictionary matches nothing.** Every
        // string operand belongs to the keyword family, whose layers all carry one, so this is
        // unreachable through the API: the operator/family check at the parse refuses a string
        // operator on a category or a numeric column, and `FilterColumns::compose` refuses the
        // dictionary/family mismatch in both directions. It is the second line of defence, and
        // empty is the direction that fails safely — it under-reports, which narrows `M_sel` under
        // **I12**, where comparing a needle against a code would answer a different question.
        FilterOperand::TextEquals(_)
        | FilterOperand::TextIn(_)
        | FilterOperand::TextPrefix(_)
        | FilterOperand::TextContains(_)
        // `match` never reaches a value column: the text family has none, and the parse gate
        // refuses the operator elsewhere. Empty is the same fail-safe reading the string arms take.
        | FilterOperand::Match { .. }
        | FilterOperand::Phrase { .. } => Bitmap::new(),
        FilterOperand::NumEquals(n) => values.scan_num_eq(candidate, *n),
        FilterOperand::NumIn(ns) => values.scan_num_in(candidate, ns),
        FilterOperand::Range { lo, hi } => values.scan_range(candidate, *lo, *hi),
    }
}
