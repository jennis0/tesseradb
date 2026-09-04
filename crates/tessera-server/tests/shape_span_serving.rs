//! **A shape layer over two views whose frames differ, published through the control plane**
//! ([decision 0111](../../../docs/decisions/0111-a-shape-spans-projected-views-through-wgs84.md),
//! `polygon-membership.md` §4.3).
//!
//! `authored_shape_space.rs` holds the single-frame equality — one boundary, four declarations, one
//! placement. This file holds the case that only exists once a bundle carries two frames: the same
//! boundary is a **different canonical form in each view**, and a publication that resolved it once
//! and stored the answer under both names would place one of the two wrong with nothing saying so.
//!
//! Three things are asserted, and each is a way `/control/layers` could be wrong while every
//! build-side test still passed:
//!
//! - the per-view report the publication answers with carries a row per view;
//! - the served geometry differs between the two views, and the shape is the one that view's own
//!   frame produces;
//! - a shape wholly outside one view's extent is **published and warned about**, not refused —
//!   `outside` set for that view and clear for the other.

mod common;

use std::path::Path;
use std::sync::Arc;

use arrow::array::{Float64Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use common::*;
use parquet::arrow::ArrowWriter;
use serde_json::json;
use tempfile::TempDir;
use tessera_build::config::Fields;
use tessera_build::{build, BuildArgs, ViewArgs};
use tessera_spatial::{Bounds, Projection};

/// Points in longitude and latitude, inside the boundary published below.
const PLACES: &[(f64, f64)] = &[
    (-3.0, 55.5),
    (-6.0, 56.0),
    (0.0, 57.5),
    (-4.0, 54.0),
    (-2.0, 56.5),
    (-7.0, 55.0),
];

/// The whole world in Web Mercator.
fn world_frame() -> Bounds {
    Bounds {
        x_min: 0.0,
        x_max: 1.0,
        y_min: 0.0,
        y_max: 1.0,
    }
}

/// **The same projection, a different frame**: a view zoomed on north-west Europe spends its whole
/// grid there. Nothing in the declaration ties the two extents together, which is decision 0040's
/// point and the reason a shape cannot be canonicalised once for both.
fn europe_frame() -> Bounds {
    let (x0, y0) = Projection::WebMercator.forward(-15.0, 62.0);
    let (x1, y1) = Projection::WebMercator.forward(15.0, 45.0);
    Bounds {
        x_min: x0,
        x_max: x1,
        y_min: y0,
        y_max: y1,
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
    .unwrap();
    let mut w = ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn view(id: &str, extent: Bounds, projection: Projection, points: &Path, pairs: &Path) -> ViewArgs {
    ViewArgs {
        visibility: None,
        view_id: id.to_string(),
        projection,
        extent,
        points: points.to_path_buf(),
        point_fields: Fields::moved(format!("view '{id}'"), [("x", "lon"), ("y", "lat")]),
        select: None,
        access: tessera_build::config::AccessInput::relation(pairs.to_path_buf()),
    }
}

/// A bundle of three views over one entity space: two projected ones with **different extents**,
/// and one embedding, which is what the mixed-projection refusal below needs to exist at all.
async fn serve_two_frames(tmp: &TempDir) -> TestServer {
    let dir = tmp.path();
    let points = dir.join("points.parquet");
    let pairs = dir.join("pairs.parquet");
    write_lon_lat_points(&points);
    write_pairs_n(&pairs, PLACES.len() as u64);
    let bundle = dir.join("bundle");
    build(&BuildArgs {
        arena_order: Default::default(),
        views: vec![
            view("world", world_frame(), Projection::WebMercator, &points, &pairs),
            view(
                "europe",
                europe_frame(),
                Projection::WebMercator,
                &points,
                &pairs,
            ),
            // An embedding: no projection, its own coordinate range.
            view(
                "embedding",
                Bounds {
                    x_min: -40.0,
                    x_max: 40.0,
                    y_min: -40.0,
                    y_max: 40.0,
                },
                Projection::None,
                &points,
                &pairs,
            ),
        ],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: Vec::new(),
        out: bundle.clone(),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: FIXTURE_IDSET,
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
    })
    .expect("the two-frame fixture builds");
    spawn_server(&bundle, &dir.join("cache"), &dir.join("wal.log")).await
}

fn spatial_layer(name: &str, views: &[&str]) -> serde_json::Value {
    json!({
        "name": name,
        "title": format!("{name} (title)"),
        "views": views,
        "membership": "spatial",
        "shape": { "kind": "bbox" },
        "visibility": "public",
        "artifact_visibility": { "field": null, "default": "inherited" },
        "require_member_visibility": null,
        "hierarchy": { "kind": "flat", "prune_children": false },
        "depends_on": [],
        "levels": []
    })
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
    let text = resp.text().await.unwrap_or_default();
    (
        status,
        serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text)),
    )
}

/// The artifact geometry a viewer is served in one view.
async fn shape_in_view(server: &TestServer, view: &str) -> Vec<Vec<Vec<[u32; 2]>>> {
    let auth = authorise(server, &["0"]).await;
    let token = auth["token"].as_str().unwrap();
    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&json!({
            "view": view, "zoom": 0, "bbox": [0.0, 0.0, 1.0, 1.0], "k": 200,
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
        .map(|row| row.shape.expect("a drawn geometry"))
        .collect::<Vec<_>>()
        .pop()
        .expect("one artifact")
}

/// **One `wgs84` declaration, two frames, two canonical forms.** The publication answers with a
/// report per view, and each view serves the geometry its own frame produced.
#[tokio::test]
async fn one_wgs84_shape_is_canonicalised_per_view_and_served_per_view() {
    let tmp = TempDir::new().unwrap();
    let server = serve_two_frames(&tmp).await;
    assert_eq!(
        register(&server, spatial_layer("regions/uk", &["world", "europe"]))
            .await
            .0,
        201
    );
    let (status, body) = publish(
        &server,
        "regions/uk",
        json!({
            "addressing": "external",
            "default_space": "wgs84",
            "artifacts": [{ "key": "uk", "members": [], "bbox": [-8.0, 50.0, 2.0, 58.0] }]
        }),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let report = body["shapes"][0]["views"]
        .as_array()
        .expect("a shape report per view");
    let views: Vec<&str> = report.iter().map(|r| r["view"].as_str().unwrap()).collect();
    assert_eq!(views, ["world", "europe"], "a row per view: {body}");
    // The decompositions differ, and by a lot: the regional frame spends its whole grid on the
    // boundary's neighbourhood, so the same box is two orders of magnitude more cells there.
    let cells = |view: &str| {
        report
            .iter()
            .find(|r| r["view"] == view)
            .unwrap()["interior_tiles"]
            .as_u64()
            .unwrap()
    };
    assert!(
        cells("europe") > cells("world"),
        "one frame's decomposition is not the other's: {body}"
    );

    let world = shape_in_view(&server, "world").await;
    let europe = shape_in_view(&server, "europe").await;
    assert!(!world.is_empty() && !europe.is_empty(), "both views drew it");
    assert_ne!(
        world, europe,
        "one boundary on two frames is two sets of grid coordinates"
    );
}

/// **A shape wholly outside one view's extent is published and warned about, never refused**
/// (§4.3): empty membership there, the count beside the clip counts, and the other view untouched.
#[tokio::test]
async fn a_shape_outside_one_views_extent_is_published_and_reported_not_refused() {
    let tmp = TempDir::new().unwrap();
    let server = serve_two_frames(&tmp).await;
    assert_eq!(
        register(&server, spatial_layer("regions/nz", &["world", "europe"]))
            .await
            .0,
        201
    );
    // New Zealand: inside the world frame, nowhere near the European one.
    let (status, body) = publish(
        &server,
        "regions/nz",
        json!({
            "addressing": "external",
            "default_space": "wgs84",
            "artifacts": [{ "key": "nz", "members": [], "bbox": [166.0, -47.0, 179.0, -34.0] }]
        }),
    )
    .await;
    assert_eq!(status, 201, "an out-of-extent shape is not a refusal: {body}");
    let report = body["shapes"][0]["views"].as_array().unwrap();
    let outside = |view: &str| {
        report
            .iter()
            .find(|r| r["view"] == view)
            .expect("a row per view")["outside"]
            .as_bool()
            .expect("the out-of-extent flag")
    };
    assert!(!outside("world"), "inside the world frame: {body}");
    assert!(outside("europe"), "reported for the view it misses: {body}");
}

/// **A shape layer's views are all projected or all `none`, refused at the declaration**
/// (decision 0111) — naming the layer and both sides, before any geometry is submitted.
#[tokio::test]
async fn a_shape_layer_spanning_a_projected_and_an_unprojected_view_is_refused_at_declaration() {
    let tmp = TempDir::new().unwrap();
    let server = serve_two_frames(&tmp).await;
    let (status, body) = register(
        &server,
        spatial_layer("regions/mixed", &["world", "embedding"]),
    )
    .await;
    assert_eq!(status, 422, "{body}");
    let detail = body.to_string();
    assert!(detail.contains("regions/mixed"), "{detail}");
    assert!(detail.contains("'world'"), "{detail}");
    assert!(detail.contains("'embedding'"), "{detail}");

    // The same declaration over the two projected views is accepted: what is refused is the mix.
    assert_eq!(
        register(&server, spatial_layer("regions/ok", &["world", "europe"]))
            .await
            .0,
        201
    );
}

/// **`space = "view"` geometry spans only identical frames**, and the refusal is the row's — the
/// space is a fact about the submission, so it cannot be known at the declaration.
#[tokio::test]
async fn a_view_space_shape_over_two_frames_is_refused_at_the_row() {
    let tmp = TempDir::new().unwrap();
    let server = serve_two_frames(&tmp).await;
    assert_eq!(
        register(&server, spatial_layer("regions/vs", &["world", "europe"]))
            .await
            .0,
        201
    );
    let (status, body) = publish(
        &server,
        "regions/vs",
        json!({
            "addressing": "external",
            "artifacts": [{ "key": "uk", "members": [], "bbox": [0.4, 0.2, 0.5, 0.3] }]
        }),
    )
    .await;
    assert_eq!(status, 422, "{body}");
    let detail = body.to_string();
    assert!(detail.contains("wgs84"), "{detail}");
    assert!(detail.contains("europe") || detail.contains("world"), "{detail}");
}
