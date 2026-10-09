//! **Unique fields on the wire**: `/v1/meta` says which columns are unique and which operators a
//! unique column with no other filter home takes; a filter compares an integer past 2^53 exactly
//! when it is sent as a decimal string; an ingest row carrying a held value names its holder, and
//! is `409` naming the holder's `tessera_id` where it would change it; and `PUT
//! /control/attributes` declares `unique` on a column that exists, `409` where the column holds a
//! value twice.
//!
//! The engine-level cases are `tessera-engine/tests/unique_fields.rs`; this file pins the routes.

mod common;

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{Float32Array, StringArray, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use common::*;
use serde_json::{json, Value};

const N: u64 = 40;
const BIG: u64 = (1 << 60) + 1;

const SCHEMA_TOML: &str = r#"
[[attribute]]
name   = "gid"
type   = "u64"
unique = true

[[attribute]]
name = "code"
type = "keyword"
"#;

fn gid_of(e: u64) -> u64 {
    BIG + 2 * e
}

/// `code` repeats every second item's value, so declaring it unique is refused.
fn code_of(e: u64) -> String {
    format!("c{}", e / 2)
}

fn fixture(dir: &Path) -> std::path::PathBuf {
    let points = dir.join("points.parquet");
    let pairs = dir.join("pairs.parquet");
    let ids: Vec<u64> = (0..N).collect();
    write_points(
        &points,
        &ids,
        scatter,
        vec![
            column("gid", false, UInt64Array::from_iter_values(ids.iter().map(|e| gid_of(*e)))),
            column("code", false, StringArray::from_iter_values(ids.iter().map(|e| code_of(*e)))),
        ],
    );
    write_pairs_n(&pairs, N);
    let out = dir.join("bundle");
    build_declared(&out, &points, &pairs, &format!("{SCHEMA_TOML}{ID_ATTRIBUTE}"));
    out
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

async fn filtered(served: &Served, filters: Value) -> (u16, BTreeSet<u64>) {
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
    let status = resp.status().as_u16();
    if status != 200 {
        return (status, BTreeSet::new());
    }
    let (_, points) = decode_viewport(&resp.bytes().await.unwrap());
    (status, points.into_iter().map(|(id, _)| id).collect())
}

/// Rows of `(gid, code)` at one position.
fn batch(rows: &[(u64, &str)]) -> Vec<u8> {
    let labels: Vec<&[&str]> = rows.iter().map(|_| &["0"][..]).collect();
    let access = access_lists(&labels);
    let fields = vec![
        Field::new("x", DataType::Float32, false),
        Field::new("y", DataType::Float32, false),
        access_field(&access),
        Field::new("gid", DataType::UInt64, true),
        Field::new("code", DataType::Utf8, true),
    ];
    let columns: Vec<arrow::array::ArrayRef> = vec![
        Arc::new(Float32Array::from_iter_values(rows.iter().map(|_| 500.0))),
        Arc::new(Float32Array::from_iter_values(rows.iter().map(|_| 500.0))),
        Arc::new(access),
        Arc::new(UInt64Array::from_iter_values(rows.iter().map(|r| r.0))),
        Arc::new(StringArray::from_iter_values(rows.iter().map(|r| r.1))),
    ];
    let schema = Arc::new(Schema::new(fields));
    let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();
    let mut writer = arrow::ipc::writer::StreamWriter::try_new(Vec::new(), &schema).unwrap();
    writer.write(&batch).unwrap();
    writer.into_inner().unwrap()
}

async fn ingest(served: &Served, batch_id: &str, body: Vec<u8>) -> (u16, Value) {
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
    (status, resp.json().await.unwrap_or(Value::Null))
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

// ---------------------------------------------------------------------------------------------

/// **`/v1/meta` says a column is unique, and a unique column with no other filter home takes
/// `eq` and `in` alone**; an operator outside them is `422`.
#[tokio::test]
async fn meta_publishes_unique_and_its_operators() {
    let served = Served::build(fixture).await;
    let meta = meta(&served).await;
    let gid = meta["declared_scalars"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["name"] == "gid")
        .unwrap();
    assert_eq!(gid["unique"], true);
    let operands = meta["filter_operands"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["column"] == "gid")
        .expect("a unique column is a filter operand");
    assert_eq!(operands["operands"], json!(["eq", "in"]));
    assert!(
        !meta["filter_operands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["column"] == "code"),
        "a column with neither flag is no operand"
    );
    let (status, _) = filtered(&served, json!({ "gid": { "range": { "gte": 0 } } })).await;
    assert_eq!(status, 422);
}

/// **An integer past 2^53 is exact as a decimal string**: the item holding `2^60 + 1` is found,
/// and its neighbour, one apart, is not, which a double could not tell apart.
#[tokio::test]
async fn a_decimal_string_finds_an_integer_past_two_to_the_fifty_three() {
    let served = Served::build(fixture).await;
    let (status, one) = filtered(&served, json!({ "gid": { "eq": gid_of(0).to_string() } })).await;
    assert_eq!(status, 200);
    assert_eq!(one.len(), 1);
    let (_, none) = filtered(&served, json!({ "gid": { "eq": (gid_of(0) + 1).to_string() } })).await;
    assert!(none.is_empty(), "no item holds the odd neighbour");
    let asked: Vec<String> = (0..5).map(|e| gid_of(e).to_string()).collect();
    let (_, five) = filtered(&served, json!({ "gid": { "in": asked } })).await;
    assert_eq!(five.len(), 5);
    let (status, _) = filtered(&served, json!({ "gid": { "eq": "12x" } })).await;
    assert_eq!(status, 422);
}

/// **A row carrying a held value names its holder**: it edits the holder, which keeps its
/// `tessera_id`.
#[tokio::test]
async fn a_row_carrying_a_held_value_names_the_holder() {
    let served = Served::build(fixture).await;
    let (_, holder) = filtered(&served, json!({ "gid": { "eq": gid_of(3).to_string() } })).await;
    let holder = *holder.iter().next().unwrap();
    let (status, body) = ingest(&served, "held", batch(&[(gid_of(3), "fresh")])).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["edited"], 1, "{body}");
    assert_eq!(body["tessera_ids"][0], holder.to_string(), "{body}");
}

/// **`PUT /control/attributes` changes `unique` on a column that exists**: refused with `409`
/// where the column holds a value twice, `200` where it does not, and `unique: false` answers
/// `200` and stops refusing.
#[tokio::test]
async fn unique_is_declared_and_removed_on_a_column_that_exists() {
    let served = Served::build(fixture).await;
    let (status, body) = declare(&served, json!({ "name": "code", "type": "keyword", "unique": true })).await;
    assert_eq!(status, 409, "{body}");
    let detail = body["detail"].as_str().unwrap_or_default();
    assert!(
        (0..N / 2).any(|k| detail.contains(&format!("'c{k}'"))),
        "the refusal names a value held twice: {body}"
    );

    let (status, body) = declare(&served, json!({ "name": "gid", "type": "u64", "unique": false })).await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = ingest(&served, "same", batch(&[(gid_of(3), "z")])).await;
    assert_eq!(status, 200, "no longer unique, so the value names no item: {body}");
    assert_eq!(body["created"], 1, "{body}");
    let (status, body) = declare(&served, json!({ "name": "gid", "type": "u64", "unique": true })).await;
    assert_eq!(status, 409, "two items hold the value now: {body}");

    let (status, body) = declare(&served, json!({ "name": "f", "type": "f64", "unique": true })).await;
    assert_eq!(status, 422, "unique on a float: {body}");
}

/// **`unique` declared after the rows it covers restarts to what the live service served.** The
/// log holds rows, then the declaration, then a row naming a holder by the value: a restart reads
/// the declarations and the rows in separate walks, and must still find each value's holder, keep
/// the one flushed and the one buffered row, and refuse nothing it accepted.
#[tokio::test]
async fn unique_declared_after_its_rows_restarts_to_what_was_served() {
    let served = Served::build(fixture).await;
    let (status, body) = declare(
        &served,
        json!({ "name": "gid", "type": "u64", "unique": false }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let (flushed, buffered) = (5_000u64, 5_002u64);
    let (status, body) = ingest(&served, "flushed", batch(&[(flushed, "f")])).await;
    assert_eq!(status, 200, "{body}");
    drain(&served.server).await;
    let (status, body) = ingest(&served, "buffered", batch(&[(buffered, "b")])).await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = declare(
        &served,
        json!({ "name": "gid", "type": "u64", "unique": true }),
    )
    .await;
    assert_eq!(status, 200, "every value is held once: {body}");
    let (status, body) = ingest(&served, "names", batch(&[(flushed, "f")])).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["created"], 0, "the value names its holder: {body}");

    drain(&served.server).await;
    let holders = async |served: &Served| {
        let mut out = Vec::new();
        for value in [flushed, buffered] {
            out.push(filtered(served, json!({ "gid": { "eq": value.to_string() } })).await);
        }
        out
    };
    let live = holders(&served).await;
    assert!(live.iter().all(|held| held.1.len() == 1), "{live:?}");

    let served = served.restart().await;
    assert_eq!(
        holders(&served).await,
        live,
        "the restart serves each value's one holder"
    );
    let (status, body) = ingest(&served, "again", batch(&[(buffered, "b")])).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["tessera_ids"][0],
        live[1].1.iter().next().unwrap().to_string(),
        "and `unique` is in force over the replayed rows: {body}"
    );
}
