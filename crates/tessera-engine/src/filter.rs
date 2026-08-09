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
//! # Why the work carries no timing channel
//!
//! Resolution walks the candidate and tests the values it selects, so the work is a function of
//! `(candidate, column)` and never of the value sought. A value the principal cannot see costs what
//! a value that does not exist costs — per-point-attributes §3.8's requirement that the two be
//! indistinguishable *in work*, obtained structurally rather than by padding. The one place that
//! could be given away is the accelerator: a category's derived posting is intersected rather than
//! scanned, and `probes/2026-08-08-filter-layout/` measures that pair at 0.000 ms alike for a
//! valueless value and a hidden 250M-member one, because Roaring short-circuits on container keys.
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

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use croaring::Bitmap;
use rustc_hash::FxHashSet;
use tessera_filter::ValueColumn;

/// The operand value types, re-exported so a caller building a [`FilterOperand`] needs no
/// dependency on the filter crate — `check-layers.sh` denies `tessera-server` that edge, to keep the
/// server on engine API types only, and an operand's *values* are part of this crate's API surface
/// even though the column they are compared against is not.
pub use tessera_filter::{Endpoint, Scalar};
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
/// **Any boolean combination, evaluated inside the candidate** (decision 0059). Every node returns a
/// subset of the candidate — a leaf does, and union and intersection of subsets are subsets — so
/// **I12**'s "a filter narrows `M_sel` and never widens it" is a property of the shape rather than a
/// check, and no expression can name a set outside the principal's own mask.
///
/// `NoneOf` is deliberately absent: it needs the `per_viewer` rule decision 0059 records — negation
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
            FilterExpr::AllOf(kids) | FilterExpr::AnyOf(kids) => {
                1 + kids.iter().map(FilterExpr::depth).max().unwrap_or(0)
            }
        }
    }
}

/// One bundle's filter columns, keyed by declared column name.
///
/// Opened once per generation, not per request. A column absent from the map is one the schema did
/// not declare filterable — [`FilterColumns::resolve`] returns `None` for it, which the surface
/// turns into a refusal naming the column rather than an empty operand: an *undeclared* column is a
/// caller error, where an unresolvable *value* is an empty operand (`filter-surface.md` §2.1).
#[derive(Debug, Default)]
pub struct FilterColumns {
    columns: BTreeMap<String, Layers>,
}

/// One column as it is scanned: the build's base column, then one layer per flush that has
/// published since (see this module's header).
///
/// `Arc` per layer because a publication builds the next generation's columns from the live ones —
/// the base is a memory map of a multi-gigabyte file, and the flush that added one entity must not
/// re-open it.
#[derive(Debug, Clone)]
struct Layers {
    layers: Vec<Arc<ValueColumn>>,
    /// Every entity any layer holds a value for. Kept so a new extent's disjointness can be
    /// checked in one bitmap operation — see [`FilterColumns::compose`] — rather than trusted.
    covered: Bitmap,
}

/// Why a filter could not be answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterError {
    /// The column is not declared filterable. A caller error, distinguishable from an empty
    /// result, which an undeclared column must never be served as.
    UndeclaredColumn(String),
    /// The expression nests deeper than [`MAX_FILTER_DEPTH`].
    TooDeep { depth: usize, max: usize },
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
        }
    }
}

impl std::error::Error for FilterError {}

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
    pub fn open(
        prefix_dir: &Path,
        partition: &str,
        declared: &[tessera_store::manifest::DeclaredScalar],
        extents: &[tessera_store::manifest::AttrExtent],
        mmap: bool,
    ) -> std::io::Result<Self> {
        let partition_dir = prefix_dir.join("partitions").join(partition);
        let mut columns = BTreeMap::new();
        for scalar in declared.iter().filter(|d| d.filter) {
            let dir = partition_dir.join("attrs").join(&scalar.name);
            let base = Arc::new(ValueColumn::open_dir(&dir, mmap)?);
            let covered = base.present();
            columns.insert(
                scalar.name.clone(),
                Layers {
                    layers: vec![base],
                    covered,
                },
            );
        }
        let mut open = FilterColumns { columns };
        for extent in extents {
            let column = tessera_filter::open_extent(
                &prefix_dir.join(&extent.values),
                &prefix_dir.join(&extent.presence),
                mmap,
            )?;
            open.compose(&extent.column, Arc::new(column))?;
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
    fn compose(&mut self, column: &str, extent: Arc<ValueColumn>) -> std::io::Result<()> {
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
        layers.layers.push(extent);
        Ok(())
    }

    /// This generation's columns with one flush's extents added — the successor generation's.
    ///
    /// Cheap by construction: the base columns are `Arc`s, so a flush that published one entity
    /// clones pointers rather than re-opening a memory-mapped column per declared attribute.
    pub fn with_extents(
        &self,
        extents: &[(String, Arc<ValueColumn>)],
    ) -> std::io::Result<FilterColumns> {
        let mut next = FilterColumns {
            columns: self.columns.clone(),
        };
        for (column, extent) in extents {
            next.compose(column, Arc::clone(extent))?;
        }
        Ok(next)
    }

    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
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
        let column = self
            .columns
            .get(column)
            .ok_or_else(|| FilterError::UndeclaredColumn(column.to_string()))?;
        let mut out = Bitmap::new();
        for values in &column.layers {
            out |= scan(values, operand, candidate);
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
        self.eval(expr, candidate)
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
        }
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
