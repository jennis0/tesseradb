//! **An artifact key is unique per `(layer, view)` on a group-scoped layer** (`views.md` §3.5,
//! contracts §3.4 r84; issue #152).
//!
//! One key on two views of the group is two artifacts, each with its own membership, drawn on its
//! own view and on no other — which is what the control plane's publication route already takes,
//! so a build that refused it was the fail-closed half of one rule stated twice (decision 0091).
//! The same key twice on *one* view is still one name for two artifacts, and is still refused.
//!
//! The member source is read the same way: a key alone would name either of the two artifacts, so
//! its rows carry the view column too and each row joins the artifact of its own view.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{ArrayRef, Float64Array, StringArray, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use tessera_build::{build, BuildArgs, ScopedLayer, ViewArgs};
use tessera_spatial::Bounds;
use tessera_store::read::open_bundle;
use tessera_types::IdentityKey;

const TEST_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";

/// Both views hold every entity, so a masked count that came from the wrong view's artifact would
/// still be a plausible number — which is why the assertions below are on the artifact *count*
/// and on each artifact's own members rather than on a total.
const ENTITIES: std::ops::Range<u64> = 0..24;

fn extent() -> Bounds {
    Bounds {
        x_min: 0.0,
        x_max: 1000.0,
        y_min: 0.0,
        y_max: 1000.0,
    }
}

fn group_frame() -> tessera_store::manifest::Quantisation {
    tessera_store::manifest::Quantisation {
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

fn write_points(path: &Path, spread: f64) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
    ]));
    let ids: Vec<u64> = ENTITIES.collect();
    let xs: Vec<f64> = ids.iter().map(|&e| (e % 8) as f64 * spread).collect();
    let ys: Vec<f64> = ids.iter().map(|&e| (e / 8) as f64 * spread).collect();
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
    let entities: Vec<u64> = ENTITIES.collect();
    let terms: Vec<u32> = entities.iter().map(|_| 1u32).collect();
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

/// The artifacts table: one row per `(key, view)`.
fn write_artifacts(path: &Path, rows: &[(&str, &str)]) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("key", DataType::Utf8, false),
        Field::new("slice", DataType::Utf8, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from(
                rows.iter().map(|(key, _)| *key).collect::<Vec<_>>(),
            )) as ArrayRef,
            Arc::new(StringArray::from(
                rows.iter().map(|(_, view)| *view).collect::<Vec<_>>(),
            )),
        ],
    )
    .unwrap();
    write(path, schema, batch);
}

