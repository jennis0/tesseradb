//! Task 7, Step 1: hand-assemble a tiny bundle in a tempdir (real digests, computed with
//! `sha2`, the same crate the reader uses) and exercise the full read protocol end to end:
//! `open_bundle`, `ColumnsRef` accessors, `Permutation::project` against a per-entity
//! `row_of` loop, and `tile_ranges` against a linear scan of the morton array. Then corrupt
//! one byte of `columns.arrow` and confirm `open_bundle` fails closed with a digest error.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use croaring::Bitmap;
use sha2::{Digest, Sha256};

use tessera_spatial::tiler::{sort_batch, TilerItem};
use tessera_spatial::{tiles_for_bbox, Extent, Tile};
use tessera_store::manifest::{
    CurrentPointer, FileDigest, IdentityDescriptor, Manifest, PartitionDescriptor, Quantisation,
    SegmentDescriptor, SegmentsManifest, SliceDescriptor,
};
use tessera_store::write::{write_permutation, write_segment};
use tessera_store::{open_bundle, tile_ranges, StoreError};
use tessera_types::{EntityId, TesseraId, IDENTITY_CONSTRUCTION, IDENTITY_ROUNDS};

/// A synthetic `tessera_id`-shaped value for test fixtures: full splitmix64 output over a
/// seed, so its top 16 bits are a `priority` prefix like any real `tessera_id` (contracts
/// §2.6), without claiming this is the actual Feistel construction (Task 5's own tests cover
/// that separately).
fn synthetic_tessera_id(seed: u64) -> TesseraId {
    let mut z = seed.wrapping_add(0x9E3779B97F4A7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^= z >> 31;
    TesseraId::new(z)
}

fn unit_extent() -> Extent {
    Extent {
        x_min: 0.0,
        x_max: 1.0,
        y_min: 0.0,
        y_max: 1.0,
    }
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

/// Build a tiny bundle at `root` with one partition ("default"), one slice ("main"), one
/// segment ("seg0"), `n` entities `0..n`. Returns the sorted items (row order) and their
/// morton codes, so the test can independently recompute expectations.
fn build_bundle(root: &Path, n: u64) -> (Vec<TilerItem>, Vec<u32>) {
    let extent = unit_extent();
    let mut items: Vec<TilerItem> = (0..n)
        .map(|entity_id| TilerItem {
            tessera_id: synthetic_tessera_id(entity_id),
            // Spread points across the grid deterministically so tiles split them up.
            x: ((entity_id * 37) % 100) as f32 / 100.0,
            y: ((entity_id * 61) % 100) as f32 / 100.0,
            scalars: vec![],
        })
        .collect();
    let mut entity_ids: Vec<EntityId> = (0..n).map(EntityId::new).collect();
    let codes = sort_batch(&mut items, &mut entity_ids, &extent);

    let prefix_dir = root.join("v00000");
    let partition_dir = prefix_dir.join("partitions").join("default");
    let slice_dir = partition_dir.join("slices").join("main");
    let seg_dir = slice_dir.join("segments").join("seg0");
    fs::create_dir_all(&seg_dir).expect("mkdir seg_dir");

    write_segment(&seg_dir, &items, &codes, &[]).expect("write_segment");

    let row_order_entities: Vec<EntityId> = entity_ids.clone();
    let bound = n;
    write_permutation(
        &slice_dir.join("permutation.bin"),
        &row_order_entities,
        bound,
    )
    .expect("write_permutation");

    // SEGMENTS-0.json's `files` map: prefix-relative paths (R1) for every file this partition
    // added since MANIFEST.
    let mut segments_files = BTreeMap::new();
    segments_files.insert(
        "partitions/default/slices/main/permutation.bin".to_string(),
        file_digest(&slice_dir.join("permutation.bin")),
    );
    segments_files.insert(
        "partitions/default/slices/main/segments/seg0/columns.arrow".to_string(),
        file_digest(&seg_dir.join("columns.arrow")),
    );
    segments_files.insert(
        "partitions/default/slices/main/segments/seg0/morton.u32".to_string(),
        file_digest(&seg_dir.join("morton.u32")),
    );

    let segments_manifest = SegmentsManifest {
        segments_version: 0,
        watermark: n,
        entity_id_high_water: n,
        segments: vec![SegmentDescriptor {
            slice: "main".to_string(),
            seg_id: "seg0".to_string(),
            row_count: items.len() as u32,
            entity_lo: 0,
            entity_hi: n.saturating_sub(1),
        }],
        deltas: vec![],
        dict_extents: vec![],
        external_id_extents: vec![],
        tombstones: vec![],
        deny: vec![],
        files: segments_files,
    };
    let segments_bytes = serde_json::to_vec_pretty(&segments_manifest).expect("serialise");
    fs::write(partition_dir.join("SEGMENTS-0.json"), &segments_bytes).expect("write SEGMENTS-0");

    let manifest = Manifest {
        bundle_format: 1,
        created_at: "2026-07-28T00:00:00Z".to_string(),
        data_plugin_hash: "builtin:passthrough:1".to_string(),
        declared_bounds: serde_json::json!({}),
        declared_scalars: vec![],
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
            epoch: 1,
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

    (items, codes)
}

#[test]
fn open_bundle_loads_segments_and_columns_round_trip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (items, codes) = build_bundle(dir.path(), 200);

    let bundle = open_bundle(dir.path()).expect("open_bundle");
    assert_eq!(bundle.manifest.bundle_format, 1);

    let partition = bundle.partitions.get("default").expect("default partition");
    assert_eq!(partition.manifest.segments_version, 0);

    let slice = partition.slices.get("main").expect("main slice");
    assert_eq!(slice.segments.len(), 1);
    let seg = &slice.segments[0];
    assert_eq!(seg.seg_id, "seg0");
    assert_eq!(seg.row_count, items.len() as u32);

    // columns.arrow round-trips row 0..n exactly.
    let tessera_id_col = seg.columns.tessera_id();
    let x_col = seg.columns.x();
    let y_col = seg.columns.y();
    let priority_col = seg.columns.priority();
    assert_eq!(tessera_id_col.len(), items.len());
    for (i, item) in items.iter().enumerate() {
        assert_eq!(tessera_id_col[i], item.tessera_id.raw());
        assert_eq!(x_col[i], item.x);
        assert_eq!(y_col[i], item.y);
        assert_eq!(priority_col[i], item.tessera_id.priority());
    }

    // morton.u32 round-trips exactly what sort_batch computed.
    assert_eq!(seg.morton.u32(), codes.as_slice());
}

#[test]
fn permutation_project_matches_per_entity_row_of_loop() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (items, _codes) = build_bundle(dir.path(), 500);

    let bundle = open_bundle(dir.path()).expect("open_bundle");
    let slice = &bundle.partitions["default"].slices["main"];

    // A mask over roughly half the entity space, including a few IDs outside [0, n) — those
    // must be silently skipped (no row here), not treated as an error.
    let mut mask = Bitmap::new();
    for e in (0..items.len() as u32).step_by(3) {
        mask.add(e);
    }
    mask.add(10_000); // out of bound: must be dropped, not panic

    let projected = slice.permutation.project(&mask);

    // Independently recompute the expected row set via `row_of`, one entity at a time.
    let mut expected_rows: Vec<u32> = Vec::new();
    for e in mask.iter() {
        if let Some(row) = slice.permutation.row_of(EntityId::new(e as u64)) {
            expected_rows.push(row.raw());
        }
    }
    expected_rows.sort_unstable();

    let projected_rows: Vec<u32> = projected.iter().collect();
    assert_eq!(projected_rows, expected_rows);
    assert!(
        !projected_rows.is_empty(),
        "sanity: the mask should have produced at least one row"
    );
}

#[test]
fn tile_ranges_agrees_with_linear_scan_of_morton_array() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (_items, codes) = build_bundle(dir.path(), 300);

    let bundle = open_bundle(dir.path()).expect("open_bundle");
    let seg = &bundle.partitions["default"].slices["main"].segments[0];

    // A handful of tiles at different depths, including the whole-grid tile (depth 0).
    for tile in [
        Tile {
            prefix: 0,
            depth: 0,
        },
        Tile {
            prefix: 0,
            depth: 2,
        },
        Tile {
            prefix: 1,
            depth: 2,
        },
        Tile {
            prefix: 3,
            depth: 2,
        },
        Tile {
            prefix: (1u64 << 8) - 1,
            depth: 4,
        },
    ] {
        let (lo, hi) = tile.code_range();
        let expected_start = codes
            .iter()
            .position(|&c| c as u64 >= lo)
            .unwrap_or(codes.len());
        let expected_end = codes
            .iter()
            .position(|&c| c as u64 >= hi)
            .unwrap_or(codes.len());

        let range = tile_ranges(seg, &tile);
        assert_eq!(range.start as usize, expected_start, "tile {tile:?} start");
        assert_eq!(range.end as usize, expected_end, "tile {tile:?} end");
    }
}

