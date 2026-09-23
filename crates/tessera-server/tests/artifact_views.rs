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

use std::collections::BTreeSet;
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
fn build_group(dir: &Path, gate_first_view: bool) -> std::path::PathBuf {
    // **`gate_first_view` puts q1 behind the label `1`** (`views.md` §6), which only the entities
    // divisible by three carry (`common::terms_of`). A principal holding `0` alone then reaches q2
    // and not q1, which is the visible-view set the identifier case below needs.
    let gate = |slot: usize| (gate_first_view && slot == 0).then(|| vec!["1".to_string()]);
    let pairs = dir.join("pairs.parquet");
    write_pairs_n(&pairs, ITEMS);
    let views: Vec<tessera_build::ViewArgs> = KEYS
        .iter()
        .enumerate()
        .map(|(slot, key)| {
            let points = dir.join(format!("{key}.parquet"));
            write_points(&points, slot as f64 * 10.0, 0..IN_VIEW[slot]);
            tessera_build::ViewArgs {
                visibility: gate(slot),
                ..view_args(
                    &format!("quarter:{key}"),
                    &points,
                    AccessInput::relation(&pairs),
                )
            }
        })
        .collect();
    let e = extent();
    let out = dir.join("bundle");
    build(&BuildArgs {
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
                .enumerate()
                .map(|(slot, key)| GroupViewDescriptor {
                    key: key.to_string(),
                    visibility: gate(slot),
                    metadata: Default::default(),
                })
                .collect(),
        }],
        ..build_args(&out, views)
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
    build_group(tmp.path(), false);
    open(tmp).await
}

/// The same fixture with q1 behind a gate — see [`build_group`].
async fn serve_gated(tmp: &TempDir) -> TestServer {
    build_group(tmp.path(), true);
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
    let token = auth["token"].as_str().unwrap().to_string();
    served_as(server, &token, view, layer).await
}

/// The same frame for a principal already authorised — a gated view needs a credential `served`'s
/// own does not carry.
async fn served_as(
    server: &TestServer,
    token: &str,
    view: &str,
    layer: &str,
) -> Vec<(String, u64)> {
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

/// One principal's browse page of a layer on one view: key → masked count, ascending.
async fn browsed(server: &TestServer, token: &str, view: &str, layer: &str) -> Vec<(String, u64)> {
    let resp = server
        .client
        .post(server.viewer_url("/v1/artifacts/browse"))
        .bearer_auth(token)
        .json(&json!({ "view": view, "layer": layer, "limit": 200 }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "{:?}", resp.text().await);
    let body: serde_json::Value = resp.json().await.unwrap();
    let mut rows: Vec<(String, u64)> = body["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["key"].as_str().unwrap().to_string(),
                row["masked_count"].as_u64().unwrap(),
            )
        })
        .collect();
    rows.sort();
    rows
}

