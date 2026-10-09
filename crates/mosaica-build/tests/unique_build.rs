//! **A build's unique columns**: every type `unique` applies to builds its index, and `mosaica
//! verify --deep` agrees the index with the column; a row holding a value an earlier row holds is
//! refused, reported and left out; `unique` on a type it does not apply to, or on a group-scoped
//! column, is refused at the declaration; and a damaged index is refused by the deep verifier.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{
    ArrayRef, Float64Array, Int16Array, Int64Array, StringArray, TimestampMicrosecondArray,
    UInt32Array, UInt64Array, UInt8Array,
};
use arrow::datatypes::{DataType, Field, Schema, TimeUnit};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use sha2::{Digest, Sha256};

use mosaica_build::{build, verify_deep, BuildArgs, BuildReport, VerifyOpts};
use mosaica_spatial::Bounds;
use mosaica_store::manifest::{CurrentPointer, SegmentsManifest};
use mosaica_types::IdentityKey;

const TEST_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";
const N: u64 = 700;

fn extent() -> Bounds {
    Bounds {
        x_min: 0.0,
        x_max: 1000.0,
        y_min: 0.0,
        y_max: 1000.0,
    }
}

/// Points with one column per type `unique` applies to. Every column is distinct over the items,
/// with nulls on its own stride; `repeat` sets one item's values to another's, so every column
/// holds one value twice.
fn write_points(path: &Path, repeat: bool) {
    let ids: Vec<u64> = (0..N).collect();
    let source = |e: u64| if repeat && e == 9 { 4 } else { e };
    let column = |name: &str, ty: DataType, array: ArrayRef| (Field::new(name, ty, true), array);
    let columns: Vec<(Field, ArrayRef)> = vec![
        (
            Field::new("entity_id", DataType::UInt64, false),
            Arc::new(UInt64Array::from(ids.clone())) as ArrayRef,
        ),
        (
            Field::new("x", DataType::Float64, false),
            Arc::new(Float64Array::from(
                ids.iter().map(|e| ((e * 37) % 1000) as f64).collect::<Vec<_>>(),
            )),
        ),
        (
            Field::new("y", DataType::Float64, false),
            Arc::new(Float64Array::from(
                ids.iter().map(|e| ((e * 53) % 1000) as f64).collect::<Vec<_>>(),
            )),
        ),
        column(
            "doi",
            DataType::Utf8,
            Arc::new(StringArray::from(
                ids.iter()
                    .map(|e| {
                        let s = source(*e);
                        (e % 11 != 0).then(|| format!("10.{s}/{}", "x".repeat((s % 50) as usize)))
                    })
                    .collect::<Vec<_>>(),
            )),
        ),
        column(
            "small",
            DataType::UInt8,
            Arc::new(UInt8Array::from(
                ids.iter()
                    .map(|e| (*e < 250).then_some(source(*e) as u8))
                    .collect::<Vec<_>>(),
            )),
        ),
        column(
            "wide",
            DataType::UInt64,
            Arc::new(UInt64Array::from(
                ids.iter()
                    .map(|e| (e % 13 != 0).then_some(u64::MAX - source(*e) * 1_000_003))
                    .collect::<Vec<_>>(),
            )),
        ),
        column(
            "signed",
            DataType::Int16,
            Arc::new(Int16Array::from(
                ids.iter()
                    .map(|e| Some(source(*e) as i16 - 350))
                    .collect::<Vec<_>>(),
            )),
        ),
        column(
            "big",
            DataType::Int64,
            Arc::new(Int64Array::from(
                ids.iter()
                    .map(|e| Some(i64::MIN + source(*e) as i64 * 7))
                    .collect::<Vec<_>>(),
            )),
        ),
        column(
            "at",
            DataType::Timestamp(TimeUnit::Microsecond, None),
            Arc::new(TimestampMicrosecondArray::from(
                ids.iter()
                    .map(|e| (e % 7 != 0).then_some(-1_000_000 + source(*e) as i64 * 997))
                    .collect::<Vec<_>>(),
            )),
        ),
    ];
    let schema = Arc::new(Schema::new(
        columns.iter().map(|(f, _)| f.clone()).collect::<Vec<_>>(),
    ));
    let batch =
        RecordBatch::try_new(schema.clone(), columns.into_iter().map(|(_, a)| a).collect())
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
    let ids: Vec<u64> = (0..N).collect();
    let terms: Vec<u32> = ids.iter().map(|e| (e % 3) as u32).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(UInt64Array::from(ids)), Arc::new(UInt32Array::from(terms))],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

/// Every allowed type, at each home a value can have: `doi` in the record blob, `small` rendered,
/// `wide` indexed, the rest in the blob or rendered.
const SCHEMA: &str = r#"
[[attribute]]
name   = "doi"
type   = "keyword"
unique = true

