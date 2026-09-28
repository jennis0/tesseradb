//! Attributes read from **more than one file**, joined on the join field and reported on
//! (`configuration.md` §1's `[sources]` and `[defaults]`, §8's coverage report).
//!
//! A column read from a second file, joined on a column that file spells differently, lands on
//! exactly the items it names and on no others.
//!
//! **Every item the build creates has a row in each attribute source**, as a new item at a running
//! service carries every declared column; a null in that row is how the source says it has no
//! value. A source that leaves an item out is refused naming it. Rows naming entities this build
//! did not load are ignored, which is the ordinary case for a table covering a superset. The cases
//! below drive each through the real declaration parser, so the surface and the pass are exercised
//! together.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{Float64Array, Int64Array, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema as ArrowSchema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use tessera_build::config::Config;
use tessera_build::{build, build_in_memory, BuildArgs};
use tessera_filter::{Access, RecordBlob, RecordValue};
use tessera_spatial::Bounds;
use tessera_store::open_bundle;
use tessera_types::IdentityKey;

mod common;

const TEST_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";
const N: u64 = 40;

/// The points file: the join field `id`, geometry, and one column of its own.
fn write_points(path: &Path) {
    write_points_counting(path, Count::Values(|e| Some((e * 11) as i64)));
}

/// How a points file carries `count`.
enum Count {
    Omitted,
    Values(fn(u64) -> Option<i64>),
    /// A column of Arrow's `null` type.
    NullType,
}

/// [`write_points`] with `count` carried as `count` says.
fn write_points_counting(path: &Path, count: Count) {
    let mut fields = vec![
        Field::new("id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
    ];
    let ids: Vec<u64> = (0..N).collect();
    let mut columns: Vec<arrow::array::ArrayRef> = vec![
        Arc::new(UInt64Array::from(ids.clone())),
        Arc::new(Float64Array::from(
            ids.iter()
                .map(|e| ((e * 37) % 1000) as f64)
                .collect::<Vec<_>>(),
        )),
        Arc::new(Float64Array::from(
            ids.iter()
                .map(|e| ((e * 53) % 1000) as f64)
                .collect::<Vec<_>>(),
        )),
    ];
    match count {
        Count::Omitted => {}
        Count::Values(count) => {
            fields.push(Field::new("count", DataType::Int64, true));
            columns.push(Arc::new(Int64Array::from(
                ids.iter().map(|&e| count(e)).collect::<Vec<_>>(),
            )));
        }
        Count::NullType => {
            fields.push(Field::new("count", DataType::Null, true));
            columns.push(Arc::new(arrow::array::NullArray::new(ids.len())));
        }
    }
    let schema = Arc::new(ArrowSchema::new(fields));
    let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

/// A second file, keyed on a column it calls `doc_id`, with a row for each of `ids` carrying
/// `score` where `covered` holds the id and null elsewhere.
fn write_scores(path: &Path, ids: &[u64], covered: &[u64]) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("doc_id", DataType::UInt64, false),
        Field::new("score", DataType::Float64, true),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids.to_vec())),
            Arc::new(Float64Array::from(
                ids.iter()
                    .map(|&e| covered.contains(&e).then(|| score_of(e)))
                    .collect::<Vec<_>>(),
            )),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn score_of(e: u64) -> f64 {
    e as f64 * 0.5 + 0.25
}

fn write_empty_pairs(path: &Path) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("id", DataType::UInt64, false),
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

/// Two sources, two join columns, no column placed anywhere but the record blob — which is what
/// lets the values be read back per entity without a serving path.
const DECLARATION: &str = r#"
[sources]
points = "points.parquet"
scores = "scores.parquet"
pairs  = "pairs.parquet"

[defaults]
source     = "points"
join_field = "id"

[[view]]
name             = "s0"
extent           = { min = 0.0, max = 1000.0 }
point_visibility = { source = "pairs", default = "public" }

[[attribute]]
name = "count"
type = "i64"

