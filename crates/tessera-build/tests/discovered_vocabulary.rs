//! Build-level tests for `vocabulary = "discovered"` (issue #82's build half).
//!
//! `schema.rs`'s unit tests cover the parse-time rules (the refusal lifted, the empty-start
//! case, the relaxed `listing = "public"` combination). These exercise the whole build: minting
//! through `tessera_store::vocabulary::VocabularyMinter`, the batch-level pre-pass in
//! `input::scan_attributes`, and the result landing in `MANIFEST.vocabularies`.

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{BinaryArray, Float64Array, StringArray, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema as ArrowSchema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use tessera_build::schema::Schema;
use tessera_build::{build, build_in_memory, BuildArgs};
use tessera_spatial::Bounds;
use tessera_store::{open_bundle, ScalarSlice};
use tessera_types::{EntityId, IdentityKey};

const TEST_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";

fn test_key() -> IdentityKey {
    IdentityKey::from_hex(TEST_KEY_HEX).unwrap()
}

fn extent() -> Bounds {
    Bounds {
        x_min: 0.0,
        x_max: 1000.0,
        y_min: 0.0,
        y_max: 1000.0,
    }
}

/// `points.parquet` with `entity_id`, `x`, `y`, and a nullable `department` utf8 column —
/// `department_of(e)` gives item `e`'s key, or `None` for an absent value.
fn write_points_with_department(
    path: &Path,
    n: u64,
    department_of: impl Fn(u64) -> Option<String>,
) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("department", DataType::Utf8, true),
    ]));
    let ids: Vec<u64> = (0..n).collect();
    let xs: Vec<f64> = ids.iter().map(|e| ((e * 37) % 1000) as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|e| ((e * 53) % 1000) as f64).collect();
    let departments: Vec<Option<String>> = ids.iter().map(|&e| department_of(e)).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
            Arc::new(StringArray::from(departments)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

/// `pairs.parquet` with no rows: every item carries an empty signature. The discovered-vocabulary
/// machinery under test has nothing to do with the label side, and an empty relation is already a
/// supported shape (see `build_equivalence.rs`'s `synth_terms` group 0).
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

/// A `values_key` seed file: `key`/`code` columns, no `label` (§4.4).
fn write_vocabulary_seed(path: &Path, values: &[(&str, u32)]) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("key", DataType::Utf8, false),
        Field::new("code", DataType::UInt32, false),
    ]));
    let keys: Vec<&str> = values.iter().map(|(k, _)| *k).collect();
    let codes: Vec<u32> = values.iter().map(|(_, c)| *c).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from(keys)),
            Arc::new(UInt32Array::from(codes)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn parse_schema(text: &str, values: &HashMap<String, PathBuf>) -> Schema {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("schema.toml");
    std::fs::write(&path, text).unwrap();
    Schema::parse(&path, values).expect("schema parses")
}

fn args(points: &Path, pairs: &Path, out: PathBuf, schema: Schema) -> BuildArgs {
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
        layers: None,
        artifacts: None,
        artifact_members: None,
        mint_external_ids: true,
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema,
    }
}

/// Every `(source_id, entity_id)` pair, read from a built bundle's external-id sidecar — the
/// join a test needs to find which row a source item landed at, since entity IDs are
/// signature-sorted rather than source order (§11.1).
fn source_to_entity(out: &Path) -> HashMap<u64, u64> {
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
                map.insert(source, ent.value(i) as u64);
            }
        }
    }
    map
}

fn current_prefix(out: &Path) -> String {
    let current: serde_json::Value =
        serde_json::from_slice(&std::fs::read(out.join("CURRENT")).unwrap()).unwrap();
    current["prefix"].as_str().unwrap().to_string()
}

