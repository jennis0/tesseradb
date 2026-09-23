//! **A membership grows through the control plane** (decision 0127; `artifacts-from-points.md`
//! §6.1): `PATCH /control/layers/{name}/artifacts` adds members to artifacts the level already
//! holds, by the key each was published under.
//!
//! What is asserted, over the real routes: an artifact published with a first slice and grown in
//! two more serves the union of the three, masked per principal; an unknown key and a deleted
//! member each refuse the whole batch with nothing applied; a suppressed member joins and stays
//! outside every mask; a suppressed artifact grows and stays suppressed; the growth comes back from
//! a restart and survives a fold; the same under `tessera` addressing, with a stale idset refused;
//! and the body takes keys, members and the fixed parts and nothing else (the fills are
//! `artifact_fill.rs`'s subject). The response carries a `tessera_id` and the count that joined,
//! and never an ordinal or a membership size (C8).

mod common;

use common::*;
use serde_json::json;
use tempfile::TempDir;

const LAYER: &str = "clusters/grown";

/// How many of `range` the fixture gives term 1 to — the narrow principal's expected count.
fn narrow_count(range: std::ops::Range<u64>) -> u64 {
    range.filter(|s| terms_of(*s).contains(&1)).count() as u64
}

fn artifacts_url(server: &TestServer) -> String {
    server.control_url(&format!(
        "/control/layers/{}/artifacts",
        LAYER.replace('/', "%2F")
    ))
}

/// Publish one artifact under `key`; return its `tessera_id`.
async fn publish(server: &TestServer, key: &str, body_members: Vec<String>) -> String {
    let resp = server
        .client
        .put(artifacts_url(server))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({
            "addressing": "external",
            "artifacts": [{ "key": key, "members": body_members }]
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(status, 201, "{body}");
    // Durable at the acknowledgement, served from the next publication (`ingest.md` §1.3).
    tick(server).await;
    body["artifacts"][0]["tessera_id"]
        .as_str()
        .expect("a publication answers an identifier")
        .to_string()
}

/// `PATCH` with an arbitrary body; the status and the decoded body.
async fn grow_raw(server: &TestServer, body: serde_json::Value) -> (u16, serde_json::Value) {
    let resp = server
        .client
        .patch(artifacts_url(server))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.json().await.unwrap_or(serde_json::Value::Null);
    if status < 300 {
        // Durable at the acknowledgement, served from the next publication (`ingest.md` §1.3).
        tick(server).await;
    }
    (status, body)
}

/// Grow under external addressing, one or more artifacts.
async fn grow(server: &TestServer, artifacts: serde_json::Value) -> (u16, serde_json::Value) {
    grow_raw(
        server,
        json!({ "addressing": "external", "artifacts": artifacts }),
    )
    .await
}

/// The one artifact this layer serves to `terms`, if any.
async fn served(server: &TestServer, terms: &[&str]) -> Option<ArtifactRow> {
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
            "layers": [LAYER],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let rows = decode_viewport_frames(&resp.bytes().await.unwrap())
        .artifacts
        .unwrap_or_default();
    assert!(rows.len() <= 1, "one artifact is published: {rows:?}");
    rows.into_iter().next()
}

/// The masked count `terms` is shown for the layer's artifact.
async fn count(server: &TestServer, terms: &[&str]) -> u64 {
    served(server, terms)
        .await
        .expect("the artifact is served to this principal")
        .masked_count
}

/// One `/control/changes` entry.
async fn change(server: &TestServer, item: serde_json::Value) {
    let resp = server
        .client
        .post(server.control_url("/control/changes"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!([item]))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    assert_eq!(status, 200, "{}", resp.text().await.unwrap());
}

/// The receipt says what joined and nothing else.
fn assert_receipt(artifact: &serde_json::Value, key: &str, tessera_id: &str, joined: u64) {
    assert_eq!(artifact["key"], key, "{artifact}");
    assert_eq!(
        artifact["tessera_id"], tessera_id,
        "the identifier is the publication's: {artifact}"
    );
    assert_eq!(artifact["joined"], joined, "{artifact}");
    for field in ["ordinal", "members", "size", "count", "membership"] {
        assert!(
            artifact.get(field).is_none(),
            "a growth discloses no {field}: {artifact}"
        );
    }
}

/// **The headline.** Published with a first slice, grown in two more, and the viewer plane serves
/// the union — masked per principal, so the narrow principal sees only the members they may.
#[tokio::test]
async fn an_artifact_published_with_one_slice_grows_in_two_more_and_serves_the_union() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    register(&server, flat_layer(LAYER)).await;
    let id = publish(&server, "a", members(0..10)).await;
    assert_eq!(count(&server, &["0"]).await, 10);

    let (status, body) = grow(&server, json!([{ "key": "a", "members": members(10..20) }])).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["artifacts"].as_array().unwrap().len(), 1);
    assert_receipt(&body["artifacts"][0], "a", &id, 10);

    let (status, body) = grow(&server, json!([{ "key": "a", "members": members(20..30) }])).await;
    assert_eq!(status, 200, "{body}");
    assert_receipt(&body["artifacts"][0], "a", &id, 10);

    assert_eq!(
        count(&server, &["0"]).await,
        30,
        "the broad principal sees the union"
    );
    assert_eq!(
        count(&server, &["1"]).await,
        narrow_count(0..30),
        "the narrow principal sees the union inside their mask"
    );
    let row = served(&server, &["0"]).await.unwrap();
    assert_eq!(row.tessera_id.to_string(), id, "the identity did not move");

    // The no-op: the same slice again names the artifact and adds nothing, and is accepted.
    let (status, body) = grow(&server, json!([{ "key": "a", "members": members(20..30) }])).await;
    assert_eq!(status, 200, "{body}");
    assert_receipt(&body["artifacts"][0], "a", &id, 0);
    assert_eq!(count(&server, &["0"]).await, 30);

    // And a slice that overlaps reports only what was new.
    let (status, body) = grow(&server, json!([{ "key": "a", "members": members(25..35) }])).await;
    assert_eq!(status, 200, "{body}");
    assert_receipt(&body["artifacts"][0], "a", &id, 5);
    assert_eq!(count(&server, &["0"]).await, 35);
}

/// A key the level does not hold refuses the batch, the artifact beside it included: nothing is
/// minted and nothing joins.
#[tokio::test]
async fn an_unknown_key_refuses_the_whole_batch() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    register(&server, flat_layer(LAYER)).await;
    publish(&server, "a", members(0..10)).await;

    let (status, body) = grow(
        &server,
        json!([
            { "key": "a", "members": members(10..20) },
            { "key": "never-published", "members": members(20..30) },
        ]),
    )
    .await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["error"], "contract", "{body}");
    assert!(
        body["detail"].as_str().unwrap_or_default().contains("never-published"),
        "the refusal names the key: {body}"
    );
    assert_eq!(
        count(&server, &["0"]).await,
        10,
        "the artifact beside the unknown key did not grow"
    );
    assert_eq!(
        server.state.engine.published_artifacts(),
        1,
        "and nothing was minted under the unknown key"
    );
}

