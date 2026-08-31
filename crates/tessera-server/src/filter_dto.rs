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

use std::sync::Arc;

use serde_json::Value;

use tessera_engine::filter::{Endpoint, Family, FilterExpr, FilterOperand, RegionLeaf, Scalar};
use tessera_engine::LeafColumn;
use tessera_engine::shapes::{Bounds, CanonError, Projection, ShapeF64, ShapeSpace};
use tessera_types::{AttrLocalId, TesseraId};

use crate::error::ApiError;

/// The engine's sentinel for a value that does not resolve — see its doc for why this is an empty
/// operand rather than a refusal. Named there rather than here because it is a property of
/// evaluation, and `check-layers.sh` keeps this crate on engine API types only.
use tessera_engine::filter::UNRESOLVABLE_VALUE as UNRESOLVABLE_ID;
const UNRESOLVABLE: u32 = UNRESOLVABLE_ID.raw();

/// What a `region` leaf is canonicalised against (selection-operand §2, `polygon-membership.md`
/// §4.3–§4.4): the request's view's extent, the transform that placed its points there, and the
/// deployment's vertex cap.
pub struct RegionContext {
    pub extent: Bounds,
    /// **The view's own declared projection** — what a `space = "wgs84"` leaf is put through, and
    /// the same function every point in the view went through (`projections.md` §10).
    pub projection: Projection,
    pub max_vertices: u64,
}

/// Parse `filters` into an expression, or refuse.
///
/// `column_of` resolves a leaf's **spelling** — which may pin a group-scoped attribute's view,
/// `sentiment@2026-Q3` or `sentiment@#3` (`views.md` §5) — to the column the engine evaluates and
/// its family, or reports why it cannot. `resolve` maps `(column, key)` to a code. `region` is
/// what a `region` leaf's geometry is quantised against.
pub fn parse(
    filters: &Value,
    column_of: &dyn Fn(&str) -> LeafColumn,
    resolve: &dyn Fn(&str, &str) -> Option<u32>,
    region: &RegionContext,
) -> Result<FilterExpr, ApiError> {
    parse_node(filters, column_of, resolve, region)
}

/// The column a leaf's spelling names, or the refusal it earns (`views.md` §5).
///
/// **Three refusals, two codes, and the split is contracts §3.1's closed list.** An unknown column
/// and an ambiguous one are both `422`: the caller wrote something this schema cannot answer, and
/// can be told so. A **pin naming nothing** is the `404` an unknown view already gets, and is
/// deliberately the same answer for a key nobody declared, an ordinal no view holds and — when
/// `views.md` §6's gate lands — a view this principal may not reach: a `422` there would make the
/// filter surface an existence oracle over a roster the viewer plane refuses to enumerate.
fn resolve_leaf(leaf: &str, column: LeafColumn) -> Result<(String, Family), ApiError> {
    match column {
        LeafColumn::Resolved { column, family } => Ok((column, family)),
        LeafColumn::Unknown => Err(bad(format!(
            "'{leaf}' is not a filterable column. `/v1/meta`'s `filter_operands` lists \
             the columns and the operators each accepts"
        ))),
        LeafColumn::Unpinned { group } => Err(bad(format!(
            "'{leaf}' is scoped to view group '{group}' and this request's view is not one of \
             its views, so the leaf names no column to read. Pin the view it means — \
             '{leaf}@<key>' or '{leaf}@#<ordinal>' — as `/v1/meta`'s `filter_operands` entry \
             for it says"
        ))),
        LeafColumn::UnknownPin { group, pin } => Err(ApiError::Unknown(format!(
            "unknown view '{pin}' of group '{group}'"
        ))),
        LeafColumn::PinOnUnscoped { column } => Err(bad(format!(
            "'{column}' is not scoped to a view group, so there is nothing for '@{}' to choose \
             between: it is one column for the corpus and every view reads it",
            leaf.split_once(tessera_engine::filter::PIN).map_or("", |(_, pin)| pin)
        ))),
    }
}

fn bad(detail: impl Into<String>) -> ApiError {
    ApiError::Contract(detail.into())
}

