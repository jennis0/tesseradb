//! **Two views' derived files are two files** (`tessera_store::derived::DerivedIndex`).
//!
//! The build's artifact pass runs once per view, and every derived kind is named from a running
//! index. An index that restarted per call named the second view's files after the first's: the
//! bytes were overwritten, both views' manifest entries survived, and each of them then named a
//! column projected over the other view's row space. Nothing served a wrong answer, because the
//! engine's adoption guards compare the column's row count against the level's — but the guard is
//! a count, and the first view of every multi-view bundle lost the structures it had paid to
//! compose.
//!
//! So the assertions are the two the defect broke: no two derived entries name one path, and each
//! column's declared row count is its own view's.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{ArrayRef, Float64Array, StringArray, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use tessera_build::{build, BuildArgs, ViewArgs};
use tessera_spatial::Bounds;
use tessera_store::read::open_bundle;
use tessera_types::IdentityKey;

const TEST_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";

/// The prefix a build publishes into. Every manifest path is relative to it.
const PREFIX: &str = "v00000";

/// `wide` holds 0..60 and `narrow` 0..25. Two populations, so a column written for one view is
/// the wrong length for the other and the arithmetic below can say which is which.
const WIDE: std::ops::Range<u64> = 0..60;
const NARROW: std::ops::Range<u64> = 0..25;

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

fn write_points(path: &Path, ids: std::ops::Range<u64>) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
    ]));
    let ids: Vec<u64> = ids.collect();
    let xs: Vec<f64> = ids.iter().map(|&e| ((e * 37) % 1000) as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|&e| ((e * 53) % 1000) as f64).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)) as ArrayRef,
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
        ],
    )
    .unwrap();
    write(path, schema, batch);
}

fn write_pairs(path: &Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("term_id", DataType::UInt32, false),
    ]));
    let entities: Vec<u64> = WIDE.collect();
    let terms: Vec<u32> = entities.iter().map(|&e| (e % 4) as u32 + 1).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(entities)) as ArrayRef,
            Arc::new(UInt32Array::from(terms)),
        ],
    )
    .unwrap();
    write(path, schema, batch);
}

/// Two artifacts, and every item of `wide` in one of them — a partition, so the pinned column
/// composes in both views.
fn write_clusters(path: &Path) {
    let schema = Arc::new(Schema::new(vec![Field::new("key", DataType::Utf8, false)]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(StringArray::from(vec!["c-0000", "c-0001"])) as ArrayRef],
    )
    .unwrap();
    write(path, schema, batch);
}

fn write_cluster_members(path: &Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("key", DataType::Utf8, false),
        Field::new("entity", DataType::UInt64, false),
    ]));
    let keys: Vec<&str> = WIDE
        .map(|e| if e % 2 == 0 { "c-0000" } else { "c-0001" })
        .collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from(keys)) as ArrayRef,
            Arc::new(UInt64Array::from(WIDE.collect::<Vec<_>>())),
        ],
    )
    .unwrap();
    write(path, schema, batch);
}

/// The layer is drawn on both views and its layout is pinned, so each view gets a column of its
/// own row count rather than whichever layout the pick would have taken.
const CONFIG: &str = r#"
[sources]
points           = "wide.parquet"
narrow_points    = "narrow.parquet"
clusters         = "clusters.parquet"
clusters_members = "clusters_members.parquet"

[defaults]
source          = "points"
allocation_view = "wide"

[[view]]
name             = "wide"
extent           = { min = 0.0, max = 1000.0 }
point_visibility = { default = "public" }

[[view]]
name             = "narrow"
source           = "narrow_points"
extent           = { min = 0.0, max = 1000.0 }
point_visibility = { default = "public" }

[[layer]]
name = "clusters/a"
title = "clusters"
views = ["wide", "narrow"]
source = "clusters"
membership = "enumerated"
layout = "column"
visibility = "public"
artifact_visibility = { default = "inherited" }
require_member_visibility = "none"
hierarchy = { kind = "flat", prune_children = false }
content = { computed = ["centroid"] }

  [layer.members]
  source = "clusters_members"
"#;

