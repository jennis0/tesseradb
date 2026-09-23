//! Each merged output is reopened, or its counts checked, before the manifest can name it, so a
//! merge defect fails the pass instead of being published. For an external-id run, the store
//! checks the rows it wrote against the rows the merge emitted.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tessera_authz::{coalesce_delta_tiers, coalesce_dict_extents, DeltaTier};
use tessera_store::coalesce_external_id_runs;
use tessera_store::manifest::{
    AttrExtent, DictExtent, EntityTermsExtent, FileDigest, LocatorExtent, RecordExtent,
    TextExtent,
};

use super::{coalesced_column_rel, CoalesceContext, ColumnWindow, OpenedTier};
use crate::flush::{
    digest_of, failed, remove_spool_on_error, MaintenanceFailed, SMALL_TERM_THRESHOLD,
};

fn digest_outputs<'r>(
    files: &mut BTreeMap<String, FileDigest>,
    ctx: &CoalesceContext,
    rels: impl IntoIterator<Item = &'r str>,
) -> Result<(), MaintenanceFailed> {
    for rel in rels {
        files.insert(rel.to_string(), digest_of(&ctx.prefix_dir.join(rel))?);
    }
    Ok(())
}

/// The delta tiers unioned into one fragment, with its reader reopened.
pub(super) fn coalesce_tiers(
    tiers: &[String],
    ctx: &CoalesceContext,
    files: &mut BTreeMap<String, FileDigest>,
) -> Result<OpenedTier, MaintenanceFailed> {
    let rel = format!("{}/delta.arrow", ctx.out_rel);
    let path = ctx.prefix_dir.join(&rel);
    let inputs: Vec<PathBuf> = tiers.iter().map(|p| ctx.prefix_dir.join(p)).collect();
    coalesce_delta_tiers(&inputs, &path, SMALL_TERM_THRESHOLD).map_err(failed("delta tiers"))?;
    digest_outputs(files, ctx, [rel.as_str()])?;
    let reader = DeltaTier::open(&path).map_err(failed("coalesced tier"))?;
    Ok((rel, Arc::new(reader)))
}

/// The external-id runs merged into one, with a locator extent over their union span. The spans
/// may overlap; each entity's slot comes from the newest run binding it.
pub(super) fn coalesce_runs(
    locators: &[LocatorExtent],
    ctx: &CoalesceContext,
    files: &mut BTreeMap<String, FileDigest>,
) -> Result<LocatorExtent, MaintenanceFailed> {
    let inputs: Vec<PathBuf> = locators
        .iter()
        .map(|e| ctx.prefix_dir.join(&e.external_id_run))
        .collect();
    let extent = LocatorExtent {
        path: format!("{}/ext-locator.u32", ctx.out_rel),
        entity_lo: locators.iter().map(|e| e.entity_lo).min().expect("a window has extents"),
        entity_hi: locators.iter().map(|e| e.entity_hi).max().expect("a window has extents"),
        external_id_run: format!("{}/external-ids.arrow", ctx.out_rel),
    };
    let out_dir = ctx.prefix_dir.join(&ctx.out_rel);
    coalesce_external_id_runs(&inputs, extent.entity_lo, extent.entity_hi, &out_dir)
        .map_err(failed("external-id runs"))?;
    digest_outputs(files, ctx, extent.files())?;
    Ok(extent)
}

pub(super) fn coalesce_dicts(
    dicts: &[DictExtent],
    ctx: &CoalesceContext,
    files: &mut BTreeMap<String, FileDigest>,
) -> Result<DictExtent, MaintenanceFailed> {
    let rel = format!("{}/terms-0.dict", ctx.out_rel);
    let inputs: Vec<PathBuf> = dicts.iter().map(|e| ctx.prefix_dir.join(&e.path)).collect();
    let records = coalesce_dict_extents(&inputs, &ctx.prefix_dir.join(&rel))
        .map_err(failed("dictionary extents"))?;
    // The counts differ only if an input repeated a descriptor, which would renumber every
    // ordinal after it.
    let declared: u64 = dicts.iter().map(|e| e.records).sum();
    if records != declared {
        return Err(MaintenanceFailed(format!(
            "the coalesced dictionary extent holds {records} records where its inputs declare \
             {declared}; an input repeated a descriptor, and coalescing it \
             would renumber every ordinal after the repeat"
        )));
    }
    digest_outputs(files, ctx, [rel.as_str()])?;
    Ok(DictExtent { path: rel, records })
}

