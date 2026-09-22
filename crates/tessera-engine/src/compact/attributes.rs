use std::path::Path;

use croaring::Bitmap;

use tessera_spatial::tiler::ScalarType;
use tessera_store::manifest::{AttrExtent, DeclaredScalar, ScopedScalar};
use tessera_types::view::ViewIncarnation;

use crate::flush::MaintenanceFailed;

use super::execute::{failed, remove_spool_on_error, FoldContext, FoldOutput};
use super::plan::FoldPlan;

/// An entity-scoped column, or one view of a group-scoped family.
struct ColumnJob {
    /// Prefix-relative directory, the same under both prefixes.
    rel: String,
    name: String,
    /// `None`, like `incarnation`, for an entity-scoped column.
    view: Option<String>,
    incarnation: Option<ViewIncarnation>,
    /// False for a column declared at a running service, which has extents only until this pass
    /// writes its base.
    has_base: bool,
    arrow_type: ScalarType,
    /// True for a category, whose postings are rebuilt from the folded column.
    postings: bool,
}

impl ColumnJob {
    fn holds(&self, column: &str, view: Option<&str>, incarnation: Option<ViewIncarnation>) -> bool {
        column == self.name && view == self.view.as_deref() && incarnation == self.incarnation
    }
}

/// The columns the predicates select, entity-scoped then group-scoped, in manifest order. A view
/// with no known incarnation is skipped.
fn column_jobs(
    plan: &FoldPlan,
    ctx: &FoldContext,
    entity: impl Fn(&DeclaredScalar) -> bool,
    scoped: impl Fn(&ScopedScalar) -> bool,
) -> Vec<ColumnJob> {
    let mut jobs: Vec<ColumnJob> = ctx
        .declared_scalars
        .iter()
        .filter(|d| entity(d))
        .map(|d| ColumnJob {
            rel: format!("partitions/{}/attrs/{}", plan.partition, d.name),
            name: d.name.clone(),
            view: None,
            incarnation: None,
            has_base: !ctx.runtime_attributes.contains(&d.name),
            arrow_type: d.arrow_type,
            postings: crate::filter::owes_postings(d, &ctx.vocabularies),
        })
        .collect();
    for family in ctx.scoped_scalars.iter().filter(|f| scoped(f)) {
        for view in &family.views {
            let Some(&incarnation) = ctx.view_incarnations.get(view) else {
                continue;
            };
            jobs.push(ColumnJob {
                rel: tessera_store::scoped_column_rel(
                    &plan.partition,
                    &family.name,
                    view,
                    incarnation,
                ),
                name: family.name.clone(),
                view: Some(view.clone()),
                incarnation: Some(incarnation),
                has_base: true,
                arrow_type: family.arrow_type,
                postings: crate::filter::scoped_owes_postings(family),
            });
        }
    }
    jobs
}

