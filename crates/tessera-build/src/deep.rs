//! `tessera verify --deep` — the structural verifier's deep mode (correctness-suite §11, §12.4).
//!
//! [`verify_deep`] runs the whole of [`crate::verify`] — the read protocol, bijectivity over the
//! full row space, the identity column — and then one linear pass over the structures identity says
//! nothing about: postings, the dictionary extents and the oracle's `pairs.parquet`. It needs no
//! fixture, no generator and no oracle, which is what lets it run against a bundle whose data came
//! from somewhere real.
//!
//! ## What the open already refuses, and why this module does not re-check it
//!
//! Two of §11's checks are discharged by the open the shallow pass performs, fail-closed and
//! unconditionally: Morton codes not non-decreasing refuse at `MortonSlice::load`, and a column
//! shorter than its segment's declared `row_count` refuses at segment load (Arrow itself refuses
//! a batch whose columns disagree in length, and the loader compares the batch against the
//! manifest). Re-running those loops here would be code no input can reach — the same reasoning
//! §11 records for the rejected residual-in-cell check, which cannot fail because the residual is
//! the low half of the same 32-bit split whose high half is the cell code. What holds the claim
//! instead of a comment is the deliberate-damage pair in `tests/verify_deep.rs` (§18 obligation
//! 9): if the loader's refusals are ever relaxed, those tests fail, and the checks move here.
//!
//! ## The pairs check is scoped to the base, deliberately
//!
//! `terms/pairs.parquet` is written by the build and rewritten by the fold, and by nothing else —
//! a flush's delta tiers hold pairs it has never seen. The check is therefore exact equality with
//! the union of the **base** `terms/postings.arrow`, deltas excluded; an unscoped check against
//! base ∪ deltas would refuse every bundle that has absorbed a write.
//!
//! ## Cost model, and what this pass must not be pointed at
//!
//! One linear pass. Cadence is the suite's concern (§11: per-stage at the small tiers, per-run
//! above).
//!
//! ⊘ **At rest only, for now.** This resolves `CURRENT` through `open_bundle`, so a fold flipping
//! `CURRENT` mid-pass could swap the prefix under it. §12.4's answer — name the prefix once and
//! never re-read `CURRENT`, with a vanishing file reported as a race rather than a defect — needs
//! `tessera-store`'s named-prefix open made public, which has not happened. Until it has, point
//! this at a bundle no live engine is publishing into.

use std::collections::HashSet;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use arrow::array::{Array, UInt32Array, UInt64Array};
use parquet::arrow::arrow_reader::{ParquetRecordBatchReader, ParquetRecordBatchReaderBuilder};

use tessera_authz::{DeltaTier, PostingRef, PostingsReader};
use tessera_store::manifest::{Manifest, SegmentsManifest};

use tessera_types::{IdentityKey, TesseraId};

use crate::error::{BuildError, Result};
use crate::VerifyReport;

/// Options for [`verify_deep`].
#[derive(Debug, Clone)]
pub struct VerifyOpts {
    /// The points file this bundle claims to have been built from (correctness-suite §11.1).
    ///
    /// ⊘ **Specified, not implemented.** Checking it needs `source.digest` in `MANIFEST.json`,
    /// and that field belongs to contracts §2.2 — an amendment that has not been made. Passing
    /// `Some` is refused rather than silently ignored, so no harness can believe a binding was
    /// checked when nothing exists to check it against.
    pub source: Option<PathBuf>,
    /// The identity check's window threshold, in rows ([`crate::verify_with_window_rows`]).
    ///
    /// [`Default`] is [`crate::DIRECT_WINDOW_ROWS`], which is what an operator's `verify --deep`
    /// runs. A test lowers it to reach the partition route with a fixture it can afford to build.
    pub direct_window_rows: u64,
}

impl Default for VerifyOpts {
    fn default() -> VerifyOpts {
        VerifyOpts {
            source: None,
            direct_window_rows: crate::DIRECT_WINDOW_ROWS,
        }
    }
}

/// What [`verify_deep`] checked, beyond the shallow pass.
#[derive(Debug, Clone)]
pub struct VerifyDeepReport {
    pub shallow: VerifyReport,
    /// Base postings records walked (sorted, duplicate-free, bounded), across partitions.
    pub terms: u64,
    /// Delta postings tiers walked under the same rules.
    pub delta_tiers: usize,
    /// `pairs.parquet` rows matched one-to-one against the base postings' union; 0 where the
    /// bundle legitimately carries no pairs file (`--no-oracle-pairs`).
    pub pairs_rows: u64,
    /// Dictionary records parsed across the extent lists, none repeated (decision 0042).
    pub dict_records: u64,
    /// Record-blob rows walked across the base blob and every extent of every partition, each
    /// one's identity checked against the block that holds it and against the has-row bitmap's
    /// member at its rank ([`check_record_blobs`]); 0 where no column is blob-resident.
    pub record_rows: u64,
    /// `(segment, column)` pairs confirmed to hold a group-scoped **render** family's lane
    /// ([`check_scoped_render_lanes`]); 0 where no family declares `render`, which is every
    /// bundle whose attributes are entity-scoped.
    pub scoped_render_lanes: u64,
    /// Leaf Morton cells confirmed to begin where `cuts.u32` says and to hold ascending
    /// identities ([`check_cut_index`]), across every segment of every view.
    pub cells: u64,
    /// Band entries confirmed to be their rows, with their copies and the cell codes beside them
    /// ([`check_bands`]), across every segment of every view.
    pub band_entries: u64,
    /// Band label copies confirmed to hold their label column's label at every entry
    /// ([`check_band_labels`]).
    pub band_label_copies: u64,
    /// Row-major columns whose member file was confirmed to be what their labels give
    /// ([`check_row_members`]).
    pub row_member_files: u64,
    /// Unique index entries confirmed to agree with their column's values in both directions,
    /// at most one live entity to a key ([`check_unique_indexes`]); 0 where no column is unique.
    pub unique_entries: u64,
    /// Edited-item pairs confirmed to be held by both directions of the map
    /// ([`check_edited_items`]); 0 where no item has been edited.
    pub edited_pairs: u64,
    /// Segment rows whose entity is not their number, each confirmed to be a pair of the map.
    pub edited_rows: u64,
}

