//! A point shard opened at a running service, or by a log replayed past every manifest: its empty
//! base, written by the store's writer for a shard of no item, and its record.

use std::collections::BTreeMap;
use std::path::Path;

use mosaica_store::empty_shard::{write_empty_shard, EmptyShard, EmptyView};
use mosaica_store::manifest::{
    DeclaredScalar, FileDigest, MembershipExtent, RecordPackExtent, ScopedColumn, ScopedScalar,
    ShardRecord, UniqueIndexRuns,
};
use mosaica_store::read::Bundle;
use mosaica_store::{ShardLayout, StoreError};
use mosaica_types::{LayerSpace, ShardId};

use crate::error::EngineError;

/// An opened shard's base segment, in every view.
const BASE_SEG_ID: &str = "seg-0";

/// What every shard opened together is written from, beside the bundle each is added to.
pub(crate) struct Opening<'a> {
    pub(crate) bundle_root: &'a Path,
    pub(crate) prefix_dir: &'a Path,
    pub(crate) prefix: &'a str,
    pub(crate) phash: &'a str,
    /// How many terms the dictionary holds.
    pub(crate) terms: u64,
    /// Every record pack the partition's layer spaces name.
    pub(crate) record_packs: Vec<(LayerSpace, RecordPackExtent)>,
    /// [`OpenedBase::runtime`], from [`runtime_columns`].
    pub(crate) runtime: Vec<String>,
}

/// The columns declared at a running service that an opened shard holds the base of: the
/// entity-scoped ones, then the group-scoped families.
pub(crate) fn runtime_columns(
    (entity, scoped): (Vec<DeclaredScalar>, Vec<ScopedScalar>),
) -> Vec<String> {
    entity
        .into_iter()
        .map(|attribute| attribute.name)
        .chain(scoped.into_iter().map(|family| family.name))
        .collect()
}

/// A shard [`open_shards`] wrote: its member packs, and their files' digests.
pub(crate) struct OpenedMembers {
    pub(crate) id: ShardId,
    pub(crate) extents: Vec<MembershipExtent>,
    pub(crate) files: BTreeMap<String, FileDigest>,
}

/// Write each of `ids` as [`write_opened_shard`] does and add it to `bundle`'s partition, so the
/// bundle serves it. The record's identity is taken from the `MANIFEST.json` digest `CURRENT`
/// names.
pub(crate) fn open_shards(
    opening: &Opening<'_>,
    bundle: &mut Bundle,
    ids: &[ShardId],
) -> Result<Vec<OpenedMembers>, EngineError> {
    let manifest_digest = crate::engine::read_current(opening.bundle_root)?.manifest_digest;
    let mut opened = Vec::with_capacity(ids.len());
    for &id in ids {
        let (record, files) = write_opened_shard(
            &OpenedBase {
                prefix_dir: opening.prefix_dir,
                phash: opening.phash,
                bundle,
                manifest_digest: &manifest_digest,
                terms: opening.terms,
                record_packs: &opening.record_packs,
                runtime: &opening.runtime,
            },
            id,
        )
        .map_err(EngineError::Store)?;
        opened.push(OpenedMembers {
            id,
            extents: record.membership_extents.clone(),
            files: files
                .iter()
                .filter(|(rel, _)| mosaica_store::layout::is_artifact_file(opening.phash, rel))
                .map(|(rel, digest)| (rel.clone(), digest.clone()))
                .collect(),
        });
        bundle
            .add_opened_shard(
                opening.prefix_dir,
                opening.prefix,
                opening.phash,
                record,
                files,
            )
            .map_err(EngineError::Store)?;
    }
    Ok(opened)
}

/// What an opened shard's base is written from.
struct OpenedBase<'a> {
    prefix_dir: &'a Path,
    phash: &'a str,
    bundle: &'a Bundle,
    /// The hex digest of the prefix's `MANIFEST.json`, which the shard's identity is taken from.
    manifest_digest: &'a str,
    /// How many terms the dictionary holds.
    terms: u64,
    /// Every record pack the partition's layer spaces name.
    record_packs: &'a [(LayerSpace, RecordPackExtent)],
    /// The columns declared at a running service and not yet written into `MANIFEST.json` by a
    /// compaction, entity and group-scoped.
    runtime: &'a [String],
}

/// Write shard `id`'s empty base: what a build of no item writes for the columns and views the
/// bundle declares, and a member pack listing nothing beside each record pack. Returns the record,
/// open at high water 0, and the digest of every file written. The record lists as based each
/// column of `runtime` the bundle declares, and each group-scoped column another shard lists, since
/// it holds the base of all of them.
fn write_opened_shard(
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
