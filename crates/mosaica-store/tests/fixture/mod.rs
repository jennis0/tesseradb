//! A hand-assembled bundle, and the pieces a flush would publish into it.
//!
//! Each test binary compiles this module on its own, so a helper only one of them uses is dead
//! code in the others. That is what the allow below is for, and it is scoped to this fixture.
#![allow(dead_code)]

use std::fs;
use std::path::Path;

use sha2::{Digest, Sha256};

use mosaica_spatial::fixed32;
use mosaica_spatial::tiler::{sort_batch, TilerItem};
use mosaica_store::manifest::{
    CurrentPointer, FileDigest, IdentityDescriptor, Manifest, PartitionDescriptor, Quantisation,
    SegmentDescriptor, SegmentsManifest, ViewDescriptor,
};
use mosaica_store::permutation::SegmentExtent;
use mosaica_store::read::SegmentData;
use mosaica_store::write::{write_permutation, write_segment};
use mosaica_store::{write_flush_segment, Bundle, FlushInput, FlushRow, ShardLayout};
use mosaica_types::{
    EntityId, IdentityKey, MosaicaId, ShardId, IDENTITY_CONSTRUCTION, IDENTITY_ROUNDS,
};

pub const PARTITION: &str = "default";
pub const SHARD: ShardLayout<'static> = ShardLayout::new(PARTITION, ShardId::ZERO);
pub const VIEW: &str = "main";

