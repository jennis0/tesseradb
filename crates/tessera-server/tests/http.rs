//! The viewer plane, the session plane, config, byte shape and the compute-admission gate, end to
//! end — spawned in-process on port 0 against a small synthetic bundle (the same fixture pattern
//! `tessera-engine`'s `tests/viewport.rs` uses).
//!
//! The **write-path** cases live in `tests/http_write.rs` — `/control/ingest`, `/control/changes`,
//! `/control/status`, batch-id idempotency, allocation and the health endpoints — and pins and
//! session revocation in `tests/http_engine_state.rs`. Shared fixtures live in [`common`]; some doc
//! comments below refer to tests in the sibling files.

mod common;

use base64::Engine as _;
use tempfile::TempDir;

use tessera_engine::viewport::SERIAL_FALLBACK_MAX_ROWS;
use tessera_engine::{Engine, EngineConfig};
use tessera_plugin::Passthrough;
use tessera_server::state::ComputeGate;
use tessera_spatial::tiles_for_bbox;

use common::*;

/// Item count for the two byte-equality tests below — see `tessera-engine/tests/viewport.rs`'s
/// identically-named constant for the full argument (same fixed-extent scatter, so
/// `Σ range.len() == n` exactly for a full-extent request). This crate does not depend on
/// `tessera-engine`'s test binary, so the constant and its reasoning are duplicated rather than
/// shared, matching this file's own existing "same fixture pattern" duplication of
/// `tests/viewport.rs`'s fixture builder (this file's module doc).
///
/// **Item count alone does not reach the parallel branch.** `SERIAL_FALLBACK_MAX_ROWS` sits at
/// 500,000,000 (see that constant's doc in `tessera-engine` for the calibration), comfortably above
/// this fixture — which the assertion below pins. The two tests below therefore force the branch
/// directly via `Engine::set_serial_fallback_max_rows_for_test` (`bench-timing`-gated, test-only) on
/// each server's `Engine` before it starts serving. This constant still matters independent of that
/// override: it is what gives the request a genuinely multi-tile, multi-thousand-row shape
/// (cross-tile ordering, the underlay path) rather than a token one.
const PARALLEL_HEADLINE_ITEMS: u64 = 300_000;

/// Sanity check that [`PARALLEL_HEADLINE_ITEMS`] stays deliberately unit-test-scale small relative
/// to the production threshold — not load-bearing for the two tests' correctness any more (the
/// `bench-timing` override makes them reach the parallel branch regardless of this relationship),
/// but a true and worth-keeping fact about why this fixture is cheap to build.
const _: () = assert!(PARALLEL_HEADLINE_ITEMS < SERIAL_FALLBACK_MAX_ROWS);

#[tokio::test]
async fn a_authorise_then_viewport_succeeds_with_matching_counts() {
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
            "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 5
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.headers().contains_key("x-tessera-pin"));
    let bytes = resp.bytes().await.unwrap();
    let (tiles, points) = decode_viewport(&bytes);
    assert_eq!(tiles.len(), 1);
    assert_eq!(tiles[0].1, N_ITEMS, "every item carries term 0");
    assert_eq!(tiles[0].1, tiles[0].2, "matched == visible (no filters)");
    assert_eq!(points.len(), 5, "k=5 caps sampled points, not the count");
}