#[test]
fn tile_ranges_covers_the_whole_segment_at_depth_zero() {
    // Depth 0's exclusive code-range end is `1 << 32`, which does not fit in u32. This test
    // exists because narrowing `Tile::code_range` alongside the stored column would overflow
    // here and silently return an empty range in release builds — the codes are widened for
    // the comparison instead (contracts §2.5, r5).
    let dir = tempfile::tempdir().expect("tempdir");
    let (_items, codes) = build_bundle(dir.path(), 300);

    let bundle = open_bundle(dir.path()).expect("open_bundle");
    let seg = &bundle.partitions["default"].slices["main"].segments[0];

    let whole = tile_ranges(
        seg,
        &Tile {
            prefix: 0,
            depth: 0,
        },
    );
    assert_eq!(
        whole,
        0u32..codes.len() as u32,
        "depth 0 must select every row in the segment"
    );
}

/// Read `partitions/default/SEGMENTS-0.json` under `root`'s bundle prefix, apply `edit` to its
/// parsed JSON, and rewrite it — the standard way these tests attack the manifest layer without
/// touching the real segment files or breaking the `MANIFEST.json` digest chain (SEGMENTS
/// manifests carry no digest of their own; only the `files` entries *inside* them are
/// verified).
fn edit_segments_manifest(root: &Path, edit: impl FnOnce(&mut serde_json::Value)) {
    let path = root.join("v00000/partitions/default/SEGMENTS-0.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).expect("read SEGMENTS-0.json"))
            .expect("parse SEGMENTS-0.json");
    edit(&mut value);
    fs::write(&path, serde_json::to_vec_pretty(&value).expect("serialise")).expect("rewrite");
}

#[test]
fn open_bundle_fails_closed_on_a_corrupted_columns_arrow_byte() {
    let dir = tempfile::tempdir().expect("tempdir");
    build_bundle(dir.path(), 50);

    let columns_path = dir
        .path()
        .join("v00000/partitions/default/slices/main/segments/seg0/columns.arrow");
    let mut bytes = fs::read(&columns_path).expect("read columns.arrow");
    // Flip a byte roughly in the middle of the file — inside the record batch body, not the
    // footer/magic, so this is a content corruption a naive length-only check would miss.
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xFF;
    fs::write(&columns_path, &bytes).expect("rewrite corrupted columns.arrow");

    let err = open_bundle(dir.path()).expect_err("corrupted columns.arrow must fail open_bundle");
    // With only one SEGMENTS-<n>.json candidate (n=0), the outer error is always
    // `NoVerifyingSegmentsManifest` — but it must carry the *specific* reason the candidate
    // failed, not silently swallow it (the bug this replaces: `FileVerificationFailed` was
    // previously unreachable from `open_bundle`, since the step-down loop discarded every
    // per-candidate error).
    match err {
        StoreError::NoVerifyingSegmentsManifest {
            last_error: Some(reason),
            ..
        } => {
            assert!(
                reason.contains("columns.arrow"),
                "reason should name columns.arrow, got: {reason}"
            );
            assert!(
                reason.to_ascii_lowercase().contains("sha-256")
                    || reason.to_ascii_lowercase().contains("digest"),
                "reason should describe a digest mismatch, got: {reason}"
            );
        }
        other => panic!(
            "expected NoVerifyingSegmentsManifest with a digest-mismatch reason, got: {other}"
        ),
    }
}

#[test]
fn open_bundle_rejects_a_segments_manifest_that_omits_columns_arrow_from_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    build_bundle(dir.path(), 40);

    // The segment files on disk are all still byte-for-byte correct — only the SEGMENTS-0.json
    // `files` map is edited to drop the `columns.arrow` entry. `verify_files` alone would treat
    // this SEGMENTS manifest as fully verifying (there's nothing left in `files` that doesn't
    // check out) — that vacuous-verification gap is exactly what `ensure_verified` at the
    // loader call sites must close.
    edit_segments_manifest(dir.path(), |value| {
        value["files"]
            .as_object_mut()
            .expect("files is an object")
            .remove("partitions/default/slices/main/segments/seg0/columns.arrow");
    });

    let err = open_bundle(dir.path())
        .expect_err("a SEGMENTS manifest omitting columns.arrow from files must be rejected");
    match err {
        StoreError::UnverifiedFile { path } => {
            assert!(
                path.ends_with("columns.arrow"),
                "expected the unverified path to be columns.arrow, got: {}",
                path.display()
            );
        }
        other => panic!("expected UnverifiedFile, got: {other}"),
    }
}

