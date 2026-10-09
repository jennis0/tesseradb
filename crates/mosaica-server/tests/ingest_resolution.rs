//! **Which item an ingest row names, on the wire**: a row names an item by its `tessera_id` or a
//! unique value, and may leave any column out. A row naming an item and carrying
//! what it stores changes nothing and answers the item's `tessera_id`; a row naming no item and
//! carrying a position creates one. A row naming two items, naming an item or setting a unique
//! value an earlier row does, or naming a `tessera_id` nobody holds is refused: listed with its
//! reason while the batch applies its other rows, or, under `strict=true`, refusing the batch with
//! `409`. A row naming no item and carrying no position is refused as naming no item, and a batch
//! in which no row carries a position needs a column to name items by, or is `422`. A row carrying
//! one coordinate is `422`. A resent batch answers its first `tessera_id`s and refused rows across
//! a restart.
//!
//! The engine's rules are pinned by `mosaica-engine/tests/identity_model.rs`; this file pins the
//! route's decode and answers.

mod common;

use std::path::Path;

use arrow::array::{StringArray, UInt64Array};
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
            column(
                "gid",
                false,
                UInt64Array::from_iter_values(ids.iter().map(|e| gid_of(*e))),
            ),
            column(
                "code",
                false,
                StringArray::from_iter_values(ids.iter().map(|e| format!("c{e}"))),
            ),
        ],
    );
    write_pairs_n(&pairs, N);
    let out = dir.join("bundle");
    build_declared(
        &out,
        &points,
        &pairs,
        &format!("{SCHEMA_TOML}{ID_ATTRIBUTE}"),
    );
    out
}

async fn ingest(served: &Served, batch_id: &str, rows: Value) -> (u16, Value) {
    send(served, "/control/ingest", batch_id, rows).await
}

async fn ingest_strict(served: &Served, batch_id: &str, rows: Value) -> (u16, Value) {
    send(served, "/control/ingest?strict=true", batch_id, rows).await
}

