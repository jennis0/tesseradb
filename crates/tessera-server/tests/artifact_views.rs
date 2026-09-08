//! **`view` in an artifact's identity on the wire** (`ingest.md` §1.5; `views.md` §3.5): a layer
//! whose `scope` names a group carries a different artifact set per view, each artifact belonging
//! to one, keys unique per `(layer, view)` and no edge crossing a view.
//!
//! What is asserted, over the real routes: a publication into such a layer without a `view` is a
//! `422` and one into an entity-scoped layer with a `view` is a `422`; the same key in two views
//! is two artifacts with two identifiers, each drawn on its own view and on no other; a parent in
//! another view is refused naming the crossing; and both survive a restart, the identity coming
//! back off the log.

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
use tessera_build::{build, BuildArgs, GroupDescriptor, GroupViewDescriptor, Quantisation};

/// The group's two views, each drawn over the whole corpus so that a membership means the same
/// thing in both and the counts differ only by what the caller published.
const KEYS: [&str; 2] = ["q1", "q2"];
const SCOPED: &str = "clusters/quarterly";
const PLAIN: &str = "clusters/whole";
const ITEMS: u64 = 400;

fn member(source_id: u64) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(external_id_of(source_id))
}

fn members(range: std::ops::Range<u64>) -> Vec<String> {
    range.map(member).collect()
}

fn write_points(path: &Path, offset: f64) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
    ]));
    let ids: Vec<u64> = (0..ITEMS).collect();
    let xs: Vec<f64> = ids
        .iter()
        .map(|e| ((e * 7) % 900) as f64 + offset)
        .collect();
    let ys: Vec<f64> = ids.iter().map(|e| ((e * 11) % 900) as f64).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

/// A two-view group over one corpus: every entity has a row in each view, so an artifact of one
/// view could be projected into the other's row space — which is exactly the mistake `view` in
/// the identity prevents.
fn build_group(dir: &Path) -> std::path::PathBuf {
    let pairs = dir.join("pairs.parquet");
    write_pairs_n(&pairs, ITEMS);
    let views: Vec<tessera_build::ViewArgs> = KEYS
        .iter()
        .enumerate()
        .map(|(slot, key)| {
            let points = dir.join(format!("{key}.parquet"));
            write_points(&points, slot as f64 * 10.0);
            tessera_build::ViewArgs {
                visibility: None,
                view_id: format!("quarter:{key}"),
                projection: tessera_spatial::Projection::None,
                extent: extent(),
                points,
                point_fields: Default::default(),
                select: None,
                access: tessera_build::config::AccessInput::relation(pairs.clone()),
            }
        })
        .collect();
    let e = extent();
    let out = dir.join("bundle");
    build(&BuildArgs {
        views,
        anchor: 0,
        groups: vec![GroupDescriptor {
            title: None,
            point_default: Some("public".to_string()),
            visibility: None,
            name: "quarter".to_string(),
            members_of: None,
            scoped_scalars: Vec::new(),
            quantisation: Quantisation {
                x_min: e.x_min,
                x_max: e.x_max,
                y_min: e.y_min,
                y_max: e.y_max,
            },
            projection: tessera_spatial::Projection::None,
            metadata: Vec::new(),
            views: KEYS
                .iter()
                .map(|key| GroupViewDescriptor {
                    key: key.to_string(),
                    visibility: None,
                    metadata: Default::default(),
                })
                .collect(),
        }],
        scoped_attributes: Vec::new(),
        attribute_sources: Vec::new(),
        out: out.clone(),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: FIXTURE_IDSET,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        mint_external_ids: true,
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: Default::default(),
    })
    .expect("the two-view group builds");
    out
}