pub(super) fn coalesce_attr_window(
    window: &ColumnWindow<AttrExtent>,
    ctx: &CoalesceContext,
    files: &mut BTreeMap<String, FileDigest>,
) -> Result<crate::filter::OpenedExtent, MaintenanceFailed> {
    // A keyword layer's values are ordinals into its own dictionary; a plain layer's are the
    // values. A window mixing them has no single reading.
    let with_dict = window.extents.iter().filter(|e| e.dict.is_some()).count();
    let keyword = match with_dict {
        0 => false,
        n if n == window.extents.len() => true,
        _ => {
            return Err(MaintenanceFailed(format!(
                "column '{}' has {with_dict} layers with their own dictionaries and {} \
                 without; a keyword layer's values are ordinals and another family's are \
                 values, so the window has no single reading and neither merge takes it",
                window.column,
                window.extents.len() - with_dict
            )));
        }
    };
    let column = &window.column;
    let column_rel = coalesced_column_rel(&ctx.out_rel, column, window.view.as_deref());
    std::fs::create_dir_all(ctx.prefix_dir.join(&column_rel))
        .map_err(failed(format!("coalesce dir for '{column}'")))?;
    let inputs: Vec<tessera_filter::ValueColumn> = window
        .extents
        .iter()
        .map(|extent| {
            tessera_filter::open_extent(
                &ctx.prefix_dir.join(&extent.values),
                &ctx.prefix_dir.join(&extent.presence),
                tessera_filter::Access::Mapped,
            )
        })
        .collect::<std::io::Result<_>>()
        .map_err(failed(format!("attr extent for '{column}'")))?;

    let extent = AttrExtent {
        column: column.clone(),
        view: window.view.clone(),
        incarnation: window.incarnation,
        values: format!("{column_rel}/{}", tessera_filter::VALUES_FILE),
        presence: format!("{column_rel}/{}", tessera_filter::PRESENCE_FILE),
        dict: keyword.then(|| format!("{column_rel}/{}", tessera_filter::DICT_FILE)),
        postings: None,
        offsets: None,
    };
    let values_path = ctx.prefix_dir.join(&extent.values);
    let presence_path = ctx.prefix_dir.join(&extent.presence);
    let dict_path = extent.dict.as_ref().map(|rel| ctx.prefix_dir.join(rel));
    match &dict_path {
        Some(dict_path) => {
            merge_keyword_window(window, &inputs, ctx, &values_path, &presence_path, dict_path)?
        }
        None => merge_values_window(column, &inputs, &values_path, &presence_path)?,
    }
    digest_outputs(files, ctx, extent.files())?;

    let values =
        tessera_filter::open_extent(&values_path, &presence_path, tessera_filter::Access::Mapped)
            .map_err(failed("coalesced attr extent"))?;
    let dict = dict_path
        .map(|path| {
            tessera_filter::SortedDict::open(&path, tessera_filter::Access::Mapped).map(Arc::new)
        })
        .transpose()
        .map_err(failed("the coalesced dictionary does not reopen"))?;
    Ok(crate::filter::OpenedExtent {
        extent,
        values: Arc::new(values),
        dict,
    })
}

fn merge_keyword_window(
    window: &ColumnWindow<AttrExtent>,
    inputs: &[tessera_filter::ValueColumn],
    ctx: &CoalesceContext,
    values_path: &Path,
    presence_path: &Path,
    dict_path: &Path,
) -> Result<(), MaintenanceFailed> {
    let column = &window.column;
    // Sequential: the merge walks each dictionary once in ordinal order.
    let dicts: Vec<tessera_filter::SortedDict> = window
        .extents
        .iter()
        .map(|extent| {
            let rel = extent
                .dict
                .as_deref()
                .expect("every extent of a keyword window names a dictionary");
            tessera_filter::SortedDict::open(
                &ctx.prefix_dir.join(rel),
                tessera_filter::Access::MappedSequential,
            )
        })
        .collect::<Result<_, _>>()
        .map_err(failed(format!("keyword dictionary for '{column}'")))?;
    let layers: Vec<tessera_filter_write::KeywordLayer<'_>> = inputs
        .iter()
        .zip(&dicts)
        .map(|(values, dict)| tessera_filter_write::KeywordLayer { values, dict })
        .collect();
    tessera_filter_write::coalesce_keyword_extents(&layers, values_path, presence_path, dict_path)
        .map_err(failed(format!("keyword coalesce for '{column}'")))
}

