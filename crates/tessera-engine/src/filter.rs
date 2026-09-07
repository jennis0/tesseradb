//! Attribute filtering: the entity-space operand, and the candidate it is evaluated under.
//!
//! `filter-index.md` owns the artefact and `filter-surface.md` the query surface. This module is
//! the seam between them — it holds the bundle's filter columns and turns a request's operand into
//! an entity-space bitmap.
//!
//! # The candidate is the composed verdict, and that is the whole safety argument
//!
//! A scan returns whatever its candidate contained. The session's **fragment** is not that set: a
//! suppression never touches the artefact (write-path §5.4, Rule S) and a deletion does not until
//! the fold executes it, so the fragment still holds entities the viewer must not see. Scanning
//! under it would resurrect every one of them, silently and with the right-looking shape.
//!
//! So [`candidate`] composes first — `fragment ∖ denied`, plus the buffered entities whose verdict
//! passes — and the scan runs under *that*. An earlier design reached the same place from the other
//! side, composing the *result* because the operand had been evaluated unmasked. Masked evaluation
//! moves the obligation earlier, which is both simpler and stricter: a result that was never
//! scanned cannot be forgotten to be composed.
//!
//! # Why the work carries no timing channel — and the one column where it does, by declaration
//!
//! A scan walks the candidate and tests the values it selects, so the work is a function of
//! `(candidate, column)` and never of the value sought. A value the principal cannot see costs what
//! a value that does not exist costs — per-point-attributes §3.8's requirement that the two be
//! indistinguishable *in work*, obtained structurally rather than by padding.
//!
//! **A category's derived postings answer `eq` and `in` where, and only where, the column's
//! vocabulary is `visibility = "public"`** (decision 0063). Postings resolve over the whole corpus and
//! are then intersected with the candidate, where the scan takes the candidate as its input — so
//! their work is a function of the *value named*. `probes/2026-08-08-filter-layout/` arm 9 measures
//! a hidden, scattered 10⁷-member value at **2.1 ms** intersected where an absent value costs
//! **0.000 ms**: a scattered value's members meet every container even when no bits do. Under
//! `derived` that difference is a disclosure of exactly what the declaration withholds, so a
//! `derived` column keeps the scan. Under `public` the value set is served to every principal
//! alike by `/v1/categories`, so the timing distinguishes only a fact the client already holds —
//! registered as leak-register row **C24**.
//!
//! **The route is fixed at open from the declaration**, never chosen per request, per principal or
//! from a statistic: §8.2 forbids a statistics-driven route because it makes execution time a
//! function of how much the principal can see. [`Layers::route`] is therefore a field, not an
//! argument.
//!
//! **Postings cover the base build and nothing since**, so the routed answer is
//! `postings ∩ candidate` unioned with a *scan* of every extent layer. Answering from the postings
//! alone would omit every entity ingested since the build — narrower, safe under **I12**, and
//! indistinguishable from a correct answer, which is the failure this subsystem exists to avoid.
//!
//! # The keyword family: one dictionary per layer, and the needle resolved inside it
//!
//! A `keyword` column stores a `u32` **ordinal** per present entity and, beside it, that layer's own
//! front-coded sorted dictionary of the distinct values the layer holds (records §4.3). Every fast
//! operator is then an ordinal question, answered by the same fixed-width scan the numeric families
//! use: `eq` resolves the needle to one ordinal, `in` to a list of them, and `prefix` — sortedness
//! being the reason the dictionary is sorted at all — to a contiguous ordinal range the range scan
//! already knows how to test.
//!
//! **The resolve is per layer, against that layer's own dictionary.** An ordinal is a position in
//! one dictionary and means nothing outside it: the base build and every flush extent number their
//! own keys, so one string is a different ordinal in each, and reading one layer's ordinals against
//! another's dictionary is a recolouring with no symptom (`tessera_filter::SortedDict`'s module
//! doc). [`Layer`] therefore holds the value column and its dictionary together, and an ordinal
//! produced by a resolve never outlives the single scan it was made for — it is never stored, never
//! served, and never compared against an ordinal from elsewhere.
//!
//! **A needle no dictionary resolves is scanned for anyway, and that is security-bearing.** The
//! miss becomes [`NO_SUCH_ORDINAL`], which no slot can hold, and the scan runs over the whole
//! candidate exactly as it would for a needle that resolved. Returning early instead would make
//! *no item has this value* measurably cheaper than *some do* — a timing channel about content the
//! principal cannot see (records §4.3; per-point-attributes §3.8, whose rule is that the two be
//! indistinguishable in outcome **and in work**). [`OrdinalPredicate`] is the shape that keeps the
//! early return out: it is total, it has no "matches nothing, so skip the scan" variant, and every
//! arm of [`scan_ordinals`] runs a scan.
//!
//! The shape is not the whole assurance, because a short circuit can be added above it and still
//! answer correctly. So the rule is also **asserted in work**: `keyword_tests` compares the slots a
//! resolving needle traverses against a non-resolving one over the same candidate and layers, which
//! records §10 names as the conformance suite's one deliberate work assertion. It catches the
//! version no answer-level test can — a resolve that skips a layer whose dictionary does not hold
//! the needle, which returns exactly the right entities for less work.
//!
//! **`contains` has two routes and a crossover that reads no data.** The broad route walks the
//! dictionary, decodes and substring-searches **every** key whatever the needle — front coding
//! elides shared prefixes, so a substring can span an elided one — and scans for the ordinals it
//! collected. The narrow route takes each candidate entity's ordinal and probes the dictionary for
//! that one key. [`contains_route`] chooses between them from the candidate's cardinality and the
//! layer's dictionary size and nothing else: the first is the principal's own quantity, which they
//! can compute for themselves, and the second is a property of the bundle, identical for every
//! principal — §8.2's admissible class, never a statistic about what the principal's data contains.
//!
//! # A column is layers, because the corpus grows and the build's column does not
//!
//! The batch build writes a column covering `[0, entity_id_high_water)`, and every flush since has
//! published entities above it. Each flush therefore appends an **extent** — its own entities'
//! values with its own presence bitmap (`filter-index.md` §2.1, §2.5) — and a column here is the
//! base plus every live extent, scanned in turn and unioned.
//!
//! **The layers are disjoint in entity space and that is checked, not assumed.** Entity ids are
//! permanent and issued from the high-water (**I9**), so a flush can only add entities no earlier
//! layer holds; [`FilterColumns::compose`] refuses an extent that overlaps what is already
//! composed, because two layers claiming one entity would make it match both values, and a filter
//! naming either would return it. That is a wrong answer with no symptom, so it is a refusal at
//! open rather than a comment.
//!
//! Composition is per **generation**, not per request: a published flush builds the next
//! `FilterColumns` from the live one by pushing a pointer, and the per-request cost is one scan per
//! layer over a candidate that has already been intersected with the layer's presence. So the work
//! stays a function of `(candidate, column)` — the number of layers is a property of the bundle,
//! not of what is being asked for.
//!
//! # What is still answered short, and why that one is the design
//!
//! A **buffered** entity — accepted, acked, not yet flushed — is in the candidate and in no layer,
//! so it matches no predicate. `filter-index.md` §5 rules on that directly: a buffered entity has
//! no row, the entity-space verbs under-report until its flush, and under-reporting narrows `M_sel`
//! and is safe under **I12**. It is a bounded lag measured in one flush interval, not a coverage
//! cliff that never closes, which is what the refusal this composition replaced was answering.
//! The row-space route below under-reports the same entities for the same reason — a buffered
//! entity has no row for the hot column to hold — so the two routes cannot disagree about them.
//!
//! # The row-space operand (decision 0068), and how this module routes a tree
//!
//! A column with `render = true` is filterable **over the request's own rows**, against the hot
//! column in `columns.arrow`. Such a leaf produces no entity-space bitmap at all; it is evaluated
//! in `viewport.rs` over the request's merged tile ranges, and the answer is exact only over that
//! domain (`FilterRows::Viewport`).
//!
//! **Two families reach that route and they say "no value" differently.** A category reserves code
//! 0 out of its vocabulary, so the hot column itself carries the absence. A number, a datetime and
//! a bool have no spare value — the hot column is non-nullable and an absent one is written as the
//! type's zero, which is an ordinary value — so their absence is decision 0064's presence bitmap
//! beside the column, read by the scan and never inferred from the stored bytes. Which rule
//! applies is the column's [`Placement::family`], carried into the routed tree rather than guessed
//! from the width, because a rendered `u8` category and a rendered `u8` number are the same bytes.
//!
//! [`FilterColumns::evaluate_routed`] is the seam. It routes each leaf by the column's
//! [`Placement`] — entity space, row space, or both — and where a column affords both, by the
//! caller's route preference, which `viewport.rs` derives from 0068's rule:
//! **row space while `rows_in_ranges ≤ |M_auth|`, entity space past it** — both quantities the
//! caller could compute, never a statistic about the principal's data (§8.2). A tree whose every
//! leaf routes entity-space evaluates here exactly as [`FilterColumns::evaluate`] always has; a
//! tree with any row-space leaf comes back as a [`RowExpr`]: its maximal entity-space sub-trees
//! already evaluated to bitmaps **under the composed candidate**, its row-space leaves left for
//! the per-tile evaluation, to be crossed once and combined in row space (0062's tree, one
//! crossing per request — placement memo §2.2).
//!
//! **The candidate is the composed verdict — the fragment with the overlay applied — never the
//! raw fragment, and for the row-space half that is a property of consumption, stated here
//! bindingly.** Suppressions touch no artefact (write-path §5.4, Rule S), so the hot column still
//! holds a suppressed entity's row and value, and a row-space leaf tests it like any other row.
//! What keeps it out of every viewport, count and record is that a row-space result enters the
//! request **only** through `EffectiveMask::with_filter`, whose every consumer intersects it with
//! the composed mask — the filter is applied last, by intersection, on top of
//! `base ∖ minus ∪ plus` (`compose.rs`) — and the entity-space sub-trees are evaluated under
//! [`candidate`], which subtracts the overlay before any scan runs. A route evaluated under
//! anything less would silently resurrect a suppressed entity (records §6, review N2);
//! `tests/filtering.rs`'s suppression differential is the proof.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use croaring::Bitmap;
use rustc_hash::{FxHashMap, FxHashSet};
use tessera_filter::{
    resolve_union, CodeSet, Codes, ColumnPostings, DictError, KeyMatcher, RecordExtentPaths,
    RecordStack, RecordValue, SortedDict, ValueColumn,
};

/// The operand value types, re-exported so a caller building a [`FilterOperand`] needs no
/// dependency on the filter crate — `check-layers.sh` denies `tessera-server` that edge, to keep the
/// server on engine API types only, and an operand's *values* are part of this crate's API surface
/// even though the column they are compared against is not.
pub use tessera_filter::{Endpoint, Scalar};
use tessera_store::manifest::Visibility;
use tessera_types::{AttrLocalId, TermId};

use crate::compose::verdict;
use tessera_authz::fragment::FrozenFragment;
use tessera_lifecycle::buffer::IngestBuffer;
use tessera_lifecycle::overlay::Overlay;

/// A filterable column's family, which decides **which operators apply to it**.
///
/// Published per column by `/v1/meta` so a client need not infer it, and checked at the parse: an
/// operator outside a column's family is a *shape* error, refused like an unknown column rather
/// than answered as an empty operand. That distinction is safe to make because a family is
/// deployment schema — identical for every principal — where a *value*'s existence is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// Values are vocabulary entries: `eq`, `in`, over a key or a code.
    Category,
    /// Values are short strings matched exactly, stored as an ordinal into the layer's own sorted
    /// dictionary (records §4.3): `eq`, `in`, `prefix`, `contains`, byte-exact against the key the
    /// ordinal names — see this module's header for the cost profile.
    Keyword,
    /// Values are numbers — every integer width, both floats, `timestamp_us` and `bool`. A range
    /// is a scan like everything else: no level tree, no bit slicing, no zone map
    /// (`filter-index.md` §3).
    Numeric,
    /// Values are prose, matched by what they say (records §4.4): `match` and its m-of-n form,
    /// answered from per-token postings rather than from a scan.
    ///
    /// **The only family with no value column at all.** Its terms are postings over a token
    /// dictionary and its prose is a record-blob row, so there is nothing per entity to scan — the
    /// reason this family reaches neither the hot column nor the entity-space scan, and the reason
    /// its layers carry a dictionary and postings where every other family's carry values.
    Text,
}

impl Family {
    /// One column's family, from its declaration — **the single derivation**, used by the engine's
    /// routing, by `/v1/meta`'s operand list and by the request parser alike, so the operators a
    /// client is published cannot differ from the ones its requests are held to, and neither can
    /// differ from the predicate the scan applies.
    /// **`Numeric` is enumerated rather than defaulted to, and that is a fail-closed choice.** A
    /// type this function does not recognise falls to `Keyword`, which is the direction that fails
    /// safely: a keyword column is opened with its layer's dictionary and a layer without one is
    /// refused in both directions (see [`FilterColumns::compose`]), so a misclassified column
    /// refuses to open. Defaulting to `Numeric` would instead compare a keyword's per-layer
    /// *ordinals* as though they were values — publishing `range` for the column, accepting one,
    /// and answering the same request differently against the base and against a flush extent,
    /// none of it visible.
    pub fn of(scalar: &tessera_store::manifest::DeclaredScalar) -> Family {
        if scalar.vocabulary.is_some() {
            return Family::Category;
        }
        if is_numeric(scalar.arrow_type) {
            return Family::Numeric;
        }
        if scalar.arrow_type == tessera_spatial::tiler::ScalarType::Text {
            return Family::Text;
        }
        Family::Keyword
    }

    /// One **group-scoped** column family's family (`views.md` §5) — the same derivation as
    /// [`Family::of`], over the declaration a scoped family records instead of a declared
    /// scalar's. The two read the same two fields, and a scope changes only which column file a
    /// predicate reads, never how its values are read.
    pub fn of_scoped(scoped: &tessera_store::manifest::ScopedScalar) -> Family {
        if scoped.vocabulary.is_some() {
            return Family::Category;
        }
        if is_numeric(scoped.arrow_type) {
            return Family::Numeric;
        }
        if scoped.arrow_type == tessera_spatial::tiler::ScalarType::Text {
            return Family::Text;
        }
        Family::Keyword
    }

    /// The operator names this family accepts, in the order `/v1/meta` publishes them.
    pub fn operands(self) -> &'static [&'static str] {
        match self {
            Family::Category => &["eq", "in"],
            Family::Keyword => &["eq", "in", "prefix", "contains"],
            Family::Numeric => &["eq", "in", "range"],
            Family::Text => &["match", "phrase"],
        }
    }

    /// The family name `/v1/meta` publishes, which is also the name the request parser accepts —
    /// one definition, so the surface a client is published cannot name a family its requests
    /// would be refused for.
    pub fn as_str(self) -> &'static str {
        match self {
            Family::Category => "category",
            Family::Keyword => "keyword",
            Family::Numeric => "numeric",
            Family::Text => "text",
        }
    }

    /// Does a value of this family occupy a slot in the hot column, and so afford the row-space
    /// route (decision 0068; records §6.2)?
    ///
    /// Only the fixed-width families. `render` on a `keyword` is refused at the schema because the
    /// hot column is a fixed-width slot per row and a keyword's value is not one — so a keyword is
    /// filterable in entity space alone. Stated once here rather than at each site that asks, so
    /// the two cannot come to disagree about which families the row route reaches.
    pub fn reaches_hot_column(self) -> bool {
        match self {
            Family::Category | Family::Numeric => true,
            // Neither is a fixed-width slot: a keyword's value is its bytes and a text column's
            // value is not per entity at all.
            Family::Keyword | Family::Text => false,
        }
    }
}

/// The types whose values are numbers — every integer width, both floats, `timestamp_us` and
/// `bool`. Listed, because [`Family::of`] must not reach `Numeric` by default; see its doc.
fn is_numeric(ty: tessera_spatial::tiler::ScalarType) -> bool {
    use tessera_spatial::tiler::ScalarType as T;
    matches!(
        ty,
        T::Bool
            | T::U8
            | T::U16
            | T::U32
            | T::U64
            | T::I8
            | T::I16
            | T::I32
            | T::I64
            | T::F32
            | T::F64
            | T::TimestampUs
    )
}

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
    /// (selection-operand §5). Row space only: [`FilterColumns::evaluate`] refuses it, and
    /// [`FilterColumns::evaluate_routed`] resolves it through the caller's resolver, which is
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

    /// Refuse a [`FilterExpr::NoneOf`] whose sub-expressions do not name exactly one column.
    ///
    /// **A whole-tree check run once, before evaluation** — not per node during it. A negation that
    /// spanned two columns would have to pick a presence set, and either choice is a silent answer
    /// to a question the caller did not ask: requiring both makes `none_of: [A, B]` narrower than
    /// the caller's reading, requiring either makes it fail-open under a lost layer. So the shape
    /// is refused rather than resolved, and `all_of: [none_of: [A], none_of: [B]]` says which was
    /// meant.
    fn check_negations(&self) -> Result<(), FilterError> {
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

/// One bundle's filter columns, keyed by declared column name — plus the record-blob stack,
/// which shares this type's lifecycle rather than its name.
///
/// Opened once per generation, not per request.
///
/// **Membership is not filterability.** The map holds every column the build wrote a value column
/// for — which includes a `visibility = "derived"` category that is not declared filterable, since
/// its postings are what `/v1/categories` derives value visibility from. [`FilterColumns::resolve`]
/// gates on [`Layers::filterable`] rather than on presence, so such a column is refused exactly as
/// an undeclared one is: an *undeclared* column is a caller error, where an unresolvable *value* is
/// an empty operand (`filter-surface.md` §2.1).
///
/// **Why the record blob rides here.** The stack (records §3) is not a filter column — no query
/// ever reads it (records §3's rule: a column the scan reads is never compressed; the blob is
/// never read by a query) — but it is opened from the same manifests, at the same two sites, and
/// it belongs to the published prefix exactly as the value columns do: carried forward by every
/// flush successor, replaced whole at a fold's prefix rotation. Housing it here gives it that
/// lifecycle without a second prefix-tracking mechanism to keep correct; the alternative was a
/// parallel field threaded through every generation constructor for a reader only drill-down
/// takes.
pub struct FilterColumns {
    columns: BTreeMap<String, Layers>,
    /// The evaluation space(s) each filterable column affords — including a rendered category
    /// with no entity-space layers at all, which [`FilterColumns::columns`] cannot represent.
    placements: BTreeMap<String, Placement>,
    /// The access mode this generation was opened with, so a successor composing a flush's
    /// extents opens them the same way. A generation that mapped its columns and read its
    /// successor's would be two cost models in one bundle.
    access: tessera_filter::Access,
    /// The record blob: the build's base (present iff the compiled schema has a blob-resident
    /// column) plus every flush extent the manifest names. Empty — zero layers — when neither
    /// exists, which answers `fields_of` with an ordinary absence.
    records: Arc<RecordStack>,
    /// The entity→term transpose: the build's base plus every flush extent the manifest names
    /// (contracts §2.4). It rides here for the record blob's reasons exactly — opened from the
    /// same manifests at the same two sites, carried forward by every flush successor, replaced
    /// whole at a fold's prefix rotation — and it is read by the same two callers a record is:
    /// the drill-down (intersected with the session's satisfied set, decision 0114) and the write
    /// path's join arm (`views.md` §4).
    entity_terms: Arc<tessera_store::EntityTermsStack>,
}

// Hand-written because `RecordStack` carries no `Debug` of its own (it is a stack of mapped
// artefacts); the columns and placements are the parts worth printing.
impl std::fmt::Debug for FilterColumns {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FilterColumns")
            .field("columns", &self.columns)
            .field("placements", &self.placements)
            .finish_non_exhaustive()
    }
}

impl Default for FilterColumns {
    fn default() -> Self {
        FilterColumns {
            columns: BTreeMap::new(),
            placements: BTreeMap::new(),
            access: tessera_filter::Access::Read,
            records: Arc::new(empty_record_stack()),
            entity_terms: Arc::new(tessera_store::EntityTermsStack::empty()),
        }
    }
}

/// One indexed column's value layers, base first — a **read** view over what
/// [`FilterColumns::value_layers`] holds.
///
/// **The split into base and extents is the whole reason this type exists.** An artifact layer's
/// row-addressed membership is written over the *base* row space and survives a flush because an
/// append moves no bit it holds (`RowSpace::project_base`); the entities a flush published since sit
/// above that base and are exactly the ones the **extent** layers hold values for. So a reader that
/// wants both halves has to be able to ask for each, and one that asks for all of them at once —
/// which is what a fold's rewrite wants — takes both.
///
/// **Disjoint in entity space by I9**, so no entity has a value in two layers and the order they
/// are visited decides nothing — which is what makes the base/extent split a partition of the
/// column rather than a filter over it.
#[derive(Clone, Copy)]
pub struct ValueLayers<'a> {
    layers: &'a [Layer],
}

impl<'a> ValueLayers<'a> {
    /// The build's own column — the one whose entities the base row space covers. `None` where the
    /// column arrived entirely in flush extents, which is a column declared after the build.
    pub fn base(&self) -> Option<&'a ValueColumn> {
        self.layers
            .iter()
            .find(|layer| layer.values_rel.is_none())
            .map(|layer| layer.values.as_ref())
    }

    /// The flush extents, oldest first — the entities published since the base was written.
    pub fn extents(&self) -> impl Iterator<Item = &'a ValueColumn> {
        self.layers
            .iter()
            .filter(|layer| layer.values_rel.is_some())
            .map(|layer| layer.values.as_ref())
    }
}

/// A stack of zero layers — what a schema with no blob-resident column and no extents owns.
/// Infallible: `RecordStack::open` touches no file when given nothing to open.
fn empty_record_stack() -> RecordStack {
    RecordStack::open(None, &[], tessera_filter::Access::Read)
        .expect("a record stack over no layers opens without IO")
}

/// The evaluation space(s) one filterable column affords, and the family whose rules its values
/// are read by (decision 0068; records §6.2).
///
/// Derived at open from the compiled declaration alone — never from a statistic, never per
/// principal (§8.2): `entity` where the column has an entity-space value column it may answer a
/// filter from (`index = true`, or a rendered category whose vocabulary floor stores one —
/// `visibility = "derived"`); `row` where `render = true` put it in the hot column, which is every
/// rendered column, `utf8` being refused from the hot column at the schema.
///
/// **The family travels with the placement because the row route cannot infer it from the
/// stored width.** A rendered `u8` category and a rendered `u8` number are the same slice of
/// bytes, and their absence rules are opposite: the category's is code 0, the number's is the
/// presence bitmap beside the column (decision 0064), where 0 is an ordinary value. A scan that
/// guessed would resurrect every valueless row of one or drop every genuine zero of the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub entity: bool,
    pub row: bool,
    pub family: Family,
}

