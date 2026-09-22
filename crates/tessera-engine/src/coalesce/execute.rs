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
use crate::flush::{digest_of, MaintenanceFailed, SMALL_TERM_THRESHOLD};

/// The consumed delta tiers unioned into one fragment, with the reopened reader.
pub(super) fn coalesce_tiers(
    tiers: &[String],
    ctx: &CoalesceContext,
    out_dir: &Path,
    files: &mut BTreeMap<String, FileDigest>,
) -> Result<OpenedTier, MaintenanceFailed> {
    let rel = |name: &str| format!("{}/{name}", ctx.out_rel);
    let inputs: Vec<PathBuf> = tiers.iter().map(|p| ctx.prefix_dir.join(p)).collect();
    let path = out_dir.join("delta.arrow");
    coalesce_delta_tiers(&inputs, &path, SMALL_TERM_THRESHOLD)
        .map_err(|e| MaintenanceFailed(format!("delta tiers: {e}")))?;
    files.insert(rel("delta.arrow"), digest_of(&path)?);
    let reader =
        DeltaTier::open(&path).map_err(|e| MaintenanceFailed(format!("coalesced tier: {e}")))?;
    Ok((rel("delta.arrow"), Arc::new(reader)))
}

/// The consumed external-id runs merged into one run, with the locator extent indexing it.
pub(super) fn coalesce_runs(
    locators: &[LocatorExtent],
    ctx: &CoalesceContext,
    out_dir: &Path,
    files: &mut BTreeMap<String, FileDigest>,
) -> Result<LocatorExtent, MaintenanceFailed> {
    let rel = |name: &str| format!("{}/{name}", ctx.out_rel);
    let inputs: Vec<PathBuf> = locators
        .iter()
        .map(|e| ctx.prefix_dir.join(&e.external_id_run))
        .collect();
    // The union of the consumed extents' spans: the planner has already checked they are one
    // ascending, non-overlapping sequence, so this is a single span with the same coverage.
    let entity_lo = locators[0].entity_lo;
    let entity_hi = locators[locators.len() - 1].entity_hi;
    coalesce_external_id_runs(&inputs, entity_lo, entity_hi, out_dir)
        .map_err(|e| MaintenanceFailed(format!("external-id runs: {e}")))?;
    files.insert(
        rel("external-ids.arrow"),
        digest_of(&out_dir.join("external-ids.arrow"))?,
    );
    files.insert(
        rel("ext-locator.u32"),
        digest_of(&out_dir.join("ext-locator.u32"))?,
    );
    Ok(LocatorExtent {
        path: rel("ext-locator.u32"),
        entity_lo,
        entity_hi,
        external_id_run: rel("external-ids.arrow"),
    })
}

/// The consumed dictionary extents merged into one, its record count checked against theirs.
pub(super) fn coalesce_dicts(
    dicts: &[DictExtent],
    ctx: &CoalesceContext,
    out_dir: &Path,
    files: &mut BTreeMap<String, FileDigest>,
) -> Result<DictExtent, MaintenanceFailed> {
    let rel = |name: &str| format!("{}/{name}", ctx.out_rel);
    let inputs: Vec<PathBuf> = dicts
        .iter()
        .map(|e| ctx.prefix_dir.join(&e.path))
        .collect();
    let path = out_dir.join("terms-0.dict");
    let records = coalesce_dict_extents(&inputs, &path)
        .map_err(|e| MaintenanceFailed(format!("dictionary extents: {e}")))?;
    // The record count is checked against the inputs' declared counts, which differ only if
    // an input extent repeated a descriptor. That would renumber every ordinal after it, so
    // the pass fails here instead of publishing it.
    let declared: u64 = dicts.iter().map(|e| e.records).sum();
    if records != declared {
        return Err(MaintenanceFailed(format!(
            "the coalesced dictionary extent holds {records} records where its inputs declare \
             {declared}; an input repeated a descriptor, and coalescing it \
             would renumber every ordinal after the repeat"
        )));
    }
    files.insert(rel("terms-0.dict"), digest_of(&path)?);
    Ok(DictExtent {
        path: rel("terms-0.dict"),
        records,
    })
}

