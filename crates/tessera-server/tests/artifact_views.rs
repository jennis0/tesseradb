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

/// The group's two views. **q1 draws the whole corpus and q2 its first half**, so a per-view
/// answer — the complement of an exclusion, above all — is a different number in each and a
/// route that took the wrong view's entities is caught by the count rather than by nothing.
const KEYS: [&str; 2] = ["q1", "q2"];
/// How many of the corpus's entities have a row in each view, by position in [`KEYS`].
const IN_VIEW: [u64; 2] = [ITEMS, ITEMS / 2];
const SCOPED: &str = "clusters/quarterly";
const PLAIN: &str = "clusters/whole";
const SHAPES: &str = "regions/quarterly";
const ITEMS: u64 = 400;

fn member(source_id: u64) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(external_id_of(source_id))
}

fn members(range: std::ops::Range<u64>) -> Vec<String> {
    range.map(member).collect()
}

fn write_points(path: &Path, offset: f64, ids: std::ops::Range<u64>) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
    ]));
    let ids: Vec<u64> = ids.collect();
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

/// A two-view group over one corpus: every entity of q2 has a row in q1 as well, so an artifact
/// of one view could be projected into the other's row space — which is exactly the mistake
/// `view` in the identity prevents.
fn build_group(dir: &Path) -> std::path::PathBuf {
    let pairs = dir.join("pairs.parquet");
    write_pairs_n(&pairs, ITEMS);
    let views: Vec<tessera_build::ViewArgs> = KEYS
        .iter()
        .enumerate()
        .map(|(slot, key)| {
            let points = dir.join(format!("{key}.parquet"));
            write_points(&points, slot as f64 * 10.0, 0..IN_VIEW[slot]);
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

    // A view no view of the bundle answers to is refused rather than acked and drawn nowhere.
    let (status, body) = put(
        &server,
        SCOPED,
        json!([{ "key": "c1", "view": "q9", "members": members(0..10) }]),
    )
    .await;
    assert_eq!(status, 422, "{body}");
    let detail = body["detail"].as_str().unwrap_or_default().to_string();
    assert!(detail.contains("q9"), "{detail}");
    assert!(
        detail.contains("q1, q2"),
        "the keys held are named: {detail}"
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
    server.shutdown().await;
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

async fn wait_until(
    server: &TestServer,
    what: &str,
    done: impl Fn(&tessera_engine::ExecutorStats) -> bool,
) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        if done(&server.state.engine.write_executor_stats()) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{what}: never happened"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// Flush, then fold — `grow_memberships.rs`' sequence, and its reason: a flush with nothing
/// buffered publishes nothing, so a row is ingested first to give the fold something to fold.
async fn flush_and_fold(server: &TestServer) {
    let ingested = external_id_of(9_001);
    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "views-fold")
        // A multi-view bundle names the batch's view: which one a row belongs to is not
        // inferable (contracts §3.4).
        .header("x-tessera-view", format!("quarter:{}", KEYS[0]))
        .header("content-type", "application/vnd.apache.arrow.stream")
        .body(build_ingest_batch_optional(&[(
            Some(&ingested[..]),
            10.0,
            10.0,
            "0",
        )]))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "{}",
        resp.text().await.unwrap()
    );
    let before = server.state.engine.write_executor_stats();
    let resp = server
        .client
        .post(server.control_url("/control/flush"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 202);
    wait_until(server, "the flush published", move |now| {
        now.flushes > before.flushes
    })
    .await;
    let before = server.state.engine.write_executor_stats();
    let resp = server
        .client
        .post(server.control_url("/control/compact"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 202);
    wait_until(server, "the fold published", move |now| {
        assert_eq!(
            now.fold_failures, before.fold_failures,
            "the fold was discarded rather than published"
        );
        now.folds > before.folds
    })
    .await;
}

/// **The view survives the fold and the reopen** (`bundle_format` 8): the fold packs each record
/// into a membership extent and a restart seeds the store from those bytes, so a view lost there
/// would collapse two views' keys into one index and draw one view's artifacts on every view of
/// the group. Asserted on the served answers and on the key's uniqueness scope afterwards.
#[tokio::test]
async fn a_group_scoped_level_survives_a_fold_and_a_reopen_with_its_views() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    register(&server, declaration(SCOPED, Some("quarter"), "flat")).await;

    let (status, body) = put(
        &server,
        SCOPED,
        json!([
            { "key": "c1", "view": "q1", "members": members(0..10) },
            { "key": "c1", "view": "q2", "members": members(0..30) },
            { "key": "c2", "view": "q2", "members": members(0..50) },
        ]),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let q1 = body["artifacts"][0]["tessera_id"]
        .as_str()
        .unwrap()
        .to_string();

    flush_and_fold(&server).await;
    assert_eq!(
        served(&server, "quarter:q1", SCOPED).await,
        vec![("c1".to_string(), 10)],
        "the folded level keeps q1's one artifact"
    );
    assert_eq!(
        served(&server, "quarter:q2", SCOPED).await,
        vec![("c1".to_string(), 30), ("c2".to_string(), 50)]
    );

    // Reopened from the packed extents, with the log's publication behind the fold's high-water:
    // the views come back off the blob.
    server.shutdown().await;
    let server = open(&tmp).await;
    assert_eq!(
        served(&server, "quarter:q1", SCOPED).await,
        vec![("c1".to_string(), 10)],
        "the view came back off the packed extent"
    );
    assert_eq!(
        served(&server, "quarter:q2", SCOPED).await,
        vec![("c1".to_string(), 30), ("c2".to_string(), 50)]
    );

    // And the key's uniqueness scope is still the view's: `c1` in q1 is the held artifact, not a
    // key the restored index has confused with q2's.
    let (status, body) = put(
        &server,
        SCOPED,
        json!([{ "key": "c1", "view": "q1", "members": members(0..10) }]),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["created"], 0, "{body}");
    assert_eq!(body["artifacts"][0]["tessera_id"], q1, "{body}");
}

/// A shape layer scoped to the group: its membership is the rows inside the box, resolved per
/// view, which is the route a stored membership does not take.
fn spatial_declaration(name: &str) -> serde_json::Value {
    json!({
        "name": name,
        "title": name,
        "views": KEYS.iter().map(|k| format!("quarter:{k}")).collect::<Vec<_>>(),
        "membership": "spatial",
        "shape": { "kind": "bbox" },
        "visibility": null,
        "artifact_visibility": { "field": null, "default": "inherited" },
        "require_member_visibility": null,
        "hierarchy": { "kind": "flat", "prune_children": false },
        "content": { "computed": [], "supplied": [] },
        "depends_on": [],
        "levels": [],
        "scope": { "group": "quarter" }
    })
}

/// **A spatial level is per view too** (`views.md` §3.5): a shape published into one view of a
/// group has no membership and no count on another view of the same group. The three spatial row
/// forms — the resolved build, the shape level's decomposition and the inverted column — each
/// read the level, and one that read it whole would give a polygon published into q2 a real,
/// nonzero masked count on q1's map.
#[tokio::test]
async fn a_shape_published_into_one_view_is_drawn_on_no_other() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    register(&server, spatial_declaration(SHAPES)).await;

    // One box over the whole extent, published into q2 alone. Every row of both views is inside
    // it, so a form built over the wrong view's records would count them.
    let (status, body) = put(
        &server,
        SHAPES,
        json!([{ "key": "everywhere", "view": "q2", "members": [], "bbox": [0.0, 0.0, 1000.0, 1000.0] }]),
    )
    .await;
    assert_eq!(status, 201, "{body}");

    assert_eq!(
        served(&server, "quarter:q2", SHAPES).await,
        vec![("everywhere".to_string(), IN_VIEW[1])],
        "the shape resolves over its own view's rows"
    );
    assert_eq!(
        served(&server, "quarter:q1", SHAPES).await,
        Vec::new(),
        "and is drawn on no other view of the group"
    );

    // The same after a fold, which rebuilds the level's forms from the packed records.
    flush_and_fold(&server).await;
    assert_eq!(
        served(&server, "quarter:q1", SHAPES).await,
        Vec::new(),
        "the fold's rebuild keeps the shape in its own view"
    );
    assert!(
        !served(&server, "quarter:q2", SHAPES).await.is_empty(),
        "and keeps it in that one"
    );
}

/// **An exclusion on a group-scoped layer complements against its own view's entities**
/// (`ingest.md` §2.3): the artifact names a view's key and the generation holds view ids, so the
/// key is resolved against the layer's declared views — q2 holds half the corpus, and its
/// complement is that half less the list rather than the whole corpus or nothing at all.
#[tokio::test]
async fn a_group_scoped_exclusion_complements_against_its_own_views_entities() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    register(&server, declaration(SCOPED, Some("quarter"), "flat")).await;

    let (status, body) = put(
        &server,
        SCOPED,
        json!([
            { "key": "rest", "view": "q1", "excluding": members(0..3) },
            { "key": "rest", "view": "q2", "excluding": members(0..3) },
        ]),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["created"], 2, "{body}");

    assert_eq!(
        served(&server, "quarter:q1", SCOPED).await,
        vec![("rest".to_string(), IN_VIEW[0] - 3)],
        "q1's complement is q1's entities less the three named"
    );
    assert_eq!(
        served(&server, "quarter:q2", SCOPED).await,
        vec![("rest".to_string(), IN_VIEW[1] - 3)],
        "q2's is its own half, not the corpus and not nothing"
    );
}
