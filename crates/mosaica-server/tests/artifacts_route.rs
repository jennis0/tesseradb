//! `POST /v1/artifacts` over HTTP: a read carried across responses by its cursor returns every
//! artifact a viewer is served once, in publication order, the set the viewport serves; the head
//! counts what the read returns; compression changes the bytes and not the rows; a cursor is bound
//! to its credential and its route; a withheld parent answers as a parent with no children; `ids`
//! reads what the identifiers name, a withheld one answering as one naming nothing; and the route
//! takes its slot from the bulk-read lane. The artifacts viewport's own lane is checked here too.

mod common;

use std::collections::HashSet;
use std::time::{Duration, Instant};

use arrow::array::{Array, RecordBatch, StringArray};
use common::*;
use serde_json::{json, Value};
use tempfile::TempDir;
use mosaica_server::state::ComputeGate;

const N: u64 = 1_200;
const LAYER: &str = "clusters/tree";
const ROOTS: u64 = 10;

/// A root keeps 120 members and each of its three children 20; a viewer is served an artifact
/// while it sees at least ten of its members, so the narrow viewer, who sees a third, is served
/// the roots and none of the children.
fn declaration() -> Value {
    json!({
        "name": LAYER,
        "title": LAYER,
        "views": ["s0"],
        "membership": "enumerated",
        "value_set": "closed",
        "visibility": null,
        "artifact_visibility": { "field": null, "default": "inherited" },
        "require_member_visibility": { "count": 10 },
        "hierarchy": { "kind": "nested", "prune_children": false },
        "content": { "computed": [], "supplied": [] },
        "depends_on": [],
        "levels": []
    })
}

/// The tree in publication order: each root, then its three children.
fn planted() -> Vec<Value> {
    let mut out = Vec::new();
    for r in 0..ROOTS {
        out.push(json!({ "key": format!("r{r}"), "members": members(r * 120..r * 120 + 120) }));
        for c in 0..3 {
            let lo = r * 120 + c * 20;
            out.push(json!({
                "key": format!("r{r}-c{c}"),
                "members": members(lo..lo + 20),
                "parent": [format!("r{r}")],
            }));
        }
    }
    out
}

fn served_keys(broad: bool) -> Vec<String> {
    planted()
        .iter()
        .map(|a| a["key"].as_str().unwrap().to_string())
        .filter(|key| broad || !key.contains("-c"))
        .collect()
}

struct Fixture {
    _tmp: TempDir,
    server: TestServer,
}

async fn fixture_with(bulk_gate: ComputeGate) -> Fixture {
    let tmp = TempDir::new().unwrap();
    let bundle = build_fixture(tmp.path(), N);
    let server = spawn_server_with_bulk_reads(
        &bundle,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        generous_test_gate(),
        bulk_gate,
        |_| {},
    )
    .await;
    register(&server, declaration()).await;
    let resp = server
        .client
        .put(server.control_url(&format!(
            "/control/layers/{}/artifacts",
            LAYER.replace('/', "%2F")
        )))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({ "artifacts": planted() }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201, "{}", resp.text().await.unwrap());
    tick(&server).await;
    Fixture { _tmp: tmp, server }
}

async fn fixture() -> Fixture {
    fixture_with(generous_bulk_gate()).await
}

async fn post(server: &TestServer, route: &str, token: &str, body: &Value) -> reqwest::Response {
    server
        .client
        .post(server.viewer_url(route))
        .bearer_auth(token)
        .json(body)
        .send()
        .await
        .unwrap()
}

async fn artifacts_ok(server: &TestServer, token: &str, body: &Value) -> DecodedRecords {
    let resp = post(server, "/v1/artifacts", token, body).await;
    let status = resp.status().as_u16();
    let bytes = resp.bytes().await.unwrap();
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
    decode_records(&bytes)
}

/// Every response of a read, from `body` until `next` is null.
async fn read_all(server: &TestServer, token: &str, body: &Value) -> Vec<DecodedRecords> {
    let mut responses = Vec::new();
    let mut body = body.clone();
    loop {
        let decoded = artifacts_ok(server, token, &body).await;
        let next = decoded.trailer["next"].clone();
        responses.push(decoded);
        assert!(responses.len() < 10_000, "a read that never ends");
        match next {
            Value::Null => return responses,
            Value::String(cursor) => {
                body["cursor"] = json!(cursor);
                body.as_object_mut().unwrap().remove("count");
            }
            other => panic!("next is {other}"),
        }
    }
}