[[attribute]]
name   = "score"
type   = "f64"
source = "scores"
fields = { id = "doc_id" }

[[attribute]]
name   = "id"
type   = "u64"
unique = true
"#;

/// Write the declaration and both data files into `dir`, with `scores` holding a row for each of
/// `ids`, valued where `covered` holds the id.
fn project(dir: &Path, ids: &[u64], covered: &[u64]) -> Config {
    write_points(&dir.join("points.parquet"));
    write_scores(&dir.join("scores.parquet"), ids, covered);
    write_empty_pairs(&dir.join("pairs.parquet"));
    let path = dir.join("schema.toml");
    std::fs::write(&path, DECLARATION).unwrap();
    Config::parse(&path, &HashMap::new()).expect("the declaration parses")
}

fn args(config: &Config, out: PathBuf) -> BuildArgs {
    let acquired = config.acquire().expect("the declaration acquires");
    let registry = config.build_views().expect("the registry compiles");
    let acquired_view =
        tessera_build::config::acquire_view(&registry[0]).expect("the view acquires its inputs");
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
            points: acquired_view.points,
            point_fields: acquired_view.point_fields,
            select: None,
            access: acquired_view.access,
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: acquired.attribute_sources,
        out,
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
        schema: config.schema.clone(),
    }
}

