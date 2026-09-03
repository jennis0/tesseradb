//! **A build with no points is a bundle, not a refusal** ([decision
//! 0091](../../../docs/decisions/0091-build-is-ingest-into-an-empty-database.md)).
//!
//! A deployment must be able to start from a bundle with no points, with its frame stated, and
//! take the whole corpus in through `/control/ingest`. Until 2026-09-03 both builds refused
//! `n = 0` outright — *"no points selected — a bundle with no items has no expressible entity
//! range"* — which made *ingest into an empty database* a state the system could not be put in
//! through its own tools, and so made 0091's own headline untestable
//! (`tessera-server/tests/membership_column.rs` says so in its module doc, and now has the
//! wholly-ingested arm the refusal used to block).
//!
//! What the zero-item bundle must be is *ordinary*: empty segments, empty postings, a dictionary
//! holding only what the declaration mints, `entity_id_high_water = 0`, every declared column
//! present and empty, every declared layer registered with no artifacts, and the manifests and
//! digests as for any other bundle. So the assertions here are the same ones the populated
//! fixtures make, read off a bundle whose row count happens to be nothing.
//!
//! **`extent = "auto"` over no rows stays a refusal** and is asserted here too: a frame cannot be
//! fitted to nothing, and the message already names the remedy (state the box). That is the one
//! build-only refusal 0091 permits, because it is about *acquisition* — where rows come from —
//! rather than about what a bundle can mean.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{ArrayRef, Float64Array, StringArray, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use tessera_build::{build, build_in_memory, BuildArgs};
use tessera_spatial::Bounds;
use tessera_store::read::open_bundle;
use tessera_types::IdentityKey;

const TEST_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";

fn extent() -> Bounds {
    Bounds {
        x_min: 0.0,
        x_max: 1000.0,
        y_min: 0.0,
        y_max: 1000.0,
    }
}

fn write(path: &Path, schema: Arc<Schema>, batch: RecordBatch) {
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

/// The points file with **the right schema and no rows** — which is what a deployment that means
/// to start empty writes, and what `ingest_cycle.py`'s *f* = 100% cell filters its corpus down to.
/// It carries the declared attribute columns beside the geometry, because a schema-less empty
/// build would not exercise the column pass at all.
fn write_empty_points(path: &Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("published", DataType::UInt64, true),
        Field::new("title", DataType::Utf8, true),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(Vec::<u64>::new())) as ArrayRef,
            Arc::new(Float64Array::from(Vec::<f64>::new())),
            Arc::new(Float64Array::from(Vec::<f64>::new())),
            Arc::new(UInt64Array::from(Vec::<Option<u64>>::new())),
            Arc::new(StringArray::from(Vec::<Option<&str>>::new())),
        ],
    )
    .unwrap();
    write(path, schema, batch);
}

fn write_empty_pairs(path: &Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("term_id", DataType::UInt32, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(Vec::<u64>::new())) as ArrayRef,
            Arc::new(UInt32Array::from(Vec::<u32>::new())),
        ],
    )
    .unwrap();
    write(path, schema, batch);
}

/// The clustering's roster, **carried whole**: an artifact exists because the declaration names
/// it, so a layer over an empty corpus registers its artifacts and holds none of the points they
/// would have held. That is the state a deployment starting empty is in for as long as it takes
/// the first batch to arrive.
fn write_clusters(path: &Path) {
    let schema = Arc::new(Schema::new(vec![Field::new("key", DataType::Utf8, false)]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(StringArray::from(vec!["c-0000", "c-0001"])) as ArrayRef],
    )
    .unwrap();
    write(path, schema, batch);
}

fn write_empty_members(path: &Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("key", DataType::Utf8, false),
        Field::new("rank", DataType::UInt32, true),
        Field::new("entity", DataType::UInt64, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from(Vec::<&str>::new())) as ArrayRef,
            Arc::new(UInt32Array::from(Vec::<Option<u32>>::new())),
            Arc::new(UInt64Array::from(Vec::<u64>::new())),
        ],
    )
    .unwrap();
    write(path, schema, batch);
}

const CONFIG: &str = r#"
[sources]
points           = "points.parquet"
clusters         = "clusters.parquet"
clusters_members = "clusters_members.parquet"

[defaults]
source = "points"

[[view]]
name             = "s0"
extent           = { min = 0.0, max = 1000.0 }
point_visibility = { default = "public" }

# Rendered as well as indexed, so the zero-row case reaches the *segment* write and not only the
# filter emit: a render column's values buffer is what `write_columns` builds an array over, and an
# empty one has no allocation behind it to be aligned.
[[attribute]]
name = "published"
type = "u64"
index = true
render = true

[[attribute]]
name = "title"
type = "text"

[[layer]]
name = "clusters/a"
title = "clusters"
views = ["s0"]
source = "clusters"
membership = "enumerated"
visibility = "public"
artifact_visibility = { default = "inherited" }
require_member_visibility = "none"
hierarchy = { kind = "flat", prune_children = false }
content = { computed = ["centroid"] }

  [layer.members]
  source = "clusters_members"
"#;

struct Inputs {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
    points: PathBuf,
    pairs: PathBuf,
    config: PathBuf,
}

fn inputs() -> Inputs {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path().to_path_buf();
    write_empty_points(&dir.join("points.parquet"));
    write_empty_pairs(&dir.join("pairs.parquet"));
    write_clusters(&dir.join("clusters.parquet"));
    write_empty_members(&dir.join("clusters_members.parquet"));
    std::fs::write(dir.join("config.toml"), CONFIG).unwrap();
    Inputs {
        points: dir.join("points.parquet"),
        pairs: dir.join("pairs.parquet"),
        config: dir.join("config.toml"),
        _tmp: tmp,
        dir,
    }
}