/// Is this column filterable at all under decision 0068 — `index = true`, or rendered?
///
/// **The server's `/v1/meta` operand list and its parse gate call this**, so the surface a client
/// is published cannot drift from the one the engine routes. A rendered **string** column is
/// excluded rather than assumed away: the schema refuses `render` on `utf8`, and on `keyword` and
/// `text` alike, because the hot column is fixed-width — so the combination reaches no manifest,
/// and a predicate that relied on that instead of stating it would publish a byte predicate over a
/// column the row scan cannot read.
pub fn is_filterable(scalar: &tessera_store::manifest::DeclaredScalar) -> bool {
    scalar.index || (scalar.render && Family::of(scalar).reaches_hot_column())
}

/// The character a filter leaf **pins** a group-scoped attribute's view with — `sentiment@2026-Q3`
/// (`views.md` §5).
///
/// Reserved out of a column name at the build, which is what makes the split unambiguous: a leaf
/// carries at most one `@`, everything before it is a column and everything after it is a view's
/// key within the attribute's own group.
pub const PIN: char = '@';

/// The internal name one view's column of a group-scoped family is held under —
/// `sentiment@quarter:2026-Q3`.
///
/// **Not a spelling any caller writes.** A request pins by *key* within the attribute's own group
/// (`sentiment@2026-Q3`), which is a view's only address
/// ([decision 0113](../../../docs/decisions/0113-ordinals-are-removed-and-the-key-is-the-only-address.md));
/// resolution turns that into the view's id and this function into the key the column map answers
/// on. Holding the resolved form here is
/// what lets a scoped column evaluate as an unscoped one of its family does — one map, one
/// `evaluate`, and no second route for a leaf to take.
pub fn scoped_column_name(name: &str, view_id: &str) -> String {
    format!("{name}{PIN}{view_id}")
}

/// The name a manifest extent entry composes onto — the column's own for an entity-scoped one, and
/// [`scoped_column_name`]'s resolved form where the entry names a view (`views.md` §5).
///
/// **One function, so every producer and every reader of an extent agree.** A flush, a coalesce,
/// a restart's `open` and the live composition each turn an `(column, view)` pair into the key the
/// column map answers on; two spellings of that rule would compose a flush's layer under a name no
/// leaf resolves to, and the values would be served as the absence below with no error anywhere.
pub fn extent_column_name(column: &str, view: Option<&str>) -> String {
    match view {
        Some(view) => scoped_column_name(column, view),
        None => column.to_string(),
    }
}

/// Is this group-scoped family on the filter surface — published by `/v1/meta`'s
/// `filter_operands` and resolvable by a leaf (`views.md` §5)?
///
/// **`index`, or `render`** — [`is_filterable`]'s licence, asked of a family, and the two are the
/// same rule since the asymmetry between them was closed (2026-08-31, owner ruling). Every one of
/// the four families is served, the build writing per view exactly what its entity-scoped
/// counterpart writes bundle-wide: a value column and a presence bitmap for a number, those and a
/// dictionary for a keyword, those and keyed postings for a category, and a token dictionary with
/// positional postings — no value column at all — for text.
///
/// **What `render` licences here is the entity-space column, not the row tail.** A rendered
/// entity-scoped column has no entity-space column of its own — `owes_value_column` is `index`
/// or a `derived` vocabulary — so decision 0068 answers it over the request's own rows. A scoped
/// family's per-view column *is* entity space and is written whatever the family's flags, so a
/// rendered one is answered from it by the ordinary scan, which is what makes a **pin** work:
/// a leaf naming another view's column is read where it lives rather than from rows the request
/// does not hold. The row tail a rendered family also occupies answers no filter at all
/// ([`open_scoped_column`]'s placement).
///
/// **`text` is excluded from the render arm** rather than assumed away, as [`is_filterable`]
/// excludes the string families from its own: `render` on a scoped `text` family is refused at the
/// declaration, so the combination reaches no manifest a build wrote — and a manifest that
/// carried it would name a token index no pass produced, which this predicate would otherwise
/// demand at open.
/// Does this family have an entity-space **value column** — the artefact a flush writes an extent
/// into and the drill-down reads a value out of?
///
/// **Every family but `text`**, whose extent is a token dictionary and positional postings and
/// holds nothing per entity. This is deliberately *wider* than [`scoped_is_filterable`]: a family
/// declaring neither `index` nor `render` is stored and served at the drill-down without being
/// searchable or drawn (owner ruling), so its column is opened and its extents composed while its
/// leaf stays the unknown-column refusal — `EngineMeta::resolve_filter_column` requires
/// [`scoped_is_filterable`] and answers `Unknown` before any column is looked up, so opening one
/// here puts nothing on the filter surface.
///
/// The rule itself is [`ScopedScalar::has_value_column`], one crate down beside its licence
/// sibling; this is the engine's name for it and nothing more.
pub fn scoped_has_value_column(scoped: &tessera_store::manifest::ScopedScalar) -> bool {
    scoped.has_value_column()
}
///
/// **The rule itself is `ScopedScalar::is_filterable`**, one crate down, because the build decides
/// what to *write* on the same licence and `check-layers.sh` denies the build this crate. This is
/// the engine's name for it and nothing more.
pub fn scoped_is_filterable(scoped: &tessera_store::manifest::ScopedScalar) -> bool {
    scoped.is_filterable()
}

/// Does this scoped family's per-view column carry keyed postings — the build's
/// `scoped_postings_are_owed`, on the manifest's own types?
///
/// A category's, and only a category's: the postings are what an `eq` or an `in` is answered from
/// on a `public` vocabulary, and what `/v1/categories` derives value visibility from on a
/// `derived` one. The two functions must agree, or the open demands a file no pass wrote — a
/// refusal — or leaves one no reader touches.
///
/// **[`scoped_is_filterable`] is the whole condition, where an entity-scoped column's is `index`
/// *or* a `derived` vocabulary.** The difference is that a scoped family's admission decides both
/// surfaces at once: a family on no filter surface has no `/v1/categories` answer either, that
/// route resolving a scoped column through the same admission the filter parse makes — so
/// postings written for one would be read by nothing. A **rendered** category is therefore on
/// both surfaces and owes them, which is where this parts from the entity-scoped rendered
/// category: that one has no entity-space column for postings to key, and this one always has.
pub(crate) fn scoped_owes_postings(scoped: &tessera_store::manifest::ScopedScalar) -> bool {
    scoped.vocabulary.is_some() && scoped_is_filterable(scoped)
}

/// Open one view's column of a group-scoped attribute family (`views.md` §5) — the name it is held
/// under, its placement, and its layers.
///
/// **One column per view, opened under its resolved name**, so a scoped leaf evaluates through
/// exactly the machinery an unscoped one of its family does: the same `ValueColumn`, the same
/// scan, the same presence rules for absence. The only thing the scope decides is which file —
/// which is what keeps the attribute inside I2's argument unchanged, every value being indexed by
/// entity and every predicate answering a bitmap in entity space that the mask meets before any
/// permutation.
///
/// **Two callers, one body.** [`FilterColumns::open`] walks every family's `views` at startup; a
/// flush that wrote the *first* column of a family for a view created since the build composes it
/// onto the live generation through [`FilterColumns::with_scoped_columns`]. The two must produce
/// the same reader, or a running process and the same bundle reopened would disagree about what a
/// pin resolves to.
fn open_scoped_column(
    partition_dir: &Path,
    family: &tessera_store::manifest::ScopedScalar,
    view_id: &str,
    incarnation: tessera_types::view::ViewIncarnation,
    vocabularies: &[tessera_store::manifest::ManifestVocabulary],
    mmap: bool,
) -> std::io::Result<(String, Placement, Layers)> {
    let scoped_family = Family::of_scoped(family);
    // **A family with neither flag is opened and is not filterable** (owner ruling 2026-09-01).
    // Its per-view column is on disc exactly as an indexed one's is, and the drill-down reads one
    // entity's value out of it — so the column is held here, `filterable: false`, which is the
    // same standing an entity-scoped `derived` category with neither flag already has: `resolve`
    // refuses it by name and `FilterColumns::stored_value` answers from it.
    let filterable = family.is_filterable();
    // The analyser a text family's terms were produced by: an analyser this binary does not carry
    // is the same refusal an entity-scoped text column's is — a `match` answered from a different
    // segmentation is a wrong answer wearing a correct one's clothes.
    let analyser = (scoped_family == Family::Text)
        .then(|| resolve_analyser(&family.name, family.analyser.as_deref()))
        .transpose()?;
    // `attrs/<column>/<group>/<key>/` — the view id's own path components, through the one place a
    // view id becomes a path, so the opener cannot drift from the writer. Above the declared
    // incarnation the key carries its own suffix (decision 0115): a recreated key opens its own
    // base, and its predecessor's is left where the fold's reclaim expects to find it.
    let mut dir = partition_dir.join("attrs").join(&family.name);
    for component in tessera_store::scoped_column_components(view_id, incarnation) {
        dir.push(component);
    }
    let name = scoped_column_name(&family.name, view_id);
    let placement = Placement {
        entity: true,
        // **Never the row route**, though a rendered family does occupy a row tail
        // (`views.md` §5): a leaf resolves to one entity-space column and a pin may make that
        // another view's, which no scan of *these* rows can answer. A rendered family is an
        // operand through the entity column beside that tail rather than through it, so the
        // entity route is the whole filter surface — see [`scoped_is_filterable`].
        row: false,
        family: scoped_family,
    };
    // **No position in `declared_scalars`, because it is not one of them.** The tag is the record
    // blob's field key and a scoped column is never blob-resident — it has an entity-space home by
    // construction, which is the condition `blob_resident` is the negation of. The sentinel is what
    // a reader would see if that ever stopped being true, rather than another column's field.
    let declared_index = usize::MAX;
    // **Text opens with no value column at all**, per view exactly as bundle-wide: its artefacts
    // are the token dictionary and the positional postings over it. The base's layer is opened
    // here; a flush's layers are appended by the composition, which is where every text extent
    // enters whatever its scope.
    if scoped_family == Family::Text {
        let text = vec![text_layer(
            SortedDict::open_dir(&dir, request_access(mmap))?,
            ColumnPostings::open(&dir.join("postings.arrow"), mmap)?,
            &name,
            "base",
            // The base writes no presence file of its own, here for the same reason the
            // entity-scoped base writes none: see `TextLayer::present`.
            Bitmap::new(),
            None,
        )?];
        return Ok((
            name,
            placement,
            Layers {
                declared_index,
                layers: Vec::new(),
                covered: Bitmap::new(),
                filterable,
                postings: None,
                analyser,
                text,
                route: Route::Postings,
                family: scoped_family,
            },
        ));
    }
    let base = Arc::new(ValueColumn::open_dir(&dir, request_access(mmap))?);
    let dict = (scoped_family == Family::Keyword)
        .then(|| SortedDict::open_dir(&dir, request_access(mmap)).map(Arc::new))
        .transpose()?;
    let covered = base.present();
    // A category's keyed postings, in this view's own directory — opened on the declaration rather
    // than probed for, the rule every open here keeps.
    let postings = scoped_owes_postings(family)
        .then(|| ColumnPostings::open_keyed(&dir.join("postings.arrow")).map(Arc::new))
        .transpose()?;
    // The same routing the entity-scoped family takes, and for decision 0063's reason rather than
    // a tuning one: a `derived` vocabulary's postings answer *membership* and must not answer the
    // filter, whose work would then be a function of the value named.
    let route = if postings.is_some()
        && scoped_visibility_of(family, vocabularies) == Some(Visibility::Public)
    {
        Route::Postings
    } else {
        Route::Scan
    };
    Ok((
        name,
        placement,
        Layers {
            declared_index,
            layers: vec![Layer {
                values_rel: None,
                values: base,
                dict,
            }],
            covered,
            // **The licence, not the fact that it opened.** A family carrying neither flag is
            // opened so the drill-down can read a value out of it, and `evaluate` gates on this
            // flag — so a leaf that somehow reached it is refused exactly as an unfilterable
            // entity-scoped column's is. `EngineMeta::resolve_filter_column` refuses such a leaf
            // one layer earlier, before any column is looked up; this is the second of the two.
            filterable: scoped_is_filterable(family),
            postings,
            analyser: None,
            text: Vec::new(),
            route,
            family: scoped_family,
        },
    ))
}

/// The `visibility` of the vocabulary a scoped category's codes index — [`visibility_of`]'s
/// question over a family's declaration.
pub(crate) fn scoped_visibility_of(
    scoped: &tessera_store::manifest::ScopedScalar,
    vocabularies: &[tessera_store::manifest::ManifestVocabulary],
) -> Option<Visibility> {
    let name = scoped.vocabulary.as_deref()?;
    vocabularies
        .iter()
        .find(|v| v.name == name)
        .map(|v| v.visibility)
}

/// How a category operand is answered on one column — decided at open from the declaration alone.
///
/// **Not a tuning knob and not a per-request choice.** See this module's header and decision 0063:
/// the postings' work is a function of the value named, which is a disclosure under
/// `visibility = "derived"` and a published fact under `visibility = "public"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    /// Every operand is answered by scanning the value column, layer by layer.
    Scan,
    /// `eq` and `in` over the base are answered by intersecting the derived postings; every other
    /// operand, and every extent layer, is scanned.
    Postings,
}

/// One column as it is served: the build's base column, then one layer per flush that has
/// published since (see this module's header), plus the derived postings where the column has them.
///
/// `Arc` per layer because a publication builds the next generation's columns from the live ones —
/// the base is a memory map of a multi-gigabyte file, and the flush that added one entity must not
/// re-open it.
#[derive(Debug, Clone)]
struct Layers {
    /// This column's position in the manifest's `declared_scalars`.
    ///
    /// **An index internal, and the record blob's addressing key**: a blob row tags each field by
    /// this number rather than by name, so a route that must read a column's *value* out of the
    /// blob — the phrase verify — has no other way to pick its field out of a row. Never
    /// serialised anywhere; drill-down resolves the same tag against the same list.
    declared_index: usize,
    layers: Vec<Layer>,
    /// Every entity any layer holds a value for. Kept so a new extent's disjointness can be
    /// checked in one bitmap operation — see [`FilterColumns::compose`] — rather than trusted.
    covered: Bitmap,
    /// **Declared `index = true`.** A column may be held here without being filterable: a
    /// `visibility = "derived"` category owes membership postings whatever its `index` says
    /// (`filter-index.md` §2.3), and `/v1/categories` reads them from here. [`FilterColumns::resolve`]
    /// refuses such a column exactly as it refuses an undeclared one, so holding it opens no
    /// operand the schema did not declare.
    filterable: bool,
    /// The base build's per-value postings, where the column has them: every category column whose
    /// vocabulary is `derived`, and every category column declared filterable.
    ///
    /// Held whatever the route, because the membership question `/v1/categories` asks is answered
    /// from these on a `derived` column that the *filter* route deliberately does not use them
    /// for.
    postings: Option<Arc<ColumnPostings>>,
    /// The analyser this column was **indexed** with, resolved from the manifest's recorded
    /// identity rather than from a default. A query is analysed with it, which is what makes the
    /// two token streams the same one.
    analyser: Option<Arc<tessera_analyse::Analyser>>,
    /// A `text` column's layers: the base build's index first, then one per flush extent, oldest
    /// first. Held here rather than on a [`Layer`] because that type is a value column and its
    /// dictionary, and this family has neither — its terms are postings and its prose is a blob
    /// row.
    ///
    /// **Disjoint in entity space by I9**, so a `match` unions across them and order decides
    /// nothing; there is no coverage check to keep, because an entity id is never reused and no
    /// two layers can hold the same entity's terms.
    text: Vec<TextLayer>,
    route: Route,
    /// The family whose rules this column's values are read by — carried so a layer's storage can
    /// be checked against its declaration rather than inferred from it. A `keyword` column's layers
    /// each owe a dictionary, and [`FilterColumns::compose`] refuses one that arrives without it:
    /// a `u32` ordinal column read as though it were a category's codes would answer every string
    /// predicate with the empty set, which under-reports silently rather than failing.
    family: Family,
}

/// One layer of a column, and the manifest entry it came from.
///
/// **The path is carried because a coalesce replaces layers by name** (`filter-index.md` §5.2). A
/// `ValueColumn` has no identity of its own, so a pass that consumed eight of a column's extents
/// could not otherwise say which eight of the live generation's layers its output stands for —
/// and matching them by presence instead would be circular, since presence equality is exactly the
/// property [`FilterColumns::with_coalesced`] is checking.
///
/// `None` is the build's base column, which is named in `MANIFEST.files` rather than in
/// `attr_extents` and which no entity-space pass may take (`crate::coalesce`'s module doc).
/// One `text` layer: its own dictionary, its own postings over that dictionary, and the entities
/// it holds a value for.
///
/// **The three travel together because an ordinal is a position in *this* dictionary.** Postings
/// read against another layer's terms would answer every `match` from the wrong words with no
/// symptom, which is why the manifest names them as one record.
#[derive(Debug, Clone)]
struct TextLayer {
    dict: Arc<SortedDict>,
    postings: Arc<ColumnPostings>,
    /// The entities this layer holds a value for. **Stored rather than derived from the postings**:
    /// text that analyses to no terms — an empty string, a field of pure punctuation — carries a
    /// value and appears in no posting.
    ///
    /// **Read by the coalesce's replacement rule** ([`FilterColumns::with_coalesced`]): a
    /// coalesced layer must stand for exactly the entities its inputs did, and presence is the only
    /// thing that says so. Not read by the fold, which rebuilds the base by merging postings and
    /// subtracting the deleted set and needs no coverage; not by `match`, which unions across
    /// layers that are disjoint by **I9**.
    ///
    /// ⊘ **Empty for the base**, which writes no presence file — so it is a layer's coverage and
    /// not the column's. The day a text column gains a presence predicate the base owes one too
    /// ([#123](https://github.com/jennis0/tessera-index/issues/123)).
    present: Bitmap,
    /// The manifest path that named this layer, or `None` for the base build's index — the identity
    /// a coalesce or fold names a layer by, for [`Layer::values_rel`]'s reason.
    dict_rel: Option<String>,
}

/// One text layer's two halves, checked against each other before the layer is served.
///
/// **Posting *i* is term *i*'s, and nothing else says so.** A postings file short of its dictionary
/// answers `None` for every ordinal past the gap — `PostingsReader::posting_at` returns it rather
/// than refusing — so a truncated layer would report every word after the gap as carried by
/// nobody: an under-report with no symptom, which is the shape this codebase refuses everywhere
/// else. The fold makes the same check on its inputs before merging them, and a reader that did not
/// would be trusting an artefact the writer's own consumer will not.
fn text_layer(
    dict: SortedDict,
    postings: ColumnPostings,
    column: &str,
    which: &str,
    present: Bitmap,
    dict_rel: Option<String>,
) -> std::io::Result<TextLayer> {
    if dict.len() != postings.record_count() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "column '{column}': the {which} text layer holds {} terms but {} postings records.                  An ordinal names a position in its own layer's dictionary, so serving these                  together would answer `match` from the wrong words",
                dict.len(),
                postings.record_count()
            ),
        ));
    }
    Ok(TextLayer {
        dict: Arc::new(dict),
        postings: Arc::new(postings),
        present,
        dict_rel,
    })
}

/// The three files one published text extent names, resolved to paths.
#[derive(Debug, Clone)]
pub struct TextExtentPaths {
    pub column: String,
    /// **The manifest path, which is the layer's identity** — what `text_extents` lists and what a
    /// later coalesce names its inputs by. Distinct from `dict` below, which is that path resolved
    /// against the prefix directory: a composition storing the resolved one would leave the two
    /// producers of a layer (a flush's, and `open`'s at startup) naming the same layer differently,
    /// and a coalesce's lookup would find the layer after a restart and miss it after a flush.
    pub dict_rel: String,
    pub dict: std::path::PathBuf,
    pub postings: std::path::PathBuf,
    pub presence: std::path::PathBuf,
}

#[derive(Debug, Clone)]
struct Layer {
    values_rel: Option<String>,
    values: Arc<ValueColumn>,
    /// The layer's own sorted dictionary, for a `keyword` column and nothing else.
    ///
    /// **Held beside the values it numbers, because an ordinal has no meaning apart from them.**
    /// The base and every extent mint their own ordinals, so pairing them here is what makes
    /// "resolve the needle in *this* layer's dictionary, then scan *this* layer's ordinals" the
    /// only expressible order of operations — the alternative, a dictionary per column, would read
    /// an extent's ordinals against the base's keys and recolour the layer with no symptom
    /// (`tessera_filter::SortedDict`'s module doc; records §7).
    dict: Option<Arc<SortedDict>>,
}

/// One column's window of extents, and the coalesced extent that replaces them.
///
/// Named by the manifest's own paths on both sides, which is what makes the replace ABA-safe
/// against the flushes that published while the pass ran: a `seg_id` and the paths derived from it
/// are never reused (contracts §2.1), so a path still listed at publication is still the same bytes.
#[derive(Debug, Clone)]
pub struct CoalescedWindow {
    pub column: String,
    /// The consumed extents' values paths, as `attr_extents` names them.
    pub consumed: Vec<String>,
    /// The coalesced extent's values path.
    pub values_rel: String,
    pub values: Arc<ValueColumn>,
    /// The dictionary the coalesced values are ordinals into — a keyword column's merged
    /// dictionary, `None` for every other family. It arrives beside the values it numbers, as a
    /// flush's [`PublishedExtent`] carries its own, and [`FilterColumns::with_coalesced`] installs
    /// the two as one [`Layer`] or refuses: a keyword window without one has no reading, and a
    /// dictionary on another family's window means the pass and the schema disagree about what
    /// the values are.
    pub dict: Option<Arc<SortedDict>>,
}

/// One text column's window of extents, and the coalesced extent that replaces them.
///
/// Named by dictionary path on both sides — a text layer's identity, and the never-reused one
/// [`CoalescedWindow`] takes for its own reason. The replacement carries its three files rather
/// than an opened layer because a text layer is composed from paths wherever it enters, the flush's
/// extents included ([`FilterColumns::with_extents`]).
#[derive(Debug, Clone)]
pub struct CoalescedTextWindow {
    /// The consumed extents' dictionary paths, as `text_extents` names them.
    pub consumed: Vec<String>,
    /// The replacement, named exactly as a flush's extent is — the column included.
    pub paths: TextExtentPaths,
}

/// A filter expression routed for one request (decision 0068) — see
/// [`FilterColumns::evaluate_routed`].
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
    /// [`Placement`] for why a scan that guessed reads one family's absence as the other's value.
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
            RowExpr::AllOf(kids) | RowExpr::AnyOf(kids) | RowExpr::NotInRows(kids) => {
                for kid in kids {
                    kid.collect_verdicts(out);
                }
            }
            RowExpr::NoneOf { kids, .. } => {
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
            RowExpr::AllOf(kids) | RowExpr::AnyOf(kids) | RowExpr::NotInRows(kids) => kids
                .iter()
                .filter_map(RowExpr::region_verdict)
                .reduce(|a, b| a.coarser(b)),
            RowExpr::NoneOf { kids, .. } => kids
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
/// [`FilterColumns::evaluate_routed`] and on every frame of [`FilterColumns::route`].
pub struct RowLeafResolvers<'a> {
    pub regions: &'a RegionResolver<'a>,
    pub members: &'a MemberResolver<'a>,
}

