use std::collections::BTreeSet;
use std::sync::Arc;

use croaring::Bitmap;
use tessera_types::AttrLocalId;

use super::{Endpoint, Family, FilterError, Scalar};

/// What a request may ask of one column.
///
/// `PartialEq` but not `Eq`: a float bound may be NaN, which is not equal to itself. That is the
/// same IEEE rule that makes NaN match no range, so the type reflects it rather than papering over
/// it with a total-equality shim.
///
/// **Narrow on purpose.** The leak register is exhaustive because the query surface is enumerable
/// (§8.2), so a new operand is a design change with a per-family soundness statement, not an
/// addition here. Negation is absent for that reason and not by oversight: no corpus document
/// defines it, it is principal-dependent by construction, and it inverts a superset into a subset.
#[derive(Debug, Clone, PartialEq)]
pub enum FilterOperand {
    /// A category code. One value.
    Equals(AttrLocalId),
    /// A set of category codes — evaluated in one pass, so an IN-set naming ten invisible values
    /// costs what one naming ten visible values costs.
    In(Vec<AttrLocalId>),
    /// Exact string match against the stored bytes.
    TextEquals(String),
    /// Stored value equals any of these — `eq` over a list, the same generalisation `In` is for a
    /// category. The two do not cross: a category's set is codes, a string's is strings.
    TextIn(Vec<String>),
    /// Stored value starts with this.
    TextPrefix(String),
    /// Stored value contains this. Needs no trigram index: the value column *is* the verification
    /// route a trigram superset would have had to be checked against.
    TextContains(String),
    /// **`match`, and its m-of-n form in one operand** (records §4.4). `tokens` are the analyser's
    /// output over what the caller typed — analysed by the *column's* analyser, so the query and
    /// the index agree on segmentation — and `minimum` is how many must appear.
    ///
    /// Plain `match` is `minimum == tokens.len()`; `minimum_should_match` names a smaller number.
    /// One operand rather than two because they are one evaluation with a different threshold, and
    /// two would let the counting union and the intersection drift apart.
    ///
    /// **Duplicates collapse and order is dropped** when the query is analysed: a repeated token
    /// says nothing a set does not, and letting it raise the denominator would make `match` of
    /// `"the the"` stricter than `match` of `"the"` over the same field.
    ///
    /// **The query is carried unanalysed and the engine analyses it**, with the column's *own*
    /// analyser, taken from the identity the manifest recorded when the index was built. That is
    /// the whole correctness property of this operand: a query segmented by one pipeline against an
    /// index segmented by another matches on precisely the strings where they differ, with no error
    /// anywhere. Analysing at the wire would put the choice in a second place, free to drift.
    Match { query: String, minimum: Option<u32> },
    /// **Exact phrase**: the query's tokens appear in the field *in this order and adjacent*.
    ///
    /// Carried unanalysed for [`FilterOperand::Match`]'s reason, and answered in two stages
    /// (`records-and-search.md` §4.5). The postings narrow: an item can only carry the phrase if it
    /// carries every word in it, so the conjunction inside the candidate is an exact
    /// over-approximation of the answer. Then each survivor's own prose is read out of the record
    /// blob, re-analysed, and its token sequence searched for the query's — which is what makes the
    /// answer *exact* rather than a word-bag approximation, and what makes the operand cost **no
    /// storage at all**.
    ///
    /// **Token-bigram terms are refuted and must not be re-derived**: 3.8M distinct pairs over 2.4M
    /// titles at 61.7 B/entity, 2.7× the unigram index they would sit beside
    /// (`probes/2026-08-12-string-storage/`). The verify buys exactness for zero bytes; the index
    /// would buy latency for a multiple of the family's whole storage cost.
    Phrase { query: String },
    /// Numeric equality.
    NumEquals(Scalar),
    /// Numeric set membership.
    NumIn(Vec<Scalar>),
    /// A numeric range. Either endpoint may be absent, which is an open side; both absent matches
    /// every entity **carrying a value**, which is not every entity.
    Range {
        lo: Option<Endpoint>,
        hi: Option<Endpoint>,
    },
}

/// The code a predicate names when its value does not resolve.
///
/// **An unresolvable value is an empty operand, never a refusal** (`filter-surface.md` §2.1).
/// Refusing would say the value exists, which is exactly what `visibility = "derived"` hides — so a
/// filter naming a value the principal may not see must be answered, and answered with nothing.
///
/// Code 0 is the vocabulary's reserved *absent* sentinel: never drawn, never bound to a key. That
/// makes it the correct answer rather than a convenient one — an item carrying no value must not
/// match a filter naming some other value either, and this is the code such an item carries.
pub const UNRESOLVABLE_VALUE: AttrLocalId = AttrLocalId::new(0);

