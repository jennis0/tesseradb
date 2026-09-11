//! The build half of the record blob (records §3, §7): a declared column with neither index nor
//! render set is blob-resident, and the build writes `attrs/record/*`, digested into the manifest
//! like every base artefact.
//!
//! The declarations are constructed **programmatically**, not parsed: on this branch the schema
//! parse still refuses a neither-column (the `used_for` migration deletes that refusal in this
//! same epic), and driving the pipeline directly is what lets the stage land first. The values
//! are read back through `tessera_filter::RecordBlob` — the reader that will serve drill-down —
//! and compared against the fixture's own generation functions, which is the conformance
//! relation's shape (records §3, review B7).

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{BinaryArray, Float64Array, Int64Array, StringArray, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema as ArrowSchema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use tessera_build::config::{Attribute, Schema};
use tessera_build::{build, BuildArgs};
use tessera_filter::{Access, RecordBlob, RecordValue};
use tessera_spatial::tiler::ScalarType;
use tessera_spatial::Bounds;
use tessera_store::open_bundle;
use tessera_types::IdentityKey;

const TEST_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";
const N: u64 = 40;

/// The three blob-resident generation functions. Source 0 carries none of the three, so its
/// entity must be absent from has-row entirely; source 3 carries the empty string, which is a
/// value and must survive as one.
fn note_of(e: u64) -> Option<String> {
    match e {
        e if e.is_multiple_of(5) => None,
        3 => Some(String::new()),
        e => Some(format!("note-{e}")),
    }
}

/// The same notes without the empty string, for the fixture that declares `note` **filterable**.
///
/// The two differ because the families differ, not to dodge a check. A blob row stores the bytes
/// the wire carried, so the empty string is a value it can hold and this file asserts it does; a
/// `keyword`'s values are dictionary keys, and the empty string is refused as a key at the build
/// (records §4.3) — an unset field and a client bug both produce it, so absence is the null.
fn filterable_note_of(e: u64) -> Option<String> {
    match e {
        e if e.is_multiple_of(5) => None,
        e => Some(format!("note-{e}")),
    }
}

fn score_of(e: u64) -> Option<f64> {
    (!e.is_multiple_of(7)).then_some(e as f64 * 0.5 + 0.25)
}

fn count_of(e: u64) -> Option<i64> {
    (!e.is_multiple_of(3)).then_some((e * 11) as i64)
}

/// A render-only column, present so the test can assert a rendered column contributes nothing to
/// the blob.
fn flag_of(e: u64) -> i64 {
    (e % 2) as i64
}

fn write_points(path: &Path, note: &dyn Fn(u64) -> Option<String>) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("note", DataType::Utf8, true),
        Field::new("score", DataType::Float64, true),
        Field::new("count", DataType::Int64, true),
        Field::new("flag", DataType::Int64, false),
    ]));
    let ids: Vec<u64> = (0..N).collect();
    let xs: Vec<f64> = ids.iter().map(|e| ((e * 37) % 1000) as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|e| ((e * 53) % 1000) as f64).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids.clone())),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
            Arc::new(StringArray::from(
                ids.iter().map(|&e| note(e)).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                ids.iter().map(|&e| score_of(e)).collect::<Vec<_>>(),
            )),
            Arc::new(Int64Array::from(
                ids.iter().map(|&e| count_of(e)).collect::<Vec<_>>(),
            )),
            Arc::new(Int64Array::from(
                ids.iter().map(|&e| Some(flag_of(e))).collect::<Vec<_>>(),
            )),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn write_empty_pairs(path: &Path) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("term_id", DataType::UInt32, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(Vec::<u64>::new())),
            Arc::new(UInt32Array::from(Vec::<u32>::new())),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

