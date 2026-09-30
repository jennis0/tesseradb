//! `/control/changes` names each item in `match`, by its `tessera_id` and the values of unique
//! fields, under the identity rule: a change naming no item, or two, is refused, listed in the
//! answer while the rest apply, or refusing the request under `strict=true`.
//!
//! A `tessera_id` is a keyed permutation of entity space, so a record carrying one would resolve
//! under whatever key the bundle holds at replay. It is inverted once, at admission, and the
//! entity is what the write-ahead log keeps.

mod common;

use common::*;
use tempfile::TempDir;

/// The whole fixture, as a count a suppression moves.
async fn visible(server: &TestServer, token: &str) -> u64 {
    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0]
        }))
        .send()
        .await
        .unwrap();
    let (tiles, _) = decode_viewport(&resp.bytes().await.unwrap());
    // `TileRow` is `(tile, visible, served)`.
    tiles.iter().map(|(_, visible, _)| *visible).sum()
}

/// A `tessera_id` for an item the fixture actually carries — taken off a viewport response, which
/// is exactly where a client gets one. `PointRow` is `(tessera_id, code)`.
async fn a_drawn_tessera_id(server: &TestServer, token: &str) -> u64 {
    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0]
        }))
        .send()
        .await
        .unwrap();
    let (_, points) = decode_viewport(&resp.bytes().await.unwrap());
    points.first().expect("the fixture draws points").0
}

