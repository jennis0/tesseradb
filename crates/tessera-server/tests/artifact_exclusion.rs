//! **Membership by exclusion on the wire** (`ingest.md` §1.3, §2.3; contracts §3.4): `excluding`
//! on a publication names the entities the membership leaves out, and the executor complements it
//! once — against the view's entity set as of that step — before the record is written.
//!
//! What is asserted, over the real routes: the two spellings serve the same answers on a
//! quiescent database and after a restart; a list over `max_excluded_per_request` is a `422`
//! naming the limit and the inclusion spelling; the complement holds a point ingested and not yet
//! flushed, and leaves out a deleted entity; a second `excluding` on a held key is a `409`.
//!
//! The database is quiescent for the first case by construction — nothing else writes — which is
//! the condition `annotation-write-cycle.md` §6.1's byte-identity claim carries at ingest.

mod common;

use common::*;
use serde_json::json;
use tempfile::TempDir;

const LAYER: &str = "clusters/excluded";
/// The layer the deleted-entity case uses: served only where the masked count is at least nine
/// tenths of the **declared** membership, which is what makes the denominator observable.
const PROPORTIONAL: &str = "clusters/proportional";

fn declaration(name: &str, criterion: serde_json::Value) -> serde_json::Value {
    json!({
        "name": name,
        "title": name,
        "views": ["s0"],
        "membership": "enumerated",
        "value_set": "closed",
        "visibility": null,
        "artifact_visibility": { "field": null, "default": "inherited" },
        "require_member_visibility": criterion,
        "hierarchy": { "kind": "flat", "prune_children": false },
        "content": { "computed": [], "supplied": [] },
        "depends_on": [],
        "levels": []
    })
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

/// The layer's served rows for a principal holding `terms`, by key — the answer, not the bytes.
async fn served(server: &TestServer, layer: &str, terms: &[&str]) -> Vec<(String, u64)> {
    let auth = authorise(server, terms).await;
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

/// **The two spellings are one membership** (`ingest.md` §2.3): an artifact published by
/// exclusion and one published by inclusion over the same set serve the same masked count to each
/// principal, and both come back from a restart — the complement being what the log carries.
#[tokio::test]
async fn an_exclusion_serves_what_the_inclusion_spelling_serves_and_replays() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    register(&server, declaration(LAYER, serde_json::Value::Null)).await;

    // Everything but the first three items, said both ways. The inclusion names 997 members; the
    // exclusion names three.
    let (status, body) = put(
        &server,
        LAYER,
        json!([
            { "key": "by-exclusion", "excluding": members(0..3) },
            { "key": "by-inclusion", "members": members(3..N_ITEMS) },
        ]),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["created"], 2, "{body}");

    // The broad principal sees every item; the narrow one sees a third of them. Both artifacts
    // answer the same count to each, which is the property — the same *served answers*, not the
    // same bytes.
    let broad = served(&server, LAYER, &["0"]).await;
    assert_eq!(broad.len(), 2, "{broad:?}");
    assert_eq!(broad[0].1, broad[1].1, "{broad:?}");
    assert_eq!(
        broad[0].1,
        N_ITEMS - 3,
        "the complement is the view's entities minus the three named: {broad:?}"
    );
    let narrow = served(&server, LAYER, &["1"]).await;
    assert_eq!(narrow.len(), 2, "{narrow:?}");
    assert_eq!(narrow[0].1, narrow[1].1, "{narrow:?}");
    assert!(narrow[0].1 < broad[0].1, "{narrow:?} against {broad:?}");

    // The record carries the inclusion, so replay lands the same membership.
    let server = restart(server, &tmp).await;
    assert_eq!(
        served(&server, LAYER, &["0"]).await,
        broad,
        "the complement replayed as the membership it was acked as"
    );
}

/// **The bound is on the list** (`ingest.md` §2.3, ruling 4): at the published value the
/// publication lands, and one over it is a `422` naming the limit and the remedy — the inclusion
/// spelling, which pages.
#[tokio::test]
async fn a_list_over_the_bound_is_refused_naming_the_inclusion_spelling() {
    let tmp = TempDir::new().unwrap();
    let bundle = build_fixture(tmp.path(), N_ITEMS);
    let mut engine = tessera_engine::Engine::open(
        &bundle,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        tessera_plugin::Passthrough::new(),
        default_engine_config(),
    )
    .unwrap();
    engine.start_write_executor(1024).unwrap();
    let server = mount_server_with_ingest_limits(
        engine,
        200,
        generous_test_gate(),
        IngestLimits {
            max_excluded_per_request: 4,
            ..generous_ingest_limits()
        },
    )
    .await;
    register(&server, declaration(LAYER, serde_json::Value::Null)).await;

    let status = server
        .client
        .get(server.control_url("/control/status"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(
        status["limits"]["publish"]["max_excluded_per_request"], 4,
        "the block publishes the value the route refuses over: {status}"
    );

    let (code, body) = put(
        &server,
        LAYER,
        json!([{ "key": "at-the-bound", "excluding": members(0..4) }]),
    )
    .await;
    assert_eq!(code, 201, "four is at the limit, not over it: {body}");

    let (code, body) = put(
        &server,
        LAYER,
        json!([{ "key": "over-the-bound", "excluding": members(0..5) }]),
    )
    .await;
    assert_eq!(code, 422, "{body}");
    let detail = body["detail"].as_str().unwrap_or_default().to_string();
    assert_eq!(body["error"], "contract", "{body}");
    assert!(detail.contains("max_excluded_per_request"), "{detail}");
    assert_eq!(
        served(&server, LAYER, &["0"]).await.len(),
        1,
        "the refused publication created nothing"
    );
}

/// **A point acknowledged and not yet flushed is in the view** (`ingest.md` §2.3): the complement
/// holds it, so the artifact counts it from the tick that publishes its row, and a growth naming
/// it joins nothing.
#[tokio::test]
async fn the_complement_holds_a_point_that_is_buffered_and_not_yet_flushed() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    register(&server, declaration(LAYER, serde_json::Value::Null)).await;

    let new_id = N_ITEMS + 7;
    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "one-more-point")
        .header("content-type", "application/vnd.apache.arrow.stream")
        .body(build_ingest_batch_optional(&[(
            Some(&external_id_of(new_id)),
            20.0,
            20.0,
            "0",
        )]))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "the point is durable");

    // Published while that row is still in the buffer.
    let (status, body) = put(
        &server,
        LAYER,
        json!([{ "key": "everything", "excluding": [] }]),
    )
    .await;
    assert_eq!(status, 201, "{body}");

    // The membership already holds it: a growth naming it joins nothing.
    let (status, body) = patch(
        &server,
        LAYER,
        json!([{ "key": "everything", "members": [member(new_id)] }]),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["artifacts"][0]["joined"], 0,
        "the buffered point was already a member: {body}"
    );

    // And it is counted once its row is published.
    drain(&server).await;
    assert_eq!(
        served(&server, LAYER, &["0"]).await,
        vec![("everything".to_string(), N_ITEMS + 1)]
    );
}

