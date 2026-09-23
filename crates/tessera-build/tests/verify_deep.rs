//! The verifier against the bundle shapes the write path actually produces.
//!
//! The fixture is a bundle that has **flushed and re-ingested**: a batch build, one flush
//! segment above it, a delta postings tier the flush's terms live in, and an external-id key
//! the flush re-binds after its build-time holder was deleted (decision 0047) — retained
//! superseded binding, tombstone and all. Correctness-suite §18 obligation 10 is why the accept
//! tests exist at all: the false-refusal direction is what a checker extended past its original
//! single-segment shape gets wrong, and nothing else in the tree exercises the verifier over a
//! multi-segment bundle.
//!
//! The refuse tests damage one artefact each and **repair its manifest digest**, so what fails
//! is the structural check under test, never the digest sweep in front of it (§18 obligation 9:
//! a checker nobody has seen fail is a checker nobody knows works).

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{Array, BinaryArray, Float64Array, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use sha2::{Digest, Sha256};

use tessera_build::{build, verify, verify_deep, verify_with_window_rows, BuildArgs, VerifyOpts};
use tessera_spatial::Bounds;
use tessera_store::flush::{write_flush_segment, FlushInput, FlushRow};
use tessera_store::manifest::{CurrentPointer, DenySet, FileDigest, SegmentsManifest};
use tessera_store::write_segments_manifest;
use tessera_types::{EntityId, IdentityKey, TermId, SMALL_TERM_THRESHOLD_DEFAULT};

const TEST_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";
const N_ITEMS: u64 = 48;
/// The build item whose external id the flush re-binds (decision 0047's delete + re-ingest).
const REBOUND_SOURCE: u64 = 5;

fn extent() -> Bounds {
    Bounds {
        x_min: 0.0,
        x_max: 1000.0,
        y_min: 0.0,
        y_max: 1000.0,
    }
}

fn write_points(path: &Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
    ]));
    let ids: Vec<u64> = (0..N_ITEMS).collect();
    let xs: Vec<f64> = ids.iter().map(|e| ((e * 37) % 1000) as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|e| ((e * 53) % 1000) as f64).collect();
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
        for t in [e % 5, (e * 3) % 7] {
            entities.push(e);
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

/// A bundle that has flushed and re-ingested: build (external ids minted, oracle pairs on), one
/// flush segment holding two entities, a delta tier carrying their postings, a re-bound external
/// id whose superseded holder is tombstoned, and a complete-current-state `SEGMENTS-1.json`.
fn flushed_bundle(root: &Path) {
    let points = root.join("points.parquet");
    let pairs = root.join("pairs.parquet");
    let out = root.join("bundle");
    write_points(&points);
    write_pairs(&pairs);
    let args = BuildArgs {
        views: vec![tessera_build::ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: tessera_spatial::Projection::None,
            extent: extent(),
            points,
            point_fields: Default::default(),
            select: None,
            access: tessera_build::config::AccessInput::relation(pairs),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: Vec::new(),
        out: out.clone(),
        limit: None,
        identity_key: IdentityKey::from_hex(TEST_KEY_HEX).unwrap(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        mint_external_ids: true,
        emit_oracle_pairs: true,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: Default::default(),
    };
    build(&args).expect("the batch build succeeds");

    let prefix_dir = out.join("v00000");
    let manifest: tessera_store::manifest::Manifest = serde_json::from_slice(
        &fs::read(prefix_dir.join("MANIFEST.json")).expect("MANIFEST.json readable"),
    )
    .expect("MANIFEST.json parses");
    let seg0: SegmentsManifest = serde_json::from_slice(
        &fs::read(prefix_dir.join("partitions/default/SEGMENTS-0.json")).expect("SEGMENTS-0"),
    )
    .expect("SEGMENTS-0 parses");
    let n = manifest.entity_id_high_water;
    assert_eq!(n, N_ITEMS);

    // The deleted holder of the re-bound key: resolved through the sidecar *before* the flush
    // publishes the newer binding, so the tombstone names the entity the build actually bound.
    let old_holder = {
        let bundle = tessera_store::read::open_bundle(&out).expect("the built bundle opens");
        let partition = bundle.partitions.get("default").expect("one partition");
        let sidecar = tessera_store::ExternalIdSidecar::deferred_from_manifest(
            &bundle.manifest,
            &partition.manifest,
            &prefix_dir,
        )
        .expect("the sidecar constructs");
        sidecar
            .resolve(&REBOUND_SOURCE.to_le_bytes())
            .expect("the sidecar resolves")
            .expect("the build bound this key")
    };

    // Two flushed entities: one under a fresh key, one re-binding the deleted holder's key.
    let key = IdentityKey::from_hex(TEST_KEY_HEX).unwrap();
    let rows = vec![
        FlushRow {
            entity_id: EntityId::new(n),
            external_id: Some(9_999u64.to_le_bytes().to_vec()),
            x: 10.0,
            y: 10.0,
            scalars: Vec::new(),
        },
        FlushRow {
            entity_id: EntityId::new(n + 1),
            external_id: Some(REBOUND_SOURCE.to_le_bytes().to_vec()),
            x: 990.0,
            y: 990.0,
            scalars: Vec::new(),
        },
    ];
    let flush = write_flush_segment(
        &prefix_dir,
        "default",
        "s0",
        FlushInput {
            incarnation: 0,
            seg_id: "flush-1",
            rows,
            quantisation: manifest
                .quantisation_of("s0")
                .expect("the built manifest declares view 's0'"),
            identity_key: &key,
            shard_id: 0,
            scalar_schema: &[],
            row_base: n as u32,
        },
    )
    .expect("the flush segment writes");

    // The flushed entities' postings live in a delta tier the base `pairs.parquet` has never
    // seen — the state the pairs check must not refuse.
    let delta_rel = "partitions/default/views/s0/segments/flush-1/delta.arrow".to_string();
    let delta_path = prefix_dir.join(&delta_rel);
    tessera_authz::write_delta_tier(
        &delta_path,
        &[(TermId::new(3), vec![n as u32, n as u32 + 1])],
        SMALL_TERM_THRESHOLD_DEFAULT,
    )
    .expect("the delta tier writes");

    let mut files: BTreeMap<String, FileDigest> = flush.files.clone();
    files.insert(
        delta_rel.clone(),
        tessera_store::digest_of(&delta_path).expect("the tier digests"),
    );

    let mut segments = seg0.segments.clone();
    segments.push(flush.segment.clone());
    let mut external_id_runs = seg0.external_id_runs.clone();
    external_id_runs.push(flush.external_id_run.clone());

    let manifest_1 = SegmentsManifest {
        watermark: flush.watermark,
        entity_id_high_water: flush.entity_id_high_water,
        entity_id_low_water: seg0.entity_id_low_water,
        layers: seg0.layers.clone(),
        layer_tombstones: seg0.layer_tombstones.clone(),
        layer_registry_version: seg0.layer_registry_version,
        segments,
        deltas: vec![delta_rel],
        dict_extents: seg0.dict_extents.clone(),
        external_id_runs,
        locator_extents: vec![flush.locator_extent.clone()],
        tombstones: DenySet::of(&croaring::Bitmap::of(&[old_holder.raw() as u32])),
        files,
        ..SegmentsManifest::empty()
    };
    write_segments_manifest(&prefix_dir, "default", 1, &manifest_1)
        .expect("SEGMENTS-1.json commits");
}

fn bundle_root(temp: &tempfile::TempDir) -> PathBuf {
    temp.path().join("bundle")
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Re-digest `rel` (prefix-relative) into whichever manifest names it, so a damage test fails on
/// the structural check under test rather than on the digest sweep in front of it. Rewrites
/// `SEGMENTS-1.json` directly when that map holds the entry; otherwise `MANIFEST.json` — whose
/// own digest then has to chase into `CURRENT`.
fn refresh_digest(root: &Path, rel: &str) {
    let prefix_dir = root.join("v00000");
    let bytes = fs::read(prefix_dir.join(rel)).expect("the damaged file reads back");
    let digest = serde_json::json!({ "size": bytes.len(), "sha256": hex_sha256(&bytes) });

    let segments_path = prefix_dir.join("partitions/default/SEGMENTS-1.json");
    let mut segments: serde_json::Value =
        serde_json::from_slice(&fs::read(&segments_path).unwrap()).unwrap();
    if segments["files"].get(rel).is_some() {
        segments["files"][rel] = digest;
        fs::write(
            &segments_path,
            serde_json::to_vec_pretty(&segments).unwrap(),
        )
        .unwrap();
        return;
    }

    let manifest_path = prefix_dir.join("MANIFEST.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    assert!(
        manifest["files"].get(rel).is_some(),
        "{rel} is named by neither files map"
    );
    manifest["files"][rel] = digest;
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).unwrap();
    fs::write(&manifest_path, &manifest_bytes).unwrap();
    let current = CurrentPointer {
        prefix: "v00000".to_string(),
        manifest_digest: hex_sha256(&manifest_bytes),
    };
    fs::write(
        root.join("CURRENT"),
        serde_json::to_vec_pretty(&current).unwrap(),
    )
    .unwrap();
}

/// The false-refusal direction (§18 obligation 10): a valid bundle that has flushed and
/// re-ingested must verify — shallow and deep. Before the row-offset fix the shallow verifier
/// refused every such bundle: its bijection sweep counted only the base permutation's rows and
/// its identity loop restarted the row index at zero per segment while indexing a view-wide
/// array.
#[test]
fn a_flushed_and_reingested_bundle_verifies_shallow_and_deep() {
    let temp = tempfile::TempDir::new().unwrap();
    flushed_bundle(temp.path());
    let root = bundle_root(&temp);

    let report = verify(&root).expect("shallow verify accepts a valid multi-segment bundle");
    assert_eq!(report.segments, 2);
    assert_eq!(report.rows, N_ITEMS + 2);

    let deep = verify_deep(&root, &VerifyOpts::default())
        .expect("deep verify accepts a valid multi-segment bundle");
    assert_eq!(deep.shallow.segments, 2);
    // The delta tier's pairs are not in `pairs.parquet` — the scoped check must not refuse them.
    assert_eq!(deep.delta_tiers, 1);
    assert!(deep.pairs_rows > 0, "the base pairs were compared");
    // Two runs (build + flush), and the re-bound key appears in both — newest binding first,
    // not a bijection (decision 0047).
    assert_eq!(deep.external_id_bindings, N_ITEMS + 2);
}

/// **Both routes of the identity check, over one fixture.**
///
/// Above `DIRECT_WINDOW_ROWS` the check goes through a row partition on disk and below it through
/// a window over the whole row space. Every bundle a test can afford to build is below, and every
/// bundle at corpus scale is above, so without a threshold a test can lower, the route that runs
/// in anger runs nowhere in CI. `verify_with_window_rows(root, 0)` puts every view through the
/// partition; the two routes must agree on an accepted bundle's report and on a damaged one's
/// refusal.
#[test]
fn the_partition_route_and_the_window_route_agree() {
    let temp = tempfile::TempDir::new().unwrap();
    flushed_bundle(temp.path());
    let root = bundle_root(&temp);

    let window = verify(&root).expect("the window route accepts a valid bundle");
    let partition =
        verify_with_window_rows(&root, 0).expect("the partition route accepts the same bundle");
    assert_eq!(window.rows, partition.rows);
    assert_eq!(window.segments, partition.segments);
    assert_eq!(window.views, partition.views);
    assert_eq!(window.partitions, partition.partitions);
    assert_eq!(window.entity_id_high_water, partition.entity_id_high_water);
    assert_eq!(window.bundle_bytes, partition.bundle_bytes);

    // The deep pass reaches the same route through its options, and reports the same figures.
    let deep = verify_deep(
        &root,
        &VerifyOpts {
            direct_window_rows: 0,
            ..VerifyOpts::default()
        },
    )
    .expect("the partition route accepts the same bundle deep");
    assert_eq!(deep.shallow.rows, window.rows);
    assert_eq!(deep.external_id_bindings, N_ITEMS + 2);

    // One `tessera_id` of the *base* segment is replaced by another entity's, so the row is
    // claimed — surjectivity still holds — and what refuses is the derivation. The base is the
    // segment where the comparison is between two artefacts rather than a restatement of the
    // extent inversion the open performs.
    let rel = "partitions/default/views/s0/segments/seg-0/columns.arrow";
    let path = root.join("v00000").join(rel);
    let reader = arrow::ipc::reader::FileReader::try_new(File::open(&path).unwrap(), None).unwrap();
    let schema = reader.schema();
    let batches: Vec<RecordBatch> = reader.map(|b| b.unwrap()).collect();
    let ids = batches[0]
        .column(0)
        .as_any()
        .downcast_ref::<UInt64Array>()
        .expect("column 0 is the u64 tessera_id");
    let mut values: Vec<u64> = ids.values().to_vec();
    assert!(values.len() >= 2 && values[0] != values[1]);
    values[0] = values[1];
    let mut columns = batches[0].columns().to_vec();
    columns[0] = Arc::new(UInt64Array::from(values));
    let damaged = RecordBatch::try_new(batches[0].schema(), columns).unwrap();
    let mut writer =
        arrow::ipc::writer::FileWriter::try_new(File::create(&path).unwrap(), &schema).unwrap();
    writer.write(&damaged).unwrap();
    writer.finish().unwrap();
    drop(writer);
    refresh_digest(&root, rel);

    let window = verify(&root).expect_err("the window route refuses the damaged column");
    let partition =
        verify_with_window_rows(&root, 0).expect_err("the partition route refuses it too");
    assert!(
        window
            .to_string()
            .contains("does not match identity.key's derivation"),
        "expected the derivation refusal, got: {window}"
    );
    assert_eq!(window.to_string(), partition.to_string());
}

/// The damage helper's own premise, asserted once: the fixture's re-bound key really does appear
/// in two runs, so the accept test above is exercising 0047's retained superseded binding rather
/// than a corpus where every key is unique.
#[test]
fn the_fixture_carries_a_key_bound_in_two_runs() {
    let temp = tempfile::TempDir::new().unwrap();
    flushed_bundle(temp.path());
    let root = bundle_root(&temp);
    let prefix_dir = root.join("v00000");

    let mut holders = 0usize;
    let segments: SegmentsManifest = serde_json::from_slice(
        &fs::read(prefix_dir.join("partitions/default/SEGMENTS-1.json")).unwrap(),
    )
    .unwrap();
    for rel in &segments.external_id_runs {
        let reader = arrow::ipc::reader::FileReader::try_new(
            File::open(prefix_dir.join(rel)).unwrap(),
            None,
        )
        .unwrap();
        for batch in reader {
            let batch = batch.unwrap();
            let keys = batch
                .column_by_name("external_id")
                .and_then(|c| c.as_any().downcast_ref::<arrow::array::BinaryArray>())
                .unwrap();
            for i in 0..batch.num_rows() {
                if keys.value(i) == REBOUND_SOURCE.to_le_bytes().as_slice() {
                    holders += 1;
                }
            }
        }
    }
    assert_eq!(holders, 2, "the re-bound key must appear in both runs");
}

// ---- deliberate damage (§18 obligation 9) --------------------------------------------------
//
// Each test damages exactly one artefact of the valid fixture, repairs its manifest digest so
// the digest sweep stays green, and asserts the deep verifier refuses with a message naming the
// defect. A checker nobody has seen fail is a checker nobody knows works.

fn expect_refusal(root: &Path, needle: &str) {
    let err =
        verify_deep(root, &VerifyOpts::default()).expect_err("the damaged bundle must be refused");
    let message = err.to_string();
    assert!(
        message.contains(needle),
        "expected the refusal to name the defect ('{needle}'), got: {message}"
    );
}

/// §18 obligation 9, first half: a segment whose declared column is one row short. The flush
/// segment's `columns.arrow` is rewritten with its last row dropped while the manifest still
/// declares the full count — the attribute tail's defect class, caught structurally.
#[test]
fn a_segment_whose_column_is_one_row_short_is_refused() {
    let temp = tempfile::TempDir::new().unwrap();
    flushed_bundle(temp.path());
    let root = bundle_root(&temp);
    let rel = "partitions/default/views/s0/segments/flush-1/columns.arrow";
    let path = root.join("v00000").join(rel);

    let reader = arrow::ipc::reader::FileReader::try_new(File::open(&path).unwrap(), None).unwrap();
    let schema = reader.schema();
    let batches: Vec<RecordBatch> = reader.map(|b| b.unwrap()).collect();
    assert_eq!(batches.len(), 1);
    let short = batches[0].slice(0, batches[0].num_rows() - 1);
    let mut writer =
        arrow::ipc::writer::FileWriter::try_new(File::create(&path).unwrap(), &schema).unwrap();
    writer.write(&short).unwrap();
    writer.finish().unwrap();
    refresh_digest(&root, rel);

    expect_refusal(&root, "row_count");
}

/// §18 obligation 9, second half: a segment whose Morton column is out of order. Two codes of
/// the flush segment's `morton.u32` are swapped; the fixture places the flush's two points in
/// opposite grid corners so the codes are guaranteed to differ.
#[test]
fn a_segment_whose_morton_column_is_out_of_order_is_refused() {
    let temp = tempfile::TempDir::new().unwrap();
    flushed_bundle(temp.path());
    let root = bundle_root(&temp);
    let rel = "partitions/default/views/s0/segments/flush-1/morton.u32";
    let path = root.join("v00000").join(rel);

    let mut bytes = fs::read(&path).unwrap();
    assert_eq!(bytes.len(), 8, "two rows, one u32 code each");
    let (first, second) = bytes.split_at_mut(4);
    assert_ne!(
        first, second,
        "the fixture's codes must differ for the swap to damage"
    );
    first.swap_with_slice(second);
    fs::write(&path, &bytes).unwrap();
    refresh_digest(&root, rel);

    expect_refusal(&root, "not sorted ascending");
}

/// Postings bounded by `entity_id_high_water`: the base file is rewritten with one term's
/// posting naming an entity past the high water.
#[test]
fn a_posting_past_the_high_water_is_refused() {
    let temp = tempfile::TempDir::new().unwrap();
    flushed_bundle(temp.path());
    let root = bundle_root(&temp);
    let rel = "partitions/default/terms/postings.arrow";
    let path = root.join("v00000").join(rel);

    let mut per_term = read_base_postings(&path);
    per_term[0].push(1_000_000); // far past the fixture's high water of N_ITEMS + 2
    tessera_authz::write_postings(&path, &per_term, SMALL_TERM_THRESHOLD_DEFAULT).unwrap();
    refresh_digest(&root, rel);

    expect_refusal(&root, "entity_id_high_water");
}

/// Postings sorted and duplicate-free: the first *carried* term's record is hand-encoded with its
/// own first entity duplicated (the honest writer refuses such input, so the record is built from
/// raw bytes; the duplicate repeats a genuine pair so the sortedness check fires before the pairs
/// comparison could). Not term 0, which is the reserved `public` label and carries a posting only
/// where the corpus declares it.
#[test]
fn a_posting_with_a_duplicate_entity_is_refused() {
    let temp = tempfile::TempDir::new().unwrap();
    flushed_bundle(temp.path());
    let root = bundle_root(&temp);
    let rel = "partitions/default/terms/postings.arrow";
    let path = root.join("v00000").join(rel);

    let per_term = read_base_postings(&path);
    let (carried, first) = per_term
        .iter()
        .enumerate()
        .find_map(|(t, entities)| entities.first().map(|e| (t, *e)))
        .expect("some term has a posting");
    let mut records: Vec<Vec<u8>> = per_term
        .iter()
        .enumerate()
        .map(|(t, entities)| {
            tessera_authz::encode_posting(t, entities, SMALL_TERM_THRESHOLD_DEFAULT).unwrap()
        })
        .collect();
    let mut damaged = vec![0u8]; // tag 0: raw little-endian u32 array
    damaged.extend_from_slice(&first.to_le_bytes());
    damaged.extend_from_slice(&first.to_le_bytes());
    records[carried] = damaged;
    tessera_authz::write_posting_records(&path, &records).unwrap();
    refresh_digest(&root, rel);

    expect_refusal(&root, "strictly ascending");
}

/// The locator and the runs disagreeing: two build entities' locator slots are swapped, so each
/// addresses the other's binding row.
#[test]
fn a_locator_slot_addressing_another_entitys_binding_is_refused() {
    let temp = tempfile::TempDir::new().unwrap();
    flushed_bundle(temp.path());
    let root = bundle_root(&temp);
    let rel = "partitions/default/entities/ext-locator.u32";
    let path = root.join("v00000").join(rel);

    let mut bytes = fs::read(&path).unwrap();
    assert!(bytes.len() >= 8);
    let (first, rest) = bytes.split_at_mut(4);
    first.swap_with_slice(&mut rest[..4]);
    fs::write(&path, &bytes).unwrap();
    refresh_digest(&root, rel);

    expect_refusal(&root, "the locator and the runs disagree");
}

/// A run whose external ids stop ascending. The sidecar binary-searches each run, so a key out
/// of order makes a binding unreachable; the scan holds no key but the previous one, and the pair
/// it compares is the whole check.
#[test]
fn a_run_whose_external_ids_stop_ascending_is_refused() {
    let temp = tempfile::TempDir::new().unwrap();
    flushed_bundle(temp.path());
    let root = bundle_root(&temp);

    rewrite_first_run_keys(&root, |values| values.swap(0, 1));

    expect_refusal(&root, "external ids are not strictly ascending at row 1");
}

/// Rewrite the first external-id run's key column through `damage`, leaving its entity column
/// where it was, and repair the run's manifest digest. What then fails is the ordering check and
/// never the digest sweep in front of it.
fn rewrite_first_run_keys(root: &Path, damage: impl FnOnce(&mut Vec<Vec<u8>>)) {
    let prefix_dir = root.join("v00000");
    let segments: serde_json::Value = serde_json::from_slice(
        &fs::read(prefix_dir.join("partitions/default/SEGMENTS-1.json")).unwrap(),
    )
    .unwrap();
    let rel = segments["external_id_runs"][0]
        .as_str()
        .unwrap()
        .to_string();
    let path = prefix_dir.join(&rel);

    let reader = arrow::ipc::reader::FileReader::try_new(File::open(&path).unwrap(), None).unwrap();
    let schema = reader.schema();
    let batches: Vec<RecordBatch> = reader.map(|batch| batch.unwrap()).collect();
    assert!(
        batches[0].num_rows() >= 2,
        "the run must hold two keys for there to be an order to break"
    );
    let keys = batches[0]
        .column_by_name("external_id")
        .and_then(|c| c.as_any().downcast_ref::<BinaryArray>())
        .expect("the run carries a binary 'external_id' column");
    let mut values: Vec<Vec<u8>> = (0..keys.len()).map(|i| keys.value(i).to_vec()).collect();
    damage(&mut values);
    let damaged: Vec<&[u8]> = values.iter().map(|v| v.as_slice()).collect();
    let at = batches[0].schema().index_of("external_id").unwrap();
    let mut columns = batches[0].columns().to_vec();
    columns[at] = Arc::new(BinaryArray::from(damaged));
    let first = RecordBatch::try_new(batches[0].schema(), columns).unwrap();

    let mut writer =
        arrow::ipc::writer::FileWriter::try_new(File::create(&path).unwrap(), &schema).unwrap();
    writer.write(&first).unwrap();
    for batch in &batches[1..] {
        writer.write(batch).unwrap();
    }
    writer.finish().unwrap();
    drop(writer);
    refresh_digest(root, &rel);
}

/// The same external id twice in one run. A duplicate is not merely out of order: a check written
/// as `previous > key` accepts it and the sidecar's binary search then reaches one of the two rows
/// and never the other. Strictly ascending is the requirement, and this is the case that says so.
#[test]
fn a_run_repeating_an_external_id_is_refused() {
    let temp = tempfile::TempDir::new().unwrap();
    flushed_bundle(temp.path());
    let root = bundle_root(&temp);

    rewrite_first_run_keys(&root, |values| {
        values[1] = values[0].clone();
    });

    expect_refusal(&root, "external ids are not strictly ascending at row 1");
}

/// A sidecar file whose bytes stopped matching the manifest: the family is exempt from the
/// open-time digest sweep (contracts §0.3 deviation 9), so the deep pass must be the one that
/// catches it at rest. The damage here is *not* followed by a digest repair — that is the test.
#[test]
fn a_sidecar_file_failing_its_digest_is_refused() {
    let temp = tempfile::TempDir::new().unwrap();
    flushed_bundle(temp.path());
    let root = bundle_root(&temp);
    let path = root.join("v00000/partitions/default/entities/ext-locator.u32");

    let mut bytes = fs::read(&path).unwrap();
    bytes[0] ^= 0xFF;
    fs::write(&path, &bytes).unwrap();

    expect_refusal(&root, "deviation 9");
}

/// Decision 0042: a dictionary extent repeating a descriptor the list already carries. A second
/// extent is appended whose one record duplicates the first extent's first descriptor.
#[test]
fn a_dict_extent_repeating_a_descriptor_is_refused() {
    let temp = tempfile::TempDir::new().unwrap();
    flushed_bundle(temp.path());
    let root = bundle_root(&temp);
    append_dict_extent_repeating_first_descriptor(&root, 1);

    expect_refusal(&root, "decision 0042");
}

/// The positional half of the same rule: an extent whose record count disagrees with what the
/// manifest declares shifts every later term's ordinal.
#[test]
fn a_dict_extent_with_a_miscounted_declaration_is_refused() {
    let temp = tempfile::TempDir::new().unwrap();
    flushed_bundle(temp.path());
    let root = bundle_root(&temp);
    let prefix_dir = root.join("v00000");

    let segments_path = prefix_dir.join("partitions/default/SEGMENTS-1.json");
    let mut segments: serde_json::Value =
        serde_json::from_slice(&fs::read(&segments_path).unwrap()).unwrap();
    let declared = segments["dict_extents"][0]["records"].as_u64().unwrap();
    segments["dict_extents"][0]["records"] = serde_json::json!(declared + 1);
    fs::write(
        &segments_path,
        serde_json::to_vec_pretty(&segments).unwrap(),
    )
    .unwrap();

    expect_refusal(&root, "positional");
}

/// The pairs check's refusal direction: `pairs.parquet` rewritten with one pair dropped, so it
/// is no longer the union of the base postings it was written with.
#[test]
fn a_pairs_file_missing_a_base_pair_is_refused() {
    let temp = tempfile::TempDir::new().unwrap();
    flushed_bundle(temp.path());
    let root = bundle_root(&temp);
    let rel = "partitions/default/terms/pairs.parquet";
    let path = root.join("v00000").join(rel);

    let mut per_term =
        read_base_postings(&root.join("v00000/partitions/default/terms/postings.arrow"));
    let with_rows = per_term
        .iter()
        .position(|entities| !entities.is_empty())
        .expect("some term has a posting");
    per_term[with_rows].pop();
    let mut writer = tessera_store::PairsParquetWriter::create(&path).unwrap();
    for (t, entities) in per_term.iter().enumerate() {
        writer.push_run(t as u32, entities).unwrap();
    }
    writer.finish().unwrap();
    refresh_digest(&root, rel);

    expect_refusal(&root, "pairs.parquet");
}

/// §11's other half — **the permutation covers exactly the rows the segments claim** — in its
/// refusal direction. One entity's slot in `permutation.bin` is overwritten with the row-absent
/// sentinel, so its row in `columns.arrow` is addressable by no entity at all.
///
/// The damage is deliberately the *surjective* one, because it is the only half nothing else
/// holds: the mapping stays injective and in range, so `Permutation::load`'s header and length
/// checks and `validate_rows`' aliasing sweep on the read path both still accept the bundle, and
/// `open_bundle` hands the verifier a row space it is happy with. What is left is a row the
/// segments count and the row space does not claim.
///
/// **The needle is the count check specifically**, and not merely any refusal. The identity
/// sweep's per-row companion (*"no entity claims this row"*) also meets this damage, so a test
/// content with an error of any kind would pass with the count check deleted; pinning the message
/// keeps §11's own clause — the row space claims exactly the rows the segments hold — the thing
/// under test.
///
/// Mutations this kills: dropping or weakening the `claimed != total_rows` refusal — under
/// `let _ = claimed;` the count check is silent and the refusal comes from the companion instead.
#[test]
fn a_permutation_leaving_a_row_unclaimed_is_refused() {
    let temp = tempfile::TempDir::new().unwrap();
    flushed_bundle(temp.path());
    let root = bundle_root(&temp);
    let rel = "partitions/default/views/s0/permutation.bin";
    let path = root.join("v00000").join(rel);

    // `permutation.bin` is a 24-byte header, a `u32` per page of directory, zero padding to a
    // 4 KiB boundary, then the present pages of 2¹⁶ slots each (contracts R4;
    // `tessera_store::permutation`). The fixture's bound is well under one page, so entity
    // `ORPHANED`'s slot sits at the payload's start.
    const PAYLOAD: usize = 4096;
    const ROW_ABSENT: [u8; 4] = [0xff; 4];
    const ORPHANED: usize = 3;
    let mut bytes = fs::read(&path).unwrap();
    let slot = PAYLOAD + ORPHANED * 4;
    assert_eq!(
        u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
        N_ITEMS,
        "the fixture's base permutation must cover the built entities"
    );
    assert_ne!(
        bytes[slot..slot + 4],
        ROW_ABSENT,
        "entity {ORPHANED} must hold a row before this test takes it away, or the damage is no \
         damage"
    );
    bytes[slot..slot + 4].copy_from_slice(&ROW_ABSENT);
    fs::write(&path, &bytes).unwrap();
    refresh_digest(&root, rel);

    expect_refusal(&root, "the row space claims");
}

/// The source binding is specified (correctness-suite §11.1) but its manifest field is not: a
/// request to check it must refuse loudly, never pretend the binding was checked.
#[test]
fn a_source_binding_request_is_refused_until_the_contract_carries_the_field() {
    let temp = tempfile::TempDir::new().unwrap();
    flushed_bundle(temp.path());
    let root = bundle_root(&temp);

    let opts = VerifyOpts {
        source: Some(root.join("points.parquet")),
        ..VerifyOpts::default()
    };
    let err = verify_deep(&root, &opts).expect_err("the unimplemented binding must refuse");
    assert!(
        err.to_string().contains("source.digest"),
        "the refusal must name the missing manifest field, got: {err}"
    );
}

/// Decode the base postings back into per-term entity lists, so a damage test can rewrite the
/// file with one surgical change.
fn read_base_postings(path: &Path) -> Vec<Vec<u32>> {
    let reader = tessera_authz::PostingsReader::open(path, false).unwrap();
    (0..reader.term_count())
        .map(|t| match reader.posting_at(t).unwrap().unwrap() {
            tessera_authz::PostingRef::Array(bytes) => bytes
                .chunks_exact(4)
                .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
                .collect(),
            tessera_authz::PostingRef::Roaring(view) => view.iter().collect(),
        })
        .collect()
}

/// Append `dictionary/terms-<k>.dict` holding one record that duplicates the first extent's
/// first descriptor, and name it in `SEGMENTS-1.json`'s extent list and files map.
fn append_dict_extent_repeating_first_descriptor(root: &Path, k: usize) {
    let prefix_dir = root.join("v00000");
    let first = fs::read(prefix_dir.join("dictionary/terms-0.dict")).unwrap();
    let len = u32::from_le_bytes(first[0..4].try_into().unwrap()) as usize;
    let record = &first[..4 + len];
    let rel = format!("dictionary/terms-{k}.dict");
    fs::write(prefix_dir.join(&rel), record).unwrap();

    let segments_path = prefix_dir.join("partitions/default/SEGMENTS-1.json");
    let mut segments: serde_json::Value =
        serde_json::from_slice(&fs::read(&segments_path).unwrap()).unwrap();
    segments["dict_extents"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({ "path": rel, "records": 1 }));
    segments["files"][&rel] =
        serde_json::json!({ "size": record.len(), "sha256": hex_sha256(record) });
    fs::write(
        &segments_path,
        serde_json::to_vec_pretty(&segments).unwrap(),
    )
    .unwrap();
}

/// **A rendered group-scoped family's lane is verified per (build segment, view)** (`views.md`
/// §5). The lane is the placement `render` buys, and it is the one artefact whose absence is
/// *silent*: a segment that does not hold the column is served as a row with no value, which is a
/// legitimate state for a flushed segment and indistinguishable, at the wire, from a build that
/// wrote no lane at all. Nothing else in either pass would notice — the digests match, the row
/// space is a bijection, the identity column is untouched.
///
/// Both directions, on one fixture: the intact bundle verifies and counts the lanes it checked,
/// and the same bundle with one view's column dropped out of `columns.arrow` is refused by name.
#[test]
fn a_missing_scoped_render_lane_is_refused_and_an_intact_one_is_counted() {
    use arrow::array::{Float32Array, StringArray};
    use tessera_spatial::tiler::ScalarType;

    let temp = tempfile::TempDir::new().unwrap();
    let dir = temp.path();
    // One points file, a `quarter` discriminator, one value column: the group's two views are two
    // selections of it (`views.md` §3.1's form B).
    let points = dir.join("quarters.parquet");
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("quarter", DataType::Utf8, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("heat", DataType::Float32, true),
    ]));
    let (mut ids, mut keys) = (Vec::new(), Vec::new());
    let (mut xs, mut ys, mut heat) = (Vec::new(), Vec::new(), Vec::new());
    for (slot, key) in ["2026-Q1", "2026-Q2"].into_iter().enumerate() {
        for e in 0..N_ITEMS {
            ids.push(e);
            keys.push(key.to_string());
            xs.push(((e * 37) % 1000) as f64);
            ys.push(((e * 53 + slot as u64 * 7) % 1000) as f64);
            heat.push((e % 3 != 0).then_some(e as f32 + slot as f32));
        }
    }
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(StringArray::from(keys)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
            Arc::new(Float32Array::from(heat)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(&points).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
    let pairs = dir.join("pairs.parquet");
    write_pairs(&pairs);

    let view = |key: &str| tessera_build::ViewArgs {
        visibility: None,
        view_id: format!("quarter:{key}"),
        projection: tessera_spatial::Projection::None,
        extent: extent(),
        points: points.clone(),
        point_fields: Default::default(),
        select: Some(tessera_build::config::ViewSelector {
            column: "quarter".to_string(),
            value: key.to_string(),
            keys: vec!["2026-Q1".to_string(), "2026-Q2".to_string()],
            view_id: format!("quarter:{key}"),
        }),
        access: tessera_build::config::AccessInput::relation(pairs.clone()),
    };
    let out = dir.join("bundle");
    build(&BuildArgs {
        views: vec![view("2026-Q1"), view("2026-Q2")],
        anchor: 0,
        groups: vec![tessera_build::GroupDescriptor {
            title: None,
            point_default: Some("public".to_string()),
            visibility: None,
            name: "quarter".to_string(),
            members_of: None,
            views: ["2026-Q1", "2026-Q2"]
                .into_iter()
                .map(|key| tessera_build::GroupViewDescriptor {
                    key: key.to_string(),
                    visibility: None,
                    metadata: Default::default(),
                })
                .collect(),
            quantisation: tessera_build::Quantisation {
                x_min: extent().x_min,
                x_max: extent().x_max,
                y_min: extent().y_min,
                y_max: extent().y_max,
            },
            projection: tessera_spatial::Projection::None,
            metadata: Vec::new(),
            scoped_scalars: Vec::new(),
        }],
        scoped_attributes: vec![tessera_build::ScopedColumnFamily {
            attribute: tessera_build::config::Attribute {
                name: "heat".to_string(),
                title: None,
                field: None,
                ty: ScalarType::F32,
                analyser: None,
                vocabulary: None,
                value_set: None,
                index: false,
                render: true,
            },
            group: "quarter".to_string(),
            views: vec![0, 1],
            source: None,
        }],
        attribute_sources: Vec::new(),
        out: out.clone(),
        limit: None,
        identity_key: IdentityKey::from_hex(TEST_KEY_HEX).unwrap(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        mint_external_ids: false,
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: Default::default(),
    })
    .expect("a rendered scoped family builds");

    let report = verify_deep(&out, &VerifyOpts::default()).expect("the intact bundle verifies");
    assert_eq!(
        report.scoped_render_lanes, 2,
        "one lane per (build segment, view of the group)"
    );

    // The damage: the same rows, the same identities, the same row count — with the family's
    // column dropped out of one view's tail.
    let rel = "partitions/default/views/quarter/2026-Q1/segments/seg-0/columns.arrow";
    let path = out.join("v00000").join(rel);
    let reader = arrow::ipc::reader::FileReader::try_new(File::open(&path).unwrap(), None).unwrap();
    let batches: Vec<RecordBatch> = reader.map(|b| b.unwrap()).collect();
    assert_eq!(batches.len(), 1);
    let held = batches[0].schema();
    let keep: Vec<usize> = held
        .fields()
        .iter()
        .enumerate()
        .filter(|(_, f)| f.name() != "heat")
        .map(|(i, _)| i)
        .collect();
    assert_eq!(keep.len(), held.fields().len() - 1, "the lane was there");
    let stripped = Arc::new(Schema::new(
        keep.iter()
            .map(|&i| held.field(i).clone())
            .collect::<Vec<_>>(),
    ));
    let columns = keep
        .iter()
        .map(|&i| batches[0].column(i).clone())
        .collect::<Vec<_>>();
    let without = RecordBatch::try_new(stripped.clone(), columns).unwrap();
    let mut writer =
        arrow::ipc::writer::FileWriter::try_new(File::create(&path).unwrap(), &stripped).unwrap();
    writer.write(&without).unwrap();
    writer.finish().unwrap();

    // The digest follows the damage, so what refuses is the missing lane and not the hash.
    let bytes = fs::read(&path).unwrap();
    let manifest_path = out.join("v00000").join("MANIFEST.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest["files"][rel] =
        serde_json::json!({ "size": bytes.len(), "sha256": hex_sha256(&bytes) });
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).unwrap();
    fs::write(&manifest_path, &manifest_bytes).unwrap();
    fs::write(
        out.join("CURRENT"),
        serde_json::to_vec_pretty(&CurrentPointer {
            prefix: "v00000".to_string(),
            manifest_digest: hex_sha256(&manifest_bytes),
        })
        .unwrap(),
    )
    .unwrap();

    expect_refusal(&out, "does not hold it");
}

/// **A cut index that names one boundary too few is refused.** The boundaries of `cuts.u32` are
/// what makes the cells cover the segment without overlapping, and selection reads a cell's
/// identities as ascending on the strength of them (contracts §2.6): a missing boundary joins two
/// cells, and the join is where the ascending property fails. The file stays strictly ascending
/// and still starts at row 0, so the open's own validation passes it and only the pass that holds
/// both columns can tell.
///
/// The other direction — a boundary where the Morton code does not change — is refused by the
/// same loop; this fixture places every point in a cell of its own, so there is no row to put one
/// at.
#[test]
fn a_cut_index_missing_a_cell_boundary_is_refused() {
    let temp = tempfile::TempDir::new().unwrap();
    flushed_bundle(temp.path());
    let root = bundle_root(&temp);
    let rel = "partitions/default/views/s0/segments/seg-0/cuts.u32";
    let path = root.join("v00000").join(rel);

    let bytes = fs::read(&path).unwrap();
    assert!(
        bytes.len() >= 12,
        "the base segment must hold at least three cells for a boundary to be droppable"
    );
    let mut damaged = bytes.clone();
    damaged.drain(4..8);
    fs::write(&path, &damaged).unwrap();
    refresh_digest(&root, rel);

    expect_refusal(&root, "where the Morton code changes at row");
}