/// A row-major label column's header: magic, version, width, a pad byte, the row count, the
/// ordinal count (`tessera_store::membership::pack_label_column`). The row count is what says
/// which view a column was projected over.
fn column_rows(path: &Path) -> u32 {
    let bytes = std::fs::read(path).expect("a column the manifest names is on disk");
    assert!(bytes.len() >= 16, "{}: short header", path.display());
    u32::from_le_bytes(bytes[8..12].try_into().unwrap())
}

#[test]
fn two_views_derived_files_do_not_collide() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_points(&dir.join("wide.parquet"), WIDE);
    write_points(&dir.join("narrow.parquet"), NARROW);
    write_pairs(&dir.join("pairs.parquet"));
    write_clusters(&dir.join("clusters.parquet"));
    write_cluster_members(&dir.join("clusters_members.parquet"));
    let config_path = dir.join("config.toml");
    std::fs::write(&config_path, CONFIG).unwrap();
    let config = tessera_build::config::Config::parse(&config_path, &Default::default())
        .expect("the fixture declaration parses");

    let out = dir.join("bundle");
    let view = |name: &str, points: &str| ViewArgs {
        visibility: None,
        view_id: name.to_string(),
        projection: tessera_spatial::Projection::None,
        extent: extent(),
        points: dir.join(points),
        point_fields: Default::default(),
        select: None,
        access: tessera_build::config::AccessInput::relation(dir.join("pairs.parquet")),
    };
    build(&BuildArgs {
        views: vec![
            view("wide", "wide.parquet"),
            view("narrow", "narrow.parquet"),
        ],
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
        layers: config.layers,
        layer_inputs: config.layer_sources,
        scoped_layers: Default::default(),
        mint_external_ids: false,
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: Default::default(),
    })
    .expect("a two-view build with a layer succeeds");

    let bundle = open_bundle(&out).expect("the bundle opens");
    let manifest = &bundle
        .partitions
        .get("default")
        .expect("one partition")
        .manifest;

    // **Every derived path is named once.** Two entries naming one file is the defect itself: one
    // of them describes bytes that are not there any more.
    let mut paths: Vec<&str> = Vec::new();
    paths.extend(manifest.derived_extents.iter().map(|e| e.path.as_str()));
    paths.extend(manifest.term_image_extents.iter().map(|e| e.path.as_str()));
    let total = paths.len();
    paths.sort_unstable();
    paths.dedup();
    assert_eq!(
        paths.len(),
        total,
        "two derived manifest entries name one file: {:?}",
        manifest
            .derived_extents
            .iter()
            .map(|e| (&e.view, &e.layer, e.level, &e.path))
            .collect::<Vec<_>>()
    );

    // **A containment partition is not per view**, so the two views share the level's one file
    // rather than each writing its own.
    let containment: Vec<(&str, u32)> = manifest
        .derived_extents
        .iter()
        .filter(|e| e.form == tessera_store::manifest::DerivedForm::Containment)
        .map(|e| (e.layer.as_str(), e.level))
        .collect();
    let mut once = containment.clone();
    once.sort_unstable();
    once.dedup();
    assert_eq!(
        once.len(),
        containment.len(),
        "a level has more than one containment partition: {containment:?}"
    );

    // **Each column is its own view's.** `wide` is 60 rows and `narrow` 25; before the counter
    // both entries named a 25-row column.
    let columns: Vec<(&str, u32)> = manifest
        .derived_extents
        .iter()
        .filter(|e| {
            matches!(
                e.form,
                tessera_store::manifest::DerivedForm::RowColumn { .. }
            )
        })
        .map(|e| {
            (
                e.view.as_deref().expect("a column names its view"),
                column_rows(&out.join(PREFIX).join(&e.path)),
            )
        })
        .collect();
    assert_eq!(columns.len(), 2, "one column per view: {columns:?}");
    for (view, rows) in &columns {
        let expected = match *view {
            "wide" => (WIDE.end - WIDE.start) as u32,
            _ => (NARROW.end - NARROW.start) as u32,
        };
        assert_eq!(
            *rows, expected,
            "the column filed for '{view}' was projected over {rows} rows, not its own {expected}"
        );
    }
}
