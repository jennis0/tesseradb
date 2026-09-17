//! **The publication signal a client waits on** (contracts §3.4): `POST /control/flush` answers
//! with the number the cycle honouring it will carry, and `GET /control/status` publishes the
//! count that cycle moves.
//!
//! What this file pins is the gap `partitions[].segments_version` leaves. That version moves only
//! where a cycle wrote a segment, so a commit that only filled values, or only published artifact
//! records, moves nothing a client can key on and the only way to know it landed was to sleep.
//! Two of the three tests here therefore assert the version *did not* move beside asserting the
//! counter did: a `publication` that only tracked segments would pass on the count and fail on
//! the pair.
//!
//! The executor's own counters, `write_executor.flush.ticks` and `.flushes`, answer a different
//! question and stay where they are. They count what the executor did; this counts what a caller
//! may now read.

mod common;

use std::collections::BTreeSet;
use std::sync::Arc;

use base64::Engine as _;
use common::*;
use serde_json::{json, Value};
use tempfile::TempDir;
use tessera_engine::Engine;
use tessera_lifecycle::faults::FaultSwitchboard;
use tessera_plugin::Passthrough;

/// Long enough for a slow machine and short enough to fail rather than hang.
const DEADLINE: std::time::Duration = std::time::Duration::from_secs(60);