/// The leaf name a region leaf answers to in [`FilterExpr::columns`] — the one word a column may
/// not be called, refused at the build as `all_of`/`any_of`/`none_of` are (selection-operand §2).
pub const REGION_COLUMN: &str = "region";

/// The `region` leaf's two spellings (selection-operand §2; `polygon-membership.md` §8).
///
/// **A row-space operand over the whole view, and the one leaf that carries no authorisation.**
/// A shape sent as geometry arrives already canonical — quantised to the view's grid at the wire —
/// so the same shape from two callers is one value, one cache entry and one row set. A published
/// shape is named by its `tessera_id` and answered from its held membership, under the artifact's
/// own verdict: an artifact this principal would not be served is an **empty operand**, and so is
/// an id that names nothing, one on another view, one whose layer draws an authored shape, or one
/// below this principal's criterion — one answer, indistinguishable by construction (C17).
#[derive(Debug, Clone, PartialEq)]
pub enum RegionLeaf {
    /// A box, circle, ellipse or polygon in its canonical grid-unit form.
    Shape(Arc<tessera_spatial::shape::Shape>),
    /// A published artifact's membership.
    Artifact(tessera_types::TesseraId),
}

/// The leaf name a `member_of` leaf answers to in [`FilterExpr::columns`] — reserved at the build
/// exactly as `region` is (`highlight-and-hierarchy.md` §3).
pub const MEMBER_OF_COLUMN: &str = "member_of";

/// The `member_of` leaf: one artifact of one layer, resolved to `membership ∩ M_auth` in row space
/// over the whole view (`highlight-and-hierarchy.md` §3).
///
/// **The layer is deployment schema and the artifact is a value**, which is what settles the two
/// different refusals: a layer this principal does not reach is [`FilterError::UnknownLayer`], a
/// `422` on `contracts.md` §3.2's unknown-column rule, while an identifier that names nothing —
/// or an artifact below this principal's own existence criterion, or suppressed, or of another
/// layer — is the **empty operand**. Answering `422` to the second would make the leaf an
/// existence oracle over exactly what the criterion withholds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberOfLeaf {
    pub layer: String,
    pub artifact: tessera_types::TesseraId,
}

/// A filter expression: a leaf predicate over one column, or a combinator over sub-expressions.
///
/// **Any boolean combination, evaluated inside the candidate** (decision 0062). Every node returns a
/// subset of the candidate — a leaf does, and union and intersection of subsets are subsets — so
/// **I12**'s "a filter narrows `M_sel` and never widens it" is a property of the shape rather than a
/// check, and no expression can name a set outside the principal's own mask.
///
/// `NoneOf` is deliberately absent: it needs the `derived` rule decision 0062 records — negation
/// over a gated category must be evaluated *within the visible vocabulary*, or it becomes an
/// existence oracle over the values `visibility` hides.
#[derive(Debug, Clone, PartialEq)]
pub enum FilterExpr {
    /// One column's predicate.
    Leaf {
        column: String,
        operand: FilterOperand,
    },
    /// A region — a drawn shape or a published one — as a set of rows over the whole view
    /// (selection-operand §5). Row space only: [`crate::filter::FilterColumns::evaluate`] refuses it, and
    /// [`crate::filter::FilterColumns::evaluate_routed`] resolves it through the caller's resolver, which is
    /// where the mask, the segments and the cache live.
    Region(RegionLeaf),
    /// One artifact's membership, as a set of rows over the whole view
    /// (`highlight-and-hierarchy.md` §3). Row space only, for [`FilterExpr::Region`]'s reason, and
    /// resolved through the caller's resolver, which holds the mask, the layer registry and the
    /// artifact's own existence criterion.
    MemberOf(MemberOfLeaf),
    /// Every sub-expression must match. Empty matches the whole candidate — the identity, and what
    /// an absent filter means.
    AllOf(Vec<FilterExpr>),
    /// At least one sub-expression must match. **Empty matches nothing**, which is the identity for
    /// union and not an oversight: `any_of: []` asks for items matching one of no alternatives.
    AnyOf(Vec<FilterExpr>),
    /// The entity **carries a value in this column** and no sub-expression matches it.
    ///
    /// **Not the complement, and the difference is the whole of why a negation is expressible at
    /// all.** `candidate ∖ any_of(kids)` would put every entity carrying *no* value into the result,
    /// and two separate arguments break at once if it did:
    ///
    /// - **Positivity** (filter-index §5). Every "this failure degrades safely under **I12**"
    ///   argument in the design holds because each operand is positive: an entity whose value is
    ///   unreachable — not yet flushed, in a layer that failed to compose, blanked at the fold —
    ///   matches nothing, so a lost value under-reports and under-reporting narrows. Under a
    ///   complement those same failures *widen*. Requiring presence keeps the sign: a value that
    ///   cannot be read is not a value that fails the predicate.
    /// - **C11**, decision 0062's existence oracle. `none_of: [every value I was offered]` returning
    ///   a non-empty set would prove there exist values the principal was not shown. It cannot here:
    ///   evaluation is inside the candidate, and an entity in the candidate carrying value *v* is
    ///   itself the witness that makes *v* visible under C11's derivation, so *v* was offered. The
    ///   set is empty by construction rather than by an extra intersection.
    ///
    /// **Every sub-expression must name one and the same column**, refused otherwise
    /// ([`FilterError::NegationSpansColumns`]) — which costs no expressiveness, because
    /// `all_of: [none_of: [A], none_of: [B]]` is exactly the multi-column reading and says which
    /// presence it requires.
    NoneOf(Vec<FilterExpr>),
}