/// The declarations, constructed rather than parsed — see the module doc. Tags follow declared
/// position: note 0, score 1, count 2, flag 3.
fn blob_schema() -> Schema {
    let neither = |name: &str, ty: ScalarType| Attribute {
        field: None,
        name: name.to_string(),
        title: None,
        ty,
        analyser: None,
        vocabulary: None,
        value_set: None,
        index: false,
        render: false,
    };
    Schema {
        attributes: vec![
            neither("note", ScalarType::Keyword),
            neither("score", ScalarType::F64),
            neither("count", ScalarType::I64),
            Attribute {
                field: None,
                name: "flag".to_string(),
                title: None,
                ty: ScalarType::I64,
                analyser: None,
                vocabulary: None,
                value_set: None,
                index: false,
                render: true,
            },
        ],
        vocabularies: HashMap::new(),
    }
}

/// The same fixture with every column rendered or filtered: no blob-resident column, so the
/// stage must write nothing and the open must demand nothing.
fn no_blob_schema() -> Schema {
    let mut schema = blob_schema();
    for attribute in &mut schema.attributes {
        match attribute.ty {
            // A string may be filter-only, never rendered.
            ScalarType::Keyword => attribute.index = true,
            _ => attribute.render = true,
        }
    }
    schema
}

fn args(points: &Path, pairs: &Path, out: PathBuf, schema: Schema) -> BuildArgs {
    BuildArgs {
        views: vec![tessera_build::ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: tessera_spatial::Projection::None,
            extent: Bounds {
                x_min: 0.0,
                x_max: 1000.0,
                y_min: 0.0,
                y_max: 1000.0,
            },
            points: points.to_path_buf(),
            point_fields: Default::default(),
            select: None,
            access: tessera_build::config::AccessInput::relation(pairs.to_path_buf()),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: tessera_build::config::AttributeSource::over(
            points.to_path_buf(),
            &schema,
        ),
        out,
        limit: None,
        identity_key: IdentityKey::from_hex(TEST_KEY_HEX).unwrap(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        mint_external_ids: true,
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema,
    }
}

fn build_with(schema: Schema, note: &dyn Fn(u64) -> Option<String>) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let points = dir.path().join("points.parquet");
    let pairs = dir.path().join("pairs.parquet");
    write_points(&points, note);
    write_empty_pairs(&pairs);
    let out = dir.path().join("bundle");
    build(&args(&points, &pairs, out, schema)).expect("the build succeeds");
    dir
}

fn current_prefix(out: &Path) -> String {
    let current: serde_json::Value =
        serde_json::from_slice(&std::fs::read(out.join("CURRENT")).unwrap()).unwrap();
    current["prefix"].as_str().unwrap().to_string()
}

/// Source id → entity id through the external-id sidecar: entity ids are signature-sorted
/// (§11.1), so a source id is emphatically not its own entity id.
fn source_to_entity(out: &Path) -> HashMap<u64, u32> {
    let bundle = open_bundle(out).unwrap();
    let part = bundle.partitions.values().next().unwrap();
    let prefix = current_prefix(out);
    let mut map = HashMap::new();
    for rel in &part.manifest.external_id_runs {
        let path = out.join(&prefix).join(rel);
        let reader =
            arrow::ipc::reader::FileReader::try_new(File::open(&path).unwrap(), None).unwrap();
        for batch in reader {
            let batch = batch.unwrap();
            let ext = batch
                .column(0)
                .as_any()
                .downcast_ref::<BinaryArray>()
                .unwrap();
            let ent = batch
                .column(1)
                .as_any()
                .downcast_ref::<UInt32Array>()
                .unwrap();
            for i in 0..batch.num_rows() {
                let source = u64::from_le_bytes(ext.value(i).try_into().unwrap());
                map.insert(source, ent.value(i));
            }
        }
    }
    map
}

fn record_dir(out: &Path) -> PathBuf {
    let bundle = open_bundle(out).unwrap();
    let phash = bundle.partitions.keys().next().unwrap().clone();
    out.join(current_prefix(out))
        .join("partitions")
        .join(phash)
        .join("attrs")
        .join("record")
}

/// Every entity's blob row equals the fixture's own generation functions — field for field, tag
/// for tag, absence for absence — read through the serving reader. The blob is these columns'
/// only home, so this is the whole of "the record always exists" for the build half.
#[test]
fn the_blob_carries_every_neither_columns_values() {
    let dir = build_with(blob_schema(), &note_of);
    let out = dir.path().join("bundle");
    let entity_of = source_to_entity(&out);
    let blob = RecordBlob::open_dir(&record_dir(&out), Access::Read).expect("the blob opens");
    blob.self_check().expect("the artefact is self-consistent");

    for source in 0..N {
        let entity = entity_of[&source];
        let mut expected = Vec::new();
        if let Some(note) = note_of(source) {
            expected.push((0u16, RecordValue::Utf8(note)));
        }
        if let Some(score) = score_of(source) {
            expected.push((1u16, RecordValue::F64(score)));
        }
        if let Some(count) = count_of(source) {
            expected.push((2u16, RecordValue::I64(count)));
        }
        let got = blob.fields_of(entity).expect("a well-formed read");
        if expected.is_empty() {
            assert_eq!(got, None, "source {source} carries nothing blob-resident");
            continue;
        }
        let got: Vec<(u16, RecordValue)> = got
            .unwrap_or_else(|| panic!("source {source} owes a row"))
            .into_iter()
            .map(|f| (f.tag, f.value))
            .collect();
        assert_eq!(got, expected, "source {source}");
        // The render column never reaches the blob: tag 3 is `flag`'s, and no row carries it.
        assert!(got.iter().all(|(tag, _)| *tag != 3), "source {source}");
    }
}

/// The three files are digested into `MANIFEST.files` like every base artefact, which is what
/// puts them under the digest-or-refuse rule (records §7).
#[test]
fn the_blob_files_are_under_the_manifest_digest() {
    let dir = build_with(blob_schema(), &note_of);
    let out = dir.path().join("bundle");
    let bundle = open_bundle(&out).unwrap();
    let phash = bundle.partitions.keys().next().unwrap();
    for file in ["blocks.bin", "hasrow.roaring", "directory.arrow"] {
        let key = format!("partitions/{phash}/attrs/record/{file}");
        assert!(
            bundle.manifest.files.contains_key(&key),
            "MANIFEST.files covers {key}; it holds {:?}",
            bundle.manifest.files.keys().collect::<Vec<_>>()
        );
    }
}

/// The open rule's digest half: a corrupted `blocks.bin` fails the bundle open, exactly as any
/// digested file does — the blob is not outside the rule every other artefact is under.
#[test]
fn a_corrupted_blob_file_refuses_the_bundle_open() {
    let dir = build_with(blob_schema(), &note_of);
    let out = dir.path().join("bundle");
    let blocks = record_dir(&out).join("blocks.bin");
    let mut bytes = std::fs::read(&blocks).unwrap();
    bytes[0] ^= 0xFF;
    std::fs::write(&blocks, &bytes).unwrap();
    assert!(
        open_bundle(&out).is_err(),
        "a blob file that fails its digest must refuse the open"
    );
}

/// A schema with no blob-resident column writes nothing under `attrs/record/` and the manifest
/// names nothing there: the stage and the open rule are both functions of the compiled schema.
#[test]
fn no_blob_columns_means_no_blob_files() {
    let dir = build_with(no_blob_schema(), &filterable_note_of);
    let out = dir.path().join("bundle");
    assert!(
        !record_dir(&out).exists(),
        "no blob-resident column, so no attrs/record directory"
    );
    let bundle = open_bundle(&out).unwrap();
    assert!(
        !bundle
            .manifest
            .files
            .keys()
            .any(|k| k.contains("attrs/record/")),
        "the manifest must name no blob file"
    );
}

/// Re-digest a prefix-relative file into `MANIFEST.json`, and chase that manifest's own digest
/// into `CURRENT`, so a damage test below fails on the addressing check under test rather than on
/// the digest sweep in front of it. The blob's three files are all named by `MANIFEST.files`, so
/// there is no segments map to consider (`tests/verify_deep.rs` has the version that does).
fn refresh_digest(out: &Path, rel: &str) {
    use sha2::{Digest, Sha256};
    let hex = |bytes: &[u8]| {
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    let prefix_dir = out.join(current_prefix(out));
    let bytes = std::fs::read(prefix_dir.join(rel)).expect("the damaged file reads back");
    let manifest_path = prefix_dir.join("MANIFEST.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    assert!(
        manifest["files"].get(rel).is_some(),
        "{rel} is not named by MANIFEST.files"
    );
    manifest["files"][rel] =
        serde_json::json!({ "size": bytes.len(), "sha256": hex(&bytes) });
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).unwrap();
    std::fs::write(&manifest_path, &manifest_bytes).unwrap();
    std::fs::write(
        out.join("CURRENT"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "prefix": current_prefix(out),
            "manifest_digest": hex(&manifest_bytes),
        }))
        .unwrap(),
    )
    .unwrap();
}