/// Deep-verify the bundle at `root`: the shallow [`crate::verify`] pass, then §11's structural
/// checks over postings, dictionary extents and `pairs.parquet`. See the module doc for what each
/// check accepts on purpose.
pub fn verify_deep(root: &Path, opts: &VerifyOpts) -> Result<VerifyDeepReport> {
    if opts.source.is_some() {
        return Err(BuildError::Invalid(
            "verifying against a source file is specified (correctness-suite §11.1) but not \
             implemented: it needs `source.digest` in MANIFEST.json, a contracts §2.2 amendment \
             that has not been made. Refused rather than ignored, so nothing can believe a \
             binding was checked"
                .into(),
        ));
    }

    let (bundle, shallow) = crate::verified_open(root, opts.direct_window_rows)?;
    let prefix_dir = root.join(&shallow.prefix);

    let mut report = VerifyDeepReport {
        shallow,
        terms: 0,
        delta_tiers: 0,
        pairs_rows: 0,
        dict_records: 0,
        record_rows: 0,
        scoped_render_lanes: 0,
        cells: 0,
        band_entries: 0,
        band_label_copies: 0,
        row_member_files: 0,
        unique_entries: 0,
        edited_pairs: 0,
        edited_rows: 0,
    };

    // Sorted so two runs over the same defective bundle refuse with the same message.
    let mut partitions: Vec<_> = bundle.partitions.iter().collect();
    partitions.sort_by(|a, b| a.0.cmp(b.0));
    for (phash, partition) in partitions {
        check_postings_and_pairs(
            &prefix_dir,
            phash,
            &bundle.manifest,
            &partition.manifest,
            &mut report,
        )?;
        check_dict_extents(&prefix_dir, &partition.manifest, &mut report)?;
        check_record_blobs(
            &prefix_dir,
            phash,
            &bundle.manifest,
            &partition.manifest,
            &mut report,
        )?;
        check_scoped_render_lanes(&bundle.manifest, phash, partition, &mut report)?;
        check_cut_index(phash, partition, &mut report)?;
        check_bands(&prefix_dir, phash, partition, &mut report)?;
        check_band_labels(&prefix_dir, partition, &mut report)?;
        check_row_members(&prefix_dir, partition, &mut report)?;
        check_unique_indexes(
            root,
            &prefix_dir,
            phash,
            &bundle.manifest,
            partition,
            &mut report,
        )?;
        check_edited_items(&prefix_dir, phash, &bundle.manifest, partition, &mut report)?;
    }

    Ok(report)
}

/// **The edited-items map agrees with itself and with the rows.** Its two directions hold the same
/// pairs, an entity holds one number, a number has at most one live entity, and every row an edit
/// moved is a pair of the map, its number being what its `tessera_id` inverts to.
fn check_edited_items(
    prefix_dir: &Path,
    phash: &str,
    manifest: &tessera_store::manifest::Manifest,
    partition: &tessera_store::read::PartitionData,
    report: &mut VerifyDeepReport,
) -> Result<()> {
    use std::collections::{BTreeMap, BTreeSet};
    let runs = &partition.manifest.edited_items;
    let unreadable = |e: tessera_store::StoreError| {
        BuildError::Invalid(format!("partition {phash}: the edited items' runs: {e}"))
    };
    let forward: BTreeSet<(u32, u32)> = tessera_store::edited::entries(&runs.by_number, prefix_dir)
        .map_err(unreadable)?
        .into_iter()
        .collect();
    let mut number_of: BTreeMap<u32, u32> = BTreeMap::new();
    for (entity, number) in
        tessera_store::edited::entries(&runs.by_entity, prefix_dir).map_err(unreadable)?
    {
        if let Some(held) = number_of.insert(entity, number) {
            if held != number {
                return Err(BuildError::Invalid(format!(
                    "partition {phash}: the edited items give entity {entity} two numbers, \
                     {held} and {number}"
                )));
            }
        }
    }
    let backward: BTreeSet<(u32, u32)> = number_of.iter().map(|(e, n)| (*n, *e)).collect();
    if let Some((number, entity)) = forward.symmetric_difference(&backward).next() {
        let (held, missing) = match forward.contains(&(*number, *entity)) {
            true => ("by number", "by entity"),
            false => ("by entity", "by number"),
        };
        return Err(BuildError::Invalid(format!(
            "partition {phash}: the edited items hold number {number} and entity {entity} {held} \
             and not {missing}"
        )));
    }
    report.edited_pairs += forward.len() as u64;

    // A number's live entity is one no deletion names that has a row: an edit deletes the entity
    // it moves an item away from.
    let deleted = partition
        .manifest
        .tombstones
        .entities()
        .cloned()
        .unwrap_or_default();
    let live = |entity: u32| {
        !deleted.contains(entity)
            && partition.views.values().any(|data| {
                data.row_space
                    .row_of(tessera_types::EntityId::new(u64::from(entity)))
                    .is_some()
            })
    };
    let mut entities_of: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for &(number, entity) in &forward {
        entities_of.entry(number).or_default().push(entity);
    }
    for (number, entities) in &entities_of {
        let alive: Vec<u32> = std::iter::once(*number)
            .chain(entities.iter().copied())
            .filter(|entity| live(*entity))
            .collect();
        if alive.len() > 1 {
            return Err(BuildError::Invalid(format!(
                "partition {phash}: number {number} has {} live entities, {alive:?}; an item is \
                 held by one",
                alive.len()
            )));
        }
    }

    let key = IdentityKey::from_hex(&manifest.identity.key)
        .map_err(|e| BuildError::Invalid(format!("the manifest's identity key: {e}")))?;
    let mut views: Vec<_> = partition.views.iter().collect();
    views.sort_by(|a, b| a.0.cmp(b.0));
    for (view, data) in views {
        for segment in &data.segments {
            let ids = segment.columns.tessera_id();
            for (row, entity) in segment.entities.moved(ids, &key) {
                let tessera_id = ids.get(row as usize).copied().ok_or_else(|| {
                    BuildError::Invalid(format!(
                        "view {view}, segment {}: moved row {row} is past its {} rows",
                        segment.seg_id,
                        ids.len()
                    ))
                })?;
                let (_, number) = key.invert(TesseraId::new(tessera_id));
                if !u32::try_from(number.raw()).is_ok_and(|n| forward.contains(&(n, entity))) {
                    return Err(BuildError::Invalid(format!(
                        "view {view}, segment {}: row {row} holds entity {entity}, which the \
                         edited items do not give number {}",
                        segment.seg_id,
                        number.raw()
                    )));
                }
                report.edited_rows += 1;
            }
        }
    }
    Ok(())
}