fn merge_values_window(
    column: &str,
    inputs: &[tessera_filter::ValueColumn],
    values_path: &Path,
    presence_path: &Path,
) -> Result<(), MaintenanceFailed> {
    let refs: Vec<&tessera_filter::ValueColumn> = inputs.iter().collect();
    tessera_filter_write::coalesce_attr_extents(&refs, values_path, presence_path)
        .map_err(failed(format!("attr coalesce for '{column}'")))
}

/// The record extents merged by entity and re-blocked toward the format's target block size.
pub(super) fn coalesce_records(
    records: &[RecordExtent],
    ctx: &CoalesceContext,
    files: &mut BTreeMap<String, FileDigest>,
) -> Result<RecordExtent, MaintenanceFailed> {
    let record_rel = format!("{}/attrs/record", ctx.out_rel);
    std::fs::create_dir_all(ctx.prefix_dir.join(&record_rel))
        .map_err(failed("coalesce dir for the record blob"))?;
    let open = |extent: &RecordExtent| {
        tessera_filter::RecordBlob::open(
            &ctx.prefix_dir.join(&extent.blocks),
            &ctx.prefix_dir.join(&extent.hasrow),
            &ctx.prefix_dir.join(&extent.directory),
            tessera_filter::Access::Mapped,
        )
    };
    let inputs: Vec<tessera_filter::RecordBlob> = records
        .iter()
        .map(open)
        .collect::<Result<_, _>>()
        .map_err(failed("record extent"))?;
    let refs: Vec<&tessera_filter::RecordBlob> = inputs.iter().collect();

    let extent = RecordExtent {
        blocks: format!("{record_rel}/{}", tessera_filter::RECORD_BLOCKS_FILE),
        hasrow: format!("{record_rel}/{}", tessera_filter::RECORD_HASROW_FILE),
        directory: format!("{record_rel}/{}", tessera_filter::RECORD_DIRECTORY_FILE),
    };
    tessera_filter_write::coalesce_record_extents(
        &refs,
        &ctx.prefix_dir.join(&extent.blocks),
        &ctx.prefix_dir.join(&extent.hasrow),
        &ctx.prefix_dir.join(&extent.directory),
        tessera_filter::RECORD_BLOCK_TARGET,
    )
    .map_err(failed("record coalesce"))?;
    digest_outputs(files, ctx, extent.files())?;
    open(&extent).map_err(failed("the coalesced record extent does not reopen"))?;
    Ok(extent)
}

/// Nothing per entity stores a text ordinal, so the merge changes only the extent's three files.
pub(super) fn coalesce_text_window(
    window: &ColumnWindow<TextExtent>,
    ctx: &CoalesceContext,
    files: &mut BTreeMap<String, FileDigest>,
) -> Result<TextExtent, MaintenanceFailed> {
    let column = &window.column;
    let column_rel = coalesced_column_rel(&ctx.out_rel, column, window.view.as_deref());
    let column_dir = ctx.prefix_dir.join(&column_rel);
    std::fs::create_dir_all(&column_dir).map_err(failed(format!("coalesce dir for '{column}'")))?;

    // Dictionaries stream once each. Postings are read from whichever layer holds the least key,
    // so they are not opened for sequential access.
    let dicts: Vec<tessera_filter::SortedDict> = window
        .extents
        .iter()
        .map(|extent| {
            tessera_filter::SortedDict::open(
                &ctx.prefix_dir.join(&extent.dict),
                tessera_filter::Access::MappedSequential,
            )
        })
        .collect::<Result<_, _>>()
        .map_err(failed(format!("text extent for '{column}'")))?;
    let postings: Vec<tessera_filter::ColumnPostings> = window
        .extents
        .iter()
        .map(|extent| {
            tessera_filter::ColumnPostings::open(&ctx.prefix_dir.join(&extent.postings), true)
        })
        .collect::<std::io::Result<_>>()
        .map_err(failed(format!("text extent for '{column}'")))?;
    let presences: Vec<croaring::Bitmap> = window
        .extents
        .iter()
        .map(|extent| {
            std::fs::read(ctx.prefix_dir.join(&extent.presence))
                .map(|bytes| croaring::Bitmap::deserialize::<croaring::Portable>(&bytes))
        })
        .collect::<std::io::Result<_>>()
        .map_err(failed(format!("text presence for '{column}'")))?;
    let inputs: Vec<tessera_filter_write::TextLayerRef<'_>> = dicts
        .iter()
        .zip(&postings)
        .zip(&presences)
        .map(
            |((dict, postings), present)| tessera_filter_write::TextLayerRef {
                dict,
                postings,
                present: Some(present),
            },
        )
        .collect();

    let extent = TextExtent {
        column: column.clone(),
        view: window.view.clone(),
        incarnation: window.incarnation,
        dict: format!("{column_rel}/{}", tessera_filter::DICT_FILE),
        postings: format!("{column_rel}/postings.arrow"),
        presence: format!("{column_rel}/presence.roaring"),
    };
    let dict_path = ctx.prefix_dir.join(&extent.dict);
    let postings_path = ctx.prefix_dir.join(&extent.postings);
    let spool_path = column_dir.join("postings.spool");
    remove_spool_on_error(
        tessera_filter_write::coalesce_text_extents(
            &inputs,
            &dict_path,
            &postings_path,
            &ctx.prefix_dir.join(&extent.presence),
            &spool_path,
        ),
        &spool_path,
    )
    .map_err(failed(format!("text coalesce for '{column}'")))?;
    digest_outputs(files, ctx, extent.files())?;

    // A dictionary and postings that disagree would give ordinals that name the wrong words.
    let no_reopen = "the coalesced text extent does not reopen";
    let reopened_dict = tessera_filter::SortedDict::open(&dict_path, tessera_filter::Access::Read)
        .map_err(failed(no_reopen))?;
    let reopened_postings =
        tessera_filter::ColumnPostings::open(&postings_path, false).map_err(failed(no_reopen))?;
    if reopened_dict.len() != reopened_postings.record_count() {
        return Err(MaintenanceFailed(format!(
            "the coalesced text extent for '{column}' holds {} terms and {} postings records",
            reopened_dict.len(),
            reopened_postings.record_count()
        )));
    }
    Ok(extent)
}