/// How deep a filter expression may nest.
///
/// Unbounded depth is unbounded per-request work from one authenticated call, which is the argument
/// §7.3's sub-cell budget already makes. Refused rather than truncated: a silently flattened
/// expression answers a question the caller did not ask.
pub const MAX_FILTER_DEPTH: usize = 4;

impl FilterExpr {
    /// Nesting depth, with a leaf at 1.
    pub fn depth(&self) -> usize {
        match self {
            FilterExpr::Leaf { .. } | FilterExpr::Region(_) | FilterExpr::MemberOf(_) => 1,
            FilterExpr::AllOf(kids) | FilterExpr::AnyOf(kids) | FilterExpr::NoneOf(kids) => {
                1 + kids.iter().map(FilterExpr::depth).max().unwrap_or(0)
            }
        }
    }

    /// Every column named anywhere in this expression, in name order.
    pub fn columns(&self) -> BTreeSet<&str> {
        let mut out = BTreeSet::new();
        self.collect_columns(&mut out);
        out
    }

    fn collect_columns<'a>(&'a self, out: &mut BTreeSet<&'a str>) {
        match self {
            FilterExpr::Leaf { column, .. } => {
                out.insert(column.as_str());
            }
            // The reserved word, so `none_of: [region, region]` is one column and
            // `none_of: [region, department]` is two — the rule needs no special case for it.
            FilterExpr::Region(_) => {
                out.insert(REGION_COLUMN);
            }
            // The second reserved word, on the same argument: `none_of: [member_of, member_of]`
            // is one column and needs no special case in the negation rule.
            FilterExpr::MemberOf(_) => {
                out.insert(MEMBER_OF_COLUMN);
            }
            FilterExpr::AllOf(kids) | FilterExpr::AnyOf(kids) | FilterExpr::NoneOf(kids) => {
                for kid in kids {
                    kid.collect_columns(out);
                }
            }
        }
    }

    /// What every evaluation of an expression establishes before it begins: that it nests no
    /// deeper than [`MAX_FILTER_DEPTH`] and that each of its negations names exactly one column.
    pub(in crate::filter) fn check(&self) -> Result<(), FilterError> {
        let depth = self.depth();
        if depth > MAX_FILTER_DEPTH {
            return Err(FilterError::TooDeep {
                depth,
                max: MAX_FILTER_DEPTH,
            });
        }
        self.check_negations()
    }

    /// The one column a [`FilterExpr::NoneOf`]'s sub-expressions name — [`FilterExpr::check`] has
    /// already established that there is exactly one, so this cannot pick the wrong one.
    pub(in crate::filter) fn negated_column(kids: &[FilterExpr]) -> &str {
        kids.iter()
            .flat_map(|kid| kid.columns())
            .next()
            .expect("check_negations admits exactly one column")
    }

    /// Refuse a [`FilterExpr::NoneOf`] whose sub-expressions do not name exactly one column.
    ///
    /// **A whole-tree check run once, before evaluation** — not per node during it. A negation that
    /// spanned two columns would have to pick a presence set, and either choice is a silent answer
    /// to a question the caller did not ask: requiring both makes `none_of: [A, B]` narrower than
    /// the caller's reading, requiring either makes it fail-open under a lost layer. So the shape
    /// is refused rather than resolved, and `all_of: [none_of: [A], none_of: [B]]` says which was
    /// meant.
    pub(in crate::filter) fn check_negations(&self) -> Result<(), FilterError> {
        match self {
            FilterExpr::Leaf { .. } | FilterExpr::Region(_) | FilterExpr::MemberOf(_) => Ok(()),
            FilterExpr::AllOf(kids) | FilterExpr::AnyOf(kids) => {
                kids.iter().try_for_each(FilterExpr::check_negations)
            }
            FilterExpr::NoneOf(kids) => {
                let columns = self.columns();
                if columns.len() != 1 {
                    return Err(FilterError::NegationSpansColumns {
                        columns: columns.into_iter().map(str::to_string).collect(),
                    });
                }
                kids.iter().try_for_each(FilterExpr::check_negations)
            }
        }
    }
}