fn keys_of(responses: &[DecodedRecords]) -> Vec<String> {
    responses
        .iter()
        .flat_map(|r| r.pages.iter())
        .flat_map(|(batch, _)| {
            let keys = batch
                .column_by_name("key")
                .unwrap()
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap();
            (0..keys.len()).map(|i| keys.value(i).to_string()).collect::<Vec<_>>()
        })
        .collect()
}

fn ids_of(responses: &[DecodedRecords]) -> Vec<u64> {
    responses.iter().flat_map(DecodedRecords::tessera_ids).collect()
}

/// The identifiers the tile route serves in the layer over the whole map.
async fn viewport_ids(server: &TestServer, token: &str) -> HashSet<u64> {
    let body = json!({
        "view": "s0", "zoom": 0, "per_tile": 1000, "bbox": [0.0, 0.0, 1000.0, 1000.0],
        "layers": [LAYER], "budget": 10_000,
    });
    let resp = post(server, "/v1/artifacts/viewport", token, &body).await;
    assert_eq!(resp.status().as_u16(), 200);
    decode_artifact_frames(&resp.bytes().await.unwrap())
        .artifacts
        .unwrap_or_default()
        .into_iter()
        .map(|a| a.tessera_id)
        .collect()
}

/// **Every served artifact once, in publication order, across many responses**, the set the
/// viewport serves, with the head's counts equal to the rows the read returns.
#[tokio::test]
async fn a_full_read_returns_every_served_artifact_once() {
    let f = fixture().await;
    for (terms, broad) in [(&["0"][..], true), (&["1"][..], false)] {
        let token = token_for(&f.server, terms).await;
        let body = json!({
            "view": "s0", "layer": LAYER, "fields": ["key", "masked_count"],
            "page_rows": 3, "pages": 2, "count": true,
        });
        let responses = read_all(&f.server, &token, &body).await;
        assert!(responses.len() > 1, "the read spans several responses");
        assert_eq!(keys_of(&responses), served_keys(broad));
        let ids = ids_of(&responses);
        let set: HashSet<u64> = ids.iter().copied().collect();
        assert_eq!(set.len(), ids.len(), "an artifact returned twice");
        assert_eq!(set, viewport_ids(&f.server, &token).await);
        let head = &responses[0].head;
        assert_eq!(head["served"], ids.len() as u64);
        assert_eq!(head["matched"], ids.len() as u64);
        assert_eq!(head["page_rows"], 3);
    }
}

/// **zstd changes the bytes and not the rows.**
#[tokio::test]
async fn zstd_pages_decode_to_the_uncompressed_pages() {
    let f = fixture().await;
    let token = token_for(&f.server, &["0"]).await;
    let body = json!({
        "view": "s0", "layer": LAYER,
        "fields": ["key", "level", "parents", "masked_count", "centroid", "box"],
        "page_rows": 7,
    });
    let plain = read_all(&f.server, &token, &body).await;
    let mut zstd_body = body.clone();
    zstd_body["compression"] = json!("zstd");
    let zstd = read_all(&f.server, &token, &zstd_body).await;
    let batches = |responses: &[DecodedRecords]| -> Vec<RecordBatch> {
        responses
            .iter()
            .flat_map(|r| r.pages.iter().map(|(batch, _)| batch.clone()))
            .collect()
    };
    assert_eq!(batches(&plain), batches(&zstd));
}

/// **A cursor opens only for its credential and its route**: another credential, and an items
/// cursor on this route or this route's cursor on the items route, are one `422`.
#[tokio::test]
async fn a_cursor_opens_only_for_its_credential_and_route() {
    let f = fixture().await;
    let broad = token_for(&f.server, &["0"]).await;
    let first = artifacts_ok(
        &f.server,
        &broad,
        &json!({ "view": "s0", "layer": LAYER, "fields": [], "page_rows": 2, "pages": 1 }),
    )
    .await;
    let cursor = first.trailer["next"].as_str().unwrap().to_string();
    let body = json!({ "view": "s0", "layer": LAYER, "fields": [], "cursor": cursor });
    let again = token_for(&f.server, &["0"]).await;
    assert!(!artifacts_ok(&f.server, &again, &body).await.pages.is_empty());

    let narrow = token_for(&f.server, &["1"]).await;
    let resp = post(&f.server, "/v1/artifacts", &narrow, &body).await;
    assert_eq!(resp.status().as_u16(), 422);

    let resp = post(
        &f.server,
        "/v1/items",
        &broad,
        &json!({ "view": "s0", "fields": [], "cursor": cursor }),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 422);
    let items = post(
        &f.server,
        "/v1/items",
        &broad,
        &json!({ "view": "s0", "fields": [], "page_rows": 2, "pages": 1 }),
    )
    .await;
    let items = decode_records(&items.bytes().await.unwrap());
    let items_cursor = items.trailer["next"].as_str().unwrap();
    let resp = post(
        &f.server,
        "/v1/artifacts",
        &broad,
        &json!({ "view": "s0", "layer": LAYER, "fields": [], "cursor": items_cursor }),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 422);
}

