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

use arrow::array::{Array, Float64Array, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use sha2::{Digest, Sha256};

use tessera_build::{build, verify, verify_deep, BuildArgs, VerifyOpts};
use tessera_spatial::Bounds;
use tessera_store::flush::{write_flush_segment, FlushInput, FlushRow};
use tessera_store::manifest::{CurrentPointer, FileDigest, SegmentsManifest};
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
        points,
        pairs,
        out: out.clone(),
        extent: extent(),
        slice_id: "s0".to_string(),
        limit: None,
        identity_key: IdentityKey::from_hex(TEST_KEY_HEX).unwrap(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
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
            seg_id: "flush-1",
            rows,
            quantisation: manifest.quantisation,
            identity_key: &key,
            shard_id: 0,
            scalar_schema: &[],
            row_base: n as u32,
        },
    )
    .expect("the flush segment writes");

    // The flushed entities' postings live in a delta tier the base `pairs.parquet` has never
    // seen — the state the pairs check must not refuse.
    let delta_rel = "partitions/default/slices/s0/segments/flush-1/delta.arrow".to_string();
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
        segments,
        deltas: vec![delta_rel],
        dict_extents: seg0.dict_extents.clone(),
        attr_extents: Vec::new(),
        record_extents: Vec::new(),
        text_extents: Vec::new(),
        external_id_runs,
        locator_extents: vec![flush.locator_extent.clone()],
        tombstones: vec![old_holder.raw()],
        deny: Vec::new(),
        vocabulary_extensions: Vec::new(),
        files,
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
        fs::write(&segments_path, serde_json::to_vec_pretty(&segments).unwrap()).unwrap();
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
/// its identity loop restarted the row index at zero per segment while indexing a slice-wide
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
    let err = verify_deep(root, &VerifyOpts::default())
        .expect_err("the damaged bundle must be refused");
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
    let rel = "partitions/default/slices/s0/segments/flush-1/columns.arrow";
    let path = root.join("v00000").join(rel);

    let reader =
        arrow::ipc::reader::FileReader::try_new(File::open(&path).unwrap(), None).unwrap();
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
    let rel = "partitions/default/slices/s0/segments/flush-1/morton.u32";
    let path = root.join("v00000").join(rel);

    let mut bytes = fs::read(&path).unwrap();
    assert_eq!(bytes.len(), 8, "two rows, one u32 code each");
    let (first, second) = bytes.split_at_mut(4);
    assert_ne!(first, second, "the fixture's codes must differ for the swap to damage");
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

/// Postings sorted and duplicate-free: term 0's record is hand-encoded with its own first entity
/// duplicated (the honest writer refuses such input, so the record is built from raw bytes; the
/// duplicate repeats a genuine pair so the sortedness check fires before the pairs comparison
/// could).
#[test]
fn a_posting_with_a_duplicate_entity_is_refused() {
    let temp = tempfile::TempDir::new().unwrap();
    flushed_bundle(temp.path());
    let root = bundle_root(&temp);
    let rel = "partitions/default/terms/postings.arrow";
    let path = root.join("v00000").join(rel);

    let per_term = read_base_postings(&path);
    let first = *per_term[0].first().expect("term 0 has a posting");
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
    records[0] = damaged;
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
    fs::write(&segments_path, serde_json::to_vec_pretty(&segments).unwrap()).unwrap();

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

    let mut per_term = read_base_postings(&root.join("v00000/partitions/default/terms/postings.arrow"));
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

/// The source binding is specified (correctness-suite §11.1) but its manifest field is not: a
/// request to check it must refuse loudly, never pretend the binding was checked.
#[test]
fn a_source_binding_request_is_refused_until_the_contract_carries_the_field() {
    let temp = tempfile::TempDir::new().unwrap();
    flushed_bundle(temp.path());
    let root = bundle_root(&temp);

    let opts = VerifyOpts {
        source: Some(root.join("points.parquet")),
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
        .map(|t| {
            match reader.posting_at(t).unwrap().unwrap() {
                tessera_authz::PostingRef::Array(bytes) => bytes
                    .chunks_exact(4)
                    .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
                    .collect(),
                tessera_authz::PostingRef::Roaring(view) => view.iter().collect(),
            }
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
    fs::write(&segments_path, serde_json::to_vec_pretty(&segments).unwrap()).unwrap();
}