/// The `department` **key** a built bundle records for `source_id`, decoded through that
/// bundle's own `MANIFEST.vocabularies` — `None` if the row carries the absent code.
fn department_key_of(
    out: &Path,
    source_id: u64,
    entity_of_source: &HashMap<u64, u64>,
) -> Option<String> {
    let bundle = open_bundle(out).unwrap();
    let part = bundle.partitions.values().next().unwrap();
    let slice = &part.slices["s0"];
    let entity = entity_of_source[&source_id];
    let row = slice
        .row_space
        .row_of(EntityId::new(entity))
        .expect("every entity has a row")
        .raw() as usize;
    let segment = &slice.segments[0];
    let code = match segment
        .columns
        .scalar("department")
        .expect("the segment carries a department column")
    {
        ScalarSlice::U8(values) => values[row] as u32,
        ScalarSlice::U16(values) => values[row] as u32,
        ScalarSlice::U32(values) => values[row],
        other => panic!("unexpected scalar kind for department: {other:?}"),
    };
    if code == 0 {
        return None;
    }
    let vocab = bundle
        .manifest
        .vocabularies
        .iter()
        .find(|v| v.name == "department")
        .expect("MANIFEST.vocabularies carries 'department'");
    Some(
        vocab
            .values
            .iter()
            .find(|v| v.code == code)
            .unwrap_or_else(|| panic!("code {code} has no key in MANIFEST.vocabularies"))
            .key
            .clone(),
    )
}

const DISCOVERED_NO_SEED: &str = r#"
[[attribute]]
name = "department"
type = "category"
width = "u16"
render = true
vocabulary = "discovered"
listing = "per_viewer"
"#;

/// The refusal is gone, and a build over data with novel keys mints one code per distinct key,
/// all recorded in `MANIFEST.vocabularies`.
#[test]
fn discovered_vocabulary_mints_every_novel_key_and_records_it() {
    let temp = tempfile::tempdir().unwrap();
    let points = temp.path().join("points.parquet");
    let pairs = temp.path().join("pairs.parquet");
    let out = temp.path().join("bundle");

    const N: u64 = 40;
    let keys = ["eng", "sales", "ops", "finance", "legal"];
    // e % 7 == 0 is absent (null): the absent path must stay code 0 beside a minted vocabulary.
    let department_of = |e: u64| -> Option<String> {
        if e.is_multiple_of(7) {
            None
        } else {
            Some(keys[(e % keys.len() as u64) as usize].to_string())
        }
    };
    write_points_with_department(&points, N, department_of);
    write_empty_pairs(&pairs);

    let schema = parse_schema(DISCOVERED_NO_SEED, &HashMap::new());
    build(&args(&points, &pairs, out.clone(), schema)).expect("build succeeds");

    let bundle = open_bundle(&out).unwrap();
    let vocab = bundle
        .manifest
        .vocabularies
        .iter()
        .find(|v| v.name == "department")
        .expect("MANIFEST.vocabularies carries 'department'");
    assert_eq!(vocab.listing, tessera_store::manifest::Listing::PerViewer);
    assert!(vocab.reserved.is_empty());
    let mut got_keys: Vec<&str> = vocab.values.iter().map(|v| v.key.as_str()).collect();
    got_keys.sort_unstable();
    let mut want_keys: Vec<&str> = keys.to_vec();
    want_keys.sort_unstable();
    assert_eq!(
        got_keys, want_keys,
        "every distinct key must be minted exactly once"
    );
    // Every code is nonzero (0 is reserved for absent) and within the declared u16 width.
    for value in &vocab.values {
        assert_ne!(value.code, 0);
        assert!(value.code <= u32::from(u16::MAX));
    }
    // No two keys share a code.
    let mut codes: Vec<u32> = vocab.values.iter().map(|v| v.code).collect();
    codes.sort_unstable();
    let before = codes.len();
    codes.dedup();
    assert_eq!(codes.len(), before, "no two keys may share a code");

    // And the rows agree with the source data, decoded back through the minted codes.
    let entity_of_source = source_to_entity(&out);
    for e in 0..N {
        assert_eq!(
            department_key_of(&out, e, &entity_of_source),
            department_of(e),
            "item {e}'s stored department must decode back to what the corpus supplied"
        );
    }
}