/// Pass 4a, text: each indexed text column's dictionary and postings merged across its layers,
/// less the tombstone set. A suppression changes no artefact. A word whose only carriers were
/// deleted leaves the dictionary. The folded base carries no presence bitmap.
pub(super) fn fold_text_columns(
    plan: &FoldPlan,
    ctx: &FoldContext,
    out: &mut FoldOutput,
) -> Result<(), MaintenanceFailed> {
    let jobs = column_jobs(
        plan,
        ctx,
        |d| d.arrow_type == ScalarType::Text && d.index,
        |f| f.arrow_type == ScalarType::Text && f.index,
    );
    for job in &jobs {
        let column_rel = job.rel.clone();
        let from_dir = ctx.from_prefix_dir.join(&column_rel);
        let to_dir = ctx.to_prefix_dir.join(&column_rel);
        std::fs::create_dir_all(&to_dir).map_err(|e| failed("pass 4a (text)", &e))?;

        // Dictionaries are read sequentially; postings are not, since the merge interleaves layers.
        let mut layers = Vec::new();
        if job.has_base {
            layers.push((
                tessera_filter::SortedDict::open_dir(
                    &from_dir,
                    tessera_filter::Access::MappedSequential,
                )
                .map_err(|e| failed("pass 4a (text: the base dictionary)", &e))?,
                tessera_filter::ColumnPostings::open(&from_dir.join("postings.arrow"), true)
                    .map_err(|e| failed("pass 4a (text: the base postings)", &e))?,
            ));
            out.read(&from_dir, [tessera_filter::DICT_FILE, "postings.arrow"]);
        }
        for extent in plan
            .text_extents
            .iter()
            .filter(|e| job.holds(&e.column, e.view.as_deref(), e.incarnation))
        {
            layers.push((
                tessera_filter::SortedDict::open(
                    &ctx.from_prefix_dir.join(&extent.dict),
                    tessera_filter::Access::MappedSequential,
                )
                .map_err(|e| failed("pass 4a (text: an extent's dictionary)", &e))?,
                tessera_filter::ColumnPostings::open(
                    &ctx.from_prefix_dir.join(&extent.postings),
                    true,
                )
                .map_err(|e| failed("pass 4a (text: an extent's postings)", &e))?,
            ));
            out.read(&ctx.from_prefix_dir, extent.files());
        }
        let dict_rel = format!("{column_rel}/{}", tessera_filter::DICT_FILE);
        let postings_rel = format!("{column_rel}/postings.arrow");
        let dict_path = ctx.to_prefix_dir.join(&dict_rel);
        let postings_path = ctx.to_prefix_dir.join(&postings_rel);
        let spool_path = to_dir.join("postings.spool");

        let inputs: Vec<tessera_filter_write::TextLayerRef<'_>> = layers
            .iter()
            .map(|(dict, postings)| tessera_filter_write::TextLayerRef {
                dict,
                postings,
                present: None,
            })
            .collect();
        remove_spool_on_error(
            tessera_filter_write::merge_text_layers(
                &inputs,
                &plan.tombstones,
                &dict_path,
                &postings_path,
                &spool_path,
            ),
            &spool_path,
        )
        .map_err(|e| failed(&format!("pass 4a (text: column '{}')", job.name), &e))?;

        out.wrote(dict_rel, dict_path);
        out.wrote(postings_rel, postings_path);
    }
    Ok(())
}

/// Pass 4a, value columns. A value column is positional, so this is where a deleted entity's
/// filter value leaves the corpus. A keyword's dictionary is rebuilt from the survivors and its
/// ordinals renumbered; other families' values are carried byte for byte.
pub(super) fn fold_value_columns(
    plan: &FoldPlan,
    ctx: &FoldContext,
    out: &mut FoldOutput,
) -> Result<(), MaintenanceFailed> {
    // The opener's predicates, so the fold writes exactly the columns the opener demands.
    let jobs = column_jobs(
        plan,
        ctx,
        |d| crate::filter::owes_value_column(d, &ctx.vocabularies),
        crate::filter::scoped_has_value_column,
    );
    for job in jobs {
        fold_value_column(plan, ctx, &job, out)?;
    }
    Ok(())
}