fn current_prefix(out: &Path) -> String {
    let current: serde_json::Value =
        serde_json::from_slice(&std::fs::read(out.join("CURRENT")).unwrap()).unwrap();
    current["prefix"].as_str().unwrap().to_string()
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

/// Every entity's blob row, keyed by **source** id, as `(tag, value)` pairs. Tag 0 is `count` and
/// tag 1 is `score` — declared position, which is what the tail is stored by.
fn rows_by_source(out: &Path) -> BTreeMap<u64, Vec<(u16, RecordValue)>> {
    let entity_of = common::entities_of(out, "id", 0..N);
    let blob = RecordBlob::open_dir(&record_dir(out), Access::Read).expect("the blob opens");
    blob.self_check().expect("the artefact is self-consistent");
    let mut rows = BTreeMap::new();
    for (source, entity) in entity_of {
        let read: Vec<(u16, RecordValue)> = blob
            .fields_of(entity)
            .expect("a well-formed read")
            .unwrap_or_default()
            .into_iter()
            .map(|f| (f.tag, f.value))
            .collect();
        rows.insert(source, read);
    }
    rows
}

/// **A column from a second file lands on exactly the entities that file names.** The join is the
/// join field — spelled `doc_id` there and `id` here — and nothing about the file decides which
/// entity space a value belongs to.
#[test]
fn a_column_from_a_second_source_lands_on_the_entities_it_names() {
    let dir = tempfile::tempdir().unwrap();
    let covered: Vec<u64> = (0..N).filter(|e| e.is_multiple_of(3)).collect();
    let all: Vec<u64> = (0..N).collect();
    let config = project(dir.path(), &all, &covered);
    let out = dir.path().join("bundle");
    build(&args(&config, out.clone())).expect("a two-source build succeeds");

    let rows = rows_by_source(&out);
    for source in 0..N {
        let fields = &rows[&source];
        assert_eq!(
            fields.iter().find(|(t, _)| *t == 0).map(|(_, v)| v.clone()),
            Some(RecordValue::I64((source * 11) as i64)),
            "the points file's own column, for source {source}"
        );
        let score = fields.iter().find(|(t, _)| *t == 1).map(|(_, v)| v.clone());
        if covered.contains(&source) {
            assert_eq!(
                score,
                Some(RecordValue::F64(score_of(source))),
                "source {source} is named by the scores file"
            );
        } else {
            assert_eq!(score, None, "source {source} is named by no score row");
        }
    }
}

/// **Every declared column is required.** A points file without `count` is refused, and one that
/// carries `count` as nulls, typed or of Arrow's `null` type, builds with no entity holding a
/// value.
#[test]
fn a_declared_column_the_file_omits_is_refused_and_a_column_of_nulls_builds() {
    let dir = tempfile::tempdir().unwrap();
    let all: Vec<u64> = (0..N).collect();
    let config = project(dir.path(), &all, &[]);
    let points = dir.path().join("points.parquet");

    write_points_counting(&points, Count::Omitted);
    let out = dir.path().join("omitted");
    assert!(build(&args(&config, out.clone())).is_err());
    assert!(!out.join("CURRENT").is_file(), "a refused build writes no bundle");

    for (name, count) in [("nulls", Count::Values(|_| None)), ("null-type", Count::NullType)] {
        write_points_counting(&points, count);
        let out = dir.path().join(name);
        build(&args(&config, out.clone())).expect("a column of nulls builds");
        let rows = rows_by_source(&out);
        assert_eq!(rows.len(), N as usize, "{name}");
        assert!(
            rows.values().all(|fields| fields.iter().all(|(t, _)| *t != 0)),
            "{name}"
        );
    }
}

/// **A source naming entities this build did not load is ignored, not refused.** That is what a
/// join does, and it is the ordinary shape of a table covering a superset of one build's corpus.
#[test]
fn rows_naming_entities_this_build_did_not_load_are_ignored() {
    let dir = tempfile::tempdir().unwrap();
    // A row for each of this build's entities, half of them valued, and a thousand ids no build
    // here ever assigns.
    let mut covered: Vec<u64> = (0..N).filter(|e| e.is_multiple_of(2)).collect();
    covered.extend(1_000..2_000);
    let ids: Vec<u64> = (0..N).chain(1_000..2_000).collect();
    let config = project(dir.path(), &ids, &covered);
    let out = dir.path().join("bundle");
    build(&args(&config, out.clone())).expect("unmatched rows are ignored, never refused");

    let rows = rows_by_source(&out);
    for source in 0..N {
        let score = rows[&source]
            .iter()
            .find(|(t, _)| *t == 1)
            .map(|(_, v)| v.clone());
        assert_eq!(
            score,
            source
                .is_multiple_of(2)
                .then(|| RecordValue::F64(score_of(source))),
            "source {source}"
        );
    }
}

/// **A source without a row for an item the build creates is refused, at both builds**, naming
/// the item, as a new item at a running service that leaves out a declared column is. The same
/// item with a row of nulls builds.
#[test]
fn a_source_without_a_row_for_an_item_the_build_creates_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let rows: Vec<u64> = (0..N - 1).collect();
    let config = project(dir.path(), &rows, &rows);
    for (name, result) in [
        ("streamed", build(&args(&config, dir.path().join("streamed")))),
        ("linear", build_in_memory(&args(&config, dir.path().join("linear")))),
    ] {
        let err = result.expect_err("an item with no row in `scores` is refused");
        assert!(err.to_string().contains(&format!("item {}", N - 1)), "{name}: {err}");
    }

    let all: Vec<u64> = (0..N).collect();
    let config = project(dir.path(), &all, &rows);
    build(&args(&config, dir.path().join("nulls"))).expect("a row of nulls builds");
}

/// The two builds must agree byte for byte here as everywhere: the streaming pipeline's per-source
/// merge sweep and the linear build's per-source probe are two implementations of one join, and a
/// disagreement about which entity a value landed on is exactly the defect with no symptom.
#[test]
fn both_build_paths_place_a_second_sources_values_identically() {
    let dir = tempfile::tempdir().unwrap();
    let covered: Vec<u64> = (0..N).filter(|e| !e.is_multiple_of(4)).collect();
    let all: Vec<u64> = (0..N).collect();
    let config = project(dir.path(), &all, &covered);

    let streamed = dir.path().join("streamed");
    let linear = dir.path().join("linear");
    build(&args(&config, streamed.clone())).expect("the streaming build succeeds");
    build_in_memory(&args(&config, linear.clone())).expect("the linear build succeeds");

    assert_eq!(rows_by_source(&streamed), rows_by_source(&linear));
}
