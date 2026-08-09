//! Parsing a request's `filters` object into an engine [`FilterExpr`] (contracts §3.2).
//!
//! **This is the trust boundary for the filter surface.** Everything downstream assumes a
//! well-formed expression over declared columns; this module is where caller bytes become that, or
//! become a typed refusal.
//!
//! # The one distinction the whole module turns on
//!
//! An **unknown column** is a `422`. An **unknown value** is an empty operand.
//!
//! They look alike and are not. A caller naming a column that does not exist made an error and can
//! be told so. Refusing an unknown *value* would make the filter an existence oracle over exactly
//! the vocabulary `listing = "per_viewer"` hides: ask for `department = "black-programmes"`, and a
//! `422` says the value exists while an empty result says nothing. So an unresolvable value
//! resolves to a code no row carries and the request is answered normally
//! (`filter-surface.md` §2.1).
//!
//! The same rule is why `/v1/categories` filters the offered value set rather than refusing: the
//! two surfaces have to agree, or one of them becomes the oracle for the other.
//!
//! # Keys or codes, and why both are accepted
//!
//! A category value may arrive as its **key** (a JSON string) or its **code** (a JSON integer), and
//! the two are unambiguous because a key is never an integer. `/control/ingest` takes keys only, and
//! contracts §2.4 gives two reasons — a caller supplying a code would be the minting authority, and
//! a code can only be range-checked where a key is membership-checked. **Neither transfers here.**
//! Filtering mints nothing, and the checkability that makes keys better on ingest is the very thing
//! this path is forbidden to act on, per the rule above. So the client may use whichever it holds —
//! and it holds codes, since the points batch ships codes and nothing else.

use serde_json::Value;

use tessera_engine::filter::{Family, FilterExpr, FilterOperand};
use tessera_types::AttrLocalId;

use crate::error::ApiError;

/// The engine's sentinel for a value that does not resolve — see its doc for why this is an empty
/// operand rather than a refusal. Named there rather than here because it is a property of
/// evaluation, and `check-layers.sh` keeps this crate on engine API types only.
use tessera_engine::filter::UNRESOLVABLE_VALUE as UNRESOLVABLE_ID;
const UNRESOLVABLE: u32 = UNRESOLVABLE_ID.raw();

/// Parse `filters` into an expression, or refuse.
///
/// `family_of` reports a column's family, or `None` for a name that is not a declared filterable
/// column. `resolve` maps `(column, key)` to a code.
pub fn parse(
    filters: &Value,
    family_of: &dyn Fn(&str) -> Option<Family>,
    resolve: &dyn Fn(&str, &str) -> Option<u32>,
) -> Result<FilterExpr, ApiError> {
    parse_node(filters, family_of, resolve)
}

fn bad(detail: impl Into<String>) -> ApiError {
    ApiError::Contract(detail.into())
}

fn parse_node(
    node: &Value,
    family_of: &dyn Fn(&str) -> Option<Family>,
    resolve: &dyn Fn(&str, &str) -> Option<u32>,
) -> Result<FilterExpr, ApiError> {
    let obj = node
        .as_object()
        .ok_or_else(|| bad("a filter node must be an object"))?;
    if obj.len() != 1 {
        return Err(bad(format!(
            "a filter node must carry exactly one key — a column name, or `all_of`/`any_of` — and \
             this one carries {}. Two keys would need an implicit operator between them, and the \
             expression says which it wants",
            obj.len()
        )));
    }
    let (name, body) = obj.iter().next().expect("length checked");

    match name.as_str() {
        // `none_of` is named here only so it refuses with its reason rather than as an unknown
        // column: it is specified and not built (decision 0059), and it carries a rule that must
        // ship with it.
        "none_of" => Err(bad(
            "`none_of` is specified and not built (decision 0059). Negation over a `per_viewer` \
             category must be evaluated within the visible vocabulary, or it proves the existence \
             of values the listing hides",
        )),
        combinator @ ("all_of" | "any_of") => {
            let arr = body.as_array().ok_or_else(|| {
                bad(format!("`{combinator}` takes an array of filter expressions"))
            })?;
            let kids = arr
                .iter()
                .map(|k| parse_node(k, family_of, resolve))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(if combinator == "all_of" {
                FilterExpr::AllOf(kids)
            } else {
                FilterExpr::AnyOf(kids)
            })
        }
        column => {
            // **An unknown column is an error; an unknown value is not.** See the module header.
            let Some(family) = family_of(column) else {
                return Err(bad(format!(
                    "'{column}' is not a filterable column. `/v1/meta`'s `filter_operands` lists \
                     the columns and the operators each accepts"
                )));
            };
            Ok(FilterExpr::Leaf {
                column: column.to_string(),
                operand: parse_operand(column, family, body, resolve)?,
            })
        }
    }
}

