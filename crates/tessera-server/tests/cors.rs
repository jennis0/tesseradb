//! The two browser seams. See `tessera_server::cors`.
//!
//! `serve.dev_cors_origins` is the development affordance: it covers the viewer *and* session
//! planes, because a browser on a laptop calls `/session/authorise` with the deployment's session
//! credential before it can call anything at all. `serve.cors_origins` is the production list
//! (decision 0102): it covers the **viewer plane only**, because that plane's bearer is a token —
//! per-principal, already scoped, already expiring — and the session plane's is the credential
//! that mints tokens, which a browser must never hold.
//!
//! The load-bearing assertion in this file is therefore the negative one:
//! [`the_session_plane_refuses_a_production_origin`]. Everything else here would still pass if
//! `cors_origins` had been wired to both routers.
//!
//! Neither list has a wildcard, on either plane. A wildcard is refused at parse, asserted here
//! against `config::load` rather than only against the parser's own unit tests, because it is the
//! deployment file an operator actually writes.

mod common;

use tempfile::TempDir;

use common::*;

/// The two origins this file uses, one per list. Distinct strings so a test that passes because
/// the wrong list was consulted fails instead.
const DEV_ORIGIN: &str = "http://localhost:5173";
const PRODUCTION_ORIGIN: &str = "https://app.example";

/// The seven response headers a browser client must be able to read: the delta-serving
/// coordinates it keys and revalidates a replica by (`delta-serving.md` §2), the stamp it echoes,
/// the two timings the stats panel shows, and the region leaf's verdict (`selection-operand.md`
/// §6), without which every region count renders inexact.
const EXPOSED: [&str; 7] = [
    "etag",
    "x-tessera-identity-key",
    "x-tessera-stale",
    "x-tessera-pin",
    "x-tessera-server-us",
    "x-tessera-admission-us",
    "x-tessera-region",
];

/// Build a bundle and serve it, with the CORS lists as given.
async fn server_with_cors(tmp: &TempDir, cors: CorsOrigins) -> TestServer {
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
        cors,
    )
    .await
}

/// A browser's preflight: `OPTIONS`, an `Origin`, and the method and headers the real request
/// would carry. The real request never happens if this one is not answered, which is why every
/// plane assertion below is written against it rather than against a `GET`.
async fn preflight(server: &TestServer, url: String, origin: &str) -> reqwest::Response {
    server
        .client
        .request(reqwest::Method::OPTIONS, url)
        .header("Origin", origin)
        .header("Access-Control-Request-Method", "POST")
        .header(
            "Access-Control-Request-Headers",
            "authorization,content-type",
        )
        .send()
        .await
        .unwrap()
}

/// `access-control-expose-headers`, lowercased, or a panic naming what was there instead.
fn exposed_headers(resp: &reqwest::Response) -> String {
    resp.headers()
        .get("access-control-expose-headers")
        .unwrap_or_else(|| panic!("response headers are exposed; got {:?}", resp.headers()))
        .to_str()
        .unwrap()
        .to_ascii_lowercase()
}

// ---- No list ---------------------------------------------------------------------------------

#[tokio::test]
async fn absent_cors_origins_means_no_cors_headers() {
    let tmp = TempDir::new().unwrap();
    let server = server_with_cors(&tmp, CorsOrigins::none()).await;
    let auth = authorise(&server, &["0"]).await;

    let resp = server
        .client
        .get(server.viewer_url("/v1/meta"))
        .header("Origin", DEV_ORIGIN)
        .bearer_auth(auth["token"].as_str().unwrap())
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert!(
        resp.headers().get("access-control-allow-origin").is_none(),
        "no origin configured must mean no CORS layer at all — a default would let a browser seam \
         ride into a deployment nobody asked to open"
    );
}

// ---- The development list --------------------------------------------------------------------