async fn open(tmp: &TempDir) -> TestServer {
    spawn_server(
        &tmp.path().join("bundle"),
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await
}

async fn serve(tmp: &TempDir) -> TestServer {
    build_group(tmp.path());
    open(tmp).await
}

/// `scope` names the group where `group` is `Some`; the layer is drawn on the group's views
/// either way, which at runtime is the view ids themselves.
fn declaration(name: &str, group: Option<&str>, kind: &str) -> serde_json::Value {
    json!({
        "name": name,
        "title": name,
        "views": KEYS.iter().map(|k| format!("quarter:{k}")).collect::<Vec<_>>(),
        "membership": "enumerated",
        "value_set": "closed",
        "visibility": null,
        "artifact_visibility": { "field": null, "default": "inherited" },
        "require_member_visibility": null,
        "hierarchy": { "kind": kind, "prune_children": false },
        "content": { "computed": [], "supplied": [] },
        "depends_on": [],
        "levels": [],
        "scope": match group {
            Some(group) => json!({ "group": group }),
            None => json!("entity"),
        }
    })
}

async fn register(server: &TestServer, declaration: serde_json::Value) {
    let resp = server
        .client
        .put(server.control_url("/control/layers"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&declaration)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        201,
        "the layer registers: {:?}",
        resp.text().await
    );
}

async fn put(
    server: &TestServer,
    layer: &str,
    artifacts: serde_json::Value,
) -> (u16, serde_json::Value) {
    let resp = server
        .client
        .put(server.control_url(&format!(
            "/control/layers/{}/artifacts",
            layer.replace('/', "%2F")
        )))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({ "addressing": "external", "artifacts": artifacts }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(serde_json::Value::Null))
}

/// One view's served artifact rows of a layer, by key — the answer a viewer is given.
async fn served(server: &TestServer, view: &str, layer: &str) -> Vec<(String, u64)> {
    let auth = authorise(server, &["0"]).await;
    let token = auth["token"].as_str().unwrap();
    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&json!({
            "view": view,
            "zoom": 0,
            "bbox": [0.0, 0.0, 1000.0, 1000.0],
            "k": 200,
            "layers": [layer],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let mut rows: Vec<(String, u64)> = decode_viewport_frames(&resp.bytes().await.unwrap())
        .artifacts
        .unwrap_or_default()
        .into_iter()
        .filter(|row| row.layer == layer)
        .filter_map(|row| row.key.clone().map(|key| (key, row.masked_count)))
        .collect();
    rows.sort();
    rows
}

/// **The view is required, refused where the layer has one set, and part of the key's uniqueness
/// scope**: one key in two views is two artifacts, each drawn on its own view, and both come back
/// from a restart under the same identifiers.
#[tokio::test]
async fn one_key_in_two_views_is_two_artifacts_each_drawn_on_its_own_view() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    register(&server, declaration(SCOPED, Some("quarter"), "flat")).await;
    register(&server, declaration(PLAIN, None, "flat")).await;

    // Absent where the layer's artifacts are a set per view.
    let (status, body) = put(
        &server,
        SCOPED,
        json!([{ "key": "c1", "members": members(0..10) }]),
    )
    .await;
    assert_eq!(status, 422, "{body}");
    let detail = body["detail"].as_str().unwrap_or_default().to_string();
    assert!(detail.contains("no `view`"), "{detail}");
    assert!(detail.contains("quarter"), "{detail}");

    // Named where the layer has one set drawn on every view.
    let (status, body) = put(
        &server,
        PLAIN,
        json!([{ "key": "c1", "view": "q1", "members": members(0..10) }]),
    )
    .await;
    assert_eq!(status, 422, "{body}");
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("entity-scoped"),
        "{body}"
    );

    // One key, two views, one batch: two artifacts and two identifiers.
    let (status, body) = put(
        &server,
        SCOPED,
        json!([
            { "key": "c1", "view": "q1", "members": members(0..10) },
            { "key": "c1", "view": "q2", "members": members(0..30) },
        ]),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["created"], 2, "{body}");
    let first = body["artifacts"][0]["tessera_id"].as_str().unwrap();
    let second = body["artifacts"][1]["tessera_id"].as_str().unwrap();
    assert_ne!(first, second, "two artifacts, two identities: {body}");
    let first = first.to_string();

    // Each is drawn on its own view and on no other: the counts are the memberships the caller
    // published in each, not one artifact projected into both row spaces.
    assert_eq!(
        served(&server, "quarter:q1", SCOPED).await,
        vec![("c1".to_string(), 10)]
    );
    assert_eq!(
        served(&server, "quarter:q2", SCOPED).await,
        vec![("c1".to_string(), 30)]
    );

    // The identity comes back off the log, so a re-`PUT` of the same key in the same view is the
    // held artifact rather than a third.
    drop(server);
    let server = open(&tmp).await;
    assert_eq!(
        served(&server, "quarter:q1", SCOPED).await,
        vec![("c1".to_string(), 10)],
        "the view replayed with the artifact"
    );
    assert_eq!(
        served(&server, "quarter:q2", SCOPED).await,
        vec![("c1".to_string(), 30)]
    );
    let (status, body) = put(
        &server,
        SCOPED,
        json!([{ "key": "c1", "view": "q1", "members": members(0..10) }]),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["created"], 0, "{body}");
    assert_eq!(body["artifacts"][0]["tessera_id"], first, "{body}");
}

/// **An edge may not cross views** (`views.md` §3.5): a parent this level holds in another view
/// is a `422` naming where the key is held, and the same key in the child's own view resolves.
#[tokio::test]
async fn a_parent_in_another_view_is_refused() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    register(&server, declaration(SCOPED, Some("quarter"), "nested")).await;

    let (status, body) = put(
        &server,
        SCOPED,
        json!([{ "key": "root", "view": "q1", "members": members(0..20) }]),
    )
    .await;
    assert_eq!(status, 201, "{body}");

    let (status, body) = put(
        &server,
        SCOPED,
        json!([{ "key": "leaf", "view": "q2", "members": members(0..5), "parent": ["root"] }]),
    )
    .await;
    assert_eq!(status, 422, "{body}");
    let detail = body["detail"].as_str().unwrap_or_default().to_string();
    assert!(
        detail.contains("q1"),
        "the refusal says where it is held: {detail}"
    );
    assert!(detail.contains("cross"), "{detail}");

    // The same edge inside one view is the ordinary case.
    let (status, body) = put(
        &server,
        SCOPED,
        json!([{ "key": "leaf", "view": "q1", "members": members(0..5), "parent": ["root"] }]),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(
        served(&server, "quarter:q1", SCOPED).await,
        vec![("leaf".to_string(), 5), ("root".to_string(), 20)]
    );
}