/// A deleted member refuses the batch; a suppressed member joins and stays outside every mask
/// until the suppression is lifted (`artifacts-from-points.md` §6.1's two refusals).
#[tokio::test]
async fn a_deleted_member_refuses_the_batch_and_a_suppressed_member_joins() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    register(&server, flat_layer(LAYER)).await;
    publish(&server, "a", members(0..10)).await;

    change(
        &server,
        json!({ "external_id": member(15), "op": "delete" }),
    )
    .await;
    let (status, body) = grow(&server, json!([{ "key": "a", "members": members(10..20) }])).await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["error"], "contract", "{body}");
    let detail = body["detail"].as_str().expect("the envelope's detail");
    // The body carries a count and the key, never an entity id (I10): the only number in it is
    // the one deleted member.
    let numbers: Vec<&str> = detail
        .split(|c: char| !c.is_ascii_digit())
        .filter(|run| !run.is_empty())
        .collect();
    assert_eq!(numbers, vec!["1"], "{detail}");
    assert_eq!(count(&server, &["0"]).await, 10, "nothing joined");

    // 16 is suppressed: it joins, and is not counted until the suppression is lifted.
    change(
        &server,
        json!({ "external_id": member(16), "op": "suppress" }),
    )
    .await;
    let (status, body) = grow(&server, json!([{ "key": "a", "members": members(16..20) }])).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["artifacts"][0]["joined"], 4, "{body}");
    assert_eq!(
        count(&server, &["0"]).await,
        13,
        "the suppressed member is not counted"
    );

    change(
        &server,
        json!({ "external_id": member(16), "op": "unsuppress" }),
    )
    .await;
    assert_eq!(
        count(&server, &["0"]).await,
        14,
        "it was a member all along, and counts once the suppression is lifted"
    );
}

