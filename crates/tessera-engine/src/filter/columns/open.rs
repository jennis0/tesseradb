use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use tessera_filter::{ColumnPostings, RecordExtentPaths, RecordStack, SortedDict, ValueColumn};
use tessera_store::manifest::Visibility;

use super::{record_open_error, request_access, Column, FilterColumns, Layer, Route, TextLayer};
use crate::filter::declared::{resolve_analyser, visibility_of};
use crate::filter::error::ComposeError;
use crate::filter::{
    blob_resident, carries_live_view, owes_postings, owes_value_column, scoped_column_name,
    scoped_is_filterable, scoped_owes_postings, scoped_visibility_of, Family, Placement,
};

/// Open one view's column of a group-scoped attribute family: the name it is held under, its
/// placement, and its layers.
///
/// One column per view, opened under its resolved name, so a scoped leaf evaluates through
/// exactly the machinery an unscoped one of its family does. Called both by
/// [`FilterColumns::open`] at startup and, through [`FilterColumns::with_scoped_columns`], by a
/// flush writing the first column of a family for a view created since the build.
pub(super) fn open_scoped_column(
    partition_dir: &Path,
    family: &tessera_store::manifest::ScopedScalar,
    view_id: &str,
    incarnation: tessera_types::view::ViewIncarnation,
    vocabularies: &[tessera_store::manifest::ManifestVocabulary],
    mmap: bool,
) -> Result<(String, Placement, Column), ComposeError> {
    let scoped_family = Family::of_scoped(family);
    // A family with neither flag is opened and is not filterable: the drill-down still reads one
    // entity's value out of it, so it is held here with `filterable: false`, the same standing an
    // entity-scoped `derived` category with neither flag already has.
    let filterable = scoped_is_filterable(family);
    // `attrs/<column>/<group>/<key>/`. Above the declared incarnation the key carries its own
    // suffix: a recreated key opens its own base, leaving its predecessor's where the fold's
    // reclaim expects to find it.
    let mut dir = partition_dir.join("attrs").join(&family.name);
    for component in tessera_store::scoped_column_components(view_id, incarnation) {
        dir.push(component);
    }
    let name = scoped_column_name(&family.name, view_id);
    let placement = Placement {
        entity: true,
        // Never the row route: a leaf resolves to one entity-space column and a pin may make that
        // another view's, which no scan of these rows can answer.
        row: false,
        family: scoped_family,
    };
    // No position in `declared_scalars`, since it is not one of them and a scoped column is never
    // blob-resident.
    let declared_index = usize::MAX;
    if scoped_family == Family::Text {
        let analyser = resolve_analyser(&family.name, family.analyser.as_deref())?;
        let text = vec![TextLayer::open_base(&name, &dir, request_access(mmap))?];
        return Ok((
            name,
            placement,
            Column::text(declared_index, filterable, analyser, text),
        ));
    }
    // The same artefacts and routing the entity-scoped family takes: a `derived` vocabulary's
    // postings answer membership and must not answer the filter, whose work would then be a
    // function of the value named.
    let (base, postings, route) = open_value_base(
        &dir,
        scoped_family,
        scoped_owes_postings(family),
        scoped_visibility_of(family, vocabularies),
        mmap,
    )?;
    Ok((
        name,
        placement,
        Column::values(
            declared_index,
            filterable,
            scoped_family,
            Some(base),
            postings,
            route,
        ),
    ))
}

/// A column's base value layer, the derived postings where they are owed, and the route the
/// vocabulary's visibility fixes.
///
/// Everything here is opened on the declaration rather than probed for: a column the manifest
/// says is indexed and whose artefacts are absent is a bundle that is not what its manifest says,
/// and reading that as "no entity carries a value" would answer wrongly while looking right.
fn open_value_base(
    dir: &Path,
    family: Family,
    owes_postings: bool,
    visibility: Option<Visibility>,
    mmap: bool,
) -> Result<(Layer, Option<Arc<ColumnPostings>>, Route), ComposeError> {
    let values = Arc::new(ValueColumn::open_dir(dir, request_access(mmap))?);
    let dict = (family == Family::Keyword)
        .then(|| SortedDict::open_dir(dir, request_access(mmap)).map(Arc::new))
        .transpose()?;
    let postings = owes_postings
        .then(|| ColumnPostings::open_keyed(&dir.join("postings.arrow")).map(Arc::new))
        .transpose()?;
    let route = if postings.is_some() && visibility == Some(Visibility::Public) {
        Route::Postings
    } else {
        Route::Scan
    };
    Ok((
        Layer {
            values_rel: None,
            values,
            dict,
        },
        postings,
        route,
    ))
}

