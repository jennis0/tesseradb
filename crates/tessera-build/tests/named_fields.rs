//! The readers take the names the declaration resolved (`configuration.md` §8).
//!
//! `config`'s own tests cover the map's *validation* — every name known, every name declared. What
//! is here needs a data file open: that a moved name reaches the reader and is what it reads, and
//! that a declared field the file does not carry is a refusal rather than an empty column. The
//! second is the one that matters: an absent column is silent in both directions that count, since
//! absent geometry puts every point at the origin and an absent access column puts every point in
//! no principal's mask.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{Float64Array, StringArray, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema as ArrowSchema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use tessera_build::config::{Config, Fields};
use tessera_build::{build, BuildArgs};
use tessera_spatial::Bounds;
use tessera_types::IdentityKey;

const TEST_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";
const N: u64 = 40;

fn extent() -> Bounds {
    Bounds {
        x_min: 0.0,
        x_max: 1000.0,
        y_min: 0.0,
        y_max: 1000.0,
    }
}

/// A points file whose every column is spelled the way a producer chose rather than the way this
/// build names it: `id`, `u`, `v`, and a `dept` column for the attribute pass.
fn write_moved_points(path: &Path) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("id", DataType::UInt64, false),
        Field::new("u", DataType::Float64, false),
        Field::new("v", DataType::Float64, false),
        Field::new("dept", DataType::Utf8, true),
    ]));
    let ids: Vec<u64> = (0..N).collect();
    let xs: Vec<f64> = ids.iter().map(|e| ((e * 37) % 1000) as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|e| ((e * 53) % 1000) as f64).collect();
    let depts: Vec<Option<String>> = ids.iter().map(|e| Some(format!("d{}", e % 3))).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
            Arc::new(StringArray::from(depts)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn write_pairs(path: &Path) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("term_id", DataType::UInt32, false),
    ]));
    let ids: Vec<u64> = (0..N).collect();
    let terms: Vec<u32> = ids.iter().map(|e| (e % 4) as u32).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(UInt32Array::from(terms)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

/// One view, one open vocabulary and one category over it, all read from moved names.
///
/// **The identity column is moved once, in `[defaults]`.** The points file spells it `id`, so the
/// view and the attribute both join on `id` without either saying so — which is what
/// `entity_id_field` is for. The exploded relation is not reached by it and keeps the canonical
/// `(entity_id, term_id)` (`configuration.md` §8), which is what the pairs file here carries.
const MOVED: &str = r#"
[sources]
points = "points.parquet"
pairs  = "pairs.parquet"

[defaults]
source          = "points"
entity_id_field = "id"

[[view]]
name             = "s0"
fields           = { x = "u", y = "v" }
extent           = { min = 0.0, max = 1000.0 }
point_visibility = { source = "pairs", default = "public" }

[[vocabulary]]
name       = "departments"
width      = "u16"
value_set  = "open"
visibility = "public"

[[attribute]]
name       = "department"
field      = "dept"
type       = "category"
vocabulary = "departments"
render     = true
"#;

fn args(dir: &Path, config: &Config, out: PathBuf) -> BuildArgs {
    let acquired = config.acquire().expect("the view acquires its inputs");
    let registry = config.build_views().expect("the registry compiles");
    let acquired_view =
        tessera_build::config::acquire_view(&registry[0]).expect("the view acquires its inputs");
    let _ = dir;
    BuildArgs {
        views: vec![tessera_build::ViewArgs {
            view_id: "s0".to_string(),
            projection: tessera_spatial::Projection::None,
            extent: extent(),
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
        schema: config.schema.clone(),
    }
}

fn parse(dir: &Path, text: &str) -> Config {
    let path = dir.join("config.toml");
    std::fs::write(&path, text).unwrap();
    Config::parse(&path, &Default::default()).expect("the declaration parses")
}

/// Identity, geometry and an attribute column, none of them spelled canonically, all read.
#[test]
fn a_build_reads_the_columns_the_declaration_named() {
    let dir = tempfile::tempdir().unwrap();
    write_moved_points(&dir.path().join("points.parquet"));
    write_pairs(&dir.path().join("pairs.parquet"));
    let config = parse(dir.path(), MOVED);
    let report =
        build(&args(dir.path(), &config, dir.path().join("out"))).expect("the build reads");
    assert_eq!(report.items, N);
    // Three departments, minted from a column the canonical name would never have found.
    let bundle = tessera_store::open_bundle(&dir.path().join("out")).expect("the bundle opens");
    let vocabulary = bundle
        .manifest
        .vocabularies
        .iter()
        .find(|v| v.name == "departments")
        .expect("the vocabulary is compiled");
    assert_eq!(vocabulary.values.len(), 3, "{:?}", vocabulary.values);
}

/// **The refusal a parser cannot make.** A name the declaration gave that the file does not carry
/// is a build failure naming the object, the field, the column looked for and the columns the file
/// has — never an empty column read on in silence.
#[test]
fn a_declared_field_absent_from_the_source_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    write_moved_points(&dir.path().join("points.parquet"));
    write_pairs(&dir.path().join("pairs.parquet"));
    let config = parse(dir.path(), &MOVED.replace("y = \"v\"", "y = \"w\""));
    let message = format!(
        "{}",
        build(&args(dir.path(), &config, dir.path().join("out"))).expect_err("expected a refusal")
    );
    assert!(message.contains("view 's0'"), "the object: {message}");
    assert!(message.contains("field `y`"), "the field: {message}");
    assert!(message.contains("'w'"), "the name looked for: {message}");
    assert!(
        message.contains("id, u, v, dept"),
        "the names the file carries: {message}"
    );
}

/// The same rule for an attribute, whose one-field map is spelled `field` rather than in a table.
#[test]
fn an_attribute_field_absent_from_the_corpus_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    write_moved_points(&dir.path().join("points.parquet"));
    write_pairs(&dir.path().join("pairs.parquet"));
    let config = parse(
        dir.path(),
        &MOVED.replace("field      = \"dept\"", "field      = \"team\""),
    );
    let message = format!(
        "{}",
        build(&args(dir.path(), &config, dir.path().join("out"))).expect_err("expected a refusal")
    );
    assert!(message.contains("attribute 'department'"), "{message}");
    assert!(message.contains("'team'"), "{message}");
    assert!(message.contains("id, u, v, dept"), "{message}");
}

/// A moved geometry name is an assertion about the shape, so a miss on it is a refusal rather
/// than a quiet fall through to a Morton column sitting there under its canonical name.
#[test]
fn a_moved_geometry_name_does_not_fall_through_to_the_other_shape() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("points.parquet");
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("morton", DataType::UInt64, false),
    ]));
    let ids: Vec<u64> = (0..N).collect();
    let codes: Vec<u64> = ids.iter().map(|e| e * 3).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(UInt64Array::from(codes)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(&path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();

    let fields = Fields::moved("view 's0'", [("x", "u"), ("y", "v")]);
    let message = format!(
        "{}",
        tessera_build::input::read_points(
            &path,
            &fields,
            tessera_spatial::Projection::None,
            &extent(),
            None,
                None,
    )
        .expect_err("expected a refusal")
    );
    assert!(message.contains("field `x`"), "{message}");
    assert!(message.contains("entity_id, morton"), "{message}");
}