/// **The two view coordinates reach a client, and they are distinguishable and stable.**
///
/// The content coordinate travels as an entity tag because that is what HTTP already means by it,
/// and because the tile-addressed route will want the same value for browser caching. The identity
/// coordinate gets its own header: it answers a different question — whether a held band may be
/// rendered at all rather than declared — and HTTP has one validator slot.
///
/// A client keys its replica partition on the identity coordinate, so a value that moved between
/// two requests under one principal would drop every held band on every pan.
#[tokio::test]
async fn viewport_serves_an_etag_and_an_identity_key_that_are_stable_across_requests() {
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

    let fetch = || async {
        let resp = server
            .client
            .post(server.viewer_url("/v1/viewport"))
            .bearer_auth(token)
            .json(&serde_json::json!({
                "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 5
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let etag = resp.headers()["etag"].to_str().unwrap().to_string();
        let identity = resp.headers()["x-tessera-identity-key"]
            .to_str()
            .unwrap()
            .to_string();
        (etag, identity)
    };

    let (etag, identity) = fetch().await;

    // Quoted, 32 hex characters, no weak-tag prefix: an exact comparison of an opaque value.
    assert!(
        etag.starts_with('"') && etag.ends_with('"') && etag.len() == 34,
        "entity tag must be a quoted 16-byte hex value, got {etag}"
    );
    assert!(etag[1..33].chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(identity.len(), 32);
    assert!(identity.chars().all(|c| c.is_ascii_hexdigit()));
    assert_ne!(
        &etag[1..33],
        identity.as_str(),
        "the two coordinates answer different questions and must not be one value"
    );

    // Nothing wrote in between, so neither may move.
    let (etag_again, identity_again) = fetch().await;
    assert_eq!(etag, etag_again);
    assert_eq!(identity, identity_again);

    // A different principal is a different render partition, and the client drops its bands on it.
    let other = authorise(&server, &["1"]).await;
    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(other["token"].as_str().unwrap())
        .json(&serde_json::json!({
            "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 5
        }))
        .send()
        .await
        .unwrap();
    assert_ne!(
        resp.headers()["x-tessera-identity-key"].to_str().unwrap(),
        identity,
        "a different mask must not share a render partition — that is decision 0029's disclosure"
    );
}

/// **A request names its tile set exactly once, by bbox or by list.**
///
/// The list is how a client with a replica elides: a tile it can prove it holds is simply absent,
/// and an absent tile costs the engine nothing. Both operands together is a contradiction the
/// server must not resolve on the caller's behalf, and neither leaves nothing to answer for.
#[tokio::test]
async fn viewport_takes_exactly_one_of_bbox_and_tiles() {
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

    let post = |body: serde_json::Value| {
        let client = server.client.clone();
        let url = server.viewer_url("/v1/viewport");
        let token = token.to_string();
        async move {
            client
                .post(url)
                .bearer_auth(token)
                .json(&body)
                .send()
                .await
                .unwrap()
        }
    };

    // Both: refused rather than silently preferring one.
    let resp = post(serde_json::json!({
        "view": "s0", "zoom": 1, "bbox": [0.0, 0.0, 1000.0, 1000.0], "tiles": [0], "k": 5
    }))
    .await;
    assert_eq!(resp.status(), 422);

    // Neither: there is no tile set to answer for.
    let resp = post(serde_json::json!({"view": "s0", "zoom": 1, "k": 5})).await;
    assert_eq!(resp.status(), 422);

    // A prefix with bits above the request's own depth names a tile at a depth nobody asked about.
    let resp = post(serde_json::json!({"view": "s0", "zoom": 1, "tiles": [64], "k": 5})).await;
    assert_eq!(resp.status(), 422);

    // And the list, alone and well-formed, is served.
    let resp =
        post(serde_json::json!({"view": "s0", "zoom": 1, "tiles": [0, 1, 2, 3], "k": 5})).await;
    assert_eq!(resp.status(), 200);
}

/// **A listed tile set answers for exactly those tiles, once each, and omitting one omits its cost.**
///
/// This is the whole point of the operand: the bbox spends the full per-tile pipeline on every tile
/// it spans, whereas a client that already holds a tile leaves it out and pays nothing for it.
/// Duplicates are collapsed at the boundary, because the points stream is a flat concatenation in
/// tile order and a repeated tile would be served — and drawn — twice.
#[tokio::test]
async fn a_listed_tile_set_is_answered_exactly_and_deduplicated() {
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

    let fetch = |body: serde_json::Value| {
        let client = server.client.clone();
        let url = server.viewer_url("/v1/viewport");
        let token = token.to_string();
        async move {
            let resp = client
                .post(url)
                .bearer_auth(token)
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200);
            let bytes = resp.bytes().await.unwrap();
            decode_viewport(&bytes)
        }
    };

    // Every depth-1 tile, by bbox — the whole corpus.
    let (all_tiles, all_points) = fetch(serde_json::json!({
        "view": "s0", "zoom": 1, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 500
    }))
    .await;
    assert!(!all_tiles.is_empty());

    // The same set named explicitly, with one tile repeated three times.
    let mut listed: Vec<u64> = all_tiles.iter().map(|t| t.0).collect();
    let repeated = listed[0];
    listed.extend([repeated, repeated]);
    let (dedup_tiles, dedup_points) = fetch(serde_json::json!({
        "view": "s0", "zoom": 1, "tiles": listed, "k": 500
    }))
    .await;
    assert_eq!(
        dedup_tiles.len(),
        all_tiles.len(),
        "a repeated tile must be collapsed, not served twice"
    );
    assert_eq!(dedup_points.len(), all_points.len());

    // Now drop one tile, as a client holding it would. Its counts and its points both go with it.
    let dropped = all_tiles[0];
    let kept: Vec<u64> = all_tiles.iter().skip(1).map(|t| t.0).collect();
    if !kept.is_empty() {
        let (subset_tiles, subset_points) =
            fetch(serde_json::json!({"view": "s0", "zoom": 1, "tiles": kept, "k": 500})).await;
        assert_eq!(subset_tiles.len(), all_tiles.len() - 1);
        assert!(
            !subset_tiles.iter().any(|t| t.0 == dropped.0),
            "an omitted tile must not be answered for at all"
        );
        assert!(
            subset_points.len() < all_points.len(),
            "and its points must not be gathered"
        );
    }
}

/// Owner ruling (contracts §3.2): `/v1/items` returns the identical `404` for "no such id" and
/// "exists but is not visible to this principal" -- same status, same body, byte for byte. This
/// test deliberately never learns which *external id* the invisible `tessera_id` names (that
/// would require inverting the identity, which I10 forbids even to a test): it gets a genuinely
/// existing id from session A's own viewport (everyone carries term "0") and finds one that
/// session B -- authorised for term "1" only, so it sees strictly fewer items (`terms_of`'s
/// multiples-of-3 subset) -- cannot see, entirely through the HTTP surface a client has.
#[tokio::test]
async fn i_item_404s_identically_for_unknown_and_invisible() {
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

    // Session A: term "0" -- every item carries it, so A sees the whole bundle.
    let auth_a = authorise(&server, &["0"]).await;
    let token_a = auth_a["token"].as_str().unwrap();
    // Session B: term "1" only -- `terms_of`'s multiples-of-3 subset, strictly fewer items.
    let auth_b = authorise(&server, &["1"]).await;
    let token_b = auth_b["token"].as_str().unwrap();

    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token_a)
        .json(&serde_json::json!({
            "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 200
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let (_, points) = decode_viewport(&resp.bytes().await.unwrap());
    assert!(
        !points.is_empty(),
        "session A's viewport must return some points to pick from"
    );

    // Find a tessera_id that is real (session A's own viewport returned it) but invisible to B.
    let mut invisible_to_b = None;
    for &(tessera_id, _) in &points {
        let resp_b = post_item(&server, token_b, tessera_id).await;
        if resp_b.status() == 404 {
            invisible_to_b = Some(tessera_id);
            break;
        }
    }
    let invisible_to_b = invisible_to_b
        .expect("the fixture's multiples-of-3 term split must leave something invisible to B");

    // Sanity: A, which is the session that surfaced this id in its own viewport, can fetch it.
    let resp_a = post_item(&server, token_a, invisible_to_b).await;
    assert_eq!(
        resp_a.status(),
        200,
        "session A must be able to fetch an id its own viewport just returned"
    );

    let unknown_to_everyone = 0xDEAD_BEEF_DEAD_BEEFu64;
    let resp_unknown = post_item(&server, token_b, unknown_to_everyone).await;
    let resp_invisible = post_item(&server, token_b, invisible_to_b).await;

    assert_eq!(resp_unknown.status(), 404);
    assert_eq!(resp_invisible.status(), 404);
    assert_eq!(resp_unknown.status(), resp_invisible.status());

    let unknown_body = resp_unknown.text().await.unwrap();
    let invisible_body = resp_invisible.text().await.unwrap();
    assert_eq!(
        unknown_body, invisible_body,
        "identical 404 required byte-for-byte -- any difference is an oracle for \"this id exists\""
    );
}

#[tokio::test]
async fn b_missing_or_garbage_token_is_401() {
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

    let body = serde_json::json!({
        "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0]
    });

    let resp_missing = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp_missing.status(), 401);

    let resp_garbage = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth("not-a-real-token")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp_garbage.status(), 401);
}

#[tokio::test]
async fn d_unknown_view_404_and_malformed_bbox_422() {
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
            "view": "does-not-exist", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "unknown");

    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "view": "s0", "zoom": 0, "bbox": [1000.0, 0.0, 0.0, 1000.0]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 422);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "contract");
}

#[tokio::test]
async fn h_config_missing_disclosure_refuses_to_start() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );

    std::env::set_var("TESSERA_TEST_H_SESSION", SESSION_CREDENTIAL);
    std::env::set_var("TESSERA_TEST_H_OPERATOR", OPERATOR_CREDENTIAL);

    let toml_text = format!(
        r#"
        [bundle]
        path = "{bundle}"
        cache = "{cache}"
        wal = "{wal}"
        [plugin]
        module = "builtin:passthrough"
        [serve]
        viewer = "127.0.0.1:0"
        session = "127.0.0.1:0"
        control = "127.0.0.1:0"
        session_credential_env = "TESSERA_TEST_H_SESSION"
        operator_credential_env = "TESSERA_TEST_H_OPERATOR"
        "#,
        bundle = bundle_root.display(),
        cache = tmp.path().join("cache").display(),
        wal = tmp.path().join("wal.log").display(),
    );
    let config_path = tmp.path().join("tessera.toml");
    std::fs::write(&config_path, toml_text).unwrap();

    let result = tessera_server::prepare(&config_path);
    assert!(
        result.is_err(),
        "a config missing [disclosure] must refuse to start"
    );
}

// --- Authentication and disclosure regressions ---