[[attribute]]
name   = "small"
type   = "u8"
render = true
unique = true

[[attribute]]
name   = "wide"
type   = "u64"
index  = true
unique = true

[[attribute]]
name   = "signed"
type   = "i16"
unique = true

[[attribute]]
name   = "big"
type   = "i64"
index  = true
unique = true

[[attribute]]
name   = "at"
type   = "timestamp_us"
unique = true

[[attribute]]
name   = "id"
type   = "u64"
unique = true
field  = "entity_id"
"#;

fn parse(dir: &Path, toml: &str) -> Result<mosaica_build::config::Schema, String> {
    let path = dir.join("schema.toml");
    fs::write(&path, toml).unwrap();
    mosaica_build::config::Config::parse(&path, &Default::default())
        .map(|config| config.schema)
        .map_err(|e| e.to_string())
}

fn build_in(dir: &Path, repeat: bool, streaming: bool) -> Result<PathBuf, String> {
    build_reported(dir, repeat, streaming).map(|(out, _)| out)
}

/// [`build_in`], keeping the build's report.
fn build_reported(
    dir: &Path,
    repeat: bool,
    streaming: bool,
) -> Result<(PathBuf, BuildReport), String> {
    let points = dir.join("points.parquet");
    let pairs = dir.join("pairs.parquet");
    write_points(&points, repeat);
    write_pairs(&pairs);
    let schema = parse(dir, SCHEMA).expect("the schema parses");
    let out = dir.join("bundle");
    let args = BuildArgs {
        views: vec![mosaica_build::ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: mosaica_spatial::Projection::None,
            extent: extent(),
            points: points.clone(),
            point_fields: Default::default(),
            select: None,
            access: mosaica_build::config::AccessInput::relation(pairs),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: mosaica_build::config::AttributeSource::over(points, &schema),
        out: out.clone(),
        limit: None,
        strict: false,
        identity_key: IdentityKey::from_hex(TEST_KEY_HEX).unwrap(),
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema,
    };
    let built = match streaming {
        true => build(&args),
        false => mosaica_build::build_in_memory(&args),
    };
    built.map(|report| (out, report)).map_err(|e| e.to_string())
}

fn segments_manifest(root: &Path) -> SegmentsManifest {
    serde_json::from_slice(
        &fs::read(root.join("v00000/partitions/default/SEGMENTS-0.json")).unwrap(),
    )
    .unwrap()
}

/// **Every type `unique` applies to builds its index, and the deep verifier agrees it with the
/// column**, whether the value lives in the record blob, the row tail or an indexed column.
#[test]
fn every_unique_type_builds_an_index_the_deep_verifier_accepts() {
    let temp = tempfile::tempdir().unwrap();
    let root = build_in(temp.path(), false, true).expect("a build with distinct values");
    let indexes = segments_manifest(&root).unique_indexes;
    let names: Vec<&str> = indexes.iter().map(|i| i.attribute.as_str()).collect();
    assert_eq!(names, ["doi", "small", "wide", "signed", "big", "at", "id"]);
    assert!(indexes
        .iter()
        .all(|i| i.live.is_empty() && !i.base.is_empty()));
    let report = verify_deep(&root, &VerifyOpts::default()).expect("the bundle verifies deep");
    // Every value but the nulls: doi misses every 11th, small holds 250, wide misses every 13th,
    // signed, big and id hold every item, at misses every 7th.
    let held = |every: u64| N - N.div_ceil(every);
    let expected = held(11) + 250 + held(13) + N + N + held(7) + N;
    assert_eq!(report.unique_entries, expected);
}

/// **The linear build writes the same index as the streaming one**, bytes and all.
#[test]
fn the_linear_build_writes_the_same_index() {
    let streaming = tempfile::tempdir().unwrap();
    let linear = tempfile::tempdir().unwrap();
    let a = build_in(streaming.path(), false, true).unwrap();
    let b = build_in(linear.path(), false, false).unwrap();
    let (ia, ib) = (segments_manifest(&a).unique_indexes, segments_manifest(&b).unique_indexes);
    assert_eq!(ia, ib);
    for index in &ia {
        for rel in index.files() {
            assert_eq!(
                fs::read(a.join("v00000").join(rel)).unwrap(),
                fs::read(b.join("v00000").join(rel)).unwrap(),
                "{rel}"
            );
        }
    }
}

/// **A row holding a value an earlier row holds is refused, reported and left out**, on both
/// build paths: item 9's row repeats item 4's values, so the bundle holds every other item.
#[test]
fn a_value_held_twice_is_reported_and_left_out() {
    for streaming in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let (_, report) = build_reported(temp.path(), true, streaming).expect("the build goes on");
        let refused: Vec<(&str, &str, u64)> = report
            .refused
            .iter()
            .map(|entry| (entry.object.as_str(), entry.reason.as_str(), entry.rows))
            .collect();
        // The pairs row naming item 9 by its id names no item, since the item was never created.
        assert_eq!(
            refused,
            [
                ("view 's0'", "one_value_twice", 1),
                ("point_visibility", "names_no_item", 1)
            ],
            "streaming {streaming}"
        );
        assert_eq!(report.items, N - 1, "streaming {streaming}");
    }
}