/// **Every unique index agrees with its column, and no key names two live entities.**
///
/// Each run is digested against the manifest, since the open-time sweep defers key runs to their
/// pages' own checksums, and checked page by page ([`tessera_store::key_index::verify_run`]).
/// The runs are then merged, the manifest's tombstoned entities dropped, into one sorted stream,
/// and the column's values for every entity not tombstoned are sorted into another through the
/// same spill; the two must be equal entry for entry, and no key may carry two entities. The
/// values are read from the column's own home: its value column, the record blob, or, for a
/// column rendered alone, each view's row tail.
fn check_unique_indexes(
    root: &Path,
    prefix_dir: &Path,
    phash: &str,
    manifest: &Manifest,
    partition: &tessera_store::read::PartitionData,
    report: &mut VerifyDeepReport,
) -> Result<()> {
    use tessera_store::unique::{key_of, KeyKind, UniqueSpill};
    let partition_manifest = &partition.manifest;
    if partition_manifest.unique_indexes.is_empty() {
        return Ok(());
    }
    let store = |e: tessera_store::StoreError| BuildError::Invalid(e.to_string());
    // The schema as served: the build's columns and those declared at a running service.
    let served = manifest.with_attributes(&partition_manifest.attributes, &[]);
    let tombstones = partition_manifest
        .tombstones
        .entities()
        .cloned()
        .ok_or_else(|| {
            BuildError::Invalid(format!("partition {phash}: the tombstones do not decode"))
        })?;
    let bound = u32::try_from(partition_manifest.entity_id_high_water.max(manifest.entity_id_high_water))
        .unwrap_or(u32::MAX);
    for index in &partition_manifest.unique_indexes {
        let attribute = &index.attribute;
        let Some((at, declared)) = served
            .declared_scalars
            .iter()
            .enumerate()
            .find(|(_, d)| &d.name == attribute)
        else {
            return Err(BuildError::Invalid(format!(
                "partition {phash}: a unique index names attribute '{attribute}', which the \
                 manifests do not declare"
            )));
        };
        let kind = KeyKind::of(declared.arrow_type).ok_or_else(|| {
            BuildError::Invalid(format!(
                "partition {phash}: attribute '{attribute}' has a unique index and a type that \
                 cannot be unique"
            ))
        })?;
        let mut inputs: Vec<PathBuf> = Vec::new();
        for rel in index.files() {
            let digest = partition_manifest
                .files
                .get(rel)
                .or_else(|| manifest.files.get(rel))
                .ok_or_else(|| {
                    BuildError::Invalid(format!("{rel}: a unique index run no files map digests"))
                })?;
            let path = join_rel(prefix_dir, rel)?;
            let actual = crate::digest_file(&path)?;
            if actual.size != digest.size || actual.sha256 != digest.sha256 {
                return Err(BuildError::Invalid(format!(
                    "{rel}: bytes do not match the manifest digest"
                )));
            }
            let checked = tessera_store::key_index::verify_run(&path)
                .map_err(|e| BuildError::Invalid(format!("{rel}: {e}")))?;
            let width = if kind == KeyKind::Keyword { 16 } else { 8 };
            if checked.key_width != width {
                return Err(BuildError::Invalid(format!(
                    "{rel}: holds {}-byte keys and '{attribute}' takes {width}-byte keys",
                    checked.key_width
                )));
            }
            inputs.push(path);
        }
        let scratch = crate::VerifyTmp::create(root)?;
        let homes_value = declared.index;
        let blob = !declared.render && !declared.index;
        // The spill a build under the machine's own budget would sort the column in.
        let budget = crate::pipeline::detect_memory_budget();
        let spill_bytes = crate::unique_index::spill_budget(budget, budget);
        let mut spill = UniqueSpill::create(kind, scratch.path(), spill_bytes).map_err(store)?;
        let mut push = |entity: u32, value: &tessera_spatial::ScalarValue| -> Result<()> {
            if entity >= bound || tombstones.contains(entity) {
                return Ok(());
            }
            if let Some(key) = key_of(declared.arrow_type, value) {
                spill.push(key, entity).map_err(store)?;
            }
            Ok(())
        };
        if homes_value {
            let dir = prefix_dir.join("partitions").join(phash).join("attrs").join(attribute);
            let access = tessera_filter::Access::MappedSequential;
            let mut layers: Vec<(tessera_filter::ValueColumn, Option<tessera_filter::SortedDict>)> =
                Vec::new();
            let base_rel = format!("partitions/{phash}/attrs/{attribute}/{}", tessera_filter::VALUES_FILE);
            if manifest.files.contains_key(&base_rel) {
                let values = tessera_filter::ValueColumn::open_dir(&dir, access)
                    .map_err(|e| BuildError::io(&dir, e))?;
                let dict = (declared.arrow_type == tessera_spatial::ScalarType::Keyword)
                    .then(|| tessera_filter::SortedDict::open_dir(&dir, access))
                    .transpose()
                    .map_err(|e| BuildError::Invalid(format!("{attribute}: {e}")))?;
                layers.push((values, dict));
            }
            for extent in partition_manifest
                .attr_extents
                .iter()
                .filter(|e| &e.column == attribute && e.view.is_none())
            {
                let values = tessera_filter::open_extent(
                    &join_rel(prefix_dir, &extent.values)?,
                    &join_rel(prefix_dir, &extent.presence)?,
                    access,
                )
                .map_err(|e| BuildError::Invalid(format!("{}: {e}", extent.values)))?;
                let dict = extent
                    .dict
                    .as_ref()
                    .map(|rel| {
                        join_rel(prefix_dir, rel).and_then(|path| {
                            tessera_filter::SortedDict::open(&path, access)
                                .map_err(|e| BuildError::Invalid(format!("{rel}: {e}")))
                        })
                    })
                    .transpose()?;
                layers.push((values, dict));
            }
            let mut scratch_bytes = Vec::new();
            let mut keyed = |dict: &tessera_filter::SortedDict, ordinal: u32| {
                dict.key_of(ordinal, &mut scratch_bytes)
                    .map(|key| tessera_spatial::ScalarValue::Utf8(key.to_string()))
                    .map_err(|e| BuildError::Invalid(format!("{attribute}: {e}")))
            };
            for (values, dict) in &layers {
                let present = values.present();
                values.for_each_record_value_in(&present, |entity, value| {
                    use tessera_filter::RecordValue as RV;
                    let value = match (dict, value) {
                        (None, value) => record_as_scalar(value),
                        (Some(dict), RV::U8(o)) => keyed(dict, u32::from(o))?,
                        (Some(dict), RV::U16(o)) => keyed(dict, u32::from(o))?,
                        (Some(dict), RV::U32(o)) => keyed(dict, o)?,
                        (Some(_), other) => {
                            return Err(BuildError::Invalid(format!(
                                "{attribute}: a keyword column holds {other:?} where it stores \
                                 ordinals"
                            )))
                        }
                    };
                    push(entity, &value)
                })?;
            }
        } else if !blob {
            // Rendered alone: every view's rows carry the value, so each entity is read from the
            // first view that holds it.
            let mut wanted = croaring::Bitmap::new();
            wanted.add_range(0..bound);
            let mut views: Vec<_> = partition.views.iter().collect();
            views.sort_by(|a, b| a.0.cmp(b.0));
            let mut failed: Option<BuildError> = None;
            for (view, data) in views {
                let mut read = croaring::Bitmap::new();
                data.for_each_rendered(attribute, &wanted, &mut |entity, value| {
                    read.add(entity);
                    if failed.is_none() {
                        if let Err(e) = push(entity, &value) {
                            failed = Some(e);
                        }
                    }
                })
                .map_err(|e| BuildError::Invalid(format!("view '{view}': {e}")))?;
                wanted.andnot_inplace(&read);
            }
            if let Some(e) = failed {
                return Err(e);
            }
        } else {
            let record_dir = prefix_dir.join("partitions").join(phash).join("attrs").join("record");
            let base_rel = format!("partitions/{phash}/attrs/record/{}", tessera_filter::RECORD_BLOCKS_FILE);
            let base = manifest.files.contains_key(&base_rel).then_some(record_dir.as_path());
            let extents: Vec<tessera_filter::RecordExtentPaths> = partition_manifest
                .record_extents
                .iter()
                .map(|e| {
                    Ok(tessera_filter::RecordExtentPaths {
                        blocks: join_rel(prefix_dir, &e.blocks)?,
                        hasrow: join_rel(prefix_dir, &e.hasrow)?,
                        directory: join_rel(prefix_dir, &e.directory)?,
                    })
                })
                .collect::<Result<_>>()?;
            let stack = tessera_filter::RecordStack::open(
                base,
                &extents,
                tessera_filter::Access::MappedSequential,
            )
            .map_err(|e| BuildError::Invalid(format!("the record blob: {e}")))?;
            let mut wanted = croaring::Bitmap::new();
            wanted.add_range(0..bound);
            let mut failed: Option<BuildError> = None;
            let walked = stack.for_each_row_in(&wanted, &mut |entity, fields| {
                if let Some(field) = fields.into_iter().find(|f| f.tag as usize == at) {
                    if let Err(e) = push(entity, &record_as_scalar(field.value)) {
                        failed = Some(e);
                        // Ends the walk; the error returned is `failed`.
                        return Err(tessera_filter::RecordError::Malformed(String::new()));
                    }
                }
                Ok(())
            });
            if let Some(e) = failed {
                return Err(e);
            }
            walked.map_err(|e| BuildError::Invalid(format!("the record blob: {e}")))?;
        }
        let mut twice = 0u64;
        let column_dir = scratch.path().join("column");
        fs::create_dir_all(&column_dir).map_err(|e| BuildError::io(&column_dir, e))?;
        let column_side: Vec<PathBuf> = spill
            .finish(&column_dir, "column", |_| twice += 1)
            .map_err(store)?
            .into_iter()
            .map(|run| run.path)
            .collect();
        if twice > 0 {
            return Err(BuildError::Invalid(format!(
                "unique column '{attribute}' holds {twice} value(s) for more than one live item"
            )));
        }
        let compared =
            tessera_store::unique::compare_unique_runs(kind, &inputs, &tombstones, &column_side)
                .map_err(store)?;
        if let Some((key, one, other)) = compared.shared_key {
            return Err(BuildError::Invalid(format!(
                "unique index '{attribute}': key {:x} names entities {one} and {other}, both live",
                key.widen()
            )));
        }
        if let Some(first) = compared.first_difference {
            return Err(BuildError::Invalid(format!(
                "unique index '{attribute}' holds {} live entries and its column {} values; they \
                 first differ at entry {first}",
                compared.index_entries, compared.column_entries
            )));
        }
        report.unique_entries += compared.index_entries;
    }
    Ok(())
}