fn args(inputs: &Inputs, out: &Path) -> BuildArgs {
    BuildArgs {
        views: vec![tessera_build::ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: tessera_spatial::Projection::None,
            extent: extent(),
            points: inputs.points.clone(),
            point_fields: Default::default(),
            select: None,
            access: tessera_build::config::AccessInput::relation(inputs.pairs.clone()),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: Vec::new(),
        out: out.to_path_buf(),
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
    }
}

/// The declaration and the build, in the order the CLI does them.
fn built(inputs: &Inputs, out: &Path) -> tessera_build::BuildReport {
    let config = tessera_build::config::Config::parse(&inputs.config, &Default::default()).unwrap();
    let mut args = args(inputs, out);
    args.layers = config.layers;
    args.layer_inputs = config.layer_sources;
    args.attribute_sources =
        tessera_build::config::AttributeSource::over(inputs.points.clone(), &config.schema);
    args.schema = config.schema;
    build(&args).expect("a build over no points writes a bundle")
}

/// **The headline.** Every stage runs over nothing and the bundle is ordinary: it opens, its
/// digests verify, its watermark is zero, its declared columns are all there, and its layer is
/// registered with the mark that keeps the ids it claimed out of the next allocation's reach.
#[test]
fn a_build_with_no_points_writes_a_bundle() {
    let inputs = inputs();
    let out = inputs.dir.join("bundle");
    let report = built(&inputs, &out);
    assert_eq!(report.items, 0);

    let bundle = open_bundle(&out).expect("a zero-item bundle opens and verifies");
    let partition = bundle.partitions.values().next().expect("one partition");
    let manifest = &partition.manifest;

    assert_eq!(manifest.entity_id_high_water, 0);
    assert_eq!(manifest.watermark, 0);
    assert_eq!(manifest.segments.len(), 1);
    assert_eq!(manifest.segments[0].row_count, 0);

    // Every declared column is present and empty — the schema is the bundle's, not the corpus's,
    // so a column exists because it was declared and not because a row carried a value.
    let declared: Vec<&str> = bundle
        .manifest
        .declared_scalars
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert_eq!(declared, vec!["published", "title"]);

    // `public` is term 0 in every bundle whether or not a point carries it, so a dictionary over
    // no points is not an empty file (`tessera_authz::PUBLIC_TERM`).
    assert_eq!(manifest.dict_extents.len(), 1);
    assert_eq!(manifest.dict_extents[0].records, 1);

    // The layer is registered, and the row-less mark records what it claimed: without it the
    // first online registration would reissue ids this build already handed out (decision 0074).
    let names: Vec<&str> = manifest
        .layers
        .iter()
        .map(|l| l.declaration.name.as_str())
        .collect();
    assert_eq!(names, vec!["clusters/a"]);
    assert!(manifest.entity_id_low_water < tessera_types::layer::ROWLESS_CEILING);
    for layer in &manifest.layers {
        assert!(layer.entity.raw() >= manifest.entity_id_low_water);
    }
}

/// The two builds agree about the empty bundle exactly as they agree about a populated one: the
/// oracle is the reference for an assignment that is permanent under I9, and `n = 0` is an
/// assignment like any other.
#[test]
fn the_two_builds_agree_about_the_empty_bundle() {
    let inputs = inputs();
    let streaming = inputs.dir.join("streaming");
    let reference = inputs.dir.join("reference");
    built(&inputs, &streaming);

    let config = tessera_build::config::Config::parse(&inputs.config, &Default::default()).unwrap();
    let mut args = args(&inputs, &reference);
    args.layers = config.layers;
    args.layer_inputs = config.layer_sources;
    args.attribute_sources =
        tessera_build::config::AttributeSource::over(inputs.points.clone(), &config.schema);
    args.schema = config.schema;
    build_in_memory(&args).expect("the oracle builds the empty bundle too");

    let left = collect(&streaming);
    let right = collect(&reference);
    assert_eq!(
        left.keys().collect::<Vec<_>>(),
        right.keys().collect::<Vec<_>>(),
        "the two builds wrote different files"
    );
    for (name, bytes) in &left {
        if name == "v00000/MANIFEST.json" || name == "CURRENT" {
            continue; // `created_at`, and the digest of the manifest carrying it
        }
        assert_eq!(bytes, &right[name], "{name} is not byte-identical");
    }
}

/// **`extent = "auto"` over no rows is still refused**, and the message says what to write
/// instead. This is the one refusal the empty build keeps, and it is about acquisition rather
/// than about meaning — so it is not the build-only refusal 0091 calls a bug.
#[test]
fn auto_extent_over_no_rows_is_still_refused() {
    let inputs = inputs();
    let error = tessera_build::config::frame_view(
        "s0",
        tessera_spatial::Projection::None,
        &tessera_build::config::Extent::Auto { margin: 0.01 },
        &inputs.points,
        &Default::default(),
        None,
    )
    .expect_err("`auto` cannot be fitted to no rows");
    let text = error.to_string();
    assert!(
        text.contains("selects no rows") && text.contains("state the frame"),
        "the refusal must name the remedy: {text}"
    );
}

fn collect(root: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut std::collections::BTreeMap<String, Vec<u8>>) {
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
    let mut out = std::collections::BTreeMap::new();
    walk(root, root, &mut out);
    out
}