/// §3.4's whole point: dense first-seen codes make a visible code a lower bound on vocabulary
/// cardinality. Minting many keys into a wide (`u16`) vocabulary must not produce `1..=k`.
#[test]
fn minted_codes_are_not_dense() {
    let temp = tempfile::tempdir().unwrap();
    let points = temp.path().join("points.parquet");
    let pairs = temp.path().join("pairs.parquet");
    let out = temp.path().join("bundle");

    const N: u64 = 300;
    const K: u64 = 200;
    write_points_with_department(&points, N, |e| Some(format!("dept-{}", e % K)));
    write_empty_pairs(&pairs);

    let schema = parse_schema(DISCOVERED_NO_SEED, &HashMap::new());
    build(&args(&points, &pairs, out.clone(), schema)).expect("build succeeds");

    let bundle = open_bundle(&out).unwrap();
    let vocab = bundle
        .manifest
        .vocabularies
        .iter()
        .find(|v| v.name == "department")
        .unwrap();
    assert_eq!(vocab.values.len() as u64, K);
    let mut codes: Vec<u32> = vocab.values.iter().map(|v| v.code).collect();
    codes.sort_unstable();
    let dense: Vec<u32> = (1..=K as u32).collect();
    assert_ne!(
        codes, dense,
        "codes 1..k make a visible code a lower bound on vocabulary cardinality"
    );
    // The probability of 200 uniform draws from 65,535 all landing below 512 is ~0.
    assert!(
        codes.iter().any(|&c| c > 511),
        "a draw confined to the low bytes is not uniform over the declared u16 width: {codes:?}"
    );
}

/// §4.4: a `values_key` seed pins its codes verbatim, and the build mints only for the keys the
/// seed does not carry.
#[test]
fn a_values_key_seed_pins_codes_and_mints_only_the_rest() {
    let temp = tempfile::tempdir().unwrap();
    let points = temp.path().join("points.parquet");
    let pairs = temp.path().join("pairs.parquet");
    let out = temp.path().join("bundle");
    let seed_path = temp.path().join("seed.parquet");

    // Deliberately high codes, nowhere near a first-fit draw's likely range, so a build that
    // silently re-derived them (rather than pinning them) would probably be caught by a
    // different assertion too.
    write_vocabulary_seed(&seed_path, &[("eng", 4000), ("sales", 5000)]);
    write_points_with_department(&points, 30, |e| {
        Some(["eng", "sales", "ops"][(e % 3) as usize].to_string())
    });
    write_empty_pairs(&pairs);

    let text = r#"
[[attribute]]
name = "department"
type = "category"
width = "u16"
render = true
vocabulary = "discovered"
listing = "per_viewer"
values_key = "dept_seed"
"#;
    let mut values = HashMap::new();
    values.insert("dept_seed".to_string(), seed_path);
    let schema = parse_schema(text, &values);
    build(&args(&points, &pairs, out.clone(), schema)).expect("build succeeds");

    let bundle = open_bundle(&out).unwrap();
    let vocab = bundle
        .manifest
        .vocabularies
        .iter()
        .find(|v| v.name == "dept_seed")
        .expect("a values_key-bound vocabulary is named for the key, not the attribute");
    let code_of = |key: &str| vocab.values.iter().find(|v| v.key == key).map(|v| v.code);
    assert_eq!(
        code_of("eng"),
        Some(4000),
        "a seeded key's code is pinned verbatim"
    );
    assert_eq!(
        code_of("sales"),
        Some(5000),
        "a seeded key's code is pinned verbatim"
    );
    let ops_code = code_of("ops").expect("the novel key 'ops' must have been minted");
    assert_ne!(ops_code, 4000);
    assert_ne!(ops_code, 5000);
    assert_eq!(
        vocab.values.len(),
        3,
        "exactly the seeded pair plus the one minted key"
    );
}

/// §5's declare-then-use is unconditional for a **declared** vocabulary: an unknown key is still
/// a build failure, not an auto-mint, however discovered vocabularies now behave.
#[test]
fn a_declared_vocabulary_still_refuses_an_unknown_key() {
    let temp = tempfile::tempdir().unwrap();
    let points = temp.path().join("points.parquet");
    let pairs = temp.path().join("pairs.parquet");
    let out = temp.path().join("bundle");

    write_points_with_department(&points, 10, |e| {
        Some(["eng", "sales", "ops"][(e % 3) as usize].to_string())
    });
    write_empty_pairs(&pairs);

    let text = r#"
[[attribute]]
name = "department"
type = "category"
width = "u8"
render = true
vocabulary = "declared"
listing = "per_viewer"
  [attribute.values]
  eng = 1
  sales = 2
"#;
    let schema = parse_schema(text, &HashMap::new());
    let err = build(&args(&points, &pairs, out, schema)).expect_err("an unknown key must refuse");
    let message = format!("{err}");
    assert!(
        message.contains("does not list") && message.contains("ops"),
        "the refusal must name the unknown key: {message}"
    );
}