/// A stored value at the shape the unique key derivation takes.
fn record_as_scalar(value: tessera_filter::RecordValue) -> tessera_spatial::ScalarValue {
    use tessera_filter::RecordValue as RV;
    use tessera_spatial::ScalarValue as SV;
    match value {
        RV::U8(x) => SV::U8(x),
        RV::U16(x) => SV::U16(x),
        RV::U32(x) => SV::U32(x),
        RV::U64(x) => SV::U64(x),
        RV::I8(x) => SV::I8(x),
        RV::I16(x) => SV::I16(x),
        RV::I32(x) => SV::I32(x),
        RV::I64(x) => SV::I64(x),
        RV::TimestampUs(x) => SV::TimestampUs(x),
        RV::Utf8(s) => SV::Utf8(s),
        _ => SV::Null,
    }
}

/// **`cuts.u32` names exactly the rows at which a leaf Morton cell begins, and each cell's
/// identities ascend** (contracts §2.6).
///
/// The index is what selection walks instead of reading every visible row of a tile, and both
/// halves of that walk rest on a property no other check establishes. The boundaries being where
/// the code changes is what makes the cells cover the segment without overlapping — a boundary too
/// few merges two cells and the ascending-identity property fails across the join; one too many
/// splits a cell, which loses no row but ends a prefix early and would serve a smaller `C_θ`.
/// Identities ascending within a cell is the row order itself (`(morton, tessera_id)`), stated
/// where selection depends on it rather than assumed from the producer.
///
/// The open has already refused a `cuts.u32` that is not strictly ascending, that starts anywhere
/// but row 0, or whose last cell begins past the segment
/// ([`tessera_store::read::CutIndex::load`]); what needs both columns is checked here.
///
/// One forward pass over `morton.u32` and the identity column, holding a row index and the
/// previous row's two values.
fn check_cut_index(
    phash: &str,
    partition: &tessera_store::read::PartitionData,
    report: &mut VerifyDeepReport,
) -> Result<()> {
    let mut views: Vec<_> = partition.views.iter().collect();
    views.sort_by(|a, b| a.0.cmp(b.0));
    for (view, data) in views {
        for segment in &data.segments {
            let codes = segment.morton.u32();
            let starts = segment.cuts.starts();
            let ids = segment.columns.tessera_id();
            let where_at = |row: usize| {
                format!("partition {phash}, view '{view}', segment '{}', row {row}", segment.seg_id)
            };
            let mut cell = 0usize;
            for row in 0..codes.len() {
                let opens = row == 0 || codes[row] != codes[row - 1];
                if opens {
                    match starts.get(cell) {
                        Some(&start) if start as usize == row => cell += 1,
                        Some(&start) => {
                            return Err(BuildError::Invalid(format!(
                                "cuts.u32 names row {start} where the Morton code changes at \
                                 row {row} ({})",
                                where_at(row)
                            )))
                        }
                        None => {
                            return Err(BuildError::Invalid(format!(
                                "cuts.u32 names {} cells and the Morton code changes again at \
                                 row {row} ({})",
                                starts.len(),
                                where_at(row)
                            )))
                        }
                    }
                } else {
                    if starts.get(cell).is_some_and(|&start| start as usize == row) {
                        return Err(BuildError::Invalid(format!(
                            "cuts.u32 opens a cell at row {row}, where the Morton code is \
                             unchanged ({})",
                            where_at(row)
                        )));
                    }
                    if ids[row] <= ids[row - 1] {
                        return Err(BuildError::Invalid(format!(
                            "identity {} does not follow {} inside one Morton cell — selection \
                             reads a cell's identities as ascending ({})",
                            ids[row],
                            ids[row - 1],
                            where_at(row)
                        )));
                    }
                }
            }
            if cell != starts.len() {
                return Err(BuildError::Invalid(format!(
                    "cuts.u32 names {} cells and the Morton column holds {cell} (partition \
                     {phash}, view '{view}', segment '{}')",
                    starts.len(),
                    segment.seg_id
                )));
            }
            report.cells += cell as u64;
        }
    }
    Ok(())
}