/// **`verify --deep` walks the blob.** Nothing else in the bundle can see the blob's addressing:
/// the file digests match whatever the addressing says, and the read path refuses a defect one
/// request at a time, which means a fold or coalesce that corrupted it would first be noticed by a
/// viewer receiving another entity's record. The deep pass reports the rows it walked, and the
/// count is what says the walk happened rather than finding nothing to do.
#[test]
fn verify_deep_walks_every_blob_row() {
    let dir = build_with(blob_schema(), &note_of);
    let out = dir.path().join("bundle");
    let rows = RecordBlob::open_dir(&record_dir(&out), Access::Read)
        .expect("the blob opens")
        .rows();
    assert!(rows > 0, "the fixture must have a blob for this to mean anything");

    let report = tessera_build::verify_deep(&out, &tessera_build::VerifyOpts::default())
        .expect("a well-formed bundle verifies deep");
    assert_eq!(report.record_rows, rows);
}

/// **And refuses one whose has-row bitmap names a different entity at a rank.** The cardinality is
/// unchanged, so every count agrees and the directory is untouched; what disagrees is which entity
/// the bitmap and the block say a rank belongs to, which is the addressing defect that would serve
/// one entity's record to another. The digest is refreshed so the refusal is the walk's and not
/// the sweep's.
///
/// Mutation killed: dropping the blob from `verify_deep`'s pass, which leaves this bundle
/// verifying clean while `RecordBlob::fields_of` refuses every read of it.
#[test]
fn verify_deep_refuses_a_blob_whose_bitmap_renames_a_rank() {
    use croaring::{Bitmap, Portable};

    let dir = build_with(blob_schema(), &note_of);
    let out = dir.path().join("bundle");
    let rel = format!(
        "partitions/{}/attrs/record/{}",
        open_bundle(&out)
            .unwrap()
            .partitions
            .keys()
            .next()
            .unwrap()
            .clone(),
        tessera_filter::RECORD_HASROW_FILE
    );
    let hasrow_path = record_dir(&out).join(tessera_filter::RECORD_HASROW_FILE);
    let mut bitmap =
        Bitmap::try_deserialize::<Portable>(&std::fs::read(&hasrow_path).unwrap()).unwrap();
    // Move the last member up by one. Nothing else has that id — the build's entity space stops
    // at N — so the cardinality holds and only that rank's entity changes.
    let last = bitmap.maximum().expect("the blob holds rows");
    bitmap.remove(last);
    bitmap.add(last + 1);
    std::fs::write(&hasrow_path, bitmap.serialize::<Portable>()).unwrap();
    refresh_digest(&out, &rel);

    let err = tessera_build::verify_deep(&out, &tessera_build::VerifyOpts::default())
        .expect_err("the deep pass must refuse a blob whose bitmap and blocks disagree");
    let message = err.to_string();
    assert!(
        message.contains("entity") && message.contains("rank"),
        "the refusal must name the disagreement, got: {message}"
    );
}
