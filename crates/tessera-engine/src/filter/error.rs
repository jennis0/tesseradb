/// Why a filter could not be answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterError {
    /// The column is not declared filterable. A caller error, distinguishable from an empty
    /// result, which an undeclared column must never be served as.
    UndeclaredColumn(String),
    /// The expression nests deeper than [`crate::filter::MAX_FILTER_DEPTH`].
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
    /// as ordinary. An ordinary miss is not this — it is [`crate::filter::scan::keyword::NO_SUCH_ORDINAL`], and it still scans.
    DictionaryUnreadable { column: String, detail: String },
    /// The column has no derived membership postings, so the `derived` visibility predicate
    /// cannot be evaluated for it. Fail-closed for the reason `categories.rs` gives: an empty value
    /// set is what a principal who may see none of them is told.
    MembershipUnavailable(String),
    /// A `none_of` names more or fewer than one column — see [`crate::filter::FilterExpr::NoneOf`].
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
    /// [`crate::filter::FilterColumns::evaluate_routed`] takes a tree carrying one.
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