/// Attribute extents: one merged extent per window, under `<out>/attrs/<column>/`.
pub(super) fn coalesce_attr_window(
    window: &ColumnWindow<AttrExtent>,
    ctx: &CoalesceContext,
    files: &mut BTreeMap<String, FileDigest>,
) -> Result<crate::filter::OpenedExtent, MaintenanceFailed> {
    // Which merge runs is decided by whether the window's extents all name a dictionary or
    // all do not: a keyword layer's values are ordinals into its dictionary, and any other
    // family's values are the values themselves, so a window that mixes the two has no single
    // reading.
    let with_dict = window
        .extents
        .iter()
        .filter(|extent| extent.dict.is_some())
        .count();
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
    // Per `(column, view)`, not per column: two views of one scoped family share the
    // column's name, so a single directory would have the second window truncate the first's
    // mapped files.
    let column_rel = coalesced_column_rel(&ctx.out_rel, &window.column, window.view.as_deref());
    let column_dir = ctx.prefix_dir.join(&column_rel);
    std::fs::create_dir_all(&column_dir)
        .map_err(|e| MaintenanceFailed(format!("coalesce dir for '{}': {e}", window.column)))?;
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
        .map_err(|e| MaintenanceFailed(format!("attr extent for '{}': {e}", window.column)))?;

    let values_rel = format!("{column_rel}/{}", tessera_filter::VALUES_FILE);
    let presence_rel = format!("{column_rel}/{}", tessera_filter::PRESENCE_FILE);
    let values_path = ctx.prefix_dir.join(&values_rel);
    let presence_path = ctx.prefix_dir.join(&presence_rel);
    let mut dict_rel = None;
    if keyword {
        // Each input's dictionary beside its values, in the same order the manifest entry
        // states. Opened sequentially: the merge's cursors walk each file once in ordinal
        // order.
        let dicts: Vec<tessera_filter::SortedDict> = window
            .extents
            .iter()
            .map(|extent| {
                let rel = extent
                    .dict
                    .as_deref()
                    .expect("counted above: every extent of a keyword window names one");
                tessera_filter::SortedDict::open(
                    &ctx.prefix_dir.join(rel),
                    tessera_filter::Access::MappedSequential,
                )
            })
            .collect::<Result<_, _>>()
            .map_err(|e: tessera_filter::DictError| {
                MaintenanceFailed(format!("keyword dictionary for '{}': {e}", window.column))
            })?;
        let layers: Vec<tessera_filter_write::KeywordLayer<'_>> = inputs
            .iter()
            .zip(dicts.iter())
            .map(|(values, dict)| tessera_filter_write::KeywordLayer { values, dict })
            .collect();
        let rel = format!("{column_rel}/{}", tessera_filter::DICT_FILE);
        let dict_path = ctx.prefix_dir.join(&rel);
        // The merge checks its remap against the written dictionary before writing an
        // ordinal, so a wrong remap fails the pass here rather than publishing it.
        tessera_filter_write::coalesce_keyword_extents(
            &layers,
            &values_path,
            &presence_path,
            &dict_path,
        )
        .map_err(|e| MaintenanceFailed(format!("keyword coalesce for '{}': {e}", window.column)))?;
        files.insert(rel.clone(), digest_of(&dict_path)?);
        dict_rel = Some(rel);
    } else {
        let refs: Vec<&tessera_filter::ValueColumn> = inputs.iter().collect();
        tessera_filter_write::coalesce_attr_extents(&refs, &values_path, &presence_path)
            .map_err(|e| MaintenanceFailed(format!("attr coalesce for '{}': {e}", window.column)))?;
    }
    files.insert(values_rel.clone(), digest_of(&values_path)?);
    files.insert(presence_rel.clone(), digest_of(&presence_path)?);
    // Reopened here, on the pool, so publication on the executor is a pointer push. The
    // manifest entry below names the same paths this pair was read from.
    let values =
        tessera_filter::open_extent(&values_path, &presence_path, tessera_filter::Access::Mapped)
            .map_err(|e| MaintenanceFailed(format!("coalesced attr extent: {e}")))?;
    let dict = dict_rel
        .as_ref()
        .map(|rel| {
            tessera_filter::SortedDict::open(
                &ctx.prefix_dir.join(rel),
                tessera_filter::Access::Mapped,
            )
            .map(Arc::new)
        })
        .transpose()
        .map_err(|e| MaintenanceFailed(format!("the coalesced dictionary does not reopen: {e}")))?;
    Ok(crate::filter::OpenedExtent {
        extent: AttrExtent {
            column: window.column.clone(),
            view: window.view.clone(),
            incarnation: window.incarnation,
            values: values_rel,
            presence: presence_rel,
            dict: dict_rel,
            postings: None,
            offsets: None,
        },
        values: Arc::new(values),
        dict,
    })
}

