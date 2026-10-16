//! A point shard opened at a running service, or by a log replayed past every manifest: its empty
//! base, written by the store's writer for a shard of no item, and its record.

use std::collections::BTreeMap;
use std::path::Path;

use mosaica_store::empty_shard::{write_empty_shard, EmptyShard, EmptyView};
use mosaica_store::manifest::{
    FileDigest, RecordPackExtent, ScopedColumn, ShardRecord, UniqueIndexRuns,
};
use mosaica_store::read::Bundle;
use mosaica_store::{ShardLayout, StoreError};
use mosaica_types::{LayerSpace, ShardId};

/// An opened shard's base segment, in every view.
const BASE_SEG_ID: &str = "seg-0";

/// What an opened shard's base is written from.
pub(crate) struct OpenedBase<'a> {
    pub(crate) prefix_dir: &'a Path,
    pub(crate) phash: &'a str,
    pub(crate) bundle: &'a Bundle,
    /// The hex digest of the prefix's `MANIFEST.json`, which the shard's identity is taken from.
    pub(crate) manifest_digest: &'a str,
    /// How many terms the dictionary holds.
    pub(crate) terms: u64,
    /// Every record pack the partition's layer spaces name.
    pub(crate) record_packs: &'a [(LayerSpace, RecordPackExtent)],
    /// The columns declared at a running service and not yet folded, entity and group-scoped.
    pub(crate) runtime: &'a [String],
}

/// Write shard `id`'s empty base: what a build of no item writes for the columns and views the
/// bundle declares, and a member pack listing nothing beside each record pack. Returns the record,
/// open at high water 0, and the digest of every file written. The record lists as based each
/// column of `runtime` the bundle declares, and each group-scoped column another shard lists, since
/// it holds the base of all of them.
pub(crate) fn write_opened_shard(
    base: &OpenedBase<'_>,
    id: ShardId,
) -> Result<(ShardRecord, BTreeMap<String, FileDigest>), StoreError> {
    let manifest = &base.bundle.manifest;
    let partition =
        base.bundle
            .partitions
            .get(base.phash)
            .ok_or_else(|| StoreError::MalformedBundle {
                detail: format!("no partition '{}' in this bundle", base.phash),
            })?;
    let layout = ShardLayout::new(base.phash, id);
    let scoped = manifest.scoped_scalars();
    let runtime_bases: Vec<String> = base
        .runtime
        .iter()
        .filter(|name| {
            manifest.declared_scalars.iter().any(|d| d.name == **name)
                || scoped.iter().any(|f| f.name == **name)
        })
        .cloned()
        .collect();
    let mut scoped_columns: Vec<ScopedColumn> = Vec::new();
    for column in partition
        .shards
        .iter()
        .flat_map(|shard| shard.record.scoped_columns.iter())
    {
        let written = manifest.is_live_incarnation(&column.view, column.incarnation)
            && scoped
                .iter()
                .any(|f| f.name == column.column && f.views.contains(&column.view));
        if written && !scoped_columns.contains(column) {
            scoped_columns.push(column.clone());
        }
    }
    let views: Vec<EmptyView<'_>> = manifest
        .views
        .iter()
        .map(|view| EmptyView {
            id: &view.id,
            incarnation: view.incarnation,
            render: super::view_scalar_schema_of(manifest, &view.id),
        })
        .collect();
    let pairs = partition.shards.first().is_some_and(|first| {
        let rel = ShardLayout::new(base.phash, first.record.id).pairs_rel();
        manifest.files.contains_key(&rel) || partition.manifest.files.contains_key(&rel)
    });
    let written = write_empty_shard(&EmptyShard {
        prefix_dir: base.prefix_dir,
        layout,
        terms: base.terms,
        small_term_threshold: manifest.small_term_threshold,
        pairs,
        declared: &manifest.declared_scalars,
        vocabularies: &manifest.vocabularies,
        scoped: &scoped,
        views: &views,
        seg_id: BASE_SEG_ID,
    })?;
    let mut paths = written.files;
    let mut membership_extents = Vec::with_capacity(base.record_packs.len());
    for (space, records) in base.record_packs {
        let extent =
            mosaica_store::membership::empty_member_pack(base.prefix_dir, layout, *space, records)?;
        paths.push((extent.path.clone(), base.prefix_dir.join(&extent.path)));
        membership_extents.push(extent);
    }
    let files = paths
        .iter()
        .map(|(rel, path)| Ok((rel.clone(), mosaica_store::digest_of(path)?)))
        .collect::<Result<BTreeMap<_, _>, StoreError>>()?;
    let bundle_digest =
        mosaica_store::manifest::hex_32::decode(base.manifest_digest).ok_or_else(|| {
            StoreError::MalformedBundle {
                detail: format!(
                    "CURRENT names a MANIFEST.json digest '{}' that is not 64 hexadecimal digits",
                    base.manifest_digest
                ),
            }
        })?;
    let record = ShardRecord {
        identity: mosaica_store::manifest::shard_identity(&bundle_digest, id),
        segments: written.segments,
        unique_indexes: partition
            .manifest
            .unique_columns
            .iter()
            .map(|attribute| UniqueIndexRuns {
                attribute: attribute.clone(),
                base: Vec::new(),
                live: Vec::new(),
            })
            .collect(),
        membership_extents,
        scoped_columns,
        runtime_bases,
        ..ShardRecord::empty(id)
    };
    Ok((record, files))
}
