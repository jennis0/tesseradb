use std::path::Path;

use croaring::Bitmap;

use tessera_spatial::tiler::ScalarType;
use tessera_store::manifest::{AttrExtent, DeclaredScalar, ScopedScalar};
use tessera_types::view::ViewIncarnation;

use crate::flush::MaintenanceFailed;

use super::execute::{failed, file_len, FoldContext, FoldOutput};
use super::plan::FoldPlan;

/// One column the attribute passes fold: where its files live, and what its declaration says. One
/// shape covers both an entity-scoped column and one view of a group-scoped family.
struct ColumnJob {
    /// Prefix-relative directory, the same under both prefixes.
    rel: String,
    name: String,
    /// The view this column belongs to, for a scoped family; `None` for an entity-scoped column.
    view: Option<String>,
    /// The view's live incarnation, for a scoped family; `None` for an entity-scoped column.
    incarnation: Option<ViewIncarnation>,
    /// False for an entity-scoped column declared at a running service, which has extents alone
    /// until this pass writes its base.
    has_base: bool,
    arrow_type: ScalarType,
    /// Does the folded column owe rebuilt keyed postings? True only for a category.
    postings: bool,
}

impl ColumnJob {
    /// Does an extent naming this column, view and incarnation belong to this job?
    fn holds(&self, column: &str, view: Option<&str>, incarnation: Option<ViewIncarnation>) -> bool {
        column == self.name && view == self.view.as_deref() && incarnation == self.incarnation
    }
}

/// The columns the two predicates select, entity-scoped then group-scoped, in manifest order. A
/// view whose incarnation this manifest cannot say is skipped.
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

/// Rebuild each indexed `text` column's index from the layers the snapshot named, minus the
/// tombstoned entities.
///
/// A text column has no value column: its index is a token dictionary plus postings, so its
/// postings are merged directly, a union per term and a subtraction, rather than re-derived from
/// a folded value column.
///
/// The blanked set is the tombstone set, the same set every other pass takes; a suppression
/// touches no artefact here. A term whose only carriers were deleted is dropped from the merged
/// dictionary, so the word itself leaves the corpus. Carrying the index forward untouched would
/// leave a deleted entity's terms in the postings after its overlay entry retired.
///
/// Streamed one term at a time: the layers' dictionaries are merged by a k-way scan, each
/// surviving term's posting is encoded and appended to a spool, and the dictionary is written
/// through [`tessera_filter::SortedDictWriter`] as the merge decides each key.
///
/// The folded layer carries no presence bitmap: after this pass, "carries a value" is answered
/// from the record blob instead.
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

        // The base build's layer first, then one per published extent. The dictionaries are
        // advised sequential; the postings are not, since the merge interleaves reads across layers.
        let mut dicts = Vec::new();
        let mut postings = Vec::new();
        if job.has_base {
            dicts.push(
                tessera_filter::SortedDict::open_dir(
                    &from_dir,
                    tessera_filter::Access::MappedSequential,
                )
                .map_err(|e| failed("pass 4a (text: the base dictionary)", &e))?,
            );
            postings.push(
                tessera_filter::ColumnPostings::open(&from_dir.join("postings.arrow"), true)
                    .map_err(|e| failed("pass 4a (text: the base postings)", &e))?,
            );
            out.attr_read += file_len(&from_dir.join(tessera_filter::DICT_FILE))
                + file_len(&from_dir.join("postings.arrow"));
        }
        for extent in plan
            .text_extents
            .iter()
            .filter(|e| job.holds(&e.column, e.view.as_deref(), e.incarnation))
        {
            dicts.push(
                tessera_filter::SortedDict::open(
                    &ctx.from_prefix_dir.join(&extent.dict),
                    tessera_filter::Access::MappedSequential,
                )
                .map_err(|e| failed("pass 4a (text: an extent's dictionary)", &e))?,
            );
            postings.push(
                tessera_filter::ColumnPostings::open(
                    &ctx.from_prefix_dir.join(&extent.postings),
                    true,
                )
                .map_err(|e| failed("pass 4a (text: an extent's postings)", &e))?,
            );
            for rel in extent.files() {
                out.attr_read += file_len(&ctx.from_prefix_dir.join(rel));
            }
        }
        let dict_rel = format!("{column_rel}/{}", tessera_filter::DICT_FILE);
        let postings_rel = format!("{column_rel}/postings.arrow");
        let dict_path = ctx.to_prefix_dir.join(&dict_rel);
        let postings_path = ctx.to_prefix_dir.join(&postings_rel);
        let spool_path = to_dir.join("postings.spool");

        // No presence is passed: this pass writes a base, and a base carries no presence bitmap.
        let inputs: Vec<tessera_filter_write::TextLayerRef<'_>> = dicts
            .iter()
            .zip(postings.iter())
            .map(|(dict, postings)| tessera_filter_write::TextLayerRef {
                dict,
                postings,
                present: None,
            })
            .collect();
        let outcome = tessera_filter_write::merge_text_layers(
            &inputs,
            &plan.tombstones,
            &dict_path,
            &postings_path,
            &spool_path,
        )
        .map_err(|e| failed(&format!("pass 4a (text: column '{}')", job.name), &e));
        // `finish` removes the spool on success; on failure it is this function's to remove.
        if outcome.is_err() {
            let _ = std::fs::remove_file(&spool_path);
        }
        outcome?;

        out.wrote(dict_rel, dict_path);
        out.wrote(postings_rel, postings_path);
    }
    Ok(())
}

