use std::sync::Arc;

use tessera_store::manifest::Visibility;

use super::error::ComposeError;

/// A filterable column's family, which decides which operators apply to it.
///
/// Published per column by `/v1/meta`. An operator outside a column's family is a shape error,
/// refused like an unknown column rather than answered as an empty operand, which is safe because
/// a family is deployment schema, identical for every principal, where a value's existence is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// Values are vocabulary entries: `eq`, `in`, over a key or a code.
    Category,
    /// Values are short strings matched exactly, as an ordinal into the layer's own sorted
    /// dictionary: `eq`, `in`, `prefix`, `contains`.
    Keyword,
    /// Values are numbers: every integer width, both floats, `timestamp_us` and `bool`. A range is
    /// a scan like everything else.
    Numeric,
    /// Values are prose, matched by what they say: `match` and its m-of-n form, from per-token
    /// postings rather than a scan. The only family with no value column: its terms are postings
    /// over a token dictionary and its prose is a record-blob row.
    Text,
}

impl Family {
    /// One column's family, from its declaration: the single derivation used by routing, `/v1/meta`
    /// and the request parser alike. An unrecognised type falls to `Keyword`, which refuses to open
    /// without a dictionary, rather than to `Numeric`, which would compare ordinals as values.
    pub fn of(scalar: &tessera_store::manifest::DeclaredScalar) -> Family {
        family_of(scalar.vocabulary.as_deref(), scalar.arrow_type)
    }

    /// A group-scoped family's family, the same derivation as [`Family::of`]. A scope changes only
    /// which column file a predicate reads, never how its values are read.
    pub fn of_scoped(scoped: &tessera_store::manifest::ScopedScalar) -> Family {
        family_of(scoped.vocabulary.as_deref(), scoped.arrow_type)
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

    /// The family name `/v1/meta` and the request parser both use.
    pub fn as_str(self) -> &'static str {
        match self {
            Family::Category => "category",
            Family::Keyword => "keyword",
            Family::Numeric => "numeric",
            Family::Text => "text",
        }
    }

    /// Does a value of this family occupy a slot in the hot column, and so afford the row-space
    /// route? Only the fixed-width families; `render` on a `keyword` is refused at the schema.
    pub fn reaches_hot_column(self) -> bool {
        match self {
            Family::Category | Family::Numeric => true,
            // Neither is a fixed-width slot: a keyword's value is its bytes and a text column's
            // value is not per entity at all.
            Family::Keyword | Family::Text => false,
        }
    }
}

/// The family a declaration's two deciding fields name.
fn family_of(vocabulary: Option<&str>, arrow_type: tessera_spatial::tiler::ScalarType) -> Family {
    if vocabulary.is_some() {
        return Family::Category;
    }
    if is_numeric(arrow_type) {
        return Family::Numeric;
    }
    if arrow_type == tessera_spatial::tiler::ScalarType::Text {
        return Family::Text;
    }
    Family::Keyword
}

/// The types whose values are numbers, listed because [`Family::of`] must not reach `Numeric` by
/// default.
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

/// The evaluation space(s) one filterable column affords, and the family whose rules its values are
/// read by.
///
/// The family travels with the placement because the row route cannot infer it from the stored
/// width: a rendered `u8` category and a rendered `u8` number are the same bytes, but the
/// category's absence is code 0 and the number's is the presence bitmap beside the column, where 0
/// is an ordinary value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub entity: bool,
    pub row: bool,
    pub family: Family,
}

impl Placement {
    /// The spaces one declared column affords, or `None` where it affords neither: a column stored
    /// and served at the drill-down and on no filter surface at all.
    pub(in crate::filter) fn of(
        scalar: &tessera_store::manifest::DeclaredScalar,
        vocabularies: &[tessera_store::manifest::ManifestVocabulary],
    ) -> Option<Placement> {
        // A rendered fixed-width column affords the row route. The entity route needs a value
        // column plus `index` or `render`. Text is entity-routed from postings with no value
        // column.
        let family = Family::of(scalar);
        let row = scalar.render && family.reaches_hot_column();
        let entity = if family == Family::Text {
            scalar.index
        } else {
            owes_value_column(scalar, vocabularies) && (scalar.index || row)
        };
        (row || entity).then_some(Placement {
            entity,
            row,
            family,
        })
    }
}