/// A file whose bytes disagree with what they were derived from, named by its path.
fn disagrees(path: PathBuf, reason: String) -> BuildError {
    BuildError::Store(tessera_store::StoreError::FileVerificationFailed { path, reason })
}

/// **Every segment's bands are its rows, and its cell codes its cells.** Each band holds exactly
/// the rows whose identity has that many leading zero bits, in row order, with the row's own
/// identity, code and residual and a copy of each render column's value; each cell code is the
/// Morton code of the cell's first row ([`tessera_store::bands`]). A refusal names the file.
fn check_bands(
    prefix_dir: &Path,
    phash: &str,
    partition: &tessera_store::read::PartitionData,
    report: &mut VerifyDeepReport,
) -> Result<()> {
    let mut views: Vec<_> = partition.views.iter().collect();
    views.sort_by(|a, b| a.0.cmp(b.0));
    for (view, data) in views {
        for segment in &data.segments {
            let dir = tessera_store::view_path(&prefix_dir.join("partitions").join(phash), view)
                .join("segments")
                .join(&segment.seg_id);
            let codes = segment.morton.u32();
            let expected: Vec<u32> = segment
                .cuts
                .starts()
                .iter()
                .map(|&start| codes[start as usize])
                .collect();
            if segment.cell_codes.codes() != expected.as_slice() {
                return Err(disagrees(
                    dir.join(tessera_store::bands::CELL_CODES_FILE),
                    "a cell code is not its cell's Morton code".to_string(),
                ));
            }
            segment
                .bands
                .check_against(codes, &segment.columns)
                .map_err(|reason| disagrees(dir.join(tessera_store::bands::BANDS_FILE), reason))?;
            report.band_entries += segment.bands.entries() as u64;
        }
    }
    Ok(())
}

/// **Every current label column has a band label copy, and every copy holds its column's label at
/// each entry's row.** A label column is current where the manifest records its level at the
/// version it was written at: a [`DerivedForm::LevelLabels`] is named only then, and a row-major
/// label column is named past it, without a copy. A copy is read against the label column of the
/// same view, layer, level and level version, and against the bands of the segment it names. A
/// refusal names the file: the copy, or the column that has none.
fn check_band_labels(
    prefix_dir: &Path,
    partition: &tessera_store::read::PartitionData,
    report: &mut VerifyDeepReport,
) -> Result<()> {
    use tessera_store::manifest::{DerivedExtent, DerivedForm};
    use tessera_types::layer::ServingLayout;
    let extents = &partition.manifest.derived_extents;
    let current = |e: &DerivedExtent| {
        partition
            .manifest
            .level_versions
            .iter()
            .any(|v| v.layer == e.layer && v.level == e.level && v.version == e.level_version)
    };
    let is_label_column = |e: &DerivedExtent| {
        matches!(
            e.form,
            DerivedForm::LevelLabels
                | DerivedForm::RowColumn {
                    layout: ServingLayout::RowMajorLabel
                }
        )
    };
    let same_level = |a: &DerivedExtent, b: &DerivedExtent| {
        a.layer == b.layer
            && a.level == b.level
            && a.view == b.view
            && a.level_version == b.level_version
    };
    for column in extents.iter().filter(|e| is_label_column(e) && current(e)) {
        let copied = extents
            .iter()
            .any(|e| matches!(e.form, DerivedForm::BandLabels { .. }) && same_level(e, column));
        if !copied {
            return Err(disagrees(
                prefix_dir.join(&column.path),
                format!(
                    "layer '{}' level {}'s label column has no copy in band order",
                    column.layer, column.level
                ),
            ));
        }
    }
    for copy in extents {
        let DerivedForm::BandLabels { seg_id } = &copy.form else {
            continue;
        };
        let path = prefix_dir.join(&copy.path);
        let column = extents
            .iter()
            .find(|e| is_label_column(e) && same_level(e, copy))
            .ok_or_else(|| disagrees(path.clone(), "names no label column".to_string()))?;
        let segment = copy
            .view
            .as_ref()
            .and_then(|view| partition.views.get(view))
            .and_then(|data| data.segments.iter().find(|s| &s.seg_id == seg_id))
            .ok_or_else(|| {
                disagrees(
                    path.clone(),
                    format!("names segment '{seg_id}', which its view does not hold"),
                )
            })?;
        let labels =
            tessera_store::membership::LabelColumnPack::open(&prefix_dir.join(&column.path))
                .map_err(BuildError::Store)?;
        let copied = tessera_store::bands::BandLabels::open(&path, &segment.bands)
            .map_err(|e| disagrees(path.clone(), e.to_string()))?;
        for (e, &row) in segment.bands.rows().iter().enumerate() {
            if copied.label(e) != labels.label(row as usize) {
                return Err(disagrees(
                    path,
                    format!(
                        "holds {} at entry {e} and its label column holds {} at row {row}",
                        copied.label(e),
                        labels.label(row as usize)
                    ),
                ));
            }
        }
        report.band_label_copies += 1;
    }
    Ok(())
}