/// **A deleted entity is not in the view's entity set** (`ingest.md` §2.3), and the denominator
/// is what says so: the artifact is served only at nine tenths of its **declared** membership, so
/// a complement that had counted the deleted would put it under its own criterion and it would be
/// absent.
#[tokio::test]
async fn the_complement_leaves_out_a_deleted_entity() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    register(
        &server,
        declaration(PROPORTIONAL, json!({ "fraction": 0.9 })),
    )
    .await;

    let deleted = 200u64;
    let resp = server
        .client
        .post(server.control_url("/control/changes"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(
            &(0..deleted)
                .map(|id| json!({ "external_id": member(id), "op": "delete" }))
                .collect::<Vec<_>>(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "the deletions are accepted");

    let (status, body) = put(
        &server,
        PROPORTIONAL,
        json!([{ "key": "the-living", "excluding": [] }]),
    )
    .await;
    assert_eq!(status, 201, "{body}");

    // Declared 800, masked 800: the whole of it, so the artifact clears its criterion. Had the
    // complement carried the 200 deleted entities the declared size would be 1,000 and the same
    // masked count would be 0.8 — under the bar, and the artifact absent.
    assert_eq!(
        served(&server, PROPORTIONAL, &["0"]).await,
        vec![("the-living".to_string(), N_ITEMS - deleted)]
    );
}

/// **A second `excluding` on a held key is a `409`** (`ingest.md` §1.3): the complement it asks
/// for is taken over the entities that exist now, so the same list means a different set.
#[tokio::test]
async fn a_second_exclusion_on_a_held_key_is_a_conflict() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    register(&server, declaration(LAYER, serde_json::Value::Null)).await;

    let (status, body) = put(
        &server,
        LAYER,
        json!([{ "key": "c1", "excluding": members(0..3) }]),
    )
    .await;
    assert_eq!(status, 201, "{body}");

    let (status, body) = put(
        &server,
        LAYER,
        json!([{ "key": "c1", "excluding": members(0..3) }]),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"], "conflict", "{body}");
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("exclusion"),
        "{body}"
    );

    // A record names one spelling or the other. Neither is a refusal — `members` is optional
    // only where `excluding` is given — and an empty `members` list is the artifact whose
    // membership holds nobody, which is a state a record has always been able to publish.
    let (status, body) = put(&server, LAYER, json!([{ "key": "c0" }])).await;
    assert_eq!(status, 422, "{body}");
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("neither `members` nor `excluding`"),
        "{body}"
    );
    let (status, body) = put(&server, LAYER, json!([{ "key": "c0", "members": [] }])).await;
    assert_eq!(status, 201, "an empty membership is a real state: {body}");

    // A membership has one spelling: both on one row is refused at decoding.
    let (status, body) = put(
        &server,
        LAYER,
        json!([{ "key": "c2", "members": members(0..2), "excluding": members(2..4) }]),
    )
    .await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["error"], "contract", "{body}");
}