fn parse_operand(
    column: &str,
    family: Family,
    body: &Value,
    resolve: &dyn Fn(&str, &str) -> Option<u32>,
) -> Result<FilterOperand, ApiError> {
    let obj = body
        .as_object()
        .ok_or_else(|| bad(format!("column '{column}' needs an operator object")))?;
    if obj.len() != 1 {
        return Err(bad(format!(
            "column '{column}' must carry exactly one operator, not {}",
            obj.len()
        )));
    }
    let (op, value) = obj.iter().next().expect("length checked");

    // **An operator outside the column's family is a shape error, not an empty operand.** The
    // family is deployment schema — `/v1/meta` publishes it precisely so a client need not infer
    // it — and is identical for every principal, so refusing discloses nothing. The alternative,
    // answering empty, turns a client typo into a silent "no matches" and contradicts the refusal
    // `match` already gets one operator over. (An unknown *value* stays an empty operand: that one
    // is viewer data, and refusing it would be an existence oracle.)
    let applies = family.operands().contains(&op.as_str());
    if !applies {
        return Err(match op.as_str() {
            // ⊘ Named rather than folded into the generic refusal: `match` is the analysed-token
            // operator a `text` column would take, and that type is not declarable yet (#44), so
            // this names what is absent rather than refusing generically (decision 0013).
            "match" => bad(format!(
                "column '{column}': `match` needs a column of declared type `text`, whose \
                 analysed-token matching is specified and not built"
            )),
            other => bad(format!(
                "column '{column}' is a {} column, which takes {:?}; it does not take '{other}'",
                family.as_str(),
                family.operands()
            )),
        });
    }

    match (family, op.as_str()) {
        (Family::Category, "eq") => Ok(FilterOperand::Equals(AttrLocalId::new(category_value(
            column, value, resolve,
        )?))),
        (Family::Category, "in") => {
            let arr = value
                .as_array()
                .ok_or_else(|| bad(format!("column '{column}': `in` takes an array")))?;
            let codes = arr
                .iter()
                .map(|v| category_value(column, v, resolve).map(AttrLocalId::new))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(FilterOperand::In(codes))
        }
        (Family::Text, "eq") => Ok(FilterOperand::TextEquals(text_value(column, op, value)?)),
        (Family::Text, "in") => {
            let arr = value
                .as_array()
                .ok_or_else(|| bad(format!("column '{column}': `in` takes an array")))?;
            let needles = arr
                .iter()
                .map(|v| text_value(column, op, v))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(FilterOperand::TextIn(needles))
        }
        (Family::Text, "prefix") => Ok(FilterOperand::TextPrefix(text_value(column, op, value)?)),
        (Family::Text, "contains") => Ok(FilterOperand::TextContains(text_value(column, op, value)?)),
        // `applies` above is derived from the same table this matches on, so every accepted
        // (family, operator) pair has an arm. Unreachable rather than a fallback: a new operator
        // added to `Family::operands` without an arm here should fail loudly in test, not parse
        // into some other operator's meaning.
        (family, op) => unreachable!("{} accepts '{op}' with no arm to build it", family.as_str()),
    }
}

/// A category value: a **key** (JSON string) or a **code** (JSON integer).
///
/// An unresolvable key yields [`UNRESOLVABLE`] rather than an error — the module header's rule. A
/// code is taken as given and range-checked only, which is safe for the same reason: an unassigned
/// code matches nothing, exactly as an unknown key does.
fn category_value(
    column: &str,
    value: &Value,
    resolve: &dyn Fn(&str, &str) -> Option<u32>,
) -> Result<u32, ApiError> {
    match value {
        Value::String(key) => Ok(resolve(column, key).unwrap_or(UNRESOLVABLE)),
        Value::Number(n) => n
            .as_u64()
            .and_then(|c| u32::try_from(c).ok())
            .ok_or_else(|| {
                bad(format!(
                    "column '{column}': a category code must be a non-negative integer below 2³²"
                ))
            }),
        _ => Err(bad(format!(
            "column '{column}': a category value is its key (a string) or its code (an integer)"
        ))),
    }
}

