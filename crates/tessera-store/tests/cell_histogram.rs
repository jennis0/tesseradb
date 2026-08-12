//! End-to-end test of the `cell_histogram` example (the B1 cell-occupancy probe — see the
//! example's doc comment): hand-assemble a tiny bundle whose Morton cell sizes are chosen in
//! advance, run the example binary against it, and assert the reported histogram and summary
//! statistics match what was constructed — including the bbox drilldown, whose region excludes
//! exactly one designed cell.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;

use sha2::{Digest, Sha256};

use tessera_spatial::tiler::{sort_batch, TilerItem};
use tessera_spatial::{fixed32, Bounds};
use tessera_store::manifest::{
    CurrentPointer, FileDigest, IdentityDescriptor, Manifest, PartitionDescriptor, Quantisation,
    SegmentDescriptor, SegmentsManifest, SliceDescriptor,
};
use tessera_store::write::{write_permutation, write_segment};
use tessera_types::{EntityId, TesseraId, IDENTITY_CONSTRUCTION, IDENTITY_ROUNDS};

/// A synthetic `tessera_id`-shaped value (splitmix64 over a seed) — same stand-in as
/// `bundle_read.rs`; the identity construction itself is not under test here.
fn synthetic_tessera_id(seed: u64) -> TesseraId {
    let mut z = seed.wrapping_add(0x9E3779B97F4A7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^= z >> 31;
    TesseraId::new(z)
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
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

/// The designed occupancy: `(cx, cy, rows)` — distinct 16-bit cells, so each entry is one
/// distinct Morton code carrying exactly `rows` rows. The first five sit in the depth-1 tile
/// (0, 0) (cells 0..=32767 on both axes); the 20-row cell sits in quadrant (1, 1) so a bbox
/// over the lower-left quadrant excludes it and nothing else.
const CELLS: [(u16, u16, u64); 6] = [
    (100, 100, 1),
    (200, 300, 2),
    (1000, 2000, 3),
    (5000, 5000, 5),
    (20000, 20000, 8),
    (40000, 40000, 20),
];

/// Build a one-partition, one-slice, one-segment bundle at `root` realising [`CELLS`] over the
/// unit extent. Point coordinates are cell centres, so quantisation cannot straddle a cell
/// boundary.
fn build_bundle(root: &Path) {
    let extent = Bounds {
        x_min: 0.0,
        x_max: 1.0,
        y_min: 0.0,
        y_max: 1.0,
    };
    let mut items: Vec<TilerItem> = Vec::new();
    for &(cx, cy, rows) in &CELLS {
        for _ in 0..rows {
            items.push(TilerItem {
                tessera_id: synthetic_tessera_id(items.len() as u64),
                qx: fixed32((f64::from(cx) + 0.5) / 65536.0, 0.0, 1.0),
                qy: fixed32((f64::from(cy) + 0.5) / 65536.0, 0.0, 1.0),
                scalars: vec![],
            });
        }
    }
    let n = items.len() as u64;
    let mut entity_ids: Vec<EntityId> = (0..n).map(EntityId::new).collect();
    let codes = sort_batch(&mut items, &mut entity_ids);

    let prefix_dir = root.join("v00000");
    let partition_dir = prefix_dir.join("partitions").join("default");
    let slice_dir = partition_dir.join("slices").join("main");
    let seg_dir = slice_dir.join("segments").join("seg0");
    fs::create_dir_all(&seg_dir).expect("mkdir seg_dir");

    write_segment(&seg_dir, &items, &codes, &[]).expect("write_segment");
    write_permutation(&slice_dir.join("permutation.bin"), &entity_ids, n)
        .expect("write_permutation");

    let mut segments_files = BTreeMap::new();
    for rel in [
        "partitions/default/slices/main/permutation.bin",
        "partitions/default/slices/main/segments/seg0/columns.arrow",
        "partitions/default/slices/main/segments/seg0/morton.u32",
    ] {
        segments_files.insert(rel.to_string(), file_digest(&prefix_dir.join(rel)));
    }

    let segments_manifest = SegmentsManifest {
        watermark: n,
        entity_id_high_water: n,
        segments: vec![SegmentDescriptor {
            slice: "main".to_string(),
            seg_id: "seg0".to_string(),
            row_count: items.len() as u32,
            entity_lo: 0,
            entity_hi: n - 1,
        }],
        deltas: vec![],
        dict_extents: vec![],
        attr_extents: Vec::new(),
        record_extents: Vec::new(),
        external_id_runs: vec![],
        locator_extents: vec![],
        tombstones: vec![],
        deny: vec![],
        vocabulary_extensions: Vec::new(),
        files: segments_files,
    };
    let segments_bytes = serde_json::to_vec_pretty(&segments_manifest).expect("serialise");
    fs::write(partition_dir.join("SEGMENTS-0.json"), &segments_bytes).expect("write SEGMENTS-0");

    let manifest = Manifest {
        bundle_format: 2,
        created_at: "2026-07-31T00:00:00Z".to_string(),
        data_plugin_hash: "builtin:passthrough:1".to_string(),
        declared_bounds: serde_json::json!({}),
        declared_scalars: vec![],
        vocabularies: vec![],
        small_term_threshold: 32,
        quantisation: Quantisation {
            x_min: extent.x_min,
            x_max: extent.x_max,
            y_min: extent.y_min,
            y_max: extent.y_max,
        },
        entity_id_high_water: n,
        identity: IdentityDescriptor {
            construction: IDENTITY_CONSTRUCTION.to_string(),
            rounds: IDENTITY_ROUNDS,
            key: "0123456789abcdef0123456789abcdef".to_string(),
            shard_id: 0,
            idset: 1,
        },
        slices: vec![SliceDescriptor {
            id: "main".to_string(),
            display_name: "Main".to_string(),
        }],
        partitions: vec![PartitionDescriptor {
            phash: "default".to_string(),
            required_terms: vec![],
        }],
        provenance: serde_json::json!({}),
        files: BTreeMap::new(),
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).expect("serialise");
    fs::write(prefix_dir.join("MANIFEST.json"), &manifest_bytes).expect("write MANIFEST.json");

    let current = CurrentPointer {
        prefix: "v00000".to_string(),
        manifest_digest: hex_sha256(&manifest_bytes),
    };
    fs::write(
        root.join("CURRENT"),
        serde_json::to_vec_pretty(&current).expect("serialise CURRENT"),
    )
    .expect("write CURRENT");
}

/// The non-empty buckets a histogram array should carry, as `(label, cells, rows)`.
fn nonzero_buckets(histogram: &serde_json::Value) -> Vec<(String, u64, u64)> {
    histogram
        .as_array()
        .expect("histogram is an array")
        .iter()
        .filter_map(|bucket| {
            let cells = bucket["cells"].as_u64().expect("cells");
            let rows = bucket["rows"].as_u64().expect("rows");
            (cells != 0 || rows != 0).then(|| {
                (
                    bucket["bucket"].as_str().expect("bucket label").to_string(),
                    cells,
                    rows,
                )
            })
        })
        .collect()
}

#[test]
fn reports_the_designed_histogram_globally_and_within_a_region() {
    let dir = tempfile::tempdir().expect("tempdir");
    build_bundle(dir.path());

    // Run the example binary itself — the deliverable is the tool, not a re-derivation of its
    // arithmetic in test code. Region args cover the lower-left quadrant at zoom 1.
    let output = Command::new(env!("CARGO"))
        .args([
            "run",
            "--quiet",
            "-p",
            "tessera-store",
            "--example",
            "cell_histogram",
            "--",
        ])
        .arg(dir.path())
        .args(["0", "0", "0.4", "0.4", "1"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run cell_histogram");
    assert!(
        output.status.success(),
        "cell_histogram failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout is one JSON object");

    assert_eq!(report["segment"], "seg0");
    assert_eq!(report["rows"], 39);

    // Global: all six designed cells. Sizes {1, 2, 3, 5, 8, 20} → buckets 1, 2, 3-4, 5-8
    // (5 and 8 share it), 17-32.
    let global = &report["global"];
    assert_eq!(global["rows"], 39);
    assert_eq!(global["distinct_codes"], 6);
    assert_eq!(global["mean_rows_per_cell"], 6.5);
    assert_eq!(global["median_rows_per_cell"], 3); // cumulative cells 1,2,3 ≥ ⌈6/2⌉ at size 3
    assert_eq!(global["rows_weighted_median_cell_size"], 20); // cumulative rows reach ⌈39/2⌉=20 only at size 20
    assert_eq!(global["max_cell_size"], 20);
    assert_eq!(
        nonzero_buckets(&global["histogram"]),
        vec![
            ("1".to_string(), 1, 1),
            ("2".to_string(), 1, 2),
            ("3-4".to_string(), 1, 3),
            ("5-8".to_string(), 2, 13),
            ("17-32".to_string(), 1, 20),
        ]
    );
    let ge = &global["rows_fraction_in_cells_ge"];
    assert_eq!(ge["8"], 28.0 / 39.0); // the 8- and 20-row cells
    assert_eq!(ge["32"], 0.0);
    assert_eq!(ge["128"], 0.0);
    assert_eq!(ge["1024"], 0.0);

    // Region: one depth-1 tile, holding every cell but the 20-row one.
    let region = &report["region"];
    assert_eq!(region["tiles"], 1);
    assert_eq!(region["tiles_occupied"], 1);
    assert_eq!(region["mean_occupied_cells_per_tile"], 5.0);
    assert_eq!(region["mean_rows_per_occupied_cell_per_tile"], 19.0 / 5.0);
    let occupancy = &region["occupancy"];
    assert_eq!(occupancy["rows"], 19);
    assert_eq!(occupancy["distinct_codes"], 5);
    assert_eq!(occupancy["median_rows_per_cell"], 3); // cumulative cells 1,2,3 ≥ ⌈5/2⌉ at size 3
    assert_eq!(occupancy["rows_weighted_median_cell_size"], 5); // cumulative rows 1,3,6,11 ≥ ⌈19/2⌉=10 at size 5
    assert_eq!(occupancy["max_cell_size"], 8);
    assert_eq!(
        nonzero_buckets(&occupancy["histogram"]),
        vec![
            ("1".to_string(), 1, 1),
            ("2".to_string(), 1, 2),
            ("3-4".to_string(), 1, 3),
            ("5-8".to_string(), 2, 13),
        ]
    );
    assert_eq!(occupancy["rows_fraction_in_cells_ge"]["8"], 8.0 / 19.0);
}
