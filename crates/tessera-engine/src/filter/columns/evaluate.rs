use croaring::Bitmap;
use tessera_filter::{resolve_union, RecordValue};
use tessera_types::AttrLocalId;

use super::{FilterColumns, Layers, Route};
use crate::filter::declared::{Family, Placement};
use crate::filter::error::FilterError;
use crate::filter::expr::{
    FilterExpr, FilterOperand, RoutedFilter, RowExpr, RowLeafResolvers, MAX_FILTER_DEPTH,
    MEMBER_OF_COLUMN, REGION_COLUMN,
};
use crate::filter::scan::text::{contains_phrase, text_match};
use crate::filter::scan::{scan, scan_layer};

/// The space a sub-tree evaluates in — [`FilterColumns::space_of`]'s answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Space {
    Entity,
    Row,
    Mixed,
}

impl FilterColumns {
    /// Entities in `candidate` whose value for `column` satisfies `operand`.
    ///
    /// The result is a subset of `candidate` by
    /// construction, so it is already inside the composed verdict — **I12**'s "a filter narrows
    /// `M_sel` and never widens it" is a property of the shape here rather than a check.
    ///
    /// **Every layer is scanned and the results unioned.** The layers partition entity space, so
    /// the union is disjoint and an entity is tested against exactly one value however many flushes
    /// have published — which is what makes composition a union rather than a precedence rule.
    pub fn resolve(
        &self,
        column: &str,
        operand: &FilterOperand,
        candidate: &Bitmap,
    ) -> Result<Bitmap, FilterError> {
        let name = column;
        let column = self
            .columns
            .get(name)
            .filter(|layers| layers.filterable)
            .ok_or_else(|| FilterError::UndeclaredColumn(name.to_string()))?;

        // **Text answers from postings and nothing else**, because it has nothing else: no value
        // column and no scan. Every layer is asked and the answers are unioned — the base build's
        // index plus one per flush — which is sound because the layers are disjoint in entity space
        // (I9) and each holds its own entities' terms whole.
        if column.family == Family::Text {
            let Some(analyser) = column.analyser.as_ref() else {
                // Unreachable: `open` inserts a text column only with both, and refuses if either
                // is absent. Empty is the fail-safe reading — it narrows.
                return Ok(Bitmap::new());
            };
            // An operator outside this family is refused at the parse gate, which asks
            // `Family::operands` — the same list `/v1/meta` publishes. Empty here is the second
            // line of defence and the direction that narrows.
            let (query, minimum, phrase) = match operand {
                FilterOperand::Match { query, minimum } => (query, *minimum, false),
                FilterOperand::Phrase { query } => (query, None, true),
                // An operator outside this family is refused at the parse gate, which asks
                // `Family::operands` — the same list `/v1/meta` publishes. Empty here is the second
                // line of defence and the direction that narrows.
                _ => return Ok(Bitmap::new()),
            };
            // **Order and duplicates survive the analyser, and a phrase needs both** — which is why
            // the sort and the deduplication happen here, on the copy the postings take, rather
            // than in `Analyser::tokens`.
            let ordered = analyser.tokens(query);
            let mut tokens = ordered.clone();
            tokens.sort();
            tokens.dedup();
            let minimum = minimum.unwrap_or(tokens.len() as u32);
            let mut out = Bitmap::new();
            for layer in &column.text {
                out |= text_match(&layer.dict, &layer.postings, &tokens, minimum, candidate)
                    .map_err(|e| FilterError::PostingsUnreadable {
                        column: name.to_string(),
                        detail: e.to_string(),
                    })?;
            }
            // A one-word phrase *is* a `match`, and short-circuiting it is worth stating: the
            // verify below decompresses a record block per survivor, and for the commonest phrase
            // shape there is nothing for it to establish that the conjunction has not.
            if !phrase || ordered.len() < 2 {
                return Ok(out);
            }
            return self.verify_phrase(name, column, &ordered, out, candidate);
        }

        // **The routed pair, and the split between them is the whole of decision 0063.** The base
        // build's answer comes from the postings; every extent layer is scanned, because no flush
        // writes postings and an answer from the postings alone would omit every entity ingested
        // since the build.
        if let (Route::Postings, Some(postings), Some(values)) =
            (column.route, column.postings.as_ref(), codes_of(operand))
        {
            let mut out = resolve_union(postings, values)
                .map_err(|e| FilterError::PostingsUnreadable {
                    column: name.to_string(),
                    detail: e.to_string(),
                })?
                .and(candidate);
            // Every layer that is *not* the base, which is the one the postings cover. Selected by
            // the absence of a manifest path rather than by position: a coalesce replaces a window
            // of extents with one layer appended at the end, so "the base is layer 0" would hold
            // today and stop holding the first time the list is rewritten.
            for layer in column.layers.iter().filter(|l| l.values_rel.is_some()) {
                out |= scan(&layer.values, operand, candidate);
            }
            return Ok(out);
        }

        let mut out = Bitmap::new();
        for layer in &column.layers {
            out |= scan_layer(name, layer, operand, candidate)?;
        }
        Ok(out)
    }