/// Lists are copied verbatim, because a term ordinal is a position in the append-only dictionary.
pub(super) fn coalesce_entity_terms(
    terms: &[EntityTermsExtent],
    ctx: &CoalesceContext,
    files: &mut BTreeMap<String, FileDigest>,
) -> Result<EntityTermsExtent, MaintenanceFailed> {
    let terms_rel = format!("{}/entities/terms", ctx.out_rel);
    std::fs::create_dir_all(ctx.prefix_dir.join(&terms_rel))
        .map_err(failed("coalesce dir for the transpose"))?;
    let open = |extent: &EntityTermsExtent| {
        tessera_store::EntityTerms::open(
            &ctx.prefix_dir.join(&extent.hasrow),
            &ctx.prefix_dir.join(&extent.offsets),
            &ctx.prefix_dir.join(&extent.terms),
            &ctx.prefix_dir.join(&extent.bases),
        )
    };
    let inputs: Vec<tessera_store::EntityTerms> = terms
        .iter()
        .map(open)
        .collect::<Result<_, _>>()
        .map_err(failed("entity-terms extent"))?;
    let expected: u64 = inputs.iter().map(tessera_store::EntityTerms::len).sum();
    let refs: Vec<&tessera_store::EntityTerms> = inputs.iter().collect();

    let extent = EntityTermsExtent {
        hasrow: format!("{terms_rel}/{}", tessera_store::ENTITY_TERMS_HASROW_FILE),
        offsets: format!("{terms_rel}/{}", tessera_store::ENTITY_TERMS_OFFSETS_FILE),
        terms: format!("{terms_rel}/{}", tessera_store::ENTITY_TERMS_TERMS_FILE),
        bases: format!("{terms_rel}/{}", tessera_store::ENTITY_TERMS_BASES_FILE),
    };
    let written = tessera_store::coalesce_entity_terms_extents(
        &refs,
        &ctx.prefix_dir.join(&extent.hasrow),
        &ctx.prefix_dir.join(&extent.offsets),
        &ctx.prefix_dir.join(&extent.terms),
        &ctx.prefix_dir.join(&extent.bases),
    )
    .map_err(failed("entity-terms coalesce"))?;
    // Fewer than the inputs hold means an input listed an entity with no offsets; publishing that
    // would lose those entities' label sets with no symptom.
    if written != expected {
        return Err(MaintenanceFailed(format!(
            "the coalesced entity-terms extent holds {written} entities where its inputs hold \
             {expected}"
        )));
    }
    digest_outputs(files, ctx, extent.files())?;
    open(&extent).map_err(failed("the coalesced entity-terms extent does not reopen"))?;
    Ok(extent)
}