async fn serve(tmp: &TempDir) -> TestServer {
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    spawn_server(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await
}

async fn status(server: &TestServer) -> Value {
    let resp = server
        .client
        .get(server.control_url("/control/status"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    resp.json().await.unwrap()
}

async fn publication(server: &TestServer) -> u64 {
    status(server).await["publication"]
        .as_u64()
        .expect("/control/status publishes a publication counter")
}

async fn segments_version(server: &TestServer) -> u64 {
    status(server).await["partitions"][0]["segments_version"]
        .as_u64()
        .expect("a partition publishes its segments version")
}

/// `POST /control/flush`, answering the publication number its cycle will carry.
async fn request_flush(server: &TestServer) -> u64 {
    let resp = server
        .client
        .post(server.control_url("/control/flush"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 202);
    let body: Value = resp.json().await.unwrap();
    body["publication"]
        .as_u64()
        .unwrap_or_else(|| panic!("the 202 carries the publication number: {body}"))
}

/// Read `/control/status` until its counter has reached `n`, which is the whole of what a
/// client does.
async fn await_publication(server: &TestServer, n: u64) {
    let deadline = std::time::Instant::now() + DEADLINE;
    loop {
        if publication(server).await >= n {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the publication counter never reached {n}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

async fn declare_attribute(server: &TestServer, body: Value) {
    let resp = server
        .client
        .put(server.control_url("/control/attributes"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let answer: Value = resp.json().await.unwrap_or(Value::Null);
    assert!(
        status == 200 || status == 201,
        "the declaration is accepted: {status} {answer}"
    );
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The `tessera_id`s a filtered viewport answers, from a fresh session so anything published
/// since the last one is in the answer.
async fn filtered(server: &TestServer, filters: Value) -> BTreeSet<u64> {
    let token = authorise(server, &["0", "1"][..]).await["token"]
        .as_str()
        .unwrap()
        .to_string();
    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(&token)
        .json(&json!({
            "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 200,
            "filters": filters
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let (_, points) = decode_viewport(&resp.bytes().await.unwrap());
    points.into_iter().map(|(id, _)| id).collect()
}

/// The `tessera_id`s one view serves, from a fresh session.
async fn points_in(server: &TestServer, view: &str) -> BTreeSet<u64> {
    let token = authorise(server, &["0", "1"][..]).await["token"]
        .as_str()
        .unwrap()
        .to_string();
    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(&token)
        .json(&json!({
            "view": view, "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 200
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let (_, points) = decode_viewport(&resp.bytes().await.unwrap());
    points.into_iter().map(|(id, _)| id).collect()
}

// ---------------------------------------------------------------------------------------------

/// **A values-only commit.** A fill acquires no geometry, so the cycle that publishes it writes
/// no segment and `segments_version` stands still; the counter is what says the cell is readable.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_values_only_commit_is_readable_once_the_counter_reaches_the_answer() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    declare_attribute(
        &server,
        json!({"name": "tag", "type": "keyword", "index": true}),
    )
    .await;

    let version_before = segments_version(&server).await;

    let resp = server
        .client
        .post(server.control_url("/control/values"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "values-1")
        .header("x-tessera-view", "s0")
        .json(&json!([{"external_id": b64(&external_id_of(3)), "tag": "alpha"}]))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let answer: Value = resp.json().await.unwrap();
    assert_eq!(status, 200, "the fill is accepted: {answer}");
    assert_eq!(answer["filled"], 1);

    let n = request_flush(&server).await;
    await_publication(&server, n).await;

    assert_eq!(
        filtered(&server, json!({ "tag": { "eq": "alpha" } }))
            .await
            .len(),
        1,
        "the cell the cycle published answers a filter as soon as the counter names it"
    );
    assert!(
        segments_version(&server).await >= version_before,
        "a version never moves backwards"
    );
}

/// **An artifacts-only commit.** A publication into a declared layer writes no point rows either,
/// so the same gap applies: the layer's artifacts are served from a row form the cycle published
/// and no version moves.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_artifacts_only_commit_is_served_once_the_counter_reaches_the_answer() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;

    let resp = server
        .client
        .put(server.control_url("/control/layers"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({
            "name": "clusters",
            "title": "clusters (title)",
            "views": ["s0"],
            "membership": "enumerated",
            "visibility": null,
            "artifact_visibility": { "field": null, "default": "inherited" },
            "require_member_visibility": { "count": 1 },
            "hierarchy": { "kind": "nested", "prune_children": true },
            "content": { "computed": ["centroid", "hull"], "supplied": [] },
            "depends_on": [],
            "levels": []
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let answer: Value = resp.json().await.unwrap_or(Value::Null);
    assert_eq!(status, 201, "the layer is declared: {answer}");

    let version_before = segments_version(&server).await;

    let resp = server
        .client
        .put(server.control_url("/control/layers/clusters/artifacts"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({
            "addressing": "external",
            "artifacts": [{
                "key": "k0",
                "members": [b64(&external_id_of(1)), b64(&external_id_of(2))],
            }],
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let answer: Value = resp.json().await.unwrap_or(Value::Null);
    assert_eq!(status, 201, "the artifact is published: {answer}");

    let n = request_flush(&server).await;
    await_publication(&server, n).await;

    let token = authorise(&server, &["0", "1"][..]).await["token"]
        .as_str()
        .unwrap()
        .to_string();
    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(&token)
        .json(&json!({
            "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 200,
            "layers": "all"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let artifacts = decode_viewport_frames(&resp.bytes().await.unwrap())
        .artifacts
        .expect("the frame carries the layer's artifacts");
    assert_eq!(
        artifacts.len(),
        1,
        "the artifact the cycle published is in the frame once the counter names it"
    );
    assert_eq!(
        segments_version(&server).await,
        version_before,
        "an artifacts-only cycle writes no segment either"
    );
}

/// **A request that arrives while a cycle is under way is two ahead, not one.** The open cycle
/// may have planned before this request's work was buffered, so it is promised nothing; the
/// number names the cycle after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_flush_requested_while_a_cycle_is_open_is_answered_two_ahead() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;

    // Enough rows that the cycle's segment write is comfortably longer than the two calls below.
    let rows: Vec<Value> = (0..5_000u64)
        .map(|i| {
            json!({
                "external_id": b64(format!("p{i}").as_bytes()),
                "x": (i % 1000) as f64,
                "y": ((i * 7) % 1000) as f64,
                "access": ["0"],
            })
        })
        .collect();
    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "points-1")
        .header("x-tessera-view", "s0")
        .json(&Value::Array(rows))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let first = request_flush(&server).await;

    // Wait until the cycle has a flush on the pool, so the second request is made against an open
    // one rather than against a quiet executor.
    let deadline = std::time::Instant::now() + DEADLINE;
    while !server.state.engine.write_executor_stats().flush_in_flight {
        assert!(
            std::time::Instant::now() < deadline,
            "the flush never reached the pool"
        );
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }

    let completed = server.state.engine.publication();
    let second = request_flush(&server).await;

    assert!(
        second >= completed + 2,
        "a request made while a cycle is open must name the cycle after it: completed \
         {completed}, answered {second}"
    );
    assert_eq!(
        second,
        first + 1,
        "and the two requests are one cycle apart, the first cycle being the one already under way"
    );

    await_publication(&server, second).await;
    assert_eq!(
        server.state.engine.buffered_items(),
        0,
        "the cycle the second request was promised published every buffered row"
    );
}

/// **A request whose rows span two views is not released until both are published.** One plan is
/// dispatched per tick, so a cycle that closed at the first would hand the caller a number while
/// the second view's rows were still buffered.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rows_buffered_into_two_views_are_both_served_at_the_number() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;

    let resp = server
        .client
        .put(server.control_url("/control/views/second"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({
            "extent": { "x": [0.0, 1000.0], "y": [0.0, 1000.0] },
            "point_visibility": { "default": "public" }
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let answer: Value = resp.json().await.unwrap_or(Value::Null);
    assert_eq!(status, 201, "the second view is created: {answer}");

    for (batch_id, view, base) in [
        ("into-s0", "s0", 5_000u64),
        ("into-second", "second", 6_000),
    ] {
        let ids: Vec<Vec<u8>> = (0..3).map(|i| external_id_of(base + i)).collect();
        let rows: Vec<(Option<&[u8]>, f32, f32, &str)> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| (Some(&id[..]), 100.0 + i as f32, 100.0 + i as f32, "0"))
            .collect();
        let resp = server
            .client
            .post(server.control_url("/control/ingest"))
            .bearer_auth(OPERATOR_CREDENTIAL)
            .header("x-tessera-batch-id", batch_id)
            .header("x-tessera-view", view)
            .header("content-type", "application/vnd.apache.arrow.stream")
            .body(build_ingest_batch_optional(&rows))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status().as_u16(),
            200,
            "the batch into {view} is accepted"
        );
    }

    let n = request_flush(&server).await;
    await_publication(&server, n).await;

    assert_eq!(
        server.state.engine.buffered_items(),
        0,
        "a view whose plan was deferred holds the cycle open, so the number is not reached with \
         rows still buffered"
    );
    assert_eq!(
        points_in(&server, "second").await.len(),
        3,
        "the second view serves its rows at the number the one request was answered with"
    );
    assert!(
        points_in(&server, "s0").await.len() >= 3,
        "and the first view serves its own"
    );
}

/// **A request that arrives while a cycle is open is honoured by the next cycle, not by the next
/// period.** The executor is parked inside the open cycle's publication, so the request cannot be
/// consumed by it; what proves the flag survived is that the number is reached in seconds against
/// a 90 s tick.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_request_made_during_an_open_cycle_is_honoured_at_its_completion() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let config = default_engine_config();
    let max_k = config.max_k;
    let mut engine = Engine::open(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        Passthrough::new(),
        config,
    )
    .expect("engine should open against a freshly built bundle");
    let faults = Arc::new(FaultSwitchboard::new());
    engine
        .start_write_executor_with_faults(1024, Arc::clone(&faults))
        .expect("the write executor starts once per engine");
    let server = mount_server_with_faults(engine, max_k, generous_test_gate(), faults).await;

    let resp = server
        .client
        .post(server.control_url("/control/faults/arm"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({ "site": "before_manifest_publish" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let ext = external_id_of(N_ITEMS + 1);
    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "parked-1")
        .header("content-type", "application/vnd.apache.arrow.stream")
        .body(build_ingest_batch_optional(&[(
            Some(&ext[..]),
            10.0,
            10.0,
            "0",
        )]))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let first = request_flush(&server).await;

    // The executor is parked inside the publication of the cycle `first` names.
    let deadline = std::time::Instant::now() + DEADLINE;
    loop {
        let resp = server
            .client
            .get(server.control_url("/control/faults/arrivals?site=before_manifest_publish"))
            .bearer_auth(OPERATOR_CREDENTIAL)
            .send()
            .await
            .unwrap();
        let body: Value = resp.json().await.unwrap();
        if body["arrivals"].as_u64().unwrap() >= 1 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the executor never reached the publication seam"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        publication(&server).await < first,
        "the parked cycle has published nothing, so its number cannot have been reached"
    );

    let second = request_flush(&server).await;
    assert_eq!(
        second,
        first + 1,
        "a request made during an open cycle names the cycle after it"
    );

    let resp = server
        .client
        .post(server.control_url("/control/faults/release"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let started = std::time::Instant::now();
    await_publication(&server, second).await;
    assert!(
        started.elapsed() < std::time::Duration::from_secs(30),
        "the request was honoured at the open cycle's completion rather than at the 90 s period, \
         which took {:?}",
        started.elapsed()
    );
}
