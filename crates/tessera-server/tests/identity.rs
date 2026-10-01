//! Principals, credentials and sessions, through the three listeners: what a session holds, which
//! catalogue changes end which sessions, OIDC access tokens, key expiry, and the catalogue across
//! a restart.

mod common;

use std::sync::Arc;

use base64::Engine as _;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde_json::{json, Value};
use tempfile::TempDir;

use common::*;
use tessera_server::state::now_secs;

const PASSWORD: &str = "correct horse battery staple";

async fn control(server: &TestServer, method: reqwest::Method, path: &str, body: Value) -> Value {
    let resp = server
        .client
        .request(method.clone(), server.control_url(path))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap();
    assert_eq!(status, 200, "{method} {path}: {text}");
    serde_json::from_str(&text).unwrap()
}

/// Creates a person holding `read`, the terms given and the password [`PASSWORD`].
async fn person(server: &TestServer, name: &str, terms: &[&str]) {
    let post = reqwest::Method::POST;
    control(server, post.clone(), "/control/principals", json!({ "name": name, "kind": "person" })).await;
    control(
        server,
        reqwest::Method::PUT,
        &format!("/control/principals/{name}/password"),
        json!({ "password": PASSWORD }),
    )
    .await;
    control(server, post.clone(), "/control/grants", json!({ "principal": name, "permission": "read" })).await;
    for term in terms {
        control(server, post.clone(), "/control/grants", json!({ "principal": name, "term": term })).await;
    }
}

async fn login(server: &TestServer, body: Value) -> reqwest::Response {
    server
        .client
        .post(server.viewer_url("/v1/login"))
        .json(&body)
        .send()
        .await
        .unwrap()
}