/// The space a sub-tree evaluates in — [`FilterColumns::space_of`]'s answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Space {
    Entity,
    Row,
    Mixed,
}

/// Why a filter could not be answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterError {
    /// The column is not declared filterable. A caller error, distinguishable from an empty
    /// result, which an undeclared column must never be served as.
    UndeclaredColumn(String),
    /// The expression nests deeper than [`MAX_FILTER_DEPTH`].
    TooDeep { depth: usize, max: usize },
    /// A routed column's postings could not be read. **Fail-closed**: the alternative — falling
    /// back to the scan — would answer correctly and hide that the bundle's accelerator is
    /// unreadable, and the alternative to *that* — an empty result — says no entity carries the
    /// value. Neither is distinguishable from a right answer, so this refuses.
    PostingsUnreadable { column: String, detail: String },
    /// A keyword layer's sorted dictionary refused a read. **Fail-closed, for the reason
    /// [`FilterError::PostingsUnreadable`] gives and one more of its own**: a dictionary that
    /// answered wrongly would resolve a needle to the wrong ordinal and return a different value's
    /// entities, so a read that cannot vouch for its answer must refuse rather than treat the miss
    /// as ordinary. An ordinary miss is not this — it is [`NO_SUCH_ORDINAL`], and it still scans.
    DictionaryUnreadable { column: String, detail: String },
    /// The column has no derived membership postings, so the `derived` visibility predicate
    /// cannot be evaluated for it. Fail-closed for the reason `categories.rs` gives: an empty value
    /// set is what a principal who may see none of them is told.
    MembershipUnavailable(String),
    /// A `none_of` names more or fewer than one column — see [`FilterExpr::NoneOf`].
    NegationSpansColumns { columns: Vec<String> },
    /// A `none_of` names a column with no presence set to subtract from — a `text` column, whose
    /// index is postings over words and whose values are blob rows.
    ///
    /// **Fail-closed, and the alternative is what makes it worth a variant of its own.** A negation
    /// is `present ∖ matched`; with no presence the left operand is empty and every such request
    /// answers "no items" with a 200, which is indistinguishable from a corpus where nothing
    /// matches. Refusing names the column and the reason, so a caller can say what they meant a
    /// different way.
    NegationWithoutPresence { column: String, family: String },
    /// A region leaf reached the entity-space evaluator, which cannot answer it: a region is a
    /// statement about position, and position is row space (I4). Only
    /// [`FilterColumns::evaluate_routed`] takes a tree carrying one.
    RegionInEntitySpace,
    /// The region resolver could not answer a leaf for this generation — a cancelled build, a
    /// view whose segments could not be assembled. Fail-closed: an empty operand here would be
    /// indistinguishable from a shape that holds nothing.
    RegionUnavailable(String),
    /// A `member_of` leaf reached the entity-space evaluator. Row space only, exactly as
    /// [`FilterError::RegionInEntitySpace`] is.
    MemberOfInEntitySpace,
    /// A `member_of` leaf named a layer this principal's `/v1/meta` does not list. **A `422`, and
    /// the caller's fault**: a layer name is deployment schema, resolved through the same probe
    /// that answers alike for a gate-failed name and a never-registered one
    /// (`LayerRegistry::resolve_for`), so refusing by name discloses nothing this principal was
    /// not already told. The *artifact* is a value and is never refused (§3).
    UnknownLayer(String),
    /// The `member_of` resolver could not answer for this generation. Fail-closed, for
    /// [`FilterError::RegionUnavailable`]'s reason.
    MemberOfUnavailable(String),
}

impl std::fmt::Display for FilterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FilterError::UndeclaredColumn(name) => {
                write!(f, "column '{name}' is not declared filterable")
            }
            FilterError::TooDeep { depth, max } => write!(
                f,
                "the filter expression nests {depth} deep; the limit is {max}. Refused rather than \
                 flattened, which would answer a different question"
            ),
            FilterError::PostingsUnreadable { column, detail } => write!(
                f,
                "column '{column}' is routed through its derived postings and they could not be \
                 read ({detail}); refused rather than answered short"
            ),
            FilterError::DictionaryUnreadable { column, detail } => write!(
                f,
                "column '{column}' is a keyword column and one of its layers' sorted dictionaries \
                 could not be read ({detail}); refused rather than answered, because a dictionary \
                 that cannot be trusted resolves a needle to another value's ordinal"
            ),
            FilterError::MembershipUnavailable(column) => write!(
                f,
                "column '{column}' carries no derived membership postings, so its per-viewer value \
                 visibility cannot be derived"
            ),
            FilterError::NegationSpansColumns { columns } => match columns.len() {
                0 => write!(
                    f,
                    "a 'none_of' names no column, so there is no value for an item to be required \
                     to carry. 'none_of' means *carries a value in this column, and none of these \
                     matches it*, which needs a column to be about"
                ),
                _ => write!(
                    f,
                    "a 'none_of' names {} columns ({}), and it may name only one: it requires the \
                     item to carry a value in the column it negates, and two columns give two \
                     answers to which. Write all_of: [{{none_of: [...]}}, {{none_of: [...]}}], \
                     which is the same set and says which presence each clause requires",
                    columns.len(),
                    columns.join(", ")
                ),
            },
            FilterError::NegationWithoutPresence { column, family } => write!(
                f,
                "a 'none_of' names column '{column}', which is a {family} column and stores no \
                 per-item value to be present or absent — its index is the words its documents \
                 use. 'none_of' means *carries a value in this column, and none of these matches \
                 it*, and there is nothing here to answer the first half. Say what is wanted with \
                 a positive expression instead"
            ),
            FilterError::RegionInEntitySpace => write!(
                f,
                "a 'region' leaf is answered in row space over the whole view and cannot be \
                 evaluated by the entity-space evaluator"
            ),
            FilterError::RegionUnavailable(detail) => write!(
                f,
                "the 'region' leaf could not be resolved against this generation ({detail}); \
                 refused rather than answered empty"
            ),
            FilterError::MemberOfInEntitySpace => write!(
                f,
                "a 'member_of' leaf is answered in row space over the whole view and cannot be \
                 evaluated by the entity-space evaluator"
            ),
            FilterError::UnknownLayer(layer) => write!(
                f,
                "'member_of' names layer '{layer}', which this deployment does not publish to you \
                 — /v1/meta lists the layers a 'member_of' leaf may name. The *artifact* it names \
                 is never refused: an identifier that resolves to nothing you may see is an empty \
                 operand"
            ),
            FilterError::MemberOfUnavailable(detail) => write!(
                f,
                "the 'member_of' leaf could not be resolved against this generation ({detail}); \
                 refused rather than answered empty"
            ),
        }
    }
}

impl FilterError {
    /// Is this the caller's fault or the deployment's?
    ///
    /// **The distinction decides a status code, so it lives with the variants rather than at the
    /// mapping.** A malformed expression is a `422` — the caller can fix it, and refusing tells
    /// them nothing about the corpus, since a column's existence and its family are deployment
    /// schema published to every principal alike (`/v1/meta`). An artefact that cannot be read is a
    /// `500` — fail-closed, because the alternatives are answering short or answering empty and
    /// neither is distinguishable from a right answer.
    ///
    /// Note which side [`FilterError::UndeclaredColumn`] falls on: the *name* of a filterable
    /// column is public, so refusing by name discloses nothing. An unknown **value** is a different
    /// matter entirely and is never an error at all — it is an empty operand, because refusing it
    /// would make the filter an existence oracle over exactly what `visibility = "derived"` hides.
    pub fn is_callers_fault(&self) -> bool {
        match self {
            FilterError::UndeclaredColumn(_)
            | FilterError::TooDeep { .. }
            | FilterError::NegationSpansColumns { .. }
            | FilterError::NegationWithoutPresence { .. }
            | FilterError::UnknownLayer(_) => true,
            FilterError::PostingsUnreadable { .. }
            | FilterError::DictionaryUnreadable { .. }
            | FilterError::MembershipUnavailable(_)
            | FilterError::RegionInEntitySpace
            | FilterError::RegionUnavailable(_)
            | FilterError::MemberOfInEntitySpace
            | FilterError::MemberOfUnavailable(_) => false,
        }
    }
}

impl std::error::Error for FilterError {}

/// The `visibility` of the vocabulary a column draws from, or `None` where it is not a category.
///
/// Read from the **vocabulary**, which is the object that carries it. A column naming a vocabulary
/// the manifest does not hold is refused at seed (`Vocabularies::seed`), so the `None` this returns
/// for one means "not a category" and nothing else.
fn visibility_of(
    scalar: &tessera_store::manifest::DeclaredScalar,
    vocabularies: &[tessera_store::manifest::ManifestVocabulary],
) -> Option<Visibility> {
    let name = scalar.vocabulary.as_deref()?;
    vocabularies
        .iter()
        .find(|v| v.name == name)
        .map(|v| v.visibility)
}

/// Does the build write a value column for this column?
///
/// **The mirror of `tessera_build`'s `postings_are_owed`, and it must stay one.** Reading a file set
/// the build did not write is a refusal at open; failing to read one it did write is a column whose
/// values are on disk and unserved. Two reasons, and the second is the one a reader will not expect:
/// `index = true` is the obvious one, and `visibility = "derived"` is the other — that
/// control's gate is membership-derived (per-point-attributes §3.3) and the member sets it needs are
/// the postings derived from this column, so it gets both whatever its `index` says
/// (`filter-index.md` §2.3).
pub(crate) fn owes_value_column(
    scalar: &tessera_store::manifest::DeclaredScalar,
    vocabularies: &[tessera_store::manifest::ManifestVocabulary],
) -> bool {
    // **Text owes none, and that is the family's defining property rather than an exception to be
    // remembered at each call site.** Every other indexed family stores one value per entity; a
    // text field has many terms per entity and no per-entity slot at all, so its index is a token
    // dictionary and postings over it and its value is a blob row. Every pass that iterates "the
    // columns with a value column" — the flush's extent writer, the fold's merge — would otherwise
    // reach for a `values.bin` that no writer has ever produced.
    if scalar.arrow_type == tessera_spatial::tiler::ScalarType::Text {
        return false;
    }
    scalar.index || visibility_of(scalar, vocabularies) == Some(Visibility::Derived)
}

/// Does this column's value live in the record blob? **A field is blob-resident exactly when it has
/// no other home** (records §3, and §4.2's owner ruling of 2026-08-12) — no hot column, and no
/// entity-space structure.
///
/// **Categories are not exempt, and reading the exemption as the family's is the bug this function
/// exists to prevent.** §4.2's floor belongs to a category's *readers* — `/v1/categories` and the
/// `derived` gate — not to the family: the entity-space structures are granted to an `index`ed
/// or `derived` category, so a **`public` category declared with neither flag has no reader and
/// no floor**. Excluding every category here leaves that shape with nowhere to store a value, which
/// no declaration refuses: the build writes the field, the flush drops it, and drill-down shows it
/// for built items and omits it for ingested ones.
///
/// `tessera_build::pipeline::postings_are_owed` is this predicate's other half, over the build's
/// own schema types, and the two must agree. They differ only in the type they read, and a build
/// that placed a field differently from the engine would write bytes the fold cannot find.
pub(crate) fn blob_resident(
    scalar: &tessera_store::manifest::DeclaredScalar,
    vocabularies: &[tessera_store::manifest::ManifestVocabulary],
) -> bool {
    // **Text is blob-resident whether or not it is indexed** (records §4.4). Its index is postings
    // over terms, which answer `match` and reconstruct nothing — only the blob can answer
    // `entity → value` for prose, so an indexed text column has both homes rather than one.
    if scalar.arrow_type == tessera_spatial::tiler::ScalarType::Text {
        return true;
    }
    !scalar.render && !owes_value_column(scalar, vocabularies)
}

/// Does the build write derived postings for this column? Only a category earns them — a string's
/// values carry no identity a posting could be keyed by, and a numeric's are near-unique
/// (`filter-index.md` §2.3).
pub(crate) fn owes_postings(
    scalar: &tessera_store::manifest::DeclaredScalar,
    vocabularies: &[tessera_store::manifest::ManifestVocabulary],
) -> bool {
    scalar.vocabulary.is_some() && owes_value_column(scalar, vocabularies)
}

/// **The analyser that indexed a text column, not the default** — one resolution for the
/// entity-scoped column and the group-scoped family alike, so neither can come to open with a
/// pipeline the other would refuse.
///
/// A bundle records the identity its build resolved; opening with anything else would answer
/// `match` against a token stream the index was not built from. Both failures are refusals rather
/// than fallbacks: a text column with no recorded identity is a bundle that is not what its
/// manifest says, and an identity this binary does not carry is terms it cannot reproduce.
fn resolve_analyser(
    column: &str,
    identity: Option<&str>,
) -> std::io::Result<Arc<tessera_analyse::Analyser>> {
    let identity = identity.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "column '{column}' is text but the manifest records no analyser identity — the \
                 build is what resolves one, so this bundle is not what its manifest says"
            ),
        )
    })?;
    tessera_analyse::analyser(identity.split('/').next().unwrap_or_default())
        .filter(|a| a.identity() == identity)
        .map(Arc::new)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "column '{column}' was indexed by analyser '{identity}', which this binary \
                     does not carry. Its terms cannot be reproduced, so every `match` over it \
                     would answer from a different segmentation"
                ),
            )
        })
}

/// The request path's two modes, and only those: `MappedSequential` is the fold's and is
/// deliberately unreachable from here (decision 0052).
fn request_access(mmap: bool) -> tessera_filter::Access {
    if mmap {
        tessera_filter::Access::Mapped
    } else {
        tessera_filter::Access::Read
    }
}

/// A record-blob open failure, in the `io::Result` this opener speaks. Fail-closed either way:
/// a missing, short or malformed layer refuses the whole open (records §3), never "those
/// entities have no record".
/// The stack a column declared at a running service opens with before any fold: no base, no
/// postings, the extents composed later (`ingest.md` §6.3). `None` for a column with no
/// entity-space home, which holds no stack at all.
fn runtime_layers(
    scalar: &tessera_store::manifest::DeclaredScalar,
    declared_index: usize,
    vocabularies: &[tessera_store::manifest::ManifestVocabulary],
) -> std::io::Result<Option<Layers>> {
    let family = Family::of(scalar);
    if family == Family::Text {
        if !scalar.index {
            return Ok(None);
        }
        let analyser = Some(resolve_analyser(&scalar.name, scalar.analyser.as_deref())?);
        return Ok(Some(Layers {
            declared_index,
            layers: Vec::new(),
            covered: Bitmap::new(),
            filterable: true,
            postings: None,
            analyser,
            text: Vec::new(),
            route: Route::Postings,
            family,
        }));
    }
    if !owes_value_column(scalar, vocabularies) {
        return Ok(None);
    }
    let row = scalar.render && family.reaches_hot_column();
    Ok(Some(Layers {
        declared_index,
        layers: Vec::new(),
        covered: Bitmap::new(),
        filterable: scalar.index || row,
        postings: None,
        analyser: None,
        text: Vec::new(),
        // Every operand is a scan until the fold rebuilds the postings from the folded column.
        route: Route::Scan,
        family,
    }))
}

fn record_open_error(e: tessera_filter::RecordError) -> std::io::Error {
    match e {
        tessera_filter::RecordError::Io(io) => io,
        malformed => std::io::Error::new(std::io::ErrorKind::InvalidData, malformed.to_string()),
    }
}

/// One flushed extent, as publication hands it over: the column it extends, the prefix-relative
/// path of its values, the opened column, and — for a keyword — the dictionary those values are
/// ordinals into.
///
/// **The dictionary travels with the values or not at all**, which is why this is one tuple rather
/// than two arguments that could disagree: an extent's ordinals are positions in its own
/// dictionary and name nothing against any other (`records-and-search.md` §4.3), so composition
/// refuses a half. On disc the same pairing is `AttrExtent`'s single record.
pub type PublishedExtent = (String, String, Arc<ValueColumn>, Option<Arc<SortedDict>>);

/// The one rule under which a layer may join a column: a keyword layer brings its own dictionary,
/// and no other family's layer brings one.
///
/// **This is where "no ordinal is resolved against a dictionary other than the one that minted
/// it" is enforced**, for every route a layer takes into the live composition — a flush's extent
/// ([`FilterColumns::compose`]) and a coalesce's replacement ([`FilterColumns::with_coalesced`])
/// both pass through here, and both then build one [`Layer`] from the pair. A [`Layer`] is the
/// only thing a scan or a drill-down reads a dictionary from, and it is constructed nowhere a
/// dictionary could arrive apart from the values it numbers. A keyword layer without one is
/// refused rather than scanned as codes, which would answer every string predicate with the empty
/// set; a dictionary on another family's layer is refused because the caller and the schema
/// disagree about what the values are.
fn check_dictionary_pairing(family: Family, column: &str, has_dict: bool) -> std::io::Result<()> {
    match (family == Family::Keyword, has_dict) {
        (true, false) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "a layer for keyword column '{column}' carries no sorted dictionary; its values \
                 are ordinals into the dictionary minted beside them (records §4.3, §7), and a \
                 layer without one has no reading"
            ),
        )),
        (false, true) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "a layer for column '{column}' carries a sorted dictionary, but the schema does \
                 not declare the column a keyword; the two disagree about what its values are"
            ),
        )),
        _ => Ok(()),
    }
}