/// **`unique` applies to keyword, integer and timestamp columns scoped to the item**, and the
/// declaration refuses it anywhere else.
#[test]
fn unique_on_another_type_is_refused_at_the_declaration() {
    let temp = tempfile::tempdir().unwrap();
    for ty in ["f32", "f64", "bool", "text"] {
        let toml = format!("[[attribute]]\nname = \"c\"\ntype = \"{ty}\"\nunique = true\n");
        assert!(parse(temp.path(), &toml).is_err(), "{ty}");
    }
    let category = r#"
[[vocabulary]]
name = "v"
width = "u8"
value_set = "closed"
visibility = "public"
values = ["a"]

[[attribute]]
name = "c"
type = "category"
vocabulary = "v"
unique = true
"#;
    assert!(parse(temp.path(), category).is_err());
    for ty in ["keyword", "u8", "u16", "u32", "u64", "i8", "i16", "i32", "i64", "timestamp_us"] {
        let toml = format!("[[attribute]]\nname = \"c\"\ntype = \"{ty}\"\nunique = true\n");
        assert!(parse(temp.path(), &toml).is_ok(), "{ty}");
    }
}

fn hex_sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// Rewrite one file's digest in `MANIFEST.json`, and `CURRENT` after it, so the damage below is
/// found by the structural check rather than the digest.
fn refresh_digest(root: &Path, rel: &str) {
    let prefix = root.join("v00000");
    let bytes = fs::read(prefix.join(rel)).unwrap();
    let manifest_path = prefix.join("MANIFEST.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest["files"][rel] = serde_json::json!({ "size": bytes.len(), "sha256": hex_sha256(&bytes) });
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).unwrap();
    fs::write(&manifest_path, &manifest_bytes).unwrap();
    let current = CurrentPointer {
        prefix: "v00000".to_string(),
        manifest_digest: hex_sha256(&manifest_bytes),
    };
    fs::write(root.join("CURRENT"), serde_json::to_vec_pretty(&current).unwrap()).unwrap();
}

/// The path of `attribute`'s first base run, prefix-relative, and on disc.
fn first_run(root: &Path, attribute: &str) -> (String, PathBuf) {
    let index = segments_manifest(root)
        .unique_indexes
        .into_iter()
        .find(|i| i.attribute == attribute)
        .unwrap();
    let rel = index.base[0].path.clone();
    let path = root.join("v00000").join(&rel);
    (rel, path)
}

/// **A run whose bytes no longer match the manifest is refused.**
#[test]
fn a_run_failing_its_digest_is_refused_by_the_deep_verifier() {
    let temp = tempfile::tempdir().unwrap();
    let root = build_in(temp.path(), false, true).unwrap();
    let (_, path) = first_run(&root, "signed");
    let mut damaged = fs::read(&path).unwrap();
    damaged[4096 + 9] ^= 0x01;
    fs::write(&path, &damaged).unwrap();
    assert!(verify_deep(&root, &VerifyOpts::default()).is_err());
}

/// **An index whose pages are intact but whose entries name another entity than the column says
/// holds the value is refused**, for a column in the record blob and one rendered alone.
#[test]
fn an_index_disagreeing_with_its_column_is_refused_by_the_deep_verifier() {
    for attribute in ["signed", "small"] {
        let temp = tempfile::tempdir().unwrap();
        let root = build_in(temp.path(), false, true).unwrap();
        let (rel, path) = first_run(&root, attribute);
        let mut entries: Vec<(u64, u32)> = Vec::new();
        let run = mosaica_store::key_index::KeyRun::<u64>::open(&path).unwrap();
        for entry in run.iter() {
            entries.push(entry.unwrap());
        }
        drop(run);
        entries[1].1 = entries[2].1;
        entries.sort_unstable();
        entries.dedup();
        fs::remove_file(&path).unwrap();
        let dir = path.parent().unwrap();
        let stem = path.file_stem().unwrap().to_str().unwrap().trim_end_matches("-0");
        let mut writer = mosaica_store::key_index::KeyRunWriter::<u64>::create(
            dir,
            stem,
            std::num::NonZeroU64::new(u64::MAX).unwrap(),
        );
        for (key, entity) in entries {
            writer.push(key, entity).unwrap();
        }
        writer.finish().unwrap();
        refresh_digest(&root, &rel);
        assert!(
            verify_deep(&root, &VerifyOpts::default()).is_err(),
            "the index of '{attribute}' disagrees with its column"
        );
    }
}