#[test]
fn open_bundle_rejects_a_permutation_slot_pointing_past_row_count() {
    let dir = tempfile::tempdir().expect("tempdir");
    build_bundle(dir.path(), 60);

    let perm_path = dir
        .path()
        .join("v00000/partitions/default/slices/main/permutation.bin");
    let mut bytes = fs::read(&perm_path).expect("read permutation.bin");

    // Header is 16 bytes (magic + version + reserved + bound); slot 0 starts right after.
    // Overwrite it with a row index far past this segment's row_count (60) — still a
    // structurally valid `u32`, not the sentinel, just out of range.
    let corrupt_slot = 9_999u32.to_le_bytes();
    bytes[16..20].copy_from_slice(&corrupt_slot);
    fs::write(&perm_path, &bytes).expect("rewrite corrupted permutation.bin");

    // Recompute the digest so this reaches content validation (`Permutation::validate_rows`)
    // rather than being caught earlier by the plain size+SHA-256 check — the two are different
    // defences (one catches bit-flips, the other catches internally-consistent-but-wrong data).
    let corrected = file_digest(&perm_path);
    edit_segments_manifest(dir.path(), |value| {
        let entry = &mut value["files"]["partitions/default/slices/main/permutation.bin"];
        entry["size"] = serde_json::json!(corrected.size);
        entry["sha256"] = serde_json::json!(corrected.sha256);
    });

    let err =
        open_bundle(dir.path()).expect_err("a permutation slot past row_count must be rejected");
    match err {
        StoreError::InvalidPermutation { detail, .. } => {
            assert!(
                detail.contains("out of bound"),
                "expected an out-of-bound detail, got: {detail}"
            );
        }
        other => panic!("expected InvalidPermutation, got: {other}"),
    }
}

