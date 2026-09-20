use croaring::Bitmap;
use tessera_filter::{resolve_union, RecordValue};
use tessera_types::AttrLocalId;

use super::{ColumnLayers, FilterColumns, Route, TextLayer};
use crate::filter::declared::Placement;
use crate::filter::error::FilterError;
use crate::filter::expr::{
    FilterExpr, FilterOperand, RoutedFilter, RowExpr, RowLeafResolvers, MEMBER_OF_COLUMN,
    REGION_COLUMN,
};
use crate::filter::scan::text::{contains_phrase, text_match};
use crate::filter::scan::{scan, scan_layer};

/// The space a sub-tree evaluates in: [`FilterColumns::space_of`]'s answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Space {
    Entity,
    Row,
    Mixed,
}

impl FilterColumns {
    /// Entities in `candidate` whose value for `column` satisfies `operand`, a subset of
    /// `candidate` by construction. Every layer is scanned and the results unioned; the layers
    /// partition entity space, so an entity is tested against exactly one value.
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
            .filter(|column| column.filterable)
            .ok_or_else(|| FilterError::UndeclaredColumn(name.to_string()))?;

        // Text answers from postings only: no value column and no scan.
        let (layers, postings, route) = match &column.layers {
            ColumnLayers::Text { layers, analyser } => {
                let declared_index = column.declared_index;
                return self
                    .resolve_text(name, declared_index, layers, analyser, operand, candidate);
            }
            ColumnLayers::Values {
                layers,
                postings,
                route,
                ..
            } => (layers, postings.as_deref(), *route),
        };

        // The base build's answer comes from the postings; every extent layer is scanned, since
        // no flush writes postings.
        if let (Route::Postings, Some(postings), Some(values)) =
            (route, postings, codes_of(operand))
        {
            let mut out = resolve_union(postings, values)
                .map_err(|e| FilterError::postings_unreadable(name, e))?
                .and(candidate);
            // Selected by the absence of a manifest path, not position: a coalesce appends its
            // replacement at the end of the list.
            for layer in layers.iter().filter(|l| l.values_rel.is_some()) {
                out |= scan(&layer.values, operand, candidate);
            }
            return Ok(out);
        }