/// Merges one column's base and extents in entity order into a new base without the tombstoned
/// entities, and rebuilds a category's postings from the result.
fn fold_value_column(
    plan: &FoldPlan,
    ctx: &FoldContext,
    job: &ColumnJob,
    out: &mut FoldOutput,
) -> Result<(), MaintenanceFailed> {
    let extents: Vec<&AttrExtent> = plan
        .attr_extents
        .iter()
        .filter(|e| job.holds(&e.column, e.view.as_deref(), e.incarnation))
        .collect();
    let column_rel = job.rel.clone();
    let from_dir = ctx.from_prefix_dir.join(&column_rel);
    let to_dir = ctx.to_prefix_dir.join(&column_rel);
    std::fs::create_dir_all(&to_dir).map_err(|e| failed("pass 4a (attributes)", &e))?;

    let mut opened = Vec::new();
    if job.has_base {
        opened.push(
            tessera_filter::ValueColumn::open_dir(
                &from_dir,
                tessera_filter::Access::MappedSequential,
            )
            .map_err(|e| failed("pass 4a (attributes: the base column)", &e))?,
        );
        out.read(
            &from_dir,
            [tessera_filter::VALUES_FILE, tessera_filter::PRESENCE_FILE],
        );
    }
    for extent in &extents {
        opened.push(
            tessera_filter::open_extent(
                &ctx.from_prefix_dir.join(&extent.values),
                &ctx.from_prefix_dir.join(&extent.presence),
                tessera_filter::Access::MappedSequential,
            )
            .map_err(|e| failed("pass 4a (attributes: an extent)", &e))?,
        );
        out.read(&ctx.from_prefix_dir, [&extent.values, &extent.presence]);
    }
    let layers: Vec<&tessera_filter::ValueColumn> = opened.iter().collect();
    // Empty unless the column is a keyword; empty selects the generic fold below.
    let mut keyword_dicts: Vec<tessera_filter::SortedDict> = Vec::new();
    if job.arrow_type == tessera_spatial::tiler::ScalarType::Keyword {
        if job.has_base {
            keyword_dicts.push(
                tessera_filter::SortedDict::open_dir(
                    &from_dir,
                    tessera_filter::Access::MappedSequential,
                )
                .map_err(|e| failed("pass 4a (attributes: the base dictionary)", &e))?,
            );
        }
        for extent in &extents {
            let Some(dict_rel) = extent.dict.as_ref() else {
                return Err(MaintenanceFailed(format!(
                    "pass 4a (attributes): keyword column '{}' has an extent with no \
                     dictionary; its ordinals name nothing",
                    job.name
                )));
            };
            keyword_dicts.push(
                tessera_filter::SortedDict::open(
                    &ctx.from_prefix_dir.join(dict_rel),
                    tessera_filter::Access::MappedSequential,
                )
                .map_err(|e| failed("pass 4a (attributes: an extent dictionary)", &e))?,
            );
        }
    }

    let values_rel = format!("{column_rel}/{}", tessera_filter::VALUES_FILE);
    let presence_rel = format!("{column_rel}/{}", tessera_filter::PRESENCE_FILE);
    let dict_rel = format!("{column_rel}/{}", tessera_filter::DICT_FILE);
    let values_path = ctx.to_prefix_dir.join(&values_rel);
    let presence_path = ctx.to_prefix_dir.join(&presence_rel);
    let dict_path = ctx.to_prefix_dir.join(&dict_rel);
    // A column dense to this bound writes no presence bitmap.
    let bound = u32::try_from(plan.entity_bound).map_err(|_| {
        MaintenanceFailed("pass 4a (attributes): the entity bound exceeds u32".to_string())
    })?;
    let partial = if layers.is_empty() {
        // No layer holds the column yet.
        write_empty_value_column(
            &values_path,
            &presence_path,
            column_kind_of(job.arrow_type, job.postings),
        )
        .map_err(|e| failed("pass 4a (attributes: an empty base)", &e))?;
        if job.arrow_type == tessera_spatial::tiler::ScalarType::Keyword {
            write_empty_dictionary(&dict_path)
                .map_err(|e| failed("pass 4a (attributes: an empty dictionary)", &e))?;
            out.wrote(dict_rel, dict_path.clone());
        }
        true
    } else if keyword_dicts.is_empty() {
        tessera_filter_write::fold_value_column(
            &layers,
            &plan.tombstones,
            bound,
            &values_path,
            &presence_path,
        )
        .map_err(|e| failed("pass 4a (attributes: the merge)", &e))?
    } else {
        let keyword_layers: Vec<tessera_filter_write::KeywordLayer<'_>> = layers
            .iter()
            .zip(keyword_dicts.iter())
            .map(|(values, dict)| tessera_filter_write::KeywordLayer { values, dict })
            .collect();
        let partial = tessera_filter_write::fold_keyword_column(
            &keyword_layers,
            &plan.tombstones,
            bound,
            &values_path,
            &presence_path,
            &dict_path,
        )
        .map_err(|e| failed("pass 4a (attributes: the keyword merge)", &e))?;
        out.wrote(dict_rel, dict_path.clone());
        partial
    };
    out.wrote(values_rel, values_path.clone());
    if partial {
        out.wrote(presence_rel, presence_path.clone());
    }

    if !job.postings {
        return Ok(());
    }
    // No sequential hint: the banded emit scans the folded column once per band.
    let folded = tessera_filter::ValueColumn::open(
        &values_path,
        partial.then_some(presence_path.as_path()),
        tessera_filter::Access::Mapped,
    )
    .map_err(|e| failed("pass 4a (attributes: reopening the folded column)", &e))?;
    let postings_rel = format!("{column_rel}/postings.arrow");
    let postings_path = ctx.to_prefix_dir.join(&postings_rel);
    tessera_filter_write::write_category_postings(
        &postings_path,
        &job.name,
        &folded,
        tessera_filter_write::POSTINGS_BAND_ROWS,
    )
    .map_err(|e| failed("pass 4a (attributes: the postings rebuild)", &e))?;
    out.wrote(postings_rel, postings_path);

    Ok(())
}