impl FilterColumns {
    /// Open every filter column the manifest declares, with every extent the partition's
    /// side-manifest names.
    ///
    /// A declared column whose files are missing is an **error**, not an absence: the manifest
    /// digests them, so a missing one means the bundle is not what its manifest says it is. The
    /// same rule covers an extent, and there it is the whole safety argument — an extent that
    /// failed to open and was skipped would answer "those entities carry no value", which is
    /// indistinguishable from a correct answer.
    ///
    /// **`mmap` decides whether a declared column costs resident memory before anyone filters on
    /// it.** Every declared column is opened here, at once, and a value column is 1 GB per byte of
    /// declared width per 10⁹ entities — so reading them into the heap would make sixteen declared
    /// columns tens of GB of residency paid at open by a deployment that may never issue a filter.
    /// Mapped, the pages are faulted in by the scans that touch them and reclaimable under
    /// pressure. The engine passes `true`; tests that build a column and read it back in the same
    /// process pass `false`, exactly as they do for `PostingsReader::open`.
    ///
    /// **It stays a `bool` where the reader beneath it takes a three-way [`tessera_filter::Access`],
    /// and that is the point.** The third case is `MappedSequential`, the fold's `MADV_SEQUENTIAL`,
    /// and [decision 0052](../../../docs/decisions/0052-the-folds-page-cache-mitigation-is-a-hint-not-a-throttle.md)
    /// rules that it belongs only to mappings the fold owns and must never be applied to these —
    /// which are the request path's. A `bool` here cannot express it, so the rule is enforced by the
    /// signature rather than by a comment asking the next caller to remember it.
    #[allow(clippy::too_many_arguments)] // One argument per artefact class the manifest names;
                                         // bundling them into a struct would be a second shape to keep in step with the manifest.
    pub fn open(
        prefix_dir: &Path,
        partition: &str,
        declared: &[tessera_store::manifest::DeclaredScalar],
        // Every group's scoped column families, in manifest order (`views.md` §5).
        scoped: &[tessera_store::manifest::ScopedScalar],
        // Which incarnation each view is, from the roster (`Manifest::incarnation_of`,
        // decision 0115). A family names the views that have a column; this is what places one on
        // disc, a recreated key's base living beside its predecessor's rather than over it.
        view_incarnation: &dyn Fn(&str) -> Option<tessera_types::view::ViewIncarnation>,
        vocabularies: &[tessera_store::manifest::ManifestVocabulary],
        extents: &[tessera_store::manifest::AttrExtent],
        record_extents: &[tessera_store::manifest::RecordExtent],
        artifact_record_extents: &[tessera_store::manifest::RecordExtent],
        entity_terms_extents: &[tessera_store::manifest::EntityTermsExtent],
        text_extents: &[tessera_store::manifest::TextExtent],
        // The entity-scoped columns declared at a running service that no fold has written a
        // base for: the side manifest's `attributes`, by name (`ingest.md` §6.3). Each opens as
        // an empty stack the extents compose onto; every other declared column's base is
        // demanded.
        unfolded: &[String],
        mmap: bool,
    ) -> std::io::Result<Self> {
        let partition_dir = prefix_dir.join("partitions").join(partition);
        let mut columns = BTreeMap::new();
        let mut placements = BTreeMap::new();
        for (declared_index, scalar) in declared.iter().enumerate() {
            // The route affordances, from the compiled declaration alone (decision 0068). A
            // rendered column always affords the row route — its values are in the hot column,
            // and both families that reach it can express absence there. The entity route needs an
            // entity-space value column AND a licence to answer a filter from it — `index`, or
            // 0068's "render implies filterable" over the per-viewer vocabulary floor. A
            // `derived` column with neither flag keeps its value column for membership and
            // stays unfilterable, exactly as before.
            let family = Family::of(scalar);
            let row = scalar.render && family.reaches_hot_column();
            // Text is entity-space filterable without a value column: its `match` is answered from
            // postings, which is the one route in this system that reads no per-entity slot.
            let entity = if family == Family::Text {
                scalar.index
            } else {
                owes_value_column(scalar, vocabularies) && (scalar.index || row)
            };
            if row || entity {
                placements.insert(
                    scalar.name.clone(),
                    Placement {
                        entity,
                        row,
                        family,
                    },
                );
            }
            // **A column declared at a running service and not yet folded has no base**
            // (`ingest.md` §6.3): its stack starts empty and the extents the flushes since the
            // declaration published compose onto it below. A base is demanded from the fold
            // onward, when the side manifest no longer names the column.
            if unfolded.iter().any(|name| name == &scalar.name) {
                if let Some(layers) = runtime_layers(scalar, declared_index, vocabularies)? {
                    columns.insert(scalar.name.clone(), layers);
                }
                continue;
            }
            // **Text opens before the value-column gate, because it owes none.** Its entity-space
            // artefacts are a token dictionary and postings over it; the prose is a blob row. Both
            // are opened on the declaration rather than probed for, the same rule every other open
            // here keeps: a column the manifest says is indexed and whose index is absent is a
            // bundle that is not what its manifest says, and reading that as "no entity matches"
            // would answer a `match` wrongly while looking right.
            if family == Family::Text {
                if !scalar.index {
                    continue;
                }
                let dir = partition_dir.join("attrs").join(&scalar.name);
                // The base build's layer, then one per published extent, oldest first.
                let mut text_layers = vec![text_layer(
                    SortedDict::open_dir(&dir, request_access(mmap))?,
                    // Positional, not keyed: a token ordinal is a dense position in this
                    // dictionary, where a category's code is a scattered vocabulary entry (§2.5).
                    ColumnPostings::open(&dir.join("postings.arrow"), mmap)?,
                    &scalar.name,
                    "base",
                    // The base writes no presence file of its own — the build writes none and the
                    // fold therefore writes none — so nothing here can say which entities carry a
                    // value. See `TextLayer::present`.
                    Bitmap::new(),
                    None,
                )?];
                for extent in text_extents
                    .iter()
                    .filter(|e| e.column == scalar.name && e.view.is_none())
                {
                    text_layers.push(text_layer(
                        SortedDict::open(&prefix_dir.join(&extent.dict), request_access(mmap))?,
                        ColumnPostings::open(&prefix_dir.join(&extent.postings), mmap)?,
                        &scalar.name,
                        &extent.dict,
                        Bitmap::deserialize::<croaring::Portable>(&std::fs::read(
                            prefix_dir.join(&extent.presence),
                        )?),
                        Some(extent.dict.clone()),
                    )?);
                }
                let analyser = Some(resolve_analyser(&scalar.name, scalar.analyser.as_deref())?);
                columns.insert(
                    scalar.name.clone(),
                    Layers {
                        declared_index,
                        layers: Vec::new(),
                        covered: Bitmap::new(),
                        filterable: true,
                        postings: None,
                        analyser,
                        text: text_layers,
                        route: Route::Postings,
                        family,
                    },
                );
                continue;
            }
            if !owes_value_column(scalar, vocabularies) {
                continue;
            }
            let dir = partition_dir.join("attrs").join(&scalar.name);
            let base = Arc::new(ValueColumn::open_dir(&dir, request_access(mmap))?);
            // The base layer's dictionary sits in the column's own directory under the canonical
            // name, exactly where the values do. Opened on the declaration rather than probed for:
            // a keyword column whose dictionary is missing is a bundle that is not what its
            // manifest says, and reading its ordinal column without one would answer every string
            // predicate with the empty set — a wrong answer wearing a correct one's clothes, the
            // failure every open in this function refuses instead.
            let base_dict = (family == Family::Keyword)
                .then(|| SortedDict::open_dir(&dir, request_access(mmap)).map(Arc::new))
                .transpose()?;
            let covered = base.present();
            // Opened whenever the build owed them, and a missing file is an error for the same
            // reason a missing value column is: the manifest digests them, so absence means the
            // bundle is not what its manifest says. A column routed through postings that silently
            // fell back to the scan would answer correctly and hide a broken artefact; one that
            // read an absent file as the empty set would answer that no entity carries the value.
            let postings = owes_postings(scalar, vocabularies)
                .then(|| ColumnPostings::open_keyed(&dir.join("postings.arrow")).map(Arc::new))
                .transpose()?;
            let route = if postings.is_some()
                && visibility_of(scalar, vocabularies) == Some(Visibility::Public)
            {
                Route::Postings
            } else {
                Route::Scan
            };
            columns.insert(
                scalar.name.clone(),
                Layers {
                    declared_index,
                    layers: vec![Layer {
                        values_rel: None,
                        values: base,
                        dict: base_dict,
                    }],
                    covered,
                    filterable: entity,
                    postings,
                    analyser: None,
                    text: Vec::new(),
                    route,
                    family,
                },
            );
        }
        // ---- the group-scoped column families (`views.md` §5) ------------------------------
        //
        // **One column per view, opened under its resolved name**, so a scoped leaf evaluates
        // through exactly the machinery an unscoped one of its family does: the same
        // `ValueColumn`, the same scan, the same presence rules for absence. The only thing the
        // scope decides is which file — which is what keeps the attribute inside I2's argument
        // unchanged, every value being indexed by entity and every predicate answering a bitmap
        // in entity space that the mask meets before any permutation.
        //
        // **Every family with a value column on disc is opened; only a filterable one takes a
        // placement.** The drill-down serves a scoped family's values whatever its flags (owner
        // ruling 2026-09-01), which is what gives a declaration with neither `index` nor `render`
        // its meaning — stored, served at `POST /v1/items`, on no filter surface and in no row
        // tail. Holding a column without a placement is exactly the standing an entity-scoped
        // `derived` category with neither flag already has: `placement` is `None`, `resolve`
        // refuses the name as undeclared, and `stored_value` answers from it.
        //
        // `text` is the one family skipped, and skipped because there is nothing to read: it has
        // no per-entity value slot at all, so no drill-down could serve it either. An unindexed
        // scoped `text` column is refused at the declaration, so a text family here is always
        // filterable and always takes the branch below.
        for family in scoped {
            if !family.has_value_column() && !scoped_is_filterable(family) {
                continue;
            }
            for view_id in &family.views {
                // **No incarnation, no column** (decision 0115): a family naming a view the
                // roster cannot place is a bundle whose two halves disagree, and opening it under
                // a guessed incarnation is how a dropped view's values reach a live one.
                let Some(incarnation) = view_incarnation(view_id) else {
                    continue;
                };
                let (name, placement, layers) = open_scoped_column(
                    &partition_dir,
                    family,
                    view_id,
                    incarnation,
                    vocabularies,
                    mmap,
                )?;
                if scoped_is_filterable(family) {
                    placements.insert(name.clone(), placement);
                }

                columns.insert(name, layers);
            }
        }
        // The record blob's base is owed exactly when the compiled schema has a blob-resident
        // column — one with no other home ([`blob_resident`], records §3). Derived from the schema
        // rather than probed for on disk, so a missing base is a refusal at open, never "those
        // entities have no record".
        // A column declared at a running service has extents alone until a fold writes the
        // base, so only a column the build or a fold declared makes the base owed.
        let blob_resident = declared.iter().any(|d| {
            !unfolded.iter().any(|name| name == &d.name) && blob_resident(d, vocabularies)
        });
        let record_dir = partition_dir.join("attrs").join("record");
        // **Both lists, one stack.** Artifact content extents hold the same format and the same
        // reader as a point's; they are listed separately because their *ownership* differs (see
        // `SegmentsManifest::artifact_record_extents`), not their bytes. Opening them together is
        // what makes `fields_of` answer for an artifact entity, and it is safe because the two
        // never share one: artifact ids descend from the ceiling, point ids ascend from zero.
        let extent_paths: Vec<RecordExtentPaths> = record_extents
            .iter()
            .chain(artifact_record_extents.iter())
            .map(|e| RecordExtentPaths {
                blocks: prefix_dir.join(&e.blocks),
                hasrow: prefix_dir.join(&e.hasrow),
                directory: prefix_dir.join(&e.directory),
            })
            .collect();
        let records = RecordStack::open(
            blob_resident.then_some(record_dir.as_path()),
            &extent_paths,
            request_access(mmap),
        )
        .map_err(record_open_error)?;
        // **The transpose's base is unconditional**, where the blob's is schema-dependent: every
        // entity has a label set, so a build always writes one. A bundle that lacks it refuses the
        // open rather than reading as "no entity carries a term" — the fail-open direction on the
        // write path, where the join rule's label arm compares against it (`views.md` §4).
        let entity_terms = tessera_store::EntityTermsStack::open(
            Some(&partition_dir.join(tessera_store::ENTITY_TERMS_DIR)),
            &entity_terms_extents
                .iter()
                .map(|e| tessera_store::EntityTermsExtentPaths {
                    hasrow: prefix_dir.join(&e.hasrow),
                    offsets: prefix_dir.join(&e.offsets),
                    terms: prefix_dir.join(&e.terms),
                })
                .collect::<Vec<_>>(),
        )
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        let mut open = FilterColumns {
            columns,
            placements,
            access: request_access(mmap),
            records: Arc::new(records),
            entity_terms: Arc::new(entity_terms),
        };
        for extent in extents {
            let column = tessera_filter::open_extent(
                &prefix_dir.join(&extent.values),
                &prefix_dir.join(&extent.presence),
                request_access(mmap),
            )?;
            // An extent's dictionary is named by the manifest rather than derived from the values
            // path: `AttrExtent::column` carries the same rule for the column name, and a path
            // parsed back out of another path is one the manifest no longer digests.
            let dict = extent
                .dict
                .as_ref()
                .map(|rel| {
                    SortedDict::open(&prefix_dir.join(rel), request_access(mmap)).map(Arc::new)
                })
                .transpose()?;
            open.compose(
                &extent_column_name(&extent.column, extent.view.as_deref()),
                &extent.values,
                Arc::new(column),
                dict,
            )?;
        }
        Ok(open)
    }

    /// The record-blob stack this prefix serves drill-down from — see this type's doc for why it
    /// lives here. Empty (zero layers) when the schema has no blob-resident column and no flush
    /// has published an extent.
    pub fn records(&self) -> &RecordStack {
        &self.records
    }

    /// The entity→term transpose this prefix answers a label question from — see this type's doc
    /// for why it lives here.
    pub fn entity_terms(&self) -> &tessera_store::EntityTermsStack {
        &self.entity_terms
    }

    /// The access mode every layer of this generation was opened with — what a re-derived stack
    /// must be opened with too, or a publication would swap a mapped reader for a read one under
    /// a live request.
    pub(crate) fn access(&self) -> tessera_filter::Access {
        self.access
    }

    /// How many layers the record blob's stack holds — the base, if the schema has a
    /// blob-resident column, plus one per published extent. Operator- and test-facing: nothing on
    /// the wire carries it, and it is how a coalesce's publication can be asserted to have
    /// *shrunk* the live stack rather than merely the manifest.
    pub fn record_layers(&self) -> usize {
        self.records.layer_count()
    }

    /// How many record-blob rows drill-down and the join rule have decoded through this
    /// generation's stack — [`RecordStack::reads`]. Test-facing: the reader that answers a
    /// column's absence from the segment schema is asserted never to move it (`ingest.md` §6.3).
    pub fn record_reads(&self) -> u64 {
        self.records.reads()
    }

    /// The route affordances of one filterable column, or `None` where the column is not
    /// filterable at all — the same distinction [`FilterColumns::resolve`] refuses on.
    pub fn placement(&self, column: &str) -> Option<Placement> {
        self.placements.get(column).copied()
    }

    /// The entity-space value `column` stores for `entity`, at its storage type, or `None` where
    /// no layer holds one — drill-down's entity-space home (records §3).
    ///
    /// **Every column with a value column answers, filterable or not**: a `derived` category
    /// with neither flag still stores its codes here, and the caller has already established the
    /// *item* visible, which is exactly the membership condition §3.3 derives value visibility
    /// from — a visible entity carrying the value is the witness that offers it.
    ///
    /// The slot arithmetic is the presence-rank rule of `filter-index.md` §2.1, computed through
    /// the column's own public surface: the count of present entities strictly below this one is
    /// its slot, for a universal column (where it degenerates to the entity id) and a partial one
    /// alike. O(containers below the entity) per read — drill-down cadence, never per mark.
    /// Does any **flushed** `text` layer of `column` hold prose for `entity`?
    ///
    /// **The scoped cell arm's fail-closed source for a text family** (`views.md` §5, decision
    /// 0116). A text column has no per-entity value to read back — its extent is a dictionary,
    /// postings and this presence bitmap — so the cell arm cannot compare a supplied string with
    /// a stored one across a flush boundary. What it can establish is *occupancy*, and that is
    /// what this answers: a joining row supplying prose for a cell some layer already holds prose
    /// for is refused rather than admitted unchecked, because admitting it would write a second
    /// text layer stamped with the same view and `match` unions across layers silently.
    ///
    /// ⊘ **The base is not covered, and cannot be**: it writes no presence file at all
    /// ([`TextLayer::present`]'s own marker, issue #123), so a cell whose only prose came from the
    /// build reads as empty here. Closing that needs the base's presence bitmap, not a change to
    /// this rule.
    pub(crate) fn text_present(&self, column: &str, entity: u32) -> bool {
        let Some(layers) = self.columns.get(column) else {
            return false;
        };
        layers
            .text
            .iter()
            .any(|layer| layer.present.contains(entity))
    }

    pub(crate) fn stored_value(&self, column: &str, entity: u32) -> Option<RecordValue> {
        let layers = self.columns.get(column)?;
        let probe = Bitmap::of(&[entity]);
        for layer in &layers.layers {
            let values = &layer.values;
            if values.present_in(&probe).is_empty() {
                continue;
            }
            // **A keyword's ordinal never crosses the trust boundary**, so drill-down is served the
            // key it names rather than the number (records §4.3; **I10**). Decoded against *this*
            // layer's dictionary, which is the only one that numbers it. A dictionary that refuses
            // leaves the field with no value: under-reporting, which narrows, where the alternative
            // would publish an index internal.
            if let Some(dict) = &layer.dict {
                let ordinal = values.value_of(entity)?.raw();
                let mut scratch = Vec::new();
                return dict
                    .key_of(ordinal, &mut scratch)
                    .ok()
                    .map(|key| RecordValue::Utf8(key.to_string()));
            }
            // The layers are disjoint in entity space (I9, checked at compose), so the first
            // layer holding the entity is the only one.
            let slot = values
                .present_in(&Bitmap::from_range(0..entity))
                .cardinality() as usize;
            let read = match values.codes() {
                Codes::U8(v) => RecordValue::U8(v[slot]),
                Codes::U16(v) => RecordValue::U16(v[slot]),
                Codes::U32(v) => RecordValue::U32(v[slot]),
                Codes::U64(v) => RecordValue::U64(v[slot]),
                Codes::I8(v) => RecordValue::I8(v[slot]),
                Codes::I16(v) => RecordValue::I16(v[slot]),
                Codes::I32(v) => RecordValue::I32(v[slot]),
                Codes::I64(v) => RecordValue::I64(v[slot]),
                Codes::F32(v) => RecordValue::F32(v[slot]),
                Codes::F64(v) => RecordValue::F64(v[slot]),
            };
            return Some(read);
        }
        None
    }