/// A `mosaica_id`-shaped value: splitmix64 over `seed`, so its top 16 bits are a priority prefix
/// like a real one's.
pub fn synthetic_mosaica_id(seed: u64) -> MosaicaId {
    let mut z = seed.wrapping_add(0x9E3779B97F4A7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    MosaicaId::new(z ^ (z >> 31))
}

pub fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

pub fn file_digest(path: &Path) -> FileDigest {
    let bytes = fs::read(path).expect("read for digest");
    FileDigest {
        size: bytes.len() as u64,
        sha256: hex_sha256(&bytes),
    }
}

/// Items for entities `[entity_lo, entity_lo + count)`, spread over a 100 x 100 grid.
pub fn items_for(entity_lo: u64, count: u64) -> Vec<TilerItem> {
    (entity_lo..entity_lo + count)
        .map(|e| TilerItem {
            mosaica_id: synthetic_mosaica_id(e),
            qx: fixed32(((e * 37) % 100) as f64 / 100.0, 0.0, 1.0),
            qy: fixed32(((e * 61) % 100) as f64 / 100.0, 0.0, 1.0),
            scalars: vec![],
        })
        .collect()
}

/// A one-segment bundle over entities `[0, n)`, at prefix `v00000`. Returns its items in row
/// order and their Morton codes.
pub fn build_bundle(root: &Path, n: u64) -> (Vec<TilerItem>, Vec<u32>) {
    let built = publish_seg0(&root.join("v00000"), items_for(0, n));
    publish_manifest(root, "2026-08-02T00:00:00Z");
    built
}

/// Sorts `items`, entities `[0, n)` in order, into `seg0` of [`VIEW`], writes its permutation, and
/// publishes both as `SEGMENTS-0`. Returns the items in row order and their Morton codes.
pub fn publish_seg0(prefix_dir: &Path, mut items: Vec<TilerItem>) -> (Vec<TilerItem>, Vec<u32>) {
    let n = items.len() as u64;
    let mut entity_ids: Vec<EntityId> = (0..n).map(EntityId::new).collect();
    let codes = sort_batch(&mut items, &mut entity_ids);

    let seg_rel = SHARD.segment_rel(VIEW, "seg0");
    let seg_dir = prefix_dir.join(&seg_rel);
    fs::create_dir_all(&seg_dir).expect("mkdir");
    write_segment(&seg_dir, &items, &codes, &[], &[]).expect("write_segment");
    let permutation_rel = SHARD.permutation_rel(VIEW);
    write_permutation(&prefix_dir.join(&permutation_rel), &entity_ids, n).expect("permutation");

    let mut manifest = SegmentsManifest::empty();
    let rels = mosaica_store::SEGMENT_FILES.iter().map(|name| format!("{seg_rel}/{name}"));
    manifest.partition.files = std::iter::once(permutation_rel)
        .chain(rels)
        .map(|rel| {
            let digest = file_digest(&prefix_dir.join(&rel));
            (rel, digest)
        })
        .collect();
    let record = manifest.only_shard_mut();
    (record.watermark, record.high_water) = (n, n);
    record.segments = vec![SegmentDescriptor {
        incarnation: 0,
        view: VIEW.into(),
        seg_id: "seg0".into(),
        row_count: n as u32,
        entity_lo: 0,
        entity_hi: n.saturating_sub(1),
    }];
    mosaica_store::write_publication(prefix_dir, PARTITION, 0, &manifest, None)
        .expect("SEGMENTS-0");
    (items, codes)
}

/// The bundle manifest over [`VIEW`] in [`PARTITION`] on the unit square.
pub fn manifest(created_at: &str) -> Manifest {
    Manifest {
        bundle_format: mosaica_types::BUNDLE_FORMAT,
        created_at: created_at.to_string(),
        declared_scalars: vec![],
        vocabularies: vec![],
        small_term_threshold: 32,
        identity: IdentityDescriptor {
            construction: IDENTITY_CONSTRUCTION.to_string(),
            rounds: IDENTITY_ROUNDS,
            key: "0123456789abcdef0123456789abcdef".to_string(),
        },
        groups: Vec::new(),
        views: vec![ViewDescriptor {
            incarnation: 0,
            point_default: Some("public".to_string()),
            visibility: None,
            id: VIEW.to_string(),
            display_name: VIEW.to_string(),
            quantisation: Quantisation {
                x_min: 0.0,
                x_max: 1.0,
                y_min: 0.0,
                y_max: 1.0,
            },
            projection: mosaica_spatial::Projection::None,
        }],
        partitions: vec![PartitionDescriptor {
            phash: PARTITION.to_string(),
            required_terms: vec![],
        }],
        provenance: serde_json::json!({}),
        files: Default::default(),
    }
}

/// Writes [`manifest`] as `v00000`'s `MANIFEST.json` and points `CURRENT` at it.
pub fn publish_manifest(root: &Path, created_at: &str) {
    let manifest_bytes = serde_json::to_vec_pretty(&manifest(created_at)).expect("serialise");
    fs::write(root.join("v00000/MANIFEST.json"), &manifest_bytes).expect("write MANIFEST");

    fs::write(
        root.join("CURRENT"),
        serde_json::to_vec_pretty(&CurrentPointer {
            prefix: "v00000".to_string(),
            manifest_digest: hex_sha256(&manifest_bytes),
        })
        .expect("serialise CURRENT"),
    )
    .expect("write CURRENT");
}

/// A flush segment covering `[entity_lo, entity_lo + count)`, loaded, with the extent that places
/// it at the end of `bundle`'s current row space.
///
/// **Goes through the real [`write_flush_segment`]** rather than hand-rolling the same files: a
/// second writer here would be a second definition of what a flush produces, and the publication
/// tests would then be asserting against a shape production never writes.
pub fn flush_segment(
    root: &Path,
    bundle: &Bundle,
    entity_lo: u64,
    count: u64,
) -> (SegmentData, SegmentExtent) {
    let row_base = bundle.only_shard().views[VIEW].row_space.total_rows() as u32;
    let seg_id = format!("seg-{entity_lo}-{count}");
    let key = IdentityKey::from_hex("0123456789abcdef0123456789abcdef").expect("test key");

    let out = write_flush_segment(
        &root.join("v00000"),
        SHARD,
        VIEW,
        FlushInput {
            incarnation: 0,
            seg_id: &seg_id,
            rows: (entity_lo..entity_lo + count)
                .map(|e| FlushRow {
                    entity_id: EntityId::new(e),
                    number: EntityId::new(e),
                    tenancy: mosaica_types::Tenancy::ZERO,
                    x: ((e * 37) % 100) as f64 / 100.0,
                    y: ((e * 61) % 100) as f64 / 100.0,
                    scalars: vec![],
                })
                .collect(),
            quantisation: Quantisation {
                x_min: 0.0,
                x_max: 1.0,
                y_min: 0.0,
                y_max: 1.0,
            },
            identity_key: &key,
            scalar_schema: &[],
            indexed: &[],
            row_base,
            entity_floor: 0,
        },
    )
    .expect("write_flush_segment");

    let seg_dir = root.join("v00000").join(SHARD.segment_rel(VIEW, &seg_id));
    let segment = SegmentData::load(
        &seg_dir,
        &seg_id,
        out.segment.row_count,
        mosaica_store::edited::RowEntities::Numbers,
    )
    .expect("the flushed segment opens");
    (segment, out.extent)
}

/// `bundle`'s side-manifest with `segments_version` advanced and `watermark` moved past the
/// `added` entities the caller is publishing. The `files` map is carried forward unchanged: these
/// tests exercise generation construction, not verification, which happened at `open_bundle`.
pub fn next_manifest(bundle: &Bundle, added: u64) -> SegmentsManifest {
    let mut manifest = bundle.partitions[PARTITION].side_manifest();
    let record = manifest.only_shard_mut();
    record.watermark += added;
    record.high_water += added;
    manifest
}

/// Publication `n` of `prefix_dir`'s partition as one JSON value, every file it names merged in:
/// the shape a test edits.
pub fn read_assembled(prefix_dir: &Path, n: u64) -> serde_json::Value {
    let published =
        mosaica_store::read_publication(prefix_dir, PARTITION, n).expect("the publication reads");
    serde_json::to_value(&published.manifest).expect("serialise")
}

/// Write `value`, an assembled publication, as publication `n` of `prefix_dir`'s partition: each
/// shard record and layer space with the files under its directory in its own `SEGMENTS-<n>.json`,
/// named by digest from the partition's, which is written under `name`. Files of those names are
/// replaced, and the value need not be one a writer would produce.
pub fn write_assembled_named(prefix_dir: &Path, name: &str, n: u64, mut value: serde_json::Value) {
    use serde_json::{json, Map, Value};
    let empty = serde_json::to_value(mosaica_store::manifest::EntitySet::default()).unwrap();
    let carries = |record: &Value| {
        ["deny", "tombstones"]
            .iter()
            .any(|field| record.get(*field).is_some_and(|set| *set != empty))
    };
    let mut files: Map<String, Value> =
        serde_json::from_value(value["files"].take()).unwrap_or_default();
    let write_owned = |dir_rel: String, mut record: Value, files: &mut Map<String, Value>| {
        let under = format!("{dir_rel}/");
        let owned: Map<String, Value> = files
            .iter()
            .filter(|(rel, _)| rel.starts_with(&under))
            .map(|(rel, digest)| (rel.clone(), digest.clone()))
            .collect();
        files.retain(|rel, _| !rel.starts_with(&under));
        record["files"] = Value::Object(owned);
        let bytes = serde_json::to_vec_pretty(&record).unwrap();
        let dir = prefix_dir.join(&dir_rel);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("SEGMENTS-{n}.json")), &bytes).unwrap();
        (bytes.len() as u64, hex_sha256(&bytes), carries(&record))
    };
    let records = value["shards"]["records"].take();
    let mut shard_refs = Vec::new();
    for record in records.as_array().cloned().unwrap_or_default() {
        let id = record["id"].as_u64().unwrap();
        let rel = format!("partitions/{PARTITION}/shards/{id}");
        let (size, sha256, denies) = write_owned(rel, record, &mut files);
        shard_refs
            .push(json!({"id": id, "n": n, "size": size, "sha256": sha256, "denies": denies}));
    }
    let spaces = value["layer_spaces"].take();
    let mut space_refs = Vec::new();
    for record in spaces.as_array().cloned().unwrap_or_default() {
        let space = record["space"].as_u64().unwrap();
        let rel = format!("partitions/{PARTITION}/layers/{space}");
        let (size, sha256, denies) = write_owned(rel, record, &mut files);
        space_refs.push(
            json!({"space": space, "n": n, "size": size, "sha256": sha256, "denies": denies}),
        );
    }
    value["files"] = Value::Object(files);
    value["shards"]["files"] = Value::Array(shard_refs);
    value["shards"].as_object_mut().unwrap().remove("records");
    value["layer_spaces"] = Value::Array(space_refs);
    let dir = mosaica_store::layout::partition_dir(prefix_dir, PARTITION);
    fs::write(dir.join(name), serde_json::to_vec_pretty(&value).unwrap()).unwrap();
}

/// [`write_assembled_named`] under the canonical name.
pub fn write_assembled(prefix_dir: &Path, n: u64, value: serde_json::Value) {
    write_assembled_named(prefix_dir, &format!("SEGMENTS-{n}.json"), n, value);
}