/// Pass 4a, record blob: rewritten without the tombstoned entities' rows. A suppressed entity's
/// row streams through unchanged. Record extents with no blob-resident column declared are
/// refused, since folding them would drop their bytes.
pub(super) fn fold_record_blob(
    plan: &FoldPlan,
    ctx: &FoldContext,
    out: &mut FoldOutput,
) -> Result<(), MaintenanceFailed> {
    let blob_resident = ctx
        .declared_scalars
        .iter()
        .any(|d| crate::filter::blob_resident(d, &ctx.vocabularies));
    // A blob-resident column declared at a running service has no base yet.
    let based_blob_resident = ctx.declared_scalars.iter().any(|d| {
        !ctx.runtime_attributes.contains(&d.name)
            && crate::filter::blob_resident(d, &ctx.vocabularies)
    });
    if !blob_resident && !plan.record_extents.is_empty() {
        return Err(MaintenanceFailed(
            "pass 4a (record blob): the manifest names record extents but the schema declares no \
             blob-resident column; folding would drop their bytes silently, so it is refused"
                .to_string(),
        ));
    }
    if blob_resident {
        let record_rel = format!("partitions/{}/attrs/record", plan.partition);
        let from_dir = ctx.from_prefix_dir.join(&record_rel);
        let to_dir = ctx.to_prefix_dir.join(&record_rel);
        std::fs::create_dir_all(&to_dir).map_err(|e| failed("pass 4a (record blob)", &e))?;

        let mut opened = Vec::with_capacity(plan.record_extents.len() + 1);
        if based_blob_resident {
            opened.push(
                tessera_filter::RecordBlob::open_dir(
                    &from_dir,
                    tessera_filter::Access::MappedSequential,
                )
                .map_err(|e| failed("pass 4a (record blob: the base)", &e))?,
            );
            out.read(
                &from_dir,
                [
                    tessera_filter::RECORD_BLOCKS_FILE,
                    tessera_filter::RECORD_HASROW_FILE,
                    tessera_filter::RECORD_DIRECTORY_FILE,
                ],
            );
        }
        for extent in &plan.record_extents {
            opened.push(
                tessera_filter::RecordBlob::open(
                    &ctx.from_prefix_dir.join(&extent.blocks),
                    &ctx.from_prefix_dir.join(&extent.hasrow),
                    &ctx.from_prefix_dir.join(&extent.directory),
                    tessera_filter::Access::MappedSequential,
                )
                .map_err(|e| failed("pass 4a (record blob: an extent)", &e))?,
            );
            out.read(&ctx.from_prefix_dir, extent.files());
        }
        let layers: Vec<&tessera_filter::RecordBlob> = opened.iter().collect();

        let blocks_rel = format!("{record_rel}/{}", tessera_filter::RECORD_BLOCKS_FILE);
        let hasrow_rel = format!("{record_rel}/{}", tessera_filter::RECORD_HASROW_FILE);
        let directory_rel = format!("{record_rel}/{}", tessera_filter::RECORD_DIRECTORY_FILE);
        let blocks_path = ctx.to_prefix_dir.join(&blocks_rel);
        let hasrow_path = ctx.to_prefix_dir.join(&hasrow_rel);
        let directory_path = ctx.to_prefix_dir.join(&directory_rel);
        if layers.is_empty() {
            // An empty base, so the reopen finds a blob.
            tessera_filter_write::RecordBlobWriter::create(
                &blocks_path,
                &hasrow_path,
                &directory_path,
                tessera_filter::RECORD_BLOCK_TARGET,
            )
            .and_then(|writer| writer.finish())
            .map_err(|e| failed("pass 4a (record blob: an empty base)", &e))?;
        } else {
            tessera_filter_write::fold_record_blob(
                &layers,
                &plan.tombstones,
                &blocks_path,
                &hasrow_path,
                &directory_path,
                tessera_filter::RECORD_BLOCK_TARGET,
            )
            .map_err(|e| failed("pass 4a (record blob: the rewrite)", &e))?;
        }
        for (rel, path) in [
            (blocks_rel, blocks_path),
            (hasrow_rel, hasrow_path),
            (directory_rel, directory_path),
        ] {
            out.wrote(rel, path);
        }
    }

    Ok(())
}

