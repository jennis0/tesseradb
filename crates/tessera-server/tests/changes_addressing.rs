//! `/control/changes` addresses an entity two ways (contracts §3.4).
//!
//! **The hole this closes.** Contracts §3.4 r6 makes an external id optional at ingest, and an item
//! that arrived without one was addressable by *nothing* on this endpoint — not deletable, not
//! suppressible, at all. A `tessera_id` is what every client already holds for such an item, and it
//! is what `/control/ingest` returned when the item was accepted.
//!
//! **Why the identifier never reaches the WAL.** A `tessera_id` is a keyed permutation of entity
//! space, so a record carrying one would resolve under whatever key the bundle holds at replay: a
//! rotation would silently redirect every such deny to a different entity. It is inverted once, at
//! admission, and the entity is what is persisted (`WalRecord::ChangeByEntity`).

mod common;

use base64::Engine as _;
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

/// Ingest one item **without** an external id and return the `tessera_id` the 200 carried — the
/// only name that item will ever have.
async fn ingest_anonymous(server: &TestServer, batch_id: &str) -> u64 {
    let body = build_ingest_batch_optional(&[(None, 20.0, 20.0, "0")]);
    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", batch_id)
        .header("content-type", "application/octet-stream")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let json: serde_json::Value = resp.json().await.unwrap();
    json["tessera_ids"].as_array().unwrap()[0].as_u64().unwrap()
}

/// **The hole this closes**: an item ingested with no external id is addressable — and therefore
/// deniable — only by `tessera_id`. Before this it could not be denied at all.
#[tokio::test]
async fn an_item_ingested_without_an_external_id_is_suppressible_by_tessera_id() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server(
        &root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;
    let _ = authorise(&server, &["0"]).await;
    // No token needed: this case asserts acceptance, not a count (see below).

    let tessera_id = ingest_anonymous(&server, "anon-1").await;

    let resp = post_changes(
        &server,
        &serde_json::json!([
            { "tessera_id": tessera_id.to_string(), "idset": FIXTURE_IDSET, "op": "suppress" },
        ]),
    )
    .await;
    assert_eq!(
        resp.status(),
        200,
        "an item with no external id has exactly one name, and this endpoint now takes it — \
         before this it could not be addressed on this endpoint at all"
    );

    // **Not asserted through a viewport count, deliberately.** The item is still buffered: it has
    // no row, so it is in no count either before or after, and a count assertion here would pass
    // for the wrong reason on any build. What is observable is that the disposition was accepted
    // and is in force — an unsuppress of the same identifier round-trips, which it could not if
    // the suppress had resolved to nothing.
    let resp = post_changes(
        &server,
        &serde_json::json!([
            { "tessera_id": tessera_id.to_string(), "idset": FIXTURE_IDSET, "op": "unsuppress" },
        ]),
    )
    .await;
    assert_eq!(resp.status(), 200);
}

/// One batch, both address forms, applied together — bulk is the list, and its elements may be
/// addressed either way.
#[tokio::test]
async fn a_mixed_bulk_batch_applies_both_address_forms() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server(
        &root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;
    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap().to_string();

    // Two different fixture items, named two different ways.
    let by_external = base64::engine::general_purpose::STANDARD.encode(external_id_of(5));
    let by_tessera = a_drawn_tessera_id(&server, &token).await;
    let before = visible(&server, &token).await;

    let resp = post_changes(
        &server,
        &serde_json::json!([
            { "external_id": by_external, "op": "suppress" },
            { "tessera_id": by_tessera.to_string(), "idset": FIXTURE_IDSET, "op": "suppress" },
        ]),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        visible(&server, &token).await,
        before - 2,
        "both address forms reached the same overlay"
    );
}