fn parse_node(
    node: &Value,
    column_of: &dyn Fn(&str) -> LeafColumn,
    resolve: &dyn Fn(&str, &str) -> Option<u32>,
    region: &RegionContext,
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
                .map(|k| parse_node(k, column_of, resolve, region))
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
        // **The reserved word, before any column** (selection-operand §2): the build refuses a
        // column of this name, so the key can mean one thing.
        tessera_engine::filter::REGION_COLUMN => Ok(FilterExpr::Region(parse_region(body, region)?)),
        leaf => {
            // **An unknown column is an error; an unknown value is not.** See the module header.
            // A leaf's *spelling* is resolved here too — a group-scoped attribute's pin, and the
            // refusals a spelling can earn — because both questions are about the name and this is
            // the boundary where a name becomes a column (`views.md` §5).
            let (column, family) = resolve_leaf(leaf, column_of(leaf))?;
            Ok(FilterExpr::Leaf {
                // The **resolved** column, which for a scoped family is one view's of it. The
                // caller's own spelling stays in the refusals: a message quoting a name the caller
                // never wrote is one they cannot find in their request.
                column,
                operand: parse_operand(leaf, family, body, resolve)?,
            })
        }
    }
}

/// A `region` leaf's body: exactly one of `polygon`, `bbox`, `circle`, `ellipse` — with `space`,
/// `view` if absent — or `artifact` (`polygon-membership.md` §8).
///
/// **Geometry is canonicalised here, at the boundary**, against the view's extent and through the
/// same `fixed32` the tiler applies to a point, so what the engine holds is the grid-unit form
/// and two callers drawing one shape send one value. What refuses: a coordinate that is not one,
/// an inverted box, a non-positive radius or axis, too few vertices, too many (`422` naming the
/// count and the cap), an unknown key, a `wgs84` coordinate outside ±180 × ±90 — which is not a
/// coordinate — and `wgs84` at all on a view whose `projection` is `none`, which has one space
/// and nothing to convert from (§4.3). A shape wholly outside the extent is not refused: it holds
/// no rows, and a request may ask that.
///
/// **A `wgs84` leaf is put through the view's own transform**, each edge densified first, on the
/// rule that the space a shape is declared in defines the plane its edges are straight in
/// (R10, `projections.md` §10). The vertex cap is therefore checked twice: on what the caller
/// sent, and on what densification produced.
fn parse_region(body: &Value, ctx: &RegionContext) -> Result<RegionLeaf, ApiError> {
    let obj = body
        .as_object()
        .ok_or_else(|| bad("`region` takes an object with exactly one of polygon, bbox, circle, ellipse or artifact"))?;
    const KINDS: [&str; 5] = ["polygon", "bbox", "circle", "ellipse", "artifact"];
    let named: Vec<&str> = obj
        .keys()
        .map(String::as_str)
        .filter(|k| KINDS.contains(k))
        .collect();
    if named.len() != 1 {
        return Err(bad(format!(
            "`region` takes exactly one of polygon, bbox, circle, ellipse or artifact; this one \
             carries {}",
            if named.is_empty() {
                "none".to_string()
            } else {
                named.join(", ")
            }
        )));
    }
    for key in obj.keys() {
        if key != named[0] && key != "space" {
            return Err(bad(format!(
                "`region` takes {} and `space`, not '{key}'",
                named[0]
            )));
        }
    }
    let kind = named[0];
    if obj.contains_key("space") && obj.get("space").and_then(Value::as_str).is_none() {
        return Err(bad("`region.space` is a string"));
    }
    let space = match obj.get("space").and_then(Value::as_str) {
        None => ShapeSpace::View,
        Some(word) => ShapeSpace::parse(word)
            .map_err(|_| bad(format!("`region.space` is `view` or `wgs84`, not '{word}'")))?,
    };
    let space = space
        .resolve(ctx.projection)
        .map_err(|e| bad(format!("`region.space`: {e}")))?;
    if kind == "artifact" {
        if obj.contains_key("space") {
            return Err(bad("`region.artifact` names a published shape and carries no `space`"));
        }
        let id = match &obj["artifact"] {
            Value::Number(n) => n.as_u64(),
            Value::String(s) => s.parse::<u64>().ok(),
            _ => None,
        }
        .ok_or_else(|| bad("`region.artifact` is a tessera_id — a whole number, or its decimal string"))?;
        return Ok(RegionLeaf::Artifact(TesseraId::new(id)));
    }
    let number = |v: &Value, what: &str| -> Result<f64, ApiError> {
        v.as_f64()
            .filter(|f| f.is_finite())
            .ok_or_else(|| bad(format!("`region.{kind}`: {what} must be a finite number")))
    };
    let numbers = |n: usize| -> Result<Vec<f64>, ApiError> {
        let arr = obj[kind].as_array().ok_or_else(|| {
            bad(format!("`region.{kind}` takes an array of {n} numbers"))
        })?;
        if arr.len() != n {
            return Err(bad(format!(
                "`region.{kind}` takes {n} numbers, not {}",
                arr.len()
            )));
        }
        arr.iter().map(|v| number(v, "each entry")).collect()
    };
    let shape = match kind {
        "polygon" => {
            let arr = obj["polygon"]
                .as_array()
                .ok_or_else(|| bad("`region.polygon` takes an array of [x, y] vertices"))?;
            if arr.len() < 3 {
                return Err(bad(format!(
                    "`region.polygon` needs at least three vertices; this one has {}",
                    arr.len()
                )));
            }
            if arr.len() as u64 > ctx.max_vertices {
                return Err(bad(format!(
                    "`region.polygon` carries {} vertices; the deployment's `max_region_vertices` \
                     is {} (`/v1/meta`'s selection block). Simplify the shape before sending it",
                    arr.len(),
                    ctx.max_vertices
                )));
            }
            let mut ring = Vec::with_capacity(arr.len());
            for v in arr {
                let pair = v
                    .as_array()
                    .filter(|p| p.len() == 2)
                    .ok_or_else(|| bad("`region.polygon`: each vertex is [x, y]"))?;
                ring.push((number(&pair[0], "x")?, number(&pair[1], "y")?));
            }
            ShapeF64::Polygon(vec![vec![ring]])
        }
        "bbox" => {
            let b = numbers(4)?;
            ShapeF64::Bbox {
                min_x: b[0],
                min_y: b[1],
                max_x: b[2],
                max_y: b[3],
            }
        }
        "circle" => {
            let c = numbers(3)?;
            ShapeF64::Circle {
                cx: c[0],
                cy: c[1],
                r: c[2],
            }
        }
        "ellipse" => {
            let e = numbers(5)?;
            ShapeF64::Ellipse {
                cx: e[0],
                cy: e[1],
                a: e[2],
                b: e[3],
                angle_degrees: e[4],
            }
        }
        _ => unreachable!("the kind was checked against the five"),
    };
    let (canonical, _report) = shape.canonical(space, &ctx.extent).map_err(|e| match e {
        CanonError::NotFinite => bad(format!("`region.{kind}`: a coordinate is not finite")),
        CanonError::InvertedBox => bad(
            "`region.bbox` is [x0, y0, x1, y1] with x0 <= x1 and y0 <= y1",
        ),
        CanonError::NonPositiveAxis => bad(format!(
            "`region.{kind}`: the radius and the axes must be positive"
        )),
        CanonError::NotACoordinate => bad(format!(
            "`region.{kind}` is `space = \"wgs84\"` and carries a coordinate outside ±180 \
             longitude or ±90 latitude; a value outside that is not a coordinate \
             (`projections.md` §2)"
        )),
        // Unreachable: `ShapeSpace::resolve` refuses the pair above, naming the view.
        CanonError::NoProjection => bad(format!("`region.{kind}`: {e}")),
    })?;
    // **The cap again, on what densification produced** (R5): a `wgs84` polygon whose edges curve
    // in the frame leaves this line with more vertices than the caller sent, and a bound checked
    // only on the submission would not be a bound on the evaluation.
    let vertices = canonical.vertex_count();
    if vertices > ctx.max_vertices {
        return Err(bad(format!(
            "`region.{kind}` is {vertices} vertices once its edges are densified for \
             `space = \"wgs84\"`, and the deployment's `max_region_vertices` is {}. Simplify the \
             shape, or send it in the view's own space",
            ctx.max_vertices
        )));
    }
    Ok(RegionLeaf::Shape(Arc::new(canonical)))
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
    use tessera_engine::filter::PIN;

    /// A resolver over four columns, standing in for the engine's own: three entity-scoped, and
    /// `sentiment` scoped to the group `quarter` with the request's view deciding nothing — so the
    /// bare leaf is the unpinned refusal and `sentiment@2026-Q3` resolves.
    fn schema(name: &str) -> impl Fn(&str) -> LeafColumn + '_ {
        move |c: &str| {
            let plain = |family| LeafColumn::Resolved {
                column: c.to_string(),
                family,
            };
            match c.split_once(PIN) {
                None if c == name => plain(Family::Category),
                None if c == "title" || c == "submitter" => plain(Family::Keyword),
                None if c == "score" => plain(Family::Numeric),
                None if c == "sentiment" => LeafColumn::Unpinned {
                    group: "quarter".to_string(),
                },
                Some(("sentiment", "2026-Q3")) => LeafColumn::Resolved {
                    column: "sentiment@quarter:2026-Q3".to_string(),
                    family: Family::Numeric,
                },
                Some(("sentiment", pin)) => LeafColumn::UnknownPin {
                    group: "quarter".to_string(),
                    pin: pin.to_string(),
                },
                Some(("score", _)) => LeafColumn::PinOnUnscoped {
                    column: "score".to_string(),
                },
                _ => LeafColumn::Unknown,
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

    /// An unprojected view: its own coordinates are the only space it has.
    fn region_ctx() -> RegionContext {
        RegionContext {
            extent: Bounds {
                x_min: 0.0,
                x_max: 1000.0,
                y_min: 0.0,
                y_max: 1000.0,
            },
            projection: Projection::None,
            max_vertices: 8,
        }
    }

    /// A Web Mercator view over the whole world — the frame the United Kingdom takes.
    fn projected_ctx() -> RegionContext {
        RegionContext {
            extent: Bounds {
                x_min: 0.0,
                x_max: 1.0,
                y_min: 0.0,
                y_max: 1.0,
            },
            projection: Projection::WebMercator,
            max_vertices: 2_048,
        }
    }

    fn parse_str(text: &str) -> Result<FilterExpr, ApiError> {
        let v: Value = serde_json::from_str(text).unwrap();
        parse(&v, &schema("department"), &codes, &region_ctx())
    }

    fn parse_projected(text: &str) -> Result<FilterExpr, ApiError> {
        let v: Value = serde_json::from_str(text).unwrap();
        parse(&v, &schema("department"), &codes, &projected_ctx())
    }

    #[test]
    fn a_region_polygon_parses_to_its_canonical_shape() {
        let expr = parse_str(r#"{"region": {"polygon": [[0, 0], [500, 0], [500, 500], [0, 500]]}}"#)
            .unwrap();
        let FilterExpr::Region(RegionLeaf::Shape(shape)) = expr else {
            panic!("a region leaf");
        };
        assert_eq!(shape.vertex_count(), 4);
        // The same shape with `space = "view"` is the same value.
        let again = parse_str(
            r#"{"region": {"polygon": [[0, 0], [500, 0], [500, 500], [0, 500]], "space": "view"}}"#,
        )
        .unwrap();
        assert_eq!(again, FilterExpr::Region(RegionLeaf::Shape(shape)));
    }

    #[test]
    fn a_region_bbox_circle_and_ellipse_parse() {
        for text in [
            r#"{"region": {"bbox": [10, 10, 20, 20]}}"#,
            r#"{"region": {"circle": [500, 500, 100]}}"#,
            r#"{"region": {"ellipse": [500, 500, 100, 50, 30]}}"#,
        ] {
            assert!(matches!(
                parse_str(text).unwrap(),
                FilterExpr::Region(RegionLeaf::Shape(_))
            ));
        }
    }

    #[test]
    fn a_wgs84_region_goes_through_the_views_own_transform() {
        // The United Kingdom's diagonal, in degrees, on a Web Mercator view.
        let expr = parse_projected(
            r#"{"region": {"polygon": [[-8, 50], [2, 58], [2, 50]], "space": "wgs84"}}"#,
        )
        .unwrap();
        let FilterExpr::Region(RegionLeaf::Shape(shape)) = expr else {
            panic!("a region leaf");
        };
        // Densified: the diagonal edge is a curve in the frame, so more than three vertices
        // survive canonicalisation.
        assert!(
            shape.vertex_count() > 3,
            "the diagonal was joined by a chord: {} vertices",
            shape.vertex_count()
        );
        // The same numbers read as view coordinates land nowhere near — they are degrees on a
        // unit-square frame, so they clamp to a corner and canonicalise away.
        let as_view =
            parse_projected(r#"{"region": {"polygon": [[-8, 50], [2, 58], [2, 50]]}}"#).unwrap();
        assert_ne!(as_view, FilterExpr::Region(RegionLeaf::Shape(shape)));
    }

    #[test]
    fn a_wgs84_region_refuses_what_is_not_a_coordinate() {
        let err = parse_projected(
            r#"{"region": {"bbox": [-8, 50, 2, 91], "space": "wgs84"}}"#,
        )
        .unwrap_err();
        let ApiError::Contract(detail) = err else {
            panic!("expected a 422");
        };
        assert!(detail.contains("not a coordinate"), "{detail}");
    }

    #[test]
    fn a_wgs84_region_is_capped_on_what_densification_produced() {
        let mut small = projected_ctx();
        small.max_vertices = 8;
        let v: Value = serde_json::from_str(
            r#"{"region": {"polygon": [[-30, -30], [30, 30], [30, -30]], "space": "wgs84"}}"#,
        )
        .unwrap();
        let err = parse(&v, &schema("department"), &codes, &small).unwrap_err();
        let ApiError::Contract(detail) = err else {
            panic!("expected a 422");
        };
        assert!(detail.contains("once its edges are densified"), "{detail}");
    }

    #[test]
    fn a_region_by_artifact_takes_an_integer_or_its_decimal_string() {
        for text in [r#"{"region": {"artifact": 42}}"#, r#"{"region": {"artifact": "42"}}"#] {
            assert_eq!(
                parse_str(text).unwrap(),
                FilterExpr::Region(RegionLeaf::Artifact(TesseraId::new(42)))
            );
        }
    }

    #[test]
    fn a_region_refuses_what_it_must() {
        for (text, needle) in [
            // Two vertices is not a polygon.
            (r#"{"region": {"polygon": [[0, 0], [1, 1]]}}"#, "at least three"),
            // Over the cap, naming the count and the cap.
            (
                r#"{"region": {"polygon": [[0,0],[1,0],[2,0],[3,0],[4,0],[5,0],[6,0],[7,0],[8,0]]}}"#,
                "9 vertices; the deployment's `max_region_vertices` is 8",
            ),
            // Two kinds, or none.
            (r#"{"region": {"bbox": [0,0,1,1], "circle": [0,0,1]}}"#, "exactly one of"),
            (r#"{"region": {}}"#, "exactly one of"),
            // A view with no projection has one space, and says so.
            (r#"{"region": {"bbox": [0,0,1,1], "space": "wgs84"}}"#, "projections.md"),
            (r#"{"region": {"bbox": [0,0,1,1], "space": "utm"}}"#, "`view` or `wgs84`"),
            // The canonical form's own refusals.
            (r#"{"region": {"bbox": [5,5,1,1]}}"#, "x0 <= x1"),
            (r#"{"region": {"circle": [5,5,0]}}"#, "positive"),
            // An unknown key beside the kind.
            (r#"{"region": {"bbox": [0,0,1,1], "depth": 4}}"#, "not 'depth'"),
            // An artifact carries no space.
            (r#"{"region": {"artifact": 1, "space": "view"}}"#, "carries no `space`"),
        ] {
            let err = parse_str(text).unwrap_err();
            let ApiError::Contract(detail) = err else {
                panic!("{text}: expected a 422, got {err:?}");
            };
            assert!(detail.contains(needle), "{text}: {detail}");
        }
    }

    #[test]
    fn a_region_composes_and_negates_like_any_leaf() {
        let expr = parse_str(
            r#"{"all_of": [{"department": {"eq": "eng"}},
                           {"none_of": [{"region": {"bbox": [0, 0, 10, 10]}}]}]}"#,
        )
        .unwrap();
        let FilterExpr::AllOf(kids) = expr else {
            panic!("all_of");
        };
        assert!(matches!(kids[1], FilterExpr::NoneOf(_)));
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

    /// **The pinned leaf's three refusals, each with the code contracts §3.1 gives it**
    /// (`views.md` §5). The resolution itself is the engine's; what is checked here is that the
    /// parse carries each outcome to the wire unflattened — an ambiguous leaf and a pin naming
    /// nothing are different answers, and collapsing them would either publish a roster the
    /// viewer plane refuses to enumerate or hide a malformed request behind a 404.
    #[test]
    fn a_scoped_leaf_resolves_or_refuses_by_its_spelling() {
        // Pinned: the leaf carries the resolved column, not the caller's spelling of it.
        let FilterExpr::Leaf { column, .. } =
            parse_str(r#"{"sentiment@2026-Q3": {"range": {"gte": 0.5}}}"#).unwrap()
        else {
            panic!("a leaf");
        };
        assert_eq!(column, "sentiment@quarter:2026-Q3");

        // Bare, under a view that decides no column: 422 naming the group and the pin forms.
        let ApiError::Contract(detail) =
            parse_str(r#"{"sentiment": {"range": {"gte": 0.5}}}"#).unwrap_err()
        else {
            panic!("expected a 422");
        };
        assert!(detail.contains("quarter"), "{detail}");
        assert!(detail.contains("sentiment@#<ordinal>"), "{detail}");

        // A pin naming no view of the group: the 404 an unknown view gets, and it says no more.
        let ApiError::Unknown(detail) =
            parse_str(r#"{"sentiment@2099-Q9": {"range": {"gte": 0.5}}}"#).unwrap_err()
        else {
            panic!("expected a 404");
        };
        assert!(detail.contains("2099-Q9"), "{detail}");

        // A pin on a column with no scope: one column for the corpus, nothing to choose between.
        let ApiError::Contract(detail) =
            parse_str(r#"{"score@2026-Q3": {"range": {"gte": 1}}}"#).unwrap_err()
        else {
            panic!("expected a 422");
        };
        assert!(detail.contains("not scoped to a view group"), "{detail}");
    }
}