/// The members table: `a`'s copy of `c0` holds the even entities and `b`'s the odd ones, so which
/// artifact a row joined is readable off the membership rather than off a total.
fn write_members(path: &Path) {
    let mut keys: Vec<&str> = Vec::new();
    let mut views: Vec<&str> = Vec::new();
    let mut entities: Vec<u64> = Vec::new();
    for e in ENTITIES {
        keys.push("c0");
        views.push(if e % 2 == 0 { "a" } else { "b" });
        entities.push(e);
    }
    let schema = Arc::new(Schema::new(vec![
        Field::new("key", DataType::Utf8, false),
        Field::new("slice", DataType::Utf8, false),
        Field::new("entity", DataType::UInt64, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from(keys)) as ArrayRef,
            Arc::new(StringArray::from(views)),
            Arc::new(UInt64Array::from(entities)),
        ],
    )
    .unwrap();
    write(path, schema, batch);
}

const CONFIG: &str = r#"
[sources]
points_a           = "a.parquet"
points_b           = "b.parquet"
clusters           = "clusters.parquet"
clusters_members   = "clusters_members.parquet"

[defaults]
source          = "points_a"
allocation_view = "slices:a"

[[view_group]]
name             = "slices"
projection       = "none"
extent           = { x = [0.0, 1000.0], y = [0.0, 1000.0] }
point_visibility = { default = "public" }

[[view_group.view]]
key    = "a"
source = "points_a"

[[view_group.view]]
key    = "b"
source = "points_b"

[[layer]]
name = "clusters"
title = "Clusters"
views = ["slices"]
scope = { group = "slices" }
source = "clusters"
fields = { view = "slice" }
membership = "enumerated"
layout = "column"
visibility = "public"
artifact_visibility = { default = "inherited" }
require_member_visibility = "none"
hierarchy = { kind = "flat", prune_children = false }

  # The member rows' view column is the layer's own `fields.view`, declared once on the layer
  # above: one layer has one discriminator, and a second name for it in the member block would be
  # a second answer to the same question.
  [layer.members]
  source = "clusters_members"
"#;

/// The fixture, built. `artifacts` is the artifacts table's rows, so a case can hand it a
/// duplicate.
fn build_fixture(
    dir: &Path,
    artifacts: &[(&str, &str)],
) -> Result<tessera_build::BuildReport, tessera_build::BuildError> {
    write_points(&dir.join("a.parquet"), 100.0);
    write_points(&dir.join("b.parquet"), 90.0);
    write_pairs(&dir.join("pairs.parquet"));
    write_artifacts(&dir.join("clusters.parquet"), artifacts);
    write_members(&dir.join("clusters_members.parquet"));
    let config_path = dir.join("config.toml");
    std::fs::write(&config_path, CONFIG).unwrap();
    let config = tessera_build::config::Config::parse(&config_path, &Default::default())
        .expect("the fixture declaration parses");

    let view = |key: &str, points: &str| ViewArgs {
        visibility: None,
        view_id: format!("slices:{key}"),
        projection: tessera_spatial::Projection::None,
        extent: extent(),
        points: dir.join(points),
        point_fields: Default::default(),
        select: None,
        access: tessera_build::config::AccessInput::relation(dir.join("pairs.parquet")),
    };
    let mut layers = config.layers.clone();
    for layer in &mut layers {
        layer.views = vec!["slices:a".to_string(), "slices:b".to_string()];
    }
    build(&BuildArgs {
        views: vec![view("a", "a.parquet"), view("b", "b.parquet")],
        anchor: 0,
        groups: vec![tessera_store::manifest::GroupDescriptor {
            title: None,
            name: "slices".to_string(),
            members_of: None,
            point_default: Some("public".to_string()),
            visibility: None,
            scoped_scalars: Vec::new(),
            quantisation: group_frame(),
            projection: tessera_spatial::Projection::None,
            metadata: Vec::new(),
            views: ["a", "b"]
                .into_iter()
                .map(|key| tessera_store::manifest::GroupViewDescriptor {
                    key: key.to_string(),
                    visibility: None,
                    metadata: Default::default(),
                })
                .collect(),
        }],
        scoped_attributes: Vec::new(),
        attribute_sources: Vec::new(),
        out: dir.join("bundle"),
        limit: None,
        identity_key: IdentityKey::from_hex(TEST_KEY_HEX).unwrap(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers,
        layer_inputs: config.layer_sources.clone(),
        scoped_layers: [(
            "clusters".to_string(),
            ScopedLayer {
                group: "slices".to_string(),
                column: "slice".to_string(),
                keys: vec!["a".to_string(), "b".to_string()],
            },
        )]
        .into_iter()
        .collect(),
        mint_external_ids: false,
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: Default::default(),
    })
}

#[test]
fn one_key_on_two_views_of_a_group_is_two_artifacts() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let report = build_fixture(dir, &[("c0", "a"), ("c0", "b")])
        .expect("one key on two views of the group builds");

    // The post-bundle pass reports per `(view, layer, level)`, which is the one place a scoped
    // layer's per-view separation is visible: each view holds one artifact with rows, and the
    // level's roster holds both.
    let levels: Vec<_> = report
        .artifact_levels
        .iter()
        .filter(|level| level.layer == "clusters")
        .collect();
    assert_eq!(
        levels.iter().map(|level| level.view.as_str()).collect::<Vec<_>>(),
        ["slices:a", "slices:b"],
        "the layer is drawn on both views of the group"
    );
    for level in &levels {
        assert_eq!(
            level.registered, 2,
            "one key on two views is two artifacts in the level's roster: {}",
            level.view
        );
        assert_eq!(
            level.shape.artifacts, 1,
            "and exactly one of them has rows in {}",
            level.view
        );
    }

    // The bundle opens, and the level it published is the one the report describes.
    let bundle = open_bundle(&dir.join("bundle")).expect("the bundle opens");
    assert!(bundle.partitions.contains_key("default"));
}

#[test]
fn one_key_twice_on_one_view_is_still_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let error = build_fixture(dir, &[("c0", "a"), ("c0", "a")])
        .expect_err("a key repeated within one view is one name for two artifacts");
    let message = format!("{error}");
    assert!(
        message.contains("declared on more than one row"),
        "{message}"
    );
    assert!(
        message.contains("of view 'a'"),
        "the refusal names the view the collision is in: {message}"
    );
}
