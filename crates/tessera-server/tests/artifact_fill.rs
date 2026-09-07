//! **The fill rule on the wire** (`ingest.md` §1.5; contracts §3.4): `PATCH
//! /control/layers/{name}/artifacts` fills a fixed part an artifact lacks, `PUT` accepts a key the
//! level holds under the same rule, a differing part is a `409` naming the part and never the
//! held value, and a layer declaring supplied content publishes an artifact without it and
//! reports the count.
//!
//! What is asserted, over the real routes: a bare publication is counted as `without_content`
//! and served to nobody; a content filled by `PATCH` serves it and comes back from a restart; a
//! parent filled by `PATCH` moves the cut; a re-`PUT` of a held key is `200`, mints nothing and
//! answers the same identifier; the `409` body names the part.

mod common;

use common::*;
use serde_json::json;
use tempfile::TempDir;

const TOPICS: &str = "topics/filled";
const TREE: &str = "clusters/filled";

fn member(source_id: u64) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(external_id_of(source_id))
}

fn members(range: std::ops::Range<u64>) -> Vec<String> {
    range.map(member).collect()
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
    build_fixture(
        &tmp.path().join("bundle"),
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    open(tmp).await
}

async fn restart(server: TestServer, tmp: &TempDir) -> TestServer {
    drop(server);
    open(tmp).await
}

/// A layer declaring one supplied content that needs no generating set, or a nested tree.
fn declaration(name: &str, supplied: bool, kind: &str) -> serde_json::Value {
    let supplied = if supplied {
        json!([{ "name": "topic", "type": "text", "require_member_visibility": "inherited" }])
    } else {
        json!([])
    };
    json!({
        "name": name,
        "title": name,
        "views": ["s0"],
        "membership": "enumerated",
        "value_set": "closed",
        "visibility": null,
        "artifact_visibility": { "field": null, "default": "inherited" },
        "require_member_visibility": null,
        "hierarchy": { "kind": kind, "prune_children": true },
        "content": { "computed": [], "supplied": supplied },
        "depends_on": [],
        "levels": []
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
    assert_eq!(resp.status().as_u16(), 201, "the layer registers");
}

fn artifacts_url(server: &TestServer, layer: &str) -> String {
    server.control_url(&format!(
        "/control/layers/{}/artifacts",
        layer.replace('/', "%2F")
    ))
}

async fn put(
    server: &TestServer,
    layer: &str,
    artifacts: serde_json::Value,
) -> (u16, serde_json::Value) {
    let resp = server
        .client
        .put(artifacts_url(server, layer))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({ "addressing": "external", "artifacts": artifacts }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(serde_json::Value::Null))
}

async fn patch(
    server: &TestServer,
    layer: &str,
    artifacts: serde_json::Value,
) -> (u16, serde_json::Value) {
    let resp = server
        .client
        .patch(artifacts_url(server, layer))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({ "addressing": "external", "artifacts": artifacts }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(serde_json::Value::Null))
}

/// The layer's served rows for the broad principal, by key.
async fn served(server: &TestServer, layer: &str) -> Vec<(String, u64)> {
    let auth = authorise(server, &["0"]).await;
    let token = auth["token"].as_str().unwrap();
    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&json!({
            "view": "s0",
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

/// The drill-down's body for one artifact, or its status where it is withheld.
async fn drill(server: &TestServer, tessera_id: &str) -> (u16, serde_json::Value) {
    let auth = authorise(server, &["0"]).await;
    let token = auth["token"].as_str().unwrap();
    let resp = server
        .client
        .post(server.viewer_url(&format!("/v1/artifacts/{tessera_id}")))
        .bearer_auth(token)
        .json(&json!({ "view": "s0" }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(serde_json::Value::Null))
}

/// **A content filled by `PATCH` on an artifact published without one** (R5), served from the
/// fill and after a restart; a differing content is `409` naming the rank and not the text.
#[tokio::test]
async fn a_content_is_filled_on_an_artifact_published_without_one_and_a_differing_one_is_409() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    register(&server, declaration(TOPICS, true, "flat")).await;

    let (status, body) = put(
        &server,
        TOPICS,
        json!([{ "key": "t0", "members": members(0..40) }]),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["created"], 1, "{body}");
    assert_eq!(
        body["without_content"], 1,
        "the publication reports the artifact it accepted without content: {body}"
    );
    let id = body["artifacts"][0]["tessera_id"]
        .as_str()
        .expect("an identifier")
        .to_string();
    assert!(
        served(&server, TOPICS).await.is_empty(),
        "withheld until it carries the content its layer declares"
    );
    assert_eq!(drill(&server, &id).await.0, 404);

    let (status, body) = patch(
        &server,
        TOPICS,
        json!([{ "key": "t0", "content": [{ "rank": 0, "values": ["shipping"] }] }]),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["artifacts"][0]["filled"], 1, "{body}");
    assert_eq!(body["artifacts"][0]["joined"], 0, "{body}");
    assert_eq!(body["artifacts"][0]["tessera_id"], id, "{body}");
    assert_eq!(served(&server, TOPICS).await, vec![("t0".to_string(), 40)]);
    let (status, body) = drill(&server, &id).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["content"], json!(["shipping"]), "{body}");

    // Identical: nothing filled. Different: 409, naming the part and never the held text.
    let (status, body) = patch(
        &server,
        TOPICS,
        json!([{ "key": "t0", "content": [{ "rank": 0, "values": ["shipping"] }] }]),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["artifacts"][0]["filled"], 0, "{body}");
    let (status, body) = patch(
        &server,
        TOPICS,
        json!([{ "key": "t0", "members": members(40..50), "content": [{ "rank": 0, "values": ["logistics"] }] }]),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"], "conflict", "{body}");
    let detail = body["detail"].as_str().unwrap_or_default().to_string();
    assert!(detail.contains("content[0]"), "{detail}");
    assert!(
        !detail.contains("shipping"),
        "the held value is not echoed: {detail}"
    );
    assert_eq!(
        served(&server, TOPICS).await,
        vec![("t0".to_string(), 40)],
        "the members beside the refused part did not join"
    );

    let server = restart(server, &tmp).await;
    let (status, body) = drill(&server, &id).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["content"], json!(["shipping"]), "the fill replayed");
}

/// **A parent filled by `PATCH` moves the cut, and a `PUT` naming a held key is a `200` that
/// mints nothing.**
#[tokio::test]
async fn a_parent_is_filled_by_patch_and_a_held_key_on_put_mints_nothing() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    register(&server, declaration(TREE, false, "nested")).await;

    let (status, body) = put(
        &server,
        TREE,
        json!([
            { "key": "root", "members": members(0..60) },
            { "key": "child", "members": members(0..30) },
        ]),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["created"], 2, "{body}");
    let root = body["artifacts"][0]["tessera_id"]
        .as_str()
        .unwrap()
        .to_string();
    let child = body["artifacts"][1]["tessera_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        served(&server, TREE).await,
        vec![("child".to_string(), 30), ("root".to_string(), 60)],
        "two roots"
    );

    let (status, body) = patch(
        &server,
        TREE,
        json!([{ "key": "child", "parent": ["root"] }]),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["artifacts"][0]["filled"], 1, "{body}");
    assert_eq!(
        served(&server, TREE).await,
        vec![("child".to_string(), 30)],
        "the child now covers its root and, pruning children, replaces it"
    );

    // The same keys on PUT, identical: 200, nothing created, the same identifiers.
    let (status, body) = put(
        &server,
        TREE,
        json!([
            { "key": "root", "members": members(0..60) },
            { "key": "child", "members": members(0..30), "parent": ["root"] },
        ]),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["created"], 0, "{body}");
    assert_eq!(body["filled"], 0, "{body}");
    assert_eq!(body["joined"], 0, "{body}");
    assert_eq!(body["artifacts"][0]["tessera_id"], root, "{body}");
    assert_eq!(body["artifacts"][1]["tessera_id"], child, "{body}");

    // A held key beside a new one: one created, the held one's members join, the new one under
    // the held sibling.
    let (status, body) = put(
        &server,
        TREE,
        json!([
            { "key": "leaf", "members": members(0..10), "parent": ["child"] },
            { "key": "root", "members": members(0..70) },
        ]),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["created"], 1, "{body}");
    assert_eq!(body["joined"], 10, "{body}");
    assert_eq!(body["artifacts"][1]["tessera_id"], root, "{body}");
    assert_eq!(
        served(&server, TREE).await,
        vec![("leaf".to_string(), 10)],
        "the leaf covers the child which covers the root"
    );

    // A differing parent on PUT is the 409, by name.
    let (status, body) = put(
        &server,
        TREE,
        json!([{ "key": "child", "members": [], "parent": ["leaf"] }]),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    let detail = body["detail"].as_str().unwrap_or_default().to_string();
    assert!(detail.contains("parent"), "{detail}");
    assert!(!detail.contains("root"), "{detail}");

    let server = restart(server, &tmp).await;
    assert_eq!(
        served(&server, TREE).await,
        vec![("leaf".to_string(), 10)],
        "every fill replayed"
    );
}

/// **A key repeated within one batch with a fixed part on any of its rows is a `422` naming the
/// key, on both routes**, and nothing lands; repeated with members alone on `PATCH` it joins
/// twice.
#[tokio::test]
async fn a_key_repeated_in_one_batch_with_a_fixed_part_is_422_at_both_routes() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    register(&server, declaration(TREE, false, "nested")).await;
    let (status, body) = put(
        &server,
        TREE,
        json!([
            { "key": "a", "members": members(0..10) },
            { "key": "b", "members": members(10..20) },
            { "key": "k", "members": members(20..30) },
        ]),
    )
    .await;
    assert_eq!(status, 201, "{body}");

    let (status, body) = patch(
        &server,
        TREE,
        json!([
            { "key": "k", "parent": ["a"] },
            { "key": "k", "parent": ["b"] },
        ]),
    )
    .await;
    assert_eq!(status, 422, "{body}");
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("the key k appears more than once"),
        "{body}"
    );

    let (status, body) = put(
        &server,
        TREE,
        json!([
            { "key": "k", "members": [], "parent": ["a"] },
            { "key": "k", "members": members(30..35) },
        ]),
    )
    .await;
    assert_eq!(status, 422, "{body}");
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("the key k appears more than once"),
        "{body}"
    );
    assert_eq!(
        served(&server, TREE).await,
        vec![
            ("a".to_string(), 10),
            ("b".to_string(), 10),
            ("k".to_string(), 10)
        ],
        "nothing landed: no edge, no members"
    );

    let (status, body) = patch(
        &server,
        TREE,
        json!([
            { "key": "k", "members": members(30..35) },
            { "key": "k", "members": members(35..40) },
        ]),
    )
    .await;
    assert_eq!(status, 200, "members alone may repeat a key: {body}");
    assert_eq!(
        served(&server, TREE).await,
        vec![
            ("a".to_string(), 10),
            ("b".to_string(), 10),
            ("k".to_string(), 20)
        ]
    );
}
