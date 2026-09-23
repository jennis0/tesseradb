//! **`PUT /control/attributes`** (`ingest.md` §1.3, §6.3; decision 0136, track T4): a column is
//! declared at a running service, an identical redeclaration answers the column that exists, a
//! differing one is refused, a batch may carry the column or omit it from the answer onward, the
//! declaration survives a restart, and every viewer-plane reader answers the column over the rows
//! that carried it and absence over the rows that predate it.
//!
//! The engine-level cases are `tessera-engine/tests/runtime_attributes.rs`; what this file pins
//! is the wire: the status codes and bodies the route answers, the Arrow batch with and without
//! the column, and the viewer verbs a client reads the column through.

mod common;

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{Float32Array, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use common::*;
use serde_json::{json, Value};

const N: u64 = 60;

/// One rendered, indexed `f32` at the build, and a `public` vocabulary no build column names, so
/// a runtime category over it fixes the width at the declaration.
const SCHEMA_TOML: &str = r#"
[[vocabulary]]
name       = "dept"
width      = "u8"
value_set  = "closed"
visibility = "public"
  [vocabulary.values]
  eng = 5
  ops = 6

[[attribute]]
name   = "score"
type   = "f32"
render = true
index  = true
"#;

fn fixture(dir: &Path) -> std::path::PathBuf {
    build_scored(dir, N, SCHEMA_TOML)
}

async fn declare(served: &Served, body: Value) -> (u16, Value) {
    let resp = served
        .server
        .client
        .put(served.server.control_url("/control/attributes"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

async fn meta(served: &Served) -> Value {
    let resp = served
        .server
        .client
        .get(served.server.viewer_url("/v1/meta"))
        .bearer_auth(&served.token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    resp.json().await.unwrap()
}

fn declared_names(meta: &Value) -> Vec<String> {
    meta["declared_scalars"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["name"].as_str().unwrap().to_string())
        .collect()
}

/// One ingest row: the external id, its position, the build's `score`, and the runtime columns
/// where the batch carries them.
struct Row {
    id: &'static str,
    score: f32,
    sentiment: Option<f32>,
    tag: Option<&'static str>,
}

/// An ingest body carrying the build's columns, and the runtime ones where `runtime` says so.
fn batch(rows: &[Row], runtime: bool) -> Vec<u8> {
    let labels: Vec<&[&str]> = rows.iter().map(|_| &["0"][..]).collect();
    let access = access_lists(&labels);
    let mut fields = vec![
        Field::new("external_id", DataType::Binary, true),
        Field::new("x", DataType::Float32, false),
        Field::new("y", DataType::Float32, false),
        access_field(&access),
        Field::new("score", DataType::Float32, true),
    ];
    let mut columns: Vec<arrow::array::ArrayRef> = vec![
        Arc::new(arrow::array::BinaryArray::from_iter(
            rows.iter().map(|r| Some(r.id.as_bytes())),
        )),
        Arc::new(Float32Array::from_iter_values(rows.iter().map(|_| 500.0))),
        Arc::new(Float32Array::from_iter_values(rows.iter().map(|_| 500.0))),
        Arc::new(access),
        Arc::new(Float32Array::from_iter_values(rows.iter().map(|r| r.score))),
    ];
    if runtime {
        fields.push(Field::new("sentiment", DataType::Float32, true));
        fields.push(Field::new("tag", DataType::Utf8, true));
        columns.push(Arc::new(Float32Array::from_iter(
            rows.iter().map(|r| r.sentiment),
        )));
        columns.push(Arc::new(StringArray::from_iter(rows.iter().map(|r| r.tag))));
    }
    let schema = Arc::new(Schema::new(fields));
    let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();
    let mut writer = arrow::ipc::writer::StreamWriter::try_new(Vec::new(), &schema).unwrap();
    writer.write(&batch).unwrap();
    writer.into_inner().unwrap()
}

/// Ingest and answer the `tessera_id`s the batch was given, in row order.
async fn ingest(served: &Served, batch_id: &str, body: Vec<u8>) -> Vec<u64> {
    ingest_with_receipt(served, batch_id, body).await.0
}

/// [`ingest`], with the receipt's `padded_columns` beside the ids.
async fn ingest_with_receipt(served: &Served, batch_id: &str, body: Vec<u8>) -> (Vec<u64>, u64) {
    let resp = served
        .server
        .client
        .post(served.server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", batch_id)
        .header("x-tessera-view", "s0")
        .header("content-type", "application/vnd.apache.arrow.stream")
        .body(body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(status, 200, "batch {batch_id} is accepted: {body}");
    let ids = body["tessera_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| {
            v.as_u64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                .unwrap()
        })
        .collect();
    (ids, body["padded_columns"].as_u64().unwrap())
}

/// The `tessera_id`s a filtered viewport answers, from a fresh session so the rows flushed since
/// the last one are in the answer (`views_write.rs`'s note on `points`).
async fn filtered(served: &Served, filters: Value) -> BTreeSet<u64> {
    let token = token_for(&served.server, &["0", "1"][..]).await;
    let resp = served
        .server
        .client
        .post(served.server.viewer_url("/v1/viewport"))
        .bearer_auth(&token)
        .json(&json!({
            "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 200,
            "filters": filters
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "{}",
        resp.text().await.unwrap_or_default()
    );
    let (_, points) = decode_viewport(&resp.bytes().await.unwrap());
    points.into_iter().map(|(id, _)| id).collect()
}

async fn item(served: &Served, tessera_id: u64) -> Value {
    let resp = served
        .server
        .client
        .post(served.server.viewer_url(&format!("/v1/items/{tessera_id}")))
        .bearer_auth(&served.token)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    resp.json().await.unwrap()
}

fn sentiment() -> Value {
    json!({ "name": "sentiment", "type": "f32", "index": true })
}

fn tag() -> Value {
    json!({ "name": "tag", "type": "category", "vocabulary": "dept", "index": true })
}

// ---------------------------------------------------------------------------------------------

/// **The route's answers** (contracts §3.4): `201` for a new column, `200` for an identical
/// redeclaration, `409` for a held name under another identity, `422` for a declaration the
/// schema's rules refuse, and `/v1/meta` listing the column from the answer.
#[tokio::test]
async fn the_route_declares_answers_redeclarations_and_refuses_what_the_schema_refuses() {
    let served = Served::build(fixture).await;
    assert_eq!(declared_names(&meta(&served).await), ["score"]);

    let (status, body) = declare(&served, sentiment()).await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(
        without_publication(body),
        json!({ "name": "sentiment", "existing": false })
    );

    let (status, body) = declare(&served, sentiment()).await;
    assert_eq!(status, 200, "identical: the column that exists: {body}");
    assert_eq!(
        without_publication(body),
        json!({ "name": "sentiment", "existing": true })
    );

    let (status, body) = declare(
        &served,
        json!({ "name": "sentiment", "type": "f64", "index": true }),
    )
    .await;
    assert_eq!(status, 409, "a held name under another identity: {body}");
    assert_eq!(body["error"], "conflict");

    let (status, body) = declare(&served, json!({ "name": "score", "type": "i32" })).await;
    assert_eq!(status, 409, "the build's column is a held name too: {body}");

    // What is refused is tested in `tessera_store::declaration` and the engine; here, that a
    // refusal is a 422 with a `detail`.
    for bad in [
        json!({ "name": "region", "type": "u8" }),
        json!({ "name": "tag", "type": "category", "vocabulary": "nothing" }),
        json!({ "name": "drawn", "type": "f32", "index": true, "render": true }),
    ] {
        let (status, body) = declare(&served, bad.clone()).await;
        assert_eq!(status, 422, "{bad}: {body}");
        assert!(body["detail"].is_string(), "{bad}: {body}");
    }
    let (status, _) = declare(&served, tag()).await;
    assert_eq!(status, 201);
    assert_eq!(
        declared_names(&meta(&served).await),
        ["score", "sentiment", "tag"],
        "the runtime columns append after the build's, in declaration order"
    );
    let meta = meta(&served).await;
    let tag = meta["declared_scalars"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["name"] == "tag")
        .unwrap();
    assert_eq!(tag["arrow_type"], "u8");
    assert_eq!(tag["category"]["vocabulary"], "dept");
}

/// **A batch may carry the column or omit it from the answer onward**, and every viewer-plane
/// reader answers the column over the rows that carried it and absence over the rest: the
/// filter, the drill-down, and the categories vocabulary of a runtime category.
#[tokio::test]
async fn a_batch_carries_the_column_or_omits_it_and_every_reader_answers_it() {
    let served = Served::build(fixture).await;
    assert_eq!(declare(&served, sentiment()).await.0, 201);
    assert_eq!(declare(&served, tag()).await.0, 201);

    let carrying = ingest(
        &served,
        "carrying",
        batch(
            &[
                Row {
                    id: "c1",
                    score: 1.0,
                    sentiment: Some(0.9),
                    tag: Some("eng"),
                },
                Row {
                    id: "c2",
                    score: 2.0,
                    sentiment: Some(0.2),
                    tag: Some("ops"),
                },
                Row {
                    id: "c3",
                    score: 3.0,
                    sentiment: None,
                    tag: None,
                },
            ],
            true,
        ),
    )
    .await;
    let (omitting, padded) = ingest_with_receipt(
        &served,
        "omitting",
        batch(
            &[Row {
                id: "o1",
                score: 40.0,
                sentiment: None,
                tag: None,
            }],
            false,
        ),
    )
    .await;
    assert_eq!(
        padded, 2,
        "the receipt counts the declared columns the batch omitted"
    );
    // The JSON door pads the same way: a declared column no object names is omitted from the
    // batch, and one an object names with `null` is a cell absent in that row and nothing padded.
    let resp = served
        .server
        .client
        .post(served.server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "json-omitting")
        .header("x-tessera-view", "s0")
        .header("content-type", "application/json")
        .body(
            r#"[{"external_id":"ajE=","x":500.0,"y":500.0,"access":["0"],"score":50.0,"sentiment":null}]"#,
        )
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let receipt: Value = resp.json().await.unwrap();
    assert_eq!(status, 200, "{receipt}");
    assert_eq!(receipt["padded_columns"], json!(1), "{receipt}");
    drain(&served.server).await;

    assert_eq!(
        filtered(&served, json!({ "sentiment": { "range": { "gte": 0.5 } } })).await,
        BTreeSet::from([carrying[0]]),
        "a range over the column matches the row that carried a value in it"
    );
    assert_eq!(
        filtered(&served, json!({ "sentiment": { "range": { "gte": 0.0 } } })).await,
        BTreeSet::from([carrying[0], carrying[1]]),
        "a row with a null cell and a row from a batch omitting the column carry no value"
    );
    assert_eq!(
        filtered(&served, json!({ "score": { "range": { "gte": 40.0 } } }))
            .await
            .len(),
        2,
        "the JSON row landed beside the Arrow one; no build row scores past 6"
    );
    assert_eq!(
        filtered(&served, json!({ "tag": { "eq": "ops" } })).await,
        BTreeSet::from([carrying[1]])
    );

    let record = item(&served, carrying[0]).await;
    // An `f32` on the wire, so compared at the width it was stored at.
    assert!(
        (record["fields"]["sentiment"].as_f64().unwrap() - 0.9).abs() < 1e-6,
        "{record}"
    );
    assert_eq!(record["fields"]["tag"], json!("eng"));
    let record = item(&served, omitting[0]).await;
    assert_eq!(record["fields"]["score"], json!(40.0));
    assert!(
        record["fields"].get("sentiment").is_none() && record["fields"].get("tag").is_none(),
        "a row from a batch omitting the column carries none: {record}"
    );
    // An entity the build wrote: absent, answered from the segment schema.
    let built = served
        .server
        .state
        .engine
        .tessera_id_of(tessera_types::EntityId::new(0))
        .unwrap()
        .raw();
    let record = item(&served, built).await;
    assert!(record["fields"].get("score").is_some());
    assert!(
        record["fields"].get("sentiment").is_none(),
        "an entity that predates the declaration carries none of it: {record}"
    );

    // The vocabulary of a runtime category, resolved by code and paged.
    let resp = served
        .server
        .client
        .get(served.server.viewer_url("/v1/categories/tag?codes=5,6"))
        .bearer_auth(&served.token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let page: Value = resp.json().await.unwrap();
    let keys: Vec<&str> = page["values"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, ["eng", "ops"]);
}

/// **The declaration survives a restart**, from the log alone before any publication and from
/// the segments manifest after one, and the column keeps answering over both.
#[tokio::test]
async fn a_declaration_survives_a_restart_before_and_after_a_publication() {
    let served = Served::build(fixture).await;
    assert_eq!(declare(&served, sentiment()).await.0, 201);
    assert_eq!(declare(&served, tag()).await.0, 201);
    let served = served.restart().await;
    assert_eq!(
        declared_names(&meta(&served).await),
        ["score", "sentiment", "tag"],
        "replayed from the log"
    );
    assert_eq!(
        declare(&served, sentiment()).await.0,
        200,
        "the replayed column is the one a redeclaration meets"
    );
    let carrying = ingest(
        &served,
        "carrying",
        batch(
            &[Row {
                id: "c1",
                score: 1.0,
                sentiment: Some(0.9),
                tag: None,
            }],
            true,
        ),
    )
    .await;
    drain(&served.server).await;
    let served = served.restart().await;
    assert_eq!(
        declared_names(&meta(&served).await),
        ["score", "sentiment", "tag"],
        "carried by the segments manifest"
    );
    assert_eq!(
        filtered(&served, json!({ "sentiment": { "range": { "gte": 0.5 } } })).await,
        BTreeSet::from([carrying[0]])
    );
}

/// A declaration's answer with `publication` taken out, so a whole-object comparison stays a
/// whole-object comparison. The number is a running count and a test cannot name it, but its
/// absence would be a route that stopped telling a caller when its declaration becomes visible
/// (contracts §3.4), so this asserts it was there.
fn without_publication(mut body: Value) -> Value {
    assert!(
        body.as_object_mut()
            .expect("the answer is an object")
            .remove("publication")
            .is_some(),
        "every write acknowledgement carries a publication number: {body}"
    );
    body
}

/// One JSON batch to a write route under view `s0`, answering its body.
async fn write_json(served: &Served, route: &str, batch_id: &str, body: Value) -> Value {
    let resp = served
        .server
        .client
        .post(served.server.control_url(route))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", batch_id)
        .header("x-tessera-view", "s0")
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let answer: Value = resp.json().await.unwrap();
    assert_eq!(status, 200, "batch {batch_id} is accepted: {answer}");
    answer
}

/// A text column declared at a running service matches the same items after a restart as it did
/// before, whether its prose arrived by ingest or by a values fill.
#[tokio::test]
async fn a_text_column_declared_live_matches_the_same_items_after_a_restart() {
    let served = Served::build(fixture).await;
    let note = json!({ "name": "note", "type": "text", "index": true });
    assert_eq!(declare(&served, note).await.0, 201);
    let point = |id: &str, note: Option<&str>| {
        json!({
            "external_id": b64(id.as_bytes()), "x": 500.0, "y": 500.0, "access": ["0"], "score": 1.0,
            "note": note,
        })
    };
    let ingested = write_json(
        &served,
        "/control/ingest",
        "points",
        json!([
            point("n1", Some("cedar grove")),
            point("n2", Some("amber light")),
            point("n3", None),
        ]),
    )
    .await;
    let ids: Vec<u64> = ingested["tessera_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()).unwrap())
        .collect();
    drain(&served.server).await;
    write_json(
        &served,
        "/control/values",
        "values",
        json!([{ "external_id": b64(b"n3"), "note": "cedar bark" }]),
    )
    .await;
    drain(&served.server).await;

    let cedar = json!({ "note": { "match": "cedar" } });
    let before = filtered(&served, cedar.clone()).await;
    assert_eq!(before, BTreeSet::from([ids[0], ids[2]]));
    let served = served.restart().await;
    assert_eq!(filtered(&served, cedar).await, before);
}
