//! **An authored shape lands in the same place whichever door it came through**
//! (`polygon-membership.md` §6.1; [decision 0091](../../../docs/decisions/0091-build-is-ingest-into-an-empty-database.md)).
//!
//! An artifact's drawn geometry is read exactly as a membership shape is, and the space it was
//! written in — the table's `default_space`, a row's own `space` — is part of that reading. A
//! producer whose rows are in longitude and latitude writes both the polygon that selects and the
//! outline that is drawn from one source and in one coordinate system; a service that projected
//! the first and not the second would place a ±180 × ±90 drawing in a corner of the `[0, 1]`
//! frame, refusing nothing. R12's degrees-looking report cannot catch it either: on a projected
//! view the whole frame lies inside ±180 × ±90, so nothing looks written in degrees there.
//!
//! So the assertion is an equality across four declarations of one triangle — the membership shape
//! and the drawing, at the build and at `PUT /control/layers/{name}/artifacts` — and the served
//! rings are the observable, because they are what a viewer is actually shown.

mod common;

use std::path::Path;
use std::sync::Arc;

use arrow::array::{Array, Float64Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use serde_json::json;
use tempfile::TempDir;

use tessera_build::config::{Config, Fields};
use tessera_build::{build, BuildArgs};
use tessera_spatial::{Bounds, Projection};

use common::*;

/// 8°W 50°N → 2°E 58°N → 8°W 58°N. Two of its edges are a meridian and a parallel — straight in
/// both planes — so the diagonal is the only edge whose two readings differ, which is what makes
/// the densification visible in the ring count as well as in the placement.
const UK: &str = "POLYGON ((-8 50, 2 58, -8 58, -8 50))";

/// Places inside the triangle, so every layer below has members to be counted over.
const PLACES: &[(f64, f64)] = &[
    (-3.0, 55.5),
    (-6.0, 56.0),
    (0.0, 57.5),
    (-4.0, 54.0),
    (-2.0, 56.5),
    (-7.0, 55.0),
];

fn world_frame() -> Bounds {
    Bounds {
        x_min: 0.0,
        x_max: 1.0,
        y_min: 0.0,
        y_max: 1.0,
    }
}

fn write_lon_lat_points(path: &Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("lon", DataType::Float64, false),
        Field::new("lat", DataType::Float64, false),
    ]));
    let batch = RecordBatch::try_new(
        Arc::clone(&schema),
        vec![
            Arc::new(UInt64Array::from(
                (0..PLACES.len() as u64).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                PLACES.iter().map(|p| p.0).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                PLACES.iter().map(|p| p.1).collect::<Vec<_>>(),
            )),
        ],
    )
    .expect("the fixture batch is well-formed");
    let mut w = ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

