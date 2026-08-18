//! A hand-assembled bundle, and the pieces a flush would publish into it.
//!
//! Separate from `bundle_read.rs`'s own copy because that file's fixture returns the tiler items
//! it built (its assertions compare against them) and hard-codes one segment. This one exists to
//! be *added to*.
//!
//! Each test binary compiles this module on its own, so a helper only one of them uses is dead
//! code in the others. That is what the allow below is for, and it is scoped to this fixture.
#![allow(dead_code)]

use tessera_plugin::Plugin;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use sha2::{Digest, Sha256};

use tessera_spatial::fixed32;
use tessera_spatial::tiler::{sort_batch, TilerItem};
use tessera_store::manifest::{
    CurrentPointer, FileDigest, IdentityDescriptor, Manifest, PartitionDescriptor, Quantisation,
    SegmentDescriptor, SegmentsManifest, ViewDescriptor,
};
use tessera_store::permutation::SegmentExtent;
use tessera_store::read::{ColumnsRef, MortonSlice, SegmentData};
use tessera_store::write::{write_permutation, write_segment};
use tessera_store::{write_flush_segment, Bundle, FlushInput, FlushRow};
use tessera_types::{EntityId, IdentityKey, TesseraId, IDENTITY_CONSTRUCTION, IDENTITY_ROUNDS};

pub const PARTITION: &str = "default";
pub const VIEW: &str = "main";