        let mut out = Bitmap::new();
        for layer in layers {
            out |= scan_layer(name, layer, operand, candidate)?;
        }
        Ok(out)
    }

    /// A `match` or a `phrase` over one text column's layers: [`FilterColumns::resolve`]'s text
    /// half.
    fn resolve_text(
        &self,
        name: &str,
        declared_index: usize,
        text: &[TextLayer],
        analyser: &tessera_analyse::Analyser,
        operand: &FilterOperand,
        candidate: &Bitmap,
    ) -> Result<Bitmap, FilterError> {
        let (query, minimum, phrase) = match operand {
            FilterOperand::Match { query, minimum } => (query, *minimum, false),
            FilterOperand::Phrase { query } => (query, None, true),
            _ => return Ok(Bitmap::new()),
        };
        // `ordered` keeps the original order and duplicates, which a phrase needs.
        let ordered = analyser.tokens(query);
        let mut tokens = ordered.clone();
        tokens.sort();
        tokens.dedup();
        let minimum = minimum.unwrap_or(tokens.len() as u32);
        let mut out = Bitmap::new();
        for layer in text {
            out |= text_match(&layer.dict, &layer.postings, &tokens, minimum, candidate)
                .map_err(|e| FilterError::postings_unreadable(name, e))?;
        }
        // A one-word phrase is a `match`, so short-circuit it before the verify decompresses a
        // block per survivor.
        if !phrase || ordered.len() < 2 {
            return Ok(out);
        }
        self.verify_phrase(name, declared_index, analyser, &ordered, out, candidate)
    }

    /// Evaluate a filter expression against `candidate`.
    ///
    /// Every node is evaluated under the candidate: a conjunction narrows it as it goes, and a
    /// disjunction evaluates each branch under the original candidate and unions.
    pub fn evaluate(&self, expr: &FilterExpr, candidate: &Bitmap) -> Result<Bitmap, FilterError> {
        expr.check()?;
        self.eval(expr, candidate)
    }

    /// Evaluate a filter expression's entity-space part and route the rest.
    ///
    /// `prefer_row` decides a both-routes column's leaf: a per-request quantity, so it arrives as
    /// an argument rather than being stored at open, unlike [`Route`], which must not vary per
    /// request since the postings' work is a function of the value named.
    ///
    /// A tree with no row-space leaf returns [`RoutedFilter::Entity`]. Otherwise every maximal
    /// entity-space sub-tree is evaluated here, under `candidate`, and the returned [`RowExpr`]
    /// awaits the row-space leaves, which need the request's tile ranges.
    pub fn evaluate_routed(
        &self,
        expr: &FilterExpr,
        candidate: &Bitmap,
        prefer_row: bool,
        resolvers: &RowLeafResolvers<'_>,
    ) -> Result<RoutedFilter, FilterError> {
        expr.check()?;
        if self.space_of(expr, prefer_row)? == Space::Entity {
            return Ok(RoutedFilter::Entity(self.eval(expr, candidate)?));
        }
        Ok(RoutedFilter::Row(
            self.route(expr, candidate, prefer_row, resolvers)?,
        ))
    }

    /// One column's routed space, the single transcription of the leaf-routing rule.
    fn leaf_space(&self, column: &str, prefer_row: bool) -> Result<Space, FilterError> {
        if column == REGION_COLUMN || column == MEMBER_OF_COLUMN {
            return Ok(Space::Row);
        }
        let placement = self.placement_of(column)?;
        Ok(match (placement.entity, placement.row) {
            (true, false) => Space::Entity,
            (false, true) => Space::Row,
            (true, true) if prefer_row => Space::Row,
            (true, true) => Space::Entity,
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

    /// Which space `expr` evaluates in. `Entity` means the whole sub-tree can be answered by the
    /// existing entity-space evaluation. A `none_of` takes its single column's space whole, since
    /// its presence half and its matched half must be computed in the same space.
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
                Ok(if all_entity {
                    Space::Entity
                } else {
                    Space::Mixed
                })
            }
            FilterExpr::NoneOf(kids) => {
                let column = FilterExpr::negated_column(kids);
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
                let column = FilterExpr::negated_column(kids).to_string();
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

    /// Keep only the entities whose prose actually carries the phrase, by reading it.
    ///
    /// The postings answer which items use all these words, a superset of which items say them
    /// adjacent and in order, so the conjunction over-approximates but is never short. This pass
    /// reads each survivor's own prose out of the record blob, runs it through the same analyser
    /// the index was built with, and looks for the query's token sequence contiguously, with no
    /// positional payload, no bigram terms, no second artefact (bigram terms were measured at
    /// 61.7 B/entity over 2.4M titles, 2.7 times the unigram index).
    ///
    /// One record-block decompression and one analysis per surviving entity, result-bound, not
    /// corpus-bound: 236 to 270 µs per block on selective phrases, unbounded on an unselective
    /// one since `of the` survives the conjunction almost everywhere.
    ///
    /// A survivor outside the candidate would be a block read for an entity this principal may
    /// not see. `text_match` intersects with the candidate as it reads each posting, so this
    /// cannot happen, and it is checked anyway before a single block is touched.
    fn verify_phrase(
        &self,
        name: &str,
        declared_index: usize,
        analyser: &tessera_analyse::Analyser,
        phrase: &[String],
        survivors: Bitmap,
        candidate: &Bitmap,
    ) -> Result<Bitmap, FilterError> {
        if survivors.andnot(candidate).cardinality() != 0 {
            return Err(FilterError::postings_unreadable(
                name,
                "the phrase conjunction named an entity outside the candidate",
            ));
        }
        let tag = u16::try_from(declared_index).unwrap_or(u16::MAX);
        let mut out = Bitmap::new();
        for entity in survivors.iter() {
            // A row that will not read excludes the item rather than refusing the request: the
            // fail-closed reading narrows, where drill-down's is a refusal because it is about to
            // serve the row.
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

    /// The entities of `candidate` that carry a value in `column`, the presence half of a
    /// negation, unioned across the layers exactly as a scan is.
    ///
    /// A scan of every layer, never the postings, even for a column whose `eq` is routed: the
    /// value column answers this in one intersection per layer, where the postings answer only
    /// for the base, since no flush writes postings.
    fn present_in(&self, column: &str, candidate: &Bitmap) -> Result<Bitmap, FilterError> {
        let held = self
            .columns
            .get(column)
            .filter(|held| held.filterable)
            .ok_or_else(|| FilterError::UndeclaredColumn(column.to_string()))?;
        // A column with no value column has no presence set: a `text` column has no value layers
        // at all, and a running-service column has none until its first flush. Answering the
        // empty set would under-report every negation silently, so it is refused instead.
        let layers = held.value_layers();
        if layers.is_empty() {
            return Err(FilterError::NegationWithoutPresence {
                column: column.to_string(),
                family: held.family.as_str().to_string(),
            });
        }
        let mut out = Bitmap::new();
        for layer in layers {
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
            // `present ∖ matched`: `NoneOf` carries a value and none of these match, never the
            // complement, so an unreadable value must narrow rather than widen.
            FilterExpr::NoneOf(kids) => {
                let column = FilterExpr::negated_column(kids);
                let mut out = self.present_in(column, candidate)?;
                for kid in kids {
                    // Under `out`, not `candidate`, the same narrowing `AllOf` does.
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
/// The match is on the operand's shape, never on the values it carries, so the route cannot
/// become a function of which value was asked for.
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