/// **A parent the viewer is not served answers exactly as a parent with no children.** The
/// narrow viewer is served no child: a child as `parent` and a root as `parent` both answer an
/// empty read with the same head and trailer.
#[tokio::test]
async fn a_withheld_parent_answers_as_a_parent_with_no_children() {
    let f = fixture().await;
    let broad = token_for(&f.server, &["0"]).await;
    let narrow = token_for(&f.server, &["1"]).await;
    let all = read_all(
        &f.server,
        &broad,
        &json!({ "view": "s0", "layer": LAYER, "fields": ["key"] }),
    )
    .await;
    let by_key: std::collections::HashMap<String, u64> =
        keys_of(&all).into_iter().zip(ids_of(&all)).collect();
    let answer = |parent: u64| {
        let server = &f.server;
        let narrow = narrow.clone();
        async move {
            let decoded = artifacts_ok(
                server,
                &narrow,
                &json!({ "view": "s0", "layer": LAYER, "fields": ["key"],
                         "parent": parent.to_string(), "count": true }),
            )
            .await;
            let mut trailer = decoded.trailer.clone();
            trailer.as_object_mut().unwrap().remove("stream_us");
            let pages: Vec<(arrow::datatypes::SchemaRef, usize, Value)> = decoded
                .pages
                .iter()
                .map(|(batch, end)| (batch.schema(), batch.num_rows(), end.clone()))
                .collect();
            (decoded.head, pages, trailer)
        }
    };
    let withheld = answer(by_key["r2-c1"]).await;
    let empty = answer(by_key["r2"]).await;
    assert_eq!(withheld, empty);
    // One page of no rows, with the columns asked for, ends the read.
    let (_, pages, trailer) = &withheld;
    assert_eq!(pages.len(), 1);
    let (schema, rows, end) = &pages[0];
    let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
    assert_eq!((names, *rows), (vec!["tessera_id", "key"], 0));
    assert_eq!(schema.field(1).data_type(), &arrow::datatypes::DataType::Utf8);
    assert_eq!(end, &json!({ "next": null, "ended_by": "end" }));
    assert_eq!((&trailer["pages"], &trailer["rows"]), (&json!(1), &json!(0)));
    assert_eq!((&trailer["next"], &trailer["ended_by"]), (&Value::Null, &json!("end")));
    let children = read_all(
        &f.server,
        &broad,
        &json!({ "view": "s0", "layer": LAYER, "fields": ["key"], "parent": by_key["r2"] }),
    )
    .await;
    assert_eq!(keys_of(&children), vec!["r2-c0", "r2-c1", "r2-c2"]);
}

