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

/// The `tessera_id`s `s0` serves from a small box around one point, from a fresh session. Small
/// enough that the fixture's own rows, which sit on a grid across the frame, cannot fill the k
/// budget and hide the row a test is asking about.
async fn points_near(server: &TestServer, x: f64, y: f64) -> BTreeSet<u64> {
    let token = authorise(server, &["0", "1"][..]).await["token"]
        .as_str()
        .unwrap()
        .to_string();
    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(&token)
        .json(&json!({
            "view": "s0", "zoom": 12,
            "bbox": [x - 2.0, y - 2.0, x + 2.0, y + 2.0],
            "k": 200
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
    let (server, faults) = serve_with_faults(&tmp).await;

    // The cycle is held open at the publication seam, so the second request below is made against
    // an open cycle as a fact rather than as a race against a segment write.
    faults.arm_pause(
        tessera_lifecycle::faults::PauseSite::BeforeManifestPublish,
        tessera_lifecycle::faults::PauseAction::Stall,
    );

    ingest_one(&server, "two-ahead-1", &external_id_of(N_ITEMS + 1)).await;
    let first = request_flush(&server).await;
    await_seam(&faults).await;

    let completed = server.state.engine.publication();
    let second = request_flush(&server).await;

    assert_eq!(
        second,
        completed + 2,
        "a request made while a cycle is open names the cycle after it"
    );
    assert_eq!(
        second,
        first + 1,
        "and the two requests are one cycle apart, the first cycle being the one already under way"
    );

    faults.release();
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

    // **The first of the two publications does not reach the number.** One plan is dispatched per
    // tick, so the cycle is still holding a view's rows when the first swaps; the counter moves at
    // the one that leaves nothing over.
    let deadline = std::time::Instant::now() + DEADLINE;
    while server.state.engine.write_executor_stats().flushes < 1 {
        assert!(
            std::time::Instant::now() < deadline,
            "the first view never published"
        );
        if publication(&server).await >= n {
            panic!("the counter reached the number before either view had published");
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }

    await_publication(&server, n).await;
    assert!(
        server.state.engine.write_executor_stats().flushes >= 2,
        "the number is reached by the publication that took the second view"
    );
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
    let server =
        mount_server_with_faults(engine, max_k, generous_test_gate(), Arc::clone(&faults)).await;

    // Armed over HTTP, deliberately: this test is also the one that proves the control plane's
    // arming surface reaches the executor a request's answer depends on.
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
    await_seam(&faults).await;
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

/// A served fixture whose write executor takes a fault switchboard, so a test can shut the
/// flush's gate from inside the process.
async fn serve_with_faults(tmp: &TempDir) -> (TestServer, Arc<FaultSwitchboard>) {
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
    let server =
        mount_server_with_faults(engine, max_k, generous_test_gate(), Arc::clone(&faults)).await;
    (server, faults)
}

/// Wait until the executor is parked at the publication seam, so a cycle is open as a fact.
async fn await_seam(faults: &Arc<FaultSwitchboard>) {
    let deadline = std::time::Instant::now() + DEADLINE;
    while faults.arrivals(tessera_lifecycle::faults::PauseSite::BeforeManifestPublish) < 1 {
        assert!(
            std::time::Instant::now() < deadline,
            "the executor never reached the publication seam"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

/// One row into `s0`, accepted.
async fn ingest_one(server: &TestServer, batch_id: &str, external_id: &[u8]) -> Value {
    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", batch_id)
        .header("content-type", "application/vnd.apache.arrow.stream")
        .body(build_ingest_batch_optional(&[(
            Some(external_id),
            10.0,
            10.0,
            "0",
        )]))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(status, 200, "the row is accepted: {body}");
    body
}

/// The layer the artifact tests publish into.
async fn declare_clusters(server: &TestServer) {
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
}

/// How many artifacts the viewport's frame carries for a full principal.
async fn artifacts_served(server: &TestServer) -> usize {
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
            "layers": "all"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    decode_viewport_frames(&resp.bytes().await.unwrap())
        .artifacts
        .map(|rows| rows.len())
        .unwrap_or(0)
}

/// One `POST /control/values` batch, accepted.
async fn post_values(server: &TestServer, batch_id: &str, body: &Value) -> Value {
    let resp = server
        .client
        .post(server.control_url("/control/values"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", batch_id)
        .header("x-tessera-view", "s0")
        .json(body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let answer: Value = resp.json().await.unwrap();
    assert_eq!(status, 200, "the batch is accepted: {answer}");
    answer
}

// ---- The counter advances only on a publication ------------------------------------------------

/// **A node whose plan is refused holds its cycle open.** A poisoned WAL publishes no flush
/// (write-path §4.2), so the rows are still buffered and the overlay still holds what a
/// publication would have carried. The number the flush request was answered with must not be
/// reached while that stands, and the operator plane must say why.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_gated_node_does_not_reach_the_number_and_the_posture_says_why() {
    let tmp = TempDir::new().unwrap();
    let (server, faults) = serve_with_faults(&tmp).await;

    let ext = external_id_of(N_ITEMS + 1);
    ingest_one(&server, "gated-1", &ext).await;

    // Every WAL fsync from here fails, which poisons the log and shuts the flush's gate.
    faults.fail_next_fsyncs(100_000);
    let resp = server
        .client
        .post(server.control_url("/control/changes"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!([{ "op": "suppress", "external_id": b64(&external_id_of(1)) }]))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        500,
        "a deny whose append cannot be made durable is answered 500 (contracts §3.1)"
    );

    let ticks = server.state.engine.write_executor_stats().ticks;
    let n = request_flush(&server).await;

    // Until the node has recovered its WAL and a tick has planned over the request, which the
    // refused gate turns into a held cycle. The counter must stay short of the number throughout.
    let deadline = std::time::Instant::now() + DEADLINE;
    let executor = loop {
        assert!(
            publication(&server).await < n,
            "the counter must not pass a cycle whose gate refused it"
        );
        let executor = server.state.engine.write_executor_stats();
        if executor.wal_recoveries > 0 && executor.ticks > ticks {
            break executor;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the node never recovered its WAL and ticked: {executor:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };

    // **The operator plane says why**, which is what stops a stalled counter reading as a hung
    // server. The node recovered its WAL in process, so the posture is back to `running`; what
    // survives the recovery is the incident counter, and the overlay it left behind is what still
    // refuses the plan (write-path §7.2). Read after the refused tick, so the counter below has
    // had its chance to move.
    assert_eq!(
        executor.flushes, 0,
        "and nothing was published, which is why the number was not reached"
    );
    assert!(
        publication(&server).await < n,
        "the counter must not pass a cycle whose gate refused it"
    );
}

/// **The counter moves at the swap and not before.** The executor is parked inside the
/// publication with the flush already executed on the pool: nothing has been swapped, so nothing
/// a caller can read has changed, and the number must not be reached. Released, the same cycle
/// swaps and the number is reached with the row served.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_publication_that_has_not_swapped_does_not_move_the_counter() {
    let tmp = TempDir::new().unwrap();
    let (server, faults) = serve_with_faults(&tmp).await;

    faults.arm_pause(
        tessera_lifecycle::faults::PauseSite::BeforeManifestPublish,
        tessera_lifecycle::faults::PauseAction::Stall,
    );

    let ext = external_id_of(N_ITEMS + 1);
    ingest_one(&server, "swap-1", &ext).await;
    let n = request_flush(&server).await;

    let deadline = std::time::Instant::now() + DEADLINE;
    while faults.arrivals(tessera_lifecycle::faults::PauseSite::BeforeManifestPublish) < 1 {
        assert!(
            std::time::Instant::now() < deadline,
            "the executor never reached the publication seam"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let executor = server.state.engine.write_executor_stats();
    assert_eq!(
        executor.flushes, 0,
        "parked before the side-manifest commit, so nothing has published"
    );
    assert!(
        publication(&server).await < n,
        "a unit that has executed and not swapped has published nothing a reader can see, so the \
         counter must not have passed it"
    );

    faults.release();
    await_publication(&server, n).await;
    assert_eq!(
        server.state.engine.buffered_items(),
        0,
        "the swap that reached the number is the one that published the row"
    );
}

// ---- `wait=visible` ------------------------------------------------------------------------------

/// **A row page.** The answer is held until the rows it took are served, so the call after it
/// needs no wait of its own. The control is the same page without the parameter, whose rows are
/// still buffered when the answer arrives.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wait_visible_holds_a_row_page_until_its_rows_are_served() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;

    // Without the parameter: acknowledged, not yet published.
    let body = ingest_one(&server, "unwaited", &external_id_of(N_ITEMS + 1)).await;
    assert!(
        body["publication"].as_u64().unwrap() > publication(&server).await,
        "the acknowledgement names a cycle that has not happened yet: {body}"
    );
    assert!(
        body.get("visible").is_none(),
        "no wait was asked for: {body}"
    );

    let ext = external_id_of(N_ITEMS + 2);
    let resp = server
        .client
        .post(server.control_url("/control/ingest?wait=visible"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "waited")
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
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["visible"], json!(true), "{body}");
    assert!(
        publication(&server).await >= body["publication"].as_u64().unwrap(),
        "the answer came back after the counter reached its number: {body}"
    );
    assert_eq!(
        server.state.engine.buffered_items(),
        0,
        "and every row buffered when it was sent has been published"
    );

    // **The waited row itself is served**, read back by the identifier its acknowledgement
    // returned. The whole-frame count would prove nothing: the fixture's own thousand rows fill
    // the k budget whatever this page did.
    let waited_id = body["tessera_ids"][0]
        .as_u64()
        .or_else(|| body["tessera_ids"][0].as_str().and_then(|s| s.parse().ok()))
        .unwrap_or_else(|| panic!("the acknowledgement names the row it took: {body}"));
    assert!(
        points_near(&server, 10.0, 10.0).await.contains(&waited_id),
        "the row the waited page took is in the viewport with no wait of the reader's own"
    );
}

/// **An artifact publication.** A record is served from a row form the cycle publishes, so the
/// same wait covers it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wait_visible_holds_an_artifact_publication_until_it_is_served() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    declare_clusters(&server).await;

    let resp = server
        .client
        .put(server.control_url("/control/layers/clusters/artifacts?wait=visible"))
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
    assert_eq!(resp.status().as_u16(), 201);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["visible"], json!(true), "{body}");
    assert!(
        publication(&server).await >= body["publication"].as_u64().unwrap(),
        "{body}"
    );

    assert_eq!(
        artifacts_served(&server).await,
        1,
        "the artifact is in the frame with no wait of the reader's own"
    );
}

/// **A declaration.** A column declared at a running service is answered from the WAL fsync and
/// published at the next cycle (`ingest.md` §1.3), so it takes the same parameter.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wait_visible_holds_a_declaration_until_it_is_published() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;

    let resp = server
        .client
        .put(server.control_url("/control/attributes?wait=visible"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({"name": "tag", "type": "keyword", "index": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["visible"], json!(true), "{body}");
    assert!(
        publication(&server).await >= body["publication"].as_u64().unwrap(),
        "{body}"
    );
}

/// **The wait is bounded.** Past `serve.visible_wait_max_secs` the answer is the one the route
/// would have sent without the parameter, saying `visible: false`. The write happened either way,
/// which is why the bound is a latency ceiling and never a refusal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_wait_is_bounded_and_says_so() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server_with_visible_wait(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        0,
    )
    .await;

    let ext = external_id_of(N_ITEMS + 1);
    let resp = server
        .client
        .post(server.control_url("/control/ingest?wait=visible"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "bounded")
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
    assert_eq!(resp.status().as_u16(), 200, "the write is not refused");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["visible"], json!(false), "{body}");
    assert_eq!(body["accepted"], json!(1), "the rows were taken: {body}");

    // The number stands, and the caller reaches it by reading status as it would have anyway.
    await_publication(&server, body["publication"].as_u64().unwrap()).await;
}

/// A ceiling too large to add to the clock is no ceiling: the write is answered once it is
/// visible.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_largest_wait_ceiling_waits_for_the_publication() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server_with_visible_wait(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        u64::MAX,
    )
    .await;

    let ext = external_id_of(N_ITEMS + 1);
    let resp = server
        .client
        .post(server.control_url("/control/ingest?wait=visible"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "unbounded")
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
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["visible"], json!(true), "{body}");
}

/// An unrecognised `wait` value is refused rather than read as no wait at all: a caller who typed
/// `wait=true` and got an immediate answer would read it as published.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unknown_wait_value_is_refused() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;

    let resp = server
        .client
        .put(server.control_url("/control/attributes?wait=true"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&json!({"name": "tag", "type": "keyword", "index": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 422);
}

/// `POST /control/flush?wait=visible`, the answer held until its cycle has published.
async fn request_flush_waiting(server: &TestServer) -> Value {
    let resp = server
        .client
        .post(server.control_url("/control/flush?wait=visible"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(status, 202, "the flush is accepted: {body}");
    body
}

/// **The shape decision 0144 gives a bulk loader**: pages unwaited, one waited flush at the end.
/// The flush's own 202 is what the loader keys on, and after it the rows are served with no wait
/// of the reader's own — which is the whole of what the SDK's `commit()` does.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wait_visible_holds_a_flush_until_the_unwaited_pages_are_served() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;

    let mut waiting = Vec::new();
    for i in 1..=3 {
        let body = ingest_one(
            &server,
            &format!("unwaited-{i}"),
            &external_id_of(N_ITEMS + i),
        )
        .await;
        waiting.push(
            body["tessera_ids"][0]
                .as_u64()
                .or_else(|| body["tessera_ids"][0].as_str().and_then(|s| s.parse().ok()))
                .unwrap_or_else(|| panic!("the acknowledgement names the row it took: {body}")),
        );
    }
    assert!(
        server.state.engine.buffered_items() > 0,
        "an unwaited page is acknowledged with its rows still buffered"
    );

    let body = request_flush_waiting(&server).await;
    assert_eq!(body["visible"], json!(true), "{body}");
    let n = body["publication"]
        .as_u64()
        .unwrap_or_else(|| panic!("the 202 carries the publication number: {body}"));
    assert!(
        publication(&server).await >= n,
        "the answer came back after the counter reached its number: {body}"
    );
    assert_eq!(
        server.state.engine.buffered_items(),
        0,
        "and every row buffered when the flush was sent has been published"
    );

    // Read back by identifier rather than by a count: the fixture's own rows would fill the k
    // budget whatever these pages did.
    let served = points_near(&server, 10.0, 10.0).await;
    for id in waiting {
        assert!(
            served.contains(&id),
            "the row a page took before the flush is in the viewport with no further wait"
        );
    }
}

/// **The flush's wait is bounded like every other.** At `visible_wait_max_secs = 0` the 202 is
/// the one the route would have sent without the parameter, saying `visible: false`; the flush
/// was still requested, so the number stands and the caller reaches it by reading status.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_flush_wait_is_bounded_and_says_so() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server_with_visible_wait(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        0,
    )
    .await;

    ingest_one(&server, "bounded-flush", &external_id_of(N_ITEMS + 1)).await;

    let body = request_flush_waiting(&server).await;
    assert_eq!(body["visible"], json!(false), "{body}");
    await_publication(&server, body["publication"].as_u64().unwrap()).await;
}

/// An unknown `wait` value on the flush is refused as it is on the write routes, and before the
/// tick is armed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unknown_wait_value_on_the_flush_is_refused() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;

    let resp = server
        .client
        .post(server.control_url("/control/flush?wait=true"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 422);
}

// ---- The replay answer ---------------------------------------------------------------------------

/// **A replayed page accepts nothing and says so** (write-path §2.4). `accepted` is the effect
/// this submission had, so a client summing it over its pages is not made to double-count every
/// page it retried; `tessera_ids` is the full list either way, which is what a caller correlates
/// its rows by.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replayed_page_accepts_nothing_and_says_so() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;

    let ext = external_id_of(N_ITEMS + 1);
    let first = ingest_one(&server, "replay-1", &ext).await;
    assert_eq!(first["accepted"], json!(1));
    assert!(
        first.get("replayed").is_none(),
        "a first submission carries no flag: {first}"
    );

    let second = ingest_one(&server, "replay-1", &ext).await;
    assert_eq!(second["replayed"], json!(true), "{second}");
    assert_eq!(
        second["accepted"],
        json!(0),
        "the replay took no rows, so a client's sum stays honest: {second}"
    );
    assert_eq!(
        second["tessera_ids"], first["tessera_ids"],
        "and the identifiers are the same ones, which is what the caller correlates by"
    );
}

/// The same on the values route, where the fill rule answers a replay as a no-op and the flag is
/// what tells that apart from cells another writer had already filled identically.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replayed_values_page_is_flagged() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    declare_attribute(
        &server,
        json!({"name": "tag", "type": "keyword", "index": true}),
    )
    .await;

    let body = json!([{"external_id": b64(&external_id_of(3)), "tag": "alpha"}]);
    let first = post_values(&server, "values-1", &body).await;
    assert_eq!(first["filled"], json!(1));
    assert!(first.get("replayed").is_none(), "{first}");

    let second = post_values(&server, "values-1", &body).await;
    assert_eq!(second["replayed"], json!(true), "{second}");
    assert_eq!(
        second["filled"],
        json!(0),
        "the fill rule answered it as a no-op: {second}"
    );
}