#[test]
fn open_bundle_rejects_a_path_traversing_segment_id() {
    let dir = tempfile::tempdir().expect("tempdir");
    build_bundle(dir.path(), 20);

    // The real `seg0` files are untouched; only the SEGMENTS-0.json entry that names them is
    // edited to claim a traversal-shaped seg_id. This must be rejected before any path is ever
    // joined onto the bundle root and opened — a digest-verified manifest is safe from content
    // tampering, not from carrying unsafe *values*.
    edit_segments_manifest(dir.path(), |value| {
        value["segments"][0]["seg_id"] = serde_json::json!("../../evil");
    });

    let err =
        open_bundle(dir.path()).expect_err("a path-traversing seg_id must be rejected before use");
    match err {
        StoreError::UnsafePath { value, .. } => {
            assert_eq!(value, "../../evil");
        }
        other => panic!("expected UnsafePath, got: {other}"),
    }
}

// -------------------------------------------------------------------------------------------
// `tile_ranges_all` equivalence.
//
// `tile_ranges_all` replaces N per-tile full-column binary searches with one monotone galloping
// sweep in Morton order. Its entire safety argument is that it computes the *same answer* as
// `tile_ranges` at every index — so nothing below re-derives an expected range from a model of
// the sweep. Everything compares against `tile_ranges` itself, which is untouched and remains
// the definition. (The gallop primitive underneath has its own unit test against
// `partition_point`, in `tessera_store::read`.)
//
// Two things a plausible-looking sweep gets wrong, both tested for here:
//
//   1. Returning results in Morton order. `tiles_for_bbox` enumerates in `(ty outer, tx inner)`
//      raster order, which is NOT Morton order — and the response's tile order is load-bearing
//      (see `tile_ranges_all`'s doc). Every assertion below is positional.
//   2. Assuming the tile set is shaped the way `tiles_for_bbox` happens to shape it: one depth,
//      unique, disjoint, ascending. The function is public, so the arbitrary-tile-set test
//      feeds it mixed depths, duplicates and shuffled orders.
// -------------------------------------------------------------------------------------------

/// `out[i] == tile_ranges(seg, &tiles[i])` for every `i`, or a failure naming the index.
fn assert_matches_per_tile_search(seg: &tessera_store::SegmentData, tiles: &[Tile], what: &str) {
    let swept = tessera_store::tile_ranges_all(seg, tiles);
    assert_eq!(swept.len(), tiles.len(), "{what}: one range per tile");
    for (i, tile) in tiles.iter().enumerate() {
        assert_eq!(
            swept[i],
            tile_ranges(seg, tile),
            "{what}: tile {i} ({tile:?}) — the sweep must agree with the full-column search \
             AT ITS OWN INDEX"
        );
    }
}