/// Pass 4c: the entity-to-term transpose streamed in entity order into one base without the
/// tombstoned entities. No ordinal is remapped: a stored ordinal is a position in the dictionary
/// extents, which are carried forward unchanged.
pub(super) fn fold_entity_terms(
    plan: &FoldPlan,
    ctx: &FoldContext,
    out: &mut FoldOutput,
) -> Result<(), MaintenanceFailed> {
    let terms_rel = format!(
        "partitions/{}/{}",
        plan.partition,
        tessera_store::ENTITY_TERMS_DIR
    );
    let from_dir = ctx.from_prefix_dir.join(&terms_rel);
    let to_dir = ctx.to_prefix_dir.join(&terms_rel);
    std::fs::create_dir_all(&to_dir).map_err(|e| failed("pass 4c (entity terms)", &e))?;

    let mut extent_paths = Vec::with_capacity(plan.entity_terms_extents.len());
    for extent in &plan.entity_terms_extents {
        extent_paths.push(tessera_store::EntityTermsExtentPaths {
            hasrow: ctx.from_prefix_dir.join(&extent.hasrow),
            offsets: ctx.from_prefix_dir.join(&extent.offsets),
            terms: ctx.from_prefix_dir.join(&extent.terms),
            bases: ctx.from_prefix_dir.join(&extent.bases),
        });
        out.read(&ctx.from_prefix_dir, extent.files());
    }
    out.read(
        &from_dir,
        [
            tessera_store::ENTITY_TERMS_HASROW_FILE,
            tessera_store::ENTITY_TERMS_OFFSETS_FILE,
            tessera_store::ENTITY_TERMS_TERMS_FILE,
            tessera_store::ENTITY_TERMS_BASES_FILE,
        ],
    );
    let layers = tessera_store::EntityTermsStack::open(Some(&from_dir), &extent_paths)
        .map_err(|e| failed("pass 4c (entity terms: the layers)", &e))?;
    let mut writer = tessera_store::EntityTermsWriter::create(&to_dir)
        .map_err(|e| failed("pass 4c (entity terms: the rewrite)", &e))?;
    // Ascending, the order the writer requires.
    let live = layers.entity_set();
    for entity in live.iter() {
        if plan.tombstones.contains(entity) {
            continue;
        }
        let Some(terms) = layers
            .terms_of(entity)
            .map_err(|e| failed("pass 4c (entity terms: a layer)", &e))?
        else {
            continue;
        };
        writer
            .push(entity, &terms)
            .map_err(|e| failed("pass 4c (entity terms: the rewrite)", &e))?;
    }
    for path in writer
        .finish()
        .map_err(|e| failed("pass 4c (entity terms: the rewrite)", &e))?
    {
        let rel = format!(
            "{terms_rel}/{}",
            path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
        );
        out.wrote(rel, path);
    }

    Ok(())
}

/// The empty presence bitmap makes the reader take the column as partial, not dense.
fn write_empty_value_column(
    values_path: &Path,
    presence_path: &Path,
    kind: tessera_filter::ColumnKind,
) -> std::io::Result<()> {
    tessera_filter::ValueColumnWriter::create(values_path, presence_path, kind)?
        .finish(Some(&Bitmap::new()))
}

fn write_empty_dictionary(dict_path: &Path) -> std::io::Result<()> {
    let file = std::io::BufWriter::new(std::fs::File::create(dict_path)?);
    tessera_filter::SortedDictWriter::new(file)
        .and_then(|writer| writer.finish())
        .map(|_| ())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))
}

fn column_kind_of(arrow_type: ScalarType, category: bool) -> tessera_filter::ColumnKind {
    use tessera_filter::ColumnKind;
    if arrow_type == ScalarType::Keyword {
        return ColumnKind::U32;
    }
    if category {
        return match arrow_type {
            ScalarType::U8 => ColumnKind::U8,
            ScalarType::U16 => ColumnKind::U16,
            _ => ColumnKind::U32,
        };
    }
    match arrow_type {
        ScalarType::Bool | ScalarType::U8 => ColumnKind::U8,
        ScalarType::U16 => ColumnKind::U16,
        ScalarType::U32 => ColumnKind::U32,
        ScalarType::U64 => ColumnKind::U64,
        ScalarType::I8 => ColumnKind::I8,
        ScalarType::I16 => ColumnKind::I16,
        ScalarType::I32 => ColumnKind::I32,
        ScalarType::I64 | ScalarType::TimestampUs => ColumnKind::I64,
        ScalarType::F32 => ColumnKind::F32,
        ScalarType::F64 => ColumnKind::F64,
        // Unreachable: `utf8` is not declarable and `text` folds through its own pass.
        ScalarType::Utf8 | ScalarType::Keyword | ScalarType::Text => ColumnKind::U32,
    }
}