/// The key resolves against the store and never against what is served, so a suppressed artifact
/// grows like any other and stays suppressed (`artifacts-from-points.md` §5).
#[tokio::test]
async fn a_suppressed_artifact_grows_and_stays_suppressed() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    register(&server, flat_layer(LAYER)).await;
    let id = publish(&server, "a", members(0..10)).await;
    change(
        &server,
        json!({ "tessera_id": id, "idset": FIXTURE_IDSET, "op": "suppress" }),
    )
    .await;
    assert!(
        served(&server, &["0"]).await.is_none(),
        "the suppression is in force at the ack"
    );

    let (status, body) = grow(&server, json!([{ "key": "a", "members": members(10..20) }])).await;
    assert_eq!(status, 200, "{body}");
    assert_receipt(&body["artifacts"][0], "a", &id, 10);
    assert!(
        served(&server, &["0"]).await.is_none(),
        "grown, and still suppressed"
    );
    assert_eq!(
        server.state.engine.published_artifacts(),
        1,
        "no second artifact was minted under the suppressed key"
    );

    change(
        &server,
        json!({ "tessera_id": id, "idset": FIXTURE_IDSET, "op": "unsuppress" }),
    )
    .await;
    assert_eq!(
        count(&server, &["0"]).await,
        20,
        "lifted, the artifact serves the members that joined while it was hidden"
    );
}

/// The `ArtifactGrow` record is replayed on restart, and the fold rewrites the level whole with
/// the grown membership in it.
#[tokio::test]
async fn growth_survives_a_restart_and_a_fold() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    register(&server, flat_layer(LAYER)).await;
    let id = publish(&server, "a", members(0..10)).await;
    let (status, body) = grow(&server, json!([{ "key": "a", "members": members(10..20) }])).await;
    assert_eq!(status, 200, "{body}");

    let server = restart(server, &tmp).await;
    let row = served(&server, &["0"]).await.expect("replayed");
    assert_eq!(row.masked_count, 20, "the growth came back from the log");
    assert_eq!(row.tessera_id.to_string(), id);

    let (status, body) = grow(&server, json!([{ "key": "a", "members": members(20..30) }])).await;
    assert_eq!(status, 200, "{body}");
    assert_receipt(&body["artifacts"][0], "a", &id, 10);
    assert_eq!(count(&server, &["0"]).await, 30);

    flush_and_fold(&server, None).await;
    assert_eq!(
        count(&server, &["0"]).await,
        30,
        "the fold kept the grown membership"
    );
    assert_eq!(count(&server, &["1"]).await, narrow_count(0..30));

    let server = restart(server, &tmp).await;
    assert_eq!(
        count(&server, &["0"]).await,
        30,
        "and the folded level carries it without the log"
    );

    // Growth after the fold lands on the rewritten level.
    let (status, body) = grow(&server, json!([{ "key": "a", "members": members(30..40) }])).await;
    assert_eq!(status, 200, "{body}");
    assert_receipt(&body["artifacts"][0], "a", &id, 10);
    assert_eq!(count(&server, &["0"]).await, 40);
}