#[tokio::test]
async fn a_configured_dev_origin_round_trips_with_the_exposed_headers() {
    let tmp = TempDir::new().unwrap();
    let server = server_with_cors(&tmp, CorsOrigins::dev(&[DEV_ORIGIN])).await;
    let auth = authorise(&server, &["0"]).await;

    let resp = server
        .client
        .get(server.viewer_url("/v1/meta"))
        .header("Origin", DEV_ORIGIN)
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
        DEV_ORIGIN
    );

    // Without these, `fetch` hides the headers from the page — the timing headers read as "the
    // server emits no timings", and the delta-serving coordinates read as a server that cannot be
    // replicated against at all. `x-tessera-stage-ns` is retired (contracts §3.2 r26 — the stage
    // breakdown rides the trailer frame, in-body and outside CORS's reach) and must NOT reappear.
    let exposed = exposed_headers(&resp);
    for header in EXPOSED {
        assert!(
            exposed.contains(header),
            "{header} must be exposed, got: {exposed}"
        );
    }
    assert!(
        !exposed.contains("x-tessera-stage-ns"),
        "the retired stage header must not be resurrected in the expose list: {exposed}"
    );
}

#[tokio::test]
async fn an_unconfigured_origin_is_refused_even_when_cors_is_on() {
    let tmp = TempDir::new().unwrap();
    let server = server_with_cors(&tmp, CorsOrigins::dev(&[DEV_ORIGIN])).await;
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
async fn the_session_plane_carries_the_dev_layer_too() {
    // The viewer plane alone is not enough for the *development* seam: the browser calls
    // `/session/authorise` first, and a CORS failure there means the viewer never gets a token to
    // use on the viewer plane. This is what the production list deliberately does not do.
    let tmp = TempDir::new().unwrap();
    let server = server_with_cors(&tmp, CorsOrigins::dev(&[DEV_ORIGIN])).await;

    let resp = preflight(
        &server,
        server.session_url("/session/authorise"),
        DEV_ORIGIN,
    )
    .await;

    assert_eq!(
        resp.headers()
            .get("access-control-allow-origin")
            .expect("the session plane answers the preflight")
            .to_str()
            .unwrap(),
        DEV_ORIGIN
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

// ---- The production list (decision 0102) -------------------------------------------------------

#[tokio::test]
async fn a_listed_production_origin_gets_its_viewer_preflight() {
    let tmp = TempDir::new().unwrap();
    let server = server_with_cors(&tmp, CorsOrigins::production(&[PRODUCTION_ORIGIN])).await;

    let resp = preflight(
        &server,
        server.viewer_url("/v1/viewport"),
        PRODUCTION_ORIGIN,
    )
    .await;

    assert_eq!(
        resp.headers()
            .get("access-control-allow-origin")
            .expect("the viewer plane answers a listed production origin's preflight")
            .to_str()
            .unwrap(),
        PRODUCTION_ORIGIN
    );

    // The token rides `Authorization`. If the preflight does not name it, the browser never sends
    // the real request and the drop-in fails before it has made one call.
    let allowed = resp
        .headers()
        .get("access-control-allow-headers")
        .expect("the preflight names the allowed headers")
        .to_str()
        .unwrap()
        .to_ascii_lowercase();
    assert!(allowed.contains("authorization"), "got: {allowed}");

    // Credentials mode is deliberately off: the token travels in a header, never a cookie, and
    // turning it on would have the browser attach ambient credentials to every viewer call.
    assert!(
        resp.headers()
            .get("access-control-allow-credentials")
            .is_none(),
        "credentials mode must stay off — the bearer is a header, not a cookie"
    );
}

#[tokio::test]
async fn a_listed_production_origin_reads_the_six_replica_headers() {
    let tmp = TempDir::new().unwrap();
    let server = server_with_cors(&tmp, CorsOrigins::production(&[PRODUCTION_ORIGIN])).await;
    let auth = authorise(&server, &["0"]).await;

    let resp = server
        .client
        .get(server.viewer_url("/v1/meta"))
        .header("Origin", PRODUCTION_ORIGIN)
        .bearer_auth(auth["token"].as_str().unwrap())
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("access-control-allow-origin")
            .expect("a listed production origin is allowed on the viewer plane")
            .to_str()
            .unwrap(),
        PRODUCTION_ORIGIN
    );

    // A drop-in that cannot read these cannot key a replica, cannot revalidate one, and shows an
    // empty stats panel — the failure reads as a server that emits nothing rather than as a CORS
    // list that hides everything.
    let exposed = exposed_headers(&resp);
    for header in EXPOSED {
        assert!(
            exposed.contains(header),
            "{header} must be exposed to a production origin, got: {exposed}"
        );
    }
}

#[tokio::test]
async fn an_unlisted_origin_is_refused_against_the_production_list() {
    let tmp = TempDir::new().unwrap();
    let server = server_with_cors(&tmp, CorsOrigins::production(&[PRODUCTION_ORIGIN])).await;
    let auth = authorise(&server, &["0"]).await;

    let resp = server
        .client
        .get(server.viewer_url("/v1/meta"))
        .header("Origin", "https://evil.example")
        .bearer_auth(auth["token"].as_str().unwrap())
        .send()
        .await
        .unwrap();

    assert!(
        resp.headers().get("access-control-allow-origin").is_none(),
        "the production list is enumerated: an origin outside it gets nothing, on the plane the \
         list does cover"
    );
}

/// **The load-bearing half of decision 0102.**
///
/// `serve.cors_origins` names pages that may present a *token*. `/session/authorise` is gated by
/// the session credential — the shared secret that decides who may mint tokens at all — and a
/// browser must never hold one. Wiring the production list into the session router would make
/// every listed origin a page that could be asked to carry that credential, which is the thing the
/// decision declined rather than deferred.
#[tokio::test]
async fn the_session_plane_refuses_a_production_origin() {
    let tmp = TempDir::new().unwrap();
    let server = server_with_cors(&tmp, CorsOrigins::production(&[PRODUCTION_ORIGIN])).await;

    let resp = preflight(
        &server,
        server.session_url("/session/authorise"),
        PRODUCTION_ORIGIN,
    )
    .await;

    assert!(
        resp.headers().get("access-control-allow-origin").is_none(),
        "serve.cors_origins must not reach the session plane: the credential that mints tokens is \
         not a thing a browser may be handed (decision 0102)"
    );
}

#[tokio::test]
async fn both_lists_may_be_set_and_only_the_dev_one_opens_the_session_plane() {
    let tmp = TempDir::new().unwrap();
    let server = server_with_cors(
        &tmp,
        CorsOrigins {
            dev: vec![DEV_ORIGIN.to_string()],
            production: vec![PRODUCTION_ORIGIN.to_string()],
            loopback: false,
        },
    )
    .await;

    // Both reach the viewer plane.
    for origin in [DEV_ORIGIN, PRODUCTION_ORIGIN] {
        let resp = preflight(&server, server.viewer_url("/v1/viewport"), origin).await;
        assert_eq!(
            resp.headers()
                .get("access-control-allow-origin")
                .unwrap_or_else(|| panic!("{origin} must be allowed on the viewer plane"))
                .to_str()
                .unwrap(),
            origin
        );
    }

    // Only the development one reaches the session plane.
    let resp = preflight(
        &server,
        server.session_url("/session/authorise"),
        DEV_ORIGIN,
    )
    .await;
    assert!(resp.headers().get("access-control-allow-origin").is_some());
    let resp = preflight(
        &server,
        server.session_url("/session/authorise"),
        PRODUCTION_ORIGIN,
    )
    .await;
    assert!(
        resp.headers().get("access-control-allow-origin").is_none(),
        "setting both lists must not launder the production origin onto the session plane"
    );
}

// ---- The loopback rule (`serve.cors_loopback`) -------------------------------------------------

/// A page whose origin is a port the kernel chose cannot be enumerated in advance
/// (`python-sdk.md` §7), so the key states the set instead. What it admits is the same thing a
/// listed origin is admitted for: a page that may present a **token**.
#[tokio::test]
async fn a_loopback_origin_is_admitted_on_the_viewer_plane_with_the_exposed_headers() {
    let tmp = TempDir::new().unwrap();
    let server = server_with_cors(&tmp, CorsOrigins::loopback()).await;
    let auth = authorise(&server, &["0"]).await;

    // A port no list names, which is the case the key exists for.
    let origin = "http://localhost:41273";
    let resp = server
        .client
        .get(server.viewer_url("/v1/meta"))
        .header("Origin", origin)
        .bearer_auth(auth["token"].as_str().unwrap())
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("access-control-allow-origin")
            .expect("a loopback origin is admitted under serve.cors_loopback")
            .to_str()
            .unwrap(),
        origin
    );
    let exposed = exposed_headers(&resp);
    for header in EXPOSED {
        assert!(
            exposed.contains(header),
            "{header} must be exposed to a loopback origin too, got: {exposed}"
        );
    }
    assert!(
        resp.headers()
            .get("access-control-allow-credentials")
            .is_none(),
        "credentials mode must stay off for a loopback origin as for every other"
    );
}

#[tokio::test]
async fn every_loopback_spelling_answers_a_viewer_preflight() {
    let tmp = TempDir::new().unwrap();
    let server = server_with_cors(&tmp, CorsOrigins::loopback()).await;

    for origin in [
        "http://localhost:5173",
        "https://localhost:8888",
        "http://127.0.0.1:9000",
        "http://[::1]:9000",
    ] {
        let resp = preflight(&server, server.viewer_url("/v1/viewport"), origin).await;
        assert_eq!(
            resp.headers()
                .get("access-control-allow-origin")
                .unwrap_or_else(|| panic!("{origin} is a loopback origin"))
                .to_str()
                .unwrap(),
            origin
        );
    }
}

#[tokio::test]
async fn a_non_loopback_origin_is_refused_under_the_loopback_rule() {
    let tmp = TempDir::new().unwrap();
    let server = server_with_cors(&tmp, CorsOrigins::loopback()).await;
    let auth = authorise(&server, &["0"]).await;

    for origin in ["https://app.example", "http://192.168.0.35:5173"] {
        let resp = server
            .client
            .get(server.viewer_url("/v1/meta"))
            .header("Origin", origin)
            .bearer_auth(auth["token"].as_str().unwrap())
            .send()
            .await
            .unwrap();
        assert!(
            resp.headers().get("access-control-allow-origin").is_none(),
            "the rule is loopback and nothing else, but {origin} was admitted"
        );
    }
}

/// **The host is matched whole.** `localhost.evil.example` is a name anybody can register and
/// serve from anywhere; a prefix match would hand it every token a viewer holds.
#[tokio::test]
async fn a_host_that_merely_begins_with_a_loopback_name_is_refused() {
    let tmp = TempDir::new().unwrap();
    let server = server_with_cors(&tmp, CorsOrigins::loopback()).await;

    for origin in [
        "http://localhost.evil.example",
        "https://localhost.evil.example:8443",
        "http://127.0.0.1.evil.example",
        "http://127.0.0.1.evil.example:5173",
    ] {
        let resp = preflight(&server, server.viewer_url("/v1/viewport"), origin).await;
        assert!(
            resp.headers().get("access-control-allow-origin").is_none(),
            "{origin} is not a loopback origin and must be refused"
        );
    }
}

/// The loopback rule stops where the production list stops, and for the same reason: the session
/// plane's bearer is the credential that mints tokens (decision 0102). A page on a laptop is
/// still a browser page.
#[tokio::test]
async fn the_session_plane_never_admits_a_loopback_origin() {
    let tmp = TempDir::new().unwrap();
    let server = server_with_cors(&tmp, CorsOrigins::loopback()).await;

    let resp = preflight(
        &server,
        server.session_url("/session/authorise"),
        "http://localhost:5173",
    )
    .await;

    assert!(
        resp.headers().get("access-control-allow-origin").is_none(),
        "serve.cors_loopback must not reach the session plane"
    );
}

#[tokio::test]
async fn the_loopback_rule_admits_a_listed_origin_beside_it() {
    let tmp = TempDir::new().unwrap();
    let server = server_with_cors(
        &tmp,
        CorsOrigins {
            dev: Vec::new(),
            production: vec![PRODUCTION_ORIGIN.to_string()],
            loopback: true,
        },
    )
    .await;

    for origin in [PRODUCTION_ORIGIN, "http://127.0.0.1:7777"] {
        let resp = preflight(&server, server.viewer_url("/v1/viewport"), origin).await;
        assert_eq!(
            resp.headers()
                .get("access-control-allow-origin")
                .unwrap_or_else(|| panic!("{origin} must be admitted"))
                .to_str()
                .unwrap(),
            origin
        );
    }

    // And the session plane still admits neither.
    for origin in [PRODUCTION_ORIGIN, "http://127.0.0.1:7777"] {
        let resp = preflight(&server, server.session_url("/session/authorise"), origin).await;
        assert!(
            resp.headers().get("access-control-allow-origin").is_none(),
            "{origin} must not reach the session plane"
        );
    }
}

// ---- The control plane -------------------------------------------------------------------------

#[tokio::test]
async fn the_control_plane_never_carries_a_layer() {
    // The control plane holds the operator credential. No browser has business reaching it, and
    // the module's doc says the layer is viewer/session only — asserted rather than trusted, for
    // both lists, since either being wired there would be the same defect.
    let tmp = TempDir::new().unwrap();
    let server = server_with_cors(
        &tmp,
        CorsOrigins {
            dev: vec![DEV_ORIGIN.to_string()],
            production: vec![PRODUCTION_ORIGIN.to_string()],
            loopback: true,
        },
    )
    .await;

    for origin in [DEV_ORIGIN, PRODUCTION_ORIGIN, "http://localhost:5173"] {
        let resp = server
            .client
            .get(server.control_url("/control/status"))
            .header("Origin", origin)
            .bearer_auth(OPERATOR_CREDENTIAL)
            .send()
            .await
            .unwrap();

        assert!(
            resp.headers().get("access-control-allow-origin").is_none(),
            "the control plane must never be reachable from a browser page ({origin})"
        );
    }
}

// ---- Parse -------------------------------------------------------------------------------------

/// A wildcard is refused where an operator would actually write one: in `tessera.toml`, through
/// `config::load`. Enumerated or absent is decision 0102's whole shape, and the alternative to a
/// refusal is a server that starts, reports nothing, and serves no CORS to the operator who asked
/// for all of it.
#[test]
fn a_wildcard_origin_is_refused_when_the_deployment_file_is_loaded() {
    let tmp = TempDir::new().unwrap();
    std::env::set_var("TESSERA_TEST_CORS_SESSION", SESSION_CREDENTIAL);
    std::env::set_var("TESSERA_TEST_CORS_OPERATOR", OPERATOR_CREDENTIAL);

    let write = |extra: &str| {
        let text = format!(
            r#"
            [bundle]
            path = "b"
            cache = "c"
            wal = "w"
            [plugin]
            module = "builtin:passthrough"
            [disclosure]
            token_max_lifetime = 3600
            [serve]
            viewer = "127.0.0.1:7407"
            session = "127.0.0.1:7408"
            control = "127.0.0.1:7409"
            session_credential_env = "TESSERA_TEST_CORS_SESSION"
            operator_credential_env = "TESSERA_TEST_CORS_OPERATOR"
            {extra}
            "#
        );
        let path = tmp.path().join("tessera.toml");
        std::fs::write(&path, text).unwrap();
        path
    };

    let path = write("cors_origins = [\"*\"]");
    let err = tessera_server::config::load(&path).expect_err("a wildcard must refuse to start");
    let message = err.to_string();
    assert!(
        message.contains("cors_origins") && message.contains('*'),
        "the refusal must name the key and the value: {message}"
    );

    let path = write("dev_cors_origins = [\"*\"]");
    let err = tessera_server::config::load(&path).expect_err("a wildcard must refuse to start");
    assert!(
        err.to_string().contains("dev_cors_origins"),
        "the development list is enumerated on the same terms: {err}"
    );

    // The same file with named origins loads, so the test above is about the wildcard and not
    // about the fixture being wrong.
    let path = write("cors_origins = [\"https://app.example\"]");
    let config = tessera_server::config::load(&path).expect("named origins must load");
    assert_eq!(config.cors_origins, vec!["https://app.example"]);
}
