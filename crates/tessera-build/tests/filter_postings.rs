//! The build half of `index = true`: one keyed postings file per declared column
//! (filter-index §2.2, §2.5, §4).
//!
//! `schema.rs`'s unit tests cover the parse rules. These read what the build actually wrote,
//! through the reader that will serve it — `tessera_filter::ColumnPostings` — because the two
//! halves agreeing on the record format is the whole content of this seam, and a test that
//! re-implemented the decode would agree with itself.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{BinaryArray, Float64Array, StringArray, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema as ArrowSchema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use tessera_build::schema::Schema;
use tessera_build::{build, BuildArgs};
use tessera_filter::{ColumnPostings, ValueColumn};
use tessera_spatial::Bounds;
use tessera_store::open_bundle;
use tessera_types::{AttrLocalId, IdentityKey};

const TEST_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";
const N: u64 = 40;

/// `alpha` / `beta` / `gamma` in a fixed rotation, with every seventh item carrying no value at
/// all — the absent case, which must appear in no posting rather than in whichever one code zero
/// would name.
fn department_of(e: u64) -> Option<String> {
    if e.is_multiple_of(7) {
        return None;
    }
    Some(["alpha", "beta", "gamma"][(e % 3) as usize].to_string())
}

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

fn write_points(path: &Path) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("department", DataType::Utf8, true),
    ]));
    let ids: Vec<u64> = (0..N).collect();
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

/// `title` is distinct per item, which is the shape an inverted string index handles worst and a
/// scan handles identically to any other.
fn title_of(e: u64) -> String {
    format!("paper-{e}")
}

/// Points carrying both a category and a per-item string.
fn write_points_with_title(path: &Path) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("department", DataType::Utf8, true),
        Field::new("title", DataType::Utf8, true),
    ]));
    let ids: Vec<u64> = (0..N).collect();
    let xs: Vec<f64> = ids.iter().map(|e| ((e * 37) % 1000) as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|e| ((e * 53) % 1000) as f64).collect();
    let departments: Vec<Option<String>> = ids.iter().map(|&e| department_of(e)).collect();
    // Item 2 carries **no value at all**, so presence is partial and the slot addressing has a
    // hole in it — the shape a column whose every item carried a value would never exercise.
    let titles: Vec<Option<String>> = ids.iter().map(|&e| (e != 2).then(|| title_of(e))).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
            Arc::new(StringArray::from(departments)),
            Arc::new(StringArray::from(titles)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

/// Every item carries a value — the universal-presence case.
fn write_points_dense(path: &Path) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("department", DataType::Utf8, true),
    ]));
    let ids: Vec<u64> = (0..N).collect();
    let xs: Vec<f64> = ids.iter().map(|e| ((e * 37) % 1000) as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|e| ((e * 53) % 1000) as f64).collect();
    let departments: Vec<Option<String>> = ids
        .iter()
        .map(|&e| Some(["alpha", "beta", "gamma"][(e % 3) as usize].to_string()))
        .collect();
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

