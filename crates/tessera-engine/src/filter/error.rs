/// Why a bundle's filter columns could not be opened, or a published layer composed onto them.
///
/// Every refusal carries the facts rather than a sentence about them, so a caller, a test included,
/// can tell two refusals apart without reading their wording.
#[derive(Debug)]
pub enum ComposeError {
    /// An extent names a column this generation holds no layers of its kind for.
    UnknownColumn { column: String },
    /// An extent claims entities an earlier layer already holds values for.
    Overlap { column: String },
    /// A flush wrote a base for a group-scoped family this bundle does not declare.
    UndeclaredScopedFamily { column: String, view: String },
    /// A coalesce consumed an extent this generation holds no layer for.
    MissingLayer { column: String, rel: String },
    /// A coalesced extent stands for a different entity set from the layers it replaces.
    CoverageMismatch {
        column: String,
        replacement: u64,
        consumed: usize,
        covered: u64,
    },
    /// A coalesced entity→term stack holds lists for a different entity set from the live one.
    EntityTermsCoverage { replacement: u64, held: u64 },
    /// A keyword layer arrived without the dictionary its ordinals number.
    KeywordWithoutDictionary { column: String },
    /// A dictionary arrived on a layer of a column the schema does not call a keyword.
    DictionaryOnOtherFamily { column: String },
    /// A text layer's dictionary and its postings are different lengths.
    TermsAndPostingsDisagree {
        column: String,
        layer: String,
        terms: u32,
        postings: u32,
    },
    /// A text column whose manifest records no analyser identity.
    NoAnalyserRecorded { column: String },
    /// A text column indexed by an analyser this binary does not carry.
    UnknownAnalyser { column: String, identity: String },
    /// The record blob's stack refused to open, malformed rather than absent.
    RecordUnreadable(String),
    /// The entity→term stack refused to open.
    EntityTermsUnreadable(String),
    /// An artefact would not read. Carried whole, so the kind survives.
    Io(std::io::Error),
}

impl std::fmt::Display for ComposeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ComposeError::UnknownColumn { column } => write!(
                f,
                "an extent names column '{column}', which this generation holds no layers for"
            ),
            ComposeError::Overlap { column } => write!(
                f,
                "a filter extent for column '{column}' claims entities an earlier layer already \
                 holds values for"
            ),
            ComposeError::UndeclaredScopedFamily { column, view } => write!(
                f,
                "a flush wrote a base for the group-scoped column family '{column}' under view \
                 '{view}', which this bundle does not declare"
            ),
            ComposeError::MissingLayer { column, rel } => write!(
                f,
                "a coalesce for column '{column}' consumed extent '{rel}', which this generation \
                 holds no layer for"
            ),
            ComposeError::CoverageMismatch {
                column,
                replacement,
                consumed,
                covered,
            } => write!(
                f,
                "the coalesced extent for column '{column}' is present for {replacement} entities \
                 where the {consumed} layers it replaces cover {covered}"
            ),
            ComposeError::EntityTermsCoverage { replacement, held } => write!(
                f,
                "a coalesce's entity→term stack holds lists for {replacement} entities where the \
                 one it replaces holds {held}"
            ),
            ComposeError::KeywordWithoutDictionary { column } => write!(
                f,
                "a layer for keyword column '{column}' carries no sorted dictionary to read its \
                 ordinals against"
            ),
            ComposeError::DictionaryOnOtherFamily { column } => write!(
                f,
                "a layer for column '{column}' carries a sorted dictionary and the schema does \
                 not declare the column a keyword"
            ),
            ComposeError::TermsAndPostingsDisagree {
                column,
                layer,
                terms,
                postings,
            } => write!(
                f,
                "the {layer} text layer of column '{column}' holds {terms} terms and {postings} \
                 postings records"
            ),
            ComposeError::NoAnalyserRecorded { column } => write!(
                f,
                "column '{column}' is text and the manifest records no analyser identity for it"
            ),
            ComposeError::UnknownAnalyser { column, identity } => write!(
                f,
                "column '{column}' was indexed by analyser '{identity}', which this binary does \
                 not carry"
            ),
            ComposeError::RecordUnreadable(detail) => write!(f, "{detail}"),
            ComposeError::EntityTermsUnreadable(detail) => write!(f, "{detail}"),
            ComposeError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ComposeError {}

impl From<std::io::Error> for ComposeError {
    fn from(e: std::io::Error) -> ComposeError {
        ComposeError::Io(e)
    }
}

impl From<tessera_filter::DictError> for ComposeError {
    fn from(e: tessera_filter::DictError) -> ComposeError {
        ComposeError::Io(e.into())
    }
}

impl From<ComposeError> for std::io::Error {
    fn from(e: ComposeError) -> std::io::Error {
        match e {
            ComposeError::Io(e) => e,
            other => std::io::Error::new(std::io::ErrorKind::InvalidData, other.to_string()),
        }
    }
}