async fn post_changes(server: &TestServer, body: &serde_json::Value) -> reqwest::Response {
    server
        .client
        .post(server.control_url("/control/changes"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(body)
        .send()
        .await
        .unwrap()
}

/// Ingest one item holding no unique value and return the `tessera_id` the 200 carried: the only
/// name that item has.
async fn ingest_anonymous(server: &TestServer, batch_id: &str) -> u64 {
    let body = build_ingest_batch_optional(&[(None, 20.0, 20.0, "0")]);
    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", batch_id)
        .header("content-type", "application/vnd.apache.arrow.stream")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let json: serde_json::Value = resp.json().await.unwrap();
    ingested_ids(&json)[0]
}

async fn post_strict(server: &TestServer, body: &serde_json::Value) -> reqwest::Response {
    server
        .client
        .post(server.control_url("/control/changes?strict=true"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(body)
        .send()
        .await
        .unwrap()
}

/// An item holding no unique value is suppressed and unsuppressed by its `tessera_id`.
#[tokio::test]
async fn an_item_holding_no_unique_value_is_suppressible_by_tessera_id() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    let _ = authorise(&server, &["0"]).await;

    let tessera_id = ingest_anonymous(&server, "anon-1").await;

    let resp = post_changes(
        &server,
        &serde_json::json!([
            { "op": "suppress", "match": { "tessera_id": tessera_id.to_string() } },
        ]),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let answer: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(answer["accepted"], 1);
    assert_eq!(answer["refused"], serde_json::json!([]));

    // Not asserted through a viewport count: the item is still buffered, so it is in no count
    // before or after. An unsuppress of the same identifier is accepted, which it could not be
    // had the suppress named nothing.
    let resp = post_changes(
        &server,
        &serde_json::json!([
            { "op": "unsuppress", "match": { "tessera_id": tessera_id.to_string() } },
        ]),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.json::<serde_json::Value>().await.unwrap()["accepted"],
        1
    );
}

/// One request names items by a unique value, by a `tessera_id`, and by both agreeing.
#[tokio::test]
async fn a_change_names_its_item_by_any_identifier_it_holds() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    let token = token_for(&server, &["0"]).await;

    let by_tessera = tessera_id_of(&server, 9);
    let both = tessera_id_of(&server, 7);
    let before = visible(&server, &token).await;

    let resp = post_changes(
        &server,
        &serde_json::json!([
            { "op": "suppress", "match": { "id": 5 } },
            { "op": "suppress", "match": { "tessera_id": by_tessera.to_string() } },
            { "op": "suppress", "match": { "id": "7", "tessera_id": both.to_string() } },
        ]),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.json::<serde_json::Value>().await.unwrap()["accepted"],
        3
    );
    assert_eq!(visible(&server, &token).await, before - 3);
}

/// A change naming no item, or two, is refused and listed with its reason, and the others apply.
/// Under `strict=true` the first refused change refuses the request: 404 for naming nothing, 409
/// for naming two, and nothing applies.
#[tokio::test]
async fn a_change_naming_no_item_or_two_is_refused_alone_or_refuses_a_strict_request() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    let token = token_for(&server, &["0"]).await;

    let three = tessera_id_of(&server, 3);
    let request = serde_json::json!([
        { "op": "suppress", "match": { "id": 1, "name": "ignored", "weight": 2.5 } },
        { "op": "suppress", "match": { "tessera_id": u64::MAX.to_string() } },
        { "op": "suppress", "match": { "id": 999_999 } },
        { "op": "suppress", "match": { "id": 2, "tessera_id": three.to_string() } },
        { "op": "suppress", "match": {} },
        { "op": "suppress", "match": { "id": null } },
    ]);
    let before = visible(&server, &token).await;

    let first = serde_json::json!([request[0], request[1]]);
    let resp = post_strict(&server, &first).await;
    assert_eq!(resp.status(), 404, "a tessera_id naming nothing");
    let two = serde_json::json!([request[0], request[3]]);
    let resp = post_strict(&server, &two).await;
    assert_eq!(resp.status(), 409, "values naming two items");
    assert_eq!(
        visible(&server, &token).await,
        before,
        "a strict refusal applies nothing"
    );

    let resp = post_changes(&server, &request).await;
    assert_eq!(resp.status(), 200);
    let answer: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(answer["accepted"], 1);
    assert_eq!(
        answer["ignored_columns"],
        serde_json::json!(["name", "weight"])
    );
    assert_eq!(
        answer["refused"],
        serde_json::json!([
            { "row": 1, "reason": "unknown_tessera_id" },
            { "row": 2, "reason": "names_no_item" },
            { "row": 3, "reason": "names_two_items" },
            { "row": 4, "reason": "names_no_item" },
            { "row": 5, "reason": "names_no_item" },
        ])
    );
    assert_eq!(
        visible(&server, &token).await,
        before - 1,
        "only the first applied"
    );
}

/// A malformed request applies nothing, whatever `strict` says.
#[tokio::test]
async fn a_malformed_change_request_is_refused_whole() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    let token = token_for(&server, &["0"]).await;

    let id = ingest_anonymous(&server, "anon-1").await;
    let before = visible(&server, &token).await;

    for (what, body) in [
        (
            "no change names an item by any column",
            serde_json::json!([{ "op": "suppress", "match": {} }]),
        ),
        (
            "a change without a match",
            serde_json::json!([{ "op": "suppress" }]),
        ),
        (
            "the old flat form",
            serde_json::json!([{ "op": "suppress", "tessera_id": id.to_string() }]),
        ),
        (
            "no column that names items",
            serde_json::json!([{ "op": "suppress", "match": { "nothing": 5 } }]),
        ),
        (
            "an integer field's value is decimal digits",
            serde_json::json!([{ "op": "suppress", "match": { "id": "five" } }]),
        ),
        (
            "an integer field's value is a whole number",
            serde_json::json!([{ "op": "suppress", "match": { "id": 1.5 } }]),
        ),
        (
            "a bare number is what loses u64s past 2^53 in a browser",
            serde_json::json!([{ "op": "suppress", "match": { "tessera_id": id } }]),
        ),
    ] {
        assert_eq!(
            post_changes(&server, &body).await.status(),
            422,
            "refused wholesale: {what}"
        );
    }
    assert_eq!(visible(&server, &token).await, before);
}

/// A `tessera_id` never enters the WAL: the record carries the resolved entity, so a restart
/// replays the deny to the same entity whatever key the bundle holds.
#[tokio::test]
async fn a_tessera_addressed_deny_replays_to_the_same_entity() {
    let tmp = TempDir::new().unwrap();
    let root = build_fixture(tmp.path(), N_ITEMS);
    let cache = tmp.path().join("cache");
    let wal = tmp.path().join("wal.log");

    let before_suppression = {
        let server = spawn_server(&root, &cache, &wal).await;
        let auth = authorise(&server, &["0"]).await;
        let token = auth["token"].as_str().unwrap().to_string();

        let id = a_drawn_tessera_id(&server, &token).await;
        let before = visible(&server, &token).await;
        let resp = post_changes(
            &server,
            &serde_json::json!([
                { "op": "suppress", "match": { "tessera_id": id.to_string() } },
            ]),
        )
        .await;
        assert_eq!(resp.status(), 200);
        assert_eq!(visible(&server, &token).await, before - 1);
        // Stopped and waited for: the reopen below takes the bundle root's write lock, which this
        // server's executor holds until its engine is dropped (write-path §1.2).
        server.shutdown().await;
        before
    };

    // Reopened on the same WAL: replay is the only thing that can carry the suppression forward.
    let restarted = spawn_server(&root, &tmp.path().join("cache-2"), &wal).await;
    let token = token_for(&restarted, &["0"]).await;
    assert_eq!(
        visible(&restarted, &token).await,
        before_suppression - 1,
        "the record named an entity, so replay resolved nothing and reached the same item"
    );
}