fn synthetic_tessera_id(seed: u64) -> TesseraId {
    let mut z = seed.wrapping_add(0x9E3779B97F4A7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    TesseraId::new(z ^ (z >> 31))
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn file_digest(path: &Path) -> FileDigest {
    let bytes = fs::read(path).expect("read for digest");
    FileDigest {
        size: bytes.len() as u64,
        sha256: hex_sha256(&bytes),
    }
}

fn items_for(entity_lo: u64, count: u64) -> Vec<TilerItem> {
    (entity_lo..entity_lo + count)
        .map(|e| TilerItem {
            tessera_id: synthetic_tessera_id(e),
            qx: fixed32(((e * 37) % 100) as f64 / 100.0, 0.0, 1.0),
            qy: fixed32(((e * 61) % 100) as f64 / 100.0, 0.0, 1.0),
            scalars: vec![],
        })
        .collect()
}

/// A one-segment bundle over entities `[0, n)`, at prefix `v00000`.
pub fn build_bundle(root: &Path, n: u64) {
    let mut items = items_for(0, n);
    let mut entity_ids: Vec<EntityId> = (0..n).map(EntityId::new).collect();
    let codes = sort_batch(&mut items, &mut entity_ids);

    let prefix_dir = root.join("v00000");
    let partition_dir = prefix_dir.join("partitions").join(PARTITION);
    let view_dir = partition_dir.join("views").join(VIEW);
    let seg_dir = view_dir.join("segments").join("seg0");
    fs::create_dir_all(&seg_dir).expect("mkdir");

    write_segment(&seg_dir, &items, &codes, &[]).expect("write_segment");
    write_permutation(&view_dir.join("permutation.bin"), &entity_ids, n).expect("permutation");

    let mut files = BTreeMap::new();
    for (rel, path) in [
        (
            format!("partitions/{PARTITION}/views/{VIEW}/permutation.bin"),
            view_dir.join("permutation.bin"),
        ),
        (
            format!("partitions/{PARTITION}/views/{VIEW}/segments/seg0/columns.arrow"),
            seg_dir.join("columns.arrow"),
        ),
        (
            format!("partitions/{PARTITION}/views/{VIEW}/segments/seg0/morton.u32"),
            seg_dir.join("morton.u32"),
        ),
    ] {
        files.insert(rel, file_digest(&path));
    }

    let segments_manifest = SegmentsManifest {
        watermark: n,
        entity_id_high_water: n,
        entity_id_low_water: tessera_types::layer::ROWLESS_CEILING,
        layers: Vec::new(),
        layer_tombstones: Vec::new(),
        membership_extents: Vec::new(),
        artifact_record_extents: Vec::new(),
        segments: vec![SegmentDescriptor {
            view: VIEW.to_string(),
            seg_id: "seg0".to_string(),
            row_count: n as u32,
            entity_lo: 0,
            entity_hi: n.saturating_sub(1),
        }],
        deltas: vec![],
        dict_extents: vec![],
        attr_extents: Vec::new(),
        record_extents: Vec::new(),
        text_extents: Vec::new(),
        external_id_runs: vec![],
        locator_extents: vec![],
        tombstones: vec![],
        deny: vec![],
        vocabulary_extensions: vec![],
        files,
    };
    fs::write(
        partition_dir.join("SEGMENTS-0.json"),
        serde_json::to_vec_pretty(&segments_manifest).expect("serialise"),
    )
    .expect("write SEGMENTS-0");

    let manifest = Manifest {
        bundle_format: 2,
        created_at: "2026-08-02T00:00:00Z".to_string(),
        data_plugin_hash: tessera_plugin::Passthrough::new().data_plugin_hash(),
        declared_bounds: serde_json::json!({}),
        declared_scalars: vec![],
        vocabularies: vec![],
        small_term_threshold: 32,
        quantisation: Quantisation {
            x_min: 0.0,
            x_max: 1.0,
            y_min: 0.0,
            y_max: 1.0,
        },
        entity_id_high_water: n,
        identity: IdentityDescriptor {
            construction: IDENTITY_CONSTRUCTION.to_string(),
            rounds: IDENTITY_ROUNDS,
            key: "0123456789abcdef0123456789abcdef".to_string(),
            shard_id: 0,
            idset: 1,
        },
        views: vec![ViewDescriptor {
            id: VIEW.to_string(),
            display_name: VIEW.to_string(),
        }],
        partitions: vec![PartitionDescriptor {
            phash: PARTITION.to_string(),
            required_terms: vec![],
        }],
        provenance: serde_json::json!({}),
        files: BTreeMap::new(),
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).expect("serialise");
    fs::write(prefix_dir.join("MANIFEST.json"), &manifest_bytes).expect("write MANIFEST");

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
    let row_base = bundle.partitions[PARTITION].views[VIEW]
        .row_space
        .total_rows() as u32;
    let seg_id = format!("seg-{entity_lo}-{count}");
    let key = IdentityKey::from_hex("0123456789abcdef0123456789abcdef").expect("test key");

    let out = write_flush_segment(
        &root.join("v00000"),
        PARTITION,
        VIEW,
        FlushInput {
            seg_id: &seg_id,
            rows: (entity_lo..entity_lo + count)
                .map(|e| FlushRow {
                    entity_id: EntityId::new(e),
                    external_id: Some(format!("ext-{e}").into_bytes()),
                    x: ((e * 37) % 100) as f32 / 100.0,
                    y: ((e * 61) % 100) as f32 / 100.0,
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
            shard_id: 0,
            scalar_schema: &[],
            row_base,
        },
    )
    .expect("write_flush_segment");

    let seg_dir = root
        .join("v00000/partitions")
        .join(PARTITION)
        .join("views")
        .join(VIEW)
        .join("segments")
        .join(&seg_id);
    let segment = SegmentData {
        seg_id,
        row_count: out.segment.row_count,
        morton: MortonSlice::load(&seg_dir.join("morton.u32")).expect("morton"),
        columns: ColumnsRef::load(&seg_dir.join("columns.arrow")).expect("columns"),
    };
    (segment, out.extent)
}

/// `bundle`'s side-manifest with `segments_version` advanced and `watermark` moved past the
/// `added` entities the caller is publishing. The `files` map is carried forward unchanged: these
/// tests exercise generation construction, not verification, which happened at `open_bundle`.
pub fn next_manifest(bundle: &Bundle, added: u64) -> SegmentsManifest {
    let mut manifest = bundle.partitions[PARTITION].manifest.clone();
    manifest.watermark += added;
    manifest.entity_id_high_water += added;
    manifest
}