/// **Every row-major column has one member file beside it, and the file is what the column's
/// labels give.** The members and coverings are written again from the column into a scratch
/// directory and compared artifact by artifact. A refusal names the member file, or the column that
/// has none.
fn check_row_members(
    prefix_dir: &Path,
    partition: &tessera_store::read::PartitionData,
    report: &mut VerifyDeepReport,
) -> Result<()> {
    use tessera_store::manifest::{DerivedExtent, DerivedForm};
    use tessera_store::row_members::RowMembersPack;
    let extents = &partition.manifest.derived_extents;
    let same_level = |a: &DerivedExtent, b: &DerivedExtent| {
        a.layer == b.layer
            && a.level == b.level
            && a.view == b.view
            && a.level_version == b.level_version
            && a.incarnation == b.incarnation
    };
    for members in extents.iter().filter(|e| e.form == DerivedForm::RowMembers) {
        if !extents
            .iter()
            .any(|e| matches!(e.form, DerivedForm::RowColumn { .. }) && same_level(e, members))
        {
            return Err(disagrees(
                prefix_dir.join(&members.path),
                "names no row-major column".to_string(),
            ));
        }
    }
    let scratch =
        std::env::temp_dir().join(format!("tessera-verify-members-{}", std::process::id()));
    let checked = (|| {
        for column in extents.iter() {
            let DerivedForm::RowColumn { layout } = column.form else {
                continue;
            };
            let column_path = prefix_dir.join(&column.path);
            let mut beside = extents
                .iter()
                .filter(|e| e.form == DerivedForm::RowMembers && same_level(e, column));
            let (Some(members), None) = (beside.next(), beside.next()) else {
                return Err(disagrees(
                    column_path,
                    format!(
                        "layer '{}' level {}'s column has no single member file beside it",
                        column.layer, column.level
                    ),
                ));
            };
            let members_path = prefix_dir.join(&members.path);
            let stored = RowMembersPack::open(&members_path).map_err(BuildError::Store)?;
            let fresh_path =
                tessera_store::derived::stage_row_members(&column_path, layout, &scratch)
                    .map_err(BuildError::Store)?;
            let fresh = RowMembersPack::open(&fresh_path);
            let _ = fs::remove_file(&fresh_path);
            let fresh = fresh.map_err(BuildError::Store)?;
            if (stored.ordinals(), stored.rows()) != (fresh.ordinals(), fresh.rows()) {
                return Err(disagrees(
                    members_path,
                    format!(
                        "covers {} ordinals over {} rows and its column {} over {}",
                        stored.ordinals(),
                        stored.rows(),
                        fresh.ordinals(),
                        fresh.rows()
                    ),
                ));
            }
            for ordinal in 0..fresh.ordinals() {
                if stored.members(ordinal) != fresh.members(ordinal) {
                    return Err(disagrees(
                        members_path,
                        format!("holds other members for ordinal {ordinal} than its column labels"),
                    ));
                }
                if !stored.covering(ordinal).eq(fresh.covering(ordinal)) {
                    return Err(disagrees(
                        members_path,
                        format!(
                            "holds another covering for ordinal {ordinal} than its column gives"
                        ),
                    ));
                }
            }
            report.row_member_files += 1;
        }
        Ok(())
    })();
    let _ = fs::remove_dir_all(&scratch);
    checked
}

/// **A rendered group-scoped family's lane is present in every build segment of every view that
/// renders it** (`views.md` §5).
///
/// The lane is what `render` on a family *is*: one column in the row tail of each view of the
/// group, and of any group sharing those views. It is the one artefact of the placement whose
/// absence is silent — serving reads a missing scoped column as the ordinary absence a flush
/// leaves, so a build that wrote no lane at all is indistinguishable, at the wire, from one whose
/// rows genuinely carry no value. Nothing else in this pass would notice: the file digests match
/// (the lane changes `columns.arrow`, which is digested as a whole), the row space is a bijection
/// either way, and the identity column is untouched.
///
/// **Every segment of a view of the family's own group is checked, the build's and the write
/// path's alike.** A flush of such a view writes the lane from the row's own scoped values, and a
/// merge and a fold take the view's schema rather than the bundle's — so a missing lane there is
/// the same silent defect it is at a build, not the write half's deliberate absence it once was.
///
/// ⊘ **A view of a group that declares `members` keeps the build-only exemption.** Its rows render
/// the owner's family, but no batch into it may carry a value: the column is the owner's, and a
/// second writer for one `(entity, view)` column is two layers claiming one entity (`views.md`
/// §5's ingest paragraph). A flush of such a view writes the lane holding absences once the family
/// lists the key — and cannot before, the owner's own first flush being what puts it there — so a
/// segment of a sharing group's view may legitimately hold no lane and requiring one would refuse
/// a bundle that has ingested in that order. A build segment is one whose `columns.arrow` is named
/// in `MANIFEST.files`, which is contracts §2.2's own division: that map covers what existed at
/// build time and `SEGMENTS-<n>.files` covers what has appeared since.
fn check_scoped_render_lanes(
    manifest: &tessera_store::manifest::Manifest,
    phash: &str,
    partition: &tessera_store::read::PartitionData,
    report: &mut VerifyDeepReport,
) -> Result<()> {
    let families: Vec<_> = manifest
        .scoped_scalars()
        .into_iter()
        .filter(|f| f.render)
        .collect();
    if families.is_empty() {
        return Ok(());
    }
    for (view_id, view) in &partition.views {
        // Which families this view renders — its own group's, or those of the group it declares
        // `members` of, the keys being the owner's (§3.3). Stated here off the manifest's roster,
        // as the engine states it off the same roster at the request and the build off its
        // arguments; all three must agree, and this is the one that can catch a disagreement.
        let Some((group, key)) = manifest.groups.iter().find_map(|g| {
            let key = view_id
                .strip_prefix(g.name.as_str())?
                .strip_prefix(tessera_store::GROUP_SEPARATOR)?;
            g.views
                .iter()
                .any(|v| v.key == key)
                .then_some((g.name.as_str(), key))
        }) else {
            continue;
        };
        let owner = manifest
            .groups
            .iter()
            .find(|g| g.name == group)
            .and_then(|g| g.members_of.as_deref())
            .unwrap_or(group);
        let owed: Vec<_> = families
            .iter()
            .filter(|f| {
                f.group == owner
                    && f.views
                        .contains(&format!("{owner}{}{key}", tessera_store::GROUP_SEPARATOR))
            })
            .collect();
        if owed.is_empty() {
            continue;
        }
        for segment in &view.segments {
            // The `files` map's own key: partition-qualified, forward slashes, exactly as the
            // build laid it down (contracts §2.2).
            let rel = format!(
                "partitions/{phash}/{}/segments/{}/columns.arrow",
                tessera_store::view_rel(view_id),
                segment.seg_id
            );
            if owner != group && !manifest.files.contains_key(&rel) {
                continue;
            }
            for family in &owed {
                if segment.columns.scalar(&family.name).is_none() {
                    return Err(BuildError::Invalid(format!(
                        "view '{view_id}', segment '{}': the manifest declares '{}' as a \
                         `render` family of group '{}' with a column for this view, and the \
                         segment's tail does not hold it. A rendered scoped column is served from \
                         the row tail, and its absence is read as an absent value rather than as \
                         a fault (views §5)",
                        segment.seg_id, family.name, family.group
                    )));
                }
                report.scoped_render_lanes += 1;
            }
        }
    }
    Ok(())
}

/// Join a manifest-supplied, forward-slash relative path onto the prefix directory, refusing
/// anything that could escape it — the same rule the read protocol applies, restated here because
/// its implementation is private to `tessera-store`.
fn join_rel(prefix_dir: &Path, rel: &str) -> Result<PathBuf> {
    if rel.is_empty() || rel.starts_with('/') || rel.contains('\\') {
        return Err(BuildError::Invalid(format!(
            "manifest path '{rel}' is not a safe prefix-relative path"
        )));
    }
    let mut path = prefix_dir.to_path_buf();
    for component in rel.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err(BuildError::Invalid(format!(
                "manifest path '{rel}' contains an unsafe component"
            )));
        }
        path.push(component);
    }
    Ok(path)
}

