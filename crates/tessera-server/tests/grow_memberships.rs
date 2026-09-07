//! **A membership grows through the control plane** (decision 0127; `artifacts-from-points.md`
//! §6.1): `PATCH /control/layers/{name}/artifacts` adds members to artifacts the level already
//! holds, by the key each was published under.
//!
//! What is asserted, over the real routes: an artifact published with a first slice and grown in
//! two more serves the union of the three, masked per principal; an unknown key and a deleted
//! member each refuse the whole batch with nothing applied; a suppressed member joins and stays
//! outside every mask; a suppressed artifact grows and stays suppressed; the growth comes back from
//! a restart and survives a fold; the same under `tessera` addressing, with a stale idset refused;
//! and the body takes keys and members and nothing else. The response carries a `tessera_id` and
//! the count that joined, and never an ordinal or a membership size (C8).

mod common;

use common::*;
use serde_json::json;
use tempfile::TempDir;

const LAYER: &str = "clusters/grown";

fn member(source_id: u64) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(external_id_of(source_id))
}

fn members(range: std::ops::Range<u64>) -> Vec<String> {
    range.map(member).collect()
}

/// How many of `range` the fixture gives term 1 to — the narrow principal's expected count.
fn narrow_count(range: std::ops::Range<u64>) -> u64 {
    range.filter(|s| terms_of(*s).contains(&1)).count() as u64
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

/// Reopen the same bundle and the same WAL. The old server is dropped first so its executor
/// releases the log.
async fn restart(server: TestServer, tmp: &TempDir) -> TestServer {
    drop(server);
    open(tmp).await
}

async fn register(server: &TestServer) {
    let resp = server
        .client
        .put(server.control_url("/control/layers"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({
            "name": LAYER,
            "title": LAYER,
            "views": ["s0"],
            "membership": "enumerated",
            "value_set": "closed",
            "visibility": null,
            "artifact_visibility": { "field": null, "default": "inherited" },
            "require_member_visibility": null,
            "hierarchy": { "kind": "flat", "prune_children": false },
            "content": { "computed": [], "supplied": [] },
            "depends_on": [],
            "levels": []
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201, "the layer registers");
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
    (status, resp.json().await.unwrap_or(serde_json::Value::Null))
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

async fn wait_until(
    server: &TestServer,
    what: &str,
    done: impl Fn(&tessera_engine::ExecutorStats) -> bool,
) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        if done(&server.state.engine.write_executor_stats()) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{what}: never happened"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// Flush, then fold. **One point is ingested first**: a flush with nothing buffered publishes
/// nothing, so the row is what gives the flush an extent to write and the fold something to fold
/// into the base.
async fn flush_and_fold(server: &TestServer) {
    let ingested = external_id_of(9_001);
    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "grow-fold")
        .header("content-type", "application/octet-stream")
        .body(build_ingest_batch_optional(&[(
            Some(&ingested[..]),
            10.0,
            10.0,
            "0",
        )]))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "{}",
        resp.text().await.unwrap()
    );
    let before = server.state.engine.write_executor_stats();
    let resp = server
        .client
        .post(server.control_url("/control/flush"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 202);
    wait_until(server, "the flush published", move |now| {
        now.flushes > before.flushes
    })
    .await;
    let before = server.state.engine.write_executor_stats();
    let resp = server
        .client
        .post(server.control_url("/control/compact"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 202);
    wait_until(server, "the fold published", move |now| {
        assert_eq!(
            now.fold_failures, before.fold_failures,
            "the fold was discarded rather than published"
        );
        now.folds > before.folds
    })
    .await;
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
    register(&server).await;
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
    register(&server).await;
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
    assert!(
        body.to_string().contains("never-published"),
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
    register(&server).await;
    publish(&server, "a", members(0..10)).await;

    change(
        &server,
        json!({ "external_id": member(15), "op": "delete" }),
    )
    .await;
    let (status, body) = grow(&server, json!([{ "key": "a", "members": members(10..20) }])).await;
    assert_eq!(status, 422, "{body}");
    let detail = body["detail"].as_str().expect("the envelope's detail");
    assert!(detail.contains("deleted"), "{detail}");
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
    register(&server).await;
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
    register(&server).await;
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

    flush_and_fold(&server).await;
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
    register(&server).await;

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

/// The body is keys and members. Content, lineage, a shape, an attachment and a missing key are
/// each refused at decoding, so a growth cannot carry what a publication carries.
#[tokio::test]
async fn a_growth_body_carries_keys_and_members_and_nothing_else() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    register(&server).await;
    publish(&server, "a", members(0..10)).await;

    for extra in [
        json!({ "content": [{ "values": ["x"] }] }),
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
    let (status, body) = grow(&server, json!([{ "members": members(10..20) }])).await;
    assert_eq!(status, 422, "a growth without a key: {body}");
    let (status, body) = grow(&server, json!([])).await;
    assert_eq!(status, 422, "a growth naming nothing: {body}");
    let (status, body) = grow_raw(
        &server,
        json!({
            "addressing": "external",
            "default_space": "wgs84",
            "artifacts": [{ "key": "a", "members": members(10..20) }]
        }),
    )
    .await;
    assert_eq!(status, 422, "a batch field a growth has no use for: {body}");

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
    assert!(
        body.to_string()
            .contains("id 2 of artifact 0 names nothing this deployment holds"),
        "{body}"
    );
    assert_eq!(count(&server, &["0"]).await, 10);
}

/// The verb sits under the control plane's credential gate like every other route.
#[tokio::test]
async fn a_growth_requires_the_operator_credential() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    let resp = server
        .client
        .patch(artifacts_url(&server))
        .json(&json!({ "addressing": "external", "artifacts": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);
    let resp = server
        .client
        .patch(artifacts_url(&server))
        .bearer_auth("not-the-operator-credential")
        .json(&json!({ "addressing": "external", "artifacts": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);
}
