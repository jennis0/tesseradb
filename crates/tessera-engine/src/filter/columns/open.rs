use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use croaring::Bitmap;
use tessera_filter::{ColumnPostings, RecordExtentPaths, RecordStack, SortedDict, ValueColumn};
use tessera_store::manifest::Visibility;

use super::{
    record_open_error, request_access, text_layer, Column, FilterColumns, Layer, Route, TextLayer,
};
use crate::filter::declared::{resolve_analyser, visibility_of};
use crate::filter::{
    blob_resident, carries_live_view, extent_column_name, owes_postings, owes_value_column,
    scoped_column_name, scoped_is_filterable, scoped_owes_postings, scoped_visibility_of, Family,
    Placement,
};

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
pub(super) fn open_scoped_column(
    partition_dir: &Path,
    family: &tessera_store::manifest::ScopedScalar,
    view_id: &str,
    incarnation: tessera_types::view::ViewIncarnation,
    vocabularies: &[tessera_store::manifest::ManifestVocabulary],
    mmap: bool,
) -> std::io::Result<(String, Placement, Column)> {
    let scoped_family = Family::of_scoped(family);
    // **A family with neither flag is opened and is not filterable** (owner ruling 2026-09-01).
    // Its per-view column is on disc exactly as an indexed one's is, and the drill-down reads one
    // entity's value out of it — so the column is held here, `filterable: false`, which is the
    // same standing an entity-scoped `derived` category with neither flag already has: `resolve`
    // refuses it by name and `FilterColumns::stored_value` answers from it.
    let filterable = scoped_is_filterable(family);
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
    // are the token dictionary and the positional postings over it. The base's layer alone is
    // opened here; each caller appends the published extents this view's column has taken since —
    // [`FilterColumns::open`] from the manifest, and a flush from what it wrote.
    if scoped_family == Family::Text {
        // The analyser a text family's terms were produced by: an analyser this binary does not
        // carry is the same refusal an entity-scoped text column's is — a `match` answered from a
        // different segmentation is a wrong answer wearing a correct one's clothes.
        let analyser = resolve_analyser(&family.name, family.analyser.as_deref())?;
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
            Column::text(declared_index, filterable, analyser, text),
        ));
    }
    let base = Arc::new(ValueColumn::open_dir(&dir, request_access(mmap))?);
    let dict = (scoped_family == Family::Keyword)
        .then(|| SortedDict::open_dir(&dir, request_access(mmap)).map(Arc::new))
        .transpose()?;
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
        // **The licence, not the fact that it opened.** A family carrying neither flag is opened
        // so the drill-down can read a value out of it, and `evaluate` gates on that flag — so a
        // leaf that somehow reached it is refused exactly as an unfilterable entity-scoped
        // column's is. `EngineMeta::resolve_filter_column` refuses such a leaf one layer earlier,
        // before any column is looked up; this is the second of the two.
        Column::values(
            declared_index,
            filterable,
            scoped_family,
            Some(Layer {
                values_rel: None,
                values: base,
                dict,
            }),
            postings,
            route,
        ),
    ))
}

/// The layers a run of published text extents opens as, in the order the manifest lists them —
/// what a column's base is followed by at [`FilterColumns::open`], entity-scoped and group-scoped
/// alike. `column` is the name the column is held under, which for a group-scoped family is
/// [`scoped_column_name`]'s resolved form.
fn text_extent_layers<'a>(
    prefix_dir: &Path,
    extents: impl Iterator<Item = &'a tessera_store::manifest::TextExtent>,
    column: &str,
    mmap: bool,
) -> std::io::Result<Vec<TextLayer>> {
    extents
        .map(|extent| {
            text_layer(
                SortedDict::open(&prefix_dir.join(&extent.dict), request_access(mmap))?,
                ColumnPostings::open(&prefix_dir.join(&extent.postings), mmap)?,
                column,
                &extent.dict,
                Bitmap::deserialize::<croaring::Portable>(&std::fs::read(
                    prefix_dir.join(&extent.presence),
                )?),
                Some(extent.dict.clone()),
            )
        })
        .collect()
}

/// The stack a column declared at a running service opens with before any fold: no base, no
/// postings, the extents composed later (`ingest.md` §6.3). `None` for a column with no
/// entity-space home, which holds no stack at all.
pub(super) fn runtime_layers(
    scalar: &tessera_store::manifest::DeclaredScalar,
    declared_index: usize,
    vocabularies: &[tessera_store::manifest::ManifestVocabulary],
) -> std::io::Result<Option<Column>> {
    let family = Family::of(scalar);
    if family == Family::Text {
        if !scalar.index {
            return Ok(None);
        }
        let analyser = resolve_analyser(&scalar.name, scalar.analyser.as_deref())?;
        return Ok(Some(Column::text(
            declared_index,
            true,
            analyser,
            Vec::new(),
        )));
    }
    if !owes_value_column(scalar, vocabularies) {
        return Ok(None);
    }
    let row = scalar.render && family.reaches_hot_column();
    Ok(Some(Column::values(
        declared_index,
        scalar.index || row,
        family,
        None,
        None,
        // Every operand is a scan until the fold rebuilds the postings from the folded column.
        Route::Scan,
    )))
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
                text_layers.extend(text_extent_layers(
                    prefix_dir,
                    text_extents
                        .iter()
                        .filter(|e| e.column == scalar.name && e.view.is_none()),
                    &scalar.name,
                    mmap,
                )?);
                let analyser = resolve_analyser(&scalar.name, scalar.analyser.as_deref())?;
                columns.insert(
                    scalar.name.clone(),
                    Column::text(declared_index, true, analyser, text_layers),
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
                Column::values(
                    declared_index,
                    entity,
                    family,
                    Some(Layer {
                        values_rel: None,
                        values: base,
                        dict: base_dict,
                    }),
                    postings,
                    route,
                ),
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
                let (name, placement, mut column) = open_scoped_column(
                    &partition_dir,
                    family,
                    view_id,
                    incarnation,
                    vocabularies,
                    mmap,
                )?;
                // A text family's flushed layers are added here; every other family's arrive
                // through `compose` below. A key created again shares `(column, view)` with its
                // predecessor, whose extents stay listed until a fold.
                if let Some(text) = column.text_layers_mut() {
                    text.extend(text_extent_layers(
                        prefix_dir,
                        text_extents.iter().filter(|e| {
                            e.column == family.name
                                && e.view.as_deref() == Some(view_id.as_str())
                                && carries_live_view(
                                    view_incarnation,
                                    e.view.as_deref(),
                                    e.incarnation,
                                )
                        }),
                        &name,
                        mmap,
                    )?);
                }
                if scoped_is_filterable(family) {
                    placements.insert(name.clone(), placement);
                }

                columns.insert(name, column);
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
                    bases: prefix_dir.join(&e.bases),
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
            // A key created again shares `(column, view)` with its predecessor, whose extents
            // stay listed until a fold.
            if !carries_live_view(view_incarnation, extent.view.as_deref(), extent.incarnation) {
                continue;
            }
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
}
