//! The load-bearing test for the streaming build: it must produce **byte-identical** bundles to
//! the linear reference build, and must be deterministic run to run.
//!
//! Entity IDs are assigned by signature order and are permanent under I9 — the ordering chosen
//! at the first build is the ordering the corpus keeps forever. A streaming build that ordered
//! items even slightly differently would not be a faster build, it would be a *different
//! corpus*, and every posting, permutation and handle derived from it would be invalid. Byte
//! equality against `build_in_memory` is how that is proved, and it covers the derived files
//! too: the dictionary's term-id assignment, the postings' tag choice and Roaring encoding, the
//! `(term, entity)` order in `pairs.parquet`, the external-ids byte sort, the tiler's
//! `(morton, priority, entity)` order and the permutation.
//!
//! Two files are compared after normalisation rather than raw: `MANIFEST.json` carries a
//! wall-clock `created_at`, and `CURRENT` carries that manifest's digest. Everything else,
//! including every digest `MANIFEST.json` records for every other file, is compared verbatim —
//! so a single differing byte anywhere in the bundle still fails here.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{Float64Array, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use tessera_build::{build, build_in_memory, BuildArgs};
use tessera_spatial::Extent;
use tessera_types::IdentityKey;

const N_ITEMS: u64 = 4_000;
const N_TERMS: u64 = 61;

/// A fixed, non-degenerate test key shared by every fixture in this file — never a mint, since
/// the tests need a stable, reproducible identity to assert byte equality against.
const TEST_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";

fn test_key() -> IdentityKey {
    IdentityKey::from_hex(TEST_KEY_HEX).unwrap()
}

fn extent() -> Extent {
    Extent {
        x_min: -20.0,
        x_max: 980.0,
        y_min: 0.0,
        y_max: 1000.0,
    }
}

/// Deterministic synthetic input, shaped to exercise every branch the two builds could disagree
/// on: items with no terms at all, many items sharing an identical signature, signatures that
/// agree on their first two terms but differ later (the streaming build's pre-sort key ties),
/// signatures longer than two terms, duplicate `(entity, term)` rows, and terms whose first
/// appearance is late in entity order (which is what fixes their term id).
fn synth_terms(e: u64) -> Vec<u64> {
    match e % 13 {
        0 => vec![],
        1 => vec![7],
        2 => vec![7, 11],
        3 => vec![11, 7], // same set, different input order
        4 => vec![7, 11, 13],
        5 => vec![7, 11, 13, 17],
        6 => vec![7, 11, 19],
        7 => vec![7, 11, 13, 17, 19, 23, 29],
        8 => vec![(e / 97) % N_TERMS],
        9 => vec![41, 43],
        10 => vec![41, 43, (e / 31) % N_TERMS],
        11 => vec![N_TERMS - 1], // first appears late, so its term id is late
        _ => {
            let mut t = vec![e % 5, (e * 7) % N_TERMS, (e / 11) % N_TERMS];
            t.sort_unstable();
            t.dedup();
            t
        }
    }
}