/// Pass 4a: every declared filter column that owes a value column, in manifest order.
pub(super) fn fold_value_columns(
    plan: &FoldPlan,
    ctx: &FoldContext,
    out: &mut FoldOutput,
) -> Result<(), MaintenanceFailed> {
    // The opener's own predicates, so the columns written here are the columns it demands.
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

/// Fold one value column: its base and every snapshot extent merged in entity order into one new
/// base, the tombstoned entities blanked from presence with their value bytes never written, and
/// a category's postings rebuilt whole from the folded column. This pass is where a deleted
/// entity's filter value leaves the corpus, since the value column is positional. A suppression
/// touches no attribute artefact.
fn fold_value_column(
    plan: &FoldPlan,
    ctx: &FoldContext,
    job: &ColumnJob,
    out: &mut FoldOutput,
) -> Result<(), MaintenanceFailed> {
    let belongs = |e: &&AttrExtent| job.holds(&e.column, e.view.as_deref(), e.incarnation);
    let column_rel = job.rel.clone();
    let from_dir = ctx.from_prefix_dir.join(&column_rel);
    let to_dir = ctx.to_prefix_dir.join(&column_rel);
    std::fs::create_dir_all(&to_dir).map_err(|e| failed("pass 4a (attributes)", &e))?;

    // Advised sequential: the merge below streams each layer exactly once in entity order.
    let base = if job.has_base {
        let base = tessera_filter::ValueColumn::open_dir(
            &from_dir,
            tessera_filter::Access::MappedSequential,
        )
        .map_err(|e| failed("pass 4a (attributes: the base column)", &e))?;
        out.attr_read += file_len(&from_dir.join(tessera_filter::VALUES_FILE))
            + file_len(&from_dir.join(tessera_filter::PRESENCE_FILE));
        Some(base)
    } else {
        None
    };
    let mut extents = Vec::new();
    for extent in plan.attr_extents.iter().filter(belongs) {
        extents.push(
            tessera_filter::open_extent(
                &ctx.from_prefix_dir.join(&extent.values),
                &ctx.from_prefix_dir.join(&extent.presence),
                tessera_filter::Access::MappedSequential,
            )
            .map_err(|e| failed("pass 4a (attributes: an extent)", &e))?,
        );
        out.attr_read += file_len(&ctx.from_prefix_dir.join(&extent.values))
            + file_len(&ctx.from_prefix_dir.join(&extent.presence));
    }
    let layers: Vec<&tessera_filter::ValueColumn> = base.iter().chain(extents.iter()).collect();
    // A keyword layer's dictionary, opened beside its ordinals. Empty for every other family,
    // which selects the generic fold below.
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
        for extent in plan.attr_extents.iter().filter(belongs) {
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
    // The snapshot's entity space, which the folded column covers. A column dense to this bound
    // writes no presence bitmap; a deletion below the bound is what takes that away.
    let bound = u32::try_from(plan.entity_bound).map_err(|_| {
        MaintenanceFailed("pass 4a (attributes): the entity bound exceeds u32".to_string())
    })?;
    // A keyword folds through its own pass: its dictionary is rebuilt from the survivors and its
    // ordinals renumbered, unlike the generic fold which carries values byte-preserved.
    let partial = if layers.is_empty() {
        // Nothing has carried the column: an empty base with an empty presence.
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
    // Rebuilt from the folded column, read back rather than from the layers it was merged from.
    // Mapped without the sequential hint, since the banded emit scans this column once per band.
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

/// Pass 4a, continued: the record blob rewritten without the tombstoned entities' rows, so a
/// deleted entity's prose is absent from the folded artefact. A suppressed entity's row streams
/// through unchanged. The blob exists only where the schema declares a blob-resident column, so a
/// mismatch refuses rather than silently dropping extents' bytes.
pub(super) fn fold_record_blob(
    plan: &FoldPlan,
    ctx: &FoldContext,
    out: &mut FoldOutput,
) -> Result<(), MaintenanceFailed> {
    let blob_resident = ctx
        .declared_scalars
        .iter()
        .any(|d| crate::filter::blob_resident(d, &ctx.vocabularies));
    // A blob-resident column declared at a running service has extents alone until this pass writes the base.
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

        // The fold's own mappings, advised sequential: each layer streams once, block by block.
        let base = if based_blob_resident {
            let base = tessera_filter::RecordBlob::open_dir(
                &from_dir,
                tessera_filter::Access::MappedSequential,
            )
            .map_err(|e| failed("pass 4a (record blob: the base)", &e))?;
            for name in [
                tessera_filter::RECORD_BLOCKS_FILE,
                tessera_filter::RECORD_HASROW_FILE,
                tessera_filter::RECORD_DIRECTORY_FILE,
            ] {
                out.attr_read += file_len(&from_dir.join(name));
            }
            Some(base)
        } else {
            None
        };
        let mut extents = Vec::with_capacity(plan.record_extents.len());
        for extent in &plan.record_extents {
            extents.push(
                tessera_filter::RecordBlob::open(
                    &ctx.from_prefix_dir.join(&extent.blocks),
                    &ctx.from_prefix_dir.join(&extent.hasrow),
                    &ctx.from_prefix_dir.join(&extent.directory),
                    tessera_filter::Access::MappedSequential,
                )
                .map_err(|e| failed("pass 4a (record blob: an extent)", &e))?,
            );
            for rel in extent.files() {
                out.attr_read += file_len(&ctx.from_prefix_dir.join(rel));
            }
        }
        let layers: Vec<&tessera_filter::RecordBlob> = base.iter().chain(extents.iter()).collect();

        let blocks_rel = format!("{record_rel}/{}", tessera_filter::RECORD_BLOCKS_FILE);
        let hasrow_rel = format!("{record_rel}/{}", tessera_filter::RECORD_HASROW_FILE);
        let directory_rel = format!("{record_rel}/{}", tessera_filter::RECORD_DIRECTORY_FILE);
        let blocks_path = ctx.to_prefix_dir.join(&blocks_rel);
        let hasrow_path = ctx.to_prefix_dir.join(&hasrow_rel);
        let directory_path = ctx.to_prefix_dir.join(&directory_rel);
        if layers.is_empty() {
            // No flush has carried this column yet: an empty base, so the reopen finds the blob.
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

/// Pass 4c: the entity→term transpose, base plus every snapshot extent streamed in entity order
/// into one new base, with the tombstoned entities emitting nothing. A suppressed entity's list
/// streams through unchanged. No ordinal is remapped, since a stored ordinal is a position in the
/// concatenation of the dictionary extents, carried forward verbatim.
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
        for rel in extent.files() {
            out.attr_read += file_len(&ctx.from_prefix_dir.join(rel));
        }
    }
    for name in [
        tessera_store::ENTITY_TERMS_HASROW_FILE,
        tessera_store::ENTITY_TERMS_OFFSETS_FILE,
        tessera_store::ENTITY_TERMS_TERMS_FILE,
        tessera_store::ENTITY_TERMS_BASES_FILE,
    ] {
        out.attr_read += file_len(&from_dir.join(name));
    }
    let layers = tessera_store::EntityTermsStack::open(Some(&from_dir), &extent_paths)
        .map_err(|e| failed("pass 4c (entity terms: the layers)", &e))?;
    let mut writer = tessera_store::EntityTermsWriter::create(&to_dir)
        .map_err(|e| failed("pass 4c (entity terms: the rewrite)", &e))?;
    // One ascending pass over the union of the layers' has-row sets, the order the writer
    // requires and every layer already holds.
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

/// An entity-space value column with no entity in it. Written with an empty presence bitmap, so
/// the reader takes it as partial rather than as dense to the bound.
fn write_empty_value_column(
    values_path: &Path,
    presence_path: &Path,
    kind: tessera_filter::ColumnKind,
) -> std::io::Result<()> {
    tessera_filter::ValueColumnWriter::create(values_path, presence_path, kind)?
        .finish(Some(&Bitmap::new()))
}

/// A keyword column's dictionary with no key in it, beside [`write_empty_value_column`]'s ordinals.
fn write_empty_dictionary(dict_path: &Path) -> std::io::Result<()> {
    let file = std::io::BufWriter::new(std::fs::File::create(dict_path)?);
    tessera_filter::SortedDictWriter::new(file)
        .and_then(|writer| writer.finish())
        .map(|_| ())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))
}

/// The storage kind a value column of this declared type is written at. A keyword's values are
/// `u32` ordinals; a `bool` stores as a `u8`, a `timestamp_us` as the `i64` it is.
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