/// Why a filter could not be answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterError {
    /// The column is not declared filterable. A caller error, distinguishable from an empty result,
    /// which an undeclared column must never be served as.
    UndeclaredColumn(String),
    /// The expression nests deeper than [`crate::filter::MAX_FILTER_DEPTH`].
    TooDeep { depth: usize, max: usize },
    /// A routed column's postings could not be read. Fail-closed: falling back to the scan would
    /// hide that the accelerator is unreadable, and an empty result would say no entity carries the
    /// value; neither is distinguishable from a right answer.
    PostingsUnreadable { column: String, detail: String },
    /// A keyword layer's sorted dictionary refused a read. Fail-closed for one more reason of its
    /// own: a dictionary that answered wrongly would resolve a needle to the wrong ordinal and
    /// return a different value's entities. An ordinary miss is not this; it is
    /// [`crate::filter::scan::keyword::NO_SUCH_ORDINAL`], and it still scans.
    DictionaryUnreadable { column: String, detail: String },
    /// The column has no derived membership postings, so the `derived` visibility predicate cannot
    /// be evaluated for it. Fail-closed: an empty value set is what a principal who may see none of
    /// them is told.
    MembershipUnavailable(String),
    /// A `none_of` names more or fewer than one column; see [`crate::filter::FilterExpr::NoneOf`].
    NegationSpansColumns { columns: Vec<String> },
    /// A `none_of` names a column with no presence set to subtract from, a `text` column, whose
    /// index is postings over words and whose values are blob rows.
    ///
    /// Fail-closed: a negation is `present ∖ matched`, and with no presence the left operand is
    /// empty, so every such request would answer "no items" with a 200, indistinguishable from a
    /// corpus where nothing matches. Refusing names the column and the reason instead.
    NegationWithoutPresence { column: String, family: String },
    /// A region leaf reached the entity-space evaluator, which cannot answer it: a region is a
    /// statement about position, and position is row space. Only
    /// [`crate::filter::FilterColumns::evaluate_routed`] takes a tree carrying one.
    RegionInEntitySpace,
    /// The region resolver could not answer a leaf for this generation, a cancelled build or a view
    /// whose segments could not be assembled. Fail-closed: an empty operand here would be
    /// indistinguishable from a shape that holds nothing.
    RegionUnavailable(String),
    /// A `member_of` leaf reached the entity-space evaluator. Row space only, exactly as
    /// [`FilterError::RegionInEntitySpace`] is.
    MemberOfInEntitySpace,
    /// A `member_of` leaf named a layer this principal's `/v1/meta` does not list. The caller's
    /// fault: a layer name is deployment schema, resolved the same way for a gate-failed name and a
    /// never-registered one, so refusing by name discloses nothing the principal was not already
    /// told. The artifact itself is a value and is never refused this way.
    UnknownLayer(String),
    /// The `member_of` resolver could not answer for this generation. Fail-closed, for
    /// [`FilterError::RegionUnavailable`]'s reason.
    MemberOfUnavailable(String),
}

impl std::fmt::Display for FilterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FilterError::UndeclaredColumn(name) => write!(
                f,
                "column '{name}' is not declared filterable; /v1/meta lists the columns a filter \
                 may name"
            ),
            FilterError::TooDeep { depth, max } => write!(
                f,
                "the filter nests {depth} deep and the limit is {max}; flatten it"
            ),
            FilterError::PostingsUnreadable { column, detail } => write!(
                f,
                "column '{column}' is routed through its derived postings and they could not be \
                 read ({detail})"
            ),
            FilterError::DictionaryUnreadable { column, detail } => write!(
                f,
                "a sorted dictionary of keyword column '{column}' could not be read ({detail})"
            ),
            FilterError::MembershipUnavailable(column) => write!(
                f,
                "column '{column}' carries no derived membership postings, so its per-viewer value \
                 visibility cannot be derived"
            ),
            FilterError::NegationSpansColumns { columns } => match columns.len() {
                0 => write!(
                    f,
                    "a 'none_of' names no column, so there is nothing for an item to be required \
                     to carry a value in; name one column inside it"
                ),
                _ => write!(
                    f,
                    "a 'none_of' names {} columns ({}) and may name only one; write all_of: \
                     [{{none_of: [...]}}, {{none_of: [...]}}], one 'none_of' per column",
                    columns.len(),
                    columns.join(", ")
                ),
            },
            FilterError::NegationWithoutPresence { column, family } => write!(
                f,
                "a 'none_of' names column '{column}', which is a {family} column and holds no \
                 per-item value to be present or absent; say what is wanted with a positive \
                 expression instead"
            ),
            FilterError::RegionInEntitySpace => write!(
                f,
                "a 'region' leaf is answered in row space and cannot be evaluated in entity space"
            ),
            FilterError::RegionUnavailable(detail) => write!(
                f,
                "the 'region' leaf could not be resolved against this generation ({detail})"
            ),
            FilterError::MemberOfInEntitySpace => write!(
                f,
                "a 'member_of' leaf is answered in row space and cannot be evaluated in entity \
                 space"
            ),
            FilterError::UnknownLayer(layer) => write!(
                f,
                "'member_of' names layer '{layer}', which this deployment does not publish to \
                 you; /v1/meta lists the layers a 'member_of' leaf may name"
            ),
            FilterError::MemberOfUnavailable(detail) => write!(
                f,
                "the 'member_of' leaf could not be resolved against this generation ({detail})"
            ),
        }
    }
}

impl FilterError {
    /// A postings read that could not vouch for its answer, named by the column it was made for.
    pub(in crate::filter) fn postings_unreadable(
        column: &str,
        detail: impl std::fmt::Display,
    ) -> FilterError {
        FilterError::PostingsUnreadable {
            column: column.to_string(),
            detail: detail.to_string(),
        }
    }

    /// Is this the caller's fault or the deployment's? Decides the status code.
    ///
    /// A malformed expression is a `422`: the caller can fix it, and refusing tells them nothing
    /// about the corpus, since a column's existence and its family are deployment schema published
    /// to every principal alike. An artefact that cannot be read is a `500`, fail-closed, because
    /// answering short or answering empty is indistinguishable from a right answer.
    ///
    /// [`FilterError::UndeclaredColumn`] is the caller's fault because a filterable column's name
    /// is public. An unknown value is different again and never an error at all: it is an empty
    /// operand, because refusing it would make the filter an existence oracle over exactly what
    /// `visibility = "derived"` hides.
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