/// Record-blob extents: the window merged by concatenation, re-blocking toward the format's
/// target block size. The merge retires nothing; there is no tombstone parameter to pass.
pub(super) fn coalesce_records(
    records: &[RecordExtent],
    ctx: &CoalesceContext,
    files: &mut BTreeMap<String, FileDigest>,
) -> Result<RecordExtent, MaintenanceFailed> {
    let record_rel = format!("{}/attrs/record", ctx.out_rel);
    let record_dir = ctx.prefix_dir.join(&record_rel);
    std::fs::create_dir_all(&record_dir)
        .map_err(|e| MaintenanceFailed(format!("coalesce dir for the record blob: {e}")))?;
    let inputs: Vec<tessera_filter::RecordBlob> = records
        .iter()
        .map(|extent| {
            tessera_filter::RecordBlob::open(
                &ctx.prefix_dir.join(&extent.blocks),
                &ctx.prefix_dir.join(&extent.hasrow),
                &ctx.prefix_dir.join(&extent.directory),
                tessera_filter::Access::Mapped,
            )
        })
        .collect::<Result<_, _>>()
        .map_err(|e| MaintenanceFailed(format!("record extent: {e}")))?;
    let refs: Vec<&tessera_filter::RecordBlob> = inputs.iter().collect();

    let extent = RecordExtent {
        blocks: format!("{record_rel}/{}", tessera_filter::RECORD_BLOCKS_FILE),
        hasrow: format!("{record_rel}/{}", tessera_filter::RECORD_HASROW_FILE),
        directory: format!("{record_rel}/{}", tessera_filter::RECORD_DIRECTORY_FILE),
    };
    let blocks_path = ctx.prefix_dir.join(&extent.blocks);
    let hasrow_path = ctx.prefix_dir.join(&extent.hasrow);
    let directory_path = ctx.prefix_dir.join(&extent.directory);
    tessera_filter_write::coalesce_record_extents(
        &refs,
        &blocks_path,
        &hasrow_path,
        &directory_path,
        tessera_filter::RECORD_BLOCK_TARGET,
    )
    .map_err(|e| MaintenanceFailed(format!("record coalesce: {e}")))?;
    for rel in extent.files() {
        files.insert(rel.to_string(), digest_of(&ctx.prefix_dir.join(rel))?);
    }
    // Reopened before the manifest can name it: a merge defect fails the pass here rather
    // than publishing an extent the reader would refuse later.
    tessera_filter::RecordBlob::open(
        &blocks_path,
        &hasrow_path,
        &directory_path,
        tessera_filter::Access::Mapped,
    )
    .map_err(|e| MaintenanceFailed(format!("the coalesced record extent does not reopen: {e}")))?;
    Ok(extent)
}

/// Text extents: the window merged into one layer, dictionary and all. Nothing per entity
/// stores a text ordinal, so nothing outside the three files needs remapping.
pub(super) fn coalesce_text_window(
    window: &ColumnWindow<TextExtent>,
    ctx: &CoalesceContext,
    files: &mut BTreeMap<String, FileDigest>,
) -> Result<TextExtent, MaintenanceFailed> {
    let column_rel = coalesced_column_rel(&ctx.out_rel, &window.column, window.view.as_deref());
    let column_dir = ctx.prefix_dir.join(&column_rel);
    std::fs::create_dir_all(&column_dir)
        .map_err(|e| MaintenanceFailed(format!("coalesce dir for '{}': {e}", window.column)))?;

    // The dictionaries are streamed sequentially, each once. The postings are not: the merge
    // reads record `at[i]` of whichever layer holds the least key, interleaving across
    // layers rather than walking each in order.
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
        .map_err(|e: tessera_filter::DictError| {
            MaintenanceFailed(format!("text extent for '{}': {e}", window.column))
        })?;
    let postings: Vec<tessera_filter::ColumnPostings> = window
        .extents
        .iter()
        .map(|extent| {
            tessera_filter::ColumnPostings::open(&ctx.prefix_dir.join(&extent.postings), true)
        })
        .collect::<std::io::Result<_>>()
        .map_err(|e| MaintenanceFailed(format!("text extent for '{}': {e}", window.column)))?;
    let presences: Vec<croaring::Bitmap> = window
        .extents
        .iter()
        .map(|extent| {
            std::fs::read(ctx.prefix_dir.join(&extent.presence))
                .map(|bytes| croaring::Bitmap::deserialize::<croaring::Portable>(&bytes))
        })
        .collect::<std::io::Result<_>>()
        .map_err(|e| MaintenanceFailed(format!("text presence for '{}': {e}", window.column)))?;
    let inputs: Vec<tessera_filter_write::TextLayerRef<'_>> = dicts
        .iter()
        .zip(postings.iter())
        .zip(presences.iter())
        .map(
            |((dict, postings), present)| tessera_filter_write::TextLayerRef {
                dict,
                postings,
                present: Some(present),
            },
        )
        .collect();

    let extent = TextExtent {
        column: window.column.clone(),
        view: window.view.clone(),
        incarnation: window.incarnation,
        dict: format!("{column_rel}/{}", tessera_filter::DICT_FILE),
        postings: format!("{column_rel}/postings.arrow"),
        presence: format!("{column_rel}/presence.roaring"),
    };
    let dict_path = ctx.prefix_dir.join(&extent.dict);
    let postings_path = ctx.prefix_dir.join(&extent.postings);
    let presence_path = ctx.prefix_dir.join(&extent.presence);
    // Scratch, removed on every exit path so a left-behind spool does not accumulate.
    let spool_path = column_dir.join("postings.spool");
    let outcome = tessera_filter_write::coalesce_text_extents(
        &inputs,
        &dict_path,
        &postings_path,
        &presence_path,
        &spool_path,
    );
    let _ = std::fs::remove_file(&spool_path);
    outcome.map_err(|e| MaintenanceFailed(format!("text coalesce for '{}': {e}", window.column)))?;
    drop(inputs);
    drop(postings);
    drop(dicts);

    for rel in extent.files() {
        files.insert(rel.to_string(), digest_of(&ctx.prefix_dir.join(rel))?);
    }
    // Reopened before the manifest can name it: the two halves are checked against each
    // other here, so a merge defect fails the pass rather than publishing a layer whose
    // ordinals name the wrong words.
    let reopened_dict = tessera_filter::SortedDict::open(&dict_path, tessera_filter::Access::Read)
        .map_err(|e| MaintenanceFailed(format!("the coalesced text extent does not reopen: {e}")))?;
    let reopened_postings = tessera_filter::ColumnPostings::open(&postings_path, false)
        .map_err(|e| MaintenanceFailed(format!("the coalesced text extent does not reopen: {e}")))?;
    if reopened_dict.len() != reopened_postings.record_count() {
        return Err(MaintenanceFailed(format!(
            "the coalesced text extent for '{}' holds {} terms and {} postings records",
            window.column,
            reopened_dict.len(),
            reopened_postings.record_count()
        )));
    }
    Ok(extent)
}

