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

use std::collections::BTreeMap;
use std::path::Path;

use croaring::Bitmap;
use rustc_hash::FxHashSet;
use tessera_filter::ValueColumn;
use tessera_types::{AttrLocalId, TermId};

use crate::compose::verdict;
use tessera_authz::fragment::FrozenFragment;
use tessera_lifecycle::buffer::IngestBuffer;
use tessera_lifecycle::overlay::Overlay;

/// What a request may ask of one column.
///
/// **Narrow on purpose.** The leak register is exhaustive because the query surface is enumerable
/// (§8.2), so a new operand is a design change with a per-family soundness statement, not an
/// addition here. Negation is absent for that reason and not by oversight: no corpus document
/// defines it, it is principal-dependent by construction, and it inverts a superset into a subset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterOperand {
    /// A category code. One value.
    Equals(AttrLocalId),
    /// A set of category codes — evaluated in one pass, so an IN-set naming ten invisible values
    /// costs what one naming ten visible values costs.
    In(Vec<AttrLocalId>),
    /// Exact string match against the stored bytes.
    TextEquals(String),
    /// Stored value starts with this.
    TextPrefix(String),
    /// Stored value contains this. Needs no trigram index: the value column *is* the verification
    /// route a trigram superset would have had to be checked against.
    TextContains(String),
}

/// One bundle's filter columns, keyed by declared column name.
///
/// Opened once per generation, not per request. A column absent from the map is one the schema did
/// not declare filterable — [`FilterColumns::resolve`] returns `None` for it, which the surface
/// turns into a refusal naming the column rather than an empty operand: an *undeclared* column is a
/// caller error, where an unresolvable *value* is an empty operand (`filter-surface.md` §2.1).
#[derive(Debug, Default)]
pub struct FilterColumns {
    columns: BTreeMap<String, ValueColumn>,
    /// Entities `[0, covered)` have values in these columns. Entities at or above it were
    /// allocated after the build and **have no filter data at all** — see [`FilterError`].
    covered: u32,
}

/// Why a filter could not be answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterError {
    /// The column is not declared filterable. A caller error, distinguishable from an empty
    /// result, which an undeclared column must never be served as.
    UndeclaredColumn(String),
    /// ⊘ The candidate reaches entities the filter artefact does not cover.
    ///
    /// **Refused rather than answered short.** A flush appends entities; `filter-index.md` §2.1
    /// specifies a value-column extent per flush, and nothing emits one, so entities allocated
    /// since the build carry no values. Answering anyway would omit them — which narrows `M_sel`
    /// and is safe under **I12**, but produces a result *indistinguishable from a correct one*.
    /// That is the same reason `/v1/categories` refuses a `per_viewer` column rather than serving
    /// it empty (per-point-attributes §3.3): an underived answer must not wear a derived one's
    /// clothes (decision 0013).
    CoverageEndsAtBuild { covered: u32, requested: u32 },
}

impl std::fmt::Display for FilterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FilterError::UndeclaredColumn(name) => {
                write!(f, "column '{name}' is not declared filterable")
            }
            FilterError::CoverageEndsAtBuild { covered, requested } => write!(
                f,
                "the filter index covers entities below {covered}; this request reaches {requested}. \
                 Entities allocated since the build carry no filter values, because the per-flush \
                 value-column extent (filter-index §2.1) is specified and not built. Refused rather \
                 than answered short: a short result is indistinguishable from a correct one"
            ),
        }
    }
}

impl std::error::Error for FilterError {}

impl FilterColumns {
    /// Open every filter column the manifest declares, under `partition_dir`.
    ///
    /// A declared column whose files are missing is an **error**, not an absence: the manifest
    /// digests them, so a missing one means the bundle is not what its manifest says it is.
    /// `covered` is the build's entity high-water: the columns hold values for `[0, covered)` and
    /// nothing above it.
    pub fn open(
        partition_dir: &Path,
        declared: &[tessera_store::manifest::DeclaredScalar],
        covered: u32,
    ) -> std::io::Result<Self> {
        let mut columns = BTreeMap::new();
        for scalar in declared.iter().filter(|d| d.filter) {
            let dir = partition_dir.join("attrs").join(&scalar.name);
            columns.insert(scalar.name.clone(), ValueColumn::open_dir(&dir)?);
        }
        Ok(FilterColumns { columns, covered })
    }

    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    /// Refuse a candidate that reaches past the artefact. Checked on the candidate's **maximum**,
    /// so a principal whose visible set predates the build is answered normally — the refusal is as
    /// narrow as the gap is.
    fn check_coverage(&self, candidate: &Bitmap) -> Result<(), FilterError> {
        match candidate.maximum() {
            Some(max) if max >= self.covered => Err(FilterError::CoverageEndsAtBuild {
                covered: self.covered,
                requested: max,
            }),
            _ => Ok(()),
        }
    }

    /// Entities in `candidate` whose value for `column` satisfies `operand`.
    ///
    /// The result is a subset of `candidate` by
    /// construction, so it is already inside the composed verdict — **I12**'s "a filter narrows
    /// `M_sel` and never widens it" is a property of the shape here rather than a check.
    pub fn resolve(
        &self,
        column: &str,
        operand: &FilterOperand,
        candidate: &Bitmap,
    ) -> Result<Bitmap, FilterError> {
        self.check_coverage(candidate)?;
        let values = self
            .columns
            .get(column)
            .ok_or_else(|| FilterError::UndeclaredColumn(column.to_string()))?;
        Ok(match operand {
            FilterOperand::Equals(v) => values.scan_eq(candidate, *v),
            FilterOperand::In(vs) => values.scan_in(candidate, vs),
            FilterOperand::TextEquals(s) => values.scan_text_eq(candidate, s),
            FilterOperand::TextPrefix(s) => values.scan_text_prefix(candidate, s),
            FilterOperand::TextContains(s) => values.scan_text_contains(candidate, s),
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
        self.check_coverage(candidate)?;
        let mut live = candidate.clone();
        for (column, operand) in operands {
            live = self.resolve(column, operand, &live)?;
            if live.is_empty() {
                break;
            }
        }
        Ok(live)
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
