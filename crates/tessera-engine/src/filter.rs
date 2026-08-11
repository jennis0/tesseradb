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
//! vocabulary is `listing = "public"`** (decision 0061). Postings resolve over the whole corpus and
//! are then intersected with the candidate, where the scan takes the candidate as its input — so
//! their work is a function of the *value named*. `probes/2026-08-08-filter-layout/` arm 9 measures
//! a hidden, scattered 10⁷-member value at **2.1 ms** intersected where an absent value costs
//! **0.000 ms**: a scattered value's members meet every container even when no bits do. Under
//! `per_viewer` that difference is a disclosure of exactly what the declaration withholds, so a
//! `per_viewer` column keeps the scan. Under `public` the value set is served to every principal
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

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use croaring::Bitmap;
use rustc_hash::FxHashSet;
use tessera_filter::{resolve_union, ColumnPostings, ValueColumn};

/// The operand value types, re-exported so a caller building a [`FilterOperand`] needs no
/// dependency on the filter crate — `check-layers.sh` denies `tessera-server` that edge, to keep the
/// server on engine API types only, and an operand's *values* are part of this crate's API surface
/// even though the column they are compared against is not.
pub use tessera_filter::{Endpoint, Scalar};
use tessera_store::manifest::Listing;
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
    /// Values are row data: `eq`, `in`, `prefix`, `contains`, over the stored bytes.
    Text,
    /// Values are numbers — every integer width, both floats, `timestamp_us` and `bool`. A range
    /// is a scan like everything else: no level tree, no bit slicing, no zone map
    /// (`filter-index.md` §3).
    Numeric,
}

impl Family {
    /// The operator names this family accepts, in the order `/v1/meta` publishes them.
    pub fn operands(self) -> &'static [&'static str] {
        match self {
            Family::Category => &["eq", "in"],
            Family::Text => &["eq", "in", "prefix", "contains"],
            Family::Numeric => &["eq", "in", "range"],
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Family::Category => "category",
            Family::Text => "string",
            Family::Numeric => "numeric",
        }
    }
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
/// Refusing would say the value exists, which is exactly what `listing = "per_viewer"` hides — so a
/// filter naming a value the principal may not see must be answered, and answered with nothing.
///
/// Code 0 is the vocabulary's reserved *absent* sentinel: never drawn, never bound to a key. That
/// makes it the correct answer rather than a convenient one — an item carrying no value must not
/// match a filter naming some other value either, and this is the code such an item carries.
pub const UNRESOLVABLE_VALUE: AttrLocalId = AttrLocalId::new(0);

/// A filter expression: a leaf predicate over one column, or a combinator over sub-expressions.
///
/// **Any boolean combination, evaluated inside the candidate** (decision 0060). Every node returns a
/// subset of the candidate — a leaf does, and union and intersection of subsets are subsets — so
/// **I12**'s "a filter narrows `M_sel` and never widens it" is a property of the shape rather than a
/// check, and no expression can name a set outside the principal's own mask.
///
/// `NoneOf` is deliberately absent: it needs the `per_viewer` rule decision 0060 records — negation
/// over a gated category must be evaluated *within the visible vocabulary*, or it becomes an
/// existence oracle over the values `listing` hides.
#[derive(Debug, Clone, PartialEq)]
pub enum FilterExpr {
    /// One column's predicate.
    Leaf {
        column: String,
        operand: FilterOperand,
    },
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
    /// - **C11**, decision 0060's existence oracle. `none_of: [every value I was offered]` returning
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
            FilterExpr::Leaf { .. } => 1,
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
            FilterExpr::Leaf { .. } => Ok(()),
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

/// One bundle's filter columns, keyed by declared column name.
///
/// Opened once per generation, not per request.
///
/// **Membership is not filterability.** The map holds every column the build wrote a value column
/// for — which includes a `listing = "per_viewer"` category that is not declared filterable, since
/// its postings are what `/v1/categories` derives value visibility from. [`FilterColumns::resolve`]
/// gates on [`Layers::filterable`] rather than on presence, so such a column is refused exactly as
/// an undeclared one is: an *undeclared* column is a caller error, where an unresolvable *value* is
/// an empty operand (`filter-surface.md` §2.1).
#[derive(Debug, Default)]
pub struct FilterColumns {
    columns: BTreeMap<String, Layers>,
}

/// How a category operand is answered on one column — decided at open from the declaration alone.
///
/// **Not a tuning knob and not a per-request choice.** See this module's header and decision 0061:
/// the postings' work is a function of the value named, which is a disclosure under
/// `listing = "per_viewer"` and a published fact under `listing = "public"`.
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
    layers: Vec<Layer>,
    /// Every entity any layer holds a value for. Kept so a new extent's disjointness can be
    /// checked in one bitmap operation — see [`FilterColumns::compose`] — rather than trusted.
    covered: Bitmap,
    /// **Declared `used_for = "filter"`.** A column may be held here without being filterable: a
    /// `listing = "per_viewer"` category owes membership postings whatever its `used_for` says
    /// (`filter-index.md` §2.3), and `/v1/categories` reads them from here. [`FilterColumns::resolve`]
    /// refuses such a column exactly as it refuses an undeclared one, so holding it opens no
    /// operand the schema did not declare.
    filterable: bool,
    /// The base build's per-value postings, where the column has them: every category column whose
    /// vocabulary is `per_viewer`, and every category column declared filterable.
    ///
    /// Held whatever the route, because the membership question `/v1/categories` asks is answered
    /// from these on a `per_viewer` column that the *filter* route deliberately does not use them
    /// for.
    postings: Option<Arc<ColumnPostings>>,
    route: Route,
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
#[derive(Debug, Clone)]
struct Layer {
    values_rel: Option<String>,
    values: Arc<ValueColumn>,
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
    /// The column has no derived membership postings, so the `per_viewer` visibility predicate
    /// cannot be evaluated for it. Fail-closed for the reason `categories.rs` gives: an empty value
    /// set is what a principal who may see none of them is told.
    MembershipUnavailable(String),
    /// A `none_of` names more or fewer than one column — see [`FilterExpr::NoneOf`].
    NegationSpansColumns { columns: Vec<String> },
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
        }
    }
}