/// Entity-to-term extents: the window merged by concatenation. The merge walks the inputs'
/// entity sets in ascending order and copies each list verbatim; there is no remap, because a
/// term ordinal is a dictionary position and the dictionary is append-only. It retires
/// nothing: no tombstone parameter exists to pass.
pub(super) fn coalesce_entity_terms(
    terms: &[EntityTermsExtent],
    ctx: &CoalesceContext,
    files: &mut BTreeMap<String, FileDigest>,
) -> Result<EntityTermsExtent, MaintenanceFailed> {
    let terms_rel = format!("{}/entities/terms", ctx.out_rel);
    let terms_dir = ctx.prefix_dir.join(&terms_rel);
    std::fs::create_dir_all(&terms_dir)
        .map_err(|e| MaintenanceFailed(format!("coalesce dir for the transpose: {e}")))?;
    let inputs: Vec<tessera_store::EntityTerms> = terms
        .iter()
        .map(|extent| {
            tessera_store::EntityTerms::open(
                &ctx.prefix_dir.join(&extent.hasrow),
                &ctx.prefix_dir.join(&extent.offsets),
                &ctx.prefix_dir.join(&extent.terms),
                &ctx.prefix_dir.join(&extent.bases),
            )
        })
        .collect::<Result<_, _>>()
        .map_err(|e| MaintenanceFailed(format!("entity-terms extent: {e}")))?;
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
    .map_err(|e| MaintenanceFailed(format!("entity-terms coalesce: {e}")))?;
    // A count short of the sum means an input's has-row bitmap named an entity its offsets
    // did not: publishing that would lose a flush's worth of label sets with no symptom.
    if written != expected {
        return Err(MaintenanceFailed(format!(
            "the coalesced entity-terms extent holds {written} entities where its inputs hold \
             {expected}"
        )));
    }
    for rel in extent.files() {
        files.insert(rel.to_string(), digest_of(&ctx.prefix_dir.join(rel))?);
    }
    // Reopened before the manifest can name it, for the same reason as the other axes.
    drop(inputs);
    tessera_store::EntityTerms::open(
        &ctx.prefix_dir.join(&extent.hasrow),
        &ctx.prefix_dir.join(&extent.offsets),
        &ctx.prefix_dir.join(&extent.terms),
        &ctx.prefix_dir.join(&extent.bases),
    )
    .map_err(|e| {
        MaintenanceFailed(format!(
            "the coalesced entity-terms extent does not reopen: {e}"
        ))
    })?;
    Ok(extent)
}