fn text_value(column: &str, op: &str, value: &Value) -> Result<String, ApiError> {
    value
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| bad(format!("column '{column}': `{op}` takes a string")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema(name: &str) -> impl Fn(&str) -> Option<Family> + '_ {
        move |c: &str| {
            if c == name {
                Some(Family::Category)
            } else if c == "title" {
                Some(Family::Text)
            } else {
                None
            }
        }
    }

    fn codes(column: &str, key: &str) -> Option<u32> {
        match (column, key) {
            ("department", "eng") => Some(7),
            ("department", "sales") => Some(9),
            _ => None,
        }
    }

    fn parse_str(text: &str) -> Result<FilterExpr, ApiError> {
        let v: Value = serde_json::from_str(text).unwrap();
        parse(&v, &schema("department"), &codes)
    }

    #[test]
    fn a_leaf_resolves_a_key_to_its_code() {
        let expr = parse_str(r#"{"department": {"eq": "eng"}}"#).unwrap();
        assert_eq!(
            expr,
            FilterExpr::Leaf {
                column: "department".into(),
                operand: FilterOperand::Equals(AttrLocalId::new(7)),
            }
        );
    }

    /// Keys and codes are freely mixed, and unambiguous: a key is never an integer.
    #[test]
    fn keys_and_codes_may_be_mixed_in_one_set() {
        let expr = parse_str(r#"{"department": {"in": ["eng", 4711]}}"#).unwrap();
        let FilterExpr::Leaf { operand, .. } = expr else {
            panic!("expected a leaf")
        };
        assert_eq!(
            operand,
            FilterOperand::In(vec![AttrLocalId::new(7), AttrLocalId::new(4711)])
        );
    }

    /// **The disclosure control.** An unknown value is not an error — refusing it would say the
    /// value exists, which is what `per_viewer` hides. It resolves to the reserved absent code, so
    /// the request is answered normally and matches nothing.
    #[test]
    fn an_unknown_value_is_an_empty_operand_not_a_refusal() {
        let expr = parse_str(r#"{"department": {"eq": "black-programmes"}}"#)
            .expect("an unknown value must not refuse");
        let FilterExpr::Leaf { operand, .. } = expr else {
            panic!("expected a leaf")
        };
        assert_eq!(operand, FilterOperand::Equals(AttrLocalId::new(UNRESOLVABLE)));
    }

    /// An unknown *column* is the opposite: a caller error, and safe to name.
    #[test]
    fn an_unknown_column_is_refused() {
        let err = parse_str(r#"{"nope": {"eq": "eng"}}"#).unwrap_err();
        assert!(format!("{err:?}").contains("not a filterable column"), "{err:?}");
    }

    #[test]
    fn combinators_nest() {
        let expr = parse_str(
            r#"{"all_of": [{"department": {"eq": "eng"}},
                           {"any_of": [{"title": {"prefix": "smi"}}]}]}"#,
        )
        .unwrap();
        assert_eq!(expr.depth(), 3);
    }

    /// A node with two keys is refused rather than given an implicit operator between them.
    #[test]
    fn a_two_key_node_is_refused() {
        let err = parse_str(r#"{"department": {"eq": "eng"}, "title": {"prefix": "x"}}"#)
            .unwrap_err();
        assert!(format!("{err:?}").contains("exactly one key"), "{err:?}");
    }

    /// `none_of` refuses with its own reason rather than as an unknown column, so a caller learns
    /// it is unbuilt rather than misspelt.
    #[test]
    fn none_of_names_what_is_absent() {
        let err = parse_str(r#"{"none_of": [{"department": {"eq": "eng"}}]}"#).unwrap_err();
        assert!(format!("{err:?}").contains("specified and not built"), "{err:?}");
        assert!(format!("{err:?}").contains("per_viewer"), "{err:?}");
    }

    /// `in` over a string column is `eq` over a list — the same generalisation a category gets,
    /// and it produces the string-valued operand, not the code-valued one.
    #[test]
    fn in_over_a_string_column_is_string_valued() {
        let expr = parse_str(r#"{"title": {"in": ["smith", "jones"]}}"#).unwrap();
        let FilterExpr::Leaf { operand, .. } = expr else {
            panic!("expected a leaf")
        };
        assert_eq!(
            operand,
            FilterOperand::TextIn(vec!["smith".into(), "jones".into()])
        );
    }

    /// **An operator outside the column's family is a shape error, not an empty operand.** The
    /// family is deployment schema and identical for every principal, so refusing discloses
    /// nothing — where refusing an unknown *value* would be an existence oracle.
    #[test]
    fn an_operator_outside_the_family_is_refused() {
        let err = parse_str(r#"{"department": {"prefix": "al"}}"#).unwrap_err();
        assert!(format!("{err:?}").contains("category column"), "{err:?}");
        assert!(format!("{err:?}").contains("does not take 'prefix'"), "{err:?}");
    }

    #[test]
    fn match_names_the_absent_text_type() {
        let err = parse_str(r#"{"title": {"match": "smith"}}"#).unwrap_err();
        assert!(format!("{err:?}").contains("declared type `text`"), "{err:?}");
    }

    /// An operator no family has is refused by the same family check — the message names what
    /// the column *does* take rather than only what it does not.
    #[test]
    fn an_unknown_operator_is_refused() {
        let err = parse_str(r#"{"title": {"regex": "s.*"}}"#).unwrap_err();
        assert!(format!("{err:?}").contains("does not take 'regex'"), "{err:?}");
        assert!(format!("{err:?}").contains("string column"), "{err:?}");
    }
}
