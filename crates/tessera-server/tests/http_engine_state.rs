//! Engine state over HTTP: pin identity and session revocation — the server-plane half of what
//! `tessera-engine/tests/pins.rs` asserts in-process.
//!
//! Split out of `tests/http.rs` at Task 0c (Phase 2 stage 2.1) so Track C owns a server-plane
//! file. Task 4 replaces `PinManager`'s equality check with a drain list, and Task 5 makes
//! `revoke` prune the projection cache; both need somewhere to land an end-to-end assertion, and
//! `http.rs` is frozen for the stage.
//!
//! Named for Track C's subject in the plan (*engine state*) rather than for pins alone: it already
//! holds a session-revocation case, and Task 5's cache-pruning assertions belong here too, so
//! `http_pins.rs` would have read wrong by the end of the stage (Task 0 gate, minor).
//!
//! The property `g2_pins_survive_overlay_swaps` guards is lifecycle §2.3 and it is the one most
//! easily broken by Task 4: **a pin fixes row-space geometry and never authorisation state**, so
//! an overlay swap must not expire it — and, in the other direction, a suppression must apply to
//! a pinned request the moment it is accepted. `PinnedGeometry` exists to make the second half
//! structural; this file is where the first half stays honest.

mod common;

use base64::Engine as _;
use tempfile::TempDir;

use common::*;

#[tokio::test]
async fn c_revoke_then_viewport_is_rejected() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;

    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap().to_string();
    let token_id = auth["token_id"].as_u64().unwrap();

    let resp = server
        .client
        .post(server.session_url("/session/revoke"))
        .bearer_auth(SESSION_CREDENTIAL)
        .json(&serde_json::json!({ "token_id": token_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);

    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "slice": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0]
        }))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status() == 403 || resp.status() == 401,
        "revoked token must be rejected as 403 or 401, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn g_stale_pin_is_410() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;
    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap();

    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "slice": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0],
            "pin": { "prefix": "v00000", "segments_version": 999 }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 410);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "pin-expired");
}

#[tokio::test]
async fn g2_pins_survive_overlay_swaps() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;
    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap();

    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "slice": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0]
        }))
        .send()
        .await
        .unwrap();
    let pin_header = resp
        .headers()
        .get("x-tessera-pin")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let pin: serde_json::Value = serde_json::from_str(&pin_header).unwrap();
    let (tiles_before, _) = decode_viewport(&resp.bytes().await.unwrap());

    const SUPPRESS_SOURCE_ID: u64 = 7;
    let external_id =
        base64::engine::general_purpose::STANDARD.encode(external_id_of(SUPPRESS_SOURCE_ID));
    let resp = server
        .client
        .post(server.control_url("/control/changes"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&serde_json::json!([{ "external_id": external_id, "op": "suppress" }]))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Re-query WITH the pin taken before the suppression: 200 (not 410 — a pin fixes geometry,
    // never authorisation), and the count reflects the suppression immediately.
    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "slice": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0],
            "pin": pin
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "a pin must survive an overlay swap");
    let (tiles_after, _) = decode_viewport(&resp.bytes().await.unwrap());
    assert_eq!(tiles_after[0].1, tiles_before[0].1 - 1);
}

// --- Task 5: cache pruning, the startup bound, and check_bearer ---