/// Is this column filterable at all: `index = true`, or rendered?
///
/// Render counts only for a family that reaches the hot column: `utf8`, `keyword` and `text` are
/// fixed-width refusals at the schema, so a rendered one of those never reaches this check true.
pub fn is_filterable(scalar: &tessera_store::manifest::DeclaredScalar) -> bool {
    scalar.index || (scalar.render && Family::of(scalar).reaches_hot_column())
}

/// The character a filter leaf pins a group-scoped attribute's view with: `sentiment@2026-Q3`.
/// Reserved out of a column name at the build, so a leaf carries at most one `@`.
pub const PIN: char = '@';

/// The internal key one view's column of a group-scoped family is held under, not a spelling a
/// caller writes: a request pins by key within the attribute's own group, and resolution produces
/// this.
pub fn scoped_column_name(name: &str, view_id: &str) -> String {
    format!("{name}{PIN}{view_id}")
}

/// The name a manifest extent entry composes onto: the column's own for an entity-scoped one, and
/// [`scoped_column_name`]'s resolved form for a view. One function, so a flush, a coalesce, open
/// and the live composition agree on the key.
pub fn extent_column_name(column: &str, view: Option<&str>) -> String {
    match view {
        Some(view) => scoped_column_name(column, view),
        None => column.to_string(),
    }
}

/// Is this extent's `(view, incarnation)` pair one the roster still declares?
///
/// `(None, None)` (entity-scoped) is always live. A half-stamped pair matches nothing and is
/// omitted: fail-closed rather than served under a guessed incarnation.
pub(crate) fn carries_live_view(
    incarnation_of: &dyn Fn(&str) -> Option<tessera_types::view::ViewIncarnation>,
    view: Option<&str>,
    incarnation: Option<tessera_types::view::ViewIncarnation>,
) -> bool {
    match (view, incarnation) {
        (None, None) => true,
        (Some(view), Some(incarnation)) => incarnation_of(view) == Some(incarnation),
        _ => false,
    }
}

/// Does this family have an entity-space value column, the artefact a flush writes an extent into
/// and the drill-down reads a value out of?
///
/// Every family but `text`. Wider than [`scoped_is_filterable`]: a family declaring neither `index`
/// nor `render` is stored and served at the drill-down without being searchable.
pub fn scoped_has_value_column(scoped: &tessera_store::manifest::ScopedScalar) -> bool {
    scoped.has_value_column()
}

/// Is this group-scoped family on the filter surface?
///
/// `index`, or `render`. Render licenses the per-view entity-space column, never the row route,
/// because a pin may name another view's column. `text` is excluded from the render arm, as
/// [`is_filterable`] excludes the string families from its own. The rule lives on
/// `ScopedScalar::is_filterable`, one crate down.
pub fn scoped_is_filterable(scoped: &tessera_store::manifest::ScopedScalar) -> bool {
    scoped.is_filterable()
}

/// Does this scoped family's per-view column carry keyed postings?
///
/// A category's, and only a category's: the postings answer `eq`/`in` on a `public` vocabulary and
/// derive value visibility for `/v1/categories` on a `derived` one.
pub(crate) fn scoped_owes_postings(scoped: &tessera_store::manifest::ScopedScalar) -> bool {
    scoped.vocabulary.is_some() && scoped_is_filterable(scoped)
}

/// The `visibility` of the vocabulary a scoped category's codes index.
pub(crate) fn scoped_visibility_of(
    scoped: &tessera_store::manifest::ScopedScalar,
    vocabularies: &[tessera_store::manifest::ManifestVocabulary],
) -> Option<Visibility> {
    visibility_of_vocabulary(scoped.vocabulary.as_deref(), vocabularies)
}

/// The `visibility` of the vocabulary a declaration names, or `None` where it names none.
fn visibility_of_vocabulary(
    vocabulary: Option<&str>,
    vocabularies: &[tessera_store::manifest::ManifestVocabulary],
) -> Option<Visibility> {
    let name = vocabulary?;
    vocabularies
        .iter()
        .find(|v| v.name == name)
        .map(|v| v.visibility)
}