    /// Add one flush's extent to a column, refusing an entity two layers both claim.
    ///
    /// **The refusal is what keeps a layered column a function.** Entity ids are permanent and
    /// issued from the high-water (**I9**), so an extent's entities belong to no earlier layer and
    /// the overlap is unreachable — which is exactly why it is checked here rather than reasoned
    /// about at the call site: if I9 ever failed, the symptom would be an entity matching two
    /// values at once and a filter naming either returning it, with nothing to notice.
    ///
    /// **A keyword extent must bring its own dictionary, and one that does not is refused rather
    /// than composed** ([`check_dictionary_pairing`]). Its values are ordinals into a dictionary
    /// this flush minted, so a layer without one has no reading at all: scanned as codes it would
    /// answer every string predicate with the empty set, and resolved against the base's keys it
    /// would return another value's entities.
    fn compose(
        &mut self,
        column: &str,
        values_rel: &str,
        extent: Arc<ValueColumn>,
        dict: Option<Arc<SortedDict>>,
    ) -> std::io::Result<()> {
        let Some(layers) = self.columns.get_mut(column) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "a filter extent names column '{column}', which the schema does not declare \
                     filterable"
                ),
            ));
        };
        check_dictionary_pairing(layers.family, column, dict.is_some())?;
        let present = extent.present();
        if layers.covered.and_cardinality(&present) != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "a filter extent for column '{column}' claims entities an earlier layer \
                     already holds values for; entity ids are permanent (I9) and an extent may \
                     only add ids no layer holds"
                ),
            ));
        }
        layers.covered |= present;
        layers.layers.push(Layer {
            values_rel: Some(values_rel.to_string()),
            values: extent,
            dict,
        });
        Ok(())
    }

    /// This generation's columns with an attribute column declared at a running service added,
    /// at its position in the served schema (`ingest.md` §1.3, §6.3): an empty stack the next
    /// flush's extent composes onto, and the placement its flags afford. A column with no
    /// entity-space home (rendered or blob-resident and not indexed) takes a placement or nothing.
    pub(crate) fn with_runtime_column(
        &self,
        scalar: &tessera_store::manifest::DeclaredScalar,
        declared_index: usize,
        vocabularies: &[tessera_store::manifest::ManifestVocabulary],
    ) -> std::io::Result<FilterColumns> {
        let mut next = FilterColumns {
            columns: self.columns.clone(),
            placements: self.placements.clone(),
            access: self.access,
            records: Arc::clone(&self.records),
            entity_terms: Arc::clone(&self.entity_terms),
        };
        let family = Family::of(scalar);
        let row = scalar.render && family.reaches_hot_column();
        let entity = if family == Family::Text {
            scalar.index
        } else {
            owes_value_column(scalar, vocabularies) && (scalar.index || row)
        };
        if row || entity {
            next.placements.insert(
                scalar.name.clone(),
                Placement {
                    entity,
                    row,
                    family,
                },
            );
        }
        if let Some(layers) = runtime_layers(scalar, declared_index, vocabularies)? {
            next.columns.insert(scalar.name.clone(), layers);
        }
        Ok(next)
    }

    /// This generation's columns with a **newly based** group-scoped column opened onto them —
    /// the first flush of a view a family had no column for (`views.md` §5).
    ///
    /// **Applied before the extents compose, and that order is the whole of it.** A flush of a
    /// view created since the build writes the family's base and its own extent in one unit; the
    /// extent composes onto a column, so the column has to exist first. A `(column, view)` pair
    /// this generation already holds is a no-op rather than a refusal — a re-publication reaching
    /// the same state — because the base is written once and named by its files, not by a counter.
    pub fn with_scoped_columns(
        &self,
        partition_dir: &Path,
        columns: &[(String, String, tessera_types::view::ViewIncarnation)],
        scoped: &[tessera_store::manifest::ScopedScalar],
        vocabularies: &[tessera_store::manifest::ManifestVocabulary],
        mmap: bool,
    ) -> std::io::Result<FilterColumns> {
        let mut next = FilterColumns {
            columns: self.columns.clone(),
            placements: self.placements.clone(),
            access: self.access,
            records: Arc::clone(&self.records),
            entity_terms: Arc::clone(&self.entity_terms),
        };
        for (column, view, incarnation) in columns {
            let Some(family) = scoped.iter().find(|f| f.name == *column) else {
                return std::io::Result::Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "a flush wrote a base for the group-scoped column family '{column}' under \
                         view '{view}', which this bundle does not declare"
                    ),
                ));
            };
            if !scoped_is_filterable(family) && !scoped_has_value_column(family) {
                continue;
            }
            if next.columns.contains_key(&scoped_column_name(column, view)) {
                continue;
            }
            let (name, placement, layers) = open_scoped_column(
                partition_dir,
                family,
                view,
                *incarnation,
                vocabularies,
                mmap,
            )?;
            next.placements.insert(name.clone(), placement);
            next.columns.insert(name, layers);
        }
        Ok(next)
    }

    /// This generation's columns with one flush's extents added — the successor generation's.
    ///
    /// Cheap by construction: the base columns are `Arc`s, so a flush that published one entity
    /// clones pointers rather than re-opening a memory-mapped column per declared attribute. Each
    /// extent is `(column, values path, opened column)`, the path being what the manifest names it
    /// by and what a later coalesce replaces it by.
    ///
    /// A keyword column's extent carries the dictionary the flush minted beside the values it
    /// numbers, so the pair composes as one — see [`FilterColumns::compose`], which refuses either
    /// half without the other. Reopening the generation from the manifest reaches the same state,
    /// [`FilterColumns::open`] taking each extent's dictionary from `AttrExtent::dict`.
    pub fn with_extents(
        &self,
        extents: &[PublishedExtent],
        records: &[RecordExtentPaths],
        entity_terms: &[tessera_store::EntityTermsExtentPaths],
        texts: &[TextExtentPaths],
    ) -> std::io::Result<FilterColumns> {
        // The record blob's extent composes here for the same reason a filter extent does: the
        // manifest entry makes the bytes reachable to a *reopen*, and this process serves from the
        // stack it holds. A flush that published one and did not compose it would leave every
        // entity it flushed with its blob-resident fields silently absent from drill-down until the
        // next fold — an entity in no layer being the ordinary `Ok(None)`.
        let records = if records.is_empty() {
            Arc::clone(&self.records)
        } else {
            Arc::new(
                self.records
                    .with_extents(records, self.access)
                    .map_err(record_open_error)?,
            )
        };
        // The transpose's extent composes here for the record blob's reason, plus one of its own:
        // a flush's labels that no live stack holds leave the join rule's label arm unable to
        // compare against the batch that just landed, which is the arm's whole point.
        let entity_terms =
            if entity_terms.is_empty() {
                Arc::clone(&self.entity_terms)
            } else {
                Arc::new(self.entity_terms.with_extents(entity_terms).map_err(|e| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
                })?)
            };
        let mut next = FilterColumns {
            columns: self.columns.clone(),
            placements: self.placements.clone(),
            access: self.access,
            records,
            entity_terms,
        };
        // A text extent appends a layer: its own dictionary, its own postings, and the entities it
        // covers. Composed here for the same reason a filter extent is — a published layer the live
        // generation does not hold answers no `match` until the next fold.
        for text in texts {
            let Some(layers) = next.columns.get_mut(text.column.as_str()) else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "a flush published a text extent for column '{}', which this generation \
                         does not hold",
                        text.column
                    ),
                ));
            };
            layers.text.push(text_layer(
                SortedDict::open(&text.dict, self.access)?,
                ColumnPostings::open(&text.postings, self.access != tessera_filter::Access::Read)?,
                &text.column,
                &text.dict_rel,
                Bitmap::deserialize::<croaring::Portable>(&std::fs::read(&text.presence)?),
                Some(text.dict_rel.clone()),
            )?);
        }
        for (column, values_rel, extent, dict) in extents {
            next.compose(column, values_rel, Arc::clone(extent), dict.clone())?;
        }
        Ok(next)
    }

    /// This generation's columns with each window of extents **replaced** by the one that carries
    /// their values — the successor generation's, after an entity-space coalesce (§5.2).
    ///
    /// **Replace is not append, and its correctness condition is a different one.** Appending
    /// checks the new layer is disjoint from what is covered; replacing N layers with one must
    /// check that the replacement's presence **equals** the union of the ones it consumes. Without
    /// that, `covered` drifts — the coalesced layer's entities are removed from it with the
    /// consumed layers and added back only as far as the replacement reaches — and every later
    /// disjointness check tests against the wrong coverage, silently. The merge's own
    /// duplicate-entity guard (`tessera_filter_write::coalesce_attr_extents`) makes the mismatch
    /// unreachable, which is exactly why it is cheap to verify and wrong to assume.
    ///
    /// A consumed layer this generation does not hold is a refusal rather than a no-op: the plan
    /// was made against a manifest, so a layer it names and this process cannot find means the two
    /// disagree about what the bundle is, and publishing on that basis would serve a column short
    /// of a window's worth of entities.
    ///
    /// **A keyword window arrives with the dictionary its coalesce minted, and the two are
    /// installed as one layer.** A coalesce merges the window's dictionaries and renumbers every
    /// ordinal, so the replacement's ordinals name positions in a dictionary no consumed layer
    /// held. [`CoalescedWindow::dict`] carries it beside the values, [`check_dictionary_pairing`]
    /// refuses a keyword window without one (and a dictionary on any other family's window), and
    /// the pair becomes a single [`Layer`] in one push — the consumed layers leave and the
    /// replacement enters in the same generation, so no generation ever holds the new ordinals
    /// beside an old dictionary or the old ordinals beside the new one. That is the composition's
    /// half of records §7's rule that a keyword layer's files swap atomically; `AttrExtent` is the
    /// manifest's half.
    ///
    /// **A text column takes the same rule through its own record.** A text layer's dictionary,
    /// postings and presence are one entry, replaced together, so the coalesced layer's new
    /// ordinals arrive with the dictionary that minted them and nothing outside the three files
    /// ever held one. `texts` carries those windows; the coverage equality above is checked for
    /// them too, against `TextLayer::present`, which is exactly what a flush extent stores and
    /// what makes the check expressible for this family.
    /// **The transpose is replaced whole rather than patched**, and `entity_terms` is the stack
    /// the caller re-derived from the rebased manifest — `None` where the axis did not run, in
    /// which case the live stack rides through unchanged. Its ordinals need no attention either
    /// way: they are dictionary positions, which `coalesce_dict_extents` preserves by construction
    /// (it replaces a contiguous window with the same records in the same order), so unlike a
    /// keyword column's they name the same terms after every coalesce.
    ///
    /// The coverage rule above holds for it too, and is checked the same way: the replacement's
    /// entity set must **equal** the live one's. A merge that lost a layer would leave the
    /// drill-down answering *unknown* for entities that carry labels, and the join rule's arm
    /// comparing against nothing — which is a `409` that does not fire.
    pub fn with_coalesced(
        &self,
        windows: &[CoalescedWindow],
        texts: &[CoalescedTextWindow],
        entity_terms: Option<Arc<tessera_store::EntityTermsStack>>,
        records: Option<Arc<RecordStack>>,
    ) -> std::io::Result<FilterColumns> {
        let entity_terms = match entity_terms {
            None => Arc::clone(&self.entity_terms),
            Some(next) => {
                let held = self.entity_terms.entity_set();
                let replacement = next.entity_set();
                if held != replacement {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "a coalesce's entity→term stack holds lists for {} entities where the one it \
                             replaces holds {}; replacing on that would answer 'unknown' for an \
                             entity that carries labels, which on the write path is a 409 that \
                             does not fire",
                            replacement.cardinality(),
                            held.cardinality()
                        ),
                    ));
                }
                next
            }
        };
        // **The record axis's stack is replaced, not carried through** — the same rule the
        // transpose above takes, and it had the same defect the transpose was fixed for: a
        // coalesce that folded a window of record extents into one edited the manifest and left
        // the live stack holding the layers it had consumed, so the running process kept probing
        // them until a restart while a reopen of the same bundle held one. Nothing served a wrong
        // answer — the layers are disjoint in entity space (I9), so an extra layer answers for
        // the entities it always answered for — but the cost the coalesce exists to remove stayed
        // until a restart removed it, and the process and its own manifest disagreed about what
        // it was serving from. `None` where the axis did not run, in which case the live stack
        // rides through untouched, which is the ordinary case.
        let records = match records {
            None => Arc::clone(&self.records),
            Some(next) => next,
        };
        let mut next = FilterColumns {
            columns: self.columns.clone(),
            placements: self.placements.clone(),
            access: self.access,
            records,
            entity_terms,
        };
        for window in windows {
            let Some(layers) = next.columns.get_mut(&window.column) else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "a coalesce names column '{}', which this generation does not hold",
                        window.column
                    ),
                ));
            };
            check_dictionary_pairing(layers.family, &window.column, window.dict.is_some())?;
            let mut union = Bitmap::new();
            for rel in &window.consumed {
                let Some(layer) = layers
                    .layers
                    .iter()
                    .find(|l| l.values_rel.as_deref() == Some(rel.as_str()))
                else {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "a coalesce for column '{}' consumed extent '{rel}', which this \
                             generation holds no layer for",
                            window.column
                        ),
                    ));
                };
                union |= layer.values.present();
            }
            if union != window.values.present() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "the coalesced extent for column '{}' is present for {} entities where the \
                         {} layers it replaces cover {}; replacing on that would leave the \
                         column's coverage wrong and every later disjointness check testing \
                         against it",
                        window.column,
                        window.values.present().cardinality(),
                        window.consumed.len(),
                        union.cardinality()
                    ),
                ));
            }
            layers.layers.retain(|l| {
                l.values_rel
                    .as_ref()
                    .is_none_or(|rel| !window.consumed.contains(rel))
            });
            // One push of one struct: the coalesced ordinals and the dictionary that numbers
            // them enter together, checked as a pair above, and the consumed layers left with
            // their own dictionaries in the `retain` above.
            layers.layers.push(Layer {
                values_rel: Some(window.values_rel.clone()),
                values: Arc::clone(&window.values),
                dict: window.dict.clone(),
            });
            // `covered` is unchanged by construction — the equality above is what says so — so it
            // is neither recomputed nor adjusted here.
        }
        for window in texts {
            let column = &window.paths.column;
            let Some(layers) = next.columns.get_mut(column) else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("a coalesce names text column '{column}', which this generation does not hold"),
                ));
            };
            let mut union = Bitmap::new();
            for rel in &window.consumed {
                let Some(layer) = layers
                    .text
                    .iter()
                    .find(|l| l.dict_rel.as_deref() == Some(rel.as_str()))
                else {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "a coalesce for text column '{column}' consumed extent '{rel}', which \
                             this generation holds no layer for"
                        ),
                    ));
                };
                union |= &layer.present;
            }
            // Opened before anything is removed, so a replacement that will not open leaves the
            // consumed layers standing rather than a column short of a window's worth of terms.
            let replacement = text_layer(
                SortedDict::open(&window.paths.dict, self.access)?,
                ColumnPostings::open(
                    &window.paths.postings,
                    self.access != tessera_filter::Access::Read,
                )?,
                column,
                &window.paths.dict_rel,
                Bitmap::deserialize::<croaring::Portable>(&std::fs::read(&window.paths.presence)?),
                Some(window.paths.dict_rel.clone()),
            )?;
            // The attribute axis's replacement rule, and this family can state it because a text
            // extent stores presence: the replacement must stand for **exactly** the entities its
            // inputs did. A merge that dropped a layer answers every later `match` short of that
            // layer's documents, silently, and no cardinality anywhere else would move.
            if union != replacement.present {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "the coalesced text extent for column '{column}' is present for {} \
                         entities where the {} layers it replaces cover {}",
                        replacement.present.cardinality(),
                        window.consumed.len(),
                        union.cardinality()
                    ),
                ));
            }
            layers.text.retain(|l| {
                l.dict_rel
                    .as_ref()
                    .is_none_or(|rel| !window.consumed.contains(rel))
            });
            layers.text.push(replacement);
        }
        Ok(next)
    }

    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    /// How many layers this generation serves `column` from — the base plus one per live extent.
    ///
    /// **This is where the coalesce's bound is realised, and the manifest is not.** A pass that
    /// edited the manifest without replacing the live layers would be a bound that arrives only at
    /// the next restart, which is exactly what `tessera_engine`'s tier-list assertion exists to
    /// catch on the delta axis.
    pub fn layer_count(&self, column: &str) -> Option<usize> {
        self.columns.get(column).map(|c| c.layers.len())
    }

    /// How many **text** layers this generation serves `column` from — the base plus one per live
    /// extent. A separate count because a text column has no value layers at all: its index is
    /// postings over terms and it owes no value column ([`FilterColumns::open`]), so
    /// [`FilterColumns::layer_count`] answers `1` for one however many flushes have published.
    ///
    /// Read for the coalesce's bound, on [`FilterColumns::layer_count`]'s argument: a pass that
    /// edited the manifest without replacing the live layers is a bound that arrives at the next
    /// restart.
    pub fn text_layer_count(&self, column: &str) -> Option<usize> {
        self.columns.get(column).map(|c| c.text.len())
    }

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

    /// **One indexed column's value layers, for a reader that wants the values themselves rather
    /// than a predicate over them** — the attribute-predicate membership, which *is* the column
    /// (`design/artifact-serving-at-scale.md` §5.1).
    ///
    /// `None` where the column is not held here at all, which is every undeclared name and every
    /// column with no entity-space storage. A caller that finds none serves the layer with no
    /// column, which is the fail-closed answer: no artifact of it is a candidate anywhere.
    pub(crate) fn value_layers(&self, column: &str) -> Option<ValueLayers<'_>> {
        self.columns.get(column).map(|layers| ValueLayers {
            layers: &layers.layers,
        })
    }

    /// The membership question `/v1/categories` asks of a `derived` column: which of this
    /// column's values does at least one entity in `candidate` carry (per-point-attributes §3.3)?
    ///
    /// **Derived, never maintained**, and evaluated entirely inside the composed verdict — so a
    /// value whose last visible member was suppressed stops being offered without a third
    /// retirement rule.
    ///
    /// **The postings are the base build's, so the extents are swept here, once.** A value carried
    /// only by entities ingested since the build must still be offered to a principal who can see
    /// one of them; deriving that per value would rescan the extents per value, so the sweep
    /// collects the codes the candidate's extent entities carry in a single pass and the per-value
    /// test is then a bitmap intersection against the postings plus a set lookup.
    pub fn category_membership<'a>(
        &'a self,
        column: &str,
        candidate: &'a Bitmap,
    ) -> Result<CategoryMembership<'a>, FilterError> {
        let layers = self
            .columns
            .get(column)
            .ok_or_else(|| FilterError::UndeclaredColumn(column.to_string()))?;
        // A column declared at a running service has no base and no postings until the fold
        // (`ingest.md` §6.3); its members are in the extents alone, which the sweep below covers.
        // A column with a base and no postings is one whose member sets cannot be read.
        let postings: Option<&ColumnPostings> = match layers.postings.as_deref() {
            Some(postings) => Some(postings),
            None if layers.layers.iter().all(|l| l.values_rel.is_some()) => None,
            None => return Err(FilterError::MembershipUnavailable(column.to_string())),
        };

        // **A count per code, not a set of codes.** The sweep is the same one pass over the same
        // entities either way, and counting in it is what lets `?counts=true` be exact without a
        // second sweep: the extents and the base postings are disjoint in entity space — a posting
        // covers the base build and an extent covers entities ingested since it — so the two halves
        // of a value's count add rather than overlapping.
        let mut from_extents: FxHashMap<u32, u64> = FxHashMap::default();
        for layer in layers.layers.iter().filter(|l| l.values_rel.is_some()) {
            let layer = &layer.values;
            for entity in layer.present().and(candidate).iter() {
                if let Some(code) = layer.value_of(entity) {
                    *from_extents.entry(code.raw()).or_default() += 1;
                }
            }
        }
        // **The one thing a count may not assume**, checked where it is cheap rather than argued
        // where it is not: `intersection_cardinality` is exact per source and cardinality does not
        // distribute over a union, so a category column that ever acquired delta postings tiers
        // would need the materialising route. None does today — a flush writes extents for a
        // category, never postings (decision 0063) — and this is where that stops being an
        // assumption. `carries` is unaffected either way, existence *does* distribute.
        let postings_are_single_source = postings.is_none_or(|p| !p.has_tiers());
        Ok(CategoryMembership {
            column: column.to_string(),
            postings,
            candidate,
            from_extents,
            postings_are_single_source,
        })
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

    /// Which space `expr` evaluates in, given each leaf's placement and the request's preference.
    ///
    /// `Entity` means the whole sub-tree can be answered by the existing entity-space evaluation;
    /// anything else means at least one leaf must be tested against the hot column. A `none_of`
    /// takes its single column's space whole — `check_negations` has already established there is
    /// exactly one — because its presence half and its matched half must be computed in the same
    /// space or the subtraction would mix domains.
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

/// One column's value-visibility predicate for one principal, at one generation.
///
/// Built by [`FilterColumns::category_membership`]; see its doc for why the extents are swept up
/// front and the postings probed per value.
pub struct CategoryMembership<'a> {
    column: String,
    /// The base build's postings, or `None` for a column that has no base yet: one declared at a
    /// running service and not yet folded, whose every member is in `from_extents`.
    postings: Option<&'a ColumnPostings>,
    candidate: &'a Bitmap,
    /// The codes the candidate's *post-build* entities carry, **and how many of them carry each** —
    /// the half no posting covers. Disjoint from the postings' half in entity space, which is what
    /// lets [`Self::count`] add the two.
    from_extents: FxHashMap<u32, u64>,
    /// Whether the column's postings are one record per value rather than a base plus live tiers.
    /// [`Self::count`] refuses otherwise; see [`FilterColumns::category_membership`].
    postings_are_single_source: bool,
}

impl CategoryMembership<'_> {
    /// The column this predicate was built for, for a caller shaping a refusal that names it.
    pub fn column(&self) -> &str {
        &self.column
    }

    /// Is `code` carried by at least one entity this principal may see?
    ///
    /// The extent half is answered first because it is a hash lookup against a set the sweep
    /// already built, and because a value minted since the build has no posting at all — asking the
    /// postings first would be a file read per such value for an answer already in hand.
    pub fn carries(&self, code: u32) -> Result<bool, FilterError> {
        if code == UNRESOLVABLE_VALUE.raw() {
            // The reserved *absent* sentinel: never drawn, never bound to a key, and carried by
            // exactly the entities that carry no value. It is not a value and is never visible.
            return Ok(false);
        }
        if self.from_extents.contains_key(&code) {
            return Ok(true);
        }
        // **A boolean against the mapped view, never a materialised posting.** A value's posting is
        // corpus-wide — every entity carrying it, hidden ones included — and the question is one
        // bit, asked once per value walked. `ColumnPostings::intersects` short-circuits at the
        // first container the two sets share and allocates nothing.
        let Some(postings) = self.postings else {
            return Ok(false);
        };
        postings
            .intersects(AttrLocalId::new(code), self.candidate)
            .map_err(|e| FilterError::PostingsUnreadable {
                column: self.column.clone(),
                detail: e.to_string(),
            })
    }

    /// **How many items carrying `code` this principal may see** — C8's `and_cardinality` against
    /// the composed mask, exact, computed per request and never precomputed
    /// (`value-suggestion.md` §3).
    ///
    /// The two halves add because they are disjoint in entity space: the extents sweep counted the
    /// candidate's *post-build* entities and the postings cover the base build alone.
    ///
    /// Never a sort key. The count is the viewer's own number and would be admissible as one under
    /// **I2**, but a count-ordered page is a top-*k* over the prefix and depends on which values
    /// were examined before the budget ran out (§8.2, and decision 0069 for the corpus-global
    /// alternative). Ordering is the matched text's, and this is information beside a row.
    pub fn count(&self, code: u32) -> Result<u64, FilterError> {
        if code == UNRESOLVABLE_VALUE.raw() {
            return Ok(0);
        }
        if !self.postings_are_single_source {
            return Err(FilterError::MembershipUnavailable(self.column.clone()));
        }
        let extents = self.from_extents.get(&code).copied().unwrap_or(0);
        let base = match self.postings {
            None => 0,
            Some(postings) => postings
                .intersection_cardinality(AttrLocalId::new(code), self.candidate)
                .map_err(|e| FilterError::PostingsUnreadable {
                    column: self.column.clone(),
                    detail: e.to_string(),
                })?,
        };
        Ok(extents + base)
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

/// The ordinal a needle no dictionary resolves is scanned for.
///
/// **No dictionary can mint it**, which is what makes it a reserved value rather than a convenient
/// one: `SortedDictWriter` refuses the `u32::MAX`-th key, so a dictionary's ordinals run
/// `0..key_count` with `key_count ≤ u32::MAX`, and `u32::MAX` is therefore outside every layer's
/// ordinal space at once. A slot cannot hold it, so a scan for it matches nothing — while still
/// walking every entity in the candidate, which is the whole point (see this module's header).
const NO_SUCH_ORDINAL: u32 = u32::MAX;

/// What one layer's dictionary turns a keyword operand into: a question about ordinals that the
/// fixed-width scan can answer.
///
/// **Total on purpose, and this is the security-bearing shape.** There is deliberately no variant
/// meaning *matches nothing, so do not scan*. A needle the layer does not hold becomes
/// [`NO_SUCH_ORDINAL`] and a prefix no key carries becomes the sentinel *range*. Every arm of
/// [`scan_ordinals`] then runs a scan over the whole candidate, so a dictionary miss costs what a
/// hit costs — the rule records §4.3 states and per-point-attributes §3.8 requires. A future
/// variant that skipped the scan would have to be added here *and* given an arm there, which is
/// where a reader is most likely to see what it is for.
///
/// **`contains` no longer arrives here**, by either route: it ends in
/// [`ValueColumn::scan_ordinal_set`] over a table sized by the dictionary, where an empty table
/// scans identically to a full one and the same rule therefore needs no sentinel to state it. The
/// move was not for tidiness — a sorted list made the scan's per-slot cost `O(log k)` in the number
/// of dictionary keys carrying the substring, which is a corpus-wide quantity and not one the work
/// may depend on.
#[derive(Debug, Clone, PartialEq, Eq)]
enum OrdinalPredicate {
    /// One ordinal — `eq`, resolved.
    Eq(u32),
    /// A list of ordinals — `in`, **one entry per needle the caller named** and nothing else.
    /// `O(log k)` per slot is priced for that *k*; a set whose size is a corpus quantity belongs in
    /// [`ValueColumn::scan_ordinal_set`] instead.
    In(Vec<u32>),
    /// A contiguous ordinal range, **inclusive at both ends** — what a prefix's dictionary range
    /// becomes, sortedness being the reason the dictionary is sorted.
    ///
    /// **Inclusive because the natural spelling of an empty prefix range is the early return
    /// wearing another hat.** `SortedDict::prefix_range` answers "no key carries this" with a
    /// half-open `k..k`, and where the prefix sorts below every key — an ordinary case, a needle
    /// alphabetically before the whole dictionary — that is `0..0`. Carried through as an
    /// exclusive upper bound of 0, the range scan narrows it to −1, finds it unrepresentable in
    /// the column's `u32` and returns *without scanning* (`values.rs`'s `narrow_hi`, whose
    /// `Unsatisfiable` verdict short-circuits by design for bounds a client wrote). So that
    /// spelling would make exactly the prefixes nobody carries the cheap ones. The sentinel range
    /// `NO_SUCH_ORDINAL..=NO_SUCH_ORDINAL` is representable, matches no slot, and scans.
    Range { lo: u32, hi: u32 },
}

/// A dictionary's half-open prefix range as an inclusive ordinal predicate.
fn ordinal_range(range: Range<u32>) -> OrdinalPredicate {
    if range.start >= range.end {
        return OrdinalPredicate::Range {
            lo: NO_SUCH_ORDINAL,
            hi: NO_SUCH_ORDINAL,
        };
    }
    OrdinalPredicate::Range {
        lo: range.start,
        hi: range.end - 1,
    }
}

/// Resolve one operand against **one layer's** dictionary.
///
/// A miss is ordinary and is not an error: `SortedDict::resolve` says so, and the whole point of
/// [`NO_SUCH_ORDINAL`] is that the scan proceeds. Only a malformed dictionary refuses.
///
/// `in` yields exactly one ordinal per needle the caller named, misses included, so the list this
/// hands the scan has the caller's own length rather than a length that counts how many of their
/// needles this layer happens to hold. The set scan then sorts and de-duplicates it, which leaves
/// one residual difference in work — a list of *k* distinct resolved ordinals against a list of one
/// sentinel costs `O(log k)` more per candidate slot, `k` being the operand's own size and never a
/// corpus quantity. Named rather than engineered around: the scan itself, which dominates, runs
/// identically either way, and the same residual is what the category route's unresolved keys have
/// always had.
fn keyword_ordinals(
    dict: &SortedDict,
    operand: &FilterOperand,
) -> Result<OrdinalPredicate, DictError> {
    Ok(match operand {
        // `match` is the text family's and reaches no keyword layer: the parse gate refuses an
        // operator outside a column's family, so this is the second line of defence and the
        // sentinel — which scans and matches nothing — is the fail-closed reading.
        FilterOperand::Match { .. } | FilterOperand::Phrase { .. } => {
            OrdinalPredicate::Eq(NO_SUCH_ORDINAL)
        }
        FilterOperand::TextEquals(needle) => {
            OrdinalPredicate::Eq(dict.resolve(needle)?.unwrap_or(NO_SUCH_ORDINAL))
        }
        FilterOperand::TextIn(needles) => {
            let mut ordinals = Vec::with_capacity(needles.len());
            for needle in needles {
                ordinals.push(dict.resolve(needle)?.unwrap_or(NO_SUCH_ORDINAL));
            }
            OrdinalPredicate::In(ordinals)
        }
        FilterOperand::TextPrefix(prefix) => ordinal_range(dict.prefix_range(prefix)?),
        // **The second line of defence, and it still scans.** `contains` is routed by
        // [`keyword_contains`] before this is reached, and the remaining operands belong to other
        // families — the parse refuses each of them on a keyword column, so arriving here means a
        // caller built the expression directly. The sentinel is the fail-closed answer: it matches
        // nothing, and it matches nothing the same way a needle nobody holds does.
        FilterOperand::TextContains(_)
        | FilterOperand::Equals(_)
        | FilterOperand::In(_)
        | FilterOperand::NumEquals(_)
        | FilterOperand::NumIn(_)
        | FilterOperand::Range { .. } => OrdinalPredicate::Eq(NO_SUCH_ORDINAL),
    })
}

/// One ordinal predicate against one layer's `u32` ordinal column.
///
/// **Every arm scans, and none of them may learn to return early.** See [`OrdinalPredicate`].
///
/// The ordinals are handed to the value column as `AttrLocalId` because that is the type its
/// fixed-width scans compare, and the crossing is safe here for a reason worth stating: the value
/// is minted from *this* layer's dictionary two calls above, consumed by *this* layer's column, and
/// never stored, returned or compared against an ordinal from anywhere else. It is a comparand for
/// the width of one call, not an identity.
fn scan_ordinals(values: &ValueColumn, predicate: &OrdinalPredicate, candidate: &Bitmap) -> Bitmap {
    match predicate {
        OrdinalPredicate::Eq(ordinal) => values.scan_eq(candidate, AttrLocalId::new(*ordinal)),
        OrdinalPredicate::In(ordinals) => {
            let ids: Vec<AttrLocalId> = ordinals.iter().copied().map(AttrLocalId::new).collect();
            values.scan_in(candidate, &ids)
        }
        OrdinalPredicate::Range { lo, hi } => values.scan_range(
            candidate,
            Some(Endpoint {
                value: Scalar::Int(i128::from(*lo)),
                inclusive: true,
            }),
            Some(Endpoint {
                value: Scalar::Int(i128::from(*hi)),
                inclusive: true,
            }),
        ),
    }
}

/// Does `document`'s token sequence contain `phrase`'s contiguously and in order?
///
/// **Both sides come from the same analyser**, so this is a comparison of the index's own units and
/// not of raw text: a phrase found here is one whose words the index holds adjacent. A repeated
/// word is not collapsed on either side — `"the the"` matches a document that says it twice in a
/// row and not one that says it once — which is what distinguishes a phrase from the word-bag the
/// conjunction already answered.
fn contains_phrase(document: &[String], phrase: &[String]) -> bool {
    // An empty phrase is refused upstream, and a phrase longer than the document cannot occur —
    // `windows` would panic on a zero length and yields nothing past the end, so both are stated
    // rather than left to it.
    if phrase.is_empty() || phrase.len() > document.len() {
        return false;
    }
    document.windows(phrase.len()).any(|w| w == phrase)
}

/// `match` over one text column: intersect the tokens' postings inside the candidate, or count
/// them where fewer than all are required.
///
/// **Every posting is masked as it is read, before anything is unioned or counted**, so no bitmap
/// that reaches the answer holds an entity outside `M_sel` — which is I2 held by construction
/// rather than by a final intersection that could be forgotten.
///
/// It is masked *as read* and not *before*: `ColumnPostings::entities` returns an owned bitmap, so
/// each token's corpus-wide posting is materialised transiently and then narrowed. That is the
/// resident cost of a `match` and it is a function of the tokens named rather than of what the
/// principal may see — the same quantity Appendix C's C25 registers as observable in the timing,
/// here in bytes. Nothing derived from it survives the intersection.
///
/// **An unresolved token contributes an empty posting rather than short-circuiting.** Under plain
/// `match` that yields the empty set either way; under m-of-n it must still consume its place in
/// the count, or `match` of three tokens with `minimum = 2` would silently become a two-token
/// question when one of them is absent from the corpus. Decision 0067 accepts the timing this
/// leaves — a term's existence and coarse carrier count are observable in a postings route, for
/// text and keyword alike — and Appendix C carries the row.
fn text_match(
    dict: &SortedDict,
    postings: &ColumnPostings,
    tokens: &[String],
    minimum: u32,
    candidate: &Bitmap,
) -> std::io::Result<Bitmap> {
    if tokens.is_empty() || minimum == 0 {
        // No token can be satisfied by no evidence: an empty `match` matches nothing rather than
        // everything, which is the same reading `any_of([])` takes.
        return Ok(Bitmap::new());
    }
    // **More required than asked for is unsatisfiable, not the conjunction.** `minimum` counts
    // *distinct* tokens — the caller's query is deduplicated before it reaches here, so "the same
    // word twice" is one piece of evidence — and a request for four of two words is one no item
    // can meet. Folding it into the `>=` branch below would answer the two-word conjunction, which
    // is a different and strictly wider question than the one asked.
    if minimum as usize > tokens.len() {
        return Ok(Bitmap::new());
    }
    // Each token's ordinal, resolved before any posting is read. Cheap — a binary search over a
    // front-coded dictionary — and separating it from the reads is what lets the conjunction below
    // narrow token by token without a second dictionary pass.
    let mut ordinals: Vec<Option<u32>> = Vec::with_capacity(tokens.len());
    for token in tokens {
        ordinals
            .push(dict.resolve(token).map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
            })?);
    }

    // ---- plain `match`: one running set, narrowed token by token ------------------------------
    //
    // **The accumulator is the whole memory argument.** Materialising every token's answer and
    // intersecting at the end holds `n` bitmaps at once; carrying one running set holds one, and
    // that set only ever *shrinks*. It starts at the candidate — the entities this principal may
    // see — so the answer is inside `M_sel` from the first step rather than from a final
    // intersection, which is I2 by construction and one fewer place to forget it.
    //
    // It is also faster, for the reason the cost model gives: a bitmap operation costs
    // O(containers touched), so every step after the first works against an already-narrowed set.
    //
    // **What it does not do is short-circuit on empty**, and that is deliberate rather than
    // overlooked: `text_match`'s contract is that an unresolved token reads its place rather than
    // stopping, so the *shape* of the work does not distinguish "no document has this word" from
    // "none you may see has it" any more sharply than Appendix C's C25 already accepts. A running
    // set that emptied at token 1 and returned would make the remaining reads a function of that
    // distinction. The loop runs to the end; the reads it makes past an emptied set are cheap by
    // the same container arithmetic, `narrow` taking an empty candidate as an immediate answer.
    if minimum as usize == tokens.len() {
        // **The candidate is borrowed for the first step, never cloned**, and the difference is
        // measurable rather than tidy: a clone of a million-entity mask costs ~800 ns, which is
        // most of a one-word query's whole budget. The first `narrow` reads the candidate and
        // writes the answer; every step after it reads the previous answer.
        let mut live: Option<Bitmap> = None;
        for ordinal in &ordinals {
            match live.as_mut() {
                // The first token reads the candidate — **borrowed, never cloned**, which the
                // measurement forced: a clone of a million-entity mask is ~800 ns, most of a
                // one-word query's whole budget.
                None => {
                    live = Some(match ordinal {
                        Some(ordinal) => postings.narrow(AttrLocalId::new(*ordinal), candidate)?,
                        // A token no dictionary holds is carried by nothing, so the conjunction is
                        // empty from here on — reached by narrowing to nothing rather than by
                        // returning, per the paragraph above.
                        None => Bitmap::new(),
                    });
                }
                // Every token after it narrows the running set **in place**, which allocates
                // nothing. Chaining the out-of-place form instead measured 19–51% slower than the
                // route this replaced, on a full-coverage principal intersecting common words —
                // see `ColumnPostings::narrow_inplace`.
                Some(live) => match ordinal {
                    Some(ordinal) => postings.narrow_inplace(AttrLocalId::new(*ordinal), live)?,
                    None => live.clear(),
                },
            }
        }
        // `tokens` is non-empty here, so the loop ran at least once; the default is the fail-safe
        // reading of a state the guards above have already excluded.
        let mut out = live.unwrap_or_default();
        // Once, at the end. The narrowing steps deliberately skip it — run-optimising a set the
        // next intersection is about to shrink is work thrown away.
        out.run_optimize();
        return Ok(out);
    }

    // ---- m-of-n: the per-token answers, each already inside the candidate ----------------------
    //
    // This shape genuinely needs every token's answer at once — a count cannot be accumulated into
    // one set — so it holds `n` bitmaps. Each is bounded by the **candidate** rather than by the
    // posting, `narrow` never assembling the corpus-wide set, so the peak is `n × |M_sel|` and not
    // `n × |corpus|`.
    let mut per_token: Vec<Bitmap> = Vec::with_capacity(tokens.len());
    for ordinal in &ordinals {
        per_token.push(match ordinal {
            Some(ordinal) => postings.narrow(AttrLocalId::new(*ordinal), candidate)?,
            // An unresolved token still takes its place in the count, or `match` of three tokens
            // with `minimum = 2` would silently become a two-token question when one is absent.
            None => Bitmap::new(),
        });
    }

    // m-of-n: how many of the tokens each candidate entity carries. Counted over the union rather
    // than over the candidate, so the work is the postings' size and not the mask's.
    let mut union = Bitmap::new();
    for token in &per_token {
        union |= token;
    }
    let mut out = Bitmap::new();
    for entity in union.iter() {
        let hits = per_token.iter().filter(|t| t.contains(entity)).count();
        if hits as u32 >= minimum {
            out.add(entity);
        }
    }
    Ok(out)
}

