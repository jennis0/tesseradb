//! Engine state over HTTP: pin identity and session revocation — the server-plane half of what
//! `tessera-engine/tests/pins.rs` asserts in-process.
//!
//! Named for *engine state* rather than for pins alone: it holds session revocation and the
//! cache-pruning assertions too, all of which are the same subject — server-observable state the
//! engine owns.
//!
//! The property `g2_pins_survive_overlay_swaps` guards is lifecycle §2.3, and it is the one a
//! plausible change to the drain list most easily breaks: **a pin fixes row-space geometry and
//! never authorisation state**, so
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
            "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0]
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

/// **A superseded stamp is answered normally, with the staleness signal set**
/// (`geometry-pinning.md` §12, obligations 3 and 4). It used to be a `410 pin-expired`; the
/// retention that made that meaningful is gone, and the stamp is advisory.
///
/// The presented stamp names a superseded *prefix* as well as an impossible `segments_version`, so
/// this covers obligation 4 too — under decision 0040 a Morton prefix is a permanently stable
/// address, so a prefix change is a freshness question and not a correctness one.
#[tokio::test]
async fn a_superseded_stamp_is_answered_with_the_staleness_signal() {
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

    let body = serde_json::json!({
        "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0],
        "pin": { "prefix": "v99999", "segments_version": 999 }
    });
    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "a stamp never refuses a request");
    assert_eq!(
        resp.headers().get("x-tessera-stale").unwrap(),
        "1",
        "and the client is told its held view is out of date"
    );
    let stale_tiles = decode_viewport(&resp.bytes().await.unwrap()).0;

    // Presenting no stamp at all answers identically and reports fresh — the flag is about the
    // client's own stamp, not about the corpus having a history.
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
    assert_eq!(resp.headers().get("x-tessera-stale").unwrap(), "0");
    assert_eq!(
        decode_viewport(&resp.bytes().await.unwrap()).0,
        stale_tiles,
        "a stale stamp changes the signal and nothing about the answer"
    );
}

#[tokio::test]
async fn an_overlay_swap_does_not_stale_a_geometry_stamp() {
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
            "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0]
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

    // Re-query WITH the stamp taken before the suppression. A suppression moves the overlay and
    // not geometry, so the stamp is still current — `x-tessera-stale` stays 0 — and the count
    // reflects the suppression immediately. That second half is the one that matters: the stamp
    // has never had any bearing on authorisation state, and does not acquire one by being echoed.
    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0],
            "pin": pin
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("x-tessera-stale").unwrap(),
        "0",
        "an overlay swap moves no geometry, so the stamp is not stale"
    );
    let (tiles_after, _) = decode_viewport(&resp.bytes().await.unwrap());
    assert_eq!(tiles_after[0].1, tiles_before[0].1 - 1);
}

// --- Cache pruning, the startup bound, and check_bearer ---

/// **End-to-end revoke pruning.** `tests/cache.rs` asserts the engine-level pruner; this asserts
/// the handler actually calls it, which is a separate failure — a pruner nothing invokes closes no
/// deferral.
///
/// The revoked session is unusable either way (the registry removal is what does that, and
/// `c_revoke_then_viewport_is_rejected` above covers it), so the observable here is memory: the
/// projection is gone from the cache.
///
/// **The observable is `row_projection_cache_stats().entries`, read through `TestServer::state`,
/// and that is the whole point of this test.** Asserting a 204 and that a survivor still gets 200
/// touches the cache not at all: deleting `state.engine.prune_token(req.token_id)` from the revoke
/// handler leaves every status code in this crate's tests unchanged, so a test written that way
/// would stand over the handler claiming a property it never checks.
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
                "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0]
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
            "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0]
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
/// value.
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
         value while its sibling is checked; got: {message}"
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