impl std::error::Error for FilterError {}

/// The `listing` of the vocabulary a column draws from, or `None` where it is not a category.
///
/// Read from the **vocabulary**, which is the object that carries it. A column naming a vocabulary
/// the manifest does not hold is refused at seed (`Vocabularies::seed`), so the `None` this returns
/// for one means "not a category" and nothing else.
fn listing_of(
    scalar: &tessera_store::manifest::DeclaredScalar,
    vocabularies: &[tessera_store::manifest::ManifestVocabulary],
) -> Option<Listing> {
    let name = scalar.vocabulary.as_deref()?;
    vocabularies
        .iter()
        .find(|v| v.name == name)
        .map(|v| v.listing)
}

/// Does the build write a value column for this column?
///
/// **The mirror of `tessera_build`'s `postings_are_owed`, and it must stay one.** Reading a file set
/// the build did not write is a refusal at open; failing to read one it did write is a column whose
/// values are on disk and unserved. Two reasons, and the second is the one a reader will not expect:
/// `used_for = "filter"` is the obvious one, and `listing = "per_viewer"` is the other — that
/// control's gate is membership-derived (per-point-attributes §3.3) and the member sets it needs are
/// the postings derived from this column, so it gets both whatever its `used_for` says
/// (`filter-index.md` §2.3).
pub(crate) fn owes_value_column(
    scalar: &tessera_store::manifest::DeclaredScalar,
    vocabularies: &[tessera_store::manifest::ManifestVocabulary],
) -> bool {
    scalar.filter || listing_of(scalar, vocabularies) == Some(Listing::PerViewer)
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

/// The request path's two modes, and only those: `MappedSequential` is the fold's and is
/// deliberately unreachable from here (decision 0052).
fn request_access(mmap: bool) -> tessera_filter::Access {
    if mmap {
        tessera_filter::Access::Mapped
    } else {
        tessera_filter::Access::Read
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
    pub fn open(
        prefix_dir: &Path,
        partition: &str,
        declared: &[tessera_store::manifest::DeclaredScalar],
        vocabularies: &[tessera_store::manifest::ManifestVocabulary],
        extents: &[tessera_store::manifest::AttrExtent],
        mmap: bool,
    ) -> std::io::Result<Self> {
        let partition_dir = prefix_dir.join("partitions").join(partition);
        let mut columns = BTreeMap::new();
        for scalar in declared
            .iter()
            .filter(|d| owes_value_column(d, vocabularies))
        {
            let dir = partition_dir.join("attrs").join(&scalar.name);
            let base = Arc::new(ValueColumn::open_dir(&dir, request_access(mmap))?);
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
                && listing_of(scalar, vocabularies) == Some(Listing::Public)
            {
                Route::Postings
            } else {
                Route::Scan
            };
            columns.insert(
                scalar.name.clone(),
                Layers {
                    layers: vec![Layer {
                        values_rel: None,
                        values: base,
                    }],
                    covered,
                    filterable: scalar.filter,
                    postings,
                    route,
                },
            );
        }
        let mut open = FilterColumns { columns };
        for extent in extents {
            let column = tessera_filter::open_extent(
                &prefix_dir.join(&extent.values),
                &prefix_dir.join(&extent.presence),
                request_access(mmap),
            )?;
            open.compose(&extent.column, &extent.values, Arc::new(column))?;
        }
        Ok(open)
    }

    /// Add one flush's extent to a column, refusing an entity two layers both claim.
    ///
    /// **The refusal is what keeps a layered column a function.** Entity ids are permanent and
    /// issued from the high-water (**I9**), so an extent's entities belong to no earlier layer and
    /// the overlap is unreachable — which is exactly why it is checked here rather than reasoned
    /// about at the call site: if I9 ever failed, the symptom would be an entity matching two
    /// values at once and a filter naming either returning it, with nothing to notice.
    fn compose(
        &mut self,
        column: &str,
        values_rel: &str,
        extent: Arc<ValueColumn>,
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
        });
        Ok(())
    }

    /// This generation's columns with one flush's extents added — the successor generation's.
    ///
    /// Cheap by construction: the base columns are `Arc`s, so a flush that published one entity
    /// clones pointers rather than re-opening a memory-mapped column per declared attribute. Each
    /// extent is `(column, values path, opened column)`, the path being what the manifest names it
    /// by and what a later coalesce replaces it by.
    pub fn with_extents(
        &self,
        extents: &[(String, String, Arc<ValueColumn>)],
    ) -> std::io::Result<FilterColumns> {
        let mut next = FilterColumns {
            columns: self.columns.clone(),
        };
        for (column, values_rel, extent) in extents {
            next.compose(column, values_rel, Arc::clone(extent))?;
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
    pub fn with_coalesced(&self, windows: &[CoalescedWindow]) -> std::io::Result<FilterColumns> {
        let mut next = FilterColumns {
            columns: self.columns.clone(),
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
            layers.layers.push(Layer {
                values_rel: Some(window.values_rel.clone()),
                values: Arc::clone(&window.values),
            });
            // `covered` is unchanged by construction — the equality above is what says so — so it
            // is neither recomputed nor adjusted here.
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

        // **The routed pair, and the split between them is the whole of decision 0061.** The base
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
            out |= scan(&layer.values, operand, candidate);
        }
        Ok(out)
    }

    /// The membership question `/v1/categories` asks of a `per_viewer` column: which of this
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
        let postings = layers
            .postings
            .as_ref()
            .ok_or_else(|| FilterError::MembershipUnavailable(column.to_string()))?;

        let mut from_extents = FxHashSet::default();
        for layer in layers.layers.iter().filter(|l| l.values_rel.is_some()) {
            let layer = &layer.values;
            for entity in layer.present().and(candidate).iter() {
                if let Some(code) = layer.value_of(entity) {
                    from_extents.insert(code.raw());
                }
            }
        }
        Ok(CategoryMembership {
            column: column.to_string(),
            postings,
            candidate,
            from_extents,
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

    /// The entities of `candidate` that carry a value in `column` — the presence half of a
    /// negation, unioned across the layers exactly as a scan is.
    ///
    /// **A scan of every layer, never the postings**, even for a column whose `eq` is routed
    /// (decision 0061). Presence derived from postings would be a union over every code in the
    /// vocabulary — O(values) file reads to answer a question the value column answers in one
    /// intersection per layer — and it would answer it only for the base, since no flush writes
    /// postings.
    fn present_in(&self, column: &str, candidate: &Bitmap) -> Result<Bitmap, FilterError> {
        let layers = self
            .columns
            .get(column)
            .filter(|layers| layers.filterable)
            .ok_or_else(|| FilterError::UndeclaredColumn(column.to_string()))?;
        let mut out = Bitmap::new();
        for layer in &layers.layers {
            out |= layer.values.present_in(candidate);
        }
        Ok(out)
    }

    fn eval(&self, expr: &FilterExpr, candidate: &Bitmap) -> Result<Bitmap, FilterError> {
        match expr {
            FilterExpr::Leaf { column, operand } => self.resolve(column, operand, candidate),
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
    postings: &'a ColumnPostings,
    candidate: &'a Bitmap,
    /// The codes the candidate's *post-build* entities carry — the half no posting covers.
    from_extents: FxHashSet<u32>,
}

impl CategoryMembership<'_> {
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
        if self.from_extents.contains(&code) {
            return Ok(true);
        }
        let members = self
            .postings
            .entities(AttrLocalId::new(code))
            .map_err(|e| FilterError::PostingsUnreadable {
                column: self.column.clone(),
                detail: e.to_string(),
            })?;
        Ok(members.intersect(self.candidate))
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

/// One operand against one layer.
///
/// **The dispatch is here, once, rather than per layer inside a scan.** Each arm is the scan the
/// column crate exposes for that family, and the match is on the *operand* — never on the values —
/// so a layer costs what its share of the candidate costs and nothing about which value is sought
/// reaches this decision.
fn scan(values: &ValueColumn, operand: &FilterOperand, candidate: &Bitmap) -> Bitmap {
    match operand {
        FilterOperand::Equals(v) => values.scan_eq(candidate, *v),
        FilterOperand::In(vs) => values.scan_in(candidate, vs),
        FilterOperand::TextEquals(s) => values.scan_text_eq(candidate, s),
        FilterOperand::TextIn(ss) => values.scan_text_in(candidate, ss),
        FilterOperand::TextPrefix(s) => values.scan_text_prefix(candidate, s),
        FilterOperand::TextContains(s) => values.scan_text_contains(candidate, s),
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