fn parse_schema(text: &str) -> Schema {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("schema.toml");
    std::fs::write(&path, text).unwrap();
    Schema::parse(&path, &HashMap::new()).expect("schema parses")
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

fn current_prefix(out: &Path) -> String {
    let current: serde_json::Value =
        serde_json::from_slice(&std::fs::read(out.join("CURRENT")).unwrap()).unwrap();
    current["prefix"].as_str().unwrap().to_string()
}

/// Source id → entity id, through the external-id sidecar. Entity ids are signature-sorted
/// (§11.1), so a source id is emphatically not its own entity id and a test that assumed so
/// would compare the right sets against the wrong items.
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

/// The code the built bundle bound each vocabulary key to. Read from the manifest rather than
/// assumed: codes are minted scattered, so nothing in the build guarantees `alpha` is 1.
fn codes_of(out: &Path, column: &str) -> HashMap<String, u32> {
    let bundle = open_bundle(out).unwrap();
    let vocabulary = bundle
        .manifest
        .vocabularies
        .iter()
        .find(|v| v.name == column)
        .expect("the manifest records the column's vocabulary");
    vocabulary
        .values
        .iter()
        .map(|v| (v.key.clone(), v.code))
        .collect()
}

fn column_dir(out: &Path, column: &str) -> PathBuf {
    let bundle = open_bundle(out).unwrap();
    let phash = bundle.partitions.keys().next().unwrap().clone();
    out.join(current_prefix(out))
        .join("partitions")
        .join(phash)
        .join("attrs")
        .join(column)
}

fn postings_path(out: &Path, column: &str) -> PathBuf {
    let bundle = open_bundle(out).unwrap();
    let phash = bundle.partitions.keys().next().unwrap().clone();
    out.join(current_prefix(out))
        .join("partitions")
        .join(phash)
        .join("attrs")
        .join(column)
        .join("postings.arrow")
}

const FILTER_SCHEMA: &str = r#"
[[attribute]]
name = "department"
type = "category"
width = "u16"
render = true
index = true
vocabulary = "discovered"
listing = "per_viewer"
"#;

const RENDER_ONLY_SCHEMA: &str = r#"
[[attribute]]
name = "department"
type = "category"
width = "u16"
render = true
vocabulary = "discovered"
listing = "per_viewer"
"#;

const PUBLIC_RENDER_ONLY_SCHEMA: &str = r#"
[[attribute]]
name = "department"
type = "category"
width = "u16"
render = true
vocabulary = "declared"
listing = "public"
  [attribute.values]
  alpha = 11
  beta = 22
  gamma = 33
"#;

fn build_with(schema_text: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let points = dir.path().join("points.parquet");
    let pairs = dir.path().join("pairs.parquet");
    write_points(&points);
    write_empty_pairs(&pairs);
    let out = dir.path().join("bundle");
    build(&args(&points, &pairs, out, parse_schema(schema_text))).expect("the build succeeds");
    dir
}

/// Every value's posting is exactly the set of entities carrying it — resolved through the
/// source→entity map, since the build reorders items.
#[test]
fn each_value_posting_is_the_entities_that_carry_it() {
    let dir = build_with(FILTER_SCHEMA);
    let out = dir.path().join("bundle");
    let entity_of = source_to_entity(&out);
    let codes = codes_of(&out, "department");

    let column = ColumnPostings::open_keyed(&postings_path(&out, "department")).unwrap();
    for key in ["alpha", "beta", "gamma"] {
        let expected: HashSet<u32> = (0..N)
            .filter(|&e| department_of(e).as_deref() == Some(key))
            .map(|e| entity_of[&e])
            .collect();
        assert!(!expected.is_empty(), "the fixture exercises '{key}'");
        let got: HashSet<u32> = column
            .entities(AttrLocalId::new(codes[key]))
            .unwrap()
            .iter()
            .collect();
        assert_eq!(got, expected, "posting for '{key}'");
    }
}

/// An item with no value is in no posting at all. The entity-major buffer the emit reads is
/// initialised to zero, which is also the reserved absent code — so this is the assertion that
/// separates "carries nothing" from "carries whatever code zero names".
#[test]
fn an_absent_value_is_in_no_posting() {
    let dir = build_with(FILTER_SCHEMA);
    let out = dir.path().join("bundle");
    let entity_of = source_to_entity(&out);
    let codes = codes_of(&out, "department");

    let column = ColumnPostings::open_keyed(&postings_path(&out, "department")).unwrap();
    let mut union = croaring::Bitmap::new();
    for &code in codes.values() {
        union |= column.entities(AttrLocalId::new(code)).unwrap();
    }
    for source in (0..N).filter(|e| department_of(*e).is_none()) {
        assert!(
            !union.contains(entity_of[&source]),
            "source {source} carries no department and must appear in no posting"
        );
    }
    assert_eq!(
        union.cardinality(),
        (0..N).filter(|e| department_of(*e).is_some()).count() as u64
    );
}

/// The reserved absent code is not a key. A keyed file drops empty postings rather than writing
/// them (filter-index §6), so this reads as the empty set rather than as an error.
#[test]
fn the_absent_code_is_not_a_key() {
    let dir = build_with(FILTER_SCHEMA);
    let out = dir.path().join("bundle");
    let column = ColumnPostings::open_keyed(&postings_path(&out, "department")).unwrap();
    assert!(column.entities(AttrLocalId::new(0)).unwrap().is_empty());
}

/// The manifest digests the postings file. Without this the artefact is outside the
/// digest-or-refuse rule every other bundle file is under, and a corrupted one opens silently.
#[test]
fn the_postings_file_is_under_the_manifest_digest() {
    let dir = build_with(FILTER_SCHEMA);
    let out = dir.path().join("bundle");
    let bundle = open_bundle(&out).unwrap();
    let phash = bundle.partitions.keys().next().unwrap();
    let key = format!("partitions/{phash}/attrs/department/postings.arrow");
    assert!(
        bundle.manifest.files.contains_key(&key),
        "MANIFEST.files covers {key}; it holds {:?}",
        bundle.manifest.files.keys().collect::<Vec<_>>()
    );
}

/// A `per_viewer` category gets postings whatever its `index` says, because the listing control
/// is membership-derived and the member sets *are* these postings (per-point-attributes §3.3).
/// `RENDER_ONLY_SCHEMA` declares `listing = "per_viewer"` without `filter`, so this is the case.
#[test]
fn per_viewer_owes_postings_without_a_filter_declaration() {
    let dir = build_with(RENDER_ONLY_SCHEMA);
    let out = dir.path().join("bundle");
    let column = ColumnPostings::open_keyed(&postings_path(&out, "department")).unwrap();
    let codes = codes_of(&out, "department");
    assert!(!column
        .entities(AttrLocalId::new(codes["alpha"]))
        .unwrap()
        .is_empty());
}

/// A `public` category with no `filter` declaration emits nothing. Postings are a placement, not a
/// consequence of being a category: a `public` value set is served as authored, so no membership is
/// derived and a build that indexed every category would pay for an artefact nothing reads.
#[test]
fn a_public_render_only_column_emits_no_postings() {
    let dir = build_with(PUBLIC_RENDER_ONLY_SCHEMA);
    let out = dir.path().join("bundle");
    assert!(!postings_path(&out, "department").exists());
}

/// **The value column is the record and the postings are derived from it**, so the two must agree
/// exactly. This is the property that lets a deployment build the accelerator or not and answer
/// identically (filter-index §2.3) — and the one a future change to either writer would break
/// silently, since both files would still open and still answer.
#[test]
fn the_derived_postings_agree_with_the_value_column() {
    let dir = build_with(FILTER_SCHEMA);
    let out = dir.path().join("bundle");
    let cdir = column_dir(&out, "department");
    let column = ValueColumn::open_dir(&cdir, tessera_filter::Access::Mapped).unwrap();
    let postings = ColumnPostings::open_keyed(&postings_path(&out, "department")).unwrap();

    // Every entity, so the scan's candidate excludes nothing.
    let mut all = croaring::Bitmap::new();
    all.add_range(0u32..N as u32);

    for (_key, code) in codes_of(&out, "department") {
        let scanned = column.scan_eq(&all, AttrLocalId::new(code));
        let indexed = postings.entities(AttrLocalId::new(code)).unwrap();
        assert_eq!(
            scanned.iter().collect::<Vec<_>>(),
            indexed.iter().collect::<Vec<_>>(),
            "column and postings disagree for code {code}"
        );
    }
}

/// `entity → value` is the direction an inverted index cannot answer. Having it is why the
/// conformance oracle can read the artefact under test rather than a parallel relation, and why
/// substring matching needs no trigram index to verify against (filter-index §9, §1.1).
#[test]
fn the_column_answers_entity_to_value() {
    let dir = build_with(FILTER_SCHEMA);
    let out = dir.path().join("bundle");
    let entity_of = source_to_entity(&out);
    let codes = codes_of(&out, "department");
    let cdir = column_dir(&out, "department");
    let column = ValueColumn::open_dir(&cdir, tessera_filter::Access::Mapped).unwrap();

    for source in 0..N {
        let entity = entity_of[&source];
        match department_of(source) {
            Some(key) => assert_eq!(
                column.value_of(entity),
                Some(AttrLocalId::new(codes[&key])),
                "source {source}"
            ),
            None => assert_eq!(
                column.value_of(entity),
                None,
                "source {source} carries none"
            ),
        }
    }
}

/// A column every entity carries a value in writes **no** presence bitmap: the entity id is then
/// the array index, which is the scan's fast path (measured 28.7 ms against 1,078 ms at 10⁹).
#[test]
fn a_universal_column_writes_no_presence_bitmap() {
    let dir = tempfile::tempdir().unwrap();
    let points = dir.path().join("points.parquet");
    let pairs = dir.path().join("pairs.parquet");
    // Every item carries a department: no absent code anywhere.
    write_points_dense(&points);
    write_empty_pairs(&pairs);
    let out = dir.path().join("bundle");
    build(&args(
        &points,
        &pairs,
        out.clone(),
        parse_schema(FILTER_SCHEMA),
    ))
    .unwrap();

    let cdir = column_dir(&out, "department");
    assert!(cdir.join("values.arrow").exists());
    assert!(
        !cdir.join("presence.roaring").exists(),
        "a universal column must not write a presence bitmap"
    );
    let column = ValueColumn::open_dir(&cdir, tessera_filter::Access::Mapped).unwrap();
    assert_eq!(column.present().cardinality(), N);
}

const STRING_SCHEMA: &str = r#"
[[attribute]]
name = "department"
type = "category"
width = "u16"
render = true
vocabulary = "discovered"
listing = "public"

[[attribute]]
name = "title"
type = "keyword"
index = true
"#;

/// A keyword column is its ordinal column and its dictionary, and no accelerator beside them: no
/// FST, no postings (filter-index §2.3; records §4.3). Decision 0067's per-term postings are a
/// separate, ruled addition and are not derived here.
///
/// The predicates themselves are not a build test's to make: `eq`, `prefix` and `contains` over a
/// keyword go through the dictionary, which is the engine's read route (`filtering.rs`) and the
/// conformance differential's. What a build owes is the pair those read — asserted here and, value
/// by value, in `keyword_column.rs`.
#[test]
fn a_string_filter_column_emits_its_pair_and_no_postings() {
    let dir = tempfile::tempdir().unwrap();
    let points = dir.path().join("points.parquet");
    let pairs = dir.path().join("pairs.parquet");
    write_points_with_title(&points);
    write_empty_pairs(&pairs);
    let out = dir.path().join("bundle");
    build(&args(
        &points,
        &pairs,
        out.clone(),
        parse_schema(STRING_SCHEMA),
    ))
    .unwrap();

    let cdir = column_dir(&out, "title");
    assert!(cdir.join("values.arrow").exists());
    assert!(
        !cdir.join("postings.arrow").exists(),
        "a string column earns no accelerator: its values neither repeat densely nor carry an \
         identity a posting could be keyed by"
    );
    assert!(
        cdir.join(tessera_filter::DICT_FILE).exists(),
        "a keyword's ordinals are positions in the dictionary written beside them"
    );
}

/// A declaration with neither `render` nor `index` builds, and the manifest records it with
/// both flags false — the blob-resident home is derived from the two flags, never stored
/// (records §3; `DeclaredScalar`'s rule). ⊘ The record blob itself is not built here: until its
/// build stage lands, the declaration is the column's only artefact, so the assertion is that
/// the build accepts it, compiles it, and materialises nothing under `attrs/` for it.
const BLOB_RESIDENT_SCHEMA: &str = r#"
[[attribute]]
name = "department"
type = "category"
width = "u16"
render = true
vocabulary = "discovered"
listing = "public"

[[attribute]]
name = "title"
type = "keyword"
"#;

#[test]
fn a_declaration_with_neither_key_builds_and_the_manifest_records_it_blob_resident() {
    let dir = tempfile::tempdir().unwrap();
    let points = dir.path().join("points.parquet");
    let pairs = dir.path().join("pairs.parquet");
    write_points_with_title(&points);
    write_empty_pairs(&pairs);
    let out = dir.path().join("bundle");
    build(&args(
        &points,
        &pairs,
        out.clone(),
        parse_schema(BLOB_RESIDENT_SCHEMA),
    ))
    .unwrap();

    let bundle = open_bundle(&out).unwrap();
    let scalar_of = |name: &str| {
        bundle
            .manifest
            .declared_scalars
            .iter()
            .find(|s| s.name == name)
            .expect("every declared column reaches the manifest, whatever its flags")
    };
    let title = scalar_of("title");
    assert!(!title.index, "neither key compiles to `index = false`");
    assert!(!title.render, "neither key compiles to `render = false`");
    let department = scalar_of("department");
    assert!(department.render && !department.index);

    assert!(
        !column_dir(&out, "title").exists(),
        "a blob-resident column owes no entity-space files; writing any would be the old \
         surface's double store coming back unasked"
    );
}

/// The build's own writer schema omits a filter-only column, and the manifest records the
/// placement so the *serving* path can narrow the same way. Asserted against a real built bundle
/// rather than a constructed manifest: the two halves agreeing is the whole point, and a merge
/// whose schema named `title` would refuse this very segment.
#[test]
fn the_manifest_records_the_placement_the_tail_was_built_from() {
    let dir = tempfile::tempdir().unwrap();
    let points = dir.path().join("points.parquet");
    let pairs = dir.path().join("pairs.parquet");
    write_points_with_title(&points);
    write_empty_pairs(&pairs);
    let out = dir.path().join("bundle");
    build(&args(
        &points,
        &pairs,
        out.clone(),
        parse_schema(STRING_SCHEMA),
    ))
    .unwrap();

    let bundle = open_bundle(&out).unwrap();
    let declared: Vec<&str> = bundle
        .manifest
        .declared_scalars
        .iter()
        .map(|d| d.name.as_str())
        .collect();
    let tail: Vec<&str> = bundle
        .manifest
        .render_scalars()
        .map(|d| d.name.as_str())
        .collect();

    // Both columns are declared — the ingest plane supplies values for each.
    assert_eq!(declared, vec!["department", "title"]);
    // Only one is in the tail, and it is exactly what the segment carries.
    assert_eq!(tail, vec!["department"]);
    let slice = &bundle.partitions.values().next().unwrap().slices["s0"];
    let columns = &slice.segments[0].columns;
    assert!(columns.scalar("department").is_some());
    assert!(columns.scalar("title").is_none());
}

/// **A `filter`-only column is not in the hot column.** §10.3 routes by access cadence: per-query
/// data lives in entity space, per-mark data in `columns.arrow`. Writing a filter-only column into
/// the tail as well would spend a slot in every row — and for a `keyword` column that slot would
/// hold either the value's bytes or a per-layer ordinal, which is exactly what `render` on
/// `keyword` is refused for.
#[test]
fn a_filter_only_column_is_absent_from_the_hot_column() {
    let dir = tempfile::tempdir().unwrap();
    let points = dir.path().join("points.parquet");
    let pairs = dir.path().join("pairs.parquet");
    write_points_with_title(&points);
    write_empty_pairs(&pairs);
    let out = dir.path().join("bundle");
    build(&args(
        &points,
        &pairs,
        out.clone(),
        parse_schema(STRING_SCHEMA),
    ))
    .unwrap();

    let bundle = open_bundle(&out).unwrap();
    let slice = &bundle.partitions.values().next().unwrap().slices["s0"];
    let columns = &slice.segments[0].columns;
    assert!(
        columns.scalar("department").is_some(),
        "a render column is in the tail"
    );
    assert!(
        columns.scalar("title").is_none(),
        "a filter-only column must not occupy a slot in every row"
    );
}