// ---------------------------------------------------------------------------------------------
// The session registry's expiry sweep.
//
// The registry is the third attacker-driven memory path on this server, and the one whose cost is
// least visible from its own size: each retained session holds an `Arc<FrozenFragment>`, a live
// mapping, so the fragment cache's byte bound cannot release what a dead session still references.
// The four cases below separate the two things the sweep must be — a memory mechanism — from the
// two it must never become: the thing that refuses an expired session, or the thing that applies a
// revocation.
//
// `token_max_lifetime_secs = 0` is how expiry is reached without waiting: a session is minted with
// `expires_at == now`, so it is expired on arrival. **No test here sleeps**; every wait is on a
// response the server has already produced.
// ---------------------------------------------------------------------------------------------

/// Mint `n` sessions against the same grant, sequentially. Same grant deliberately: distinct token
/// ids and distinct registry entries, but one shared fragment, so these cases measure the registry
/// rather than the fragment cache.
async fn authorise_n(server: &TestServer, n: usize) -> Vec<serde_json::Value> {
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(authorise(server, &["0"]).await);
    }
    out
}

/// Sessions minted with a zero lifetime, at a rate the registry's own floor is set well below.
///
/// `token_max_lifetime_secs = 0` means every session expires on arrival, so the live set is empty
/// throughout and the registry should hold nothing but the residue between sweeps. Without a sweep
/// this is one retained entry per authorisation, for the life of the process, each pinning a
/// fragment.
///
/// **The assertion is the retained count, not the sweep counters**, because the counters can be
/// made to move by a sweep that removes nothing. `swept_total` is checked as well so that a
/// `retained` that stayed low for some *other* reason — a registry that failed to insert at all —
/// does not read as success.
#[tokio::test]
async fn expired_sessions_do_not_accumulate_in_the_registry() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let mut config = default_engine_config();
    config.token_max_lifetime_secs = 0;
    let server = spawn_server_with_config(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        config,
    )
    .await;

    const MINTED: usize = 40;
    let sessions = authorise_n(&server, MINTED).await;
    assert_eq!(sessions.len(), MINTED);

    let status = control_status(&server).await;
    let retained = status["sessions"]["retained"].as_u64().unwrap();
    let swept = status["sessions"]["swept_total"].as_u64().unwrap();
    let sweep_at = status["sessions"]["sweep_at"].as_u64().unwrap();

    // The live set is empty, so the threshold is the floor and the residue cannot exceed it.
    assert_eq!(
        sweep_at, 16,
        "with no live sessions the next sweep is due at the floor"
    );
    assert!(
        retained <= sweep_at,
        "the registry must not retain past its own threshold: {retained} retained of {MINTED} \
         minted, sweeping at {sweep_at}"
    );
    assert!(
        swept > 0,
        "a low retained count with nothing swept would mean the sessions were never inserted, \
         not that they were reclaimed"
    );
    assert_eq!(
        swept + retained,
        MINTED as u64,
        "every minted session must be either retained or accounted for as swept"
    );
}