/// The layers a run of published text extents opens as, in manifest order, following a column's
/// base at [`FilterColumns::open`].
fn text_extent_layers<'a>(
    prefix_dir: &Path,
    extents: impl Iterator<Item = &'a tessera_store::manifest::TextExtent>,
    column: &str,
    mmap: bool,
) -> Result<Vec<TextLayer>, ComposeError> {
    extents
        .map(|extent| {
            TextLayer::open(
                column,
                &extent.dict,
                &prefix_dir.join(&extent.dict),
                &prefix_dir.join(&extent.postings),
                &prefix_dir.join(&extent.presence),
                request_access(mmap),
            )
        })
        .collect()
}

/// The layers an entity-scoped text column's published extents open as.
fn entity_text_layers(
    prefix_dir: &Path,
    texts: &[tessera_store::manifest::TextExtent],
    column: &str,
    mmap: bool,
) -> Result<Vec<TextLayer>, ComposeError> {
    text_extent_layers(
        prefix_dir,
        texts.iter().filter(|e| e.column == column && e.view.is_none()),
        column,
        mmap,
    )
}

/// The record blob's stack for one partition: the base the schema owes, plus every extent named.
///
/// The base is owed exactly when the compiled schema has a blob-resident column with no other
/// home ([`blob_resident`]); `unfolded` names the columns declared at a running service that have
/// extents alone until a fold writes the base. Derived from the schema rather than probed for on
/// disk, so a missing base is a refusal, never "those entities have no record".
pub(crate) fn open_record_stack(
    partition_dir: &Path,
    declared: &[tessera_store::manifest::DeclaredScalar],
    vocabularies: &[tessera_store::manifest::ManifestVocabulary],
    unfolded: &[String],
    extents: &[RecordExtentPaths],
    access: tessera_filter::Access,
) -> Result<RecordStack, tessera_filter::RecordError> {
    let owes_base = declared
        .iter()
        .any(|d| !unfolded.iter().any(|name| name == &d.name) && blob_resident(d, vocabularies));
    let record_dir = partition_dir.join("attrs").join("record");
    RecordStack::open(owes_base.then_some(record_dir.as_path()), extents, access)
}

/// The entity→term transpose's stack for one partition: the base and every extent named.
///
/// The base is unconditional, where the blob's is schema-dependent: every entity has a label set,
/// so a build always writes one. A bundle that lacks it refuses the open rather than reading as
/// "no entity carries a term", the fail-open direction on the write path.
pub(crate) fn open_entity_terms_stack(
    partition_dir: &Path,
    extents: &[tessera_store::EntityTermsExtentPaths],
) -> Result<tessera_store::EntityTermsStack, tessera_store::StoreError> {
    tessera_store::EntityTermsStack::open(
        Some(&partition_dir.join(tessera_store::ENTITY_TERMS_DIR)),
        extents,
    )
}

/// The stack a column declared at a running service opens with before any fold: no base, no
/// postings, the extents composed later. `None` for a column with no entity-space home.
pub(super) fn runtime_layers(
    scalar: &tessera_store::manifest::DeclaredScalar,
    declared_index: usize,
    vocabularies: &[tessera_store::manifest::ManifestVocabulary],
) -> Result<Option<Column>, ComposeError> {
    let family = Family::of(scalar);
    let filterable = Placement::of(scalar, vocabularies).is_some_and(|placement| placement.entity);
    if family == Family::Text {
        if !scalar.index {
            return Ok(None);
        }
        let analyser = resolve_analyser(&scalar.name, scalar.analyser.as_deref())?;
        return Ok(Some(Column::text(
            declared_index,
            filterable,
            analyser,
            Vec::new(),
        )));
    }
    if !owes_value_column(scalar, vocabularies) {
        return Ok(None);
    }
    Ok(Some(Column::values(
        declared_index,
        filterable,
        family,
        None,
        None,
        // Every operand is a scan until the fold rebuilds the postings from the folded column.
        Route::Scan,
    )))
}

