//! **Which item an ingest row names, on the wire**: a row names an item by its `tessera_id`, its
//! external id or a unique value, and may leave any column out. A row naming an item and carrying
//! what it stores changes nothing and answers the item's `tessera_id`; a row naming no item and
//! carrying a position creates one; a batch that would change an item, names two items in one row,
//! names one item in two rows or names a `tessera_id` nobody holds is `409` and writes nothing; a
//! row creating an item with no position, or carrying one coordinate, is `422`. A resent batch
//! answers its first `tessera_id`s across a restart.
//!
//! The engine's rules are pinned by `tessera-engine/tests/identity_model.rs`; this file pins the
//! route's decode and answers.

mod common;

use std::path::Path;

use arrow::array::{StringArray, UInt64Array};
use base64::Engine as _;
use common::*;
use serde_json::{json, Value};

const N: u64 = 20;

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
    1_000 + e
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
            column("code", false, StringArray::from_iter_values(ids.iter().map(|e| format!("c{e}")))),
        ],
    );
    write_pairs_n(&pairs, N);
    let out = dir.join("bundle");
    build_declared(&out, &points, &pairs, SCHEMA_TOML);
    out
}

async fn ingest(served: &Served, batch_id: &str, rows: Value) -> (u16, Value) {
    let resp = served
        .server
        .client
        .post(served.server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", batch_id)
        .header("x-tessera-view", "s0")
        .json(&rows)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// The `tessera_id` of the built item holding `gid`, as the viewer plane serves it.
async fn holder(served: &Served, gid: u64) -> String {
    let resp = served
        .server
        .client
        .post(served.server.viewer_url("/v1/viewport"))
        .bearer_auth(&served.token)
        .json(&json!({
            "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 200,
            "filters": { "gid": { "eq": gid.to_string() } }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let (_, points) = decode_viewport(&resp.bytes().await.unwrap());
    assert_eq!(points.len(), 1, "one item holds gid {gid}");
    points[0].0.to_string()
}

fn external(e: u64) -> String {
    base64::engine::general_purpose::STANDARD.encode(e.to_le_bytes())
}

/// **A row naming an item and carrying what it stores changes nothing**, by each identifier, with
/// every column left out or sent as stored; it answers the item's `tessera_id` and writes
/// nothing. A row naming no item and carrying a position creates one.
#[tokio::test]
async fn rows_naming_items_change_nothing_and_rows_naming_none_create() {
    let served = Served::build(fixture).await;
    let three = holder(&served, gid_of(3)).await;
    let four = holder(&served, gid_of(4)).await;
    let five = holder(&served, gid_of(5)).await;
    let (status, body) = ingest(
        &served,
        "named",
        json!([
            { "tessera_id": three },
            { "gid": gid_of(4), "code": "c4" },
            { "external_id": external(5), "access": ["0"] },
        ]),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["unchanged"], 3, "{body}");
    assert_eq!(body["created"], 0, "{body}");
    assert_eq!(body["tessera_ids"], json!([three, four, five]));

    let (status, body) = ingest(
        &served,
        "new",
        json!([{ "x": 500.0, "y": 500.0, "access": ["0"], "gid": 7 }]),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["created"], 1, "{body}");
    let created = body["tessera_ids"][0].clone();

    // The same row again names the item it created, by its unique value.
    let (status, body) = ingest(
        &served,
        "new-again",
        json!([{ "x": 500.0, "y": 500.0, "access": ["0"], "gid": 7 }]),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["unchanged"], 1, "{body}");
    assert_eq!(body["tessera_ids"][0], created);
}

/// **A batch the rules refuse is `409` and writes nothing**: a row that would change an item, a
/// row naming two items, two rows naming one, two rows setting one value, and a `tessera_id`
/// nobody holds. Each refusal names the items by `tessera_id`, never an entity id.
#[tokio::test]
async fn refused_batches_are_conflicts_and_write_nothing() {
    let served = Served::build(fixture).await;
    let three = holder(&served, gid_of(3)).await;
    let four = holder(&served, gid_of(4)).await;

    let refused = [
        ("edit", json!([{ "tessera_id": three, "code": "changed" }])),
        ("two-items", json!([{ "tessera_id": three, "gid": gid_of(4) }])),
        ("one-item-twice", json!([{ "tessera_id": three }, { "gid": gid_of(3) }])),
        (
            "one-value-twice",
            json!([
                { "x": 1.0, "y": 1.0, "access": ["0"], "gid": 9 },
                { "x": 2.0, "y": 2.0, "access": ["0"], "gid": 9 },
            ]),
        ),
        ("unknown", json!([{ "tessera_id": "12345", "x": 1.0, "y": 1.0, "access": ["0"] }])),
    ];
    for (batch, rows) in refused {
        let (status, body) = ingest(&served, batch, rows).await;
        assert_eq!(status, 409, "{batch}: {body}");
    }
    let (_, body) = ingest(&served, "two-items-again", json!([{ "tessera_id": three, "gid": gid_of(4) }])).await;
    let detail = body["detail"].as_str().unwrap_or_default();
    assert!(detail.contains(&three) && detail.contains(&four), "{detail}");

    // Nothing was written: gid 9 names no item, and the edited item still carries its code.
    let (status, body) = ingest(
        &served,
        "after",
        json!([{ "tessera_id": three, "code": "c3" }, { "x": 3.0, "y": 3.0, "access": ["0"], "gid": 9 }]),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!((body["unchanged"].clone(), body["created"].clone()), (json!(1), json!(1)));
}

/// **A row creating an item needs a position, and a row carries both coordinates or neither**:
/// either is `422`.
#[tokio::test]
async fn a_new_item_needs_both_coordinates() {
    let served = Served::build(fixture).await;
    let (status, body) = ingest(&served, "no-position", json!([{ "access": ["0"], "gid": 11 }])).await;
    assert_eq!(status, 422, "{body}");
    let (status, body) =
        ingest(&served, "one-coordinate", json!([{ "x": 5.0, "access": ["0"], "gid": 11 }])).await;
    assert_eq!(status, 422, "{body}");
}

/// **A resent batch answers its first `tessera_id`s**, before and after a restart, and changes
/// nothing.
#[tokio::test]
async fn a_resent_batch_answers_its_first_tessera_ids_across_a_restart() {
    let served = Served::build(fixture).await;
    let three = holder(&served, gid_of(3)).await;
    let rows = json!([
        { "tessera_id": three },
        { "x": 5.0, "y": 5.0, "access": ["0"], "gid": 12 },
    ]);
    let (status, first) = ingest(&served, "kept", rows.clone()).await;
    assert_eq!(status, 200, "{first}");
    let (_, replayed) = ingest(&served, "kept", rows.clone()).await;
    assert_eq!(replayed["replayed"], true, "{replayed}");
    assert_eq!(replayed["tessera_ids"], first["tessera_ids"]);

    let served = served.restart().await;
    let (status, replayed) = ingest(&served, "kept", rows).await;
    assert_eq!(status, 200, "{replayed}");
    assert_eq!(replayed["replayed"], true, "{replayed}");
    assert_eq!(replayed["created"], 0, "{replayed}");
    assert_eq!(replayed["tessera_ids"], first["tessera_ids"]);
}