/// Postings sorted, duplicate-free and bounded by `entity_id_high_water` — the base file and
/// every delta tier — and `pairs.parquet` exactly the union of the **base** postings (see the
/// module doc for why the deltas are excluded on purpose).
fn check_postings_and_pairs(
    prefix_dir: &Path,
    phash: &str,
    bundle_manifest: &Manifest,
    partition_manifest: &SegmentsManifest,
    report: &mut VerifyDeepReport,
) -> Result<()> {
    let high_water = partition_manifest.entity_id_high_water;
    let named = |rel: &str| {
        partition_manifest.files.contains_key(rel) || bundle_manifest.files.contains_key(rel)
    };

    let postings_rel = format!("partitions/{phash}/terms/postings.arrow");
    let pairs_rel = format!("partitions/{phash}/terms/pairs.parquet");
    if named(&postings_rel) {
        let path = join_rel(prefix_dir, &postings_rel)?;
        let reader =
            PostingsReader::open(&path, false).map_err(|e| BuildError::io(&path, e))?;

        let mut pairs = if named(&pairs_rel) {
            Some(PairsCursor::open(&join_rel(prefix_dir, &pairs_rel)?)?)
        } else {
            None
        };

        for t in 0..reader.term_count() {
            let posting = reader
                .posting_at(t)
                .map_err(|e| BuildError::io(&path, e))?
                .expect("t < term_count, so the record exists");
            let what = format!("{postings_rel}: term {t}");
            walk_posting(&what, &posting, high_water, |entity| {
                let Some(cursor) = pairs.as_mut() else {
                    return Ok(());
                };
                match cursor.next()? {
                    Some((pe, pt)) if pt == t && pe == entity as u64 => Ok(()),
                    Some((pe, pt)) => Err(BuildError::Invalid(format!(
                        "{pairs_rel}: holds (entity {pe}, term {pt}) where the base postings \
                         hold (entity {entity}, term {t}) — the file must be exactly the union \
                         of the base postings it was written with"
                    ))),
                    None => Err(BuildError::Invalid(format!(
                        "{pairs_rel}: ends before the base postings do — (entity {entity}, \
                         term {t}) is missing from it"
                    ))),
                }
            })?;
            report.terms += 1;
        }
        if let Some(mut cursor) = pairs {
            if let Some((pe, pt)) = cursor.next()? {
                return Err(BuildError::Invalid(format!(
                    "{pairs_rel}: holds (entity {pe}, term {pt}) beyond the base postings' \
                     union — the file is written by the build and rewritten by the fold, and \
                     must carry no pair the base it was written with does not"
                )));
            }
            report.pairs_rows += cursor.consumed;
        }
    } else if named(&pairs_rel) {
        return Err(BuildError::Invalid(format!(
            "{pairs_rel} is named but {postings_rel} is not — there is no base to scope its \
             union against"
        )));
    }

    for delta_rel in &partition_manifest.deltas {
        let path = join_rel(prefix_dir, delta_rel)?;
        let tier = DeltaTier::open(&path).map_err(|e| BuildError::io(&path, e))?;
        for ordinal in tier.ordinals() {
            let posting = tier
                .posting_at(ordinal)
                .map_err(|e| BuildError::io(&path, e))?
                .expect("the ordinal came from the tier's own iterator");
            let what = format!("{delta_rel}: term ordinal {ordinal}");
            walk_posting(&what, &posting, high_water, |_| Ok(()))?;
        }
        report.delta_tiers += 1;
    }
    Ok(())
}

/// Walk one posting's entities in stored order, refusing an unsorted or duplicated entry and any
/// entity at or past `high_water`, handing each accepted entity to `and_then` (the pairs
/// comparison, where one is running). A tag-1 record is a Roaring bitmap — a set, sorted and
/// duplicate-free by construction — so for it the order check is vacuous and the walk exists for
/// the bound and for `and_then`.
fn walk_posting(
    what: &str,
    posting: &PostingRef<'_>,
    high_water: u64,
    mut and_then: impl FnMut(u32) -> Result<()>,
) -> Result<()> {
    let mut previous: Option<u32> = None;
    let mut visit = |entity: u32| -> Result<()> {
        if let Some(previous) = previous {
            if entity <= previous {
                return Err(BuildError::Invalid(format!(
                    "{what}: entity {entity} follows {previous} — postings must be sorted \
                     strictly ascending, duplicate-free"
                )));
            }
        }
        if entity as u64 >= high_water {
            return Err(BuildError::Invalid(format!(
                "{what}: entity {entity} is at or past entity_id_high_water {high_water}"
            )));
        }
        previous = Some(entity);
        and_then(entity)
    };
    match posting {
        PostingRef::Array(bytes) => {
            // A payload length that is not a multiple of 4 was refused at the reader's open.
            for chunk in bytes.as_chunks::<4>().0 {
                visit(u32::from_le_bytes(*chunk))?;
            }
        }
        PostingRef::Roaring(view) => {
            for entity in view.iter() {
                visit(entity)?;
            }
        }
    }
    Ok(())
}

/// A sequential cursor over `pairs.parquet`'s `(entity_id, term_id)` rows, in file order — which
/// the writer fixes as `(term_id, entity_id)`-sorted, exactly the order the base postings walk
/// produces.
struct PairsCursor {
    path: PathBuf,
    reader: ParquetRecordBatchReader,
    batch: Option<(UInt64Array, UInt32Array)>,
    idx: usize,
    consumed: u64,
}

impl PairsCursor {
    fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).map_err(|e| BuildError::io(path, e))?;
        let reader = ParquetRecordBatchReaderBuilder::try_new(file)
            .and_then(|builder| builder.build())
            .map_err(|e| BuildError::Parquet {
                path: path.to_path_buf(),
                detail: e.to_string(),
            })?;
        Ok(PairsCursor {
            path: path.to_path_buf(),
            reader,
            batch: None,
            idx: 0,
            consumed: 0,
        })
    }

    fn next(&mut self) -> Result<Option<(u64, u32)>> {
        loop {
            if let Some((entities, terms)) = &self.batch {
                if self.idx < entities.len() {
                    let row = (entities.value(self.idx), terms.value(self.idx));
                    self.idx += 1;
                    self.consumed += 1;
                    return Ok(Some(row));
                }
            }
            let Some(batch) = self.reader.next() else {
                return Ok(None);
            };
            let batch = batch.map_err(|e| BuildError::Arrow {
                path: self.path.clone(),
                detail: e.to_string(),
            })?;
            let malformed =
                |detail: &str| BuildError::Invalid(format!("{}: {detail}", self.path.display()));
            let entities = batch
                .column_by_name("entity_id")
                .and_then(|c| c.as_any().downcast_ref::<UInt64Array>())
                .ok_or_else(|| malformed("no uint64 'entity_id' column"))?
                .clone();
            let terms = batch
                .column_by_name("term_id")
                .and_then(|c| c.as_any().downcast_ref::<UInt32Array>())
                .ok_or_else(|| malformed("no uint32 'term_id' column"))?
                .clone();
            self.batch = Some((entities, terms));
            self.idx = 0;
        }
    }
}