/// **The route takes its slot from the bulk-read lane**: with the lane full it is shed at once.
#[tokio::test]
async fn a_full_bulk_lane_sheds_an_artifacts_read() {
    let f = fixture_with(ComputeGate::for_bulk_reads(1)).await;
    let token = token_for(&f.server, &["0"]).await;
    let deadline = Instant::now() + Duration::from_secs(30);
    let held = loop {
        match f.server.state.bulk_gate.admit().await {
            Ok((permits, _)) => break permits,
            Err(_) => {
                assert!(Instant::now() < deadline, "the lane never came free");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    };
    let body = json!({ "view": "s0", "layer": LAYER, "fields": [] });
    let resp = post(&f.server, "/v1/artifacts", &token, &body).await;
    assert_eq!(resp.status().as_u16(), 429);
    assert!(resp.headers().contains_key("retry-after"));
    drop(held);
    artifacts_ok(&f.server, &token, &body).await;
}

/// Every artifact of the layer by key, as the broad viewer is served it.
async fn ids_by_key(f: &Fixture) -> std::collections::HashMap<String, u64> {
    let broad = token_for(&f.server, &["0"]).await;
    let all = read_all(
        &f.server,
        &broad,
        &json!({ "view": "s0", "layer": LAYER, "fields": ["key"] }),
    )
    .await;
    keys_of(&all).into_iter().zip(ids_of(&all)).collect()
}

/// **`ids` reads the artifacts it names, and a withheld one is one naming nothing**: the narrow
/// viewer, not served the children, asking for a root and a child gets the same response as asking
/// for the root and an identifier nobody issued — rows, head and cursor alike.
#[tokio::test]
async fn ids_read_what_they_name_and_a_withheld_one_answers_as_one_naming_nothing() {
    let f = fixture().await;
    let by_key = ids_by_key(&f).await;
    let broad = token_for(&f.server, &["0"]).await;
    let narrow = token_for(&f.server, &["1"]).await;
    let read = |token: &str, ids: Vec<Value>| {
        let body = json!({
            "view": "s0", "layer": LAYER, "fields": ["key", "masked_count"], "ids": ids,
            "count": true,
        });
        let server = &f.server;
        let token = token.to_string();
        async move { read_all(server, &token, &body).await }
    };

    // Both spellings of an identifier, out of order and repeated, read once each in publication
    // order.
    let named = vec![
        json!(by_key["r3-c1"]),
        json!(by_key["r1"].to_string()),
        json!(by_key["r3-c1"]),
    ];
    let responses = read(&broad, named).await;
    assert_eq!(keys_of(&responses), ["r1", "r3-c1"]);
    assert_eq!(responses[0].head["served"], 2);

    let withheld = read(&narrow, vec![json!(by_key["r2"]), json!(by_key["r2-c0"])]).await;
    let unknown = read(&narrow, vec![json!(by_key["r2"]), json!(9_999_999_999u64)]).await;
    assert_eq!(keys_of(&withheld), ["r2"]);
    let pages = |responses: &[DecodedRecords]| -> Vec<RecordBatch> {
        responses
            .iter()
            .flat_map(|r| r.pages.iter().map(|(batch, _)| batch.clone()))
            .collect()
    };
    assert_eq!(pages(&withheld), pages(&unknown), "the rows tell the two apart");
    assert_eq!(withheld[0].head, unknown[0].head, "the head tells the two apart");

    // An identifier that is not one is refused, as `parent`'s is.
    let resp = post(
        &f.server,
        "/v1/artifacts",
        &narrow,
        &json!({ "view": "s0", "layer": LAYER, "fields": [], "ids": ["r2"] }),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 422);
}

/// **The artifacts viewport sheds under its own limit**, `serve.artifact_admission`, and
/// `/control/status` reports the shed under its own block; a points viewport is not held behind it.
#[tokio::test]
async fn a_full_artifact_lane_sheds_an_artifacts_viewport_alone() {
    let f = fixture().await;
    let token = token_for(&f.server, &["0"]).await;
    let gate = &f.server.state.artifact_gate;
    let mut held = Vec::new();
    for _ in 0..gate.compute_admission {
        held.push(gate.admit().await.expect("the lane is free").0);
    }
    let before = control_status(&f.server).await["artifacts"]["shed_total"].as_u64().unwrap();
    let body = json!({
        "view": "s0", "zoom": 0, "per_tile": 10, "bbox": [0.0, 0.0, 1000.0, 1000.0],
        "layers": [LAYER],
    });
    let resp = post(&f.server, "/v1/artifacts/viewport", &token, &body).await;
    assert_eq!(resp.status().as_u16(), 429);
    assert!(resp.headers().contains_key("retry-after"));
    let status = control_status(&f.server).await;
    assert_eq!(status["artifacts"]["shed_total"].as_u64().unwrap(), before + 1);
    assert_eq!(status["compute"]["shed_total"].as_u64().unwrap(), 0);

    let points = json!({
        "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 10, "layers": [LAYER],
    });
    assert_eq!(post(&f.server, "/v1/viewport", &token, &points).await.status().as_u16(), 200);

    drop(held);
    let resp = post(&f.server, "/v1/artifacts/viewport", &token, &body).await;
    assert_eq!(resp.status().as_u16(), 200);
    let decoded = decode_artifact_frames(&resp.bytes().await.unwrap());
    // The treed layer's frame, then the one tile at zoom 0, which a treed layer leaves empty.
    assert_eq!(decoded.frames.len(), 2);
    assert!(decoded.frames[0].0.is_none() && !decoded.frames[0].1.is_empty());
    assert!(decoded.frames[1].1.is_empty());
}