async fn send(served: &Served, path: &str, batch_id: &str, rows: Value) -> (u16, Value) {
    let resp = served
        .server
        .client
        .post(served.server.control_url(path))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-mosaica-batch-id", batch_id)
        .header("x-mosaica-view", "s0")
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
            { "gid": gid_of(5), "access": ["0"] },
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

/// **A refused row writes nothing and the batch applies the rest**: a row naming two items, a
/// later row naming an item or setting a value an earlier row does, and a `tessera_id` nobody
/// holds. Each is listed by position with its reason and answers `null` for its `tessera_id`.
#[tokio::test]
async fn refused_rows_are_listed_and_the_rest_apply() {
    let served = Served::build(fixture).await;
    let three = holder(&served, gid_of(3)).await;
    let (status, body) = ingest(
        &served,
        "mixed",
        json!([
            { "tessera_id": three, "gid": gid_of(4) },
            { "tessera_id": three },
            { "gid": gid_of(3) },
            { "x": 1.0, "y": 1.0, "access": ["0"], "gid": 9 },
            { "x": 2.0, "y": 2.0, "access": ["0"], "gid": 9 },
            { "tessera_id": "12345", "x": 1.0, "y": 1.0, "access": ["0"] },
        ]),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["refused"],
        json!([
            { "row": 0, "reason": "names_two_items" },
            { "row": 2, "reason": "one_item_twice" },
            { "row": 4, "reason": "one_value_twice" },
            { "row": 5, "reason": "unknown_tessera_id" },
        ])
    );
    assert_eq!(
        (body["unchanged"].clone(), body["created"].clone()),
        (json!(1), json!(1))
    );
    let ids = body["tessera_ids"].as_array().unwrap();
    assert_eq!(ids[1], json!(three));
    assert!(ids[3].is_string(), "{body}");
    for refused in [0, 2, 4, 5] {
        assert!(ids[refused].is_null(), "{body}");
    }

    // The kept row created the item holding gid 9, which a later row now names.
    let (status, body) = ingest(
        &served,
        "after",
        json!([{ "x": 1.0, "y": 1.0, "access": ["0"], "gid": 9 }]),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["unchanged"], 1, "{body}");
    assert_eq!(body["tessera_ids"][0], ids[3]);
}

/// **A strict batch the rules refuse is `409` and writes nothing.** The refusal names the items by
/// `tessera_id`, never an entity id.
#[tokio::test]
async fn a_strict_batch_with_a_refused_row_is_a_conflict_and_writes_nothing() {
    let served = Served::build(fixture).await;
    let three = holder(&served, gid_of(3)).await;
    let four = holder(&served, gid_of(4)).await;

    let refused = [
        (
            "two-items",
            json!([{ "tessera_id": three, "gid": gid_of(4) }]),
        ),
        (
            "one-item-twice",
            json!([{ "tessera_id": three }, { "gid": gid_of(3) }]),
        ),
        (
            "one-value-twice",
            json!([
                { "x": 1.0, "y": 1.0, "access": ["0"], "gid": 9 },
                { "x": 2.0, "y": 2.0, "access": ["0"], "gid": 9 },
            ]),
        ),
        (
            "unknown",
            json!([{ "tessera_id": "12345", "x": 1.0, "y": 1.0, "access": ["0"] }]),
        ),
    ];
    for (batch, rows) in refused {
        let (status, body) = ingest_strict(&served, batch, rows).await;
        assert_eq!(status, 409, "{batch}: {body}");
    }
    let (_, body) = ingest_strict(
        &served,
        "two-items-again",
        json!([{ "tessera_id": three, "gid": gid_of(4) }]),
    )
    .await;
    let detail = body["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains(&three) && detail.contains(&four),
        "{detail}"
    );

    // Nothing was written: gid 9 names no item.
    let (status, body) = ingest_strict(
        &served,
        "after",
        json!([{ "tessera_id": three, "code": "c3" }, { "x": 3.0, "y": 3.0, "access": ["0"], "gid": 9 }]),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        (body["unchanged"].clone(), body["created"].clone()),
        (json!(1), json!(1))
    );
    assert_eq!(body["refused"], json!([]));
}

/// **A row creating an item needs a position**: a row naming no item and carrying none is refused
/// as naming no item, and the batch's other rows apply. **A row carries both coordinates or
/// neither**: one alone is `422`.
#[tokio::test]
async fn a_new_item_needs_both_coordinates() {
    let served = Served::build(fixture).await;
    let three = holder(&served, gid_of(3)).await;
    let (status, body) = ingest(
        &served,
        "no-position",
        json!([{ "access": ["0"], "gid": 11 }, { "tessera_id": three }]),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["refused"],
        json!([{ "row": 0, "reason": "names_no_item" }])
    );
    assert_eq!(body["unchanged"], 1, "{body}");
    // A refused row sets no value, so a later row giving the value a position creates the item.
    let (status, body) = ingest(
        &served,
        "no-position-then-placed",
        json!([
            { "access": ["0"], "gid": 11 },
            { "x": 4.0, "y": 4.0, "access": ["0"], "gid": 11 },
        ]),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["refused"],
        json!([{ "row": 0, "reason": "names_no_item" }])
    );
    assert_eq!(body["created"], 1, "{body}");
    let (status, body) = ingest_strict(
        &served,
        "no-position-strict",
        json!([{ "access": ["0"], "gid": 12 }]),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    let (status, body) = ingest(
        &served,
        "one-coordinate",
        json!([{ "x": 5.0, "access": ["0"], "gid": 11 }]),
    )
    .await;
    assert_eq!(status, 422, "{body}");
}

/// **A resent batch answers its first `tessera_id`s and refused rows**, before and after a
/// restart, and changes nothing; a restart applies the accepted rows alone.
#[tokio::test]
async fn a_resent_batch_answers_its_first_tessera_ids_across_a_restart() {
    let served = Served::build(fixture).await;
    let three = holder(&served, gid_of(3)).await;
    let rows = json!([
        { "tessera_id": three },
        { "x": 5.0, "y": 5.0, "access": ["0"], "gid": 12 },
        { "x": 6.0, "y": 6.0, "access": ["0"], "gid": 12 },
    ]);
    let (status, first) = ingest(&served, "kept", rows.clone()).await;
    assert_eq!(status, 200, "{first}");
    assert_eq!(
        first["refused"],
        json!([{ "row": 2, "reason": "one_value_twice" }])
    );
    let (_, replayed) = ingest(&served, "kept", rows.clone()).await;
    assert_eq!(replayed["replayed"], true, "{replayed}");
    assert_eq!(replayed["tessera_ids"], first["tessera_ids"]);
    assert_eq!(replayed["refused"], first["refused"]);

    let served = served.restart().await;
    let (status, replayed) = ingest(&served, "kept", rows).await;
    assert_eq!(status, 200, "{replayed}");
    assert_eq!(replayed["replayed"], true, "{replayed}");
    assert_eq!(replayed["created"], 0, "{replayed}");
    assert_eq!(replayed["tessera_ids"], first["tessera_ids"]);
    assert_eq!(replayed["refused"], first["refused"]);

    // The restart replayed the kept row alone: gid 12 names the one item it created, at (5, 5).
    let (status, body) = ingest(&served, "again", json!([{ "x": 5.0, "y": 5.0, "gid": 12 }])).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["unchanged"], 1, "{body}");
    assert_eq!(body["tessera_ids"][0], first["tessera_ids"][1]);
}

/// **A deleted item's `tessera_id` names nothing once a fold has removed its rows**: a strict
/// ingest row naming it is `409`, and a strict change naming it is `404`.
#[tokio::test]
async fn a_folded_away_items_tessera_id_names_nothing() {
    let served = Served::build(fixture).await;
    let (status, body) = ingest(
        &served,
        "made",
        json!([{ "x": 5.0, "y": 5.0, "access": ["0"], "gid": 21 }]),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let made = body["tessera_ids"][0].as_str().unwrap().to_string();
    tick(&served.server).await;

    let change = |op: &str| json!([{ "op": op, "match": { "tessera_id": made } }]);
    let changes = |body: Value| {
        served
            .server
            .client
            .post(served.server.control_url("/control/changes?strict=true"))
            .bearer_auth(OPERATOR_CREDENTIAL)
            .json(&body)
            .send()
    };
    assert_eq!(changes(change("delete")).await.unwrap().status(), 200);
    fold(&served.server).await;

    let (status, body) = ingest_strict(&served, "named", json!([{ "tessera_id": made }])).await;
    assert_eq!(status, 409, "{body}");
    let resp = changes(change("suppress")).await.unwrap();
    assert_eq!(resp.status(), 404, "a change naming it names nothing");
}

/// **A batch that creates nothing addresses items, and needs a column to address them by**: with
/// no row carrying a position, a batch with neither a `tessera_id` nor a unique column is `422`,
/// and one whose unique column is null in a row refuses that row as naming no item.
#[tokio::test]
async fn a_batch_that_only_addresses_items_needs_a_column_to_name_them_by() {
    let served = Served::build(fixture).await;
    let (status, body) = ingest(&served, "no-identifier", json!([{ "code": "c9" }])).await;
    assert_eq!(status, 422, "{body}");
    let (status, body) = ingest(
        &served,
        "null-identifier",
        json!([{ "gid": null, "code": "c9" }, { "gid": gid_of(3), "code": "c3" }]),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["refused"],
        json!([{ "row": 0, "reason": "names_no_item" }])
    );
    assert_eq!(body["unchanged"], 1, "{body}");
}