/// `GET /v1/meta` must require a valid session token — it discloses bundle extents, views and the
/// declared-scalar schema.
#[tokio::test]
async fn viewer_meta_requires_bearer() {
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

    let resp = server
        .client
        .get(server.viewer_url("/v1/meta"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap();
    let resp = server
        .client
        .get(server.viewer_url("/v1/meta"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

/// Contracts §2.2 r6: `GET /v1/meta` reports the idset as `idset` —
/// and reports **only** the idset: the identity key appears in no API response on any plane.
/// Nothing asserted either half before, which is what let S2's idset regression sit untested.
#[tokio::test]
async fn viewer_meta_reports_the_idset_and_never_the_key() {
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
        .get(server.viewer_url("/v1/meta"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body["idset"], FIXTURE_IDSET,
        "/v1/meta must report the bundle's idset: {body}"
    );
    let raw = body.to_string();
    assert!(
        !raw.contains(TEST_KEY_HEX),
        "/v1/meta must never carry the identity key: {raw}"
    );
}

/// Contracts §2.2/§3.2 r6: `POST /v1/items/{tessera_id}` accepts an optional `idset` and answers
/// `409 conflict` — "stale idset; re-resolve by external_id" — when it does not match.
/// The 409 had no test at any level, and the check is decided before inversion, so a matching
/// idset must not alter the answer for the same id.
#[tokio::test]
async fn item_with_a_stale_idset_is_409_and_a_matching_idset_changes_nothing() {
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

    // A real, visible id, so the 409 is not confusable with the 404 an unknown id would give.
    let viewport = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 1
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(viewport.status(), 200);
    let (_tiles, points) = decode_viewport(&viewport.bytes().await.unwrap());
    let tessera_id = points[0].0;

    // Baseline: no idset at all → 200.
    let plain = post_item(&server, token, tessera_id).await;
    assert_eq!(plain.status(), 200);

    // A stale idset → 409, with the contract's own detail string.
    let stale = server
        .client
        .post(server.viewer_url(&format!("/v1/items/{tessera_id}")))
        .bearer_auth(token)
        .json(&serde_json::json!({ "idset": FIXTURE_IDSET + 1 }))
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), 409);
    let body: serde_json::Value = stale.json().await.unwrap();
    assert_eq!(body["error"], "conflict");
    assert_eq!(body["detail"], "stale idset; re-resolve by external_id");

    // The matching idset is a no-op: same 200, same body as the idset-less request.
    let matching = server
        .client
        .post(server.viewer_url(&format!("/v1/items/{tessera_id}")))
        .bearer_auth(token)
        .json(&serde_json::json!({ "idset": FIXTURE_IDSET }))
        .send()
        .await
        .unwrap();
    assert_eq!(matching.status(), 200);

    // And a stale idset on an id naming nothing is still the 409, decided before inversion —
    // identical for every identifier, so it opens no channel (Appendix C, C4).
    let stale_unknown = server
        .client
        .post(server.viewer_url("/v1/items/0"))
        .bearer_auth(token)
        .json(&serde_json::json!({ "idset": FIXTURE_IDSET + 1 }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        stale_unknown.status(),
        409,
        "the idset check must be entity-independent, not fall through to 404"
    );
}

/// The trailer's `stage_ns` key (formerly the `x-tessera-stage-ns` header — contracts §3.2 r26)
/// obeys **both** of its gates, and carries no identifier.
///
/// `spawn_server` sets `stage_timing: true`, so the runtime gate is open throughout this test.
/// The compile-time gate therefore decides on its own, and this asserts each direction rather
/// than only the one the current build happens to take — a release binary that started emitting
/// the breakdown would otherwise pass a test written for the instrumented build. The retired
/// header is asserted absent in BOTH directions: nothing may quietly resurrect it.
///
/// The field-count assertion pins the CSV contract `tessera-bench` and `scripts/bench_*.py`
/// parse. Append-only: adding a stage means bumping the expected count here deliberately.
#[tokio::test]
async fn stage_timing_header_respects_the_compile_gate_and_carries_no_identifier() {
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
            "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 5
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    assert!(
        resp.headers().get("x-tessera-stage-ns").is_none(),
        "the stage header is retired (contracts §3.2 r26): the breakdown rides the trailer"
    );
    let decoded = decode_viewport_frames(&resp.bytes().await.unwrap());
    let stage_ns = decoded.trailer.get("stage_ns").cloned();

    if cfg!(feature = "bench-timing") {
        let value =
            stage_ns.expect("bench-timing is on and stage_timing is true, so stage_ns is present");
        let text = value.as_str().expect("stage_ns is a CSV string").to_string();
        let text = text.as_str();

        let fields: Vec<&str> = text.split(',').collect();
        assert_eq!(
            fields.len(),
            22,
            "stage header field count is a contract with the bench harnesses: {text}"
        );
        for f in &fields {
            assert!(
                f.parse::<u64>().is_ok(),
                "every field is an unsigned integer — no names, no identifiers: {text}"
            );
        }

        // Positions 12..=17 are the work counters (see `stage_header`'s field order).
        let tiles_nonempty: u64 = fields[13].parse().unwrap();
        let sigma_visible: u64 = fields[14].parse().unwrap();
        let materialised: u64 = fields[16].parse().unwrap();
        let gathered: u64 = fields[17].parse().unwrap();
        assert_eq!(tiles_nonempty, 1, "zoom 0 is one tile");
        assert_eq!(sigma_visible, N_ITEMS, "every item carries term 0");
        assert_eq!(gathered, 5, "k=5");
        // NOT re-asserted here. This test's job is the two gates and the no-identifier property;
        // the counter's semantics belong to the engine-side canary, which uses a partial-coverage
        // fixture so that both directions of the comparison are detectable. This session is
        // full-coverage, where `sigma_visible == rows_in_ranges` and the comparison is half blind.
        // Asserting it here anyway would read as coverage it does not provide.
        assert!(
            materialised > 0,
            "the counter must reach the wire at all: {text}"
        );

        // The three fields appended for §7.2's θ anchor and §7.3's underlay. This request asks for
        // no underlay, so both underlay fields must be zero — the default path must not pay for a
        // feature it did not request.
        let theta_anchor_ns: u64 = fields[19].parse().unwrap();
        let underlay_ns: u64 = fields[20].parse().unwrap();
        let underlay_cells: u64 = fields[21].parse().unwrap();
        let _ = theta_anchor_ns; // a duration; only its presence and parseability are contractual
        assert_eq!(
            underlay_ns, 0,
            "no underlay was requested, so it must cost nothing"
        );
        assert_eq!(
            underlay_cells, 0,
            "no underlay was requested, so no cells were evaluated"
        );
    } else {
        assert!(
            stage_ns.is_none(),
            "without the bench-timing feature the trailer must carry no stage_ns even when \
             `stage_timing = true` — a release build must not emit it"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// The two-stage admission gate, the 429 `backpressure` contract, and the
// x-tessera-server-us / x-tessera-admission-us timing split.
// ---------------------------------------------------------------------------------------------

/// A slow viewport request, engineered exactly as `healthz_stays_prompt_while_a_long_viewport_runs`
/// does (see its doc for the cost-model argument): `zoom = 0`, `underlay_offset = 12` against a
/// server whose `EngineConfig` has been widened to allow it. Used throughout the gate tests below
/// to hold the compute permit for long enough to deterministically observe saturation.
fn slow_viewport_body() -> serde_json::Value {
    serde_json::json!({
        "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 1,
        "underlay_offset": 12
    })
}

fn fast_viewport_body() -> serde_json::Value {
    serde_json::json!({ "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 1 })
}

/// A server config wide enough for [`slow_viewport_body`] to pass `Engine::viewport`'s own
/// bounds checks rather than being refused as `EngineError::UnderlayRefused` before it costs
/// anything.
fn engine_config_for_slow_viewport() -> EngineConfig {
    let mut config = default_engine_config();
    config.max_underlay_offset = 12;
    config.max_underlay_cells = 20_000_000;
    config
}

/// Poll `/control/status` until `compute.in_flight` reaches `want`, panicking after a generous
/// bound rather than looping forever. **Deterministic, not a timing bet**: this is the
/// poll-until-a-real-condition-holds pattern the brief asks for in place of a fixed sleep or a
/// tuned yield count — it directly observes the gate's own state (derived from the semaphores'
/// live permit counts, `state::ComputeGate::status`) rather than guessing how long "the slow
/// request has started" takes on this run's scheduler.
async fn poll_until_in_flight(server: &TestServer, want: u64) {
    // Bound is generous (10s, not the original 2s) precisely so this helper's own panic stays
    // rare: on a slow or contended runner, a tight bound here fires *this* panic instead of
    // whichever ratio/timing assertion the calling test actually exists to check, which reads to
    // a future maintainer as "the gate never reached this state" (implicating the mechanism under
    // test) rather than "the runner was too slow for the poll bound" (an unrelated, purely
    // cosmetic failure mode) — both are still test failures either way, just with different, and
    // differently misleading, messages.
    for _ in 0..10_000 {
        let status = control_status(server).await;
        if status["compute"]["in_flight"].as_u64() == Some(want) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    panic!(
        "compute.in_flight did not reach {want} within the 10s poll bound -- this is \
         poll_until_in_flight's own generous-but-finite timeout firing, not necessarily the \
         calling test's real assertion; check whether the gate is genuinely stuck before \
         assuming a regression in the mechanism the calling test targets"
    );
}

/// With `compute_admission = 1, compute_queue = 0` — the deterministic configuration — a second
/// concurrent `/v1/viewport` while the first is still running gets
/// an immediate 429 — `try_acquire` on the outer slots semaphore fails synchronously, so this
/// does not even need `admission_timeout_ms` to elapse. Verifies the full 429 contract: status,
/// `Retry-After: 1` header, and `{"error": "backpressure", "retry_after_s": 1}` body.
#[tokio::test]
async fn saturated_gate_sheds_a_second_viewport_with_429_and_retry_after() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server_with_config_and_gate(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        engine_config_for_slow_viewport(),
        ComputeGate::new(1, 0, 250),
    )
    .await;

    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap().to_string();

    let viewer_url = server.viewer_url("/v1/viewport");
    let client = server.client.clone();
    let slow_token = token.clone();
    let slow_task = tokio::spawn(async move {
        client
            .post(viewer_url)
            .bearer_auth(slow_token)
            .json(&slow_viewport_body())
            .send()
            .await
            .unwrap()
    });

    // Deterministic: wait until the slow request has actually acquired its compute permit
    // (`in_flight == 1`), not a guessed delay.
    poll_until_in_flight(&server, 1).await;

    let second_resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(&token)
        .json(&fast_viewport_body())
        .send()
        .await
        .unwrap();

    assert_eq!(second_resp.status(), 429);
    assert_eq!(
        second_resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok()),
        Some("1"),
        "a 429 must carry Retry-After: 1"
    );
    let body: serde_json::Value = second_resp.json().await.unwrap();
    assert_eq!(body["error"], "backpressure");
    assert_eq!(body["retry_after_s"], 1);

    let slow_resp = slow_task.await.unwrap();
    assert_eq!(
        slow_resp.status(),
        200,
        "the request that actually held the gate must still succeed"
    );
}

/// `/healthz`, `/v1/meta`, `/session/revoke`, and a `/control/changes` suppress must all succeed
/// while the viewer/session gate is fully saturated by a slow viewport. None of them is a gated
/// path — the gate's list is exactly `/v1/viewport`, `/v1/items` and `/session/authorise` — and the
/// deny priority lane (lifecycle §1.3) must never be blocked by compute-admission pressure on an
/// unrelated plane.
#[tokio::test]
async fn never_gated_routes_succeed_while_the_viewer_gate_is_saturated() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server_with_config_and_gate(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        engine_config_for_slow_viewport(),
        ComputeGate::new(1, 0, 250),
    )
    .await;

    // Both sessions are minted BEFORE the gate is saturated below: `/session/authorise` IS a gated
    // path (it shares the viewer/session compute budget), so acquiring a *second*
    // session token during saturation would itself race the gate rather than testing the
    // never-gated routes this test is actually about.
    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap().to_string();
    let second_auth = authorise(&server, &["0"]).await;
    let second_token_id = second_auth["token_id"].as_u64().unwrap();

    let viewer_url = server.viewer_url("/v1/viewport");
    let client = server.client.clone();
    let slow_token = token.clone();
    let slow_task = tokio::spawn(async move {
        client
            .post(viewer_url)
            .bearer_auth(slow_token)
            .json(&slow_viewport_body())
            .send()
            .await
            .unwrap()
    });

    poll_until_in_flight(&server, 1).await;

    // `/healthz`: no bearer, no gate.
    let healthz_resp = server
        .client
        .get(server.viewer_url("/healthz"))
        .send()
        .await
        .unwrap();
    assert_eq!(healthz_resp.status(), 200, "/healthz must never be gated");

    // `/v1/meta`: viewer-plane bearer, but never gated (the gated-paths list is exact).
    let meta_resp = server
        .client
        .get(server.viewer_url("/v1/meta"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(meta_resp.status(), 200, "/v1/meta must never be gated");

    // `/session/revoke`: session-plane credential, never gated. Revokes the SECOND session
    // (minted before saturation, above) so the slow request's own `Arc<SessionEntry>` — cloned
    // into its `spawn_blocking` closure before this point — is unaffected either way; this
    // assertion is purely about the revoke endpoint's own responsiveness under a saturated gate.
    let revoke_resp = server
        .client
        .post(server.session_url("/session/revoke"))
        .bearer_auth(SESSION_CREDENTIAL)
        .json(&serde_json::json!({ "token_id": second_token_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        revoke_resp.status(),
        204,
        "/session/revoke must never be gated"
    );

    // `/control/changes` suppress: the case this test exists for. The entire control plane is off
    // the viewer/session gate; a deny op must reach the WAL regardless.
    const SUPPRESS_SOURCE_ID: u64 = 3;
    let external_id =
        base64::engine::general_purpose::STANDARD.encode(external_id_of(SUPPRESS_SOURCE_ID));
    let suppress_resp = server
        .client
        .post(server.control_url("/control/changes"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&serde_json::json!([{ "external_id": external_id, "op": "suppress" }]))
        .send()
        .await
        .unwrap();
    assert_eq!(
        suppress_resp.status(),
        200,
        "a suppress must succeed while the viewer gate is fully saturated"
    );

    let slow_resp = slow_task.await.unwrap();
    assert_eq!(slow_resp.status(), 200);
}

/// Spec constraint: no permit leak. After a shed (a second request while the gate is saturated)
/// and after the holder's own completion, both the outer and inner semaphores must show their
/// permits fully returned — observed twice, live, via `/control/status`'s gauges rather than by
/// inference from a single before/after snapshot.
#[tokio::test]
async fn no_permit_leak_after_a_shed_or_a_completion() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server_with_config_and_gate(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        engine_config_for_slow_viewport(),
        ComputeGate::new(1, 0, 250),
    )
    .await;

    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap().to_string();

    let viewer_url = server.viewer_url("/v1/viewport");
    let client = server.client.clone();
    let slow_token = token.clone();
    let slow_task = tokio::spawn(async move {
        client
            .post(viewer_url)
            .bearer_auth(slow_token)
            .json(&slow_viewport_body())
            .send()
            .await
            .unwrap()
    });

    poll_until_in_flight(&server, 1).await;

    let shed_before = control_status(&server).await["compute"]["shed_total"]
        .as_u64()
        .unwrap();

    let shed_resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(&token)
        .json(&fast_viewport_body())
        .send()
        .await
        .unwrap();
    assert_eq!(shed_resp.status(), 429);

    // The shed attempt's own (failed) permit acquisition must not have leaked: `in_flight` still
    // reads exactly 1 (the still-running slow request, nothing more, nothing less) and
    // `shed_total` incremented by exactly one.
    let after_shed = control_status(&server).await;
    assert_eq!(after_shed["compute"]["in_flight"], 1);
    assert_eq!(after_shed["compute"]["waiting"], 0);
    assert_eq!(
        after_shed["compute"]["shed_total"].as_u64().unwrap(),
        shed_before + 1
    );

    let slow_resp = slow_task.await.unwrap();
    assert_eq!(slow_resp.status(), 200);

    // Deterministic wait for the completed request's permits to be returned, then a fresh
    // request must succeed — a leaked permit would make it shed too.
    poll_until_in_flight(&server, 0).await;
    let after_completion = control_status(&server).await;
    assert_eq!(after_completion["compute"]["waiting"], 0);

    let third_resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(&token)
        .json(&fast_viewport_body())
        .send()
        .await
        .unwrap();
    assert_eq!(
        third_resp.status(),
        200,
        "a leaked permit would make this request shed too"
    );
}

/// `x-tessera-server-us`'s clock starts AFTER admission, so it stays close to what an
/// unqueued request measures even when this request was forced to queue for a long time; the
/// queueing itself shows up only in `x-tessera-admission-us`, which must grow to reflect it.
///
/// Self-scaling, not a fixed wall-clock bet (this file's established pattern): rather than
/// asserting an absolute microsecond bound, this compares the *queued* fast request's own two
/// headers against each other (`server_us` must be much smaller than `admission_us` — most of
/// its total time was spent waiting, not computing) and against a genuinely unqueued baseline
/// request measured in the same run (`server_us` close to baseline; `admission_us` far above the
/// baseline's own near-zero admission wait).
#[tokio::test]
async fn server_us_excludes_admission_wait_while_admission_us_captures_it() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    // compute_queue = 1 (not 0): the queued fast request below must be ADMITTED (a slot) and
    // then WAIT for a compute permit, rather than being shed outright by stage 1 — that wait is
    // exactly what `x-tessera-admission-us` needs to capture. A generous timeout so it is never
    // shed by stage 2 either; this test is about the timing split, not the shedding contract.
    let server = spawn_server_with_config_and_gate(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        engine_config_for_slow_viewport(),
        ComputeGate::new(1, 1, 60_000),
    )
    .await;

    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap().to_string();

    // Baseline: a solo fast request with no contention at all.
    let baseline_resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(&token)
        .json(&fast_viewport_body())
        .send()
        .await
        .unwrap();
    assert_eq!(baseline_resp.status(), 200);
    let baseline_admission_us: u64 = header_u64(&baseline_resp, "x-tessera-admission-us");
    let baseline_server_us: u64 = header_u64(&baseline_resp, "x-tessera-server-us");

    // Now hold the gate with a slow request, and send a fast one behind it.
    let viewer_url = server.viewer_url("/v1/viewport");
    let client = server.client.clone();
    let slow_token = token.clone();
    let slow_task = tokio::spawn(async move {
        client
            .post(viewer_url)
            .bearer_auth(slow_token)
            .json(&slow_viewport_body())
            .send()
            .await
            .unwrap()
    });
    poll_until_in_flight(&server, 1).await;

    let queued_resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(&token)
        .json(&fast_viewport_body())
        .send()
        .await
        .unwrap();
    assert_eq!(queued_resp.status(), 200);
    let queued_admission_us = header_u64(&queued_resp, "x-tessera-admission-us");
    let queued_server_us = header_u64(&queued_resp, "x-tessera-server-us");

    let slow_resp = slow_task.await.unwrap();
    assert_eq!(slow_resp.status(), 200);

    assert!(
        queued_admission_us > baseline_admission_us,
        "a request forced to queue behind a slow one must show a larger admission wait than an \
         unqueued baseline: queued={queued_admission_us}us baseline={baseline_admission_us}us"
    );
    assert!(
        queued_server_us < queued_admission_us,
        "server_us must exclude the queueing this request experienced -- it should be far \
         smaller than admission_us, not comparable to it: server_us={queued_server_us}us \
         admission_us={queued_admission_us}us"
    );
    // Generous relative bound (self-scaling, not an absolute figure): the queued request's own
    // compute cost stays within an order of magnitude of the baseline's, plus a fixed epsilon so
    // a near-zero baseline (a handful of microseconds, quite possible for this fixture's tiny
    // corpus) cannot make the ratio unstable.
    assert!(
        queued_server_us < baseline_server_us.max(2_000) * 10,
        "server_us should stay close to the unqueued baseline: queued={queued_server_us}us \
         baseline={baseline_server_us}us"
    );
}

fn header_u64(resp: &reqwest::Response, name: &str) -> u64 {
    resp.headers()
        .get(name)
        .unwrap_or_else(|| panic!("response is missing the {name} header"))
        .to_str()
        .unwrap()
        .parse()
        .unwrap_or_else(|_| panic!("{name} header is not a valid u64"))
}

// ---------------------------------------------------------------------------------------------
// Cooperative cancellation wired to client disconnect (the rapid-pan case).
// ---------------------------------------------------------------------------------------------

/// A slow viewport request engineered to spread its cost across MANY tiles rather than
/// [`slow_viewport_body`]'s one giant tile. The per-tile cancellation check sits at the top of
/// the tile loop — it is deliberately not checked mid-tile (a tile's own underlay sweep is
/// bounded, in-flight work, same as every other per-tile stage) — so a single-tile fixture like
/// `slow_viewport_body` (`zoom = 0`) cannot demonstrate early interruption at all: cancellation
/// would only ever be observed once that one tile's entire sweep has already finished, which is
/// indistinguishable from no cancellation. `zoom = 2` gives 16 tiles; `underlay_offset = 9` costs
/// ~262144 sub-cell evaluations per tile (~4.2M total, tens of tiles' worth of real work), so a
/// disconnect landing after any prefix of tiles releases the gate long before the rest would have
/// run.
fn slow_multi_tile_viewport_body() -> serde_json::Value {
    serde_json::json!({
        "view": "s0", "zoom": 2, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 1,
        "underlay_offset": 9
    })
}

/// Warm-session scope: a client that drops its connection mid-viewport — the rapid-pan case
/// — releases the compute-admission gate's permit well before the full service time an
/// uncancelled request of the same shape takes. Observed two ways: directly, via `/control/
/// status`'s `compute.in_flight` gauge dropping back to 0 promptly rather than only once the full
/// sweep would naturally finish; and indirectly, via a follow-up request being admitted at once
/// instead of shed.
///
/// **Warm-session scope, deliberately.** A single-flight row-projection build is non-cancellable
/// bounded work by design: its result serves later arrivals, so it always runs to completion. A
/// COLD first viewport's build cost would dominate this test's timing regardless of cancellation
/// and would prove nothing about the per-tile checks. A fast warm-up request first, on the SAME token, gets this token/view's row projection
/// to `Ready` before either slow request below, so the slow request's cost is entirely its
/// (cancellation-interruptible, per-tile) [`slow_multi_tile_viewport_body`] sweep.
///
/// **Self-scaling, not a fixed wall-clock bet** — same pattern as this file's other slow-viewport
/// tests (see e.g. `healthz_stays_prompt_while_a_long_viewport_runs`'s doc): `baseline_elapsed` is
/// this run's own measured time for the full, uncancelled sweep to complete on this machine, and
/// the disconnected run's release time is compared against a fraction of it, never an absolute
/// figure.
///
/// **Why `slow_task.abort()` is a faithful stand-in for a real client disconnect.** Aborting the
/// tokio task driving the `reqwest` request drops that request's future at its next await point —
/// which drops the underlying (not-yet-complete) connection, the same event a real browser
/// tearing down a stale fetch produces. On the server side this is indistinguishable from any
/// other broken connection: axum/hyper notice the peer went away and drop the handler's own
/// future, which is the ONLY signal this transport gives for "the client left" and exactly what
/// `CancelGuard` (`tessera-server::viewer`) is wired to.
///
/// **What this test does NOT claim.** Like the engine-level timing test
/// (`cancel_flipped_from_another_thread_aborts_a_long_request_before_it_completes` in
/// `tessera-engine`'s `tests/viewport.rs`), this does not pin down which of `Engine::viewport`'s
/// three checkpoints the disconnect is caught at — `poll_until_in_flight(&server, 1)` only proves
/// the request has been admitted and started running compute, not how far into the sweep it has
/// gotten by the time `abort()` fires. The disconnect could equally land at the pre-compose
/// checkpoint, before any tile. This test's value is observing permit release end to end (the
/// drop-guard flips, SOME checkpoint catches it, the gate frees up) rather than proving the
/// per-tile check specifically fires mid-sweep. Where in the sweep the check sits is a code-review
/// concern, not one this test can settle.
#[tokio::test]
async fn dropping_a_client_connection_mid_viewport_releases_the_gate_promptly() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let server = spawn_server_with_config_and_gate(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        engine_config_for_slow_viewport(),
        ComputeGate::new(1, 0, 250),
    )
    .await;

    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap().to_string();

    // Warm-session scope (see this test's doc): warms this token/view's row-projection cache
    // before either slow request below.
    let warm = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(&token)
        .json(&fast_viewport_body())
        .send()
        .await
        .unwrap();
    assert_eq!(warm.status(), 200);

    // Baseline: the full, uncancelled slow sweep's own wall-clock time on this run/machine, over
    // the now-warm session.
    let baseline_start = std::time::Instant::now();
    let baseline_resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(&token)
        .json(&slow_multi_tile_viewport_body())
        .send()
        .await
        .unwrap();
    let baseline_elapsed = baseline_start.elapsed();
    assert_eq!(baseline_resp.status(), 200);
    assert!(
        baseline_elapsed > std::time::Duration::from_millis(50),
        "the uncancelled baseline finished in {baseline_elapsed:?}, too fast to exercise this \
         test's early-release scenario -- widen the underlay offset"
    );

    // The actual scenario: a second slow request, admitted and genuinely running
    // (`in_flight == 1`, a deterministic poll rather than a guessed delay) before the client
    // disconnects.
    let viewer_url = server.viewer_url("/v1/viewport");
    let client = server.client.clone();
    let slow_token = token.clone();
    let slow_task = tokio::spawn(async move {
        client
            .post(viewer_url)
            .bearer_auth(slow_token)
            .json(&slow_multi_tile_viewport_body())
            .send()
            .await
    });

    poll_until_in_flight(&server, 1).await;

    let release_start = std::time::Instant::now();
    slow_task.abort();
    // The task is cancelled at its next await point -- whether it resolves at all (and with what)
    // depends on exactly where the abort landed; this test only cares about server-side gate
    // state below, so the client-side outcome is discarded either way.
    let _ = slow_task.await;

    // The drop-guard flips the token when axum drops the handler future on disconnect; the
    // engine's per-tile check observes it and aborts; the `spawn_blocking` closure returns `Err`
    // and drops `_gate_permits` -- releasing both `OwnedSemaphorePermit`s well before the full
    // sweep would naturally finish.
    poll_until_in_flight(&server, 0).await;
    let release_elapsed = release_start.elapsed();

    assert!(
        release_elapsed < baseline_elapsed / 2,
        "the gate took {release_elapsed:?} to free its permit after the client disconnected, \
         not meaningfully less than the {baseline_elapsed:?} an uncancelled sweep takes on this \
         run -- the engine does not appear to be aborting on disconnect"
    );

    // Observable via a follow-up request being admitted promptly, not shed with 429.
    let follow_up = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(&token)
        .json(&fast_viewport_body())
        .send()
        .await
        .unwrap();
    assert_eq!(
        follow_up.status(),
        200,
        "the gate's only slot should already be free after the disconnect -- a 429 here would \
         mean the permit leaked past the client's disconnect"
    );
}

// ---------------------------------------------------------------------------------------------
// Concurrency — intra-request rayon parallelism
// ---------------------------------------------------------------------------------------------

/// THE HEADLINE TEST, server-side: the full Arrow response **body** `POST /v1/viewport`
/// returns is byte-for-byte identical whether `serve.compute_threads` is 1 or 8 — the same claim
/// `tessera-engine`'s own
/// `viewport_output_is_byte_identical_at_compute_threads_1_and_8` pins at the engine level,
/// carried one layer further to what a real client actually receives on the wire, through
/// `run_viewport`'s Arrow IPC framing (`viewer.rs`) and axum's response body.
///
/// Two servers, same bundle, differing only in `EngineConfig::compute_threads`; the same
/// authorisation terms (so both sessions see the identical mask) and the identical request body.
/// The compared region is the body **minus its trailer frame** (`streamed-serving.md` §7): the
/// trailer carries wall-clock figures of this specific run and is canonicalised by key set in
/// `decode_viewport_frames` rather than compared by bytes; `x-tessera-server-us` and
/// `x-tessera-admission-us` are likewise timing headers outside the claim. `x-tessera-pin` IS
/// compared -- it is derived from the bundle's own `(prefix, segments_version)`, not from
/// timing, so it must agree too.
///
/// Uses `PARALLEL_HEADLINE_ITEMS` (300,000), not this file's default
/// `N_ITEMS` (1,000), for a genuinely multi-tile, multi-thousand-row request. But item count alone
/// no longer gets this test to the parallel branch at all: `SERIAL_FALLBACK_MAX_ROWS` rose to
/// 500,000,000 in the post-B9 three-scale re-calibration, and a fixture that reaches it is
/// impractical at unit-test scale. Review caught that this left `pool.install` untested end to
/// end. Fixed the same way as the engine-level headline test
/// (`tessera-engine/tests/viewport.rs`): each server's `Engine` has its threshold forced to 0 via
/// `Engine::set_serial_fallback_max_rows_for_test` (`bench-timing`-gated, test-only) BEFORE it is
/// handed to `spawn_server_from_engine`, so both servers genuinely take `pool.install`, differing
/// only in worker count. Without `bench-timing` (the method does not exist there at all) this
/// falls back to comparing the serial fold on both configs — still real byte-equality coverage,
/// just not of the branch this test's name is about; every guard-rail invocation that matters for
/// this specific claim builds with `bench-timing`.
#[tokio::test]
async fn viewport_response_body_is_byte_identical_at_compute_threads_1_and_8() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture_n(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
        PARALLEL_HEADLINE_ITEMS,
    );

    let config_1 = EngineConfig {
        compute_threads: 1,
        ..default_engine_config()
    };
    let config_8 = EngineConfig {
        compute_threads: 8,
        ..default_engine_config()
    };

    let engine_1 = Engine::open(
        &bundle_root,
        &tmp.path().join("cache-1"),
        &tmp.path().join("wal-1.log"),
        Passthrough::new(),
        config_1,
    )
    .expect("engine should open against a freshly built bundle");
    let engine_8 = Engine::open(
        &bundle_root,
        &tmp.path().join("cache-8"),
        &tmp.path().join("wal-8.log"),
        Passthrough::new(),
        config_8,
    )
    .expect("engine should open against a freshly built bundle");
    // Force the genuine parallel branch on both — see this test's doc.
    #[cfg(feature = "bench-timing")]
    {
        engine_1.set_serial_fallback_max_rows_for_test(0);
        engine_8.set_serial_fallback_max_rows_for_test(0);
    }

    let server_1 = spawn_server_from_engine(engine_1, config_1.max_k, generous_test_gate()).await;
    let server_8 = spawn_server_from_engine(engine_8, config_8.max_k, generous_test_gate()).await;

    let auth_1 = authorise(&server_1, &["0"]).await;
    let token_1 = auth_1["token"].as_str().unwrap();
    let auth_8 = authorise(&server_8, &["0"]).await;
    let token_8 = auth_8["token"].as_str().unwrap();

    // zoom=3 over the full extent: 64 candidate tiles, most non-empty over this fixture's
    // `(e*37, e*53) % 1000` scatter across `N_ITEMS = 1000` -- multiple non-empty tiles, so the
    // response's tile-order/point-concatenation ordering is actually exercised, plus an underlay
    // request so that per-tile path runs across tiles too.
    let body = serde_json::json!({
        "view": "s0", "zoom": 3, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 50,
        "underlay_offset": 2
    });

    let resp_1 = server_1
        .client
        .post(server_1.viewer_url("/v1/viewport"))
        .bearer_auth(token_1)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp_1.status(), 200);
    let pin_1 = resp_1
        .headers()
        .get("x-tessera-pin")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let bytes_1 = resp_1.bytes().await.unwrap();

    let resp_8 = server_8
        .client
        .post(server_8.viewer_url("/v1/viewport"))
        .bearer_auth(token_8)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp_8.status(), 200);
    let pin_8 = resp_8
        .headers()
        .get("x-tessera-pin")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let bytes_8 = resp_8.bytes().await.unwrap();

    assert_eq!(
        pin_1, pin_8,
        "the pin must agree -- same bundle, same generation"
    );

    let decoded_1 = decode_viewport_frames(&bytes_1);
    let decoded_8 = decode_viewport_frames(&bytes_8);
    assert!(
        decoded_1.tiles.len() > 1,
        "need more than one non-empty tile to exercise cross-tile ordering, got {}",
        decoded_1.tiles.len()
    );
    assert!(
        !decoded_1.points.is_empty(),
        "the fixture must return some points"
    );

    assert_eq!(
        decoded_1.deterministic_bytes, decoded_8.deterministic_bytes,
        "every frame before the trailer must be byte-for-byte identical regardless of \
         compute_threads -- this is also the statement that the Python differential oracle and \
         the conformance byte-scanner's vectors are unaffected: they consume exactly these bytes \
         and know nothing about compute_threads"
    );
}

/// Server-level twin of
/// `tessera-engine`'s `viewport_output_is_byte_identical_at_compute_threads_1_and_8_with_sparse_empty_tiles`
/// (fix-wave minor: the headline test above, like its engine-level counterpart, never exercises
/// `tile_result`'s `visible == 0 -> Ok(None)` empty-tile skip path). Same trick, no new fixture
/// data: this file's fixture scatter is `(e*37, e*53) % 1000`, a bijection of `e % 1000` onto the
/// 1000×1000 residue lattice, so `N_ITEMS = 1_000` items occupy up to 1,000 distinct locations
/// spread across the full extent -- dense enough at `zoom = 3` (64 candidate tiles) to leave almost
/// every tile non-empty, but at `zoom = 8` (up to 65,536 candidate tiles) sparse enough that most
/// candidate tiles are genuinely empty while a real minority are not.
///
/// Same arrangement as the headline test above: each server's `Engine` has its
/// threshold forced to 0 (`Engine::set_serial_fallback_max_rows_for_test`, `bench-timing`-gated)
/// before being handed to `spawn_server_from_engine`, so both genuinely take `pool.install`. The
/// occupied/empty tile mix this test is actually for (still 1,000 distinct locations, more items
/// stacked on each) is unaffected — see the doc above.
#[tokio::test]
async fn viewport_response_body_is_byte_identical_at_compute_threads_1_and_8_with_sparse_empty_tiles(
) {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture_n(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
        PARALLEL_HEADLINE_ITEMS,
    );

    let config_1 = EngineConfig {
        compute_threads: 1,
        ..default_engine_config()
    };
    let config_8 = EngineConfig {
        compute_threads: 8,
        ..default_engine_config()
    };

    let engine_1 = Engine::open(
        &bundle_root,
        &tmp.path().join("cache-1"),
        &tmp.path().join("wal-1.log"),
        Passthrough::new(),
        config_1,
    )
    .expect("engine should open against a freshly built bundle");
    let engine_8 = Engine::open(
        &bundle_root,
        &tmp.path().join("cache-8"),
        &tmp.path().join("wal-8.log"),
        Passthrough::new(),
        config_8,
    )
    .expect("engine should open against a freshly built bundle");
    #[cfg(feature = "bench-timing")]
    {
        engine_1.set_serial_fallback_max_rows_for_test(0);
        engine_8.set_serial_fallback_max_rows_for_test(0);
    }

    let server_1 = spawn_server_from_engine(engine_1, config_1.max_k, generous_test_gate()).await;
    let server_8 = spawn_server_from_engine(engine_8, config_8.max_k, generous_test_gate()).await;

    let auth_1 = authorise(&server_1, &["0"]).await;
    let token_1 = auth_1["token"].as_str().unwrap();
    let auth_8 = authorise(&server_8, &["0"]).await;
    let token_8 = auth_8["token"].as_str().unwrap();

    let bbox = [0.0, 0.0, 1000.0, 1000.0];
    let zoom = 8;
    let body = serde_json::json!({
        "view": "s0", "zoom": zoom, "bbox": bbox, "k": 50
    });
    let candidate_tiles = tiles_for_bbox(bbox, zoom, &extent()).len();

    let resp_1 = server_1
        .client
        .post(server_1.viewer_url("/v1/viewport"))
        .bearer_auth(token_1)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp_1.status(), 200);
    let bytes_1 = resp_1.bytes().await.unwrap();

    let resp_8 = server_8
        .client
        .post(server_8.viewer_url("/v1/viewport"))
        .bearer_auth(token_8)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp_8.status(), 200);
    let bytes_8 = resp_8.bytes().await.unwrap();

    let decoded_1 = decode_viewport_frames(&bytes_1);
    let decoded_8 = decode_viewport_frames(&bytes_8);
    let tiles = &decoded_1.tiles;
    assert!(
        !tiles.is_empty(),
        "need at least one non-empty tile for this to be a real mixed case, got none"
    );
    assert!(
        tiles.len() < candidate_tiles,
        "need at least one genuinely empty (Ok(None)-skipped) tile among the {candidate_tiles} \
         candidates to exercise the skip path this test is for -- got {} non-empty tiles",
        tiles.len()
    );

    assert_eq!(
        decoded_1.deterministic_bytes, decoded_8.deterministic_bytes,
        "every frame before the trailer must be byte-for-byte identical regardless of \
         compute_threads, including on the mostly-empty-tile Ok(None) skip path"
    );
}

/// **Concurrent viewports on a cold session are all served, off one build** — decision 0058's
/// whole point, at the boundary a client sees.
///
/// A cold session is one whose row projection has not been built. Eight simultaneous viewports on
/// one therefore all miss, one becomes the builder, and the other seven find a `Building` slot.
/// They used to be shed with 429 `backpressure`; they now wait for that build and are served its
/// result.
///
/// **The assertions are unconditional now, and that is the change.** This test previously allowed
/// that no racer might lose, in which case there was no 429 and nothing to check — it could pass
/// vacuously. Waiting removes the branch: every racer is served whatever the interleaving, and
/// `misses == 1` says the eight of them cost one projection build rather than eight. The
/// `shed_total` check stays, because it is what distinguishes this mechanism from the
/// compute-admission gate, and the gate still sheds nothing here.
///
/// The fixture is deliberately larger than [`N_ITEMS`] so the build is wide enough to race.
#[tokio::test]
async fn concurrent_viewports_on_a_cold_session_are_all_served_off_one_build() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture_n(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
        PARALLEL_HEADLINE_ITEMS,
    );
    let server = spawn_server(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    )
    .await;

    // The generous default gate: 48 admission permits against the handful of requests below, so
    // any 429 here would be the single-flight builder's and not the gate's.
    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap().to_string();

    let mut racers = Vec::new();
    for _ in 0..8 {
        let client = server.client.clone();
        let url = server.viewer_url("/v1/viewport");
        let token = token.clone();
        racers.push(tokio::spawn(async move {
            let resp = client
                .post(url)
                .bearer_auth(token)
                .json(&serde_json::json!({
                    "view": "s0", "zoom": 4, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 5
                }))
                .send()
                .await
                .unwrap();
            let status = resp.status();
            let body: serde_json::Value = if status == 429 {
                resp.json().await.unwrap()
            } else {
                serde_json::Value::Null
            };
            (status, body)
        }));
    }

    for racer in racers {
        let (status, body) = racer.await.unwrap();
        assert_eq!(
            status, 200,
            "every racer on a cold session must be served, not shed: {body}"
        );
    }

    let status = control_status(&server).await;
    assert_eq!(
        status["compute"]["shed_total"], 0,
        "the compute gate admitted every request and sheds nothing on this path"
    );
    let cache = &status["row_projection_cache"];
    assert_eq!(
        cache["misses"], 1,
        "eight concurrent racers must cost one projection build, not eight"
    );
    assert_eq!(
        cache["building_refusals"], 0,
        "nothing was refused — the racers waited and were served"
    );
    // Not asserted as exactly seven: a racer that arrives after the publish is an ordinary hit and
    // never waits at all, which is a legitimate interleaving rather than a failure. What is
    // asserted is that hits and waits together account for the other seven, which `misses == 1`
    // above already says.
    assert!(
        cache["waits_satisfied"].as_u64().unwrap() <= 7,
        "a wait cannot be satisfied for a racer that never waited"
    );
}

// ---------------------------------------------------------------------------------------------
// The streamed response (`streamed-serving.md`; contracts §3.2 r26): multi-frame chunking,
// request-order emission, and the frame grammar over real HTTP.
// ---------------------------------------------------------------------------------------------

/// A flush threshold far below one response's bytes produces many point frames, whose payloads
/// concatenate to exactly the single-frame body a generous threshold serves — the server-level
/// face of the engine's collector-equivalence test, over a real socket. The frame grammar
/// (tiles first, trailer last, closed trailer key set, `sum(served) == points`) is enforced by
/// `decode_viewport_frames` on both responses.
#[tokio::test]
async fn a_tiny_flush_threshold_streams_many_point_frames_with_identical_content() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );

    let chunked = spawn_server_with_stream_flush(
        &bundle_root,
        &tmp.path().join("cache-chunked"),
        &tmp.path().join("wal-chunked.log"),
        // 1 KiB: dozens of flushes for this fixture's zoom-3 response.
        1 << 10,
        10_000,
    )
    .await;
    let whole = spawn_server(
        &bundle_root,
        &tmp.path().join("cache-whole"),
        &tmp.path().join("wal-whole.log"),
    )
    .await;

    let body = serde_json::json!({
        "view": "s0", "zoom": 3, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 64
    });
    let mut decoded = Vec::new();
    for server in [&chunked, &whole] {
        let auth = authorise(server, &["0"]).await;
        let token = auth["token"].as_str().unwrap();
        let resp = server
            .client
            .post(server.viewer_url("/v1/viewport"))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        decoded.push(decode_viewport_frames(&resp.bytes().await.unwrap()));
    }
    let (chunked, whole) = (&decoded[0], &decoded[1]);

    assert!(
        chunked.point_frames > 1,
        "a 1 KiB threshold must chunk this response, got {} frame(s)",
        chunked.point_frames
    );
    assert_eq!(
        whole.point_frames, 1,
        "the 1 MiB default must leave this fixture-sized response in one frame"
    );
    // Chunk boundaries are not contract; content is.
    assert_eq!(chunked.tiles, whole.tiles);
    assert_eq!(chunked.served, whole.served);
    assert_eq!(chunked.points, whole.points);
    assert_eq!(chunked.sub_cells, whole.sub_cells);
}

/// The `tiles` request form's response order is the request's own order (contracts §3.2 r26):
/// reversed request, reversed response — with a duplicate in the list dropped at its first
/// occurrence rather than served twice.
#[tokio::test]
async fn viewport_tiles_are_served_in_request_order_with_first_occurrence_dedup() {
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

    // Which depth-2 tiles are non-empty, from a bbox request.
    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "view": "s0", "zoom": 2, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 4
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let all = decode_viewport_frames(&resp.bytes().await.unwrap());
    let mut wanted: Vec<u64> = all.tiles.iter().map(|t| t.0).collect();
    assert!(wanted.len() >= 2, "need at least two non-empty tiles");
    wanted.reverse();

    // The same tiles, explicitly, reversed, with the first tile repeated at the end — the
    // duplicate must be dropped (first occurrence kept), never served twice.
    let mut with_duplicate = wanted.clone();
    with_duplicate.push(wanted[0]);
    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "view": "s0", "zoom": 2, "tiles": with_duplicate, "k": 4
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let out = decode_viewport_frames(&resp.bytes().await.unwrap());
    let got: Vec<u64> = out.tiles.iter().map(|t| t.0).collect();
    assert_eq!(
        got, wanted,
        "the tiles batch must report in the request's order, deduplicated, unsorted"
    );

    // The points follow the tiles: split the bbox response's flat concatenation by its per-tile
    // `served` counts, reverse the groups, and the explicit-order response must match exactly —
    // same points, same within-tile ascending order, opposite tile order.
    let mut groups: Vec<&[(u64, u64)]> = Vec::new();
    let mut at = 0usize;
    for &served in &all.served {
        groups.push(&all.points[at..at + served as usize]);
        at += served as usize;
    }
    let expected: Vec<(u64, u64)> = groups.iter().rev().flat_map(|g| g.iter().copied()).collect();
    assert_eq!(
        out.points, expected,
        "points must concatenate in the request's tile order"
    );
}

/// The emit phase's shed machinery, end to end over a real socket — the three behaviours the
/// implementation review named as dark: a reader that STOPS reading is shed by the write-stall
/// deadline; a reader that DISCONNECTS is shed by the closed channel; and in both cases the
/// `streaming` gauge (slot held, compute released — `streamed-serving.md` §5) returns to zero
/// and the gate goes on serving.
///
/// Uses a 2M-item fixture so the response (~32 MB at these parameters) genuinely overruns the
/// loopback socket buffers, which autotune to ~10 MB combined — a response that fits in kernel
/// buffers "streams" to a stopped reader without the producer ever parking, and the test would
/// pass without touching the shed path at all (measured: the 300k fixture's ~5 MB response did
/// exactly that on first run).
///
/// ## ⊘ This test is environment-sensitive, and the sensitivity is unresolved
///
/// **The premise is that a reader which stops reading creates backpressure.** Where it does not,
/// the producer never parks, the stream completes, and the final assertion here fires with *"a
/// shed stream must never read as a complete response"* — the whole body arriving with its
/// trailer intact.
///
/// Observed 2026-08-16 on a WSL2 host with 48 GB: reproducible failure, having passed repeatedly
/// on the same machine and the **same binary** earlier the same day. What was ruled out:
///
/// - **Not a code regression.** Bisected to `06c7542`, which predates the artifacts frame, and it
///   fails there identically.
/// - **Not kernel socket buffers.** `net.ipv4.tcp_rmem` maxes at 33 554 432 here — not the "~10 MB
///   combined" above, so the stated margin was already zero. Widening the response to ~78 MB
///   (zoom 7, which serves every item rather than `tiles × k` of them) did not change the outcome,
///   which is what rules the kernel out: 2.4× the bytes against a fixed ceiling would have.
///
/// What that leaves is buffering above the socket — the HTTP client draining the body into its own
/// memory while nothing polls it — which no response size defeats. **Fixing it means giving the
/// test a reader that genuinely refuses to consume**, not a larger response and not more patience.
/// Until then this is a known-red test on hosts where it reproduces, and it is a real gap: the
/// stall-shed path it covers is otherwise untested.
#[tokio::test]
async fn a_stalled_or_disconnected_stream_is_shed_and_the_gauge_returns_to_zero() {
    const SHED_FIXTURE_ITEMS: u64 = 2_000_000;
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture_n(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
        SHED_FIXTURE_ITEMS,
    );
    let server = spawn_server_with_stream_flush(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        // Small flushes, so the producer parks on the channel as soon as the client stops.
        1 << 12,
        // A short stall budget, so the shed happens inside the test's patience.
        1_500,
    )
    .await;
    let auth = authorise(&server, &["0"]).await;
    let token = auth["token"].as_str().unwrap();

    let big_request = serde_json::json!({
        "view": "s0", "zoom": 6, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 200
    });

    // Observed through `TestServer::state` after driving the requests over HTTP — the
    // observe-not-drive licence that field's doc grants. `/control/status` publishes the same
    // gauge; the direct read spares the operator credential and a JSON parse per poll.
    let wait_for_streaming = |state: std::sync::Arc<tessera_server::state::AppState>,
                              want: usize,
                              patience_ms: u64| async move {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(patience_ms);
        loop {
            let now = state.compute_gate.status().streaming;
            if now == want {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "streaming gauge stuck at {now}, wanted {want}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    };

    // 1. The stalled reader: take the headers, then stop reading, holding the connection open.
    //    The producer fills the channel and the socket buffers, parks, and the stall deadline
    //    sheds it — observable as the gauge rising and then returning to zero while we still
    //    hold the response.
    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&big_request)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    wait_for_streaming(std::sync::Arc::clone(&server.state), 1, 5_000).await;
    wait_for_streaming(std::sync::Arc::clone(&server.state), 0, 10_000).await;
    // The shed is loud at this end too: reading the held body now must NOT produce a complete
    // response — either the transport aborts, or the bytes end without a trailer.
    match resp.bytes().await {
        Err(_) => {}
        Ok(bytes) => {
            let frames = tessera_wire::split_frames(&bytes);
            let complete = matches!(
                &frames,
                Ok(frames) if frames.last().map(|(k, _)| *k) == Some(tessera_wire::FRAME_TRAILER)
            );
            assert!(
                !complete,
                "a shed stream must never read as a complete response"
            );
        }
    }

    // 2. The disconnecting reader: drop the response as soon as the headers arrive. The body —
    //    and with it the channel receiver and the cancel guard — drops, and the producer's next
    //    send observes the closure immediately.
    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&big_request)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    drop(resp);
    wait_for_streaming(std::sync::Arc::clone(&server.state), 0, 10_000).await;

    // 3. The gate is undamaged: a full request is served and decodes complete.
    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&big_request)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let decoded = decode_viewport_frames(&resp.bytes().await.unwrap());
    assert!(!decoded.points.is_empty());
}