/// A random depth and an in-range prefix for it.
fn random_tile(rng: &mut impl rand::Rng) -> Tile {
    let depth: u8 = rng.gen_range(0..=16);
    let prefix = if depth == 0 {
        0
    } else {
        rng.gen::<u64>() & ((1u64 << (2 * depth as u32)) - 1)
    };
    Tile { prefix, depth }
}

#[test]
fn tile_ranges_all_agrees_with_the_full_column_search_over_depths_bboxes_and_orders() {
    let dir = tempfile::tempdir().expect("tempdir");
    // 5,000 items over `build_bundle`'s 100x100 position grid, so every Morton code is repeated
    // ~50 times — which is exactly where a sweep that floors the next search at the previous
    // tile's *end* rather than its *start* begins to lie.
    let (_items, _codes) = build_bundle(dir.path(), 5_000);
    let bundle = open_bundle(dir.path()).expect("open_bundle");
    let seg = &bundle.partitions["default"].slices["main"].segments[0];
    let extent = unit_extent();

    // Every bbox shape a viewport request can produce a tile set from, including the ones a
    // hand-written sweep tends to skip: corners given in reverse, a degenerate single-point box,
    // the full extent, and boxes that clamp at an edge.
    let bboxes: [(&str, [f64; 4]); 7] = [
        ("interior", [0.10, 0.20, 0.40, 0.55]),
        ("reversed corners", [0.40, 0.55, 0.10, 0.20]),
        ("degenerate point", [0.33, 0.33, 0.33, 0.33]),
        ("full extent", [0.0, 0.0, 1.0, 1.0]),
        ("clamped below", [-5.0, -5.0, 0.05, 0.05]),
        ("clamped above", [0.95, 0.95, 5.0, 5.0]),
        ("straddling the quadrant split", [0.45, 0.45, 0.55, 0.55]),
    ];

    for (label, bbox) in bboxes {
        // Depth is capped at 6 because the full-extent bbox enumerates the *whole* grid at that
        // depth (4^6 = 4,096 tiles, x3 orderings, x7 bboxes); the property under test is
        // depth-independent, and `tile_ranges_all_agrees_..._for_arbitrary_tile_sets` below
        // reaches depth 16 directly.
        for depth in 0u8..=6 {
            let tiles = tiles_for_bbox(bbox, depth, &extent);
            assert_matches_per_tile_search(seg, &tiles, &format!("{label} d{depth} as-enumerated"));

            // Reversed: an implementation that leaked its own sweep order into the result can
            // still coincide with raster order on some inputs, but not on this one.
            let mut reversed = tiles.clone();
            reversed.reverse();
            assert_matches_per_tile_search(seg, &reversed, &format!("{label} d{depth} reversed"));

            // Every tile twice, adjacent: a repeated code range must resolve identically both
            // times.
            let doubled: Vec<Tile> = tiles.iter().flat_map(|t| [*t, *t]).collect();
            assert_matches_per_tile_search(seg, &doubled, &format!("{label} d{depth} doubled"));
        }
    }
}

#[test]
fn tile_ranges_all_agrees_with_the_full_column_search_for_arbitrary_tile_sets() {
    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};

    let dir = tempfile::tempdir().expect("tempdir");
    let (_items, _codes) = build_bundle(dir.path(), 5_000);
    let bundle = open_bundle(dir.path()).expect("open_bundle");
    let seg = &bundle.partitions["default"].slices["main"].segments[0];

    // Randomised rather than `proptest`-generated so the bundle fixture (tempdir, Arrow write,
    // real SHA-256 digests) is built once instead of once per generated case. The seed is fixed,
    // so any failure reproduces by running this test again.
    let mut rng = StdRng::seed_from_u64(0x7E55E4A);
    for case in 0..2_000u32 {
        let n = rng.gen_range(0..24usize);
        // Mixed depths in one set — the case where ordering the sweep by `prefix` instead of by
        // the code-range low bound gives a wrong visit order and a truncated range.
        let tiles: Vec<Tile> = (0..n).map(|_| random_tile(&mut rng)).collect();
        assert_matches_per_tile_search(seg, &tiles, &format!("random case {case}"));
    }
}

#[test]
fn tile_ranges_all_over_an_empty_tile_set_is_empty() {
    let dir = tempfile::tempdir().expect("tempdir");
    build_bundle(dir.path(), 64);
    let bundle = open_bundle(dir.path()).expect("open_bundle");
    let seg = &bundle.partitions["default"].slices["main"].segments[0];
    assert!(tessera_store::tile_ranges_all(seg, &[]).is_empty());
}