fn write_points(path: &Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
    ]));
    // Source ids are deliberately neither dense nor in file order: the build must not depend on
    // either, and the ordinal space it derives has to be the sorted one.
    let ids: Vec<u64> = (0..N_ITEMS).map(|e| (e * 7919) % 1_000_003).collect();
    // Repeated coordinates on purpose, so the tiler's priority tiebreak is exercised.
    let xs: Vec<f64> = ids.iter().map(|e| ((e % 40) * 25) as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|e| ((e % 37) * 27) as f64).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn write_pairs(path: &Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("term_id", DataType::UInt32, false),
    ]));
    let mut entities = Vec::new();
    let mut terms = Vec::new();
    for e in 0..N_ITEMS {
        let source_id = (e * 7919) % 1_000_003;
        for t in synth_terms(e) {
            entities.push(source_id);
            terms.push(t as u32);
            // Every twelfth item repeats its rows: the label set is a *set*, and a repeated
            // input row must not become a repeated posting in either build.
            if e % 12 == 0 {
                entities.push(source_id);
                terms.push(t as u32);
            }
        }
    }
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(entities)),
            Arc::new(UInt32Array::from(terms)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn args_for(points: &Path, pairs: &Path, out: PathBuf) -> BuildArgs {
    BuildArgs {
        points: points.to_path_buf(),
        pairs: pairs.to_path_buf(),
        out,
        extent: extent(),
        slice_id: "s0".to_string(),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        mint_external_ids: true,
        emit_oracle_pairs: true,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
    }
}

/// Every file under `root`, keyed by its `root`-relative slash-separated path.
fn collect(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                let rel = path
                    .strip_prefix(root)
                    .unwrap()
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                out.insert(rel, std::fs::read(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// Compare two bundles file for file. `MANIFEST.json` is compared with its wall-clock
/// `created_at` blanked, and `CURRENT` (which is nothing but that manifest's digest) with it.
/// Every other file, including all the digests `MANIFEST.json` records, is compared verbatim.
fn assert_bundles_identical(left: &Path, right: &Path, what: &str) {
    let a = collect(left);
    let b = collect(right);
    assert_eq!(
        a.keys().collect::<Vec<_>>(),
        b.keys().collect::<Vec<_>>(),
        "{what}: the two bundles do not contain the same files"
    );
    for (name, left_bytes) in &a {
        let right_bytes = &b[name];
        if name == "v00000/MANIFEST.json" {
            let normalise = |bytes: &[u8]| {
                let mut value: serde_json::Value = serde_json::from_slice(bytes).unwrap();
                value["created_at"] = serde_json::Value::Null;
                value
            };
            assert_eq!(
                normalise(left_bytes),
                normalise(right_bytes),
                "{what}: MANIFEST.json differs (ignoring created_at)"
            );
            continue;
        }
        if name == "CURRENT" {
            continue; // the digest of a manifest that legitimately differs in created_at only
        }
        assert_eq!(
            left_bytes,
            right_bytes,
            "{what}: {name} is not byte-identical ({} vs {} bytes)",
            left_bytes.len(),
            right_bytes.len()
        );
    }
    // A bundle that only contained CURRENT and MANIFEST would pass the loop vacuously.
    assert!(
        a.len() > 6,
        "{what}: expected a full bundle, found {} files",
        a.len()
    );
}

#[test]
fn streaming_build_is_byte_identical_to_the_reference_build() {
    let temp = tempfile::TempDir::new().unwrap();
    let points = temp.path().join("points.parquet");
    let pairs = temp.path().join("pairs.parquet");
    write_points(&points);
    write_pairs(&pairs);

    let reference_out = temp.path().join("reference");
    let streaming_out = temp.path().join("streaming");
    let reference = build_in_memory(&args_for(&points, &pairs, reference_out.clone())).unwrap();
    let streaming = build(&args_for(&points, &pairs, streaming_out.clone())).unwrap();

    assert_eq!(reference.items, streaming.items);
    assert_eq!(reference.terms, streaming.terms);
    assert_eq!(reference.pairs, streaming.pairs);
    assert_eq!(reference.bundle_bytes, streaming.bundle_bytes);
    assert_bundles_identical(&reference_out, &streaming_out, "streaming vs reference");
}

#[test]
fn streaming_build_is_deterministic() {
    let temp = tempfile::TempDir::new().unwrap();
    let points = temp.path().join("points.parquet");
    let pairs = temp.path().join("pairs.parquet");
    write_points(&points);
    write_pairs(&pairs);

    let first = temp.path().join("first");
    let second = temp.path().join("second");
    build(&args_for(&points, &pairs, first.clone())).unwrap();
    build(&args_for(&points, &pairs, second.clone())).unwrap();
    assert_bundles_identical(&first, &second, "run 1 vs run 2");
}

/// Signatures engineered around the streaming build's tie-group refinement, covering the group
/// shapes `synth_terms` only produces incidentally:
///
/// * an **all-long group** — prefix (1,2) is carried only by >2-term signatures, so its group
///   has no short member at all (the short/long partition must be a no-op at the front);
/// * long tails that are **prefixes of one another** ((1,2,3) vs (1,2,3,4) vs (1,2,3,4,5));
/// * **equal full signatures** among longs (the ordinal tiebreak inside the tail sort);
/// * a **mixed group** (5,6) with shorts and longs interleaved in ordinal order;
/// * a shorts-only group, a single-term group with several members, empty signatures, and an
///   item whose input rows arrive unsorted and duplicated.
fn shape_terms(e: u64) -> Vec<u64> {
    match e % 11 {
        0 => vec![1, 2, 3 + (e % 7)],
        1 => vec![1, 2, 3],
        2 => vec![1, 2, 3, 4],
        3 => vec![1, 2, 3, 4, 5],
        4 => vec![1, 2, 9, 10],
        5 => vec![5, 6],
        6 => vec![5, 6, 7 + (e % 5)],
        7 => vec![8, 9],
        8 => vec![11],
        9 => vec![],
        _ => vec![12, 3, 12], // unsorted, with a repeated term in the input rows
    }
}

const N_SHAPE_ITEMS: u64 = 3_300;

fn shape_source_id(e: u64) -> u64 {
    // Sparse, non-monotonic, arbitrary-looking: nothing about the ids' shape may matter.
    (e * 104_729) % 2_000_003
}

fn write_shape_points(path: &Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
    ]));
    let ids: Vec<u64> = (0..N_SHAPE_ITEMS).map(shape_source_id).collect();
    let xs: Vec<f64> = ids.iter().map(|e| ((e % 40) * 25) as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|e| ((e % 37) * 27) as f64).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn write_shape_pairs(path: &Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("term_id", DataType::UInt32, false),
    ]));
    let mut entities = Vec::new();
    let mut terms = Vec::new();
    for e in 0..N_SHAPE_ITEMS {
        for t in shape_terms(e) {
            entities.push(shape_source_id(e));
            terms.push(t as u32);
        }
    }
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(entities)),
            Arc::new(UInt32Array::from(terms)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

#[test]
fn tie_group_shapes_are_byte_identical_to_the_reference_build() {
    let temp = tempfile::TempDir::new().unwrap();
    let points = temp.path().join("points.parquet");
    let pairs = temp.path().join("pairs.parquet");
    write_shape_points(&points);
    write_shape_pairs(&pairs);

    let reference_out = temp.path().join("reference");
    let streaming_out = temp.path().join("streaming");
    build_in_memory(&args_for(&points, &pairs, reference_out.clone())).unwrap();
    build(&args_for(&points, &pairs, streaming_out.clone())).unwrap();
    assert_bundles_identical(&reference_out, &streaming_out, "tie-group shapes");
}

/// The spec-conformant default build — no minted external IDs (contracts §2.4), and here also
/// no oracle `pairs.parquet` — must hold the same byte-identity between the two
/// implementations, produce a bundle with no sidecar or pairs files at all, and still pass
/// `verify` (MANIFEST lists only what was written, so nothing is unverifiable).
#[test]
fn conformant_no_mint_no_pairs_build_is_byte_identical_and_verifiable() {
    let temp = tempfile::TempDir::new().unwrap();
    let points = temp.path().join("points.parquet");
    let pairs = temp.path().join("pairs.parquet");
    write_points(&points);
    write_pairs(&pairs);

    let make_args = |out: PathBuf| {
        let mut args = args_for(&points, &pairs, out);
        args.mint_external_ids = false;
        args.emit_oracle_pairs = false;
        args
    };

    let reference_out = temp.path().join("reference");
    let streaming_out = temp.path().join("streaming");
    build_in_memory(&make_args(reference_out.clone())).unwrap();
    build(&make_args(streaming_out.clone())).unwrap();
    assert_bundles_identical(&reference_out, &streaming_out, "conformant no-mint");

    let files = collect(&streaming_out);
    for name in files.keys() {
        assert!(
            !name.contains("external-ids-")
                && !name.contains("ext-locator")
                && !name.ends_with("pairs.parquet"),
            "a no-mint, no-oracle-pairs bundle must not contain {name}"
        );
    }
    let manifest: serde_json::Value =
        serde_json::from_slice(&files["v00000/MANIFEST.json"]).unwrap();
    for listed in manifest["files"].as_object().unwrap().keys() {
        assert!(
            !listed.contains("external-ids-") && !listed.contains("ext-locator"),
            "MANIFEST must not list an unwritten file: {listed}"
        );
    }

    let report = tessera_build::verify(&streaming_out).unwrap();
    assert_eq!(report.rows, N_ITEMS);
}

/// Batch-scoped assignment (§11.1 r23): with an explicit batch size the streaming build must
/// (a) byte-match the reference build running the same per-chunk sort, (b) spill its buckets
/// and sweep multiple bands when forced (the band seam), (c) record the batch size in
/// MANIFEST provenance, and (d) stay deterministic. The batch splits ordinal space mid-corpus
/// — including an uneven tail — so cross-batch band emission, per-batch dedup, and the
/// entity-base continuation are all on the line.
#[test]
fn batched_build_is_byte_identical_to_the_batched_reference() {
    let temp = tempfile::TempDir::new().unwrap();
    let points = temp.path().join("points.parquet");
    let pairs = temp.path().join("pairs.parquet");
    write_points(&points);
    write_pairs(&pairs);

    // Two batches with an uneven tail (4000 = 2100 + 1900), a tiny budget that forces the
    // buckets to spill (but stays above the plan's floors), and bands of at most 400 pre-dedup
    // rows so the postings sweep runs several bands.
    let make_args = |out: PathBuf| {
        let mut args = args_for(&points, &pairs, out);
        args.batch_items = Some(2_100);
        args.memory_budget = Some(96 << 20);
        args.band_rows = Some(400);
        args
    };

    let reference_out = temp.path().join("reference");
    let streaming_out = temp.path().join("streaming");
    build_in_memory(&make_args(reference_out.clone())).unwrap();
    build(&make_args(streaming_out.clone())).unwrap();
    assert_bundles_identical(
        &reference_out,
        &streaming_out,
        "batched, forced spill+bands",
    );

    // Determinism of the batched path.
    let again = temp.path().join("again");
    build(&make_args(again.clone())).unwrap();
    assert_bundles_identical(&streaming_out, &again, "batched run 1 vs run 2");

    // The batch size is identity-bearing and must be recorded (and the single-batch builds
    // above must NOT record one — checked in the conformant test's manifest assertions).
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(streaming_out.join("v00000/MANIFEST.json")).unwrap())
            .unwrap();
    assert_eq!(
        manifest["provenance"]["batch_items"].as_u64(),
        Some(2_100),
        "a batched build must record its batch size in provenance"
    );
    assert!(
        !streaming_out.join(".build-tmp").exists(),
        "spill files must not survive a successful build"
    );

    // And the batching genuinely changed the assignment (the fragmentation is real, not a
    // no-op): the batched bundle differs from the single-batch one.
    let single = temp.path().join("single");
    build(&args_for(&points, &pairs, single.clone())).unwrap();
    let batched_perm =
        std::fs::read(streaming_out.join("v00000/partitions/default/slices/s0/permutation.bin"))
            .unwrap();
    let single_perm =
        std::fs::read(single.join("v00000/partitions/default/slices/s0/permutation.bin")).unwrap();
    assert_ne!(
        batched_perm, single_perm,
        "two batches must produce a different (per-batch) assignment than one"
    );
    let single_manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(single.join("v00000/MANIFEST.json")).unwrap())
            .unwrap();
    assert!(
        single_manifest["provenance"].get("batch_items").is_none(),
        "a single-batch build must not record a batch size"
    );
}