async fn login_password(server: &TestServer, name: &str) -> String {
    let resp = login(
        server,
        json!({ "password": { "principal": name, "password": PASSWORD } }),
    )
    .await;
    assert_eq!(resp.status(), 200);
    resp.json::<Value>().await.unwrap()["token"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn authorise_as(server: &TestServer, key: &str, principal: &str) -> String {
    let resp = server
        .client
        .post(server.session_url("/session/authorise"))
        .bearer_auth(key)
        .json(&json!({ "principal": principal }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    resp.json::<Value>().await.unwrap()["token"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// The status `/v1/meta` answers for `token`: 200 while its session lives, 403 once it has ended.
async fn meta_status(server: &TestServer, token: &str) -> u16 {
    server
        .client
        .get(server.viewer_url("/v1/meta"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

/// How many items `token`'s session may see over the whole fixture.
async fn visible(server: &TestServer, token: &str) -> u64 {
    let resp = server
        .client
        .post(server.viewer_url("/v1/viewport"))
        .bearer_auth(token)
        .json(&json!({ "view": "s0", "zoom": 0, "bbox": [0.0, 0.0, 1000.0, 1000.0], "k": 1 }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let (tiles, _) = decode_viewport(&resp.bytes().await.unwrap());
    tiles.iter().map(|(_, visible, _)| visible).sum()
}

#[tokio::test]
async fn a_session_holds_exactly_the_terms_granted_to_its_principal() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    person(&server, "ann", &["1"]).await;
    let narrow = login_password(&server, "ann").await;
    // Every third item carries `1`, and every item carries `0`.
    assert_eq!(visible(&server, &narrow).await, N_ITEMS.div_ceil(3));

    control(&server, reqwest::Method::POST, "/control/grants", json!({ "principal": "ann", "term": "0" })).await;
    let wide = login_password(&server, "ann").await;
    assert_eq!(visible(&server, &wide).await, N_ITEMS);

    // A group's terms reach its members.
    person(&server, "bo", &[]).await;
    assert_eq!(visible(&server, &login_password(&server, "bo").await).await, 0);
    let post = reqwest::Method::POST;
    control(&server, post.clone(), "/control/groups", json!({ "name": "g" })).await;
    control(&server, reqwest::Method::PUT, "/control/groups/g/members/bo", json!({})).await;
    control(&server, post, "/control/grants", json!({ "group": "g", "term": "1" })).await;
    assert_eq!(
        visible(&server, &login_password(&server, "bo").await).await,
        N_ITEMS.div_ceil(3)
    );
}

#[tokio::test]
async fn a_catalogue_change_ends_exactly_the_sessions_it_affects() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    let post = reqwest::Method::POST;
    person(&server, "ann", &["0"]).await;
    person(&server, "bob", &["0"]).await;

    // A grant to ann ends her login and the session minted for her, and leaves bob's.
    let ann = login_password(&server, "ann").await;
    let ann_minted = authorise_as(&server, &server.integrator_key, "ann").await;
    let bob = login_password(&server, "bob").await;
    let change = control(&server, post.clone(), "/control/grants", json!({ "principal": "ann", "term": "1" })).await;
    assert_eq!(change["sessions_ended"], 2);
    assert_eq!(meta_status(&server, &ann).await, 403);
    assert_eq!(meta_status(&server, &ann_minted).await, 403);
    assert_eq!(meta_status(&server, &bob).await, 200);

    // A grant already held changes nothing and ends nothing.
    let ann = login_password(&server, "ann").await;
    let change = control(&server, post.clone(), "/control/grants", json!({ "principal": "ann", "term": "1" })).await;
    assert_eq!(change["sessions_ended"], 0);
    assert_eq!(meta_status(&server, &ann).await, 200);

    // Setting a password ends the principal's sessions.
    control(
        &server,
        reqwest::Method::PUT,
        "/control/principals/bob/password",
        json!({ "password": PASSWORD }),
    )
    .await;
    assert_eq!(meta_status(&server, &bob).await, 403);

    // Revoking a key ends the sessions it authenticated and those it minted.
    let issued = control(&server, post.clone(), "/control/principals/ann/keys", json!({})).await;
    let by_key = login(&server, json!({ "api_key": issued["key"] })).await;
    let by_key = by_key.json::<Value>().await.unwrap()["token"].as_str().unwrap().to_owned();
    let bob_minted = authorise_as(&server, &server.integrator_key, "bob").await;
    let prefix = issued["prefix"].as_str().unwrap();
    let change = control(&server, reqwest::Method::DELETE, &format!("/control/keys/{prefix}"), json!({})).await;
    assert_eq!(change["sessions_ended"], 1);
    assert_eq!(meta_status(&server, &by_key).await, 403);
    assert_eq!(meta_status(&server, &ann).await, 200);
    let keys = control(&server, reqwest::Method::GET, &format!("/control/principals/{INTEGRATOR}/keys"), json!({})).await;
    let integrator_prefix = keys["keys"][0]["prefix"].as_str().unwrap().to_owned();
    control(&server, reqwest::Method::DELETE, &format!("/control/keys/{integrator_prefix}"), json!({})).await;
    assert_eq!(meta_status(&server, &bob_minted).await, 403);

    // A change to a group ends its members' sessions.
    control(&server, post.clone(), "/control/groups", json!({ "name": "g" })).await;
    control(&server, reqwest::Method::PUT, "/control/groups/g/members/ann", json!({})).await;
    let ann = login_password(&server, "ann").await;
    control(&server, post.clone(), "/control/grants", json!({ "group": "g", "term": "2" })).await;
    assert_eq!(meta_status(&server, &ann).await, 403);

    // Disabling a principal ends its sessions and refuses its credentials.
    let ann = login_password(&server, "ann").await;
    control(&server, reqwest::Method::PATCH, "/control/principals/ann", json!({ "disabled": true })).await;
    assert_eq!(meta_status(&server, &ann).await, 403);
    let resp = login(&server, json!({ "password": { "principal": "ann", "password": PASSWORD } })).await;
    assert_eq!(resp.status(), 401);

    // Ending a principal's sessions from the control plane.
    let bob = login_password(&server, "bob").await;
    let listed = control(&server, reqwest::Method::GET, "/control/sessions?principal=bob", json!({})).await;
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 1);
    let change = control(&server, post, "/control/sessions/end", json!({ "principal": "bob" })).await;
    assert_eq!(change["sessions_ended"], 1);
    assert_eq!(meta_status(&server, &bob).await, 403);
}

#[tokio::test]
async fn a_session_ends_no_later_than_the_key_that_authorised_it() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    person(&server, "ann", &["0"]).await;
    let expiry = now_secs() + 600;
    let issued = control(
        &server,
        reqwest::Method::POST,
        "/control/principals/ann/keys",
        json!({ "expires_at": expiry }),
    )
    .await;
    let resp = login(&server, json!({ "api_key": issued["key"] })).await;
    assert_eq!(resp.json::<Value>().await.unwrap()["expires_at"], expiry);

    // A key that has expired is refused.
    let issued = control(
        &server,
        reqwest::Method::POST,
        "/control/principals/ann/keys",
        json!({ "expires_at": now_secs() - 1 }),
    )
    .await;
    let resp = login(&server, json!({ "api_key": issued["key"] })).await;
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn a_write_needs_write_and_bypass() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    let post = reqwest::Method::POST;
    control(&server, post.clone(), "/control/principals", json!({ "name": "pipeline", "kind": "service" })).await;
    control(&server, post.clone(), "/control/grants", json!({ "principal": "pipeline", "permission": "write" })).await;
    let key = control(&server, post.clone(), "/control/principals/pipeline/keys", json!({})).await;
    let key = key["key"].as_str().unwrap().to_owned();
    let flush = |key: String| {
        let server = &server;
        async move {
            server
                .client
                .post(server.control_url("/control/changes"))
                .bearer_auth(key)
                .json(&json!([{ "op": "suppress", "match": { "id": member(3) } }]))
                .send()
                .await
                .unwrap()
                .status()
                .as_u16()
        }
    };
    assert_eq!(flush(key.clone()).await, 403);
    control(&server, reqwest::Method::PATCH, "/control/principals/pipeline", json!({ "bypass": true })).await;
    assert_eq!(flush(key).await, 200);
}

/// A test identity provider: an Ed25519 key published at a loopback JWKS URL.
struct Idp {
    url: String,
    keys: Arc<parking_lot::Mutex<Value>>,
    signers: Vec<(String, Vec<u8>, Value)>,
    _task: tokio::task::JoinHandle<()>,
}

impl Idp {
    async fn start() -> Idp {
        let keys = Arc::new(parking_lot::Mutex::new(json!({ "keys": [] })));
        let served = Arc::clone(&keys);
        let app = axum::Router::new().route(
            "/keys",
            axum::routing::get(move || {
                let served = Arc::clone(&served);
                async move { axum::Json(served.lock().clone()) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/keys", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let mut idp = Idp {
            url,
            keys,
            signers: Vec::new(),
            _task: task,
        };
        idp.add_key("k1");
        idp.publish(&["k1"]);
        idp
    }

    fn add_key(&mut self, kid: &str) {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let jwk = json!({
            "kty": "OKP",
            "crv": "Ed25519",
            "kid": kid,
            "alg": "EdDSA",
            "use": "sig",
            "x": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(pair.public_key().as_ref()),
        });
        self.signers.push((kid.to_owned(), pkcs8.as_ref().to_vec(), jwk));
    }

    fn publish(&self, kids: &[&str]) {
        let keys: Vec<Value> = self
            .signers
            .iter()
            .filter(|(kid, _, _)| kids.contains(&kid.as_str()))
            .map(|(_, _, jwk)| jwk.clone())
            .collect();
        *self.keys.lock() = json!({ "keys": keys });
    }

    fn token(&self, kid: &str, claims: Value) -> String {
        let (_, der, _) = self.signers.iter().find(|(k, _, _)| k == kid).unwrap();
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some(kid.to_owned());
        jsonwebtoken::encode(&header, &claims, &EncodingKey::from_ed_der(der)).unwrap()
    }
}

const ISSUER: &str = "https://login.example.org";

fn claims(groups: &[&str], exp: u64) -> Value {
    json!({ "iss": ISSUER, "aud": "tessera", "sub": "u-1", "exp": exp, "groups": groups })
}

#[tokio::test(flavor = "multi_thread")]
async fn an_oidc_identity_logs_in_and_administers_through_its_role_mappings() {
    let tmp = TempDir::new().unwrap();
    let server = serve(&tmp).await;
    let mut idp = Idp::start().await;
    let post = reqwest::Method::POST;
    for (group, permissions) in [("readers", &["read"][..]), ("admins", &["read", "admin"][..])] {
        control(&server, post.clone(), "/control/groups", json!({ "name": group })).await;
        for p in permissions {
            control(&server, post.clone(), "/control/grants", json!({ "group": group, "permission": p })).await;
        }
    }
    let provider = json!({
        "issuer": ISSUER,
        "audience": "tessera",
        "jwks_url": idp.url,
        "claim_rules": [{ "claim": "groups[*]", "template": "{value}" }],
        "role_mappings": [
            { "claim": "groups[*]", "value": "tessera-readers", "group": "readers" },
            { "claim": "groups[*]", "value": "tessera-admins", "group": "admins" },
        ],
    });
    control(&server, reqwest::Method::PUT, "/control/providers/corp", provider.clone()).await;

    // A reader's claim `1` is a term, and the mapping gives `read`. Its session ends at `exp`.
    let exp = now_secs() + 300;
    let token = idp.token("k1", claims(&["1", "tessera-readers"], exp));
    let resp = login(&server, json!({ "access_token": token })).await;
    assert_eq!(resp.status(), 200);
    let answer: Value = resp.json().await.unwrap();
    assert_eq!(answer["expires_at"], exp);
    let session = answer["token"].as_str().unwrap().to_owned();
    assert_eq!(visible(&server, &session).await, N_ITEMS.div_ceil(3));

    // An identity no mapping gives `read` is refused a session, and one whose token fails any
    // check is refused as any bad credential is.
    let resp = login(&server, json!({ "access_token": idp.token("k1", claims(&["1"], exp)) })).await;
    assert_eq!(resp.status(), 403);
    let mut wrong_audience = claims(&["tessera-readers"], exp);
    wrong_audience["aud"] = json!("someone-else");
    let mut no_subject = claims(&["tessera-readers"], exp);
    no_subject.as_object_mut().unwrap().remove("sub");
    let hs256 = jsonwebtoken::encode(
        &Header::new(Algorithm::HS256),
        &claims(&["tessera-readers"], exp),
        &EncodingKey::from_secret(b"anyone can sign this"),
    )
    .unwrap();
    for bad in [
        idp.token("k1", wrong_audience),
        idp.token("k1", no_subject),
        idp.token("k1", claims(&["tessera-readers"], now_secs() - 1)),
        hs256,
    ] {
        let resp = login(&server, json!({ "access_token": bad })).await;
        assert_eq!(resp.status(), 401);
    }

    // An admin mapping reaches the catalogue on the control plane with the token as its bearer,
    // and no write.
    let admin = idp.token("k1", claims(&["tessera-admins"], exp));
    let resp = server
        .client
        .get(server.control_url("/control/principals"))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let resp = server
        .client
        .post(server.control_url("/control/flush"))
        .bearer_auth(idp.token("k1", claims(&["tessera-readers"], exp)))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);

    // An integrator mints a session for the identity by passing its token on.
    let resp = server
        .client
        .post(server.session_url("/session/authorise"))
        .bearer_auth(&server.integrator_key)
        .json(&json!({ "access_token": idp.token("k1", claims(&["0", "tessera-readers"], exp)) }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let minted = resp.json::<Value>().await.unwrap()["token"].as_str().unwrap().to_owned();
    assert_eq!(visible(&server, &minted).await, N_ITEMS);

    // A change to a group a mapping names ends every session authorised through the provider.
    control(&server, post.clone(), "/control/grants", json!({ "group": "readers", "term": "2" })).await;
    assert_eq!(meta_status(&server, &session).await, 403);
    assert_eq!(meta_status(&server, &minted).await, 403);

    // A rotated key is fetched when a token names it.
    idp.add_key("k2");
    idp.publish(&["k1", "k2"]);
    tokio::time::sleep(std::time::Duration::from_secs(11)).await;
    let resp = login(&server, json!({ "access_token": idp.token("k2", claims(&["tessera-readers"], exp)) })).await;
    assert_eq!(resp.status(), 200);
    let session = resp.json::<Value>().await.unwrap()["token"].as_str().unwrap().to_owned();

    // Replacing the provider ends the sessions authorised through it.
    control(&server, reqwest::Method::PUT, "/control/providers/corp", provider).await;
    assert_eq!(meta_status(&server, &session).await, 403);
}

#[tokio::test]
async fn the_catalogue_survives_a_restart() {
    let tmp = TempDir::new().unwrap();
    build_fixture(tmp.path(), N_ITEMS);
    let catalogue_dir = tmp.path().join("catalogue");
    let start = |dir: &std::path::Path| {
        let engine = tessera_engine::Engine::open(
            &tmp.path().join("bundle"),
            &tmp.path().join("cache"),
            &tmp.path().join("wal.log"),
            tessera_plugin::Passthrough::new(),
            default_engine_config(),
        )
        .unwrap();
        let (catalogue, key) = test_identity_at(dir);
        let state = tessera_server::state::AppState {
            engine,
            sessions: parking_lot::Mutex::new(Default::default()),
            heap: Default::default(),
            limits: Default::default(),
            suggest_admission: Default::default(),
            compute_gate: generous_test_gate(),
            bulk_gate: generous_bulk_gate(),
            ingest_admission: tessera_server::state::IngestAdmission::new(4),
            catalogue,
            oidc: Default::default(),
            operator_credential: OPERATOR_CREDENTIAL.to_owned(),
            faults: Arc::new(tessera_lifecycle::faults::FaultSwitchboard::new()),
        };
        (Arc::new(state), key)
    };
    let (state, key) = start(&catalogue_dir);
    let server = serve_state(state, key, None).await;
    person(&server, "ann", &["0"]).await;
    server.shutdown().await;

    let (state, key) = start(&catalogue_dir);
    let server = serve_state(state, key, None).await;
    let token = login_password(&server, "ann").await;
    assert_eq!(visible(&server, &token).await, N_ITEMS);
}

/// A deployment declaring no catalogue refuses to start, and a provider declared in the file is
/// listed and cannot be changed through the API.
#[tokio::test]
async fn providers_declared_in_the_file_are_read_only() {
    let tmp = TempDir::new().unwrap();
    let bundle = build_fixture(tmp.path(), N_ITEMS);
    std::fs::write(tmp.path().join("operator.cred"), OPERATOR_CREDENTIAL).unwrap();
    let write = |catalogue: &str| {
        let text = format!(
            r#"
            [bundle]
            path = "{bundle}"
            cache = "cache"
            wal = "wal.log"
            [plugin]
            module = "builtin:passthrough"
            [disclosure]
            token_max_lifetime = 3600
            [serve]
            viewer = "127.0.0.1:0"
            session = "127.0.0.1:0"
            control = "127.0.0.1:0"
            operator_credential_file = "operator.cred"
            {catalogue}
            "#,
            bundle = bundle.display(),
        );
        let path = tmp.path().join("tessera.toml");
        std::fs::write(&path, text).unwrap();
        path
    };

    assert!(
        tessera_server::prepare(&write("")).is_err(),
        "a deployment with no catalogue is refused"
    );

    let prepared = tessera_server::prepare(&write(
        r#"
        [catalogue]
        dir = "catalogue"
        [[catalogue.providers]]
        name = "corp"
        issuer = "https://login.example.org"
        audience = "tessera"
        jwks_url = "https://login.example.org/keys"
        "#,
    ))
    .expect("the deployment starts");
    let server = serve_state(prepared.state, String::new(), None).await;
    let listed = control(&server, reqwest::Method::GET, "/control/providers", json!({})).await;
    assert_eq!(listed["providers"][0]["name"], "corp");
    assert_eq!(listed["providers"][0]["read_only"], true);
    let resp = server
        .client
        .delete(server.control_url("/control/providers/corp"))
        .bearer_auth(OPERATOR_CREDENTIAL)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
}