/// §3.6: never widen and never wrap. A `u8` vocabulary holds 255 usable values (code 0 is
/// reserved); the 256th distinct key must refuse with a typed error naming the column and width,
/// not panic and not silently wrap.
#[test]
fn exhaustion_at_build_is_a_typed_error_naming_column_and_width() {
    let temp = tempfile::tempdir().unwrap();
    let points = temp.path().join("points.parquet");
    let pairs = temp.path().join("pairs.parquet");
    let out = temp.path().join("bundle");

    const N: u64 = 256; // one distinct key per item -> 256 distinct keys, one too many for u8.
    write_points_with_department(&points, N, |e| Some(format!("k{e}")));
    write_empty_pairs(&pairs);

    let text = r#"
[[attribute]]
name = "department"
type = "category"
width = "u8"
render = true
vocabulary = "discovered"
listing = "per_viewer"
"#;
    let schema = parse_schema(text, &HashMap::new());
    let err = build(&args(&points, &pairs, out, schema)).expect_err("256 keys exhaust a u8 space");
    let message = format!("{err}");
    assert!(
        message.contains("department"),
        "must name the column: {message}"
    );
    assert!(message.contains("u8"), "must name the width: {message}");
    assert!(
        message.contains("full at 255 assigned codes"),
        "must wrap MintError::Exhausted's own message, not restate it: {message}"
    );
}

/// **A retired code is never re-minted** (§3.4's `reserved`).
///
/// `Schema::discovered_minters` seeds each minter's assigned set with the vocabulary's `reserved`
/// list as well as its pinned values, and this is the only test that proves it: the minter's own
/// unit tests in `tessera-store` exercise `seed_reserved` directly, so deleting the `reserved` loop
/// from the *build's* seeding leaves every one of them green while every later build quietly
/// re-issues a retired code to a new key. Rows written under the retirement and rows written under
/// the new key would then be the same colour, with no error and no digest mismatch.
///
/// Constructed so the draw has no room to be lucky: 250 of a `u8`'s 255 usable codes are retired
/// and one is pinned, so the four remaining keys can only land on the four free codes.
#[test]
fn a_retired_code_is_never_minted_to_a_new_key() {
    let temp = tempfile::tempdir().unwrap();
    let points = temp.path().join("points.parquet");
    let pairs = temp.path().join("pairs.parquet");
    let out = temp.path().join("bundle");

    // `pinned` takes code 1; codes 2..=251 are retired; 252..=255 are the only free codes.
    let retired: Vec<String> = (2..=251u32).map(|c| c.to_string()).collect();
    let text = format!(
        r#"
[[attribute]]
name = "department"
type = "category"
width = "u8"
render = true
vocabulary = "discovered"
listing = "per_viewer"

[attribute.values]
pinned = 1
reserved = [{}]
"#,
        retired.join(", ")
    );

    write_points_with_department(&points, 40, |e| {
        Some(["pinned", "alpha", "beta", "gamma", "delta"][(e % 5) as usize].to_string())
    });
    write_empty_pairs(&pairs);

    let schema = parse_schema(&text, &HashMap::new());
    build(&args(&points, &pairs, out.clone(), schema)).expect("four free codes are enough");

    let bundle = open_bundle(&out).unwrap();
    let vocab = bundle
        .manifest
        .vocabularies
        .iter()
        .find(|v| v.name == "department")
        .expect("the discovered vocabulary reaches the manifest");

    let minted: std::collections::BTreeSet<u32> = vocab
        .values
        .iter()
        .filter(|v| v.key != "pinned")
        .map(|v| v.code)
        .collect();
    assert_eq!(
        minted,
        (252..=255u32).collect::<std::collections::BTreeSet<_>>(),
        "the four novel keys can only have taken the four codes no retirement and no pin held"
    );
    assert_eq!(
        vocab
            .values
            .iter()
            .find(|v| v.key == "pinned")
            .unwrap()
            .code,
        1,
        "a pinned code is not re-drawn either"
    );
}