/// One artifact table for the drawing layer: a key, a membership and one ranked content holding
/// the triangle's WKT — the shape a caller's own file has, and the only route by which a table's
/// `default_space` governs an authored shape.
fn write_drawings(path: &Path) {
    use arrow::array::{ListBuilder, StringBuilder, UInt64Builder};

    let mut keys = StringBuilder::new();
    keys.append_value("uk");
    let mut members = ListBuilder::new(UInt64Builder::new());
    for id in 0..PLACES.len() as u64 {
        members.values().append_value(id);
    }
    members.append(true);
    let mut contents = ListBuilder::new(ListBuilder::new(StringBuilder::new()));
    contents.values().values().append_value(UK);
    contents.values().append(true);
    contents.append(true);

    let keys = Arc::new(keys.finish());
    let members = Arc::new(members.finish());
    let contents = Arc::new(contents.finish());
    let schema = Arc::new(Schema::new(vec![
        Field::new("key", DataType::Utf8, false),
        Field::new("members", members.data_type().clone(), true),
        Field::new("contents", contents.data_type().clone(), true),
    ]));
    let batch = RecordBatch::try_new(Arc::clone(&schema), vec![keys, members, contents])
        .expect("the fixture artifact table is well-formed");
    let mut w = ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

/// The two layers the **build** publishes: one whose membership is the triangle, written inline
/// with the row's own `space`, and one that merely draws it, read from a table whose
/// `default_space` is the whole declaration of where its geometry is written. Between them the
/// build's two spellings of the space are both exercised.
///
/// Parsed through `Config` rather than assembled as values, so the declaration-time half —
/// a space is honourable only where the layer's views can honour it — is on the same route an
/// operator's document takes.
fn built_layers(dir: &Path) -> Config {
    let path = dir.join("corpus.toml");
    std::fs::write(
        &path,
        format!(
            r#"
[sources]
points   = "points.parquet"
drawings = "drawings.parquet"

[defaults]
source = "points"

[[view]]
name             = "s0"
projection       = "web_mercator"
extent           = {{ lon = [-180.0, 180.0], lat = [-85.0511287798066, 85.0511287798066] }}
point_visibility = {{ default = "public" }}

[[layer]]
name                      = "regions/selects"
views                     = ["s0"]
membership                = "spatial"
hierarchy                 = {{ kind = "flat", prune_children = false }}
visibility                = "public"
artifact_visibility       = {{ default = "inherited" }}
require_member_visibility = "none"
artifacts = [
  {{ key = "uk", wkt = "{UK}", space = "wgs84" }},
]

  [layer.shape]
  kind = "polygon"

[[layer]]
name                      = "regions/built"
views                     = ["s0"]
membership                = "enumerated"
source                    = "drawings"
default_space             = "wgs84"
hierarchy                 = {{ kind = "flat", prune_children = false }}
visibility                = "public"
artifact_visibility       = {{ default = "inherited" }}
require_member_visibility = "none"

  [[layer.content.supplied]]
  name = "outline"
  type = "polygon"
  require_member_visibility = "inherited"
"#
        ),
    )
    .unwrap();
    write_drawings(&dir.join("drawings.parquet"));
    Config::parse(&path, &Default::default()).expect("the fixture corpus parses")
}

/// A `web_mercator` bundle over the whole world, carrying the two build-published layers.
fn build_projected(out: &Path, tmp: &Path) -> Config {
    let points_path = tmp.join("points.parquet");
    let pairs_path = tmp.join("pairs.parquet");
    write_lon_lat_points(&points_path);
    write_pairs_n(&pairs_path, PLACES.len() as u64);
    let config = built_layers(tmp);
    build(&BuildArgs {
        views: vec![tessera_build::ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: Projection::WebMercator,
            extent: world_frame(),
            points: points_path,
            point_fields: Fields::moved("view 's0'", [("x", "lon"), ("y", "lat")]),
            select: None,
            access: tessera_build::config::AccessInput::relation(pairs_path),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: Vec::new(),
        out: out.to_path_buf(),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: FIXTURE_IDSET,
        shard_id: 0,
        layers: config.layers.clone(),
        layer_inputs: config.layer_sources.clone(),
        scoped_layers: Default::default(),
        mint_external_ids: true,
        emit_oracle_pairs: true,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: Default::default(),
    })
    .expect("the projected fixture builds");
    config
}

async fn serve_projected(tmp: &TempDir) -> TestServer {
    let bundle_root = tmp.path().join("bundle");
    build_projected(&bundle_root, tmp.path());
    spawn_server(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await
}

/// The declaration `PUT /control/layers` takes for a layer that draws one authored polygon.
fn drawing_layer(name: &str) -> serde_json::Value {
    json!({
        "name": name,
        "title": format!("{name} (title)"),
        "views": ["s0"],
        "membership": "enumerated",
        "visibility": "public",
        "artifact_visibility": { "field": null, "default": "inherited" },
        "require_member_visibility": null,
        "hierarchy": { "kind": "flat", "prune_children": false },
        "content": {
            "computed": [],
            "supplied": [{
                "name": "outline",
                "type": "polygon",
                "require_member_visibility": "inherited"
            }],
            "withdraw_on_member_deletion": true
        },
        "depends_on": [],
        "levels": []
    })
}

/// A member as `addressing: "external"` names it: the build's own 8-byte spelling, base64'd.
fn member(source_id: u64) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(external_id_of(source_id))
}

async fn register(server: &TestServer, declaration: serde_json::Value) -> (u16, serde_json::Value) {
    let resp = server
        .client
        .put(server.control_url("/control/layers"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&declaration)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(serde_json::Value::Null))
}

async fn publish(
    server: &TestServer,
    layer: &str,
    body: serde_json::Value,
) -> (u16, serde_json::Value) {
    let encoded = layer.replace('/', "%2F");
    let resp = server
        .client
        .put(server.control_url(&format!("/control/layers/{encoded}/artifacts")))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(serde_json::Value::Null))
}

/// The drawn geometry of every served artifact, by layer — the frame the client is handed.
async fn shapes_by_layer(
    server: &TestServer,
) -> std::collections::BTreeMap<String, Vec<Vec<Vec<[u32; 2]>>>> {
    let auth = authorise(server, &["0"]).await;
    let token = auth["token"].as_str().unwrap();
    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&json!({
            "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1.0, 1.0], "k": 200,
            "layers": "all", "computed": ["shape"]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    decode_viewport_frames(&resp.bytes().await.unwrap())
        .artifacts
        .expect("the artifacts frame")
        .into_iter()
        .map(|row| (row.layer, row.shape.expect("a drawn geometry")))
        .collect()
}

/// **One triangle in degrees, four declarations, one placement.** The membership shape and the
/// drawing, published by the build and by the control plane, all land on the same frame
/// coordinates — which is what makes the space a property of the submission rather than of the
/// kind of geometry that carries it.
#[tokio::test]
async fn an_authored_wgs84_shape_lands_where_a_membership_one_does_through_either_door() {
    let tmp = TempDir::new().unwrap();
    let server = serve_projected(&tmp).await;

    // The control plane's two: the batch's `default_space`, and a row overriding it.
    assert_eq!(
        register(&server, drawing_layer("regions/batch")).await.0,
        201
    );
    assert_eq!(register(&server, drawing_layer("regions/row")).await.0, 201);
    let members: Vec<String> = (0..PLACES.len() as u64).map(member).collect();
    let (status, body) = publish(
        &server,
        "regions/batch",
        json!({
            "addressing": "external",
            "default_space": "wgs84",
            "artifacts": [{ "key": "uk", "members": members, "content": [{ "values": [UK] }] }]
        }),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let (status, body) = publish(
        &server,
        "regions/row",
        json!({
            "addressing": "external",
            "artifacts": [{
                "key": "uk", "members": members, "space": "wgs84",
                "content": [{ "values": [UK] }]
            }]
        }),
    )
    .await;
    assert_eq!(status, 201, "{body}");

    let shapes = shapes_by_layer(&server).await;
    let selects = shapes
        .get("regions/selects")
        .expect("the build's membership shape");
    // The triangle reached the frame at all. Read as view coordinates these degrees are wholly
    // outside the unit square and clip away to nothing, and an equality between two empty shapes
    // would say nothing.
    assert!(
        selects.first().is_some_and(|part| !part.is_empty()),
        "the triangle drew no ring; it did not reach the frame: {selects:?}"
    );
    for layer in ["regions/built", "regions/batch", "regions/row"] {
        assert_eq!(
            shapes.get(layer),
            Some(selects),
            "'{layer}' placed the same degrees somewhere else"
        );
    }
}

/// **A `wgs84` authored shape on a view with no projection is refused**, at the control plane as
/// at the build: such a view has one space and nothing to convert a degree from
/// (`polygon-membership.md` §4.3). And a `wgs84` coordinate outside ±180 × ±90 is not a
/// coordinate (`projections.md` §2). Both refusals are the membership path's, and the authored
/// path reaches them because the space now reaches it.
#[tokio::test]
async fn the_authored_wgs84_refusals_are_the_membership_shapes_own() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    // The fixture bundle every other server test uses: one view, `projection = "none"`.
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;
    assert_eq!(
        register(&server, drawing_layer("regions/flat")).await.0,
        201
    );

    let (status, body) = publish(
        &server,
        "regions/flat",
        json!({
            "addressing": "external",
            "default_space": "wgs84",
            "artifacts": [{ "key": "uk", "members": [], "content": [{ "values": [UK] }] }]
        }),
    )
    .await;
    assert_eq!(status, 422, "{body}");
    assert!(
        body.to_string().contains("`projection` is `none`"),
        "{body}"
    );

    // The same declaration in view space is accepted, which is what says the refusal is about the
    // space and not about the drawing.
    let (status, body) = publish(
        &server,
        "regions/flat",
        json!({
            "addressing": "external",
            "artifacts": [{ "key": "uk", "members": [], "content": [{ "values": [UK] }] }]
        }),
    )
    .await;
    assert_eq!(status, 201, "{body}");

    // And on the projected bundle, a latitude that is not one.
    let tmp = TempDir::new().unwrap();
    let server = serve_projected(&tmp).await;
    assert_eq!(
        register(&server, drawing_layer("regions/batch")).await.0,
        201
    );
    let (status, body) = publish(
        &server,
        "regions/batch",
        json!({
            "addressing": "external",
            "default_space": "wgs84",
            "artifacts": [{
                "key": "uk", "members": [],
                "content": [{ "values": ["POLYGON ((-8 50, 2 91, -8 91, -8 50))"] }]
            }]
        }),
    )
    .await;
    assert_eq!(status, 422, "{body}");
    assert!(body.to_string().contains("not a coordinate"), "{body}");
}