    /// Compose several operands: `candidate ∧ op₁ ∧ … ∧ opₙ`.
    ///
    /// **Intersection is the only top-level operator** (§8.2), and each operand is evaluated under
    /// the *running* result rather than under the original candidate — so a selective first operand
    /// makes every later one cheaper, and no intermediate is ever wider than the candidate.
    pub fn resolve_all<'a>(
        &self,
        operands: impl IntoIterator<Item = (&'a str, &'a FilterOperand)>,
        candidate: &Bitmap,
    ) -> Result<Bitmap, FilterError> {
        let mut live = candidate.clone();
        for (column, operand) in operands {
            live = self.resolve(column, operand, &live)?;
            if live.is_empty() {
                break;
            }
        }
        Ok(live)
    }

    /// Evaluate a filter expression against `candidate`.
    ///
    /// **Every node is evaluated under the candidate, never over the column at large.** A
    /// conjunction narrows the candidate as it goes, so a selective first clause makes the rest
    /// cheaper; a disjunction evaluates each branch under the *original* candidate and unions —
    /// which is what keeps `any_of` a subset of it, since each branch already is.
    pub fn evaluate(&self, expr: &FilterExpr, candidate: &Bitmap) -> Result<Bitmap, FilterError> {
        let depth = expr.depth();
        if depth > MAX_FILTER_DEPTH {
            return Err(FilterError::TooDeep {
                depth,
                max: MAX_FILTER_DEPTH,
            });
        }
        expr.check_negations()?;
        self.eval(expr, candidate)
    }

    /// Evaluate a filter expression's entity-space part and route the rest — the seam decision
    /// 0068 admits (this module's header carries the full argument).
    ///
    /// `prefer_row` decides a both-routes column's leaf: the caller derives it from 0068's rule
    /// (`rows_in_ranges ≤ |M_auth|`), which is a per-request quantity, so it arrives as an
    /// argument rather than being stored at open — unlike [`Route`], which must not vary per
    /// request because the postings' work is a function of the value named. This preference
    /// carries no such channel: both routes' work is a function of the request's shape and the
    /// mask, never of the value (placement memo §2).
    ///
    /// A tree with no row-space leaf returns [`RoutedFilter::Entity`], evaluated exactly as
    /// [`FilterColumns::evaluate`] would have. Otherwise every maximal entity-space sub-tree is
    /// evaluated **here, under `candidate`** — the composed verdict — and the returned
    /// [`RowExpr`] awaits the one crossing and the row-space leaves, which need the request's
    /// tile ranges and so live in `viewport.rs`.
    ///
    /// `resolvers` answers each region leaf (selection-operand §5) and each `member_of` leaf
    /// (`highlight-and-hierarchy.md` §3): both are always row space and always the whole view, so
    /// a tree carrying either never returns [`RoutedFilter::Entity`].
    pub fn evaluate_routed(
        &self,
        expr: &FilterExpr,
        candidate: &Bitmap,
        prefer_row: bool,
        resolvers: &RowLeafResolvers<'_>,
    ) -> Result<RoutedFilter, FilterError> {
        let depth = expr.depth();
        if depth > MAX_FILTER_DEPTH {
            return Err(FilterError::TooDeep {
                depth,
                max: MAX_FILTER_DEPTH,
            });
        }
        expr.check_negations()?;
        if self.space_of(expr, prefer_row)? == Space::Entity {
            return Ok(RoutedFilter::Entity(self.eval(expr, candidate)?));
        }
        Ok(RoutedFilter::Row(
            self.route(expr, candidate, prefer_row, resolvers)?,
        ))
    }

    /// One column's routed space — **the single transcription of the leaf-routing rule**, called
    /// for a leaf and for a `none_of`'s one column alike, so the two cannot drift.
    fn leaf_space(&self, column: &str, prefer_row: bool) -> Result<Space, FilterError> {
        // The reserved words name no column: a region and a `member_of` are row space whatever
        // the request's span.
        if column == REGION_COLUMN || column == MEMBER_OF_COLUMN {
            return Ok(Space::Row);
        }
        let placement = self.placement_of(column)?;
        Ok(match (placement.entity, placement.row) {
            (true, false) => Space::Entity,
            (false, true) => Space::Row,
            // Both routes: 0068's rule, carried in by the caller.
            (true, true) if prefer_row => Space::Row,
            (true, true) => Space::Entity,
            // Never inserted — `open` only stores a placement with at least one space.
            (false, false) => unreachable!("a placement affords at least one space"),
        })
    }

    /// One filterable column's placement, refused exactly as [`FilterColumns::resolve`] refuses an
    /// undeclared one.
    fn placement_of(&self, column: &str) -> Result<Placement, FilterError> {
        self.placements
            .get(column)
            .copied()
            .ok_or_else(|| FilterError::UndeclaredColumn(column.to_string()))
    }

    /// Which space `expr` evaluates in, given each leaf's placement and the request's preference.
    ///
    /// `Entity` means the whole sub-tree can be answered by the existing entity-space evaluation;
    /// anything else means at least one leaf must be tested against the hot column. A `none_of`
    /// takes its single column's space whole — `check_negations` has already established there is
    /// exactly one — because its presence half and its matched half must be computed in the same
    /// space or the subtraction would mix domains.
    fn space_of(&self, expr: &FilterExpr, prefer_row: bool) -> Result<Space, FilterError> {
        match expr {
            FilterExpr::Leaf { column, .. } => self.leaf_space(column, prefer_row),
            FilterExpr::Region(_) | FilterExpr::MemberOf(_) => Ok(Space::Row),
            FilterExpr::AllOf(kids) | FilterExpr::AnyOf(kids) => {
                let mut all_entity = true;
                for kid in kids {
                    if self.space_of(kid, prefer_row)? != Space::Entity {
                        all_entity = false;
                    }
                }
                // An empty combinator is pure entity space: its identity value needs no hot
                // column (`AllOf([])` is the candidate, `AnyOf([])` is empty).
                Ok(if all_entity {
                    Space::Entity
                } else {
                    Space::Mixed
                })
            }
            FilterExpr::NoneOf(kids) => {
                let column = kids
                    .iter()
                    .flat_map(|kid| kid.columns())
                    .next()
                    .expect("check_negations admits exactly one column");
                self.leaf_space(column, prefer_row)
            }
        }
    }

    /// Build the routed tree: entity-pure sub-trees evaluated to verdicts under `candidate`,
    /// row-space leaves carried through for the per-tile evaluation.
    fn route(
        &self,
        expr: &FilterExpr,
        candidate: &Bitmap,
        prefer_row: bool,
        resolvers: &RowLeafResolvers<'_>,
    ) -> Result<RowExpr, FilterError> {
        if self.space_of(expr, prefer_row)? == Space::Entity {
            return Ok(RowExpr::Entity(self.eval(expr, candidate)?));
        }
        match expr {
            FilterExpr::Leaf { column, operand } => Ok(RowExpr::Leaf {
                column: column.clone(),
                family: self.placement_of(column)?.family,
                operand: operand.clone(),
            }),
            FilterExpr::Region(leaf) => Ok(RowExpr::Region((resolvers.regions)(leaf)?)),
            FilterExpr::MemberOf(leaf) => Ok(RowExpr::MemberOf((resolvers.members)(leaf)?)),
            FilterExpr::AllOf(kids) => Ok(RowExpr::AllOf(
                kids.iter()
                    .map(|kid| self.route(kid, candidate, prefer_row, resolvers))
                    .collect::<Result<Vec<_>, _>>()?,
            )),
            FilterExpr::AnyOf(kids) => Ok(RowExpr::AnyOf(
                kids.iter()
                    .map(|kid| self.route(kid, candidate, prefer_row, resolvers))
                    .collect::<Result<Vec<_>, _>>()?,
            )),
            FilterExpr::NoneOf(kids) => {
                let column = kids
                    .iter()
                    .flat_map(|kid| kid.columns())
                    .next()
                    .expect("check_negations admits exactly one column")
                    .to_string();
                let routed = kids
                    .iter()
                    .map(|kid| self.route(kid, candidate, prefer_row, resolvers))
                    .collect::<Result<Vec<_>, _>>()?;
                if column == REGION_COLUMN || column == MEMBER_OF_COLUMN {
                    return Ok(RowExpr::NotInRows(routed));
                }
                let family = self.placement_of(&column)?.family;
                Ok(RowExpr::NoneOf {
                    column,
                    family,
                    kids: routed,
                })
            }
        }
    }

    /// Keep only the entities whose prose actually carries the phrase, by reading it (§4.5).
    ///
    /// # Why the answer is exact, and why it costs no storage
    ///
    /// The postings answer *which items use all these words*, which is a superset of *which items
    /// say them in this order and adjacent*: an item cannot carry the phrase without carrying every
    /// word in it, so the conjunction is an over-approximation that is never short. This pass then
    /// reads each survivor's own prose out of the record blob, runs it through the **same analyser
    /// the index was built with**, and looks for the query's token sequence contiguously in the
    /// document's. Same pipeline both sides, so a phrase is found exactly where the words the index
    /// holds are adjacent — no positional payload, no bigram terms, no second artefact.
    ///
    /// The refuted alternative is worth naming at the site, because it is the one a reader reaches
    /// for: **token-bigram terms cost 61.7 B/entity over 2.4M titles, 2.7× the whole unigram index
    /// they would sit beside** (`probes/2026-08-12-string-storage/`). They are not to be
    /// re-derived.
    ///
    /// # What it costs, and the class that cost belongs to
    ///
    /// One record-block decompression and one analysis per **surviving** entity — result-bound, not
    /// corpus-bound. Measured at 236–270 µs per block through the built reader on selective
    /// phrases. It is unbounded on an unselective one — `of the` survives the conjunction almost
    /// everywhere, so almost every visible item is read — which is the same accepted class as
    /// `filter-index.md` §2.2's unselective predicates and not a new one: the work is a function of
    /// the candidate and the query, both of which the caller already holds.
    ///
    /// # The gate, discharged rather than assumed
    ///
    /// The verify decompresses a block on behalf of each survivor, so a survivor outside the
    /// candidate would be a block read for an entity this principal may not see. `text_match`
    /// intersects with the candidate as it reads each posting, so this cannot happen — and it is
    /// checked anyway, before a single block is touched, because "cannot happen" is what an
    /// intersection moved one line would make false with no other symptom.
    fn verify_phrase(
        &self,
        name: &str,
        column: &Layers,
        phrase: &[String],
        survivors: Bitmap,
        candidate: &Bitmap,
    ) -> Result<Bitmap, FilterError> {
        if survivors.andnot(candidate).cardinality() != 0 {
            return Err(FilterError::PostingsUnreadable {
                column: name.to_string(),
                detail: "the phrase conjunction named an entity outside the candidate, so the                          verify would decompress a record block on behalf of an item this                          principal may not see"
                    .to_string(),
            });
        }
        let Some(analyser) = column.analyser.as_ref() else {
            return Ok(Bitmap::new());
        };
        let tag = u16::try_from(column.declared_index).unwrap_or(u16::MAX);
        let mut out = Bitmap::new();
        for entity in survivors.iter() {
            // **A row that will not read excludes the item rather than refusing the request.** The
            // conjunction has already established that the item's words are in the index, so a
            // missing or malformed blob row is a bundle defect — but the fail-closed reading of it
            // here is exclusion, which narrows, where drill-down's is a refusal because it is about
            // to *serve* the row. Two different questions about the same bytes.
            let Ok(Some(fields)) = self.records.fields_of(entity) else {
                continue;
            };
            let Some(RecordValue::Utf8(prose)) =
                fields.into_iter().find(|f| f.tag == tag).map(|f| f.value)
            else {
                continue;
            };
            if contains_phrase(&analyser.tokens(&prose), phrase) {
                out.add(entity);
            }
        }
        Ok(out)
    }

    /// The entities of `candidate` that carry a value in `column` — the presence half of a
    /// negation, unioned across the layers exactly as a scan is.
    ///
    /// **A scan of every layer, never the postings**, even for a column whose `eq` is routed
    /// (decision 0063). Presence derived from postings would be a union over every code in the
    /// vocabulary — O(values) file reads to answer a question the value column answers in one
    /// intersection per layer — and it would answer it only for the base, since no flush writes
    /// postings.
    fn present_in(&self, column: &str, candidate: &Bitmap) -> Result<Bitmap, FilterError> {
        let layers = self
            .columns
            .get(column)
            .filter(|layers| layers.filterable)
            .ok_or_else(|| FilterError::UndeclaredColumn(column.to_string()))?;
        // **A column with no value column has no presence set, and answering the empty one is a
        // wrong answer wearing a right one's clothes.** `Layers::layers` holds value columns; a
        // `text` column has none — its index is postings over words and its prose is a blob row —
        // so this loop would union nothing and every negation over it would return no entities, for
        // every principal and every corpus, with a 200. Narrowing, so no disclosure; simply wrong,
        // and silently.
        //
        // Refused rather than answered from the postings. The presence a negation needs is *carries
        // a value*, and for prose that is not derivable from the index: text analysing to no terms
        // at all — an empty string, a line of punctuation — carries a value and appears in no
        // posting. A flush extent stores a presence bitmap for exactly that reason; the base build
        // writes none, so there is nothing to answer from until it does.
        if layers.layers.is_empty() {
            return Err(FilterError::NegationWithoutPresence {
                column: column.to_string(),
                family: layers.family.as_str().to_string(),
            });
        }
        let mut out = Bitmap::new();
        for layer in &layers.layers {
            out |= layer.values.present_in(candidate);
        }
        Ok(out)
    }

    fn eval(&self, expr: &FilterExpr, candidate: &Bitmap) -> Result<Bitmap, FilterError> {
        match expr {
            FilterExpr::Leaf { column, operand } => self.resolve(column, operand, candidate),
            FilterExpr::Region(_) => Err(FilterError::RegionInEntitySpace),
            FilterExpr::MemberOf(_) => Err(FilterError::MemberOfInEntitySpace),
            FilterExpr::AllOf(kids) => {
                let mut live = candidate.clone();
                for kid in kids {
                    live = self.eval(kid, &live)?;
                    if live.is_empty() {
                        break;
                    }
                }
                Ok(live)
            }
            FilterExpr::AnyOf(kids) => {
                let mut out = Bitmap::new();
                for kid in kids {
                    out |= self.eval(kid, candidate)?;
                }
                Ok(out)
            }
            // `present ∖ matched`, and `present` is what makes this a positive predicate — see
            // `FilterExpr::NoneOf` for the two arguments that rest on it.
            //
            // `check_negations` has already established that the sub-expressions name exactly one
            // column, so this cannot pick the wrong one.
            FilterExpr::NoneOf(kids) => {
                let column = kids
                    .iter()
                    .flat_map(|kid| kid.columns())
                    .next()
                    .expect("check_negations admits exactly one column");
                let mut out = self.present_in(column, candidate)?;
                for kid in kids {
                    // Under `out`, not `candidate`: each clause need only be evaluated over what is
                    // still standing, so a selective first clause makes the rest cheaper — the same
                    // narrowing `AllOf` does, for the same reason.
                    out.andnot_inplace(&self.eval(kid, &out)?);
                    if out.is_empty() {
                        break;
                    }
                }
                Ok(out)
            }
        }
    }
}

/// The vocabulary codes an operand names, or `None` where the operand is not a category one.
///
/// The match is on the operand's *shape*, never on the values it carries: a category column reaches
/// the postings for `eq` and `in` alike and for nothing else, so the route cannot become a function
/// of which value was asked for.
fn codes_of(operand: &FilterOperand) -> Option<&[AttrLocalId]> {
    match operand {
        FilterOperand::Equals(v) => Some(std::slice::from_ref(v)),
        FilterOperand::In(vs) => Some(vs),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------
// The keyword route: a dictionary per layer, an ordinal question per operand
// ---------------------------------------------------------------------------------------------