/// `POST /v1/artifacts/{id}` on one view, as a status and a body.
async fn drilled(
    server: &TestServer,
    token: &str,
    view: &str,
    tessera_id: &str,
) -> (u16, serde_json::Value) {
    let resp = server
        .client
        .post(server.viewer_url(&format!("/v1/artifacts/{tessera_id}")))
        .bearer_auth(token)
        .json(&json!({ "view": view }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(serde_json::Value::Null))
}

async fn token_for(server: &TestServer, terms: &[&str]) -> String {
    authorise(server, terms).await["token"]
        .as_str()
        .unwrap()
        .to_string()
}

/// **A view of a group-scoped layer serves that view's artifacts and no others** (`views.md`
/// §3.5). An artifact of another view of the group is absent entire — from the viewport's
/// artifacts frame, from browse, and from the identifier route — rather than served with a masked
/// count of zero beside its key and its `tessera_id`.
///
/// The layer declares no existence criterion, which is the condition that makes the distinction
/// observable: a zero count clears no bar, so nothing else withholds the row. The two views carry
/// **different keys**, so what a wrong answer hands over is the other view's name as well as its
/// identifier.
///
/// q1 is gated and q2 is not, so the second principal here holds q2 alone in its visible-view set
/// and can reach q1's artifact by no route at all: q1 is a 404 to it, and q2's own verbs are asked
/// for q1's key and for q1's identifier.
#[tokio::test]
async fn an_artifact_of_another_view_of_the_group_is_absent_from_every_verb() {
    let tmp = TempDir::new().unwrap();
    let server = serve_gated(&tmp).await;
    register(&server, declaration(SCOPED, Some("quarter"), "flat")).await;

    let (status, body) = put(
        &server,
        SCOPED,
        json!([
            { "key": "only-q1", "view": "q1", "members": members(0..10) },
            { "key": "only-q2", "view": "q2", "members": members(0..30) },
        ]),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let q1_id = body["artifacts"][0]["tessera_id"]
        .as_str()
        .unwrap()
        .to_string();
    let q2_id = body["artifacts"][1]["tessera_id"]
        .as_str()
        .unwrap()
        .to_string();

    // A principal who reaches both views, so the asymmetry is the layer's and not the gate's.
    let both = token_for(&server, &["0", "1"]).await;
    // A principal whose visible-view set holds q2 alone.
    let only_q2 = token_for(&server, &["0"]).await;

    // Twice: over the publication the control plane took, and again over the packed records a
    // fold rebuilds the level's forms from.
    for round in ["published", "folded"] {
        if round == "folded" {
            flush_and_fold(&server).await;
        }
        assert_eq!(
            browsed(&server, &both, "quarter:q1", SCOPED).await,
            vec![("only-q1".to_string(), 10)],
            "{round}: q1 browses its own artifact alone"
        );
        assert_eq!(
            browsed(&server, &both, "quarter:q2", SCOPED).await,
            vec![("only-q2".to_string(), 30)],
            "{round}: q2 browses its own artifact alone"
        );
        assert_eq!(
            served_as(&server, &both, "quarter:q1", SCOPED).await,
            vec![("only-q1".to_string(), 10)],
            "{round}: and the artifacts frame agrees with browse"
        );
        assert_eq!(
            served_as(&server, &both, "quarter:q2", SCOPED).await,
            vec![("only-q2".to_string(), 30)],
            "{round}: and the artifacts frame agrees with browse"
        );

        // The identifier route, each view asked for the other's artifact. Each resolves its own,
        // so a `404` here is the view's answer and not an identifier that names nothing.
        assert_eq!(
            drilled(&server, &both, "quarter:q1", &q1_id).await.0,
            200,
            "{round}: q1's own artifact resolves on q1"
        );
        assert_eq!(
            drilled(&server, &both, "quarter:q1", &q2_id).await.0,
            404,
            "{round}: q2's artifact is not reachable on q1 by identifier"
        );
        assert_eq!(
            drilled(&server, &both, "quarter:q2", &q1_id).await.0,
            404,
            "{round}: q1's artifact is not reachable on q2 by identifier"
        );

        // The principal outside q1 entirely: q1 is a 404 as a view, and q2 hands over neither
        // q1's key nor its artifact for q1's identifier.
        let (status, _) = drilled(&server, &only_q2, "quarter:q1", &q1_id).await;
        assert_eq!(
            status, 404,
            "{round}: the gated view is a 404 to this principal"
        );
        assert_eq!(
            browsed(&server, &only_q2, "quarter:q2", SCOPED).await,
            vec![("only-q2".to_string(), 30)],
            "{round}: q2's browse names no artifact of the view this principal cannot reach"
        );
        assert_eq!(
            drilled(&server, &only_q2, "quarter:q2", &q1_id).await.0,
            404,
            "{round}: nor does q2's identifier route hand it over"
        );
    }

    // And across a restart, the store seeded from the packed extents rather than from the log.
    server.shutdown().await;
    let server = open(&tmp).await;
    let both = token_for(&server, &["0", "1"]).await;
    let only_q2 = token_for(&server, &["0"]).await;
    assert_eq!(
        browsed(&server, &both, "quarter:q2", SCOPED).await,
        vec![("only-q2".to_string(), 30)],
        "reopened: q2 browses its own artifact alone"
    );
    assert_eq!(
        drilled(&server, &both, "quarter:q2", &q1_id).await.0,
        404,
        "reopened: and resolves no artifact of q1"
    );
    assert_eq!(
        drilled(&server, &only_q2, "quarter:q2", &q1_id).await.0,
        404,
        "reopened: for the principal outside q1 either"
    );
}

/// The rows one view's request matched under a filter — what a `member_of` leaf naming an
/// artifact resolves to, summed over the tiles.
async fn matched(server: &TestServer, token: &str, view: &str, filters: serde_json::Value) -> u64 {
    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&json!({
            "view": view,
            "zoom": 0,
            "bbox": [0.0, 0.0, 1000.0, 1000.0],
            "k": 200,
            "filters": filters,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "{:?}", resp.text().await);
    let (tiles, _) = decode_viewport(&resp.bytes().await.unwrap());
    tiles.iter().map(|tile| tile.2).sum()
}

/// A `member_of` leaf over the scoped layer, by identifier.
fn member_of(tessera_id: &str) -> serde_json::Value {
    json!({ "member_of": { "layer": SCOPED, "artifact": tessera_id } })
}

/// **One flush tick, asked for and waited on** — the one moment a held row form takes the
/// interval's deltas (`ingest.md` §1.3). `POST /control/flush` pulls the tick's deadline forward
/// rather than publishing off the cadence, and with nothing buffered the tick publishes the row
/// forms and no geometry, which is the whole of what this needs.
async fn ticked(server: &TestServer) {
    let before = server.state.engine.write_executor_stats().ticks;
    let resp = server
        .client
        .post(server.control_url("/control/flush"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 202);
    wait_until(
        server,
        "the tick published the interval's deltas",
        move |now| now.ticks > before,
    )
    .await;
}

/// What the second write into the other view is: a new artifact, or more members for one that
/// view already holds. The first reaches a held form as `DeltaKind::Published`, the second as
/// `DeltaKind::Grown` — two arms of the same amendment, and each is confined to its own view's
/// ordinals or neither is.
#[derive(Clone, Copy, PartialEq)]
enum Second {
    Published,
    Grown,
}

/// **A held row form takes no delta of another view of its group** (`views.md` §3.5) — the warm
/// path, which no cold projection covers.
///
/// A form is built per `(view, layer, level)` from the store's own per-view slice, so a view that
/// has never been browsed cannot hold another view's artifact. But the tick applies one interval's
/// deltas to *every* held form of the level, so a view whose form is already warm is the case
/// where a write into another view of the group can reach it. The sequence is exactly that: browse
/// A, which builds and holds A's form; write into B; let the tick pass; browse A again.
///
/// What a wrong answer hands a principal of A: B's key, B's `tessera_id` and a live count of A's
/// own rows — and with them the drill-down, the `member_of` highlight and the region leaves, which
/// all read the same form.
async fn a_warm_form_takes_no_delta_of_another_view(warm: usize, second: Second) {
    let tmp = TempDir::new().unwrap();
    let server = serve_gated(&tmp).await;
    register(&server, declaration(SCOPED, Some("quarter"), "flat")).await;
    let (a, b) = (KEYS[warm], KEYS[1 - warm]);
    let (a_view, b_view) = (format!("quarter:{a}"), format!("quarter:{b}"));
    let (a_key, b_key) = (format!("only-{a}"), format!("only-{b}"));
    // **B's membership is drawn from the whole corpus**, so every one of its members has a row in
    // A's row space as well: an artifact of B projected into A's form would carry a real count
    // there rather than an empty one, which is the answer this asserts against.
    let b_members = 30;

    // A's own artifact, and — for the growth arm — B's, published before A's form is warm so that
    // the only thing reaching that form afterwards is the growth delta.
    let mut first = vec![json!({ "key": a_key, "view": a, "members": members(0..10) })];
    if second == Second::Grown {
        first.push(json!({ "key": b_key, "view": b, "members": members(0..5) }));
    }
    let (status, body) = put(&server, SCOPED, json!(first)).await;
    assert_eq!(status, 201, "{body}");
    let a_id = body["artifacts"][0]["tessera_id"]
        .as_str()
        .unwrap()
        .to_string();

    let both = token_for(&server, &["0", "1"]).await;
    // **The warming read.** After this the level's form for A is in the map, and every tick from
    // here amends it in place rather than rebuilding it.
    assert_eq!(
        browsed(&server, &both, &a_view, SCOPED).await,
        vec![(a_key.clone(), 10)],
        "the premise: A holds its own artifact and its form is now warm"
    );

    let (status, body) = put(
        &server,
        SCOPED,
        json!([{ "key": b_key, "view": b, "members": members(0..b_members) }]),
    )
    .await;
    assert_eq!(
        status,
        if second == Second::Grown { 200 } else { 201 },
        "{body}"
    );
    let b_id = body["artifacts"][0]["tessera_id"]
        .as_str()
        .unwrap()
        .to_string();
    ticked(&server).await;

    // Both principals reach A: one who reaches the whole group, and — where A is the ungated view
    // — one whose visible-view set holds A alone and which can reach B by no route at all.
    let mut principals = vec![("both views", both.clone())];
    if a == KEYS[1] {
        principals.push(("A alone", token_for(&server, &["0"]).await));
    }
    for (who, token) in &principals {
        assert_eq!(
            browsed(&server, token, &a_view, SCOPED).await,
            vec![(a_key.clone(), 10)],
            "{who}: A's browse page names A's artifact alone after the tick"
        );
        assert_eq!(
            served_as(&server, token, &a_view, SCOPED).await,
            vec![(a_key.clone(), 10)],
            "{who}: and A's artifacts frame agrees with it"
        );
        assert_eq!(
            drilled(&server, token, &a_view, &b_id).await.0,
            404,
            "{who}: B's artifact is not reachable on A by identifier"
        );
        assert_eq!(
            matched(&server, token, &a_view, member_of(&b_id)).await,
            0,
            "{who}: and a `member_of` leaf naming it matches no row of A"
        );
        // The same leaf over A's own artifact, so what is asserted above is the view's answer and
        // not a filter that matches nothing whatever it is given.
        assert_eq!(
            matched(&server, token, &a_view, member_of(&a_id)).await,
            10,
            "{who}: A's own artifact still highlights its members"
        );
    }

    // And B is unharmed: the amendment was confined, not dropped.
    assert_eq!(
        browsed(&server, &both, &b_view, SCOPED).await,
        vec![(b_key, b_members)],
        "B holds the write that was made into it"
    );
}

/// [`a_warm_form_takes_no_delta_of_another_view`] with q1 warm and the publication into q2.
#[tokio::test]
async fn a_warm_q1_form_takes_no_publication_of_q2() {
    a_warm_form_takes_no_delta_of_another_view(0, Second::Published).await;
}

/// The same with the roles swapped — and with a principal whose visible-view set holds q2 alone,
/// which q2 being the ungated view is what makes possible.
#[tokio::test]
async fn a_warm_q2_form_takes_no_publication_of_q1() {
    a_warm_form_takes_no_delta_of_another_view(1, Second::Published).await;
}

/// The growth arm: the second write is more members for an artifact the other view already holds,
/// which reaches the held form as a membership join rather than as a publication.
#[tokio::test]
async fn a_warm_q1_form_takes_no_growth_of_q2() {
    a_warm_form_takes_no_delta_of_another_view(0, Second::Grown).await;
}

/// The growth arm with the roles swapped.
#[tokio::test]
async fn a_warm_q2_form_takes_no_growth_of_q1() {
    a_warm_form_takes_no_delta_of_another_view(1, Second::Grown).await;
}

/// A group view dropped and created again at a running service, fed as many rows as its
/// predecessor held and none of them inside the level's shape, answers the same after a restart
/// as before it; the untouched view answers the same throughout.
///
/// The level takes no publication after the fold, so its version is still the one the fold
/// stamped the predecessor's row-major column with, and only the incarnation tells the two
/// views' row spaces apart.
#[tokio::test]
async fn a_recreated_view_takes_no_row_structure_of_the_view_it_replaced_at_a_restart() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    let mut declaration = spatial_declaration(SHAPES);
    declaration["layout"] = json!("row_major_label");
    register(&server, declaration).await;
    let (status, body) = put(
        &server,
        SHAPES,
        json!([
            { "key": "left", "view": "q1", "members": [], "bbox": [0.0, 0.0, 500.0, 1000.0] },
            { "key": "left", "view": "q2", "members": [], "bbox": [0.0, 0.0, 500.0, 1000.0] },
        ]),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    flush_and_fold(&server).await;
    let untouched = served(&server, "quarter:q1", SHAPES).await;
    assert!(!untouched.is_empty(), "the control view draws its shape");

    recreate(&server, "q2").await;
    ingest_right_of_the_shape(&server, "quarter:q2", "recreated-q2").await;
    flush(&server).await;

    let recreated = served(&server, "quarter:q2", SHAPES).await;
    assert!(
        recreated.iter().all(|(_, count)| *count == 0),
        "no row of the recreated view is inside the shape: {recreated:?}"
    );
    assert_eq!(served(&server, "quarter:q1", SHAPES).await, untouched);

    server.shutdown().await;
    let server = open(&tmp).await;
    assert_eq!(
        served(&server, "quarter:q2", SHAPES).await,
        recreated,
        "the recreated view answers as it did before the restart"
    );
    assert_eq!(served(&server, "quarter:q1", SHAPES).await, untouched);
}

/// Drop a view of the group and create it again under the same key.
async fn recreate(server: &TestServer, key: &str) {
    drop_key(server, key).await;
    create_key(server, key).await;
}

/// Drop a view of the group, leaving its items undeleted.
async fn drop_key(server: &TestServer, key: &str) {
    let dropped = server
        .client
        .delete(server.control_url(&format!(
            "/control/views/quarter/{key}?delete_dangling=false"
        )))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(dropped.status().as_u16(), 200);
}

/// Create a view of the group under `key`.
async fn create_key(server: &TestServer, key: &str) {
    let created = server
        .client
        .put(server.control_url(&format!("/control/views/quarter/{key}")))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status().as_u16(), 201);
}

/// As many new items as q2 was built with, every one right of the shapes these tests publish,
/// returning their `tessera_id`s.
async fn ingest_right_of_the_shape(server: &TestServer, view: &str, batch_id: &str) -> BTreeSet<u64> {
    let ids: Vec<Vec<u8>> = (0..IN_VIEW[1]).map(|i| external_id_of(20_000 + i)).collect();
    let rows: Vec<(Option<&[u8]>, f32, f32, &str)> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| (Some(&id[..]), 600.0 + (i % 300) as f32, (i * 3 % 1000) as f32, "0"))
        .collect();
    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", batch_id)
        .header("x-tessera-view", view)
        .header("content-type", "application/vnd.apache.arrow.stream")
        .body(build_ingest_batch_optional(&rows))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    body["tessera_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap())
        .collect()
}

async fn flush(server: &TestServer) {
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
}

/// Request a fold and wait until it has either published or been discarded, returning whether
/// it published.
async fn fold_settled(server: &TestServer) -> bool {
    let before = server.state.engine.write_executor_stats();
    let resp = server
        .client
        .post(server.control_url("/control/compact"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 202);
    wait_until(server, "the fold settled", move |now| {
        now.folds > before.folds || now.fold_failures > before.fold_failures
    })
    .await;
    server.state.engine.write_executor_stats().folds > before.folds
}

/// The `tessera_id`s one view serves over the whole extent, to a principal holding every term.
async fn points_of(server: &TestServer, view: &str) -> BTreeSet<u64> {
    points_as(server, &["0", "1"], view).await
}

/// The same, to a principal holding `terms`.
async fn points_as(server: &TestServer, terms: &[&str], view: &str) -> BTreeSet<u64> {
    let auth = authorise(server, terms).await;
    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(auth["token"].as_str().unwrap())
        .json(&json!({"view": view, "zoom": 8, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 1000}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let (_, points) = decode_viewport(&resp.bytes().await.unwrap());
    points.into_iter().map(|(id, _)| id).collect()
}

/// A view dropped and created again while a fold is in flight serves none of its predecessor's
/// rows once that fold settles, and exactly its own items after a restart and a later fold.
#[tokio::test]
async fn a_view_recreated_during_a_fold_takes_none_of_its_predecessors_rows() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    // Something for the fold to fold.
    ingest_right_of_the_shape(&server, "quarter:q1", "into-q1").await;
    flush(&server).await;

    server.state.engine.set_fold_paused_for_test(true);
    let before = server.state.engine.write_executor_stats();
    let resp = server
        .client
        .post(server.control_url("/control/compact"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 202);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while !server.state.engine.fold_is_holding_for_test() {
        assert!(std::time::Instant::now() < deadline, "the fold never reached its hold");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    recreate(&server, "q2").await;
    assert!(points_of(&server, "quarter:q2").await.is_empty());

    server.state.engine.set_fold_paused_for_test(false);
    wait_until(&server, "the fold settled", move |now| {
        now.folds > before.folds || now.fold_failures > before.fold_failures
    })
    .await;
    assert!(
        points_of(&server, "quarter:q2").await.is_empty(),
        "the fold that was in flight gives the recreated view none of its predecessor's rows"
    );

    let own = ingest_right_of_the_shape(&server, "quarter:q2", "recreated-q2").await;
    flush(&server).await;
    assert_eq!(points_of(&server, "quarter:q2").await, own);

    server.shutdown().await;
    let server = open(&tmp).await;
    assert_eq!(points_of(&server, "quarter:q2").await, own, "after a restart");

    assert!(fold_settled(&server).await, "a later fold lands");
    assert_eq!(points_of(&server, "quarter:q2").await, own, "after a later fold");
}

/// A row-major spatial level's folded column covers the view's base alone: a flushed segment with
/// as many rows as the base is resolved from its own geometry after a restart, not read off the
/// column.
#[tokio::test]
async fn a_flushed_segment_the_size_of_the_base_is_not_read_off_the_base_column() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    let mut declaration = spatial_declaration(SHAPES);
    declaration["layout"] = json!("row_major_label");
    register(&server, declaration).await;
    let (status, body) = put(
        &server,
        SHAPES,
        json!([{ "key": "left", "view": "q2", "members": [], "bbox": [0.0, 0.0, 500.0, 1000.0] }]),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    flush_and_fold(&server).await;

    ingest_right_of_the_shape(&server, "quarter:q2", "outside").await;
    flush(&server).await;
    let before = served(&server, "quarter:q2", SHAPES).await;
    assert_eq!(before.len(), 1, "the shape is drawn: {before:?}");

    server.shutdown().await;
    let server = open(&tmp).await;
    assert_eq!(served(&server, "quarter:q2", SHAPES).await, before);
}

/// A view dropped and created again with as many rows as its predecessor held serves exactly its
/// own items after a restart, to a principal who may see them and to one who may not.
#[tokio::test]
async fn a_recreated_view_the_size_of_its_predecessor_serves_its_own_items_after_a_restart() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    recreate(&server, "q2").await;
    let own = ingest_right_of_the_shape(&server, "quarter:q2", "recreated-q2").await;
    flush(&server).await;
    assert_eq!(points_of(&server, "quarter:q2").await, own);

    // The new items carry `0` alone, so a principal holding `1` alone sees none of them. The
    // dropped view's items divisible by three carry `1`.
    assert!(points_as(&server, &["1"], "quarter:q2").await.is_empty());

    server.shutdown().await;
    let server = open(&tmp).await;
    assert_eq!(points_of(&server, "quarter:q2").await, own);
    assert!(
        points_as(&server, &["1"], "quarter:q2").await.is_empty(),
        "a restart masks the recreated view's rows by its own items"
    );
}

/// A view dropped while its first flush is in flight takes none of that flush's rows, whether its
/// key is created again during the flight or only after it, live and after a restart.
#[tokio::test]
async fn a_view_dropped_during_its_first_flush_takes_none_of_its_rows() {
    for recreated_in_flight in [true, false] {
        let tmp = TempDir::new().unwrap();
        let server = serve(&tmp).await;
        create_key(&server, "q3").await;
        ingest_right_of_the_shape(&server, "quarter:q3", "into-q3").await;

        server.state.engine.set_flush_paused_for_test(true);
        ticked(&server).await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        while !server.state.engine.flush_is_holding_for_test() {
            assert!(std::time::Instant::now() < deadline, "the flush never reached its hold");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        if recreated_in_flight {
            recreate(&server, "q3").await;
        } else {
            drop_key(&server, "q3").await;
        }
        server.state.engine.set_flush_paused_for_test(false);
        wait_until(&server, "the held flush left the pool", |now| !now.flush_in_flight).await;
        // Two ticks: the first drains the handed-back flush, the second follows its publication.
        ticked(&server).await;
        ticked(&server).await;
        if !recreated_in_flight {
            create_key(&server, "q3").await;
        }
        assert!(
            points_of(&server, "quarter:q3").await.is_empty(),
            "the view created again holds none of its predecessor's in-flight rows \
             (recreated in flight: {recreated_in_flight})"
        );

        server.shutdown().await;
        let server = open(&tmp).await;
        assert!(
            points_of(&server, "quarter:q3").await.is_empty(),
            "nor after a restart (recreated in flight: {recreated_in_flight})"
        );
        server.shutdown().await;
    }
}