/// The record blob's addressing, walked layer by layer: every block decompressed, every row
/// framed inside its extent, every row's identity taken from the block that holds it and compared
/// with the has-row bitmap's member at its rank (`records-and-search.md` §3, §10).
///
/// **This is the one structure whose addressing nothing else in the bundle can see.** The other
/// homes are positional, so there is no offset to be wrong; the blob reaches a row through three
/// derived indirections, and a fold or coalesce that rewrote them wrongly would produce a bundle
/// whose files all match their digests and whose drill-down serves a neighbour's record. The read
/// path refuses that per request; without this pass nothing reports it offline, so a defect would
/// first be seen by a viewer receiving someone else's record.
///
/// The walk is [`tessera_filter::RecordBlob::for_each_row`], which is the reader's own self-check
/// and not a transcription of it: a verifier with its own idea of the format would agree with a
/// blob the reader refuses, or refuse one it serves.
///
/// **The base blob is walked where the manifest names its files**, not where the directory happens
/// to exist: a build writes `attrs/record/` only where a column is blob-resident, and probing the
/// filesystem would read a base deleted from under the manifest as "this bundle has no base".
/// Extents come from the segments manifest's two lists, the flushes' and the artifact levels',
/// which hold the same format and take the same reader.
fn check_record_blobs(
    prefix_dir: &Path,
    phash: &str,
    manifest: &Manifest,
    partition_manifest: &SegmentsManifest,
    report: &mut VerifyDeepReport,
) -> Result<()> {
    let base_rel = format!(
        "partitions/{phash}/attrs/record/{}",
        tessera_filter::RECORD_BLOCKS_FILE
    );
    let mut layers: Vec<(String, PathBuf, PathBuf, PathBuf)> = Vec::new();
    if manifest.files.contains_key(&base_rel) {
        let dir = prefix_dir
            .join("partitions")
            .join(phash)
            .join("attrs")
            .join("record");
        layers.push((
            format!("partitions/{phash}/attrs/record"),
            dir.join(tessera_filter::RECORD_BLOCKS_FILE),
            dir.join(tessera_filter::RECORD_HASROW_FILE),
            dir.join(tessera_filter::RECORD_DIRECTORY_FILE),
        ));
    }
    for extent in partition_manifest
        .record_extents
        .iter()
        .chain(partition_manifest.artifact_record_extents.iter())
    {
        layers.push((
            extent.blocks.clone(),
            join_rel(prefix_dir, &extent.blocks)?,
            join_rel(prefix_dir, &extent.hasrow)?,
            join_rel(prefix_dir, &extent.directory)?,
        ));
    }

    for (name, blocks, hasrow, directory) in layers {
        // Mapped, not read: `Access::Read` pulls `blocks.bin` and `directory.arrow` into the
        // heap whole, and a corpus's record blocks are the corpus. The open-time digest sweep
        // has already vouched for both files, so nothing here needs a private copy of them.
        //
        // Sequential because the walk visits each block exactly once and wants the drop-behind:
        // without it a deep verify on a serving box leaves the whole blob resident in the cache
        // the request path is using. `RecordBlob::open` takes one access for both files, so the
        // hint reaches `directory.arrow` as well — it is decoded at open and then read in block
        // order, so the most a drop-behind costs there is one re-fault of a file whose size is a
        // handful of bytes per block.
        let blob = tessera_filter::RecordBlob::open(
            &blocks,
            &hasrow,
            &directory,
            tessera_filter::Access::MappedSequential,
        )
        .map_err(|e| BuildError::Invalid(format!("{name}: {e}")))?;
        let mut rows = 0u64;
        blob.for_each_row(&mut |_, _| {
            rows += 1;
            Ok(())
        })
        .map_err(|e| BuildError::Invalid(format!("{name}: {e}")))?;
        if rows != blob.rows() {
            return Err(BuildError::Invalid(format!(
                "{name}: the walk returned {rows} rows where the has-row bitmap holds {} — the \
                 blocks and the bitmap do not address the same rows",
                blob.rows()
            )));
        }
        report.record_rows += rows;
    }
    Ok(())
}

/// Dictionary extents positional and never repeating a descriptor (decision 0042): each extent's
/// record count must equal what the manifest declares — a term's ordinal is its position in the
/// concatenation, so a miscount shifts every later term — and no descriptor may appear twice
/// across the list, which after a restart renumbers everything past the repeat. The reader
/// (`Dict::load`) tolerates a repeat by skipping it, deliberately; this is the artefact-side
/// statement that no correct writer produces one.
///
/// **This is the pass's remaining unbounded arm.** It reads each extent whole and holds a `HashSet`
/// of every descriptor it has seen, so its memory is the term vocabulary's — not the corpus's,
/// which is why it survived the bounding of the row space and the record blob, but a deployment
/// whose vocabulary is itself corpus-scale would find it here. Bounding it needs the descriptors
/// sorted rather than hashed, which is a partition pass like the identity check's.
fn check_dict_extents(
    prefix_dir: &Path,
    partition_manifest: &SegmentsManifest,
    report: &mut VerifyDeepReport,
) -> Result<()> {
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    for extent in &partition_manifest.dict_extents {
        let path = join_rel(prefix_dir, &extent.path)?;
        let bytes = fs::read(&path).map_err(|e| BuildError::io(&path, e))?;
        let mut offset = 0usize;
        let mut records = 0u64;
        while offset < bytes.len() {
            let truncated = |what: &str| {
                BuildError::Invalid(format!(
                    "{}: record {records} {what} — the extent is not a whole number of \
                     `u32 length ‖ descriptor` records",
                    extent.path
                ))
            };
            if bytes.len() - offset < 4 {
                return Err(truncated("cuts off inside its length field"));
            }
            let len = u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("4 bytes"))
                as usize;
            offset += 4;
            if bytes.len() - offset < len {
                return Err(truncated("overruns the end of the file"));
            }
            let descriptor = &bytes[offset..offset + len];
            if !seen.insert(descriptor.to_vec()) {
                return Err(BuildError::Invalid(format!(
                    "{}: record {records} repeats a descriptor an earlier extent record already \
                     carries — ordinals are positions in the concatenation, and a repeat \
                     renumbers every term after it across a restart (decision 0042)",
                    extent.path
                )));
            }
            offset += len;
            records += 1;
        }
        if records != extent.records {
            return Err(BuildError::Invalid(format!(
                "{}: holds {records} records but the manifest declares {} — the extent list is \
                 positional, so the declared counts are what place every later extent's terms",
                extent.path, extent.records
            )));
        }
        report.dict_records += records;
    }
    Ok(())
}