/// The mirror, and the one that stops the sweep from being written as `clear()`.
///
/// Every session here is live for an hour, so a correct sweep removes nothing at all and the
/// registry grows to exactly what was minted. The sweep is asserted to have *run* — otherwise this
/// case would pass on a build with no sweep in it and would cover nothing — and the first token
/// minted, the one a coarse policy would evict first, must still be served.
#[tokio::test]
async fn the_sweep_keeps_every_live_session() {
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

    const MINTED: usize = 40;
    let sessions = authorise_n(&server, MINTED).await;

    let status = control_status(&server).await;
    assert_eq!(
        status["sessions"]["retained"].as_u64().unwrap(),
        MINTED as u64,
        "a live session must survive every sweep"
    );
    assert!(
        status["sessions"]["sweeps"].as_u64().unwrap() > 0,
        "the sweep must have run, or this case asserts nothing about it"
    );
    assert_eq!(status["sessions"]["swept_total"].as_u64().unwrap(), 0);

    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(sessions[0]["token"].as_str().unwrap())
        .json(&serde_json::json!({
            "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "the oldest session is the one a sweep written as an eviction policy would take first"
    );
}

/// The sweep's engine half: what the registry drops, the engine drops too.
///
/// A session that has served a viewport owns a row projection, and the cache holding it is keyed by
/// `token_id` — which the registry is the only holder of. Sweeping the registry without telling the
/// engine leaves that projection resident until a byte bound chooses it, which for a small entry is
/// a long way off, and the session it belongs to can never be presented again.
///
/// **This case sleeps, and the three above it do not.** They reach expiry with
/// `token_max_lifetime_secs = 0`, which mints a session that is expired on arrival and can
/// therefore never serve a request — so there would be no engine-side entry to prune. A session
/// that works and *then* expires needs a lifetime the clock can pass, and `expires_at` is a
/// wall-clock second.
#[tokio::test]
async fn the_sweep_prunes_what_the_engine_holds_for_an_expired_session() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let mut config = default_engine_config();
    config.token_max_lifetime_secs = 2;
    let server = spawn_server_with_config(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        config,
    )
    .await;

    let doomed = authorise(&server, &["0"]).await;
    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(doomed["token"].as_str().unwrap())
        .json(&serde_json::json!({
            "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let status = control_status(&server).await;
    assert_eq!(
        status["row_projection_cache"]["entries"].as_u64().unwrap(),
        1,
        "a served viewport leaves the session's row projection resident"
    );

    // Past the deadline, then over the sweep threshold. The sessions minted here expire on the
    // same schedule and are swept in their turn; what is asserted is the first one's entry.
    tokio::time::sleep(std::time::Duration::from_millis(2_500)).await;
    authorise_n(&server, 20).await;

    let status = control_status(&server).await;
    assert!(
        status["sessions"]["swept_total"].as_u64().unwrap() > 0,
        "the sweep must have run, or this case asserts nothing"
    );
    assert_eq!(
        status["row_projection_cache"]["entries"].as_u64().unwrap(),
        0,
        "the swept session's engine-side entries go with it, as a revocation's do"
    );
}

/// **Revocation is immediate and owes nothing to the sweep.** A revoked session must not survive
/// until some later pass notices it — that would be fail-open for the interval in between.
///
/// The registry here holds two sessions, well below the threshold at which a sweep runs, and the
/// status body is asserted to confirm that no sweep has run. The revoked token is refused anyway.
/// Routing removal through the sweep — the plausible simplification, since both delete from the
/// same two maps — fails here.
#[tokio::test]
async fn revocation_takes_effect_without_waiting_for_a_sweep() {
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

    let status = control_status(&server).await;
    assert_eq!(
        status["sessions"]["sweeps"].as_u64().unwrap(),
        0,
        "two sessions is below the sweep threshold; if a sweep has run, this test no longer \
         demonstrates that revocation is independent of it"
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

    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(doomed["token"].as_str().unwrap())
        .json(&serde_json::json!({
            "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401, "a revoked session is unusable at once");

    // The positive control, for the symmetric failure: a revoke that emptied the registry would
    // satisfy the assertion above just as well.
    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(survivor["token"].as_str().unwrap())
        .json(&serde_json::json!({
            "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

/// **Expiry is enforced by the deadline check, not by the sweep**, and this is the case that says
/// so: one session, minted already expired, with the registry far below its sweep threshold — so
/// the entry is demonstrably still present — and the request is refused all the same.
///
/// If this ever answered 200, the sweep would have become the mechanism that expires a session,
/// which is fail-open by exactly the interval between sweeps.
#[tokio::test]
async fn an_expired_session_is_refused_while_still_retained() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let mut config = default_engine_config();
    config.token_max_lifetime_secs = 0;
    let server = spawn_server_with_config(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        config,
    )
    .await;

    let auth = authorise(&server, &["0"]).await;

    let status = control_status(&server).await;
    assert_eq!(
        status["sessions"]["retained"].as_u64().unwrap(),
        1,
        "the entry must still be in the registry, or this case would pass for the wrong reason"
    );
    assert_eq!(status["sessions"]["sweeps"].as_u64().unwrap(), 0);

    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(auth["token"].as_str().unwrap())
        .json(&serde_json::json!({
            "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "expired-token");
}