/// The plan's refusals are typed and arrive before any output: a batch size far below what the
/// budget supports permanently fragments posting runs and is refused (the operator states a
/// matching budget to make a small batch deliberate), and `--batch-items 0` is meaningless.
#[test]
fn needlessly_small_batches_are_refused() {
    let temp = tempfile::TempDir::new().unwrap();
    let points = temp.path().join("points.parquet");
    let pairs = temp.path().join("pairs.parquet");
    write_points(&points);
    write_pairs(&pairs);

    let mut args = args_for(&points, &pairs, temp.path().join("out"));
    args.batch_items = Some(50); // 80 batches where the budget supports one
    let err = build(&args).unwrap_err().to_string();
    assert!(
        err.contains("fragment"),
        "refusal must explain the permanent fragmentation: {err}"
    );
    assert!(
        !temp.path().join("out").join("CURRENT").exists(),
        "a refused build must produce no bundle"
    );

    let mut zero = args_for(&points, &pairs, temp.path().join("out2"));
    zero.batch_items = Some(0);
    assert!(build(&zero).is_err());
}

/// The same equivalence under `--limit`, which selects a prefix of *source* entity space and so
/// changes which terms appear at all, and in what order they first appear.
#[test]
fn streaming_build_matches_the_reference_under_a_limit() {
    let temp = tempfile::TempDir::new().unwrap();
    let points = temp.path().join("points.parquet");
    let pairs = temp.path().join("pairs.parquet");
    write_points(&points);
    write_pairs(&pairs);

    let reference_out = temp.path().join("reference");
    let streaming_out = temp.path().join("streaming");
    let mut a = args_for(&points, &pairs, reference_out.clone());
    a.limit = Some(500_000);
    let mut b = args_for(&points, &pairs, streaming_out.clone());
    b.limit = Some(500_000);
    let reference = build_in_memory(&a).unwrap();
    let streaming = build(&b).unwrap();
    assert!(reference.items > 0 && reference.items < N_ITEMS);
    assert_eq!(reference.items, streaming.items);
    assert_bundles_identical(&reference_out, &streaming_out, "limited");
}

