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
//! the vocabulary `visibility = "derived"` hides: ask for `department = "black-programmes"`, and a
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

use tessera_engine::filter::{Endpoint, Family, FilterExpr, FilterOperand, Scalar};
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
            "a filter node must carry exactly one key — a column name, or \
             `all_of`/`any_of`/`none_of` — and this one carries {}. Two keys would need an \
             implicit operator between them, and the expression says which it wants",
            obj.len()
        )));
    }
    let (name, body) = obj.iter().next().expect("length checked");

    match name.as_str() {
        combinator @ ("all_of" | "any_of" | "none_of") => {
            let arr = body.as_array().ok_or_else(|| {
                bad(format!(
                    "`{combinator}` takes an array of filter expressions"
                ))
            })?;
            let kids = arr
                .iter()
                .map(|k| parse_node(k, family_of, resolve))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(match combinator {
                "all_of" => FilterExpr::AllOf(kids),
                "any_of" => FilterExpr::AnyOf(kids),
                // **The one-column rule is not checked here**, and deliberately: the engine checks
                // it once over the whole tree (`FilterExpr::check_negations`), where the same rule
                // also covers an expression this parser never saw — one an embedder built directly.
                // A copy here would be a second statement of a safety rule, which is the shape
                // `verdict`'s doc argues against.
                _ => FilterExpr::NoneOf(kids),
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
            // Named rather than folded into the generic refusal, because the mistake it catches
            // is a schema one rather than a syntax one: the caller wants word matching and the
            // column they named does not store words. Saying which declaration would give it to
            // them is the difference between a fixable error and a puzzling one.
            "match" => bad(format!(
                "column '{column}': `match` matches analysed words and needs a column declared \
                 `type = \"text\"`. `/v1/meta` lists which columns are text"
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
        // **The operand kinds are the request's string predicates, not one family's storage.** The
        // four mean byte-exact equality, prefix and substring over the value the item carries;
        // which of them a dictionary resolve answers and which an ordinal scan does is the
        // *column's* business, not the request's. Keeping them named for the predicate rather than
        // for the family is what stops the wire shape depending on a storage choice the client is
        // not told about and cannot act on. The `text` family joined without a wire change of its
        // own for the same reason, bringing one operand rather than a second spelling of these
        // four (records §4.4).
        (Family::Keyword, "eq") => Ok(FilterOperand::TextEquals(text_value(column, op, value)?)),
        (Family::Keyword, "in") => {
            let arr = value
                .as_array()
                .ok_or_else(|| bad(format!("column '{column}': `in` takes an array")))?;
            let needles = arr
                .iter()
                .map(|v| text_value(column, op, v))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(FilterOperand::TextIn(needles))
        }
        (Family::Keyword, "prefix") => {
            Ok(FilterOperand::TextPrefix(text_value(column, op, value)?))
        }
        (Family::Keyword, "contains") => {
            Ok(FilterOperand::TextContains(text_value(column, op, value)?))
        }
        // **`match` carries the query text, not tokens.** The engine analyses it with the column's
        // own analyser, taken from the identity the manifest recorded when the index was built, so
        // the query and the index cannot be segmented by different pipelines (decision 0070). A
        // parser that tokenised here would be a second place that choice lives.
        //
        // The scalar form is plain `match`: every token must appear. The object form names
        // `minimum_should_match` beside the query — Elasticsearch's own key, semantics intact.
        (Family::Text, "match") => {
            if let Some(query) = value.as_str() {
                return Ok(FilterOperand::Match {
                    query: query.to_string(),
                    minimum: None,
                });
            }
            let obj = value.as_object().ok_or_else(|| {
                bad(format!(
                    "column '{column}': `match` takes a string, or an object with `query` and \
                     optionally `minimum_should_match`"
                ))
            })?;
            let query = obj
                .get("query")
                .and_then(Value::as_str)
                .ok_or_else(|| bad(format!("column '{column}': `match` needs a `query` string")))?
                .to_string();
            let minimum = match obj.get("minimum_should_match") {
                None => None,
                Some(v) => {
                    let n = v.as_u64().filter(|n| *n > 0).ok_or_else(|| {
                        bad(format!(
                            "column '{column}': `minimum_should_match` is a positive whole number \
                             of tokens"
                        ))
                    })?;
                    Some(u32::try_from(n).map_err(|_| {
                        bad(format!("column '{column}': `minimum_should_match` is too large"))
                    })?)
                }
            };
            for key in obj.keys() {
                if key != "query" && key != "minimum_should_match" {
                    return Err(bad(format!(
                        "column '{column}': `match` takes `query` and `minimum_should_match`, not \
                         '{key}'"
                    )));
                }
            }
            Ok(FilterOperand::Match { query, minimum })
        }
        // **`phrase` takes a string and nothing else.** There is no m-of-n form of a phrase — "at
        // least two of these words, adjacent, in order" is not a question with one answer — so the
        // object form is refused rather than accepted and ignored. The query is carried unanalysed
        // for `match`'s reason, and its word *order* is what distinguishes the two operands: the
        // engine deduplicates and sorts a `match`'s tokens and keeps a phrase's exactly as the
        // analyser produced them.
        (Family::Text, "phrase") => Ok(FilterOperand::Phrase {
            query: value
                .as_str()
                .ok_or_else(|| {
                    bad(format!(
                        "column '{column}': `phrase` takes a string. There is no                          `minimum_should_match` for a phrase — adjacency is not a count"
                    ))
                })?
                .to_string(),
        }),
        (Family::Numeric, "eq") => Ok(FilterOperand::NumEquals(numeric_value(column, value)?)),
        (Family::Numeric, "in") => {
            let arr = value
                .as_array()
                .ok_or_else(|| bad(format!("column '{column}': `in` takes an array")))?;
            Ok(FilterOperand::NumIn(
                arr.iter()
                    .map(|v| numeric_value(column, v))
                    .collect::<Result<Vec<_>, _>>()?,
            ))
        }
        (Family::Numeric, "range") => parse_range(column, value),
        // `applies` above is derived from the same table this matches on, so every accepted
        // (family, operator) pair has an arm. Unreachable rather than a fallback: a new operator
        // added to `Family::operands` without an arm here should fail loudly in test, not parse
        // into some other operator's meaning.
        (family, op) => unreachable!("{} accepts '{op}' with no arm to build it", family.as_str()),
    }
}

/// A range's endpoints: `gte`/`gt` below, `lte`/`lt` above, any subset.
///
/// **One operator carrying a bounds object, rather than two operator keys.** A leaf carries exactly
/// one operator by construction (a two-key node would need an implicit conjunction between them),
/// and a range is one predicate with two sides rather than two predicates.
///
/// At least one bound is required. An empty `range` is refused rather than read as "no constraint":
/// a client that meant no constraint omits the leaf, and one that sent an empty object more likely
/// built it from an unpopulated form.
fn parse_range(column: &str, value: &Value) -> Result<FilterOperand, ApiError> {
    let obj = value
        .as_object()
        .ok_or_else(|| bad(format!("column '{column}': `range` takes a bounds object")))?;

    let mut lo: Option<Endpoint> = None;
    let mut hi: Option<Endpoint> = None;
    for (key, v) in obj {
        let endpoint = |inclusive: bool| -> Result<Endpoint, ApiError> {
            Ok(Endpoint {
                value: numeric_value(column, v)?,
                inclusive,
            })
        };
        let (slot, e) = match key.as_str() {
            "gte" => (&mut lo, endpoint(true)?),
            "gt" => (&mut lo, endpoint(false)?),
            "lte" => (&mut hi, endpoint(true)?),
            "lt" => (&mut hi, endpoint(false)?),
            other => {
                return Err(bad(format!(
                    "column '{column}': unknown range bound '{other}'. A range takes gte, gt, lte \
                     and lt"
                )))
            }
        };
        // Refused rather than resolved by precedence: `{gte: 3, gt: 5}` has no reading a caller
        // could have intended and every reading loses one of the two numbers they wrote.
        if slot.is_some() {
            return Err(bad(format!(
                "column '{column}': `range` carries two bounds on the same side"
            )));
        }
        *slot = Some(e);
    }
    if lo.is_none() && hi.is_none() {
        return Err(bad(format!(
            "column '{column}': `range` needs at least one of gte, gt, lte, lt. A leaf with no \
             constraint is an omitted leaf"
        )));
    }
    Ok(FilterOperand::Range { lo, hi })
}

/// A numeric comparand. An integer is carried exactly as `i128`; a fractional or out-of-range
/// number becomes `f64`. A JSON boolean is accepted for a `bool` column, which stores as 0/1.
fn numeric_value(column: &str, value: &Value) -> Result<Scalar, ApiError> {
    match value {
        Value::Bool(b) => Ok(Scalar::Int(i128::from(*b))),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(Scalar::Int(i as i128))
            } else if let Some(u) = n.as_u64() {
                Ok(Scalar::Int(u as i128))
            } else if let Some(f) = n.as_f64() {
                Ok(Scalar::Float(f))
            } else {
                Err(bad(format!(
                    "column '{column}': '{n}' is not a number this build can compare"
                )))
            }
        }
        _ => Err(bad(format!(
            "column '{column}': a numeric comparand must be a number"
        ))),
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
            } else if c == "title" || c == "submitter" {
                Some(Family::Keyword)
            } else if c == "score" {
                Some(Family::Numeric)
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
    /// value exists, which is what `derived` hides. It resolves to the reserved absent code, so
    /// the request is answered normally and matches nothing.
    #[test]
    fn an_unknown_value_is_an_empty_operand_not_a_refusal() {
        let expr = parse_str(r#"{"department": {"eq": "black-programmes"}}"#)
            .expect("an unknown value must not refuse");
        let FilterExpr::Leaf { operand, .. } = expr else {
            panic!("expected a leaf")
        };
        assert_eq!(
            operand,
            FilterOperand::Equals(AttrLocalId::new(UNRESOLVABLE))
        );
    }

    /// An unknown *column* is the opposite: a caller error, and safe to name.
    #[test]
    fn an_unknown_column_is_refused() {
        let err = parse_str(r#"{"nope": {"eq": "eng"}}"#).unwrap_err();
        assert!(
            format!("{err:?}").contains("not a filterable column"),
            "{err:?}"
        );
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
        let err =
            parse_str(r#"{"department": {"eq": "eng"}, "title": {"prefix": "x"}}"#).unwrap_err();
        assert!(format!("{err:?}").contains("exactly one key"), "{err:?}");
    }

    /// `none_of` parses like the other two combinators and carries its clauses through.
    ///
    /// **The one-column rule is not this parser's** — see the comment at the `none_of` arm — so a
    /// multi-column `none_of` parses here and is refused by the engine. The test for *that* lives
    /// with the rule, in `tests/filtering.rs`.
    #[test]
    fn none_of_parses_as_a_combinator() {
        let expr = parse_str(r#"{"none_of": [{"department": {"eq": "eng"}}]}"#).unwrap();
        let FilterExpr::NoneOf(kids) = expr else {
            panic!("expected a negation")
        };
        assert_eq!(kids.len(), 1);
        assert!(matches!(kids[0], FilterExpr::Leaf { .. }));
    }

    /// A misspelt combinator is still an unknown *column*, which is the right error: the node's key
    /// is a column name unless it is one of the three reserved words.
    #[test]
    fn a_misspelt_combinator_is_an_unknown_column() {
        let err = parse_str(r#"{"non_of": [{"department": {"eq": "eng"}}]}"#).unwrap_err();
        assert!(
            format!("{err:?}").contains("not a filterable column"),
            "{err:?}"
        );
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
        assert!(
            format!("{err:?}").contains("does not take 'prefix'"),
            "{err:?}"
        );
    }

    #[test]
    fn a_range_carries_both_endpoints_with_their_inclusivity() {
        let expr = parse_str(r#"{"score": {"range": {"gte": 30, "lt": 40}}}"#).unwrap();
        let FilterExpr::Leaf { operand, .. } = expr else {
            panic!("expected a leaf")
        };
        assert_eq!(
            operand,
            FilterOperand::Range {
                lo: Some(Endpoint {
                    value: Scalar::Int(30),
                    inclusive: true
                }),
                hi: Some(Endpoint {
                    value: Scalar::Int(40),
                    inclusive: false
                }),
            }
        );
    }

    /// An open side is one bound. An *empty* range is refused: a client meaning "no constraint"
    /// omits the leaf, and one that sent `{}` more likely built it from an unpopulated form.
    #[test]
    fn a_range_needs_at_least_one_bound() {
        assert!(parse_str(r#"{"score": {"range": {"gte": 30}}}"#).is_ok());
        let err = parse_str(r#"{"score": {"range": {}}}"#).unwrap_err();
        assert!(format!("{err:?}").contains("at least one of"), "{err:?}");
    }

    /// Two bounds on one side are refused rather than resolved by precedence: every reading
    /// discards one of the two numbers the caller wrote.
    #[test]
    fn a_range_refuses_two_bounds_on_one_side() {
        let err = parse_str(r#"{"score": {"range": {"gte": 3, "gt": 5}}}"#).unwrap_err();
        assert!(format!("{err:?}").contains("same side"), "{err:?}");
    }

    /// A 64-bit integer bound survives the parse exactly — the parser must not route it through
    /// `f64` any more than the scan does.
    #[test]
    fn a_large_integer_bound_is_carried_exactly() {
        let big = (1u64 << 53) + 1;
        let expr = parse_str(&format!(r#"{{"score": {{"eq": {big}}}}}"#)).unwrap();
        let FilterExpr::Leaf { operand, .. } = expr else {
            panic!("expected a leaf")
        };
        assert_eq!(operand, FilterOperand::NumEquals(Scalar::Int(big as i128)));
    }

    /// `range` is a numeric operator; a string column does not take it, and the family check says
    /// so rather than answering empty.
    #[test]
    fn range_on_a_string_column_is_refused() {
        let err = parse_str(r#"{"title": {"range": {"gte": 3}}}"#).unwrap_err();
        assert!(
            format!("{err:?}").contains("does not take 'range'"),
            "{err:?}"
        );
    }

    /// `match` on a column that is not `text` names the declaration that would give it, rather
    /// than refusing generically: the caller wants word matching and the column they picked does
    /// not store words, which is a schema mistake and not a syntax one.
    #[test]
    fn match_on_a_non_text_column_names_the_declaration_it_needs() {
        let err = parse_str(r#"{"title": {"match": "smith"}}"#).unwrap_err();
        let text = format!("{err:?}");
        assert!(text.contains(r#"`type = \"text\"`"#), "{text}");
        assert!(
            !text.contains("not built"),
            "the operator is built; a refusal saying otherwise sends the caller to look for a \
             missing feature instead of at their column: {text}"
        );
    }

    /// **A keyword column parses exactly as a `utf8` one does.** The four string predicates carry
    /// the same meaning over both, and which of them a dictionary answers is the column's business:
    /// a client that had to send a different operand shape for a keyword would be told a storage
    /// choice it cannot act on.
    #[test]
    fn a_keyword_column_takes_the_four_string_operators() {
        for (op, expected) in [
            (
                r#"{"submitter": {"eq": "hep-th"}}"#,
                FilterOperand::TextEquals("hep-th".into()),
            ),
            (
                r#"{"submitter": {"prefix": "hep"}}"#,
                FilterOperand::TextPrefix("hep".into()),
            ),
            (
                r#"{"submitter": {"contains": "p-t"}}"#,
                FilterOperand::TextContains("p-t".into()),
            ),
            (
                r#"{"submitter": {"in": ["hep-th", "cs"]}}"#,
                FilterOperand::TextIn(vec!["hep-th".into(), "cs".into()]),
            ),
        ] {
            let FilterExpr::Leaf { operand, .. } = parse_str(op).unwrap() else {
                panic!("expected a leaf")
            };
            assert_eq!(operand, expected, "{op}");
        }
    }

    /// A keyword takes no `range`, and the refusal names the family — which is what makes the
    /// family worth publishing separately from `string` even though the operator lists match.
    #[test]
    fn range_on_a_keyword_column_is_refused() {
        let err = parse_str(r#"{"submitter": {"range": {"gte": 3}}}"#).unwrap_err();
        assert!(format!("{err:?}").contains("keyword column"), "{err:?}");
        assert!(
            format!("{err:?}").contains("does not take 'range'"),
            "{err:?}"
        );
    }

    /// An operator no family has is refused by the same family check — the message names what
    /// the column *does* take rather than only what it does not.
    #[test]
    fn an_unknown_operator_is_refused() {
        let err = parse_str(r#"{"title": {"regex": "s.*"}}"#).unwrap_err();
        assert!(
            format!("{err:?}").contains("does not take 'regex'"),
            "{err:?}"
        );
        assert!(format!("{err:?}").contains("keyword column"), "{err:?}");
    }
}
