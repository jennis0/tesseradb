//! `serve.dev_cors_origins` — the dev-only browser seam (MVP client spec §3).
//!
//! Three assertions and no more: absent means no CORS headers at all, a configured origin
//! round-trips including the response headers the viewer's stats panel reads, and an origin that
//! was not configured is refused even while the layer is mounted.
//!
//! The last one is the point of the key having no wildcard: this is a development affordance that
//! lets a browser present a session token and the session credential to this process, so the set
//! of origins that may do so is enumerated or the layer is not mounted at all.

mod common;

use tempfile::TempDir;

use common::*;

/// Build a bundle and serve it, with `dev_cors_origins` as given.
async fn server_with_cors(tmp: &TempDir, origins: Vec<String>) -> TestServer {
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    spawn_server_with_cors(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        origins,
    )
    .await
}

#[tokio::test]
async fn absent_dev_cors_origins_means_no_cors_headers() {
    let tmp = TempDir::new().unwrap();
    let server = server_with_cors(&tmp, Vec::new()).await;
    let auth = authorise(&server, &["0"]).await;

    let resp = server
        .client
        .get(server.viewer_url("/v1/meta"))
        .header("Origin", "http://localhost:5173")
        .bearer_auth(auth["token"].as_str().unwrap())
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert!(
        resp.headers().get("access-control-allow-origin").is_none(),
        "no origin configured must mean no CORS layer at all — a default would let a dev \
         affordance ride into a deployment"
    );
}

#[tokio::test]
async fn a_configured_origin_round_trips_with_the_exposed_headers() {
    let tmp = TempDir::new().unwrap();
    let server = server_with_cors(&tmp, vec!["http://localhost:5173".to_string()]).await;
    let auth = authorise(&server, &["0"]).await;

    let resp = server
        .client
        .get(server.viewer_url("/v1/meta"))
        .header("Origin", "http://localhost:5173")
        .bearer_auth(auth["token"].as_str().unwrap())
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("access-control-allow-origin")
            .expect("the configured origin is allowed")
            .to_str()
            .unwrap(),
        "http://localhost:5173"
    );

    // Without these, `fetch` hides the headers from the page and the viewer's stats panel shows
    // nothing — a failure that reads as "the server emits no timings" rather than as a CORS
    // configuration hiding them.
    let exposed = resp
        .headers()
        .get("access-control-expose-headers")
        .expect("response headers are exposed")
        .to_str()
        .unwrap()
        .to_ascii_lowercase();
    for header in [
        "x-tessera-pin",
        "x-tessera-server-us",
        "x-tessera-admission-us",
        "x-tessera-stage-ns",
    ] {
        assert!(exposed.contains(header), "{header} must be exposed, got: {exposed}");
    }
}

#[tokio::test]
async fn an_unconfigured_origin_is_refused_even_when_cors_is_on() {
    let tmp = TempDir::new().unwrap();
    let server = server_with_cors(&tmp, vec!["http://localhost:5173".to_string()]).await;
    let auth = authorise(&server, &["0"]).await;

    let resp = server
        .client
        .get(server.viewer_url("/v1/meta"))
        .header("Origin", "http://evil.example")
        .bearer_auth(auth["token"].as_str().unwrap())
        .send()
        .await
        .unwrap();

    assert!(
        resp.headers().get("access-control-allow-origin").is_none(),
        "an origin outside the configured list gets no allow-origin header, so a browser refuses \
         to hand the response to the page"
    );
}

#[tokio::test]
async fn the_session_plane_carries_the_layer_too() {
    // The viewer plane alone is not enough: the browser calls `/session/authorise` first, and a
    // CORS failure there means the viewer never gets a token to use on the viewer plane.
    let tmp = TempDir::new().unwrap();
    let server = server_with_cors(&tmp, vec!["http://localhost:5173".to_string()]).await;

    let resp = server
        .client
        .request(
            reqwest::Method::OPTIONS,
            server.session_url("/session/authorise"),
        )
        .header("Origin", "http://localhost:5173")
        .header("Access-Control-Request-Method", "POST")
        .header("Access-Control-Request-Headers", "authorization,content-type")
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.headers()
            .get("access-control-allow-origin")
            .expect("the session plane answers the preflight")
            .to_str()
            .unwrap(),
        "http://localhost:5173"
    );
    let allowed = resp
        .headers()
        .get("access-control-allow-headers")
        .expect("the preflight names the allowed headers")
        .to_str()
        .unwrap()
        .to_ascii_lowercase();
    assert!(allowed.contains("authorization"), "got: {allowed}");
}

#[tokio::test]
async fn the_control_plane_never_carries_the_layer() {
    // The control plane holds the operator credential. No browser has business reaching it, and
    // the key's doc says the layer is viewer/session only — asserted rather than trusted.
    let tmp = TempDir::new().unwrap();
    let server = server_with_cors(&tmp, vec!["http://localhost:5173".to_string()]).await;

    let resp = server
        .client
        .get(server.control_url("/control/status"))
        .header("Origin", "http://localhost:5173")
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();

    assert!(
        resp.headers().get("access-control-allow-origin").is_none(),
        "the control plane must never be reachable from a browser page"
    );
}
