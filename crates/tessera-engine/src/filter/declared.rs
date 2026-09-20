use std::sync::Arc;

use tessera_store::manifest::Visibility;

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
    /// refused in both directions (see [`crate::filter::FilterColumns::compose`]), so a misclassified column
    /// refuses to open. Defaulting to `Numeric` would instead compare a keyword's per-layer
    /// *ordinals* as though they were values — publishing `range` for the column, accepting one,
    /// and answering the same request differently against the base and against a flush extent,
    /// none of it visible.
    pub fn of(scalar: &tessera_store::manifest::DeclaredScalar) -> Family {
        family_of(scalar.vocabulary.as_deref(), scalar.arrow_type)
    }

    /// One **group-scoped** column family's family (`views.md` §5) — the same derivation as
    /// [`Family::of`], over the declaration a scoped family records instead of a declared
    /// scalar's. The two read the same two fields, and a scope changes only which column file a
    /// predicate reads, never how its values are read.
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

/// The family a declaration's two deciding fields name — what [`Family::of`] and
/// [`Family::of_scoped`] each ask, over the two declaration types.
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

impl Placement {
    /// The spaces one declared column affords, or `None` where it affords neither — a column
    /// stored and served at the drill-down and on no filter surface at all.
    ///
    /// **The single derivation**, so the placement a restart's `open` records, the one a runtime
    /// declaration takes and the licence a column's own `filterable` carries (which is `entity`)
    /// cannot come to disagree about one declaration.
    pub(in crate::filter) fn of(
        scalar: &tessera_store::manifest::DeclaredScalar,
        vocabularies: &[tessera_store::manifest::ManifestVocabulary],
    ) -> Option<Placement> {
        // A rendered column always affords the row route — its values are in the hot column, and
        // both families that reach it can express absence there. The entity route needs an
        // entity-space value column AND a licence to answer a filter from it: `index`, or 0068's
        // "render implies filterable" over the per-viewer vocabulary floor. A `derived` column
        // with neither flag keeps its value column for membership and stays unfilterable.
        let family = Family::of(scalar);
        let row = scalar.render && family.reaches_hot_column();
        // Text is entity-space filterable without a value column: its `match` is answered from
        // postings, which is the one route in this system that reads no per-entity slot.
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
/// ([`crate::filter::columns::open::open_scoped_column`]'s placement).
///
/// **`text` is excluded from the render arm** rather than assumed away, as [`is_filterable`]
/// excludes the string families from its own: `render` on a scoped `text` family is refused at the
/// declaration, so the combination reaches no manifest a build wrote — and a manifest that
/// carried it would name a token index no pass produced, which this predicate would otherwise
/// demand at open.
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

/// The `visibility` of the vocabulary a scoped category's codes index — [`visibility_of`]'s
/// question over a family's declaration.
pub(crate) fn scoped_visibility_of(
    scoped: &tessera_store::manifest::ScopedScalar,
    vocabularies: &[tessera_store::manifest::ManifestVocabulary],
) -> Option<Visibility> {
    visibility_of_vocabulary(scoped.vocabulary.as_deref(), vocabularies)
}

/// The `visibility` of the vocabulary a declaration names, or `None` where it names none — what
/// [`visibility_of`] and [`scoped_visibility_of`] each ask, over the two declaration types.
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
///
/// Read from the **vocabulary**, which is the object that carries it. A column naming a vocabulary
/// the manifest does not hold is refused at seed (`Vocabularies::seed`), so the `None` this returns
/// for one means "not a category" and nothing else.
pub(in crate::filter) fn visibility_of(
    scalar: &tessera_store::manifest::DeclaredScalar,
    vocabularies: &[tessera_store::manifest::ManifestVocabulary],
) -> Option<Visibility> {
    visibility_of_vocabulary(scalar.vocabulary.as_deref(), vocabularies)
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
pub(in crate::filter) fn resolve_analyser(
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
    tessera_analyse::analyser_with_identity(identity)
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