/// `listing = "public"` with a discovered vocabulary now builds (warns rather than refuses) —
/// the end-to-end counterpart of `schema.rs`'s parse-level test.
#[test]
fn public_listing_with_a_discovered_vocabulary_builds() {
    let temp = tempfile::tempdir().unwrap();
    let points = temp.path().join("points.parquet");
    let pairs = temp.path().join("pairs.parquet");
    let out = temp.path().join("bundle");

    write_points_with_department(&points, 10, |e| {
        Some(["eng", "sales"][(e % 2) as usize].to_string())
    });
    write_empty_pairs(&pairs);

    let text = r#"
[[attribute]]
name = "department"
type = "category"
width = "u8"
render = true
vocabulary = "discovered"
listing = "public"
"#;
    let schema = parse_schema(text, &HashMap::new());
    build(&args(&points, &pairs, out, schema)).expect("public + discovered builds now");
}

/// **Both build implementations must agree, in the presence of fresh minting.**
///
/// A fresh mint draws from OS entropy (`VocabularyMinter::mint`), so two independent builds over
/// the same input do NOT produce the same codes for the same keys — that is expected and correct
/// (codes are pinned at first build, never reproduced), and it is exactly why this test cannot
/// use `build_equivalence.rs`'s byte-for-byte helper for a schema that mints. What must still
/// hold, and what this asserts, is that the two implementations mint the *same set of keys* and
/// agree on *which key* every row carries, however their numeric codes differ.
#[test]
fn both_implementations_agree_on_keys_though_fresh_codes_differ() {
    let temp = tempfile::tempdir().unwrap();
    let points = temp.path().join("points.parquet");
    let pairs = temp.path().join("pairs.parquet");
    write_points_with_department(&points, 60, |e| {
        if e.is_multiple_of(11) {
            None
        } else {
            Some(["eng", "sales", "ops", "finance"][(e % 4) as usize].to_string())
        }
    });
    write_empty_pairs(&pairs);

    let reference_out = temp.path().join("reference");
    let streaming_out = temp.path().join("streaming");
    let schema_a = parse_schema(DISCOVERED_NO_SEED, &HashMap::new());
    let schema_b = parse_schema(DISCOVERED_NO_SEED, &HashMap::new());
    build_in_memory(&args(&points, &pairs, reference_out.clone(), schema_a))
        .expect("reference builds");
    build(&args(&points, &pairs, streaming_out.clone(), schema_b)).expect("streaming builds");

    let reference_vocab = {
        let bundle = open_bundle(&reference_out).unwrap();
        bundle
            .manifest
            .vocabularies
            .iter()
            .find(|v| v.name == "department")
            .unwrap()
            .clone()
    };
    let streaming_vocab = {
        let bundle = open_bundle(&streaming_out).unwrap();
        bundle
            .manifest
            .vocabularies
            .iter()
            .find(|v| v.name == "department")
            .unwrap()
            .clone()
    };
    let mut reference_keys: Vec<&str> = reference_vocab
        .values
        .iter()
        .map(|v| v.key.as_str())
        .collect();
    let mut streaming_keys: Vec<&str> = streaming_vocab
        .values
        .iter()
        .map(|v| v.key.as_str())
        .collect();
    reference_keys.sort_unstable();
    streaming_keys.sort_unstable();
    assert_eq!(
        reference_keys, streaming_keys,
        "both implementations must mint exactly the same set of distinct keys"
    );

    // Row-for-row, by source id (entity assignment itself is already proven identical between
    // the two implementations by build_equivalence.rs; this test only needs each bundle's own
    // key-level content to agree, not the codes).
    let reference_entities = source_to_entity(&reference_out);
    let streaming_entities = source_to_entity(&streaming_out);
    for e in 0..60u64 {
        assert_eq!(
            department_key_of(&reference_out, e, &reference_entities),
            department_key_of(&streaming_out, e, &streaming_entities),
            "item {e} must carry the same department key in both bundles"
        );
    }
}