/// **End-to-end revoke pruning.** `tests/cache.rs` asserts the engine-level pruner; this asserts
/// the handler actually calls it, which is a separate failure — a pruner nothing invokes closes no
/// deferral.
///
/// The revoked session is unusable either way (the registry removal is what does that, and
/// `c_revoke_then_viewport_is_rejected` above covers it), so the observable here is memory: the
/// projection is gone from the cache.
///
/// **The observable is `row_projection_cache_stats().entries`, read through `TestServer::state`,
/// and that is the whole point of this test.** Its first version asserted a 204 and that a survivor
/// still got 200 — neither of which touches the cache — so deleting
/// `state.engine.prune_token(req.token_id)` from the revoke handler left all 38 tests in this crate
/// green while the doc above claimed the opposite (round-1 review, MX1). A test doc that asserts
/// what the test does not is worse than no test: the next worker to refactor this handler sees
/// green and reopens the Phase 1 deferral with a test standing over it.
///
/// The survivor's entry is asserted to *remain* for the symmetric reason: a `prune_token` that
/// cleared the whole cache would satisfy "the doomed entry is gone" just as well.
#[tokio::test]
async fn revoke_prunes_the_token() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;

    let doomed = authorise(&server, &["0"]).await;
    let survivor = authorise(&server, &["0"]).await;
    for auth in [&doomed, &survivor] {
        let resp = server
            .client
            .post(server.viewer_url("/v1/viewport"))
            .bearer_auth(auth["token"].as_str().unwrap())
            .json(&serde_json::json!({
                "slice": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    // Two sessions, two distinct `token_id`s, so two distinct cache keys.
    assert_eq!(
        server.state.engine.row_projection_cache_stats().entries,
        2,
        "each session's first viewport must have published its own projection"
    );

    let resp = server
        .client
        .post(server.session_url("/session/revoke"))
        .bearer_auth(SESSION_CREDENTIAL)
        .json(&serde_json::json!({ "token_id": doomed["token_id"].as_u64().unwrap() }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);

    // **The assertion the name promises.** Deleting `prune_token` from the handler fails here and
    // nowhere else in this crate.
    assert_eq!(
        server.state.engine.row_projection_cache_stats().entries,
        1,
        "the revoke handler must prune the revoked token's projections — a 204 alone says nothing \
         about whether it did"
    );

    // The survivor must still be served — a prune that dropped everything would satisfy a
    // "the doomed entry is gone" check just as well.
    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(survivor["token"].as_str().unwrap())
        .json(&serde_json::json!({
            "slice": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "revoking one session must not disturb another"
    );
}

/// **The startup refusal.** A cache bound below `expected_concurrent_sessions × per-entry` is not a
/// slow configuration, it is a collapsing one — every request pays a multi-second rebuild while
/// holding an admission permit, so the gate saturates and warm requests are shed too. `prepare`
/// refuses rather than binding a listener.
///
/// Both caches are covered, and the second half of this test is the one that matters: a validation
/// covering only the projection cache leaves the fragment bound free to be set to a collapsing
/// value (Task 0 gate, F11).
#[tokio::test]
async fn an_undersized_cache_bound_refuses_to_start() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    std::env::set_var("TESSERA_TASK5_SESSION", SESSION_CREDENTIAL);
    std::env::set_var("TESSERA_TASK5_OPERATOR", OPERATOR_CREDENTIAL);

    let write_config = |extra: &str| {
        let text = format!(
            r#"
            [bundle]
            path = "{bundle}"
            cache = "{cache}"
            wal = "{wal}"
            [plugin]
            module = "builtin:passthrough"
            [disclosure]
            min_visible_members = 10
            token_max_lifetime = 3600
            [serve]
            viewer = "127.0.0.1:0"
            session = "127.0.0.1:0"
            control = "127.0.0.1:0"
            session_credential_env = "TESSERA_TASK5_SESSION"
            operator_credential_env = "TESSERA_TASK5_OPERATOR"
            {extra}
            "#,
            bundle = bundle_root.display(),
            cache = tmp.path().join("cache").display(),
            wal = tmp.path().join("wal.log").display(),
        );
        let path = tmp.path().join(format!("tessera-{}.toml", extra.len()));
        std::fs::write(&path, text).unwrap();
        path
    };

    // Control first: the defaults satisfy the relation, so a plain config must start. Without this,
    // both refusals below could be caused by anything at all in `prepare`.
    let ok = tessera_server::prepare(&write_config("expected_concurrent_sessions = 2"));
    assert!(
        ok.is_ok(),
        "the default bounds must admit two sessions: {:?}",
        ok.err()
    );

    let projection = tessera_server::prepare(&write_config(
        "expected_concurrent_sessions = 8\nrow_projection_cache_bytes = 1000000",
    ));
    let message = format!("{:?}", projection.err().expect("must refuse to start"));
    assert!(
        message.contains("row_projection_cache_bytes"),
        "the refusal must name the key to raise, got: {message}"
    );

    let fragment = tessera_server::prepare(&write_config(
        "expected_concurrent_sessions = 8\nfragment_cache_bytes = 1000000",
    ));
    let message = format!("{:?}", fragment.err().expect("must refuse to start"));
    assert!(
        message.contains("fragment_cache_bytes"),
        "the fragment bound must be validated too, or it is free to be set to a collapsing \
         value while its sibling is checked (Task 0 gate, F11); got: {message}"
    );
}

/// `check_bearer` no longer short-circuits on a prefix or on length.
///
/// This cannot observe timing, and does not pretend to — what it pins is the *behaviour* the
/// rewrite had to preserve while removing the early exit: a prefix of the credential, an extension
/// of it, and the empty string are all rejected, and the exact credential is still accepted. A
/// rewrite that hashed only one side, or compared digests of different lengths, breaks one of these
/// four.
#[tokio::test]
async fn check_bearer_rejects_prefixes_extensions_and_the_empty_string() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;

    let auth_data = base64::engine::general_purpose::STANDARD
        .encode(serde_json::json!({ "terms": ["0"] }).to_string());

    let prefix = &SESSION_CREDENTIAL[..SESSION_CREDENTIAL.len() - 1];
    let extension = format!("{SESSION_CREDENTIAL}x");
    for wrong in [prefix, extension.as_str(), "", "session-secreT"] {
        let resp = server
            .client
            .post(server.session_url("/session/authorise"))
            .bearer_auth(wrong)
            .json(&serde_json::json!({ "auth_data": auth_data }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401, "credential {wrong:?} must be rejected");
    }

    // The positive control: the real credential still works, so the four refusals above are not
    // "check_bearer rejects everything" — which is the shape a broken rewrite most easily takes.
    let resp = server
        .client
        .post(server.session_url("/session/authorise"))
        .bearer_auth(SESSION_CREDENTIAL)
        .json(&serde_json::json!({ "auth_data": auth_data }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}