/// Every extent list one partition's side-manifest names, as [`FilterColumns::open`] reads them:
/// the base each artefact class owes plus whatever has been published since.
#[derive(Default, Clone, Copy)]
pub struct PartitionExtents<'a> {
    pub attrs: &'a [tessera_store::manifest::AttrExtent],
    pub records: &'a [tessera_store::manifest::RecordExtent],
    /// An artifact's content extents, listed apart from a point's because their ownership differs
    /// rather than their bytes.
    pub artifact_records: &'a [tessera_store::manifest::RecordExtent],
    pub entity_terms: &'a [tessera_store::manifest::EntityTermsExtent],
    pub texts: &'a [tessera_store::manifest::TextExtent],
}

impl<'a> PartitionExtents<'a> {
    /// The lists one partition's side-manifest names. `None` is a bundle carrying no partition,
    /// which names none.
    pub fn of(manifest: Option<&'a tessera_store::manifest::SegmentsManifest>) -> Self {
        let Some(manifest) = manifest else {
            return PartitionExtents::default();
        };
        PartitionExtents {
            attrs: &manifest.attr_extents,
            records: &manifest.record_extents,
            artifact_records: &manifest.artifact_record_extents,
            entity_terms: &manifest.entity_terms_extents,
            texts: &manifest.text_extents,
        }
    }
}