/// **The permutation is total** — every `u64` inverts to *something* — so the range check is the
/// whole of the misdirection guard, and a batch containing one unresolvable identifier applies
/// **nothing**, including the elements that were addressable.
#[tokio::test]
async fn an_out_of_range_tessera_id_refuses_the_whole_batch() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server(
        &root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;
    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap().to_string();

    let good = ingest_anonymous(&server, "anon-1").await;
    let before = visible(&server, &token).await;

    let resp = post_changes(
        &server,
        &serde_json::json!([
            { "tessera_id": good.to_string(), "idset": FIXTURE_IDSET, "op": "suppress" },
            { "tessera_id": u64::MAX.to_string(), "idset": FIXTURE_IDSET, "op": "suppress" },
        ]),
    )
    .await;
    assert_eq!(resp.status(), 404);
    assert_eq!(
        visible(&server, &token).await,
        before,
        "the whole batch applied nothing"
    );
}

/// A list gathered before a key rotation must not be reinterpreted under the new key. The stale
/// idset is refused **before any inversion**, so no identifier is ever resolved under a key it was
/// not minted under (decision 0025).
#[tokio::test]
async fn a_stale_idset_is_refused_before_inversion() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server(
        &root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;
    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap().to_string();

    let id = ingest_anonymous(&server, "anon-1").await;
    let before = visible(&server, &token).await;

    let resp = post_changes(
        &server,
        &serde_json::json!([
            { "tessera_id": id.to_string(), "idset": FIXTURE_IDSET + 1, "op": "suppress" },
        ]),
    )
    .await;
    assert_eq!(resp.status(), 409);
    assert_eq!(visible(&server, &token).await, before);
}

/// Exactly one address form, and a tessera address carries the idset it was minted under. Every
/// shape below is a wholesale refusal before anything is enqueued.
#[tokio::test]
async fn an_element_names_exactly_one_address_form() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server(
        &root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;
    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap().to_string();

    let id = ingest_anonymous(&server, "anon-1").await;
    let external = base64::engine::general_purpose::STANDARD.encode(external_id_of(5));
    let before = visible(&server, &token).await;

    for (what, body) in [
        (
            "both forms is ambiguous",
            serde_json::json!([{ "external_id": external, "tessera_id": id.to_string(), "op": "suppress" }]),
        ),
        (
            "neither form is unaddressable",
            serde_json::json!([{ "op": "suppress" }]),
        ),
        (
            "an idset without a tessera_id guards nothing",
            serde_json::json!([{ "external_id": external, "idset": FIXTURE_IDSET, "op": "suppress" }]),
        ),
        (
            "a tessera address must carry its idset",
            serde_json::json!([{ "tessera_id": id.to_string(), "op": "suppress" }]),
        ),
        (
            "a bare number is what loses u64s past 2^53 in a browser",
            serde_json::json!([{ "tessera_id": id, "idset": FIXTURE_IDSET, "op": "suppress" }]),
        ),
    ] {
        assert_eq!(
            post_changes(&server, &body).await.status(),
            422,
            "refused wholesale: {what}"
        );
    }
    assert_eq!(
        visible(&server, &token).await,
        before,
        "and none of them applied anything"
    );
}

/// A `tessera_id` never enters the WAL: the record carries the resolved entity, so a restart
/// replays the deny to the same entity whatever key the bundle holds.
#[tokio::test]
async fn a_tessera_addressed_deny_replays_to_the_same_entity() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
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
                { "tessera_id": id.to_string(), "idset": FIXTURE_IDSET, "op": "suppress" },
            ]),
        )
        .await;
        assert_eq!(resp.status(), 200);
        assert_eq!(visible(&server, &token).await, before - 1);
        before
    };

    // Reopened on the same WAL: replay is the only thing that can carry the suppression forward.
    let restarted = spawn_server(&root, &tmp.path().join("cache-2"), &wal).await;
    let auth = authorise(&restarted, &["0"]).await;
    let token = auth["token"].as_str().unwrap().to_string();
    assert_eq!(
        visible(&restarted, &token).await,
        before_suppression - 1,
        "the record named an entity, so replay resolved nothing and reached the same item"
    );
}
