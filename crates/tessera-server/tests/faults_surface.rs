//! The faults build's arming surface, over real HTTP — decision 0071's deliverable, exercised
//! the way the correctness suite's driver will use it (correctness-suite §12.3).
//!
//! This binary runs against the faults build unconditionally: the crate's self dev-dependency
//! enables `fault-injection` for every integration test, exactly as `tessera-lifecycle`'s and
//! `tessera-engine`'s do. What it proves is the whole reachability chain the decision ruled on —
//! a *booted server* whose `/control/faults/*` routes arm the same switchboard the write
//! executor consults — not just the switchboard's own semantics, which its home crate's tests
//! already pin.
//!
//! The one seam driven here is the flush's side-manifest commit, because it is the seam a driver
//! reaches with nothing but the five control routes: ingest, pull the tick, watch the executor
//! park, release, watch the publication land. The other seams are exercised at the engine layer
//! (`tessera-engine/tests/seam_pause.rs`, `tests/merge.rs`), where a fold and a merge can be
//! provoked without a server-sized fixture.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use tessera_engine::Engine;
use tessera_lifecycle::faults::FaultSwitchboard;
use tessera_plugin::Passthrough;

/// The wire analogue of `FaultSwitchboard::arrivals`, polled: how many times the executor has
/// reached `site` since it was armed, read over the control plane.
async fn arrivals(server: &TestServer, site: &str) -> u64 {
    let resp = server
        .client
        .get(server.control_url(&format!("/control/faults/arrivals?site={site}")))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let json: serde_json::Value = resp.json().await.unwrap();
    json["arrivals"].as_u64().unwrap()
}

async fn flushes_published(server: &TestServer) -> u64 {
    control_status(server).await["write_executor"]["flush"]["flushes"]
        .as_u64()
        .unwrap()
}

/// Arm over HTTP, ingest, pull the tick, observe the executor demonstrably parked at the
/// manifest-publish seam — flush executed on the pool, nothing published — then release over
/// HTTP and observe the publication land. Arrive, block, proceed: the pause-site contract, held
/// end to end from the control plane.
#[tokio::test]
async fn the_manifest_seam_pauses_and_releases_over_the_control_plane() {
    let tmp = tempfile::TempDir::new().unwrap();
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

    // Arm the seam before anything is in flight.
    let resp = server
        .client
        .post(server.control_url("/control/faults/arm"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&serde_json::json!({ "site": "before_manifest_publish" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Ingest still acks: the seam sits at publication, not on the WAL path a 200 depends on.
    let ext = external_id_of(N_ITEMS + 1);
    let body = build_ingest_batch_optional(&[(Some(&ext[..]), 10.0, 10.0, "0")]);
    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "faults-surface-1")
        .header("content-type", "application/vnd.apache.arrow.stream")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Pull the tick; the flush executes on the pool and its publication runs into the armed site.
    let resp = server
        .client
        .post(server.control_url("/control/flush"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);

    // The executor arrives: a thread is demonstrably parked, which is the driver's kill
    // precondition. Observed by polling the server's own state rather than sleeping a guessed
    // duration, and bounded so a genuine hang fails rather than wedging CI.
    wait_until(
        "the executor reaching before_manifest_publish",
        Duration::from_secs(30),
        async || arrivals(&server, "before_manifest_publish").await >= 1,
    )
    .await;

    // Parked means *blocked*: the flush has executed, and nothing has been published.
    assert_eq!(
        flushes_published(&server).await,
        0,
        "the executor is parked before the side-manifest commit, so no flush may have published"
    );

    // Release over the wire; the parked thread proceeds and the publication lands.
    let resp = server
        .client
        .post(server.control_url("/control/faults/release"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    wait_until(
        "the released flush publishing",
        Duration::from_secs(30),
        async || flushes_published(&server).await >= 1,
    )
    .await;
}

/// The surface is on the credential-gated plane (401 without the bearer, from the router layer
/// no route can opt out of) and refuses an unknown site with the contract's 422 rather than
/// arming nothing silently.
#[tokio::test]
async fn the_arming_surface_is_gated_and_names_its_sites() {
    let tmp = tempfile::TempDir::new().unwrap();
    let server = serve(&tmp).await;

    let resp = server
        .client
        .post(server.control_url("/control/faults/arm"))
        .json(&serde_json::json!({ "site": "before_current_flip" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401, "no bearer, no arming");

    let resp = server
        .client
        .post(server.control_url("/control/faults/arm"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&serde_json::json!({ "site": "before_the_horse" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 422, "an unknown site is a refusal, never a no-op");
}

/// An executor that panics reports itself dead: `/readyz` answers 503 on both listeners that
/// carry it, and `/control/status` names the posture.
#[tokio::test]
async fn an_executor_panic_fails_readiness_and_reports_dead() {
    let tmp = tempfile::TempDir::new().unwrap();
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

    let ready = server
        .client
        .get(server.viewer_url("/readyz"))
        .send()
        .await
        .unwrap();
    assert_eq!(ready.status(), 200);

    server.state.faults.arm_pause(
        tessera_lifecycle::faults::PauseSite::BeforeManifestPublish,
        tessera_lifecycle::faults::PauseAction::Panic,
    );
    let ext = external_id_of(N_ITEMS + 1);
    let body = build_ingest_batch_optional(&[(Some(&ext[..]), 10.0, 10.0, "0")]);
    let resp = server
        .client
        .post(server.control_url("/control/ingest"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .header("x-tessera-batch-id", "faults-surface-panic")
        .header("content-type", "application/vnd.apache.arrow.stream")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let resp = server
        .client
        .post(server.control_url("/control/flush"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);

    wait_until(
        "/readyz answering 503 after the executor panicked",
        Duration::from_secs(30),
        async || {
            let resp = server.client.get(server.viewer_url("/readyz")).send().await;
            resp.unwrap().status() == 503
        },
    )
    .await;
    let session_ready = server
        .client
        .get(server.session_url("/readyz"))
        .send()
        .await
        .unwrap();
    assert_eq!(session_ready.status(), 503);
    let status = control_status(&server).await;
    assert_eq!(status["write_executor"]["posture"], "dead");
    assert_eq!(status["write_executor"]["ready"], false);
}