/// A filter expression routed for one request (decision 0068) — see
/// [`crate::filter::FilterColumns::evaluate_routed`].
#[derive(Debug)]
pub enum RoutedFilter {
    /// Every leaf routed entity space: the operand as §8.2 always had it, evaluated under the
    /// composed candidate, awaiting the existing crossing.
    Entity(Bitmap),
    /// At least one leaf routes row space. The entity-space sub-trees are already verdicts; the
    /// rest is evaluated over the request's own rows and combined in row space (`viewport.rs`).
    Row(RowExpr),
}

/// A filter tree ready for row-space evaluation: entity-space sub-trees collapsed to their
/// verdicts, row-space leaves awaiting the hot column.
///
/// The [`RowExpr::Entity`] verdicts are crossed into row space **once per request**, jointly, by
/// the measured crossing rule — 0062's one-crossing composition, upheld by the evaluator rather
/// than by each leaf crossing for itself.
#[derive(Debug)]
pub enum RowExpr {
    /// An entity-space sub-tree's verdict, evaluated under the composed candidate.
    Entity(Bitmap),
    /// A render-column leaf, to be tested against the hot column over the crossing domain.
    ///
    /// The **family is carried** rather than inferred from the operand or the stored width: see
    /// [`crate::filter::Placement`] for why a scan that guessed reads one family's absence as the other's value.
    Leaf {
        column: String,
        family: Family,
        operand: FilterOperand,
    },
    /// Intersection in row space.
    AllOf(Vec<RowExpr>),
    /// Union in row space. Empty matches nothing, exactly as [`FilterExpr::AnyOf`] does.
    AnyOf(Vec<RowExpr>),
    /// `present ∖ matched` in row space — the same positive predicate [`FilterExpr::NoneOf`] is in
    /// entity space, with the same failure sign: a row that cannot be read matches nothing, and
    /// under-reporting narrows.
    ///
    /// Presence is the family's own statement of it, which is why the family is carried here as
    /// well as on a leaf: a non-sentinel code for a category, the presence bitmap beside the
    /// column for a number (decision 0064).
    NoneOf {
        column: String,
        family: Family,
        kids: Vec<RowExpr>,
    },
    /// A region leaf, already resolved: its rows over the **whole view**, boundary rows tested
    /// under the request's composed mask (`crate::region`). Exact at any range, so a tree made of
    /// these and projected entity verdicts alone is [`crate::compose::FilterRows::Complete`].
    Region(crate::region::RegionRows),
    /// A `member_of` leaf, already resolved: `membership ∩ M_auth` over the **whole view**
    /// (`highlight-and-hierarchy.md` §3). Exact at any range, so it composes into
    /// [`crate::compose::FilterRows::Complete`] as a region does — and unlike a region it has
    /// already met the mask, so its cardinality is a quantity the principal may already read off
    /// the artifacts frame.
    MemberOf(Bitmap),
    /// `none_of` over region or `member_of` leaves: every rowed entity carries a position and may
    /// be a member, so the presence half is the whole view — or the request's domain, where a
    /// sibling leaf bounds the tree — and the answer is the complement of the union within it
    /// (selection-operand §5). A buffered entity has no row and matches neither the leaf nor this.
    NotInRows(Vec<RowExpr>),
}

impl RowExpr {
    /// Every entity-space verdict in this tree, in a fixed pre-order — the joint crossing walks
    /// this exact order, so the evaluator can pair images back up positionally.
    pub fn entity_verdicts(&self) -> Vec<&Bitmap> {
        let mut out = Vec::new();
        self.collect_verdicts(&mut out);
        out
    }