impl FilterColumns {
    /// Open every filter column the manifest declares, with every extent the partition's
    /// side-manifest names.
    ///
    /// A declared column whose files are missing is an error, not an absence: the manifest
    /// digests them, so a missing one means the bundle is not what its manifest says it is. The
    /// same rule covers an extent that failed to open: skipping it would answer "those entities
    /// carry no value", indistinguishable from a correct answer.
    ///
    /// `mmap` decides whether a declared column costs resident memory before anyone filters on
    /// it: a value column is 1 GB per byte of declared width per 10⁹ entities, so reading every
    /// declared column into the heap at open would pay for filters a deployment may never issue.
    /// Mapped, the pages are faulted in by the scans that touch them. The engine passes `true`;
    /// tests pass `false`.
    ///
    /// It stays a `bool` where the reader beneath it takes a three-way [`tessera_filter::Access`]:
    /// the third case, `MappedSequential`, is the fold's hint and must never be applied to the
    /// request path's mappings, which a `bool` cannot express.
    pub fn open(
        prefix_dir: &Path,
        partition: &str,
        manifest: &tessera_store::manifest::Manifest,
        extents: PartitionExtents<'_>,
        // The entity-scoped columns declared at a running service that no fold has written a
        // base for. Each opens as an empty stack the extents compose onto; every other declared
        // column's base is demanded.
        unfolded: &[String],
        mmap: bool,
    ) -> Result<Self, ComposeError> {
        let declared = &manifest.declared_scalars;
        let vocabularies = &manifest.vocabularies;
        let scoped = manifest.scoped_scalars();
        let view_incarnation = &|view: &str| manifest.incarnation_of(view);
        let partition_dir = prefix_dir.join("partitions").join(partition);
        let mut columns = BTreeMap::new();
        let mut placements = BTreeMap::new();
        for (declared_index, scalar) in declared.iter().enumerate() {
            let family = Family::of(scalar);
            let placement = Placement::of(scalar, vocabularies);
            let entity = placement.is_some_and(|placement| placement.entity);
            if let Some(placement) = placement {
                placements.insert(scalar.name.clone(), placement);
            }
            // A column declared at a running service and not yet folded has no base: its stack
            // starts empty and its extents compose onto it, a text column's here and a value
            // column's with the attribute extents below.
            if unfolded.iter().any(|name| name == &scalar.name) {
                if let Some(mut column) = runtime_layers(scalar, declared_index, vocabularies)? {
                    if let Some(text) = column.text_layers_mut() {
                        text.extend(entity_text_layers(
                            prefix_dir,
                            extents.texts,
                            &scalar.name,
                            mmap,
                        )?);
                    }
                    columns.insert(scalar.name.clone(), column);
                }
                continue;
            }
            // Text opens before the value-column gate, because it owes none: its entity-space
            // artefacts are a token dictionary and postings over it, and the prose is a blob row.
            if family == Family::Text {
                if !scalar.index {
                    continue;
                }
                let dir = partition_dir.join("attrs").join(&scalar.name);
                let mut text_layers = vec![TextLayer::open_base(
                    &scalar.name,
                    &dir,
                    request_access(mmap),
                )?];
                text_layers.extend(entity_text_layers(
                    prefix_dir,
                    extents.texts,
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
            let (base, postings, route) = open_value_base(
                &dir,
                family,
                owes_postings(scalar, vocabularies),
                visibility_of(scalar, vocabularies),
                mmap,
            )?;
            columns.insert(
                scalar.name.clone(),
                Column::values(declared_index, entity, family, Some(base), postings, route),
            );
        }
        // ---- the group-scoped column families ------------------------------------------------
        //
        // Every family with a value column on disc is opened; only a filterable one takes a
        // placement, which is what gives a declaration with neither `index` nor `render` its
        // meaning: stored, served at `POST /v1/items`, on no filter surface and in no row tail.
        //
        // `text` is skipped when unfilterable, since it has no per-entity value slot for
        // drill-down to serve; an unindexed scoped `text` column is refused at the declaration.
        for family in &scoped {
            if !family.has_value_column() && !scoped_is_filterable(family) {
                continue;
            }
            for view_id in &family.views {
                // No incarnation, no column: opening under a guessed incarnation is how a dropped
                // view's values would reach a live one.
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
                if let Some(text) = column.text_layers_mut() {
                    text.extend(text_extent_layers(
                        prefix_dir,
                        extents.texts.iter().filter(|e| {
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
        // Artifact content extents hold the same format and reader as a point's, listed
        // separately because ownership differs, not bytes. Safe to open together because the two
        // never share an id: artifact ids descend from the ceiling, point ids ascend from zero.
        let extent_paths: Vec<RecordExtentPaths> = extents
            .records
            .iter()
            .chain(extents.artifact_records.iter())
            .map(|e| RecordExtentPaths {
                blocks: prefix_dir.join(&e.blocks),
                hasrow: prefix_dir.join(&e.hasrow),
                directory: prefix_dir.join(&e.directory),
            })
            .collect();
        let records = open_record_stack(
            &partition_dir,
            declared,
            vocabularies,
            unfolded,
            &extent_paths,
            request_access(mmap),
        )
        .map_err(record_open_error)?;
        let entity_terms = open_entity_terms_stack(
            &partition_dir,
            &extents
                .entity_terms
                .iter()
                .map(|e| tessera_store::EntityTermsExtentPaths {
                    hasrow: prefix_dir.join(&e.hasrow),
                    offsets: prefix_dir.join(&e.offsets),
                    terms: prefix_dir.join(&e.terms),
                    bases: prefix_dir.join(&e.bases),
                })
                .collect::<Vec<_>>(),
        )
        .map_err(|e| ComposeError::EntityTermsUnreadable(e.to_string()))?;
        let mut open = FilterColumns {
            columns,
            placements,
            access: request_access(mmap),
            records: Arc::new(records),
            entity_terms: Arc::new(entity_terms),
        };
        for extent in extents.attrs {
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
            let dict = extent
                .dict
                .as_ref()
                .map(|rel| {
                    SortedDict::open(&prefix_dir.join(rel), request_access(mmap)).map(Arc::new)
                })
                .transpose()?;
            open.push_opened(&crate::filter::OpenedExtent {
                extent: extent.clone(),
                values: Arc::new(column),
                dict,
            })?;
        }
        Ok(open)
    }
}