/// One operand against one keyword layer: resolve in that layer's dictionary, then scan its
/// ordinals.
fn scan_keyword(
    values: &ValueColumn,
    dict: &SortedDict,
    operand: &FilterOperand,
    candidate: &Bitmap,
) -> Result<Bitmap, DictError> {
    if let FilterOperand::TextContains(needle) = operand {
        return keyword_contains(values, dict, needle, candidate);
    }
    let predicate = keyword_ordinals(dict, operand)?;
    Ok(scan_ordinals(values, &predicate, candidate))
}

/// Which of `contains`' two routes a layer takes (records §4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContainsRoute {
    /// Decode and substring-search **every** key in the layer's dictionary, collect the matching
    /// ordinals, then scan for them. Front coding elides shared prefixes, so a substring can span
    /// an elided one and a flat search of the file's bytes would miss matches — it is a per-key
    /// loop, not a byte stream. Reading every key whatever the needle is also what keeps this
    /// route's work a function of `(candidate, column)`.
    Broad,
    /// Take each candidate entity's ordinal and probe the dictionary for that one key. One random
    /// dictionary access per candidate entity, and none at all for keys no visible entity carries.
    Narrow,
}

/// Per-key cost of the broad route's walk, in nanoseconds.
///
/// **Measured**: 11.0–18.8 ns to decode one key, over the three real arXiv columns records §4.3
/// quotes, through the shipped reader at the shipped restart interval
/// (`probes/2026-08-13-keyword-dict/results.md`). 15 is the middle of that band. The figure is
/// decode *alone* — the substring search over the decoded key is not in it — so this understates
/// the broad route and biases the choice towards it, which is the conservative direction: the
/// broad route is the one whose cost is bounded by the artefact rather than by the request.
///
/// The measurement is at 2.4M keys and records §4.3 sizes this family at 10⁹, so cache behaviour at
/// 400× the size is not in it. **The 2–10 s the design originally modelled must not be quoted**;
/// the honest extrapolation is 11–19 s single-threaded at 10⁹ before the search.
const BROAD_KEY_NS: u64 = 15;

/// Per-candidate **upper bound** on the narrow route's cost, in nanoseconds.
///
/// 0.10 µs is one `SortedDict::key_of` at the restart interval the campaign chose — the bottom of
/// records §4.3's *modelled* 0.1–0.3 µs band, and the figure that interval was chosen against
/// (`tessera_filter::DEFAULT_RESTART_INTERVAL`). The substring search over one decoded key and the
/// ordinal scan the broad route also pays per candidate entity (0.25–0.28 ns contiguous) both
/// vanish beside it at this precision, so neither is carried separately.
///
/// **It is a bound and no longer the typical cost, and it is deliberately not lowered.** Since the
/// route walks blocks rather than probing keys, what it pays per candidate entity depends on how
/// that candidate's ordinals cluster: 19.4–59.8 ns measured across six real shapes at a 25%
/// candidate (`2026-08-13-contains-recovery`), against ~100 for the worst case this constant has to
/// cover — a small, scattered candidate over a unique vocabulary, where every entity opens its own
/// block and nothing amortises. Pricing the route at its bound errs towards the broad route, whose
/// cost is capped by the vocabulary where the narrow route's grows without limit in the candidate;
/// pricing it at its typical cost would pick it in exactly the cell where it is worst.
///
/// ⊘ The price of that safety is real and now measured: on a contiguous 25% candidate over a
/// unique column the rule takes the broad route at 41.1 ms where this one costs 11.6 ms. Closing
/// that gap means a rule that consults the candidate's *distinct* ordinal count — a statistic about
/// what the principal's own data contains — which is the fence's stop-and-report A and an §8.2
/// admissibility question the owner has not ruled on. Not closed here.
const NARROW_PROBE_NS: u64 = 100;

/// Choose a `contains` route from **the candidate's cardinality and the layer's dictionary size,
/// and nothing else** (records §4.3).
///
/// `|candidate| · NARROW_PROBE_NS` against `|dictionary| · BROAD_KEY_NS`: the narrow route costs
/// one probe per candidate entity, the broad route one decode per key plus an ordinal scan the
/// narrow route does not run. Both inputs are admissible under §8.2 — the candidate's cardinality
/// is the principal's own quantity, which the caller can compute for itself and already receives as
/// a request's `visible` count, and a dictionary's key count is a property of the bundle, identical
/// for every principal. Neither reads a statistic about *what* the principal's data contains, which
/// is the class §6 forbids a route rule to consult, and neither depends on the needle: the same
/// request over the same mask takes the same route whether the value exists or not.
fn contains_route(candidate_entities: u64, dictionary_keys: u64) -> ContainsRoute {
    if candidate_entities.saturating_mul(NARROW_PROBE_NS)
        < dictionary_keys.saturating_mul(BROAD_KEY_NS)
    {
        ContainsRoute::Narrow
    } else {
        ContainsRoute::Broad
    }
}

/// `contains` against one keyword layer, by whichever route [`contains_route`] names.
///
/// **The routes are benched against each other and against the flat `utf8` scan they replaced**,
/// in the one window where both formats existed
/// ([the fence](../../../docs/evidence/memos/2026-08-13-utf8-retirement-fence.md)) and again
/// after both routes were repaired
/// ([the recovery](../../../docs/evidence/memos/2026-08-13-contains-recovery.md)). The
/// crossover's constants above are calibration; either route answers correctly whichever is
/// chosen.
fn keyword_contains(
    values: &ValueColumn,
    dict: &SortedDict,
    needle: &str,
    candidate: &Bitmap,
) -> Result<Bitmap, DictError> {
    // Every key contains the empty needle, so the answer is *carries a value in this column* — the
    // whole ordinal range, which the range scan expresses without materialising an ordinal per key.
    // No key is read because no key's content bears on the answer, and the needle's emptiness is
    // the caller's own input rather than anything about the corpus.
    if needle.is_empty() {
        let whole = if dict.is_empty() {
            OrdinalPredicate::Range {
                lo: NO_SUCH_ORDINAL,
                hi: NO_SUCH_ORDINAL,
            }
        } else {
            OrdinalPredicate::Range {
                lo: 0,
                hi: dict.len() - 1,
            }
        };
        return Ok(scan_ordinals(values, &whole, candidate));
    }
    match contains_route(candidate.cardinality(), u64::from(dict.len())) {
        ContainsRoute::Broad => contains_broad(values, dict, needle, candidate),
        ContainsRoute::Narrow => contains_narrow(values, dict, needle, candidate),
    }
}

/// The broad route: walk the dictionary, keep the ordinals whose keys contain the needle, scan.
fn contains_broad(
    values: &ValueColumn,
    dict: &SortedDict,
    needle: &str,
    candidate: &Bitmap,
) -> Result<Bitmap, DictError> {
    // `SortedDict::walk` has no early exit by construction, so the walk's cost is the dictionary's
    // size and never the needle's selectivity. The matcher is built once outside it for the reason
    // [`KeyMatcher`] gives — a searcher constructed per key was measured at up to 47% of this
    // route — and hoisting it changes no work the walk does per key.
    //
    // Matches go straight into a table over the dictionary's ordinals rather than into a list.
    // Two reasons, and the second is the load-bearing one. It bounds the allocation: a one-byte
    // needle over a near-unique vocabulary matches most of it, and a `Vec<u32>` of that is four
    // bytes per matching key where the table is an eighth of a bit per key whatever matched. And
    // it makes the scan that follows cost the same per slot however many keys matched — see
    // [`ValueColumn::scan_ordinal_set`], which is where the argument lives.
    let mut matched = CodeSet::over_domain(dict.len().saturating_sub(1));
    let matcher = KeyMatcher::new(needle);
    dict.walk(|ordinal, key| {
        if matcher.matches(key) {
            matched.insert(ordinal);
        }
    })?;
    // No key matched: an empty table, and the scan still runs over every candidate slot exactly as
    // it does for a full one. The sentinel `OrdinalPredicate::In` needed to say this is not needed
    // here — an empty domain-sized table already traverses identically — which is the rule stated
    // in the structure rather than in a reserved value.
    Ok(values.scan_ordinal_set(candidate, &matched))
}

/// The narrow route: read only the dictionary the candidate's own values occupy.
///
/// **The route reads the candidate's ordinals, not its entities.** Entities sharing a value name
/// the same ordinal, and `SortedDict::key_of` decodes a whole block prefix to return one key — so
/// probing per candidate entity paid `restart_interval / 2` discarded decodes for every entity,
/// including the duplicates. Deduplicating first and handing the sorted result to
/// `SortedDict::walk_ordinals` pays each *block* once instead. The whole replacement — the
/// deduplication, the block walk and [`KeyMatcher`] — measures **1.91–6.05×** the probe-per-entity
/// loop across six real shapes, best where the candidate is contiguous or its column repeats and
/// worst where it is scattered over a unique vocabulary
/// ([`contains-recovery`](../../../docs/evidence/memos/2026-08-13-contains-recovery.md)).
///
/// **The route now ends where the broad route ends** — one `OrdinalPredicate::In` scan over the
/// candidate — and the two differ only in how the matching ordinal set is computed: from the whole
/// dictionary, or from the blocks the candidate's own values sit in. That is what puts this route
/// inside `tessera_filter::take_scan_work`, which the per-entity probe loop was outside: the
/// keyword shape whose traversal no test could assert is now asserted by the same harness as every
/// other.
///
/// The transient is one `u32` per candidate entity carrying a value. The crossover admits this
/// route only below `0.15 × |dictionary|` candidate entities, so that allocation is at most
/// **0.6 B per dictionary key** against a dictionary the same campaign measures at 4.15–9.61 B/key
/// — a fraction of the artefact it is reading, not a second copy of it.
fn contains_narrow(
    values: &ValueColumn,
    dict: &SortedDict,
    needle: &str,
    candidate: &Bitmap,
) -> Result<Bitmap, DictError> {
    // A keyword layer's ordinals are `u32` by construction. Any other width means the column and
    // its declaration disagree, and the fail-closed reading is that nothing matches — the same
    // second line of defence `scan_eq` keeps for a code predicate over a text column.
    let Codes::U32(ordinals) = values.codes() else {
        return Ok(Bitmap::new());
    };
    let present = values.present();
    let mut wanted: Vec<u32> = Vec::new();
    for_each_slot_run(&present, candidate, ordinals.len(), |slot0, count, _| {
        wanted.extend_from_slice(&ordinals[slot0..slot0 + count]);
    });
    wanted.sort_unstable();
    wanted.dedup();

    let matcher = KeyMatcher::new(needle);
    let mut matched = CodeSet::over_domain(dict.len().saturating_sub(1));
    dict.walk_ordinals(&wanted, |ordinal, key| {
        if matcher.matches(key) {
            matched.insert(ordinal);
        }
    })?;
    // The same table the broad route ends in, for the same reason: an empty one scans every
    // candidate slot exactly as a full one does, so "no key the candidate carries matched" needs
    // no sentinel to say it.
    Ok(values.scan_ordinal_set(candidate, &matched))
}

/// Walk `candidate ∩ present` as `(slot0, count, entity0)` runs — a layer's values are addressed by
/// **rank**, and this is what turns an entity id into the slot holding its ordinal.
///
/// **Deliberately a second copy of `tessera_filter`'s own slot walk, which is private to the scan.**
/// The narrow `contains` route needs the same mapping outside that crate, and the two obvious
/// alternatives are worse than a duplicate: `ValueColumn::value_of` per entity pays a bitmap rank
/// per read — drill-down cadence, and O(containers below the entity), which at 10⁹ is thousands of
/// container steps per candidate entity — while stepping the presence bitmap one bit at a time
/// costs O(present) however small the candidate is, defeating the route's whole reason to exist.
///
/// Rank is **affine inside a run**: an entity `e` in a presence run starting at `ps`, with `base`
/// bits set before that run, is at slot `base + (e − ps)`. Merging the two bitmaps' runs therefore
/// gives every slot by arithmetic at O(runs).
///
/// Being a second copy, this walk is **not** seen by `tessera_filter::take_scan_work`, which counts
/// the scan's own traversal. What it collects is a set of ordinals, not an answer: both `contains`
/// routes end in one `OrdinalPredicate::In` scan over the candidate, and it is that scan the
/// harness counts. So the traversal this function performs is uncounted, and the traversal that
/// decides the answer is asserted for both routes alike.
fn for_each_slot_run(
    present: &Bitmap,
    candidate: &Bitmap,
    len: usize,
    mut f: impl FnMut(usize, usize, u32),
) {
    let live = candidate.and(present);
    let mut pres = RunIter::new(present);
    let mut liv = RunIter::new(&live);
    let mut base: u64 = 0;
    let mut p = pres.next();
    let mut l = liv.next();
    while let (Some((ps, pl)), Some((ls, ll))) = (p, l) {
        if pl < ls {
            base += u64::from(pl - ps) + 1;
            p = pres.next();
            continue;
        }
        if ll < ps {
            l = liv.next();
            continue;
        }
        let lo = ls.max(ps);
        let hi = ll.min(pl);
        let slot0 = (base + u64::from(lo - ps)) as usize;
        let count = (hi - lo) as usize + 1;
        if slot0 < len {
            f(slot0, count.min(len - slot0), lo);
        }
        if ll <= pl {
            l = liv.next();
        } else {
            base += u64::from(pl - ps) + 1;
            p = pres.next();
        }
    }
}

/// How many runs one bulk read from a bitmap cursor collects.
const RUN_BUF: usize = 64;

/// One run at a time from a bitmap, buffered through the cursor's bulk read.
struct RunIter<'a> {
    cursor: croaring::bitmap::BitmapCursor<'a>,
    buf: [croaring::RangeInclusive<u32>; RUN_BUF],
    filled: usize,
    at: usize,
}

impl<'a> RunIter<'a> {
    fn new(bitmap: &'a Bitmap) -> Self {
        RunIter {
            cursor: bitmap.cursor(),
            buf: [croaring::RangeInclusive::<u32> { start: 0, last: 0 }; RUN_BUF],
            filled: 0,
            at: 0,
        }
    }

    /// The next run as `(start, last)`, inclusive.
    fn next(&mut self) -> Option<(u32, u32)> {
        if self.at == self.filled {
            self.filled = self.cursor.read_many_ranges(&mut self.buf);
            self.at = 0;
            if self.filled == 0 {
                return None;
            }
        }
        let r = self.buf[self.at];
        self.at += 1;
        Some((r.start, r.last))
    }
}