/// `tessera` addressing: the identifiers a viewer holds, under the idset they were minted under.
/// A stale idset is a `409` and nothing joins.
#[tokio::test]
async fn tessera_addressing_grows_under_the_current_idset_and_refuses_a_stale_one() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    register(&server, flat_layer(LAYER)).await;

    // The identifiers of served points, as a client would hold them.
    let auth = authorise(&server, &["0"]).await;
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
            "layers": [],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let points = decode_viewport_frames(&resp.bytes().await.unwrap()).points;
    let ids: Vec<String> = points
        .iter()
        .map(|(tessera_id, _)| tessera_id.to_string())
        .collect();
    assert!(
        ids.len() >= 100,
        "the fixture serves enough points: {}",
        ids.len()
    );
    let (first, second) = (ids[..50].to_vec(), ids[50..100].to_vec());

    let resp = server
        .client
        .put(artifacts_url(&server))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({
            "addressing": "tessera",
            "idset": FIXTURE_IDSET,
            "artifacts": [{ "key": "a", "members": first }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201);
    let body: serde_json::Value = resp.json().await.unwrap();
    let id = body["artifacts"][0]["tessera_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(count(&server, &["0"]).await, 50);

    let (status, body) = grow_raw(
        &server,
        json!({
            "addressing": "tessera",
            "idset": FIXTURE_IDSET,
            "artifacts": [{ "key": "a", "members": second }]
        }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_receipt(&body["artifacts"][0], "a", &id, 50);
    assert_eq!(count(&server, &["0"]).await, 100);

    // The same members again, to say the receipt counts what was new.
    let (status, body) = grow_raw(
        &server,
        json!({
            "addressing": "tessera",
            "idset": FIXTURE_IDSET,
            "artifacts": [{ "key": "a", "members": ids[..100].to_vec() }]
        }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_receipt(&body["artifacts"][0], "a", &id, 0);

    // A stale idset is refused before anything is inverted.
    let (status, body) = grow_raw(
        &server,
        json!({
            "addressing": "tessera",
            "idset": FIXTURE_IDSET + 1,
            "artifacts": [{ "key": "a", "members": ids[100..].to_vec() }]
        }),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(count(&server, &["0"]).await, 100, "nothing joined");

    // And identifiers without an idset, or an idset beside external ids, are the publication's
    // two 422s.
    let (status, body) = grow_raw(
        &server,
        json!({
            "addressing": "tessera",
            "artifacts": [{ "key": "a", "members": ids[100..].to_vec() }]
        }),
    )
    .await;
    assert_eq!(status, 422, "{body}");
    let (status, body) = grow_raw(
        &server,
        json!({
            "addressing": "external",
            "idset": FIXTURE_IDSET,
            "artifacts": [{ "key": "a", "members": members(100..110) }]
        }),
    )
    .await;
    assert_eq!(status, 422, "{body}");
}

/// The body is keys, the rank a row pages, the members joining and leaving, and the fixed parts
/// (`ingest.md` §1.1, §1.5). A field outside that set, a
/// missing key and a batch naming nothing are each refused at decoding; a part this layer cannot
/// hold — a shape on a layer declaring none, content on a layer declaring none, a parent on a flat
/// layer, an attachment into an undeclared layer — is refused by the engine, the whole batch
/// without effect. The fills themselves are `artifact_fill.rs`'s subject.
#[tokio::test]
async fn a_growth_body_carries_keys_members_and_the_fixed_parts_and_nothing_else() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    register(&server, flat_layer(LAYER)).await;
    publish(&server, "a", members(0..10)).await;

    for extra in [
        json!({ "content": [{ "rank": 0, "values": ["x"] }] }),
        json!({ "parent": ["a"] }),
        json!({ "bbox": [0.0, 0.0, 1.0, 1.0] }),
        json!({ "attached_to": { "layer": LAYER, "key": "a" } }),
    ] {
        let mut artifact = json!({ "key": "a", "members": members(10..20) });
        for (k, v) in extra.as_object().unwrap() {
            artifact[k] = v.clone();
        }
        let (status, body) = grow(&server, json!([artifact])).await;
        assert_eq!(status, 422, "{extra}: {body}");
    }
    let (status, body) = grow(
        &server,
        json!([{ "key": "a", "members": members(10..20), "excluded": [] }]),
    )
    .await;
    assert_eq!(status, 422, "a field outside the body: {body}");
    let (status, body) = grow(&server, json!([{ "members": members(10..20) }])).await;
    assert_eq!(status, 422, "a growth without a key: {body}");
    let (status, body) = grow(&server, json!([])).await;
    assert_eq!(status, 422, "a growth naming nothing: {body}");

    assert_eq!(
        count(&server, &["0"]).await,
        10,
        "none of them applied anything"
    );

    // An unresolvable member is refused by position, as a publication's is.
    let (status, body) = grow(
        &server,
        json!([{ "key": "a", "members": [member(10), member(11), member(10_000)] }]),
    )
    .await;
    assert_eq!(status, 404, "{body}");
    assert_eq!(body["error"], "unknown", "{body}");
    assert_eq!(count(&server, &["0"]).await, 10);
}

/// **A growth restating members the artifact holds appends no record, and one restating part of a
/// page appends the rest alone** (issue #155).
///
/// `prepare_grow` built its delta from the caller's set alone, so a page a producer resent put a
/// delta that changes nothing into the log and pinned the log at it, a growth pin being one the
/// compaction fold alone releases. The receipt already read the difference, so the log is where
/// this is visible: the records are counted from the reopened log, and the second page's delta is
/// decoded and checked against the members it had not sent before.
#[tokio::test]
async fn a_restated_growth_appends_only_the_members_the_artifact_does_not_hold() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    register(&server, flat_layer(LAYER)).await;
    publish(&server, "a", members(0..10)).await;

    // Restated whole: every member the publication gave it.
    let (status, body) = grow(&server, json!([{ "key": "a", "members": members(0..10) }])).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["artifacts"][0]["joined"], 0, "{body}");

    // Restated in part: the ten it holds and ten it does not.
    let (status, body) = grow(&server, json!([{ "key": "a", "members": members(0..20) }])).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["artifacts"][0]["joined"], 10, "{body}");
    assert_eq!(count(&server, &["0"]).await, 20);

    server.shutdown().await;
    let (_wal, records) =
        tessera_lifecycle::wal::Wal::open(tmp.path().join("wal.log")).expect("the log reopens");
    let growths: Vec<&Vec<tessera_lifecycle::wal::MembershipGrowth>> = records
        .iter()
        .filter_map(|record| match record {
            tessera_lifecycle::wal::WalRecord::ArtifactGrow { growth, .. } => Some(growth),
            _ => None,
        })
        .collect();
    assert_eq!(
        growths.len(),
        1,
        "the wholly restated page appended nothing, and the partly restated one appended once"
    );
    let joining: Vec<u64> = growths[0]
        .iter()
        .map(|grown| {
            tessera_lifecycle::membership::deserialise_members(&grown.joining)
                .expect("the delta decodes")
                .cardinality()
        })
        .collect();
    assert_eq!(joining, vec![10], "the delta is the ten that had not joined");
}