/// The `visibility` of the vocabulary a column draws from, or `None` where it is not a category.
pub(in crate::filter) fn visibility_of(
    scalar: &tessera_store::manifest::DeclaredScalar,
    vocabularies: &[tessera_store::manifest::ManifestVocabulary],
) -> Option<Visibility> {
    visibility_of_vocabulary(scalar.vocabulary.as_deref(), vocabularies)
}

/// Does the build write a value column for this column?
///
/// `index = true`, or a `derived` vocabulary, since that control's membership postings are derived
/// from this column. Must agree with the build's own rule for the same question.
pub(crate) fn owes_value_column(
    scalar: &tessera_store::manifest::DeclaredScalar,
    vocabularies: &[tessera_store::manifest::ManifestVocabulary],
) -> bool {
    // Text owes none: it has many terms per entity and no per-entity slot.
    if scalar.arrow_type == tessera_spatial::tiler::ScalarType::Text {
        return false;
    }
    scalar.index || visibility_of(scalar, vocabularies) == Some(Visibility::Derived)
}

/// Does this column's value live in the record blob?
///
/// Blob-resident exactly when it has no other home. A public category with neither `index` nor
/// `render` is blob-resident like any other field with no other home.
/// Text is always blob-resident: its postings answer `match` but cannot reconstruct prose.
pub(crate) fn blob_resident(
    scalar: &tessera_store::manifest::DeclaredScalar,
    vocabularies: &[tessera_store::manifest::ManifestVocabulary],
) -> bool {
    if scalar.arrow_type == tessera_spatial::tiler::ScalarType::Text {
        return true;
    }
    !scalar.render && !owes_value_column(scalar, vocabularies)
}

/// Where a declared field's value is read from. A field may have more than one home: a rendered,
/// indexed column is in the view's row tail and in its value column both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldHomes {
    /// In the row tail of every view, read in the view a request names.
    pub rendered: bool,
    /// In a per-item column in stored order: [`owes_value_column`].
    pub value_column: bool,
    /// In the record store: [`blob_resident`].
    pub record: bool,
}

impl FieldHomes {
    /// One declared field's homes, by the rules the build writes them by.
    pub fn of(
        scalar: &tessera_store::manifest::DeclaredScalar,
        vocabularies: &[tessera_store::manifest::ManifestVocabulary],
    ) -> FieldHomes {
        FieldHomes {
            rendered: scalar.render,
            value_column: owes_value_column(scalar, vocabularies),
            record: blob_resident(scalar, vocabularies),
        }
    }

    /// The homes by the names `/v1/meta` publishes, in that order.
    pub fn names(self) -> Vec<&'static str> {
        [
            (self.rendered, "rendered"),
            (self.value_column, "value_column"),
            (self.record, "record"),
        ]
        .into_iter()
        .filter_map(|(held, name)| held.then_some(name))
        .collect()
    }
}

/// Does the build write derived postings for this column? Only a category earns them.
pub(crate) fn owes_postings(
    scalar: &tessera_store::manifest::DeclaredScalar,
    vocabularies: &[tessera_store::manifest::ManifestVocabulary],
) -> bool {
    scalar.vocabulary.is_some() && owes_value_column(scalar, vocabularies)
}

/// The analyser that indexed a text column, not the default: one resolution shared by the
/// entity-scoped column and the group-scoped family, so neither opens with a pipeline the index
/// was not built from.
/// A missing or unrecognised identity is a refusal, never a fallback.
pub(in crate::filter) fn resolve_analyser(
    column: &str,
    identity: Option<&str>,
) -> Result<Arc<tessera_analyse::Analyser>, ComposeError> {
    let identity = identity.ok_or_else(|| ComposeError::NoAnalyserRecorded {
        column: column.to_string(),
    })?;
    tessera_analyse::analyser_with_identity(identity)
        .map(Arc::new)
        .ok_or_else(|| ComposeError::UnknownAnalyser {
            column: column.to_string(),
            identity: identity.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

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


    /// Catches a numeric type falling through to the wrong family, and a stray `utf8` manifest
    /// landing on `Numeric` instead of the refusal `Keyword` gives it.
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
            // A string family never takes `range`: a keyword's values are ordinals, and comparing
            // them numerically is the specific defect a `Numeric` fallthrough would produce.
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


    /// Catches the keyword operator list or published name drifting from what the engine routes.
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