    fn collect_verdicts<'a>(&'a self, out: &mut Vec<&'a Bitmap>) {
        match self {
            RowExpr::Entity(bitmap) => out.push(bitmap),
            RowExpr::Leaf { .. } | RowExpr::Region(_) | RowExpr::MemberOf(_) => {}
            RowExpr::AllOf(kids)
            | RowExpr::AnyOf(kids)
            | RowExpr::NotInRows(kids)
            | RowExpr::NoneOf { kids, .. } => {
                for kid in kids {
                    kid.collect_verdicts(out);
                }
            }
        }
    }

    /// Whether every row-space node answers over the whole view — no render-column leaf, whose
    /// answer is bounded by the request's own rows. Decides whether the tree's result can be
    /// `FilterRows::Complete`.
    pub fn is_whole_view(&self) -> bool {
        match self {
            RowExpr::Entity(_) | RowExpr::Region(_) | RowExpr::MemberOf(_) => true,
            RowExpr::Leaf { .. } | RowExpr::NoneOf { .. } => false,
            RowExpr::AllOf(kids) | RowExpr::AnyOf(kids) | RowExpr::NotInRows(kids) => {
                kids.iter().all(RowExpr::is_whole_view)
            }
        }
    }

    /// Whether a region leaf is anywhere in the tree — the tree's row set then carries interior
    /// rows the mask has not yet met, and must not be counted before it does.
    pub fn has_region(&self) -> bool {
        match self {
            // **A `member_of` leaf is not one**, though it shares the negation node: its set has
            // already met the mask, so its cardinality is the masked count the artifacts frame
            // already serves. The negation is, because its presence half is every row in scope
            // and the mask has not met those.
            RowExpr::Region(_) | RowExpr::NotInRows(_) => true,
            RowExpr::Entity(_) | RowExpr::Leaf { .. } | RowExpr::MemberOf(_) => false,
            RowExpr::AllOf(kids) | RowExpr::AnyOf(kids) => kids.iter().any(RowExpr::has_region),
            RowExpr::NoneOf { kids, .. } => kids.iter().any(RowExpr::has_region),
        }
    }

    /// The coarsest verdict any region leaf in the tree reached, or `None` where there is none —
    /// the `x-tessera-region` header's value.
    pub fn region_verdict(&self) -> Option<crate::region::RegionVerdict> {
        match self {
            RowExpr::Region(rows) => Some(rows.verdict),
            RowExpr::Entity(_) | RowExpr::Leaf { .. } | RowExpr::MemberOf(_) => None,
            RowExpr::AllOf(kids)
            | RowExpr::AnyOf(kids)
            | RowExpr::NotInRows(kids)
            | RowExpr::NoneOf { kids, .. } => kids
                .iter()
                .filter_map(RowExpr::region_verdict)
                .reduce(|a, b| a.coarser(b)),
        }
    }
}

/// How a routed evaluation answers a region leaf — the caller's, because the answer needs the
/// request's composed mask, the view's segments and the engine's cache, none of which this module
/// holds. Refusing is the resolver's own affair: a shape that cannot be decomposed for this
/// generation is [`FilterError::RegionUnavailable`], never an empty operand.
pub type RegionResolver<'a> =
    dyn Fn(&RegionLeaf) -> Result<crate::region::RegionRows, FilterError> + 'a;

/// How a routed evaluation answers a `member_of` leaf — the caller's, for
/// [`RegionResolver`]'s reason and one more: the gate is the artifact's own existence criterion,
/// which lives in `viewport.rs` beside the one the artifacts frame applies, and two
/// transcriptions of it is the failure this codebase has written down more than once.
///
/// The answer is `membership ∩ M_auth` over the whole view. An artifact this principal would not
/// be served is the **empty bitmap** and never an error; only a layer name outside their own
/// (`FilterError::UnknownLayer`) and a generation that cannot answer at all
/// (`FilterError::MemberOfUnavailable`) refuse.
pub type MemberResolver<'a> = dyn Fn(&MemberOfLeaf) -> Result<Bitmap, FilterError> + 'a;

/// The two row-space leaves' resolvers, together — one argument rather than two on
/// [`crate::filter::FilterColumns::evaluate_routed`] and on every frame of [`crate::filter::FilterColumns::route`].
pub struct RowLeafResolvers<'a> {
    pub regions: &'a RegionResolver<'a>,
    pub members: &'a MemberResolver<'a>,
}