/// I9's assignment rule, asserted directly rather than only through byte equality: entity IDs
/// follow the signature order, so an item's signature is never lexicographically greater than
/// the next entity's, and items with identical signatures occupy a contiguous entity range.
#[test]
fn entity_ids_follow_signature_order() {
    let temp = tempfile::TempDir::new().unwrap();
    let points = temp.path().join("points.parquet");
    let pairs = temp.path().join("pairs.parquet");
    write_points(&points);
    write_pairs(&pairs);
    let out = temp.path().join("bundle");
    build(&args_for(&points, &pairs, out.clone())).unwrap();

    // Recover each entity's signature from pairs.parquet, which is the (term, entity) relation.
    let bundle = tessera_store::read::open_bundle(&out).unwrap();
    let partition = bundle.partitions.values().next().unwrap();
    let n = partition.manifest.entity_id_high_water as usize;
    let pairs_path = out
        .join("v00000")
        .join("partitions")
        .join("default")
        .join("terms")
        .join("pairs.parquet");
    let reader = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(
        File::open(&pairs_path).unwrap(),
    )
    .unwrap()
    .build()
    .unwrap();
    let mut signatures: Vec<Vec<u32>> = vec![Vec::new(); n];
    for batch in reader {
        let batch = batch.unwrap();
        let entities = batch
            .column(0)
            .as_any()
            .downcast_ref::<UInt64Array>()
            .unwrap();
        let terms = batch
            .column(1)
            .as_any()
            .downcast_ref::<UInt32Array>()
            .unwrap();
        for i in 0..batch.num_rows() {
            signatures[entities.value(i) as usize].push(terms.value(i));
        }
    }
    for signature in &mut signatures {
        signature.sort_unstable();
    }
    for entity in 1..n {
        assert!(
            signatures[entity - 1] <= signatures[entity],
            "entity {entity} breaks the signature order: {:?} then {:?}",
            signatures[entity - 1],
            signatures[entity]
        );
    }
    // The point of the ordering: identical signatures form runs, which is what compresses the
    // postings — measured at 8.9-36.7x (`probes/results.md`).
    let distinct = signatures.windows(2).filter(|w| w[0] != w[1]).count() + 1;
    assert!(
        distinct < n / 4,
        "expected identical signatures to be contiguous runs, saw {distinct} runs over {n} items"
    );
}

/// A manual scale check, ignored by default: builds the probe corpus prefix through the
/// **reference** path so its peak RSS can be compared against the streaming one under
/// `/usr/bin/time -v`. Run as
/// `cargo test --release -p tessera-build --test build_equivalence -- --ignored reference_build_at_scale`.
#[test]
#[ignore = "reads the probe corpus; run manually for a memory comparison"]
fn reference_build_at_scale() {
    let limit: u64 = std::env::var("TESSERA_SCALE_LIMIT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_400_000);
    let out = PathBuf::from("/tmp/tessera-reference-scale");
    let _ = std::fs::remove_dir_all(&out);
    let report = build_in_memory(&BuildArgs {
        points: PathBuf::from("data/scaled/geometry.parquet"),
        pairs: PathBuf::from("data/scaled/pairs/categories-subclass.pairs.parquet"),
        out,
        extent: Extent {
            x_min: 0.0,
            x_max: 65536.0,
            y_min: 0.0,
            y_max: 65536.0,
        },
        slice_id: "s0".to_string(),
        limit: Some(limit),
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        mint_external_ids: true,
        emit_oracle_pairs: true,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
    })
    .unwrap();
    assert_eq!(report.items, limit);
}