/// One operand against one layer, whichever family the layer belongs to.
///
/// **The dictionary decides, not the declaration, and that is the narrower of the two.** A layer
/// carries a dictionary exactly when it is a keyword layer — [`FilterColumns::compose`] refuses
/// every other pairing in both directions — so reading the route off the layer keeps the resolve
/// and the ordinals it is compared against inseparable by construction, where consulting the
/// column's family here would leave a keyword layer with a missing dictionary silently scanning
/// its ordinals as though they were bytes.
fn scan_layer(
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
fn scan(values: &ValueColumn, operand: &FilterOperand, candidate: &Bitmap) -> Bitmap {
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

/// The entity-space set a filter may be evaluated over: the session's fragment with the deny state
/// composed in, plus the buffered entities whose verdict passes.
///
/// **Not the fragment.** See this module's header: the fragment still contains suppressed and
/// deleted-but-unfolded entities, and a scan returns whatever its candidate held.
///
/// The buffer walk is bounded by the buffer, not by the corpus — a buffered entity is one accepted
/// since the last flush — and `verdict` is the *same* function the row-space composition calls, so
/// the two cannot drift about what a given entity's disposition is.
pub fn candidate(
    fragment: &FrozenFragment,
    satisfied: &FxHashSet<TermId>,
    overlay: &Overlay,
    buffer: &IngestBuffer,
) -> Bitmap {
    let mut live = fragment.view().andnot(&overlay.denied());
    for (&entity, _) in buffer.iter() {
        if let Some(true) = verdict(overlay, buffer, satisfied, entity) {
            // The allocator caps entity ids at `u32::MAX` (I9's assignment is a position in the
            // signature order), so this cannot truncate; asserting it here rather than casting
            // keeps the cap a checked property at the one place entity space meets a bitmap.
            let raw = u32::try_from(entity.raw()).expect("entity ids are bounded by the allocator");
            live.add(raw);
        }
    }
    live
}

#[cfg(test)]
mod phrase_tests {
    use super::contains_phrase;

    fn t(words: &str) -> Vec<String> {
        if words.is_empty() {
            return Vec::new();
        }
        words.split(' ').map(str::to_string).collect()
    }

    /// **Adjacency and order, at the two boundaries and past them.**
    ///
    /// Run over the token *sequence* rather than through the analyser, because what this predicate
    /// owns is the sequence comparison — the analyser has its own golden vectors and mixing the two
    /// would make a segmentation change fail here.
    #[test]
    fn a_phrase_is_a_contiguous_run_in_order() {
        let doc = t("the quick brown fox jumps");
        assert!(contains_phrase(&doc, &t("quick brown")));
        assert!(contains_phrase(&doc, &t("the quick")), "at the start");
        assert!(contains_phrase(&doc, &t("jumps")), "the last word alone");
        assert!(contains_phrase(&doc, &t("fox jumps")), "at the end");
        assert!(contains_phrase(&doc, &doc), "the whole document");

        assert!(!contains_phrase(&doc, &t("brown quick")), "order is a term");
        assert!(
            !contains_phrase(&doc, &t("quick fox")),
            "both words, not adjacent — the conjunction's answer and not this one"
        );
        assert!(!contains_phrase(&doc, &t("the fox")));
    }

    /// **A repeated word is evidence twice over, on both sides.** This is what separates a phrase
    /// from the deduplicated word-bag `match` resolves: `"the the"` is a claim about a document
    /// saying it twice in a row, and the conjunction that narrows to it cannot tell the difference.
    #[test]
    fn a_repeated_word_is_not_collapsed() {
        assert!(contains_phrase(&t("had had had had"), &t("had had")));
        assert!(!contains_phrase(&t("the cat the hat"), &t("the the")));
        assert!(contains_phrase(&t("the the cat"), &t("the the")));
    }

    /// A phrase longer than the document, and an empty one on either side. `windows(0)` panics and
    /// `windows(n > len)` yields nothing, so both are decided before the walk rather than by it.
    #[test]
    fn the_degenerate_shapes_are_decided_before_the_walk() {
        assert!(!contains_phrase(&t("one two"), &t("one two three")));
        assert!(!contains_phrase(&[], &t("anything")));
        assert!(
            !contains_phrase(&t("a document"), &[]),
            "an empty phrase is not everywhere"
        );
        assert!(!contains_phrase(&[], &[]));
    }
}

#[cfg(test)]
mod keyword_tests {
    //! The keyword read route, over dictionaries and ordinal columns built in this process.
    //!
    //! **Unit level, and deliberately so for the parts that carry the argument.** A dictionary and
    //! an ordinal column are cheap to build in a test and the interesting cases — two layers that
    //! number one key differently, a scattered presence under a scattered candidate, a needle
    //! nobody holds — are ones a fixture would have to be contrived to produce. What these cover
    //! is every step the route is made of: the per-layer resolve, the sentinel rule, the two
    //! `contains` routes against each other, and the slot arithmetic the narrow one depends on.
    //!
    //! ⊘ An end-to-end pass over a built bundle — a keyword column filtered through a real
    //! principal's mask, which is what `tests/filtering.rs` does for every other family — is owed
    //! and is not here.

    use super::*;
    use tessera_filter::{take_scan_work, ScanWork, SortedDictWriter};

    /// A dictionary over already-sorted distinct keys, read back from memory.
    fn dict(keys: &[&str]) -> Arc<SortedDict> {
        let mut bytes = Vec::new();
        let mut writer = SortedDictWriter::new(&mut bytes).expect("a writer opens");
        for key in keys {
            writer.push(key).expect("keys ascend strictly");
        }
        writer.finish().expect("the dictionary closes");
        Arc::new(SortedDict::from_vec(bytes).expect("the dictionary reads back"))
    }

    /// An ordinal column every entity carries a value in: entity id is the slot.
    fn universal(ordinals: &[u32]) -> Arc<ValueColumn> {
        Arc::new(ValueColumn::universal(Codes::U32(ordinals.to_vec().into())))
    }

    /// An ordinal column only `entities` carry a value in, in ascending entity order.
    fn partial(entities: &[u32], ordinals: &[u32]) -> Arc<ValueColumn> {
        Arc::new(
            ValueColumn::partial(Codes::U32(ordinals.to_vec().into()), Bitmap::of(entities))
                .expect("one ordinal per present entity"),
        )
    }

    fn set(entities: &[u32]) -> Bitmap {
        Bitmap::of(entities)
    }

    fn members(bitmap: &Bitmap) -> Vec<u32> {
        bitmap.iter().collect()
    }

    /// A keyword column of one or more layers, each with its own dictionary — the shape
    /// [`FilterColumns::open`] builds from a manifest, assembled here without one.
    fn keyword_column(
        name: &str,
        layers: Vec<(Option<&str>, Arc<ValueColumn>, Arc<SortedDict>)>,
    ) -> FilterColumns {
        let mut covered = Bitmap::new();
        let layers: Vec<Layer> = layers
            .into_iter()
            .map(|(values_rel, values, dict)| {
                covered |= values.present();
                Layer {
                    values_rel: values_rel.map(str::to_string),
                    values,
                    dict: Some(dict),
                }
            })
            .collect();
        let mut columns = BTreeMap::new();
        columns.insert(
            name.to_string(),
            Layers {
                declared_index: 0,
                layers,
                covered,
                filterable: true,
                postings: None,
                analyser: None,
                text: Vec::new(),
                route: Route::Scan,
                family: Family::Keyword,
            },
        );
        let mut placements = BTreeMap::new();
        placements.insert(
            name.to_string(),
            Placement {
                entity: true,
                row: false,
                family: Family::Keyword,
            },
        );
        FilterColumns {
            columns,
            placements,
            access: tessera_filter::Access::Read,
            records: Arc::new(empty_record_stack()),
            entity_terms: Arc::new(tessera_store::EntityTermsStack::empty()),
        }
    }

    // -------------------------------------------------------------------------------------
    // The sentinel rule
    // -------------------------------------------------------------------------------------

    /// **A needle the layer does not hold resolves to the reserved ordinal, never to a shortcut.**
    /// The assertion is on the *representation* rather than on the answer, because both are empty:
    /// what must not drift is that a miss is carried to the scan as a value to look for.
    #[test]
    fn a_dictionary_miss_is_the_reserved_ordinal() {
        let d = dict(&["alpha", "beta", "gamma"]);
        assert_eq!(
            keyword_ordinals(&d, &FilterOperand::TextEquals("delta".into())).unwrap(),
            OrdinalPredicate::Eq(NO_SUCH_ORDINAL)
        );
        assert_eq!(
            keyword_ordinals(&d, &FilterOperand::TextEquals("beta".into())).unwrap(),
            OrdinalPredicate::Eq(1)
        );
    }

    /// **A prefix no key carries is the sentinel *range*, not an empty one**, and that distinction
    /// is the whole reason [`OrdinalPredicate::Range`] is inclusive — see its doc for the bound
    /// that would make an empty range skip the scan. Both directions are covered: `zeta` sorts
    /// above every key here and `aa` below every one of them, the second being the `0..0` case.
    #[test]
    fn a_prefix_no_key_carries_is_the_sentinel_range() {
        let d = dict(&["alpha", "alpine", "beta"]);
        for absent in ["zeta", "aa"] {
            assert_eq!(
                keyword_ordinals(&d, &FilterOperand::TextPrefix(absent.into())).unwrap(),
                OrdinalPredicate::Range {
                    lo: NO_SUCH_ORDINAL,
                    hi: NO_SUCH_ORDINAL
                },
                "prefix {absent:?}"
            );
        }
        assert_eq!(
            keyword_ordinals(&d, &FilterOperand::TextPrefix("alp".into())).unwrap(),
            OrdinalPredicate::Range { lo: 0, hi: 1 }
        );
    }

    /// **The pin on the whole rule: a sentinel predicate is answered by a real scan.**
    ///
    /// The column below holds `NO_SUCH_ORDINAL` in a slot, which no dictionary can mint and no real
    /// keyword column therefore has — so the only way that entity comes back is if the scan walked
    /// the candidate and compared. An implementation that recognised the miss and returned
    /// `Bitmap::new()` — the early return records §4.3 forbids, because it makes *no item has this
    /// value* cheaper than *some do* — returns nothing here and fails, in all three predicate
    /// shapes at once.
    #[test]
    fn a_sentinel_predicate_is_scanned_for_and_not_short_circuited() {
        let column = universal(&[7, NO_SUCH_ORDINAL, 9]);
        let all = set(&[0, 1, 2]);
        for predicate in [
            OrdinalPredicate::Eq(NO_SUCH_ORDINAL),
            OrdinalPredicate::In(vec![NO_SUCH_ORDINAL]),
            OrdinalPredicate::Range {
                lo: NO_SUCH_ORDINAL,
                hi: NO_SUCH_ORDINAL,
            },
        ] {
            assert_eq!(
                members(&scan_ordinals(&column, &predicate, &all)),
                vec![1],
                "{predicate:?} must reach the slot holding the reserved ordinal"
            );
        }
    }

    /// The reserved ordinal names no key, which is what makes it safe to scan for rather than
    /// merely unlikely to collide.
    ///
    /// That a dictionary's ordinals run `0..key_count` with `key_count` a `u32` puts `u32::MAX`
    /// outside them **as a type-level tautology** — clippy says so if it is asserted — so what is
    /// worth checking is the reader's own answer to it.
    #[test]
    fn no_dictionary_holds_the_reserved_ordinal() {
        let d = dict(&["alpha", "beta"]);
        assert!(d.key_of(NO_SUCH_ORDINAL, &mut Vec::new()).is_err());
    }

    /// `in` hands the scan one ordinal per needle the caller named, misses included, so the list's
    /// length is the operand's own and never a count of how many of them this layer holds.
    #[test]
    fn an_in_set_carries_one_ordinal_per_needle_hit_or_miss() {
        let d = dict(&["alpha", "beta", "gamma"]);
        let operand = FilterOperand::TextIn(vec!["gamma".into(), "delta".into(), "alpha".into()]);
        assert_eq!(
            keyword_ordinals(&d, &operand).unwrap(),
            OrdinalPredicate::In(vec![2, NO_SUCH_ORDINAL, 0])
        );
    }

    /// An operand from another family cannot reach a keyword column through the parse; arriving
    /// here from an embedder that built the expression directly, it matches nothing **and still
    /// scans** — the same fail-closed answer a needle nobody holds gets.
    #[test]
    fn an_operand_from_another_family_takes_the_sentinel() {
        let d = dict(&["alpha"]);
        assert_eq!(
            keyword_ordinals(&d, &FilterOperand::NumEquals(Scalar::Int(3))).unwrap(),
            OrdinalPredicate::Eq(NO_SUCH_ORDINAL)
        );
    }

    // -------------------------------------------------------------------------------------
    // The sentinel rule, asserted in work
    // -------------------------------------------------------------------------------------
    //
    // The tests above pin what a miss *is* — the reserved ordinal, the sentinel range — and what a
    // scan for it *answers*. These pin what it **costs**, which is the property records §4.3 states
    // and per-point-attributes §3.8 requires: a needle no dictionary holds must be indistinguishable
    // from one every dictionary holds in work, not merely in outcome. Records §10 names this as the
    // conformance suite's one deliberate work assertion, "the one place a work assertion is the test,
    // because the rule exists for it".
    //
    // **The unit is traversed slots and runs, never elapsed time.** A stopwatch assertion would be
    // flaky in exactly the direction that lets the channel reopen — it goes green on a loaded
    // machine — so what is compared is the count `tessera_filter::take_scan_work` reports, which is
    // zero for a scan that did not happen and identical for two that ran to completion over the same
    // candidate. That module's header argues where the counter sits and what it costs when tests are
    // not running.
    //
    // Each of these routes through [`FilterColumns::resolve`] rather than through `scan_ordinals`,
    // because the early return this is guarding against has more than one place to hide: the resolve,
    // the predicate, the scan, and — the one no answer-level test can see — the per-layer loop.

    /// The work one operand costs over `column`, and the entities it returns.
    ///
    /// The counter is taken *before* the resolve as well as after, so what comes back is this
    /// resolve's own traversal rather than it plus whatever the assertion before it left behind.
    fn work_of(
        columns: &FilterColumns,
        column: &str,
        operand: &FilterOperand,
        candidate: &Bitmap,
    ) -> (ScanWork, Vec<u32>) {
        let _ = take_scan_work();
        let out = columns
            .resolve(column, operand, candidate)
            .expect("a declared keyword column resolves");
        (take_scan_work(), members(&out))
    }

    /// Every operand in `ops` traverses exactly what the first one does — **and the first traverses
    /// something**, which is what stops the equality holding vacuously if the scan were removed
    /// altogether rather than merely short-circuited for the sentinel.
    fn traverse_alike(
        columns: &FilterColumns,
        column: &str,
        candidate: &Bitmap,
        ops: &[(&str, FilterOperand)],
    ) {
        let (first, rest) = ops.split_first().expect("at least one operand to compare");
        let (baseline, _) = work_of(columns, column, &first.1, candidate);
        assert!(
            baseline.runs > 0 && baseline.slots > 0,
            "{}: the scan traversed nothing, so the comparisons below would hold vacuously. A \
             --release build is the ordinary cause — the counter is compiled under debug_assertions \
             (tessera_filter::take_scan_work)",
            first.0
        );
        for (label, operand) in rest {
            let (work, _) = work_of(columns, column, operand, candidate);
            assert_eq!(
                work, baseline,
                "{label} traversed differently from {}: the two must cost the same",
                first.0
            );
        }
    }

    /// **`eq` costs the same whether or not the needle exists.** The three needles below are a key
    /// the layer holds, a key sorting after every key it holds, and one sorting before all of them —
    /// so a short circuit reached by any of the resolve's paths shows up as a shorter traversal.
    #[test]
    fn eq_traverses_alike_whether_or_not_the_needle_resolves() {
        let columns = keyword_column(
            "sub",
            vec![(
                None,
                universal(&[0, 1, 0, 2]),
                dict(&["alpha", "beta", "gamma"]),
            )],
        );
        let candidate = set(&[0, 1, 2, 3]);
        // The held needle really does match, so the equality below is two scans that found
        // different things, not two that found nothing.
        let (_, held) = work_of(&columns, "sub", &eq("beta"), &candidate);
        assert_eq!(held, vec![1]);
        let (_, absent) = work_of(&columns, "sub", &eq("delta"), &candidate);
        assert!(absent.is_empty(), "no entity carries delta");

        traverse_alike(
            &columns,
            "sub",
            &candidate,
            &[
                ("a needle the dictionary holds", eq("beta")),
                ("a needle no dictionary holds", eq("delta")),
                ("a needle sorting below every key", eq("aa")),
            ],
        );
    }

    /// **The shape a flush produces: layers that disagree about a key.** `alpha` is in the base's
    /// dictionary and not the extent's, `gamma` in the extent's and not the base's, `delta` in
    /// neither — and all three must scan both layers in full.
    ///
    /// This is the case a naive early return optimises and no answer-level test can catch: skipping
    /// a layer whose dictionary does not resolve the needle returns exactly the right entities, from
    /// exactly the layers that could hold them, for less work — which is the channel, and is why
    /// these route through [`FilterColumns::resolve`] rather than through one layer's scan.
    #[test]
    fn eq_traverses_alike_where_the_layers_disagree() {
        let columns = keyword_column(
            "sub",
            vec![
                // Base: alpha = 0, beta = 1, over entities 0..3.
                (None, universal(&[0, 1, 0]), dict(&["alpha", "beta"])),
                // Extent: beta = 0, gamma = 1, over entities 10 and 11.
                (
                    Some("extents/f1.arrow"),
                    partial(&[10, 11], &[1, 0]),
                    dict(&["beta", "gamma"]),
                ),
            ],
        );
        let candidate = set(&[0, 1, 2, 10, 11]);
        for (needle, expected) in [
            ("alpha", vec![0, 2]),
            ("gamma", vec![10]),
            ("beta", vec![1, 11]),
            ("delta", vec![]),
        ] {
            let (_, out) = work_of(&columns, "sub", &eq(needle), &candidate);
            assert_eq!(out, expected, "{needle} over both layers");
        }

        traverse_alike(
            &columns,
            "sub",
            &candidate,
            &[
                ("a needle both layers hold", eq("beta")),
                ("a needle only the base holds", eq("alpha")),
                ("a needle only the extent holds", eq("gamma")),
                ("a needle neither holds", eq("delta")),
            ],
        );
    }

    /// **A prefix nothing carries costs what a prefix everything carries costs**, including the
    /// prefix that sorts below every key.
    ///
    /// That last one is the case [`OrdinalPredicate::Range`]'s inclusive bound exists for: as a
    /// half-open `0..0` it narrows to an upper bound of −1, which the range scan finds
    /// unrepresentable and answers *without scanning*. `zeta` sorts above every key and `alphabet`
    /// falls inside the dictionary while matching no key, so all three empty shapes are here.
    #[test]
    fn a_prefix_matching_nothing_traverses_what_a_matching_prefix_does() {
        let columns = keyword_column(
            "sub",
            vec![(
                None,
                universal(&[0, 1, 2, 0]),
                dict(&["alpha", "alpine", "beta"]),
            )],
        );
        let candidate = set(&[0, 1, 2, 3]);
        let (_, matched) = work_of(&columns, "sub", &prefix("alp"), &candidate);
        assert_eq!(matched, vec![0, 1, 3]);

        traverse_alike(
            &columns,
            "sub",
            &candidate,
            &[
                ("a prefix two keys carry", prefix("alp")),
                ("a prefix sorting below every key", prefix("aa")),
                ("a prefix sorting above every key", prefix("zeta")),
                (
                    "a prefix inside the dictionary that no key carries",
                    prefix("alphabet"),
                ),
            ],
        );
    }

    /// **An `in` set costs the same however many of its needles resolve.** The three sets below name
    /// three needles each — the operand's own length held equal, because the per-slot search is
    /// `O(log k)` in *k*, the caller's own quantity, and it is the traversal rather than *k* that
    /// must not vary with what the corpus holds.
    #[test]
    fn an_in_set_traverses_alike_however_many_needles_resolve() {
        let columns = keyword_column(
            "sub",
            vec![(
                None,
                universal(&[0, 1, 2, 1]),
                dict(&["alpha", "beta", "gamma"]),
            )],
        );
        let candidate = set(&[0, 1, 2, 3]);
        let (_, some) = work_of(
            &columns,
            "sub",
            &in_set(&["alpha", "delta", "zulu"]),
            &candidate,
        );
        assert_eq!(some, vec![0], "only alpha of the three is held");

        traverse_alike(
            &columns,
            "sub",
            &candidate,
            &[
                ("every needle resolves", in_set(&["alpha", "beta", "gamma"])),
                (
                    "one needle of three resolves",
                    in_set(&["alpha", "delta", "zulu"]),
                ),
                ("no needle resolves", in_set(&["delta", "zulu", "aa"])),
            ],
        );
    }

    /// **`contains` too**, which records §4.3 states the rule for by name: the broad route's walk
    /// reads every key whatever the needle, and the ordinal scan that follows it runs on the
    /// sentinel when no key matched. The candidate and dictionary here put [`contains_route`] on the
    /// broad route, which is the one whose scan this counter sees.
    #[test]
    fn a_contains_matching_no_key_traverses_what_a_matching_one_does() {
        let columns = keyword_column(
            "sub",
            vec![(
                None,
                universal(&[0, 1, 2, 1]),
                dict(&["alpha", "alpine", "beta"]),
            )],
        );
        let candidate = set(&[0, 1, 2, 3]);
        assert_eq!(
            contains_route(candidate.cardinality(), 3),
            ContainsRoute::Broad
        );
        let (_, matched) = work_of(&columns, "sub", &contains("lph"), &candidate);
        assert_eq!(matched, vec![0]);

        traverse_alike(
            &columns,
            "sub",
            &candidate,
            &[
                ("a fragment one key contains", contains("lph")),
                ("a fragment no key contains", contains("zzz")),
            ],
        );
    }

    fn eq(needle: &str) -> FilterOperand {
        FilterOperand::TextEquals(needle.into())
    }

    fn prefix(needle: &str) -> FilterOperand {
        FilterOperand::TextPrefix(needle.into())
    }

    fn contains(needle: &str) -> FilterOperand {
        FilterOperand::TextContains(needle.into())
    }

    fn in_set(needles: &[&str]) -> FilterOperand {
        FilterOperand::TextIn(needles.iter().map(|n| (*n).into()).collect())
    }

    // -------------------------------------------------------------------------------------
    // Per-layer resolution
    // -------------------------------------------------------------------------------------

    /// **The layer's own dictionary, and nothing else would be correct.** The two layers below
    /// number `beta` differently — 1 in the base, 0 in the extent — which is what independent
    /// per-layer numbering produces. Resolving once and scanning both layers with that one ordinal
    /// returns the wrong entities from one of them; this asserts the right ones from both.
    #[test]
    fn a_needle_resolves_against_each_layers_own_dictionary() {
        let columns = keyword_column(
            "sub",
            vec![
                // Base: alpha = 0, beta = 1. Entities 0..3.
                (None, universal(&[0, 1, 0]), dict(&["alpha", "beta"])),
                // Extent: beta = 0, gamma = 1. Entities 10, 11.
                (
                    Some("extents/f1.arrow"),
                    partial(&[10, 11], &[1, 0]),
                    dict(&["beta", "gamma"]),
                ),
            ],
        );
        let candidate = set(&[0, 1, 2, 10, 11]);
        let hits = columns
            .resolve("sub", &FilterOperand::TextEquals("beta".into()), &candidate)
            .unwrap();
        assert_eq!(members(&hits), vec![1, 11]);

        // `gamma` exists only in the extent's dictionary; the base's resolve misses and still
        // scans, contributing nothing.
        let hits = columns
            .resolve(
                "sub",
                &FilterOperand::TextEquals("gamma".into()),
                &candidate,
            )
            .unwrap();
        assert_eq!(members(&hits), vec![10]);

        // A needle no layer holds is an ordinary empty answer, not a refusal.
        let hits = columns
            .resolve(
                "sub",
                &FilterOperand::TextEquals("omega".into()),
                &candidate,
            )
            .unwrap();
        assert!(hits.is_empty());
    }

    /// The result is a subset of the candidate whatever the operand — **I12** as a property of the
    /// shape, checked here for the family whose leaves are new.
    #[test]
    fn a_keyword_leaf_never_widens_the_candidate() {
        let columns = keyword_column(
            "sub",
            vec![(None, universal(&[0, 1, 0, 1]), dict(&["alpha", "beta"]))],
        );
        let candidate = set(&[1, 3]);
        for operand in [
            FilterOperand::TextEquals("alpha".into()),
            FilterOperand::TextIn(vec!["alpha".into(), "beta".into()]),
            FilterOperand::TextPrefix("".into()),
            FilterOperand::TextContains("a".into()),
        ] {
            let hits = columns.resolve("sub", &operand, &candidate).unwrap();
            assert!(
                hits.and(&candidate) == hits,
                "{operand:?} escaped the candidate"
            );
        }
    }

    /// `prefix` is the contiguous ordinal range sortedness gives it, tested by the range scan.
    #[test]
    fn a_prefix_is_a_contiguous_ordinal_range() {
        let columns = keyword_column(
            "sub",
            vec![(
                None,
                universal(&[0, 1, 2, 3]),
                dict(&["alpha", "alpine", "beta", "gamma"]),
            )],
        );
        let candidate = set(&[0, 1, 2, 3]);
        let hits = columns
            .resolve("sub", &FilterOperand::TextPrefix("alp".into()), &candidate)
            .unwrap();
        assert_eq!(members(&hits), vec![0, 1]);
        // The empty prefix is every key, which is every entity carrying a value — the honest
        // reading, and not every entity.
        let hits = columns
            .resolve("sub", &FilterOperand::TextPrefix("".into()), &candidate)
            .unwrap();
        assert_eq!(members(&hits), vec![0, 1, 2, 3]);
    }

    /// `in` over a keyword column is `eq` over a list, and a set naming values nobody holds costs
    /// the same shape of answer as one naming values everybody does.
    #[test]
    fn an_in_set_unions_its_needles() {
        let columns = keyword_column(
            "sub",
            vec![(
                None,
                universal(&[0, 1, 2, 0]),
                dict(&["alpha", "beta", "gamma"]),
            )],
        );
        let candidate = set(&[0, 1, 2, 3]);
        let hits = columns
            .resolve(
                "sub",
                &FilterOperand::TextIn(vec!["gamma".into(), "alpha".into(), "nope".into()]),
                &candidate,
            )
            .unwrap();
        assert_eq!(members(&hits), vec![0, 2, 3]);
    }

    // -------------------------------------------------------------------------------------
    // `contains`, and its two routes
    // -------------------------------------------------------------------------------------

    /// **The two routes must agree, or the crossover is a correctness switch rather than a cost
    /// one.** Run over a partial column with a scattered presence and a scattered candidate, which
    /// is the shape the narrow route's slot arithmetic is most likely to get wrong.
    #[test]
    fn the_two_contains_routes_agree() {
        let keys = [
            "arxiv/0001",
            "arxiv/0002",
            "arxiv/1001",
            "bio/0001",
            "bio/2002",
            "cs/0003",
            "cs/1001",
            "math/0001",
        ];
        let d = dict(&keys);
        let entities = [1u32, 2, 5, 9, 40, 41, 42, 100_000, 100_001, 200_000];
        let ordinals = [0u32, 3, 6, 1, 7, 2, 4, 5, 0, 6];
        let values = partial(&entities, &ordinals);
        for candidate in [
            set(&entities),
            set(&[1, 42, 200_000]),
            set(&[5]),
            set(&[7, 8]),
            Bitmap::new(),
        ] {
            for needle in ["1001", "arxiv", "0001", "zzz", "/", "math/0001"] {
                let broad = contains_broad(&values, &d, needle, &candidate).unwrap();
                let narrow = contains_narrow(&values, &d, needle, &candidate).unwrap();
                assert_eq!(
                    members(&broad),
                    members(&narrow),
                    "routes disagree on {needle:?} over {:?}",
                    members(&candidate)
                );
            }
        }
    }

    /// **`contains`' ordinal test is sized by the dictionary, not by what matched.**
    ///
    /// The traversal counter cannot see this one: `runs` and `slots` were already equal across
    /// needles, because both routes always scanned the whole candidate. What differed was the cost
    /// *per slot* — a sorted list of matching ordinals is `O(log k)`, and for `contains` that *k*
    /// is the number of dictionary keys carrying the substring: a corpus-wide count, including keys
    /// no visible entity carries, that a caller can move by choosing a fragment. A table over the
    /// ordinal domain is the same size and the same test whatever matched, which is what the three
    /// needles below assert directly, since no counter can.
    #[test]
    fn contains_tests_ordinals_through_a_table_sized_by_the_dictionary() {
        let keys: Vec<String> = (0..64).map(|i| format!("host-{i:03}.example")).collect();
        let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
        let d = dict(&refs);

        // Nothing, one key, and every key — the span a fragment-guessing caller would sweep.
        for (needle, expected) in [("zzz", 0usize), ("host-007", 1), ("example", 64)] {
            let mut matched = tessera_filter::CodeSet::over_domain(d.len() - 1);
            let matcher = tessera_filter::KeyMatcher::new(needle);
            let mut hits = 0usize;
            d.walk(|ordinal, key| {
                if matcher.matches(key) {
                    matched.insert(ordinal);
                    hits += 1;
                }
            })
            .unwrap();
            assert_eq!(hits, expected, "needle {needle:?}");
            assert_eq!(
                matched.domain(),
                d.len() - 1,
                "needle {needle:?}: the table is sized by the dictionary, whatever matched"
            );
        }
    }

    /// **The two routes traverse alike**, and not merely answer alike. They differ in how the
    /// matching ordinal set is found — every key in the dictionary, or only the blocks holding the
    /// candidate's own values — and not at all in the scan that turns that set into an answer. So
    /// whichever the crossover picks, the traversal `take_scan_work` counts is the same one, and
    /// the narrow route is no longer the one keyword shape sitting outside that harness.
    ///
    /// This is what makes the crossover a *cost* choice with nothing else riding on it: a route
    /// rule that changed the observable work would be choosing a disclosure profile as well as a
    /// price.
    #[test]
    fn the_two_contains_routes_traverse_alike() {
        let d = dict(&[
            "arxiv/0001",
            "arxiv/1001",
            "bio/0001",
            "cs/0003",
            "math/0001",
        ]);
        let entities = [1u32, 2, 5, 9, 40, 41, 100_000];
        let ordinals = [0u32, 3, 1, 4, 2, 0, 3];
        let values = partial(&entities, &ordinals);
        let mut compared = 0;
        for candidate in [
            set(&entities),
            set(&[1, 41, 100_000]),
            set(&[5]),
            set(&[7, 8]),
        ] {
            for needle in ["0001", "arxiv", "zzz", "/", "math/0001", ""] {
                let _ = take_scan_work();
                let broad = contains_broad(&values, &d, needle, &candidate).unwrap();
                let broad_work = take_scan_work();
                let narrow = contains_narrow(&values, &d, needle, &candidate).unwrap();
                let narrow_work = take_scan_work();
                assert_eq!(
                    members(&broad),
                    members(&narrow),
                    "{needle:?} answers differ"
                );
                assert_eq!(
                    broad_work,
                    narrow_work,
                    "{needle:?} over {:?}: the routes traversed differently",
                    members(&candidate)
                );
                if broad_work.runs > 0 && broad_work.slots > 0 {
                    compared += 1;
                }
            }
        }
        assert!(
            compared > 0,
            "every comparison traversed nothing, so they all held vacuously. A --release build is \
             the ordinary cause — the counter is compiled under debug_assertions"
        );
    }

    /// The routes agree on a universal column too, where the entity id is the slot and the run
    /// merge has no presence bitmap to thread.
    #[test]
    fn the_two_contains_routes_agree_over_a_universal_column() {
        let d = dict(&["ab", "abc", "bc", "cd"]);
        let values = universal(&[0, 1, 2, 3, 1, 0]);
        for candidate in [set(&[0, 1, 2, 3, 4, 5]), set(&[1, 4]), set(&[5])] {
            for needle in ["b", "ab", "cd", "q"] {
                assert_eq!(
                    members(&contains_broad(&values, &d, needle, &candidate).unwrap()),
                    members(&contains_narrow(&values, &d, needle, &candidate).unwrap()),
                    "routes disagree on {needle:?}"
                );
            }
        }
    }

    /// A substring that spans an elided shared prefix is still found — the reason the broad route
    /// decodes every key rather than searching the file's bytes. `alphabet` front-codes against
    /// `alpha`, so `phab` exists in no stored suffix.
    #[test]
    fn contains_finds_a_substring_spanning_an_elided_prefix() {
        let d = dict(&["alpha", "alphabet"]);
        let values = universal(&[0, 1]);
        let candidate = set(&[0, 1]);
        assert_eq!(
            members(&contains_broad(&values, &d, "phab", &candidate).unwrap()),
            vec![1]
        );
        assert_eq!(
            members(&contains_narrow(&values, &d, "phab", &candidate).unwrap()),
            vec![1]
        );
    }

    /// `contains` matching no key is the empty answer by way of the sentinel, and the whole
    /// candidate is still scanned for it.
    #[test]
    fn a_contains_matching_no_key_still_scans() {
        let d = dict(&["alpha", "beta"]);
        let values = universal(&[0, 1]);
        assert!(contains_broad(&values, &d, "zzz", &set(&[0, 1]))
            .unwrap()
            .is_empty());
    }

    /// The empty needle is *carries a value in this column*, which is not every entity: entity 2
    /// below has no value and matches nothing.
    #[test]
    fn an_empty_contains_needle_is_carrying_a_value() {
        let columns = keyword_column(
            "sub",
            vec![(
                None,
                partial(&[0, 1, 3], &[0, 1, 0]),
                dict(&["alpha", "beta"]),
            )],
        );
        let hits = columns
            .resolve(
                "sub",
                &FilterOperand::TextContains("".into()),
                &set(&[0, 1, 2, 3]),
            )
            .unwrap();
        assert_eq!(members(&hits), vec![0, 1, 3]);
    }

    /// **The crossover reads two numbers and neither is about content.** A candidate small against
    /// the dictionary takes the narrow route; one large against it takes the broad. The boundary is
    /// the ratio of the two measured constants, and the same request over the same mask takes the
    /// same route whatever the needle.
    #[test]
    fn the_contains_crossover_is_candidate_size_against_dictionary_size() {
        // 10³ candidate entities against 10⁹ keys: probing a thousand keys beats decoding a
        // billion.
        assert_eq!(contains_route(1_000, 1_000_000_000), ContainsRoute::Narrow);
        // The whole corpus against a small vocabulary: one pass over the dictionary, then a scan.
        assert_eq!(contains_route(1_000_000_000, 1_000), ContainsRoute::Broad);
        // The boundary itself, from the constants rather than from a remembered number.
        let keys = 1_000_000u64;
        let boundary = keys * BROAD_KEY_NS / NARROW_PROBE_NS;
        assert_eq!(contains_route(boundary - 1, keys), ContainsRoute::Narrow);
        assert_eq!(contains_route(boundary, keys), ContainsRoute::Broad);
        // An empty dictionary can only be walked — there is nothing to probe for.
        assert_eq!(contains_route(0, 0), ContainsRoute::Broad);
    }

    // -------------------------------------------------------------------------------------
    // The layer pairing, and what crosses the boundary
    // -------------------------------------------------------------------------------------

    /// A keyword extent arriving without its dictionary is refused rather than composed: scanned
    /// as codes it would answer every string predicate with the empty set, which under-reports
    /// with no symptom.
    #[test]
    fn a_keyword_extent_without_its_dictionary_is_refused() {
        let mut columns = keyword_column(
            "sub",
            vec![(None, universal(&[0, 1]), dict(&["alpha", "beta"]))],
        );
        let err = columns
            .compose("sub", "extents/f1.arrow", partial(&[10], &[0]), None)
            .unwrap_err();
        assert!(
            err.to_string().contains("carries no sorted dictionary"),
            "{err}"
        );
    }

    /// And the other direction: a dictionary for a column the schema does not call a keyword means
    /// the caller and the declaration disagree about what its values are.
    #[test]
    fn a_dictionary_on_a_non_keyword_extent_is_refused() {
        let mut columns = keyword_column(
            "sub",
            vec![(None, universal(&[0, 1]), dict(&["alpha", "beta"]))],
        );
        columns.columns.get_mut("sub").expect("the column").family = Family::Numeric;
        let err = columns
            .compose(
                "sub",
                "extents/f1.arrow",
                partial(&[10], &[0]),
                Some(dict(&["alpha"])),
            )
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("does not declare the column a keyword"),
            "{err}"
        );
    }

    /// **Drill-down is served the key, never the ordinal** (records §4.3, **I10**): an ordinal is a
    /// position in one layer's dictionary and an index internal, so it does not cross the trust
    /// boundary. Decoded against the layer that holds the entity, which is what makes the two
    /// layers' clashing numbering come out right.
    #[test]
    fn drill_down_reads_the_key_not_the_ordinal() {
        let columns = keyword_column(
            "sub",
            vec![
                (None, universal(&[0, 1]), dict(&["alpha", "beta"])),
                (
                    Some("extents/f1.arrow"),
                    partial(&[10], &[0]),
                    dict(&["beta", "gamma"]),
                ),
            ],
        );
        assert_eq!(
            columns.stored_value("sub", 1),
            Some(RecordValue::Utf8("beta".into()))
        );
        assert_eq!(
            columns.stored_value("sub", 10),
            Some(RecordValue::Utf8("beta".into()))
        );
        assert_eq!(columns.stored_value("sub", 7), None);
    }

    /// **A coalesced keyword window is installed with the dictionary its merge minted, as one
    /// layer, and every entity still reads its own key.** The merged dictionary numbers `beta` and
    /// `gamma` the other way round from either consumed layer, so a replacement resolved against
    /// a consumed layer's dictionary — or a consumed layer left behind beside the merged one —
    /// would answer another key's entities.
    ///
    /// The refusals are the same test's other half: the window without its dictionary is refused
    /// with the generation unchanged, and a dictionary on a window of a column the schema does not
    /// call a keyword is refused too — both through the one pairing rule a flush's extent passes.
    #[test]
    fn a_coalesced_keyword_window_installs_its_dictionary_beside_its_values() {
        let columns = keyword_column(
            "sub",
            vec![
                (None, universal(&[0, 1]), dict(&["alpha", "beta"])),
                // Extent 1: gamma = 0, over entity 10. Extent 2: beta = 0, over entity 11.
                (
                    Some("extents/f1.arrow"),
                    partial(&[10], &[0]),
                    dict(&["gamma"]),
                ),
                (
                    Some("extents/f2.arrow"),
                    partial(&[11], &[0]),
                    dict(&["beta"]),
                ),
            ],
        );
        let consumed = vec![
            "extents/f1.arrow".to_string(),
            "extents/f2.arrow".to_string(),
        ];
        // The merge's output: dictionary [beta, gamma], so 10 -> gamma is ordinal 1 and
        // 11 -> beta is ordinal 0 — neither consumed layer's numbering.
        let window = |dict: Option<Arc<SortedDict>>| CoalescedWindow {
            column: "sub".to_string(),
            consumed: consumed.clone(),
            values_rel: "coalesced/c-1/attrs/sub/values.arrow".to_string(),
            values: partial(&[10, 11], &[1, 0]),
            dict,
        };
        let next = columns
            .with_coalesced(&[window(Some(dict(&["beta", "gamma"])))], &[], None, None)
            .expect("a keyword window with its dictionary replaces its layers");
        assert_eq!(
            next.layer_count("sub"),
            Some(2),
            "base plus the one coalesced layer"
        );
        let candidate = set(&[0, 1, 10, 11]);
        let resolve = |columns: &FilterColumns, key: &str| {
            members(
                &columns
                    .resolve("sub", &FilterOperand::TextEquals(key.into()), &candidate)
                    .expect("answers"),
            )
        };
        assert_eq!(resolve(&next, "alpha"), vec![0]);
        assert_eq!(resolve(&next, "beta"), vec![1, 11]);
        assert_eq!(resolve(&next, "gamma"), vec![10]);
        assert_eq!(
            next.stored_value("sub", 10),
            Some(RecordValue::Utf8("gamma".into()))
        );
        assert_eq!(
            next.stored_value("sub", 11),
            Some(RecordValue::Utf8("beta".into()))
        );
        // And exactly what the consumed layers answered, key for key.
        for key in ["alpha", "beta", "gamma", "delta"] {
            assert_eq!(resolve(&next, key), resolve(&columns, key), "{key}");
        }

        // The half: a keyword window with no dictionary has no reading and is refused.
        let err = columns
            .with_coalesced(&[window(None)], &[], None, None)
            .expect_err("a keyword window without its dictionary is refused");
        assert!(
            err.to_string().contains("carries no sorted dictionary"),
            "{err}"
        );
        assert_eq!(
            columns.layer_count("sub"),
            Some(3),
            "the generation is untouched"
        );

        // The other direction: a dictionary on a column the schema does not call a keyword. The
        // same three layers under a column whose declared family is numeric.
        let mut numeric = keyword_column(
            "sub",
            vec![
                (None, universal(&[0, 1]), dict(&["alpha", "beta"])),
                (
                    Some("extents/f1.arrow"),
                    partial(&[10], &[0]),
                    dict(&["gamma"]),
                ),
                (
                    Some("extents/f2.arrow"),
                    partial(&[11], &[0]),
                    dict(&["beta"]),
                ),
            ],
        );
        numeric.columns.get_mut("sub").expect("the column").family = Family::Numeric;
        let err = numeric
            .with_coalesced(&[window(Some(dict(&["beta", "gamma"])))], &[], None, None)
            .expect_err("a dictionary on a non-keyword window is refused");
        assert!(
            err.to_string()
                .contains("does not declare the column a keyword"),
            "{err}"
        );
    }

    /// **A flush that lands between a coalesce's plan and its replace keeps its own dictionary.**
    /// The replace names the consumed layers by path, so an extent appended meanwhile is neither
    /// consumed nor renumbered: it stays a layer of its own, its ordinals read against the
    /// dictionary that minted them, beside the coalesced layer read against the merged one. The
    /// appended extent numbers `gamma` as ordinal 0 where the merged dictionary numbers it 1, so
    /// a replace that read either against the other would answer wrongly here.
    #[test]
    fn a_flush_landing_between_plan_and_replace_keeps_its_own_dictionary() {
        let planned = keyword_column(
            "sub",
            vec![
                (None, universal(&[0]), dict(&["alpha"])),
                (
                    Some("extents/f1.arrow"),
                    partial(&[10], &[0]),
                    dict(&["gamma"]),
                ),
                (
                    Some("extents/f2.arrow"),
                    partial(&[11], &[0]),
                    dict(&["beta"]),
                ),
            ],
        );
        // The flush's extent, composed onto the generation after the window was planned.
        let live = planned
            .with_extents(
                &[(
                    "sub".to_string(),
                    "extents/f3.arrow".to_string(),
                    partial(&[12], &[0]),
                    Some(dict(&["gamma"])),
                )],
                &[],
                &[],
                &[],
            )
            .expect("the flush's extent composes");
        let next = live
            .with_coalesced(
                &[CoalescedWindow {
                    column: "sub".to_string(),
                    consumed: vec!["extents/f1.arrow".into(), "extents/f2.arrow".into()],
                    values_rel: "coalesced/c-1/attrs/sub/values.arrow".to_string(),
                    values: partial(&[10, 11], &[1, 0]),
                    dict: Some(dict(&["beta", "gamma"])),
                }],
                &[],
                None,
                None,
            )
            .expect("the window still rebases: its layers are named by path");
        assert_eq!(
            next.layer_count("sub"),
            Some(3),
            "base, the coalesced layer, and the flush's own"
        );
        let candidate = set(&[0, 10, 11, 12]);
        let resolve = |key: &str| {
            members(
                &next
                    .resolve("sub", &FilterOperand::TextEquals(key.into()), &candidate)
                    .expect("answers"),
            )
        };
        assert_eq!(resolve("gamma"), vec![10, 12]);
        assert_eq!(resolve("beta"), vec![11]);
        assert_eq!(
            next.stored_value("sub", 12),
            Some(RecordValue::Utf8("gamma".into()))
        );
    }

    /// A declaration as the manifest carries it.
    fn declared(name: &str, spelling: &str) -> tessera_store::manifest::DeclaredScalar {
        tessera_store::manifest::DeclaredScalar {
            name: name.to_string(),
            arrow_type: tessera_spatial::tiler::ScalarType::parse(spelling)
                .expect("the caller checked the spelling parses"),
            vocabulary: None,
            analyser: None,
            index: true,
            render: false,
        }
    }

    /// **Every declared type lands on a family deliberately, and every numeric one must be
    /// enumerated.** A keyword read as `Numeric` would publish `range` for a column of per-layer
    /// ordinals and accept one, comparing positions as though they were values — and answering the
    /// same request differently against the base and against a flush extent.
    ///
    /// `utf8` is in the table as a **negative**: it is not a declarable type (the schema parse
    /// refuses it), and a manifest carrying it anyway must not be read as the flat byte column that
    /// no longer exists. It lands on `Keyword`, whose open demands the dictionary beside the values
    /// and fails without one — a refusal, where a fallthrough to `Numeric` would answer.
    ///
    /// A spelling this build cannot parse is skipped rather than asserted about, so the case
    /// strengthens by itself the moment the declaration gains the type.
    #[test]
    fn every_declared_type_lands_on_a_deliberate_family() {
        use tessera_spatial::tiler::ScalarType;
        let cases = [
            ("bool", Family::Numeric),
            ("u8", Family::Numeric),
            ("u16", Family::Numeric),
            ("u32", Family::Numeric),
            ("u64", Family::Numeric),
            ("i8", Family::Numeric),
            ("i16", Family::Numeric),
            ("i32", Family::Numeric),
            ("i64", Family::Numeric),
            ("f32", Family::Numeric),
            ("f64", Family::Numeric),
            ("timestamp_us", Family::Numeric),
            ("utf8", Family::Keyword),
            ("keyword", Family::Keyword),
        ];
        let mut exercised = 0;
        for (spelling, expected) in cases {
            if ScalarType::parse(spelling).is_none() {
                continue;
            }
            let scalar = declared("col", spelling);
            let family = Family::of(&scalar);
            assert_eq!(family, expected, "type {spelling:?}");
            // **A string family never takes `range`.** A keyword's values are ordinals; comparing
            // them numerically is meaningless, and it is the specific defect a `Numeric`
            // fallthrough would produce.
            if family != Family::Numeric {
                assert!(
                    !family.operands().contains(&"range"),
                    "{spelling:?} must not be offered `range`"
                );
            }
            exercised += 1;
        }
        assert!(
            exercised >= cases.len() - 1,
            "only {exercised} of {} spellings parsed; the mapping is barely tested",
            cases.len()
        );
    }

    /// The family's operator list and its published name, which `/v1/meta` and the request parser
    /// both read from here — one derivation, so a client cannot be offered an operator the engine
    /// would not route.
    #[test]
    fn the_keyword_family_publishes_the_four_string_operators() {
        assert_eq!(Family::Keyword.as_str(), "keyword");
        assert_eq!(
            Family::Keyword.operands(),
            &["eq", "in", "prefix", "contains"]
        );
        // A string is never in the hot column, so a keyword affords the entity route alone.
        assert!(!Family::Keyword.reaches_hot_column());
    }
}
